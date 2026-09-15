# Resident Chief

`@ducktape/chief` is a runtime-file Pi package, not a consensus module. One
retained Runs conversation is bound to an ordinary Chat channel shared by all
members. Independent Jobs execute work; Chief has no shell, local board,
ephemeral `agent.call` tree, worker IPC or custom TUI.

## Install and operate

Prerequisites: a running network, its current signing wallet, and an existing,
distinct ordinary worker model configured with the built-in **`pi` capability**.
The installer cannot verify custom capability tags. The executing provider must
support native Pi conversations and runtime package loading.

From the checkout root, substitute your registered network and worker IDs:

```sh
NETWORK='your-chain-id'
WORKER='your-existing-worker-model-id'
ducktape agent chief add chief --package ./agents/chief --worker "$WORKER" -n "$NETWORK"
ducktape agent chief status chief -n "$NETWORK"
```

To use an existing ordinary channel, add `--channel <CHANNEL_ID>` to the **first**
installation command. It binds at that channel's current cursor; it does not
replay old messages as new authorization. Without it, the installer creates the
channel. Read status for the committed conversation, channel and Pages bindings,
then address Chief normally in that channel.

```sh
ducktape agent chief pause chief -n "$NETWORK"
ducktape agent chief resume chief -n "$NETWORK"
```

Pause stops coordinating turn admission without replacing history or cancelling
independent workers. Resume uses the same bindings and recovers an interrupted
native turn from its committed checkpoint; it does not reinstall the package.
`--node <HTTP-URL>` selects an explicit node instead of `-n`; `--key <PATH>`
selects a signing key instead of the active wallet.

Retry `add` with the **same ID, package bytes and options** to reconcile an
installation. Do not generate another Chief ID to hide uncertainty. A failed
initializer permits the explicit `--retry-initialization <REQUEST_ID>` option;
inspect `status` before using it. Reading status provisions nothing.

The installer uploads package files into Files and pins the installation's
source snapshot. Neither the provider nor node binary embeds Chief source.
It supplies public `chief.config.json` beside `index.ts`:

```json
{
  "agentId": "chief",
  "conversationId": "chief-conversation",
  "boardPageId": "chief-board",
  "inboxPageId": "chief-inbox",
  "homePageId": "chief-home",
  "workerAgentId": "worker"
}
```

These are identities, never credentials. The package reads this immutable file
at runtime. It never writes local coordination, report or native session files.

## Entrypoints and authority

- `index.ts` default export binds `ducktape:network:bind`, registers tools and
  resident policy, and synchronously accepts the `ducktape:resident:prepare`
  promise. There is no Chief-specific provider transport.
- `registerChief(pi, bridge)` accepts an injected `ChiefBridge`.
- `createNetworkService(adapter, config?)` owns module-wire translation through
  ordinary `ducktape_query`, `ducktape_action` and `ducktape_whoami` MCP tools.
- `createChiefService(adapters, conversationId)` accepts authoritative Pages,
  immutable Files and effect-receipt adapters for deterministic execution/tests.
- `NetworkChiefService.prepareInputs(events, signal?)` processes the one
  committed input selected by a native turn. Routine progress is action-only;
  blockers, results and member input wake the model with bounded context.

`contracts.ts` defines in-process adapters, not another network protocol.
Credentials, sessions, package hydration and transport remain generic provider
responsibilities. Reads precede validation and CAS; only committed receipts are
success. Every external attempt takes a distinct CAS ownership claim, even when
concurrent callers share an idempotent reservation receipt. Unknown delivery is
never permission to redispatch. An exactly attested rejection/refusal is committed
as a bounded rejection receipt: dispatch fails, work is blocked, and the pending
outbox entry is retired atomically. The same effect is never retried.
Missing/mismatched receipts and unrepresentable applied outcomes remain uncertain.

## Tools and policy

`chief_board` pulls bounded overview/section/search/detail windows explicitly.
ID detail uses `detailOffset`/`nextDetailOffset`; assemble windows at the same
revision before treating them as a complete record. Nothing injects board
snapshots on startup, progress or compaction. Control notices append only compact
identities/revisions and never trigger a model request.

