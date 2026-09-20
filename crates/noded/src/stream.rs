use std::collections::{BTreeMap, VecDeque};
use std::io::{Result as IoResult, Write};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket};
use duckfs_core::{Change, FilesMsg};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, watch};
use tracing_subscriber::fmt::MakeWriter;

use crate::NodeHandle;

/// the TIMER beat: the liveness floor while no blocks commit, and (×2.5) the
/// client watchdog's timeout basis. a heartbeat frame also rides every block
/// wake, so on a moving chain the tip reaches clients per block, not per tick.
pub const HEARTBEAT_INTERVAL_MS: u64 = 3_000;
/// THE ANTI-ENTROPY BACKSTOP. A block wake sweeps the index topics only when
/// the block appended rows ([`BlockWake`]), which makes delivery depend on
/// every index writer announcing. That set is provable today and silently
/// breakable tomorrow — a writer added later that forgets would strand a topic
/// with no error, no `lagged`, and a head that keeps rising. This bounds every
/// such miss, known or not, to one period.
///
/// It REPLACES a sweep that ran once per block — 1 Hz on an idle chain, since
/// the nop filler publishes every block time — so it is 30x less work than
/// what it stands in for, not new work.
pub const INDEX_BACKSTOP_INTERVAL: Duration = Duration::from_secs(30);
pub const STREAM_CATCHUP_BUDGET: usize = 256;
/// per-connection subscription ceiling. the ws surface is unauthenticated
/// (trusted-client convention), so per-connection state must stay bounded:
/// the console needs ~15 module topics + logs + files:watch + metrics + a
/// few run-output panes; far below this. beyond it, subscribes refuse
/// per-topic.
pub const MAX_TOPICS_PER_CONNECTION: usize = 64;
/// the ws frame/message ceiling for `/v1/ws` — this surface is unauthenticated
/// like the rest of the file, so tungstenite's 64 MiB default is 1000x more
/// than any legitimate client message: a `Subscribe` at the topic cap above
/// (64 names + a same-sized `resume` map) or one run-output publish
/// ([`MAX_RUN_OUTPUT_LINE`], 16 KiB) both fit many times over inside this.
pub const MAX_WS_MESSAGE_BYTES: usize = 64 * 1024;
/// rows a files:watch catch-up may SCAN (not just emit) per wakeup — a
/// stage-heavy history is mostly non-commit rows, and an unbounded back-scan
/// would stall the session task; past this the topic lags to live instead.
pub const FILES_SCAN_BUDGET: usize = STREAM_CATCHUP_BUDGET * 4;
pub const LOG_RING_CAPACITY: usize = 4_096;
pub const RUN_OUTPUT_MAX_RUNS: usize = 32;
pub const RUN_OUTPUT_MAX_LINES: usize = 2_048;
/// the exact width of a run-output id: `runs_wire::dispatch_id_for` is a hex
/// sha256, and the agent data plane's `valid_event` enforces the same 64-hex
/// shape before forwarding a line to a peer. This is NOT cosmetic — see
/// [`ClientMsg::RunOutput`].
const RUN_OUTPUT_ID_LEN: usize = 64;
/// the longest run-output line accepted from a ws publisher.
///
/// The agent data plane refuses to write a serialized event over 64 KiB, so
/// this must stay comfortably under that: a line admitted here but refused
/// there would be the same stream teardown, one layer later. 16 KiB is far
/// above any real provider line while leaving room for the peer forwarder's
/// `[node xxxxxxxx] ` prefix and the json envelope.
pub(crate) const MAX_RUN_OUTPUT_LINE: usize = 16 * 1024;

/// how long a command may wait to reach the attached service daemon before the
/// link is declared wedged. Generous — a healthy daemon takes one in microseconds
/// (it only enqueues), so anything near this is a stuck process, not a slow one.
const SERVICE_COMMAND_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// what a ws client may say to this node.
///
/// `deny_unknown_fields`: there is no live network and no compat obligation, so
/// a frame carrying a field this build does not know is refused with a
/// `BadFrame` naming it, never decoded into whatever subset happens to match.
/// Silently dropping the rest would make the sender's intent unobservable — and
/// this PR is a live instance of the direction that used to be tolerated, since
/// it deleted `ServiceAttach.build`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientMsg {
    ComputeAttach {
        token: String,
    },
    RunControlReply {
        id: u64,
        result: Result<serde_json::Value, String>,
    },
    /// join one or more topics. THIS is where a topic's admission is decided —
    /// see [`Topic::admission`] — so the handles this frame hands back are
    /// themselves the capability, and a family a caller was never admitted to
    /// leaves nothing on the connection to act on.
    Subscribe {
        topics: Vec<String>,
        #[serde(default)]
        resume: BTreeMap<String, String>,
        /// this node's own 0600 workspace secret ([`crate::services::LINK_TOKEN_FILE`]),
        /// for the families that carry a member's terminal or a run's output.
        ///
        /// The SAME secret [`Self::ServiceAttach`] presents, deliberately: the
        /// node has exactly one proof that a caller can read its own workspace,
        /// and a second would be a second thing to leak. Presenting it here
        /// claims no link and displaces no daemon — it only answers the
        /// admission question.
        ///
        /// Absent on a caller that wants only the public families (committed
        /// module events, the metrics exposition): those admit anyone the ws
        /// surface admits, because the same bytes already leave this node over
        /// an unauthenticated HTTP route. The log ring is not one of them: it
        /// is the operator's, proved at the upgrade (see [`Topic::admission`]).
        #[serde(default)]
        token: Option<String>,
    },
    Unsubscribe {
        topics: Vec<String>,
    },
    /// one live output line from a run this node's COMPUTE DAEMON is executing.
    ///
    /// The daemon runs out of process, so the in-process `OutputSink` that used
    /// to feed [`RunOutputRegistry`] cannot reach it any more; this is that sink
    /// across the process boundary, on the ws connection the daemon already
    /// holds for work-intake hints. It is a publish, not a subscription, which
    /// is why it is a `ClientMsg` and not a topic.
    ///
    /// Trust: honored ONLY on a connection that has taken the compute
    /// attachment ([`Self::ComputeAttach`], this node's service-link token) —
    /// the credential the one real publisher, the compute daemon
    /// (`bin/node/src/compute/link.rs`), already presents before its first
    /// line. Any other connection's line is refused with a `forbidden` error
    /// frame and never reaches the ring, the same rule [`Self::AgentEvent`]
    /// keeps for the agent link: a run id names consensus state anyone learns
    /// from the chain, so knowing one is no evidence of executing it.
    ///
    /// READING that ring is a different question with a different answer:
    /// `run-output:<id>` is [`Admission::Run`] — the workspace secret, or the
    /// run's own creator proved at the upgrade.
    ///
    /// What is NOT accepted is an unbounded or malformed one. `id` must be the
    /// 64-hex shape the agent data plane's `valid_event` enforces, because a
    /// line that reaches the ring is broadcast to every overlay peer and a
    /// rejected write there tears the peer's stream down — one bad frame would
    /// otherwise be a remote, repeatable denial of service against every peer's
    /// telemetry. `line` is capped for the same reason. Both are checked at this
    /// boundary and dropped with a stable reason; the ring's own
    /// `RUN_OUTPUT_MAX_RUNS`/`RUN_OUTPUT_MAX_LINES` bound line COUNT, never
    /// bytes, so they are no substitute.
    RunOutput {
        id: String,
        stream: RunStream,
        line: String,
    },
    /// a local service daemon claims this connection as its command link.
    ///
    /// The agent daemon owns the ptys behind this node's interactive plane, and
    /// it is the only side that dials — so it must be able to say "commands for
    /// the terminal plane come to me". Until one connection does this, the node
    /// has no interactive plane and every create refuses.
    ///
    /// The claim carries no build stamp and this node compares none: node and
    /// daemon are separate processes with independent restart timing, so skew
    /// is ordinary, and it is named by `service status` rather than refused
    /// (see [`crate::services::build_identity`]). What IS refused is a second
    /// holder — only one connection may hold the link at a time, which is what
    /// stops a local impersonator from displacing the live daemon and receiving
    /// the create commands (and lent-credential records) meant for it.
    ServiceAttach {
        kind: String,
        /// the node's own 0600 link secret, read from its workspace. Holding the
        /// link means BECOMING this node's interactive plane and receiving every
        /// lent-credential record with it, so dialing loopback is not enough.
        token: String,
    },
    /// one lifecycle fact about a pty from the daemon that owns it. Honored ONLY
    /// on a connection that has attached: without that gate, any local process
    /// could inject output into a session's ring or fake its end.
    AgentEvent {
        event: agent_service::wire::Event,
    },
}

// Serialize-only: the node SENDS frames and never parses its own.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    RunControlSnapshot {
        topic: String,
        control: Option<serde_json::Value>,
    },
    Subscribed {
        /// admitted topic -> its start cursor. A REFUSED topic is not in here;
        /// it got its own `Error` frame, ahead of this one, naming the code.
        /// (This was `Option<String>` with exactly one inhabitant — the only
        /// insert always carried a cursor — which read as "admitted but
        /// cursorless", a state that has never existed.)
        topics: BTreeMap<String, String>,
    },
    Event {
        topic: String,
        cursor: String,
        op: StreamOpRow,
    },
    Tail {
        topic: String,
        cursor: String,
        item: TailItem,
    },
    /// one command for the attached agent daemon. Sent only on the connection
    /// that holds the service link, so it never reaches an ordinary subscriber.
    ServiceCommand {
        command: agent_service::wire::Command,
    },
    Lagged {
        topic: String,
        cursor: String,
    },
    Heartbeat {
        height: u64,
        root_hash: String,
        time_ms: u64,
        interval_ms: u64,
    },
    Error {
        topic: String,
        code: StreamErrorCode,
        detail: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StreamErrorCode {
    UnknownTopic,
    Unavailable,
    BadCursor,
    BadFrame,
    /// the topic exists and this caller may not hold it — see
    /// [`Topic::admission`]. Distinct from `UnknownTopic` on purpose: a client
    /// that mistyped a name and one that omitted the node's secret need
    /// different fixes, and collapsing them would send an operator hunting a
    /// typo that is not there.
    Forbidden,
}

/// The ws projection of one stored (borsh) op row — the same json row shape
/// the /v1/index/*/ops lane serves.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StreamOpRow {
    pub height: u64,
    pub seq: u32,
    pub time: u64,
    pub origin: StreamOrigin,
    // skip_serializing_if omits the field on the wire, so the TS side must
    // read `payload?: …` (absent), not `payload: … | null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_hex: Option<String>,
    /// the module-assigned stamp of the dispatch (empty stamps are omitted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_hex: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StreamOrigin {
    pub kind: StreamOriginKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StreamOriginKind {
    External,
    Program,
    Module,
    System,
}

/// project one stored (borsh) op row onto the ws frame shape — the same
/// json row the /v1/index/*/ops lane serves.
fn stream_op_row(row: indexer::OpRow) -> StreamOpRow {
    let payload: Option<serde_json::Value> = serde_json::from_slice(&row.payload).ok();
    let payload_hex = payload.is_none().then(|| crate::hex_bytes(&row.payload));
    let assigned: Option<serde_json::Value> = (!row.assigned.is_empty())
        .then(|| serde_json::from_slice(&row.assigned).ok())
        .flatten();
    let assigned_hex =
        (!row.assigned.is_empty() && assigned.is_none()).then(|| crate::hex_bytes(&row.assigned));
    StreamOpRow {
        height: row.height,
        seq: row.seq,
        time: row.time,
        origin: StreamOrigin {
            kind: match row.origin.kind {
                indexer::OriginKind::External => StreamOriginKind::External,
                indexer::OriginKind::Program => StreamOriginKind::Program,
                indexer::OriginKind::Module => StreamOriginKind::Module,
                indexer::OriginKind::System => StreamOriginKind::System,
            },
            id: row.origin.id,
        },
        payload,
        payload_hex,
        assigned,
        assigned_hex,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum TailItem {
    Log {
        line: String,
    },
    FileChange {
        height: u64,
        time: u64,
        message: String,
        base_snapshot: Option<String>,
        paths: Vec<String>,
    },
    RunOutput {
        stream: RunStream,
        line: String,
    },
    /// one OpenMetrics snapshot — the same text GET /metrics serves, pushed
    /// per heartbeat tick while the `metrics` topic is subscribed. `time_ms`
    /// is the server-side sample instant, so a client derives counter rates
    /// from one clock instead of its own frame-arrival jitter.
    Metrics {
        time_ms: u64,
        text: String,
    },
    /// one direct-peer sample — the SAME view `GET /v1/peers` composes, pushed
    /// per heartbeat tick while the `peers` topic is subscribed. `time_ms` is
    /// the server-side sample instant, the denominator a client needs to derive
    /// per-peer message rates from the cumulative counters inside.
    ///
    /// distinct from [`Self::Metrics`] under `untagged` by its required
    /// `peers` field, which no other variant carries.
    Peers {
        time_ms: u64,
        peers: crate::peers::PeersView,
    },
    /// one node-status snapshot — the same projection `GET /v1/status` serves.
    /// Free to sample: it is a read of the cell the owning actor publishes at
    /// each boundary, with no registry encode behind it (unlike its two
    /// sibling snapshot topics). Distinguished under `untagged` by `status`.
    Status {
        time_ms: u64,
        status: Box<crate::NodeStatus>,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStream {
    Stdout,
    Stderr,
}

/// What a block wake owes the index-tier topics.
///
/// The tip snapshot is owed UNCONDITIONALLY — a console's head moves on nop
/// fillers, which feed no topic at all — so this gates the index SWEEP alone,
/// never the heartbeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockWake {
    /// the tip moved and nothing else. An idle chain nop-fills once per
    /// block time (network.toml `block_time_ms`) and that filler appends no
    /// per-module op row, so every scan it used to trigger returned empty.
    TipOnly,
    /// op rows were appended under the subscribers.
    IndexChanged,
}

impl BlockWake {
    /// Op rows are built one-for-one from dispatches (`index_block_ops`), so an
    /// empty dispatch list appends none and the index tier owes nothing.
    ///
    /// NOT `applied`, and not the explorer `record`: `applied` is false on a
    /// System-only block whose dispatches are not (see `projection.rs`, where
    /// System dispatches merge after the member loop), and `record` lands in
    /// the blocks db, which no ws topic reads.
    pub fn from_dispatches(dispatches: &[host::DispatchRecord]) -> Self {
        match dispatches.is_empty() {
            true => Self::TipOnly,
            false => Self::IndexChanged,
        }
    }
}

/// What one block wake tells a session to do.
///
/// A VALUE, not a branch taken in place: the arm that consumes it is inside a
/// `select!` over a live socket, so this is the only way the decision is
/// reachable from a test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockAction {
    /// send the tip, then re-scan the index topics.
    SweepIndex,
    /// send the tip and nothing else — the block appended no op row.
    TipOnly,
    /// the hub is gone; the session is over.
    Stop,
}

/// Decide what a block wake owes, from the wake alone. Writes nothing.
fn block_action(note: Result<BlockWake, broadcast::error::RecvError>) -> BlockAction {
    match note {
        Ok(BlockWake::IndexChanged) => BlockAction::SweepIndex,
        Ok(BlockWake::TipOnly) => BlockAction::TipOnly,
        // N WAKES WERE DROPPED AND THEIR DISCRIMINANTS WITH THEM. Any one may
        // have been `IndexChanged` and there is no way to tell which, so sweep:
        // a scan re-reads the store as it stands now, which costs a miss
        // nothing, while skipping one strands the topic until the backstop.
        Err(broadcast::error::RecvError::Lagged(_)) => BlockAction::SweepIndex,
        Err(broadcast::error::RecvError::Closed) => BlockAction::Stop,
    }
}

#[derive(Clone)]
pub struct StreamHub {
    /// block wakeups carrying whether the index tier changed — `publish_block`
    /// primes `tip` before broadcasting, so the wake always reads its own
    /// block. A `TipOnly` wake still moves every subscriber's head; it just
    /// does not send them back to the store for rows that are not there.
    blocks: broadcast::Sender<BlockWake>,
    tip: Arc<RwLock<Option<(u64, String)>>>,
    logs: LogRing,
    run_output: RunOutputRegistry,
    /// wired once at boot by a daemon that registers [`crate::NodeMetrics`], so
    /// the index sweeps this hub gates can be COUNTED. Unwired (simnode, the
    /// router tests) every record is a no-op.
    metrics: Arc<std::sync::OnceLock<crate::NodeMetrics>>,
}

impl StreamHub {
    #[cfg(test)]
    pub fn new(buffer: usize) -> Self {
        Self::with_log_ring(buffer, LogRing::default())
    }

    pub fn with_log_ring(buffer: usize, logs: LogRing) -> Self {
        let (blocks, _) = broadcast::channel(buffer);
        Self {
            blocks,
            tip: Arc::new(RwLock::new(None)),
            logs,
            run_output: RunOutputRegistry::default(),
            metrics: Arc::new(std::sync::OnceLock::new()),
        }
    }

    pub fn publish_block(&self, height: u64, root_hash: impl Into<String>, wake: BlockWake) {
        self.prime(height, root_hash);
        let _ = self.blocks.send(wake);
    }

    /// Wire the metrics registry so index sweeps are counted. Once per
    /// process, beside [`crate::StatusCell::wire_metrics`].
    pub fn wire_metrics(&self, metrics: &crate::NodeMetrics) {
        self.metrics
            .set(metrics.clone())
            .ok()
            .expect("stream hub metrics wired twice");
    }

    /// One session went back to the store, and what sent it.
    fn note_index_sweep(&self, cause: crate::metrics::SweepCause) {
        if let Some(metrics) = self.metrics.get() {
            metrics.record_index_sweep(cause);
        }
    }

    /// One snapshot topic re-composed its document for one session.
    fn note_snapshot_sample(&self, topic: crate::metrics::SnapshotTopic) {
        if let Some(metrics) = self.metrics.get() {
            metrics.record_snapshot_sample(topic);
        }
    }

    pub fn prime(&self, height: u64, root_hash: impl Into<String>) {
        *self.tip.write().expect("stream tip lock poisoned") = Some((height, root_hash.into()));
    }

    pub fn log_ring(&self) -> LogRing {
        self.logs.clone()
    }

    pub fn run_output(&self) -> RunOutputRegistry {
        self.run_output.clone()
    }

    /// one subscription to the block wake. The ws sessions ride it, and so
    /// does any node-local task that must re-read committed state once per
    /// block WITHOUT sitting on the drain's select loop.
    pub fn subscribe_blocks(&self) -> broadcast::Receiver<BlockWake> {
        self.blocks.subscribe()
    }

    fn tip(&self) -> Option<(u64, String)> {
        self.tip.read().expect("stream tip lock poisoned").clone()
    }
}

#[derive(Clone)]
pub struct LogRing {
    inner: Arc<Mutex<LogRingInner>>,
    watch: watch::Sender<u64>,
}

#[derive(Default)]
struct LogRingInner {
    next_seq: u64,
    floor_seq: u64,
    lines: VecDeque<(u64, String)>,
}

impl Default for LogRing {
    fn default() -> Self {
        let (watch, _) = watch::channel(0);
        Self {
            inner: Arc::new(Mutex::new(LogRingInner::default())),
            watch,
        }
    }
}

impl LogRing {
    pub fn push_line(&self, line: impl Into<String>) {
        let mut inner = self.inner.lock().expect("log ring lock poisoned");
        inner.next_seq += 1;
        let seq = inner.next_seq;
        inner.lines.push_back((seq, line.into()));
        while inner.lines.len() > LOG_RING_CAPACITY {
            if let Some((evicted, _)) = inner.lines.pop_front() {
                inner.floor_seq = evicted;
            }
        }
        drop(inner);
        let _ = self.watch.send(seq);
    }

    pub fn read_after(&self, seq: u64, budget: usize) -> (Vec<(u64, String)>, u64) {
        let inner = self.inner.lock().expect("log ring lock poisoned");
        let rows = inner
            .lines
            .iter()
            .filter(|(line_seq, _)| *line_seq > seq)
            .take(budget)
            .cloned()
            .collect();
        (rows, inner.floor_seq)
    }

    pub fn latest_seq(&self) -> u64 {
        *self.watch.borrow()
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.watch.subscribe()
    }
}

impl<'a> MakeWriter<'a> for LogRing {
    type Writer = LogRingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogRingWriter {
            ring: self.clone(),
            buf: Vec::new(),
        }
    }
}

pub struct LogRingWriter {
    ring: LogRing,
    buf: Vec<u8>,
}

impl LogRingWriter {
    fn push_complete_lines(&mut self) {
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let mut line = self.buf.drain(..=pos).collect::<Vec<_>>();
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.ring
                .push_line(String::from_utf8_lossy(&line).into_owned());
        }
    }

    fn flush_partial(&mut self) {
        self.push_complete_lines();
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            self.ring
                .push_line(String::from_utf8_lossy(&line).into_owned());
        }
    }
}

