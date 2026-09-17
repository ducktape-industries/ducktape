//! node metrics: the `ducktape_*` Prometheus series and the GET /metrics
//! scrape handler.

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use commonware_runtime::telemetry::metrics::{EncodeLabelSet, MetricsExt as _, Registered, raw};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    ConsensusOperationalStatus, FollowOperationalStatus, IndexOperationalStatus, NetstackSwap,
    NetstackSwapOutcome, NodeHandle, NodePhase, NodeRole, OperationalStatus,
    StoreOperationalStatus, SyncOperationalStatus,
};

// ---------------------------------------------------------------------------
// node metrics: the `ducktape_*` Prometheus series behind GET /metrics.
// shared by every binary serving this surface — the embedded daemon folds a
// block in at submit, the consensus validator at drain — so one Grafana board
// reads them all.
// ---------------------------------------------------------------------------

/// histogram buckets for block apply latency, in SECONDS (Prometheus
/// convention). ~100µs to ~1s — the range one local block apply falls in.
const LATENCY_BUCKETS: [f64; 13] = [
    0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0,
];

/// labels for the per-dispatch counter. kept LOW-CARDINALITY: `module` is the
/// bounded registered set; `origin` is the trigger KIND only — never the
/// specific submitter name or emitter id.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct DispatchLabels {
    module: String,
    origin: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct PhaseLabels {
    role: String,
    phase: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct OutcomeLabels {
    outcome: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct SweepLabels {
    cause: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct SnapshotLabels {
    topic: String,
}

/// A snapshot topic re-composed its sample.
///
/// A closed set: these are the three topics whose catch-up rebuilds a whole
/// document on the heartbeat rather than scanning a cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotTopic {
    /// the whole OpenMetrics exposition.
    Metrics,
    /// the direct-peer sample — a WHOLE registry encode per sample.
    Peers,
    /// the node-status projection; a cell read, effectively free.
    Status,
}

impl SnapshotTopic {
    fn label(self) -> &'static str {
        match self {
            Self::Metrics => "metrics",
            Self::Peers => "peers",
            Self::Status => "status",
        }
    }
}

/// What sent a ws session back to the derived index.
///
/// A closed set, so a caller cannot invent a label and split the series.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SweepCause {
    /// a block that appended index rows — the intended path.
    Block,
    /// the periodic floor. Climbing means rows reached the index without their
    /// writer announcing, and the wake is no longer carrying the plane.
    Backstop,
}

impl SweepCause {
    fn label(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Backstop => "backstop",
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ModuleLabels {
    module: String,
}

/// labels for the retained-store footprint gauges. LOW-CARDINALITY by
/// construction: the two names below and nothing else.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct StoreLabels {
    store: String,
}

/// the node-local stores that retain op payloads forever. the name is both the
/// gauge label and the directory under `<storage>`: `blobstore` holds one flat
/// file per op payload digest, `index` one indexer op row per dispatch per
/// module.
const RETAINED_STORES: [&str; 2] = ["blobstore", "index"];

/// how often [`spawn_store_footprint_sampler`] re-walks the retained stores.
/// they grow by OPS, not by seconds, and the walk is O(files) over a directory
/// that holds one file per op payload — so it is slow-paced, and it never runs
/// on a node's own task.
const STORE_FOOTPRINT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// how long a node may sit below a tip it has heard, with its OWN height
/// unmoved, before the projection stops calling it `serving`.
///
/// THREE consecutive tip polls. Both lanes that hear a peer's `TipCoords` poll
/// on a 12-second tick — the validator's `sync::divergence::ROOT_POLL_TICK` and
/// the parked resident's `constants::RESIDENT_FALLBACK_POLL` — so one silent
/// tick is an unreachable peer, two is a block landing between two polls, and
/// three that all report a tip above us while our own height never moved is
/// not a race. It is a node that has stopped following, and every later poll
/// says the same thing.
///
/// Not a knob: an operator who could tune this could tune away the only
/// sentence that tells a user why their screen is stale.
const BEHIND_AFTER_SECONDS: u64 = 36;

/// The phase one peer-tip reading implies, given the phase this node is in.
///
/// ONLY the serving↔behind pair moves. Every other phase is a lifecycle fact
/// the node asserted about ITSELF — recovering, joining, syncing, validating,
/// draining, halted — and a peer's tip is an observation, never evidence
/// against one of those.
fn phase_after_tip(phase: NodePhase, behind_by: u64, stalled_for: u64) -> NodePhase {
    let follows_a_tip = matches!(phase, NodePhase::Serving | NodePhase::Behind);
    if !follows_a_tip {
        return phase;
    }
    let below_the_tip = behind_by > 0;
    let stopped_advancing = stalled_for >= BEHIND_AFTER_SECONDS;
    if below_the_tip && stopped_advancing {
        return NodePhase::Behind;
    }
    NodePhase::Serving
}

/// the low-cardinality trigger KIND of a dispatch origin — the metrics label.
fn origin_kind(origin: &sdk::Origin) -> &'static str {
    match origin {
        sdk::Origin::External(_) => "external",
        sdk::Origin::Module(_) => "module",
        sdk::Origin::Program(_) => "program",
        sdk::Origin::System => "system",
    }
}

