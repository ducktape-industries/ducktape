# Found a network

The steps to found a hosted network from nothing: a node on Kubernetes, the
first accounts, and agents that answer. They go in order. Each step gives the
command, what you must decide, and where the answer lives. The example values
are a second network on the machine that runs duckpond: Kubernetes context
`fabrics-1`, network `sealpond`, reachable only on the host's Tailscale
address `100.65.253.102`, with its agents on duckpond's ccs.

`$CORE` is a checkout of forge `core` and `$PROGRAMS` one of forge
`programs`. Both come from duckpond's forge (`AGENTS.md` § Branching and
Delivery says how to reach it).

## 0. Decide the spec

Write these down before you build anything. Two of them are fixed for good.

| Decision | Where it lands | Notes |
| --- | --- | --- |
| The network's name | the founding file's `network`, the Kubernetes namespace, the chain id every signature names | fixed for good |
| How it is reached | step 4 | a public name through a Cloudflare tunnel, or only a host address such as a Tailscale IP |
| Its validators | the founding file's `[[validators]]` | one node is a network; more join later (`ducktape join`) |
| Its limits | the founding file's `[limits]` | a hosted network with agents founds unmetered, as duckpond does: leave `[limits]` out (its nodes still meter queries: `load.query_fuel`) |
| Who manages the first agent | step 8 | fixed for good: identity refuses `SetOwnership` |
| The model gateway | step 5 | the egress service the provider's route names (`SetRoute`, § 10 step 6) |

## 1. Pick the commits

- Programs: a commit on forge `programs` `dev` at or after `8c44f3e7`, the
  first whose harness reads `provider` and routes; an older harness refuses
  a configuration that names one. A hosted network runs the
  release profile's bytes of a forge `dev` commit and nothing else
  (`AGENTS.md` § Branching and Delivery). programs' release workflow,
  which published `wasm-<commit>` on GitHub, is disabled; a local founding
  (kit, qa) builds with `make wasm-founding` too.
- Core: a commit on forge `core` `dev` whose `crates/kernel/abi` matches the
  core rev programs' root `Cargo.toml` pins:

  ```sh
  pin=$(grep -o 'ducktape", rev = "[0-9a-f]*' $PROGRAMS/Cargo.toml | head -n1 | grep -o '[0-9a-f]*$')
  git -C $CORE diff --stat $pin <core commit> -- crates/kernel/abi   # empty: the node runs these programs
  ```

- The desktop app connects only to a node whose contract equals its own
  (`noded::NODE_CONTRACT`, which `ducktape version` prints).

## 2. Build the founding

```sh
git -C $PROGRAMS worktree add --detach .worktree/founding <programs commit>
cd $PROGRAMS/.worktree/founding
nice -n 19 make wasm-founding WASM_OPT=/path/to/binaryen-version_132/bin/wasm-opt
```

It writes `target/wasm32-unknown-unknown/release/founding.tar.gz`: every
program with its describe module, schema and view, stamped with the commit
(`ducktape.source`), and a `founding.toml` with `network`, `time`, the
validator's `key`, `address` and `halo` left to fill. The programs README §
Building names the tools it needs (Binaryen wasm-opt 132, `wasm-tools`,
Python 3). From cold on 24 cores it takes about 12 minutes and leaves about
8 GB in `target/`.

## 3. Build the node image

`ops/k8s/deploy NETWORK COMMIT` builds `ops/k8s/Containerfile` from
`git archive COMMIT`, pushes the image to the cluster node's registry, then
applies the manifests (steps 6 and 7). Run it from a core worktree at the
node's commit, so the manifests match the image. Every `kubectl` call it
makes names the context `CONTEXT`, the current context unless you set it, and
it prints which. It applies the StorageClass `local-path-retain`
(`ops/k8s/storage.yaml`), which every network's claim shares, only when the
cluster lacks it.

## 4. Choose the address

The node serves HTTP, the WebSocket peers and the app dial (`/v1/p2p`), on
8844 in its pod, and halo on UDP 9100 (`ops/k8s/Containerfile`).

