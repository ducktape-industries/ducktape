<!-- LATTICE_LANE: 98cbfdac-a5d9-4ec5-bb26-75a001f3f7bf -->

# Repository Instructions

## Where to look

`docs/README.md` is the one index: one line per document, grouped by the
question it answers. Load the document that answers the question, never the
tree. It covers the repository README, this file and the per-area README
under `ops/`.

## No Legacy, No Compat (until a live network exists)

- There are ZERO live ducktape networks. Nothing deployed needs backward
  compatibility, wire-format tolerance, or an upgrade path from older behavior.
- Keep ONLY the latest-spec implementation of every module and protocol. When a
  spec or format changes, replace the old code — never keep a legacy decoder, a
  versioned enum arm, a compat shim, or a config alias "just in case". Dual-path
  code is a defect, not prudence.
- Version numbering is reset to v1 and stays there: no protocol-version bumps,
  no v2/v3 names, no admission gates keyed on a version number. The
  invitation/join flow in particular is v1 — a "v2" hint anywhere in it is a
  bug — except the app↔node contract number (`noded::NODE_CONTRACT`): an
  equality check the desktop app alone performs against `/v1/status`; it is
  never a tolerance window and nothing on the node or between peers reads it.
- This holds until a real network is live. Re-introducing versioning, upgrade
  gating, or migration machinery is an explicit, user-requested decision —
  never a side effect of a task.

## No Embedded Wasm (the binary is not the program set)

- A ducktape binary NEVER carries a wasm artifact in its bytes. No
  `include_bytes!` or `include_str!` of a program, in any binary or library
  crate that a binary links — not behind a feature, not behind an env var,
  not for "just this one".
- The node is one artifact and a program is another. A program ships, pins
  and swaps independently of the binary that runs it: `ducktape init` reads
  every program the founding file names into the genesis block, a joiner
  receives the bytes as blobs over state sync, and a network swaps a program
  at a block through the program bound to the programs role. Bytes compiled
  into a binary are a second copy of a program that only a rebuild can change, and a rebuild
  changing what a node founds or joins with is a silent network change.
- Tests take a fixture program's bytes from `fixture-wasm`
  (`crates/kernel/fixture-wasm`), a dev-dependency only: its build script
  compiles the `crates/kernel/fixtures/` workspace for wasm32 whenever a suite
  that loads a fixture builds, and its constants include those fresh files. No
  `.wasm` is committed, and no test includes a checked-in blob.

## No Invented Restrictions (an invented limit is an invented cap)

- An operation is refused only for a reason the user asked for or that
  correctness forces: a signature that does not verify, a key or account that
  does not exist, acting for an account the signer does not control, or an
  account that is suspended or revoked.
