<!-- LATTICE_LANE: 98cbfdac-a5d9-4ec5-bb26-75a001f3f7bf -->

# Repository Instructions

## Where to look

`docs/README.md` is the one index: one line per document, grouped by the
question it answers. Load the document that answers the question, never the
tree. It covers the operator runbooks (`docs/deploy/`, `docs/dogfood.md`,
`docs/sandbox-macos.md`), the references code cites by path (`docs/records/`),
the per-area READMEs (`ops/`, `app/`, `crates/airlock/`) and the agent runbooks
in `skills/` (`qa`, `sim-lane`, `module-dev`).

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

## No Embedded Wasm (the binary is not the module set)

- A ducktape binary NEVER carries a wasm artifact in its bytes. No
  `include_bytes!` or `include_str!` of a `component.wasm`, an `index.wasm`,
  or any other guest, in any binary or library crate that a binary links —
  not behind a feature, not behind an env var, not for "just this one".
- The node is one artifact and the module set is another. A module ships,
  pins and swaps independently of the binary that runs it: `node init` hashes
  each `<id>.component.wasm` out of a directory into the descriptor, a member
  `join` verifies its copy against those hashes, and the code registry swaps a
  module at a block. Bytes compiled into a binary are a second copy of a module
  that only a rebuild can change, and a rebuild changing what a node founds or
  joins with is a silent network change.
- Every wasm a node runs reaches it as a FILE it reads at runtime: a
  network's from its workspace `genesis` file (`node init` composes it out of
  the founding set `cargo build` stages beside the binary, resolved by
  `workspace_config::modules_dir()`; a joiner installs it at `join --genesis`
  or fetches it off the mesh at first boot). Getting the files there is a
  build, install, or hydration step, never a compile step.
- Tests may `include_bytes!` a committed fixture — a test pins bytes on
  purpose. Nothing else may.

## Internal Skills

- Keep repo-specific operational runbooks in `skills/` (`qa`, `sim-lane`,
  `module-dev`). Nothing else lives there: a prompt for an agent running
  inside a network is product, not a runbook.
- `.claude/skills` and `.codex/skills` both point to the shared `skills/` directory.
- Keep assistant-facing repository guidance in this file; `CLAUDE.md` links here so both assistants read the same instructions.
- Workflow helpers are user-global, not repo-tracked; the branching and
  delivery rules below still bind assistant work in this repo.

## Docs Are Not a Record

- There is no decision-record system: no ADRs, no plan/spec archive, no docs
  site. The code, its comments, the skills, and git history are the record. A
  comment states its rule outright; it never cites a document for it.
- A document states what is true at HEAD and nothing else: no dates, no
  "shipped"/"phase N"/"status" framing, no issue or PR numbers, no "what
  remains" lists. An open item is an issue on the tracker, not a paragraph.
  Every path, symbol, flag and constant a document names must exist in the
  tree; a claim that cannot be checked against the code is deleted.
- Specs and plans that planning workflows write under `docs/superpowers/` are
  local working files: the directory is gitignored and nothing under it ships
  in a PR. When the PR merges the plan is done and the file is garbage, like
  its worktree.
- `docs/` holds only what an operator executes (`deploy/`, `dogfood.md`,
  `sandbox-macos.md`) and the few records code or a skill cites by path
  (`records/`); `docs/README.md` is the index and every document is one hop
  from it. A record nothing cites is deleted, not archived.

## Branching and Delivery

- All task work targets `dev`: worktrees fork from `origin/dev`, PRs are based
  on `dev`, and high-confidence reviewed PRs merge into `dev`. `main` advances
  only by an explicit, user-requested release of `dev`.
- Default feature/fix/doc work happens in an isolated git worktree rather than
  directly in the primary checkout. Use the current checkout only when the user
  explicitly asks for in-place work or the task is limited to repo-state repair.
- Put every task worktree inside the primary checkout, in one of its gitignored
  worktree directories (`.claude/worktrees/`, `.codex/worktrees/`, `.worktree/`
  are all ignored; the user-global worktree hook owns the exact path). Never
  create one as a sibling checkout or under `/tmp`. Keep Cargo targets and
  other large build outputs in the disk-backed worktree too: `/tmp` may be a
  memory-backed filesystem, so building there consumes RAM and swap.
- After the worktree change is implemented and verified, submit it as a PR
  against `dev`. Do not treat local completion as done when the requested flow
  is delivery.
- Merge to `dev` only when confidence is high: the change is understood and the
  relevant gates are green or any skips are justified. If confidence is medium
  or low, leave the PR open with the risks, failed checks, or follow-up review
  needed instead of merging by default.

## Delivery Speed (high confidence is the gate, not the calendar)

- **Local gates decide the merge; CI is not waited on.** When the touched
  crates' gates are green locally and the change is understood, merge at once
  (`gh pr merge --squash`). Do not arm a watch on the CI run, do not wait for
  a review round, do not open a follow-up "verify" pass. CI stays light and
  catches what the box missed after the fact; a red there is a new issue, not
  a reason to have waited.
- **Gate what you touched, not the tree.** Run the per-crate clippy gate and
  the tests of the crates the diff changes. A whole-workspace run is for a
  change that spans the workspace. Never run the e2e node suites for a change
  that cannot reach them.
- **One session, one task, end to end.** The session that implements a unit
  also gates it, opens the PR, merges it, and closes the issue by hand (a
  merge into `dev` closes nothing; `main` is the default branch). No
  implement → review → verify chains; a second pair of eyes is for a change
  the author says they do not understand.
