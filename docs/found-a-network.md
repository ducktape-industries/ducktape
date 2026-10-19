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
| Its limits | the founding file's `[limits]` | a hosted network with agents founds unmetered, as duckpond does: leave `[limits]` out |
| Who manages the first agent | step 8 | fixed for good: identity refuses `SetOwnership` |
| The model gateway | step 5 | the egress service the agents' model record names |

## 1. Pick the commits

- Programs: a commit on forge `programs` `dev`. A hosted network runs the
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
service its model record names, and a node that fronts that service in its
`egress.toml` answers. The node must also serve `relay`, which harness uses
for every tool call. The service name is yours to choose. duckpond's is
`anthropic`, and sealpond uses the same.

On fabrics-1 the gateway is ccs, in namespace `duckpond`. ccs binds only
`127.0.0.1:4141`, so a socat sidecar forwards its pod's port 4142 to it. The
Service `ccs` takes 4141 to 4142, and the NetworkPolicy `ccs-node-only`
admits only pods labelled `app: node` in the namespaces it lists. The gateway
key, which `ccs serve --key` prints, is `CCS_KEY` in the Secret `egress`. A second network
needs two things before its node calls ccs. Both touch duckpond's objects, so
ask duckpond's operator first:

```sh
kubectl -n duckpond patch networkpolicy ccs-node-only --type=json -p '[{"op":"add","path":"/spec/ingress/-","value":{"from":[{"namespaceSelector":{"matchLabels":{"kubernetes.io/metadata.name":"sealpond"}},"podSelector":{"matchLabels":{"app":"node"}}}],"ports":[{"port":4142,"protocol":"TCP"}]}}]'
kubectl -n duckpond get secret egress -o json \
  | python3 -c 'import json,sys; s=json.load(sys.stdin); print(json.dumps({"apiVersion":"v1","kind":"Secret","metadata":{"name":"egress","namespace":"sealpond"},"data":{"CCS_KEY":s["data"]["CCS_KEY"]}}))' \
  | kubectl apply -f -
```

The node's pod resolves names through public resolvers only (`dnsPolicy:
None` in `ops/k8s/node.yaml`), so a cluster name such as `ccs.duckpond.svc`
does not resolve there. Name the Service by its ClusterIP:

```sh
kubectl -n duckpond get svc ccs -o jsonpath='{.spec.clusterIP}'   # 10.43.41.41
```

`ops/k8s/node.yaml` gives the node each key of the Secret `egress` both as an
environment variable and as a file under `/secrets/egress/`. A file secret
changes with `ducktape egress reload` and no restart (README § Answer calls
outside the chain). The node's `egress.toml`:

```toml
relay = true

[services.anthropic]
base = "http://10.43.41.41:4141"
secrets = { x-api-key = { file = "/secrets/egress/CCS_KEY" } }
```

Instead of either kubectl step, a network can borrow the service from a
network that fronts it, and nothing of the lender's is copied (README § Answer
calls outside the chain, lending). Your node's key makes an account on the
lending network, the service's owner there names that account in the access
list on egress `lend/<service>`, and your node's `egress.toml` names the
lending node:

```toml
relay = true

[services.anthropic]
lent = "https://duckpond.ducktape.industries"
```

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
ducktape --node http://100.65.253.102:8845 query identity '{"Profile": 17}'
```

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

1. `founder`: identity `{"Create": {"name": "founder", "scheme": "Ed25519"}}`.
   It returns the account number F.
2. `founder`: identity `{"CreateAgent": {"name": "chief"}}`. It returns C,
   managed by F.
3. `founder`: identity `AddKey`, adding `chief`'s wallet key to C:
   `{"AddKey": {"consent": {"key", "account": C, "expires_at", "proof"}, "scheme": "Ed25519", "label": null}}`.
   chief's workspace signs the consent:
   `ducktape --workspace CHIEF_WS wallet consent --account C` prints it. It
   signs identity's `Admission` for chief's own key, at the generation
   identity holds for that key, on the node's network.
   `--expires <unix ms>` bounds it. Without it, it never expires, and a
   leaked one gives nobody anything: the `AddKey` still needs the manager's
   signature, and the consent is bound to that generation, the account and
   the network.
