//! PID 1 inside a run's microVM.
//!
//! The seat `conmon` occupied under podman, and deliberately smaller: mount the
//! pseudo-filesystems and whatever block devices the host attached, read the
//! run manifest off the kernel command line, exec the agent CLI, carry its
//! stdout and stderr back over vsock as separate streams, report the exit code,
//! then halt the VM.
//!
//! Two things about being PID 1 shape the whole file:
//!
//! - **Returning from `main` panics the kernel.** Every path ends at
//!   [`halt`], including every failure path. A guest that dies without halting
//!   hangs its run until the host's idle timeout, holding all of its memory.
//! - **The HOST picks the halt method** (`DUCK_HALT=poweroff` on the kernel
//!   command line, or absent): the two VMMs are exact opposites about what
//!   ends a VM, and getting it wrong is a hang on one and a boot loop on the
//!   other — see [`halt`].

// Both halves of the host<->guest contract live in sandbox-host and are
// included here verbatim rather than depended on. The host has to ENCODE the
// manifest this parses and DECODE the frames this writes, so a second
// definition on either side is a wire-format fork waiting to happen — while
// PID 1 still must not carry tokio and tracing.
//
// Each end uses only part of each module (the guest parses manifests and
// encodes frames; the host does the reverse), so the unused part is dead code
// here by construction, not by oversight.
#[allow(dead_code)]
#[path = "../../../crates/services/sandbox/src/guest_manifest.rs"]
mod manifest;

#[allow(dead_code)]
#[path = "../../../crates/services/sandbox/src/guest_proto.rs"]
mod guest_proto;

#[allow(dead_code)]
#[path = "../../../crates/services/sandbox/src/guest_paths.rs"]
mod paths;

use guest_proto::{Frame, encode};
use manifest::RunManifest;
use std::ffi::CString;
use std::io::Write as _;
use std::os::fd::{AsRawFd as _, RawFd};

/// exit code reported when the init itself failed, as opposed to the run. Well
/// clear of the 0-125 a program picks and of the 128+N a signal produces.
const INIT_FAILED: i32 = 126;

fn main() {
    // The console is the only diagnostic channel before vsock is up, and this
    // is a guest program's own output rather than node logging — the tracing
    // rule in CLAUDE.md governs the node, which has a subscriber and a log
    // ring. This has neither.
    let code = match run() {
        Ok(code) => code,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "duck-guest-init: {e}");
            INIT_FAILED
        }
    };
    let _ = writeln!(std::io::stderr(), "duck-guest-init: exit {code}");
    halt();
}

fn run() -> Result<i32, String> {
    mount_base_filesystems()?;

    let manifest = read_manifest()?;

    for mount in &manifest.mounts {
        mount_ext4(mount)?;
    }

    for (index, port) in manifest.tunnel_ports.iter().enumerate() {
        // best-effort: a run whose tunnel fails to bind will fail its API calls
        // with a clear connection error from the CLI, which is a better
        // diagnosis than refusing to boot with none of the run's own output.
        if let Err(e) = start_tunnel(*port, index) {
            let _ = writeln!(std::io::stderr(), "duck-guest-init: tunnel {port}: {e}");
        }
    }

    let host = connect_to_host()?;
    let code = exec_and_pump(&manifest, host);

    // Flush the workspace image before the VM dies. `sync` is not optional: the
    // host reads the image back with debugfs immediately after the VMM exits,
    // and an unflushed page cache means the run's output is simply missing.
    unsafe { libc::sync() };
    for mount in manifest.mounts.iter().rev() {
        unmount(&mount.at);
    }
    code
}

/// read the run manifest off its own block device.
///
/// RAW, with no filesystem to mount, because the manifest is what says which
/// filesystems to mount. Reading the whole device rather than the payload is
/// deliberate: its length lives in the blob's header, so there is nothing to
/// know before the read.
fn read_manifest() -> Result<RunManifest, String> {
    let device = manifest::MANIFEST_DEVICE;
    let blob = std::fs::read(device).map_err(|e| format!("read {device}: {e}"))?;
    manifest::decode(&blob).map_err(|e| format!("{device}: {e}"))
}

