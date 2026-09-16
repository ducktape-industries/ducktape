//! Resource-measurement harness for terminal session and reader lifetime.
//!
//! These are measurements, not correctness gates: they print a table and are
//! ignored by default so a normal `cargo test` run stays a correctness run.
//!
//! `cargo test -p ducktape-terminal --test measure -- --ignored --nocapture`
//!
//! Printing is program output, so it uses `println!` rather than `tracing`.

use agent_service::wire;
use ducktape_terminal::{
    runtime::Runtime,
    state::{Caller, Mode, Sessions},
};
use std::{collections::BTreeMap, time::Instant};

const ID: &str = "0000000000000001";

fn owner() -> Caller {
    Caller::Account {
        account: 7,
        node: [1; 32],
    }
}

/// Resident and peak-resident kibibytes for this process.
fn memory() -> (u64, u64) {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return (0, 0);
    };
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_default()
    };
    (field("VmRSS:"), field("VmHWM:"))
}

fn open_descriptors() -> usize {
    std::fs::read_dir("/proc/self/fd").map_or(0, Iterator::count)
}

fn threads() -> usize {
    std::fs::read_dir("/proc/self/task").map_or(0, Iterator::count)
}

/// Percentile by nearest-rank over an already-sorted sample.
fn rank(sorted: &[u64], percent: u64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = (sorted.len() as u64 * percent).div_ceil(100).max(1) - 1;
    sorted[(index as usize).min(sorted.len() - 1)]
}

fn distribution(mut samples: Vec<u64>) -> (u64, u64, u64) {
    samples.sort_unstable();
    (rank(&samples, 50), rank(&samples, 95), rank(&samples, 99))
}

fn filled(chunks: u64, size: usize) -> Sessions {
    let mut sessions = Sessions::default();
    sessions.insert(ID.into(), owner(), Mode::Single).expect("insert");
    sessions.created(ID);
    for _ in 0..chunks {
        sessions.output(ID, vec![b'x'; size]).expect("output");
    }
    sessions
}

/// A caught-up reader polls from the head it just consumed. The returned page
/// is empty every time, so this isolates the scan the service pays per
/// wake-up from the cost of the bytes it actually ships.
#[test]
#[ignore = "measurement harness"]
fn caught_up_reader_poll_cost_by_history_depth() {
    const PROBES: usize = 200;
    println!("== caught-up reader poll, 64-byte chunks ==");
    println!("depth\tpoll_p50_ns\tpoll_p95_ns\tpoll_p99_ns\tns_per_retained_chunk");
    for depth in [1_000u64, 4_000, 16_000, 64_000, 256_000] {
        let sessions = filled(depth, 64);
        let mut samples = Vec::with_capacity(PROBES);
        for _ in 0..PROBES {
            let start = Instant::now();
            let replay = sessions.replay(ID, &owner(), depth, 0).expect("replay");
            samples.push(start.elapsed().as_nanos() as u64);
            assert!(replay.chunks.is_empty(), "a caught-up reader ships nothing");
        }
        let (p50, p95, p99) = distribution(samples);
        println!("{depth}\t{p50}\t{p95}\t{p99}\t{:.3}", p50 as f64 / depth as f64);
    }
}

/// The same poll, driven the way the WebSocket loop drives it: one output
/// chunk, one wake-up, one replay. Total cost against stream length is the
/// curve that matters for a long-lived session.
#[test]
#[ignore = "measurement harness"]
fn streaming_reader_total_cost_by_stream_length() {
    println!("== one reader woken per output chunk ==");
    println!("chunks\ttotal_ms\tus_per_chunk\tretained_kib\trss_kib\tpeak_kib");
    for chunks in [1_000u64, 4_000, 16_000, 64_000] {
        let mut sessions = Sessions::default();
        sessions.insert(ID.into(), owner(), Mode::Single).expect("insert");
        sessions.created(ID);
        let start = Instant::now();
        for sequence in 0..chunks {
            sessions.output(ID, vec![b'x'; 64]).expect("output");
            let page = sessions.replay(ID, &owner(), sequence, 0).expect("replay");
            assert_eq!(page.chunks.len(), 1, "one chunk per wake-up");
        }
        let elapsed = start.elapsed();
        let (rss, peak) = memory();
        println!(
            "{chunks}\t{:.1}\t{:.2}\t{}\t{rss}\t{peak}",
            elapsed.as_secs_f64() * 1000.0,
            elapsed.as_micros() as f64 / chunks as f64,
            chunks * 64 / 1024
        );
        drop(sessions);
    }
}

