# Ducktape

**A BFT-replicated sandbox for wasm programs.** A network is a set of
validators running the same programs over one authenticated state. Anyone
with a wallet key submits a signed frame; consensus orders it; every node
applies it and commits one root. Two nodes that agree on that root agree on
everything.

Every program is wasm the host runs against its own key space.
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
[programs](https://github.com/ducktape-industries/programs) publishes a
founding for each commit on its `main`: the release `wasm-<commit>` carries
`founding.tar.gz`, which holds every program it builds, packed with its
view, and a `founding.toml` with only its network, time, key, address and
halo address left to fill. The fields:

```toml
network = "mynet"
time = 1700000000000          # the genesis block's time, unix milliseconds
epoch_length = 64             # blocks per validator epoch
block_time_ms = 1000
member_cap = 16               # optional, default 16: the most members (validators and residents) it ever holds, 1 to 128; fixed at founding
hooks = ["activity"]          # optional: founding programs the kernel tells of every frame; the programs role holds the list from here on

[roles]                       # the founding program the kernel calls in each role; egress optional, the rest required
programs = "programs"         # registers and swaps programs
validators = "valset"         # seats each epoch's validators; runs membership and the validators' votes
identity = "identity"         # resolves a key or a program to its account
blobs = "blobs"               # records each bundle of blobs put and which validators hold it
awaits = "awaits"             # keeps the tickets of frames that await a call, and hands each its answer
egress = "egress"             # optional: answers programs' calls outside the chain; nodes read it, the kernel never calls it

[[validators]]
key = "…"                     # hex ed25519 public key: `ducktape identity`
address = "tcp://a.example.org:9000"  # optional: where peers dial it; see Reach a node
halo = "a.example.org:9100"   # host:port where members reach it over halo; `init --halo` fills this node's

[[programs]]
id = "programs"               # one entry per program; the programs and validators roles' params are the kernel's
code = "programs.wasm"

[[programs]]
id = "valset"
code = "valset.wasm"

[[programs]]
id = "identity"
code = "identity.wasm"

[[programs]]
id = "blobs"
code = "blobs.wasm"

[[programs]]
id = "awaits"
code = "awaits.wasm"

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
off the programs role, it gives the program an account: the identity role's
`RegisterProgram`, executed with the `System` origin. Once per frame the host
asks the identity role who the frame acts as, `Account(key)` for a signed
frame and `OfProgram(program)` for a message or a reply, and passes the answer
as `Env.sender`; a refusal rejects the frame, and a key that holds no account
runs as no one (`None`). A program may send a message as another account
(`Message.acting`, the program SDK's `emit_as`): the host asks the identity role
`Runs { program, account }` and runs the message as that account only when
the role answers with it, so an agent account its manager hands to a program
acts through that program with no key held anywhere. The interfaces are `abi::role::{programs,
validators, identity, awaits, egress}`; the programs repository's `docs/roles.md` walks through them.

A program can wait for another program's answer. Its `execute` is an async
Rust function (the program SDK's `awaiting_program!`), and each
`ctx.call(target, op).await` takes a ticket from the host. A call may act as
an account the program runs, as a message may (`call_raw_as`), and the host
asks the identity role `Runs` the same way. When the run ends still waiting,
the host runs each called program with `Cause::Call { ticket }`.
A called program that answers in that run, with its output or its refusal,
resumes the waiting program in the same submission. One that cannot answer
yet calls `defer` and keeps the ticket. The host then stores the waiting
program's memory in `$parked`, only the 4 KiB pages that differ from a fresh
instance, and tells the awaits role which tickets the frame waits on. Later
the called program sends `resolve`, a message to the awaits role. That role
checks that the sender is the program the ticket called, and the host
restores the memory and resumes the program inside the resolver's
submission, on the resolver's fuel. A ticket nobody answers expires after
`EXPIRY_BLOCKS`: anyone may then send the awaits role `Expire`, and the
program resumes with a refusal. A frame that finishes answers whoever waited
on it: the emitter of a message that wanted a reply gets its `Completion`
then, and a waiting caller gets its answer. When the programs role gives a
program new code, the host ends every frame that program had parked and
answers each waiter with a refusal. Each run between two waits is its own
transaction, so a refusal undoes only the run it ends. Storing a frame and
restoring it each cost `PAGE_FUEL` per stored page.

A hook is a program the kernel tells of every frame a block carries, after
the frame has run: at each block it asks the programs role `Hooks(height)` and,
after each frame, calls every hook it named with `ProgramCall::Hook` and an
`abi::hook::Record` (the height, the frame's position in the block, its
signer, the account it acted as, its target and whether it applied), from
the `System` origin on a fixed budget of `HOOK_FUEL`. A hook never changes
the frame: one that refuses, traps, spends its fuel or sends a message has
its own writes undone, and the block goes on. The founding file's `hooks`
seeds the programs role's list; a running network hooks or unhooks a program
through the programs role like any other change.

```sh
curl -fsSL https://github.com/ducktape-industries/programs/releases/download/wasm-<commit>/founding.tar.gz | tar xz
ducktape identity                                   # mint this workspace's node key
$EDITOR founding/founding.toml                      # network, time (`date +%s000`), that key, this node's address
ducktape init founding/founding.toml --halo a.example.org:9100   # found the network in $DUCKTAPE_HOME
ducktape run --listen 127.0.0.1:9000 --http 127.0.0.1:8844 --halo 0.0.0.0:9100
```

Any node joins through a running one. `join` signs an enrollment with the
node's key and sends it to the program in the validators role. Once the
epoch that seats the node begins, `join` adopts the network's state, and the
node runs like a founder. A stranger joins as a resident: it follows the
chain and never votes or proposes. A validator named in the file joins the
same way and keeps its seat.

```sh
ducktape join https://a.example.org --halo b.example.org:9100 --invite <hex>
ducktape run --listen 127.0.0.1:9000 --http 127.0.0.1:8844 --halo 0.0.0.0:9100
```

Membership changes only by the validators' vote. The door starts closed, so
a stranger needs an invite a validator signed: `ducktape invite` prints one,
good for one enrollment for 72 hours (`--hours`). Validators vote with
`ducktape vote promote|demote|remove <key>`, `ducktape vote open|close` (the
door), and `ducktape vote programs <op>`, which sends the programs role a
program change once it passes; the programs role takes a change from nothing
else. A motion passes when more than two thirds of the current validators
vote for it. `ducktape leave` takes the node out; the last validator cannot
leave. Each prints the block that carried it.

The workspace (`--workspace`, default `$DUCKTAPE_HOME` else `~/.ducktape`)
holds `identity.key`, the network descriptor, the anchor the node started
from, the address its HTTP surface bound, the runtime's storage and, when the
operator writes one, `egress.toml`.

## Answer calls outside the chain

A program reaches something outside the chain through the program bound to the
egress role (`abi::role::egress`): it calls that program with a `Request`, the
chain elects a member node serving the request's service, and that node answers
in a later block. A `Request::Http` names a service and an `http::Head`: for the
reserved `http` service the target is a full URL on the public internet, for any
other service a path under the base the node's operator gives it. `relay` is the
other reserved service: it is answered with nothing, a later block's call back.
Any other service is registered on chain by an account, which then assigns the
node keys that may serve it; the chain elects among the assigned members that
serve it.

The node does this itself. After each block it applies, it asks the egress
program which open requests the chain elected it for and which are past their
deadline. It calls each new one and signs the answer with its own key, and it
sends `Expire` for each overdue one. It also sends `Serve` while the chain's
record of the services it serves differs from its `egress.toml`. It submits each
of these frames again every block until a block carries it, and signs one again
when its anchor grows old. An answer larger than a block carries goes out as a
failure. On a network that binds no egress role the node does none of this.

```toml
# $DUCKTAPE_HOME/egress.toml: read when the node starts; absent means it serves nothing
relay = true                  # answer `relay` requests
http = true                   # answer `http` requests, to the public internet
private = false               # optional: let `http` reach loopback, private and link-local addresses

[services.ccs]                # a registered service this node fronts
base = "http://127.0.0.1:4141"
secrets = { x-api-key = "CCS_KEY" }   # header = the environment variable holding its value
```

A service request goes to `base` joined with its path; a path that would leave
`base` fails. The node adds the service's secrets, and it adds
`ducktape-chain`, `ducktape-program` (the program that asked) and
`ducktape-request` (the request id) so a proxy in front of the service can decide
what each program may do. A request may set none of those headers itself, nor
`host`, `content-length`, `transfer-encoding` or `connection`. An `http` request
gets no secrets and no `ducktape-*` headers, and reaches only public addresses,
checked for an address in the URL and after the DNS lookup, unless `private =
true`. No redirect is followed and no proxy is used: the whole response,
redirects included, is the answer, and every node can read it. The node acts as
an HTTP proxy in both directions: it forwards no hop-by-hop header
(`connection` and every header it names, `proxy-connection`, `keep-alive`, `te`,
`transfer-encoding`, `upgrade`, `proxy-authenticate`, `proxy-authorization`),
so none of them is sent upstream or written into an answer.

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

Halo is the network members move blobs over. It runs QUIC on UDP, apart
from the mesh. Every member registers a halo address, `host:port`, that the
other members dial: a founder in the founding file or with `init --halo`, a
joiner with `join --halo`, and any member changes its own with
`ducktape halo <host:port>`. A registered address with an unspecified or
loopback host (`0.0.0.0`, `::`, `127.0.0.1`, `localhost`) is refused. `run
--halo` names the UDP socket the node binds, where `0.0.0.0:<port>` is
right. Each node dials every member once it is seated and keeps that
connection open, and a connection carries streams both ways, so a member
whose halo address takes no inbound UDP still takes part by dialing out. A
changed address is dialed from the next block on. A dial to a member that
never answers ends when QUIC's idle timeout passes, and the node dials again.
`--halo-egress` caps the bytes per second a node sends over halo;
without it the node sends as fast as the path allows.
`--halo-inbound-streams` is how many streams members open to the node wait
for it to take them; past it the node takes no more until one is taken.
`--halo-datagrams` is how many datagrams from members wait for the node to
read them; past it a datagram is dropped.

Five more `run` flags set how much work a node takes on at once.
`--reads-in-flight` and `--uploads-in-flight` are the HTTP reads and the
uploads (submit, blob put) it serves together; the rest wait their turn.
`--blob-serves-in-flight` and `--blob-gatherers` are the blobs it sends to
members and gathers from members together. `--blob-backlog` is how many
members' blob requests wait for a send; past it a request is refused.
`ducktape run --help` shows each default.

```sh
ducktape run --listen 127.0.0.1:9000 --http 127.0.0.1:8844 --halo 0.0.0.0:9100   # the tunnel serves https://b.example.org from 127.0.0.1:8844
```

## Use a network

```sh
ducktape status                         # height, tip, root, epoch, identity, role bindings
ducktape wallet new alice               # a user key; DUCKTAPE_PASSWORD or a prompt
ducktape submit ping request.bin        # sign a frame with the active wallet and pool it; --preconfirm prints its predicted receipt
ducktape get ping 6b6579                # a key in hex, from the preconfirmed layer
ducktape get ping 6b6579 --confirmed    # the committed layer
ducktape scan ping --lo 6b               # a key range
ducktape changes ping                   # follow a program's confirmed writes
ducktape query ping request.bin         # a signed read-only call
ducktape programs                       # every program and its code blob
ducktape blob get sha256:…              # a blob's framed bytes
ducktape blob put code.wasm --kind program --wait  # put a bundle, print its ids, wait until it is held
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
refused.

Large bytes travel as blobs, never in a frame. `ducktape blob put FILE…
--kind K` frames each file as a blob of kind `K` and uploads the files to the
node together with a `Put` frame, signed by the active wallet key's account,
that names each blob by id, length and kind. The node keeps the bytes only
when the frame is a valid put naming exactly those blobs, then submits it.
The program in the blobs role records the put as a numbered bundle. Every
member gathers every bundle over halo, asking the member that relayed the
put first, then the validators, then the residents. A validator that holds
every blob of a bundle, each checked against its id and declared kind, signs
a receipt with its node key. Once two thirds of the current validators have
receipted a bundle, the blobs program marks it held, and stops listing who
receipted. `--wait` follows the blobs program's confirmed writes until then.
Consensus never waits on a blob: a program reads a blob that a program put
or a held bundle names (`ctx.blob_get`), and a node that lacks one when a
block reads it fetches it over halo before it applies the block. New program
code is a bundle of kind `program`: the programs role schedules only code the
blobs program reports held.

`submit` pools the frame and prints its id; the block that carries it
decides its outcome. The node runs a frame before its block only when the
submit asks with `--preconfirm`: the frame's writes then show in the
preconfirmed layer, the node runs it again after each block until a block
carries it, and the receipt `submit` prints is that prediction, not a verdict.
A frame predicted to fail is still relayed and waits for a block. `blob put`,
votes and `ducktape halo` never preconfirm. `ducktape logs`,
`ducktape log-filter`, `ducktape metrics` and `ducktape shutdown` operate the
node; the mutating ones are signed with the node's own identity key. Each verb's
`--help` carries the rest.

## How it is built

| Layer | Where | What |
| --- | --- | --- |
| Kernel | `crates/kernel/` | `abi` (the bytes ABI), `program` (the small program SDK the fixtures compile against), `runtime` (the wasmtime embedding), `state` (the authenticated store and its commitments), `host` (the sandbox: submit, query, and the messages a frame emits, run in that frame), `node` (frames, blocks, the mempool), `consensus` (Simplex BFT over marshal, per-epoch engines, catch-up), `statesync` (a joiner adopts a network's state); `fixtures/` is its own workspace of wasm32 test programs |
| Programs | [`ducktape-industries/programs`](https://github.com/ducktape-industries/programs) | The contracts a program compiles against (`crates/sdk/abi`, a copy of `crates/kernel/abi` here, and `crates/sdk/program`, the full program SDK), the boot set (`crates/system/`: `programs`, `valset`, `identity`, `blobs` and `awaits`, whose suite founds this host over their bytes) and the app programs. The system programs beyond the boot set are archived at `ducktape-industries/ducktape-system-modules-archive` |
| Blobs | `crates/blob/` | `blob`: the framing a blob's id hashes and the blobs program's contract (wasm32, no features); the content-addressed store (feature `store`); and the service a node gathers, serves and receipts blobs with over halo (feature `node`) |
| Network | `crates/networking/halo/` | `halo`, the network between members off consensus: QUIC on UDP at each member's registered halo address, admission of current members by node key, lanes, the egress pacer and its priority classes |
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
