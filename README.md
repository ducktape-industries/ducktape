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
queries = "signed"            # optional, default "signed": "open" answers unsigned queries too; the validators program holds it from here on (`ducktape vote queries`)
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

[limits]                      # optional; absent means frames run unmetered (queries: load.query_fuel)
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
validators, identity, blobs, awaits, halo), naming the role; a roster
whose table misses one, or names a program the roster does not run, changes
nothing. Every frame's env carries the table (`Env.roles`); a program looks a
role up by name and gets nothing for a role the network does not fill.

Before an op runs, on a network that binds a program to the access role, the
kernel asks its target program what the op touches (`ProgramCall::Access`, answered as `role::access::Need`s: a key in the
program's own space and whether the op writes or deletes it). When the answer
names anything, the program filling the access role checks it for the parties
the frame acts as (the account, and the sending program for a message) before
the op runs, as part of the op: a refusal refuses the op, and the check's
writes fall with a refused op. The access program keeps the lists owners set;
the owner of a key and the list that holds until one is set are the target
program's to say (`ProgramCall::Resource`), which the access program asks
through `HostOp::Resource`. A key shaped like a path answers to the nearest
list on it or above it, and no list anywhere means anyone may act. A query is
never checked. With no program bound to the access role, the kernel asks no
target program and checks no op. A roster that removes a program unbinds every
role it fills in the same block, so removing the access program lifts the
per-op check as unbinding the role does.

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
yet calls `defer` and keeps the ticket. The host then keeps the waiting
program's memory, only the 4 KiB pages that differ from a fresh instance,
packed as one blob of kind `parked` that the holder `$parked` holds on the
line `frame/<id>` (the row `blob/<id>` in `$parked` names it), and tells the
awaits role which tickets the frame waits on. Only the blob's id sits in the
commitment, so a commit never re-copies an image, and a joiner fetches the
blob as it fetches every blob its state names. On a network whose layout
height is still ahead (`LAYOUT_FROM`), a frame keeps the packed image in the
row `image/<id>` instead; one parked that way still wakes, and moves to a
blob the next time it parks. Later
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
the frame has run: at each block it asks the programs role `Hooks(height)` and
tells every hook it named of each frame with an `abi::hook::Record` (the
height, the frame's position in the block, its signer, the account it acted
as, its target and whether it applied), from the `System` origin. A hook
whose code exports `abi::hook::BATCH_EXPORT` (an empty function named
`hooks`; the core fixture SDK's `hooks!()` writes it) is called once per
block, after its frames and before the scheduler, with `ProgramCall::Hooks`
and every record, on `HOOK_FUEL` per record. Its writes land after every frame
of the block, so a frame that reads its program in the same block sees the
state the block started from. When it refuses that call, its
writes are undone and it is told each record alone, as any other hook is.
Any other hook is called after each frame with `ProgramCall::Hook` and that
frame's record, on a fixed budget of `HOOK_FUEL`, so a frame reads what the
hook wrote for the frames before it. Whether a hook takes batches is a
property of its code, so a block replays as it ran under the code its height
ran. A hook never changes the frame: one that refuses, traps, spends its fuel or sends a message has
its own writes undone, and the block goes on. The founding file's `hooks`
seeds the programs role's list; a running network hooks or unhooks a program
through the programs role like any other change.

Each node keeps an activity index of its own, in `activity/` under its data
directory, which no commitment covers and state sync never ships: each frame
a block carried that acted as an account (its height, position, time, signer,
target and whether it applied), newest first under that account, each
account's count of frames per program, and the refusal of each frame that did
not apply. The host returns a
block's records with its receipts (`host::Applied`'s `records`), and a thread
of the node's own writes them after the block, so the index may lag the chain
but never holds a block up, and logs `activity_lag` when it falls more than
an epoch behind, once until it has caught up again. `POST /v1/activity` reads it: a borsh
`noded::wire::activity::Query` (`Acted` and `Used`, each a page resumed by its
`next`, `Indexed`, the first height whose frames it holds and the newest it has
indexed, and `Refusal`, why the frame at a height and position did not apply)
answered by a `Reply`; `Frames` and `Signed` answer `unsupported`, since
`/v1/blocks` lists every frame a block carried. An index opens at
the node's tip, seeded from the `activity` program's tables where the network
still runs that program, so a node's history starts where its index did. An
index an older build kept also holds every frame network-wide and by signing
key: it lets go of those rows once, a block's frames at a time, as it opens.

A node that is no validator of the epoch a block opens, and whose data
directory takes a reflink, keeps a snapshot of its state in
`snapshots/<height>/`; a validator makes none, since the clone holds its
writer up. At the last block of each epoch, after that block's commit and
the next epoch's seating, and before the next block's commit, the writer
clones every program's
commitment files (every runtime partition under `state::partition_prefix`)
into the snapshot's `runtime/` and
writes its `manifest`, a borsh `statesync::Manifest`: the tip and each
program's sync target, the root and op range its copy opens at. The snapshot
appears whole by a rename, and the node serves it from then on; a thread of
its own, off the block CPUs, removes each older one once no download reads it,
and the one before that even while a download reads it, so a node holds at
most two (a stop waits for that thread).
A node that already knows a later epoch's last block is finalized (it is
catching up) skips the snapshot, since the next one replaces it; marshal's
finalized tip reaches the writer through `Chain::finalized`.
A node whose filesystem refuses the clone logs `snapshot_unsupported` once,
clears `snapshots/` and makes none until it restarts; a snapshot that fails
logs `snapshot_failed` and leaves the one before it.