impl Write for LogRingWriter {
    fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
        self.buf.extend_from_slice(buf);
        self.push_complete_lines();
        Ok(buf.len())
    }

    fn flush(&mut self) -> IoResult<()> {
        self.flush_partial();
        Ok(())
    }
}

impl Drop for LogRingWriter {
    fn drop(&mut self) {
        self.flush_partial();
    }
}

// ---------------------------------------------------------------------------
// the numbering that outlives a ring entry
// ---------------------------------------------------------------------------

/// how many evicted ids keep their numbering.
///
/// Basis: a record is one `u64`, a `Copy` mark and the id string — under a
/// hundred bytes — while the ONE ring entry it stands in for holds up to
/// [`RUN_OUTPUT_MAX_LINES`] lines or a quarter megabyte of scrollback. So the
/// bound sits orders of magnitude above the entry caps it backs (32 runs, 16
/// terminal sessions): every id a node plausibly touches keeps its numbering,
/// and the cap is here only so a peer minting fresh ids cannot grow the map
/// without end.
const SEQ_MEMORY_MAX_IDS: usize = 2_048;

/// the per-id numbering that survives a whole-entry eviction.
///
/// Every ring in this crate is bounded twice: by rows within an id, and by id
/// count across the map. Shedding ROWS is the point — the bytes are
/// observational. Shedding the id's COUNTERS with them is not: the next append
/// to that still-live id would restart at seq 1, which every mirror peer
/// refuses as out-of-order and every subscribed cursor sits above forever. So
/// eviction hands the id's head here on the way out, a re-created entry
/// continues numbering from it, and while no entry exists this is what the id
/// reports as BOTH head and floor — with no rows left, everything up to the
/// head really is gone, which is what turns a stale cursor into a `Lagged`
/// frame instead of silence.
pub(crate) struct SeqMemory<M> {
    records: BTreeMap<String, SeqRecord<M>>,
    touch: u64,
}

struct SeqRecord<M> {
    /// the last seq the evicted entry stamped: its head, and — no rows having
    /// survived — its floor.
    head: u64,
    mark: M,
    remembered: u64,
}

impl<M> Default for SeqMemory<M> {
    fn default() -> Self {
        Self {
            records: BTreeMap::new(),
            touch: 0,
        }
    }
}

impl<M: Copy> SeqMemory<M> {
    /// stash an id's numbering as its entry is dropped. Oldest-first eviction
    /// past [`SEQ_MEMORY_MAX_IDS`], by the order records were remembered.
    pub(crate) fn remember(&mut self, id: &str, head: u64, mark: M) {
        self.touch += 1;
        let remembered = self.touch;
        self.records.insert(
            id.to_string(),
            SeqRecord {
                head,
                mark,
                remembered,
            },
        );
        while self.records.len() > SEQ_MEMORY_MAX_IDS {
            let Some(oldest) = self
                .records
                .iter()
                .min_by_key(|(_, record)| record.remembered)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            self.records.remove(&oldest);
        }
    }

    /// the head a re-created entry continues from, and the mark the evicted one
    /// carried.
    pub(crate) fn recall(&self, id: &str) -> Option<(u64, M)> {
        self.records
            .get(id)
            .map(|record| (record.head, record.mark))
    }

    /// what an id with no entry reports as its head and floor. `0` — a cursor
    /// of 0 is not behind — for an id this ring has never held.
    pub(crate) fn head_of(&self, id: &str) -> u64 {
        self.records.get(id).map_or(0, |record| record.head)
    }

    /// the entry owns the numbering again.
    pub(crate) fn forget(&mut self, id: &str) {
        self.records.remove(id);
    }
}

/// where a subscriber's cursor actually lands on a ring: up to `floor`
/// (everything at or below it was evicted) and down to `head` (a cursor above
/// the last stamped seq belongs to numbering this ring no longer has — a
/// restart, or an entry evicted and re-created). The catch-up path emits
/// `Lagged` whenever this moves the cursor, which is what keeps an impossible
/// cursor from waiting on rows that will never come.
pub(crate) fn resume_within(after: u64, floor: u64, head: u64) -> u64 {
    after.max(floor).min(head)
}

#[derive(Clone)]
pub struct RunOutputRegistry {
    pub(crate) controls: crate::run_control::Hub,
    inner: Arc<Mutex<RunOutputInner>>,
    watch: watch::Sender<u64>,
    appends: broadcast::Sender<RunOutputEvent>,
}

/// One provider line as it entered this node's local registry. The node's
/// agent data-plane subscribes to this feed and forwards it to peer nodes;
/// remotely ingested lines deliberately do not re-enter the feed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunOutputEvent {
    pub id: String,
    pub stream: RunStream,
    pub line: String,
}

#[derive(Default)]
struct RunOutputInner {
    version: u64,
    touch: u64,
    runs: BTreeMap<String, RunRing>,
    /// the numbering (and origin) of runs whose whole ring the cap evicted —
    /// see [`SeqMemory`]. A run that keeps printing after its ring was shed
    /// continues its seq from here instead of restarting at 1.
    memory: SeqMemory<RunOrigin>,
}

impl RunOutputInner {
    /// shed a ring's ROWS, never its numbering: the id's head and origin move
    /// to [`Self::memory`], so a run that keeps printing after the cap dropped
    /// its ring resumes its seq instead of restarting at 1.
    fn evict(&mut self, id: &str) {
        let Some(ring) = self.runs.remove(id) else {
            return;
        };
        self.memory.remember(id, ring.next_seq, ring.origin);
    }
}

/// who fed this ring its lines. A run this node hosts is never writable by a
/// peer, and the cap's eviction never sacrifices one to make room for a peer's
/// — see [`RunOutputRegistry::push`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum RunOrigin {
    Local,
    Remote,
}

struct RunRing {
    origin: RunOrigin,
    next_seq: u64,
    floor_seq: u64,
    touched: u64,
    lines: VecDeque<(u64, RunStream, String)>,
}

impl RunRing {
    fn new(origin: RunOrigin) -> Self {
        Self {
            origin,
            next_seq: 0,
            floor_seq: 0,
            touched: 0,
            lines: VecDeque::new(),
        }
    }
}

impl Default for RunOutputRegistry {
    fn default() -> Self {
        let (watch, _) = watch::channel(0);
        let (appends, _) = broadcast::channel(RUN_OUTPUT_MAX_LINES);
        Self {
            controls: crate::run_control::Hub::default(),
            inner: Arc::new(Mutex::new(RunOutputInner::default())),
            watch,
            appends,
        }
    }
}

impl RunOutputRegistry {
    pub fn output_sink(&self) -> provider_host::OutputSink {
        let registry = self.clone();
        Arc::new(move |ctx, line| {
            let Some(run_key) = ctx.run_key.as_deref() else {
                return;
            };
            let stream = match line.stream {
                provider_host::OutputStream::Stdout => RunStream::Stdout,
                provider_host::OutputStream::Stderr => RunStream::Stderr,
            };
            registry.append(run_key, stream, line.line);
        })
    }

    pub fn append(&self, id: impl Into<String>, stream: RunStream, line: impl Into<String>) {
        self.push(id.into(), stream, line.into(), RunOrigin::Local);
    }

    /// Add a line received from another node without broadcasting it again.
    /// Refused (`false`) when `event.id` names a run this node hosts locally,
    /// or when admitting a never-seen remote id would have to evict a local
    /// ring to fit under [`RUN_OUTPUT_MAX_RUNS`] — see [`Self::push`].
    #[must_use]
    pub fn append_remote(&self, event: RunOutputEvent) -> bool {
        self.push(event.id, event.stream, event.line, RunOrigin::Remote)
    }

    /// whether `id` names a run this node hosts locally. Checked by the agent
    /// data plane before it even spends a peer's remote-binding budget on the
    /// id: unlike a term session's 16-hex random id, a run id is consensus
    /// state — every member can learn every hosted run's id — so "unseen id"
    /// is not a signal a peer could not have forged for a run it does not own.
    pub fn is_local(&self, id: &str) -> bool {
        self.inner
            .lock()
            .expect("run output lock poisoned")
            .runs
            .get(id)
            .is_some_and(|ring| ring.origin == RunOrigin::Local)
    }

    /// `origin` decides who this line may come from and how the run-count cap
    /// is enforced:
    /// - `Local` (this node's own provider output): always admitted, and
    ///   growing past [`RUN_OUTPUT_MAX_RUNS`] evicts the globally
    ///   least-recently-touched OTHER ring, local or remote — this node's own
    ///   work always wins a slot.
    /// - `Remote` (a peer's line): refused outright against an existing
    ///   `Local` ring — a peer never appends to, or evicts, a run this node
    ///   hosts. A brand-new remote id at the cap evicts only the
    ///   least-recently-touched REMOTE ring; with none to evict (every held
    ///   ring is local), the line is refused rather than growing past the cap.
    ///
    /// Returns whether the line was admitted.
    fn push(&self, id: String, stream: RunStream, line: String, origin: RunOrigin) -> bool {
        let mut inner = self.inner.lock().expect("run output lock poisoned");
        let held = inner.runs.get(&id).map(|ring| ring.origin);
        // the Local/Remote mark outlives the rows: a local run whose ring the
        // cap shed is still a run this node hosts, and a peer may no more claim
        // it after the eviction than before.
        let known = held.or_else(|| inner.memory.recall(&id).map(|(_, mark)| mark));
        // the cap counts RINGS, so only an id holding none can grow the map.
        let needs_slot = held.is_none() && inner.runs.len() >= RUN_OUTPUT_MAX_RUNS;
        match (origin, known) {
            (RunOrigin::Remote, Some(RunOrigin::Local)) => return false,
            (RunOrigin::Remote, _) if needs_slot => {
                let victim = inner
                    .runs
                    .iter()
                    .filter(|(_, ring)| ring.origin == RunOrigin::Remote)
                    .min_by_key(|(_, ring)| ring.touched)
                    .map(|(run_id, _)| run_id.clone());
                match victim {
                    Some(victim) => inner.evict(&victim),
                    None => return false,
                }
            }
            _ => {}
        }
        // a re-created ring continues the evicted one's numbering; with every
        // row gone, that head is also its floor.
        let restored = held
            .is_none()
            .then(|| inner.memory.recall(&id))
            .flatten()
            .map(|(head, _)| head);
        inner.version += 1;
        inner.touch += 1;
        let version = inner.version;
        let touch = inner.touch;
        let ring = inner
            .runs
            .entry(id.clone())
            .or_insert_with(|| RunRing::new(origin));
        if let Some(head) = restored {
            ring.next_seq = head;
            ring.floor_seq = head;
        }
        ring.touched = touch;
        ring.next_seq += 1;
        let seq = ring.next_seq;
        ring.lines.push_back((seq, stream, line.clone()));
        while ring.lines.len() > RUN_OUTPUT_MAX_LINES {
            if let Some((evicted, _, _)) = ring.lines.pop_front() {
                ring.floor_seq = evicted;
            }
        }
        if origin == RunOrigin::Local {
            while inner.runs.len() > RUN_OUTPUT_MAX_RUNS {
                let Some(victim) = inner
                    .runs
                    .iter()
                    .filter(|(run_id, _)| *run_id != &id)
                    .min_by_key(|(_, ring)| ring.touched)
                    .map(|(run_id, _)| run_id.clone())
                else {
                    break;
                };
                inner.evict(&victim);
            }
        }
        if restored.is_some() {
            inner.memory.forget(&id);
        }
        drop(inner);
        let _ = self.watch.send(version);
        if origin == RunOrigin::Local {
            let _ = self.appends.send(RunOutputEvent { id, stream, line });
        }
        true
    }

    pub fn read_after(
        &self,
        id: &str,
        seq: u64,
        budget: usize,
    ) -> (Vec<(u64, RunStream, String)>, u64) {
        let mut inner = self.inner.lock().expect("run output lock poisoned");
        inner.touch += 1;
        let touch = inner.touch;
        let Some(ring) = inner.runs.get_mut(id) else {
            // no ring, but the numbering may have outlived it: report the
            // evicted head as the floor, since every row up to it is gone.
            return (Vec::new(), inner.memory.head_of(id));
        };
        ring.touched = touch;
        let rows = ring
            .lines
            .iter()
            .filter(|(line_seq, _, _)| *line_seq > seq)
            .take(budget)
            .cloned()
            .collect();
        (rows, ring.floor_seq)
    }

    /// where a subscriber's cursor lands on this run's ring — see
    /// [`resume_within`]. The catch-up path `Lagged`s whenever it moves.
    pub fn resume_cursor(&self, id: &str, after: u64) -> u64 {
        let inner = self.inner.lock().expect("run output lock poisoned");
        let Some(ring) = inner.runs.get(id) else {
            return inner.memory.head_of(id);
        };
        resume_within(after, ring.floor_seq, ring.next_seq)
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.watch.subscribe()
    }

    pub fn subscribe_appends(&self) -> broadcast::Receiver<RunOutputEvent> {
        self.appends.subscribe()
    }
}

#[derive(Clone, Debug)]
enum TopicState {
    Module {
        module: String,
        cursor: String,
    },
    FilesWatch {
        cursor: String,
    },
    Logs {
        seq: u64,
    },
    RunOutput {
        id: String,
        seq: u64,
    },
    /// a SNAPSHOT topic: each wakeup re-samples the whole exposition, so the
    /// cursor (the last sample's `time_ms`) is bookkeeping, never a resume
    /// point — there is no backlog to replay and the topic never lags.
    Metrics {
        sampled_ms: u64,
    },
    /// the peer-sample SNAPSHOT topic, same contract as [`Self::Metrics`]:
    /// every wakeup re-composes the whole sample, so the cursor is bookkeeping
    /// and the topic never lags.
    Peers {
        sampled_ms: u64,
    },
    /// the node-status SNAPSHOT topic, same contract again.
    Status {
        sampled_ms: u64,
    },
}

impl TopicState {
    fn cursor(&self) -> String {
        match self {
            Self::Module { cursor, .. } | Self::FilesWatch { cursor } => cursor.clone(),
            Self::Logs { seq } | Self::RunOutput { seq, .. } => seq.to_string(),
            Self::Metrics { sampled_ms }
            | Self::Peers { sampled_ms }
            | Self::Status { sampled_ms } => sampled_ms.to_string(),
        }
    }
}

struct CatchUpResult {
    frames: Vec<ServerFrame>,
    drop_topic: bool,
}

impl CatchUpResult {
    fn keep(frames: Vec<ServerFrame>) -> Self {
        Self {
            frames,
            drop_topic: false,
        }
    }

    fn drop(frames: Vec<ServerFrame>) -> Self {
        Self {
            frames,
            drop_topic: true,
        }
    }
}

/// which wakeup source fired — each catch-up pass only visits the topic
/// classes that source can have fed, so a log-line storm never re-scans
/// module topics and a run-output append never touches the index. `All`
/// covers subscribe replay, where any topic may owe frames. `Tick` is the
/// heartbeat interval — the cadence of snapshot topics (metrics), which
/// re-sample on time rather than on any append.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wake {
    Block,
    Logs,
    RunOutput,
    Tick,
    All,
}

impl TopicState {
    fn wakes_on(&self, wake: Wake) -> bool {
        match wake {
            Wake::All => true,
            Wake::Block => matches!(self, Self::Module { .. } | Self::FilesWatch { .. }),
            Wake::Logs => matches!(self, Self::Logs { .. }),
            Wake::RunOutput => matches!(self, Self::RunOutput { .. }),
            Wake::Tick => matches!(
                self,
                Self::Metrics { .. } | Self::Peers { .. } | Self::Status { .. }
            ),
        }
    }
}

