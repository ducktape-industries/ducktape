//! the compute daemon's work intake: the `watch` verb in its cheapest form.
//!
//! A daemon executes no blocks, so no effect ever reaches it. It discovers its
//! node's assigned work the way a synced RESIDENT already does — by asking the
//! saga module's COMMITTED projection (`SagaQuery::AssignedPending`) for the
//! exact [`WorkerRequest`]s the effect lane would have carried — except the
//! query rides `/v1/query` instead of an in-process `Host`.
//!
//! Each pass offers new assignments to the off-loop [`compute_service::DispatchPool`]
//! ONCE and submits whatever follow-up ops are due. A pass NEVER awaits a
//! provider: the gate verdict is inline and immediate, the CLI runs on a spawned
//! task, and its result re-enters through [`WorkPump::completed`].
//!
//! ## exactly once, across a lossy read
//!
//! The chain is the source of truth, so a missed hint DELAYS work but never
//! loses it. The converse — treating a failed read as an empty projection —
//! would be far worse: emptiness retires entries, and retiring a live attempt
//! re-offers it as fresh work (a second child process for the same paid call,
//! and a computed-but-unsent result silently dropped). So:
//!
//! - an unreadable projection is not an empty one: the pass does nothing;
//! - plain absence needs a CONFIRMING second read before an entry retires;
//! - authoritative supersession (terminal saga, a higher attempt, a changed
//!   assignee) retires on the first read, and cancels the local run;
//! - the pump's own latch — not the pool's in-flight map, which prunes at
//!   delivery — is what keeps a still-pending attempt from being re-offered.
//!
//! A restart re-runs at worst once; the saga module's result singularity
//! collapses the duplicate.

use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::sync::Mutex;

use compute_service::AttemptControl;
use host::worker::{WorkOutcome, Worker};
use noded::node_link::NodeLink;
use saga::{SagaMsg, SagaOrigin, SagaQuery, SagaReply, WorkerRequest};
use sdk::{Event, Msg};
use serde_json::Value;

use crate::work_admission::{self, WorkVerdict};

/// the daemon's committed-read transport, behind the one method the work
/// admission needs. The node's own actor lane wears the same trait in
/// `term_plane`, so both lanes reach one decision through one seam.
#[async_trait::async_trait]
impl work_admission::CommittedReader for NodeLink {
    async fn read(&self, target: &str, request: Vec<u8>) -> Result<Vec<u8>, String> {
        self.query(target, &request).await
    }
}

/// one attempt's idempotency key — what the worker echoes into its result.
type AttemptKey = (String, u32);

/// why an attempt vanished from this daemon's assigned subset.
enum AttemptProjection {
    Active,
    Retired,
}

/// why an announcement needs no further bid from this pump. ONE discriminant —
/// the two reasons are settled differently and must not be confused: a bid we
/// won arrives back through the lease lane, a refusal never does.
enum ClaimState {
    /// an `Accept` was submitted; the saga's first-accept-wins rule decides.
    Bid,
    /// this node does not run that submitter's work.
    ///
    /// ponytail: the DECISION is latched, not just its log — so admitting the
    /// submitter later does not re-open an announcement already seen (the
    /// saga's next attempt, a new key, is decided fresh). Re-deciding every
    /// pass would cost two committed reads per refused announcement per tick.
    NotAdmitted,
}

/// where one assigned attempt sits in the execute-then-submit lifecycle.
enum Stage {
    /// offered to the pool; the provider runs (or queues) on a spawned task.
    /// Nothing to submit yet, and the attempt must NOT be offered again.
    Executing,
    /// the follow-up op is computed and needs a (re)submit.
    Due(Msg),
    /// nothing further to send — the op committed, or the request was a
    /// deliberate skip. Held until committed state retires the attempt, so it
    /// is never re-offered.
    Settled,
}

/// one tracked attempt: its stage plus the retire confirmation. `missed` marks
/// a read that failed to name the attempt; only a SECOND consecutive miss
/// retires the entry.
struct Entry {
    stage: Stage,
    missed: bool,
}

pub(crate) struct WorkPump {
    /// the off-loop execution pool behind the shared `Worker` seam: gate
    /// inline, spawn the provider, return immediately.
    pool: Box<dyn Worker>,
    /// cancellation for attempts already handed to the pool.
    control: AttemptControl,
    /// the node's external submit key — the lease identity queried for, and
    /// the identity `/v1/submit` stamps on every op this pump sends.
    me: Vec<u8>,
    work: HashMap<AttemptKey, Entry>,
    /// announcements this pump has already settled: bid for, or refused
    /// admission.
    ///
    /// SEPARATE from `work`, and that separation is load-bearing: `Accept`
    /// keeps the announcement's `(saga_id, attempt)` and merely fills in the
    /// assignee, so the attempt this node WINS arrives in the lease projection
    /// under the exact same key. Sharing one latch would leave the winner's
    /// entry settled-by-the-claim and the run would never execute — a silent
    /// no-op, which is the worst possible shape for this bug.
    claims: HashMap<AttemptKey, ClaimState>,
    /// this workspace, for the work-admission policy. Read on every FIRST
    /// SIGHTING rather than latched at boot: the node's own terminal lane reads
    /// the same file the same way, and a boot-time copy here would make one
    /// process see `ducktape node work admit` immediately and the other only
    /// after a restart.
    workspace: std::path::PathBuf,
    /// whether the last lease read failed. Latched so an unreachable node says
    /// so ONCE and again when it recovers: a silent return would make a node
    /// that cannot be read look exactly like an idle one, which is the single
    /// most misleading thing this pump could do.
    unreadable: bool,
    record_requesters: compute_service::SessionRecordRequesterMap,
}