`POST /v1/snapshot` answers the newest snapshot in one stream. It takes a
borsh `noded::wire::snapshot::Ask` and answers the manifest as a length-framed
borsh `statesync::Manifest`, then each file under the snapshot's `runtime/` in
the order of its path, as a length-framed `wire::snapshot::Part` (its path, its
length and where its bytes start) followed by those bytes, then an empty
frame. A download holds the snapshot it reads, so a newer one does not remove
it until its answer ends; once a second newer snapshot lands, the answer is cut
and that snapshot goes, and a resume downloads the newest. An `Ask` whose
`resume` names the snapshot's height,
the last file a cut download holds and that file's length is answered from
there; one naming another height gets the newest snapshot whole.
`Client::snapshot` writes a snapshot into a directory (its `manifest` and
`runtime/`) and resumes from whatever that directory holds. Nothing fsyncs a
snapshot, so a node that restarts serves its newest one only if its manifest
decodes and names that height (`snapshot_unserved` otherwise), and removes the
rest. A node with no snapshot refuses `not_found`.

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
epoch that seats the node begins, `join` adopts the network's state and
fetches every program's code, and the node runs like a founder. While it
syncs the state, `join` holds the head it read open (`POST /v1/sync/hold`):
the node it joins through keeps serving that head, past its own prunes, until
the hold closes: once the state is synced, or when the hold's connection drops (the
joiner stops, or a proxy between them closes it), so a busy network cannot
prune past a slow join. The joiner reads the hold's answer as it comes, one
line per block the source applies; when that answer ends or breaks, the join
drops the request it is waiting on and reads the head again under a new hold.
A hold the source let go of while its connection stays open shows as the next
sync the source refuses as pruned, and the join reads the head again then. `run` fetches every other blob the state names
afterwards (see the blobs below). A stranger joins as a resident: it follows the
chain and never votes or proposes. A validator named in the file joins the
same way and keeps its seat. `ducktape enroll` takes the same source, `--halo` and
`--invite` and sends only the enrollment, with nothing synced, for a key that
runs no node (an egress box, below).

```sh
ducktape join https://a.example.org --halo b.example.org:9100 --invite <hex>
ducktape run --listen 127.0.0.1:9000 --http 127.0.0.1:8844 --halo 0.0.0.0:9100
```

Each epoch's first block carries, as its last frame, the root of the
network's state after the previous epoch's last block (`node::Settled`): the
hash over every program's commitment root that `ducktape status` prints. A
node seats an epoch's validators only once it has applied the block that
closes the epoch before, so each holds that root, and each refuses a first
block whose record names another root, and a record on any other block.
The record is no frame: nothing runs it.

Membership changes only by the validators' vote. The door starts closed, so
a stranger needs an invite a validator signed: `ducktape invite` prints one,
good for one enrollment for 72 hours (`--hours`). Validators vote with
`ducktape vote promote|demote|remove <key>`, `ducktape vote open|close` (the
door), `ducktape vote queries signed|open` (whether the network answers unsigned
queries), `ducktape vote role <role> <op>`, which runs `op` on the program bound
to the role once it passes (`programs` takes a program change, `halo` a lane
to open, set or close; neither takes a change from anything else). A role
moves only with the program change that publishes the program declaring it,
which only the validators' vote schedules. A motion passes when more than two thirds of
the current validators vote for it. `ducktape leave` takes the node out; the last validator cannot
leave. Each prints the block that carried it.

The workspace (`--workspace`, default `$DUCKTAPE_HOME` else `~/.ducktape`)
holds `identity.key`, the network descriptor, the anchor the node started
from, the node a joined node read its state from (`source`), the address its
HTTP surface bound, the runtime's storage and, when the
operator writes them, `node.toml`, `egress.toml`, `egress-grants` and `machine.key`. Under `schemas/` the
CLI keeps the schema section of each program code it has spoken to in JSON,
and under `tools/` its tools section (`{}` for a code that carries none),
each named by the code's id, so `ducktape query`, `ducktape op` and
`ducktape mcp` fetch a code from the node once, not on every call; an id names its bytes, so a kept
schema never goes stale, and one that no longer reads is fetched and kept
again.

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