/// Serve one ws connection.
///
/// `reader_of` and `operator` are the capabilities this socket may have been
/// given before it existed: the dispatch id whose output ring the caller
/// proved it may read ([`admit_run_reader`]), and whether the upgrade proved
/// this node's operator ([`crate::signed_req::upgrade_is_operator`]). Both are
/// set at the upgrade and never change, so a connection cannot talk its way
/// into another run's output, or the operator's topics, mid-session.
pub async fn stream_session(
    mut socket: WebSocket,
    handle: NodeHandle,
    reader_of: Option<String>,
    operator: bool,
) {
    let hub = handle.stream_hub();
    let mut block_rx = hub.subscribe_blocks();
    let mut log_rx = hub.log_ring().subscribe();
    let mut run_rx = hub.run_output().subscribe();
    let mut heartbeat = tokio::time::interval(Duration::from_millis(HEARTBEAT_INTERVAL_MS));
    // FIRST TICK AFTER ONE PERIOD, not immediately: `interval` fires at once,
    // and a session that has just run its subscribe-time `Wake::All` owes
    // nothing yet. Starting late also makes the sweep counter mean what it says
    // over a short window.
    let mut index_backstop = tokio::time::interval_at(
        tokio::time::Instant::now() + INDEX_BACKSTOP_INTERVAL,
        INDEX_BACKSTOP_INTERVAL,
    );
    let mut topics = BTreeMap::new();
    // set once, by a `ServiceAttach` that this node accepts. The guard's Drop —
    // on every `return` below, and on the task being cancelled — releases the
    // link, so the next daemon can take it.
    let mut attached: Option<crate::service_link::AttachGuard> = None;
    let mut service_rx: Option<mpsc::Receiver<agent_service::wire::Command>> = None;
    let controls_changed = hub.run_output().controls.changed.clone();
    let mut worker: Option<crate::run_control::Worker> = None;
    let mut control_rx: Option<mpsc::Receiver<crate::run_control::Command>> = None;

    loop {
        tokio::select! {
            _ = controls_changed.notified() => {
                if !catch_up(&handle, &mut socket, &mut topics, Wake::RunOutput).await { return; }
            }
            frame = socket.next() => {
                let Some(frame) = frame else { return };
                match frame {
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<ClientMsg>(text.as_str()) {
                            // a compute daemon's live run tail: append to the
                            // same ring the in-process sink used to feed, so
                            // `run-output:<id>` subscribers cannot tell which
                            // process produced the line.
                            Ok(ClientMsg::ComputeAttach { token }) => {
                                let authorized = worker.is_none() && handle.workspace_secret_matches(&token);
                                if !authorized { return; }
                                let (attached, receiver) = hub.run_output().controls.attach();
                                worker = Some(attached); control_rx = Some(receiver);
                            }
                            Ok(ClientMsg::RunControlReply { id, result }) => {
                                if let Some(worker) = &worker { worker.reply(id,result); }
                            }
                            // a run's output is the compute daemon's to
                            // publish, on the connection it attached.
                            Ok(ClientMsg::RunOutput { id, stream, line }) => match &worker {
                                Some(worker) => {
                                    worker.observe(&id, &line);
                                    handle_run_output(&hub, id, stream, line);
                                }
                                None => {
                                    if !send_frame(&mut socket, unattached_run_output()).await {
                                        return;
                                    }
                                }
                            },
                            // a service daemon claiming this connection as its
                            // command link, and the events it publishes back.
                            Ok(ClientMsg::ServiceAttach { kind, token }) => {
                                match take_service_link(&handle, &kind, &token) {
                                    Ok((guard, rx)) => {
                                        attached = Some(guard);
                                        service_rx = Some(rx);
                                    }
                                    Err(reason) => {
                                        if !send_frame(&mut socket, ServerFrame::Error {
                                            topic: String::new(),
                                            code: StreamErrorCode::Unavailable,
                                            detail: reason.to_string(),
                                        }).await {
                                            return;
                                        }
                                    }
                                }
                            }
                            Ok(ClientMsg::AgentEvent { event }) => {
                                handle_agent_event(&handle, attached.is_some(), event);
                            }
                            Ok(msg) => {
                                let frames = handle_client_msg(
                                    &handle,
                                    &mut topics,
                                    reader_of.as_deref(),
                                    operator,
                                    msg,
                                );
                                if !send_frames(&mut socket, frames).await {
                                    return;
                                }
                                if !catch_up(&handle, &mut socket, &mut topics, Wake::All).await {
                                    return;
                                }
                            }
                            Err(err) => {
                                if !send_frame(&mut socket, ServerFrame::Error {
                                    topic: String::new(),
                                    code: StreamErrorCode::BadFrame,
                                    detail: err.to_string(),
                                }).await {
                                    return;
                                }
                            }
                        }
                    }
                    Ok(Message::Binary(_)) | Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
                    Ok(Message::Close(_)) | Err(_) => return,
                }
            }
            note = block_rx.recv() => {
                let sweep = match block_action(note) {
                    BlockAction::Stop => return,
                    BlockAction::SweepIndex => true,
                    BlockAction::TipOnly => false,
                };
                // the tip rides every block wake — nop fillers included, which
                // feed no topic — so a console's height ticks per block instead
                // of waiting out the timer beat below (the idle/stall floor).
                // NOT gated on `sweep`: gating it here re-freezes the head on an
                // idle chain, which is the bug #1021 fixed.
                if !send_frame(&mut socket, heartbeat_frame(&hub)).await {
                    return;
                }
                // An idle block appended no op row, so every scan it used to
                // trigger read the store and found nothing.
                if sweep {
                    hub.note_index_sweep(crate::metrics::SweepCause::Block);
                    if !catch_up(&handle, &mut socket, &mut topics, Wake::Block).await {
                        return;
                    }
                }
            }
            _ = index_backstop.tick() => {
                // see `INDEX_BACKSTOP_INTERVAL`: the floor under every writer
                // that appends rows and tells nobody.
                hub.note_index_sweep(crate::metrics::SweepCause::Backstop);
                if !catch_up(&handle, &mut socket, &mut topics, Wake::Block).await {
                    return;
                }
            }
            _ = heartbeat.tick() => {
                if !send_frame(&mut socket, heartbeat_frame(&hub)).await {
                    return;
                }
                if !catch_up(&handle, &mut socket, &mut topics, Wake::Tick).await {
                    return;
                }
            }
            changed = log_rx.changed() => {
                if changed.is_err() {
                    return;
                }
                if !catch_up(&handle, &mut socket, &mut topics, Wake::Logs).await {
                    return;
                }
            }
            changed = run_rx.changed() => {
                if changed.is_err() {
                    return;
                }
                if !catch_up(&handle, &mut socket, &mut topics, Wake::RunOutput).await {
                    return;
                }
            }
            // the service link's outbound half. Inert on every connection that
            // is not the attached daemon (see `next_service_command`).
            //
            // BOUNDED, and it is the only write here that is: every other frame
            // on this loop goes to a subscriber, but this one goes to another
            // PROCESS that is simultaneously writing events back. If the daemon
            // ever stopped reading, an unbounded await here would stop this loop
            // reading its events, and the two blocked writes would deadlock with
            // nothing to break them — taking the whole interactive plane with
            // them, permanently. A daemon that cannot accept a command in this
            // long is wedged; dropping the link ends its sessions cleanly
            // (`AttachGuard`) and lets it redial.
            command = async {
                match &mut control_rx {
                    Some(receiver) => receiver.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let Some(command) = command else { return; };
                let frame = serde_json::json!({"type":"run_control","command":command});
                if socket.send(Message::Text(frame.to_string().into())).await.is_err() { return; }
            }
            command = next_service_command(&mut service_rx) => {
                let sent = tokio::time::timeout(
                    SERVICE_COMMAND_WRITE_TIMEOUT,
                    send_frame(&mut socket, ServerFrame::ServiceCommand { command }),
                )
                .await;
                let Ok(true) = sent else {
                    tracing::warn!(
                        target: "ducktape::service",
                        reason = "service_link_write_stalled",
                        "dropping the agent service link"
                    );
                    return;
                };
            }
        }
    }
}

/// the attached daemon's next command, or never.
///
/// A connection that holds no service link must not make this arm ready — it
/// would spin the select loop — so it parks forever instead. Same for a link
/// whose sender the bridge has already dropped: the guard tidies up when this
/// socket closes, and until then there is nothing to send.
async fn next_service_command(
    rx: &mut Option<mpsc::Receiver<agent_service::wire::Command>>,
) -> agent_service::wire::Command {
    let Some(rx) = rx else {
        return std::future::pending().await;
    };
    match rx.recv().await {
        Some(command) => command,
        None => std::future::pending().await,
    }
}

/// Admit a service daemon's claim on this connection, or name why not.
///
/// Two refusals, each a stable reason an operator can act on: a kind this node
/// hosts no plane for, and a link another daemon already holds.
///
/// Build equality is NOT one of them. It authenticated nobody (a stamp is
/// compiled into a binary any local process can read), it excluded every
/// separately-compiled daemon by construction, and its `None` case refused
/// every link on any build without `.git`.
///
/// Skew is caught per FRAME instead, and that is a claim with an enforcement
/// site rather than a hope: [`ClientMsg`] and every `agent_service::wire` type
/// carry `deny_unknown_fields` and default nothing, so a field this build does
/// not know is refused by name and a field it does know cannot go missing. A
/// connection-wide version check would refuse frames this node understands
/// perfectly; the per-frame check refuses exactly the ones it does not.
///
/// The strictness is ONE-DIRECTIONAL, and saying otherwise would be the same
/// overclaim the deleted gate's justification made. Daemon→node is the clean
/// half: an undecodable frame earns this connection a `BadFrame` naming the
/// field and the socket stays open. Node→daemon only DROPS — the daemon warns
/// `malformed_command` and the node's create waits on a reply that never
/// arrives. That gap and its fix live at the drop site,
/// `bin/node/src/agent/link.rs`'s `classify`.
fn take_service_link(
    handle: &NodeHandle,
    kind: &str,
    token: &str,
) -> Result<
    (
        crate::service_link::AttachGuard,
        mpsc::Receiver<agent_service::wire::Command>,
    ),
    &'static str,
> {
    if kind != crate::services::AGENT_KIND {
        return Err("only the agent service has a command link on this node");
    }
    let link = handle
        .service_link()
        .ok_or("the agent service link is not enabled on this node")?;
    link.attach(token).ok_or(
        "refused: present this node's service-link token, and only one agent service may attach",
    )
}

/// every `ducktape::service` refusal below that a CLIENT drives per frame (a
/// publisher hammering an unattached connection), latched by reason. Unlatched,
/// it repeats at whatever rate the client sends frames and evicts the whole
/// 4096-line ring. First occurrence, then every 100th, carrying `occurrences`;
/// the counter is the diagnosis.
static LINK_WARN: crate::log::Latch = crate::log::Latch::new(100);

/// Apply one daemon-published event to the service link, or drop it.
///
/// The `attached` gate is a trust boundary, not tidiness: a receipt reaches the
/// collaboration pump, which acknowledges it on-chain, so an unattached
/// connection publishing one would be speaking for another member's daemon.
fn handle_agent_event(handle: &NodeHandle, attached: bool, event: agent_service::wire::Event) {
    if !attached {
        if let Some(occurrences) = LINK_WARN.hit("unattached_publisher") {
            tracing::warn!(
                target: "ducktape::service",
                reason = "unattached_publisher",
                occurrences,
                "agent event dropped"
            );
        }
        return;
    }
    let Some(link) = handle.service_link() else {
        if let Some(occurrences) = LINK_WARN.hit("no_service_link") {
            tracing::warn!(
                target: "ducktape::service",
                reason = "no_service_link",
                occurrences,
                "agent event dropped"
            );
        }
        return;
    };
    link.on_event(event);
}

fn handle_client_msg(
    handle: &NodeHandle,
    topics: &mut BTreeMap<String, TopicState>,
    reader_of: Option<&str>,
    operator: bool,
    msg: ClientMsg,
) -> Vec<ServerFrame> {
    match msg {
        ClientMsg::Subscribe {
            topics: requested,
            resume,
            token,
        } => subscribe_topics(
            handle,
            topics,
            requested,
            &resume,
            token.as_deref(),
            reader_of,
            operator,
        ),
        ClientMsg::Unsubscribe { topics: requested } => {
            for topic in requested {
                topics.remove(&topic);
            }
            Vec::new()
        }
        // handled inline in `stream_session` (they act on the session manager,
        // off this connection's topic set), so they never reach here — but the
        // match stays exhaustive.
        ClientMsg::ComputeAttach { .. }
        | ClientMsg::RunControlReply { .. }
        | ClientMsg::RunOutput { .. }
        | ClientMsg::ServiceAttach { .. }
        | ClientMsg::AgentEvent { .. } => Vec::new(),
    }
}

/// every `ducktape::agent` refusal below, latched by reason: a compute daemon
/// publishes one `RunOutput` frame per line of its run, so a malformed id or
/// an oversized line repeats at the daemon's own output rate. First
/// occurrence, then every 100th, carrying `occurrences`.
static AGENT_WARN: crate::log::Latch = crate::log::Latch::new(100);

/// Refuse one run-output line from a connection that never took the compute
/// attachment: the frame back to the caller, and a latched `warn` — the
/// sender repeats at its own line rate. See [`ClientMsg::RunOutput`].
fn unattached_run_output() -> ServerFrame {
    if let Some(occurrences) = AGENT_WARN.hit("unattached_publisher") {
        tracing::warn!(
            target: "ducktape::agent",
            reason = "unattached_publisher",
            occurrences,
            "run output dropped"
        );
    }
    ServerFrame::Error {
        topic: String::new(),
        code: StreamErrorCode::Forbidden,
        detail: "run output is published by this node's compute daemon — send \
                 `compute_attach` with the node's service-link token first"
            .into(),
    }
}

/// Admit one published run-output line, or drop it with a named reason.
///
/// The two checks are a trust boundary, not tidiness: see [`ClientMsg::RunOutput`].
/// Dropping is deliberate — a malformed line is not worth closing an otherwise
/// healthy publisher's connection over, and the `warn` carries the counter.
fn handle_run_output(hub: &StreamHub, id: String, stream: RunStream, line: String) {
    let id_well_formed =
        id.len() == RUN_OUTPUT_ID_LEN && id.bytes().all(|byte| byte.is_ascii_hexdigit());
    if !id_well_formed {
        if let Some(occurrences) = AGENT_WARN.hit("malformed_run_id") {
            tracing::warn!(
                target: "ducktape::agent",
                reason = "malformed_run_id",
                occurrences,
                "run output dropped"
            );
        }
        return;
    }
    if line.len() > MAX_RUN_OUTPUT_LINE {
        if let Some(occurrences) = AGENT_WARN.hit("run_output_line_too_long") {
            tracing::warn!(
                target: "ducktape::agent",
                bytes = line.len(),
                reason = "run_output_line_too_long",
                occurrences,
                "run output dropped"
            );
        }
        return;
    }
    hub.run_output().append(id, stream, line);
}

fn subscribe_topics(
    handle: &NodeHandle,
    states: &mut BTreeMap<String, TopicState>,
    requested: Vec<String>,
    resume: &BTreeMap<String, String>,
    token: Option<&str>,
    reader_of: Option<&str>,
    operator: bool,
) -> Vec<ServerFrame> {
    // No caller ever legitimately needs more names in ONE message than the
    // connection may ever hold: at most `MAX_TOPICS_PER_CONNECTION` states
    // exist, so a request past it is either a mistake or a fan-out attempt
    // (a 64 MiB frame naming millions of names, each turned into its own
    // refusal `ServerFrame` before this used to look at the cap at all). Stop
    // BEFORE the per-topic loop runs — one frame, sized by the request, not
    // by `requested.len()`.
    if requested.len() > MAX_TOPICS_PER_CONNECTION {
        let requested_count = requested.len();
        return vec![unavailable(
            "",
            format!(
                "subscribe named {requested_count} topics, over the \
                 {MAX_TOPICS_PER_CONNECTION}-topic connection cap; split the request"
            ),
        )];
    }
    let store = handle.stream_index();
    // ONE constant-time compare per frame, not per topic: the secret is
    // connection-wide, so this is both the cheapest place to spend it and the
    // only place the presented bytes are touched at all.
    let holds_workspace_secret = token.is_some_and(|token| handle.workspace_secret_matches(token));
    let mut frames = Vec::new();
    let mut accepted = BTreeMap::new();
    for topic in requested {
        // the cap counts a NEW topic only — re-subscribing (re-cursoring) an
        // existing one is always allowed.
        if !states.contains_key(&topic) && states.len() >= MAX_TOPICS_PER_CONNECTION {
            frames.push(unavailable(
                &topic,
                format!("subscription cap ({MAX_TOPICS_PER_CONNECTION} topics) reached"),
            ));
            continue;
        }
        match prepare_topic(
            &topic,
            holds_workspace_secret,
            reader_of,
            operator,
            resume.get(&topic),
            store.as_ref(),
        ) {
            Ok((state, lagged)) => {
                accepted.insert(topic.clone(), state.cursor());
                states.insert(topic, state);
                if let Some(frame) = lagged {
                    frames.push(frame);
                }
            }
            Err(frame) => frames.push(frame),
        }
    }
    frames.push(ServerFrame::Subscribed { topics: accepted });
    frames
}

/// the `files` module's live-change topic, spelled once.
const FILES_WATCH_TOPIC: &str = "files:watch";
/// the log-ring tail topic.
const LOGS_TOPIC: &str = "logs";
/// the metrics-exposition snapshot topic.
const METRICS_TOPIC: &str = "metrics";
/// How recently a snapshot topic must have sampled for the next wakeup to be
/// a no-op.
///
/// A subscriber used to be sampled TWICE within milliseconds: the subscribe's
/// `Wake::All`, and then the heartbeat's first tick, which fires at once
/// (`tokio::time::interval`, unlike `index_backstop`'s `interval_at`). For
/// `peers` that is two whole-registry encodes — 485 KB and ~10 ms each — for
/// one subscribe, the second carrying nothing the first did not.
///
/// HALF THE BEAT, and both bounds are load-bearing.
///
/// The lower bound is the subscribe-to-first-tick gap, which is milliseconds.
///
/// The upper bound is COMPOSE LATENCY, and it is the subtler of the two. The
/// stamp is written AFTER the document is built, so every sample lands L ms
/// past the tick that asked for it, and every following tick is therefore only
/// `interval - L` old. A window at or above `interval - L` treats that as too
/// soon, skips, and the topic delivers every OTHER beat — permanently, not as
/// a phase artefact that corrects itself. Measured L on an idle single-node
/// daemon is ~40 ms, so a 1500 ms window leaves the whole of the rest of the
/// beat as margin; L would have to reach 1.5 s to halve the cadence.
///
/// L grows with the registry, which is why the bound is pinned by a test
/// rather than left to the constant looking obviously small.
const SNAPSHOT_MIN_INTERVAL_MS: u64 = HEARTBEAT_INTERVAL_MS / 2;

/// the direct-peer snapshot topic — the same sample `GET /v1/peers` composes.
const PEERS_TOPIC: &str = "peers";
/// the node-status snapshot topic — the same projection `GET /v1/status` serves.
const STATUS_TOPIC: &str = "status";
const MODULE_PREFIX: &str = "module:";
const RUN_OUTPUT_PREFIX: &str = "run-output:";