/// the node's own Prometheus series, registered INTO commonware's runtime
/// registry so one `context.encode()` (GET /metrics) serves runtime + app
/// metrics together. each `Registered` handle is retained for the process life;
/// updates go through its `Deref` to the underlying metric.
#[derive(Clone)]
pub struct NodeMetrics {
    block_height: Registered<raw::Gauge>,
    blocks_total: Registered<raw::Counter>,
    apply_latency: Registered<raw::Histogram>,
    dispatch_total: Registered<raw::Family<DispatchLabels, raw::Counter>>,
    node_phase: Registered<raw::Family<PhaseLabels, raw::Gauge>>,
    op_outcomes: Registered<raw::Family<OutcomeLabels, raw::Counter>>,
    consensus_epoch: Registered<raw::Gauge>,
    consensus_view: Registered<raw::Gauge>,
    consensus_validators: Registered<raw::Gauge>,
    consensus_quorum: Registered<raw::Gauge>,
    consensus_reachable: Registered<raw::Gauge>,
    consensus_pending: Registered<raw::Gauge>,
    block_beat_stalled: Registered<raw::Gauge>,
    last_finalized_at: Registered<raw::Gauge>,
    sync_target_height: Registered<raw::Gauge>,
    sync_applied_height: Registered<raw::Gauge>,
    sync_retries: Registered<raw::Counter>,
    sync_failures: Registered<raw::Counter>,
    sync_last_progress_at: Registered<raw::Gauge>,
    network_height: Registered<raw::Gauge>,
    behind_by: Registered<raw::Gauge>,
    checkpoint_height: Registered<raw::Gauge>,
    index_poisoned: Registered<raw::Gauge>,
    index_height: Registered<raw::Family<ModuleLabels, raw::Gauge>>,
    store_bytes: Registered<raw::Family<StoreLabels, raw::Gauge>>,
    store_files: Registered<raw::Family<StoreLabels, raw::Gauge>>,
    stream_index_sweeps: Registered<raw::Family<SweepLabels, raw::Counter>>,
    stream_snapshot_samples: Registered<raw::Family<SnapshotLabels, raw::Counter>>,
    operations: Arc<RwLock<OperationalStatus>>,
}

