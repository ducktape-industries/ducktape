# Operator scripts

Repo-side helpers for running, seeding, and maintaining a ducktape node. The
runnable surfaces are the node daemon (`node-bin`/`noded`), the deterministic
`simnode`, the UDP coordinator, and the native Iced desktop app (`app/`,
`cargo run -p ducktape-app`) — the scripts here drive the node side, and
`demo-seed.sh` seeds a workspace the app can then open. Most scripts back a
`make` target; see the repository `Makefile`.

## Dev and demo network

```bash
make dev         # ops/dev.sh        — the app dev loop: found "demo" anew, start its node + services + forge, run the app
make demo-seed   # ops/demo-seed.sh  — seed a solo "demo" workspace with sample data
make demo-app    # ops/demo-app.sh   — serve the user-hosted app behind its gateway route
make dev-clear   # ops/dev-clear.sh  — stop make dev's background runtime; preserve state
make demo-clear  # ops/demo-clear.sh — stop and delete the demo workspace
```

`demo-gateway.mjs` and `demo-kanban.mjs` publish the demo's gateway web-app
routes (a network-hosted DuckFS site and a user-hosted loopback app).

## Running a node as a service

- `node/` — `ducktape-node@.service` (instance = workspace selector for
  `ducktape node run -n`), `ducktape-service@.service` (instance = kind for
  `ducktape service run compute|agent|airlock`) and the `copytruncate`
  logrotate drop-in for `daemon.log` / `<kind>.log`. `install.sh` runs the
  Linux install end to end (`--dry-run` prints it). The install, port and
  log recipe is `docs/deploy/node-service.md`; what to back up is
  `docs/deploy/backup-and-keys.md`.
- `node/dev.ducktape.node.plist` + `node/install-macos.sh` — the macOS half:
  a per-user LaunchAgent template and the script that renders it for one
  workspace and hands it to `launchctl bootstrap gui/$(id -u)`
  (`--dry-run` prints the rendered plist, `--uninstall` boots it out).

## Sandbox (microVM) hosts

- `build-guest-rootfs.sh` — builds one workspace's kernel and rootfs
  (`OUT=<workspace>/guest`) for Firecracker (Linux) or vz (macOS). Linux
  installs the pinned Rust and the `wasm-tools` CLI of the componentizer's
  release through `guest-rust-tools.sh` by
  default; `ROOTFS_SETUP` selects a custom setup.
- `macos-preflight.sh` — checks a macOS host for everything the vz backend
  needs (Hypervisor.framework, the CLT, `e2fsprogs`/`squashfs`/`zstd`, the musl
  target, the entitled `bin/duck-vz-shim` on PATH)
  and reports release-signing readiness — the Developer ID identities in the
  keychain and the `ICE_NOTARY_*` variables — informationally, since a local
  build needs neither. `--prompt` (what `make dev` passes) offers to run the
  fixes it can.
- `firecracker/` — `boot-bench.sh` and `snapshot-bench.sh`, the cold-boot and
  snapshot-restore timing lanes for the microVM sandbox.

## Forge

- `dogfood-forge.sh` (`make dogfood-forge`) — mirror GitHub `origin/dev` into
  the local node's Forge `dev` without moving release-only `main`; needs a
  running node.

## Node operator CLI

- `agent-system` — a compact operator CLI over a running node's module surface
  (raw query/submit, agent list/pause/resume); takes the node from
  `DUCKTAPE_NODE` (the same variable the `ducktape` CLI, the app, and every run
  read), else the url `use` remembered (`$XDG_STATE_HOME/ducktape/agent-system-url`,
  `~/.local/state` by default), else the one workspace under the ducktape home
  — `$DUCKTAPE_HOME` when set, else `~/.ducktape`. It talks to a loopback node only, so a
  `DUCKTAPE_NODE` pointing at a remote one is refused by name rather than
  silently ignored; `use`, `help` and `cgroup` need no node and never read it.
- `completions/` — shell completions for the `ducktape` CLI.

## Networking and media harnesses

- `coordinator/` — systemd unit, env example, and Dockerfile for the UDP
  coordinator (see `coordinator/README.md`).