/// Output retention against cumulative bytes, and what ending a session
/// returns. `end` marks the record; it is not an eviction.
#[test]
#[ignore = "measurement harness"]
fn retained_bytes_and_end_of_session_release() {
    println!("== retention ==");
    println!("chunks\tchunk_bytes\tstream_mib\trss_after_kib\trss_after_end_kib\trss_after_drop_kib");
    for (chunks, size) in [(16_000u64, 1_024usize), (16_000, 8_192), (4_000, 65_536)] {
        let base = memory().0;
        let mut sessions = filled(chunks, size);
        let loaded = memory().0;
        sessions.end(ID);
        let ended = memory().0;
        drop(sessions);
        let dropped = memory().0;
        println!(
            "{chunks}\t{size}\t{:.1}\t{}\t{}\t{}",
            (chunks as f64 * size as f64) / (1024.0 * 1024.0),
            loaded.saturating_sub(base),
            ended.saturating_sub(base),
            dropped.saturating_sub(base)
        );
    }
}

struct Echo;

#[async_trait::async_trait]
impl provider_host::Provider for Echo {
    fn capability(&self) -> &str {
        "echo"
    }

    async fn run(&self, _: &str, _: &provider_host::RunContext) -> Result<String, String> {
        Err("interactive only".into())
    }

    async fn spawn_interactive(
        &self,
        _: &provider_host::RunContext,
        _: bool,
    ) -> Result<provider_host::InteractiveSession, String> {
        provider_host::InteractiveSession::spawn_local(tokio::process::Command::new("cat"))
    }
}

fn providers() -> provider_host::ProviderSet {
    let spec = provider_host::CapabilitySpec::parse(
        r#"
        spec = 1
        [capability]
        tag = "echo"
        description = "measurement"
        [detect]
        bin = "cat"
        [invoke]
        args = []
        prompt = "stdin"
        [output]
        format = "text"
    "#,
        "measure",
    )
    .expect("capability spec");
    provider_host::ProviderSet::assemble(
        provider_host::SpecSet::from_specs(vec![spec]),
        vec![Box::new(Echo)],
    )
}

/// Create/destroy churn: whether repeated session lifetimes return threads,
/// descriptors and memory, and what the never-evicted records cost.
#[test]
#[ignore = "measurement harness"]
fn session_churn_cleanup_and_residue() {
    const ROUNDS: u64 = 200;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async {
        let directory = tempfile::tempdir().expect("workdir");
        let (service, driver) =
            Runtime::start(providers(), "measure".into(), directory.path().into());
        let (base_rss, _) = memory();
        let base_threads = threads();
        let base_fds = open_descriptors();
        println!("== create/destroy churn ==");
        println!("rounds\trss_delta_kib\tpeak_kib\tthread_delta\tfd_delta\tworkdirs_left");
        let mut samples = Vec::with_capacity(ROUNDS as usize);
        for round in 0..ROUNDS {
            let session = format!("{round:016x}");
            let start = Instant::now();
            service
                .create(
                    owner(),
                    Mode::Single,
                    wire::Create {
                        session: session.clone(),
                        provider: "echo".into(),
                        restricted: false,
                        limits: BTreeMap::new(),
                        credential: None,
                    },
                )
                .await
                .expect("create");
            service
                .close(session.clone(), owner())
                .await
                .expect("close");
            samples.push(start.elapsed().as_micros() as u64);
            // Wait on the session's own end, never on a timer.
            let mut changes = service.changes();
            while !service
                .replay(session.clone(), owner(), 0, 0)
                .await
                .expect("replay")
                .ended
            {
                changes.changed().await.expect("runtime notifications");
            }
        }
        let (rss, peak) = memory();
        println!(
            "{ROUNDS}\t{}\t{peak}\t{}\t{}\t{}",
            rss.saturating_sub(base_rss),
            threads() as i64 - base_threads as i64,
            open_descriptors() as i64 - base_fds as i64,
            std::fs::read_dir(directory.path()).expect("workdir listing").count()
        );
        let (p50, p95, p99) = distribution(samples);
        println!("create+close round-trip us: p50={p50} p95={p95} p99={p99}");
        service.stop().await.expect("stop");
        driver.await.expect("driver join").expect("driver result");
    });
}
