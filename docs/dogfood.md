# Dogfood ceremony: ducktape develops ducktape

The operator's runbook for the agent dogfooding loop: host this repo in its
own forge, provision a model user over it, open an issue that references a Pages
spec, mention the agent, and review the resulting PR and block-anchored spec
commentary in the app, with run outcomes available through the query API.

## 0. Prereqs

- **A running node.** The dev app (`make dev`) runs a single-node workspace
  where your node is the validator — the simplest setup, everything below
  targets it. A multi-node network works too; where it matters ("validator"
  vs "resident") the steps say so. By default every TCP listener binds
  `127.0.0.1`; `DEV_LISTEN=0.0.0.0 DEV_ADVERTISED=<this box's LAN ip>`
  widens the p2p mesh and HTTP API binds AND the dial hint peers actually
  use, so a second machine can join, huddle in, or point its app at this
  node (the WireGuard plane is bound wide regardless).
- **Host `git` on `PATH`, with worktree support.** The provisioner probes
  once at construction (`git init` + `git worktree list` in a scratch dir,
  `crates/noded/src/agent_provision/forge.rs`); a failed probe makes the forge
  lane permanently unavailable, loudly. Forge repos are git-default **sha1**
  — do NOT set `init.defaultObjectFormat = sha256` host-wide; a sha256 clone
  cannot interop with the node's repos.
- **An enabled compute service with a working microVM sandbox.** Install
  guest-architecture executors into the workspace with `ducktape agent
  install claude -n <chain-id>` or `ducktape agent install codex -n
  <chain-id>`, including the spec's declared companions. Discovery checks the
  workspace's executor directory (`<workspace>/executors`). Custom specs live
  under `<workspace>/capabilities/` (`docs/records/specs/capability-spec.md`).
  `make demo-seed` registers the demo's ChiefDuck on the built-in `claude`
  spec with the full action vocabulary, forge read and push on the seeded
  `playground` repo and on `ducktape`, every page, and its persona
  (`ops/chiefduck/SKILL.md`) as an always-loaded skill; `make dev` then
  installs the claude CLI into the workspace so its runs execute.
- **Provider authentication on the executing service.** Codex uses
  `OPENAI_API_KEY` or `CODEX_HOME/auth.json`; Claude uses `ANTHROPIC_API_KEY`,
  `CLAUDE_CODE_OAUTH_TOKEN`, or Claude Code's own login on that host — the
  macOS Keychain item `Claude Code-credentials` where Claude Code uses the
  Keychain, else `$CLAUDE_CONFIG_DIR/.credentials.json` (`~/.claude` by
  default). Installing an executable does not configure its credentials.
- **Raise the provider timeout for cold Rust builds.**
  `DUCKTAPE_PROVIDER_TIMEOUT_SECS=<secs>` on the executing compute service
  overrides the spec's *idle* timeout. The hard wall-clock cap is
  **idle × `[invoke].hard_timeout_factor`** (default 36)
  and the child is killed at the cap even while producing output, so budget
  for a first `cargo build` in a fresh worktree.

## 1. Push the repo: `make dogfood-forge`

```sh
make dogfood-forge
```

`ops/dogfood-forge.sh` resolves the node's HTTP base
from `DUCKTAPE_DEV_FORGE_URL` or, with exactly one workspace under the
ducktape home, that workspace's `http_listen` in its `node.toml`, failing if
neither resolves (the home is `$DUCKTAPE_HOME` when set, else `~/.ducktape`),
fetches `origin/dev`, and reconciles it with Forge's committed `refs/heads/dev`.
The local workspace's operator credential authorizes generic blob uploads and
module submissions, so this automation publishes as the node. `ops/forge-import.py`
builds bounded packs and submits exact previous/new ref comparisons. The script
reads materialized objects from the workspace's configured storage directory.