impl NodeMetrics {
    /// register the `ducktape_*` series on the runtime context (root context, so
    /// names carry no child prefix).
    pub fn register<C: commonware_runtime::Metrics>(context: &C) -> Self {
        let metrics = Self {
            block_height: context.gauge(
                "ducktape_block_height",
                "latest committed local block height",
            ),
            // NB: the registry appends the OpenMetrics `_total` suffix to a
            // counter, so the exposed names are `ducktape_blocks_total` and
            // `ducktape_dispatch_total{…}` — DON'T put `_total` in the name here
            // or it doubles.
            blocks_total: context.counter(
                "ducktape_blocks",
                "committed local blocks since daemon start",
            ),
            // THE FAN-OUT'S ONLY WITNESS. A block wake sweeps the index topics
            // only when the block appended rows, so this counts how often a
            // session was sent back to the store. Before that gate it ticked
            // once per block per subscribed topic per session — on an idle
            // chain, forever, finding nothing. If this climbs on a quiet chain
            // the gate has stopped working, and nothing else would say so.
            stream_index_sweeps: context.family(
                "ducktape_stream_index_sweeps",
                "ws sessions sent to re-scan the derived index, by what woke them",
            ),
            // THE SUBSCRIPTION-IS-THE-BUDGET CLAIM'S ONLY WITNESS. A snapshot
            // topic re-composes its whole document per heartbeat tick, per
            // session, for as long as the session holds it — and `peers`
            // encodes the ENTIRE metrics registry to do it. The whole cost
            // argument is that dropping the socket stops that at the source,
            // which is a claim about session teardown that nothing else
            // observes. If this keeps climbing after the last subscriber
            // leaves, sessions are outliving their sockets and no other series
            // would say so.
            stream_snapshot_samples: context.family(
                "ducktape_stream_snapshot_samples",
                "snapshot-topic documents re-composed for a subscriber, by topic",
            ),
            apply_latency: context.histogram(
                "ducktape_block_apply_latency_seconds",
                "node-local wall-clock cost of applying one block",
                LATENCY_BUCKETS,
            ),
            dispatch_total: context.family(
                "ducktape_dispatch",
                "module dispatches, by module and trigger-origin kind",
            ),
            node_phase: context.family(
                "ducktape_node_phase",
                "whether this node is currently in a bounded role and lifecycle phase",
            ),
            op_outcomes: context.family(
                "ducktape_ops_outcome",
                "finalized member operations by applied or rejected outcome",
            ),
            consensus_epoch: context.gauge("ducktape_consensus_epoch", "current consensus epoch"),
            consensus_view: context.gauge(
                "ducktape_consensus_view",
                "latest locally finalized view in the current epoch",
            ),
            consensus_validators: context.gauge(
                "ducktape_consensus_validators",
                "validators in the current epoch",
            ),
            consensus_quorum: context.gauge(
                "ducktape_consensus_quorum",
                "validators required to finalize in the current epoch",
            ),
            consensus_reachable: context.gauge(
                "ducktape_consensus_reachable_validators",
                "current validators reachable by this node, including itself when a member",
            ),
            consensus_pending: context.gauge(
                "ducktape_consensus_pending_ops",
                "operations staged locally or waiting in the consensus orderer",
            ),
            block_beat_stalled: context.gauge(
                "ducktape_block_beat_stalled_seconds",
                "seconds since this node last sealed a height, 0 while the chain is beating",
            ),
            last_finalized_at: context.gauge(
                "ducktape_last_finalized_timestamp_seconds",
                "unix timestamp of this node's latest finalized block",
            ),
            sync_target_height: context.gauge(
                "ducktape_statesync_target_height",
                "target boundary height of the local state-sync attempt",
            ),
            sync_applied_height: context.gauge(
                "ducktape_statesync_applied_height",
                "latest height installed by the local state-sync attempt",
            ),
            sync_retries: context.counter(
                "ducktape_statesync_retries",
                "local state-sync retries since process start",
            ),
            sync_failures: context.counter(
                "ducktape_statesync_failures",
                "failed local state-sync attempts since process start",
            ),
            sync_last_progress_at: context.gauge(
                "ducktape_statesync_last_progress_timestamp_seconds",
                "unix timestamp of the latest local state-sync progress",
            ),
            network_height: context.gauge(
                "ducktape_network_height",
                "finalized height the peer this node last polled answered with",
            ),
            behind_by: context.gauge(
                "ducktape_behind_by",
                "heights between this node's served height and the peer tip it last heard",
            ),
            checkpoint_height: context.gauge(
                "ducktape_checkpoint_height",
                "height of the latest durable recovery checkpoint",
            ),
            index_poisoned: context.gauge(
                "ducktape_index_poisoned",
                "whether the derived index has stopped accepting writes after a failure",
            ),
            index_height: context.family(
                "ducktape_index_height",
                "latest fully indexed height by bounded module id",
            ),
            store_bytes: context.family(
                "ducktape_store_bytes",
                "bytes on disk held by a node-local retained store; nothing prunes these",
            ),
            store_files: context.family(
                "ducktape_store_files",
                "files on disk held by a node-local retained store; nothing prunes these",
            ),
            operations: Arc::new(RwLock::new(OperationalStatus {
                phase_since: unix_seconds(),
                ..OperationalStatus::default()
            })),
        };
        metrics.set_role_phase(NodeRole::Unknown, NodePhase::Starting);
        metrics
    }

    /// fold one applied block into the series: height, count, this node's
    /// wall-clock apply latency, and the per-module dispatch counters.
    pub fn record_block(&self, height: u64, latency_us: u64, dispatches: &[host::DispatchRecord]) {
        self.block_height.set(height as i64);
        self.record_finalized_now();
        self.blocks_total.inc();
        // microseconds → seconds for the Prometheus convention.
        self.apply_latency.observe(latency_us as f64 / 1_000_000.0);
        for d in dispatches {
            self.dispatch_total
                .get_or_create(&DispatchLabels {
                    module: d.module.clone(),
                    origin: origin_kind(&d.origin).to_string(),
                })
                .inc();
        }
    }

    /// One ws session re-scanned the derived index, labelled by what sent it.
    ///
    /// The SPLIT is the point, not the total. `block` is the intended path.
    /// `backstop` climbing means rows reached the index without their writer
    /// announcing — the gate is then being carried by a 30s floor instead of
    /// the wake, which is a bug upstream that nothing else would report.
    pub fn record_index_sweep(&self, cause: SweepCause) {
        self.stream_index_sweeps
            .get_or_create(&SweepLabels {
                cause: cause.label().to_string(),
            })
            .inc();
    }

    /// One snapshot topic re-composed its document for one session.
    pub fn record_snapshot_sample(&self, topic: SnapshotTopic) {
        self.stream_snapshot_samples
            .get_or_create(&SnapshotLabels {
                topic: topic.label().to_string(),
            })
            .inc();
    }

    /// Record deterministic finalized operation outcomes.
    pub fn record_op_outcomes(&self, applied: usize, rejected: usize) {
        for (outcome, count) in [("applied", applied), ("rejected", rejected)] {
            self.op_outcomes
                .get_or_create(&OutcomeLabels {
                    outcome: outcome.to_string(),
                })
                .inc_by(count as u64);
        }
    }

    /// the latest committed local block height — the same gauge
    /// `ducktape_block_height` exposes, for a node-local task that must
    /// compare committed state against the height it holds (the netstack
    /// governance reconciler's activation floor).
    pub fn block_height(&self) -> u64 {
        u64::try_from(self.block_height.get()).unwrap_or(0)
    }

