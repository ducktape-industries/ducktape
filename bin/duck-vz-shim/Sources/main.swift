// duck-vz-shim: Firecracker's seat on macOS.
//
// One process, one microVM, dead when this process dies. The host side of a
// run (crates/services/sandbox) drives every VMM the same way: write a
// Firecracker-schema JSON config, spawn `<vmm> --config-file <path>`, listen
// on `<uds_path>_<port>` unix sockets for the guest's outbound vsock
// connections, and read the serial console from the child's stdout. This shim
// implements exactly that contract over Virtualization.framework, so the Rust
// side has no macOS-specific branch beyond choosing this binary.
//
// What is deliberately NOT here:
// - no network device: a run reaches the host over vsock tunnels only, and
//   "offline" must mean no interface at all. A config naming one is refused.
// - no OCI, no image store, no daemon: the guest boots our own kernel and
//   `duck-guest-init` from block devices, same as under Firecracker.
//
// Diagnostics go to stdout/stderr, which the host captures into the run's
// console.log — the same file the guest's serial console lands in.

import Foundation
import Virtualization

// MARK: - the config file (Firecracker's schema, plus `vsock.listen_ports`)

struct BootSource: Decodable {
    var kernel_image_path: String
    var boot_args: String
}

struct Drive: Decodable {
    var drive_id: String
    var path_on_host: String
    var is_read_only: Bool
    var is_root_device: Bool
}

struct MachineConfig: Decodable {
    var vcpu_count: Int
    var mem_size_mib: UInt64
    // `smt` is accepted and ignored: Apple silicon has no SMT to turn off.
}

struct VsockConfig: Decodable {
    // `guest_cid` is accepted and ignored: Virtualization.framework fixes the
    // guest's CID itself, and the guest never names its own CID anyway — it
    // dials VMADDR_CID_HOST.
    var guest_cid: UInt32
    var uds_path: String
    // the guest-outbound ports the host has bound `<uds_path>_<port>`
    // listeners for. Firecracker forwards ANY guest-dialled port by
    // convention; Virtualization.framework wants each port declared, which is
    // why the Rust side emits this extension for the vz flavor only.
    var listen_ports: [UInt32]
}

struct VmFileConfig: Decodable {
    var bootSource: BootSource
    var drives: [Drive]
    var machineConfig: MachineConfig
    var vsock: VsockConfig
    var networkInterfaces: [NetworkInterface]?

    enum CodingKeys: String, CodingKey {
        case bootSource = "boot-source"
        case drives
        case machineConfig = "machine-config"
        case vsock
        case networkInterfaces = "network-interfaces"
    }
}

struct NetworkInterface: Decodable {
    var iface_id: String
    var host_dev_name: String
}

// MARK: - plumbing

func die(_ message: String) -> Never {
    FileHandle.standardError.write(Data(("duck-vz-shim: " + message + "\n").utf8))
    exit(1)
}

func loadConfig() -> VmFileConfig {
    let args = CommandLine.arguments
    guard args.count == 3, args[1] == "--config-file" else {
        die("usage: duck-vz-shim --config-file <vm.json>")
    }
    guard let data = FileManager.default.contents(atPath: args[2]) else {
        die("cannot read config file \(args[2])")
    }
    do {
        return try JSONDecoder().decode(VmFileConfig.self, from: data)
    } catch {
        die("cannot parse \(args[2]): \(error)")
    }
}

/// connect(2) to the unix socket the host bound for one vsock port.
/// Returns nil (after logging) when nothing is listening — the same outcome a
/// guest dialling an unserved port gets under Firecracker.
func dialHostSocket(path: String) -> Int32? {
    let fd = socket(AF_UNIX, SOCK_STREAM, 0)
    guard fd >= 0 else { return nil }
    var addr = sockaddr_un()
    addr.sun_family = sa_family_t(AF_UNIX)
    let capacity = MemoryLayout.size(ofValue: addr.sun_path) - 1
    let bytes = Array(path.utf8)
    guard bytes.count <= capacity else {
        close(fd)
        die("socket path over SUN_LEN (\(bytes.count) > \(capacity)): \(path)")
    }
    withUnsafeMutableBytes(of: &addr.sun_path) { raw in
        raw.copyBytes(from: bytes)
    }
    let len = socklen_t(MemoryLayout<sockaddr_un>.size)
    let rc = withUnsafePointer(to: &addr) { ptr in
        ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
            connect(fd, sa, len)
        }
    }
    guard rc == 0 else {
        close(fd)
        return nil
    }
    return fd
}

