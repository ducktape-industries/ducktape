#!/usr/bin/env python3
"""Exercise the shim's socket pump and startup signal policy, without a VM."""

import pathlib
import subprocess
import tempfile


source = pathlib.Path(__file__).with_name("Sources").joinpath("main.swift").read_text()
# Keep the real declarations and process setup; replace only VM startup.
startup = "let config = loadConfig()"
assert source.count(startup) == 1, "update the harness for the shim's entrypoint"
prefix = source.split(startup)[0]
harness = r'''
func socketPair() -> [Int32] {
    var fds: [Int32] = [0, 0]
    precondition(socketpair(AF_UNIX, SOCK_STREAM, 0, &fds) == 0)
    return fds
}

func exercisePump(closedPeer: Bool) {
    let input = socketPair()
    let output = socketPair()
    defer {
        input.forEach { close($0) }
        close(output[0])
        if !closedPeer { close(output[1]) }
    }
    if closedPeer { close(output[1]) }
    let payload = Array((0..<8192).map { UInt8($0 % 251) })
    let written = payload.withUnsafeBytes {
        write(input[0], $0.baseAddress, $0.count)
    }
    precondition(written == payload.count)
    shutdown(input[0], SHUT_WR)
    pump(from: input[1], to: output[0])
    if closedPeer { return }
    var received = [UInt8]()
    var buffer = [UInt8](repeating: 0, count: 1024)
    while true {
        let count = buffer.withUnsafeMutableBytes {
            read(output[1], $0.baseAddress, $0.count)
        }
        precondition(count >= 0)
        if count == 0 { break }
        received.append(contentsOf: buffer.prefix(count))
    }
    precondition(received == payload)
}

exercisePump(closedPeer: false)
exercisePump(closedPeer: true)
// A failed tunnel must leave the process able to carry the exit lane.
exercisePump(closedPeer: false)
print("bridge: payload, EOF, broken peer, and subsequent connection OK")
'''

with tempfile.TemporaryDirectory(prefix="duck-vz-test-") as scratch:
    swift = pathlib.Path(scratch) / "main.swift"
    binary = pathlib.Path(scratch) / "bridge-test"
    swift.write_text(prefix + harness)
    subprocess.run(
        ["xcrun", "swiftc", "-O", str(swift), "-o", str(binary),
         "-framework", "Virtualization"],
        check=True,
    )
    subprocess.run([str(binary)], check=True, timeout=10)