impl WorkPump {
    pub(crate) fn new(
        pool: Box<dyn Worker>,
        control: AttemptControl,
        me: Vec<u8>,
        workspace: std::path::PathBuf,
        record_requesters: compute_service::SessionRecordRequesterMap,
    ) -> Self {
        Self {
            pool,
            control,
            me,
            work: HashMap::new(),
            claims: HashMap::new(),
            workspace,
            unreadable: false,
            record_requesters,
        }
    }

    /// how many attempts this pump is tracking.
    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.work.len()
    }

    /// one intake pass: read the committed projections, offer new work, submit
    /// what is due.
    ///
    /// TWO lanes, deliberately independent. The LEASE lane executes work this
    /// node holds; the CLAIM lane only bids for announcements nobody holds yet.
    /// The saga's own projections are disjoint (an accepted announcement leaves
    /// one and enters the other), so running both cannot double-execute.
    pub(crate) async fn tick(&mut self, node: &NodeLink) {
        self.tick_leases(node).await;
        self.tick_claims(node).await;
    }

    /// the LEASE lane: work assigned to this node.
    async fn tick_leases(&mut self, node: &NodeLink) {
        let Some(assigned) = pending(node, &self.me, Lane::Assigned).await else {
            if !self.unreadable {
                self.unreadable = true;
                tracing::warn!(
                    target: "ducktape::saga",
                    reason = "assigned_pending_unreadable",
                    "compute intake cannot read its node's committed work; \
                     execution is paused until it answers"
                );
            }
            return;
        };
        if self.unreadable {
            self.unreadable = false;
            tracing::info!(target: "ducktape::saga", "compute intake reading committed work again");
        }
        let live: HashSet<AttemptKey> = assigned
            .iter()
            .map(|request| (request.saga_id.clone(), request.attempt))
            .collect();
        let missing: Vec<AttemptKey> = self
            .work
            .keys()
            .filter(|key| !live.contains(*key))
            .cloned()
            .collect();
        let mut retired = HashSet::new();
        let mut active = HashSet::new();
        for key in missing {
            match attempt_projection(node, &self.me, &key).await {
                Some(AttemptProjection::Retired) => {
                    retired.insert(key);
                }
                Some(AttemptProjection::Active) => {
                    active.insert(key);
                }
                // inconclusive: preserve the two-read flap tolerance.
                None => {}
            }
        }
        let decided = self.gate(node, assigned).await;
        let due = self.plan(decided, &retired, &active).await;
        for (key, msg) in due {
            self.send(node, &key, msg).await;
        }
    }

    /// This node's work admission, applied to FIRST SIGHTINGS only — before
    /// anything is offered to the pool, so upstream of workspace provisioning
    /// and of any paid call. An attempt already tracked is never re-decided:
    /// its verdict has already become an offer or a refusal op.
    ///
    /// Refused work stays in the returned list and gains an entry carrying its
    /// refusal op, so the pump's ordinary `Due` machinery submits it: a pinned
    /// saga aimed at a host that will never run it reaches `Failed` with a
    /// named reason rather than parking. Undecided work is DROPPED from this
    /// pass and reconsidered on the next — never burn an attempt on a read that
    /// did not answer.
    async fn gate(&mut self, node: &NodeLink, assigned: Vec<WorkerRequest>) -> Vec<WorkerRequest> {
        let mut decided = Vec::with_capacity(assigned.len());
        for request in assigned {
            let key = (request.saga_id.clone(), request.attempt);
            if self.work.contains_key(&key) {
                decided.push(request);
                continue;
            }
            let verdict = self.admits(node, &request.saga_id).await;
            if let Some(requester) = run_requester(node, &request.saga_id).await
                && let Ok(mut requesters) = self.record_requesters.lock()
            {
                requesters.insert(dispatch_id_for(&request.saga_id), requester);
            }
            self.record(request, key, verdict, &mut decided);
        }
        decided
    }

    /// The WRITE half of the lease gate — no I/O, so all three verdicts are
    /// unit-testable without a node. `gate` reads, this writes; neither does the
    /// other's job.
    fn record(
        &mut self,
        request: WorkerRequest,
        key: AttemptKey,
        verdict: WorkVerdict,
        decided: &mut Vec<WorkerRequest>,
    ) {
        match verdict {
            WorkVerdict::Admitted => decided.push(request),
            WorkVerdict::Refused(refusal) => {
                // once per attempt: the entry below is what stops this firing
                // again on every pass. Never the account, only the attempt.
                tracing::warn!(
                    target: "ducktape::saga",
                    attempt = ?key,
                    reason = refusal.reason(),
                    "compute attempt refused"
                );
                let stage = Stage::Due(refusal_op(&request, refusal.reason()));
                self.work.insert(
                    key,
                    Entry {
                        stage,
                        missed: false,
                    },
                );
                // kept in the list so `plan` sees it live and RETAINS the entry
                // it just made; the entry is what stops it reaching the pool.
                decided.push(request);
            }
            // dropped from this pass and reconsidered on the next. NOT tracked,
            // so no attempt is burned on a read that simply did not answer.
            WorkVerdict::AuthorityUnavailable => tracing::debug!(
                target: "ducktape::saga",
                attempt = ?key,
                reason = "work_authority_unavailable",
                "compute attempt not decided; retrying"
            ),
        }
    }

    /// **The** admission decision for this daemon: one call site, both lanes.
    /// `work_admission::both_lanes_route_through_one_verdict` pins that shape.
    ///
    /// The subject is the saga's COMMITTED origin. `/v1/submit` discards a
    /// caller's claimed submitter id and re-signs with the submitting node's own
    /// key, so an `External` origin is a proven node identity — derived, never
    /// asserted.
    async fn admits(&self, node: &NodeLink, saga_id: &str) -> WorkVerdict {
        // no origin to decide on: the read failed, or committed state no longer
        // names the saga. Neither is a refusal — the retire sweep owns the
        // second case and the next pass owns the first.
        let Some(origin) = saga_origin(node, saga_id).await else {
            return WorkVerdict::AuthorityUnavailable;
        };
        work_admission::admit(node, &self.workspace, &self.me, &origin).await
    }

    /// the CLAIM lane: announcements no node holds a lease on.
    ///
    /// The gate answers an announcement with an `Accept` bid when this host can
    /// both run the capability and seat its demands, and with a silent skip
    /// otherwise — it NEVER answers one with an execution, so this lane cannot
    /// start a run. The saga's first-accept-wins rule settles who executes, and
    /// the winner picks the work up through the lease lane on a later pass.
    async fn tick_claims(&mut self, node: &NodeLink) {
        let Some(announcements) = pending(node, &self.me, Lane::Unassigned).await else {
            // the lease lane already logged an unreadable node; a claim is pure
            // upside, so a failed read here is simply a pass that does nothing.
            return;
        };
        let live: HashSet<AttemptKey> = announcements
            .iter()
            .map(|request| (request.saga_id.clone(), request.attempt))
            .collect();
        // an announcement that left the projection was claimed (by us or by
        // someone else) or retired: either way the latch is spent.
        self.claims.retain(|key, _| live.contains(key));

        for request in announcements {
            let key = (request.saga_id.clone(), request.attempt);
            if self.claims.contains_key(&key) {
                continue;
            }
            // whose work, before what work: an announcement this node will not
            // run is never bid for, which costs the saga NOTHING — no attempt
            // is burned and another node may still claim it.
            match self.admits(node, &request.saga_id).await {
                WorkVerdict::Admitted => {}
                WorkVerdict::Refused(refusal) => {
                    tracing::warn!(
                        target: "ducktape::saga",
                        attempt = ?key,
                        reason = refusal.reason(),
                        "compute claim refused"
                    );
                    self.claims.insert(key, ClaimState::NotAdmitted);
                    continue;
                }
                // NOT latched: a read that did not answer must not settle an
                // announcement this node might well run.
                WorkVerdict::AuthorityUnavailable => continue,
            }
            // a skip (no provider, no capacity) is deliberately NOT latched:
            // capacity frees up, and re-gating is a pure, cheap decision.
            let Some(bid) = self.bid(request, &key).await else {
                continue;
            };
            if node.submit(&bid.target, &bid.payload).await.is_ok() {
                self.claims.insert(key, ClaimState::Bid);
            }
        }
    }

    /// offer one announcement to the gate. `Some` is the `Accept` op to submit.
    async fn bid(&self, request: WorkerRequest, key: &AttemptKey) -> Option<Msg> {
        let event = Event {
            source: "saga".into(),
            payload: saga::encode_worker_request(&request),
        };
        match self.pool.run(&event).await {
            Ok(WorkOutcome::Handled(Some(msg))) => Some(msg),
            // a skip this host cannot serve, or a foreign spec shape.
            Ok(WorkOutcome::Handled(None)) | Ok(WorkOutcome::NotMine) => None,
            Err(error) => {
                tracing::warn!(
                    target: "ducktape::saga",
                    attempt = ?key,
                    error = %error,
                    reason = "worker_error",
                    "compute claim skipped"
                );
                None
            }
        }
    }

    /// a completed off-loop run arrived on the result lane: queue its op.
    ///
    /// An attempt committed state no longer names (pruned while the provider
    /// ran) is dropped; its op could only be a deterministic no-op anyway.
    pub(crate) fn completed(&mut self, saga_id: String, attempt: u32, msg: Msg) {
        let key = (saga_id, attempt);
        match self.work.get_mut(&key) {
            Some(Entry {
                stage: stage @ Stage::Executing,
                ..
            }) => *stage = Stage::Due(msg),
            _ => tracing::debug!(
                target: "ducktape::saga",
                attempt = ?key,
                reason = "retired_attempt",
                "completed run dropped"
            ),
        }
    }

    /// the pump core, separated from the reads so it is unit-testable: retire
    /// what committed state proves obsolete, offer newly assigned attempts to
    /// the pool ONCE, and surface every op that needs sending.
    async fn plan(
        &mut self,
        assigned: Vec<WorkerRequest>,
        retired: &HashSet<AttemptKey>,
        active: &HashSet<AttemptKey>,
    ) -> Vec<(AttemptKey, Msg)> {
        let live: HashSet<AttemptKey> = assigned
            .iter()
            .map(|r| (r.saga_id.clone(), r.attempt))
            .collect();
        let newest = assigned
            .iter()
            .fold(HashMap::<String, u32>::new(), |mut newest, request| {
                newest
                    .entry(request.saga_id.clone())
                    .and_modify(|attempt| *attempt = (*attempt).max(request.attempt))
                    .or_insert(request.attempt);
                newest
            });
        let control = self.control.clone();
        let me = &self.me;
        self.work.retain(|key, entry| {
            if live.contains(key) || active.contains(key) {
                entry.missed = false;
                return true;
            }
            // a higher committed attempt of the same saga — including one
            // assigned elsewhere and confirmed by `SagaQuery::Get` — is proof
            // of supersession rather than a flapped projection.
            let superseded =
                retired.contains(key) || newest.get(&key.0).is_some_and(|attempt| *attempt > key.1);
            if !superseded && !entry.missed {
                entry.missed = true;
                if !matches!(entry.stage, Stage::Settled) {
                    tracing::warn!(
                        target: "ducktape::saga",
                        attempt = ?key,
                        reason = "projection_missing_mid_work",
                        "compute attempt will retire on the next absent read"
                    );
                }
                return true;
            }
            if !matches!(entry.stage, Stage::Settled) {
                control.cancel(&key.0, key.1, me);
            }
            false
        });

        let mut due = Vec::new();
        for request in assigned {
            let key = (request.saga_id.clone(), request.attempt);
            if let Some(entry) = self.work.get_mut(&key) {
                // named again: whatever absence was seen did not persist.
                entry.missed = false;
            }
            match self.work.get_mut(&key).map(|entry| &entry.stage) {
                None => {
                    let stage = self.offer(request, &key, &mut due).await;
                    self.work.insert(
                        key,
                        Entry {
                            stage,
                            missed: false,
                        },
                    );
                }
                // the provider is running off-loop; nothing to do.
                Some(Stage::Executing) => {}
                // computed but not successfully submitted yet: offer again.
                Some(Stage::Due(msg)) => due.push((key.clone(), msg.clone())),
                Some(Stage::Settled) => {}
            }
        }
        due
    }

    /// first sighting of an attempt: hand it to the pool exactly once.
    async fn offer(
        &self,
        request: WorkerRequest,
        key: &AttemptKey,
        due: &mut Vec<(AttemptKey, Msg)>,
    ) -> Stage {
        let event = Event {
            source: "saga".into(),
            payload: saga::encode_worker_request(&request),
        };
        match self.pool.run(&event).await {
            // an inline verdict WITH an op (unresolvable capability, non-utf-8
            // payload): due now.
            Ok(WorkOutcome::Handled(Some(msg))) => {
                due.push((key.clone(), msg.clone()));
                Stage::Due(msg)
            }
            // spawned — the result arrives later via `completed`. A deliberate
            // inline skip maps here too; either way the attempt is latched
            // against re-offer and the retire sweep decides what happens next.
            Ok(WorkOutcome::Handled(None)) => Stage::Executing,
            // not a dispatch WorkSpec: never ours to run.
            Ok(WorkOutcome::NotMine) => Stage::Settled,
            Err(error) => {
                // unreachable for DispatchPool (it never errors) — settle
                // rather than spin a broken worker.
                tracing::warn!(
                    target: "ducktape::saga",
                    attempt = ?key,
                    error = %error,
                    reason = "worker_error",
                    "compute attempt settled"
                );
                Stage::Settled
            }
        }
    }

    /// submit one due op. `/v1/submit` re-signs under the NODE's key, which is
    /// the assignee the saga's lease gate checks — so this is the same op, from
    /// the same identity, the in-process pool used to deliver.
    ///
    /// A refusal leaves the entry `Due`: the next pass re-sends while committed
    /// state still names the attempt. Re-sends are duplicate ops at worst — the
    /// saga's result singularity collapses them deterministically.
    async fn send(&mut self, node: &NodeLink, key: &AttemptKey, msg: Msg) {
        match node.submit(&msg.target, &msg.payload).await {
            Ok(_height) => {
                if let Some(entry) = self.work.get_mut(key) {
                    entry.stage = Stage::Settled;
                }
            }
            Err(error) => tracing::debug!(
                target: "ducktape::saga",
                attempt = ?key,
                error = %error,
                reason = "result_submit_failed",
                "compute result will be re-sent"
            ),
        }
    }
}

