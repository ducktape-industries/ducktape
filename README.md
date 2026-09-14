# Ducktape

A consensus-based workplace super-app: one BFT-replicated state machine that
hosts isolated product modules — pages, forge, chat, agent workflows — the
way CosmWasm isolates contracts, but in native Rust.

Each module owns its authenticated state substrate and exposes exactly one
32-byte root to the host. The host dispatches modules over BFT consensus and
composes the sorted module roots into a global root-hash that consensus commits.
If two nodes agree on the root-hash, they agree on every module's state.

## The Module Rule

Modules never link each other's implementation crates. A module depends only on:

- `sdk` — the module contract and deterministic system API, and
- the types-only wire types (payload/query/reply shapes + codecs) each module
  publishes at its own crate root, for the modules it needs to address.

Cross-module reads go through host-routed queries. Cross-module writes are
emitted as messages that the host drains as follow-up ops.

## Repository Layout

The tree groups by function into three layers — module / kernel / networking:

| Path | Contents |
| --- | --- |
| `crates/kernel/` | The platform: `sdk` (module contract + codec), `sdk-testkit` (dev-only `TestCtx`/`MemStore` doubles for the sdk traits), `host` (submit/execute loop, root-hash composition, the `host::worker` non-deterministic-effect seam), `node` (ordered replication), `consensus` (commonware Simplex BFT orderer), `statesync`, `recovery`, `indexer` (derived read-model tier), `index-guest` (the index-mapper guest contract the indexer and per-module mappers share), `blobstore` (node-local op-receipt byte store — like `indexer`, never in any root), `wasm-host` (pinned wasmtime runtime for hot-swappable module components), `keyscheme` (the closed set of signature schemes a ducktape key can carry, and the one verifier every signed artifact dispatches through) |
| `crates/networking/` | The netstack: host-side transport infra only — `wireguard`, `nat-traversal`, `reachability`, `data-plane`, `overlay-net`, plus the sans-I/O reachability core `netstack-machine` (pure event-in/effects-out state machine), `netstack-scenarios` (its frozen golden lifecycle traces) and `netstack-wasm` (the wasmtime embedding of the `ducktape:netstack` world). Owns no consensus module |
| `crates/modules/` | **Consensus modules and nothing else** — every crate under `system/` or `apps/` implements `sdk::Module`. Module = onchain, service = offchain; a crate that holds no consensus state belongs in `kernel/`, `services/`, or beside `airlock`/`duckdns` |
| `crates/modules/system/` | System modules: `kv` (byte-KV), `valset` (ed25519 validator membership), `governance`, `identity`, `modules` (module code registry), `saga` (deterministic async continuations), `capability`, `dispatch`, `attribution` (source-owned account relations and queued notifications), `acl` (the submit-policy federation: which standing a target module requires of an external submitter), `gateway` (the merged name→AccountId→route module, which absorbed the on-chain half of `duckdns`) |
| `crates/modules/apps/` | Product modules: `forge` (git-backed project state), `pages` (documents), `chat`, `agent` (programmable users reacting to attributions), `runs` (model configuration and execution), `tasks`, `inbox` (per-account notification queues), `automations` (rules over chat hooks), `files` (consensus manifests, node-local bytes; wraps `duckfs`) |
| `crates/services/` | Off-chain service crates — the host-side executors that serve a consensus module without being one: `compute` (the dispatch WorkSpec pool/ledger/gate), `provider` (executor spec layer + the `CliProvider` run loop), `sandbox` (the per-run microVM, egress firewall, backend probe), `broker` (run-scoped credential loopback + airlock client), `agent` (interactive pty daemon), `airlock` (the credential-LENDING gateway: node-local store + router, no TEE), `media` (the huddle voice/video/screen-share planes, off consensus, riding the overlay) |
| `crates/airlock/` | The two-party execution/auth contract (`client`/`server`/`verify`/`testkit` features). Not a module and not one party's crate: the lender service, the borrower broker and the enclave binary all consume it |
| `crates/duckdns/` | The `.duck` account naming library — hostname grammar, handle registry, wire types, canonical state codec. Not a module: the `gateway` module embeds it for the on-chain half, and `node`/`noded`/`simnode` validate names host-side |
| `crates/duckfs/` | The versioned-filesystem engine: `core` (pure, wasm-ready — the `files` module wraps it), `disk`, `client` (OS-side) |
| `crates/noded/`, `crates/rpc-client/`, `crates/workspace-config/` | The node's host-side libraries: `noded` (the embedded host behind http/ws — status cell, log ring, service catalog, projection), `rpc-client` (bounded async client for the public `/v1` surface), `workspace-config` (node.toml/network.toml shapes, `DUCKTAPE_HOME`, identity files, invites) |
| `crates/keystore/`, `crates/authpage/`, `crates/run-envelope/` | `keystore` (the device keystore: named encrypted user keys + the `active` wallet pointer), `authpage` (the client half of the `auth.ducktape.industries` WebAuthn relying-party page; the page itself is `ops/auth-page/`), `run-envelope` (the run payload's magic and the headless composer that stamps it) |
| `crates/topology/` | ONE source for the module id universe (each id and where its code comes from) and the named genesis selections (`PRODUCTION`, `SIM_BASE`, `SIM_VALSET`) every composer draws from; what a module needs from the host is its own component's `shape` export |
| `crates/module-sdk/` | The module SDK, the one crate a wasm module pins by git revision: the `ducktape:module` WIT world, the bindings and adapter every port shares, `sdk` re-exported, and the wasm32 patch crates (`stubs/`) a guest graph needs |
| `crates/guests/` | The kernel-fixture test guests only (hello, hello-replacement, noop, sibling, object). Every module carries its own port (`src/guest.rs` behind the `guest` feature) and `bin/guest-builder` builds it out of the repository at a revision — no per-module crate lives here |
| `crates/examples/` | Reference modules: `directory` (the first wasm port; a test tenant, in no genesis set), `greeter` (types-only composition example) |
| `crates/testing/` | `nettest` — the raw-HTTP-over-TCP test client, collision-safe port allocation and coarse event poll every node/daemon/sim integration harness shares |
| `crates/views/support/design/` | The desktop app's font identity and type scale (shared tokens come from `ducktape-ui`) |
| `crates/labs/` | Quarantined experimental modules (`evm`, `multisig`): in-tree and tested but registered by NO genesis set, kept as a standalone crate EXCLUDED from the workspace so its heavy deps (revm, alloy) never tax the shipping build — gated via `make labs-gate` |
| `bin/` | Runnable binaries: `node` (the unified `ducktape` CLI: `node run` plus every operator family — `node`, `user`, `account`, `wallet`, `gateway`, `fs`, `service`, `agent`, `module`, `mcp`), `noded` (`noded-bin`: the throwaway dev daemon with temp storage), `simnode` (deterministic /v1 twin), `coordinator` (STUN rendezvous + the TCP first-contact relay), `airlock-gateway` (the TEE enclave lender; the non-TEE lender is `ducktape service run airlock`), `guest-builder` (module → wasm component packaging tool), `duck-guest-init` (PID 1 inside a run's microVM), `duck-vz-shim` (the macOS Virtualization.framework VMM shim, Swift) |
| `app/` | `ducktape-app`, the native GPUI desktop host; Rust-authored WASM views live in `crates/views`, with product assets in `crates/views/support/design` |
| `ops/` | Operator scripts, the node and coordinator systemd units, the sandbox guest image builder, the hosted auth page — see `ops/README.md` |
| `docs/` | Operator runbooks (`deploy/`, `dogfood.md`, `sandbox-macos.md`) and the few records code cites by path (`records/`); `docs/README.md` indexes every document in the repo by the question it answers |

Each module publishes its wire surface — types-only payload/query/reply shapes
and codecs — at its own crate root; those wire types plus host-routed queries
are the only legal cross-module surface. `kv` remains as a crate but is no
longer registered in the production genesis module set.

### Layer contracts

Every layer boundary is a small trait, and each obeys the same three rules: the
contract lives at its crate root (opening the crate shows it first); every trait
ships a sim/test arm in the same crate — behind feature `sim` where the double
carries a build cost; and this table is the map from each boundary to its real
and swappable arms. The last four rows are not swappable-arm traits but single
shared paths that replaced the old validator/noded/simnode triplication (block
projection, worker reactor, genesis topology) plus the scripted-stepping
ordering arm.

| Contract (trait · crate) | Real arm(s) | Sim / test arm | Consumers |
| --- | --- | --- | --- |
| `Orderer` · `crates/kernel/node` | `SimplexOrderer`, `FollowerOrderer` | `RoundOrderer`, `ArrivalOrderer` | node replication loop, bin/node validator engine |
| `sdk::Module` / `sdk::Ctx` / `host::ModuleFactory` · `sdk`, `crates/kernel/host` | native module, `WasmModule` | dozens of in-crate test modules | host execute/dispatch loop |
| `host::worker::Worker` · `crates/kernel/host` | `DispatchPool` | `MockOracle`, `FlakyOracle`, `EchoWorker` | host non-deterministic-effect dispatch |
| `SyncClient` · `crates/kernel/statesync` | four fetch clients | `ChannelClient`, `StoreClient`, `LiarClient` | statesync joiner / backfill engine |
| `DataPlaneTransport` · `crates/networking/data-plane` | `OverlaySockets` | `SimEndpoint` (feature `sim`) | overlay demux + acceptor loops |
| `WireGuardEffect` · `crates/networking/wireguard` | userspace | `FakeWireGuardEffect` | mesh bring-up (bin/node boot) |
| commonware runtime `E` (`Clock` / `Storage` / `Rng`) | `tokio::Context` | `deterministic::Runner` | host, node, statesync |
| `ObjectStore` · `crates/duckfs/core` | `DiskStore` | `MemStore` | `files` module, duckfs client |
| `Blobs` · `blobstore` | `BlobHandle` (disk) | `MemBlobs` | bin/node blob_fetch/relay_runtime/explorer, statesync serve |
| `RefsStore` · `crates/duckfs/core` | `DiskRefs` | `MemRefs` | `files` module (`Files<S, R>`) |
| `MeshCarrier` · `crates/kernel/consensus` | `DiscoveryMesh` (wraps the `authenticated::discovery` Network) | `SimMesh` (feature `sim`, wraps `simulated::Network`) | bin/node validator engine, in-process cluster test |
| commonware `Clock` seam (`context.current()`) + source-parsing lint · bin/node, statesync | `tokio::Context` | `deterministic::Runner` | validator run/drain/ingress, statesync monitor |
| `TestCtx` (`sdk::Ctx`) + `MemStore` (`sdk::MerkleStore`) · `crates/kernel/sdk-testkit` | host runtime `Ctx`, `QmdbStore` | `TestCtx`, `MemStore` | module unit tests (runs, automations, files, governance, …) |
| `projection::project_block` · `noded` | one shared block-projection path (RootOp assembly + `block_row` bytes + index feed + stream publish) | golden test pins `block_row` bytes | validator drain, replica park, noded submit lane, simnode — a rejected op journals a block, validator parity |
| `Orderer` — scripted-stepping seam · `crates/kernel/node` | — (sim-only arm) | `StepOrderer` + `StepHandle` (FIFO; release-one / release-all) | simnode actor (`OrderedNode<StepOrderer>`) |
| `worker::drive` · `crates/kernel/host` | one shared reactor loop (offer events, budget rounds, follow-up `Msg`s + Nudge tail) | unit test on budget / Nudge behavior | validator drain, noded submit lane, simnode auto mode |
| `ModuleTopology` · `crates/topology` | one module id universe and its named selections (`PRODUCTION` / `SIM_BASE` / `SIM_VALSET`), composed through `noded::compose` | `genesis_registry_matches_production` + the topology's own selection pins | `bin/node` (`PRODUCTION`), noded and simnode (`SIM_BASE`, plus `SIM_VALSET` under `--with-valset`) |

## Quick Start

Prerequisites: `rustup` (the pinned toolchain and its wasm32 target install
themselves on the first build), a C compiler, and on Linux `pkg-config`,
libclang, ALSA, X11/XKB and font development libraries:

```sh
sudo apt install build-essential pkg-config libclang-dev libasound2-dev \
  libx11-xcb-dev libxkbcommon-dev libxkbcommon-x11-dev libfontconfig1-dev libfreetype6-dev
xcode-select --install  # macOS
```

`make` checks for them up front and prints that line when one is missing.
`wasm-tools` is pinned and installed under `target/` by the recipes that need
it. Views build with Cargo and are componentized into separate runtime files.

Run the workspace tests:

```sh
cargo test --workspace
```

Run the real-socket cluster e2e — REAL node processes over localhost TCP,
driven through the rpc: BFT convergence, a chat product loop, a governance
vote, a live epoch cutover, a crash-fault liveness check, and a sync-only
joiner rebuilding every module to the identical root-hash:

```sh
cargo test -p node-bin --test cluster_e2e
```

Run the joiner state-sync proof — a fresh joiner rebuilds every module and
lands on the source root-hash:

```sh
cargo test -p node-bin --test network_joiner_full
```

Run everything the repo can verify locally (the wasm-artifact drift gate, the
rust workspace including the e2e suites, the consensus sim-feature suite, and a
build of the noded + simnode binaries the test harnesses stage; two scripted
checks want `node` and `bun` and skip with a notice without them):

```sh
make test
```

Run and verify only the coordinator operator path:

```sh
make coordinator-smoke
make coordinator
target/release/coordinator --help
target/release/coordinator --listen 0.0.0.0:3478
```

### Build wasm module components (guest-builder)

No prerequisite beyond the toolchain: the componentizer is the `wit-component`
crate `guest-builder` links, pinned in `bin/guest-builder/Cargo.toml` like the
rust channel (a different one writes different component bytes). The wasm32
target is in `rust-toolchain.toml`, so rustup installs it with the pinned
channel.

Day to day you don't invoke the tool — `make wasm-modules` rebuilds every
module component (and refreshes the kernel test fixtures), and
`make wasm-modules-check` guards that the committed copies agree.

To build one module directly:

```sh
cargo run -p guest-builder -- crates/modules/apps/tasks
```

This builds the module ALONE, out of the platform repository at the checkout's
HEAD — push first; the tool refuses uncommitted inputs in the module, its
resolved SDK and sibling packages, and workspace build configuration. It uses
a shell workspace under `target/guest-builder/tasks/`, componentizes, and
writes the canonical
committed artifact to `crates/modules/apps/tasks/component.wasm` (path printed
on stdout) beside `guest.lock`, the record of the revision and registry
versions it came from. If kernel tests pin a fixture copy, refresh it too —
`make wasm-modules` does both. `--rev <sha>` builds another revision,
`--scratch` overrides the shell directory, and `--out <path>` writes the
artifact there instead, leaving the module directory (lock included)
untouched.

For the tool to accept a module it must declare the port contract: a
`guest = ["dep:ducktape-module-sdk"]` feature, the optional
`ducktape-module-sdk` workspace dep, and a `src/guest.rs` behind
`#[cfg(feature = "guest")]` containing either
`ducktape_module_sdk::snapshot_guest!` (whole-state modules), `store_guest!`
(store-backed), or a hand-written `Guest` impl + `export_module!` (the `files`
shape). `crates/modules/apps/tasks` is the reference; the full wiring runbook
is `skills/module-dev/SKILL.md`. A module authored in its own repository needs
no tool at all: it is a cdylib crate pinning `ducktape-module-sdk` by git
revision, built with cargo and `guest-builder componentize` — the recipe is in
`docs/records/architecture/wasm-module-authoring.md`.

## Run a node

Install the `ducktape` operator CLI into `~/.cargo/bin` (and the founding
set — every module's wasm — into `~/.cargo/bin/modules` beside it, which
`node init` composes a network's genesis from); on macOS this also builds the
native GPUI `.app`/`.dmg` and installs `Ducktape.app` into `~/Applications`:

```sh
make install
```

Then the path `ducktape --help` prints — every line runs as written on a
machine with one network on it:

```sh
ducktape node init --name mynet     # found your own network here
ducktape node join <invite>         # ...or join someone else's
ducktape node run                   # start it (^C checkpoints and exits)
ducktape wallet new <you>           # mint your user key
ducktape account create --name <you>
                                    # found your account on it (signed by that key)
ducktape node status                # height + root hash of the running node
```

`node init` composes the network's wasm — every module's
`<id>.component.wasm` and `<id>.index.wasm` — into `<workspace>/genesis` out of
the founding set `cargo build` stages beside the binary
(`target/<profile>/modules`; `--modules <dir>` or `$DUCKTAPE_MODULES_DIR`
overrides it), and pins that file in `network.toml`. An incomplete set is
refused by file name at `init`. A joiner installs the founder's file with
`node join --genesis <file>` (a member must; a resident may, and otherwise
fetches it off the mesh at first boot); the node hydrates its blob store and
index from the file at boot, and unpacks it as bare files into
`<workspace>/modules` (the same layout as the founding set).

Then, to run agents on it: `ducktape service run compute` offers this host's
sandbox, `ducktape user cred add claude` logs a provider in, and
`ducktape agent pty claude` attaches a terminal to a sandboxed agent. Each
verb's own `--help` carries the rest; `ducktape node list` shows every network
this machine is registered on and `-n <chain-id>` picks one when there is more
than one. The node serves `/v1` on `0.0.0.0:8844` (`http://127.0.0.1:8844`
from its own box; reads are open, writes are signed) and the listeners in
`docs/deploy/node-service.md`, which is also the systemd
recipe for keeping it up; `docs/deploy/backup-and-keys.md` says which files to
copy before a node is promoted to a validator seat.

Also runnable:

- **`noded-bin`** (dev-only) — a throwaway single-process daemon with
  temporary storage, for driving the `/v1` surface without a workspace:
  `cargo run -p noded-bin` (`http://127.0.0.1:8844`; `-- --storage <dir>` for
  persistent state, `-- --modules <dir>` for another component bundle). Not
  a network node: it has no identity, no mesh and no invites.

- **`simnode`** — a deterministic in-process twin of the node's `/v1` surface
  (`bin/simnode`), used by the test lanes; embed it in any crate's tests via
  `simnode::boot`.

- **`coordinator`** — the untrusted UDP rendezvous + TCP first-contact relay
  (see the operator path above and `docs/deploy/coordinator.md`).

- **`ducktape-app`** (`app/`) — the native GPUI desktop host for dynamically
  loaded Rust-authored WASM views. `make views && cargo run -p
  ducktape-app` (the tabs are wasm views the app loads from `target/views`;
  without them every tab says so); `app/README.md` states which node it
  dials and which key it signs with.

Seed a local "demo" network preloaded with sample data — chat channels and
messages, a tasks board, pages, a registered agent, an inbox note, an
automation rule, plus gateway web-app routes — founded as a "demo" workspace
under `~/.ducktape`, with its own guest images and shell executor:

```sh
make demo-seed
```

## Documentation

`docs/README.md` is the index: one line per document, grouped by the question
it answers — the operator runbooks under `docs/deploy/`, the records code cites
by path under `docs/records/`, the agent runbooks in `skills/`, and the
per-area READMEs. There is no docs site and no decision-record system: the
code and its comments are the record.
