# Independent application deployment

This example combines a consensus policy module, an installed HTTP/WebSocket
service, and a WASM view. None belongs to the node's founding module set or the
desktop's native product dispatch. The module stores membership and records;
the service asks its authorization query for every request and WebSocket frame.
The view uses `rpc.query`, `op.submit`, `net.request`, `net.stream`, and `net.send`.

## Run the deployment check

Use Linux with systemd, passwordless `sudo`, the repository's Rust build
dependencies, and the `wasm32-unknown-unknown` target. Build the founding module
set and views using the repository's normal `make wasm-modules` and `make views`
commands first. Run from the repository root:

```sh
python3 crates/examples/extension-probe/check.py
```

The check compiles the node, desktop, and desktop test executable and records
their hashes **before** building the example artifacts. It starts three local
validators, registers the module and view through governance, and installs the
service using the [application installer](../../../docs/deploy/application-service.md).
The installer creates temporary systemd units under a process-specific name;
the test removes its units and service state when it exits normally or unwinds.

The registry test keeps one desktop test process alive while governance replaces
an independently registered view. It uses the ordinary registry discovery,
artifact download and verification, mount, and deployment-check paths. It opens
the native desktop test window, selects the registry-discovered tab through its
real navigation button, types through the native input, and clicks the view's
Read button to query the deployed module. It keeps that window alive while
checking that the tab seat remains, the old instance retires, the new hash and
title take effect, and the draft survives the host's replacement snapshot path.
After that real loader swap, a test-only writer injects one delayed reply into
the retired instance's queue. The replacement leaves that queue untouched and
keeps its output and draft. This checks reply-queue ownership; it does not
simulate delayed packets on a real socket.

A separate file-loaded host test directly supplies input and button events to
the guest. It records state through consensus and exchanges signed HTTP and
bidirectional WebSocket messages through another validator. The service checks
reject nonmembers and oversized input, restart the publisher and service, and
verify shutdown refusal and unchanged native hashes. Service replacement stops
and unbinds ingress before installing artifacts and activating module/view code;
activation binds only after the process reports readiness.

These tests execute the desktop test binary through its normal native window,
registered-tab, input, and button paths. The standalone desktop executable is
built and hashed; the check does not launch its main entry point. Native test
input exercises widget dispatch in the headless test window. Physical input,
pixel appearance, delayed network replies during replacement, and device quality
require the [QA app lanes](../../../skills/qa/SKILL.md) or a dedicated network
fixture; the file-loaded test does not establish those properties.

The replacement service lowercases its response instead of uppercasing it. The
replacement module requires a leading `#` while retaining stored membership and
records. The replacement view changes its title and restores its draft without
reusing a native stream handle. Replacement does not migrate stored data.

## Build artifacts alone

```sh
cargo build -p guest-builder
python3 crates/examples/extension-probe/build.py
```

`artifacts/` contains the original and replacement module, view, and service
files. The directory is ignored; no native executable embeds those bytes.
The three packages are independent Cargo workspaces and can be tested using
`cargo test --manifest-path crates/examples/extension-probe/<package>/Cargo.toml`
with `module`, `view`, or `service` substituted for `<package>`.

The service tests cover malformed and oversized input, attested caller headers,
policy refusal, and revocation during a live stream. The file-loaded
`wasm-host` test `deployed_policy` verifies execution, query authorization,
replacement, and snapshot restoration against the built module artifacts.