- `wg-smoke/` — WireGuard interop and bench harnesses (the `wg_interop`
  probe binary in two rootless podman containers — podman is only this
  harness's container runtime; the node itself has no container sandbox. No
  node.toml involved).
- `huddle-lane.sh` — two real nodes in the dev shape with userspace
  WireGuard between them, one channel, one user key per side: the live
  arrangement a huddle (voice/camera/screen share) actually breaks in.

## Dedicated Proxmox view lane

`proxmox-view-lane.py` uses the existing Proxmox SSH tools and node CLI; it
requires Python 3.11+ on a POSIX workstation. A local record lock refuses
concurrent operations on the same lane. Its record lives on the operator workstation, outside
container data. The record holds the exact host, randomly named lane, runtime
container IDs, release revisions and file hashes. It never adopts an existing
container. A partial failed provision remains recorded for manual inspection;
re-running provision with that record is refused.

```sh
# Inspect live VM/CT allocation and storage before selecting these resource names.
python3 ops/proxmox-view-lane.py --record "$LANE_RECORD" inventory
# TEMPLATE, STORAGE and BRIDGE come from that inventory / Proxmox configuration.
# SSH_PUBLIC_KEY is a public key whose private half the operator already owns.
python3 ops/proxmox-view-lane.py --record "$LANE_RECORD" provision \
  --template "$TEMPLATE" --storage "$STORAGE" --bridge "$BRIDGE" \
  --ssh-key "$SSH_PUBLIC_KEY"
python3 ops/proxmox-view-lane.py --record "$LANE_RECORD" check
python3 ops/proxmox-view-lane.py --record "$LANE_RECORD" rollout \
  --binary "$NODE_BINARY" --modules "$MODULES_DIR" \
  --revision "$REPOSITORY_SHA" --ui-revision "$UI_SHA" \
  --reason "integrated build; state layout unchanged"
```

Provision allocates three free IDs from the live cluster inventory, at or above
200, and creates unprivileged 4 GiB / 2 core / 12 GiB containers. It does not
create or alter bridges or storage. It installs/enables SSH only inside those
new containers. CT descriptions and inner owner files must both match before
any later operation. DHCP IPv4 addresses are read from the actual containers at
rollout; all three configurations use concrete WireGuard addresses, loopback
RPC/HTTP and no public coordinator. A DHCP address change requires another
rollout to regenerate the peer configuration.

Rollout packages the executable and runtime module directory separately, hashes
every file, stages/checks all three copies and executes each staged binary's
`--version` before stopping services, then writes
all configurations before starting any service. It records the caller-supplied
source revisions and actual byte hashes; it does not infer build provenance.
`started_unverified` in `<record>.events.jsonl` means services were started,
not that consensus or views were verified. A failed stage leaves running
services alone; a failure after stopping services remains visible in
`pending_release` and requires operator repair. Three validators need all three
online for consensus progress.

The dev configuration recomputes genesis from founding files on every boot.
A changed `modules/` hash set is therefore refused while an existing release
or pending release record remains; complete `reset-network` before rolling out
those files. A partial start can initialize a node before rollout fails, so both
records constrain retries. A live
view replacement uses the module ceremony, not a changed founding directory.

For a breaking schema/ABI/state change, archive diagnostics and run
`reset-network --reason "<specific breaking change>"`, then repeat rollout.
Reset stops all three owned services, then renames each fixed `network`
directory to a unique sibling `network-archive-<timestamp>-<uuid>` under
`/var/lib/ducktape-view-lane`. It never deletes network state, release files,
owner markers or CTs. Before stopping services it saves an exclusive
`<record>.before-reset-<token>` copy; the journal records that backup, the remote
archive path and each verified node rename. Only three successful renames clear
the active release metadata. On partial failure the old record remains intact.

To roll back, keep all three services stopped, preserve any new network directory,
and rename each recorded archive back to `network` without overwriting another
directory. Restore the saved record and its release symlink on each node before
starting services together. Do not mix nodes from different reset attempts.