/// which committed projection a read wants. ONE discriminant, because the two
/// reads differ only in the query and the reply variant they accept.
#[derive(Clone, Copy)]
enum Lane {
    /// work leased to this node — the execute lane.
    Assigned,
    /// announcements nobody holds — the claim lane.
    Unassigned,
}

/// the committed saga projection for one lane, WALKED to the end.
///
/// The module answers a bounded page per call (its read budget is per query,
/// not per projection), so the whole lane is this loop over the reply's
/// cursor — advanced by [`advanced_cursor`], which is what makes it terminate.
///
/// `None` when the node is unreachable, the module absent, or the reply
/// unreadable — a failed read must NEVER masquerade as an empty projection:
/// emptiness retires entries, and retiring a live attempt re-runs it. That
/// holds MID-WALK too, which is why a failed page abandons the whole lane: a
/// truncated projection is an empty one for every entry it did not reach.
async fn pending(node: &NodeLink, me: &[u8], lane: Lane) -> Option<Vec<WorkerRequest>> {
    let mut requests = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let query = match lane {
            Lane::Assigned => SagaQuery::AssignedPending {
                assignee: me.to_vec(),
                after: after.clone(),
            },
            Lane::Unassigned => SagaQuery::UnassignedPending {
                after: after.clone(),
            },
        };
        let reply = node.query("saga", &saga::encode_query(&query)).await.ok()?;
        let page = match (lane, saga::decode_reply(&reply)) {
            (Lane::Assigned, Ok(SagaReply::AssignedPending(page))) => page,
            (Lane::Unassigned, Ok(SagaReply::UnassignedPending(page))) => page,
            _ => return None,
        };
        requests.extend(page.requests);
        let Some(next) = advanced_cursor(after.as_deref(), page.next) else {
            return Some(requests);
        };
        after = Some(next);
    }
}

