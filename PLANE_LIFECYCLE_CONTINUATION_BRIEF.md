Objective: Finish the smallest truthful Plane readiness/lifecycle implementation so a committed `Kind::Plane` pending swap can arm only after this validator proves the candidate can restore the currently running owner-plane state; then prove registry activation, live selection, and restart selection agree on the same hash.

Context & decisions: This is a bounded continuation of the completed Plane lane. Preserve all existing commits and dirty work. The first report is `/tmp/claude-1000/-home-eddy-dev-ducktape-ducktape/108070b1-1a54-4bce-aa31-afed4db28198/scratchpad/plane-lifecycle/REPORT.md`. Manager review accepts the uncommitted `netstack_governance.rs` direction (`modules::code_at` replaces the local pending selector), but rejects enabling ordinary Plane readiness until the owning reachability plane has proved the candidate can restore its current snapshot. The current stale `code_announce` test comment claiming pending Plane code is the designation must be corrected. No live/qanet action.

Known facts vs assumptions:
- fact: SDK b66f47f says the registry is the consensus commitment; `ScheduledSwap::armed_at` and `modules::code_at` are the shared activation/history rules.
- fact: `CodeReadinessSignaller::decide` currently forces Plane to Resident, so it never signals.
- fact: `noded::compose::validate_deployment(Kind::Plane)` only proves verified residency (`Ok(())`); it cannot prove owner-plane execution.
- fact: the actual owner seam is `reachability::executor::Host::swap`: snapshot current machine, restore candidate from that snapshot, replace only on success. `ReachabilityCommand::SwapBackend` performs the real mutation.
- assumption to prove or reject in the report: the smallest non-mutating preflight is a reachability command that snapshots the live machine, calls the same `MachineFactory::restore` on the candidate, discards the restored candidate, and leaves the current machine/execution status untouched. Do not silently substitute boot/instantiate-only validation for restore/carry-over.

Repo: `/home/eddy/dev/ducktape/ducktape`  Base: `origin/dev @ d809eaca20faf1f9ed89d61fe3eba9cb150a436c` plus existing WIP commits `3419e3a59` and `470f70813`  Branch: `fix/plane-lifecycle-boundary`  Worktree: `/home/eddy/dev/ducktape/ducktape/.codex/worktrees/plane-lifecycle` (already exists; do not create/move another worktree).

Files & symbols:
- `crates/networking/reachability/src/executor.rs`: `ReachabilityCommand`, `Input`, `MachineFactory::restore`, `Host::swap`.
- `crates/networking/reachability/tests/executor_e2e.rs`: real command/executor tests; synchronize on replies/events, never time.
- `bin/node/src/reachability_plane.rs`: `swap_netstack`, live-plane command routing.
- `bin/node/src/validator/code_announce.rs`: `CodeReadinessSignaller`, `CodeVerdict`, `CodeActions`, completion latches/tests.
- `bin/node/src/validator/run/drain.rs`: readiness pump, fetch completion lane, current synchronous module/view verdict.
- `bin/node/src/netstack_governance.rs`: keep the reviewed `modules::code_at` selection and tests; remove stale claims.
- `crates/kernel/host/tests/module_register.rs`: three SDK-b66f47f `instantiate` test callers are compile-stale; make only the direct shape correction needed by the new return type so the integration gate can run.

Scope:
1. Add the minimum owner-plane preflight seam. It must use the same candidate restore path and current live snapshot as an actual swap, but must not replace the running machine, emit a swapped status, retarget, push an interface, or otherwise mutate plane execution.
2. Integrate preflight asynchronously with the readiness pump: no signal before a successful owner answer; one in-flight preflight per exact swap/digest; success may produce `SwapReady`; deterministic refusal is reported/latches once; absence/no running plane is not falsely called loadable and may retry when the plane becomes available. Do not block the validator select loop awaiting the plane.
3. Remove the forced-Resident Plane route only when the above proof is wired. Keep residents fetch-only.
4. Preserve `modules::code_at` as the sole live/startup selection rule. Prove an armed Plane advances to active, pending clears, live reconciliation selects it, and restart/history selects it; module host never seats it.
5. Apply only the direct `module_register` test compile correction required by SDK b66f47f.