    /// follow the committed height WITHOUT recording a block apply — the
    /// validator lane calls this for rejected frames (a deterministic no-op
    /// advances the height but is not a sample worth the block series; the
    /// idle heartbeat nop lands here, so it never pollutes the histogram).
    pub fn record_height(&self, height: u64) {
        self.block_height.set(height as i64);
        self.record_finalized_now();
    }

    /// Change the bounded lifecycle coordinates and update the status snapshot
    /// and phase metric together. Old coordinates remain present at zero so a
    /// dashboard does not retain a stale `1` after a transition.
    pub fn set_role_phase(&self, role: NodeRole, phase: NodePhase) {
        let mut status = self.operations.write().expect("operations lock poisoned");
        let old = PhaseLabels {
            role: status.role.as_str().to_string(),
            phase: status.phase.as_str().to_string(),
        };
        self.node_phase.get_or_create(&old).set(0);
        if status.role != role || status.phase != phase {
            status.phase_since = unix_seconds();
        }
        status.role = role;
        status.phase = phase;
        self.node_phase
            .get_or_create(&PhaseLabels {
                role: role.as_str().to_string(),
                phase: phase.as_str().to_string(),
            })
            .set(1);
    }

    pub fn operational_status(&self) -> OperationalStatus {
        self.operations
            .read()
            .expect("operations lock poisoned")
            .clone()
    }

    /// the shared operations projection itself, for the status cell's live
    /// overlay ([`crate::StatusCell::wire_metrics`]).
    pub(crate) fn operations_handle(&self) -> Arc<RwLock<OperationalStatus>> {
        Arc::clone(&self.operations)
    }

    pub fn update_consensus(
        &self,
        epoch: u64,
        view: u64,
        validators: u64,
        reachable_validators: u64,
        pending_ops: u64,
    ) {
        let quorum = quorum(validators);
        self.consensus_epoch.set(epoch as i64);
        self.consensus_view.set(view as i64);
        self.consensus_validators.set(validators as i64);
        self.consensus_quorum.set(quorum as i64);
        self.consensus_reachable.set(reachable_validators as i64);
        self.consensus_pending.set(pending_ops as i64);
        let mut status = self.operations.write().expect("operations lock poisoned");
        // the stall is the drain's to write and this refresh is throttled, so
        // carry the live value across rather than rebuilding it to zero — a
        // wedge that ONLY this projection could show must not be erased by the
        // refresh that is supposed to publish it.
        let block_beat_stalled_seconds = status
            .consensus
            .as_ref()
            .map_or(0, |c| c.block_beat_stalled_seconds);
        status.consensus = Some(ConsensusOperationalStatus {
            epoch,
            view,
            validators,
            quorum,
            reachable_validators,
            pending_ops,
            block_beat_stalled_seconds,
        });
    }

    /// the halt detector's reading, every drain turn.
    ///
    /// Separate from [`Self::update_consensus`] because the cadences differ:
    /// operations refresh on a throttle, the block beat is observed on every
    /// turn, and a stall that is only visible between two refreshes is exactly
    /// the one worth seeing.
    pub fn record_block_beat(&self, stalled_for: std::time::Duration) {
        let seconds = stalled_for.as_secs();
        self.block_beat_stalled.set(seconds as i64);
        let mut status = self.operations.write().expect("operations lock poisoned");
        // only into a projection that already exists. Minting one here would
        // publish zeros for epoch, view, validators and quorum — a status body
        // that reads as a node with no consensus at all, which is a worse lie
        // than a missing field. The gauge above is unconditional, so a wedge in
        // the seconds before the first operations refresh is still on /metrics.
        if let Some(consensus) = status.consensus.as_mut() {
            consensus.block_beat_stalled_seconds = seconds;
        }
    }

    /// Fold one peer's answered tip into the follow projection, and let it move
    /// this node between `serving` and `behind`.
    ///
    /// The two lanes that already poll a peer's `TipCoords` call this with what
    /// came back — the validator's root-divergence watch and the parked
    /// resident's fallback poll — so a node's own lag costs the mesh nothing it
    /// was not already spending. A node serving state the network left behind
    /// is otherwise indistinguishable from a healthy one on every surface a
    /// user can see.
    pub fn record_peer_tip(&self, network_height: u64) {
        self.fold_peer_tip(network_height, unix_seconds());
    }

    /// [`Self::record_peer_tip`] with the clock handed in — the seam a test
    /// drives the stall window through without sleeping through it.
    fn fold_peer_tip(&self, network_height: u64, now: u64) {
        let behind_by = network_height.saturating_sub(self.block_height());
        self.network_height.set(network_height as i64);
        self.behind_by.set(behind_by as i64);
        let (role, phase, stalled_for) = {
            let mut status = self.operations.write().expect("operations lock poisoned");
            status.follow = Some(FollowOperationalStatus {
                network_height,
                behind_by,
                heard_at: now,
            });
            // the last time this node's OWN height moved. a node that has
            // finalized nothing since the phase began is measured from the
            // phase change instead, so entering `serving` grants the same
            // window rather than reading as an instant stall.
            let progressed_at = status.last_finalized_at.unwrap_or(status.phase_since);
            (status.role, status.phase, now.saturating_sub(progressed_at))
        };
        let next = phase_after_tip(phase, behind_by, stalled_for);
        if next == phase {
            return;
        }
        self.set_role_phase(role, next);
        // at most once per crossing (the early return above is the latch), so
        // this stays a lifecycle fact even on a 12-second poll.
        tracing::info!(
            target: "ducktape::statesync",
            event = "node_phase_transition",
            role = role.as_str(),
            phase = next.as_str(),
            reason = "peer_tip",
            network_height,
            behind_by,
            stalled_for,
            "a peer's answered tip moved this node's follow phase"
        );
    }