/// every topic family this node serves, parsed from the wire name exactly once.
///
/// ONE tagged value so admission is ONE `match` with no `_` arm ([`Self::admission`]):
/// a family added later cannot compile until that match names it, which is what
/// makes "deny by default" a build error rather than a habit. The prefix ladder
/// in [`Self::parse`] is not the decision — it is the parse, and a `&str` is not
/// a closed set; every decision downstream of it branches on this enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Topic<'a> {
    /// every committed op of one indexed module, decoded.
    Module(&'a str),
    /// the same, pinned to `files` and projected as path changes.
    FilesWatch,
    /// this node's 4096-line log ring.
    Logs,
    /// one run's stdout/stderr tail.
    RunOutput(&'a str),
    /// the Prometheus exposition, re-sampled per heartbeat.
    Metrics,
    /// the direct-peer sample, re-sampled per heartbeat.
    Peers,
    /// the node-status projection, re-sampled per heartbeat.
    Status,
}

/// what a caller must have proved to hold a topic handle.
///
/// Every value here has a MECHANISM behind it — a name without one would be a
/// lattice pretending to be a gate. The ws surface has three pieces of evidence
/// about a caller: whether it can read this node's workspace, whether its
/// upgrade proved this node's operator, and, for a run's output only, whether
/// it signed the upgrade as that run's creator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admission<'a> {
    /// nothing. The same bytes already leave this node over an HTTP route with
    /// no gate on it, so a check here would refuse an honest client and stop
    /// nobody.
    Public,
    /// this node's OPERATOR, proved at the upgrade
    /// ([`crate::signed_req::upgrade_is_operator`]): the operator credential
    /// from a loopback peer, or a signature by the operator key — the same two
    /// proofs `POST /v1/log-filter` takes to retune what feeds the ring.
    Operator,
    /// ONE run's output ring: the workspace secret, or an upgrade signed by the
    /// key that CREATED this dispatch (`?run=<id>`, admitted in
    /// [`admit_run_reader`] before the socket exists).
    ///
    /// The id is carried in the value, not checked against a flag, because the
    /// capability names one dispatch: a connection admitted for one run must not
    /// read another's, and a `bool` could not say which.
    Run(&'a str),
}

impl<'a> Topic<'a> {
    /// Parse a wire topic name, or `None` for a name no family owns.
    ///
    /// `None` is a refusal, not a fallthrough: an unparsed name reaches no
    /// `TopicState` and so hands the connection nothing.
    fn parse(name: &'a str) -> Option<Self> {
        if let Some(module) = name.strip_prefix(MODULE_PREFIX) {
            return Some(Self::Module(module));
        }
        if let Some(id) = name.strip_prefix(RUN_OUTPUT_PREFIX) {
            return Some(Self::RunOutput(id));
        }
        match name {
            FILES_WATCH_TOPIC => Some(Self::FilesWatch),
            LOGS_TOPIC => Some(Self::Logs),
            METRICS_TOPIC => Some(Self::Metrics),
            PEERS_TOPIC => Some(Self::Peers),
            STATUS_TOPIC => Some(Self::Status),
            _ => None,
        }
    }

    /// What this family costs to hold. The whole authorization decision, in one
    /// place, with every family named.
    ///
    /// The public families are public because gating them would be theater: an
    /// `Origin`-less caller already reads the identical bytes over
    /// `POST /v1/query` + `GET /v1/index/{module}/{ops,scan}` (`Module`,
    /// `FilesWatch`), `GET /metrics` (`Metrics`), `GET /v1/peers` (`Peers`) and
    /// `GET /v1/status` (`Status`).
    ///
    /// `Logs` has no open twin: the ring is this operator's process log, and its
    /// HTTP twin (`GET /v1/admin/logs/tail`) is operator-gated, so the ws topic
    /// takes the operator's proofs too — the app's Logs tab presents the one it
    /// already signs `POST /v1/log-filter` with.
    ///
    /// A run's stdout carries provider bytes with no unauthenticated HTTP twin
    /// at all, so it is gated — and a caller can reach it WITHOUT the workspace
    /// only for a run it created: a remote app is the device that asked for the
    /// run, and refusing it the progress of its own work made the feature
    /// local-only. It is still not public — see [`Admission::Run`].
    fn admission(self) -> Admission<'a> {
        match self {
            Self::Module(_) => Admission::Public,
            Self::FilesWatch => Admission::Public,
            Self::Logs => Admission::Operator,
            Self::Metrics => Admission::Public,
            Self::Peers => Admission::Public,
            Self::Status => Admission::Public,
            Self::RunOutput(id) => Admission::Run(id),
        }
    }
}

/// Why a subscribe was refused. Typed, mirroring [`crate::services::HelloRefusal`]:
/// the stable snake_case `reason` and the wire code are derived from the
/// variant, so a typo cannot silently downgrade a refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TopicRefusal {
    /// no family owns this name.
    UnknownFamily,
    /// the family is real but names a module this node does not index. A
    /// separate variant from [`Self::UnknownFamily`] because the two send an
    /// operator to different places — a typo in the topic grammar, versus a
    /// module absent from THIS node's genesis set — and one token covering both
    /// would be uncountable.
    UnknownModule,
    /// an operator family ([`Admission::Operator`]) asked for on a connection
    /// whose upgrade did not prove this node's operator.
    NotOperator,
    /// a run's output ring, asked for by a connection that neither holds the
    /// workspace nor was admitted as this run's creator. Its own token because
    /// it sends the caller somewhere else entirely — sign the upgrade — and a
    /// count of these is a count of remote readers reaching for runs that are
    /// not theirs.
    NotThisRunsReader,
}

impl TopicRefusal {
    /// the stable snake_case token — greppable, countable, never prose.
    fn reason(self) -> &'static str {
        match self {
            Self::UnknownFamily => "unknown_topic",
            Self::UnknownModule => "unknown_module",
            Self::NotOperator => "not_operator",
            Self::NotThisRunsReader => "not_this_runs_reader",
        }
    }

    fn code(self) -> StreamErrorCode {
        match self {
            Self::UnknownFamily | Self::UnknownModule => StreamErrorCode::UnknownTopic,
            Self::NotOperator | Self::NotThisRunsReader => StreamErrorCode::Forbidden,
        }
    }

    /// the caller-facing sentence. It names what the caller must PRESENT and
    /// never what this node EXPECTS: echoing the secret into a refusal body is
    /// a bug this repo has already shipped once.
    ///
    /// `&'static str` is that guarantee, structurally — there is no formatting
    /// site here for a secret to reach.
    fn detail(self) -> &'static str {
        match self {
            Self::UnknownFamily => "unknown stream topic",
            Self::UnknownModule => "this node indexes no such module",
            Self::NotOperator => {
                "this topic is the node operator's — open `/v1/ws` with the operator \
                 credential (x-ducktape-admin-token, from the node's own host) or \
                 signed by the operator key"
            }
            Self::NotThisRunsReader => {
                "run output requires the workspace token, the requester, or its program \
                 controller — open `/v1/ws?run=<dispatch>` signed by an authorized key"
            }
        }
    }
}

/// Refuse one topic: the wire frame back to the caller, and one `debug` line.
///
/// `debug`, not `warn`, and for the reason `crate::admin`'s `refuse` already
/// documents: a refusal is per-request and any local process can drive one in a
/// loop, so an unconditional `warn!` is a log-ring DoS that evicts the evidence
/// around whatever you were hunting. The topic NAME never reaches the log — it
/// carries a session id — while the frame does, because that is the caller's own
/// input going back to the caller.
fn refuse_topic(topic: &str, refusal: TopicRefusal) -> ServerFrame {
    tracing::debug!(
        target: "ducktape::stream",
        reason = refusal.reason(),
        "topic subscribe refused"
    );
    ServerFrame::Error {
        topic: topic.to_string(),
        code: refusal.code(),
        detail: refusal.detail().into(),
    }
}

/// An external requester proves its exact key. A program requester has no
/// key: its current controller's key-held account may read and steer its runs.
/// Module/system origins grant no interactive authority. Both the live and
/// settled paths resolve the same committed identity records.
pub(crate) async fn run_reader(
    handle: &NodeHandle,
    requester: &sdk::Origin,
    key: &[u8],
) -> Result<bool, String> {
    match requester {
        sdk::Origin::External(id) => Ok(!id.is_empty() && id == key),
        sdk::Origin::Program(number) => program_run_reader(handle, *number, key).await,
        sdk::Origin::Module(_) | sdk::Origin::System => Ok(false),
    }
}

async fn program_run_reader(handle: &NodeHandle, number: u64, key: &[u8]) -> Result<bool, String> {
    let (reply, rx) = futures::channel::oneshot::channel();
    handle
        .send(crate::NodeCommand::Query {
            target: "identity".into(),
            req: identity::encode_query(&identity::IdentityQuery::Get { number }),
            reply,
        })
        .await
        .map_err(|_| "actor gone".to_string())?;
    let bytes = rx
        .await
        .map_err(|_| "reply dropped".to_string())?
        .map_err(|refused| refused.message)?;
    let identity::IdentityReply::Account(account) = identity::decode_reply(&bytes)? else {
        return Err("unexpected identity reply".into());
    };
    let Some(identity::AccountView {
        control: identity::Control::Program { controller, .. },
        ..
    }) = account
    else {
        return Ok(false);
    };
    let reader = crate::handle::account_of_key(handle, key.to_vec()).await?;
    Ok(reader == Some(controller))
}

/// Admit a `/v1/ws?run=<dispatch>` upgrade as a run reader, or answer the
/// refusal to send instead.
///
/// Two steps, in this order, because the cheap one is the one that must not be
/// skipped: the signature over `GET` + this exact path+query + an empty body
/// (the data-plane trio, carried as headers so the proof never enters a query
/// string or a log), then committed run and identity reads resolving the
/// requester or its current program controller.
///
/// Decided BEFORE the socket exists, which is what keeps
/// [`subscribe_topics`] synchronous: the committed read happens once per
/// connection, never per subscribe frame.
pub(crate) async fn admit_run_reader(
    handle: &NodeHandle,
    dispatch: &str,
    headers: &axum::http::HeaderMap,
    path_and_query: &str,
) -> Result<(), axum::response::Response> {
    let key = crate::signed_req::verify_signed_request(
        handle,
        &axum::http::Method::GET,
        path_and_query,
        headers,
        b"",
    )
    .map_err(|refusal| crate::signed_req::refuse(path_and_query, refusal))?;
    let pending = pending_runs(handle).await.map_err(|reason| {
        crate::error_response(axum::http::StatusCode::SERVICE_UNAVAILABLE, &reason)
    })?;
    let authorized = match pending.iter().find(|run| run.dispatch_id == dispatch) {
        Some(run) => run_reader(handle, &run.requester, &key)
            .await
            .map_err(|_| {
                crate::error_response(
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "Could not verify run access.",
                )
            })?,
        None => indexed_run_reader(handle, dispatch, &key).await?,
    };
    if !authorized {
        // the same sentence the topic refusal carries, for the same reason: it
        // names what to present and never what this node holds. A run that has
        // owned by someone else is indistinguishable from an unknown run —
        // both are "not yours to read", and saying which would
        // answer a probe about runs the caller may not see.
        tracing::debug!(
            target: "ducktape::stream",
            event = "run_output_upgrade_refused",
            reason = TopicRefusal::NotThisRunsReader.reason(),
            "refused a run-output upgrade"
        );
        return Err(crate::error_response(
            axum::http::StatusCode::FORBIDDEN,
            TopicRefusal::NotThisRunsReader.detail(),
        ));
    }
    Ok(())
}

/// Settled runs retain their creator in the materialized journal. Missing or
/// evicted live output remains an empty trace, never a reason to widen access.
async fn indexed_run_reader(
    handle: &NodeHandle,
    dispatch: &str,
    key: &[u8],
) -> Result<bool, axum::response::Response> {
    let Some(store) = handle.index.clone() else {
        return Ok(false);
    };
    let permit = handle
        .index_view_gate
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            crate::error_response(
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                "Run journal is busy.",
            )
        })?;
    let request = serde_json::to_vec(&runs_wire::view::RunsViewQuery::Run {
        dispatch_id: dispatch.into(),
    })
    .expect("run query");
    let reading = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        store.view_with_tip("runs", &request)
    })
    .await
    .map_err(|_| {
        crate::error_response(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "Run journal unavailable.",
        )
    })?;
    let reading = reading.map_err(|_| {
        crate::error_response(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "Run journal unavailable.",
        )
    })?;
    let reply =
        serde_json::from_slice::<runs_wire::view::RunsViewReply>(&reading.bytes).map_err(|_| {
            crate::error_response(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "Run journal unavailable.",
            )
        })?;
    match reply {
        runs_wire::view::RunsViewReply::Run(Some(detail)) => {
            run_reader(handle, &detail.run.requester, key)
                .await
                .map_err(|_| {
                    crate::error_response(
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        "Could not verify run access.",
                    )
                })
        }
        runs_wire::view::RunsViewReply::Run(None) | runs_wire::view::RunsViewReply::Runs(_) => {
            Ok(false)
        }
    }
}

/// every run `runs` has pending, as committed state.
pub(crate) async fn pending_runs(
    handle: &NodeHandle,
) -> Result<Vec<runs_wire::PendingRun>, String> {
    let (reply, rx) = futures::channel::oneshot::channel();
    handle
        .send(crate::NodeCommand::Query {
            target: "runs".to_string(),
            req: runs_wire::encode_query(&runs_wire::RunsQuery::PendingRuns),
            reply,
        })
        .await
        .map_err(|_| "actor gone".to_string())?;
    let bytes = rx
        .await
        .map_err(|_| "reply dropped".to_string())?
        .map_err(|refused| refused.message)?;
    match runs_wire::decode_reply(&bytes)? {
        runs_wire::RunsReply::PendingRuns(runs) => Ok(runs),
        _ => Err("unexpected runs reply".to_string()),
    }
}

/// Decide one requested topic: admit it (with its start cursor) or refuse it.
///
/// A decide-fn as far as STATE goes — it inserts no handle, mutates nothing, and
/// the caller applies the result. It is not effect-free: [`refuse_topic`] emits
/// one `debug` line, deliberately kept beside the decision so a refusal cannot
/// be returned without being counted.
///
/// `holds_workspace_secret` is the connection-wide secret compare, made once per
/// subscribe frame by [`subscribe_topics`]; `reader_of` is the one dispatch this
/// connection proved at its upgrade ([`admit_run_reader`]), and `operator`
/// whether that upgrade proved this node's operator.
#[allow(clippy::result_large_err)]
fn prepare_topic(
    topic: &str,
    holds_workspace_secret: bool,
    reader_of: Option<&str>,
    operator: bool,
    resume: Option<&String>,
    store: Option<&Arc<indexer::IndexStore>>,
) -> Result<(TopicState, Option<ServerFrame>), ServerFrame> {
    let Some(family) = Topic::parse(topic) else {
        return Err(refuse_topic(topic, TopicRefusal::UnknownFamily));
    };
    // the refusal is the admission's own: each gated admission names the
    // proof it wanted, so the caller is sent to the right one.
    let refused = match family.admission() {
        Admission::Public => None,
        Admission::Operator => (!operator).then_some(TopicRefusal::NotOperator),
        Admission::Run(id) => {
            let reads_this_run = holds_workspace_secret || reader_of == Some(id);
            (!reads_this_run).then_some(TopicRefusal::NotThisRunsReader)
        }
    };
    if let Some(refusal) = refused {
        return Err(refuse_topic(topic, refusal));
    }
    match family {
        Topic::Module(module) => prepare_module(topic, module, resume, store),
        Topic::FilesWatch => prepare_files_watch(topic, resume, store),
        Topic::Logs => prepare_logs(topic, resume),
        Topic::RunOutput(id) => prepare_run_output(topic, id, resume),
        Topic::Metrics => prepare_metrics(),
        Topic::Peers => prepare_peers(),
        Topic::Status => prepare_status(),
    }
}

#[allow(clippy::result_large_err)]
fn prepare_module(
    topic: &str,
    module: &str,
    resume: Option<&String>,
    store: Option<&Arc<indexer::IndexStore>>,
) -> Result<(TopicState, Option<ServerFrame>), ServerFrame> {
    let store = store.ok_or_else(|| unavailable(topic, "no index store configured"))?;
    if !store.module_ids().iter().any(|id| id == module) {
        return Err(refuse_topic(topic, TopicRefusal::UnknownModule));
    }
    let (cursor, lagged) = module_start_cursor(topic, module, resume, store)?;
    Ok((
        TopicState::Module {
            module: module.to_string(),
            cursor,
        },
        lagged,
    ))
}

#[allow(clippy::result_large_err)]
fn prepare_files_watch(
    topic: &str,
    resume: Option<&String>,
    store: Option<&Arc<indexer::IndexStore>>,
) -> Result<(TopicState, Option<ServerFrame>), ServerFrame> {
    let store = store.ok_or_else(|| unavailable(topic, "no index store configured"))?;
    if !store.module_ids().iter().any(|id| id == "files") {
        return Err(refuse_topic(topic, TopicRefusal::UnknownModule));
    }
    let (cursor, lagged) = module_start_cursor(topic, "files", resume, store)?;
    Ok((TopicState::FilesWatch { cursor }, lagged))
}

#[allow(clippy::result_large_err)]
fn prepare_logs(
    topic: &str,
    resume: Option<&String>,
) -> Result<(TopicState, Option<ServerFrame>), ServerFrame> {
    Ok((
        TopicState::Logs {
            seq: start_seq(topic, resume)?,
        },
        None,
    ))
}

#[allow(clippy::result_large_err)]
fn prepare_run_output(
    topic: &str,
    id: &str,
    resume: Option<&String>,
) -> Result<(TopicState, Option<ServerFrame>), ServerFrame> {
    Ok((
        TopicState::RunOutput {
            id: id.to_string(),
            seq: start_seq(topic, resume)?,
        },
        None,
    ))
}

/// a resume cursor is accepted but meaningless for a snapshot topic: every
/// (re)subscribe starts from a fresh sample, never a replay.
#[allow(clippy::result_large_err)]
fn prepare_metrics() -> Result<(TopicState, Option<ServerFrame>), ServerFrame> {
    Ok((TopicState::Metrics { sampled_ms: 0 }, None))
}

/// same snapshot contract as [`prepare_metrics`]: no replay, no resume point.
#[allow(clippy::result_large_err)]
fn prepare_peers() -> Result<(TopicState, Option<ServerFrame>), ServerFrame> {
    Ok((TopicState::Peers { sampled_ms: 0 }, None))
}

/// same snapshot contract again.
#[allow(clippy::result_large_err)]
fn prepare_status() -> Result<(TopicState, Option<ServerFrame>), ServerFrame> {
    Ok((TopicState::Status { sampled_ms: 0 }, None))
}

/// the seq a ring-backed topic starts from: the caller's resume cursor, or the
/// bottom of the ring.
#[allow(clippy::result_large_err)]
fn start_seq(topic: &str, resume: Option<&String>) -> Result<u64, ServerFrame> {
    match resume {
        Some(cursor) => parse_seq_cursor(topic, cursor),
        None => Ok(0),
    }
}

#[allow(clippy::result_large_err)]
fn module_start_cursor(
    topic: &str,
    module: &str,
    resume: Option<&String>,
    store: &indexer::IndexStore,
) -> Result<(String, Option<ServerFrame>), ServerFrame> {
    let Some(cursor) = resume else {
        return live_cursor(store, module)
            .map(|cursor| (cursor, None))
            .map_err(|err| unavailable(topic, err.to_string()));
    };
    if !cursor.starts_with(indexer::OP_PREFIX) || cursor_height(cursor).is_none() {
        return Err(ServerFrame::Error {
            topic: topic.to_string(),
            code: StreamErrorCode::BadCursor,
            detail: "cursor must be an op/{height}/{seq} key".into(),
        });
    }
    let height = cursor_height(cursor).expect("checked above");
    match store.backfill_height(module) {
        Ok(Some(floor)) if height < floor => {
            let jump =
                live_cursor(store, module).map_err(|err| unavailable(topic, err.to_string()))?;
            Ok((
                jump.clone(),
                Some(ServerFrame::Lagged {
                    topic: topic.to_string(),
                    cursor: jump,
                }),
            ))
        }
        Ok(_) => Ok((cursor.clone(), None)),
        Err(err) => Err(unavailable(topic, err.to_string())),
    }
}

