//! process-level harness for the real-socket node e2e: spawns REAL
//! `ducktape` binaries (via `CARGO_BIN_EXE_ducktape`) with generated
//! toml configs, drains their output into a feed it waits on for the node's
//! greppable markers, and speaks the json-lines rpc — the rust replacement for
//! what `demo-2node.sh` used to orchestrate in bash.
//!
//! shared by several test binaries, each using a different subset of the
//! helpers — the per-binary dead-code lint would otherwise flag whichever
//! helpers this particular binary skips.
#![allow(dead_code)]
//!
//! the constraints this harness encodes (they are invariants of the node, not
//! choices of the tests):
//! - every process needs a DISTINCT storage root (qmdb + the simplex journal
//!   persist; shared roots corrupt each other) — one tempdir per cluster.
//! - nothing supports port 0, so ports are pre-allocated by binding the whole
//!   batch simultaneously and then releasing it.
//! - the namespace is unique per cluster so a stale process from an earlier
//!   run can never handshake its way into this mesh.
//! - `converged root_hash=` latches after ANY `validator_seeds.len()` frames
//!   apply — it is a liveness marker, not proof of specific ops; state
//!   assertions go through rpc queries instead.

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// the `ducktape module …` verbs as an e2e drives them, shared by the suites
/// that exercise a live code swap.
pub mod module_verbs;

/// the kernel's checked-in `<id>.component.wasm` fixtures: the code-swap
/// suites stage `hello` / `hello-replacement` out of here. The production
/// components in it pin the same bytes [`founding_set`] holds.
pub const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../crates/kernel/host/tests/fixtures"
);

/// the founding set `cargo build` staged beside this test executable
/// (`target/<profile>/modules`): every `<id>.component.wasm`, every
/// `<id>.index.wasm`, and the netstack guest. A network has no embedded
/// wasm, so `node init` composes its genesis out of THIS directory and the
/// dev shape derives its genesis code set from it — the same resolution the
/// `ducktape` binary under test performs beside itself.
pub fn founding_set() -> &'static str {
    static DIR: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        workspace_config::modules_dir()
            .expect("cargo build stages the founding set beside the test executable")
            .to_str()
            .expect("utf-8 founding set path")
            .to_string()
    })
}

/// the beat every harness network is FOUNDED at (`node init --block-time-ms`,
/// which lands in the descriptor); a joiner inherits it off the invite and has
/// no flag of its own. the suites wait on block counts — checkpoints, epochs,
/// finalization — so their wall-clock scales 1:1 with it, and every simplex
/// timer scales with it too: the number is a policy choice for the whole lane,
/// not a tuning of any one wait.
pub const TEST_BLOCK_TIME_MS: u64 = 100;

/// A cluster's storage root, named so an ABANDONED one can be found and swept.
///
/// `tempfile::TempDir` removes itself on Drop, which covers a normal finish AND
/// a panicking test — the unwind still runs Drop, and a clean `cluster_e2e` run
/// measurably leaks nothing. What it does not cover is the process being KILLED:
/// a CI timeout, a Ctrl-C, an OOM. Then nothing unwinds and the whole storage
/// subtree survives.
///
/// That is not a nuisance where `TMPDIR` is tmpfs — it is RAM, and this box has
/// no swap. One session left **22 GB across 11 dirs**, two of them 7.2 GB, and
/// the machine only got slower until the compiler started dying mid-build.
///
/// Worse, `tempfile`'s names are random (`.tmpXXXXXX`), so a ducktape leak was
/// indistinguishable from any other program's temp dir — reclaiming it meant
/// `rm -rf /tmp/.tmp*`, a blunt instrument pointed at everyone's data.
///
/// Naming them `ducktape-e2e-<pid>-…` fixes both halves: the leak is
/// identifiable, and each new cluster first sweeps the ones whose owning
/// process is gone. A LIVE pid is never touched (sibling test binaries run
/// concurrently), and pid reuse only makes the sweep skip a directory — it can
/// never make it delete a live one.
pub fn e2e_tempdir(tag: &str) -> tempfile::TempDir {
    sweep_abandoned_e2e_dirs();
    tempfile::Builder::new()
        .prefix(&format!("ducktape-e2e-{}-{tag}-", std::process::id()))
        .tempdir()
        .expect("e2e tempdir")
}

/// Remove `ducktape-e2e-<pid>-*` roots whose owning process is gone.
fn sweep_abandoned_e2e_dirs() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix("ducktape-e2e-")) else {
            continue;
        };
        let Some(pid) = rest.split('-').next().and_then(|p| p.parse::<i32>().ok()) else {
            continue;
        };
        if pid_is_alive(pid) {
            continue;
        }
        let _ = std::fs::remove_dir_all(entry.path());
    }
}

/// Signal 0 asks "may I signal this pid" WITHOUT delivering anything — the
/// portable liveness probe. Anything other than "no such process" is treated as
/// ALIVE, so an unreadable pid is skipped rather than swept.
fn pid_is_alive(pid: i32) -> bool {
    // SAFETY: `kill` with signal 0 delivers nothing and touches no memory.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

use commonware_cryptography::{Signer as _, ed25519};

static CLUSTER_SEQ: AtomicU64 = AtomicU64::new(0);

/// a process this harness spawned — a node, a compute daemon, a service — with
/// its output in hand.
///
/// stdout and stderr share ONE pipe. a reader thread drains it into the feed
/// every wait rides AND into the log file on disk (what a post-mortem opens).
/// a line arriving, or the pipe closing behind an exit, is the event a wait
/// wakes on; a deadline only names the failure. killed (and reaped) on drop so
/// an assertion failure never leaks a validator into the host system.
pub struct NodeProc {
    pub id: u64,
    child: Child,
    pub log: PathBuf,
    /// what this process is, for a wait's panic message.
    what: String,
    feed: Arc<OutputFeed>,
}

impl NodeProc {
    /// spawn `cmd` as process `id`, its output draining into `log` and the feed.
    pub fn spawn(id: u64, log: PathBuf, mut cmd: Command, what: &str) -> Self {
        let (reader, writer) = std::io::pipe().expect("pipe for the process output");
        let stderr = writer.try_clone().expect("clone the pipe's write end");
        let child = cmd
            .stdout(writer)
            .stderr(stderr)
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {what}: {e}"));
        // the Command holds the write ends until it is dropped, and the reader's
        // EOF is the exit event — release ours before anyone waits on it.
        drop(cmd);
        let file =
            std::fs::File::create(&log).unwrap_or_else(|e| panic!("create {}: {e}", log.display()));
        let feed = Arc::new(OutputFeed::default());
        std::thread::Builder::new()
            .name(format!("output-{what}-{id}"))
            .spawn({
                let feed = Arc::clone(&feed);
                move || drain_output(reader, file, &feed)
            })
            .expect("spawn the output reader");
        Self {
            id,
            child,
            log,
            what: what.to_string(),
            feed,
        }
    }

    /// the rest of the first line containing `marker`.
    fn wait_marker(&self, marker: &str, deadline: Instant) -> Result<String, Unanswered> {
        self.feed
            .wait(deadline, |unseen| find_marker(unseen, marker))
    }

    /// block until ONE line carries EVERY needle, and answer with that line.
    ///
    /// Matches against [`strip_ansi`]ed text: a `key=value` needle can never
    /// match the raw bytes, because the node's stderr colours every field name
    /// and its `=` separately.
    pub fn expect_line(&self, needles: &[&str], timeout: Duration) -> String {
        self.expect_line_where(
            &format!("one line carrying all of {needles:?}"),
            timeout,
            |line| needles.iter().all(|needle| line.contains(needle)),
        )
    }

    /// block until the `nth` (1-based) line carrying every needle lands.
    ///
    /// [`Self::expect_line`] rescans the whole run each call, so it answers
    /// with the FIRST match every time — right for a marker that happens once,
    /// useless for one a supervised process prints per restart. The count
    /// lives in the probe, which is offered every line exactly once.
    pub fn expect_line_nth(&self, needles: &[&str], nth: usize, timeout: Duration) -> String {
        let mut seen = 0usize;
        self.feed
            .wait(Instant::now() + timeout, |unseen| {
                strip_ansi(unseen).lines().find_map(|line| {
                    let matches = needles.iter().all(|needle| line.contains(needle));
                    if !matches {
                        return None;
                    }
                    seen += 1;
                    (seen == nth).then(|| line.to_string())
                })
            })
            .unwrap_or_else(|why| {
                panic!(
                    "{} {} without printing {needles:?} {nth} time(s) (saw {seen});\n{}",
                    self.what,
                    why.verb(),
                    self.tail(60)
                )
            })
    }

    /// block until an ANSI-stripped line satisfies `accept`, and answer with
    /// that line; `wanted` names it in the panic when the process never does.
    pub fn expect_line_where(
        &self,
        wanted: &str,
        timeout: Duration,
        accept: impl Fn(&str) -> bool,
    ) -> String {
        self.feed
            .wait(Instant::now() + timeout, |unseen| {
                strip_ansi(unseen)
                    .lines()
                    .find(|line| accept(line))
                    .map(str::to_string)
            })
            .unwrap_or_else(|why| {
                panic!(
                    "{} {} without printing {wanted};\n{}",
                    self.what,
                    why.verb(),
                    self.tail(60)
                )
            })
    }

    /// which of `markers` a line carried first.
    fn wait_any_marker(&self, markers: &[&str], deadline: Instant) -> Result<(), Unanswered> {
        self.feed.wait(deadline, |unseen| {
            markers
                .iter()
                .any(|marker| find_marker(unseen, marker).is_some())
                .then_some(())
        })
    }

    /// block until `marker` has appeared at least `count` times in total.
    ///
    /// every offered slice is counted exactly once (the feed only ever
    /// offers a line to a probe once — see `OutputFeed::wait`), so this
    /// tallies markers across repeated wakes rather than re-scanning from
    /// the top each time.
    fn wait_marker_count(
        &self,
        marker: &str,
        count: usize,
        deadline: Instant,
    ) -> Result<usize, Unanswered> {
        let mut seen = 0usize;
        self.feed.wait(deadline, |unseen| {
            seen += unseen.lines().filter(|line| line.contains(marker)).count();
            (seen >= count).then_some(seen)
        })
    }

    /// block until the process closes its output — it exited — then reap it.
    fn wait_exit(&mut self, deadline: Instant) -> Result<std::process::ExitStatus, Unanswered> {
        self.feed.wait_closed(deadline)?;
        Ok(self.child.wait().expect("reap the exited process"))
    }

    /// everything the process has written so far.
    fn text(&self) -> String {
        self.feed.text()
    }

    /// how many lines printed so far contain `marker`.
    fn marker_count(&self, marker: &str) -> usize {
        self.text()
            .lines()
            .filter(|line| line.contains(marker))
            .count()
    }

    /// the last `lines` lines the process wrote.
    fn tail(&self, lines: usize) -> String {
        log_tail(&self.text(), lines)
    }
}

impl NodeProc {
    /// SIGTERM this process and wait for it to go, SIGKILLing only if it
    /// outlives `budget`.
    ///
    /// `Drop` SIGKILLs, which is right for a node (a crash is a case under
    /// test) and wrong for a SUPERVISOR: a killed supervisor leaves the node
    /// it started running, orphaned, over a tempdir the test is about to
    /// remove. A stop signal is also what `systemctl stop` sends, so this is
    /// the shutdown the supervisor is built for.
    pub fn terminate(&mut self, budget: Duration) {
        // SAFETY: our own child, not yet reaped — `Drop` is the only other
        // reaper and it runs after this.
        unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) };
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_)) | Err(_)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for NodeProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// one process's output: everything it wrote so far, and whether it still can.
#[derive(Default)]
struct Output {
    text: String,
    closed: bool,
}

/// the seam every wait in this harness rides: a line landing, or the output
/// closing, wakes whoever is waiting.
#[derive(Default)]
struct OutputFeed {
    output: Mutex<Output>,
    changed: Condvar,
}

/// why a wait on a feed came back without its answer.
#[derive(Clone, Copy, Debug)]
enum Unanswered {
    /// the process closed its output: it exited.
    Exited,
    /// the deadline passed with the output still open.
    TimedOut,
}

impl Unanswered {
    fn verb(self) -> &'static str {
        match self {
            Unanswered::Exited => "exited",
            Unanswered::TimedOut => "timed out",
        }
    }
}

impl OutputFeed {
    fn append(&self, line: &str) {
        let mut output = self.output.lock().unwrap_or_else(|e| e.into_inner());
        output.text.push_str(line);
        self.changed.notify_all();
    }

    fn close(&self) {
        let mut output = self.output.lock().unwrap_or_else(|e| e.into_inner());
        output.closed = true;
        self.changed.notify_all();
    }

    fn text(&self) -> String {
        self.output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .text
            .clone()
    }