/// Where the next page resumes, or `None` to STOP the walk.
///
/// The walk terminates because the cursor strictly ascends by saga id, and that
/// argument is THIS loop's to make, not the reply's: a page whose `next`
/// repeats or rewinds — a module/version mismatch, a paging change that hands
/// back its last MATCH instead of its last CANDIDATE — would otherwise spin
/// inside one `tick` forever, growing `requests` without bound, and the compute
/// daemon would never reach its claim lane. `None` sorts below every `Some`, so
/// the first page (no cursor yet) accepts any id.
fn advanced_cursor(after: Option<&str>, next: Option<String>) -> Option<String> {
    next.filter(|cursor| Some(cursor.as_str()) > after)
}

/// The op that fails a refused attempt. Without it a pinned saga aimed at a
/// host that will never run it would sit `Pending` forever — and with no
/// `deadline` the `Crank` can never terminate it either, so a silent skip
/// leaks one consensus record per refusal. The `reason` is the stable token,
/// not prose, and it names no account.
fn refusal_op(request: &WorkerRequest, reason: &str) -> Msg {
    Msg {
        target: "saga".into(),
        payload: saga::encode_msg(&SagaMsg::OracleResult {
            saga_id: request.saga_id.clone(),
            attempt: request.attempt,
            outcome: Err(reason.to_string()),
            usage: None,
        }),
    }
}

