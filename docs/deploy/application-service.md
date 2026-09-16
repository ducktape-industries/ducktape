# Independently installed Gateway applications

`ops/application-service/install.py` installs an executable as an isolated Linux
systemd service. It uses the existing Gateway `(account, label)` HTTP/WebSocket
route and does not add a service ID to the node. The operator supplies a pinned
SHA-256 digest; the installer verifies the copied executable before changing an
existing installation. Application configuration is JSON owned by the application.

## Install and activate

The node runs as an unprivileged Unix user. The workspace must belong to that
user. Install the `ducktape` CLI at `/usr/local/bin/ducktape`, or pass its absolute
path using the installer's `--ducktape` option. Python 3.11+, systemd, and `runuser`
are required.

Create a manifest with exactly these fields (replace the digest and local paths):

```json
{
  "name": "team-app",
  "binary": "/srv/releases/team-app",
  "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
  "workspace": "/home/duck/.ducktape/team",
  "node_user": "duck",
  "account": 12,
  "label": "team-app",
  "port": 29134,
  "config": { "node_url": "http://127.0.0.1:3000" },
  "memory_max": 268435456,
  "cpu_quota": 100,
  "tasks_max": 64,
  "readonly_paths": [],
  "devices": []
}
```

The name and label are lowercase letters, digits, and hyphens, beginning with a
letter. The port is 1024–65535. Memory is bytes; CPU is a percentage of one CPU.
The digest must come from the release you trust; hashing an untrusted download
alone does not establish who published it. The application interprets `config`.

```sh
sudo python3 ops/application-service/install.py install /srv/releases/team-app.json
sudo python3 ops/application-service/install.py activate team-app
systemctl status ducktape-application-team-app.socket ducktape-application-team-app.service
journalctl -u ducktape-application-team-app.service
```

The signed Gateway route must independently select this publisher node and grant
the intended audience, methods, and WebSocket policy. Installation binds the
local port for the exact account and label; it does not grant network authority or
publish a signed route. Missing bindings and unauthorized routes are refused by
the Gateway before an application request is sent.

The node exports `ducktape_application_binding{account,route,authentication}` for
current local bindings. This gauge describes local admission, not process health
or signed route authorization; stopping an application removes its series.
Gateway metrics describe aggregate transport, and systemd cgroups expose each
process's resource usage. Metric labels contain neither credentials nor paths.

## Application process contract

The process receives one systemd socket: `LISTEN_FDS=1`, `LISTEN_PID` equals its own
PID, descriptor 3 is a TCP listener on `127.0.0.1`. Validate these values and use
the inherited listener. Do not bind another listener. Systemd keeps its copy open
across process restarts, so another local process cannot take that port.

The service unit uses `Type=notify` with `TimeoutStartSec=30`. After validating
configuration, credentials, storage, and the inherited listener, send `READY=1`
to systemd's `NOTIFY_SOCKET` using a Unix datagram (support its `@` abstract
address form). Activation and restart bind ingress only after this notification.
A startup failure or readiness timeout stops the process and leaves its route
unbound.

Read these files from `CREDENTIALS_DIRECTORY`:

- `upstream-token`: exactly 64 lowercase hexadecimal bytes, without a newline.
- `application.json`: the manifest's `config` object.

For **every HTTP request and WebSocket handshake**, compare the
`x-duck-upstream-token` header with the credential in constant time. Refuse a
missing or incorrect token before reading any caller metadata or upgrading the
socket. The Gateway strips caller-supplied `x-duck-*` headers and injects its own:

- `x-duck-caller-account`: the account established by the request's user proof;
  absent when the route permits an anonymous peer.
- `x-duck-caller-node`: the authenticated source peer's public key in hexadecimal.
- `x-duck-caller-operator: true`: the calling node verified its existing operator
  token or configured operator-key signature through `/v1/gateway/operator`.
  POST carries a buffered HTTP exchange. GET upgrades to a WebSocket; its `head`
  query parameter contains the JSON request head, so the data-plane signature
  covers the destination and cursor through the complete path and query.
  This asserts authority on the calling node, not on the service's host. The
  service still applies its existing local/peer admission. The node never
  forwards the operator credential itself. Ordinary proxy and view-stream
  requests cannot supply this assertion.
- `x-duck-route-account`, `x-duck-route-label`, `x-duck-route-revision`: the route
  whose current signed policy admitted the request.

An application requiring a user must reject an absent caller account. These
headers attest identity and Gateway admission; application-specific authorization
still belongs to the application. Query finalized module state through the node's
read API to decide room membership or other domain policy. Recheck authorization
for long-lived sessions when that policy can change. The process receives no
node admin token and must never ask for one.

The credential authenticates a node handoff within one machine, not an arbitrary
external caller. Systemd `DynamicUser` separates each application's identity from
the node and other applications. The original credential is readable only by the
node's Unix user; systemd gives the application a private copy. The node user and
root remain trusted. Do not share their Unix identity with untrusted processes or
expose their private files through permissive ACLs. Token contents must never be
logged or returned in responses.

