# Authoring wasm modules

How to write, build, and live-update a Ducktape wasm module. The runtime is
`crates/kernel/wasm-host` (wasmtime, pinned `=46.0.3`); the authoring contract is
the `ducktape:module` WIT world (`crates/module-sdk/wit/module.wit`, inside the
module SDK a module pins by git revision, `crates/module-sdk`);
the reference modules are `crates/guests/noop-wasm` (the smallest compliant
module: five exports over the raw WIT world, no state, every op a no-op — the
floor a module must meet and the admission fixture that touches nothing; a new
module starts from the SDK instead, see "Out-of-tree modules" below),
`crates/guests/hello-wasm` (a counter over host-owned state),
`crates/guests/hello-wasm-replacement` (its live-update target), and
`crates/guests/sibling-wasm` (the cross-module-read reference) — kernel test
fixtures, in no genesis set. The first wasm port of a native module is
`crates/examples/directory` (`src/guest.rs`), bytes-compatible with the native
implementation it replaced (same root, same snapshot encoding) — the template
every later port followed. It is in no genesis set either: the crate is a test
tenant the kernel suites construct directly. The node binary embeds
no component: `node init` composes every wasm tenant's `<id>.component.wasm`
and every declared mapper, view and asset tree out of the founding set (`--modules <dir>`,
default `$DUCKTAPE_MODULES_DIR`, else the `modules/` dir noded's build script
stages beside the binary) into the workspace `genesis` file, and pins that file
and every deployment in the network descriptor. A node hydrates its blob store
from the file and installs the running deployments' mappers at boot; a joiner takes it at `join --genesis` or
fetches it off the mesh.

## The model (design-B: host-owned state, guest as pure logic)

A wasm module is PURE LOGIC over the host surface. It holds **no durable memory
across dispatches** — the host re-instantiates a fresh component instance per
call — so all durable state lives host-side, behind the `host.state-*`
capability, staged during `execute` and published only at the block-commit
boundary. Consequences you design around:

- `root()` is host-computed from the host-owned store. A code swap preserves
  that state root. The global root binds each module's state root and deployment
  hash together, authenticating both its state and the code needed to reopen it.
- Never cache anything in guest globals/statics expecting it to survive: it
  will not. Read what you need via `state-get`, write via `state-set`.
- Determinism is by construction: fresh instance per dispatch, fuel-metered
  termination, no clock / rng / net / ambient imports (the WIT surface simply
  has none). A trap (including out-of-fuel) is a deterministic rejection — the
  same on every validator — and the host rolls the op back.
- The runtime envelope canonicalizes NaNs and trims every wasm proposal the
  integer/bytes ABI doesn't need (no SIMD/threads/GC/…): float math in a guest
  is defined, identical-everywhere behavior, and the surface can't grow by
  accident.

## The contract

Implement the five exports:

- `shape() -> module-shape` — what the host must know to run this component:
  the `backing` its committed state lives on (`map`: a host-owned key/value
  map; `store`: a host-constructed authenticated store, every key a 32-byte
  digest; `odb`: a host-side content-addressed substrate the host provides
  in a tenant-scoped directory; `git`: a Git object/refs substrate), the
  `config` keys the host seeds
  into the reserved `__config` record when the module starts fresh
  (`chain_id`, `invite`; empty when the module is not network-bound), and
  `committed-queries` (the query lane answers from committed state alone,
  regardless of caller). A pure constant of the code: the host reads it once
  from the bytes on every path a module enters a host — genesis, a registry
  admission, a reopen, a code swap — before wrapping them over a substrate,
  and refuses a backing other than the declared one. `ducktape_module_sdk` names
  the four plain shapes (`store_shape()`, `map_shape()`, `odb_shape()`,
  `git_shape()`);
  a network-bound module sets `config` on top.
- `execute(payload) -> result<_, error>` — apply one op addressed to this
  module. Reject unknown ops with `error::rejected(..)`; a rejection is a clean
  deterministic no-op, never a fork.