- **Public, through a tunnel.** Create the tunnel and its Secret
  (`ops/README.md` § A network's node on Kubernetes). The founding
  `address` is `wss://<hostname>`.
- **Only on a host address.** Deploy with `HOST=<ip>`. The node's pod then
  binds `HOST:HTTP_PORT` (TCP) and `HOST:HALO_PORT` (UDP) on its machine,
  8844 and 9100 unless you set them. The founding `address` is
  `ws://<ip>:<HTTP_PORT>` and `halo` is `<ip>:<HALO_PORT>`.

Behind a tunnel or a home router, UDP 9100 needs no port forward: members
that do not reach it carry halo over the `address`'s WebSocket (`/v1/halo`)
and punch through to a direct path where the router allows it (README §
Reach a node).

On a machine that already runs a network, pick ports nothing holds: list
the host's sockets (`ss -lntu`) and the ports pods hold
(`kubectl get pods -A -o json | jq '[.items[].spec.containers[].ports[]? | select(.hostPort)]'`).
duckpond holds `100.65.253.102:8844`; sealpond took `8845` and UDP `9101`.

Two nodes on one machine share its CPUs with no isolation between them. Each
node runs its RPC threads at nice 19, and with `block_cpus` set in its
`node.toml` it pins its block threads to those CPUs and its RPC threads to the
rest (`crates/noded/src/cores.rs`). Give each node its own `block_cpus`, and
fence them off from the other node and from every other pod, or the block
threads compete for the same CPUs like any other processes.

## 5. Reach a model gateway

An agent's model calls go out through egress: harness asks the egress
services the provider's route names (`SetRoute`, § 10 step 6), and a node
that fronts that service in its
`egress.toml` answers. The node must also serve `relay`, which harness uses
for every tool call. The service name is yours to choose. duckpond's is
`anthropic`, and sealpond uses the same.

On fabrics-1 the gateway is ccs, in namespace `duckpond`. ccs binds only
`127.0.0.1:4141`, so a socat sidecar forwards its pod's port 4142 to it. The
Service `ccs` takes 4141 to 4142, and the NetworkPolicy `ccs-node-only`
admits only pods labelled `app: node` in the namespaces it lists. ccs runs
without `--require-key`, so it answers every request that NetworkPolicy lets
through, and the node sends no gateway key. A second network needs one thing
before its node calls ccs, the NetworkPolicy entry. It touches duckpond's
objects, so ask duckpond's operator first:

```sh
kubectl -n duckpond patch networkpolicy ccs-node-only --type=json -p '[{"op":"add","path":"/spec/ingress/-","value":{"from":[{"namespaceSelector":{"matchLabels":{"kubernetes.io/metadata.name":"sealpond"}},"podSelector":{"matchLabels":{"app":"node"}}}],"ports":[{"port":4142,"protocol":"TCP"}]}}]'
```

The node's pod resolves names through public resolvers only (`dnsPolicy:
None` in `ops/k8s/node.yaml`), so a cluster name such as `ccs.duckpond.svc`
does not resolve there. Name the Service by its ClusterIP:

```sh
kubectl -n duckpond get svc ccs -o jsonpath='{.spec.clusterIP}'   # 10.43.41.41
```

The node's `egress.toml`:

```toml
relay = true

[services.anthropic]
base = "http://10.43.41.41:4141"
```

It carries no `secrets`. So `ducktape egress set` can also send it to the
running node. That verb refuses any file whose services carry secrets (README
§ Answer calls outside the chain).

## 6. Found the network into its claim

```sh
CONTEXT=fabrics-1 EGRESS=egress.toml ops/k8s/deploy sealpond <core commit> --found   # namespace, claim `home`, pod `found`, /data/egress.toml
K="kubectl -n sealpond"
$K exec found -- ducktape identity                   # the validator's key
tar xzf founding.tar.gz
$EDITOR founding/founding.toml                       # network, time ($(date +%s000)), key, address, halo
$K cp founding sealpond/found:/data/founding
$K exec found -- ducktape init /data/founding/founding.toml --halo 100.65.253.102:9101
$K delete pod found
```

The claim keeps the node's `identity.key`, its state and `egress.toml`, and
its volume stays when the claim is deleted (`ops/k8s/network.yaml`).

## 7. Start the node

```sh
CONTEXT=fabrics-1 HOST=100.65.253.102 HTTP_PORT=8845 HALO_PORT=9101 ops/k8s/deploy sealpond <core commit>
ducktape --node http://100.65.253.102:8845 status    # its network, and a height that climbs
```

`deploy` applies `ops/k8s/tunnel.yaml` only when the namespace holds the
Secret `tunnel`, so a network reached only on its host address runs no
tunnel. The node requests 8 CPUs and 2 GiB (`ops/k8s/node.yaml`), so check
the machine has that much left to request before you deploy:
`kubectl describe node` shows its allocated resources.

## 8. Plan the accounts

What identity allows (`crates/system/identity/src/rules.rs` in programs)
decides the order:

- A founding makes no user account. Any key makes its own with `Create`.
  Each founding program gets an account first, numbered from 1, so the first
  user account's number is one past the count of `[[programs]]` in the
  founding file (17 for programs' `crates/sdk/founding/founding.toml`).
- `ducktape invite` admits a node to the validators' set. It has nothing to
  do with accounts.
- harness runs only a program account whose script calls harness. That
  account comes only from `CreateAgent`, and its signer becomes its manager
  for good. `SetScript` refuses a user account.
- So the first agent needs a manager that is not itself: a **bootstrap** user
  account (`founder`) whose one job is to create it. The manager then adds a
  key to the agent with `AddKey`, and from there the agent signs as itself.
  An agent's own record lists it, so it may `SetScript` and `SetName` on
  itself. Its manager keeps `Suspend`, `Revoke` and the list of who the agent
  answers.
- The first agent creates every later agent, and so manages them.
- The desktop app keeps one key per network and makes it the first time it
  connects to that network. A person gets an account on the new network from
  the app's sign-in screen once connected (identity `Create` with that key).
  Nobody can create it for them beforehand.

## 9. Sign ops

`ducktape op <program> '<json>' --land` signs a frame with the active
wallet, waits for its block and prints, as JSON, the block and what the op
returned; a rejected op fails with its reason. `ducktape query <program>
'<json>'` prints a query's reply as JSON. Both read the program's schema
from the `ducktape.schema` section of the code the network runs and encode
with programs' `borsh-json`, as harness's `network_call` and
`network_query` do, so an op is written in the shape `network_schema` shows:
a map is a list of `[key, value]` pairs, bytes are hex, and an option field
may be left out. `ducktape submit` and `ducktape query --raw` still take
borsh bytes.

```sh
ducktape --node http://100.65.253.102:8845 query identity '{"profile": 17}'
```

`ducktape mcp` serves the same to an agent whose harness is not a
network's own (Claude Code, Codex, any MCP client) over the Model Context
Protocol: a JSON-RPC 2.0 loop on stdin and stdout, one message a line. Its
tools are `network_schema`, `network_query` and `network_call` in harness's
own words (programs' `borsh_json::discover`), and `files_read` and
`files_write`, since a file's bytes travel as chunk blobs that no single op
carries. It signs with the active wallet key, which must act as an agent
account (identity `CreateAgent`, then `AddKey`): a model can do anything the
key's account can, so at start it refuses a key that acts as a person's
account, a program's own account or none. The password comes from
`DUCKTAPE_PASSWORD`, which the client's server config sets in its `env`; the
key's bytes never reach the model, and `initialize` names the agent and its
manager. A client runs it as `ducktape --workspace W --node URL mcp`.

`ducktape wallet new <name>` makes a key in the workspace
(`DUCKTAPE_PASSWORD` or a prompt) and prints its public key, then its
mnemonic, on stdout. Keep one workspace per key, and take that output
straight into step 11's Secret, never onto disk:

```sh
w=$(DUCKTAPE_PASSWORD="$pw" ducktape --workspace CHIEF_WS wallet new chief)
kubectl -n sealpond create secret generic chief --from-file=key=CHIEF_WS/keys/chief.key \
  --from-literal=password="$pw" --from-literal=public="$(sed -n 1p <<<"$w")" --from-literal=mnemonic="$(sed -n 2p <<<"$w")"
unset w
```

## 10. Make the bootstrap account and the first agent

`ops/found/seed.sh` runs this whole step. It sends each op with `ducktape op
… --land`, keeps what each printed in a step folder so a rerun skips what
landed, and prints the three account numbers. Make `founder`'s and `chief`'s
wallets first, each in its own workspace and straight into its Secret, as
step 9 makes chief's:

```sh
DUCKTAPE="$(command -v ducktape)" ops/found/seed.sh http://100.65.253.102:8845 <node key> \
  FOUNDER_WS FOUNDER_PASSWORD CHIEF_WS CHIEF_PASSWORD STEPS
# founder 17 chief 18 tech 19
```

`MODEL` and `SERVICE` name the model and the egress service
(`claude-opus-5-5` and `anthropic` unless set). The configurations it writes
hold a placeholder prompt, which step 13 replaces. The ops it sends, by
signer, as `ducktape op` takes them:

1. `founder`: identity `{"create": {"name": "founder", "scheme": "ed25519"}}`.
   It returns the account number F.
2. `founder`: identity `{"create_agent": {"name": "chief"}}`. It returns C,
   managed by F.
3. `founder`: identity `AddKey`, adding `chief`'s wallet key to C:
   `{"add_key": {"consent": {"key", "account": C, "expires_at", "proof"}, "scheme": "ed25519", "label": null}}`.
   chief's workspace signs the consent:
   `ducktape --workspace CHIEF_WS wallet consent --account C` prints it. It
   signs identity's `Admission` for chief's own key, at the generation
   identity holds for that key, on the node's network.
   `--expires <unix ms>` bounds it. Without it, it never expires, and a
   leaked one gives nobody anything: the `AddKey` still needs the manager's
   signature, and the consent is bound to that generation, the account and
   the network.
4. `founder`: access `{"set": {"program": "harness", "key": "agents/C", "list": "anyone"}}`,
   who the agent answers (`harness::agent_key`). duckpond's agents answer
   anyone.
5. `chief`: egress `{"register": {"service": "anthropic"}}`, then
   `{"assign": {"service": "anthropic", "nodes": [<node key>]}}`.
6. `chief`: harness `{"set_route": {"provider": "anthropic", "services": [{"service": "anthropic", "wire": "anthropic_messages"}]}}`,
   the services that reach the provider's models, in the order harness tries
   them. The first account to set a provider's route owns it.
7. `chief`: its configuration file, `/shared/agents/chief/md`. File bytes
   never ride in a frame: put them as a chunk blob
   (`ducktape blob put chief.md --kind chunk --wait`), then files
   `{"commit": {"base": null, "message": "<text>", "changes": [{"put": {"path": "/shared/agents/chief/md", "exec": false, "meta": [], "content": {"chunks": {"size": <bytes>, "chunks": [<blob id>]}}}}]}}`
   (programs `crates/app/files/src/lib.rs`, `Change::Put`).
8. `chief`: identity `{"set_script": {"account": C, "script": {"calls": "harness", "config": "/shared/agents/chief/md"}}}`.
9. `chief`: identity `{"create_agent": {"name": "tech"}}`, then tech's
   configuration, `SetScript`, and access `agents/T`, as in 7, 8 and 4. On
   duckpond the manager starts its agents' conversations, which is the list
   on `agents/T/conversation` (`harness::conversation_key`):
   `{"set": {"program": "harness", "key": "agents/T/conversation", "list": {"listed": [{"account": C}]}}}`.

A configuration file is TOML front matter between `+++` lines, then the
prompt (programs `crates/app/harness/src/config.rs`). `model` (as its vendor
names it), `provider` (whose route reaches it) and `max_tokens` are required;
`provider` is required in practice: a harness from `8c44f3e7` reads a file
without one through a model record, and with none it tells the conversation
once that the file names a model harness keeps no record of, and asks no
model.
`wait`, `thinking`, `server_tools`, `listen`, `settle`, `compact_at` and
`service` are optional:

```
+++
model = "claude-opus-5-5"
provider = "anthropic"
max_tokens = 65535
thinking = "high"
server_tools = ["web_search", "web_fetch"]
settle = "5s"
compact_at = 650000
+++
The prompt.
```

`listen` names the rooms harness starts the agent hearing every message of,
once each (chat's per-room choice stands after that); an entry ending in `*`
takes every open room whose id starts with what comes before it. harness
summarizes a conversation by itself when the next answer would no longer fit
the model's window, when the model refuses it as too long, or once its
context reaches `compact_at` tokens, whichever comes first (programs
`crates/app/harness/src/turns.rs`, `crowded`). Compaction is a model call
and can fail too. A conversation stopped on a final error is listed by
harness `FinalErrors`; save pickup notes in files before closing it (`Close`)
and opening a fresh conversation in the same place (`OpenThread`). Forking
it carries the failed tail along.

The desktop app's harness view builds the same sequence for an agent
(programs `crates/app/harness-view/src/create.rs`).

## 11. Keep the keys

- The node's key is `identity.key` on the claim `home`. The claim's volume
  outlives the claim.
- Keep `founder`'s and `chief`'s wallet files (key, password, mnemonic) as
  Secrets in the network's namespace, never in an agent's pod, as step 9
  makes them.

- `founder` signs nothing after step 10.4. Its Secret is the way back in if
  chief's key is lost, because only the manager adds a key to chief.
- Write down, in files where the agents read it, who holds each account's
  key and who reads that account's rooms and DMs. A DM to an account whose key
  sits in a Secret nobody watches reaches nobody, and an agent that asks there
  waits for an answer that never comes.

## 12. Check it is healthy

- `ducktape --node <url> status`: the height climbs.
- harness `{"agent": {"agent": C}}` and the same for T: each answers its
  configuration Ready.
- identity `{"of_key": {"key": <chief's key>}}` answers C.
- egress `{"serving": {"service": "relay"}}` and the same for `anthropic`
  each list the node's key.
- A message to chief gets an answer. Then harness `"budget_facts"` holds a
  row for the ccs account that answered it.

## 13. Reach it and seed the agents

- The desktop app's connect field takes the node's URL as it is
  (`http://100.65.253.102:8845`). The app names the network
  `<network>+<genesis time>`. Inside the cluster the node is
  `node-http.<network>.svc:8844`.
- Write each agent's prompt, memory and skills into files as the agent's
  manager or the agent itself (step 10.7's chunk-and-commit; one `Commit`
  takes a `Put` per file). A `Put` makes
  every parent folder it needs; an empty folder takes `{"mkdir": {"path"}}`
  in the same commit (programs `crates/app/files/src/tree.rs`). Read back
  with files' queries: `ducktape query files '{"find": {"prefix":
  "/shared/agents/", "page": {}}}'`, and `{"read": {"path": "<path>",
  "offset": 0, "len": <size>}}`, whose `bytes` come back as hex.
- Post the first agent a hello saying who its person is. The sender is any
  key's own account, made with identity `Create` as in step 10.1. A DM room
  opens only with chat `{"create_dm_channel": {"counterpart": C, "name"}}`
  (programs `crates/app/chat/src/lib.rs`), and then a `PostMessage` to
  `dm-<lower account>-<higher account>` lands in it, its `blocks` written as
  `[{"paragraph": [{"text": "<the hello>", "marks": []}]}]`.
- An agent hears in a room what its account chooses there in chat: a mention,
  a DM or a reply in its threads by default, every message once it listens.
  It listens itself with chat `{"listen": {"channel_id": "<id>", "listening": true}}`.
  Its manager chooses for it, or for several agents at once, with
  `{"choose_for_many": {"accounts": [T], "channel_id": "<id>", "listening": true}}`
  (programs `crates/app/chat/src/lib.rs`), or names the rooms in its
  configuration's `listen` (§10).
- A person or process off the network shares work with the agents through
  files like anyone else: a query signed by any wallet key reads them (§15),
  `ducktape query files '{"read": {"path": "<path>", "offset": 0, "len": <size>}}'`,
  so nothing needs mirroring into a repository first.

## 14. Connect an external agent

An external agent is an account that a process of its own signs for (a
bridge), not harness. It talks to people through chat like any account.

**Its account and key.** Its person makes it as a program account with no
script: identity `{"create_agent": {"name": "<name>"}}`, then the bridge's key
joins it with `AddKey` and `ducktape wallet consent`, as in steps 10.2 and
10.3. Give it no `SetScript`: harness runs only an account whose script
calls harness, so it leaves this one to its bridge. The key stays in the
bridge's workspace (`ducktape --workspace WS wallet new <name>`, with
`DUCKTAPE_PASSWORD`), never in a repo. The mnemonic `wallet new` prints
need not be kept: if the key is lost, the manager adds a new one the same
way (`wallet consent` in a fresh workspace, then `AddKey`), and the account
keeps its number.

**Hearing.** chat tells attribution who each message is for: the accounts it
mentions, both peers of a `dm-<a>-<b>` room, the earlier participants of the
thread it replies in, and the room's listeners and watchers, never its author
(programs `crates/app/chat/src/attributed.rs`). A notice (chat `Notify`: a
module's status line, one `Notice` block) is for the accounts it names in
`concerns` and the room's watchers only, never its listeners. attribution records each as a
change numbered by `seq` and writes a key that is the bytes `to/`, then the
account, then the seq, each of the two 8 bytes big-endian with nothing between
them. `ducktape changes attribution` follows
attribution's writes as `height<TAB>key hex<TAB>value hex` lines, `-` for a
delete. A bridge for account N keeps the lines whose key starts with the hex
of `to/` and N, takes the seq from the key's last 8 bytes, and reads the
change:

```sh
ducktape query attribution '{"change": {"seq": <seq>}}'
```

A change whose `source.program` is `chat`, `source.object.kind` is
`message` and `kind` is `added` is a message for N. Read it with chat
`{"message_by_id": {"message_id": "<source.object.id>"}}`: its channel, seq,
thread, author, blocks and `text`.

`ducktape changes` starts from the next block, so a bridge that restarts
reads what it missed first. Start following, then page
`{"changes_to": {"recipient": N, "page": {}}}`, passing each answer's `next`
as `{"after": <next>}` until `next` is null, and handle each seq above the
last one handled.

**Speaking.** `ducktape op chat '<json>' --land`, as the bridge's key:

- post: `{"post_message": {"channel_id": "<id>", "message_id": "<unique>", "thread": null, "blocks": [{"paragraph": [{"text": "…", "marks": []}]}]}}`.
  A reply sets `"thread": <root seq>`. A mention is a span with
  `"marks": [{"mention": {"account": <number>}}]`.
- react: `{"add_reaction": {"channel_id": "<id>", "seq": <seq>, "emoji": "👀"}}`.
- a DM: `{"create_dm_channel": {"counterpart": <number>, "name": "<name>"}}`,
  then a post to `dm-<lower>-<higher>`.
- what it hears in a room: `{"listen": {"channel_id": "<id>", "listening": true}}`
  for every message, `{"set_notices": {"channel_id": "<id>", "notices": "mentions"}}`
  or `"off"` for less.
- a room's rules: chat `{"channel": {"channel_id": "<id>"}}` answers its
  `meta`, whose key `rules` holds them (a text, or a files path).

**The reference bridge.** `ops/bridge/bridge.py` does all of this for one
account:

```sh
ACCOUNT=<number> STATE=<file> HANDLER='<command>' NODE=<url> \
  DUCKTAPE_HOME=WS DUCKTAPE_PASSWORD="$pw" ops/bridge/bridge.py
```

For each message it starts the handler afresh (`sh -c`), with
`{"change": …, "message": …}` as JSON on its stdin, and sends each line the
handler prints as a chat op with `--land`. The handler gets every variable
but `DUCKTAPE_PASSWORD`, `DUCKTAPE_HOME` included, so it can't open the wallet
key by accident: without the password `ducktape op` and `query` stop at the
prompt, though it can still run the unsigned verbs, copy the sealed key file
and rewrite `STATE`. It runs as the bridge's user, so it can read the
bridge's own environment (`/proc/$PPID/environ`), password included; keeping
the key from a handler that reads text anyone can post takes another user or
a sandbox. `STATE` holds the last seq handled. An op the network rejects (a
members-only or archived room, a deleted message) is reported on stderr, and
its change still counts as handled, so a restart never sends a rejected op
again. A handler that fails, or a `ducktape` that can't reach the node, stops
the bridge before its change counts as handled, so a restart hands that
change again and resends its ops that had landed;
and the bridge ends when the node closes the stream, so run it under
something that starts it again; it then catches up from `STATE`.
With no `STATE` file it starts from seq 0 and answers every change the
account ever had: for an account with history, write the newest `seq` of
`changes_to` into `STATE` first. A reply to a message that is not in a thread
sets `"thread"` to that message's own `seq`.

**An agent product as the account.** `ducktape bridge claude` runs Claude
Code for the account in place of a handler:

```sh
printf '%s\n' "$pw" | ducktape --workspace WS --node URL bridge claude \
  --password-stdin --state STATE -- <more claude flags, such as --model>
```

Each chat thread the account answers in gets one `claude -p` session
(stream-json in and out), resumed by its session id; a message that is not in
a thread roots its own. It answers as soon as a message reaches it, whoever
else is answering there, and each post lands as it is made. While the session
works it shows a
`Drafting`, and each answer is one `Post` with message id
`bridge-<account>-<channel>-<seq>`, so a repeat is refused, never posted
twice. A message heard while the session runs goes to the same session, which
answers it in turn. It checks identity `Hears` for every message's author, as
harness does, and takes a turn for every message that passes. Which messages
reach it is the account's hearing in chat: a mention, a DM, a reply in a
thread the account is in, every message in a room it `Listen`s to or
`Watch`es, and a notice that names it. Whose messages pass is what the account
answers (identity `Answers`, set by its manager with `SetOwnership`; the
manager always passes), as for a harness agent. A turn whose reply is empty
posts nothing. Claude's ducktape tools are `ducktape mcp` served by the bridge
itself on a socket in `STATE.relay/` (each session runs `ducktape bridge
relay`), so no child holds the key, and a wake the session arms with
`WakeOn` is told back to its thread.

The password comes on stdin: a process started with `DUCKTAPE_PASSWORD`
keeps it in `/proc/<pid>/environ`, which its children can read, since they
run as the same user. Claude gets the bridge's environment without that
variable (so `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY` reach it), and
`CLAUDE_CONFIG_DIR` set to `STATE.claude` unless the environment sets it, so
its transcripts sit beside the state and nothing reads `~/.claude`. It is
allowed the ducktape tools (`--allowedTools=mcp__ducktape`); give it any other
permission after `--`. Anything running as that user can still reach the
relay socket and act as the account. `STATE` keeps the last seq settled, each
thread's session and open input, and whether a draft of the bridge's may still
stand there. On start it sends `Undraft` for every thread where one may, so a
bridge killed mid-turn leaves no draft behind, then hands its
open input again unless its answer is already posted. With no `STATE` it
starts after the account's history.

## 15. Read a network from outside, with no key

A node's public URL serves reads to anyone:

```sh
ducktape --node https://duckpond.ducktape.industries status
git -c duck.duckpond-725bcad1.node=https://duckpond.ducktape.industries clone duck://duckpond-725bcad1/forge/<repo>
ducktape --node https://duckpond.ducktape.industries blob get <id>
```

The clone needs `git-remote-duck` on `PATH`. A program query
(`ducktape query <program> '<json>'`, a files `Read` among them) needs no
account, but `ducktape query` signs it, so it needs a wallet key in the
workspace, which `ducktape wallet new` makes. A `ducktape` speaks one node contract
(`ducktape version`), and a bridge's `ducktape` must speak the contract the
node answers in `ducktape status`.
