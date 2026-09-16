---
name: module-dev
description: Use when creating a new ducktape consensus module, porting a native module to a wasm guest, or wiring a module into the genesis set — topology::PRODUCTION, host_state, crates/guests, Makefile wasm-modules. Also when a module change breaks the registry parity test, a missing component in the fixtures/modules dir, or wasm-modules-check.
---

# Module development — the end-to-end wiring runbook

A module is four layers: native crate (the logic) → wasm guest (the packaging)
→ registration (the topology, then THREE bins) → proofs (parity + fixtures).

**REQUIRED BACKGROUND:** `docs/records/architecture/wasm-module-authoring.md`
— the guest contract (host-owned state, sibling reads, live update, the
cutover pattern). This skill is the wiring checklist that record doesn't cover.
A module written outside this tree needs none of it: see "Decide first".

## Decide first: genesis registration is a root-hash break

A module in `topology::PRODUCTION` (the selection `bin/node` composes,
opens its index over, and reports) joins the genesis set: the descriptor's
module table gains a row, its `genesis_namespace()` fingerprint moves, so
every existing workspace fails closed and dev networks re-genesis. A genesis
module ⇒ a new genesis — get that agreed before wiring.

A post-genesis module leaves the genesis unchanged, changes the live root at
activation, and needs no genesis edit, no
topology row, and no bin change — it is one operator command against a LIVE
network:

```
ducktape module register <id> <component.wasm> [--index <index.wasm>] [--after N]  # admit a new id
ducktape module update   <id> <component.wasm> [--index <index.wasm>] [--after N]  # swap live code
ducktape module register <id> --view target/views/<id>_view.wasm [--assets crates/views/<id>/assets] [--after N]  # admit a VIEW-ONLY entry (kind `view`: a UI with no core, built by `ops/build-views.sh -p <id>-view`; the app draws its tab off the registry)
ducktape module status                                      # the registry, plus the open code proposals (tasteable)
```

While a code proposal is open (and once it is scheduled), every member may
TASTE its view: `module update <id> <active core> --view <new wasm>` opens the
ballot and fans the bytes out; the app's Governance card and Settings →
Proposed views offer "Try this view" for a frame whose core is byte-identical
to the active one (a `View`-kind frame always), seating it on that device
alone with the tab reading "· proposed"; withdrawal returns the seat,
activation keeps it. A proposal that changes the core is listed as
`core_changes_too` with nothing to try.