An HTTP 413 splits the import at ancestor commits and retries smaller portions.
Accepted ancestors remain committed if a later import fails. Re-running resumes
from that tip. A single oversized commit or merge fails with a diagnostic.
The script fast-forwards when possible, retains a Forge descendant, or joins
equal-tree divergence with a two-parent bridge; differing-tree divergence fails.
It queries the final ref and verifies the selected tip. Pack bytes remain in
blob storage; the digest and ref comparisons cross consensus.

Knobs: `FORGE_REPO` (default `ducktape`), `SOURCE_REMOTE` (default `origin`),
`SOURCE_BRANCH` (default `dev`), `SRC_REF` (explicit local-ref override), and
`DUCKTAPE_DEV_FORGE_URL`. A local workspace and its operator credential are required.

Verify the repository in the desktop **Forge** view. Re-run `make dogfood-forge`
before agent work. For user-owned Git push/clone access, install the
[Git application service](deploy/application-service.md#git-service) and use
SSH-signed pushes through its Gateway route.

## 2. Provision the dogfood model user

The persona is an **always skill**: a SKILL.md under
/shared/skills/<name>/ whose body the host includes in the run context.
The model user is a keyless identity account running a program; runs stores
its provider and skill references.

The commands below use curl, jq, Python 3 and the current ducktape binary.
They act through the node's operator credential, so that node's identity
account controls the new model user. Use that same controller for later
configuration changes; an app wallet is a separate principal.

```sh
BASE="<base>"
WORKSPACE="<workspace>"
OPERATOR="$(cat "$WORKSPACE/admin.token")"

submit() {
  jq -nc --arg target "$1" --argjson payload "$2" '{target:$target,payload:$payload}' |
    curl -fsS "$BASE/v1/submit" -H 'content-type: application/json' \
      -H "x-ducktape-admin-token: $OPERATOR" --data-binary @-
}
query() {
  jq -nc --arg target "$1" --argjson query "$2" '{target:$target,query:$query}' |
    curl -fsS "$BASE/v1/query" -H 'content-type: application/json' --data-binary @-
}

# Resolve the actual submitting key, founding its controller account if absent.
NODE_KEY="$(ducktape node key --out "$WORKSPACE/identity.key" | tail -n 1)"
KEY_BYTES="$(python3 -c 'import json,sys; print(json.dumps(list(bytes.fromhex(sys.argv[1]))))' "$NODE_KEY")"
CONTROLLER="$(query identity "{\"of_key\":{\"key\":$KEY_BYTES}}" | jq -r '.account.number // empty')"
if [ -z "$CONTROLLER" ]; then
  submit identity '{"create":{"name":"Dogfood operator","scheme":"ed25519"}}'
  CONTROLLER="$(query identity "{\"of_key\":{\"key\":$KEY_BYTES}}" | jq -er '.account.number')"
fi

FILES_HEAD="$(query files '{"refs":{}}' | jq -c '.refs.head')"
PERSONA_B64="$(printf '%s' 'You are the dogfooding duck. Work the referenced spec.' | base64 | tr -d '\n')"
submit files "$(jq -nc --argjson head "$FILES_HEAD" --arg b64 "$PERSONA_B64" '{commit:{base_snapshot:$head,message:"Install dogfood persona",changes:[{put:{path:"/shared/skills/dogfood/SKILL.md",exec:false,meta:{},content:{inline:{b64:$b64}}}}]}}')"

# Serialize the current default script; do not maintain a separate recipe copy.
PROGRAM="$(ducktape agent model-program dogfood)"
submit agent "$(jq -nc --argjson program "$PROGRAM" \
  '{provision:{name:"Dogfood Duck",program:$program}}')"
MODEL_ACCOUNT="$(query identity "{\"controlled\":{\"by\":$CONTROLLER,\"from\":0,\"limit\":256}}" |
  jq -er '.accounts | map(select(.name == "Dogfood Duck" and .control.program.executor == "agent")) |
    if length == 1 then .[0].number else error("choose a unique dogfood program account") end')"

submit runs "$(jq -nc --argjson account "$MODEL_ACCOUNT" '{
  configure_model:{operation:{register_model:{
    account:$account, agent_id:"dogfood", display_name:"Dogfood Duck",
    capability:"<your provider tag>",
    skills:[{name:"dogfood",source_prefix:"/shared/skills/dogfood",load:"always"}]
  }}}
}')"

query runs '{"model":{"query":{"agent":{"agent_id":"dogfood"}}}}'
```

The final reply is model.agent; its account must equal MODEL_ACCOUNT.
IdentityQuery::Controlled is paged; use its from cursor when the controller
already owns more than 256 accounts. Names are display labels, so the
selection above fails if the controller has several Dogfood Duck accounts.

ConfigureModel wraps runs::ModelMsg from
crates/modules/apps/runs/src/model.rs. Any member can update the record. The
model acts as its program account and submits whatever a member may: the
typed catalog operations, and any module's own message through the runs
`submit` operation.
A skill without source_snapshot follows the committed library head; supply
a snapshot id to pin it. The app's Agents view lists the resulting model,
its skills and its controller.

## 3. Write the spec in Pages

Create a page in the **Pages** view. Use **to-do blocks** for the task items
— `pages.set_checked` only applies to todo-kind blocks, so a checklist gives
you a checklist to tick as the model reports completed work.

Note the **page id**: the app mints a UUID per page (the root block id). It
isn't shown in the editor chrome; recover it from the pages view (the index
tier, `docs/records/specs/indexable-spec.md`):