// ---------------------------------------------------------------------------
// mounts
// ---------------------------------------------------------------------------

/// the filesystems the rootfs itself cannot provide.
///
/// Two groups, in this order for two different reasons.
///
/// devtmpfs is FIRST and non-negotiable: without it there are no `/dev/vd*`
/// nodes, so every block device in the manifest is unmountable.
///
/// The tmpfs group exists because the rootfs is READ-ONLY and shared by every
/// concurrent run on this node, and an ordinary userland expects to write these
/// four. A CLI that cannot then fails in whatever way it happens to fail, far
/// from the cause — measured, as `claude` exiting 1 with
/// `EROFS: mkdir '/tmp/claude-0'`. Each is RAM-backed and per-run, so nothing
/// written there outlives the VM or is visible to another buyer's run; the
/// guest's own memory cap is what bounds them.
fn mount_base_filesystems() -> Result<(), String> {
    for (source, target, fstype) in [
        ("devtmpfs", "/dev", "devtmpfs"),
        // AFTER devtmpfs, which lands over /dev and would bury it. An
        // interactive run's pty slave is a node in here: without devpts,
        // `ptsname` names a path that does not exist and the TUI never gets a
        // terminal. Cheap enough to mount unconditionally.
        ("devpts", "/dev/pts", "devpts"),
        ("proc", "/proc", "proc"),
        ("sysfs", "/sys", "sysfs"),
        ("tmpfs", "/tmp", "tmpfs"),
        ("tmpfs", "/run", "tmpfs"),
        ("tmpfs", "/var/tmp", "tmpfs"),
        // the run's HOME: a CLI's own state directory. Per-run and discarded —
        // the credential the run actually uses is seeded into its config home
        // inside the workspace, by the host.
        ("tmpfs", paths::GUEST_HOME, "tmpfs"),
    ] {
        std::fs::create_dir_all(target).map_err(|e| format!("create {target}: {e}"))?;
        match mount(source, target, fstype, 0, None) {
            Ok(()) => {}
            // EBUSY = already mounted, which for these three is the NORMAL
            // case, not a failure: a kernel built with CONFIG_DEVTMPFS_MOUNT
            // mounts /dev itself before handing control to PID 1. Treating it
            // as an error aborts the boot on a working guest — measured, on
            // the Firecracker CI kernel, as "the guest never dialled back".
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {}
            Err(e) => {
                return Err(format!("mount {source} -> {target} ({fstype}): {e}"));
            }
        }
    }
    Ok(())
}

fn mount_ext4(m: &manifest::GuestMount) -> Result<(), String> {
    let (device, at) = (&m.device, &m.at);
    std::fs::create_dir_all(at).map_err(|e| format!("create {at}: {e}"))?;
    // MS_RDONLY is mandatory for a drive the host attached read-only, not an
    // optimization: Firecracker refuses the write, so a read-write mount fails
    // with EACCES and the operator sees only "the guest never dialled back".
    let flags = if m.read_only { libc::MS_RDONLY } else { 0 };
    mount(device, at, "ext4", flags, None)
        .map_err(|e| format!("mount {device} -> {at} (ext4): {e}"))
}