/// one saga's committed origin — the work-admission subject.
async fn saga_origin(node: &NodeLink, saga_id: &str) -> Option<SagaOrigin> {
    let reply = node
        .query(
            "saga",
            &saga::encode_query(&SagaQuery::Get {
                saga_id: saga_id.to_string(),
            }),
        )
        .await
        .ok()?;
    match saga::decode_reply(&reply).ok()? {
        SagaReply::Saga(view) => view.map(|view| view.origin),
        SagaReply::NextExpiry(_)
        | SagaReply::AssignedPending(_)
        | SagaReply::UnassignedPending(_) => None,
    }
}

fn dispatch_id_for(saga_id: &str) -> String {
    saga_id
        .rsplit_once('\x1f')
        .map_or(saga_id, |(_, dispatch_id)| dispatch_id)
        .to_owned()
}

async fn run_requester(node: &NodeLink, saga_id: &str) -> Option<Value> {
    let dispatch_id = dispatch_id_for(saga_id);
    let request = serde_json::to_vec(&runs::view::RunsViewQuery::Run { dispatch_id }).ok()?;
    let bytes = node
        .query("runs", &request)
        .await
        .ok()?;
    let runs::view::RunsViewReply::Run(Some(detail)) = serde_json::from_slice(&bytes).ok()? else {
        return None;
    };
    Some(requester_value(&detail.run.requester))
}

fn requester_value(origin: &sdk::Origin) -> Value {
    match origin {
        sdk::Origin::External(key) => serde_json::json!({
            "kind": "external",
            "principal_id": hex::encode(key),
        }),
        sdk::Origin::Module(module_id) => serde_json::json!({
            "kind": "module",
            "module_id": module_id,
        }),
        sdk::Origin::Program(account_id) => serde_json::json!({
            "kind": "program",
            "account_id": account_id,
        }),
        sdk::Origin::System => serde_json::json!({"kind": "system"}),
    }
}