`register`/`update` drive the governance proposal that schedules the
admission/swap FIRST, then stage the component at this node's owner-gated admin
route (which fans it out to every validator and returns their receipts). That
order is load-bearing: a peer admits only a digest consensus names — an active
`code_hash`, a pending swap, or an OPEN `RegisterModule`/`UpdateModule`
proposal — so a brand-new artifact staged ahead of its proposal collects
refusals. A validator that did not take the bytes is printed as a holdout, not
a refusal to propose: the swap activates at `height + N` (`N > MIN_SWAP_LEAD`,
i.e. `> 3`; default 50 to leave room for the ceremony's own blocks) only once
every validator holds the code and signals ready, and a holdout fetches the
committed artifact off a peer before that boundary. `status` prints one row per
entry — `id  kind  active  pending` (`kind` is `module` or `view`: what the
registry says the artifact is, fixed at admission), a pending swap carrying
`ready k` (validators that signalled) or `ready ✓`. Restore and state sync compose the wasm set from the registry's
roster at the boundary (`noded::compose`, `Boot::Reopen`), so an admitted id
composes like a genesis one; a module admitted after the last checkpoint
starts fresh and is rebuilt by replay (`seat_at`, unit-pinned in
`host_state.rs`).

The CLI stages bytes, it never builds them: the component still comes from
`make wasm-modules` / `guest-builder` (§2). A module written outside this
tree needs none of that: it is a cdylib crate pinning `ducktape-module-sdk`
(`crates/module-sdk`) by git revision, built with `cargo build --target
wasm32-unknown-unknown --release` and `guest-builder componentize`, then handed
to `module register` — the manifest and recipe are in
`docs/records/architecture/wasm-module-authoring.md` ("Out-of-tree modules").
Experiments that shouldn't pay the genesis cost live unwired in `crates/labs`.

## 1. Native crate — `crates/modules/{apps|system}/<id>`

Clone the `tasks` shape:
- `src/interface.rs` — wire types + codecs at crate root. The ONLY surface
  other modules may import.
- `src/lib.rs` — the struct + `impl sdk::Module` (`root`, `execute`, `query`,
  `commit_block`) + `snapshot()`/`install()`. Re-export `interface` at root.
- `tests/` — happy path, every rejection, snapshot→install root round-trip.
- Root `Cargo.toml`: `members` entry + `[workspace.dependencies]` alias.
- Native-only deps (media engines, unix IO, tokio) must sit behind a `native`
  feature or be absent — the guest builds compile this same crate to wasm32.

## 2. Wasm guest — `src/guest.rs` in the module crate, built by guest-builder

The module carries its OWN port (the `tasks`/`chat`/`files` shape): a
`src/guest.rs` behind a wasm-only `guest = ["dep:ducktape-module-sdk"]`
feature — the doc header, the id consts, and ONE dispatch-shell macro
(`ducktape_module_sdk::snapshot_guest!` for whole-state `SnapshotBytes`
modules, `store_guest!` for store-backed ones, or a hand-written `Guest` impl
+ `export_module!` for odd tenants like files). Each macro takes the
component's `shape:` — the host learns everything it needs to run the
module from the `shape` export, never from a table: `store_shape()` /
`map_shape()` / `odb_shape()`, with `config: vec![CHAIN_ID.into()]` (or
`INVITE`) on top for a network-bound module and `committed_queries: true`
for a committed-only query lane. `#[cfg(feature = "guest")] mod guest;` in
lib.rs. No packaging crate is checked in: `bin/guest-builder` builds the
module ALONE, out of the platform repository at the checkout's HEAD (so
push first — uncommitted module, SDK, sibling, and workspace build inputs
are refused, and an unpushed HEAD fails to fetch), through an ephemeral shell workspace under
`target/guest-builder/<id>/`, and writes the canonical COMMITTED
`component.wasm` and `guest.lock` (the revision and every registry version
the artifact came from; the seed of the next build) into the module
directory. Bytes move only when something the module compiles moves.

`Makefile`: add the module to `BUILDER_MODULES` — that one entry covers the
build, the fixture `cp`, and the `wasm-modules-check` / `wasm-rebuild-check`
`cmp`s.

## 2b. Index guest (optional) — the module's derived-tier mapper