A member key with no node of its own serves egress the same way over halo's
lane `ducktape/egress`, which the validators' vote opens. The box enrolls with
`ducktape enroll`, then runs `ducktape egress serve --halo <bind>`: it reads
the members and halo's lane table from a node over HTTP (`--node`), seats the
validators and opens the lane to one of them. That node, its relay, sends it
after each block the tip, the requests egress assigned to the box's key with
their bodies, and the box's service row. The box decides and calls as a node
does, from its own `egress.toml`, signs each frame with its own key at the
relay's tip, and sends it over the lane; the relay pools and relays it as its
own and answers whether it took it. The relay gathers each body apart from
its push, asking every member once for a part it lacks; a part no member
gives fails only its own request, and the box hears of a request only once
its body is sent. A request that leaves the assigned list ends its call, and
a role change, `Unbound` or the box's end ends every call, as on a node. The
box trusts every validator with its upstream: it calls what a relay lists,
unchecked against the chain. When the relay goes, the box keeps what
it holds and opens the lane to the next validator: a frame not yet taken is
signed again at the new relay's tip, and one a block already carried counts
as done. A frame no block carries is refused on the box itself with the
refusal a node's pool gives, so an answer larger than a block carries goes
out as a failure here too. The relay runs each box's session on its RPC
runtime, beside the block threads. While the box's lane table does not list
the lane enabled (before the vote opens it, or after a vote pauses or closes
it), the box waits for the next change to the halo program and reads again.
With no validator reachable it waits until one links; after a round in which
every relay it reached dropped it, it waits for the next block; when it
reaches every one and none serves the lane, it stops with an error.