- **Reproduce before fixing, at current `dev`.** A report names a symptom; an
  attached cause may be stale. If it does not reproduce, close the issue with
  the evidence and move on — that is delivery, not a skipped task.
- **Merged means gone.** Remove the worktree and delete the branch as soon as
  the PR merges. Then `git grep` your symbol on `origin/dev`: a sibling's
  merge commit can revert it.
- **A module's bytes move with everything it compiles in.** A change to ANY
  crate a guest compiles (a module crate's `src/`, the module SDK, a library a
  module wraps such as `duckfs-core` or `files`) or to any shape a guest
  decodes (`Seed`, a module's message or query enum, a `deny_unknown_fields`
  record) ships the rebuilt `component.wasm`, `index.wasm` and kernel fixture
  in the SAME PR, for EVERY guest it reached. Even a deletion moves bytes:
  panic paths carry line numbers. `make wasm-rebuild-check` names the guests;
  `grep -l 'name = "<crate>"' crates/modules/*/*/guest.lock` says which
  guests compile a crate in. The
  committed guest is what every composed genesis runs; a host that speaks a
  field the guest never learned fails closed on every network founded from
  that `dev`, and the failure surfaces as a stranger's red hours later.
- **Hold only what is really uncertain.** A PR stays open only when the
  author can name the risk in one sentence. "Waiting for CI", "waiting for
  review", or "someone else should look" are not risks.

## Worktree Cleanup (a merged worktree is garbage — remove it)

- **A worktree's life ends when its PR merges.** Once merged, remove the
  worktree and delete its branch. Leaving it costs ~20 GB of Cargo target each
  and nothing else.
- **`ops/worktree-clean.sh` does it safely.** Dry-run by default; `--yes` to
  act. It removes worktrees whose branch is fully merged into `origin/dev`, and
  REFUSES one that is dirty, carries a commit not in `dev`, or has live
  processes under it (`--force` overrides only the last). Unmerged work is
  never its to throw away.
- Never stop desktop/QA processes with `pkill -f` — a pattern match will
  cheerfully kill an editor, a grep, or this script. Find them by process cwd,
  executable, and workspace config or let the native app shut them down.

## Logging

- Use `tracing`, never `println!`/`eprintln!`. An event reaches BOTH the node's
  stderr (tee'd into `<workspace>/daemon.log`) and the in-memory `LogRing` the
  app's Logs tab streams over the ws `logs` topic. A `println!` reaches NEITHER:
  it is invisible in the app and unfilterable by `RUST_LOG`. Program output is
  not logging — a CLI's stdout (`ducktape <subcommand>`, `ducktape fs`/`ducktape mcp` included)
  stays `println!`.
- Two conventions coexist ON PURPOSE, and they are orthogonal — a `target` says
  WHERE an event came from, an `event` field says WHAT it is:
  - `target: "ducktape::<plane>"` — the filtering handle. `RUST_LOG=ducktape::join=debug`
    must light up a plane that spans several crates, which a crate-path target
    cannot express.
  - `event = "<stable_name>"` — the operational-contract events (the node status
    `operations` projection and the `ducktape_*` metrics). These are a MACHINE
    contract: a dashboard keys on the name, so do not rename one without treating
    it as a wire change.
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
- Never log a URI path or query string (`/.duck/ws/{token}` carries a capability
  token in the path, and the ring is visible in the app) or any key
  material. A `reason` is a stable snake_case token, not prose — greppable and
  countable.
- Turn one plane up on a LIVE node rather than restarting it — a restart destroys
  the wedged state you restarted to look at:
  `ducktape node log-filter 'info,ducktape::join=debug' -n <chain-id>`
  (the route MUTATES the process — a `trace` filter fills the disk — so it takes
  a credential like every other mutating `/v1` route: the verb signs with the
  active wallet key, and a bare `curl` needs
  `-H "x-ducktape-admin-token: $(cat <workspace>/admin.token)"`.)
- The index engine (fluent31) and the index guests running inside it log
  through the same subscriber, under the engine's crate-path targets
  (`fluent31::db`, `fluent31::compaction`, `fluent31::trigger`, `fluent31::wasm`),
  every line naming its store (`db{dir=…}`). Its `info` is lifecycle-level
  (open, close, flush, compaction, value-log GC, a module or trigger added or
  removed); its `warn` includes a fold run failing with its backoff, a wasm
  trap, and a write stall beginning. A guest's `log` calls are `debug` under
  `fluent31::wasm::guest` and stay silent until that one target is turned up
  (`fluent31::wasm::guest=debug`, via `RUST_LOG` or the live filter route).

## Rust Gates

- Per-crate lint gate:
  `cargo clippy -p <crate> --tests --no-deps` — the
  `--no-deps` is deliberate. Without it, a crate whose dev-deps pull
  host/dispatch/saga inherits ~a dozen pre-existing version-drift lints from
  those crates; a task is accountable only for lints in the crates it touched.
- A crate with a bin target AND dev-dependencies (node-bin, simnode, the
  service bins) also needs `cargo build -p <crate>` with no `--tests`: under
  `--tests` every target sees the dev-dependency graph, so a source file that
  reaches a dev-only crate is green in clippy and fails the real binary.
- Don't run `cargo fmt --all`: large bin files carry pre-existing fmt debt,
  and a tree-wide reformat forces painful rebases on in-flight branches. Only
  format code you touched; the mechanical whole-tree sweep is a dedicated PR.
- The files crate's wasm-readiness gate:
  `cargo check -p files --no-default-features` must stay green
  (no `std::fs`/sdk leaks into the pure core).

## Rust House Rules (code style)

Complement the lint/build gates above. The `rust` skill (let-else guards,
macros) still applies.

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
