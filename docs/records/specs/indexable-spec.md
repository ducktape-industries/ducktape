# Indexable — the per-module materialized-view contract

The wasm index-guest architecture: a host-written op feed, engine-folded read
models (fluent31 changes-mode triggers), per-module view guests, the owed
ledger, and the op-row repair. This tier IS the human-facing read surface:
canonical module queries serve dispatch alone (§5).
Code: `crates/kernel/indexer` (the host store: feed writer + guest converge),
`index-guest` in ducktape-sdk (the contract + authoring SDK; `testmap` is the
reference mapper), the chat/tasks/pages/inbox/saga modules' `src/index.rs`
(pure decision cores) + `src/index_guest.rs` (wasm shells, packaged by
`guest-builder --index`), `crates/noded` (the feed, the HTTP lanes, the shared
store construction with bundled guests), `bin/node` (the validator and the
resident: live feed, replay feed, the owed ledger, the repair loop),
`crates/kernel/statesync` (the `IndexOps` wire lane + the puller-side walk).

## 1. Position: the derived tier

Ducktape state lives in two tiers with opposite contracts:

| | canonical tier | derived tier (this spec) |
| --- | --- | --- |
| substrate | qmdb / git — authenticated | fluent31 — ordered, scannable |
| in the root-hash | **always** (`Module::root()`) | **never** |
| cross-node | byte-identical by consensus | node-local, no determinism claim |
| reads | point lookups, module `query` | scans, search, partitions, views |
| consistency | exact at every height | **optimistic** — feed exact, views converge |
| crash story | replay / state-sync to the root | **rebuild** — replay the feed |

The canonical tier is deliberately not a database: `any::unordered` qmdb is
hashed keys and point lookups — no scans, no secondary indexes, no search.
Anything that needs those shapes is a *read model*, and read models are
derived: materialized from the finalized op stream, disposable by
construction, and invisible to consensus. Nothing a `root()` commits to may
ever live in — or be read back from — the derived tier.

NOTHING per-module is native on this tier. A module's mapper is wasm — like
its consensus logic — installed INSIDE its own index database and executed by
the engine. The node binaries hold zero per-module index code; that is what
lets an index-only node be a module-generic engine.

## 2. The two loops

The tier is two loops, coupled only through each module's database:

- **the host writer** (`IndexStore::apply_block`, called by the node's block
  loop): writes one borsh `OpRow` per applied dispatch under
  `op/{height:016x}/{seq:08x}` plus the watermark (`meta/height`), one atomic
  batch per module per block. No domain logic; this is the whole host side.
- **the fold** (the module's index guest): a fluent31 **changes-mode
  trigger** (`"fold"`) on the `op/` range delivers every committed op row to
  the guest's `on_apply` — exactly once, in commit order, the row inline up to
  the engine's inline cap (64 KiB). A larger row arrives key-only and the
  shell re-reads it from state, which is exact: op rows are immutable. The
  guest folds it into derived keys inside its own transaction: its writes and
  the event's consumption commit together (at-least-once invocation,
  exactly-once effects). Trigger-made writes never re-trigger; loops are
  impossible by engine construction.