async fn catch_up(
    handle: &NodeHandle,
    socket: &mut WebSocket,
    topics: &mut BTreeMap<String, TopicState>,
    wake: Wake,
) -> bool {
    let store = handle.stream_index();
    let hub = handle.stream_hub();
    let topic_names = topics
        .iter()
        .filter(|(_, state)| state.wakes_on(wake))
        .map(|(topic, _)| topic.clone())
        .collect::<Vec<_>>();
    for topic in topic_names {
        let Some(state) = topics.get_mut(&topic) else {
            continue;
        };
        // metrics is the one topic whose catch-up crosses the actor command
        // lane (an await); every cursor-scan topic stays on the sync path.
        let result = match state {
            TopicState::Metrics { .. } => catch_up_metrics(&topic, state, handle).await,
            TopicState::Peers { .. } => catch_up_peers(&topic, state, handle).await,
            TopicState::Status { .. } => catch_up_status(&topic, state, handle).await,
            // EVERY cursor-scan variant named, no `_`. A fourth snapshot topic
            // must fail the build here rather than fall through to the sync
            // path — where the natural "fix" is the do-nothing arm in
            // `catch_up_topic`, giving a topic that subscribes cleanly and
            // never delivers a frame. That is exactly what `files:watch` was.
            TopicState::Module { .. }
            | TopicState::FilesWatch { .. }
            | TopicState::Logs { .. }
            | TopicState::RunOutput { .. } => catch_up_topic(&topic, state, store.as_ref(), &hub),
        };
        if !send_frames(socket, result.frames).await {
            return false;
        }
        if let TopicState::RunOutput { id, .. } = state {
            let frame = ServerFrame::RunControlSnapshot {
                topic: topic.clone(),
                control: hub.run_output().controls.reading(id),
            };
            if !send_frame(socket, frame).await {
                return false;
            }
        }
        if result.drop_topic {
            topics.remove(&topic);
        }
    }
    true
}

fn catch_up_topic(
    topic: &str,
    state: &mut TopicState,
    store: Option<&Arc<indexer::IndexStore>>,
    hub: &StreamHub,
) -> CatchUpResult {
    match state {
        TopicState::Module { module, cursor } => {
            let Some(store) = store else {
                return CatchUpResult::drop(vec![unavailable(topic, "no index store configured")]);
            };
            catch_up_module(topic, module, cursor, store)
        }
        TopicState::FilesWatch { cursor } => {
            let Some(store) = store else {
                return CatchUpResult::drop(vec![unavailable(topic, "no index store configured")]);
            };
            catch_up_files(topic, cursor, store)
        }
        TopicState::Logs { seq } => catch_up_logs(topic, seq, &hub.log_ring()),
        TopicState::RunOutput { id, seq } => catch_up_run_output(topic, id, seq, &hub.run_output()),
        // routed to catch_up_metrics by the caller (it needs the actor lane,
        // an await this sync path cannot make) — nothing owed here.
        TopicState::Metrics { .. } | TopicState::Peers { .. } | TopicState::Status { .. } => {
            CatchUpResult::keep(Vec::new())
        }
    }
}

fn catch_up_module(
    topic: &str,
    module: &str,
    cursor: &mut String,
    store: &indexer::IndexStore,
) -> CatchUpResult {
    if let Some(frame) = lag_if_below_backfill(topic, module, cursor, store) {
        return frame;
    }

    let mut frames = Vec::new();
    let mut emitted = 0usize;
    loop {
        let remaining = STREAM_CATCHUP_BUDGET.saturating_sub(emitted);
        if remaining == 0 {
            return lag_to_live(topic, module, cursor, store, frames);
        }
        let page = match store.scan(
            module,
            indexer::OP_PREFIX.as_bytes(),
            Some(cursor.as_bytes()),
            remaining,
        ) {
            Ok(page) => page,
            Err(err) => {
                frames.push(unavailable(topic, err.to_string()));
                return CatchUpResult::drop(frames);
            }
        };
        let entry_count = page.entries.len();
        for (key, value) in page.entries {
            let key = String::from_utf8_lossy(&key).into_owned();
            let op = match borsh::from_slice::<indexer::OpRow>(&value) {
                Ok(row) => stream_op_row(row),
                Err(_) => {
                    frames.push(unavailable(
                        topic,
                        "stored op row was not a borsh envelope — rebuild the index",
                    ));
                    return CatchUpResult::drop(frames);
                }
            };
            *cursor = key.clone();
            emitted += 1;
            frames.push(ServerFrame::Event {
                topic: topic.to_string(),
                cursor: key,
                op,
            });
        }
        if page.has_more {
            if emitted >= STREAM_CATCHUP_BUDGET {
                return lag_to_live(topic, module, cursor, store, frames);
            }
            if entry_count == 0 {
                break;
            }
            continue;
        }
        break;
    }
    CatchUpResult::keep(frames)
}

fn catch_up_files(topic: &str, cursor: &mut String, store: &indexer::IndexStore) -> CatchUpResult {
    if let Some(frame) = lag_if_below_backfill(topic, "files", cursor, store) {
        return frame;
    }

    let mut frames = Vec::new();
    let mut emitted = 0usize;
    let mut scanned = 0usize;
    loop {
        let page = match store.scan(
            "files",
            indexer::OP_PREFIX.as_bytes(),
            Some(cursor.as_bytes()),
            STREAM_CATCHUP_BUDGET,
        ) {
            Ok(page) => page,
            Err(err) => {
                frames.push(unavailable(topic, err.to_string()));
                return CatchUpResult::drop(frames);
            }
        };
        let entry_count = page.entries.len();
        scanned += entry_count;
        for (key, value) in page.entries {
            let key = String::from_utf8_lossy(&key).into_owned();
            // THE SAME BYTES `catch_up_module` READS, so the same decoder.
            // `IndexStore::apply_block` writes one BORSH `indexer::OpRow` per
            // dispatch; this read them as json, so every files commit answered
            // "rebuild the index" and dropped the topic. The index was fine —
            // `files:watch` has simply never delivered a frame.
            let row = match borsh::from_slice::<indexer::OpRow>(&value) {
                Ok(row) => stream_op_row(row),
                Err(_) => {
                    frames.push(unavailable(
                        topic,
                        "stored op row was not a borsh envelope — rebuild the index",
                    ));
                    return CatchUpResult::drop(frames);
                }
            };
            *cursor = key.clone();
            let Some(payload) = row.payload else {
                continue;
            };
            let Ok(FilesMsg::Commit {
                base_snapshot,
                message,
                changes,
            }) = serde_json::from_value::<FilesMsg>(payload)
            else {
                continue;
            };
            let mut paths = Vec::new();
            for change in &changes {
                append_change_paths(change, &mut paths);
            }
            emitted += 1;
            frames.push(ServerFrame::Tail {
                topic: topic.to_string(),
                cursor: key,
                item: TailItem::FileChange {
                    height: row.height,
                    time: row.time,
                    message,
                    base_snapshot,
                    paths,
                },
            });
            if emitted >= STREAM_CATCHUP_BUDGET && page.has_more {
                return lag_to_live(topic, "files", cursor, store, frames);
            }
        }
        if page.has_more {
            if entry_count == 0 {
                break;
            }
            // a stage-heavy history is mostly non-commit rows that never
            // count against the emit budget — bound the raw scan too, or a
            // far-behind resume stalls the session task in one wakeup.
            if scanned >= FILES_SCAN_BUDGET {
                return lag_to_live(topic, "files", cursor, store, frames);
            }
            continue;
        }
        break;
    }
    CatchUpResult::keep(frames)
}

fn catch_up_logs(topic: &str, seq: &mut u64, logs: &LogRing) -> CatchUpResult {
    let mut frames = Vec::new();
    let (_, floor) = logs.read_after(*seq, STREAM_CATCHUP_BUDGET);
    if *seq < floor {
        // the ring wrapped past this reader: the evidence it came for is GONE.
        // `Lagged` alone re-cursors SILENTLY, so the tab just shows a shorter
        // history and nothing says why — say it in the tail itself, where the
        // human is actually looking. this is also how you learn empirically
        // whether the `info` floor is too chatty, instead of guessing.
        let dropped = floor - *seq;
        *seq = floor;
        frames.push(ServerFrame::Lagged {
            topic: topic.to_string(),
            cursor: floor.to_string(),
        });
        frames.push(ServerFrame::Tail {
            topic: topic.to_string(),
            cursor: floor.to_string(),
            item: TailItem::Log {
                line: format!("--- {dropped} earlier log line(s) dropped (ring full) ---"),
            },
        });
    }
    loop {
        let (rows, _) = logs.read_after(*seq, STREAM_CATCHUP_BUDGET);
        if rows.is_empty() {
            break;
        }
        let row_count = rows.len();
        for (line_seq, line) in rows {
            *seq = line_seq;
            frames.push(ServerFrame::Tail {
                topic: topic.to_string(),
                cursor: line_seq.to_string(),
                item: TailItem::Log { line },
            });
        }
        if row_count < STREAM_CATCHUP_BUDGET {
            break;
        }
    }
    CatchUpResult::keep(frames)
}

fn catch_up_run_output(
    topic: &str,
    id: &str,
    seq: &mut u64,
    runs: &RunOutputRegistry,
) -> CatchUpResult {
    let mut frames = Vec::new();
    // BOTH directions: below the floor the rows were evicted, above the head
    // the cursor names numbering this ring no longer has (a restart, or an
    // entry evicted and re-created). Either way the reader must be told, or it
    // waits on rows that will never come.
    let resume = runs.resume_cursor(id, *seq);
    let rewound = resume != *seq;
    if rewound {
        *seq = resume;
        frames.push(ServerFrame::Lagged {
            topic: topic.to_string(),
            cursor: resume.to_string(),
        });
    }
    loop {
        let (rows, _) = runs.read_after(id, *seq, STREAM_CATCHUP_BUDGET);
        if rows.is_empty() {
            break;
        }
        let row_count = rows.len();
        for (line_seq, stream, line) in rows {
            *seq = line_seq;
            frames.push(ServerFrame::Tail {
                topic: topic.to_string(),
                cursor: line_seq.to_string(),
                item: TailItem::RunOutput { stream, line },
            });
        }
        if row_count < STREAM_CATCHUP_BUDGET {
            break;
        }
    }
    CatchUpResult::keep(frames)
}

/// Whether a snapshot topic sampled too recently to be worth re-composing.
///
/// `sampled_ms` starts at 0, so a topic's FIRST catch-up always composes —
/// which is what makes the subscribe replay the one that survives, and the
/// immediate tick behind it the one that folds away.
///
/// `checked_sub`, NOT `saturating_sub`. The beat is monotonic
/// (`tokio::time::interval`) and this stamp is wall clock (`unix_millis`), so
/// the two can diverge: an ntp step backwards, a VM resumed from a snapshot, a
/// container syncing a drifted RTC. Saturating turns every such reading into
/// `0`, which reads as FRESH — and both overview topics would then compose
/// nothing for the length of the jump, behind heartbeat frames that keep the
/// socket looking perfectly healthy. A stamp from the future is instead read
/// as stale: the topic composes once and re-anchors to the new clock.
///
/// This mattered less before the debounce, when `sampled_ms` was cursor
/// bookkeeping and a wrong label was cosmetic. Making it a delivery decision
/// is what put a clock skew on the path.
fn snapshot_is_fresh(sampled_ms: u64, now_ms: u64) -> bool {
    now_ms
        .checked_sub(sampled_ms)
        .is_some_and(|age_ms| age_ms < SNAPSHOT_MIN_INTERVAL_MS)
}

/// re-sample the node's OpenMetrics exposition through the SAME wired source
/// GET /metrics reads (the handle's status cell), so the stream needs no
/// second registry encoder — and no actor round-trip. one Tail frame per
/// wakeup carrying the whole scrape text; an unwired source drops the topic
/// with the same `unavailable` shape the http lane's 503 carries.
async fn catch_up_metrics(
    topic: &str,
    state: &mut TopicState,
    handle: &NodeHandle,
) -> CatchUpResult {
    let TopicState::Metrics { sampled_ms } = state else {
        return CatchUpResult::keep(Vec::new());
    };
    // ONE reading, used by the guard and by the stamp below: two calls would
    // decide freshness against one clock and record another.
    let now_ms = unix_millis();
    if snapshot_is_fresh(*sampled_ms, now_ms) {
        return CatchUpResult::keep(Vec::new());
    }
    let Some(text) = handle.status_cell().exposition() else {
        return CatchUpResult::drop(vec![unavailable(
            topic,
            "no metrics exposition is wired on this daemon",
        )]);
    };
    handle
        .stream_hub()
        .note_snapshot_sample(crate::metrics::SnapshotTopic::Metrics);
    let time_ms = now_ms;
    *sampled_ms = time_ms;
    CatchUpResult::keep(vec![ServerFrame::Tail {
        topic: topic.to_string(),
        cursor: time_ms.to_string(),
        item: TailItem::Metrics { time_ms, text },
    }])
}

/// re-compose the direct-peer sample, through the SAME two sources
/// `GET /v1/peers` reads and in the same order: the live exposition for the
/// connection and traffic counters, the last-published standing for the
/// committed facts (roles, height, epoch). No actor round-trip, so a node
/// stuck in a sync stage keeps answering — the whole reason the standing is
/// published into a cell rather than asked for.
///
/// A SNAPSHOT, not a delta: peers have no op behind them and nothing in the
/// index names them, so there is no cursor to resume and no backlog to replay.
/// That is why this rides the heartbeat instead of a block wake.
async fn catch_up_peers(topic: &str, state: &mut TopicState, handle: &NodeHandle) -> CatchUpResult {
    let TopicState::Peers { sampled_ms } = state else {
        return CatchUpResult::keep(Vec::new());
    };
    // ONE reading, used by the guard and by the stamp below: two calls would
    // decide freshness against one clock and record another.
    let now_ms = unix_millis();
    if snapshot_is_fresh(*sampled_ms, now_ms) {
        return CatchUpResult::keep(Vec::new());
    }
    let cell = handle.status_cell();
    let Some(exposition) = cell.exposition() else {
        return CatchUpResult::drop(vec![unavailable(
            topic,
            "no metrics exposition is wired on this daemon",
        )]);
    };
    // BELOW the availability guard: the counter says "a document was composed",
    // and a catch-up that drops the topic composed nothing.
    handle
        .stream_hub()
        .note_snapshot_sample(crate::metrics::SnapshotTopic::Peers);
    let standing = cell.peers_standing();
    let time_ms = now_ms;
    *sampled_ms = time_ms;
    let peers =
        crate::peers::peers_from_exposition(&exposition, time_ms, standing.height, standing.epoch)
            .with_roles(&standing.validators, &standing.residents);
    CatchUpResult::keep(vec![ServerFrame::Tail {
        topic: topic.to_string(),
        cursor: time_ms.to_string(),
        item: TailItem::Peers { time_ms, peers },
    }])
}

/// re-read the published node-status projection. The CHEAP snapshot topic:
/// `current()` clones the cell the owning actor swapped at its last boundary,
/// so there is no registry encode and no actor round-trip here at all. It
/// never drops the topic — a node that has published nothing yet has a
/// zeroed status, which is the honest pre-boundary answer and the same one
/// `GET /v1/status` gives.
async fn catch_up_status(
    topic: &str,
    state: &mut TopicState,
    handle: &NodeHandle,
) -> CatchUpResult {
    let TopicState::Status { sampled_ms } = state else {
        return CatchUpResult::keep(Vec::new());
    };
    // ONE reading, used by the guard and by the stamp below: two calls would
    // decide freshness against one clock and record another.
    let now_ms = unix_millis();
    if snapshot_is_fresh(*sampled_ms, now_ms) {
        return CatchUpResult::keep(Vec::new());
    }
    handle
        .stream_hub()
        .note_snapshot_sample(crate::metrics::SnapshotTopic::Status);
    let time_ms = now_ms;
    *sampled_ms = time_ms;
    let status = Box::new(handle.status_cell().current());
    CatchUpResult::keep(vec![ServerFrame::Tail {
        topic: topic.to_string(),
        cursor: time_ms.to_string(),
        item: TailItem::Status { time_ms, status },
    }])
}

fn lag_if_below_backfill(
    topic: &str,
    module: &str,
    cursor: &mut String,
    store: &indexer::IndexStore,
) -> Option<CatchUpResult> {
    let floor = match store.backfill_height(module) {
        Ok(Some(floor)) => floor,
        Ok(None) => return None,
        Err(err) => {
            return Some(CatchUpResult::drop(vec![unavailable(
                topic,
                err.to_string(),
            )]));
        }
    };
    if cursor_height(cursor).is_some_and(|height| height < floor) {
        return Some(lag_to_live(topic, module, cursor, store, Vec::new()));
    }
    None
}

fn lag_to_live(
    topic: &str,
    module: &str,
    cursor: &mut String,
    store: &indexer::IndexStore,
    mut frames: Vec<ServerFrame>,
) -> CatchUpResult {
    match live_cursor(store, module) {
        Ok(jump) => {
            *cursor = jump.clone();
            frames.push(ServerFrame::Lagged {
                topic: topic.to_string(),
                cursor: jump,
            });
            CatchUpResult::keep(frames)
        }
        Err(err) => {
            frames.push(unavailable(topic, err.to_string()));
            CatchUpResult::drop(frames)
        }
    }
}

async fn send_frames(socket: &mut WebSocket, frames: Vec<ServerFrame>) -> bool {
    for frame in frames {
        if !send_frame(socket, frame).await {
            return false;
        }
    }
    true
}

async fn send_frame(socket: &mut WebSocket, frame: ServerFrame) -> bool {
    let text = serde_json::to_string(&frame).expect("stream frame serializes");
    socket.send(Message::Text(text.into())).await.is_ok()
}

fn heartbeat_frame(hub: &StreamHub) -> ServerFrame {
    let (height, root_hash) = hub.tip().unwrap_or_else(|| (0, String::new()));
    ServerFrame::Heartbeat {
        height,
        root_hash,
        time_ms: unix_millis(),
        interval_ms: HEARTBEAT_INTERVAL_MS,
    }
}

pub(crate) fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is past the epoch")
        .as_millis() as u64
}

fn live_cursor(store: &indexer::IndexStore, module: &str) -> Result<String, indexer::Error> {
    let applied = store.applied_height(module)?;
    // the end of that height: every real row at it is at or below the widest
    // seq, so the next scan starts at the height above. built through
    // `op_key` so the field width can never drift from the rows it pages.
    Ok(indexer::op_key(applied, u32::MAX))
}

fn cursor_height(cursor: &str) -> Option<u64> {
    let rest = cursor.strip_prefix(indexer::OP_PREFIX)?;
    let height = rest.get(0..16)?;
    if rest.as_bytes().get(16) != Some(&b'/') {
        return None;
    }
    u64::from_str_radix(height, 16).ok()
}