    pub fn begin_sync(&self, source: Option<String>, target_height: u64) {
        self.sync_target_height.set(target_height as i64);
        let mut status = self.operations.write().expect("operations lock poisoned");
        let prior = status.sync.take().unwrap_or_default();
        status.sync = Some(SyncOperationalStatus {
            source,
            target_height,
            ..prior
        });
    }

    pub fn record_sync_progress(&self, applied_height: u64) {
        let now = unix_seconds();
        self.sync_applied_height.set(applied_height as i64);
        self.sync_last_progress_at.set(now as i64);
        let mut status = self.operations.write().expect("operations lock poisoned");
        let sync = status.sync.get_or_insert_with(Default::default);
        sync.applied_height = applied_height;
        sync.last_progress_at = Some(now);
        sync.last_error = None;
    }

    pub fn record_sync_retry(&self, error: impl Into<String>) {
        self.sync_retries.inc();
        let mut status = self.operations.write().expect("operations lock poisoned");
        let sync = status.sync.get_or_insert_with(Default::default);
        sync.retries += 1;
        sync.last_error = Some(error.into());
    }

    pub fn record_sync_failure(&self, error: impl Into<String>) {
        self.sync_failures.inc();
        let mut status = self.operations.write().expect("operations lock poisoned");
        let sync = status.sync.get_or_insert_with(Default::default);
        sync.failures += 1;
        sync.last_error = Some(error.into());
    }

    pub fn update_storage<I, S>(&self, checkpoint_height: u64, index_poisoned: bool, indexes: I)
    where
        I: IntoIterator<Item = (S, u64)>,
        S: Into<String>,
    {
        self.checkpoint_height.set(checkpoint_height as i64);
        self.index_poisoned.set(i64::from(index_poisoned));
        let indexes: Vec<_> = indexes
            .into_iter()
            .map(|(module, applied_height)| IndexOperationalStatus {
                module: module.into(),
                applied_height,
            })
            .collect();
        for index in &indexes {
            self.index_height
                .get_or_create(&ModuleLabels {
                    module: index.module.clone(),
                })
                .set(index.applied_height as i64);
        }
        let mut status = self.operations.write().expect("operations lock poisoned");
        status.storage.checkpoint_height = checkpoint_height;
        status.storage.index_poisoned = index_poisoned;
        status.storage.indexes = indexes;
    }

    /// publish the retained stores' on-disk footprint, gauges and projection
    /// together — the same shape [`Self::update_storage`] gives the index
    /// watermarks.
    ///
    /// THESE TWO NUMBERS ONLY CLIMB. Every applied op payload is written to
    /// `<storage>/blobstore` under its digest and to an indexer op row, and
    /// nothing removes either (#1309), so an operator's only warning that a
    /// node is filling its disk is the SLOPE of these gauges.
    pub fn update_store_footprint(&self, stores: Vec<StoreOperationalStatus>) {
        for store in &stores {
            let labels = StoreLabels {
                store: store.store.clone(),
            };
            self.store_bytes
                .get_or_create(&labels)
                .set(store.bytes as i64);
            self.store_files
                .get_or_create(&labels)
                .set(store.files as i64);
        }
        let mut status = self.operations.write().expect("operations lock poisoned");
        status.storage.stores = stores;
    }

    /// Name the machine the reachability plane runs on — at boot, and again
    /// after every swap that took. A refused swap does NOT call this: the
    /// current machine keeps running, so the name must not move.
    pub fn set_netstack_execution(&self, backend: impl Into<String>, code_hash: Option<String>) {
        let mut status = self.operations.write().expect("operations lock poisoned");
        let netstack = status.netstack.get_or_insert_with(Default::default);
        netstack.backend = backend.into();
        netstack.code_hash = code_hash;
    }

    /// Record one swap attempt's outcome against the height it landed at.
    /// `reason` is the plane's refusal string, and is `None` on a swap that
    /// took.
    pub fn record_netstack_swap(&self, outcome: NetstackSwapOutcome, reason: Option<String>) {
        let at_height = u64::try_from(self.block_height.get()).unwrap_or(0);
        let mut status = self.operations.write().expect("operations lock poisoned");
        status
            .netstack
            .get_or_insert_with(Default::default)
            .last_swap = Some(NetstackSwap {
            outcome,
            reason,
            at_height,
        });
    }