/// Returns the raw `io::Error` rather than a message, so a caller can branch on
/// the errno — `EBUSY` means "already mounted", which is a normal outcome for
/// the pseudo-filesystems and a failure for anything else.
fn mount(
    source: &str,
    target: &str,
    fstype: &str,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> Result<(), std::io::Error> {
    let invalid = |what: &str| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{what} holds an interior NUL"),
        )
    };
    let c_source = CString::new(source).map_err(|_| invalid(source))?;
    let c_target = CString::new(target).map_err(|_| invalid(target))?;
    let c_fstype = CString::new(fstype).map_err(|_| invalid(fstype))?;
    let c_data = data
        .map(|d| CString::new(d).map_err(|_| invalid(d)))
        .transpose()?;
    let data_ptr = c_data
        .as_ref()
        .map_or(std::ptr::null(), |d| d.as_ptr().cast());
    let rc = unsafe {
        libc::mount(
            c_source.as_ptr(),
            c_target.as_ptr(),
            c_fstype.as_ptr(),
            flags,
            data_ptr,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// unmount best-effort: `sync` above already flushed, so a busy mountpoint at
/// teardown costs nothing and must not turn a finished run into a failed one.
fn unmount(mountpoint: &str) {
    let Ok(c_target) = CString::new(mountpoint) else {
        return;
    };
    unsafe { libc::umount(c_target.as_ptr()) };
}

// ---------------------------------------------------------------------------
// the broker tunnel
// ---------------------------------------------------------------------------

/// serve `127.0.0.1:port` inside the guest and forward every connection to the
/// host over vsock.
///
/// The run's CLI dials its credential broker as ordinary HTTP. With no network
/// device there is no route out of the VM, so this is the route — and it is a
/// better one: the far end is a socket the host process owns, not an interface,
/// so the guest cannot reach anything else on the host by changing an address.
/// The credential itself never enters the VM; the broker holds it and the guest
/// carries only an opaque per-run bearer.
fn start_tunnel(port: u16, index: usize) -> Result<(), String> {
    bring_up_loopback()?;
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("bind 127.0.0.1:{port}: {e}"))?;
    // the host bound its listeners in this order; the guest never names a
    // destination, it only picks the matching vsock port.
    let vsock_port = guest_proto::TUNNEL_PORT_BASE + index as u32;
    std::thread::Builder::new()
        .name(format!("tunnel-{port}"))
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                // one vsock connection per TCP connection: HTTP keep-alive and
                // concurrent requests both work, and a stuck request cannot
                // block another.
                std::thread::spawn(move || tunnel_one(stream, vsock_port));
            }
        })
        .map_err(|e| format!("spawn tunnel thread: {e}"))?;
    Ok(())
}

fn tunnel_one(tcp: std::net::TcpStream, vsock_port: u32) {
    let Ok(vsock) = connect_vsock(vsock_port) else {
        return;
    };
    let Ok(tcp_write) = tcp.try_clone() else {
        unsafe { libc::close(vsock) };
        return;
    };
    let tcp_fd = tcp.as_raw_fd();
    // two directions, two threads. Splicing both in one thread would deadlock
    // the moment either side filled its buffer.
    let up = std::thread::spawn(move || {
        splice(tcp_fd, vsock);
        // half-close so the far end sees EOF rather than waiting forever
        unsafe { libc::shutdown(vsock, libc::SHUT_WR) };
    });
    splice(vsock, tcp_write.as_raw_fd());
    let _ = tcp_write.shutdown(std::net::Shutdown::Write);
    let _ = up.join();
    unsafe { libc::close(vsock) };
}

/// copy `from` to `to` until either end closes.
fn splice(from: RawFd, to: RawFd) {
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        let n = unsafe { libc::read(from, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            return;
        }
        write_all(to, &buf[..n as usize]);
    }
}