The fold is ASYNC and OPTIMISTIC by design (the user's contract: "optimistic
now, consensus-consistent later"). The watermark vouches for the FEED alone —
"every block at or below H is fully in the feed" — and EVERY module's
watermark advances on EVERY applied block, so `watermark < H` always means
"blocks are missing", never "the module was quiet". Derived views trail the
feed by the trigger backlog, which is observable, never guessed:
`IndexStore::fold_status` (pending + last drain error) rides
`GET /v1/index/status` as `fold.{module}`.

Every module gets the feed, the watermark, and the owed ledger
(`meta/owed`, §6) with zero module code. A module becomes **indexable** by
shipping an index guest.

## 3. The index guest

A module's mapper is a fluentabi module (core wasm32) installed under the
name `index` in the module's own database, with up to two roles:

- **fold** (`on_apply` export) — consumes the feed as above;
- **view** (`query` export) — the module's read projection, served read-only
  at one MVCC snapshot (`POST /v1/index/{module}/view`).

Ship both, either, or none: a fold-only guest maintains scan-served keys; a
module with no `query` role answers `ViewUnsupported` (§5). The store
converges each database on the bundled artifact at open — install
(overwrite-put; replacing bytes IS the upgrade path), trigger registration
when the guest folds, teardown of both when a module stops shipping one. A
converge marker (`meta/guest`: artifact hash + roles) written last makes a
warm boot free — a matching marker skips every wasm compile.

Because the guest lives in the database's engine keyspace, no clear this
tier performs can touch it — `converge_guest`'s and `refold`'s clears both
sweep only derived keys. Every node installs the mapper from the module's
running deployment at open and activation. The component and optional mapper
travel together through the blob plane under one deployment hash. A changed
mapper refolds the retained feed; removing it clears its derived rows. Readers
wait through the replacement, and a view's advisory watermark is read under
the same deployment guard as its rows.

### 3.1 Authoring shape: decide pure, write thin

A mapper is two files in the module crate:

- `src/index.rs` — the DECISION core, compiled everywhere: pure
  `fold_op(&OpRow, &impl StateRead) -> Result<Writes, Fail>` and
  `serve_view(&impl StateRead, &[u8]) -> Result<Vec<u8>, Fail>` over the
  `index_guest` contract crate (borsh `OpRow`, `StateRead` reads + paging,
  `Writes` command lists, the `search` posting helpers). Unit-tested
  natively against a plain `BTreeMap` — the map backend and `apply_to_map`
  are the test twins of the engine backend.
- `src/index_guest.rs` — the SHELL, behind the crate's `index-guest`
  feature: backs `StateRead` with the engine ABI (`EngineRead`), applies the
  decided writes, and exports the roles via `index_guest::fold!`/`view!`.
  The whole file is ~15 lines; `guest-builder --index` (ducktape-sdk)
  packages it into the committed `index.wasm`.

Within one op a read never sees that op's own writes (they apply after the
decision); across ops in one feed batch it sees everything earlier — the
engine transaction and the native test harness agree on this by
construction. The decision core depends on exactly two internal things: the
`index_guest` contract crate and the module's crate-root wire types — never
the module's state machinery, never `sdk`/`host`/`indexer`.

### 3.2 Fold rules

1. **Applied ops only, all of them.** The feed carries exactly the
   dispatches consensus applied — root ops *and* follow-ups — in drain
   order. A failed op aborts its whole block and never reaches the feed.
   Mirror module semantics on that assumption (`tasks` files every
   `CreateTask` as `Open` because a duplicate create would have aborted;
   `chat` mirrors `head_seq` because every applied post assigned exactly the
   next sequence).
2. **Deterministic data-in/data-out.** No IO beyond the module's own index,
   no clock, no randomness — the fluentabi runtime enforces most of this
   (no WASI, no imports beyond the engine ABI, canonicalized floats, fuel).