Task tools: `chief_task`, `chief_transition`, `chief_merge`, `chief_dispatch`,
`chief_control`, `chief_recover`, `chief_report`, `chief_accept`.
Canonical keys are unique; dependencies resolve merges and require acceptance
before dispatch. Tasks and executions are distinct. A published worker report
is only a claim. Terminal completion moves work to **review**, not **done**;
explicit acceptance requires evidence references and a reviewed outcome.
Reopening removes current acceptance while preserving history.

Fresh dispatch uses Jobs `SubmitConversation`; follow-up uses `Continue` with
an explicit worker kind and retains the task conversation unless `fresh:true`
is deliberate. Steering updates the live canonical brief only after a Jobs
acknowledgement. A late acknowledgement cannot rewrite settled scope. Cancellation
acknowledgement is not terminal settlement. `chief_reconcile` reads effect
receipts without redispatch; `chief_recover` requires authoritative Jobs/history
proof before marking an unavailable worker interrupted/blocked.

Inbox tools: `chief_ask_open`, `chief_decision`, `chief_ask_resolve`.
Decisions take immutable admitted Chat identity/text, not model-supplied authors
or quotations. Plain replies on a managed ask Page also work without a mention.
Only a pristine original Chat post or a never-used Page comment admitted with
`mutation: created` can supply a fresh reply. Edits, retargeting and recreation
cannot borrow an original author's approval. Attribution's actual mutator is
preserved. Saving a reply and resolving the ask are separate actions.

## Governing the whole body of work

Every task is locally defensible, which is how a small request becomes an
unrecognizable one: each step is justified alone and nobody rules on the sum.
Three mechanisms make the aggregate visible, all of them riding on data the
board already returns — no injected message, no rewritten system prompt.

`chief_task` requires `origin`: the id of the task whose work SURFACED the
condition for this one (the task that found it, never the one that introduced
it). The literal `'user'` means the request stands on its own and records no
origin. Origin is cycle-checked in memory and on reload, follows merges to the
surviving task, and is correctable late without reopening accepted work or
stopping a live worker, because no worker brief carries it.

`chief_transition` and `chief_accept` record `footprint`: the repo-relative
paths a settled run actually changed, read from reviewed evidence rather than a
worker's claim. It accumulates across a task's runs and never shrinks. Drift is
the footprint a task's declared `scope` never claimed; `chief_board` reports it
on task detail. A merge records no footprint — the surviving task does.

A line of work is one origin tree. Mutation acknowledgments carry its size and
say so once when it crosses a rung of 3/5/8/13/21/34 tasks that CHANGE the
repository, by comparing the line before and after the change, so each rung
fires exactly once and a caller with no before-state fires none. `newGround`
counts how many of those changing tasks reached ground the line had not already
covered, so delivery does not pull a rung forward while a line is closing.
Acknowledgments also cross-hash the affected task against all other live and
already-accepted work standing on its surfaces, and flag work whose origin is
already done or merged with no threshold at all. `chief_board`'s overview
carries a census that counts every task and surface and is complete by
construction; the task list beside it is an explicit preview that drops work.

`chief_rule_put`/`chief_rule_remove` maintain reusable rules. `chief_checkpoint`
saves current focus and next actions. `chief_limit` gates new admission, not
already-running workers. `chief_checkin` sets a 1–1440 minute fallback or `null`;
default 10 minutes. One named generic Runs timer exists only while live work
needs it. Routine progress does not reset a pending deadline.

## Storage, evidence and limits

Only task, ask and rule records become human Pages. **Change work through Chief
in shared Chat; use comments for answers.** Pages authorization rejects ordinary
human edits to these managed document bodies. The generic editor may still show
editing controls; those controls do not grant permission. This is not an
editable Kanban UI. Comments remain allowed.