/// bring `lo` up. Without it, binding 127.0.0.1 succeeds and connecting to it
/// fails with `ENETUNREACH` — a failure that reads like the broker is down.
fn bring_up_loopback() -> Result<(), String> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        return Err(format!("socket: {}", std::io::Error::last_os_error()));
    }
    let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
    for (i, b) in b"lo".iter().enumerate() {
        req.ifr_name[i] = *b as libc::c_char;
    }
    req.ifr_ifru.ifru_flags = (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
    // `as _`, not a named type: ioctl's request argument is c_ulong on glibc
    // and c_int on musl, and this file is compiled against both — musl for the
    // shipped guest binary, glibc for `cargo test` on the host.
    let rc = unsafe { libc::ioctl(fd, libc::SIOCSIFFLAGS as _, &req) };
    unsafe { libc::close(fd) };
    if rc != 0 {
        return Err(format!("bring up lo: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// the host connection
// ---------------------------------------------------------------------------

/// dial the host over vsock. Guest-initiated is the simpler direction: the host
/// listens on `<uds>_<port>` and Firecracker bridges this connection to it, so
/// no in-guest listener and no connect-back race.
fn connect_vsock(port: u32) -> Result<RawFd, String> {
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(format!("vsock socket: {}", std::io::Error::last_os_error()));
    }
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    addr.svm_cid = libc::VMADDR_CID_HOST;
    addr.svm_port = port;
    let rc = unsafe {
        libc::connect(
            fd,
            std::ptr::addr_of!(addr).cast(),
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(format!("vsock connect to host port {port}: {err}"));
    }
    Ok(fd)
}

fn connect_to_host() -> Result<RawFd, String> {
    connect_vsock(guest_proto::VSOCK_PORT)
}

fn send(fd: RawFd, frame: &Frame) {
    let bytes = encode(frame);
    let mut written = 0usize;
    while written < bytes.len() {
        let n = unsafe { libc::write(fd, bytes[written..].as_ptr().cast(), bytes.len() - written) };
        if n <= 0 {
            // The host hung up. Nothing to report it to, and the run's exit
            // still has to reach `halt`.
            return;
        }
        written += n as usize;
    }
}

// ---------------------------------------------------------------------------
// the run
// ---------------------------------------------------------------------------

/// run the child and carry its stdio to the host, either way it is wired.
///
/// The two shapes are genuinely different plumbing, not one with a flag: pipes
/// give three descriptors and a separate stderr, a pty gives ONE bidirectional
/// descriptor whose read end is stdout+stderr merged by the kernel. Only the
/// tail — report the exit code, wait for the host to acknowledge — is shared,
/// and it is shared HERE so neither path can forget it.
fn exec_and_pump(manifest: &RunManifest, host: RawFd) -> Result<i32, String> {
    let code = match manifest.pty {
        true => run_on_pty(manifest, host)?,
        false => run_on_pipes(manifest, host)?,
    };
    send(host, &Frame::Exit(code));
    wait_for_host_close(host);
    Ok(code)
}

/// the interactive shape: the child owns a pty slave as its controlling
/// terminal, and this process holds the master.
///
/// The pty is allocated in the GUEST because a pty master and its slave are two
/// ends of one kernel object — the host's kernel cannot hand a terminal to a
/// process on another one. So the operator's keystrokes arrive as ordinary
/// stdin frames and become terminal input here.
fn run_on_pty(manifest: &RunManifest, host: RawFd) -> Result<i32, String> {
    let (master, slave) = open_pty()?;
    // A SANE INITIAL SIZE, before the child ever runs. A pty is created 0x0,
    // and a TUI handed 0x0 draws nothing until its first resize — while the
    // host's first `Resize` can easily arrive after the CLI has already
    // painted. 80x24 is what every terminal assumes when it knows nothing.
    resize_pty(master, 80, 24);

    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(format!("fork: {}", std::io::Error::last_os_error()));
    }
    if child == 0 {
        // ---- child: never returns ----
        unsafe {
            libc::close(master);
            // A NEW SESSION FIRST. TIOCSCTTY only grants a controlling terminal
            // to a session leader, and without a controlling terminal the TUI
            // gets no SIGWINCH, no job control, and no ^C.
            libc::setsid();
            libc::ioctl(slave, libc::TIOCSCTTY, 0);
            libc::dup2(slave, libc::STDIN_FILENO);
            libc::dup2(slave, libc::STDOUT_FILENO);
            libc::dup2(slave, libc::STDERR_FILENO);
            if slave > libc::STDERR_FILENO {
                libc::close(slave);
            }
        }
        exec_child(manifest, host);
    }

    // the slave stays open in the CHILD only: holding a copy here would keep
    // the master readable forever after the child exits, and the pump would
    // never see the end of the session.
    unsafe { libc::close(slave) };
    pump_pty(host, master);
    let code = wait_for(child);
    unsafe { libc::close(master) };
    Ok(code)
}

/// the headless shape: three pipes, stderr kept separate so the host can report
/// a CLI's diagnostics apart from its answer.
fn run_on_pipes(manifest: &RunManifest, host: RawFd) -> Result<i32, String> {
    let mut in_pipe = [0 as RawFd; 2];
    let mut out_pipe = [0 as RawFd; 2];
    let mut err_pipe = [0 as RawFd; 2];
    for pipe in [&mut in_pipe, &mut out_pipe, &mut err_pipe] {
        if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
            return Err(format!("pipe: {}", std::io::Error::last_os_error()));
        }
    }

    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(format!("fork: {}", std::io::Error::last_os_error()));
    }
    if child == 0 {
        // ---- child: never returns ----
        unsafe {
            libc::close(in_pipe[1]);
            libc::close(out_pipe[0]);
            libc::close(err_pipe[0]);
            libc::dup2(in_pipe[0], libc::STDIN_FILENO);
            libc::dup2(out_pipe[1], libc::STDOUT_FILENO);
            libc::dup2(err_pipe[1], libc::STDERR_FILENO);
            libc::close(in_pipe[0]);
            libc::close(out_pipe[1]);
            libc::close(err_pipe[1]);
        }
        exec_child(manifest, host);
    }

    unsafe {
        libc::close(in_pipe[0]);
        libc::close(out_pipe[1]);
        libc::close(err_pipe[1]);
    }
    pump(host, in_pipe[1], out_pipe[0], err_pipe[0]);
    Ok(wait_for(child))
}

/// allocate a pty pair. `posix_openpt` + `grantpt` + `unlockpt` + `ptsname` is
/// the portable four-step; the slave path it yields is a node under the devpts
/// [`mount_base_filesystems`] mounted.
fn open_pty() -> Result<(RawFd, RawFd), String> {
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if master < 0 {
        return Err(format!("posix_openpt: {}", std::io::Error::last_os_error()));
    }
    if unsafe { libc::grantpt(master) } != 0 {
        unsafe { libc::close(master) };
        return Err(format!("grantpt: {}", std::io::Error::last_os_error()));
    }
    if unsafe { libc::unlockpt(master) } != 0 {
        unsafe { libc::close(master) };
        return Err(format!("unlockpt: {}", std::io::Error::last_os_error()));
    }
    let mut name = [0 as libc::c_char; 128];
    let rc = unsafe { libc::ptsname_r(master, name.as_mut_ptr(), name.len()) };
    if rc != 0 {
        unsafe { libc::close(master) };
        return Err(format!(
            "ptsname_r: {}",
            std::io::Error::from_raw_os_error(rc)
        ));
    }
    let slave = unsafe { libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if slave < 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(master) };
        return Err(format!("open pty slave: {err}"));
    }
    Ok((master, slave))
}