/// Confirm WHY an assigned attempt disappeared. `AssignedPending` cannot show a
/// retry that moved to another node; the per-saga view can. An unreadable or
/// older view is inconclusive and preserves the two-read flap tolerance.
async fn attempt_projection(
    node: &NodeLink,
    me: &[u8],
    key: &AttemptKey,
) -> Option<AttemptProjection> {
    let reply = node
        .query(
            "saga",
            &saga::encode_query(&SagaQuery::Get {
                saga_id: key.0.clone(),
            }),
        )
        .await
        .ok()?;
    match saga::decode_reply(&reply).ok()? {
        SagaReply::Saga(None) => Some(AttemptProjection::Retired),
        SagaReply::Saga(Some(view))
            if view.status.is_terminal()
                || view.attempt > key.1
                || (view.attempt == key.1 && view.assignee.as_deref() != Some(me)) =>
        {
            Some(AttemptProjection::Retired)
        }
        SagaReply::Saga(Some(view))
            if view.attempt == key.1 && view.assignee.as_deref() == Some(me) =>
        {
            Some(AttemptProjection::Active)
        }
        _ => None,
    }
}

/// What the pool handed to the delivery lane. ONE discriminant, because the two
/// have different fates: a result belongs to a tracked attempt and is RETRIED
/// until it commits, while a lease heartbeat is fire-and-forget (the next beat
/// is 10s away and supersedes it).
pub(crate) enum Delivered {
    Result {
        saga_id: String,
        attempt: u32,
        msg: Msg,
    },
    Heartbeat(Msg),
}