3. **Fail loudly, never guess — but know what failure means.** A fold
   `Fail` holds the module's OWN queue: events are retained, the engine
   backs off and retries, `fold_status` surfaces depth and reason. It no
   longer poisons the store, and it no longer touches other modules. That
   cuts both ways: interface drift (an undecodable APPLIED op) should fail —
   the queue holds until a fixed guest ships, then folds on. But a
   DETERMINISTIC failure wedges the fold forever, so a mapper whose view is
   not worth a wedged queue (saga's billing ledger) documents skip-and-
   continue instead. Choose per mapper; write it down.
4. **Reserved namespaces.** `op/` and `meta/` are host-written; the trigger
   range spans `op/` alone, so bookkeeping writes never reach the guest.
   `fold/` is the SHELL's, written by `index_guest::guest::fold_batch` inside
   the fold transaction — today `fold/tip`, the `(height, seq)` of the last op
   row consumed, 12 bytes big-endian. A mapper's decision core must not write
   into any of the three (nothing enforces it engine-side — the prefixes are
   ordinary user keys there; the contract is this spec).

   `fold/tip` answers ONE question honestly: *has the fold consumed my op at
   `(H, seq)`* — read-after-your-own-write. It is not general freshness: it
   advances only on op traffic, so a quiet module's tip is arbitrarily old
   while its view is perfectly current (unlike `meta/height`, which bumps on
   every block). ABSENT is normal and means *unknown*, never height 0: a
   refold (§7) clears it with the rest of the derived state until the replay
   lands, a fresh database has none, and a module that just gained its first
   index guest has folded nothing yet. So a client waiting on the tip must
   escape by timeout, never block on it.

   A mapper UPGRADE never leaves the tip standing over the previous mapper's
   rows: `converge_guest` REFOLDS unconditionally on any artifact-hash change
   (clear the derived keyspace, re-drive the fold over the `op/` rows the
   database already holds, and WAIT it out before `open` returns), so a tip
   that survives an upgrade vouches for rows the installed mapper produced.
   `op/` and `meta/` are left alone: a new mapper changes what the rows MEAN,
   never what the feed saw.
5. **Pre-index history is out of scope.** An op referencing state the feed
   never carried (enabled mid-life, an owed range) folds to a no-op. The
   honest fixes are replaying the chain through the feed or pulling the
   source's real op rows for the owed heights (§7) — never a guessed backfill.

### 3.3 View rules

1. **Module-defined wire, like `Module::query`.** Request and reply are the
   module's own JSON shapes (externally tagged enums by house convention);
   the daemon and the store treat both as opaque bytes. A guest `Fail`
   surfaces as the view error, message intact.
2. **One snapshot per call.** Every read of a view invocation sees the same
   MVCC snapshot, concurrent with both writers.
3. **Read-only.** The engine rejects writes in a `query` invocation
   (`EROFS`), and the role check happens at the boundary.
4. **Views may be absent.** `ViewUnsupported` is a first-class answer, not a
   failure — see §5.

## 4. The endpoints (noded)

Reads never cross the node-actor lane; they run on the HTTP runtime against
snapshots, like the blob and telemetry lanes.

| route | serves |
| --- | --- |
| `GET /v1/index/status` | per-module watermarks, owed ranges, fold health (backlog + last error), the poison flag |
| `GET /v1/index/{module}/ops?after=&limit=` | the feed, paged — borsh rows projected to the JSON envelope (`payload` when the payload is JSON, `payload_hex` otherwise) |
| `GET /v1/index/{module}/scan?prefix=&after=&limit=` | raw derived keys (debugging, generic consumers) |
| `POST /v1/index/{module}/view` | **the module's own endpoint** — its guest's `query` role |

A module with no guest (or a guest with no `query` role) answers 404 on
`view`.

## 5. The substrate doctrine — where reads live

The split is a DOCTRINE, not a per-module judgment call:

- **canonical state is authenticated point-read state, nothing more.** qmdb
  is used for what it is — hash-addressable records behind a merkle root.
  A module's canonical `query` surface exists FOR dispatch: the point and
  computed reads other modules' `execute()` paths (and the host) consume.
  Consensus can never read the derived tier, so those reads stay canonical
  by necessity — and they are the ONLY reads that do.
- **no scan machinery in canonical state.** stored enumeration lists,
  stand-in range-index records, and counter-driven pagination that exists
  only to serve a human surface are defects: the engine on this tier
  iterates natively, so that is where iteration lives. (a record dispatch
  itself needs — pages' folder-parent map for cycle checks, chat's bounded
  per-message emoji index for caps and tombstone cleanup — stays canonical:
  it serves a consensus decision, not a scan.)
- **everything a human lists, scrolls, or searches is a view here.** the
  UI's read surface is `/v1/index/{module}/view`, uniformly — chat pages
  and threads, the pages sidebar and comment panels, boards, feeds, search.
- **modules hold no state in RAM.** every tenant is store-backed (qmdb) or
  odb-backed except `runs`, the one module still riding the in-memory map
  backing (`Backing::Map` in `crates/topology`); its re-platforming onto
  qmdb + this tier is the remaining move.

Exemptions are substrate facts, not preferences: forge's state *is* a git
repo (cloneable, greppable — an index would be a worse second copy), and
files' chunk bytes bypass consensus (no op stream to fold). A bounded
typed registry whose canonical reads are all dispatch-consumed (identity,
valset) simply has no read model to move.

The shipped mappers:

| module | read model | dispatch reads kept canonical |
| --- | --- | --- |
| chat | channel list, message pages, threads, revisions, reactions, members, `search` + `tags` | `Channel`, `MessagesRange` (agent context windows), `Message` (id probes) |
| pages | page list, per-target comment threads, `search` | `GetPage`/`GetBlock`, `CommentThread`/`GetComment`, `TargetThreadCount` (cap probe) |
| tasks | task `by_status` pages, job pages + census | task `List` (unpaged, dispatch-consumed), `JobsQuery::Get` |
| inbox | per-member notification pages, unread counts | none — nothing in consensus reads an inbox |
| saga | `usage` — the executor billing ledger | `Get`, `NextExpiry`, `AssignedPending` (all host/dispatch-required) |

## 6. Rebuild and failure story

There is ONE derivation path: the feed. When canonical state advances
WITHOUT the op stream — state-sync installs a boundary, an index directory
is wiped, recovery skips re-executing durable blocks — the module OWES the
heights its feed skipped (`IndexStore::owe`): the range between its watermark
and the boundary is merged into a persisted ledger under `meta/owed`, and
NOTHING else moves. No wipe, no watermark change, no trigger teardown; the
module keeps serving every row and view it holds, and new blocks keep folding
on top. The debt is visible via `GET /v1/index/status` (`owed.{module}`) and
on the wire as the floor the node vouches for (`IndexStore::vouched_floor`:
the top of its highest owed range). History re-enters only by replaying
blocks through the feed or by the op-row repair (§7). There is one fold path
and no second derivation: a mapper never re-derives rows from canonical
`Module::query` state.

- The index directory stays disposable: delete `<storage>/index` (or one
  module's subdirectory) and the tier heals — the next boot records what the
  empty databases owe, new blocks accrue above the debt, and the repair pulls
  the rest off a peer.
- noded resumes its local block counter **above** the index watermark so
  feed heights stay monotonic across restarts; at startup it records what any
  module whose watermark trails the resume floor owes (`owe_stale_modules`).
- The consensus validator feeds the identical contract from three sources:
  the live drain (every SEALED frame — a rejected frame feeds empty, it
  still consumed its height), the journal replay, and post-reboot frame
  catch-up; whatever they cannot reproduce is owed at the boot tip
  (`owe_index`) and repaired by the runtime.
- A standing RESIDENT feeds like a validator: the replica fold driver folds
  finalized frames and applies the per-block index fold from their
  dispatches. Debt is recorded only where state jumped WITHOUT frames — the
  join bootstrap, a checkpoint restart, backfilled heights — and the blocks
  database gains one honest boundary row there
  (`IndexStore::apply_block_record`).
- A ROLE CHANGE touches the index NOT AT ALL. Promotion moves the host to
  the epoch boundary and hands lanes over; the index carries its debt across
  the seat and the validator's repair loop keeps paying it. Nothing about a
  node's authority changes what its read models hold.
- **Host-write failures poison** (writes refuse, reads keep serving,
  remedy = rebuild). **Guest-fold failures never poison** — they hold that
  module's queue, observably (§3.2 rule 3). Two failure domains, two
  observables, one status route.
- Durability is `SyncMode::Periodic` (bounded loss window, torn tails
  truncate on recovery) — correct for a tier whose worst case is a rebuild.

## 7. The op-row repair

A node that state-synced holds **state**, not op history, so the feed has
nothing to carry — and a node whose views begin empty at the boundary renders
its modules as a workspace that lost its contents. The answer is the REPAIR
loop (`explorer::IndexRepair`): the node fetches a SOURCE'S OWN op rows for
the heights it owes and writes them into its own feed.

The loop is role-independent by construction. The store carries the debt,
whichever loop the node happens to be running paces the repair (the
validator's drain pass, the resident's tip poll and walk-done wake), and a
sync client any role holds serves as the source. One owed range is in flight
at a time, round-robin across modules so a range no source can pay never
starves one a source can:

1. the walk runs as its own task: `SyncRequest::IndexOps { boundary, module,
   after }` walks the source's rows for `from..=to` in ASCENDING key order,
   resuming strictly after the end of height `from - 1` (`(from - 1,
   u32::MAX)` — a watermark vouches for whole heights), cursor-paged and
   bounded by bytes, writing each page through `IndexStore::write_backfill_rows`.
   Only the delta crosses the wire; whatever the feed already held stands;
2. the settle runs ON the loop, serialized with the live block fold: the fold
   the writes triggered drains, the read model is re-derived over the whole
   feed (`IndexStore::refold`, below), and `IndexStore::settle_owed` cuts the
   range out of the ledger; `IndexStore::advance_watermark` closes it, since
   the feed now reaches the range's top;
3. a source whose own floor sits INSIDE the range vouches only for the heights
   above it: those settle, the rest stays owed, and the loop rotates to a
   source that may hold them. A source whose floor sits at or above the
   range's top vouches for none of it and counts as a refusal;
4. a refusal — no source answered, a page failed validation, a store write
   refused — leaves the range owed, rotates the source, and backs off
   (doubling from one second, capped at a minute); the first refusal logs and
   every tenth after it, carrying `attempts`. The debt is cheap to carry and
   visible, so the retry is forever.

The READ MODEL needs the refold: rows below what the fold already consumed
arrive out of key order by construction, so the derived keyspace is CLEARED
and re-driven from the whole feed afterwards (`IndexStore::refold` — drop the
guest marker, clear the derived keys, re-drive `op/` in key order, drain,
write the marker back), whether the walk finished or died holding half a
range. Views are blank for the length of that replay — the same window a
mapper swap opens at boot (§3) — with the feed under them intact throughout,
and a walk that wrote nothing skips it entirely. The marker discipline is what
makes a crash mid-refold safe: nothing vouches for the keyspace while it is
being rebuilt, so an interrupted refold is re-run whole by the next `open`
instead of being adopted.

`write_backfill_rows` never touches `meta/height` or `meta/owed`: the
watermark vouches for feed contiguity above the debt, and both move only in
the settle, once the rows below have landed.

Contents are NOT root-verifiable (the derived tier has no root by design), so
the lane trusts the serving node — accepted, because the read model is how a
node renders at all, and a joiner already trusted this exact node enough to
accept canonical state from it. What is still enforced, once, at the trust
boundary (`statesync::fetch_index_ops`): every key is byte-exactly the
canonical `op/{height:016x}/{seq:08x}` rendering of its own position,
`(height, seq)` ascends strictly across the whole walk, every height is at or
below the boundary, every row borsh-decodes as an `OpRow` agreeing with its own
key, the source's watermark covers the requested boundary (a source that folded
less would leave a HOLE above what it vouched for), and the source's own floor
stays at or below that boundary (one that rose above it holds none of the range
being asked for).

The key check is byte equality, not a successful parse, and the difference is
load-bearing. `parse_op_key` reads hex with `from_str_radix`, which accepts any
width and a leading `+`: `op/2/0` parses to `(2, 0)` while sorting AFTER
`op/0000000000000009/00000000`. Such a key would satisfy every other check above
and still break the one invariant this lane rests on — and the damage is
durable and silent, because the next refold replays `op/` in KEY order and
would rebuild every derived view from history running backwards. The FIXED
WIDTH IS THE ORDERING, so it is verified as bytes. Any violation aborts that
range's walk; the debt stands, which is honest. A source whose floor rises
MID-WALK, but stays at or below the boundary, is composed by taking the MAX
floor seen across pages: the higher floor is the one that does not overclaim.

The blocks database (`_blocks`) is deliberately NOT repaired: its rows are
node-layer observations, not derived state, and a resident writes its own
honest boundary row instead (`IndexStore::apply_block_record`, §6).

## 8. The index-only node

The tier's shape is chosen so an index-only node is a module-generic engine:
block stream in, per-module databases with their guests installed, generic
feed writes, generic view/scan/ops routes out. Zero per-module native code;
mapper upgrades arrive as data (the module-code registry distributes index
guests exactly like consensus components). Such a node is fluent31's serving
surface plus this crate's feed — nothing else. What it still lacks is a wire
lane carrying applied dispatch traces; today the feed is written only by a
node that executes blocks.
