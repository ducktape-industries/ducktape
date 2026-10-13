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

Each commit on `main` also has a built `ducktape` for Linux on x86_64 and
macOS on arm64: the prerelease `bin-<commit>` carries
`ducktape-x86_64-unknown-linux-gnu.tar.gz` and
`ducktape-aarch64-apple-darwin.tar.gz` (`.github/workflows/release.yml`).

## Found a network

A founding file names the network, its cadence, its validators and the
programs it starts with. Every path is relative to the file.
[modules](https://github.com/ducktape-industries/modules) publishes a
founding for each commit on its `main`: the release `wasm-<commit>` carries
`founding.tar.gz`, which holds every program it builds, packed with its
view, and a `founding.toml` with only its network, time, key and address
left to fill. The fields:

```toml
network = "mynet"
time = 1700000000000          # the genesis block's time, unix milliseconds
epoch_length = 64             # blocks per validator epoch
block_time_ms = 1000
member_cap = 16               # optional, default 16: the most members (validators and residents) it ever holds, 1 to 128; fixed at founding
hooks = ["activity"]          # optional: founding programs the kernel tells of every frame; the registry holds the list from here on

[roles]                       # the founding program the kernel calls in each role; all three required
registry = "module-registry"  # registers and swaps programs
validators = "valset"         # seats each epoch's validators; runs membership and the validators' votes
identity = "identity"         # resolves a key or a program to its account

[[validators]]
key = "…"                     # hex ed25519 public key: `ducktape identity`
address = "tcp://a.example.org:9000"  # optional: where peers dial it; see Reach a node

[[programs]]
id = "module-registry"        # one entry per program; the registry's and validators' params are the kernel's
code = "module_registry.wasm"

[[programs]]
id = "valset"
code = "valset.wasm"

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

The kernel calls programs by role, never by id. Founding refuses a role bound
to a program the file does not list, and every frame's env carries the
bindings (`Env.roles`). As the kernel admits a program, at founding or later
off the registry, it gives the program an account: the identity role's
`RegisterModule`, executed with the `System` origin. Once per frame the host
asks the identity role who the frame acts as, `Account(key)` for a signed
frame and `OfModule(program)` for a message or a reply, and passes the answer
as `Env.sender`; a refusal rejects the frame, and a key that holds no account
runs as no one (`None`). A program may send a message as another account
(`Message.acting`, the guest's `emit_as`): the host asks the identity role
`Runs { program, account }` and runs the message as that account only when
the role answers with it, so an agent account its manager hands to a program
acts through that program with no key held anywhere. The interfaces are `abi::role::{registry,
validators, identity}`; modules' `docs/roles.md` walks through them.

A hook is a program the kernel tells of every frame a block carries, after
the frame has run: at each block it asks the registry `Hooks(height)` and,
after each frame, calls every hook it named with `GuestCall::Hook` and an
`abi::hook::Record` (the height, the frame's position in the block, its
signer, the account it acted as, its target and whether it applied), from
the `System` origin on a fixed budget of `HOOK_FUEL`. A hook never changes
the frame: one that refuses, traps, spends its fuel or sends a message has
its own writes undone, and the block goes on. The founding file's `hooks`
seeds the registry's list; a running network hooks or unhooks a program
through the registry like any other change.

```sh
curl -fsSL https://github.com/ducktape-industries/modules/releases/download/wasm-<commit>/founding.tar.gz | tar xz
ducktape identity                                   # mint this workspace's node key
$EDITOR founding/founding.toml                      # network, time (`date +%s000`), that key, this node's address
ducktape init founding/founding.toml                # found the network in $DUCKTAPE_HOME
ducktape run --listen 127.0.0.1:9000 --http 127.0.0.1:8844
```

Any node joins through a running one. `join` signs an enrollment with the
node's key and sends it to the program in the validators role. Once the
epoch that seats the node begins, `join` adopts the network's state, and the
node runs like a founder. A stranger joins as a resident: it follows the
chain and never votes or proposes. A validator named in the file joins the
same way and keeps its seat.

```sh
ducktape join https://a.example.org --invite <hex>
ducktape run --listen 127.0.0.1:9000 --http 127.0.0.1:8844
```

Membership changes only by the validators' vote. The door starts closed, so
a stranger needs an invite a validator signed: `ducktape invite` prints one,
good for one enrollment for 72 hours (`--hours`). Validators vote with
`ducktape vote promote|demote|remove <key>`, `ducktape vote open|close` (the
door), and `ducktape vote registry <op>`, which sends the registry a
program change once it passes; the registry takes a change from nothing
else. A motion passes when more than two thirds of the current validators
vote for it. `ducktape leave` takes the node out; the last validator cannot
leave. Each prints the block that carried it.

The workspace (`--workspace`, default `$DUCKTAPE_HOME` else `~/.ducktape`)
holds `identity.key`, the network descriptor, the anchor the node started
from, the address its HTTP surface bound and the runtime's storage.

## Reach a node

A node's address names a transport and a host, and the host may be a name.
Peers check who they reached by the node's key, never by its IP, so the
connection may pass through a tunnel or a proxy. A connection carries
traffic both ways, so a node need not be reachable: one that joins or is
founded without an address dials the others and is never dialed. Two such
nodes never connect to each other, so the validators of a larger network
need addresses.

- `tcp://host:port`: the `--listen` socket.
- `tls://host:port`: the `--tls` socket. The node presents a certificate it
  makes at start. Peers check the node's key, not the certificate, so a TLS
  proxy in front of the `--listen` socket serves as well.
- `wss://host[:port]` or `ws://host[:port]`: a WebSocket on the `--http`
  socket, at `/v1/p2p`. A node behind an HTTPS tunnel (Cloudflare Tunnel,
  ngrok) or reverse proxy is reached at `wss://` the tunnel's name; it
  opens no port of its own.

```sh
ducktape run --listen 127.0.0.1:9000 --http 127.0.0.1:8844   # the tunnel serves https://b.example.org from 127.0.0.1:8844
```

## Use a network

```sh
ducktape status                         # height, tip, root, epoch, identity, role bindings
ducktape wallet new alice               # a user key; DUCKTAPE_PASSWORD or a prompt
ducktape submit ping request.bin        # sign a frame with the active wallet, print its preconfirmed receipt
ducktape get ping 6b6579                # a key in hex, from the preconfirmed layer
ducktape get ping 6b6579 --confirmed    # the committed layer
ducktape scan ping --lo 6b               # a key range
ducktape changes ping                   # follow a program's confirmed writes
ducktape query ping request.bin         # a signed read-only call
ducktape programs                       # every program and its code blob
ducktape blob get sha256:…              # a blob's framed bytes
```

Reads are open; writes are signed frames. Each frame names a recent block, its
anchor, and is valid only while that block is one of the last 256: the CLI
anchors to the tip `status` reports. A block runs a frame at most once. A node
takes every frame that is signed for its network, names a recent block and no
block has carried yet, and holds it in its mempool until a block carries it or
its anchor ages out. Any member's node takes a frame: it hands each frame a
client gives it to the epoch's validators over the mesh at once, and again
whenever a few blocks pass without the frame landing, so a frame submitted to
a member without a vote lands like one submitted to a validator. A block holds
at most 16 MiB of frames (`node::BLOCK_BYTES`); a frame no block can carry is
refused. The receipt `submit` prints is the node's prediction from its
preconfirmed layer, not a verdict: a frame predicted to fail is still relayed
and waits for a block, and the block decides. `ducktape logs`,
`ducktape log-filter`, `ducktape metrics` and `ducktape shutdown` operate the
node; the mutating ones are signed with the node's own identity key. Each
verb's `--help` carries the rest.

## How it is built

| Layer | Where | What |
| --- | --- | --- |
| Kernel | `crates/kernel/` | `abi` (the bytes ABI), `guest` (what a program compiles against), `runtime` (the wasmtime embedding), `state` (the authenticated store and its commitments), `blobs` (one content-addressed store), `host` (the sandbox: submit, query, and the messages a frame emits, run in that frame), `node` (frames, blocks, the mempool), `consensus` (Simplex BFT over marshal, per-epoch engines, catch-up), `statesync` (a joiner adopts a network's state); `fixtures/` is its own workspace of wasm32 test programs |
| Programs | [`ducktape-industries/modules`](https://github.com/ducktape-industries/modules) | The contracts a program compiles against (`crates/sdk/abi`, a copy of `crates/kernel/abi` here, and `crates/sdk/guest`, the module SDK), the boot set (`crates/system/`: `module-registry`, `valset` and `identity`, whose suite founds this host over their bytes) and the app modules. The system modules beyond the boot set are archived at `ducktape-industries/ducktape-system-modules-archive` |
| Network | `crates/networking/halo/` | `halo`, the network between members off consensus: QUIC on UDP at a member's tcp or tls port, a tcp, tls, ws or wss stream path where UDP does not answer, admission of current members by node key, lanes, the egress pacer and its priority classes, and the blob lane a node fetches attached blobs over |
| Daemon | `crates/noded/`, `bin/node/` | The `/v1` HTTP and WebSocket surface, the lookup mesh, the workspace on disk, the client, and the `ducktape` binary |

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