impl Delivered {
    /// classify one delivered op.
    ///
    /// The pool sends exactly two shapes down this lane. Anything else is wire
    /// drift, and treating it as a heartbeat (submit once, best effort) is the
    /// honest fallback: it still reaches consensus, it just is not retried.
    pub(crate) fn classify(msg: Msg) -> Self {
        match saga::decode_msg(&msg.payload) {
            Ok(SagaMsg::OracleResult {
                saga_id, attempt, ..
            }) => Delivered::Result {
                saga_id,
                attempt,
                msg,
            },
            _ => Delivered::Heartbeat(msg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// a worker that counts offers and answers with a fixed outcome — the
    /// observable stand-in for the real pool, with no provider and no node.
    struct CountingWorker {
        offers: Arc<AtomicUsize>,
        spawned: bool,
    }

    #[async_trait::async_trait(?Send)]
    impl Worker for CountingWorker {
        async fn run(&self, _event: &Event) -> Result<WorkOutcome, host::worker::Error> {
            self.offers.fetch_add(1, Ordering::SeqCst);
            match self.spawned {
                true => Ok(WorkOutcome::Handled(None)),
                false => Ok(WorkOutcome::Handled(Some(result_msg("s", 0)))),
            }
        }
    }

    const ME: &[u8] = b"compute-node-key";

    /// the real gate's shape: an ANNOUNCEMENT (no assignee) is answered with an
    /// `Accept` bid and never executed; an own LEASE is spawned.
    struct LaneAwareWorker {
        offers: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait(?Send)]
    impl Worker for LaneAwareWorker {
        async fn run(&self, event: &Event) -> Result<WorkOutcome, host::worker::Error> {
            self.offers.fetch_add(1, Ordering::SeqCst);
            let request = saga::decode_worker_request(&event.payload).expect("a work request");
            match request.assignee {
                None => Ok(WorkOutcome::Handled(Some(Msg {
                    target: "saga".into(),
                    payload: saga::encode_msg(&SagaMsg::Accept {
                        saga_id: request.saga_id,
                        attempt: request.attempt,
                    }),
                }))),
                Some(_) => Ok(WorkOutcome::Handled(None)),
            }
        }
    }

    fn announcement(saga_id: &str, attempt: u32) -> WorkerRequest {
        WorkerRequest {
            assignee: None,
            ..request(saga_id, attempt)
        }
    }

    fn result_msg(saga_id: &str, attempt: u32) -> Msg {
        Msg {
            target: "saga".into(),
            payload: saga::encode_msg(&SagaMsg::OracleResult {
                saga_id: saga_id.into(),
                attempt,
                outcome: Ok(b"done".to_vec()),
                usage: None,
            }),
        }
    }

    fn request(saga_id: &str, attempt: u32) -> WorkerRequest {
        WorkerRequest {
            saga_id: saga_id.into(),
            attempt,
            spec: b"{}".to_vec(),
            deadline: None,
            assignee: Some(ME.to_vec()),
        }
    }

    fn new_pump(spawned: bool) -> (WorkPump, Arc<AtomicUsize>) {
        let offers = Arc::new(AtomicUsize::new(0));
        let worker = CountingWorker {
            offers: offers.clone(),
            spawned,
        };
        // a control whose node key matches, so cancellation is exercised
        // against the same identity the pump queries under.
        let pool = compute_service::DispatchPool::with_limit(
            Arc::new(provider_host::ProviderSet::empty()),
            ME.to_vec(),
            Arc::new(|_, _| {}),
            Arc::new(|_| Box::pin(async {})),
            1,
            Default::default(),
            Arc::new(NoProvisioner),
        );
        let control = pool.attempt_control();
        (
            // a workspace with no `work-admit.toml` is the default policy; these
            // tests drive `plan`/`bid` directly, so the gate is exercised by its
            // own unit tests in `work_admission`.
            WorkPump::new(
                Box::new(worker),
                control,
                ME.to_vec(),
                std::path::PathBuf::from("/nonexistent-work-admission-workspace"),
                Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            ),
            offers,
        )
    }

    struct NoProvisioner;
    #[async_trait::async_trait]
    impl compute_service::WorkspaceProvisioner for NoProvisioner {
        async fn provision(
            &self,
            _spec: &compute_service::WorkspaceSpec,
        ) -> Result<Box<dyn compute_service::ProvisionedWorkspace>, String> {
            Err("no provisioner in this test".into())
        }
    }

    #[tokio::test]
    async fn an_attempt_is_offered_exactly_once_across_repeated_reads() {
        let (mut pump, offers) = new_pump(true);
        let empty = HashSet::new();
        for _ in 0..5 {
            let due = pump.plan(vec![request("s", 0)], &empty, &empty).await;
            assert!(due.is_empty(), "a spawned attempt has nothing to send yet");
        }
        assert_eq!(offers.load(Ordering::SeqCst), 1, "offered exactly once");
        assert_eq!(pump.tracked(), 1);
    }

    #[tokio::test]
    async fn an_inline_verdict_is_due_and_stays_due_until_it_is_sent() {
        let (mut pump, _) = new_pump(false);
        let empty = HashSet::new();
        let due = pump.plan(vec![request("s", 0)], &empty, &empty).await;
        assert_eq!(due.len(), 1, "an inline op is due immediately");
        // not sent yet: the next pass re-offers the SAME op rather than
        // re-running the attempt.
        let again = pump.plan(vec![request("s", 0)], &empty, &empty).await;
        assert_eq!(again.len(), 1);
    }

    #[tokio::test]
    async fn plain_absence_needs_two_reads_but_supersession_retires_at_once() {
        let (mut pump, offers) = new_pump(true);
        let empty = HashSet::new();
        pump.plan(vec![request("s", 0)], &empty, &empty).await;
        assert_eq!(pump.tracked(), 1);

        // one absent read: retained (a flapped read must not re-run a live
        // attempt as fresh work).
        pump.plan(Vec::new(), &empty, &empty).await;
        assert_eq!(pump.tracked(), 1, "one absence is not proof");
        // a second: retired.
        pump.plan(Vec::new(), &empty, &empty).await;
        assert_eq!(pump.tracked(), 0, "a confirmed absence retires");

        // a higher attempt of the same saga is authoritative supersession —
        // one read is enough, and the newcomer is offered.
        let (mut pump, offers2) = new_pump(true);
        pump.plan(vec![request("s", 0)], &empty, &empty).await;
        pump.plan(vec![request("s", 1)], &empty, &empty).await;
        assert_eq!(pump.tracked(), 1, "only the newest attempt is tracked");
        assert_eq!(offers2.load(Ordering::SeqCst), 2);
        // the first pump never re-offered after retirement.
        assert_eq!(offers.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_completed_result_becomes_due_and_a_retired_one_is_dropped() {
        let (mut pump, _) = new_pump(true);
        let empty = HashSet::new();
        pump.plan(vec![request("s", 0)], &empty, &empty).await;

        pump.completed("s".into(), 0, result_msg("s", 0));
        let due = pump.plan(vec![request("s", 0)], &empty, &empty).await;
        assert_eq!(due.len(), 1, "a completed run's op is due");

        // an attempt nothing tracks any more drops its late result.
        pump.completed("gone".into(), 7, result_msg("gone", 7));
        let due = pump.plan(vec![request("s", 0)], &empty, &empty).await;
        assert_eq!(due.len(), 1, "the unknown result added nothing");
    }

    #[tokio::test]
    async fn a_claimed_announcement_still_executes_when_its_lease_arrives() {
        // THE regression this lane exists to prevent. `Accept` keeps the
        // announcement's (saga_id, attempt) and only fills in the assignee, so
        // the attempt this node WINS arrives in the lease projection under the
        // exact same key. If the claim latch and the execute latch were one
        // map, the winner's entry would already be settled-by-the-claim and the
        // run would silently never happen.
        let offers = Arc::new(AtomicUsize::new(0));
        let pool = Box::new(LaneAwareWorker {
            offers: offers.clone(),
        });
        let (mut pump, _) = new_pump(true);
        pump.pool = pool;

        // the claim lane bids on the announcement ...
        let bid = pump
            .bid(announcement("s", 0), &("s".into(), 0))
            .await
            .expect("a capable host bids on an announcement");
        assert!(matches!(
            saga::decode_msg(&bid.payload),
            Ok(SagaMsg::Accept { .. })
        ));
        // ... and latches it, exactly as a successful submit would.
        pump.claims.insert(("s".to_string(), 0), ClaimState::Bid);

        // now the SAME key comes back as this node's lease. It must still be
        // offered to the pool and reach Executing.
        let empty = HashSet::new();
        let due = pump.plan(vec![request("s", 0)], &empty, &empty).await;
        assert!(due.is_empty(), "a spawned lease has nothing to send yet");
        assert_eq!(pump.tracked(), 1, "the won lease is tracked for execution");
        assert_eq!(
            offers.load(Ordering::SeqCst),
            2,
            "offered once as an announcement and once as the won lease"
        );
    }

    #[tokio::test]
    async fn a_host_that_cannot_serve_an_announcement_never_bids_and_never_latches() {
        // a skip must not latch: capacity frees up, and re-gating is a pure,
        // cheap decision. Latching a skip would make one busy moment permanent.
        let (pump, _) = new_pump(true);
        struct SkipWorker;
        #[async_trait::async_trait(?Send)]
        impl Worker for SkipWorker {
            async fn run(&self, _event: &Event) -> Result<WorkOutcome, host::worker::Error> {
                Ok(WorkOutcome::Handled(None))
            }
        }
        let mut pump = pump;
        pump.pool = Box::new(SkipWorker);
        assert!(
            pump.bid(announcement("s", 0), &("s".into(), 0))
                .await
                .is_none(),
            "an unservable announcement produces no bid"
        );
        assert!(pump.claims.is_empty(), "a skip is never latched");
    }

    /// **The lease gate's three writes** — the rules themselves, not their
    /// routing.
    ///
    /// Admitted work reaches the pool. REFUSED work becomes a due
    /// `OracleResult(Err)` so a pinned saga aimed at a host that will never run
    /// it reaches `Failed` with a named reason instead of parking forever (a
    /// saga with no `deadline` can never be cranked out of `Pending`, so a
    /// silent skip would leak one consensus record per refusal). UNDECIDED work
    /// is dropped untracked, so a read that did not answer never burns an
    /// attempt.
    #[tokio::test]
    async fn the_lease_gate_writes_one_effect_per_verdict() {
        let (mut pump, _) = new_pump(true);

        let mut decided = Vec::new();
        pump.record(
            request("admitted", 0),
            ("admitted".into(), 0),
            WorkVerdict::Admitted,
            &mut decided,
        );
        assert_eq!(decided.len(), 1, "admitted work goes on to the pool");
        assert_eq!(
            pump.tracked(),
            0,
            "and is tracked by `plan`, not by the gate"
        );

        pump.record(
            request("refused", 0),
            ("refused".into(), 0),
            WorkVerdict::Refused(work_admission::WorkRefusal::NotAdmitted),
            &mut decided,
        );
        assert_eq!(
            decided.len(),
            2,
            "refused work stays in the list so `plan` retains the entry just made"
        );
        let Some(Entry {
            stage: Stage::Due(msg),
            ..
        }) = pump.work.get(&("refused".to_string(), 0))
        else {
            panic!("a refused attempt must carry a due op, or the saga parks forever");
        };
        let Ok(SagaMsg::OracleResult {
            outcome, attempt, ..
        }) = saga::decode_msg(&msg.payload)
        else {
            panic!("the refusal op must be an OracleResult");
        };
        assert_eq!(attempt, 0);
        assert_eq!(outcome, Err("work_not_admitted".to_string()));

        pump.record(
            request("undecided", 0),
            ("undecided".into(), 0),
            WorkVerdict::AuthorityUnavailable,
            &mut decided,
        );
        assert_eq!(
            decided.len(),
            2,
            "an undecided attempt is dropped from the pass"
        );
        assert!(
            !pump.work.contains_key(&("undecided".to_string(), 0)),
            "an undecided attempt is NOT tracked: nothing may burn an attempt on a read \
             that did not answer"
        );
    }

    #[test]
    fn delivery_classification_splits_results_from_lease_heartbeats() {
        let result = Delivered::classify(result_msg("s", 3));
        match result {
            Delivered::Result {
                saga_id, attempt, ..
            } => {
                assert_eq!(saga_id, "s");
                assert_eq!(attempt, 3);
            }
            Delivered::Heartbeat(_) => panic!("an oracle result is not a heartbeat"),
        }

        // the lease heartbeat the pool pushes down the SAME lane: submitted
        // once, never tracked as an attempt result.
        let renew = Msg {
            target: "saga".into(),
            payload: saga::encode_msg(&SagaMsg::RenewLease {
                saga_id: "s".into(),
                attempt: 3,
            }),
        };
        assert!(matches!(
            Delivered::classify(renew),
            Delivered::Heartbeat(_)
        ));
    }

    #[test]
    fn the_page_walk_only_advances_forward() {
        // the lane walk's termination lives here: a page that repeats or rewinds
        // its cursor STOPS the walk instead of spinning the pump tick forever.
        assert_eq!(
            advanced_cursor(None, Some("a".into())).as_deref(),
            Some("a"),
            "the first page accepts any cursor"
        );
        assert_eq!(
            advanced_cursor(Some("a"), Some("b".into())).as_deref(),
            Some("b"),
            "a strictly ascending cursor keeps walking"
        );
        assert_eq!(
            advanced_cursor(Some("b"), Some("b".into())),
            None,
            "a repeated cursor stops the walk"
        );
        assert_eq!(
            advanced_cursor(Some("b"), Some("a".into())),
            None,
            "so does one that rewinds"
        );
        assert_eq!(
            advanced_cursor(Some("b"), None),
            None,
            "and the module's own end-of-index still ends it"
        );
    }
}