Non-goals: no new wire/protocol version, no compatibility path, no new governance action, no admin-route behavior change, no module/view readiness rewrite, no live/qanet operation, no guest build/artifact mutation, no edits to sibling repositories, no edits to the existing Files/workspace/completion scaffolding except preserving it. Do not commit or push; manager will reconcile the full frozen set.

Interfaces & peers: SDK stays exactly `b66f47f`; use its `Kind::Plane`, `ScheduledSwap::armed_at`, and `modules::code_at`. The reachability command API is owned here. Keep the module boundary generic and Plane-unseated. No peer messaging and no CCS registration; deliver only the report file.

Constraints: use the worktree-local `CARGO_TARGET_DIR=$PWD/target-plane-lifecycle`; no `rm`, kill, `--force`, account/route changes, secrets, sleeps/spins in tests, loosened asserts, ignored tests, env/fixture/module-name special cases as control flow, or live processes. Preserve unrelated dirty files. State-machine rule: route each new command/event through the existing visible dispatch and a named handler; decision functions do not perform I/O. Reproduce red before green where practical. Bug fix is at the shared owner-plane boundary, not a netstack-name special case.

Acceptance:
- deterministic executor test proves successful preflight restores the exact current snapshot into the candidate and discards it while the old machine remains the running execution/status;
- deterministic executor test proves incompatible restore refuses without swapping or status mutation;
- signaller/drain tests prove Plane fetch -> preflight -> one readiness signal, no premature signal, refusal latch, retry on genuinely unattempted/no-plane answer, and residents never preflight/signal;
- registry/selection tests prove unready/stale suppression, armed boundary selection, active/history restart selection, and no module host seat;
- stale comments/tests are consistent with the implementation;
- direct SDK-pin compile fallout in `module_register` is corrected.

Gates (record exact totals/first failure; use `nice -n 19` and the worktree target):
- `git diff --check`
- `CARGO_TARGET_DIR=$PWD/target-plane-lifecycle nice -n 19 cargo test -j 12 -p reachability --test executor_e2e`
- `CARGO_TARGET_DIR=$PWD/target-plane-lifecycle nice -n 19 cargo test -j 12 -p node-bin --bin ducktape code_announce -- --nocapture`
- `CARGO_TARGET_DIR=$PWD/target-plane-lifecycle nice -n 19 cargo test -j 12 -p node-bin --bin ducktape netstack_governance -- --nocapture`
- `CARGO_TARGET_DIR=$PWD/target-plane-lifecycle nice -n 19 cargo test -j 12 -p host --test module_register`
- `CARGO_TARGET_DIR=$PWD/target-plane-lifecycle nice -n 19 cargo test -j 12 -p noded --test compose`
- `CARGO_TARGET_DIR=$PWD/target-plane-lifecycle nice -n 19 cargo clippy -j 12 -p reachability -p node-bin -p host -p noded --tests --no-deps`
- `CARGO_TARGET_DIR=$PWD/target-plane-lifecycle nice -n 19 cargo build -j 12 -p node-bin`

Deliverable: write `/tmp/claude-1000/-home-eddy-dev-ducktape-ducktape/108070b1-1a54-4bce-aa31-afed4db28198/scratchpad/plane-lifecycle-continuation/REPORT.md` with outcome READY or BLOCKED, root-cause/design result, exact changed files, red/green evidence, gate totals, remaining risks, and `git diff --stat`. Leave all worktree changes uncommitted and do not end before the report's final marker `PLANE_LIFECYCLE_CONTINUATION_READY` or `PLANE_LIFECYCLE_CONTINUATION_BLOCKED` exists.