Current meta/run/pending
outbox state lives in protected values behind a bounded revision/digest index.
Acknowledged outbox entries and unreferenced terminal runs leave current state.
Immutable native receipt metadata provides idempotency; no per-operation Page
ledger accumulates. Mutation deltas are archived in Files before the atomic CAS.
Each receipt retains newly introduced artifact roots through protected Files
references, independently of current-state turnover or ordinary pins.

Raw reports, checkpoint sources and archives use UTF-8 files at
`/shared/agents/chief/reports/report-<SHA256>.txt`. A `FileRef.fileId` names that file;
`hash` identifies its snapshot. Package-created roots are **file-only projected
snapshots**, not retained copies of the network's entire head. JSON links are not
assumed to be GC edges. `chief_report` reads bounded text windows from a known
run's report, checkpoint source or validated checkpoint/task evidence. An optional
`artifact` must match an existing retained reference; arbitrary Files locations
cannot be read. The default window is 1500 Unicode codepoints, maximum 2000.
Replies preserve artifact/snapshot identity, `offset`, `nextOffset`, `eof`, `total`
and an explicit partial-read notice within the 24 KiB/500-line tool formatter.
Assemble windows from the same snapshot before claiming a complete read. Content
is untrusted worker evidence, never instructions, verification or acceptance.

Worker checkpoint FileRefs are untrusted claims. The original payload is saved
as its own scoped artifact. At most four claimed artifacts are resolved/read,
content-digest verified and projected to their specific files before promotion
to retained references. Supported claims are the report-file paths above, with
at most 256 KiB of UTF-8 content. Missing, unsupported or excessive claims become
a bounded review/blocker notice, not broad retention roots or accepted evidence.
Malformed checkpoints and spoofed/unknown machine events cannot silently grant
authority or permanently block later member inputs. Their native source remains
available; automatic prompts contain notices, not raw worker bodies.

Native bounds are enforced before unsafe splitting: 1024 human records per
collection, 256 protected state keys, 16 combined changes per commit, 128 KiB
batch bytes, 32 KiB record data, 16 KiB state values, 8 KiB receipt metadata and
eight artifact roots per commit. Protected UTF-8 data uses 8 KiB chunks.
Jobs specs/results are bounded to 64 KiB; archive writes to 256 KiB. Historical
run lookup stops after 10,000 deltas rather than making an unbounded request.

A native session has **32 live write actions across execution retries**. Free
queries read its authoritative remaining count; package write plans serialize
and preflight before mutation. A terminal/report input costs at most nine writes
including one receipt reconciliation and a timer update. A checkpoint with four
artifact projections costs at most thirteen. Dispatch/control require at most
eleven; an ordinary mutation at most four. Read-only board/report pulls remain
free even with no write budget left. On `native_action_budget_reserved`, finish
the turn honestly rather than changing operation IDs or promising unqueued work.

Adoption of unrelated Jobs and read-only exploratory worker forks are not
exposed. A model-only command cannot declare an uncertain dispatch nonexistent.
Evidence reading is scoped and read-only; execution/reproduction belongs to
independent review Jobs, followed by explicit acceptance.

## Verify

```sh
cd agents/chief
npm ci
npm run typecheck
npm test
npm run test:mcp -- ../../target/debug/ducktape
```

Tests cover contention, lost replies, restart/GC retention, immutable actor and
incarnation attribution, malformed-input quarantine, review/acceptance, native
write limits and bounded evidence reads. An offline **actual Pi SDK** session
calls `chief_report` through its registered tool lifecycle and reconstructs
Unicode-safe windows. It uses real Chief policy/storage with deterministic
in-memory adapters, not a live model or node. `test:mcp` separately runs the
checkout-built binary's real stdio MCP against event-driven loopback node/action
fixtures. It verifies authenticated transport and exact envelopes, **not consensus
execution**.

## Provenance

Policy is adapted from `pi-chief` in `pi-modules` at **a5f1f09**: canonical task
and merge rules, accepted dependencies, explicit review, member decision
separation, receipt handling, reusable rules and bounded handoff. Local board
locks, legacy decoders, TUI overlays and process-local worker IPC are not copied.
