# Provider session timing

## Scope

This unit adds timing-only telemetry for Codex/Claude session runs. It does
not change outcome, outbox, cancellation, timeout, or exit behavior, and it
does not implement warm sessions.

## Event contract

At `debug` level under target `ducktape::provider`:

- `event="provider_session_milestone"` records observed milestones with
  `run`, `protocol`, `session`, `thread`, `turn`, `milestone`, cumulative
  `elapsed_ms`, `delta_ms`, `previous_milestone`, and `exit_code` only for
  child exit.
- Codex milestones are `spawn`, `initialize_reply`, `thread_start_reply`,
  `turn_started`, one `first_item` for the first `item/*` notification,
  `turn_completed`, and `child_exit`.
- `event="provider_session_finished"` records `outcome`, stable `reason`,
  `last_milestone`, and `missing_milestones`. Refusal, cancellation, timeout,
  provider early exit, and successful completion are classified without
  inventing an unobserved milestone.

The run/session identifiers are existing run keys and provider protocol IDs.
Prompt text, credentials, URI/token values, and tool payloads are never logged.

## Guest observability

`run_session::drive` is host Rust. `duck-guest-init` has no tracing subscriber
or log ring; it forks/execs the provider and forwards provider stdio and exit
over the sandbox frame channel. The guest now sends a payload-free `Spawn`
frame immediately before `execve`. The host pump timestamps receipt with
monotonic `std::time::Instant`, and the driver starts elapsed timing there for
the real microVM path. Bare test children use the host spawn instant.

The provider daemon is launched by `ducktape service run compute`, which
installs the shared subscriber with `noded::log::init(None, ...compute.log)`;
events therefore reach the daemon log and stderr. This separate daemon has no
node `LogRing`, so these timing events are not claimed as app Logs-tab events.

## Verification

- `cargo test -p provider-host --features testkit`: 124 passed, 9 ignored.
- `cargo test -p sandbox-host`: 65 passed, 2 ignored; tracing-lint tests: 8
  passed.
- `cargo clippy -p provider-host --tests --no-deps --features testkit`: pass.
- `cargo clippy -p sandbox-host --tests --no-deps`: pass.
- `cargo build -p duck-guest-init`: pass.
- Deterministic scripted Codex protocol regression proves milestone order,
  first-item-only behavior, and child-exit lag as a separate event after
  `turn_completed`.

No live node, service, shell account, or model/network operation was used, so
this report contains no live latency sample. The chief should run an installed
candidate with `ducktape::provider=debug` and read the compute daemon log.