    fn record_finalized_now(&self) {
        let now = unix_seconds();
        self.last_finalized_at.set(now as i64);
        let mut status = self.operations.write().expect("operations lock poisoned");
        status.last_finalized_at = Some(now);
    }
}

/// sample the retained stores under `storage` forever, off every node task.
///
/// The walk is O(files) over a flat directory that gains one file per op
/// payload, so it runs on `spawn_blocking` and never on the caller's thread —
/// this measures a growth problem, it must not become one. Started once per
/// process by the binaries that own a storage directory; a store directory
/// that does not exist reads as zero rather than an error, because a role that
/// keeps no blobs is not a fault.
pub fn spawn_store_footprint_sampler(metrics: NodeMetrics, storage: std::path::PathBuf) {
    tokio::spawn(async move {
        loop {
            let roots = storage.clone();
            let Ok(sampled) = tokio::task::spawn_blocking(move || sample_stores(&roots)).await
            else {
                // the blocking pool is gone: the process is shutting down.
                return;
            };
            // NO EVENT HERE. these numbers are a gauge and a projection field,
            // both read on demand; a per-tick line saying the same thing would
            // need a plane of its own (`ducktape::blobstore` and
            // `ducktape::index` each cover half of it) and would evict 4096
            // lines of the log ring a day to repeat what `/metrics` holds.
            metrics.update_store_footprint(sampled);
            tokio::time::sleep(STORE_FOOTPRINT_INTERVAL).await;
        }
    });
}

/// every retained store's footprint, in the fixed [`RETAINED_STORES`] order.
fn sample_stores(storage: &std::path::Path) -> Vec<StoreOperationalStatus> {
    RETAINED_STORES
        .iter()
        .map(|store| {
            let (bytes, files) = dir_footprint(&storage.join(store));
            StoreOperationalStatus {
                store: (*store).to_string(),
                bytes,
                files,
            }
        })
        .collect()
}

/// `(bytes, files)` under `root`, recursively. An unreadable directory or
/// entry contributes nothing: a footprint is an observation, and a partial one
/// beats refusing to report at all.
fn dir_footprint(root: &std::path::Path) -> (u64, u64) {
    let mut bytes = 0u64;
    let mut files = 0u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                pending.push(entry.path());
                continue;
            }
            bytes = bytes.saturating_add(meta.len());
            files = files.saturating_add(1);
        }
    }
    (bytes, files)
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn quorum(validators: u64) -> u64 {
    validators.saturating_mul(2) / 3 + u64::from(validators > 0)
}