/// carry the session between the pty master and the host until the child's side
/// of the terminal closes.
///
/// ONE descriptor for both directions, which is the whole difference from
/// [`pump`]: what the child writes comes back off the master, and what the host
/// sends is written into it. Reading the master after the last slave closes
/// gives EIO on Linux rather than EOF — that is the end of the session, not an
/// error to report.
///
/// KNOWN LIMIT: "the last slave closes" means every holder, so a TUI that exits
/// leaving a background grandchild on the terminal keeps this open, and the
/// exit frame waits with it. The session then ends when the operator closes it,
/// which tears the VM down and still reads the workspace back — so the failure
/// mode is a session that looks alive rather than a lost run. Noticing the
/// child's exit independently needs a `signalfd` for SIGCHLD in the poll set;
/// it is not here because PID 1 taking over signal handling deserves its own
/// change.
fn pump_pty(host: RawFd, master: RawFd) {
    let mut host_buf: Vec<u8> = Vec::new();
    let mut fds = [
        libc::pollfd {
            fd: master,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: host,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if rc < 0 {
            let interrupted =
                std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted;
            if interrupted {
                continue;
            }
            return;
        }
        for (slot, pollfd) in fds.iter_mut().enumerate() {
            let retired = pollfd.fd < 0;
            let ready = pollfd.revents & (libc::POLLIN | libc::POLLHUP) != 0;
            if retired || !ready {
                continue;
            }
            let n = unsafe { libc::read(pollfd.fd, buf.as_mut_ptr().cast(), buf.len()) };
            let is_master = slot == 0;
            if n > 0 {
                match is_master {
                    // a pty merges the child's stdout and stderr into one
                    // stream — there is no second descriptor to tell them
                    // apart, and a terminal never had them apart.
                    true => send(host, &Frame::Stdout(buf[..n as usize].to_vec())),
                    false => {
                        host_buf.extend_from_slice(&buf[..n as usize]);
                        feed_pty(&mut host_buf, master);
                    }
                }
                continue;
            }
            // the master went quiet: the child closed its last slave
            // descriptor, so the session is over whatever the host is doing.
            if is_master {
                return;
            }
            // the host stopped sending input. NOT the end of the run: the child
            // keeps running and its exit code still has to go out on this same
            // socket, so retire only this direction.
            pollfd.fd = -1;
        }
    }
}

/// drain whole frames out of `buf` and apply them to the pty master.
///
/// A partial frame stays in the buffer for the next read.
fn feed_pty(buf: &mut Vec<u8>, master: RawFd) {
    loop {
        let frame = match guest_proto::decode(buf) {
            Ok(Some(frame)) => frame,
            Ok(None) => return,
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "duck-guest-init: input frame: {e}");
                return;
            }
        };
        match frame {
            Frame::Stdin(bytes) => write_all(master, &bytes),
            // A TERMINAL HAS NO EOF TO CLOSE. The headless path closes the
            // child's stdin here so a CLI blocking on EOF proceeds; doing that
            // to a pty would tear down the session the operator is still
            // typing into. `^D` is a byte on the Stdin lane like any other.
            Frame::StdinEof => {}
            Frame::Resize { cols, rows } => resize_pty(master, cols, rows),
            // the host never sends these.
            Frame::Stdout(_) | Frame::Stderr(_) | Frame::Exit(_) | Frame::Spawn => {}
        }
    }
}

