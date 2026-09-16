---
name: sim-lane
description: Use when any Rust #[test] in this workspace needs a deterministic in-process Ducktape node (no child processes, no fleet) — boot it with simnode::boot from bin/simnode, drive real submit → commit → query round-trips, and step commits deterministically. Covers the embedding harness and SimOpts. The desktop app has no simulator lane; its own suites run with cargo test -p ducktape-app.
---

# Sim lane — deterministic in-process node

`simnode::boot` (`bin/simnode/src/lib.rs`) boots a deterministic Ducktape node
in-process: the full noded `/v1` HTTP surface plus a synchronous control handle,
no child processes and no timers. Any crate's `#[test]` can drive real
transaction round-trips (submit → commit → query) against it. The desktop app
(`app/`) has ordinary `#[cfg(test)]` suites (`cargo test -p ducktape-app`) and
no simulator lane; only the node below is embeddable.

## Where things live

| Thing | Path |
|---|---|
| Embeddable node lib + doc comment | `bin/simnode/src/lib.rs` |
| Cross-crate embedder example | `bin/simnode/tests/embed.rs` |
| Node-semantics scenario suites | `bin/simnode/tests/*.rs` (`cargo test -p simnode`) |

## Embedding the node in any crate's test

```toml
[dev-dependencies]
simnode = { path = "../../bin/simnode" }   # adjust depth
```
```rust
let storage = tempfile::tempdir()?;        // FRESH dir per test — determinism
let sim = simnode::boot(
    storage.path(),
    "127.0.0.1:0".parse()?,                // ephemeral; real port via sim.addr()
    simnode::SimOpts { auto: true, ..Default::default() },
)?;
// HTTP: full noded /v1 surface at sim.addr(). Control: sync handle —
// step()/set_auto()/peer_block()/state()/shutdown() (Drop also tears down).
```

`SimOpts` for embedders: keep `install_log: false` (true stacks a
process-global tracing subscriber + panic hook — binary only). `auto: true`
= submits commit inline; `false` = held mode, commits happen on `step()`
(races as scripts). `echo_oracle` enables the in-process echo oracle. Governance
scenarios: `valset_keys` (raw 32-byte ed25519 pubkeys) + `invite_binding`;
`node_key` fabricates `status.public_key`; `persona` picks daemon (`op_hash`
receipts) vs validator (height-only) shape.

`modules_dir` (binary: `--modules <dir>`) is where the genesis reads each
tenant's `<id>.component.wasm` and each declared `<id>.index.wasm`. Leave it
`None`: the default is the founding set this binary's own build staged beside
it (`target/<profile>/sim-modules%<checkout path>`, or
`$DUCKTAPE_MODULES_DIR`), so a bare checkout
boots with no `make install-node` and nothing installed anywhere.
The default set is `topology::SIM_BASE` (15 tenants, all wasm components);
`--with-valset` appends `topology::SIM_VALSET` (acl and governance as
components, kv/valset/modules native). A sim boot cranelift-compiles the
set — seconds, and paid per boot, per test. After a host fatal the control
surface fails closed (every call errs with the reason); the *triggering* call
may still return Ok — check the next one.

**Tests wait on events, never on time.** The control handle's `step()` /
`state()` are the synchronization seam — no sleeps, no polling. If a test
seems to need a wait, the flow is broken, not slow.

## Wire shapes

Every module's request/reply shapes are its crate-root `interface.rs`
(`crates/modules/apps/chat/src/interface.rs` for chat: externally tagged
snake_case enums, e.g. `{"messages":[..]}`). Listing and paging reads
(channel lists, message pages, search) are index views, served at
`POST /v1/index/{module}/view` from the module's `src/index.rs`, not
canonical queries — the canonical `query` surface keeps only what dispatch
reads (`docs/records/specs/indexable-spec.md` §5). Frames carry a
caller-supplied signature and the node verifies their origin; never expect a
real `ducktape` subprocess inside an in-process test.