## Stop, restart, replace

```sh
sudo python3 ops/application-service/install.py restart team-app
sudo python3 ops/application-service/install.py stop team-app
sudo python3 ops/application-service/install.py install /srv/releases/team-app-next.json
sudo python3 ops/application-service/install.py activate team-app
```

Restart withdraws the local route, replaces the process while retaining the
systemd listener, then rebinds the route. Stop withdraws only this account's label
before stopping the socket and process, and deletes the credential. Activation
uses a fresh credential and binds only after the socket and service start.
A bare local binding withdrawal or replacement also revokes existing WebSockets
at the Gateway's 30-second authorization check plus its bounded query deadline.
Installing a replacement verifies its bytes first, then stops the previous
installation and writes the new units and configuration. Replacement has a brief
outage; existing WebSockets close when their process stops. Invalid artifact
hashes leave the existing installation running.

Executables live under `/usr/local/lib/ducktape-applications/<name>/<sha256>/service`;
root-owned installation metadata and credentials live under
`/var/lib/ducktape-applications/<name>`. Old executables remain available for an
operator-selected reinstall. Each installation reserves its route and port. The installer refuses duplicate
reservations and serializes changes with a file lock.
Systemd owns process restart and resource limits; no second supervisor runs.

For explicitly trusted native development processes, `ducktape gateway bind`
requires `--trusted-loopback`. An isolated installed application uses
`--credential-file` instead. The route configuration has no implicit trust mode.

## Checks

```sh
python3 -m unittest discover -s ops/application-service -v
cargo test -p node-bin --bin ducktape gateway_routes::tests
cargo test -p node-bin --bin ducktape gateway_plane::tests
```

The Python checks exercise verified install/replacement and route ordering with
systemd commands recorded, and real inherited-listener child processes with
spoofed credentials, missing callers, and process replacement. They do not install
units on the test host. The Gateway tests exercise real loopback HTTP and
WebSocket forwarding, caller proofs, route audience checks, and private
credential files.

The terminal executable's production startup check uses real sandbox prerequisites:

```sh
DUCK_TERMINAL_GUEST_DIR=/srv/terminal/guest \
DUCK_TERMINAL_EXECUTORS=/srv/terminal/executors \
cargo test -p ducktape-terminal --test activation production_process -- --ignored
```

It requires Firecracker and its host tools on the executable search path, access
to `/dev/kvm`, `vmlinux` and `rootfs.ext4` in the guest directory, and an installed
executor. It runs the actual terminal executable through inherited-listener
activation, readiness notification, token replacement, and signal shutdown.
It does not boot an interactive guest session or install systemd units.

The terminal service accepts `POST /sessions` with `agent` and optional `cpu`
and `mem_gb` fields. The request must carry a Gateway-attested operator whose
calling node equals the service's configured `identity`. It creates a local
single-user terminal and returns `session_id` after the executor starts. The
same node operator owns attachment input, resize, and close commands. Provider
startup failures return an error instead of a session receipt.

Terminal attachments use `/sessions/{session}?after=<output-seq>&after_command=<chat-seq>`.
Both cursors default to zero. Replay metadata gives `first`, `head`,
`command_first`, and `command_head`; following `command` frames carry `seq`,
`origin`, and `text`, while `output` frames carry `seq` and `data_b64`.
Resume from frames actually consumed, not a snapshot head announced before those
frames. Command sequence gaps include refused or deleted Chat posts. Command
history records accepted execution requests, not proof that the program executed
them. Output and command history remain in process memory without a configured
retention ceiling; process restart does not restore terminal sessions. Each
attachment queues output independently, so a slow reader does not block its
input or close commands. There is no configured attachment count, WebSocket
frame size, output queue, or send deadline ceiling.

An application may bind up to 16 explicit read-only directories through
`readonly_paths`, for example
`[{"source":"/srv/tenant/forge","destination":"/var/lib/application-storage/git"}]`.
Each source must be an exact absolute existing directory without symlinks; each
destination must be one named directory under `/var/lib/application-storage/`.
The installer mounts only those directories read-only. Their directories must
permit read/traversal and their files read access by the isolated process; the
parent workspace may remain private. No node key or administrator credential is
mounted. Applications needing no node storage use an empty list.

Every process receives a private writable `StateDirectory` owned by its isolated
systemd identity. Use the absolute `STATE_DIRECTORY` environment variable for
session data; it persists across restarts. No node workspace is writable.

The required `devices` list is empty unless the application needs explicit host
devices. Up to eight distinct, existing character devices may be granted, for
example `["/dev/kvm"]`; symlinks and non-device files are rejected. The unit
retains `PrivateDevices=yes`, uses `DevicePolicy=closed`, and binds only each
listed device with read/write access. For a group-owned device such as `/dev/kvm`,
the process receives that device's non-root group as a supplementary group;
the device must grant that group read/write access. Device grants are operator deployment
permissions and must be reviewed with the executable and configuration.

## Git service

Build the independently installed executable with
`cargo build --release -p ducktape-forge-service`. Its configuration fields are:

```json
{
  "node_url": "http://127.0.0.1:3000",
  "node_key": "<64 lowercase hex node public key>",
  "chain_id": "<chain ID from /v1/status>",
  "account": 12,
  "label": "git",
  "module": "forge",
  "git_store": "/var/lib/application-storage/git",
  "signing_seed": "<64 lowercase hex private seed belonging to this service>"
}
```

Bind only the configured module's tenant Git directory using `readonly_paths`.
The service reads its materialized committed refs and objects there. It sends
queries and signed module frames through the generic node API and uploads packs
to the generic blob CAS. Its own signing key authenticates transport; every ref
update additionally requires a user's Git SSH push certificate, verified by the
service and again by the module. Node administrator tokens do not authorize an
unsigned service push.

A submission that needs an uploaded pack declares its digest in the signed
frame's generic `required_blob` field. The node transports that blob before
relaying the operation; it does not decode the product payload to discover it.
Operator clients use `required_blob` (64 lowercase hexadecimal characters) in
`POST /v1/submit` JSON, or the same query parameter on
`POST /v1/submit/raw/{target}`. Upload the blob to the receiving node first;
a declared blob absent from its verified store is refused.

The service exposes `/{repo}/info/refs`, `/{repo}/git-receive-pack`, and
`/{repo}/git-upload-pack`. Configure the signed Gateway route to allow GET/POST
for the intended network audience. Stock Git can supply the route authority
through its HTTP header configuration when dialing the node's browser Gateway:

```sh
git -c http.extraHeader='x-duck-authority: git.team.duck' \
  clone http://127.0.0.1:3001/project
git -c http.extraHeader='x-duck-authority: git.team.duck' \
  -c gpg.format=ssh -c user.signingkey="$HOME/.ssh/id_ed25519" \
  push --signed http://127.0.0.1:3001/project HEAD:main
```

Use the actual browser Gateway port and registered authority. The application
handoff token is private to the node and service; Git clients never receive it.
Two concurrent requests are admitted per service process. Pack uploads are
bounded by the common relay transfer ceiling (127 chunks of 768 KiB); larger
histories must be imported in smaller advances. Set the manifest memory limit
to accommodate pack construction and upload within that ceiling.

## Media service

Build the process with `cargo build --release -p ducktape-media` and the
companion view with `bash ops/build-views.sh -p call-view`. Deploy
`target/views/call_view.wasm` as the registry's `call` view. The desktop loads
that artifact at runtime; its session owns call framing, mute/source controls,
speaking state, and bounded audio playout. A separate instance of the same view
renders the call window from panel properties without opening devices or another
transport session. It owns the video grid, participant captions, invite chips,
and control labels. Native code owns the microphone, speaker, camera/screen
capture, JPEG rendering resources, and window/device control effects. Live image
references resolve the latest host frame at paint time.

Use an installation manifest with `label: "media"`, `readonly_paths: []`,
`devices: []`, and:

```json
{
  "node_url": "http://127.0.0.1:3000",
  "account": 12,
  "label": "media"
}
```

The account must own the Chat channels served by this route. Publish its signed
Gateway route with GET and WebSocket upgrades allowed for the intended audience.
The service additionally requires an authenticated caller account and verifies
that the channel's committed huddle roster names that account at the attested
source node. It subscribes before reading the roster, pauses forwarding while
refreshing it, and ends a session if the account is removed or canonical state
becomes unavailable. Each room admits one session per account and source node.
The service has no configured session-count, room-size, or output-queue ceiling.

Media uses reliable WebSockets. Packet loss can delay later audio/video behind
an earlier frame; it does not retain the independent datagram queues of an
unreliable media transport. Per-client output queues retain pending frames;
slow readers can accumulate queued media. Service replacement
closes existing calls, and a view replacement or network switch releases its
native device resources. Opening another tab keeps a user-started session alive.

```sh
cargo test -p ducktape-media
cargo test --manifest-path crates/views/Cargo.toml -p call-view
```

These checks cover authenticated fanout, membership withdrawal, source metadata,
queued delivery, actual executable socket activation and replacement, and the
guest's host requests for audio, mute, video, images, and playout. Physical device
permission and microphone/camera quality require a desktop with those devices.
These automated checks do not establish equivalent media quality, end-to-end
latency, loss recovery, or jitter behavior on real networks.

### Forge merge requests

The Forge view reads `service.json` from its own verified deployment assets:

```json
{"account":12,"route":"git"}
```

Include this file in the asset directory passed to `module update forge
COMPONENT --view VIEW --assets ROOT --after HEIGHT --config NODE_CONFIG`.
The account and route must match the installed service and signed Gateway route.
Missing configuration refuses merge. The view sends `POST /merge` with the repo,
exact source and target commit IDs, and commit message. The service computes the
merge against its read-only tenant store and returns the merge ID and base64
pack bytes (or conflict paths). The view uploads the pack through `blob.put` to
its submission node, then submits `MergePr` with that digest under the seated
user's authority; the module
checks both expected branch heads before committing it.