```sh
curl -s <base>/v1/index/pages/view -X POST -H 'content-type: application/json' \
  -d '{"list_pages":{"limit":50}}'
# → ids + titles, cursor-paged; pick your spec's id.
```

## 4. Open the issue with a page link

The desktop Forge view lists issues but cannot open one. Open it with forge's
`open_issue` message through the `submit` helper from step 2, so the issue is
authored by the node's operator account, and put the page link in its body:

```sh
submit forge '{"open_issue":{"repo":"ducktape","title":"<title>","body":"[spec](duck://page/<your-page-id>)"}}'
```

At run compose, every ref found in the trigger message or the injected issue
body resolves against committed pages state and the page's whole subtree is
rendered into the run's context: headings, `- [ ]`/`- [x]` todos with an
inline `[blk:<id>]` on todo blocks (the ids the agent targets with its pages
actions), fenced code, a 64 KiB budget with a truncation marker
(`PAGE_CONTEXT_BYTES`, `crates/modules/apps/runs/src/inject.rs`). Page and
attachment resolution share the dispatch's bounded reference-read budget. An
unresolvable ref becomes a one-line "not found" marker — never a failed run.
One limit: refs sitting past the 16 KiB item-body truncation point
(`MAX_CONTEXT_BYTES`) aren't seen.

## 5. Mention the agent — and iterate in the PR

In the issue's **Discussion** tab, `@`-mention the agent (the composer's
typeahead resolves its account). The message commits, attribution delivers
the mention, and its program calls runs to start model work. The run then:

1. provisions an isolated clone with detached HEAD at the pinned base commit,
   with its skills mounted read-only beside it,
2. works, commits, and pushes `agent/item-<issue-number>`,
3. proposes a PR and a reply through program calls. The PR title prefers the
   bound issue's title, using the reply's first line as a fallback. The output
   tip, source branch and open item must still match when the action applies.

Repeating the mention in the original issue's Discussion continues
`agent/item-<issue-number>` and updates its existing PR.

A mention in PR `<n>`'s Discussion (hidden channel `forge:ducktape:<n>`)
forks that PR's source branch into `agent/item-<n>` and opens a child PR
targeting the original source branch. Repeated mentions in that same
Discussion continue `agent/item-<n>` and reuse the child PR. The original
source branch advances when the proposed changes are merged.

While it works, the agent can `pages.comment` on spec blocks (comments land
authored by its program account on the page). Whoever ticks a spec todo, human or
program, is recorded on it. Preflight skips emit debug breadcrumbs under
`ducktape::modules`; enable that log target to inspect them. Once an action
is admitted, its program call has an independent target outcome in
`ActionRequest`; a refusal cannot undo earlier successful effects. If a comment
didn't land, inspect that receipt and the target block/page id. Replies and
other proposed effects also depend on their
own call outcomes.