/// the fds we bridge must be BLOCKING: the pump below treats a failed read as
/// EOF, and the fd Virtualization.framework hands over arrives O_NONBLOCK —
/// left as-is, the very first empty read (EAGAIN) tears the whole connection
/// down, which reaches the host as "guest halted without reporting an exit
/// code" (measured on the first real boot).
func makeBlocking(_ fd: Int32) {
    let flags = fcntl(fd, F_GETFL)
    if flags >= 0 {
        _ = fcntl(fd, F_SETFL, flags & ~O_NONBLOCK)
    }
}

/// splice one direction until EOF or error, then half-close the destination so
/// the far side sees EOF. 64 KiB to match the host pump's read granularity.
///
/// Explicit pointer scopes, NEVER `&buffer[i]`: Swift's inout-to-pointer on an
/// array ELEMENT may hand `write` a pointer to a one-byte temporary, so every
/// byte after the first is garbage — measured as the guest decoding a 95 MB
/// frame length out of a StdinEof frame and dropping the stream.
func pump(from src: Int32, to dst: Int32) {
    var buffer = [UInt8](repeating: 0, count: 64 * 1024)
    while true {
        let n = buffer.withUnsafeMutableBytes { raw in
            read(src, raw.baseAddress, raw.count)
        }
        if n < 0 && errno == EINTR { continue }
        if n <= 0 { break }
        var written = 0
        var stalled = false
        buffer.withUnsafeBytes { raw in
            while written < n {
                let w = write(dst, raw.baseAddress!.advanced(by: written), n - written)
                if w < 0 && errno == EINTR { continue }
                if w <= 0 {
                    stalled = true
                    return
                }
                written += w
            }
        }
        if stalled { return }
    }
    shutdown(dst, SHUT_WR)
}

/// one guest connection bridged to one host unix socket: two pump threads and
/// a reference that keeps the VZ connection (and its fd) alive until both
/// directions are drained.
final class Bridge {
    private let connection: VZVirtioSocketConnection
    private let hostFd: Int32
    private let pending = DispatchGroup()

    init(connection: VZVirtioSocketConnection, hostFd: Int32) {
        self.connection = connection
        self.hostFd = hostFd
        let guestFd = connection.fileDescriptor
        makeBlocking(guestFd)
        makeBlocking(hostFd)
        pending.enter()
        Thread.detachNewThread { [self] in
            pump(from: guestFd, to: hostFd)
            pending.leave()
        }
        pending.enter()
        Thread.detachNewThread { [self] in
            pump(from: hostFd, to: guestFd)
            pending.leave()
        }
        pending.notify(queue: .global()) { [self] in
            close(hostFd)
            connection.close()
            Bridges.shared.drop(self)
        }
    }
}

/// keeps every live Bridge reachable; a bridge removes itself when drained.
final class Bridges {
    static let shared = Bridges()
    private var live: [ObjectIdentifier: Bridge] = [:]
    private let lock = NSLock()
    func keep(_ bridge: Bridge) {
        lock.lock()
        live[ObjectIdentifier(bridge)] = bridge
        lock.unlock()
    }
    func drop(_ bridge: Bridge) {
        lock.lock()
        live[ObjectIdentifier(bridge)] = nil
        lock.unlock()
    }
}

/// accepts guest-outbound vsock connections and bridges each to the host's
/// `<uds_path>_<port>` socket — the direction and naming Firecracker fixed.
final class Listener: NSObject, VZVirtioSocketListenerDelegate {
    private let udsPath: String
    init(udsPath: String) {
        self.udsPath = udsPath
        super.init()
    }

    func listener(
        _ listener: VZVirtioSocketListener,
        shouldAcceptNewConnection connection: VZVirtioSocketConnection,
        from socketDevice: VZVirtioSocketDevice
    ) -> Bool {
        let path = "\(udsPath)_\(connection.destinationPort)"
        guard let hostFd = dialHostSocket(path: path) else {
            // nothing listening: refuse the guest's connect, exactly like a
            // Firecracker guest dialling a port nobody served.
            FileHandle.standardError.write(
                Data("duck-vz-shim: no host listener for vsock port \(connection.destinationPort)\n".utf8))
            return false
        }
        Bridges.shared.keep(Bridge(connection: connection, hostFd: hostFd))
        return true
    }
}

/// exit when the guest does. `guestDidStop` is also where a guest-initiated
/// reboot lands (`reboot=k panic=1` in the cmdline): Virtualization.framework
/// has no reboot, so a restarting guest is a stopping VM — which is what ends
/// the run, same as Firecracker's `reboot=k` path.
final class StopWatcher: NSObject, VZVirtualMachineDelegate {
    func guestDidStop(_ virtualMachine: VZVirtualMachine) {
        exit(0)
    }
    func virtualMachine(_ virtualMachine: VZVirtualMachine, didStopWithError error: Error) {
        die("the VM stopped with an error: \(error)")
    }
}

