//! SIGUSR1 task dump: when a validator wedges, nothing in the tree can say
//! which async task it is parked on. tokio's unstable taskdump API answers
//! that with no new dependency (`--cfg tokio_unstable`, wired in
//! `.cargo/config.toml`, Linux x86_64/aarch64 only). The signal handler
//! itself is installed in `validator/run.rs`, next to SIGTERM/SIGINT; this
//! module holds only the dump-and-write step so that call site stays a
//! one-line delegation.
//!
//! The dump is never logged into the ring — a wedged node can be dumping
//! hundreds of tasks, and the ring is a 4096-line window other diagnostics
//! need. It goes straight to `<workspace>/tasks.txt`, overwritten each time.
//!
//! # The dump future is never dropped, and that is the whole design
//!
//! It is tempting to put a deadline on the dump: a wedged runtime is exactly
//! where one hangs. A deadline that DROPS the future is worse than no deadline,
//! and this module used to have one.
//!
//! `Handle::dump()` flips a process-wide `trace_requested` flag, waits for a
//! result, and flips the flag back — and the flip back is reached only after a
//! result arrives. Every worker, at its park step, reads that flag and, while
//! it is set, enters a barrier wait whose partner may never come (a worker
//! running synchronous work never reaches a park step at all; on #2481 that was
//! libgit2 `ll_find_deltas` under `forge::git::pack_closure_many`). The barrier
//! that STARTS a trace has a 250 ms timeout; the barrier that ENDS one has
//! none.
//!
//! So dropping the future strands the flag set for the life of the process.
//! From then on every worker pays a 250 ms barrier attempt at every park, for a
//! dump nobody is waiting for, and a worker that got as far as the end barrier
//! never leaves it — `gdb` on that founder found one still there twenty minutes
//! later. Without the drop, the same dump merely WAITS, and completes normally
//! as soon as the blocking worker yields.
//!
//! tokio exposes no way to cancel a dump in flight and the end barrier cannot
//! be bounded from outside, so the only safe shape is: run the dump to
//! completion on a detached task, say so if it is slow, and refuse a second
//! signal while one is outstanding — a queued second dump spins
//! `start_trace_request`'s `notify_all()`/`yield_now()` loop with no backoff.

/// How long to wait before telling the operator the dump has not landed yet.
/// This bounds the REPORT, never the dump.
#[cfg(all(
    tokio_unstable,
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
const REPORT_IF_SLOWER_THAN: std::time::Duration = std::time::Duration::from_secs(5);

/// One dump at a time, process-wide. A second SIGUSR1 while one is outstanding
/// is refused rather than queued: tokio serializes concurrent dumps by spinning
/// on `trace_requested`, so queueing one behind a dump that is waiting on a
/// blocked worker burns a core to no purpose.
#[cfg(all(
    tokio_unstable,
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
static DUMP_IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(all(
    tokio_unstable,
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(crate) async fn dump_tasks(workspace: &std::path::Path, label: &str) {
    use std::sync::atomic::Ordering;

    /// clears the in-flight flag however the dump task ends, panic included.
    struct InFlight;
    impl Drop for InFlight {
        fn drop(&mut self) {
            DUMP_IN_FLIGHT.store(false, Ordering::Release);
        }
    }

    if DUMP_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        tracing::warn!(
            target: "ducktape::node",
            node = %label,
            reason = "task_dump_in_flight",
            "SIGUSR1 ignored: the previous task dump has not finished"
        );
        return;
    }

    let path = workspace.join("tasks.txt");
    let label = label.to_string();
    let mut dumping = tokio::spawn(async move {
        let _in_flight = InFlight;
        // NOT under a timeout. See the module header: dropping this future
        // strands tokio's `trace_requested` flag and costs the runtime a
        // quarter second per worker per park, permanently.
        let dump = tokio::runtime::Handle::current().dump().await;
        write_dump(&dump, &path, &label);
    });

    // Waiting on the JoinHandle is not waiting on the dump: dropping a
    // JoinHandle detaches its task, so when this timeout expires the dump keeps
    // running and will still clear the flag and write the file.
    if tokio::time::timeout(REPORT_IF_SLOWER_THAN, &mut dumping)
        .await
        .is_err()
    {
        tracing::warn!(
            target: "ducktape::node",
            reason = "task_dump_slow",
            waited_ms = REPORT_IF_SLOWER_THAN.as_millis() as u64,
            "SIGUSR1 task dump has not landed yet — a worker running synchronous \
             work has to yield before tokio can trace. It is still running, not \
             abandoned, and will write the file when it completes"
        );
    }
}

#[cfg(all(
    tokio_unstable,
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn write_dump(dump: &tokio::runtime::Dump, path: &std::path::Path, label: &str) {
    use std::io::Write as _;

    let tasks: Vec<_> = dump.tasks().iter().collect();
    let write_result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(path)?;
        for task in &tasks {
            writeln!(file, "{}", task.trace())?;
            writeln!(file)?;
        }
        Ok(())
    })();

    if let Err(e) = write_result {
        tracing::warn!(
            target: "ducktape::node",
            node = %label,
            error = %e,
            reason = "task_dump_write_failed",
            "SIGUSR1 task dump could not be written"
        );
        return;
    }

    tracing::warn!(
        target: "ducktape::node",
        node = %label,
        event = "task_dump_written",
        tasks = tasks.len(),
        path = %path.display(),
        "SIGUSR1 task dump written"
    );
}

/// non-Linux / stable-tokio builds: no taskdump support. Called once at
/// boot so the operator knows why a `kill -USR1` did nothing.
#[cfg(not(all(
    tokio_unstable,
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
pub(crate) fn log_unsupported(label: &str) {
    tracing::debug!(
        target: "ducktape::node",
        node = %label,
        reason = "task_dump_unsupported",
        "SIGUSR1 task dump not installed on this target"
    );
}