/// apply a window size to the pty. The kernel is what turns this into the
/// SIGWINCH the TUI redraws on, so there is nothing to forward to the child.
fn resize_pty(master: RawFd, cols: u16, rows: u16) {
    let size = libc::winsize {
        ws_col: cols,
        ws_row: rows,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe { libc::ioctl(master, libc::TIOCSWINSZ, &size) };
}

/// block until the host closes the connection.
///
/// This is the run's only acknowledgement, and it is not optional. Firecracker
/// relays the guest's vsock writes to the host socket ASYNCHRONOUSLY, so a
/// guest that reset the instant after its last write loses whatever the VMM has
/// not relayed yet — and the frame that goes missing is the last one, the exit
/// code. Measured, that reaches the operator as "guest halted without reporting
/// an exit code" on a run that in fact finished cleanly.
///
/// Unbounded on purpose: the host closes as soon as it has the exit frame, and
/// if the host process dies instead, its socket closes too. There is no third
/// outcome to time out against.
fn wait_for_host_close(host: RawFd) {
    let mut buf = [0u8; 256];
    loop {
        let n = unsafe { libc::read(host, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            continue;
        }
        let interrupted =
            n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted;
        if !interrupted {
            return;
        }
    }
}

/// the child half of the fork. Diverges: on a successful `execve` this process
/// becomes the CLI, and on a failed one it must `_exit` rather than return into
/// a second copy of the init.
fn exec_child(manifest: &RunManifest, host: RawFd) -> ! {
    let fail = |msg: &str| -> ! {
        let _ = writeln!(std::io::stderr(), "duck-guest-init: {msg}");
        unsafe { libc::_exit(INIT_FAILED) }
    };

    if std::env::set_current_dir(&manifest.cwd).is_err() {
        fail(&format!("chdir {}", manifest.cwd));
    }

    let Ok(argv): Result<Vec<CString>, _> = manifest
        .argv
        .iter()
        .map(|a| CString::new(a.as_bytes()))
        .collect()
    else {
        fail("argv holds an interior NUL")
    };
    let Ok(envp): Result<Vec<CString>, _> = manifest
        .env
        .iter()
        .map(|(k, v)| CString::new(format!("{k}={v}")))
        .collect()
    else {
        fail("env holds an interior NUL")
    };

    let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    let mut envp_ptrs: Vec<*const libc::c_char> = envp.iter().map(|e| e.as_ptr()).collect();
    envp_ptrs.push(std::ptr::null());

    // The host-side timing driver cannot observe a guest fork directly. This
    // marker crosses the existing stdio vsock before exec, so session timing is
    // anchored to the actual guest child path rather than VM boot completion.
    send(host, &Frame::Spawn);
    unsafe { libc::execve(argv[0].as_ptr(), argv_ptrs.as_ptr(), envp_ptrs.as_ptr()) };
    fail(&format!("execve {}", manifest.argv[0]))
}

/// carry the run's three streams between the child and the host until both
/// output pipes reach EOF.
///
/// All three directions are driven by ONE `poll`, which is what makes stdin
/// concurrent with stdout: the run's prompt arrives on stdin and can be larger
/// than a pipe buffer, so a guest that wrote stdin first and only then read
/// output would deadlock against a CLI that streams before it drains.
///
/// `poll` rather than threads — PID 1 with four descriptors does not need a
/// runtime.
fn pump(host: RawFd, in_fd: RawFd, out_fd: RawFd, err_fd: RawFd) {
    let mut stdin_fd = in_fd;
    let mut host_buf: Vec<u8> = Vec::new();
    let mut fds = [
        libc::pollfd {
            fd: out_fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: err_fd,
            events: libc::POLLIN,
            revents: 0,
        },
        // the host half: inbound stdin frames. Retired on EOF like the others,
        // but it does NOT keep the loop alive — the run ends when the child's
        // output ends, not when the host stops talking.
        libc::pollfd {
            fd: host,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let mut buf = vec![0u8; 64 * 1024];
    let mut open = 2;
    while open > 0 {
        // -1: block. The child's own output is the only clock here — a timeout
        // would be a guess about how long a run may be quiet, and runs are
        // quiet for minutes at a time.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if rc < 0 {
            let interrupted =
                std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted;
            if interrupted {
                continue;
            }
            return;
        }
        for (slot, pollfd) in fds.iter_mut().enumerate() {
            let retired = pollfd.fd < 0;
            let ready = pollfd.revents & (libc::POLLIN | libc::POLLHUP) != 0;
            if retired || !ready {
                continue;
            }
            let n = unsafe { libc::read(pollfd.fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                let payload = buf[..n as usize].to_vec();
                match slot {
                    0 => send(host, &Frame::Stdout(payload)),
                    1 => send(host, &Frame::Stderr(payload)),
                    // the host half carries stdin frames the other way.
                    _ => {
                        host_buf.extend_from_slice(&payload);
                        feed_stdin(&mut host_buf, &mut stdin_fd);
                    }
                }
                continue;
            }
            // 0 = EOF, <0 = a broken pipe. Either way this direction is done;
            // retire it so `poll` stops reporting it ready forever.
            let is_output_pipe = slot < 2;
            if is_output_pipe {
                unsafe { libc::close(pollfd.fd) };
                open -= 1;
            }
            // The host descriptor is NOT closed here: it is the same socket
            // `send` writes to, and the run's exit code still has to go out
            // after the host has stopped sending stdin.
            pollfd.fd = -1;
        }
    }
}

/// drain whole frames out of `buf` and apply them to the child's stdin.
///
/// A partial frame stays in the buffer for the next read — a vsock read returns
/// whatever happened to arrive, and the prompt routinely spans several.
fn feed_stdin(buf: &mut Vec<u8>, stdin_fd: &mut RawFd) {
    loop {
        let frame = match guest_proto::decode(buf) {
            Ok(Some(frame)) => frame,
            // a whole frame has not arrived yet
            Ok(None) => return,
            // The host writes these, so a malformed one is our own bug rather
            // than a hostile guest. Nothing useful to do mid-run: stop feeding
            // stdin and let the child see EOF.
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "duck-guest-init: stdin frame: {e}");
                close_stdin(stdin_fd);
                return;
            }
        };
        match frame {
            Frame::Stdin(bytes) => write_all(*stdin_fd, &bytes),
            Frame::StdinEof => close_stdin(stdin_fd),
            // a window size is a property of a terminal, and a headless run's
            // stdin is a pipe. Nothing to apply it to.
            Frame::Resize { .. } => {}
            // The host never sends these; ignoring them keeps a host-side bug
            // from taking the run down.
            Frame::Stdout(_) | Frame::Stderr(_) | Frame::Exit(_) | Frame::Spawn => {}
        }
    }
}

fn close_stdin(stdin_fd: &mut RawFd) {
    if *stdin_fd < 0 {
        return;
    }
    unsafe { libc::close(*stdin_fd) };
    *stdin_fd = -1;
}

fn write_all(fd: RawFd, bytes: &[u8]) {
    if fd < 0 {
        return;
    }
    let mut written = 0usize;
    while written < bytes.len() {
        let n = unsafe { libc::write(fd, bytes[written..].as_ptr().cast(), bytes.len() - written) };
        if n <= 0 {
            return;
        }
        written += n as usize;
    }
}

/// the child's exit status as a single number, matching shell convention:
/// 128 + N for a signal death, so a killed run is distinguishable from one that
/// returned the same small number.
fn wait_for(child: libc::pid_t) -> i32 {
    let mut status: libc::c_int = 0;
    loop {
        let rc = unsafe { libc::waitpid(child, &mut status, 0) };
        if rc < 0 {
            let interrupted =
                std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted;
            if interrupted {
                continue;
            }
            return INIT_FAILED;
        }
        if libc::WIFEXITED(status) {
            return libc::WEXITSTATUS(status);
        }
        if libc::WIFSIGNALED(status) {
            return 128 + libc::WTERMSIG(status);
        }
    }
}

// ---------------------------------------------------------------------------
// halt
// ---------------------------------------------------------------------------

/// stop the VM. Diverges — PID 1 has nowhere to return to.
///
/// HOW to stop is the hypervisor's contract, so the HOST says which, via the
/// kernel command line: an unrecognized `NAME=value` boot parameter lands in
/// PID 1's environment, so `DUCK_HALT=poweroff` reaches this env read with no
/// /proc and no extra device. The two VMMs are exact opposites here, measured
/// on both:
/// - Firecracker (x86): POWER_OFF is `reboot: System halted` and the VMM
///   never exits (no ACPI); RESTART goes through the `reboot=k` i8042 reset
///   the VMM watches for. One guest hung past 120 s on POWER_OFF; an
///   otherwise identical one exited in 428 ms on RESTART.
/// - Virtualization.framework (arm64): RESTART actually REBOOTS the guest —
///   the run's init came back up, redialled a consumed listener, and the VM
///   boot-looped forever; POWER_OFF is PSCI SYSTEM_OFF, which is what stops
///   the VM and lets the shim exit.
fn halt() -> ! {
    let poweroff = std::env::var_os("DUCK_HALT").is_some_and(|v| v == "poweroff");
    if poweroff {
        unsafe { libc::reboot(libc::RB_POWER_OFF) };
        // fall through: a kernel without a wired power-off path returns here,
        // and on such a machine RESTART is the next-best exit signal.
    }
    unsafe { libc::reboot(libc::RB_AUTOBOOT) };
    // Only reachable if reboot(2) itself failed, which means this is not PID 1
    // or the kernel refused. Spin rather than return into a kernel panic.
    loop {
        unsafe { libc::pause() };
    }
}
