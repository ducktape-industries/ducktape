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

`.github/workflows/release.yml`, which published a built `ducktape` for each
commit on `main` as the prerelease `bin-<commit>`, is still in the tree, but
the workflow is disabled: build the binary with `make install-node`.

## Found a network

A founding file names the network, its cadence, its validators and the
programs it starts with. Every path is relative to the file.
programs' `make wasm-founding` packs a founding: `founding.tar.gz` holds
every program it builds, packed with its view, and a `founding.toml` with
only its network, time, key, address and halo address left to fill. A local
network and a hosted one both found from it. A hosted network runs it at a
commit on forge `programs` `dev` (`AGENTS.md` § Branching and Delivery), and
[`docs/found-a-network.md`](docs/found-a-network.md) walks it through every
step, from the founding to agents that answer. programs' release workflow,
which published a founding as the GitHub release `wasm-<commit>`, is still
in its tree, but the workflow is disabled. The fields:

```toml
network = "mynet"
time = 1700000000000          # the genesis block's time, unix milliseconds
epoch_length = 64             # blocks per validator epoch
block_time_ms = 1000
member_cap = 16               # optional, default 16: the most members (validators and residents) it ever holds, 1 to 128; fixed at founding
hooks = ["activity"]          # optional: founding programs the kernel tells of every frame; the programs role holds the list from here on

[[validators]]
key = "…"                     # hex ed25519 public key: `ducktape identity`
address = "tcp://a.example.org:9000"  # optional: where peers dial it; see Reach a node
halo = "a.example.org:9100"   # host:port where members reach it over halo; `init --halo` fills this node's

[[programs]]
id = "programs"               # one entry per program; the programs and validators roles' params are the kernel's
code = "programs.wasm"
roles = ["programs"]          # the roles it fills; a program that fills none leaves this out

[[programs]]
id = "valset"
code = "valset.wasm"
roles = ["validators"]

[[programs]]
id = "identity"
code = "identity.wasm"
roles = ["identity"]

[[programs]]
id = "blobs"
code = "blobs.wasm"
roles = ["blobs"]

[[programs]]
id = "awaits"
code = "awaits.wasm"
roles = ["awaits"]

[[programs]]
id = "halo"
code = "halo.wasm"
params = "halo.params"        # the founding lanes, in order from id 0, each pinned or not
roles = ["halo"]

[[programs]]
id = "access"
code = "access.wasm"
roles = ["access"]

[[programs]]
id = "ownership"
code = "ownership.wasm"
roles = ["ownership"]

[[programs]]
id = "ping"
code = "ping.wasm"
params = "ping.params"        # optional: the bytes the program's init receives

[limits]                      # optional; absent means unmetered
fuel = 1000000000
memory_bytes = 268435456
```

The kernel calls programs by role, never by id. The roles are a table, a map
from role name to program (`abi::Roles`; the names are constants in
`abi::role`), so a new role is a new row. A program declares the roles it
fills where it is published: `roles` on its `[[programs]]` entry at founding,
and on the change that publishes it after. The programs role keeps the table
and answers it (`programs::Query::Roles`); the kernel reads it each block with
the roster and keeps it in its own state. Founding refuses a role two programs
declare and a table missing a role in `abi::role::REQUIRED` (programs,
validators, identity, blobs, awaits, halo, access), naming the role; a roster
whose table misses one, or names a program the roster does not run, changes
nothing. Every frame's env carries the table (`Env.roles`); a program looks a
role up by name and gets nothing for a role the network does not fill.