## 6. Review in-app

- **Forge view**: the PR (files/diff, merge box), the `agent/item-<n>`
  branch, and the discussion trail.
- **The spec page**: agent-authored comment threads anchored to the blocks
  they discuss; todos ticked as the work lands.

Inspect the last 100 terminal runs with the query helper from provisioning:

```sh
query runs '"recent_runs"'
```

History records include the executing node, output reference and verified PR
number. Their creation/delivery counters measure blocks, not wall-clock time.

`result_accepted` means the model result passed validation; the program decides
which proposed actions to execute. A reported program or target refusal marks
the run `action_rejected`, and its `ActionRequest` query retains the reason.
`failed` records worker failure, cancellation, or invalid output. PR links name
only an existing verified PR or a successfully committed allocation.

## Known limits

- **No page read-authorization:** `[spec](duck://page/<id>)` injection renders any
  referenced page's subtree into the run context (pages are workspace-visible
  to members). A member can surface any page they can already see.
- **Page depth is bounded:** one document allows 64 parent edges
  (`MAX_PAGE_DEPTH`). A nested Page block is a leaf in its containing
  document and starts a separate 64-edge document.
- **Concurrent branch advance is absorbed, not lost**: on a push reject the
  provisioner fetches, rebases and re-pushes a bounded number of times
  (`rebased: true` in the receipt). Rebase conflicts, exhausted push retries
  and other commit failures can degrade the run.
- A losing re-leased attempt's push can leave branch residue in a narrow
  race — harmless, but you may see an unexpected commit on the work branch.
- **Usage starts at the indexer's deploy boundary**: the ledger doesn't
  rebuild history, so runs before the indexer first ran are absent. "All
  time" means "since this deploy".

## Deploy a component from a chat run

The node process runs the module deployment executor alongside its API. There
is no separate executor binary. A model can ask its program, with the
`modules.update` action, to deploy an existing module from the run's
committed forge output. The runs Wasm projects each validator's next
directive from committed state.
The native bridge stages a hash-pinned forge file or submits the directive's
opaque message with its own node key. Module loadability is checked by the
registry's readiness process before activation. This is automatic under
validator-ballot governance; a network using governance shares receives a rejected deployment because node keys cannot cast account ballots.

Register the target module first, for example:

```sh
ducktape module register hello crates/kernel/host/tests/fixtures/hello.component.wasm \
  --after 50 --config "$WORKSPACE/node.toml"
```

Commit an artifact containing the replacement component to the run's repository.
The guest builds it with the compiler its image carries and fetches dependencies
through the run's egress proxy (`HTTP_PROXY`/`HTTPS_PROXY`, which dials off the
host and refuses the host itself). The executor consumes committed artifacts;
source compilation belongs to the run's build tools. The existing replacement
fixture is `crates/kernel/host/tests/fixtures/hello-replacement.component.wasm`.

The model's final JSON response can include:

```json
{
  "reply_blocks": [{"kind":"paragraph","text":"Replacement committed; deployment requested."}],
  "commit_message": "Update hello to count by one hundred",
  "actions": [{"operation": "modules.update", "input": {
    "module_id": "hello",
    "artifact": "hello.module",
    "code_hash": "<canonical artifact SHA-256 in lowercase hex>",
    "after": 50
  }}]
}
```

`artifact` is a preassembled deployment file within the output commit. The host
binds the repository, output branch and exact commit before passing the action
to the program. The hash covers the canonical `ModuleArtifact`, including its
length prefixes and optional mapper, rather than the component file alone:

```sh
ducktape module pack hello.component.wasm --out hello.module
```

Pass `--index <mapper.wasm>` to include a mapper and `--view <view.wasm>
--assets <dir>` to include a desktop view. The artifact is the whole deployment:
one without a mapper or a view removes the target's existing one when it
activates, so a view-only change still packs the module's current component and
mapper. Activation is
at the governance execute height plus `after`, with readiness required from every
validator.