/// the OpenMetrics content type a Prometheus scraper negotiates for `/metrics`.
const OPENMETRICS_CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// GET /metrics — the Prometheus scrape surface. encodes the runtime
/// registry (which the daemon's `ducktape_*` series are registered into)
/// through the handle's wired exposition source — the registry is shared
/// state, so a scrape never crosses the command lane and stays live while a
/// sync/catch-up stage has the pump busy.
pub(crate) async fn metrics(State(handle): State<NodeHandle>) -> Response {
    match handle.status_cell().exposition() {
        Some(body) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, OPENMETRICS_CONTENT_TYPE)],
            body,
        )
            .into_response(),
        None => crate::error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "no metrics exposition is wired on this daemon",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The netstack projection: absent until a plane names a backend, then the
    /// backend, then one swap outcome stamped with the height it landed at. A
    /// REFUSED swap is recorded WITHOUT moving `backend` — the running machine
    /// continues, and a status that said otherwise would send an operator
    /// chasing a swap that never happened.
    #[test]
    fn netstack_projection_records_the_backend_and_the_last_swap() {
        use commonware_runtime::Runner as _;

        commonware_runtime::deterministic::Runner::default().start(|context| async move {
            let metrics = NodeMetrics::register(&context);
            assert!(metrics.operational_status().netstack.is_none());

            metrics.set_netstack_execution("starting", None);
            let netstack = metrics.operational_status().netstack.unwrap();
            assert_eq!(netstack.backend, "starting");
            assert_eq!(netstack.code_hash, None);
            assert!(netstack.last_swap.is_none());

            metrics.record_height(7);
            metrics.set_netstack_execution("guest", Some("abc".into()));
            metrics.record_netstack_swap(NetstackSwapOutcome::Swapped, None);
            let netstack = metrics.operational_status().netstack.unwrap();
            assert_eq!(netstack.backend, "guest");
            assert_eq!(
                netstack.last_swap,
                Some(NetstackSwap {
                    outcome: NetstackSwapOutcome::Swapped,
                    reason: None,
                    at_height: 7,
                })
            );

            metrics.record_height(9);
            metrics.record_netstack_swap(
                NetstackSwapOutcome::Refused,
                Some("foreign contract".into()),
            );
            let netstack = metrics.operational_status().netstack.unwrap();
            assert_eq!(netstack.backend, "guest", "a refusal must not move backend");
            assert_eq!(netstack.code_hash.as_deref(), Some("abc"));
            metrics.set_netstack_execution("failed", None);
            assert_eq!(
                metrics.operational_status().netstack.unwrap().code_hash,
                None
            );
            assert_eq!(
                netstack.last_swap,
                Some(NetstackSwap {
                    outcome: NetstackSwapOutcome::Refused,
                    reason: Some("foreign contract".into()),
                    at_height: 9,
                })
            );
        });
    }

    #[test]
    fn operational_snapshot_and_scrape_follow_the_same_updates() {
        use commonware_runtime::{Metrics as _, Runner as _};

        commonware_runtime::deterministic::Runner::default().start(|context| async move {
            let metrics = NodeMetrics::register(&context);
            metrics.set_role_phase(NodeRole::Validator, NodePhase::Validating);
            metrics.update_consensus(3, 9, 4, 3, 2);
            metrics.record_op_outcomes(5, 1);
            metrics.begin_sync(Some("peer-a".into()), 42);
            metrics.record_sync_retry("manifest unavailable");
            metrics.record_sync_progress(40);
            metrics.update_storage(36, true, [("chat", 39), ("files", 40)]);

            let status = metrics.operational_status();
            assert_eq!(status.role, NodeRole::Validator);
            assert_eq!(status.phase, NodePhase::Validating);
            assert_eq!(status.consensus.as_ref().unwrap().quorum, 3);
            assert_eq!(status.sync.as_ref().unwrap().applied_height, 40);
            assert_eq!(status.sync.as_ref().unwrap().retries, 1);
            assert!(status.storage.index_poisoned);

            let scrape = context.encode();
            for sample in [
                r#"ducktape_node_phase{role="validator",phase="validating"} 1"#,
                "ducktape_consensus_quorum 3",
                "ducktape_consensus_reachable_validators 3",
                r#"ducktape_ops_outcome_total{outcome="applied"} 5"#,
                r#"ducktape_ops_outcome_total{outcome="rejected"} 1"#,
                "ducktape_statesync_target_height 42",
                "ducktape_statesync_applied_height 40",
                "ducktape_statesync_retries_total 1",
                "ducktape_checkpoint_height 36",
                r#"ducktape_index_height{module="files"} 40"#,
                "ducktape_index_poisoned 1",
            ] {
                assert!(scrape.contains(sample), "missing {sample:?}:\n{scrape}");
            }
            assert!(
                !scrape.contains("peer-a"),
                "sync source leaked into an unbounded metric label:\n{scrape}"
            );
        });
    }

    /// the halt detector writes on every drain turn and the operations refresh
    /// is throttled, so the two race on one projection. The refresh REBUILDS
    /// the consensus block, so it has to carry the stall across — otherwise the
    /// refresh that exists to publish a wedge is what erases it.
    #[test]
    fn an_operations_refresh_does_not_erase_the_stall_under_it() {
        use commonware_runtime::{Metrics as _, Runner as _};

        commonware_runtime::deterministic::Runner::default().start(|context| async move {
            let metrics = NodeMetrics::register(&context);
            metrics.update_consensus(3, 9, 4, 3, 2);
            metrics.record_block_beat(std::time::Duration::from_secs(600));
            assert_eq!(
                metrics
                    .operational_status()
                    .consensus
                    .unwrap()
                    .block_beat_stalled_seconds,
                600
            );

            // the throttled refresh lands mid-wedge
            metrics.update_consensus(3, 9, 4, 3, 2);
            assert_eq!(
                metrics
                    .operational_status()
                    .consensus
                    .unwrap()
                    .block_beat_stalled_seconds,
                600,
                "the refresh rebuilt the projection and dropped the outage"
            );

            assert!(
                context.encode().contains("ducktape_block_beat_stalled_seconds 600"),
                "the gauge a dashboard draws the outage from"
            );

            // and a beating chain clears it on both surfaces
            metrics.record_block_beat(std::time::Duration::ZERO);
            assert_eq!(
                metrics
                    .operational_status()
                    .consensus
                    .unwrap()
                    .block_beat_stalled_seconds,
                0
            );
            assert!(
                context.encode().contains("ducktape_block_beat_stalled_seconds 0")
            );
        });
    }

    /// the growth an operator has to see coming: what the retained stores hold
    /// on disk, on the gauges AND on the projection, from ONE walk (#1309).
    #[test]
    fn the_retained_stores_report_what_they_hold_on_disk() {
        use commonware_runtime::{Metrics as _, Runner as _};

        let root = tempfile::tempdir().expect("tempdir");
        // the blobstore's real shape: one flat file per op payload.
        std::fs::create_dir_all(root.path().join("blobstore")).expect("blobstore dir");
        for (name, len) in [("aa", 3usize), ("bb", 5)] {
            std::fs::write(root.path().join("blobstore").join(name), vec![0u8; len])
                .expect("blob file");
        }
        // the index's: per-module subdirectories, so the walk has to recurse.
        std::fs::create_dir_all(root.path().join("index").join("chat")).expect("index dir");
        std::fs::write(root.path().join("index").join("chat").join("db"), [1u8; 7]).expect("db");

        let sampled = sample_stores(root.path());
        assert_eq!(
            sampled,
            vec![
                StoreOperationalStatus {
                    store: "blobstore".into(),
                    bytes: 8,
                    files: 2,
                },
                StoreOperationalStatus {
                    store: "index".into(),
                    bytes: 7,
                    files: 1,
                },
            ],
        );

        commonware_runtime::deterministic::Runner::default().start(|context| async move {
            let metrics = NodeMetrics::register(&context);
            metrics.update_store_footprint(sample_stores(root.path()));

            assert_eq!(
                metrics.operational_status().storage.stores,
                sample_stores(root.path()),
                "the projection carries the same numbers the gauges do",
            );
            let scrape = context.encode();
            for sample in [
                r#"ducktape_store_bytes{store="blobstore"} 8"#,
                r#"ducktape_store_files{store="blobstore"} 2"#,
                r#"ducktape_store_bytes{store="index"} 7"#,
            ] {
                assert!(scrape.contains(sample), "missing {sample:?}:\n{scrape}");
            }
        });
    }

    /// A node that has heard a tip above its own and has not advanced for the
    /// window stops calling itself `serving` — and carries the gap it means.
    ///
    /// No clock is waited on: the stall is handed to the fold seam, which is
    /// the same value the poll would have computed 36 seconds later.
    #[test]
    fn a_node_below_a_tip_it_heard_reads_behind_once_its_own_height_stops_moving() {
        use commonware_runtime::{Metrics as _, Runner as _};

        // the pure verdict first, over the whole window: only a node that is
        // BOTH below the tip and no longer advancing is behind, and only the
        // serving pair moves at all.
        assert_eq!(
            phase_after_tip(NodePhase::Serving, 601, BEHIND_AFTER_SECONDS),
            NodePhase::Behind
        );
        assert_eq!(
            phase_after_tip(NodePhase::Serving, 601, BEHIND_AFTER_SECONDS - 1),
            NodePhase::Serving,
            "a gap alone is a node one poll behind a busy chain, not a wedge"
        );
        assert_eq!(
            phase_after_tip(NodePhase::Serving, 0, BEHIND_AFTER_SECONDS * 10),
            NodePhase::Serving,
            "an idle chain nobody is advancing is not a node that stopped following"
        );
        assert_eq!(
            phase_after_tip(NodePhase::Behind, 0, BEHIND_AFTER_SECONDS),
            NodePhase::Serving,
            "catching up releases the phase"
        );
        for asserted in [
            NodePhase::Recovering,
            NodePhase::Joining,
            NodePhase::Syncing,
            NodePhase::Validating,
            NodePhase::Draining,
            NodePhase::Halted,
        ] {
            assert_eq!(
                phase_after_tip(asserted, 601, BEHIND_AFTER_SECONDS * 10),
                asserted,
                "a peer's tip is not evidence against a phase the node asserted"
            );
        }

        commonware_runtime::deterministic::Runner::default().start(|context| async move {
            let metrics = NodeMetrics::register(&context);
            metrics.set_role_phase(NodeRole::Resident, NodePhase::Serving);
            // the dognet resident: frozen at 155 while the founder passed 756.
            metrics.record_height(155);
            let froze_at = metrics
                .operational_status()
                .last_finalized_at
                .expect("a folded height stamps progress");

            metrics.fold_peer_tip(756, froze_at + BEHIND_AFTER_SECONDS - 1);
            let status = metrics.operational_status();
            assert_eq!(status.phase, NodePhase::Serving);
            assert_eq!(
                status.follow,
                Some(FollowOperationalStatus {
                    network_height: 756,
                    behind_by: 601,
                    heard_at: froze_at + BEHIND_AFTER_SECONDS - 1,
                }),
                "the gap is reported from the first poll, before the phase moves"
            );

            metrics.fold_peer_tip(756, froze_at + BEHIND_AFTER_SECONDS);
            let status = metrics.operational_status();
            assert_eq!(status.phase, NodePhase::Behind);
            assert_eq!(status.role, NodeRole::Resident, "the role does not move");
            assert_eq!(status.follow.expect("a tip was heard").behind_by, 601);

            let scrape = context.encode();
            for sample in [
                "ducktape_network_height 756",
                "ducktape_behind_by 601",
                r#"ducktape_node_phase{role="resident",phase="behind"} 1"#,
            ] {
                assert!(scrape.contains(sample), "missing {sample:?}:\n{scrape}");
            }

            // and following again clears it, on the very next poll.
            metrics.record_height(756);
            metrics.fold_peer_tip(756, froze_at + BEHIND_AFTER_SECONDS * 4);
            let status = metrics.operational_status();
            assert_eq!(status.phase, NodePhase::Serving);
            assert_eq!(status.follow.expect("a tip was heard").behind_by, 0);
        });
    }

    /// a store directory that does not exist is zero, not a panic: a role that
    /// keeps no blobs still reports.
    #[test]
    fn a_missing_store_directory_reads_as_zero() {
        let root = tempfile::tempdir().expect("tempdir");
        assert_eq!(dir_footprint(&root.path().join("blobstore")), (0, 0));
    }
}