Before an op runs, the kernel asks its target program what the op touches
(`ProgramCall::Access`, answered as `role::access::Need`s: a key in the
program's own space and whether the op writes or deletes it). When the answer
names anything, the program filling the access role checks it for the parties
the frame acts as (the account, and the sending program for a message) before
the op runs, as part of the op: a refusal refuses the op, and the check's
writes fall with a refused op. The access program keeps the lists owners set;
the owner of a key and the list that holds until one is set are the target
program's to say (`ProgramCall::Resource`), which the access program asks
through `HostOp::Resource`. A key shaped like a path answers to the nearest
list on it or above it, and no list anywhere means anyone may act. A query is
never checked.

Every key a program writes is shared unless the program claims it as it
creates it: `HostOp::SetOwned { key, value, kind }` writes as `Set` does and
claims the key as an item of `kind`, while `Set` and `Delete` claim nothing.
While an op runs, or a parked op resumes, on a network that binds a program
to the ownership role, the host notes the first time the run touches each key:
whether it held a value before the run, and whether the ownership program
holds a record for it, read in that program's state at
`role::ownership::record_key(program, key)`. Once the run returns, and before
anything it emitted is delivered, the host makes one call to the ownership
program, `role::ownership::Op::Guard`, from the `System` origin: the program
written, the frame's origin and sender, and each key the run created with a
claim or changed while it had a record, with the kind it was claimed as,
whether it held a value before the run and whether it holds one after it. A
claim on a key that already held a value is an ordinary write. The ownership
program decides; its refusal undoes the run, and a guard that sends, calls,
defers or parks is refused. A run that claimed no new key and changed no
recorded key makes no call, and the kernel's own runs (`System` origin), a
message the program filling the validators role sent, and the ownership
program's own runs are not guarded. The ownership role is an optional row: on
a network that binds no program to it the host notes nothing and makes no
call, `SetOwned` writes as `Set` does, and every frame
runs as it would without the guard. As the kernel admits a
program, at founding or later off the programs role, it gives the program an
account: the identity role's
`RegisterProgram`, executed with the `System` origin. Once per frame the host
asks the identity role who the frame acts as, `Account(key)` for a signed
frame and `OfProgram(program)` for a message or a reply, and passes the answer
as `Env.sender`; a refusal rejects the frame, and a key that holds no account
runs as no one (`None`). A program may send a message as another account
(`Message.acting`, the program SDK's `emit_as`): the host asks the identity role
`Runs { program, account }` and runs the message as that account only when
the role answers with it, so a program account whose script calls a program
acts through that program with no key held anywhere. The interfaces are `abi::role::{programs,
validators, identity, awaits, egress, scheduler, halo, access, ownership}`; the programs repository's `docs/roles.md` walks through them.

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
submission, on the resolver's fuel. A ticket stays open until the called
program answers it; the kernel puts no deadline on a wait. A frame that finishes answers whoever waited
on it: the emitter of a message that wanted a reply gets its `Completion`
then, and a waiting caller gets its answer. When the programs role gives a
program new code, the host ends every frame that program had parked and
answers each waiter with a refusal. Each run between two waits is its own
transaction, so a refusal undoes only the run it ends. Storing a frame and
restoring it each cost `PAGE_FUEL` per stored page.

A program can schedule a run for a later block through the program bound to
the scheduler role (`abi::role::scheduler`), an optional row. It sends that
program `Schedule { height, target, payload }` (the program SDK's
`ctx.schedule`) and gets back an entry id; the scheduler records the sending
program as the entry's owner, so a program schedules only as itself, and it
refuses a height that is not later than the current one. Only the owner may
`Cancel` an entry, and only before it runs. At the end of each block, after
its submissions and hooks and before the epoch step, the host asks the
scheduler `Due { height }` page by page and runs each entry still due: a
frame from `Origin::Program(owner)`, acting as the owner's account, with
`Cause::Scheduled { entry }`, executing `payload` on `target` under the
network's per-frame limits. A refused run undoes only its own writes. The
host then sends the scheduler `Ran { entry, receipt }` from the `System`
origin, and the scheduler keeps the receipt on the entry; the block does not
carry the receipts of scheduled runs. A scheduled run may schedule entries
for later heights. On a network that binds no scheduler role nothing runs.

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

Each node keeps an activity index of its own, in `activity/` under its data
directory, which no commitment covers and state sync never ships: every frame
a block carried (its height, position, time, signer, target and whether it
applied), newest first network-wide, under its signer and under the account it
acted as, each account's count of frames per program, and the refusal of
each frame that did not apply. The host returns a
block's records with its receipts (`host::Applied`'s `records`), and a thread
of the node's own writes them after the block, so the index may lag the chain
but never holds a block up. `POST /v1/activity` reads it: a borsh
`noded::wire::activity::Query` (`Acted`, `Signed`, `Used` and `Frames`, each a
page resumed by its `next`, and `Indexed`, the first height whose frames it
holds and the newest it has indexed, and `Refusal`, why the frame at a height
and position did not apply) answered by a `Reply`. An index opens at
the node's tip, seeded from the `activity` program's tables where the network
still runs that program, so a node's history starts where its index did.

```sh
make -C programs wasm-founding                      # a programs checkout; a hosted network's is at a forge dev commit
tar xzf programs/target/wasm32-unknown-unknown/release/founding.tar.gz
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
door), `ducktape vote role <role> <op>`, which runs `op` on the program bound
to the role once it passes (`programs` takes a program change, `halo` a lane
to open, set or close; neither takes a change from anything else). A role
moves only with the program change that publishes the program declaring it,
which only the validators' vote schedules. A motion passes when more than two thirds of
the current validators vote for it. `ducktape leave` takes the node out; the last validator cannot
leave. Each prints the block that carried it.

The workspace (`--workspace`, default `$DUCKTAPE_HOME` else `~/.ducktape`)
holds `identity.key`, the network descriptor, the anchor the node started
from, the address its HTTP surface bound, the runtime's storage and, when the
operator writes them, `node.toml` and `egress.toml`.

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

A request waits `wait` blocks for its answer, `DEFAULT_WAIT` (64) when the
caller names none. The egress program schedules its own expiry through the
scheduler role when it records the request, and cancels that entry when the
answer comes. If the expiry runs first, egress drops the request and the
caller's call ends with a refusal saying the request expired; a later answer
is refused. A network that binds egress without a scheduler refuses every
request. No node signs anything to expire a request.

A request also says how its reply comes back. `Delivery::Whole` asks for one
answer: the whole response, or a failure. `Delivery::Events { deliver }` asks
for the response's events as they arrive; only the requester can ask its
service for a stream (an Anthropic request sets `"stream": true` in the body it
builds). When such a request's response is `text/event-stream`, the node reads
the body as it arrives and sends its complete events in `Op::Events` frames,
in order: each `Event`'s `seq` counts from 0 and `data` is the event's lines as
received, without the blank line that ends it. Egress hands each event to the
requester as `deliver` followed by the encoded `Event`, each `seq` once and in
order whatever order the frames land in: it keeps an event that lands early
until the ones before it arrive, drops a repeat, and holds the end until every
event it counts is heard. It pushes the request's expiry `wait` blocks past
each block that carries a frame. When the body ends, the node sends `Op::EndStream` with
the number of events it sent and `StreamEnd::Complete`, the response's status
and headers. When the body breaks off, it sends `StreamEnd::Cut` with the
reason `stream_broken`. An event unended when the body ends is dropped. Any
other response to an events request, and every response to a whole request,
comes back whole.

The node answers requests itself. After each block it applies, it asks the
egress program which open requests the chain elected it for. It calls each new
one and signs the answer with its own key. It also sends `Serve` while the chain's
record of the services it serves differs from its `egress.toml`. It submits each
of these frames again every block until a block carries it, and signs one again
at once when its anchor grows old. A stream's frames go out as its events
arrive: the node sends the next frame as soon as the one before is in its pool,
without waiting for a block to carry it, and that frame carries every event
read since, or the end. Several frames of one stream may wait for blocks at
once. The node's own blocks carry its pool in the order it took them, but the
relay to the validators may drop a frame and offer it again blocks later, so on
a network of several validators frames may land out of order; egress puts the
events back in order. Events together larger than a block carries go out in
halves, in order. An answer larger than a block carries goes out as a
failure; one event or an end larger than a block carries ends the stream `Cut`
(`event_too_large`, `end_too_large`) and the node drops the rest. On a network that binds no egress
role the node does none of this.

```toml
# $DUCKTAPE_HOME/egress.toml: read when the node starts and on `ducktape egress reload`;
# absent means it serves nothing
relay = true                  # answer `relay` requests
http = true                   # answer `http` requests, to the public internet
private = false               # optional: let `http` reach loopback, private and link-local addresses

[services.anthropic]          # a registered service this node fronts; an agent's model record names it
base = "http://127.0.0.1:4141"   # a model gateway, such as ccs
secrets = { x-api-key = "CCS_KEY" }   # header = the environment variable holding its value

[services.pods]
base = "http://10.0.0.5:8080"
secrets = { authorization = { file = "pods.key" } }   # header = a file, under the workspace
```

A secret is the name of an environment variable or `{ file = "<path>" }`, a
file read whole with its surrounding whitespace trimmed; a relative path is
under the workspace. The node reads every secret each time it loads the file.
A variable is set when the process starts, so a new secret that must arrive
while the node runs is a file.

`ducktape egress reload` makes the running node load `egress.toml` again; no
restart is needed. From the next request on, the node calls each service at its
new base with its new secrets, and it sends `Serve` while the chain's record
differs from the new list. A request already being called finishes against the
service it started with. If the file does not load (it does not parse, a
variable is not set, a secret file does not read), the node refuses the reload
with the reason and the problem, and it keeps serving what it served before.

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

## Configure a node

`ducktape run` reads `node.toml` from the workspace. The file is optional,
and every key in it is too. Every `ducktape run` flag has a key, named like
the flag with `_` for `-`. A flag overrides the file, and the file overrides
the built-in default. `listen` and `halo` have no default, so the node
refuses to start unless the flag or the file names each. A key the file does
not know is an error, so a misspelled key stops the node instead of being
ignored. `egress.toml` stays its own file.

```toml
listen = "127.0.0.1:9000"     # --listen; no default
http = "127.0.0.1:8844"       # --http; default 127.0.0.1:0
tls = "127.0.0.1:9443"        # --tls; default none
halo = "0.0.0.0:9100"         # --halo; no default
live = "0.0.0.0:9200"         # --live; default none: desktop clients take the websocket on the http port
log = "info"                  # --log; default info
block_cpus = "0-5"            # --block-cpus; default none: block and rpc threads share every CPU

[load]
halo_egress = 10000000        # --halo-egress; default unlimited
halo_inbound_streams = 512    # --halo-inbound-streams
halo_datagrams = 2048         # --halo-datagrams
blob_serves_in_flight = 32    # --blob-serves-in-flight
blob_backlog = 128            # --blob-backlog
blob_gatherers = 8            # --blob-gatherers
live_session_inbound = 1024   # --live-session-inbound
live_session_outbound = 4096  # --live-session-outbound
live_group_queue = 256        # --live-group-queue
live_peer_queue = 1024        # --live-peer-queue
live_lane_backlog = 64        # --live-lane-backlog
live_lane_per_sender = 256    # --live-lane-per-sender
live_linger_ms = 5000         # --live-linger-ms

[consensus]
leader_timeout_ms = 1000          # --leader-timeout-ms; default one block time
certification_timeout_ms = 30000  # --certification-timeout-ms; default 30 block times
timeout_retry_ms = 2000           # --timeout-retry-ms; default 2 block times
```

The top level says where the node is reached, what it logs and which CPUs
its block threads run on. With `block_cpus` set (a Linux CPU list such as
`0-5` or `0-3,8`), `ducktape run` pins its first thread to those CPUs before
the block runtime starts, so every thread that orders, applies and stores
blocks inherits them, and the RPC runtime takes every other CPU the process
may use. Fence the same CPUs off from everything else on the host (a cpuset,
or the static CPU manager for a Guaranteed pod), or other work still lands
on them. A CPU the process may not use stops the node. `[load]`
says how much work the node takes on at once and how fast it sends over
halo; "Reach a node" describes each of its keys, and `ducktape run --help`
shows each default. A count in `[load]` is above zero, and every count but
`blob_gatherers` is at most what a tokio semaphore holds. `[consensus]`
holds the consensus timeouts.

The block time is the network's, from the founding file, so the defaults
follow it. A view gives up on its leader after the leader timeout and on
certifying a proposal after the certification timeout. Once a view has timed
out, the node resends its timeout vote every retry interval. The node skips a
view after the certification timeout plus one block time, and gives up on a
fetch after one block time. `ducktape run` refuses to start unless the leader
timeout is above zero, the certification timeout exceeds the leader timeout,
and the retry interval is above zero and below the skip timeout.

A peer link is dropped and dialed again after twenty block times in which
no bytes move on it. A message that is still arriving keeps its link alive
however long it takes, so a large block on a slow link is never cut off. The
node pings each peer every five block times, so a healthy idle link is never
silent that long. Both follow the network's block time, not the consensus
timeouts, so every node uses the same values. The node's `node_started` log
event carries the effective timeouts, the link timeout and the ping interval.

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

Three more `run` flags set how much blob work a node takes on at once.
`--blob-serves-in-flight` and `--blob-gatherers` are the blobs it sends to
members and gathers from members together. `--blob-backlog` is how many
members' blob requests wait for a send; past it a request is refused.
The live relay's queues are `run` flags too: `--live-session-inbound` and
`--live-session-outbound` are the messages waiting for one client session's
loop and for its connection (a session that far behind is closed),
`--live-group-queue` the objects waiting for one group stream's writer (past
it the group is dropped short), `--live-peer-queue`, `--live-lane-backlog` and
`--live-lane-per-sender` the lane items waiting for one peer's writer, the
peers' streams waiting to be taken and the datagrams waiting per peer.
`--live-linger-ms` is how long a
closed session's last message gets to reach the client before the connection
is closed under it. `ducktape run --help` shows each default and its
`node.toml` key.

Halo carries lanes, each a named kind of traffic (blobs ride `ducktape/blobs`). The halo
role's program holds the lane table: each lane's name, the id its streams and datagrams carry
on the wire, the highest class it is served at, whether it is served, and whether it is pinned.
A node reads the table at start and again after any block that writes it or swaps the halo
program's code, so every node changes lanes at the same block. Only the validators' vote
changes it (`ducktape vote role halo <op>`, an `abi::role::halo::Op`): it opens a lane at the next
id, sets a lane's class or pauses it, or closes one; an id is never handed out twice. A lane is pinned only at founding, and no
vote pauses or closes a pinned lane. Pin `ducktape/blobs`: a node fetches the blobs a block
reads over it, so a vote that paused it would cut a node off from the very block that undoes
the vote. A stream is served at the lower of the class it asks and its lane's cap. A stream
already open when its lane is paused or closed runs on.

Live tracks: the relay in every node (`crates/noded/src/live/`) lets a desktop
client publish and subscribe to tracks of a room off consensus, in what the
room's program granted its account under the `live` table, over QUIC on a UDP
socket of its own (`run --live 0.0.0.0:9200`, ALPN `ducktape-live`, the node
key's certificate) or the WebSocket `/v1/live`; nodes carry a track between
their clients over halo's lane `ducktape/live`. `GET /v1/live/port` names the QUIC port, or
none without `--live`, and then clients take the WebSocket. A node built before the route
answers 404, which the app reads as live unavailable, so the route moves no contract number
(`noded::NODE_CONTRACT` stays 9). The clocks a client must honour are `live-wire`'s:
`HANDSHAKE_WAIT` for each step of the handshake, `KEEP_ALIVE` and `IDLE` (a connection silent
that long is closed, on either path), and `IDLE_SESSION` for a session holding nothing.

```sh
ducktape run --listen 127.0.0.1:9000 --http 127.0.0.1:8844 --halo 0.0.0.0:9100   # the tunnel serves https://b.example.org from 127.0.0.1:8844
```

## Use a network

```sh
ducktape status                         # height, tip, root, epoch, identity, role bindings
ducktape wallet new alice               # a user key; DUCKTAPE_PASSWORD or a prompt
ducktape op identity '{"CreateAgent": {"name": "chief"}}' --land  # an op as the JSON its program's schema reads; prints the block and what it returned
ducktape submit ping request.bin        # sign a frame with the active wallet and pool it; --preconfirm prints its predicted receipt; --land waits for its block's receipt
ducktape get ping 6b6579                # a key in hex, from the preconfirmed layer
ducktape get ping 6b6579 --confirmed    # the committed layer
ducktape scan ping --lo 6b               # a key range
ducktape changes ping                   # follow a program's confirmed writes
ducktape query files '"Head"'           # a signed read-only call, written and answered as JSON
ducktape query ping --raw request.bin   # the same with borsh bytes, from a file or stdin
ducktape wallet consent --account 18    # the active key's consent to a key joining account 18, as AddKey's JSON
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
The prediction runs over the newest block the node has finished applying and
the frames it already holds, so it can be answered while the next block is
still being applied, and that block can change the outcome. After each block
the node checks every frame it holds again and drops the ones no block may
carry any more; a dropped frame never shows in `/v1/tx`.
`submit --land` waits for the verdict instead: it opens the `/v1/changes`
socket on `$recent`, submits the frame to the pool through `/v1/submit`,
prints `pooled <id>`, and reads each new block through `/v1/block` until one
carries the frame. It then prints `landed <height> applied <output>` or
`landed <height> rejected <reason>: <sentence>` and the receipt's events, or
`unlanded` once the chain passes the frame's anchor window with no block
carrying it, which is how a frame dropped at the check ends.
A frame predicted to fail is still relayed and waits for a block. `blob put`,
votes and `ducktape halo` never preconfirm. `ducktape logs`,
`ducktape log-filter`, `ducktape egress reload`, `ducktape metrics` and
`ducktape shutdown` operate the node; the mutating ones are signed with the
node's own identity key. Each verb's
`--help` carries the rest.

`ducktape shutdown`, SIGTERM and SIGINT stop `ducktape run` the same way.
The node stops taking blocks and lets the block it is applying finish. It
closes every `/v1/changes` socket with a going-away close frame, flushes its
state to disk and exits 0, so the next start replays no write log. A signal
that arrives while the node is still opening its state ends the start at
once.

## How it is built

| Layer | Where | What |
| --- | --- | --- |
| Kernel | `crates/kernel/` | `abi` (the bytes ABI), `program` (the small program SDK the fixtures compile against), `runtime` (the wasmtime embedding), `state` (the authenticated store and its commitments), `host` (the sandbox: submit, query, and the messages a frame emits, run in that frame), `node` (frames, blocks, the mempool), `consensus` (Simplex BFT over marshal, per-epoch engines, catch-up), `statesync` (a joiner adopts a network's state); `fixtures/` is its own workspace of wasm32 test programs, which `fixture-wasm` compiles from source whenever the test suites build |
| Programs | [`ducktape-industries/programs`](https://github.com/ducktape-industries/programs) | The contracts a program compiles against (`crates/sdk/abi`, a copy of `crates/kernel/abi` here, and `crates/sdk/program`, the full program SDK), the boot set (`crates/system/`: `programs`, `valset`, `identity`, `blobs`, `awaits` and `halo`, whose suite founds this host over their bytes) and the app programs. The system programs beyond the boot set are archived at `ducktape-industries/ducktape-system-modules-archive` |
| Blobs | `crates/blob/` | `blob`: the framing a blob's id hashes and the blobs program's contract (wasm32, no features); the content-addressed store (feature `store`); and the service a node gathers, serves and receipts blobs with over halo (feature `node`) |
| Network | `crates/networking/halo/`, `crates/networking/live-wire/` | `halo`, the network between members off consensus: QUIC on UDP at each member's registered halo address, admission of current members by node key, lanes bound by name over the halo role's table, the egress pacer and its priority classes; `live-wire`, the messages a desktop client and a node speak to publish and subscribe to live tracks, which a client links on its own |
| Daemon | `crates/noded/`, `bin/node/` | The `/v1` HTTP and WebSocket surface, the lookup mesh, the workspace on disk, the client, and the `ducktape` binary |

## Develop

```sh
cargo test -p host -p node -p consensus -p statesync -p noded   # the kernel and the daemon
make test                                                       # everything the repo verifies locally
```

[`AGENTS.md`](AGENTS.md) holds the rules that bind changes to this tree.

## Documentation

[`docs/README.md`](docs/README.md) is the index: one line per document,
grouped by the question it answers; [`ops/README.md`](ops/README.md) covers
the operator scripts. There is no docs site: the code and its comments are the
record.