4. `founder`: access `{"Set": {"program": "harness", "key": "agents/C", "list": "Anyone"}}`,
   who the agent answers (`harness::agent_key`). duckpond's agents answer
   anyone.
5. `chief`: egress `{"Register": {"service": "anthropic"}}`, then
   `{"Assign": {"service": "anthropic", "nodes": [<node key>]}}`.
6. `chief`: harness `{"SetModel": {"id": "<model>", "wire": "AnthropicMessages", "service": "anthropic", "name": "<model>"}}`.
   The first account to set a model owns it.
7. `chief`: its configuration file, `/shared/agents/chief/md`. File bytes
   never ride in a frame: put them as a chunk blob
   (`ducktape blob put chief.md --kind chunk --wait`), then files
   `{"Commit": {"base": null, "message": "<text>", "changes": [{"Put": {"path": "/shared/agents/chief/md", "exec": false, "meta": [], "content": {"Chunks": {"size": <bytes>, "chunks": [<blob id>]}}}}]}}`
   (programs `crates/app/files/src/lib.rs`, `Change::Put`).
8. `chief`: identity `{"SetScript": {"account": C, "script": {"calls": "harness", "config": "/shared/agents/chief/md"}}}`.
9. `chief`: identity `{"CreateAgent": {"name": "tech"}}`, then tech's
   configuration, `SetScript`, and access `agents/T`, as in 7, 8 and 4. On
   duckpond the manager starts its agents' conversations, which is the list
   on `agents/T/conversation` (`harness::conversation_key`):
   `{"Set": {"program": "harness", "key": "agents/T/conversation", "list": {"Listed": [{"Account": C}]}}}`.

A configuration file is TOML front matter between `+++` lines, then the
prompt (programs `crates/app/harness/src/config.rs`). `model` and
`max_tokens` are required. `wait`, `thinking`, `server_tools`, `listen`,
`settle`, `compact_at`, `service` and `account` are optional:

```
+++
model = "claude-opus-5-5"
max_tokens = 65535
thinking = "high"
server_tools = ["web_search", "web_fetch"]
settle = "5s"
compact_at = 650000
+++
The prompt.
```

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

## 12. Check it is healthy

- `ducktape --node <url> status`: the height climbs.
- harness `{"Agent": {"agent": C}}` and the same for T: each answers its
  configuration Ready.
- identity `{"OfKey": {"key": <chief's key>}}` answers C.
- egress `{"Serving": {"service": "relay"}}` and the same for `anthropic`
  each list the node's key.
- `chief` signs harness `"ReadBudgets"`. Once a block carries it, harness
  `"Budget"` holds ccs's status for every account ccs holds. That proves the
  path to ccs with no model tokens spent.
- A message to chief gets an answer.

## 13. Reach it and seed the agents

- The desktop app's connect field takes the node's URL as it is
  (`http://100.65.253.102:8845`). The app names the network
  `<network>+<genesis time>`. Inside the cluster the node is
  `node-http.<network>.svc:8844`.
- Write each agent's prompt, memory and skills into files as the agent's
  manager or the agent itself (step 10.7's chunk-and-commit; one `Commit`
  takes a `Put` per file). A `Put` makes
  every parent folder it needs; an empty folder takes `{"Mkdir": {"path"}}`
  in the same commit (programs `crates/app/files/src/tree.rs`). Read back
  with files' queries: `ducktape query files '{"Find": {"prefix":
  "/shared/agents/", "page": {}}}'`, and `{"Read": {"path": "<path>",
  "offset": 0, "len": <size>}}`, whose `bytes` come back as hex.
- Post the first agent a hello saying who its person is. The sender is any
  key's own account, made with identity `Create` as in step 10.1. A DM room
  opens only with chat `{"CreateDmChannel": {"counterpart": C, "name"}}`
  (programs `crates/app/chat/src/lib.rs`), and then a `PostMessage` to
  `dm-<lower account>-<higher account>` lands in it, its `blocks` written as
  `[{"Paragraph": [{"text": "<the hello>", "marks": []}]}]`.
