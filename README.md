# Ducktape

**A self-hosted workplace that runs on consensus.** Chat, pages, tasks, a git
forge, files and sandboxed AI agents, replicated across the machines of the
people who use it. No server to rent, no vendor holding the data.

Every product is a module on one BFT-replicated state machine. Each module owns
its state and exposes a single 32-byte root; the host composes those roots into
one hash that consensus commits. Two nodes that agree on that hash agree on
everything. Modules ship as wasm components a network can swap at a block,
independently of the binary that runs them.

## Install

Requires [`rustup`](https://rustup.rs) (the pinned toolchain and its wasm32
target install themselves on the first build), a C compiler, and on Linux:

```sh
sudo apt install build-essential pkg-config libclang-dev libasound2-dev \
  libx11-xcb-dev libxkbcommon-dev libxkbcommon-x11-dev libfontconfig1-dev libfreetype6-dev
```

On macOS, `xcode-select --install`. `make` checks the prerequisites up front.

```sh
git clone https://github.com/ducktape-industries/ducktape.git
cd ducktape
make install
```

This puts the `ducktape` CLI and the founding module set in `~/.cargo/bin`.
On macOS it also builds the desktop app and installs `Ducktape.app` into
`~/Applications`.

## Quick start

Every line runs as written on a machine with one network on it:

```sh
ducktape node init --name mynet     # found your own network here
ducktape node join <invite>         # ...or join someone else's
ducktape node run                   # start it (^C checkpoints and exits)
ducktape wallet new <you>           # mint your user key
ducktape account create --name <you>   # found your account on it
ducktape node status                # height + root hash of the running node
```

Then, to run agents on it:

```sh
ducktape service run compute        # offer this host's sandbox
ducktape user cred add claude       # log a provider in
ducktape agent pty claude           # attach a terminal to a sandboxed agent
```

Each verb's own `--help` carries the rest. The node serves `/v1` on port 8844;
reads are open, writes are signed by your wallet key.

For a network preloaded with sample data (channels, a task board, pages, an
agent, an automation rule):

```sh
make demo-seed
```

## Desktop app

```sh
make views && cargo run -p ducktape-app
```

The app is a native GPUI host whose tabs are Rust-authored wasm views loaded at
runtime. [`app/README.md`](app/README.md) says which node it dials and which
key it signs with.

## How it is built

| Layer | Where | What |
| --- | --- | --- |
| Kernel | `crates/kernel/` | Module SDK, host execute loop, replication, Simplex BFT consensus, state sync, indexer, wasmtime runtime |
| Networking | `crates/networking/` | WireGuard mesh, NAT traversal, reachability, overlay data plane |
| Modules | `crates/modules/` | Every consensus module: `system/` (validator set, governance, identity, module registry, ACL, gateway, ...) and `apps/` (chat, pages, tasks, forge, files, agent, runs, boards, ...) |
| Services | `crates/services/` | Off-chain executors a module drives: compute pool, provider run loop, microVM sandbox, credential broker, airlock, media |
| Binaries | `bin/` | The `ducktape` CLI and node, the coordinator, `guest-builder`, the sandbox PID 1, the dev daemon and its deterministic twin |
| App | `app/`, `crates/views/` | The desktop host and its wasm views |

The one rule modules obey: a module never links another module's crate. It
depends on the SDK and on the types-only wire shapes a sibling publishes;
cross-module reads go through host-routed queries, writes through messages the
host drains. [`crates/topology/`](crates/topology/) is the single source for
the module id universe and the genesis selections.

## Develop

```sh
cargo test --workspace                          # the Rust workspace
cargo test -p node-bin --test cluster_e2e       # real node processes over localhost TCP
make test                                       # everything the repo can verify locally
make wasm-modules                               # rebuild every module component
make wasm-modules-check                         # the committed components match the source
```

Writing a module: [`skills/module-dev/SKILL.md`](skills/module-dev/SKILL.md)
is the wiring runbook and
[`docs/records/architecture/wasm-module-authoring.md`](docs/records/architecture/wasm-module-authoring.md)
the recipe for a module authored in its own repository.
[`AGENTS.md`](AGENTS.md) holds the rules that bind changes to this tree.

## Documentation

[`docs/README.md`](docs/README.md) is the index: one line per document,
grouped by the question it answers. Operator runbooks live under
[`docs/deploy/`](docs/deploy/) (running a node under systemd, backups and keys,
the coordinator, sentries); [`ops/README.md`](ops/README.md) covers the
operator scripts. There is no docs site: the code and its comments are the
record.