Use ordinary SSH forwards through the Proxmox host to each recorded CT's
`127.0.0.1:8844`, with the public key installed at provision. Keep host-key
verification enabled (the CT's host public key can be read via `pct exec`).
Pass the three resulting loopback URLs to the Node 22+ observer:

```sh
node ops/proxmox-view-observe.mjs "$NODE_A_WS" "$NODE_B_WS" "$NODE_C_WS"
```

The URLs must end in `/v1/ws`. The observer requires a new committed height
beyond all three starting heights and an identical root at that height. Repeated
unchanged heartbeats do not pass; mismatched roots, regressing heights, socket
failure or the 180-second deadline fail. Keep its JSON result with the rollout
journal. This proves consensus progress/root agreement, not view activation or
rendering. View replacement/removal still uses the existing `module update`
ceremony, verified active artifact hashes on every node, and the actual app
host's rendering/asset/removal checks.

### Local three-process canary

`proxmox-view-local.py` runs the same three-node configuration on loopback when
remote access is unavailable. It is a local rehearsal, not a Proxmox result.
It copies the binary and all staged modules into a new task-owned directory
under `target`, records their SHA-256 values and the supplied exact source/UI
revisions in `owner.json`, and allocates fresh loopback ports. The binary's
version must match the source revision. Build the input files from the stated
revisions; the supervisor cannot infer the compiler provenance of view bytes.

```sh
python3 ops/proxmox-view-local.py --binary "$NODE_BINARY" --modules "$MODULES" \
  --revision "$SOURCE_REV" --ui-revision "$UI_REV" --seconds 7200
```

Keep this command in the foreground. It prints the record path and three HTTP
URLs, then reports `ready` only after the module rosters agree and all three
nodes demonstrate an advancing common committed root. `owner.json` records
configs, ports and child PIDs for operator commands. Interrupting the supervisor
or reaching the time limit stops only its own children and records their exit
codes; it never scans for or kills other node processes. Each restart creates
new network data. SSH forwarding and the Mac app remain separate operator
steps; no socket binds to a public interface.

The generated network uses a 500 ms idle block cadence. `--after 600` in a
module ceremony means 600 committed blocks after governance executes it,
nominally five idle minutes, not 600 seconds or a wall-clock guarantee. Allow
additional time for voting, artifact fan-out and the client observations.
When a minimum five-minute separation is required, wait at least 300 measured
seconds after the preceding phase is fully verified before proposing the next
phase, and retain its block lead. Record both timestamps and activation heights.

### Preparing the actual view ceremony

After the aggregate UI pin and view/assets staging CLI are integrated, prepare
views before rebuilding the node in the same checkout:

```sh
CARGO_TARGET_DIR="$PWD/target" make views
CARGO_TARGET_DIR="$PWD/target" cargo build --locked -p node-bin --bin ducktape
```

`make views` writes all eleven guests to the checkout's `target/views` even
when the Cargo build target is elsewhere. The subsequent noded build stages
`governance`, `files`, `pages`, `chat`, and `forge` views into the node profile's
`modules` directory. Require all five `<id>.view.wasm` files and no
`<id>.view.pending` markers before founding or rollout. Preserve
`pages.index.wasm` and `chat.index.wasm` in every ceremony; the other three
owners have no mapper. Pass `--assets` only for an existing asset directory.
Do not edit founding files to perform a live swap.

Governance's header surface calls `artifact_svg("icons/seal.svg")`. It resolves
that exact canonical path in the current verified deployment's assets; a
missing entry leaves an empty slot and logs `reason=asset_missing`. The two
static canary asset roots are `ops/proxmox-view-assets/a` and
`ops/proxmox-view-assets/b`. Both contain `icons/seal.svg`, a 24×24 seal with
identical geometry and a white check. Only its fill differs:

| Root | Fill | SHA-256 of `icons/seal.svg` |
| --- | --- | --- |
| `a` | blue `#2563eb` | `4006efe23a11bf16828074bc23920a185e984258ac3276cd8fedf15a9950df21` |
| `b` | orange `#f97316` | `7985e6eebe72292a8cf3c50d3f9c9a47725745cd98df27848cbb75a91168d216` |

These are asset-byte hashes, not deployment hashes. Once the real consensus
component and view bytes are available, package each exact combination and
record the whole artifact hash printed by `module pack`. For governance's
asset-only comparison, keep `COMPONENT` and `VIEW` identical:

```sh
"$NODE_BINARY" module pack "$COMPONENT" --view "$VIEW" \
  --assets ops/proxmox-view-assets/a --out "$ARTIFACT_A"
"$NODE_BINARY" module pack "$COMPONENT" --view "$VIEW" \
  --assets ops/proxmox-view-assets/b --out "$ARTIFACT_B"
```

Run the matching `module update governance COMPONENT --view VIEW --assets ROOT
--after 600 --config NODE_CONFIG` on the dedicated validators with the same
inputs and lead. The CLI proposes/votes before staging; a successful ballot
or blob receipt is not activation. Require all three validators' committed
active hash and activation history to agree, plus an advancing common root.
Record view A→B with the same asset root first, then hold view B fixed while
changing the asset root from `a` to `b`. Removal omits **both** `--view` and
`--assets` while retaining the backend and any mapper. Removing just the seal
asset is a separate `asset_missing` check, not view removal. No artifact hash
or successful activation is implied by the static fixtures in this directory.

The Mac client check is coordinated separately with the app owner. Use fresh
preferences because an explicit saved endpoint takes priority over the
environment. Its staged view directory must contain only the six globals
`agents`, `explorer`, `members`, `node`, `settings`, and `shell` (`*_view.wasm`),
with the five owner views absent:

```sh
DUCKTAPE_HOME="$MAC_SCRATCH" DUCKTAPE_NODE="$NODE_A_HTTP" \
DUCKTAPE_VIEWS_DIR="$GLOBALS_ONLY" \
  "$APP_BUNDLE/Contents/MacOS/ducktape-app"
```

Read-only checks need no `DUCKTAPE_USER_KEY` or server operator credential.
The app must run RPC-only, without a local node. Hand off three forwarded HTTP
URLs, the five owners' actual full artifact hashes and activation heights,
`icons/seal.svg`, and the removal module. Capture module/hash/state/generation
logs and actual PNGs for view A, view B, asset color change, missing asset, and
verified view removal. Static readiness/ABI validation and the local
three-process consensus smoke do not prove Proxmox deployment or Mac rendering.
A Linux headless app capture can verify the real guest/host loading and rendered
pixels, but is separate from a physical Mac app/window test. Label the client
platform in evidence and keep any unexecuted platform check explicit. Confirm
that each PNG depicts its reported module and visible marker after layout and
redraw; a correct swap log beside a stale capture is not visual swap evidence.

Offline command/ownership and heartbeat checks (no SSH/Proxmox access):

```sh
python3 -B ops/proxmox-view-lane-test.py
node ops/proxmox-view-observe-test.mjs
```

## Hosted auth page

- `auth-page/` — the `auth.ducktape.industries` WebAuthn relying-party page
  (`index.html`), its result-relay Worker (`worker.js`, `wrangler.toml`) and
  the dependency-free gate `node ops/auth-page/test.mjs`; see its README.

## Wasm guests

- `wasm-repro-check.sh` (`make wasm-repro-check`) — builds one guest component
  twice, in two scratch directories, and asserts the bytes are identical and
  carry no host path, so a committed artifact never depends on the builder's
  `/home/...`. Needs the wasm32 target and a pushed HEAD.
- `make wasm-rebuild-check` — the other reproducibility gate, a Makefile target
  with no script here: it rebuilds every committed guest (each `component.wasm`
  and `index.wasm`) out of the repository at HEAD, seeded from its committed
  `guest.lock`, and cmps it against the bytes in the tree. Same needs.
- `make audit` — the third out-of-band gate: `cargo deny check advisories` over
  the committed `Cargo.lock`, under the repo-root `deny.toml` where every
  carried advisory names why it is carried and what clears it. Needs `cargo
  deny` and network, so it is not in the offline `test` gate; run it when the
  lock moves.

## Worktree cleanup

Always dry-run `ops/worktree-clean.sh` before removing merged worktrees, then
pass `--yes`. It refuses a worktree that is dirty, carries a commit not in
`dev`, or has live processes under it, and it finds those processes by cwd —
never `pkill -f`.