- `query(req) -> result<list<u8>, error>` — a read-only projection over LIVE
  state (the staged overlay on committed — the same read-your-writes surface a
  native module's query serves; out of block the overlay is empty).

- `initialize(params) -> result<_, error>` — initialize a fresh module with
  network parameters. The host calls this after seeding declared configuration,
  both for genesis entries and for live admissions. Reopen and code replacement
  retain state and do not initialize again. Initialization must be idempotent
  because admission preparation or interrupted composition can retry against
  the same store. At genesis, parameters include the
  `modules` deployment map and `validators` keys; other modules may ignore them.
- `finalize-block() -> result<_, error>` — finish the block over its accumulated
  staged writes. The SDK's store wrapper flushes individual operations without
  publishing the outer block, then invokes `Module::commit_block` here. Valset
  uses this to advance its generation once for a net membership change.

ODB and Git queries execute the deployed guest, including host-routed sibling
queries. Their SDK shapes declare `committed-queries: true`: the host excludes
staged refs and staged objects from the read view while a block is open. Files
runs its pure filesystem query core over `GuestOdb`. Forge selects revisions,
checks ancestry and paths, reads tracker state, and formats browse/diff replies
inside its guest. No backing receives a product query request.

The host lends bounded `git-object-read` and `git-diff-read` capabilities to
query rounds. They address an exact object id within the tenant's named
repository, never fetch or mutate refs, and return typed commit/tree/blob data
or diff results. Native libgit2 owns object parsing and patch calculation;
WASM owns their application meaning. Object reads are capped at 16 MiB, with
native commit/tree decoding capped at 256 KiB/4 MiB. Diff requests have host
ceilings of 1 MiB output, 4096 files and 16 MiB materialized blob bytes. All
resolved data shares the host's aggregate read memo budget. These local Git
imports are unavailable during execute and lifecycle calls.

The substrate retains root calculation, objects-before-refs publication,
checkpoint installation, recovery and state sync. `odb` and `git` are distinct
storage contracts: construction and replacement require the offered engine to
match the component's declared backing. Replacement changes query policy while
retaining the existing data layout, backing and initialized configuration.

Lifecycle calls may update own state but cannot emit messages, events, or
assignments, and cannot read siblings. `state-get-committed` reads before the
outer block's staged writes; `state-get` includes them. The registry uses the
committed lane for activation while its status queries expose staged changes.

And use the imports deliberately: `get-env` for the deterministic block env
(`height`, `consensus-time`, `origin`, `me`); `state-*` for
durable state; `emit-msg` for write intents at sibling modules (drained as
follow-up ops, never reentrant); `emit-event` for observability records.

### Application I/O and native boundaries

Consensus guests remain deterministic. Off-chain application I/O runs in
[independently installed service processes](../../deploy/application-service.md),
addressed by a signed Gateway account and route name. A new application uses
that common transport without a native service enum entry, endpoint handler, or
topic parser. Its process owns request interpretation, live stream messages, and
application authorization through committed module queries.

WASM views call the common host operations in
`app/src/module_view/kernel.rs`: `net.request` carries HTTP method, path, headers
and body bytes; `net.stream` and `net.send` carry bidirectional framed traffic.
The host resolves the current route and signs the exact request with the seated
user key. That private key stays outside the view and service. Dropping a view cancels
its requests and streams; an instance from a previous connection cannot submit
work on the newly selected network.

Native changes are still required for new storage engines, cryptographic
schemes, consensus mechanisms, runtime imports, or device/rendering primitives.
Application policy, query interpretation, service protocols, and screens use
the existing capabilities. The [independent deployment example](../../../crates/examples/extension-probe/README.md)
exercises module, view, and service replacement with fixed native executables.

### Sibling reads (`module-root` / `query-module`)

Cross-module READS work from inside a guest: `module-root(target)` is the
sibling's snapshot root as of dispatch start, `query-module(target, req)` a
live host-routed read (self-query is rejected; query cycles are rejected by the
host). Mechanically they are **memoized replay**: the sync guest world cannot
await the host's async ctx, so a read the per-dispatch memo cannot answer
pauses the run, the host resolves it, and the pure guest re-runs with the
answer memoized. Design consequences:

- Reads are answered CONSISTENTLY within one dispatch (the memo), and a
  repeated read costs nothing extra.
- Keep reads bounded: a dispatch may touch at most `MAX_SIBLING_READS` (64)
  DISTINCT reads — one more is a deterministic rejection. Every distinct read
  also costs one replay of your `execute`, so put cheap guards before
  expensive read chains.
- `execute` must stay pure over its inputs (state + env + payload + read
  answers) — that is what makes the replay invisible. Guest globals/statics
  were already worthless (fresh instance per dispatch); the replay is one more
  reason.
- The ctx-less direct `Module::query` path (not host-routed) is SEALED: there,
  `module-root` answers none and `query-module` answers `unsupported`.
  Host-routed queries (`ctx.query` from a native peer, external node queries)
  resolve sibling reads for real.

## State layout is the code-swap contract

A live update swaps code while keeping the store, so it is valid only when the
replacement reads the exact same keys and value encodings. Readiness and
swap preparation reject changes to the declared backing or initialized
configuration keys; authors must preserve the data layout within that shape:

- Keep the layout byte-stable for a code-only swap.
- If the layout changes while greenfield, replace it outright and re-genesis.
  Do not add a second decoder or lazy migration.
- `hello-wasm-replacement` demonstrates the discipline: same `count` key, same
  little-endian `u64` value, different logic (`inc` steps 100, not 1).

## Build: a module is built alone, out of a repository at a revision

A module's build inputs are its own source, one revision of the platform (the
module SDK `crates/module-sdk`, plus any sibling's wire types it reads), its
lock, and the toolchain `rust-toolchain.toml` pins — nothing else. The network
takes the result by hash. A module in this tree and a module in its own
repository are built the same way; only who writes the shell differs.

### In-tree modules

`bin/guest-builder` builds one module out of the platform repository
(`https://github.com/orthory/ducktape`) at a revision — the checkout's HEAD by
default, so push first — and never out of the checkout in place: it synthesizes
a shell workspace under `target/guest-builder/<id>/` whose one dependency is
the module (its `guest` feature on) as a git source, pins the revision in the
shell lock, builds for `wasm32-unknown-unknown`, componentizes through the
`wit-component` crate it links, and writes `component.wasm` and `guest.lock`
into the module directory. The lock is the record of the build (the revision, every registry
version) and the seed of the next one. Uncommitted inputs in the module,
its resolved SDK and sibling packages, or workspace build configuration are
refused, including staged and untracked sources. Artifacts and `guest.lock`
are build outputs and may change during a rebuild.

Dependency resolution uses an explicit revision, so a new module can build
before it exists on the repository's default branch. Before compilation,
the builder removes that selector from both manifests and lock source IDs;
the lock keeps the precise commit, and `cargo build --locked` verifies it.
A first build has no seed: an old scratch lock is discarded.

```
make wasm-modules        # rebuild every guest + refresh ALL committed copies
make wasm-modules-check  # every committed copy is byte-identical, every guest has its lock
make wasm-rebuild-check  # every artifact matches a rebuild of its source at HEAD
make wasm-repro-check    # one guest, two scratch dirs: identical bytes, no host path
```

One module: `cargo run -p guest-builder -- crates/modules/<plane>/<id>`
(`--index` for its index guest, `--rev <sha>` for a revision other than HEAD).

Bytes are stable across revisions that change nothing the module compiles:
the shell names the module by git source alone and the revision lives in the
lock, which is not hashed into symbol names. "Compiles" includes line numbers:
a panic location names its line, and a guest expands the SDK's macros, so a
line added anywhere above them in `crates/module-sdk/src/lib.rs` (a comment
included) moves every guest that expands them. They are identical from any box:
the unpacked revision, the cargo home, the rustup home and the scratch are
remapped to fixed tokens. They are toolchain-dependent, and the toolchain is two pins:
a rebuild on another rustc, or through another componentizer release (it
writes the component's own sections and they move between releases), may
legitimately differ, so moving either pin rebuilds the whole set and commits it
as one change. `rust-toolchain.toml` holds the channel and
`bin/guest-builder/Cargo.toml` pins the componentizer (`wit-component`), which
the builder links rather than finds on a PATH.

The committed copies of one module's component MUST stay byte-identical
(nothing is embedded: the founder bundles the canonical artifact and the
descriptor commits the component-plus-mapper deployment hash; the kernel
test fixtures — the node pins' bundle — carry the same bytes).
`wasm-modules-check` gates that and rides the pre-push `make test` gate;
`wasm-rebuild-check` gates the artifact against its source and needs the wasm32
target and a pushed HEAD.

### Out-of-tree modules

A module authored in its own repository is a cdylib crate over the SDK, pinned
by git revision, built with plain cargo. The manifest:

```toml
[package]
name = "example-module"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
ducktape-module-sdk = { git = "https://github.com/orthory/ducktape", rev = "<sha>" }

# the wasm32 patch set every guest graph needs (crates/module-sdk/stubs at the
# same revision): deterministic getrandom refusals, a C-free blst.
[patch.crates-io]
getrandom-02 = { package = "getrandom", version = "0.2", git = "https://github.com/orthory/ducktape", rev = "<sha>" }
getrandom-03 = { package = "getrandom", version = "0.3", git = "https://github.com/orthory/ducktape", rev = "<sha>" }
getrandom-04 = { package = "getrandom", version = "0.4", git = "https://github.com/orthory/ducktape", rev = "<sha>" }
blst = { git = "https://github.com/orthory/ducktape", rev = "<sha>" }
```

`src/lib.rs` is the guest itself: `ducktape_module_sdk::store_guest!` (or
`snapshot_guest!`) over the crate's `sdk::Module` impl, or a hand-written
`Guest` + `ducktape_module_sdk::export_module!`; the module contract is
`ducktape_module_sdk::sdk`. Build and componentize:

```
cargo build --target wasm32-unknown-unknown --release
# the componentizer at the revision the module pins — the bytes the network's
# own modules came out of
cargo install --locked --git https://github.com/orthory/ducktape --rev <sha> guest-builder
guest-builder componentize target/wasm32-unknown-unknown/release/example_module.wasm --out component.wasm
```

Every platform crate the module reads must come from that ONE revision: a
sibling's wire types pinned at another revision bring a second `sdk`, and the
types no longer match. Hand the result to `ducktape module register <id>
component.wasm`; the network verifies the bytes by hash.

## Live update: how new code ships

Each deployment is a canonical Borsh `ModuleArtifact`: component bytes followed
by an optional mapper and an optional view with its asset map. Its SHA-256
covers the complete deployment.
`ducktape module pack component.wasm --out module.artifact` packages the files
and prints this hash offline; `--index index.wasm` includes a mapper.
`--view view.wasm --assets assets/` includes the module's desktop view and its
asset tree in the same deployment hash. These flags also apply to `register`
and `update`; `--assets` requires `--view`. Asset paths are exact, relative,
UTF-8 slash paths; symlinks are refused. Materialization rejects paths that
collide or cannot be represented exactly on the destination filesystem.

A founding directory uses `<id>.component.wasm`, optional `<id>.index.wasm`,
optional `<id>.view.wasm`, and optional `<id>.assets/`. Arbitrary external
module ids remain supported. `Genesis::compose` and runtime artifact reads
share the same preparation checks. A `<id>.view.pending` marker blocks both,
even when an older view file is still present.

`make views` bundles the packages declared under `crates/views/`, with fixed
source-path prefixes and explicit unoptimized output independent of PATH.
`make views-repro-check` builds the committed HEAD snapshot in two isolated
roots, compares all view bytes, and rejects embedded builder-home paths.
The consensus guest `wasm-rebuild-check` does not cover these views. The noded
build stages module views for governance, files, pages, chat, and forge when
that owner's `crates/views/<id>/Cargo.toml` exists. Other desktop views remain
desktop resources. Missing or empty declared view output leaves a pending
marker; compiling noded succeeds, but founding or packing that deployment
fails. Run `make views` and rebuild noded to prepare the founding set. A
restored view clears pending after its assets are synchronized. Removing the
owner declaration removes staged view files and assets; removing only a build
output does not. Asset files removed from a restored source tree are removed
from staging as well.

Agents commit the artifact and request `modules.update` through their final response
(see [dogfood](../../dogfood.md)). The operator CLI packages the same unit,
proposes that hash to governance, and only then stages it on the blob plane — a
peer admits a pushed digest only when consensus names it, and for a brand-new
artifact the OPEN proposal is the only record that does:

```
ducktape module register pages pages.component.wasm --index pages.index.wasm
ducktape module update pages pages.component.wasm --index pages.index.wasm
```

Omitting `--index` means the deployment has no mapper, including when updating
an existing module. Mapper-only changes still require a deployment proposal.
The `modules` and `valset` registries use these same commands and activation
rules; their presence at genesis grants no special code-loading path.

Validators verify the deployment hash, compile the component, check its shape,
and validate its mapper before signaling readiness. At the armed activation
height, the host prepares every replacement before applying any of them. The
component retains host-owned state, and the registry's injected `Advance`
records the activation. An unavailable or invalid deployment stalls that
boundary without partially replacing the running roster.

The index converges to the running deployment before folding the block. A
changed mapper clears its derived rows and refolds the retained op feed;
removing it clears its derived rows and disables its view. Readers wait through
replacement and refold. A mapper that cannot fold its feed reports a stuck fold
through index status; the derived tier remains outside consensus.

Checkpoint and state-sync manifests carry each module's deployment hash,
including the registry's. The global root authenticates those hashes alongside
state roots. Recovery loads that code directly, then reconciles each replay
height against the registry's activation history. The genesis file supplies
initial deployments and remains unchanged by later updates.

## Testing a module

- Runtime-level: `crates/kernel/wasm-host/tests/dispatch.rs` — the
  staged-writes / commit / abort / determinism / hot-swap / snapshot proofs —
  and `crates/kernel/wasm-host/tests/sibling.rs` — the sibling-read proofs
  (replay convergence, memoization, no staging leak across replays, budget).
- Host-level: `crates/kernel/host/tests/module_swap.rs` — the full live-update
  boundary (schedule → realize at H → new logic over kept state, fail-closed,
  joiner reconciliation, cross-node determinism) — plus
  `crates/kernel/host/tests/cross_module.rs` (a native peer composing with a
  wasm module) and `crates/kernel/host/tests/wasm_cutover_parity.rs` (the
  native↔wasm byte-compatibility proof for the directory cutover).
- Authorization-level: `crates/modules/system/governance/tests/
  governance_schedules_module_update.rs` — ballot → registry acceptance.

## Porting a native module (the cutover pattern)

The `directory` port is the template: split the native crate so its wire types
compile standalone (`default-features = false`, the `files`/`duckfs-core`
shape), author the guest against those SAME wire types (drift is then a compile
error), and choose the guest's state layout so the host store's canonical
encoding reproduces the native root — if the native module already hashes
`le-u64 count ‖ sorted (len‖key ‖ len‖value)`, storing the raw key/value bytes
makes root(), snapshot(), and install() BYTE-IDENTICAL across the cutover: the
root-hash does not move and pre-cutover workspaces restore unchanged. Pin that
claim with a parity test before wiring the module into `host_state`.

Point your module's tests at a committed fixture (`include_bytes!`) so the
proof is self-contained, and register the fixture in `make wasm-modules` so it
can never drift from the source crate.