```toml
# $DUCKTAPE_HOME/egress.toml: read when the node starts and on `ducktape egress reload`;
# absent means it serves nothing
relay = true                  # answer `relay` requests
http = true                   # answer `http` requests, to the public internet
private = false               # optional: let `http` reach loopback, private and link-local addresses

[services.anthropic]          # a registered service this node fronts; the provider's route names it (SetRoute, found-a-network § 10 step 6)
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

`ducktape egress set FILE` sends the running node a whole `egress.toml`
instead, over `/v1/egress/set`. The node loads it as a reload loads the file.
With `--dry` it changes nothing; otherwise it writes the text as its
`egress.toml` (a new file renamed over the old) and serves it from the next
request on. It answers how many services it then serves and how many are
added, removed and changed, and for each service whether its base answers a
plain `GET` from this node (with any status) or fails, and why; a base that
has not answered within `DEFAULT_WAIT` block times, the wait a request gets,
fails. Sets are written one at a time. A text that
does not load is refused as a reload is, and the node keeps its file and what
it serves. The CLI signs with the active wallet key, or with the node's own
key under `--node-key`. The node takes the set from its own key or from a key
the workspace's `egress-grants` file lists (one public key in hex per line),
which it reads again at each set, so a grant takes effect without a restart.
It refuses every other signer as `unauthorized`. A set carries no secrets:
one whose services name any `secrets` entry, a variable or a file, is
refused as `invalid_input` whoever signs it, naming the service. A secret
reaches `egress.toml` only when the operator writes it there; a service
that needs one sits behind a proxy in the cluster that holds it. Only the
node's own key may set `private = true`; from a granted key the set is
refused as `unauthorized`, naming `private`.

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
egress_lane_backlog = 64      # --egress-lane-backlog
forward_lane_backlog = 64     # --forward-lane-backlog
query_fuel = 1000000000       # --query-fuel; default no cap: every query is metered and counted

[consensus]
leader_timeout_ms = 1000          # --leader-timeout-ms; default one block time
certification_timeout_ms = 30000  # --certification-timeout-ms; default 30 block times
timeout_retry_ms = 2000           # --timeout-retry-ms; default 2 block times

[machine]                     # no flag; `ducktape machine run` reads it too
on = false                    # default false: true has `ducktape run` start `ducktape machine run` as its child
cpus = "6-7"                  # default: the CPUs block_cpus leaves to the rpc runtime, else every CPU
nice = 10                     # default: the node's own
memory = "32Gi"               # default none: no cap; a byte count is a number
work = "/var/lib/ducktape-work"  # default: the workspace's folder name with -machine, beside it; never inside it

[[machine.machines]]
name = "warm"
labels = ["warm", "heavy"]
backend = "shell"             # shell, podman or pods
slots = 1                     # how many runs it takes at once
serves = [17]                 # default: a shell machine its account's manager, a podman or pods machine anyone
service = "pods"              # pods only, and named there: the egress service this network registered for the pods service
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
holds the consensus timeouts. `[machine]` names the machines this workspace
runs for fleet ("Run work for fleet") and, with `on`, has the node run them
as its child.

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

A browser page of any origin may call the `--http` surface: every `/v1`
answer carries `Access-Control-Allow-Origin: *` and a preflight is answered,
with no credentials. Reads are public, and a write is a signed frame that
carries its own authority, so an origin proves nothing a node would check.

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
When it starts, and whenever its own registered address changes, a node dials
that address as a member would. If it does not reach itself, it logs a
`halo_unreachable` warning with the address and the reason (`unresolved`, or
`undialed` once QUIC's idle timeout passes): members then reach it relayed or
punched through rather than directly at that address, and a network that does
not route a host back to its own public address fails that dial while members
still reach the node directly. `GET /v1/halo/self` answers `Unregistered`,
`Dialing { at }`, `Reached { at }` or `Unreachable { at, reason }`.

A member whose `address` is `ws://` or `wss://` also carries halo on its
`--http` socket: a member that does not reach it over UDP opens a WebSocket
at `/v1/halo` there and runs the same QUIC inside it, with the same node-key
admission and lanes. Over that link each side tells the other where it is
reached directly: the IP `GET /v1/seen` answered (Cloudflare's
`CF-Connecting-IP` when the request carries it, else the TCP peer) with the
port it bound. Both then dial those addresses at once, which most home
routers let through, so a node behind a router or a tunnel needs no port
forward. The direct link is preferred, and the relayed one closes once it is
up. Behind a router that maps each remote apart (symmetric or carrier-grade
NAT) the punch fails and halo stays on the relayed link. Two members with no
`ws://` or `wss://` address learn where the other is reached from a member
both are linked to. A member tells its address only to members it admitted.
The `/v1/seen` answer is not authenticated: someone on the path can rewrite
it, and the member then tells a wrong address. That costs reachability, not
data. Its members' direct dials miss and it stays on the relayed link, while
node keys still decide who is admitted.
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
is closed under it. `--egress-lane-backlog` is how many streams egress boxes
opened wait for the node to take them. `--query-fuel` is the fuel one query this node
answers off the chain may burn (`/v1/query` and the node's own reads), the queries it makes
included; past it the query is refused as a `trap`, "the run spent its fuel". Frames never
take it: they run on the network's `[limits]`. Without it every such query is still metered,
with no cap. As each epoch ends the node logs one `query_fuel` line (`ducktape::query`): how
many `/v1/query` asks it answered over the epoch, how many ran out, the most fuel one burned
and its program, and the p50 and p99 as the power of two at or above them, the measure to set
`query_fuel` from. `ducktape run --help` shows each default and its
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
already open when its lane is paused or closed runs on. Every node binds `ducktape/egress` at
start, for egress boxes (above); it carries streams once a vote opens it.

Every node also binds `ducktape/forward` at start, and it carries streams once a vote opens it
(`Open { name: "ducktape/forward", cap: Client }`). It reaches a service on a member's machine
that has no public port, such as a build runner's sshd, over the member's halo link, relayed
when the member is behind a NAT. A call is a frame signed under `ducktape:forward` whose target
is the service and whose payload is the member node's key. The caller opens the WebSocket
`GET /v1/forward` on any node and sends the frame as its first message. That node checks the
signature, the network and the anchor (as fresh as a signed query's), opens a lane stream to
the member and passes the frame on. The member's node checks the same again, that the frame
names its own key, and that its own `forward.toml` offers the service and lists the signer,
all before it connects. A signer the service does not list is refused `not_found`, the same
as for a service the node does not offer, so no key learns what a member forwards:

```toml
[services.builds]
address = "127.0.0.1:22"
signers = ["<the caller's ed25519 key, hex>"]
```

The node re-reads `forward.toml` on every call, and a missing file offers nothing. The first
message back is a `wire::Forwarded`: `Opened`, or `Refused` with the reason. Then each binary
message carries the service's bytes. An empty one ends that direction, and a WebSocket that
closes before both ends were sent is a cut. `ducktape forward <member key hex> <service>`
pipes stdin and stdout through it, signed by the active wallet key, so ssh runs it as
`ProxyCommand`. A stream opened while the link is relayed is cut when a punch replaces the
relay; the caller opens again.

Live tracks: the relay in every node (`crates/noded/src/live/`) lets a desktop
client publish and subscribe to tracks of a room off consensus, in what the
room's program granted its account under the `live` table, over QUIC on a UDP
socket of its own (`run --live 0.0.0.0:9200`, ALPN `ducktape-live`, the node
key's certificate) or the WebSocket `/v1/live`; nodes carry a track between
their clients over halo's lane `ducktape/live`. `GET /v1/live/port` names the QUIC port, or
none without `--live`, and then clients take the WebSocket. The clocks a client must honour are `live-wire`'s:
`HANDSHAKE_WAIT` for each step of the handshake, `KEEP_ALIVE` and `IDLE` (a connection silent
that long is closed, on either path), and `IDLE_SESSION` for a session holding nothing.

```sh
ducktape run --listen 127.0.0.1:9000 --http 127.0.0.1:8844 --halo 0.0.0.0:9100   # the tunnel serves https://b.example.org from 127.0.0.1:8844
```

## Run work for fleet

fleet is the program that holds what someone asked to run, which machine
took each run and how it ended. A machine is an account with a key that runs
that work: `ducktape machine init` mints `machine.key` in the workspace (mode
0600) and prints its public half, and the account's owner makes the account
with identity's `CreateAgent` and adds that key with `AddKey`. A
`machine.key` that is the workspace's `identity.key` is refused.

`ducktape machine run` runs the machines `[machine]` names (`node.toml`, or
`--config FILE`), against `--node` or the workspace's node, talking to fleet
in JSON through its schema. Each machine says it is ready (`Ready`: its
labels, `slots` and whose runs it `serves`). The process follows fleet's
writes, and when a write touches what a machine holds it reads the machine's
`Holds`: an assigned run is claimed and started, a run being stopped is
stopped, and a run held as running that the process does not know (it
restarted) is reported `Errored`. While a machine holds a running run it
beats every `DEFAULT_WAIT / 2` blocks (`abi::role::egress::DEFAULT_WAIT`),
counted on the blocks it follows. When a run ends, its log and every file it
wrote to `$OUT` (`/out` in a container) go into files as one commit, under
the workload's `out` or `/shared/fleet/<run>/`, and the machine reports the
end with that folder and snapshot: exit 0 is `Passed`, any other exit or a
signal `Failed`, a run it was told to stop `Cancelled`, and one it could not
start or wait for `Errored`. A run is declined when its machine's backend
cannot run it (an image on `shell`, none on `podman` or `pods`, a disk scratch
on `pods`, or a forge input with no `git-remote-duck` on `PATH` on `shell` or
`podman`).

`shell` runs the command in a work tree under
`<work>/<machine>/<run>-<attempt>/`, outside the workspace (`work` in
`[machine]`, else the workspace's folder name with `-machine`, beside it: a
workspace at `~/.ducktape` runs in `~/.ducktape-machine`). The machine makes a
missing `work` with mode 0700 and refuses one that isn't a folder its user
owns with no access for group or others, or that is a symlink. It then
resolves both paths and refuses a `work` inside the workspace or holding it.
`ducktape machine run` runs only on Linux, where it can keep each run private;
elsewhere it refuses to start. A run's
inputs sit at their paths inside its folder, it runs in a process group of its
own under umask 077, and with an environment of only
`OUT`, `WORK` and the workload's `env`: the command itself is found on the
machine's `PATH`, but what it starts sees no `PATH` (the shell's own default
applies), so a workload that needs one declares it in its `env`. It still runs as the node's user and
can read whatever that user can, the workspace's keys included, so a `shell`
machine serves only its account's manager unless `serves` lists whom it takes
runs from. A shell run takes no CPU or memory of its own from the workload's
asks: it has the machine's. A stop, and the end of the command, ends the whole
group. `podman` runs `podman run --rm` as
`ducktape-<genesis>-<account>-p<pid>-<run>-<attempt>`, `<pid>` being the
machine process's own, with the image, each input
mounted at its path, `out` at `/out`, and `--cpus` and `--memory` from the
workload's asks; a podman machine serves anyone unless `serves` lists whom. A
stop is `podman kill`, and every end `podman rm -f`.

`pods` runs each run as a pod of the pods service, through fleet. Each step is one
fleet `Call`: fleet sends it to `service` as an egress request of its own, stamped
with the run (`fleet-run`, `fleet-attempt`, `fleet-asker`, `fleet-machine`), and
the machine reads the answer from the run's pieces (fleet `Pieces`) when fleet's
writes touch them, never by polling. The steps are `POST /v1/pods` (the workload's
image, its command with its output in `/out/.fleet/log`, `OUT=/out`, its inputs
and its asks as the pod's requests; pods makes the run's asker, from
`fleet-asker`, the pod's requester, and lets a call reach only the pod labelled
with its own run and attempt), then `GET /v1/pods/NAME/wait` streamed until
its `ended` event, beating every half egress wait (`DEFAULT_WAIT` blocks of the
network's block time, in whole seconds); a wait that ends without one reads the
pod and waits again while it runs. The pod's `out` container then cuts the log
and every file in `/out` into 1 MiB pieces, which pods puts in one `/blob` bundle,
the machine commits them to files as the other backends do, and the pod is
deleted. A stop deletes the pod and reports `Cancelled`. A pods machine serves
anyone unless `serves` lists whom: pods' own limits fence the pod.

A run that made commits lists each as a line `<repo> <commit> <path>` in
`$OUT/commits`, `<path>` being the repository's work tree as the run's inputs
name it. Each is packed against the repository's remote refs, which forge
already holds, into `/shared/fleet/packs/<repo>/<commit>.pack` in the run's files
commit, and a line the machine can't pack is named in the run's summary (`not
packed: …`). On `shell` and `podman` such a repository and its objects lie
inside the run's folder, with no link among the objects, no `commondir` and no
`objects/info/alternates`, so a run never has the machine publish objects from
elsewhere on its host. A forge input whose commit forge
lacks is filled from its pack: `shell` and `podman` lay the pack into the clone
and check the commit out again, and `pods`, once a pod's fetch fails, makes the
pod once more with that input at its default branch and the pack laid in before
the command runs.

When the machine stops it first stops every run it holds (their folders stay
under `work`), and when it starts it removes the containers named for its
account whose machine process is gone, and leaves those of a machine process
still running. A machine killed outright (SIGKILL) leaves its shell runs' background
processes behind; fleet hears of each lost run when the machine, started
again, reports it `Errored`. A run's folder is removed once its outputs are in
files and its end is reported, and never before: a folder kept because its
outputs, its report or its start failed is named in the run's summary or the
machine's `machine_op_failed` warning. An `out` that is a symlink is refused.
When another commit lands first, the outputs are committed again on the new
head (`machine_commit_stale`, logged at the first attempt and each power of
two). The machine starts and ends each run in a task of its own, and the work
on a run's files that grows with them (making its folder fresh, writing
inputs, listing, cutting and hashing outputs, removing folders) runs on
tokio's blocking pool, so neither the network nor that disk work holds back
the loop that beats. A task that panics is settled: what it started is
stopped as on the machine's own stop, and fleet hears its run `Errored`.

With `on = true`, `ducktape run` starts `ducktape machine run --child` against
its own HTTP surface once it is up, on `cpus` (else the CPUs `block_cpus`
leaves to the rpc runtime) at `nice`, and under `memory`: in a cgroup v2 child
`machine` of the node's own cgroup (`memory.max`, and `cpuset.cpus` where the
cpuset controller is there) when the node's cgroup delegates the memory
controller to its children or lets the node turn it on, otherwise as each
process's address-space limit, which bounds each process's virtual size, not
their sum. A node running as a systemd service holds processes in its own
cgroup, so the kernel refuses to turn the controller on there and the cap is
the address-space limit. The `machine_limits` event says which (`memory`:
`cgroup`, `rlimit` or `none`) and why (`reason`: `memory_unset`,
`cgroup_made`, `cgroup_root`, `cgroup_v1`, `cgroup_unread`,
`memory_undelegated`, `cgroup_unmade` or `cgroup_unset`). `cpus`, `nice` and
`memory` are applied only on Linux; elsewhere a node with any of them set
refuses to start. The machine itself sits in the leaf `machine/agent`, and a
shell run inherits all of these limits. Podman starts each container outside
the machine's own processes, so a podman machine hands the limits on: each
container runs with `--cgroup-parent` set to the `machine` cgroup, so its
`memory.max` and `cpuset.cpus` bound the containers and the machine together,
and with `--cpuset-cpus` set to the machine's own CPU set when that is
narrower than the CPUs online. Every podman call for such a container (its
run, kill and removal, and the start's sweep) names the `cgroupfs` cgroup
manager its run was made with. A podman machine with `memory` set runs only
inside that cgroup: run standalone, or where the node fell back to the
address-space limit, it refuses to start. `nice` reaches the podman client,
not its containers. When the
child exits, the node starts it again at the next block it applies, logging
the first exit and then each one whose count is a power of two
(`machine_exited`, with `attempts` and a `reason`); a child that exits
cleanly (status 0) starts that count again, and a crashing one never does. The node's stop stops it. The
machine logs to the node's stderr, not to the node's `/v1/logs` ring. Run
standalone, `ducktape machine run` sets the same CPU set, nice and
address-space limit on itself.

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
ducktape mcp                            # the Model Context Protocol on stdio for any MCP client: network_schema, network_query, network_call, files_read, files_write, signed by the active key, which must act as an agent account
ducktape programs                       # every program and its code blob
ducktape blob get sha256:…              # a blob's framed bytes
ducktape blob put code.wasm --kind program --wait  # put a bundle, print its ids, wait until it is held
ducktape blob unpack                    # with the node stopped, every blob back as one file, for an older build
ducktape machine init                   # mint machine.key and print its public key
ducktape machine run                    # run the work fleet assigns to the machines [machine] names
```

A query is a call to a program's `query`, which changes nothing. A signed
query is a frame signed under the namespace `ducktape:query`
(`node::Frame::sign_query`), never under `ducktape:frame`, which every op
signs under: `/v1/submit` takes only an op's signature and `/v1/query` only a
query's, so neither runs as the other. `/v1/query` takes a borsh
`noded::wire::Query { layer, ask }`. `Ask::Signed(frame)` runs as
`Origin::External` of its signer, while its anchor is one of the last
`host::ANCHOR_WINDOW` blocks, as an op's must be, so a captured query is not
read again later. `Ask::Unsigned { target, payload }` is
answered only while the program in the validators role answers
`validators::Query::QueryPolicy` with `Open` (the founding file's `queries`,
then `ducktape vote queries`), and refused otherwise. An unsigned query runs as
`Origin::External` of the empty key (`abi::Origin::unsigned()`): no signature
verifies under it and no account holds it, so a read that needs a signer's key
or an account refuses it as it refuses a stranger. No code may read the empty
key as a real one.

A workspace records the contract its state was applied under (the file
`contract`: the contract and the first height applied under it; a workspace
without one reads as contract 9). `ducktape run` refuses a workspace stamped
with another contract than its own, or whose tip is below that first height.
A contract bump that changes how blocks decode rolls as a stop at one announced
height R: with the node stopped, `ducktape roll R` stamps a workspace whose tip
is R; one behind R state-syncs from its `source` to a head at or past R, served
by a node on the new contract (the state from before goes to `pre-roll-<tip>/`
in the workspace), then stamps from the head after it; one past R is refused, and
joins again. So a binary never applies a block another contract produced.

A frame's body ends with `client`, the signer's own word for the software
that signed it (`ducktape-cli/<version>` from the CLI, the app's
`ducktape-app/<version>+<commit> (<os>; <features>)`), or none. It is signed
with the rest of the body and is part of the submission's id, and nothing
checks it. A program reads it with the host op `HostOp::Client`, which answers
the client of the submission whose frame is running (the messages and calls
that frame makes included, and the frame again when it resumes after parking:
the kernel keeps a parked frame's client beside it, under `client/<frame>`) and `None` in a query, a hook, a scheduled run and
the kernel's own runs. It is for showing and for choosing what to show, never
for deciding who may act.

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
block reads it fetches it over halo before it applies the block. When a round
over every member brings nothing, a joined node asks the node it joined
through (the workspace's `source`) for it on `/v1/blobs`, keeps it only if its
bytes hash to its id, and logs `blob_from_source` as an error once for that
blob, since halo is not reaching the members. Until one of them gives it, the
node asks the members and then the source again after each pause. A read of
such a blob (`/v1/blob/get`, `/v1/blobs`) asks every member once, or none
when the node already asks in the background: if none gives it, `/v1/blob/get` refuses it as not found and `/v1/blobs` answers it as
not served and goes on to the next id, while the node keeps asking in the
background, one fetch per blob however many read it. A blob's roster row
names the programs that hold it. A put holds its blob on a line the program
names (`BlobPut { hash, kind, body, line }`: a path, a slot, or for a one-off
the blob's own id) until a newer put by that program on the same line, or
until the program lets go with the host op `BlobRelease`. `BlobPutAlone { hash,
kind, body }` holds a blob on its own line, until released, and so does every
row written before lines existed. A program that holds one blob on several
lines holds it while any of them does, and `BlobRelease` lets go of it on all of
them. `BlobReleaseLines { prefix }` lets go, in one op, of every line the program
holds whose name starts with `prefix` (a dropped scratchpad's `scratch/<branch>/`;
an empty prefix names them all), as a newer put on each line would: a blob it
also holds alone or on a line outside the prefix stays. A line's key is its
program's length-prefixed name followed by the line's own bytes, so a prefix is one range. The
prefix is a byte prefix, not a folder: `scratch/b` also covers `scratch/bx/1`, so a
caller who means a folder passes its trailing `/`. The roster row keeps only how many lines hold a blob; each line that holds
it is a key of its own, so a put rewrites one small row however many paths share
the blob. Once no program holds it, the row goes, and after that block commits the
node deletes the blob's file, unless a held bundle names it or a program runs it
as code. Every node does this, so a read of a released blob that no held bundle
names answers no blob, the same as a read of an id nobody put. A row written before rows named their holders
names none, and nothing releases it. A joiner streams only the programs' code
and notes any its source no longer serves. If any is noted and the source's
head has moved, it syncs to the new head, holding it as it holds the first,
writes only the keys that changed since it last adopted (a program whose log
no longer reaches back that far, or one admitted or dropped since, is
rewritten whole), and fetches only the code it still lacks. It writes the
adopted keys in pieces, each kept under the store's own limit on one write,
after clearing the store's height, and writes the height last: a join cut off
part way leaves a store with no height, which a node never runs, and the next
join clears it and writes it again. Once it has all
of it, it deletes each blob it fetched that the state it ends at no longer
names. A source that refuses code its own tip names ends the join. Every other
blob comes after: when a joined node starts, `run` asks the node it joined
through (the workspace's `source`) for every blob its state names and it
lacks, in one stream on the RPC threads, beside the blocks. It writes each
blob through the same per-blob owner a fetch over halo takes, so it leaves a
blob another fetch is pulling to that fetch, and a block that needs a blob
takes it over. It goes past each blob the source no longer serves. Once the
stream ends and the next block has applied, it finds each blob it fetched
that the state no longer names, then, on an RPC thread of its own, holds the
writer between blocks and deletes each that the last block applied still
leaves unnamed (if the newest view lags the tip, it keeps them and checks again
after the next block): a release that
landed while a blob was on its way leaves no file, and a block that put the
same bytes again keeps it. A node holds every blob once `GET /v1/blob/missing`
answers none. A source that has gone away is asked again every block time
(`join_unanswered`, logged on the first try and every tenth): deleting the
workspace's `source` file and restarting the node stops it. New program
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

`/v1/watch/<program>[?keys]` is a WebSocket that carries what
`/v1/changes/<program>[?keys]` carries, plus standing queries. The client
sends borsh `Watch` frames: `Ask { id, frame }`, with a query frame signed as
`/v1/query` takes it, and `Drop { id }`. The node checks the frame and its
anchor once, runs the query on the confirmed layer as the frame's signer and
answers `Pushed::Answer { id, height, outcome }`; an ask that is refused does
not stand. A node that caps no query's fuel (no `load.query_fuel`) refuses
every ask as `unsupported`, since a query there could run forever. While the query runs, the host records what it read: each key it
got (a missing one too), the range each scan covered (up to the last key when
the scan filled its limit), each program it read whole (`Root`), what every
query it asked read, and the code of each program it ran. For each block that
writes the program or moves an answer, the node sends one
`Pushed::Block { height, root, writes, answers }`: `answers` holds each
standing query whose read set the block wrote, run again at that block's
view, whose answer differs from the last one sent. A socket holds at most one
unsent block: until the client can take it, newer blocks merge into it (their
writes joined, the newer value winning) and no query runs again.
`Outcome::Moved` says the node could not run the query again (out of fuel,
code it lacks, a host error) and the client asks it itself. A query's clock
is not a read, so a query whose answer moves only with time is not run again.
Pings and the epoch cut are as on `/v1/changes`, and a frame that is not a
`Watch` ends the socket.

A node keeps the block history of its tip and the 1024 blocks below it (each
block's summary, where each transaction first landed and the payload of each
transaction a block ran) in a store of its own under `history/` in the
workspace: keeping a block lets go of the one 1025 below it, so `/v1/blocks`
lists from the window's bottom (`listed_from`) and `/v1/block` and `/v1/tx`
answer not found below it. Only the block writer writes it, and the
block read routes read it from snapshots, so no read can hold up a block. The
node keeps its blocks in a compressed archive (zstd's default level). Each start copies
into both whatever the archives of an older build hold that they lack, into
history only inside the window, lets go of what history holds below the window,
and refuses to start when a summary names a block the block archive lacks. Before
an older build starts on the same workspace, `ducktape restore-archives`, run
with the node stopped, writes the history and the blocks back into the archives
that build reads.

A node keeps its blobs under `blobs/` in the workspace. A blob of up to a
block's 16 MiB is a zstd record in an append-only segment file (`blobs/seg/`),
and a release appends a tombstone; a segment whose released bytes reach its
live ones is copied forward and deleted on the next block. A larger blob is its
own file (`blobs/big/`) of independent zstd frames of 1 MiB with an index at
its end, so a range read decodes only the frames it covers. A whole read is
checked against the blob's id, and a frame against its own checksum. A node
started on a workspace an older build left reads its one file per blob where it
lies and moves them into segments, at most 16 MiB of them a block. Before an
older build starts on the same workspace, `ducktape blob unpack`, run with the
node stopped, writes every blob back as one framed file.

`/v1/heads` is a WebSocket of the chain's finalized heads, for a client that
needs the chain's height and time without its blocks: one borsh `Head { height,
id, time }` for each block the node applies. It opens with the newest head the
node has applied since it started (none before its first block). Like
`/v1/changes`, it pings with the height at each epoch's last block and cuts a
client that answers no ping for a whole epoch.

`ducktape shutdown`, SIGTERM and SIGINT stop `ducktape run` the same way.
The node stops taking blocks and lets the block it is applying finish. It
closes every `/v1/changes`, `/v1/watch` and `/v1/heads` socket with a going-away close frame, flushes its
state to disk and exits 0, so the next start replays no write log. A signal
that arrives while the node is still opening its state ends the start at
once.

## How it is built

| Layer | Where | What |
| --- | --- | --- |
| Kernel | `crates/kernel/` | `abi` (the bytes ABI), `program` (the small program SDK the fixtures compile against), `runtime` (the wasmtime embedding), `state` (the authenticated store and its commitments), `host` (the sandbox: submit, query, and the messages a frame emits, run in that frame), `node` (frames, blocks, the mempool), `consensus` (Simplex BFT over marshal, per-epoch engines, catch-up), `statesync` (a joiner adopts a network's state); `fixtures/` is its own workspace of wasm32 test programs, which `fixture-wasm` compiles from source whenever the test suites build |
| Programs | [`ducktape-industries/programs`](https://github.com/ducktape-industries/programs) | The contracts a program compiles against (`crates/kernel/abi` here, which it takes by git at the core rev it pins, and `crates/sdk/program`, the full program SDK), the boot set (`crates/system/`: `programs`, `valset`, `identity`, `blobs`, `awaits` and `halo`, whose suite founds this host over their bytes) and the app programs. The system programs beyond the boot set are archived at `ducktape-industries/ducktape-system-modules-archive` |
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