A module that wants a materialized view (search, listings — anything qmdb's
point lookups can't serve) ships a SECOND wasm artifact: the index guest
(spec: `docs/records/specs/indexable-spec.md`). Same two-file shape in the
module crate:
- `src/index.rs` — the pure decision core: `fold_op`/`serve_view` over the
  `index_guest` contract crate (dep `index_guest = { workspace = true }`,
  never `indexer`). Unit-test natively against a `BTreeMap`.
- `src/index_guest.rs` behind `index-guest = ["index_guest/guest"]` — the
  ~15-line engine shell (`EngineRead`, `apply`, `index_guest::fold!`/`view!`).

`guest-builder --index <module-dir>` writes the committed `index.wasm`; add
the module to `INDEX_MODULES` in the Makefile. The `src/index_guest.rs`
file IS the declaration: noded's build script stages `<id>.index.wasm` into
the founding set for exactly the module crates that carry it, and `node
init` composes the genesis from whatever `<id>.index.wasm` the set holds.
The fold runs ASYNC behind a fluent31 changes-mode trigger — views trail
the op feed observably (`/v1/index/status` `fold.{module}`), never
atomically.

The engine's side of that contract — no backfill at registration, at-least-once
invocation with exactly-once effects, a row above the inline cap arriving
key-only, a failing fold holding its queue with backoff — is fluent31's own
`SKILL.md` ("Priors that do not transfer"), at the rev the root `Cargo.toml`
pins; read it before writing a mapper. A guest's `log` output is a `debug`
event under `fluent31::wasm::guest` — turn that one target up to see it.

## 3. Registration — `module register`, or (for genesis) the topology

Post-genesis is the whole of this section for most modules: `ducktape module
register <id> <component.wasm>` admits the id on a live network and `ducktape
module update <id> <component.wasm>` swaps its code later. The registry is
consensus state, so nothing below needs editing — every node composes the
admitted module from it.

`module update` swaps the CODE, never the DATA: `WasmModule::swap_code`
replaces the component and leaves the store untouched. The new component must
therefore be schema-IDENTICAL to the old one — same key derivation, same value
encodings — not merely schema-compatible. A store-backed module's logical keys
are hashed before they touch the store (`staged_store::store_key`, `sha256`),
so the store carries no order and no prefix a new component could scan; the
`ducktape:module` WIT world exports `shape`, `initialize`, `execute`,
`query`, and `finalize-block`. Readiness and swap preparation require the
replacement to preserve its declared backing and initialized configuration
keys. There is no migrate/scan import: a new component cannot
enumerate the records a key- or value-shape
change would need to rewrite, because the keyspace it would scan is exactly
the sha256 digests it can't invert. A key-layout or value-shape change is a
new module id — a fresh `register`, decided at genesis if it must replace an
existing one — never a `module update`.

Genesis is a file of ordinary deployments. `node init --modules <dir>`
discovers components and optional mappers by filename, without consulting a
compiled roster. Each module, including `modules` and `valset`, initializes
through the same Wasm lifecycle. The default founding sets are build presets:
`PRODUCTION` stages into `modules/`; `SIM_BASE` and `SIM_VALSET` are available
in `sim-modules/` beside the binaries.

| Where | What to touch |
|---|---|
| `crates/topology/src/lib.rs` | Add a catalog row and selection only when the module belongs in a shipped default. The component declares its own backing and configuration. Arbitrary founding directories and live admissions need no catalog entry. |
| `crates/noded/src/compose.rs` | Components select a supported `Backing` engine over a private module directory. A genuinely new engine needs a host capability; a new module id needs no native table entry. |
| the indexer | Ship `src/index_guest.rs`, add the crate to `INDEX_MODULES` in the Makefile, and pass `--index` on live registration or update. `converge_host_modules` installs the running deployment's mapper at boot and activation. Omitting `--index` removes an old mapper; its derived rows clear while the feed survives. |

The deployment hash covers component and mapper together. A mapper-only
change uses the same proposal, readiness, and activation boundary as a code
change. The global root binds deployment hashes as well as state; checkpoints
and state-sync manifests authenticate the code needed to reopen the registries.

`SIM_BASE` contains 15 modules; `--with-valset` adds `acl`, `governance`,
`modules`, `valset`, and `kv`, all Wasm. When changing a shipped default set,
update its membership tests after rebuilding the artifacts.

## 4. Gates — ordering is load-bearing

```
cargo test -p <id>                                        # 1. native logic
git push                                                  # 2. the guest build reads HEAD out of the repository
cargo run -p guest-builder -- crates/modules/<plane>/<id> # 3. catches native-dep leaks
make wasm-modules                                         # 4. BEFORE the node pins run —
                                                          #    the fixtures dir needs the artifact
cargo check --workspace --all-targets                     # 5. registry parity test gates
cargo clippy -p <id> --tests --no-deps                    #    topology↔composed-host drift
make wasm-modules-check                                   # 6. committed copies byte-identical, locks present
make wasm-rebuild-check                                   # 7. every guest matches a rebuild of its source (needs wasm32)
```

## Common mistakes

| Mistake | Reality |
|---|---|
| Adding the id to `PRODUCTION` only | noded/simnode compose `SIM_BASE`/`SIM_VALSET`; the module is invisible in daemon/sim lanes until it joins one of those too |
| Topology pins left stale after adding/removing a module | `cargo test -p topology` and the genesis registry parity test fail; update both in the same commit |
| Guest added to root workspace members | guests are standalone BY DESIGN; membership poisons native feature unification |
| Node pins run before `make wasm-modules` | the fixtures dir lacks the component; `hash_bundle` refuses by name |
| Building a guest before pushing | guest-builder reads the module out of the repository at HEAD: an unpushed HEAD fails to fetch, an uncommitted edit is refused. Commit, push, then build |
| Moving the rust channel — or the componentizer — for one guest | bytes depend on BOTH pins (`rust-toolchain.toml` and the `wit-component` pin in `bin/guest-builder/Cargo.toml`, which the builder links); moving either rebuilds the whole set (`make wasm-modules`) and commits it as one change |
| Touching `crates/module-sdk/src/lib.rs` without a rebuild | panic locations carry line numbers and every guest expands the SDK's macros, so even a comment line above them moves the set; `make wasm-rebuild-check` names the stale ones |
| Native-only dep in the module crate | wasm32 build breaks; gate it behind the `native` feature (the `files` shape) |