- Never limit an operation to one kind of account (a user's, an agent, a
  program's own account) or to one role, unless the user asked for that limit
  by name. The same holds for a UI that hides an action from some kinds of
  account.
- A limit like this found in existing code is deleted in the same change. It
  is never carried forward, documented as a rule, or made configurable: no
  setting or flag that "lifts" it.
- A design that seems to need a new restriction is a question for the user
  before it is built.

## Assistant Guidance

- Keep assistant-facing repository guidance in this file; `CLAUDE.md` links here so both assistants read the same instructions.
- Workflow helpers are user-global, not repo-tracked; the branching and
  delivery rules below still bind assistant work in this repo.

## Docs Are Not a Record

- There is no decision-record system: no ADRs, no plan/spec archive, no docs
  site. The code, its comments, the skills, and git history are the record. A
  comment states its rule outright; it never cites a document for it.
- A document states what is true at HEAD and nothing else: no dates, no
  "shipped"/"phase N"/"status" framing, no issue, PR or change numbers, no
  "what remains" lists. An open item is an issue on the tracker, not a paragraph.
  Every path, symbol, flag and constant a document names must exist in the
  tree; a claim that cannot be checked against the code is deleted.
- Specs and plans that planning workflows write under `docs/superpowers/` are
  local working files: the directory is gitignored and nothing under it ships
  in a commit. When the work lands the plan is done and the file is garbage, like
  its worktree.
- `docs/` holds only what an operator executes and the few records code
  cites by path; `docs/README.md` is the index and every document is one hop
  from it. A record nothing cites is deleted, not archived.

## Branching and Delivery

- This repository lives on duckpond's forge as `core`, and forge's `dev` and
  `main` are its branches of record. GitHub's `ducktape-industries/ducktape`
  is a read-only release mirror: forge-mirror fast-forwards its `dev` and
  `main` to forge's (`ops/README.md` § Forge and GitHub), its Actions run CI
  and build releases from those commits, and a ruleset refuses every other
  write to its `dev` and `main`. Nobody opens or merges a pull request there.
  Issues stay on GitHub.
- Nobody pushes to forge's `dev` or `main`. Every piece of work, from anyone,
  lands through a forge change: push the work as a branch, open a change from
  that branch into `dev`, and merge the change. The merge's result is ONE
  squash commit on the current `dev`, with a single parent; its subject is the
  change's title followed by `(change N)`, and its body says what changed,
  why, and the gates run. No merge commits. Forge cannot force-update a
  branch and keeps every branch pushed to it, so when `dev` has moved and the
  branch no longer merges cleanly, rebase it onto `forge/dev`, push it as a
  new branch and open a new change.
- Duckpond's agents open changes with their harness tools. Operator
  workers use `forge-change.sh` in the operator workspace
  (`/mnt/data/code/qa/.local/duckpond/operator`), run from the worktree:
  `forge-change.sh open core SLUG TITLE BODY_FILE` pushes HEAD as
  `operator/SLUG` and opens its change into `dev`, the title and body being
  the squash commit's subject and body; `forge-change.sh merge core N`
  builds the squash commit on the current `dev`, publishes it as
  `operator/SLUG-merge`, merges change N and waits until forge confirms the
  merge.
- Who merges a change: a change from duckpond's agents is reviewed and
  merged only by chief (account 17), and tech workers never merge one.
  Operator workers merge their own changes. eddy (account 18) merges his own
  changes, and the changes brought over from his GitHub pull requests are his
  to land; chief merges one of them only if eddy agrees.
- `main` moves only on an explicit, user-requested release of `dev`: a forge
  change from `dev` into `main`.
- A checkout reaches forge through a remote `forge`, with git-remote-duck
  (release `git-remote-duck-628d7c1`) on `PATH`:
  `git remote add forge duck://duckpond-725bcad1/forge/core` and
  `git config duck.duckpond-725bcad1.node https://duckpond.ducktape.industries`.
  `git fetch forge` needs nothing more. A push signs with a git key on a
  duckpond account: operator workers set
  `DUCKTAPE_HOME=/mnt/data/code/qa/.local/duckpond/operator DUCKTAPE_DEVICE_KEY_STORE=file`
  (`forge-change.sh` sets both itself), the workspace holding account 22's
  "operator git" key. `origin` may stay GitHub, which holds forge's commits
  once forge-mirror has pushed them.
- A program reaches a hosted network (duckpond) as a build of a commit on
  that network's forge, on the repo's `dev`, which holds only merged work.
  Two builders make it: the network's pods service, or eddy-claude (account
  24). The builder builds it with the repo's pinned toolchain and `--locked`,
  packs it the way a founding packs a program (its view, describe module and
  any notifier embedded), and puts the blob itself. GitHub releases
  (`wasm-<commit>`) and programs' `main` play no part in a deploy: the
  programs release workflow publishes wasm for local foundings (kit, qa), not
  for a hosted network. Never deploy a build of a branch that is not merged,
  a build of a working tree with local changes, or a build made by any other
  builder. A role program (programs, identity, valset, blobs, awaits, halo,
  access, scheduler, egress) changes only by the validators' vote, which the
  operator sends. Any account schedules an app program.
- Default feature/fix/doc work happens in an isolated git worktree rather than
  directly in the primary checkout. Use the current checkout only when the user
  explicitly asks for in-place work or the task is limited to repo-state repair.
- Put every task worktree inside the primary checkout, in one of its gitignored
  worktree directories (`.claude/worktrees/`, `.codex/worktrees/`, `.worktree/`
  are all ignored; the user-global worktree hook owns the exact path). Never
  create one as a sibling checkout or under `/tmp`. Keep Cargo targets and
  other large build outputs in the disk-backed worktree too: `/tmp` may be a
  memory-backed filesystem, so building there consumes RAM and swap.
- A task worktree forks from `forge/dev`. After the change is implemented and
  verified, land it through a forge change as above. Do not treat local
  completion as done when the requested flow is delivery.