// MARK: - main

// A tunnel peer may close while either bridge thread is writing. Let write
// return EPIPE so pump ends that direction; SIGPIPE's default action would
// kill the VMM and every other connection, including the guest's exit lane.
signal(SIGPIPE, SIG_IGN)

let config = loadConfig()

// No tap on macOS, by design and not by omission: the egress story here is
// "no interface", and a config that asks for one is a host-side bug.
if let interfaces = config.networkInterfaces, !interfaces.isEmpty {
    die("the vz backend gives a guest no network device; runs reach the host over vsock only")
}

let vmConfig = VZVirtualMachineConfiguration()

let bootLoader = VZLinuxBootLoader(kernelURL: URL(fileURLWithPath: config.bootSource.kernel_image_path))
bootLoader.commandLine = config.bootSource.boot_args
vmConfig.bootLoader = bootLoader

// Sizes are hard, so out-of-range is a refusal, not a clamp: silently
// shrinking a VM would sell N cores and deliver fewer — the exact defect the
// microVM backend exists to make impossible.
let cpus = config.machineConfig.vcpu_count
let cpuRange = VZVirtualMachineConfiguration.minimumAllowedCPUCount
    ... VZVirtualMachineConfiguration.maximumAllowedCPUCount
guard cpuRange.contains(cpus) else {
    die("vcpu_count \(cpus) is outside this host's allowed range \(cpuRange)")
}
vmConfig.cpuCount = cpus

let memory = config.machineConfig.mem_size_mib * 1024 * 1024
let memoryRange = VZVirtualMachineConfiguration.minimumAllowedMemorySize
    ... VZVirtualMachineConfiguration.maximumAllowedMemorySize
guard memoryRange.contains(memory) else {
    die("mem_size_mib \(config.machineConfig.mem_size_mib) is outside this host's allowed range")
}
vmConfig.memorySize = memory

// the serial console (`console=hvc0`), written to stdout, which the host
// captures into console.log — the only diagnostic for a guest that never
// reaches userspace.
let serial = VZVirtioConsoleDeviceSerialPortConfiguration()
serial.attachment = VZFileHandleSerialPortAttachment(
    fileHandleForReading: nil,
    fileHandleForWriting: FileHandle.standardOutput
)
vmConfig.serialPorts = [serial]

// drives in config order: the guest enumerates virtio-blk as /dev/vda, /dev/vdb…
// by position, and the manifest's mountpoints were derived from that order on
// the host — reordering here would mount the workspace at the cache's path.
vmConfig.storageDevices = config.drives.map { drive in
    let url = URL(fileURLWithPath: drive.path_on_host)
    do {
        let attachment = try VZDiskImageStorageDeviceAttachment(url: url, readOnly: drive.is_read_only)
        return VZVirtioBlockDeviceConfiguration(attachment: attachment)
    } catch {
        die("cannot attach drive \(drive.drive_id) (\(drive.path_on_host)): \(error)")
    }
}

vmConfig.socketDevices = [VZVirtioSocketDeviceConfiguration()]
// the guest kernel wants entropy at boot; without a virtio-rng it stalls on
// the crng the way early-2020s cloud images famously did.
vmConfig.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]

do {
    try vmConfig.validate()
} catch {
    die("invalid VM configuration: \(error)")
}

// Everything VZVirtualMachine is queue-confined to one dispatch queue.
let vmQueue = DispatchQueue(label: "duck-vz-shim.vm")
let vm = VZVirtualMachine(configuration: vmConfig, queue: vmQueue)
// top-level lets, not locals: the delegate references VZ holds may be weak,
// and a deallocated listener silently stops accepting the guest's dial-back.
let stopWatcher = StopWatcher()
let listenerDelegate = Listener(udsPath: config.vsock.uds_path)
let socketListener = VZVirtioSocketListener()

vmQueue.async {
    vm.delegate = stopWatcher

    // listeners BEFORE start, same doctrine as the host side: the guest dials
    // the moment it is up, and a dial with nobody listening is a run that
    // produces no output at all.
    guard let socketDevice = vm.socketDevices.first as? VZVirtioSocketDevice else {
        die("the VM came up without its virtio socket device")
    }
    socketListener.delegate = listenerDelegate
    for port in config.vsock.listen_ports {
        socketDevice.setSocketListener(socketListener, forPort: port)
    }

    vm.start { result in
        if case .failure(let error) = result {
            die("the VM failed to start: \(error)")
        }
    }
}

RunLoop.main.run()