    /// block until `probe` answers on the lines it has not yet been offered,
    /// the output closes, or `deadline` passes. every line is offered to the
    /// probe exactly once (appends are whole lines), so a marker scan stays
    /// linear over the whole run no matter how often it wakes.
    fn wait<T>(
        &self,
        deadline: Instant,
        mut probe: impl FnMut(&str) -> Option<T>,
    ) -> Result<T, Unanswered> {
        let mut output = self.output.lock().unwrap_or_else(|e| e.into_inner());
        let mut offered = 0;
        loop {
            let answer = probe(&output.text[offered..]);
            offered = output.text.len();
            if let Some(answer) = answer {
                return Ok(answer);
            }
            if output.closed {
                return Err(Unanswered::Exited);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(Unanswered::TimedOut);
            }
            (output, _) = self
                .changed
                .wait_timeout(output, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    /// block until the output closes, or `deadline` passes.
    fn wait_closed(&self, deadline: Instant) -> Result<(), Unanswered> {
        match self.wait(deadline, |_| None::<std::convert::Infallible>) {
            Ok(never) => match never {},
            Err(Unanswered::Exited) => Ok(()),
            Err(Unanswered::TimedOut) => Err(Unanswered::TimedOut),
        }
    }
}

/// the reader thread's whole life: every line the process writes lands in the
/// log file and the feed, and the pipe's EOF closes the feed.
fn drain_output(reader: std::io::PipeReader, mut file: std::fs::File, feed: &OutputFeed) {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        let Ok(read) = reader.read_until(b'\n', &mut line) else {
            break;
        };
        if read == 0 {
            break;
        }
        let _ = file.write_all(&line);
        feed.append(&String::from_utf8_lossy(&line));
    }
    feed.close();
}

/// one e2e cluster: the shared config (mesh membership, ports, namespace,
/// storage tempdir) plus whichever node processes are currently running.
pub struct Cluster {
    pub namespace: String,
    pub peer_ids: Vec<u64>,
    pub validator_ids: Vec<u64>,
    /// p2p listen port per `peer_ids` position.
    pub p2p_ports: Vec<u16>,
    /// direct invite intro port per `peer_ids` position. Kept explicit because
    /// the product default (`wireguard_listen + 1`) can be another node's
    /// WireGuard port when a test cluster asks the OS for a batch of ports.
    pub(crate) invite_ports: Vec<u16>,
    /// rpc port per `peer_ids` position (every config gets one; `--sync-only`
    /// simply never binds it).
    pub rpc_ports: Vec<u16>,
    /// http/ws app-surface port per `peer_ids` position (the noded wire
    /// contract served by the validator itself; off under `--sync-only`).
    pub http_ports: Vec<u16>,
    /// per-node `advertised` override (test-only), index-aligned with
    /// `peer_ids`. `Some(addr)` emits an `advertised = "<addr>"` line right
    /// after `listen` in the generated config (e.g. a sentry/forwarder in
    /// front of the node); `None` emits nothing — byte-for-byte the plain node.
    pub advertised: Vec<Option<String>>,
    /// override for the bootstrapper address every non-founder node dials.
    /// `None` -> `127.0.0.1:p2p_ports[0]` (identical to today); `Some(addr)`
    /// points bootstrap at a forwarder in front of node 0.
    pub bootstrap_addr_override: Option<String>,
    /// When true every config gets `wireguard_listen` on the node's distinct
    /// UDP port — the reachability plane runs the real, unprivileged
    /// userspace transport (the node's only backend).
    pub wireguard: bool,
    /// extra `node.toml` lines appended verbatim to EVERY node's generated
    /// config (`spawn` regenerates the file, so a hand-edit after the fact
    /// would not survive a respawn). set before the first spawn; empty by
    /// default so existing tests are byte-for-byte unchanged.
    pub extra_toml: Vec<String>,
    /// grant every node the `compute` service, the harness twin of `ducktape
    /// service enable compute`. The compute plane needs BOTH a `[sandbox]`
    /// table (HOW runs are isolated — set via `extra_toml`) and this grant
    /// (WHETHER this node runs any), so a test that expects provider
    /// discovery, an oracle pool or a capability announce must set it.
    ///
    /// The tags are what the grant CONSENTED to announce; the node announces
    /// those intersected with what it actually discovers. `Some(vec![])` runs
    /// a daemon without announcing capability standing; that daemon cannot
    /// claim capability-gated work. `None` = no grant at all.
    pub compute_grant: Option<Vec<String>>,
    /// extra environment variables for node `idx`'s process, index-aligned
    /// with `peer_ids` (a spec's `detect.env` override, a runs root, a log
    /// filter). empty per node by default; set before spawn — a respawn
    /// re-applies.
    pub env: Vec<Vec<(String, String)>>,
    /// node `idx`'s compute-plane files, index-aligned with `peer_ids`:
    /// `Some` stages them under the node's WORKSPACE on every spawn, the way
    /// `services.toml` is — a node booting a `[sandbox]` table needs at least
    /// the guest images there; `None` stages nothing. set before spawn.
    pub sandbox: Vec<Option<SandboxStage>>,
    /// declared BEFORE `dir` so drop order kills + reaps every child first —
    /// removing the tempdir under live processes races their qmdb/journal
    /// writes and silently leaks the subtree.
    nodes: Vec<Option<NodeProc>>,
    /// each node's COMPUTE DAEMON, when `compute_grant` gave it one.
    ///
    /// The node process runs no provider work any more — `ducktape service run
    /// compute` is the compute plane — so a cluster that expects a run to
    /// EXECUTE must run the daemon beside its node, exactly as an operator
    /// does. Declared beside `nodes` so drop order reaps them before the
    /// tempdir goes.
    daemons: Vec<Option<NodeProc>>,
    /// the kinds [`Cluster::spawn_service`] granted per node, so a rewrite of
    /// `services.toml` (every `spawn`) keeps them.
    service_kinds: Vec<Vec<String>>,
    /// each node's EXPLICITLY started service daemons
    /// ([`Cluster::spawn_service`]), one per kind. Same drop-order reasoning as
    /// `daemons`.
    services: Vec<Vec<ServiceProc>>,
    dir: tempfile::TempDir,
}

impl Drop for Cluster {
    /// Kill every daemon this cluster started.
    ///
    /// There is nothing to reap after them any more. A compute daemon used to
    /// leave a `podman system service` child behind — started `--time=0` so it
    /// never idle-exited, surviving the SIGKILL in [`NodeProc::drop`], holding
    /// ~45 MB rooted in a tempdir about to vanish, and reaped only when a
    /// SUCCESSOR booted on the same root, which a torn-down cluster never gets
    /// (102 of them, ~4.5 GB, were once swept by hand). A run's VMM is a child
    /// of its daemon, armed with a parent-death signal, so the SIGKILL that
    /// ends the daemon ends its guests too.
    fn drop(&mut self) {
        for daemon in &mut self.daemons {
            *daemon = None; // NodeProc::drop kills + waits
        }
        for procs in &mut self.services {
            procs.clear(); // same: kill + wait
        }
    }
}

/// One `ducktape service run <kind>` daemon attached to a node.
struct ServiceProc {
    kind: String,
    proc: NodeProc,
}

/// a two-person network-shape ceremony: real `init`/`invite`/`join` verbs,
/// key files, network.toml descriptors, and node.toml configs.
pub struct NetworkShapeCluster {
    pub p2p_ports: Vec<u16>,
    pub rpc_ports: Vec<u16>,
    pub http_ports: Vec<u16>,
    pub founder_dir: PathBuf,
    pub friend_dir: PathBuf,
    /// extra process env per node (set before `spawn`) — the same knob
    /// [`Cluster::env`] exposes, e.g. capability spec-dir overrides.
    pub env: Vec<Vec<(String, String)>>,
    nodes: Vec<Option<NodeProc>>,
    dir: tempfile::TempDir,
}

impl NetworkShapeCluster {
    /// freeze (`SIGSTOP`) or thaw (`SIGCONT`) a running node — the closest a
    /// test gets to a laptop sleeping mid-run: the process vanishes from the
    /// scheduler while its kernel keeps its sockets ESTABLISHED, exactly the
    /// silent half-open shape a slept machine leaves its peers holding.
    pub fn signal(&self, idx: usize, signal: &str) {
        let node = self.nodes[idx].as_ref().expect("node not running");
        let pid = node.child.id();
        let status = Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(pid.to_string())
            .status()
            .expect("run kill");
        assert!(status.success(), "kill -{signal} {pid} failed");
    }

    pub fn new() -> Self {
        let dir = e2e_tempdir("shape");
        let ports = alloc_ports(6);
        let (p2p_ports, rest) = ports.split_at(2);
        let (rpc_ports, http_ports) = rest.split_at(2);
        Self {
            p2p_ports: p2p_ports.to_vec(),
            rpc_ports: rpc_ports.to_vec(),
            http_ports: http_ports.to_vec(),
            founder_dir: dir.path().join("founder"),
            friend_dir: dir.path().join("friend"),
            env: vec![Vec::new(), Vec::new()],
            nodes: vec![None, None],
            dir,
        }
    }

    /// this shape's per-node workspace — the directory holding `node.toml`, and
    /// the credential the node mints beside it.
    pub fn workspace(&self, idx: usize) -> PathBuf {
        match idx {
            0 => self.founder_dir.clone(),
            _ => self.friend_dir.clone(),
        }
    }

    /// Seed an automation-owned ref through generic node-origin transport.
    pub fn seed_forge(&self, idx: usize, source: &Path, repo: &str, branch: &str) {
        seed_forge_at(
            self.http_ports[idx],
            &self.workspace(idx),
            source,
            repo,
            branch,
        );
    }

    pub fn clone_forge(&self, idx: usize, repo: &str, destination: &Path) {
        clone_forge_at(
            self.http_ports[idx],
            &self.workspace(idx),
            repo,
            destination,
        );
    }

    /// one request against node `idx`'s app surface, carrying that node's
    /// operator credential — the shape-cluster twin of [`Cluster::http`].
    pub fn http(
        &self,
        idx: usize,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> (u16, serde_json::Value) {
        let token = noded::admin::read_operator_token(&self.workspace(idx))
            .expect("the node minted an operator credential");
        let bytes = body
            .map(|b| serde_json::to_vec(b).expect("request body serializes"))
            .unwrap_or_default();
        let (status, raw) = nettest::try_http_bytes_with(
            self.http_ports[idx],
            method,
            path,
            "application/json",
            &[(noded::admin::ADMIN_TOKEN_HEADER, &token)],
            &bytes,
        )
        .expect("app-surface request");
        (
            status,
            serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null),
        )
    }

    pub fn init_founder(&self, name: &str) -> String {
        // the join protocol refuses to mint an invite from a member with no reachability
        // plane, and this harness is deliberately coordinator-free — so every
        // founder carries a distinct-port WireGuard listen.
        let wg_listen = format!("127.0.0.1:{}", alloc_ports(1)[0]);
        let out = Command::new(env!("CARGO_BIN_EXE_ducktape"))
            .arg("node")
            .args([
                "init",
                "--name",
                name,
                // hermetic: no ambient coordinator. the default would dial the
                // LIVE public coordinator from inside the test AND flip both
                // nodes into the overlay shape (wireguard on a shared port +
                // ULA-advertised mesh) — same-host nodes then fight over one
                // interface and the underlay never assembles. the coordinated
                // shape has its own e2e (coordinated_invite_cli).
                "--primary-coordinator",
                "none",
                // the genesis wasm set lives on disk, not in the binary: found
                // from the set the build staged beside the binary, named
                // explicitly so an operator's $DUCKTAPE_MODULES_DIR cannot
                // redirect a test.
                "--modules",
                founding_set(),
                "--dir",
                self.founder_dir.to_str().expect("utf-8 founder dir"),
                "--listen",
                &format!("127.0.0.1:{}", self.p2p_ports[0]),
                "--advertised",
                &format!("127.0.0.1:{}", self.p2p_ports[0]),
                "--http",
                &format!("127.0.0.1:{}", self.http_ports[0]),
                "--rpc",
                &format!("127.0.0.1:{}", self.rpc_ports[0]),
                "--block-time-ms",
                &TEST_BLOCK_TIME_MS.to_string(),
            ])
            .args(["--wireguard-listen", &wg_listen])
            .output()
            .expect("run init");
        assert!(
            out.status.success(),
            "init failed:\n{}",
            command_output(&out)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// mint (or reuse) the friend workspace's identity via the `node key` verb
    /// and return its pubkey hex — the JOIN CODE the invite locks to.
    /// `join_friend` reuses this pre-generated identity.
    pub fn keygen_friend(&self, _idx: usize) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_ducktape"))
            .arg("node")
            .args(["key", "--dir"])
            .arg(&self.friend_dir)
            .output()
            .expect("run node key");
        assert!(
            out.status.success(),
            "keygen failed:\n{}",
            command_output(&out)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// mint a bearer invite (the join protocol: single-use, sealed to whoever redeems it
    /// at first contact). Still pre-generates the friend identity so
    /// `join_friend` reuses one stable key across the ceremony.
    pub fn invite(&self) -> String {
        self.keygen_friend(1);
        let cfg = self.config_file(0);
        let out = Command::new(env!("CARGO_BIN_EXE_ducktape"))
            .arg("node")
            .args(["invite", "--config"])
            .arg(cfg)
            .output()
            .expect("run invite");
        assert!(
            out.status.success(),
            "invite failed:\n{}",
            command_output(&out)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// a MANUAL-flow join: every invite is tokened now (mint is the
    /// admission), so the manual path is made by joining normally and then
    /// dropping the stored credential — the node parks with no announce and
    /// admission stays a member verb, which is exactly what the staged
    /// admission tests exercise.
    pub fn join_friend_manual(&self, invite: &str) -> String {
        let key = self.join_friend(invite);
        for stale in ["invite.token", "invite-wireguard.toml"] {
            let _ = std::fs::remove_file(self.friend_dir.join(stale));
        }
        key
    }

    /// the founder's verified join-request queue, parsed from the
    /// `join requests` verb's JSON stdout.
    pub fn join_requests(&self) -> serde_json::Value {
        let cfg = self.config_file(0);
        let out = Command::new(env!("CARGO_BIN_EXE_ducktape"))
            .arg("node")
            .args(["join", "requests", "--config"])
            .arg(cfg)
            .output()
            .expect("run node join requests");
        assert!(
            out.status.success(),
            "join requests failed:\n{}",
            command_output(&out)
        );
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
            .expect("join requests prints json")
    }

    /// run the `join` verb against the friend workspace WITHOUT asserting
    /// success — the caller inspects the outcome (a targeted invite refuses a
    /// mismatched local identity at the CLI, before any node spawns).
    pub fn try_join_friend(&self, invite: &str) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_ducktape"))
            .arg("node")
            .args([
                "join",
                invite,
                "--dir",
                self.friend_dir.to_str().expect("utf-8 friend dir"),
                "--listen",
                &format!("127.0.0.1:{}", self.p2p_ports[1]),
                "--advertised",
                &format!("127.0.0.1:{}", self.p2p_ports[1]),
                "--http",
                &format!("127.0.0.1:{}", self.http_ports[1]),
                "--rpc",
                &format!("127.0.0.1:{}", self.rpc_ports[1]),
                "--wireguard-listen",
                &format!("127.0.0.1:{}", alloc_ports(1)[0]),
                // hermetic: without this the joined node registers with the
                // LIVE public coordinator from inside the test.
                "--primary-coordinator",
                "none",
            ])
            .output()
            .expect("run join")
    }

    pub fn join_friend(&self, invite: &str) -> String {
        let out = self.try_join_friend(invite);
        assert!(
            out.status.success(),
            "join failed:\n{}",
            command_output(&out)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    pub fn config_file(&self, idx: usize) -> PathBuf {
        match idx {
            0 => self.founder_dir.join("node.toml"),
            1 => self.friend_dir.join("node.toml"),
            _ => panic!("unknown network-shape node idx {idx}"),
        }
    }

    pub fn spawn(&mut self, idx: usize) {
        let cfg = self.config_file(idx);
        let label = match idx {
            0 => "founder",
            1 => "friend",
            _ => panic!("unknown network-shape node idx {idx}"),
        };
        let log = self.dir.path().join(format!("{label}.log"));
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ducktape"));
        cmd.arg("node")
            .arg("run")
            .arg("--config")
            .arg(&cfg)
            .envs(self.env[idx].iter().map(|(k, v)| (k.clone(), v.clone())));
        self.nodes[idx] = Some(NodeProc::spawn(idx as u64, log, cmd, label));
    }

    /// keep node `idx`'s `<kind>` hello alive — a service daemon's ENTIRE
    /// contribution to the capability-announce lane, without the daemon.
    ///
    /// The daemon PROCESS is deliberately not spawned, and that is a SCOPING
    /// choice, not a limitation of the host: a daemon's entire contribution to
    /// the announce lane is this POST, so booting a sandbox to prove capability
    /// announcement buys no extra signal and couples this lane to `/dev/kvm`
    /// and the guest artifacts. What a real daemon would additionally prove —
    /// that a REAL hello carries the shape this lane expects — is a
    /// daemon-fixture concern, and it belongs in the dispatch e2e (#826), which
    /// owns the `[sandbox]` fixture that lane actually needs.
    ///
    /// The FIRST hello is synchronous and asserted — that IS the readiness
    /// event; the refresh then rides a heartbeat thread like the daemon's.
    ///
    /// ponytail: two known divergences from the real daemon, both latent while
    /// one serialized test uses this.
    /// 1. The real daemon treats a failed FIRST hello as FATAL; this retries
    ///    for 60s, so a permanent refusal (a build-identity skew) burns the
    ///    whole budget and surfaces as a timeout instead of failing instantly.
    ///    Fix by splitting connect-refused (retry, the node is still binding)
    ///    from an answered non-200 (fail now) when a second caller appears.
    /// 2. The heartbeat thread has no exit condition and no liveness check, and
    ///    `common` is shared across test binaries over recycled port ranges —
    ///    so a future SECOND caller could inject a stale `compute` hello into
    ///    an unrelated cluster that inherited the port. Give it a stop flag
    ///    cleared by `kill` at that point.
    pub fn signal_service(&mut self, idx: usize, kind: &str, capabilities: &[&str]) {
        let hello = noded::services::Hello {
            kind: kind.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            build: noded::services::build_identity()
                .expect("tests run from a git checkout")
                .into(),
            capabilities: capabilities.iter().map(|tag| tag.to_string()).collect(),
            scopes: Vec::new(),
            needs: Vec::new(),
        };
        let body = serde_json::to_value(&hello).expect("a hello serializes");
        let port = self.http_ports[idx];
        // a hello lands once the node has published its mesh identity — the
        // fact the real daemon waits on before its own first POST.
        self.wait_marker(idx, "mesh identity published", Duration::from_secs(60));
        let reply = nettest::try_http_json(port, "POST", "/v1/services/hello", Some(&body));
        assert!(
            matches!(reply, Ok((200, _))),
            "node idx {idx} refused a {kind:?} hello ({reply:?});\n{}",
            self.all_log_tails(60),
        );
        // detached like the daemon's own heartbeat thread: it outlives this
        // call, ignores a post to a node that has gone away, and dies with the
        // test process.
        std::thread::Builder::new()
            .name(format!("hello-{kind}-{idx}"))
            .spawn(move || {
                loop {
                    std::thread::sleep(noded::services::HELLO_TTL / 3);
                    let _ = nettest::try_http_json(port, "POST", "/v1/services/hello", Some(&body));
                }
            })
            .expect("spawn the hello heartbeat");
    }

    /// kill node `idx`'s process (reaped by NodeProc's drop).
    pub fn kill(&mut self, idx: usize) {
        self.nodes[idx] = None;
    }

    /// node `idx`'s captured stdout+stderr — for a failing test to preserve
    /// evidence before the cluster tempdir (and the logs in it) is dropped.
    pub fn log_path(&self, idx: usize) -> PathBuf {
        self.nodes[idx]
            .as_ref()
            .expect("node not running")
            .log
            .clone()
    }

    /// one json-lines rpc against node `idx` — the NetworkShapeCluster
    /// mirror of [`Cluster::rpc`] (same wire, same ports array).
    pub fn rpc(&self, idx: usize, req: serde_json::Value) -> serde_json::Value {
        let port = self.rpc_ports[idx];
        let mut line = serde_json::to_string(&req).expect("rpc request serializes");
        line.push('\n');
        let retryable = req["cmd"] == "query";
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            // the listener is up before the pump — connecting retries for
            // every cmd (nothing has been sent yet, so retrying is safe).
            // the node logs its rpc listener the moment the port is bound, and
            // nothing answers before that — the line is the connect event.
            self.wait_marker(idx, "rpc listening on", Duration::from_secs(30));
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap_or_else(|e| {
                panic!(
                    "rpc connect to node idx {idx} (port {port}) failed: {e};\n{}",
                    self.all_log_tails(40)
                )
            });
            stream
                .set_read_timeout(Some(Duration::from_secs(if retryable { 15 } else { 60 })))
                .expect("rpc read timeout");
            stream.write_all(line.as_bytes()).expect("rpc write");
            let mut reply = String::new();
            match BufReader::new(stream).read_line(&mut reply) {
                Ok(n) if n > 0 => {
                    return serde_json::from_str(reply.trim()).expect("rpc reply is json");
                }
                res => {
                    // a query is idempotent: reconnect and resend across the
                    // windows where the listener answers connects but the node
                    // cannot reply yet (a promotion reboot, an epoch-cutover
                    // engine restart). a submit is NEVER resent — a duplicate
                    // could double-apply — it gets one long read window above.
                    let why = match res {
                        Ok(_) => "connection closed before a reply line".to_string(),
                        Err(e) => e.to_string(),
                    };
                    if !retryable || Instant::now() >= deadline {
                        panic!(
                            "rpc to node idx {idx} (port {port}) failed: {why}; request: {req};\n{}",
                            self.all_log_tails(60)
                        );
                    }
                    std::thread::sleep(Duration::from_millis(300));
                }
            }
        }
    }

    pub fn query(&self, idx: usize, target: &str, req: &[u8]) -> Option<Vec<u8>> {
        let reply = self.rpc(
            idx,
            serde_json::json!({
                "cmd": "query",
                "target": target,
                "req_hex": hex(req),
            }),
        );
        if reply["ok"] != true {
            return None;
        }
        Some(unhex(
            reply["reply_hex"]
                .as_str()
                .expect("query reply carries hex"),
        ))
    }

    pub fn submit(&self, idx: usize, target: &str, payload: &[u8]) {
        let reply = self.rpc(
            idx,
            serde_json::json!({
                "cmd": "submit",
                "target": target,
                "payload_hex": hex(payload),
            }),
        );
        assert_eq!(
            reply["ok"], true,
            "submit to {target} via node idx {idx} rejected: {reply}"
        );
    }

    pub fn status(&self, idx: usize) -> serde_json::Value {
        let reply = self.rpc(idx, serde_json::json!({ "cmd": "status" }));
        assert_eq!(
            reply["ok"], true,
            "status via node idx {idx} failed: {reply}"
        );
        reply["status"].clone()
    }

    /// drive a membership ceremony verb (`member promote`, `member remove`,
    /// `resident accept`, `resident remove`) against node `idx`'s running rpc,
    /// from that node's config.
    /// `verb` is the space-separated two-token spelling; it is split into argv.
    pub fn run_membership_verb_as(
        &self,
        idx: usize,
        verb: &str,
        pubkey_hex: &str,
    ) -> (bool, String) {
        let cfg = self.config_file(idx);
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ducktape"));
        cmd.arg("node");
        for token in verb.split(' ') {
            cmd.arg(token);
        }
        let out = cmd
            .args([pubkey_hex, "--config"])
            .arg(cfg)
            .output()
            .unwrap_or_else(|e| panic!("run {verb}: {e}"));
        (out.status.success(), command_output(&out))
    }

    /// drive a membership ceremony from the founder's running node.
    pub fn run_membership_verb(&self, verb: &str, pubkey_hex: &str) -> (bool, String) {
        self.run_membership_verb_as(0, verb, pubkey_hex)
    }

    /// drive the DIRECT admission ceremony (`member promote` — the pre-staged
    /// `resident accept` semantics) from node 0's config.
    pub fn run_promote(&self, pubkey_hex: &str) -> (bool, String) {
        self.run_membership_verb("member promote", pubkey_hex)
    }

    pub fn wait_marker(&self, idx: usize, marker: &str, timeout: Duration) -> String {
        let node = self.nodes[idx].as_ref().expect("node is running");
        node.wait_marker(marker, Instant::now() + timeout)
            .unwrap_or_else(|why| {
                panic!(
                    "network-shape node idx {idx} {} without printing {marker:?};\n{}",
                    why.verb(),
                    self.all_log_tails(60),
                )
            })
    }

    /// how many times node `idx` has printed `marker` so far.
    pub fn marker_count(&self, idx: usize, marker: &str) -> usize {
        let node = self.nodes[idx].as_ref().expect("node is running");
        node.marker_count(marker)
    }

    /// wait until node `idx` has printed `marker` at least `count` times.
    pub fn wait_marker_count(
        &self,
        idx: usize,
        marker: &str,
        count: usize,
        timeout: Duration,
    ) -> usize {
        let node = self.nodes[idx].as_ref().expect("node is running");
        node.wait_marker_count(marker, count, Instant::now() + timeout)
            .unwrap_or_else(|why| {
                panic!(
                    "network-shape node idx {idx} {} before printing {marker:?} {count} times;\n{}",
                    why.verb(),
                    self.all_log_tails(60),
                )
            })
    }

    /// wait until node `idx` has COMMITTED STANDING as a member. the join protocol has
    /// two legitimate admission paths and which one lands first is a race:
    /// direct first contact prints "standing is committed" (replica/wiring),
    /// the announce-redeem park path prints "resident: standing granted".
    /// Waiting on either is the semantic event the resident tests gate on.
    pub fn wait_admitted(&self, idx: usize, timeout: Duration) {
        let markers = ["standing is committed", "resident: standing granted"];
        let node = self.nodes[idx].as_ref().expect("node is running");
        if let Err(why) = node.wait_any_marker(&markers, Instant::now() + timeout) {
            panic!(
                "network-shape node idx {idx} {} without printing any of \
                 {markers:?};\n{}",
                why.verb(),
                self.all_log_tails(60),
            );
        }
    }

    /// wait for node `idx` to exit ON ITS OWN (e.g. the fail-loud FATAL path)
    /// and reap it — the [`Cluster::wait_exit`] mirror.
    pub fn wait_exit(&mut self, idx: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let exited = self.nodes[idx]
            .as_mut()
            .expect("node is running")
            .wait_exit(deadline);
        assert!(
            exited.is_ok(),
            "network-shape node idx {idx} did not exit within {timeout:?};\n{}",
            self.all_log_tails(40)
        );
        self.nodes[idx] = None;
    }

    /// Wait until `probe` answers, re-evaluating it on node `idx`'s block wake
    /// — the shape-cluster twin of [`Cluster::await_committed`].
    pub fn await_committed<T>(
        &self,
        idx: usize,
        what: &str,
        timeout: Duration,
        probe: impl FnMut() -> Option<T>,
    ) -> T {
        await_committed_on(self.http_ports[idx], idx, what, timeout, probe, || {
            self.all_log_tails(60)
        })
    }

    fn all_log_tails(&self, lines: usize) -> String {
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(idx, n)| {
                n.as_ref().map(|n| {
                    format!(
                        "--- network-shape node idx {idx} log tail ---\n{}",
                        n.tail(lines)
                    )
                })
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// the dev-shape node.toml of a LONE node, rooted at `dir` — the same literal
/// [`Cluster::config_path`] writes, with a free `rpc_listen` and NOTHING behind
/// it. What a pure-CLI test points a workspace verb at to exercise the
/// node-is-not-running path without spawning a node.
pub fn minimal_dev_shape_toml(dir: &std::path::Path) -> String {
    Cluster::new(&[1], &[1]).config_toml(0, dir)
}

impl Cluster {
    /// lay out a cluster: `peer_ids` is the full authorized mesh (index 0 is
    /// the bootstrapper), `validator_ids` the consensus subset.
    pub fn new(peer_ids: &[u64], validator_ids: &[u64]) -> Self {
        let seq = CLUSTER_SEQ.fetch_add(1, Ordering::Relaxed);
        let namespace = format!("ducktape-e2e-{}-{seq}", std::process::id());
        let dir = e2e_tempdir("cluster");
        let ports = alloc_ports(peer_ids.len() * 4);
        let (p2p_ports, rest) = ports.split_at(peer_ids.len());
        let (rpc_ports, rest) = rest.split_at(peer_ids.len());
        let (http_ports, invite_ports) = rest.split_at(peer_ids.len());
        Self {
            namespace,
            peer_ids: peer_ids.to_vec(),
            validator_ids: validator_ids.to_vec(),
            p2p_ports: p2p_ports.to_vec(),
            invite_ports: invite_ports.to_vec(),
            rpc_ports: rpc_ports.to_vec(),
            http_ports: http_ports.to_vec(),
            advertised: peer_ids.iter().map(|_| None).collect(),
            bootstrap_addr_override: None,
            wireguard: false,
            extra_toml: Vec::new(),
            compute_grant: None,
            daemons: peer_ids.iter().map(|_| None).collect(),
            service_kinds: peer_ids.iter().map(|_| Vec::new()).collect(),
            services: peer_ids.iter().map(|_| Vec::new()).collect(),
            env: peer_ids.iter().map(|_| Vec::new()).collect(),
            sandbox: peer_ids.iter().map(|_| None).collect(),
            dir,
            nodes: peer_ids.iter().map(|_| None).collect(),
        }
    }

    /// the deterministic dev identity for a seed — what the node itself derives.
    pub fn identity(seed: u64) -> Vec<u8> {
        ed25519::PrivateKey::from_seed(seed)
            .public_key()
            .as_ref()
            .to_vec()
    }

    pub(crate) fn config_path(&self, idx: usize) -> PathBuf {
        let id = self.peer_ids[idx];
        let path = self.dir.path().join(format!("node{id}.toml"));
        std::fs::write(&path, self.config_toml(idx, self.dir.path())).expect("write node config");
        self.write_service_grants(idx);
        self.stage_sandbox(idx);
        path
    }

    /// stage node `idx`'s compute-plane files under its workspace
    /// ([`Cluster::sandbox`]): the box's guest build linked in as `guest/`,
    /// and the node's own capability specs and executors — each an EMPTY
    /// directory when the stage names none, so the node discovers nothing
    /// whatever this box has installed elsewhere.
    fn stage_sandbox(&self, idx: usize) {
        let Some(stage) = &self.sandbox[idx] else {
            return;
        };
        let workspace = self.workspace(idx);
        std::fs::create_dir_all(&workspace).expect("create workspace dir");
        stage_slot(&workspace_config::guest_dir(&workspace), Some(&guest_dir()));
        stage_slot(
            &workspace_config::capability_dir(&workspace),
            stage.capabilities.as_deref(),
        );
        stage_slot(
            &workspace_config::executor_dir(&workspace),
            stage.executors.as_deref(),
        );
    }

    /// the node.toml body [`Cluster::config_path`] writes, rooted at `root`
    /// (the cluster's own tempdir for a spawned node). Pure, so a pure-CLI
    /// test can borrow the same dev shape without a cluster on disk.
    fn config_toml(&self, idx: usize, root: &std::path::Path) -> String {
        let id = self.peer_ids[idx];
        let mut cfg = String::new();
        cfg.push_str(&format!("id = {id}\n"));
        cfg.push_str(&format!("listen = \"127.0.0.1:{}\"\n", self.p2p_ports[idx]));
        if let Some(addr) = &self.advertised[idx] {
            cfg.push_str(&format!("advertised = {addr:?}\n"));
        }
        cfg.push_str(&format!("namespace = {:?}\n", self.namespace));
        cfg.push_str(&format!("peer_seeds = {:?}\n", self.peer_ids));
        cfg.push_str(&format!("validator_seeds = {:?}\n", self.validator_ids));
        cfg.push_str(&format!("modules = {:?}\n", founding_set()));
        cfg.push_str(&self.peer_addrs_toml());
        cfg.push_str(&format!(
            "storage_dir = {:?}\n",
            root.join(format!("storage-{id}")).to_str().unwrap()
        ));
        cfg.push_str(&format!(
            "rpc_listen = \"127.0.0.1:{}\"\n",
            self.rpc_ports[idx]
        ));
        cfg.push_str(&format!(
            "http_listen = \"127.0.0.1:{}\"\n",
            self.http_ports[idx]
        ));
        cfg.push_str(&format!("block_time_ms = {TEST_BLOCK_TIME_MS}\n"));
        if self.wireguard {
            cfg.push_str(&format!(
                "wireguard_listen = \"127.0.0.1:{}\"\n",
                self.p2p_ports[idx]
            ));
            cfg.push_str(&format!(
                "invite_listen = \"127.0.0.1:{}\"\n",
                self.invite_ports[idx]
            ));
        }
        for line in &self.extra_toml {
            cfg.push_str(line);
            cfg.push('\n');
        }
        cfg
    }

    /// Write (or remove) the workspace `services.toml` the node reads at boot:
    /// one `[[service]]` per granted kind, unique and kind-SORTED — the file's
    /// own validation rule, which an append would break the moment a second
    /// kind sorted before the first.
    ///
    /// Regenerated alongside node.toml on every spawn, so a respawn after
    /// `wipe_storage` still finds its grants. The dev shape's `storage_dir` IS
    /// its workspace, so the file lands beside the node's state.
    fn write_service_grants(&self, idx: usize) {
        let workspace = self.workspace(idx);
        std::fs::create_dir_all(&workspace).expect("create workspace dir");
        let path = workspace.join("services.toml");
        // A MAP, so `services.toml`'s own rule — kinds unique and sorted — holds
        // by construction rather than by everyone remembering it: two grants of
        // one kind cannot be expressed, and iteration is already sorted. A
        // duplicate `[[service]]` would make the node under test refuse the file
        // at load and fail to boot, with the reason nowhere near the cause.
        //
        // the compute grant announces tags; every explicitly spawned kind
        // announces none (airlock, the lender, discovers no capability at all).
        let mut granted: std::collections::BTreeMap<&str, &[String]> =
            std::collections::BTreeMap::new();
        if let Some(tags) = &self.compute_grant {
            granted.insert("compute", tags);
        }
        for kind in &self.service_kinds[idx] {
            granted.entry(kind.as_str()).or_insert(&[]);
        }
        if granted.is_empty() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        let mut file = String::new();
        for (position, (kind, tags)) in granted.iter().enumerate() {
            let announced = tags
                .iter()
                .map(|tag| format!("{tag:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            // any well-formed id/nonce pair does: the node asks WHETHER a grant
            // of that kind exists and WHAT it announces, never re-deriving the
            // id. The bytes come from the grant's POSITION, which is unique by
            // construction — a first-byte-of-kind derivation collided outright
            // ("agent" and "airlock" are both 0x61).
            let byte = format!("{:02x}", position + 1);
            file.push_str(&format!(
                "\n[[service]]\nkind = \"{kind}\"\ninstance = \"{}\"\n\
                 nonce = \"{}\"\ngranted_unix = 1700000000\ncapabilities = [{announced}]\n\
                 scopes = []\n",
                byte.repeat(32),
                byte.repeat(16),
            ));
        }
        std::fs::write(&path, file).expect("write services.toml");
    }

    /// spawn the node at `idx` as a validator/mesh member, stdout+stderr to a
    /// log file. does not wait for readiness — pair with [`Self::wait_marker`].
    pub fn spawn(&mut self, idx: usize) {
        let id = self.peer_ids[idx];
        let cfg = self.config_path(idx);
        let log = self.dir.path().join(format!("node{id}.log"));
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ducktape"));
        cmd.arg("node")
            .arg("run")
            .arg("--config")
            .arg(&cfg)
            .envs(self.env[idx].iter().map(|(k, v)| (k.as_str(), v.as_str())));
        self.nodes[idx] = Some(NodeProc::spawn(id, log, cmd, "node"));
        self.spawn_compute(idx);
    }

    /// spawn node `idx`'s compute daemon, when it has a grant to act under.
    ///
    /// Waits for the node to PUBLISH ITS MESH IDENTITY first, and that is not
    /// politeness: the daemon's opening move is a `/v1/services/hello` POST
    /// whose failure is deliberately fatal (`run_service`: "the FIRST hello
    /// must land ... a build mismatch or a down node is a loud exit, not a
    /// silent spin"). Only the query lane retries. Launching both processes in
    /// the same breath therefore killed the daemon outright whenever it won the
    /// race to the socket — observed as `FATAL: POST http://…/v1/services/hello`
    /// — which is a coin-flip, not a test. An operator starts a service against
    /// a node that is already up, and so does this.
    ///
    /// "app surface listening" is the WRONG marker for it and used to be the one
    /// waited on: the node binds its HTTP listener well before the boundary
    /// status carries a `public_key`, so the daemon could reach a node that was
    /// up and still read `public_key: ""` — exiting with `FATAL: this node has
    /// not published a mesh identity yet`. `validator::run` logs the publish as
    /// its own once-per-boot fact precisely so this wait has a seam to hold.
    ///
    /// `--config` points it at the SAME dev-shape config the node reads (a dev
    /// workspace is its `storage_dir`, which does not contain the config file,
    /// so `--workspace` cannot name it).
    pub fn spawn_compute(&mut self, idx: usize) {
        if self.compute_grant.is_none() {
            return;
        }
        assert!(
            self.daemons[idx].is_none(),
            "compute already runs on node {idx}"
        );
        self.wait_marker(idx, "mesh identity published", Duration::from_secs(90));
        let id = self.peer_ids[idx];
        let cfg = self.config_path(idx);
        let log = self.dir.path().join(format!("compute{id}.log"));
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ducktape"));
        cmd.arg("service")
            .arg("run")
            .arg("compute")
            .arg("--config")
            .arg(&cfg)
            // the grant is already on disk (`write_service_grants`), so the
            // daemon must never try to mint one — there is no tty here.
            .arg("--no-enable")
            .envs(self.env[idx].iter().map(|(k, v)| (k.as_str(), v.as_str())));
        self.daemons[idx] = Some(NodeProc::spawn(id, log, cmd, "compute"));
    }

    /// Grant node `idx` a service of `kind` and run its daemon beside the node
    /// — the harness twin of `service enable <kind>` + `service run <kind>`.
    ///
    /// Called EXPLICITLY rather than folded into [`Self::spawn`] (where the
    /// compute daemon rides along) because a daemon's FIRST hello must LAND:
    /// the node has to have PUBLISHED ITS MESH IDENTITY before the daemon
    /// starts — a listening http surface is not enough, see [`Self::spawn_compute`]
    /// — and only the test knows when it has waited for that. Any caller that
    /// waited on committed state has already cleared it: the identity goes out
    /// in the startup snapshot, before the first block.
    pub fn spawn_service(&mut self, idx: usize, kind: &str) {
        let id = self.peer_ids[idx];
        // One grant per kind is the file's rule and one daemon per kind is the
        // operator's: a second `service run <kind>` beside the same node would
        // be two processes sharing one grant, which is a test bug, not a
        // scenario. Refuse it here, where the cause is visible.
        let already_running = self.services[idx]
            .iter()
            .any(|service| service.kind == kind);
        let compute_rides_along = kind == "compute" && self.compute_grant.is_some();
        assert!(
            !already_running && !compute_rides_along,
            "a {kind} daemon already runs beside node idx {idx}"
        );
        // `config_file`, not `config_path`: the latter REWRITES node.toml and
        // services.toml, which would drop the grant appended just below.
        let cfg = self.config_file(idx);
        self.service_kinds[idx].push(kind.to_string());
        self.write_service_grants(idx);
        let log = self.dir.path().join(format!("{kind}{id}.log"));
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ducktape"));
        cmd.arg("service")
            .arg("run")
            .arg(kind)
            .arg("--config")
            .arg(&cfg)
            // the grant is already on disk, so the daemon must never try to
            // mint one — there is no tty here.
            .arg("--no-enable")
            .envs(self.env[idx].iter().map(|(k, v)| (k.as_str(), v.as_str())));
        self.services[idx].push(ServiceProc {
            kind: kind.to_string(),
            proc: NodeProc::spawn(id, log, cmd, kind),
        });
    }

    /// wait until node `idx`'s `kind` service daemon prints a line containing
    /// `marker`. Fails fast if the daemon exits without printing it — the twin
    /// of [`Self::wait_marker`], for the process on the other side of the link.
    pub fn wait_service_marker(
        &self,
        idx: usize,
        kind: &str,
        marker: &str,
        timeout: Duration,
    ) -> String {
        let service = self.services[idx]
            .iter()
            .find(|service| service.kind == kind)
            .unwrap_or_else(|| panic!("no {kind} daemon runs beside node idx {idx}"));
        service
            .proc
            .wait_marker(marker, Instant::now() + timeout)
            .unwrap_or_else(|why| {
                panic!(
                    "{kind} daemon beside node idx {idx} {} without printing {marker:?};\n{}\n{}",
                    why.verb(),
                    self.all_log_tails(60),
                    service.proc.text(),
                )
            })
    }

    /// kill the node at `idx` (crash-fault injection) and reap it.
    ///
    /// Its service daemons go too: a daemon is that node's plane, and leaving
    /// one attached to a dead node would keep retrying against a port the next
    /// spawn reuses.
    pub fn kill(&mut self, idx: usize) {
        self.services[idx].clear(); // NodeProc::drop kills + waits
        self.daemons[idx] = None;
        self.nodes[idx] = None;
    }

    /// remove node `idx`'s storage directory — a killed slot reused as a
    /// FRESH resident (the sync-only rebuild) must not inherit the previous
    /// occupant's state or index locks.
    pub fn wipe_storage(&self, idx: usize) {
        let id = self.peer_ids[idx];
        let _ = std::fs::remove_dir_all(self.dir.path().join(format!("storage-{id}")));
    }

    /// send SIGTERM to node `idx` WITHOUT reaping it — the graceful-quit fault
    /// the desktop shell injects when it SIGTERMs the daemon on app quit. the
    /// node's own signal arm should run its final checkpoint and exit 0; the
    /// caller then reaps via [`Self::wait_exit`]. dependency-free: shells out to
    /// `kill(1)` on the child pid rather than pulling in libc/nix for one call.
    pub fn term(&self, idx: usize) {
        let node = self.nodes[idx].as_ref().expect("node is running");
        let pid = node.child.id();
        let status = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .expect("send SIGTERM");
        assert!(status.success(), "kill -TERM {pid} failed");
    }

    /// the deterministic path of node `idx`'s config file (written by a prior
    /// [`Self::spawn`] / [`Self::spawn_joiner`]) — for verbs that read it.
    pub fn config_file(&self, idx: usize) -> PathBuf {
        self.dir
            .path()
            .join(format!("node{}.toml", self.peer_ids[idx]))
    }

    /// Node-local workspace used by dev-shape operational commands. In this
    /// shape it is the same directory as `storage_dir`.
    pub fn workspace(&self, idx: usize) -> PathBuf {
        self.dir
            .path()
            .join(format!("storage-{}", self.peer_ids[idx]))
    }

    /// the bootstrapper address every non-founder / joiner dials: the override
    /// when set (e.g. a sentry/forwarder in front of node 0), else node 0's own
    /// p2p port — today's default behavior.
    fn bootstrap_addr(&self) -> String {
        self.bootstrap_addr_override
            .clone()
            .unwrap_or_else(|| format!("127.0.0.1:{}", self.p2p_ports[0]))
    }

    /// the full `peer_addrs` line, index-aligned with `peer_seeds`: node 0
    /// rides `bootstrap_addr()` (honoring a sentry/forwarder override), the
    /// rest their real p2p ports. the mesh has no address gossip, so every
    /// config carries the whole list; self-entries are filtered at resolve.
    fn peer_addrs_toml(&self) -> String {
        let addrs: Vec<String> = self
            .p2p_ports
            .iter()
            .enumerate()
            .map(|(i, port)| match i {
                0 => self.bootstrap_addr(),
                _ => format!("127.0.0.1:{port}"),
            })
            .collect();
        format!("peer_addrs = {addrs:?}\n")
    }

    /// spawn an UNINVITED joiner: identity seed `id`, deliberately absent
    /// from every existing member's `peer_seeds` — the mesh refuses it until
    /// governance admits it and the epoch cutover re-tracks. its own config
    /// lists the CURRENT members as mesh + validators (the invite descriptor
    /// a real joiner receives). transport/rpc/http/invite ports are allocated
    /// so the node can be driven after it promotes itself. call this AFTER
    /// every member spawn — it appends to the cluster index space.
    ///
    /// returns the joiner's cluster index.
    pub fn spawn_joiner(&mut self, id: u64) -> usize {
        let ports = alloc_ports(4);
        let path = self.dir.path().join(format!("node{id}.toml"));
        let mut cfg = String::new();
        cfg.push_str(&format!("id = {id}\n"));
        cfg.push_str(&format!("listen = \"127.0.0.1:{}\"\n", ports[0]));
        cfg.push_str(&format!("namespace = {:?}\n", self.namespace));
        cfg.push_str(&format!("peer_seeds = {:?}\n", self.peer_ids));
        cfg.push_str(&format!("validator_seeds = {:?}\n", self.validator_ids));
        cfg.push_str(&format!("modules = {:?}\n", founding_set()));
        cfg.push_str(&self.peer_addrs_toml());
        cfg.push_str(&format!(
            "storage_dir = {:?}\n",
            self.dir
                .path()
                .join(format!("storage-{id}"))
                .to_str()
                .unwrap()
        ));
        cfg.push_str(&format!("rpc_listen = \"127.0.0.1:{}\"\n", ports[1]));
        cfg.push_str(&format!("http_listen = \"127.0.0.1:{}\"\n", ports[2]));
        cfg.push_str(&format!("block_time_ms = {TEST_BLOCK_TIME_MS}\n"));
        if self.wireguard {
            cfg.push_str(&format!("wireguard_listen = \"127.0.0.1:{}\"\n", ports[0]));
            cfg.push_str(&format!("invite_listen = \"127.0.0.1:{}\"\n", ports[3]));
        }
        // the joiner runs the same node.toml a member does — including whatever
        // the test appended. Skipping it made a joiner silently diverge from the
        // cluster it was joining (a hermetic `primary_coordinator = "none"` on
        // the members, and a joiner still dialing the live public coordinator).
        for line in &self.extra_toml {
            cfg.push_str(line);
            cfg.push('\n');
        }
        std::fs::write(&path, cfg).expect("write joiner config");

        let log = self.dir.path().join(format!("node{id}.log"));
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ducktape"));
        cmd.arg("node").arg("run").arg("--config").arg(&path);
        let joiner = NodeProc::spawn(id, log, cmd, "joiner");

        self.peer_ids.push(id);
        self.p2p_ports.push(ports[0]);
        self.invite_ports.push(ports[3]);
        self.rpc_ports.push(ports[1]);
        self.http_ports.push(ports[2]);
        // keep `advertised`/`env`/`sandbox` index-aligned with the extended
        // index space so a later `config_path(joiner_idx)` / `spawn` never
        // panics.
        self.advertised.push(None);
        self.env.push(Vec::new());
        self.sandbox.push(None);
        self.nodes.push(Some(joiner));
        self.peer_ids.len() - 1
    }

    /// run a ducktape VERB (resident accept, admit, ...) to completion and
    /// return (success, combined output).
    pub fn run_verb(&self, args: &[&str]) -> (bool, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_ducktape"))
            .args(args)
            .output()
            .expect("run ducktape verb");
        (
            out.status.success(),
            format!(
                "{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    }

    /// run a ducktape VERB with extra environment and a piped stdin (the
    /// `user`/`cred` families read a key password there) and return
    /// (success, combined output).
    pub fn run_verb_with(
        &self,
        args: &[&str],
        env: &[(&str, &str)],
        stdin: &str,
    ) -> (bool, String) {
        use std::io::Write as _;
        let mut child = Command::new(env!("CARGO_BIN_EXE_ducktape"))
            .args(args)
            .envs(env.iter().copied())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn ducktape verb");
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(stdin.as_bytes())
            .expect("write the verb's stdin");
        let out = child.wait_with_output().expect("run ducktape verb");
        (
            out.status.success(),
            format!(
                "{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    }

    /// wait for node `idx` to exit ON ITS OWN (a graceful shutdown path) and
    /// reap it — the counterpart of [`Self::kill`] for restart tests.
    pub fn wait_exit(&mut self, idx: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let exited = self.nodes[idx]
            .as_mut()
            .expect("node is running")
            .wait_exit(deadline);
        assert!(
            exited.is_ok(),
            "node idx {idx} did not exit within {timeout:?};\n{}",
            self.all_log_tails(40)
        );
        self.nodes[idx] = None;
    }

    /// run the node at `idx` with `--sync-only` to completion and return
    /// (success, full log). the sync path exits on its own — 0 with a
    /// `synced root_hash=` line on parity, 1 on any mismatch.
    pub fn run_sync_only(&mut self, idx: usize, timeout: Duration) -> (bool, String) {
        // this port was reserved minutes ago (cluster layout) and nothing held
        // it since — by joiner time another process may own it. re-allocate
        // fresh: the listen addr lives only in THIS node's config (peers know
        // the joiner by identity, not address).
        self.p2p_ports[idx] = alloc_ports(1)[0];
        let id = self.peer_ids[idx];
        let cfg = self.config_path(idx);
        let log = self.dir.path().join(format!("node{id}-sync.log"));
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ducktape"));
        cmd.arg("node")
            .arg("run")
            .arg("--config")
            .arg(&cfg)
            .arg("--sync-only");
        // dropped on the way out: a joiner still running past its deadline is
        // killed and reaped like any other harness process.
        let mut joiner = NodeProc::spawn(id, log, cmd, "sync-only joiner");
        match joiner.wait_exit(Instant::now() + timeout) {
            Ok(status) => (status.success(), joiner.text()),
            Err(_) => (
                false,
                format!("JOINER TIMED OUT after {timeout:?}\n{}", joiner.text()),
            ),
        }
    }

    /// non-blocking probe: has node `idx` printed `marker` yet? returns the
    /// rest of the first matching line (trimmed — for `foo=` markers that is
    /// the value).
    pub fn marker(&self, idx: usize, marker: &str) -> Option<String> {
        let node = self.nodes[idx].as_ref().expect("node is running");
        let text = std::fs::read_to_string(&node.log).unwrap_or_default();
        find_marker(&text, marker)
    }

    /// wait until node `idx` prints a line containing `marker`, returning the
    /// rest of that line. fails fast if the process exits without printing it.
    pub fn wait_marker(&self, idx: usize, marker: &str, timeout: Duration) -> String {
        let node = self.nodes[idx].as_ref().expect("node is running");
        node.wait_marker(marker, Instant::now() + timeout)
            .unwrap_or_else(|why| {
                panic!(
                    "node #{} {} without printing {marker:?};\n{}",
                    node.id,
                    why.verb(),
                    self.all_log_tails(60)
                )
            })
    }

    /// how many times node `idx` has printed `marker` so far — the ABSENCE
    /// assertion `wait_marker` cannot express.
    pub fn marker_count(&self, idx: usize, marker: &str) -> usize {
        let node = self.nodes[idx].as_ref().expect("node is running");
        node.marker_count(marker)
    }

    /// wait until node `idx`'s COMPUTE DAEMON prints a line containing `marker`.
    ///
    /// The compute plane is a SEPARATE PROCESS with its own failure domain, and
    /// without this the suite has no eye on it at all: a daemon that exits at
    /// boot — an unconfigured `[sandbox]`, a `/dev/kvm` it cannot open, a hello that
    /// raced its node — leaves a cluster that looks perfectly healthy, because
    /// the node is. The suite then waits out a three-minute convergence budget
    /// and fails on whatever unrelated predicate it happened to be holding,
    /// while the daemon's one-line FATAL sits unread in a log nothing prints.
    /// That is exactly how a compute plane that could not claim ANY work reached
    /// review, so gating on the daemon's own lifecycle marker is the fix.
    pub fn wait_compute_marker(&self, idx: usize, marker: &str, timeout: Duration) -> String {
        let daemon = self.daemons[idx]
            .as_ref()
            .expect("node has a compute daemon (set `compute_grant` before spawn)");
        daemon
            .wait_marker(marker, Instant::now() + timeout)
            .unwrap_or_else(|why| {
                panic!(
                    "compute daemon #{} {} without printing {marker:?};\n\
                     --- compute daemon log ---\n{}",
                    daemon.id,
                    why.verb(),
                    daemon.tail(60),
                )
            })
    }

    /// every value that followed `marker` on a line of node `idx`'s COMPUTE
    /// DAEMON output, in the order the daemon printed them.
    ///
    /// unlike `wait_compute_marker` (blocks for the FIRST match) this reads
    /// what the continuously-drained feed already holds: a fact this quick to
    /// fire and this transient on disk — a run dir materializes and is
    /// cleaned up inside one run — can only be witnessed as an event stream,
    /// never a filesystem sample that might land between the create and the
    /// cleanup. See `portable_workspace_e2e`'s `materialized_dirs`.
    pub fn compute_markers(&self, idx: usize, marker: &str) -> Vec<String> {
        let daemon = self.daemons[idx]
            .as_ref()
            .expect("node has a compute daemon (set `compute_grant` before spawn)");
        extract_markers(&daemon.text(), marker)
    }

    /// Wait until `probe` answers, re-evaluating it on node `idx`'s heartbeat.
    ///
    /// Replaces a 300ms client-side sleep-and-retry with a thread that blocks on
    /// the node's own ws stream: committed state cannot change without a block,
    /// so re-reading on the node's schedule instead of the test's is both
    /// cheaper and impossible to make fire early. It is a ≤3s wake rather than a
    /// pure event — see [`BlockFeed`] for exactly why.
    ///
    /// Derived read models (`/v1/index/...`) apply BEHIND finalized state, so a
    /// predicate over one simply satisfies a block or two later; the loop is
    /// already per-block, so that costs nothing but patience.
    pub fn await_committed<T>(
        &self,
        idx: usize,
        what: &str,
        timeout: Duration,
        probe: impl FnMut() -> Option<T>,
    ) -> T {
        await_committed_on(self.http_ports[idx], idx, what, timeout, probe, || {
            self.all_log_tails(60)
        })
    }

    /// Attach to node `idx`'s block-wake feed (see [`BlockFeed`]).
    pub fn block_feed(&self, idx: usize, timeout: Duration) -> BlockFeed {
        block_feed_on(self.http_ports[idx], idx, timeout)
    }

    /// one json-lines rpc round-trip against node `idx`, with connect retries
    /// (the listener is up before the pump — early calls may still race the
    /// process start).
    pub fn rpc(&self, idx: usize, req: serde_json::Value) -> serde_json::Value {
        let port = self.rpc_ports[idx];
        let mut line = serde_json::to_string(&req).expect("rpc request serializes");
        line.push('\n');
        let retryable = req["cmd"] == "query";
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            // the listener is up before the pump — connecting retries for
            // every cmd (nothing has been sent yet, so retrying is safe).
            // the node logs its rpc listener the moment the port is bound, and
            // nothing answers before that — the line is the connect event.
            self.wait_marker(idx, "rpc listening on", Duration::from_secs(30));
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap_or_else(|e| {
                panic!(
                    "rpc connect to node idx {idx} (port {port}) failed: {e};\n{}",
                    self.all_log_tails(40)
                )
            });
            stream
                .set_read_timeout(Some(Duration::from_secs(if retryable { 15 } else { 60 })))
                .expect("rpc read timeout");
            stream.write_all(line.as_bytes()).expect("rpc write");
            let mut reply = String::new();
            match BufReader::new(stream).read_line(&mut reply) {
                Ok(n) if n > 0 => {
                    return serde_json::from_str(reply.trim()).expect("rpc reply is json");
                }
                res => {
                    // a query is idempotent: reconnect and resend across the
                    // windows where the listener answers connects but the node
                    // cannot reply yet (a promotion reboot, an epoch-cutover
                    // engine restart). a submit is NEVER resent — a duplicate
                    // could double-apply — it gets one long read window above.
                    let why = match res {
                        Ok(_) => "connection closed before a reply line".to_string(),
                        Err(e) => e.to_string(),
                    };
                    if !retryable || Instant::now() >= deadline {
                        panic!(
                            "rpc to node idx {idx} (port {port}) failed: {why}; request: {req};\n{}",
                            self.all_log_tails(60)
                        );
                    }
                    std::thread::sleep(Duration::from_millis(300));
                }
            }
        }
    }

    /// submit an op via node `idx`'s rpc and assert the lane accepted it
    /// (accepted != finalized — follow with a query poll).
    pub fn submit(&self, idx: usize, target: &str, payload: &[u8]) {
        let reply = self.try_submit(idx, target, payload);
        assert_eq!(
            reply["ok"], true,
            "submit to {target} via node idx {idx} rejected: {reply}"
        );
    }

    /// submit an op via node `idx`'s rpc and return the raw reply — for tests
    /// that assert a submit is REJECTED (cleanly, with the node still live).
    pub fn try_submit(&self, idx: usize, target: &str, payload: &[u8]) -> serde_json::Value {
        self.rpc(
            idx,
            serde_json::json!({
                "cmd": "submit",
                "target": target,
                "payload_hex": hex(payload),
            }),
        )
    }

    /// query a module through node `idx`'s rpc. `None` on a module error —
    /// polls legitimately race finalization (e.g. an unknown channel BEFORE
    /// the create op applies), so rejection is "not yet", not a harness bug.
    pub fn query(&self, idx: usize, target: &str, req: &[u8]) -> Option<Vec<u8>> {
        let reply = self.rpc(
            idx,
            serde_json::json!({
                "cmd": "query",
                "target": target,
                "req_hex": hex(req),
            }),
        );
        if reply["ok"] != true {
            return None;
        }
        Some(unhex(
            reply["reply_hex"]
                .as_str()
                .expect("query reply carries hex"),
        ))
    }

    /// one request against node `idx`'s http/ws APP SURFACE (the noded wire
    /// contract the validator now serves itself) — raw http/1.1 over std TCP,
    /// returning (status, json body). the surface trusts localhost callers,
    /// so a hand-rolled client is a full citizen by design.
    pub fn http(
        &self,
        idx: usize,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> (u16, serde_json::Value) {
        let bytes = body
            .map(|b| serde_json::to_vec(b).expect("request body serializes"))
            .unwrap_or_default();
        let (status, raw) = self.http_bytes(idx, method, path, "application/json", &bytes);
        (
            status,
            serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null),
        )
    }

    /// the raw-bytes twin of [`Self::http`], carrying this node's operator
    /// credential. EVERY mutating `/v1` route refuses a caller that presents
    /// neither that nor a user signature, and a harness driving a node it owns
    /// is exactly the local operator the credential names.
    pub fn http_bytes(
        &self,
        idx: usize,
        method: &str,
        path: &str,
        content_type: &str,
        body: &[u8],
    ) -> (u16, Vec<u8>) {
        let token = self.operator_token(idx);
        nettest::try_http_bytes_with(
            self.http_ports[idx],
            method,
            path,
            content_type,
            &[(noded::admin::ADMIN_TOKEN_HEADER, &token)],
            body,
        )
        .expect("app-surface request")
    }

    /// node `idx`'s operator credential, read out of the workspace the node
    /// minted it into at boot — the same file a real local daemon reads.
    pub fn operator_token(&self, idx: usize) -> String {
        noded::admin::read_operator_token(&self.workspace(idx))
            .expect("the node minted an operator credential")
    }

    /// Seed an automation-owned ref through generic node-origin transport.
    pub fn seed_forge(&self, idx: usize, source: &Path, repo: &str, branch: &str) {
        seed_forge_at(
            self.http_ports[idx],
            &self.workspace(idx),
            source,
            repo,
            branch,
        );
    }

    pub fn clone_forge(&self, idx: usize, repo: &str, destination: &Path) {
        clone_forge_at(
            self.http_ports[idx],
            &self.workspace(idx),
            repo,
            destination,
        );
    }

    /// a duckfs transport for node `idx` whose writes it admits.
    pub fn files(&self, idx: usize) -> duckfs_client::http::HttpNode {
        let token = self.operator_token(idx);
        duckfs_client::http::HttpNode::new(self.http_base(idx))
            .with_operator_credential(std::sync::Arc::new(move || Some(token.clone())))
    }

    /// GET a raw TEXT body from node `idx`'s app surface — for non-json
    /// responses like the Prometheus `/metrics` exposition, which the
    /// json-parsing [`Self::http`] twin would flatten to `Null`.
    pub fn http_text(&self, idx: usize, path: &str) -> (u16, String) {
        http_text_request(self.http_ports[idx], path)
    }

    /// the http base url of node `idx`'s app surface — what a `duckfs-client`
    /// `HttpNode` (or any plain http client) dials.
    pub fn http_base(&self, idx: usize) -> String {
        format!("http://127.0.0.1:{}", self.http_ports[idx])
    }

    /// Every log this cluster owns: each node's, AND each service daemon's —
    /// the panic payload that makes a stalled mesh diagnosable from a CI
    /// failure alone.
    ///
    /// The daemons are not decoration. A dispatch/agent e2e fails on a
    /// PREDICATE the node's own log cannot explain — "no provider announced its
    /// tags", "nobody bid" — because the decision was the daemon's and the node
    /// only ever saw its silence. Those logs live in a `TempDir` that Drop
    /// removes as the panic unwinds, so a tail that skips them throws away the
    /// only copy.
    pub fn all_log_tails(&self, lines: usize) -> String {
        let nodes = self
            .nodes
            .iter()
            .flatten()
            .map(|n| format!("--- node #{} log tail ---\n{}", n.id, n.tail(lines)));
        let riding = self.daemons.iter().flatten().map(|d| {
            format!(
                "--- node #{} compute daemon log tail ---\n{}",
                d.id,
                d.tail(lines)
            )
        });
        let explicit = self.services.iter().flatten().map(|s| {
            format!(
                "--- node #{} {} daemon log tail ---\n{}",
                s.proc.id,
                s.kind,
                s.proc.tail(lines)
            )
        });
        nodes
            .chain(riding)
            .chain(explicit)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// the node's status projection (height, root_hash, module roots).
    pub fn status(&self, idx: usize) -> serde_json::Value {
        let reply = self.rpc(idx, serde_json::json!({ "cmd": "status" }));
        assert_eq!(
            reply["ok"], true,
            "status via node idx {idx} failed: {reply}"
        );
        reply["status"].clone()
    }
}

/// Seed an automation-owned ref through generic node-origin transport.
fn seed_forge_at(port: u16, workspace: &Path, source: &Path, name: &str, branch: &str) {
    let token = noded::admin::read_operator_token(workspace).unwrap();
    let request = |path: &str, kind: &str, bytes: &[u8]| {
        let (status, body) = nettest::try_http_bytes_with(
            port,
            "POST",
            path,
            kind,
            &[(noded::admin::ADMIN_TOKEN_HEADER, &token)],
            bytes,
        )
        .unwrap();
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()
    };
    let repo = git2::Repository::open(source).unwrap();
    let head = repo.head().unwrap().peel_to_commit().unwrap().id();
    let pack = forge::pack_closure_many(&repo, &[head]).unwrap();
    let digest = request("/v1/files/blob", "application/octet-stream", &pack)["digest"]
        .as_str()
        .unwrap()
        .to_owned();
    let query = serde_json::json!({"target":"forge","query":{"list_refs":{"repo":name}}});
    let reply = request(
        "/v1/query",
        "application/json",
        &serde_json::to_vec(&query).unwrap(),
    );
    let previous = reply["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == branch)
        .map(|item| {
            git2::Oid::from_str(item["head"].as_str().unwrap())
                .unwrap()
                .as_bytes()
                .to_vec()
        });
    let payload = forge::encode_msg(&forge::ForgeMsg::PushRefs {
        repo: name.into(),
        updates: vec![forge::RefUpdate {
            ref_name: branch.into(),
            prev_oid: previous,
            new_oid: Some(head.as_bytes().to_vec()),
        }],
        pack_digest: Some(duckfs_core::from_hex_32(&digest).unwrap().to_vec()),
        cert: None,
    });
    request(
        &format!("/v1/submit/raw/forge?required_blob={digest}"),
        "application/octet-stream",
        &payload,
    );
}

/// Read the peer's actual objects through the installed service protocol.
fn clone_forge_at(port: u16, workspace: &Path, name: &str, destination: &Path) {
    let status = nettest::http_json(port, "GET", "/v1/status", None).1;
    let config = ducktape_forge_service::Config {
        node_url: format!("http://127.0.0.1:{port}"),
        node_key: status["public_key"].as_str().unwrap().into(),
        chain_id: status["chain_id"].as_str().unwrap().into(),
        account: 1,
        label: "git".into(),
        module: "forge".into(),
        git_store: workspace.join("forge-repo"),
        signing_seed: "09".repeat(32),
    };
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    let worker = std::thread::spawn(move || {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let router = ducktape_forge_service::router(config, [b'a'; 64]).unwrap();
                ready_tx
                    .send(listener.local_addr().unwrap().port())
                    .unwrap();
                axum::serve(listener, router)
                    .with_graceful_shutdown(async {
                        let _ = stopped.await;
                    })
                    .await
                    .unwrap();
            });
    });
    let service_port = ready_rx.recv().unwrap();
    let output = Command::new("git").args(["-c", "http.extraHeader=x-duck-upstream-token: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "-c", "http.extraHeader=x-duck-route-account: 1", "-c", "http.extraHeader=x-duck-route-label: git",
        "-c", "http.extraHeader=x-duck-route-revision: 1", "clone", "--quiet", &format!("http://127.0.0.1:{service_port}/{name}")])
        .arg(destination).output().unwrap();
    let _ = shutdown.send(());
    worker.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn command_output(out: &std::process::Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// one request against an http/ws APP SURFACE port (the noded wire contract)
/// — raw http/1.1 over std TCP, returning (status, json body). the port-keyed
/// twin of [`Cluster::http`], for harnesses that hold ports without a
/// `Cluster` (e.g. [`NetworkShapeCluster`]). the surface trusts localhost
/// callers, so a hand-rolled client is a full citizen by design.
pub fn http_request(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> (u16, serde_json::Value) {
    nettest::http_json(port, method, path, body)
}

/// GET a raw TEXT body from an app-surface port — the non-json twin of
/// [`http_request`], for bodies like the Prometheus `/metrics` exposition.
pub fn http_text_request(port: u16, path: &str) -> (u16, String) {
    nettest::http_text(port, "GET", path)
}

// ---- sandboxed compute: the `[sandbox]` table and the host gate -------------
//
// One home for both halves, because they are one decision. A suite that boots a
// compute daemon needs a `[sandbox]` table (without one the daemon exits at
// boot) AND needs to know whether this host can honour it — and the two answers
// have to agree. They were separately copied into two test binaries, which is
// how one of them ended up gating on a runtime's version string while the other
// gated on the product's own predicate.

/// What a node's WORKSPACE holds for its compute plane. The guest images are
/// the box's one build ([`guest_dir`]), linked in as `<workspace>/guest`; the
/// capability specs and the executors are the node's own, and `None` for
/// either stages an EMPTY directory — a hermetic node, which discovers
/// nothing whatever this box has installed elsewhere. [`Cluster::sandbox`]
/// stages one per node.
#[derive(Clone, Debug, Default)]
pub struct SandboxStage {
    pub capabilities: Option<PathBuf>,
    pub executors: Option<PathBuf>,
}

/// `slot` becomes a link to `target`, or an empty directory without one —
/// replacing whatever the previous spawn staged there.
fn stage_slot(slot: &Path, target: Option<&Path>) {
    let _ = std::fs::remove_dir_all(slot);
    match target {
        Some(target) => std::os::unix::fs::symlink(target, slot).expect("stage a workspace link"),
        None => std::fs::create_dir_all(slot).expect("stage an empty workspace dir"),
    }
}

/// the `[sandbox]` table a cluster node boots with. Appended LAST to
/// [`Cluster::extra_toml`] — nothing may follow a toml table header.
///
/// It says only HOW a run is isolated. WHETHER this node runs any is
/// [`Cluster::compute_grant`]; the daemon needs both, and refuses to boot
/// without the table. WHICH images it boots is the workspace's own
/// `guest/` — [`Cluster::sandbox`] stages it — so the table names no path.
///
/// The runtime is the platform's own hypervisor flavor — the same choice
/// [`unsandboxable_host`] probes with — so the daemon this table boots is the
/// one the capability gate just proved can run.
pub fn sandbox_toml() -> Vec<String> {
    let runtime = provider_host::Vmm::platform_default().config_token();
    vec![
        "[sandbox]".into(),
        format!("runtime = {runtime:?}"),
        "cores = 0".into(),
        "mem_gb = 0".into(),
    ]
}

/// Can this host isolate a run the way the compute daemon demands? `Some(why)`
/// = no.
///
/// Asks the PRODUCT'S OWN predicate rather than a weaker proxy, and the
/// difference is not academic. `firecracker` on `PATH` is one of several things
/// the backend needs — `/dev/kvm` must OPEN read-write for this process (a host
/// can list the kvm group and still get EACCES until the next login), `mke2fs`
/// and `debugfs` must exist, and the guest images must be built. A suite gated
/// on the weaker question runs anyway and FAILS instead of skipping. Gating on
/// `probe()` means a suite skips when, and only when, a real node would refuse
/// to serve compute.
///
/// The images are the box's one guest build, named by `DUCKTAPE_GUEST_DIR`:
/// a node keeps its own copy under its workspace, and a lane links every
/// node's at that build. Unset, no lane can sandbox.
pub fn unsandboxable_host() -> Option<String> {
    let Some(guest) = guest_dir_env() else {
        return Some(
            "DUCKTAPE_GUEST_DIR is unset: point it at a guest build (ops/build-guest-rootfs.sh)"
                .into(),
        );
    };
    let backend = provider_host::SandboxBackend::MicroVm {
        vmm: provider_host::Vmm::platform_default(),
        kernel: guest.join("vmlinux"),
        rootfs: guest.join("rootfs.ext4"),
        // the probe reads the images and the host, never the executors: what
        // a node's runs can exec is staged per node (`SandboxStage`).
        executors: PathBuf::new(),
    };
    backend.probe().err()
}

/// the box's guest build (`DUCKTAPE_GUEST_DIR`): the one `vmlinux` +
/// `rootfs.ext4` a sandboxed lane links into each node's workspace, where an
/// operator's `node sandbox` builds them outright.
pub fn guest_dir_env() -> Option<PathBuf> {
    std::env::var_os("DUCKTAPE_GUEST_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// [`guest_dir_env`] past the gate: [`skip_unless_sandboxed`] admits no lane
/// without it.
pub fn guest_dir() -> PathBuf {
    guest_dir_env().expect("DUCKTAPE_GUEST_DIR: the sandbox gate admits no lane without it")
}

/// the executors a LIVE lane runs — the box's own `ducktape agent install`
/// output, named by `DUCKTAPE_EXECUTOR_DIR`. Only a lane that execs a real
/// agent CLI reads this; a scripted fixture lifts its shell out of the guest
/// ([`script_executor_dir`]).
pub fn installed_executor_dir() -> PathBuf {
    std::env::var_os("DUCKTAPE_EXECUTOR_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .expect("DUCKTAPE_EXECUTOR_DIR: a live lane execs the box's installed agent CLIs")
}

/// Install only the real Linux shell the scripted provider fixtures run as
/// `sh`: the guest's own `dash`, lifted out of its rootfs — an executor runs
/// INSIDE the guest, and the host's shell links a libc the guest need not
/// carry. MicroVM discovery reads this directory; host-path detect overrides
/// are intentionally ignored by the production loader.
pub fn script_executor_dir(root: &Path) -> PathBuf {
    let dir = root.join("executors");
    std::fs::create_dir_all(&dir).expect("fixture executors dir");
    guest_binary("/usr/bin/dash", &dir.join("sh"));
    dir
}

/// copy one file out of the guest rootfs to `dest`, mode and all — how a
/// fixture gets a binary the guest can exec. `debugfs` answers a missing
/// path with a message and a clean exit, hence the file check.
pub fn guest_binary(path_in_guest: &str, dest: &Path) {
    let rootfs = guest_dir().join("rootfs.ext4");
    let debugfs =
        sandbox_host::find_system_tool("debugfs").expect("debugfs: the sandbox gate probed it");
    let out = Command::new(debugfs)
        .arg("-R")
        .arg(format!("dump -p {path_in_guest} {}", dest.display()))
        .arg(&rootfs)
        .output()
        .expect("run debugfs");
    let lifted = out.status.success() && dest.is_file();
    assert!(
        lifted,
        "lift {path_in_guest} out of {}:\n{}",
        rootfs.display(),
        command_output(&out)
    );
}

/// `Some(())` = this test cannot run here and the caller must return; `None` =
/// run it.
///
/// A host that cannot sandbox FAILS by default; skipping is the opt-in
/// ([`nettest::ALLOW_MISSING_TOOLS_ENV`]), because libtest captures stderr and a
/// "skipped" line from a passing test reaches nobody. One switch for every
/// capability gate in the tree — a second, sandbox-only one would just be a
/// second thing to remember.
pub fn skip_unless_sandboxed(test: &str) -> Option<()> {
    nettest::skip_without(test, unsandboxable_host())
}

// `n` distinct free localhost ports, collision-safe (holds every listener at
// once — sequential bind-drop could hand the same port back twice).
use nettest::alloc_ports;

/// Wait until `probe` answers, re-evaluating it on each block wake from the
/// node whose app surface listens on `http_port` — the one body behind both
/// clusters' `await_committed`. `tails` renders the diagnosis on failure,
/// alongside the committed height at entry and at timeout: equal heights name
/// a halt ("height did not move from H") instead of reading like a merely
/// slow predicate.
fn await_committed_on<T>(
    http_port: u16,
    idx: usize,
    what: &str,
    timeout: Duration,
    mut probe: impl FnMut() -> Option<T>,
    tails: impl Fn() -> String,
) -> T {
    // committed state may already satisfy it — never wait on a block that
    // has nothing left to deliver (and the chain may be idle-quiet).
    if let Some(value) = probe() {
        return value;
    }
    let mut blocks = block_feed_on(http_port, idx, timeout);
    let height_at_entry = blocks.sync_height().ok();
    loop {
        if let Err(why) = blocks.next_block() {
            let height_at_timeout = blocks.height();
            let height_note = match (height_at_entry, height_at_timeout) {
                (Some(entry), Some(timeout)) if entry == timeout => {
                    format!("height did not move from {entry}")
                }
                _ => format!(
                    "height was {height_at_entry:?} at entry, {height_at_timeout:?} at timeout"
                ),
            };
            panic!(
                "timed out after {timeout:?} waiting for {what} \
                 (via node idx {idx}): {why}; {height_note};\n{}",
                tails()
            );
        }
        if let Some(value) = probe() {
            return value;
        }
    }
}

/// Attach to the block-wake feed of the node whose app surface listens on
/// `http_port` (see [`BlockFeed`]).
fn block_feed_on(http_port: u16, idx: usize, timeout: Duration) -> BlockFeed {
    let url = format!("ws://127.0.0.1:{http_port}/v1/ws");
    let (socket, _response) = tokio_tungstenite::tungstenite::connect(&url)
        .unwrap_or_else(|e| panic!("attach to node idx {idx} block feed: {e}"));
    // the failure path's bound, not a poll interval — see [`BlockFeed`].
    if let tokio_tungstenite::tungstenite::stream::MaybeTlsStream::Plain(tcp) = socket.get_ref() {
        tcp.set_read_timeout(Some(timeout))
            .expect("bound the block feed's failure path");
    }
    BlockFeed {
        socket,
        deadline: Instant::now() + timeout,
        last_height: None,
    }
}

/// The committed height a `heartbeat` frame carries, or `None` for anything
/// else on the wire (a control frame, or a frame this node never sends on an
/// unsubscribed connection).
fn heartbeat_height(text: &str) -> Option<u64> {
    let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
    if value["type"] != "heartbeat" {
        return None;
    }
    value["height"].as_u64()
}

/// A live feed of one node's heartbeat frames — the harness's wake seam for
/// "committed state may have changed".
///
/// The node sends a `heartbeat` frame to every ws client on every block wake,
/// nop fillers included, so an unsubscribed connection is already the changed
/// feed (the compute daemon's own intake rides the same frame). Blocking on
/// that socket means the thread wakes on the chain's own event and re-reads
/// only when there is something new to read.
///
/// **A `heartbeat` frame alone is not proof of that**, and the difference
/// matters: `crates/noded/src/stream.rs` also emits a byte-identical
/// `heartbeat` on a 3s `tokio::time::interval` regardless of progress, and
/// nothing in the frame's shape distinguishes the two — only its `height`
/// does. Reading every frame as a wake made a HALTED chain (stuck at the same
/// height, ticking forever) indistinguishable from a merely slow predicate,
/// since the 3s ticks kept `next_block` returning `Ok` with nothing new to
/// show for it. This tracks the last height it saw and only reports a wake
/// when that height MOVED; an unchanged-height tick is consumed as the
/// liveness fallback it is — proof the connection is alive, not that a block
/// committed — and looped past.
///
/// The socket read timeout is the FAILURE path only — a node that has stopped
/// sending anything must fail with a diagnosis rather than hang CI forever. No
/// successful wait ever waits it out.
pub struct BlockFeed {
    socket: tokio_tungstenite::tungstenite::WebSocket<
        tokio_tungstenite::tungstenite::stream::MaybeTlsStream<TcpStream>,
    >,
    deadline: Instant,
    /// the last committed height this feed observed, or `None` before its
    /// first heartbeat. `next_block` reports a wake only on a change from
    /// this.
    last_height: Option<u64>,
}

impl BlockFeed {
    /// The last committed height this feed observed — `None` before its
    /// first heartbeat (see [`Self::sync_height`]).
    pub fn height(&self) -> Option<u64> {
        self.last_height
    }

    /// Establish the feed's baseline height, reading frames until the first
    /// `heartbeat` arrives. A no-op once a height is already known: callers
    /// that only care about the NEXT wake go straight to [`Self::next_block`],
    /// which calls this itself.
    pub fn sync_height(&mut self) -> Result<u64, String> {
        if let Some(height) = self.last_height {
            return Ok(height);
        }
        use tokio_tungstenite::tungstenite::Message;
        loop {
            if Instant::now() >= self.deadline {
                return Err("no heartbeat received".into());
            }
            let frame = self
                .socket
                .read()
                .map_err(|error| format!("block feed read failed: {error}"))?;
            let Message::Text(text) = frame else { continue };
            let Some(height) = heartbeat_height(&text) else {
                continue;
            };
            self.last_height = Some(height);
            return Ok(height);
        }
    }

    /// Block until the node reports a committed height past the last one this
    /// feed saw — a genuine block event, not merely a liveness tick at the
    /// same height (see the type doc).
    pub fn next_block(&mut self) -> Result<(), String> {
        let baseline = self.sync_height()?;
        use tokio_tungstenite::tungstenite::Message;
        loop {
            if Instant::now() >= self.deadline {
                // NOT "no block wake" — the chain is almost always producing
                // them fine and the predicate is simply still false. Saying
                // otherwise sends the reader hunting a stalled consensus that
                // is not stalled; the caller names the predicate instead.
                return Err("never became true".into());
            }
            let frame = self
                .socket
                .read()
                .map_err(|error| format!("block feed read failed: {error}"))?;
            // an unsubscribed connection carries heartbeats and nothing else,
            // but a control frame still has to be stepped over.
            let Message::Text(text) = frame else { continue };
            let Some(height) = heartbeat_height(&text) else {
                continue;
            };
            self.last_height = Some(height);
            if height != baseline {
                return Ok(());
            }
        }
    }
}

/// Drop every ANSI escape sequence from `text`.
///
/// The node's stderr layer colours unconditionally — it never consults a tty —
/// so a captured line reads `phase\x1b[0m\x1b[2m=\x1b[0m"serving"`, and any
/// assertion on a `field=value` pair has to strip first. Message text is
/// uncoloured, which is why the marker waits above never needed this.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // CSI (`ESC [`) runs to a final byte in `@`..=`~`; any other escape is
        // two characters, and its second one was just consumed.
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

fn find_marker(text: &str, marker: &str) -> Option<String> {
    text.lines().find_map(|line| {
        line.find(marker)
            .map(|at| line[at + marker.len()..].trim().to_string())
    })
}

/// every value that followed `marker` on a line of `text`, ANSI-stripped
/// first: unlike a message-only marker, `compute_markers` reads lines that
/// can carry a coloured field list after the message (see `compute_markers`
/// on `Cluster`), so slicing on the raw bytes risks handing back escape
/// codes instead of the bare value.
fn extract_markers(text: &str, marker: &str) -> Vec<String> {
    strip_ansi(text)
        .lines()
        .filter_map(|line| {
            line.find(marker)
                .map(|at| line[at + marker.len()..].trim().to_string())
        })
        .collect()
}

fn log_tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let from = all.len().saturating_sub(lines);
    all[from..].join("\n")
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
        .collect()
}

// ---- the USER lane: user-signed frames and identity accounts -----------------
//
// `Cluster::submit` rides the rpc, which re-signs every op with the NODE's key:
// its committed origin is the node, and a node key is never on an Identity
// account. Anything a user does — founding an account, claiming a handle,
// publishing a route, registering or granting a credential, scheduling a run —
// is attributed through `OfKey(origin)`, so it has to arrive as a frame the
// USER signed, over `/v1/submit/frame`. These helpers are that lane.

/// a frame's `seq` is an ordering/dedup tie-breaker (any u64); one process-wide
/// counter keeps every frame a suite signs distinct from every other.
static FRAME_SEQ: AtomicU64 = AtomicU64::new(1);

/// the budget for one user-lane op to finalize and become readable elsewhere.
const USER_LANE_FINALIZE: Duration = Duration::from_secs(60);

/// POST one frame `user` signed over `(target, payload)` to node `idx`'s
/// `/v1/submit/frame` and return (status, body). The frame's verified signer
/// becomes the op's `Origin::External`; the validator answers once the op's
/// block commits, so a same-node read right after sees it.
pub fn try_submit_frame(
    cluster: &Cluster,
    idx: usize,
    user: &ed25519::PrivateKey,
    target: &str,
    payload: &[u8],
) -> (u16, serde_json::Value) {
    let seq = FRAME_SEQ.fetch_add(1, Ordering::Relaxed);
    let frame = node::encode_frame(
        user,
        seq,
        &sdk::Msg {
            target: target.into(),
            payload: payload.to_vec(),
        },
    );
    let (status, body) = nettest::http_bytes(
        cluster.http_ports[idx],
        "POST",
        "/v1/submit/frame",
        "application/octet-stream",
        &frame,
    );
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

/// [`try_submit_frame`], asserting the lane accepted and committed the op.
pub fn submit_frame(
    cluster: &Cluster,
    idx: usize,
    user: &ed25519::PrivateKey,
    target: &str,
    payload: &[u8],
) {
    let (status, body) = try_submit_frame(cluster, idx, user, target, payload);
    assert_eq!(
        status, 200,
        "user-signed {target} frame via node idx {idx} rejected: {body}"
    );
}

/// `OfKey(key)` on node `idx`: the account `key` belongs to. `None` covers both
/// a rejected query and "no account" — all a poll needs to tell apart is
/// "resolved" from "not (yet)".
pub fn account_of_key(cluster: &Cluster, idx: usize, key: &[u8]) -> Option<identity::AccountView> {
    let bytes = cluster.query(
        idx,
        "identity",
        &identity::encode_query(&identity::IdentityQuery::OfKey { key: key.to_vec() }),
    )?;
    match identity::decode_reply(&bytes).ok()? {
        identity::IdentityReply::Account(account) => account,
        identity::IdentityReply::Accounts(_)
        | identity::IdentityReply::Resolved(_)
        | identity::IdentityReply::Gen(_) => None,
    }
}

/// `KeyGen(key)` on node `idx`: how many times `key` has been admitted
/// anywhere — the generation an add-key consent for it must sign.
pub fn key_gen(cluster: &Cluster, idx: usize, key: &[u8]) -> Option<u64> {
    let bytes = cluster.query(
        idx,
        "identity",
        &identity::encode_query(&identity::IdentityQuery::KeyGen { key: key.to_vec() }),
    )?;
    match identity::decode_reply(&bytes).ok()? {
        identity::IdentityReply::Gen(generation) => Some(generation),
        identity::IdentityReply::Account(_)
        | identity::IdentityReply::Accounts(_)
        | identity::IdentityReply::Resolved(_) => None,
    }
}

/// found an Identity account for `user` through node `idx` — `Create` as a
/// user-signed frame — and wait until `OfKey(user)` resolves to it there.
/// Returns the account number (1 for the first account a cluster founds).
pub fn create_account(
    cluster: &Cluster,
    idx: usize,
    user: &ed25519::PrivateKey,
    name: &str,
) -> u64 {
    submit_frame(
        cluster,
        idx,
        user,
        "identity",
        &identity::encode_msg(&identity::testkit::create(name)),
    );
    let key = user.public_key().as_ref().to_vec();
    cluster
        .await_committed(
            idx,
            &format!("account {name:?} to found"),
            USER_LANE_FINALIZE,
            || account_of_key(cluster, idx, &key),
        )
        .number
}

/// the expiry every consent an e2e mints carries. `consensus_time` is the
/// block height on a validator network, and a cluster test drives a few
/// hundred blocks at most — so this is past every one of them and inside
/// `identity::MAX_CONSENT_TTL` of each.
pub const CONSENT_EXPIRES: u64 = 100_000;

/// admit `new_key` into `member`'s account through node `idx`: `member`
/// consents at `new_key`'s CURRENT generation, and the JOINING key signs the
/// `AddKey` frame (the op's origin is the key being admitted). Waits until
/// `OfKey(new_key)` resolves there and returns the account as it then reads.
pub fn add_key(
    cluster: &Cluster,
    idx: usize,
    member: &ed25519::PrivateKey,
    new_key: &ed25519::PrivateKey,
) -> identity::AccountView {
    let joining = new_key.public_key().as_ref().to_vec();
    let generation = cluster.await_committed(
        idx,
        "the joining key's generation",
        USER_LANE_FINALIZE,
        || key_gen(cluster, idx, &joining),
    );
    submit_frame(
        cluster,
        idx,
        new_key,
        "identity",
        &identity::encode_msg(&identity::testkit::add_ed25519_key(
            member,
            &cluster.namespace,
            &joining,
            generation,
            None,
            account_of_key(cluster, idx, member.public_key().as_ref())
                .expect("the consenting member belongs to an account")
                .number,
            CONSENT_EXPIRES,
        )),
    );
    cluster.await_committed(
        idx,
        "the joining key to resolve",
        USER_LANE_FINALIZE,
        || account_of_key(cluster, idx, &joining),
    )
}

/// Provision the real program user for one model, under this node's signed
/// account. Read committed identity state instead of predicting an account id.
pub fn provision_model_program(cluster: &Cluster, idx: usize, model: &str) -> u64 {
    let key = Cluster::identity(cluster.peer_ids[idx]);
    let query = identity::encode_query(&identity::IdentityQuery::OfKey { key });
    let reply = cluster
        .query(idx, "identity", &query)
        .expect("identity query");
    let identity::IdentityReply::Account(controller) =
        identity::decode_reply(&reply).expect("identity reply")
    else {
        panic!("account reply");
    };
    let controller = match controller {
        Some(account) => account.number,
        None => {
            cluster.submit(
                idx,
                "identity",
                &identity::encode_msg(&identity::IdentityMsg::Create {
                    name: format!("model-controller-{idx}"),
                    scheme: identity::KeyScheme::Ed25519,
                }),
            );
            cluster.await_committed(idx, "model controller account", USER_LANE_FINALIZE, || {
                let reply = cluster.query(idx, "identity", &query)?;
                let identity::IdentityReply::Account(Some(account)) =
                    identity::decode_reply(&reply).ok()?
                else {
                    return None;
                };
                Some(account.number)
            })
        }
    };
    cluster.submit(
        idx,
        "agent",
        &agent::encode_msg(&agent::AgentMsg::Provision {
            request_id: model.into(),
            name: model.into(),
            program: runs::model_program(model),
        }),
    );
    cluster.await_committed(idx, "model program account", USER_LANE_FINALIZE, || {
        let reply = cluster.query(
            idx,
            "identity",
            &identity::encode_query(&identity::IdentityQuery::All {
                from: 0,
                limit: identity::MAX_QUERY_LIMIT,
            }),
        )?;
        let identity::IdentityReply::Accounts(accounts) = identity::decode_reply(&reply).ok()?
        else {
            return None;
        };
        accounts.into_iter().find_map(|account| {
            let controlled_here = matches!(
                &account.control,
                identity::Control::Program { controller: owner, executor, .. }
                    if *owner == controller && executor == "agent"
            );
            let mine = account.name == model && controlled_here;
            mine.then_some(account.number)
        })
    })
}

pub fn model_account(cluster: &Cluster, idx: usize, model: &str) -> u64 {
    cluster.await_committed(idx, "model configuration", USER_LANE_FINALIZE, || {
        let reply = cluster.query(
            idx,
            "runs",
            &runs::encode_query(&runs::RunsQuery::Model {
                query: runs::ModelQuery::Agent {
                    agent_id: model.into(),
                },
            }),
        )?;
        let runs::RunsReply::Model(runs::ModelReply::Agent(Some(record))) =
            runs::decode_reply(&reply).ok()?
        else {
            return None;
        };
        Some(record.account)
    })
}

/// A source mention's canonical attribution sequence determines its run id.
/// The pending and recent records let fast and slow providers prove the same run.
pub fn attributed_run_id(
    cluster: &Cluster,
    idx: usize,
    channel: &str,
    anchor: u64,
    model: &str,
) -> String {
    cluster.await_committed(idx, "attributed model run", USER_LANE_FINALIZE, || {
        let reply = cluster.query(
            idx,
            "runs",
            &runs::encode_query(&runs::RunsQuery::PendingRuns),
        )?;
        let runs::RunsReply::PendingRuns(pending) = runs::decode_reply(&reply).ok()? else {
            return None;
        };
        if let Some(run) = pending.into_iter().find(|run| {
            run.agent_id == model && run.channel_id == channel && run.anchor_seq == anchor
        }) {
            return Some(run.run_id);
        }
        let reply = cluster.query(
            idx,
            "runs",
            &runs::encode_query(&runs::RunsQuery::RecentRuns),
        )?;
        let runs::RunsReply::RecentRuns(recent) = runs::decode_reply(&reply).ok()? else {
            return None;
        };
        recent
            .into_iter()
            .find(|run| {
                run.agent_id == model && run.channel_id == channel && run.anchor_seq == anchor
            })
            .map(|run| run.run_id)
    })
}

#[cfg(test)]
mod marker_tests {
    use super::extract_markers;

    #[test]
    fn extract_markers_strips_ansi_before_slicing() {
        // shaped like a real fmt-layer capture: coloured level/target ANSI
        // ahead of the message, then a trailing coloured field the message
        // itself does not carry (e.g. another field on the same event).
        // The marker sits in the plain-text message, but a naive byte slice
        // to end-of-line would still hand back the trailing escape codes —
        // extract_markers must return the bare path only.
        let line = "\x1b[2m2026-09-05\x1b[0m \x1b[34mDEBUG\x1b[0m run dir materialized \
                     kind=rw path=/tmp/run-1\x1b[2m note\x1b[0m\x1b[2m=\x1b[0mok";
        let got = extract_markers(line, "run dir materialized kind=rw path=");
        assert_eq!(got, vec!["/tmp/run-1 note=ok".to_string()]);
    }
}
