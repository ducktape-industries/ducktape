// Tells ops/mac/mac.py when macOS sleeps, wakes and changes network, one line each on stdout: "will-sleep" holds the sleep until a line comes
// back on stdin (the Mac has said goodbye to the relay), or until macOS stops waiting on its own; "did-wake" once the machine is on again;
// "network-changed" whenever the network path changes (NWPathMonitor), so a relay that could not be reached is tried again. Run as `swift power.swift`.
import Foundation
import IOKit
import IOKit.pwr_mgt
import Network

let canSystemSleep: UInt32 = 0xE000_0270
let systemWillSleep: UInt32 = 0xE000_0280
let systemHasPoweredOn: UInt32 = 0xE000_0300

var root: io_connect_t = 0
var port: IONotificationPortRef?
var notifier: io_object_t = 0
let saying = NSLock()

func said(_ line: String) {
    saying.lock()
    print(line)
    fflush(stdout)
    saying.unlock()
}

let heard: IOServiceInterestCallback = { _, _, message, argument in
    switch message {
    case canSystemSleep:
        IOAllowPowerChange(root, Int(bitPattern: argument))
    case systemWillSleep:
        said("will-sleep")
        _ = readLine()
        IOAllowPowerChange(root, Int(bitPattern: argument))
    case systemHasPoweredOn:
        said("did-wake")
    default:
        break
    }
}

root = IORegisterForSystemPower(nil, &port, heard, &notifier)
guard root != 0, let port else {
    FileHandle.standardError.write(Data("IORegisterForSystemPower refused\n".utf8))
    exit(1)
}
CFRunLoopAddSource(CFRunLoopGetCurrent(), IONotificationPortGetRunLoopSource(port).takeUnretainedValue(), .defaultMode)
let paths = NWPathMonitor()
paths.pathUpdateHandler = { _ in said("network-changed") }
paths.start(queue: DispatchQueue(label: "industries.ducktape.mac.paths"))
CFRunLoopRun()
