HARD RULE: stay in this one turn until the terminal REPORT.md line is written. Do not end early for a build, test, or analysis wait. Run foreground gates. Do not register or bind any CCS session; report only through the file named below.

Objective

Close the release-6 `Kind::Plane` lifecycle contradiction with the smallest correct change: a committed plane registration/update must use the registry's ordinary readiness/activation/history lifecycle, and restart selection must read the committed `code_at` answer. The module host must still seat nothing for a plane. If the current APIs cannot establish that the owning plane can execute/restore the bytes before committing them active, stop with a precise blocker instead of claiming residency is executed-byte proof.

Context & decisions

- Owner priority: node/kernel–WASM separation and launcher-owned updates. Committed code/version and executed bytes must agree across nodes; inferred lifecycle state is rejected.
- SDK `b66f47f` is frozen. Its modules-wire contract says the registry is the consensus commitment to active code; validators signal readiness, `Advance` activates at the height, and `modules::code_at` is the one pre-/post-activation selection rule.
- Current combined WIP violates that contract: `CodeReadinessSignaller` forces `Kind::Plane` to `Role::Resident`, so it never signals ready, while `netstack_governance::designated_code` executes an unready/stale pending hash. The registry can say old/empty active + DEAD pending while the process runs the pending bytes.
- Existing live dognet and qanet have no `netstack` registry row. This is a pre-merge source correction; touch neither network.
- Ponytail/full: reuse the existing readiness path and `modules::code_at`; no new framework, wire shape, compatibility arm, dependency, or speculative abstraction.

Known facts vs assumptions

- Fact: current branch combines committed WIP `3419e3a59` (SDK pin/Plane routing) and `470f70813` (#2705 tests/routing) on origin/dev `df619f8b8`.
- Fact: `bin/node/src/validator/run/drain.rs` returns `CodeVerdict::Loadable` for a verified-resident Plane without asking the module factory; the owner plane is responsible for its artifact.
- Fact: `crates/networking/reachability/src/executor.rs::Host::swap` restores a candidate from the current snapshot atomically and keeps the old machine on refusal, but the code-readiness path currently has no explicit call into that owner-plane check.
- Assumption to verify: verified residency is the intended Plane readiness contract at b66f47f. Verify against the SDK docs and existing source/tests. If that cannot truthfully mean "active code is executable", report the missing preflight seam and do not weaken the invariant or invent a large protocol.
- Integration scaffolding is already uncommitted in Files/workspace-config/completion files so b66f47f can compile. Do not edit, revert, format, stage, or claim those files; the manager will transplant only your Plane diff.

Repo / branch / worktree

- Repo: `/home/eddy/dev/ducktape/ducktape`
- Base: `origin/dev` at `df619f8b8415b39793a0cb3dcd0b7832e0b95722`
- Branch: `fix/plane-lifecycle-boundary`
- Worktree: `/home/eddy/dev/ducktape/ducktape/.codex/worktrees/plane-lifecycle`
- `CARGO_TARGET_DIR=$PWD/target`

Files & symbols

- `bin/node/src/validator/code_announce.rs:174-230`, `CodeReadinessSignaller::decide`; current Plane test near `a_plane_entry_is_pulled_by_every_role_and_never_probed`.
- `bin/node/src/validator/run/drain.rs:1530-1580`, existing Plane verdict; change only if essential.
- `bin/node/src/netstack_governance.rs:70-115`, `designated_code`/`step`; lifecycle tests near lines 486-560.
- `crates/kernel/host/tests/module_register.rs`, committed Plane pass-over and opposite-build tests; preserve them.
- `crates/modules/system/modules/src/lib.rs` and tests: authoritative readiness/Advance/history behavior. Do not change its wire or stored shape.
- Read-only SDK reference: `/home/eddy/dev/ducktape/ducktape-sdk` at `b66f47f`, `crates/modules/system/modules/wire/src/lib.rs::code_at` and Kind docs.

Scope

1. Write the smallest deterministic red tests for the contradiction:
   - a validator holding a Plane pending hash follows the existing readiness path instead of being forced resident-only;
   - an unready or stale pending Plane hash is never selected for execution;
   - an armed pending is selected at its boundary, and the same hash selected from committed active/history after Advance/restart;
   - the module host still seats no Plane.
2. Replace any local pending-hash lifecycle reimplementation with `modules::code_at` where it is the exact shared rule.
3. Keep state-machine control flow explicit and source comments true.
4. Inspect the owner-plane restore seam. If no existing non-mutating readiness check can establish executability, state that remaining release-critical proof gap in REPORT.md with exact source/test; do not add a large async preflight design in this unit.

Non-goals

- No live/qanet/CT operations, release build, archive, guest rebuild, SDK/modules sibling edit, issue/PR, push, commit, or branch manipulation.
- No change to app/view paging candidate, Files refusal behavior, founding set, release launcher, or generic module state layout.
- No new compatibility path, protocol message, dependency, timer, sleep-based test, or module-id/token special case.

Constraints

- Never `rm`, `pkill`, `killall`, switch accounts, bind/register CCS, or touch another worktree.
- Use `graft callers <symbol>` first for code call graphs; use `rg` for strings/non-code or when graft reports an indexed limitation.
- Format only touched Plane files. Preserve all unrelated dirty scaffolding exactly.
- One discriminant/one match, named predicates, no boolean steering. Tests wait on deterministic state, never time.

Acceptance + gates

- The lifecycle tests fail on the branch's pre-fix behavior and pass after the correction (record the failing assertion/reason, not a giant log).
- `git diff --check`.
- `CARGO_TARGET_DIR=$PWD/target nice -n 19 cargo test -p node-bin --bin ducktape netstack_governance -- --nocapture`
- `CARGO_TARGET_DIR=$PWD/target nice -n 19 cargo test -p node-bin --bin ducktape code_announce -- --nocapture`
- `CARGO_TARGET_DIR=$PWD/target nice -n 19 cargo test -p host --test module_register`
- `CARGO_TARGET_DIR=$PWD/target nice -n 19 cargo test -p noded --test compose`
- `CARGO_TARGET_DIR=$PWD/target nice -n 19 cargo clippy -p node-bin -p host -p noded --tests --no-deps`
- `CARGO_TARGET_DIR=$PWD/target nice -n 19 cargo build -p node-bin`
- If a gate is blocked only by the named integration scaffolding or old guest bytes, record the exact first error and stop; do not change artifacts.

Deliverable

- Leave the minimal Plane source/test diff uncommitted for manager review/transplant.
- Write `/tmp/claude-1000/-home-eddy-dev-ducktape-ducktape/108070b1-1a54-4bce-aa31-afed4db28198/scratchpad/plane-lifecycle/REPORT.md` with: outcome PASS or BLOCKED; root cause; exact changed files; red-before/green-after test evidence; gate totals; any remaining executability/preflight proof gap; and an explicit list of unrelated dirty scaffolding left untouched.
- Last line exactly `PLANE_LIFECYCLE_DONE` or `PLANE_LIFECYCLE_BLOCKED: <one sentence>`.
