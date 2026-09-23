# Ducktape

**A BFT-replicated sandbox for wasm programs.** A network is a set of
validators running the same programs over one authenticated state. Anyone
with a wallet key submits a signed frame; consensus orders it; every node
applies it and commits one root. Two nodes that agree on that root agree on
everything.

Every program is a wasm guest the host runs against its own key space.
Programs speak one bytes ABI ([`crates/kernel/abi`](crates/kernel/abi)): a
call is bytes in, a reply is bytes out, and reads, writes, blobs and messages
to other programs go through host ops. A program ships as a file the founding
names, and a network swaps one at a block, independently of the binary that
runs it.

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
make install-node
```

This puts the `ducktape` binary in `~/.cargo/bin`. `make install` also
delegates the desktop installation to the pinned
[ducktape-app](https://github.com/ducktape-industries/ducktape-app) checkout.

## Found a network

A founding file names the network, its cadence, its validators and the
programs it starts with. Every path is relative to the file; the programs a
network boots with (`module-registry`, `valset`, `admission`, `identity`) are
built and committed in [modules](https://github.com/ducktape-industries/modules)
under `crates/modules/system/wasm/`:

```toml
network = "mynet"
time = 1700000000000          # the genesis block's time, unix milliseconds
epoch_length = 64             # blocks per validator epoch
block_time_ms = 1000
module-registry = "module_registry.wasm"   # the program that registers and swaps programs
valset = "valset.wasm"                     # the program that seats each epoch's validators

[[validators]]
key = "…"                     # hex ed25519 public key: `ducktape identity`
address = "203.0.113.7:9000"  # where peers dial it

[[programs]]
id = "admission"              # one entry per system program, `id` = its contract's PROGRAM
code = "admission.wasm"       # the program that enrolls a joiner as a member

[[programs]]
id = "identity"
code = "identity.wasm"

[[programs]]
id = "ping"
code = "ping.wasm"
params = "ping.params"        # optional: the bytes the program's init receives

[limits]                      # optional; absent means unmetered
fuel = 1000000000
memory_bytes = 268435456
```

```sh
ducktape identity                                   # mint this workspace's node key
ducktape init genesis.toml                          # found the network in $DUCKTAPE_HOME
ducktape run --listen 0.0.0.0:9000 --http 127.0.0.1:8844
```

Any node joins by adopting the network's state from a running node and
enrolling its key and address through the `admission` program, then runs like
a founder. A validator named in the founding file keeps its seat; anyone else
is a member the validators connect to and send blocks, without a vote. The
address is the one peers dial: a validator accepts a member only from the IP
it enrolled with, so it must be the address the node is reached at, not a
loopback or a placeholder.

A network starts with its door closed: enrolling takes an invite a validator
mints. An invite is signed by the validator's node key, names the network,
expires, and admits one node.

```sh
ducktape invite --hours 72                      # on a validator; prints the invite
ducktape join http://203.0.113.7:8844 --address 198.51.100.4:9000 --invite <invite>
ducktape run --listen 0.0.0.0:9000
```

The enrollment lands in the next block and the validators track the new
member from the epoch after it; until then the node's dials are refused and
retried. The table holds at most 1024 members (`abi::valset::MAX_MEMBERS`).

Validators decide everything else by vote, each from its own node; a motion
passes when the BFT quorum of the current validators (`n - (n - 1) / 3`) has
voted for it, and takes effect at the next epoch:

```sh
ducktape vote promote <key>    # a resident becomes a validator
ducktape vote demote <key>     # a validator becomes a resident
ducktape vote remove <key>     # a member is removed
ducktape vote open             # anyone may enroll without an invite
ducktape vote close            # enrolling takes an invite again
ducktape leave                 # this node stops being a member
```

With two validators the quorum is both, so the chain stops while either is
down.

The workspace (`--workspace`, default `$DUCKTAPE_HOME` else `~/.ducktape`)
holds `identity.key`, the network descriptor, the anchor the node started
from, the address its HTTP surface bound and the runtime's storage.

## Use a network

```sh
ducktape status                         # height, tip, root, epoch, identity
ducktape wallet new alice               # a user key; DUCKTAPE_PASSWORD or a prompt
ducktape submit ping request.bin        # sign a frame with the active wallet, print the receipt
ducktape get ping 6b6579                # a key in hex, from the preconfirmed layer
ducktape get ping 6b6579 --confirmed    # the committed layer
ducktape scan ping --lo 6b               # a key range
ducktape changes ping                   # follow a program's confirmed writes
ducktape query ping request.bin         # a signed read-only call
ducktape programs                       # every program and its code blob
ducktape blob get sha256:…              # a blob's framed bytes
```

Reads are open; writes are signed frames. Any member's node takes a frame: it
hands each frame it accepts to the epoch's validators over the mesh, and
sends again what is still waiting at every epoch, so a frame submitted to a
member without a vote lands like one submitted to a validator. A block holds
as many waiting frames as fit one mesh message (`node::BLOCK_BYTES`); a frame
no block can carry is refused. `ducktape logs`,
`ducktape log-filter`, `ducktape metrics` and `ducktape shutdown` operate the
node; the mutating ones are signed with the node's own identity key. Each
verb's `--help` carries the rest.

## How it is built

| Layer | Where | What |
| --- | --- | --- |
| Kernel | `crates/kernel/` | `abi` (the bytes ABI), `guest` (what a program compiles against), `runtime` (the wasmtime embedding), `state` (the authenticated store and its commitments), `blobs` (one content-addressed store), `host` (the sandbox: submit, query, deliver), `node` (frames, blocks, the mempool), `consensus` (Simplex BFT over marshal, per-epoch engines, catch-up), `statesync` (a joiner adopts a network's state); `fixtures/` is its own workspace of wasm32 test programs |
| Programs | [`ducktape-industries/modules`](https://github.com/ducktape-industries/modules) | The contracts a program compiles against (`crates/sdk/abi`, `crates/sdk/guest`: copies of `crates/kernel/abi` and `crates/kernel/guest` here), the boot set (`crates/modules`: the `modules` contracts crate, the `module-registry`, `valset`, `admission` and `identity` programs under `system/`, their committed bytes under `system/wasm/`, and the suite that drives them on this host) and the app modules. The eight system modules beyond the boot set are archived at `ducktape-industries/ducktape-system-modules-archive` |
| Daemon | `crates/noded/`, `bin/node/` | The `/v1` HTTP and WebSocket surface, the lookup mesh, the workspace on disk, the client, and the `ducktape` binary |
| Networking | `crates/networking/` | Off-consensus byte transport for the services; the WireGuard overlay and the coordinator are `ducktape-industries/tunnel` |
| Services | `crates/services/` | Off-chain executors: provider run loop, microVM sandbox, credential broker, airlock, media |
| Binaries | `bin/` | The airlock gateway, the media and terminal services, the sandbox PID 1 |

## Develop

```sh
cargo test -p host -p node -p consensus -p statesync -p noded   # the kernel and the daemon
make kernel-fixtures                                            # rebuild the wasm32 test programs
make test                                                       # everything the repo verifies locally
```

[`AGENTS.md`](AGENTS.md) holds the rules that bind changes to this tree.

## Documentation

[`docs/README.md`](docs/README.md) is the index: one line per document,
grouped by the question it answers; [`ops/README.md`](ops/README.md) covers
the operator scripts. There is no docs site: the code and its comments are the
record.