- Merge a change only when confidence is high: the change is understood and
  the relevant gates are green or any skips are justified. If confidence is
  medium or low, leave the change open with the risks, failed checks, or
  follow-up review needed instead of merging by default.

## Delivery Speed (high confidence is the gate, not the calendar)

- **Local gates decide the merge; CI is not waited on.** When the touched
  crates' gates are green locally and the change is understood, merge the
  forge change at once. Do not wait for a review round, do not open a
  follow-up "verify" pass. CI runs on the GitHub mirror after the merge and
  catches what the box missed after the fact; a red there is a new issue, not
  a reason to have waited.
- **Gate what you touched, not the tree.** Run the per-crate clippy gate and
  the tests of the crates the diff changes. A whole-workspace run is for a
  change that spans the workspace. Never run the e2e node suites for a change
  that cannot reach them.
- **One session, one task, end to end.** The session that implements a unit
  also gates it, opens and merges its forge change, and closes the GitHub
  issue by hand (a forge merge closes nothing on GitHub). No
  implement → review → verify chains; a second pair of eyes is for a change
  the author says they do not understand.
- **Reproduce before fixing, at current `dev`.** A report names a symptom; an
  attached cause may be stale. If it does not reproduce, close the issue with
  the evidence and move on — that is delivery, not a skipped task.
- **Merged means gone.** Remove the worktree and delete its local branch as
  soon as the change merges. Then `git grep` your symbol on `forge/dev`: a
  sibling's merge can revert it.
- **`abi` lives in two repositories.** The programs a network founds with
  live in forge's `programs`. Its `crates/sdk/abi` is a copy of
  `crates/kernel/abi` here: a change to either copy is carried to the other
  in the same change. `crates/kernel/program` here is the small program SDK
  the kernel fixtures build against; that repository's `crates/sdk/program`
  is the full SDK its programs build against. They are two crates, not
  copies.
- **Hold only what is really uncertain.** A change stays open only when the
  author can name the risk in one sentence. "Waiting for CI", "waiting for
  review", or "someone else should look" are not risks.

## Worktree Cleanup (a merged worktree is garbage — remove it)

- **A worktree's life ends when its change merges.** Then remove the
  worktree and delete its branch. Leaving it costs ~20 GB of Cargo target each
  and nothing else.
- **`ops/worktree-clean.sh` does it safely.** Dry-run by default; `--yes` to
  act. It removes worktrees whose work is on `origin/dev`, as ancestors or as
  one squash commit carrying the same patch, and REFUSES one that is dirty,
  carries work `dev` lacks, holds no commit of its own, or has live
  processes under it (`--force` overrides only the last). Unmerged work is
  never its to throw away.
- Never stop desktop/QA processes with `pkill -f` — a pattern match will
  cheerfully kill an editor, a grep, or this script. Find them by process cwd,
  executable, and workspace config or let the native app shut them down.

## Logging

- Use `tracing`, never `println!`/`eprintln!`. An event reaches BOTH the node's
  stderr and the in-memory ring `noded::Logs` serves at `/v1/logs`
  (`ducktape logs`). A `println!` reaches NEITHER: it is invisible to a reader
  of the ring and unfilterable by `RUST_LOG`. Program output is not logging — a
  CLI's stdout (`ducktape <verb>`) stays `println!`.
- Two conventions coexist ON PURPOSE, and they are orthogonal — a `target` says
  WHERE an event came from, an `event` field says WHAT it is:
  - `target: "ducktape::<plane>"` — the filtering handle. `RUST_LOG=ducktape::join=debug`
    must light up a plane that spans several crates, which a crate-path target
    cannot express.
  - `event = "<stable_name>"` — the operational-contract events. These are a
    MACHINE contract: a dashboard keys on the name, so do not rename one
    without treating it as a wire change.
  Use both together on a contract event. Neither replaces the other.
- **If it can fire more than once per block, it is not `info`.** The ring holds
  4096 lines; one `info!` per 100 ms drain tick evicts the whole thing every
  ~7 minutes, destroying the context around the event you were hunting.
  `error` = stopped and will not self-heal. `warn` = we refused or dropped
  something, for a nameable reason. `info` = a lifecycle fact, at most once per
  {boot, block, epoch, session, connection}. `debug` = per-op / per-request.
  `trace` = per-frame.