#[allow(clippy::result_large_err)]
fn parse_seq_cursor(topic: &str, cursor: &str) -> Result<u64, ServerFrame> {
    cursor.parse::<u64>().map_err(|_| ServerFrame::Error {
        topic: topic.to_string(),
        code: StreamErrorCode::BadCursor,
        detail: "cursor must be a numeric sequence".into(),
    })
}

fn unavailable(topic: impl Into<String>, detail: impl Into<String>) -> ServerFrame {
    ServerFrame::Error {
        topic: topic.into(),
        code: StreamErrorCode::Unavailable,
        detail: detail.into(),
    }
}

fn append_change_paths(change: &Change, paths: &mut Vec<String>) {
    match change {
        Change::Put { path, .. }
        | Change::Mkdir { path }
        | Change::Rm { path }
        | Change::Symlink { path, .. } => paths.push(path.clone()),
        Change::Mv { from, to } => {
            paths.push(from.clone());
            paths.push(to.clone());
        }
    }
}

#[cfg(test)]
mod tests {

    use indexer::{AppliedOp, BlockOps, IndexModule, OriginTag};
    use serde_json::json;

    use super::*;

    /// a caller that presented no workspace secret (or the wrong one).
    const NO_SECRET: bool = false;
    /// a caller whose presented secret matched.
    const HOLDS_SECRET: bool = true;
    /// a connection admitted as no run's creator — every caller but a remote
    /// app watching a run it asked for.
    const NO_RUN: Option<&str> = None;
    /// a connection whose upgrade proved nothing about the operator.
    const NOT_OPERATOR: bool = false;
    /// a connection whose upgrade proved this node's operator.
    const OPERATOR: bool = true;
    /// the workspace secret a test node mints.
    const TEST_SECRET: &str = "d3adb33fd3adb33fd3adb33fd3adb33f";

    /// a handle whose service link holds [`TEST_SECRET`] — a node with a
    /// workspace, i.e. the only shape that can admit a gated topic at all. The
    /// actor lane is unused on every subscribe path, so its receiver is dropped
    /// here rather than parked in each caller.
    fn handle_with_secret() -> crate::NodeHandle {
        let (handle, _cmds, _hub) = crate::NodeHandle::channel();
        handle.with_service_link(crate::service_link::ServiceLink::new(Some(
            TEST_SECRET.into(),
        )))
    }

    fn temp_store(modules: &[&str]) -> (tempfile::TempDir, Arc<indexer::IndexStore>) {
        let dir = tempfile::TempDir::new().expect("temp index dir");
        let bare: Vec<IndexModule> = modules.iter().map(|id| IndexModule::bare(id)).collect();
        let store = indexer::IndexStore::open(dir.path(), &bare).expect("open index");
        (dir, Arc::new(store))
    }

    fn apply_chat(store: &indexer::IndexStore, height: u64, payloads: Vec<serde_json::Value>) {
        let ops = payloads
            .into_iter()
            .map(|payload| AppliedOp {
                module: "chat".into(),
                origin: OriginTag::external("tester"),
                payload: serde_json::to_vec(&payload).expect("payload json"),
                assigned: Vec::new(),
            })
            .collect();
        store
            .apply_block(&BlockOps {
                height,
                time: height * 10,
                ops,
                record: None,
            })
            .expect("apply block");
    }

    /// FILES:WATCH HAD NEVER DELIVERED A FRAME.
    ///
    /// `IndexStore::apply_block` writes one BORSH `indexer::OpRow` per dispatch
    /// — the same bytes `catch_up_module` reads with `borsh::from_slice`. This
    /// path read them as json, so the first files commit answered "rebuild the
    /// index" and dropped the topic, blaming a store that was correct.
    ///
    /// The block goes in through the REAL `apply_block`, so the encoding under
    /// test is the one the node actually writes — which is the whole reason a
    /// test here catches it and none existed.
    #[test]
    fn files_watch_reads_the_rows_the_index_actually_wrote() {
        let (_dir, store) = temp_store(&["files"]);
        let commit = json!({
            "commit": {
                "base_snapshot": null,
                "message": "first",
                "changes": [{ "mkdir": { "path": "notes" } }],
            }
        });
        store
            .apply_block(&BlockOps {
                height: 1,
                time: 10,
                ops: vec![AppliedOp {
                    module: "files".into(),
                    origin: OriginTag::external("tester"),
                    payload: serde_json::to_vec(&commit).expect("payload json"),
                    assigned: Vec::new(),
                }],
                record: None,
            })
            .expect("apply block");

        let mut cursor = "op/0000000000000000/ffffffff".to_string();
        let result = catch_up_files("files:watch", &mut cursor, &store);
        assert!(
            !result.drop_topic,
            "a healthy index must not drop the topic: {:?}",
            result.frames
        );
        match result.frames.as_slice() {
            [
                ServerFrame::Tail {
                    item: TailItem::FileChange { paths, message, .. },
                    ..
                },
            ] => {
                assert_eq!(paths, &["notes".to_string()]);
                assert_eq!(message, "first");
            }
            other => panic!("expected one file-change tail, got {other:?}"),
        }
    }

    #[test]
    fn module_catch_up_emits_rows_and_cursors() {
        let (_dir, store) = temp_store(&["chat"]);
        apply_chat(&store, 1, vec![json!({"one": 1}), json!({"two": 2})]);
        let mut cursor = "op/0000000000000000/ffffffff".to_string();
        let result = catch_up_module("module:chat", "chat", &mut cursor, &store);
        assert!(!result.drop_topic);
        assert_eq!(result.frames.len(), 2);
        match &result.frames[0] {
            ServerFrame::Event { cursor, op, .. } => {
                assert_eq!(cursor, "op/0000000000000001/00000000");
                assert_eq!(op.payload, Some(json!({"one": 1})));
            }
            other => panic!("expected event, got {other:?}"),
        }
        assert_eq!(cursor, "op/0000000000000001/00000001");
    }

    /// A BLOCK THAT APPENDED NOTHING MUST NOT SEND ANYONE BACK TO THE STORE.
    ///
    /// An idle chain nop-fills once per block time, and that filler dispatches
    /// nothing, so every scan it used to trigger read the index and found
    /// nothing — per subscribed topic, per session, once a second, forever.
    #[test]
    fn an_unfed_block_owes_the_index_nothing() {
        assert_eq!(BlockWake::from_dispatches(&[]), BlockWake::TipOnly);
        assert_eq!(
            block_action(Ok(BlockWake::TipOnly)),
            BlockAction::TipOnly,
            "an unfed block still sends the tip — the head moves on nop blocks"
        );
        assert_eq!(
            block_action(Ok(BlockWake::IndexChanged)),
            BlockAction::SweepIndex
        );
    }

    /// A DROPPED WAKE IS SWEPT, NOT SKIPPED. The discriminants went with the
    /// dropped wakes, so any of them may have fed a module. Sweeping a block
    /// that did not costs one empty scan; skipping one that did strands the
    /// topic until the backstop.
    #[test]
    fn a_lagged_wake_sweeps_and_a_closed_hub_stops() {
        assert_eq!(
            block_action(Err(broadcast::error::RecvError::Lagged(7))),
            BlockAction::SweepIndex
        );
        assert_eq!(
            block_action(Err(broadcast::error::RecvError::Closed)),
            BlockAction::Stop
        );
    }

    #[test]
    fn module_budget_overflow_lagged_jumps_to_watermark() {
        let (_dir, store) = temp_store(&["chat"]);
        let payloads = (0..=STREAM_CATCHUP_BUDGET)
            .map(|i| json!({ "n": i }))
            .collect();
        apply_chat(&store, 1, payloads);
        let mut cursor = "op/0000000000000000/ffffffff".to_string();
        let result = catch_up_module("module:chat", "chat", &mut cursor, &store);
        assert_eq!(result.frames.len(), STREAM_CATCHUP_BUDGET + 1);
        assert!(
            matches!(result.frames.last(), Some(ServerFrame::Lagged { cursor, .. }) if cursor == "op/0000000000000001/ffffffff")
        );
        assert_eq!(cursor, "op/0000000000000001/ffffffff");
    }

    #[test]
    fn fresh_module_subscribe_starts_at_live_tip() {
        let (_dir, store) = temp_store(&["chat"]);
        apply_chat(&store, 1, vec![json!({"one": 1})]);
        let (state, lagged) = prepare_topic(
            "module:chat",
            NO_SECRET,
            NO_RUN,
            NOT_OPERATOR,
            None,
            Some(&store),
        )
        .expect("topic");
        assert!(lagged.is_none());
        assert_eq!(state.cursor(), "op/0000000000000001/ffffffff");
        let mut state = state;
        let result = catch_up_topic(
            "module:chat",
            &mut state,
            Some(&store),
            &StreamHub::new(crate::handle::EVENT_BUFFER),
        );
        assert!(result.frames.is_empty());
    }

    #[test]
    fn resume_below_backfill_floor_lagged_to_live_watermark() {
        let (_dir, store) = temp_store(&["chat"]);
        store.mark_backfilled("chat", 10).expect("mark backfilled");
        let (state, lagged) = prepare_topic(
            "module:chat",
            NO_SECRET,
            NO_RUN,
            NOT_OPERATOR,
            Some(&"op/0000000000000005/00000000".to_string()),
            Some(&store),
        )
        .expect("topic");
        assert_eq!(state.cursor(), "op/000000000000000a/ffffffff");
        assert!(
            matches!(lagged, Some(ServerFrame::Lagged { cursor, .. }) if cursor == "op/000000000000000a/ffffffff")
        );
    }

    #[test]
    fn topic_refusals_are_per_topic() {
        assert!(matches!(
            prepare_topic("module:chat", NO_SECRET, NO_RUN, NOT_OPERATOR, None, None),
            Err(ServerFrame::Error {
                code: StreamErrorCode::Unavailable,
                ..
            })
        ));
        let (_dir, store) = temp_store(&["chat"]);
        assert!(matches!(
            prepare_topic(
                "module:nope",
                NO_SECRET,
                NO_RUN,
                NOT_OPERATOR,
                None,
                Some(&store)
            ),
            Err(ServerFrame::Error {
                code: StreamErrorCode::UnknownTopic,
                ..
            })
        ));
        assert!(matches!(
            prepare_topic(
                "logs",
                NO_SECRET,
                NO_RUN,
                OPERATOR,
                Some(&"not-a-seq".to_string()),
                Some(&store)
            ),
            Err(ServerFrame::Error {
                code: StreamErrorCode::BadCursor,
                ..
            })
        ));
    }

    #[test]
    fn log_ring_wrap_reports_lagged_then_replays_from_floor() {
        let logs = LogRing::default();
        for i in 0..=LOG_RING_CAPACITY {
            logs.push_line(format!("line-{i}"));
        }
        let mut seq = 0;
        let result = catch_up_logs("logs", &mut seq, &logs);
        assert!(
            matches!(result.frames.first(), Some(ServerFrame::Lagged { cursor, .. }) if cursor == "1")
        );
        // the eviction is NAMED in the tail, not just silently re-cursored: a
        // reader must never mistake a truncated history for a quiet node.
        assert!(matches!(
            result.frames.get(1),
            Some(ServerFrame::Tail { item: TailItem::Log { line }, .. })
                if line == "--- 1 earlier log line(s) dropped (ring full) ---"
        ));
        assert!(
            matches!(result.frames.get(2), Some(ServerFrame::Tail { cursor, .. }) if cursor == "2")
        );
        assert_eq!(seq, (LOG_RING_CAPACITY + 1) as u64);
    }

    #[test]
    fn run_output_ring_wraps_and_evicts_lru_runs() {
        let runs = RunOutputRegistry::default();
        for i in 0..=RUN_OUTPUT_MAX_LINES {
            runs.append("active", RunStream::Stdout, format!("line-{i}"));
        }
        let mut seq = 0;
        let result = catch_up_run_output("run-output:active", "active", &mut seq, &runs);
        assert!(
            matches!(result.frames.first(), Some(ServerFrame::Lagged { cursor, .. }) if cursor == "1")
        );
        assert_eq!(seq, (RUN_OUTPUT_MAX_LINES + 1) as u64);

        for i in 0..RUN_OUTPUT_MAX_RUNS {
            runs.append(format!("run-{i}"), RunStream::Stderr, "x");
        }
        let (rows, floor) = runs.read_after("active", 0, 1);
        assert!(rows.is_empty(), "the rows went with the ring");
        assert_eq!(
            floor,
            (RUN_OUTPUT_MAX_LINES + 1) as u64,
            "but the numbering did not: the floor still names what was dropped"
        );
    }

    /// a run whose ring the cap shed while a pane was subscribed to it. The
    /// pane's cursor sits at the old high-water; the run keeps printing. Before
    /// the numbering survived eviction the ring restarted at 1, every new line
    /// failed the `> cursor` filter, the floor read 0 so no `Lagged` fired, and
    /// the pane sat frozen for the life of the connection.
    #[test]
    fn a_re_created_run_ring_lags_the_subscriber_instead_of_going_silent() {
        let runs = RunOutputRegistry::default();
        for i in 0..600 {
            runs.append("active", RunStream::Stdout, format!("line-{i}"));
        }
        // the pane has consumed the first 500 lines.
        let mut seq = 500;
        for i in 0..RUN_OUTPUT_MAX_RUNS {
            runs.append(format!("run-{i}"), RunStream::Stderr, "x");
        }
        runs.append("active", RunStream::Stdout, "after the eviction");

        let result = catch_up_run_output("run-output:active", "active", &mut seq, &runs);
        assert!(
            matches!(result.frames.first(), Some(ServerFrame::Lagged { cursor, .. }) if cursor == "600"),
            "the 100 lines evicted under the cursor are announced, not swallowed"
        );
        assert!(matches!(
            result.frames.last(),
            Some(ServerFrame::Tail { cursor, .. }) if cursor == "601"
        ));
        assert_eq!(seq, 601, "and the pane is live again on the new numbering");
    }

    /// the same blind spot with no eviction at all: a client resumes with a seq
    /// it saved before a restart, so the cursor is above a fresh ring's head.
    /// It must be rewound and told, never left waiting for seq 901.
    #[test]
    fn a_resume_cursor_above_the_head_lags_rather_than_waits() {
        let runs = RunOutputRegistry::default();
        let mut seq = 900;
        let result = catch_up_run_output("run-output:fresh", "fresh", &mut seq, &runs);
        assert!(
            matches!(result.frames.first(), Some(ServerFrame::Lagged { cursor, .. }) if cursor == "0")
        );
        assert_eq!(seq, 0);

        runs.append("fresh", RunStream::Stdout, "first");
        let result = catch_up_run_output("run-output:fresh", "fresh", &mut seq, &runs);
        assert!(matches!(
            result.frames.first(),
            Some(ServerFrame::Tail { cursor, .. }) if cursor == "1"
        ));
        assert_eq!(seq, 1);
    }