The run's program reply acknowledges the request. Observe actual deployment
through the durable queue, using the `query` function above:

```sh
query runs '{"next_module_update":null}'
query runs '{"module_update":{"sequence":0}}'
```

A record contains the requesting program account, run, pinned forge source,
artifact spec and `requested`, `activated` or `rejected` status. Completed
records remain queryable across restarts. An expired swap or rejected proposal
releases the queue for the next request; it does not automatically create a new
proposal. Read the rejected reason before requesting another deployment.

The `node-work` crate defines the bridge protocol. It accepts opaque submissions
and preassembled forge blobs, with source-selected success and invalid-blob
continuations. Deployment voting rules, sequencing and completion conditions
live in the runs Wasm. A module policy change uses the normal Wasm update path;
the native bridge has no governance action or deployment state to change.

A directive can repeat after a restart or concurrent transaction. The source
must derive it from committed state and choose idempotent messages. The
deployment controller records per-validator staging receipts and uses stable
proposal ids, committed votes and registry status; the bridge keeps no private
workflow checkpoint. Clock values in its query are hints from local committed
status. Every target still validates messages against its execution context.

The standard Linux guest includes the Rust toolchain from `rust-toolchain.toml`,
the wasm32 target, native build utilities and the `wasm-tools` CLI of the
componentizer's release (the `wit-component` pin in
`bin/guest-builder/Cargo.toml`). Build it at the default location with:

```sh
ops/build-guest-rootfs.sh
```

Linux setup requires Bubblewrap with user namespaces, in addition to the base
image builder's tools. Ubuntu 24.04 denies an unconfined `bwrap` its user
namespace (`bwrap: setting up uid map: Permission denied`) while
`kernel.apparmor_restrict_unprivileged_userns` is 1; set it to 0 for the build
and back afterwards. It runs inside the extracted guest root with private,
disk-backed scratch and receives no host home, credentials or caches.
`ROOTFS_SETUP=/path/to/setup.sh` replaces the setup and receives command-line
arguments; `ROOTFS_SETUP=` builds only the base image. macOS builds the base
image without this Linux setup hook.

Writable run filesystems provide 8 GiB of sparse capacity. Only written blocks
consume host disk. The read-only input image retains its measured size plus
metadata margin. Headless Claude invocations allow shell, file and Ducktape MCP
tools without interactive approval; the VM defines access.

The live repair test uses the standard Claude capability, an installed CLI and
the host broker credential. The e2e lanes stage a node's workspace themselves:
`DUCKTAPE_GUEST_DIR` names the guest build they link in as each node's
`guest/`, and `DUCKTAPE_EXECUTOR_DIR` the installed CLIs the live lane execs
(a workspace's `executors/` after `ducktape agent install`). Keep `TMPDIR`
short and disk-backed for Unix sockets.
The live test is opt-in because it spends provider budget:

```sh
cargo test -p node-bin --test chat_module_upgrade_e2e \
  a_blind_agent_repairs_and_deploys_from_symptoms \
  -- --ignored --nocapture
```

The agent receives an incident report describing the observed count after a
click, faulty counter source, its wire interface and vendored dependencies.
The source comes from `hello-wasm` with unrelated conformance operations and
fixture labels removed. No repair recipe, build script, expected patch, clean
history or repaired artifact enters its checkout. Standard MCP instructions
explain the available build tools and deployment API to every run.

The host harness provisions a single-validator network and a model with
`chat.post` and `modules.update`, registers the faulty module, opens the issue
and posts the mention. The test node advertises two cores and 4 GiB of memory.
After activation the host independently rebuilds the delivered source, checks
preserved state and corrected increments, restarts the node and checks again.
Those recovery checks are host assertions; the agent's run ends with its
deployment request. This tests blind repair of one arithmetic fault, rather
than general autonomous incident detection or agent verification after deployment.
Evidence is retained under `target/self-heal/evidence/`; the provider trace is
written as events arrive and is owner-readable only. Canary and path assertions
check those specific leaks; they do not establish what the model inferred.