- A forever-retry loop logs attempt 1, then every Nth, carrying an `attempts`
  field. An unconditional `warn!` in one is a log bomb that evicts the very
  evidence you need — and the counter IS the diagnosis.
- Never log a URI path or query string, or any key material: the ring is served
  to every reader of `/v1/logs`. A `reason` is a stable snake_case token, not
  prose — greppable and countable.
- Turn one plane up on a LIVE node rather than restarting it — a restart destroys
  the wedged state you restarted to look at:
  `ducktape log-filter 'info,ducktape::join=debug'`
  (the route MUTATES the process — a `trace` filter fills the disk — so it is an
  admin verb: the CLI signs it with the node's own identity key, and only the
  workspace holding that key can send it.)

## Rust Gates

- Per-crate lint gate:
  `cargo clippy -p <crate> --tests --no-deps` — the
  `--no-deps` is deliberate. Without it, a crate whose dev-deps pull
  host/dispatch/saga inherits ~a dozen pre-existing version-drift lints from
  those crates; a task is accountable only for lints in the crates it touched.
- A crate with a bin target AND dev-dependencies (the service bins) also
  needs `cargo build -p <crate>` with no `--tests`: under
  `--tests` every target sees the dev-dependency graph, so a source file that
  reaches a dev-only crate is green in clippy and fails the real binary.
- Don't run `cargo fmt --all`: large bin files carry pre-existing fmt debt,
  and a tree-wide reformat forces painful rebases on in-flight branches. Only
  format code you touched; the mechanical whole-tree sweep is a dedicated commit.

## Rust House Rules (code style)

Complement the lint/build gates above. The `rust` skill (let-else guards,
macros) still applies.

- **A set that grows is a table.** A collection whose members are added over
  time (the roles a network binds, the services a node offers) is a keyed
  table, a map from name to value, never one struct field per member. A borsh
  reader built before a new field cannot read the struct at all; a reader of a
  table skips the rows it does not know. A field is for what every reader must
  have.
- **Explicit control flow.** No boolean-flag steering — don't set `did_x = true`
  up top for a branch below to read; restructure (early-return, extract the two
  paths, or branch once on a discriminant). Early return over nesting: handle the
  terminal case and get out, keep the main path at the left margin. One
  discriminant, one `match`: a multi-way decision branches on one tagged value,
  never a ladder of `if`/`else if` over loosely related booleans. Hot paths (the
  consensus/drain loop, the join gate + settle, signing/redeem) must read
  top-to-bottom; a change that makes one harder to trace is wrong as written,
  even when correct — restructure until the flow is obvious again, never thread
  another flag through one to patch it.
- **State machines = one visible dispatch, pure steps.** Every input is a named
  variant on ONE event enum. ONE `match` that does nothing before or after it:
  each arm is a single delegation to a handler named for its variant — no `_`
  wildcard (a new variant must fail the build until it's routed), no match guards,
  no logic inlined in an arm. Step functions DECIDE and return command/directive
  values; a separate executor performs the effects through a few named writers, in
  order. Decide-fns never write; writers never decide — that keeps transitions
  unit-testable without I/O and effect order owned by one place. When the shape is
  load-bearing, guard it with a source-parsing lint test, not a comment.
- **Named predicates.** Every non-trivial conditional is a named `let`/`const`
  above the branch (the name is the documentation); compose a complex condition
  from smaller named predicates rather than one giant expression. Never chain
  ternaries; a second `?:`-equivalent means lifting to named predicates +
  `if`/`match`.
- **Tests wait on events, never on time.** No bounded spin / sleep-and-retry (a
  disguised timeout that flakes on slow CI). Synchronize on the system's own
  events — a channel message, a drained frame, a status callback. No wait seam
  means a missing hook in the code: add the hook, not a sleep.
- **In-seam mechanical refactors: just do them and label the step** (flag →
  discriminant, `if`/`else if` ladder → `match`, name a predicate, extract a
  nested block) — scoped to the seam you're already in, stated as its own step,
  never silently bundled into an unrelated change. Structural refactors —
  relocating code across modules, changing a boundary/public shape, adding a
  file, or fanning out beyond the seam — are ask-first; when you can't tell
  which bucket a refactor is in, treat it as structural and ask.