    #[test]
    fn remote_run_output_does_not_rebroadcast() {
        let runs = RunOutputRegistry::default();
        let mut appends = runs.subscribe_appends();
        runs.append("aa".repeat(32), RunStream::Stdout, "local");
        assert_eq!(appends.try_recv().unwrap().line, "local");

        // "bb"*32 is a run only a peer has ever named — a mirrored remote run,
        // never one this node hosts.
        assert!(runs.append_remote(RunOutputEvent {
            id: "bb".repeat(32),
            stream: RunStream::Stderr,
            line: "remote".into(),
        }));
        assert!(matches!(
            appends.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        let (rows, _) = runs.read_after(&"bb".repeat(32), 0, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].2, "remote");
    }

    #[test]
    fn a_locally_hosted_run_is_never_writable_by_a_peer() {
        let runs = RunOutputRegistry::default();
        runs.append("aa".repeat(32), RunStream::Stdout, "local");
        assert!(runs.is_local(&"aa".repeat(32)));

        assert!(!runs.append_remote(RunOutputEvent {
            id: "aa".repeat(32),
            stream: RunStream::Stderr,
            line: "forged".into(),
        }));
        let (rows, _) = runs.read_after(&"aa".repeat(32), 0, 10);
        assert_eq!(
            rows.len(),
            1,
            "the peer's line never entered the local ring"
        );
        assert_eq!(rows[0].2, "local");
    }

    #[test]
    fn thirty_two_fabricated_remote_ids_never_evict_a_local_ring() {
        let runs = RunOutputRegistry::default();
        runs.append("aa".repeat(32), RunStream::Stdout, "local");
        // fill every other slot with local runs too, so the registry sits at
        // the cap holding nothing but locally hosted rings.
        for i in 0..RUN_OUTPUT_MAX_RUNS - 1 {
            runs.append(format!("{i:064x}"), RunStream::Stdout, "x");
        }
        for i in 0..RUN_OUTPUT_MAX_RUNS {
            let forged = format!("{:064x}", i + 1_000);
            assert!(
                !runs.append_remote(RunOutputEvent {
                    id: forged,
                    stream: RunStream::Stdout,
                    line: "flood".into(),
                }),
                "no remote ring exists to evict, so the flood is refused outright"
            );
        }
        assert!(
            runs.is_local(&"aa".repeat(32)),
            "the local ring survives the flood"
        );
        let (rows, _) = runs.read_after(&"aa".repeat(32), 0, 10);
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_frame_shape_under_paused_time() {
        let hub = StreamHub::new(crate::handle::EVENT_BUFFER);
        hub.prime(7, "abc");
        let mut interval = tokio::time::interval(Duration::from_millis(HEARTBEAT_INTERVAL_MS));
        interval.tick().await;
        tokio::time::advance(Duration::from_millis(HEARTBEAT_INTERVAL_MS)).await;
        interval.tick().await;
        match heartbeat_frame(&hub) {
            ServerFrame::Heartbeat {
                height,
                root_hash,
                interval_ms,
                ..
            } => {
                assert_eq!(height, 7);
                assert_eq!(root_hash, "abc");
                assert_eq!(interval_ms, HEARTBEAT_INTERVAL_MS);
            }
            other => panic!("expected heartbeat, got {other:?}"),
        }
    }

    #[test]
    fn published_run_output_is_bounded_and_shaped_before_it_reaches_the_ring() {
        // the guard exists because a line that reaches the ring is broadcast to
        // every overlay peer, and the agent data plane REFUSES to write an event
        // whose id is not 64-hex — treating that refusal as fatal to the peer
        // stream. One malformed frame would otherwise tear down and reopen every
        // peer's telemetry, repeatably, from any local ws client.
        let hub = StreamHub::new(16);
        let runs = hub.run_output();
        let good = "a".repeat(RUN_OUTPUT_ID_LEN);

        // every id shape the peer path would reject is dropped here instead.
        for bad in [
            String::from("x"),
            String::new(),
            "a".repeat(RUN_OUTPUT_ID_LEN - 1),
            "a".repeat(RUN_OUTPUT_ID_LEN + 1),
            "g".repeat(RUN_OUTPUT_ID_LEN),
            format!("{}-", "a".repeat(RUN_OUTPUT_ID_LEN - 1)),
        ] {
            handle_run_output(&hub, bad.clone(), RunStream::Stdout, "hi".into());
            let mut seq = 0;
            assert!(
                catch_up_run_output(&format!("run-output:{bad}"), &bad, &mut seq, &runs)
                    .frames
                    .is_empty(),
                "id {bad:?} must never reach the ring"
            );
        }

        // an oversized line is dropped too — the ring's caps bound line COUNT,
        // never bytes.
        handle_run_output(
            &hub,
            good.clone(),
            RunStream::Stdout,
            "x".repeat(MAX_RUN_OUTPUT_LINE + 1),
        );
        let mut seq = 0;
        assert!(
            catch_up_run_output(&format!("run-output:{good}"), &good, &mut seq, &runs)
                .frames
                .is_empty(),
            "an oversized line must never reach the ring"
        );

        // the real shape — a hex sha256 run key, an ordinary line — is admitted,
        // so the guard bounds the surface without refusing the daemon its own
        // runs.
        handle_run_output(&hub, good.clone(), RunStream::Stdout, "real output".into());
        let mut seq = 0;
        let caught = catch_up_run_output(&format!("run-output:{good}"), &good, &mut seq, &runs);
        assert_eq!(caught.frames.len(), 1, "a well-formed line is admitted");
        // and a line exactly AT the cap is still admitted (the bound is not
        // accidentally off by one against real output).
        handle_run_output(
            &hub,
            good.clone(),
            RunStream::Stderr,
            "x".repeat(MAX_RUN_OUTPUT_LINE),
        );
        let caught = catch_up_run_output(&format!("run-output:{good}"), &good, &mut seq, &runs);
        assert_eq!(caught.frames.len(), 1, "a line at the cap is admitted");
    }

    /// Every family's admission is DECIDED, and the table is the decision.
    ///
    /// A new family cannot reach this list by accident: `Topic::admission` has
    /// no `_` arm, so adding a variant fails the build until someone writes its
    /// admission, and adding it here is how that choice gets reviewed.
    #[test]
    fn every_topic_family_has_a_decided_admission() {
        let decided = [
            (Topic::Module("chat"), Admission::Public),
            (Topic::FilesWatch, Admission::Public),
            // this operator's process log: its HTTP twin is operator-gated, so
            // the topic takes the operator's proofs too.
            (Topic::Logs, Admission::Operator),
            (Topic::Metrics, Admission::Public),
            // public for the SAME reason metrics is, and no weaker: the
            // identical sample already leaves this node over unauthenticated
            // `GET /v1/peers`, which this change does not touch.
            (Topic::Peers, Admission::Public),
            (Topic::Status, Admission::Public),
            // the workspace secret OR this run's creator, and the id travels
            // with the decision so one run's reader is not every run's.
            (Topic::RunOutput("r1"), Admission::Run("r1")),
        ];
        for (family, expected) in decided {
            assert_eq!(family.admission(), expected, "{family:?}");
        }

        // the wire names round-trip to the families above ...
        assert_eq!(Topic::parse("module:chat"), Some(Topic::Module("chat")));
        assert_eq!(Topic::parse("files:watch"), Some(Topic::FilesWatch));
        assert_eq!(Topic::parse("logs"), Some(Topic::Logs));
        assert_eq!(Topic::parse("metrics"), Some(Topic::Metrics));
        assert_eq!(Topic::parse("peers"), Some(Topic::Peers));
        assert_eq!(Topic::parse("status"), Some(Topic::Status));
        assert_eq!(Topic::parse("run-output:r1"), Some(Topic::RunOutput("r1")));

        // ... and a name no family owns parses to nothing, which is what makes
        // admission deny-by-default rather than a habit.
        for unknown in ["", "term:s1", "logs2", "modules:chat", "files:watch2"] {
            assert_eq!(Topic::parse(unknown), None, "{unknown:?} owns no family");
            assert!(matches!(
                prepare_topic(unknown, HOLDS_SECRET, NO_RUN, NOT_OPERATOR, None, None),
                Err(ServerFrame::Error {
                    code: StreamErrorCode::UnknownTopic,
                    ..
                })
            ));
        }
    }

    /// The workspace-gated family hands back NO handle without the secret.
    #[test]
    fn the_gated_family_refuses_a_caller_with_no_workspace_secret() {
        // the one family the workspace secret still gates: a run's output.
        const GATED: &str = "run-output:r1";
        let Err(ServerFrame::Error { code, detail, .. }) =
            prepare_topic(GATED, NO_SECRET, NO_RUN, NOT_OPERATOR, None, None)
        else {
            panic!("{GATED} must refuse a caller with no workspace secret");
        };
        assert_eq!(code, StreamErrorCode::Forbidden);
        // A TRIPWIRE, not the live check: `detail()` is a `&'static str`
        // with no access to any secret, so this cannot fail today — it fails
        // the day someone gives the refusal a formatted body. The real
        // guarantee is structural and is stated where it is enforced, on
        // `TopicRefusal::detail`.
        assert!(
            !detail.contains(TEST_SECRET),
            "a refusal must never carry the secret: {detail}"
        );
        // and it admits the same caller once the secret matches.
        assert!(prepare_topic(GATED, HOLDS_SECRET, NO_RUN, NOT_OPERATOR, None, None).is_ok());
        // the public families need nothing, on the same call.
        assert!(prepare_topic("metrics", NO_SECRET, NO_RUN, NOT_OPERATOR, None, None).is_ok());
    }

    /// The log ring is the OPERATOR's: a connection whose upgrade proved
    /// nothing is refused `forbidden` — and the workspace secret on the frame
    /// is not the operator's proof, so it does not open it either — while the
    /// same subscribe on an operator's connection is admitted.
    #[test]
    fn the_log_ring_is_refused_to_all_but_the_operator() {
        for holds_secret in [NO_SECRET, HOLDS_SECRET] {
            let Err(ServerFrame::Error { code, detail, .. }) =
                prepare_topic("logs", holds_secret, NO_RUN, NOT_OPERATOR, None, None)
            else {
                panic!("logs must refuse a connection that did not prove the operator");
            };
            assert_eq!(code, StreamErrorCode::Forbidden);
            assert_eq!(detail, TopicRefusal::NotOperator.detail());
        }
        assert_eq!(TopicRefusal::NotOperator.reason(), "not_operator");
        assert!(prepare_topic("logs", NO_SECRET, NO_RUN, OPERATOR, None, None).is_ok());
    }

    #[tokio::test]
    async fn program_runs_admit_only_the_current_controller_through_a_signed_upgrade() {
        use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
        use futures::SinkExt as _;
        let reader = PrivateKey::from_seed(91);
        let key = reader.public_key().as_ref().to_vec();
        let stranger = PrivateKey::from_seed(92);
        let node_key = vec![0xab; 32];
        let dispatch = "d".repeat(64);
        let (mut handle, mut commands, _) = crate::NodeHandle::channel();
        handle.admin.node_key = Some(node_key.clone());
        let program = |controller| identity::Control::Program {
            controller,
            executor: "runs".into(),
            generation: 0,
            standing: identity::ProgramStanding::Active,
        };
        let control = Arc::new(Mutex::new(Some(program(7))));
        let current = control.clone();
        let id = dispatch.clone();
        let actor_key = key.clone();
        let actor = tokio::spawn(async move {
            while let Some(command) = commands.next().await {
                let crate::NodeCommand::Query { target, req, reply } = command else {
                    continue;
                };
                let bytes = match target.as_str() {
                    "runs" => runs_wire::encode_reply(&runs_wire::RunsReply::PendingRuns(vec![
                        runs_wire::PendingRun {
                            run_id: "attributed/3/chiefduck".into(),
                            dispatch_id: id.clone(),
                            agent_id: "chiefduck".into(),
                            channel_id: "general".into(),
                            anchor_seq: 4,
                            thread_root: None,
                            job_id: None,
                            job_claim_height: 0,
                            requester: sdk::Origin::Program(42),
                            created_at: 0,
                        },
                    ])),
                    "identity" => {
                        let account = match identity::decode_query(&req).unwrap() {
                            identity::IdentityQuery::Get { number: 42 } => {
                                current.lock().unwrap().clone().map(|control| {
                                    identity::AccountView {
                                        number: 42,
                                        name: "ChiefDuck".into(),
                                        control,
                                        keys: vec![],
                                        avatar: None,
                                        bio: None,
                                        updated_at: 0,
                                    }
                                })
                            }
                            identity::IdentityQuery::OfKey { key } if key == actor_key => {
                                Some(identity::AccountView {
                                    number: 7,
                                    name: "Reader".into(),
                                    control: identity::Control::Keys,
                                    keys: vec![],
                                    avatar: None,
                                    bio: None,
                                    updated_at: 0,
                                })
                            }
                            identity::IdentityQuery::OfKey { .. } => None,
                            query => panic!("unexpected query {query:?}"),
                        };
                        identity::encode_reply(&identity::IdentityReply::Account(account))
                    }
                    target => panic!("unexpected target {target}"),
                };
                let _ = reply.send(Ok(bytes));
            }
        });
        let path = format!("/v1/ws?run={dispatch}");
        let signed = |signer: &PrivateKey| {
            let mut headers = axum::http::HeaderMap::new();
            for (name, value) in
                ::node::signed_req::request_headers(signer, "GET", &path, &node_key, b"")
            {
                headers.insert(name, value.parse().unwrap());
            }
            headers
        };
        assert!(
            admit_run_reader(&handle, &dispatch, &signed(&reader), &path)
                .await
                .is_ok()
        );
        assert!(
            admit_run_reader(&handle, &dispatch, &signed(&stranger), &path)
                .await
                .is_err()
        );
        // The same proof must deliver actual buffered output over the GUI's
        // WebSocket path, not merely return true from the admission helper.
        use tokio_tungstenite::tungstenite::{
            Message as WsMessage, client::IntoClientRequest as _,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = crate::router(handle.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let output = r#"{"method":"item/completed","params":{"item":{"id":"thought","type":"reasoning","summary":["Visible process details"]}}}"#;
        handle
            .stream_hub()
            .run_output()
            .append(&dispatch, RunStream::Stdout, output);
        let mut request = format!("ws://{address}{path}")
            .into_client_request()
            .unwrap();
        request.headers_mut().extend(signed(&reader));
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        socket
            .send(WsMessage::Text(
                serde_json::json!({"op":"subscribe","topics":[format!("run-output:{dispatch}")]})
                    .to_string(),
            ))
            .await
            .unwrap();
        loop {
            let message = socket
                .next()
                .await
                .expect("run output stream ended")
                .unwrap();
            let WsMessage::Text(text) = message else {
                continue;
            };
            let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_ne!(frame["type"], "error", "{frame}");
            if frame["type"] == "tail" {
                assert_eq!(frame["item"]["line"], output);
                break;
            }
        }
        socket.close(None).await.unwrap();
        server.abort();
        for next in [
            Some(program(8)),
            Some(identity::Control::Revoked { controller: 7 }),
            Some(identity::Control::Keys),
            None,
        ] {
            *control.lock().unwrap() = next;
            assert!(
                admit_run_reader(&handle, &dispatch, &signed(&reader), &path)
                    .await
                    .is_err()
            );
        }
        for origin in [
            sdk::Origin::External(vec![]),
            sdk::Origin::External(key[..16].to_vec()),
            sdk::Origin::Module("runs".into()),
            sdk::Origin::System,
        ] {
            assert!(!run_reader(&handle, &origin, &key).await.unwrap());
        }
        assert!(
            run_reader(&handle, &sdk::Origin::External(key.clone()), &key)
                .await
                .unwrap()
        );
        actor.abort();
    }

    /// The two gates this socket keeps for its callers, over a real upgrade:
    /// the log ring answers only an upgrade the operator key signed, and a run
    /// line lands only from a connection that took the compute attachment with
    /// the service-link token. Every bare attempt is REFUSED with a
    /// `forbidden` frame, never silently dropped.
    #[tokio::test]
    async fn the_log_ring_and_run_output_publish_take_their_callers_credentials() {
        use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
        use futures::SinkExt as _;
        use tokio_tungstenite::tungstenite::{
            Message as WsMessage, client::IntoClientRequest as _,
        };
        let operator = PrivateKey::from_seed(93);
        let stranger = PrivateKey::from_seed(94);
        let node_key = vec![0xab; 32];
        let handle = handle_with_secret().with_admin(crate::AdminConfig {
            node_key: Some(node_key.clone()),
            owner_key: Some(operator.public_key().as_ref().to_vec()),
            ..Default::default()
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = crate::router(handle.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let open = |signer: Option<&PrivateKey>| {
            let mut request = format!("ws://{address}/v1/ws")
                .into_client_request()
                .unwrap();
            if let Some(signer) = signer {
                for (name, value) in
                    ::node::signed_req::request_headers(signer, "GET", "/v1/ws", &node_key, b"")
                {
                    request.headers_mut().insert(name, value.parse().unwrap());
                }
            }
            async move { tokio_tungstenite::connect_async(request).await.unwrap().0 }
        };
        // the next frame that is not the connection's own heartbeat.
        async fn answer<S>(socket: &mut S) -> serde_json::Value
        where
            S: futures::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
                + Unpin,
        {
            loop {
                let WsMessage::Text(text) = socket.next().await.expect("socket open").unwrap()
                else {
                    continue;
                };
                let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
                if frame["type"] != "heartbeat" {
                    return frame;
                }
            }
        }
        let subscribe_logs =
            || WsMessage::Text(json!({"op": "subscribe", "topics": ["logs"]}).to_string());

        // the log ring: refused to a bare upgrade and to a stranger's key ...
        for signer in [None, Some(&stranger)] {
            let mut socket = open(signer).await;
            socket.send(subscribe_logs()).await.unwrap();
            let refused = answer(&mut socket).await;
            assert_eq!(refused["type"], "error", "{refused}");
            assert_eq!(refused["topic"], "logs");
            assert_eq!(refused["code"], "forbidden");
            let subscribed = answer(&mut socket).await;
            assert_eq!(subscribed, json!({"type": "subscribed", "topics": {}}));
        }
        // ... and held by the operator's signed upgrade.
        let mut socket = open(Some(&operator)).await;
        socket.send(subscribe_logs()).await.unwrap();
        let subscribed = answer(&mut socket).await;
        assert_eq!(subscribed["type"], "subscribed", "{subscribed}");
        assert!(subscribed["topics"].get("logs").is_some(), "{subscribed}");

        // a run line from a connection that never attached: refused, and the
        // ring never sees it.
        let run = "e".repeat(RUN_OUTPUT_ID_LEN);
        let runs = handle.stream_hub().run_output();
        let mut appended = runs.subscribe_appends();
        let publish = |line: &str| {
            WsMessage::Text(
                json!({"op": "run_output", "id": run, "stream": "stdout", "line": line})
                    .to_string(),
            )
        };
        let mut socket = open(None).await;
        socket.send(publish("spoofed")).await.unwrap();
        let refused = answer(&mut socket).await;
        assert_eq!(refused["type"], "error", "{refused}");
        assert_eq!(refused["code"], "forbidden");
        assert!(
            runs.read_after(&run, 0, 8).0.is_empty(),
            "the line was dropped"
        );
        // the same connection, once it presents the service-link token: lands.
        socket
            .send(WsMessage::Text(
                json!({"op": "compute_attach", "token": TEST_SECRET}).to_string(),
            ))
            .await
            .unwrap();
        socket.send(publish("from the daemon")).await.unwrap();
        let landed = appended.recv().await.unwrap();
        assert_eq!(
            (landed.id.as_str(), landed.line.as_str()),
            (run.as_str(), "from the daemon")
        );
        server.abort();
    }

    /// THE WHOLE REMOTE ADMISSION, END TO END: a real signature over the real
    /// path, against the committed pending set a real node would answer with.
    ///
    /// Four callers, one run. The creator is admitted. Another key, holding a
    /// signature every bit as valid, is not — which is the point: the proof says
    /// WHO, and the committed state says whether that who asked for this work. A
    /// caller with no signature at all never reaches the read, and a dispatch the
    /// pending set does not name is refused without saying so (a run that settled
    /// and a run that was never yours are the same answer, or the refusal becomes
    /// a probe).
    #[tokio::test]
    async fn only_the_key_that_created_a_run_is_admitted_to_its_output() {
        use commonware_cryptography::Signer as _;
        let creator = commonware_cryptography::ed25519::PrivateKey::from_seed(11);
        let stranger = commonware_cryptography::ed25519::PrivateKey::from_seed(12);
        let node_key = vec![0xab; 32];
        let dispatch = "d".repeat(64);

        let (mut handle, mut commands, _hub) = crate::NodeHandle::channel();
        handle.admin.node_key = Some(node_key.clone());
        // the committed answer, as `runs` would give it: one pending run, created
        // by `creator`.
        let pending = runs_wire::PendingRun {
            run_id: "chat\u{1f}channel-a\u{1f}2\u{1f}agent-1".into(),
            dispatch_id: dispatch.clone(),
            agent_id: "agent-1".into(),
            channel_id: "channel-a".into(),
            anchor_seq: 2,
            thread_root: None,
            job_id: None,
            job_claim_height: 0,
            requester: sdk::Origin::External(creator.public_key().as_ref().to_vec()),
            created_at: 0,
        };
        let answers = tokio::spawn(async move {
            while let Some(command) = commands.next().await {
                let crate::NodeCommand::Query { reply, .. } = command else {
                    continue;
                };
                let _ = reply.send(Ok(runs_wire::encode_reply(
                    &runs_wire::RunsReply::PendingRuns(vec![pending.clone()]),
                )));
            }
        });

        let path = format!("/v1/ws?run={dispatch}");
        let signed = |signer: &commonware_cryptography::ed25519::PrivateKey, path: &str| {
            let mut headers = axum::http::HeaderMap::new();
            for (name, value) in
                ::node::signed_req::request_headers(signer, "GET", path, &node_key, b"")
            {
                headers.insert(name, value.parse().expect("a header value"));
            }
            headers
        };

        assert!(
            admit_run_reader(&handle, &dispatch, &signed(&creator, &path), &path)
                .await
                .is_ok(),
            "the key that created the run must be admitted to its output"
        );
        for (who, headers, path) in [
            (
                "a stranger's valid signature",
                signed(&stranger, &path),
                path.clone(),
            ),
            (
                "no signature at all",
                axum::http::HeaderMap::new(),
                path.clone(),
            ),
            (
                // the signature covers the path it was minted for, so asking for
                // another run with it fails the verify, not the authority read.
                "a signature minted for another run",
                signed(&creator, "/v1/ws?run=elsewhere"),
                path.clone(),
            ),
        ] {
            assert!(
                admit_run_reader(&handle, &dispatch, &headers, &path)
                    .await
                    .is_err(),
                "{who} must be refused"
            );
        }
        // and the creator's own proof does not reach a run the pending set does
        // not name.
        let other = "e".repeat(64);
        let other_path = format!("/v1/ws?run={other}");
        assert!(
            admit_run_reader(&handle, &other, &signed(&creator, &other_path), &other_path)
                .await
                .is_err(),
            "a dispatch this node holds no pending run for must be refused"
        );
        drop(handle);
        answers.abort();
    }

    /// A RUN'S CREATOR READS ITS OWN RUN, AND NOTHING ELSE.
    ///
    /// The capability admitted at the upgrade names ONE dispatch. So a remote
    /// app watching the run it asked for needs no workspace secret — and the
    /// same connection asking for a second run, or for a pty, is refused exactly
    /// as a stranger would be. A `bool` here would have handed the first remote
    /// reader every run on the node.
    #[test]
    fn a_runs_creator_reads_that_run_and_no_other_gated_topic() {
        let mine = Some("dispatch-a");
        assert!(
            prepare_topic(
                "run-output:dispatch-a",
                NO_SECRET,
                mine,
                NOT_OPERATOR,
                None,
                None
            )
            .is_ok(),
            "the run this connection proved must admit"
        );
        for someone_elses in ["run-output:dispatch-b", "run-output:"] {
            let Err(ServerFrame::Error { code, .. }) =
                prepare_topic(someone_elses, NO_SECRET, mine, NOT_OPERATOR, None, None)
            else {
                panic!("{someone_elses} must refuse a connection admitted for dispatch-a");
            };
            assert_eq!(code, StreamErrorCode::Forbidden, "{someone_elses}");
        }
        // and the refusal sends a remote reader to the proof it can actually
        // make, rather than to a workspace directory it does not have.
        let Err(ServerFrame::Error { detail, .. }) = prepare_topic(
            "run-output:dispatch-b",
            NO_SECRET,
            mine,
            NOT_OPERATOR,
            None,
            None,
        ) else {
            unreachable!("refused above");
        };
        assert!(detail.contains("?run="), "{detail}");
    }

    /// A wrong secret is exactly as good as no secret — the compare is the gate,
    /// not the presence of the field.
    #[test]
    fn a_wrong_secret_admits_nothing() {
        let handle = handle_with_secret();
        for presented in [None, Some("not-the-secret"), Some("")] {
            let mut states = BTreeMap::new();
            subscribe_topics(
                &handle,
                &mut states,
                vec!["run-output:r1".into()],
                &BTreeMap::new(),
                presented,
                NO_RUN,
                NOT_OPERATOR,
            );
            assert!(
                states.is_empty(),
                "presented {presented:?} admitted a gated topic"
            );
        }
        // a node with NO SERVICE LINK admits nobody, whatever they present.
        let (bare, _cmds, _hub) = crate::NodeHandle::channel();
        let mut states = BTreeMap::new();
        subscribe_topics(
            &bare,
            &mut states,
            vec!["run-output:r1".into()],
            &BTreeMap::new(),
            Some(TEST_SECRET),
            NO_RUN,
            NOT_OPERATOR,
        );
        assert!(states.is_empty(), "a node with no link admits nobody");

        // and NEITHER does a node whose link minted no secret — the case that
        // actually ships. `bin/noded/src/main.rs` passes `None`, and
        // `bin/node/src/boot/surfaces.rs` does too whenever `mint_link_token`
        // fails. It must reach `link_token_matches` itself rather than
        // short-circuiting in `workspace_secret_matches` one level up, which is
        // where the link-less case above stops: an `is_none_or` slip inside
        // that function turns "this node minted no secret" into "this node
        // admits EVERYBODY", and only this case can see it.
        let (unminted, _cmds, _hub) = crate::NodeHandle::channel();
        let unminted = unminted.with_service_link(crate::service_link::ServiceLink::new(None));
        for presented in ["", TEST_SECRET] {
            assert!(
                !unminted.workspace_secret_matches(presented),
                "a link that minted no secret must match nothing, got {presented:?}"
            );
            let mut states = BTreeMap::new();
            subscribe_topics(
                &unminted,
                &mut states,
                vec!["run-output:r1".into()],
                &BTreeMap::new(),
                Some(presented),
                NO_RUN,
                NOT_OPERATOR,
            );
            assert!(
                states.is_empty(),
                "a link with no minted secret admitted {presented:?}"
            );
        }
    }

    #[test]
    fn a_subscribe_at_the_cap_admits_all_and_still_allows_recursoring() {
        let handle = handle_with_secret();
        let mut states = BTreeMap::new();
        let at_cap: Vec<String> = (0..MAX_TOPICS_PER_CONNECTION)
            .map(|i| format!("run-output:r{i}"))
            .collect();
        let frames = subscribe_topics(
            &handle,
            &mut states,
            at_cap.clone(),
            &BTreeMap::new(),
            Some(TEST_SECRET),
            NO_RUN,
            NOT_OPERATOR,
        );
        assert_eq!(states.len(), MAX_TOPICS_PER_CONNECTION);
        assert!(
            frames
                .iter()
                .all(|f| !matches!(f, ServerFrame::Error { .. })),
            "every topic at exactly the cap must admit: {frames:?}"
        );

        // one more NEW topic on top of an already-full connection refuses the
        // WHOLE message as one frame — never a per-topic fan-out — and leaves
        // the held state untouched.
        let mut over = at_cap.clone();
        over.push("run-output:extra".into());
        let refused = subscribe_topics(
            &handle,
            &mut states,
            over,
            &BTreeMap::new(),
            Some(TEST_SECRET),
            NO_RUN,
            NOT_OPERATOR,
        );
        assert_eq!(refused.len(), 1, "one summary refusal, not one per topic");
        assert!(matches!(
            refused[0],
            ServerFrame::Error {
                code: StreamErrorCode::Unavailable,
                ..
            }
        ));
        assert_eq!(states.len(), MAX_TOPICS_PER_CONNECTION);

        // re-subscribing exactly the EXISTING topics (at, not over, the cap)
        // re-cursors, never refuses.
        let again = subscribe_topics(
            &handle,
            &mut states,
            at_cap,
            &BTreeMap::new(),
            Some(TEST_SECRET),
            NO_RUN,
            NOT_OPERATOR,
        );
        assert!(
            again
                .iter()
                .all(|f| !matches!(f, ServerFrame::Error { .. })),
            "re-subscribe at the cap must stay allowed: {again:?}"
        );
        assert_eq!(states.len(), MAX_TOPICS_PER_CONNECTION);
    }

    /// The amplification this fixes: a `Subscribe` naming far more topics than
    /// the connection could ever hold used to walk the ENTIRE vector, pushing
    /// one heap-allocating refusal frame per name (`stream.rs`, pre-fix). It
    /// must now cost one frame regardless of how many names were sent.
    #[test]
    fn a_subscribe_far_over_the_topic_cap_never_fans_out_one_frame_per_topic() {
        let handle = handle_with_secret();
        let mut states = BTreeMap::new();
        let huge: Vec<String> = (0..MAX_TOPICS_PER_CONNECTION + 10_000)
            .map(|i| format!("bogus:{i}"))
            .collect();
        let frames = subscribe_topics(
            &handle,
            &mut states,
            huge,
            &BTreeMap::new(),
            Some(TEST_SECRET),
            NO_RUN,
            NOT_OPERATOR,
        );
        assert_eq!(
            frames.len(),
            1,
            "one refusal for the whole message, not one per requested topic"
        );
        assert!(states.is_empty());
    }

    #[test]
    fn wake_classes_route_to_their_topics() {
        let module = TopicState::Module {
            module: "chat".into(),
            cursor: String::new(),
        };
        let files = TopicState::FilesWatch {
            cursor: String::new(),
        };
        let logs = TopicState::Logs { seq: 0 };
        let run = TopicState::RunOutput {
            id: "r1".into(),
            seq: 0,
        };
        let metrics = TopicState::Metrics { sampled_ms: 0 };
        let peers = TopicState::Peers { sampled_ms: 0 };
        assert!(module.wakes_on(Wake::Block) && files.wakes_on(Wake::Block));
        assert!(!logs.wakes_on(Wake::Block) && !run.wakes_on(Wake::Block));
        assert!(logs.wakes_on(Wake::Logs) && !module.wakes_on(Wake::Logs));
        assert!(run.wakes_on(Wake::RunOutput) && !files.wakes_on(Wake::RunOutput));
        // metrics is time-driven ONLY: a block/log/run wakeup never re-samples
        // it, and no other topic class re-scans on the heartbeat tick.
        assert!(metrics.wakes_on(Wake::Tick) && !metrics.wakes_on(Wake::Block));
        assert!(!metrics.wakes_on(Wake::Logs) && !metrics.wakes_on(Wake::RunOutput));
        // peers is the second snapshot topic and rides the SAME clock: a block
        // wake must never re-sample it, or an idle chain's 2 Hz of nop fillers
        // becomes 2 Hz of whole-registry encodes.
        assert!(peers.wakes_on(Wake::Tick) && !peers.wakes_on(Wake::Block));
        assert!(!peers.wakes_on(Wake::Logs) && !peers.wakes_on(Wake::RunOutput));
        assert!(peers.wakes_on(Wake::All));
        for state in [&module, &files, &logs, &run] {
            assert!(state.wakes_on(Wake::All));
            assert!(!state.wakes_on(Wake::Tick));
        }
        assert!(metrics.wakes_on(Wake::All));
    }

    #[test]
    fn metrics_topic_subscribes_without_a_store_and_ignores_resume() {
        // metrics rides the exposition source, not the index — a daemon with
        // no index store still serves it, and a reconnect's stored cursor is
        // harmless.
        let (state, lagged) = prepare_topic(
            "metrics",
            NO_SECRET,
            NO_RUN,
            NOT_OPERATOR,
            Some(&"1752000000000".to_string()),
            None,
        )
        .expect("topic");
        assert!(lagged.is_none());
        assert_eq!(state.cursor(), "0", "a fresh subscribe never resumes");
    }

    #[tokio::test]
    async fn metrics_catch_up_samples_through_the_wired_exposition() {
        // NO actor: the topic samples the handle's wired exposition source
        // directly, so it stays live while the pump is busy (or absent).
        let (handle, _cmds, _hub) = crate::NodeHandle::channel();
        handle
            .status_cell()
            .wire_exposition(|| "ducktape_blocks_total 5\n".to_string());
        let (mut state, _) =
            prepare_topic("metrics", NO_SECRET, NO_RUN, NOT_OPERATOR, None, None).expect("topic");
        let result = catch_up_metrics("metrics", &mut state, &handle).await;
        assert!(!result.drop_topic);
        match &result.frames[..] {
            [
                ServerFrame::Tail {
                    topic,
                    cursor,
                    item: TailItem::Metrics { time_ms, text },
                },
            ] => {
                assert_eq!(topic, "metrics");
                assert_eq!(text, "ducktape_blocks_total 5\n");
                assert_eq!(cursor, &time_ms.to_string());
                assert_eq!(
                    &state.cursor(),
                    cursor,
                    "the sample instant becomes the topic cursor"
                );
            }
            other => panic!("expected one metrics tail frame, got {other:?}"),
        }
    }

    /// THE TOPIC MUST ACTUALLY DELIVER A PEER. `files:watch` subscribed
    /// cleanly for months and never produced a frame, because nothing asserted
    /// the ITEM — only that the subscribe was admitted. So this reads the row
    /// out of the frame and checks the two things composition can drop: the
    /// counters, which come from the exposition, and the role, which comes
    /// from the separately-published standing and is the half a lane that
    /// cannot read the valset legitimately leaves absent.
    #[tokio::test]
    async fn peers_catch_up_delivers_a_stamped_sample_through_both_sources() {
        let (handle, _cmds, _hub) = crate::NodeHandle::channel();
        handle.status_cell().wire_exposition(|| {
            "network_tracker_directory_connected{peer=\"aa\"} 1000\n\
             network_spawner_messages_sent_total{peer=\"aa\",message=\"data\"} 7\n"
                .to_string()
        });
        handle
            .status_cell()
            .publish_peers(crate::handle::PeersStanding {
                validators: ["aa".to_string()].into_iter().collect(),
                residents: Default::default(),
                height: 42,
                epoch: Some(3),
                builds: Default::default(),
            });

        let (mut state, _) =
            prepare_topic("peers", NO_SECRET, NO_RUN, NOT_OPERATOR, None, None).expect("topic");
        let result = catch_up_peers("peers", &mut state, &handle).await;
        assert!(!result.drop_topic);
        match &result.frames[..] {
            [
                ServerFrame::Tail {
                    topic,
                    cursor,
                    item: TailItem::Peers { time_ms, peers },
                },
            ] => {
                assert_eq!(topic, "peers");
                assert_eq!(cursor, &time_ms.to_string());
                assert_eq!(&state.cursor(), cursor);
                // the committed half, off the published standing
                assert_eq!(peers.height, 42);
                assert_eq!(peers.epoch, Some(3));
                // the live half, off the exposition
                let [peer] = &peers.peers[..] else {
                    panic!("expected exactly one peer, got {:?}", peers.peers);
                };
                assert_eq!(peer.peer, "aa");
                assert!(peer.connected);
                assert_eq!(peer.msgs_sent, 7);
                assert_eq!(
                    peer.role.as_deref(),
                    Some("validator"),
                    "the standing's roles must be stamped onto the sample, or every \
                     row renders with no standing at all"
                );
            }
            other => panic!("expected one peers tail frame, got {other:?}"),
        }
    }

    /// THE DEBOUNCE MUST NEVER SWALLOW A STEADY BEAT.
    ///
    /// The window has two bounds and only one of them is obvious. Too small and
    /// the subscribe's immediate follow-on tick composes the registry a second
    /// time. Too LARGE and the cadence eats itself — and that failure is
    /// invisible to every frame-counting test, because the samples stay 1:1
    /// with the frames delivered; only their RATE halves. Widening this
    /// constant to a full interval leaves the e2e green and merely slower,
    /// which is how it would ship.
    ///
    /// STEADY is the word that matters. A subscribe landing more than a window
    /// into a period does skip the tick right behind it, so its first gap is
    /// up to `interval + window` before it re-aligns and stays on the beat.
    /// That is bought deliberately and no constant avoids it — the two demands
    /// meet only at a window of zero. Do not read this test as forbidding
    /// every skip.
    #[test]
    fn the_debounce_window_never_swallows_a_steady_beat() {
        // the tick riding milliseconds behind the subscribe replay folds away.
        assert!(snapshot_is_fresh(0, 40));

        // THE ONE THAT PINS THE UPPER BOUND. A subscribe lands part-way INTO a
        // heartbeat period, so the next beat arrives that much short of a full
        // interval after it. Treat that as too soon and the topic delivers
        // every OTHER beat, forever.
        let landed_into_the_period = 40;
        assert!(
            !snapshot_is_fresh(landed_into_the_period, HEARTBEAT_INTERVAL_MS),
            "a beat arriving {}ms after the last sample must compose; a window \
             that swallows it halves the topic's cadence",
            HEARTBEAT_INTERVAL_MS - landed_into_the_period
        );

        // and the steady case, from any phase.
        assert!(!snapshot_is_fresh(0, HEARTBEAT_INTERVAL_MS));

        // A STAMP FROM THE FUTURE IS STALE, NOT FRESH. The beat is monotonic
        // and this stamp is wall clock, so an ntp step backwards makes `now`
        // precede the last sample. Saturating arithmetic reads that as age
        // zero — freshest possible — and both overview topics would compose
        // nothing for the length of the jump while heartbeats kept the socket
        // looking healthy.
        assert!(
            !snapshot_is_fresh(5_000, 4_000),
            "a backwards clock step must not read as fresh"
        );
    }

    /// An unwired exposition drops the topic rather than serving an empty peer
    /// set — an empty list and "this daemon cannot answer" are different
    /// answers, and the console must not paint the second as the first.
    #[tokio::test]
    async fn peers_catch_up_drops_the_topic_when_no_exposition_is_wired() {
        let (handle, _cmds, _hub) = crate::NodeHandle::channel();
        let (mut state, _) =
            prepare_topic("peers", NO_SECRET, NO_RUN, NOT_OPERATOR, None, None).expect("topic");
        let result = catch_up_peers("peers", &mut state, &handle).await;
        assert!(result.drop_topic, "an unanswerable topic must be dropped");
        assert!(matches!(
            &result.frames[..],
            [ServerFrame::Error { topic, .. }] if topic == "peers"
        ));
    }

    /// The service link is granted on the TOKEN and nothing else.
    ///
    /// `take_service_link` had no behavioural coverage at all, which left the
    /// deleted build gate's only guard a source lint — and a lint is defeated by
    /// any indirection. This is the direct assertion: present the node's token
    /// and the link is yours, whatever this binary was built from.
    ///
    /// What it CANNOT see: `build_identity()` is `option_env!`, resolved at
    /// compile time, so a test running in a stamped build cannot make the
    /// git-absent case happen. A reintroduced `if build_identity().is_none() {
    /// refuse }` would pass this test and break exactly the checkouts the gate
    /// broke. That specific hole is why the source lint stays — see
    /// `crate::services`'s `no_admission_path_reads_this_node_s_build_stamp`,
    /// which forbids the stamp anywhere in THIS file.
    #[test]
    fn a_service_link_is_granted_on_the_token_alone() {
        const TOKEN: &str = "b0a1c2d3e4f50617";
        let (handle, _cmds, _hub) = crate::NodeHandle::channel();
        let handle =
            handle.with_service_link(crate::service_link::ServiceLink::new(Some(TOKEN.into())));

        // the whole admission: the right kind, the node's own token.
        let (guard, _rx) = take_service_link(&handle, crate::services::AGENT_KIND, TOKEN)
            .expect("the token alone grants the link");

        // and the two refusals that DO exist, so this test also pins that the
        // grant above is not simply "everything succeeds".
        assert!(take_service_link(&handle, "compute", TOKEN).is_err());
        assert!(take_service_link(&handle, crate::services::AGENT_KIND, "wrong").is_err());
        assert!(
            take_service_link(&handle, crate::services::AGENT_KIND, TOKEN).is_err(),
            "first attach wins while the guard lives"
        );

        // the guard's Drop releases the link — the next daemon may claim it.
        drop(guard);
        take_service_link(&handle, crate::services::AGENT_KIND, TOKEN)
            .expect("a released link is claimable again");
    }

    /// A handle with no service link refuses every attach, and that refusal is
    /// about the NODE's wiring, never about a build.
    #[test]
    fn a_node_with_no_service_link_has_nothing_to_give() {
        let (handle, _cmds, _hub) = crate::NodeHandle::channel();
        let Err(refusal) = take_service_link(&handle, crate::services::AGENT_KIND, "any") else {
            panic!("a handle with no service link has nothing to give");
        };
        assert!(refusal.contains("service link is not enabled"), "{refusal}");
    }

    #[tokio::test]
    async fn metrics_catch_up_drops_the_topic_when_no_exposition_is_wired() {
        // no exposition source (an embedder that registers no metrics) — the
        // topic drops with the same `unavailable` shape the http 503 carries.
        let (handle, _cmds, _hub) = crate::NodeHandle::channel();
        let (mut state, _) =
            prepare_topic("metrics", NO_SECRET, NO_RUN, NOT_OPERATOR, None, None).expect("topic");
        let result = catch_up_metrics("metrics", &mut state, &handle).await;
        assert!(result.drop_topic);
        assert!(matches!(
            result.frames.first(),
            Some(ServerFrame::Error {
                code: StreamErrorCode::Unavailable,
                ..
            })
        ));
    }
}
