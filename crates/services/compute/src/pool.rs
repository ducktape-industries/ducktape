//! the off-loop execution pool: [`DispatchPool`] is the [`Worker`] real
//! hosts run.
//!
//! the worker step splits in two. the CHEAP half — decode routing and the
//! lease gate ([`crate::gate`]) — stays inline in `run()`, so the host's
//! event loop keeps its deterministic accept/skip verdicts. the EXPENSIVE
//! half — the provider CLI call, minutes-long under a 300s default timeout —
//! is handed to an injected spawner and `run()` returns "handled, result
//! later" immediately. the completed result re-enters through an injected
//! delivery lane as an ordinary submitted op, which is exactly what the
//! oracle-as-op contract expects: dispatch's never-pop-stack mailbox already
//! delivers results in a LATER block, so execution timing is invisible to
//! consensus.
//!
//! the pool is runtime-agnostic on purpose: it knows how to gate, dedup, and
//! cap — WHERE the future runs ([`SpawnFn`]) and WHERE the result goes
//! ([`DeliverFn`]) are the embedding host's business (bin/node's select-loop
//! mpsc lane, bin/noded's command channel).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::{BoxFuture, Either, select};
use host::worker::{WorkOutcome, Worker};
use provider_host::{AirlockConfig, ProviderSet, RunCancellation};
use sdk::{Event, Msg};
use tokio::sync::Semaphore;

use crate::module_contracts::{AdmissionPolicy, RESOURCE_UNAVAILABLE_RESULT};
use crate::provision::{SharedProvisioner, WorkspaceSpec, assemble_runner_result, bind_workspace};
use crate::{
    AttemptOutput, ExecJob, Gated, ResourceLedger, attempt_output, clean_error, gate,
    oracle_result_with_usage, renew_lease,
};

/// how many provider runs may execute concurrently unless
/// `DUCKTAPE_MAX_CONCURRENT_RUNS` says otherwise.
pub const DEFAULT_MAX_CONCURRENT_RUNS: usize = 4;

/// how often a running attempt renews its saga lease (one `RenewLease` op per
/// running attempt per beat). a recipe sets its lease in views, so this must
/// land well inside the shortest lease a recipe asks for; shorter only spends
/// more ops.
const LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(10);
/// the unit tests' renew beat, so a renewal is observable in milliseconds.
const TEST_LEASE_RENEW_INTERVAL: Duration = Duration::from_millis(25);
/// how long a provision or commit may block before the attempt gives up its
/// provider slot (see [`workspace_step_timeout`]). shorter frees slots sooner
/// but turns more merely slow steps into late ones held for cleanup.
const WORKSPACE_STEP_TIMEOUT: Duration = Duration::from_secs(60);
/// the unit tests' threshold, so a late step is observable without a
/// wall-clock minute.
const TEST_WORKSPACE_STEP_TIMEOUT: Duration = Duration::from_millis(250);

fn lease_renew_interval() -> Duration {
    if cfg!(test) {
        TEST_LEASE_RENEW_INTERVAL
    } else {
        LEASE_RENEW_INTERVAL
    }
}

/// Which host execution lane owns a pool future.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnKind {
    /// Cheap reservation/semaphore waiting. This lane may share runtime workers.
    Queued,
    /// An admitted run whose Drop may synchronously prove child/container absence.
    TeardownOwner,
}

/// Hand one Send future to the appropriate supervised host lane. Hosts must
/// isolate [`SpawnKind::TeardownOwner`] from shared runtime workers. The pool
/// only requests that lane after both resource admission and a provider permit,
/// so its dedicated owner count is bounded by the configured run concurrency.
pub type SpawnFn = Arc<dyn Fn(SpawnKind, BoxFuture<'static, ()>) + Send + Sync>;

/// deliver one completed `OracleResult` op to the host's submit lane. runs
/// on the spawned task, so it may await (a bounded channel send, a command
/// round-trip) without touching the host loop.
pub type DeliverFn = Arc<dyn Fn(Msg) -> BoxFuture<'static, ()> + Send + Sync>;

/// the concurrency cap: `DUCKTAPE_MAX_CONCURRENT_RUNS` when set to a
/// positive integer (the `DUCKTAPE_PROVIDER_TIMEOUT_SECS` precedent), else
/// [`DEFAULT_MAX_CONCURRENT_RUNS`].
pub fn max_concurrent_runs_from_env() -> usize {
    std::env::var("DUCKTAPE_MAX_CONCURRENT_RUNS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_CONCURRENT_RUNS)
}

/// Resolve a run's named gateway credential into a self-host broker source, on
/// the node that will execute the run. Implemented by the node binary (the only
/// place the committed-query lane and the overlay gateway are reachable); the
/// pool holds it behind this seam so credential-less runs and unit tests need no
/// node.
///
/// ROUTING ONLY. An implementation resolves WHERE the owner's gateway is and
/// WHAT to pin, and decides nothing about who may draw on it — it cannot: the
/// grant is settled by the lender, against the account the lender's own node
/// stamps when this node makes the gateway hop, which no implementation here can
/// see or predict. A local re-decision could only ever disagree with the lender,
/// and once did.
///
/// So the only `Err` left is a name this node cannot route (unregistered
/// credential, no browser gateway, owner without a `.duck` handle). It fails the
/// attempt before any provider spawns; a REFUSED grant fails slightly later, at
/// `start_broker`, still before the sandbox spawns and still before any paid
/// call.
///
/// `saga_id` is carried so the resolved config can name WHICH WORK the session
/// draws for — a pointer the LENDER resolves in its own committed state. Passing
/// it on is not a decision: an implementation reads nothing out of it.
#[async_trait::async_trait]
pub trait CredentialResolver: Send + Sync {
    async fn resolve(&self, credential: &str, saga_id: &str) -> Result<Resolved, String>;
}

/// A resolved credential: the self-host airlock config the broker draws on.
///
/// It carries no account, and must not. The sealed session the broker opens
/// names the credential and nothing about who is acting; identity enters exactly
/// once, at the gateway hop, where it is stamped by the lender's node rather than
/// asserted by this one.
pub struct Resolved {
    pub airlock: AirlockConfig,
}

pub type SharedCredentialResolver = Arc<dyn CredentialResolver>;

/// Resolve the run's named credential (if any) into the run context's airlock,
/// on the executing node. A credential with no resolver wired, or a resolver
/// refusal, fails the attempt — the caller turns the `Err` into the saga's
/// `OracleResult(Err)` for this attempt, with no provider spawn.
async fn resolve_credential_into(
    prepared: &mut crate::envelope::Prepared,
    saga_id: &str,
    resolver: &Option<SharedCredentialResolver>,
) -> Result<(), String> {
    let Some(name) = prepared.credential.take() else {
        return Ok(());
    };
    let Some(resolver) = resolver else {
        return Err("this node has no credential resolver".into());
    };
    let resolved = resolver.resolve(&name, saga_id).await?;
    prepared.ctx.airlock = Some(resolved.airlock);
    Ok(())
}

fn run_key_for(saga_id: &str) -> String {
    saga_id
        .rsplit_once('\x1f')
        .map_or(saga_id, |(_, dispatch_id)| dispatch_id)
        .to_string()
}

/// one attempt's in-flight identity — the same `(saga_id, attempt)`
/// idempotency key the saga itself dedups results on.
type AttemptKey = (String, u32);

#[derive(Clone, Copy, PartialEq, Eq)]
enum AttemptState {
    Running,
    Cancelling,
    Delivering,
}

impl AttemptState {
    /// the stable log token for the state a completion found when it lost the
    /// delivery race (see [`settle_attempt`]).
    fn token(self) -> &'static str {
        match self {
            AttemptState::Running => "running",
            AttemptState::Cancelling => "attempt_cancelled",
            AttemptState::Delivering => "attempt_already_delivering",
        }
    }
}

struct RunningAttempt {
    state: AttemptState,
    cancellation: RunCancellation,
    reservation: Option<crate::ReservationGuard>,
}

type RunningAttempts = Arc<Mutex<HashMap<AttemptKey, RunningAttempt>>>;

/// Cloneable host-local cancellation handle. Consensus cancellation reaches a
/// validator through a worker-control effect; a resident uses the same handle
/// after committed state confirms an attempt disappeared twice.
#[derive(Clone)]
pub struct AttemptControl {
    node_key: Vec<u8>,
    attempts: RunningAttempts,
}

impl AttemptControl {
    /// Cancel exactly this node's matching live attempt. The map is the single
    /// completion/cancel linearizer: only `Running` can become `Cancelling`.
    /// The task retains its resource reservation until provider teardown and
    /// any late workspace work has settled and cleaned up.
    pub fn cancel(&self, saga_id: &str, attempt: u32, assignee: &[u8]) -> bool {
        if assignee != self.node_key {
            return false;
        }
        let mut attempts = self.attempts.lock().expect("attempts lock");
        let Some(running) = attempts.get_mut(&(saga_id.to_string(), attempt)) else {
            return false;
        };
        if running.state != AttemptState::Running {
            return false;
        }
        running.state = AttemptState::Cancelling;
        running.cancellation.cancel();
        true
    }
}

/// Panic/shutdown-safe finalizer for one spawned attempt. A key cannot be
/// reinserted while this guard's map entry exists, so no generation is needed.
struct AttemptTaskGuard {
    key: AttemptKey,
    attempts: RunningAttempts,
}

impl Drop for AttemptTaskGuard {
    fn drop(&mut self) {
        let removed = self
            .attempts
            .lock()
            .expect("attempts lock")
            .remove(&self.key);
        drop(removed);
    }
}

/// Production worker for dispatch `WorkSpec` saga events: gate inline,
/// execute on a spawned task, submit the result through the delivery lane.
///
/// Providers installed in this pool must observe [`RunCancellation`] in their
/// [`provider_host::RunContext`], terminate and wait their exact child process
/// tree/container, and only then resolve. The pool intentionally retains the
/// attempt and its resource reservation until that provider future resolves.
pub struct DispatchPool {
    providers: Arc<ProviderSet>,
    /// this node's external submit key — compared against a request's
    /// `assignee` to decide whether the lease is ours to execute.
    node_key: Vec<u8>,
    spawn: SpawnFn,
    deliver: DeliverFn,
    /// caps concurrent provider runs; acquired INSIDE the spawned task so
    /// over-cap work queues there, never on the host loop.
    semaphore: Arc<Semaphore>,
    /// Caps the full lifetime of dedicated teardown owners, including late
    /// workspace cleanup and result delivery after the provider permit is free.
    /// The bound allows one late tail plus one provider per concurrency slot.
    owner_semaphore: Arc<Semaphore>,
    /// attempts executing locally right now. a redelivered `WorkerRequest`
    /// for an in-flight attempt is a claimed skip — never a second child
    /// process for the same paid call. pruned when the attempt's result has
    /// been handed to the delivery lane.
    inflight: RunningAttempts,
    /// materializes/commits/cleans a per-run workspace — injected by the node
    /// binary, where duckfs-client + the actor lane are reachable. Portable
    /// execution fails loudly if a host has not wired it.
    provisioner: SharedProvisioner,
    /// the host-local load ledger: announcements claim only current fit;
    /// assigned work that fits total capacity queues in `spawn_exec`, then
    /// reserves before the semaphore acquire. `Arc`-wrapped so the spawned
    /// task can hold its own cheap clone alongside `providers`.
    ledger: Arc<ResourceLedger>,
    /// resolves a run's named gateway credential into a self-host broker source
    /// on THIS node. `None` on a node with no resolver wired (tests, an embedder
    /// that never lends credentials) — a run carrying a credential name then
    /// fails loudly rather than silently running on the host's own source.
    credential_resolver: Option<SharedCredentialResolver>,
}

impl DispatchPool {
    /// an explicit concurrency cap (tests; embedders with their own policy)
    /// and announced resource capacity. an empty `capacity` is the direct
    /// (bare) node: only demandless jobs ever fit its ledger.
    pub fn with_limit(
        providers: Arc<ProviderSet>,
        node_key: Vec<u8>,
        spawn: SpawnFn,
        deliver: DeliverFn,
        limit: usize,
        capacity: BTreeMap<String, u64>,
        provisioner: SharedProvisioner,
    ) -> Self {
        let limit = limit.max(1);
        let owner_limit = limit.checked_mul(2).unwrap_or(limit);
        Self {
            providers,
            node_key,
            spawn,
            deliver,
            semaphore: Arc::new(Semaphore::new(limit)),
            owner_semaphore: Arc::new(Semaphore::new(owner_limit)),
            inflight: Arc::new(Mutex::new(HashMap::new())),
            provisioner,
            ledger: Arc::new(ResourceLedger::new(capacity)),
            credential_resolver: None,
        }
    }

    /// Wire the node's credential resolver (chainable). Kept off the
    /// constructors' positional lists so the many `with_limit`/`new` call sites
    /// stay untouched; a node calls this once at build time. Absent = a run that
    /// names a credential fails resolve (no host-source fallback).
    pub fn with_credential_resolver(mut self, resolver: SharedCredentialResolver) -> Self {
        self.credential_resolver = Some(resolver);
        self
    }

    /// how many attempts are executing (or queued for the semaphore) right
    /// now — observability plus the test seam for dedup/prune assertions.
    pub fn in_flight(&self) -> usize {
        self.inflight.lock().expect("inflight lock").len()
    }

    pub fn attempt_control(&self) -> AttemptControl {
        AttemptControl {
            node_key: self.node_key.clone(),
            attempts: self.inflight.clone(),
        }
    }

    /// spawn one gated job. the returned future owns everything it needs
    /// (`Arc`s all the way down), so the pool itself never blocks on it.
    fn spawn_exec(&self, key: AttemptKey, cancellation: RunCancellation, job: ExecJob) {
        let providers = self.providers.clone();
        let deliver = self.deliver.clone();
        let semaphore = self.semaphore.clone();
        let owner_semaphore = self.owner_semaphore.clone();
        let inflight = self.inflight.clone();
        let ledger = self.ledger.clone();
        let provisioner = self.provisioner.clone();
        let credential_resolver = self.credential_resolver.clone();
        let executing_node = provider_host::execution_node_id(&self.node_key);
        let owner_spawn = self.spawn.clone();
        let attempt_guard = AttemptTaskGuard {
            key: key.clone(),
            attempts: inflight.clone(),
        };
        (self.spawn)(
            SpawnKind::Queued,
            Box::pin(async move {
                let attempt_owner = attempt_guard;
                let admission = {
                    let admission = async {
                        // `run()` already stashed a converted Accept claim (or a
                        // FailFast reservation) on the attempt before spawning;
                        // only a job with neither — a directly pinned lease that
                        // was never announced — waits for capacity here.
                        let already_reserved = {
                            let mut attempts = inflight.lock().expect("attempts lock");
                            attempts
                                .get_mut(&key)
                                .and_then(|running| running.reservation.take())
                        };
                        let reservation = match already_reserved {
                            Some(reservation) => reservation,
                            None => match job.admission {
                                AdmissionPolicy::Queue => {
                                    let reservation_key =
                                        format!("{}:{}", job.saga_id, job.attempt);
                                    let Some(reservation) = ledger
                                        .reserve_when_available(
                                            &reservation_key,
                                            &job.demands,
                                            &cancellation,
                                        )
                                        .await
                                    else {
                                        return Err(
                                            "attempt cancelled while waiting for resources".into(),
                                        );
                                    };
                                    reservation
                                }
                                AdmissionPolicy::FailFast => {
                                    return Err(
                                        "attempt disappeared before resource admission".into()
                                    );
                                }
                            },
                        };
                        {
                            let mut attempts = inflight.lock().expect("attempts lock");
                            let Some(running) = attempts.get_mut(&key) else {
                                return Err("attempt disappeared before resource admission".into());
                            };
                            if running.state != AttemptState::Running {
                                return Err("attempt cancelled while waiting for resources".into());
                            }
                            running.reservation = Some(reservation);
                        }
                        // Bound the whole dedicated-owner lifetime separately from
                        // provider concurrency. A completed provider may release its
                        // permit before late commit/cleanup/delivery, but it keeps this
                        // owner permit until the dedicated future actually exits.
                        let owner_permit = tokio::select! {
                            permit = owner_semaphore.acquire_owned() => {
                                permit.expect("owner semaphore is never closed")
                            }
                            _ = cancellation.cancelled() => {
                                return Err("attempt cancelled".into());
                            }
                        };
                        // over-cap providers queue HERE, on their own task. the
                        // heartbeat below already runs while they wait, so a healthy
                        // local queue does not lose its lease before execution starts.
                        let permit = tokio::select! {
                            permit = semaphore.acquire_owned() => {
                                permit.expect("run semaphore is never closed")
                            }
                            _ = cancellation.cancelled() => {
                                return Err("attempt cancelled".into());
                            }
                        };
                        // Both branches can become ready in the same scheduler turn.
                        // A permit win must not start a provider after cancellation.
                        if cancellation.is_cancelled() {
                            return Err("attempt cancelled before provider start".into());
                        }
                        let reservation = {
                            let mut attempts = inflight.lock().expect("attempts lock");
                            let Some(running) = attempts.get_mut(&key) else {
                                return Err("attempt disappeared before teardown handoff".into());
                            };
                            if running.state != AttemptState::Running {
                                return Err("attempt cancelled before teardown handoff".into());
                            }
                            running
                                .reservation
                                .take()
                                .expect("an admitted attempt owns its ledger reservation")
                        };
                        Ok((reservation, permit, owner_permit))
                    };
                    let heartbeat_deliver = deliver.clone();
                    let heartbeat_saga = job.saga_id.clone();
                    let heartbeat_attempt = job.attempt;
                    let heartbeat = async move {
                        loop {
                            tokio::time::sleep(lease_renew_interval()).await;
                            heartbeat_deliver(renew_lease(&heartbeat_saga, heartbeat_attempt))
                                .await;
                        }
                    };
                    futures::pin_mut!(admission, heartbeat);
                    match select(admission, heartbeat).await {
                        Either::Left((admission, _)) => admission,
                        Either::Right(_) => unreachable!("lease heartbeat loop never completes"),
                    }
                };
                let (reservation, permit, owner_permit) = match admission {
                    Ok(admission) => admission,
                    Err(error) => {
                        settle_attempt(&inflight, &key, &job, Err(error), None, &deliver).await;
                        return;
                    }
                };

                // This is the ownership boundary for #40. The host maps only this
                // post-admission future to a supervised dedicated thread. Queue
                // length therefore cannot create threads, while unexpected future
                // destruction runs exact provider/container cleanup off shared
                // runtime workers and keeps both admission guards alive.
                owner_spawn(
                    SpawnKind::TeardownOwner,
                    Box::pin(async move {
                        let _attempt_guard = attempt_owner;
                        let _owner_permit = owner_permit;
                        let owned_reservation = reservation;
                        let run = async {
                            // Re-resolve inside the owner: ProviderSet is immutable for
                            // the process lifetime and the owner now holds its Arc.
                            if cancellation.is_cancelled() {
                                Err("attempt cancelled before provider start".into())
                            } else {
                                match providers.resolve(&job.capability) {
                                    Ok(provider) => match crate::envelope::prepare(&job.input) {
                                        Ok(mut prepared) => {
                                            prepared.ctx.run_key = Some(run_key_for(&job.saga_id));
                                            prepared.ctx.executing_node = Some(executing_node);
                                            prepared.ctx.limits = job.demands.clone();
                                            prepared.ctx.cancellation = Some(cancellation.clone());
                                            // resolve a named credential into
                                            // ctx.airlock BEFORE the provider
                                            // spawns: a refusal fails the
                                            // attempt with no paid call.
                                            match resolve_credential_into(
                                                &mut prepared,
                                                &job.saga_id,
                                                &credential_resolver,
                                            )
                                            .await
                                            {
                                                Ok(()) => execute(
                                                    &job,
                                                    prepared,
                                                    provider,
                                                    &provisioner,
                                                    &cancellation,
                                                    permit,
                                                )
                                                .await
                                                .map_err(clean_error),
                                                Err(error) => Err(clean_error(error)),
                                            }
                                        }
                                        Err(error) => Err(clean_error(error)),
                                    },
                                    Err(error) => Err(clean_error(error)),
                                }
                            }
                        };
                        let heartbeat_deliver = deliver.clone();
                        let heartbeat_saga = job.saga_id.clone();
                        let heartbeat_attempt = job.attempt;
                        let heartbeat = async move {
                            loop {
                                tokio::time::sleep(lease_renew_interval()).await;
                                heartbeat_deliver(renew_lease(&heartbeat_saga, heartbeat_attempt))
                                    .await;
                            }
                        };
                        // `run` is declared after both admission guards, so destroying
                        // this future drops the live provider/workspace owner first,
                        // then releases the ledger reservation, then the owner slot.
                        futures::pin_mut!(run, heartbeat);
                        let outcome = match select(run, heartbeat).await {
                            Either::Left((outcome, _)) => outcome,
                            Either::Right(_) => {
                                unreachable!("lease heartbeat loop never completes")
                            }
                        };
                        settle_attempt(
                            &inflight,
                            &key,
                            &job,
                            outcome,
                            Some(owned_reservation),
                            &deliver,
                        )
                        .await;
                    }),
                );
            }),
        );
    }
}

async fn settle_attempt(
    inflight: &RunningAttempts,
    key: &AttemptKey,
    job: &ExecJob,
    outcome: Result<AttemptOutput, String>,
    owned_reservation: Option<crate::ReservationGuard>,
    deliver: &DeliverFn,
) {
    // Error/timeout results are submitted like any other. Cancellation has
    // already latched `Cancelling`, so it loses this completion race and is
    // deliberately suppressed.
    let (outcome, usage) = match outcome {
        Ok(output) => (Ok(output.bytes), output.usage),
        Err(error) => (Err(error), None),
    };
    // won → deliver; lost → the reason it lost. A suppressed completion is a
    // run that finished and whose result never reaches consensus: its saga
    // stays Pending behind an expiring lease, and nothing anywhere used to
    // record that it happened.
    let (suppressed, queued_reservation) = {
        let mut attempts = inflight.lock().expect("attempts lock");
        match attempts.get_mut(key) {
            Some(running) if running.state == AttemptState::Running => {
                running.state = AttemptState::Delivering;
                (None, running.reservation.take())
            }
            Some(running) => (Some(running.state.token()), running.reservation.take()),
            None => (Some("attempt_not_tracked"), None),
        }
    };
    // Both the pre-handoff and admitted paths converge here. No waiter may
    // start until provider termination and late workspace cleanup have settled.
    drop(owned_reservation);
    drop(queued_reservation);
    let Some(reason) = suppressed else {
        tracing::debug!(
            target: "ducktape::compute",
            saga = %job.saga_id,
            attempt = job.attempt,
            failed = outcome.is_err(),
            "submitting an attempt's result"
        );
        deliver(oracle_result_with_usage(
            &job.saga_id,
            job.attempt,
            outcome,
            usage,
        ))
        .await;
        return;
    };
    tracing::warn!(
        target: "ducktape::compute",
        saga = %job.saga_id,
        attempt = job.attempt,
        reason,
        "a finished attempt's result was NOT submitted"
    );
}

/// provision → bind → run → commit → assemble → cleanup, at the dispatch
/// boundary on the spawned task.
///
/// Portable execution requires a wired provisioner. The winning attempt's
/// bytes are the host-assembled `RunnerResult` (prose + receipt).
/// commit runs ONLY after a successful provider run (provider failure yields a
/// saga `Err` with no `output_ref`). Cleanup ownership is retained (W5), either
/// synchronously, including after a late storage future settles. A
/// commit-mechanism failure produces a degraded receipt (R4) —
/// the run's answer is never lost to a receipt-plumbing error.
/// the workspace bracket's host-side threshold (#298): provision and commit
/// can block on actor/spawn-blocking work. Crossing the threshold releases the
/// provider slot, but the attempt retains resource admission and heartbeat
/// fail-closed until the late operation settles and cleanup completes. This
/// prevents host-side storage work from overlapping a replacement beyond the
/// node's aggregate resource cap. The model call itself is bounded separately
/// (X3, in the provider). Tests shrink the window so a late step is
/// observable without a wall-clock minute.
fn workspace_step_timeout() -> Duration {
    if cfg!(test) {
        TEST_WORKSPACE_STEP_TIMEOUT
    } else {
        WORKSPACE_STEP_TIMEOUT
    }
}

async fn execute(
    job: &ExecJob,
    prepared: crate::envelope::Prepared,
    provider: &dyn provider_host::Provider,
    provisioner: &SharedProvisioner,
    cancellation: &RunCancellation,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<AttemptOutput, String> {
    let mut permit = Some(permit);
    let crate::envelope::Prepared {
        input,
        mut ctx,
        workspace: plan,
        // the credential name was consumed by the pool's resolver seam (it set
        // `ctx.airlock`) before this executor ran.
        credential: _,
    } = prepared;
    // the REQUESTED sink (Chain when the envelope carried none) — echoed on
    // the assembled RunnerResult below so runs' delivery can route it.
    let sink = plan.sink;
    let spec = WorkspaceSpec {
        run_id: format!("{}:{}", job.saga_id, job.attempt),
        agent: Some(crate::AgentExecution {
            run_id: plan.consensus_run_id,
            native_conversation: plan.native_conversation,
            attempt: job.attempt,
            agent_id: ctx
                .agent_id
                .clone()
                .ok_or("agent execution has no model identity")?,
            display_name: plan.agent_display_name,
        }),
        // the tagged source (duckfs subtree or forge repo@commit) crosses to
        // the provisioner verbatim — the pool never interprets it.
        source: plan.source,
        ro_mounts: plan.skills, // C4 skill ro mounts (phase 5)
    };
    // (a)+(b) materialize OUTSIDE storage. A late blocking result releases the
    // provider slot, but keeps this attempt's resource admission until cleanup.
    let provisioner = Arc::clone(provisioner);
    let provision_spec = spec.clone();
    let mut provision = Box::pin(async move { provisioner.provision(&provision_spec).await });
    let provision_timeout = tokio::time::sleep(workspace_step_timeout());
    futures::pin_mut!(provision_timeout);
    let mut provision_cancelled = false;
    let provision_result = tokio::select! {
        result = &mut provision => Some(result),
        _ = &mut provision_timeout => None,
        _ = cancellation.cancelled() => {
            provision_cancelled = true;
            None
        }
    };
    let Some(ws) = provision_result else {
        // An actor/spawn_blocking request may still create a workspace after
        // the threshold. Await it fail-closed and reap that workspace exactly
        // once before this attempt releases aggregate resource admission.
        drop(permit.take());
        if let Ok(ws) = provision.await {
            ws.cleanup().await;
        }
        return Err(if provision_cancelled {
            "attempt cancelled during workspace provision".into()
        } else {
            format!(
                "workspace provision for {} timed out after {:?}",
                spec.run_id,
                workspace_step_timeout()
            )
        });
    };
    let ws: Arc<dyn crate::provision::ProvisionedWorkspace> = ws?.into();
    bind_workspace(ws.as_ref(), &mut ctx); // set workdir_override/env/context doc
    let native_artifacts_missing = spec
        .agent
        .as_ref()
        .is_some_and(|agent| agent.native_conversation.is_some())
        && ctx.native_conversation.is_none();
    if native_artifacts_missing {
        drop(permit.take());
        ws.cleanup().await;
        return Err("provisioner did not restore the requested native conversation".into());
    }
    if cancellation.is_cancelled() {
        drop(permit.take());
        ws.cleanup().await;
        return Err("attempt cancelled after workspace provision".into());
    }
    // run → commit → assemble, unwind-guarded: a panicking provider (or
    // receipt path) must not skip the cleanup below and leak the per-run
    // dir. the panic surfaces as this attempt's Err — the saga settles a
    // failed attempt instead of a silent task death.
    let mut cleanup_here = true;
    let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(async {
        match run_provider(provider, &input, &ctx, cancellation).await {
            Ok(output) => {
                let native_terminal = ctx.native_conversation.is_some() && output.text.is_empty();
                let invalid_terminal = match output.disposition {
                    provider_host::OutputDisposition::Answer => false,
                    provider_host::OutputDisposition::InputHandled => !native_terminal || output.usage.is_some(),
                    provider_host::OutputDisposition::Cancelled => !native_terminal,
                };
                if invalid_terminal {
                    return Err("provider returned an inconsistent native terminal result".into());
                }
                // The provider process/container has exited and been waited.
                // Commit and cleanup are storage work, not provider concurrency.
                drop(permit.take());
                if cancellation.is_cancelled() {
                    ws.cleanup().await;
                    cleanup_here = false;
                    return Err("attempt cancelled after provider completion".into());
                }
                // (d) capture output_ref. a commit-MECHANISM failure (conflict,
                // transport, rejection, a hung actor lane) must never
                // masquerade as a clean tree: the receipt records the error
                // and the status degrades, while the run's answer still
                // delivers (R4 — never lost to receipt plumbing). only
                // `CommitError::Nothing` is a true `no_changes`, and the
                // workspace impl already maps that to Ok.
                let audit_message = format!("agent run {}", spec.run_id);
                let proposal = crate::provision::commit_message_from_response_text(&output.text);
                let commit_ws = Arc::clone(&ws);
                let mut commit =
                    Box::pin(
                        async move { commit_ws.commit(&audit_message, proposal.as_deref()).await },
                    );
                let commit_timeout = tokio::time::sleep(workspace_step_timeout());
                futures::pin_mut!(commit_timeout);
                let mut commit_cancelled = false;
                let commit_result = tokio::select! {
                    result = &mut commit => Some(result),
                    _ = &mut commit_timeout => None,
                    _ = cancellation.cancelled() => {
                        commit_cancelled = true;
                        None
                    }
                };
                let (receipt, status) = match commit_result {
                    Some(Ok(receipt)) => (receipt, crate::provision::Status::Ok),
                    Some(Err(e)) => {
                        // This is what a `degraded: true` run looks like from
                        // the inside, and on raw stderr it reached no operator:
                        // the chain says degraded, the node said nothing.
                        tracing::warn!(
                            target: "ducktape::compute",
                            reason = "workspace_commit_failed",
                            run = %spec.run_id,
                            error = %e,
                            "the run's workspace did not commit; delivering degraded"
                        );
                        (
                            crate::provision::WorkspaceReceipt::commit_failed(&spec, e),
                            crate::provision::Status::Degraded,
                        )
                    }
                    None => {
                        let _ =
                            futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(commit))
                                .await;
                        ws.cleanup().await;
                        cleanup_here = false;
                        if commit_cancelled {
                            return Err("attempt cancelled during workspace commit".into());
                        }
                        let error =
                            format!("commit timed out after {:?}", workspace_step_timeout());
                        tracing::warn!(
                            target: "ducktape::compute",
                            reason = "workspace_commit_timeout",
                            run = %spec.run_id,
                            timeout_s = workspace_step_timeout().as_secs(),
                            "the run's workspace commit timed out; delivering degraded"
                        );
                        (
                            crate::provision::WorkspaceReceipt::commit_failed(&spec, error),
                            crate::provision::Status::Degraded,
                        )
                    }
                };
                // Echo the plan's requested sink so Runs can route delivery;
                // Runs remains the sole owner of strict response parsing.
                let bytes = match output.disposition {
                    provider_host::OutputDisposition::Answer => {
                        assemble_runner_result(&output.text, &receipt, sink, status)
                    }
                    provider_host::OutputDisposition::InputHandled => {
                        crate::provision::assemble_handled_input_result(&receipt, sink, status)
                    }
                    provider_host::OutputDisposition::Cancelled => {
                        crate::provision::assemble_cancelled_result(&receipt, sink, status)
                    }
                };
                Ok(attempt_output(output, bytes))
            }
            Err(e) => Err(e), // failed run: no commit, no output_ref
        }
    }))
    .await;
    // A normal provider settlement releases its slot before commit. A panic
    // still releases here, before storage cleanup begins.
    drop(permit.take());
    if cleanup_here {
        ws.cleanup().await; // (e) W5 always — even past a panicking provider
    }
    match outcome {
        Ok(result) => result,
        Err(panic) => Err(format!(
            "provider panicked mid-run: {}",
            panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic payload".into())
        )),
    }
}

/// A production provider must treat `ctx.cancellation` as a fail-closed
/// lifecycle contract: after the signal it terminates and waits its exact child
/// process tree/container, then resolves this future. Dispatch deliberately has
/// no grace-drop escape hatch; reservation release before that future settles
/// would allow replacement work to overlap a provider that may still be alive.
async fn run_provider(
    provider: &dyn provider_host::Provider,
    input: &str,
    ctx: &provider_host::RunContext,
    cancellation: &RunCancellation,
) -> Result<provider_host::ProviderOutput, String> {
    let run = provider.run_with_usage(input, ctx);
    futures::pin_mut!(run);
    tokio::select! {
        result = &mut run => result,
        _ = cancellation.cancelled() => run.as_mut().await,
    }
}

#[async_trait::async_trait(?Send)]
impl Worker for DispatchPool {
    async fn run(&self, event: &Event) -> Result<WorkOutcome, host::worker::Error> {
        if let Ok(control) = crate::module_contracts::decode_worker_control(&event.payload) {
            match control.command {
                crate::module_contracts::WorkerControlCommand::CancelAttempt {
                    saga_id,
                    attempt,
                    assignee,
                } => {
                    self.attempt_control().cancel(&saga_id, attempt, &assignee);
                    return Ok(WorkOutcome::Handled(None));
                }
            }
        }
        match gate(&self.providers, &self.node_key, &self.ledger, event) {
            Gated::NotMine => Ok(WorkOutcome::NotMine),
            Gated::Skip(reason) => {
                tracing::debug!(
                    target: "ducktape::compute",
                    reason,
                    "the pool claimed an effect and did not run it"
                );
                Ok(WorkOutcome::Handled(None))
            }
            Gated::Immediate(msg) => Ok(WorkOutcome::Handled(Some(msg))),
            Gated::Execute(mut job) => {
                let key: AttemptKey = (job.saga_id.clone(), job.attempt);
                // Insert before spawning so an assigned job that fits total
                // capacity is retained while it waits for current occupancy.
                let mut attempts = self.inflight.lock().expect("attempts lock");
                if attempts.contains_key(&key) {
                    // idempotent by design — but an entry that never leaves the
                    // map swallows every later offer of the same attempt in
                    // exactly this shape, so it is worth being able to see.
                    tracing::debug!(
                        target: "ducktape::compute",
                        saga = %job.saga_id,
                        attempt = job.attempt,
                        reason = "attempt_already_in_flight",
                        "the pool re-saw an attempt it is already running"
                    );
                    return Ok(WorkOutcome::Handled(None));
                }
                let cancellation = RunCancellation::new();
                // A job carrying its own Accept's pending claim converts it
                // directly — the demands are already reserved, so re-reserving
                // here would either double-count them or race the window
                // between the two. Only a job with no claim (FailFast, or a
                // directly pinned lease that was never announced) reserves now.
                let reservation = if let Some(claimed) = job.claimed.take() {
                    Some(claimed)
                } else if job.admission == AdmissionPolicy::FailFast {
                    let reservation_key = format!("{}:{}", job.saga_id, job.attempt);
                    let Some(reservation) = self.ledger.try_reserve(&reservation_key, &job.demands)
                    else {
                        return Ok(WorkOutcome::Handled(Some(oracle_result_with_usage(
                            &job.saga_id,
                            job.attempt,
                            Ok(RESOURCE_UNAVAILABLE_RESULT.to_vec()),
                            None,
                        ))));
                    };
                    Some(reservation)
                } else {
                    None
                };
                attempts.insert(
                    key.clone(),
                    RunningAttempt {
                        state: AttemptState::Running,
                        cancellation: cancellation.clone(),
                        reservation,
                    },
                );
                drop(attempts);
                self.spawn_exec(key, cancellation, job);
                // handled, result later: the follow-up op arrives through
                // the delivery lane, not this return value.
                Ok(WorkOutcome::Handled(None))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Condvar;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use crate::module_contracts::{
        SagaMsg, WorkerControl, WorkerRequest, encode_worker_control, encode_worker_request,
    };
    use crate::module_contracts::{WORK_SPEC_KIND, WorkSpec, decode_saga_msg, encode_work_spec};
    use crate::provision::{ProvisionedWorkspace, WorkspaceReceipt};
    use futures::StreamExt as _;

    fn spec_toml(tag: &str) -> provider_host::CapabilitySpec {
        provider_host::CapabilitySpec::parse(
            &format!(
                r#"
spec = 1
[capability]
tag = "{tag}"
[detect]
bin = "{tag}-cli"
[invoke]
args = []
prompt = "stdin"
[output]
format = "text"
"#
            ),
            "test",
        )
        .expect("mock spec parses")
    }

    /// a provider that records concurrency (current + peak), executions, and
    /// the last (input, ctx) it saw, then sleeps — the observable stand-in
    /// for a slow CLI.
    struct SlowProvider {
        tag: String,
        delay: Duration,
        executions: Arc<AtomicUsize>,
        current: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        last_run: Arc<Mutex<Option<(String, provider_host::RunContext)>>>,
        fail: bool,
        usage: Option<provider_host::TokenUsage>,
    }

    struct FixedProvider(&'static str);

    #[async_trait::async_trait]
    impl provider_host::Provider for FixedProvider {
        fn capability(&self) -> &str {
            "alpha"
        }

        async fn run(
            &self,
            _prompt: &str,
            _ctx: &provider_host::RunContext,
        ) -> Result<String, String> {
            Ok(self.0.to_owned())
        }
    }

    #[async_trait::async_trait]
    impl provider_host::Provider for SlowProvider {
        fn capability(&self) -> &str {
            &self.tag
        }
        async fn run(
            &self,
            prompt: &str,
            ctx: &provider_host::RunContext,
        ) -> Result<String, String> {
            self.executions.fetch_add(1, Ordering::SeqCst);
            *self.last_run.lock().unwrap() = Some((prompt.to_string(), ctx.clone()));
            let now = self.current.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            if let Some(cancellation) = &ctx.cancellation {
                tokio::select! {
                    _ = tokio::time::sleep(self.delay) => {}
                    _ = cancellation.cancelled() => {
                        // Model a provider that has observed cancellation but
                        // still needs a bounded TERM/wait teardown window.
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        self.current.fetch_sub(1, Ordering::SeqCst);
                        return Err("mock provider cancelled after teardown".into());
                    }
                }
            } else {
                tokio::time::sleep(self.delay).await;
            }
            self.current.fetch_sub(1, Ordering::SeqCst);
            if self.fail {
                Err(format!("provider exploded on: {prompt}"))
            } else {
                Ok(format!("answer to: {prompt}"))
            }
        }

        async fn run_with_usage(
            &self,
            prompt: &str,
            ctx: &provider_host::RunContext,
        ) -> Result<provider_host::ProviderOutput, String> {
            self.run(prompt, ctx)
                .await
                .map(|text| provider_host::ProviderOutput {
                    text,
                    usage: self.usage,
                    disposition: provider_host::OutputDisposition::Answer,
                })
        }
    }

    /// Deliberately ignores RunCancellation until the external release. The
    /// pool must fail closed and retain the attempt instead of dropping this
    /// provider future after an arbitrary grace period.
    struct FailClosedProvider {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        finished: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl provider_host::Provider for FailClosedProvider {
        fn capability(&self) -> &str {
            "alpha"
        }

        async fn run(
            &self,
            _prompt: &str,
            _ctx: &provider_host::RunContext,
        ) -> Result<String, String> {
            self.entered.notify_one();
            self.release.notified().await;
            self.finished.store(true, Ordering::SeqCst);
            Ok("late provider result".into())
        }
    }

    struct BlockingDropProvider {
        entered: Arc<tokio::sync::Notify>,
        cleanup_entered: Arc<AtomicBool>,
        cleanup_release: Arc<(Mutex<bool>, Condvar)>,
    }

    struct BlockingCleanup {
        entered: Arc<AtomicBool>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl Drop for BlockingCleanup {
        fn drop(&mut self) {
            self.entered.store(true, Ordering::SeqCst);
            let (lock, ready) = &*self.release;
            let mut released = lock.lock().expect("cleanup release lock");
            while !*released {
                released = ready.wait(released).expect("cleanup release wait");
            }
        }
    }

    #[async_trait::async_trait]
    impl provider_host::Provider for BlockingDropProvider {
        fn capability(&self) -> &str {
            "alpha"
        }

        async fn run(
            &self,
            _prompt: &str,
            _ctx: &provider_host::RunContext,
        ) -> Result<String, String> {
            let _cleanup = BlockingCleanup {
                entered: self.cleanup_entered.clone(),
                release: self.cleanup_release.clone(),
            };
            self.entered.notify_one();
            futures::future::pending().await
        }
    }

    struct Probes {
        executions: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        last_run: Arc<Mutex<Option<(String, provider_host::RunContext)>>>,
    }

    fn slow_providers(delay: Duration, fail: bool) -> (Arc<ProviderSet>, Probes) {
        let executions = Arc::new(AtomicUsize::new(0));
        let current = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let last_run = Arc::new(Mutex::new(None));
        let provider = SlowProvider {
            tag: "alpha".into(),
            delay,
            executions: executions.clone(),
            current,
            peak: peak.clone(),
            last_run: last_run.clone(),
            fail,
            usage: None,
        };
        let providers = Arc::new(ProviderSet::assemble(
            provider_host::SpecSet::from_specs(vec![spec_toml("alpha")]),
            vec![Box::new(provider)],
        ));
        (
            providers,
            Probes {
                executions,
                peak,
                last_run,
            },
        )
    }

    /// a pool wired to tokio::spawn with an unbounded result lane — the
    /// in-test twin of the hosts' wiring.
    fn pool_with(
        providers: Arc<ProviderSet>,
        limit: usize,
    ) -> (DispatchPool, futures::channel::mpsc::UnboundedReceiver<Msg>) {
        pool_with_capacity(providers, limit, Default::default())
    }

    fn pool_with_capacity(
        providers: Arc<ProviderSet>,
        limit: usize,
        capacity: BTreeMap<String, u64>,
    ) -> (DispatchPool, futures::channel::mpsc::UnboundedReceiver<Msg>) {
        let (tx, rx) = futures::channel::mpsc::unbounded::<Msg>();
        let spawn: SpawnFn = Arc::new(|_, fut| {
            tokio::spawn(fut);
        });
        let deliver: DeliverFn = Arc::new(move |msg| {
            let tx = tx.clone();
            Box::pin(async move {
                let _ = tx.unbounded_send(msg);
            })
        });
        let (provisioned, committed, cleaned) = flags();
        let provisioner: SharedProvisioner = Arc::new(MockProvisioner {
            provisioned,
            committed,
            cleaned,
            fail_commit: None,
        });
        (
            DispatchPool::with_limit(
                providers,
                b"me".to_vec(),
                spawn,
                deliver,
                limit,
                capacity,
                provisioner,
            ),
            rx,
        )
    }

    fn effect_with_payload(
        saga_id: &str,
        attempt: u32,
        assignee: Option<&[u8]>,
        payload: &[u8],
    ) -> Event {
        Event {
            source: "saga".into(),
            payload: encode_worker_request(&WorkerRequest {
                saga_id: saga_id.into(),
                attempt,
                spec: encode_work_spec(&WorkSpec {
                    kind: WORK_SPEC_KIND.into(),
                    dispatch_id: "d1".into(),
                    capability: "alpha".into(),
                    payload: payload.to_vec(),
                    demands: Default::default(),
                    admission: AdmissionPolicy::Queue,
                }),
                deadline: None,
                assignee: assignee.map(|a| a.to_vec()),
            }),
        }
    }

    fn effect_for(saga_id: &str, attempt: u32, assignee: Option<&[u8]>) -> Event {
        effect_with_payload(saga_id, attempt, assignee, &envelope_payload())
    }

    fn effect_with_demands(
        saga_id: &str,
        attempt: u32,
        assignee: Option<&[u8]>,
        demands: BTreeMap<String, u64>,
    ) -> Event {
        effect_with_admission(saga_id, attempt, assignee, demands, AdmissionPolicy::Queue)
    }

    fn effect_with_admission(
        saga_id: &str,
        attempt: u32,
        assignee: Option<&[u8]>,
        demands: BTreeMap<String, u64>,
        admission: AdmissionPolicy,
    ) -> Event {
        Event {
            source: "saga".into(),
            payload: encode_worker_request(&WorkerRequest {
                saga_id: saga_id.into(),
                attempt,
                spec: encode_work_spec(&WorkSpec {
                    kind: WORK_SPEC_KIND.into(),
                    dispatch_id: saga_id.into(),
                    capability: "alpha".into(),
                    payload: envelope_payload(),
                    demands,
                    admission,
                }),
                deadline: None,
                assignee: assignee.map(|a| a.to_vec()),
            }),
        }
    }

    fn cancel_effect(saga_id: &str, attempt: u32, assignee: &[u8]) -> Event {
        Event {
            source: "saga".into(),
            payload: encode_worker_control(&WorkerControl::cancel_attempt(
                saga_id.into(),
                attempt,
                assignee.to_vec(),
            )),
        }
    }

    fn response_text(bytes: Vec<u8>) -> String {
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["response_text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// a canned resolver: either a fixed `Resolved` or a fixed error string.
    /// Records whether it was consulted so a credential-less run can prove it was
    /// never asked to resolve.
    struct FixedResolver {
        outcome: Result<AirlockConfig, String>,
        seen: Arc<AtomicUsize>,
    }

    impl FixedResolver {
        fn ok(airlock: AirlockConfig) -> (Arc<Self>, Arc<AtomicUsize>) {
            let seen = Arc::new(AtomicUsize::new(0));
            (
                Arc::new(Self {
                    outcome: Ok(airlock),
                    seen: seen.clone(),
                }),
                seen,
            )
        }

        fn err(reason: &str) -> (Arc<Self>, Arc<AtomicUsize>) {
            let seen = Arc::new(AtomicUsize::new(0));
            (
                Arc::new(Self {
                    outcome: Err(reason.to_string()),
                    seen: seen.clone(),
                }),
                seen,
            )
        }
    }

    #[async_trait::async_trait]
    impl CredentialResolver for FixedResolver {
        async fn resolve(&self, _credential: &str, _saga_id: &str) -> Result<Resolved, String> {
            self.seen.fetch_add(1, Ordering::SeqCst);
            self.outcome.clone().map(|airlock| Resolved { airlock })
        }
    }

    /// a self-host airlock config to hand a resolver in tests — its internals are
    /// opaque here (private fields), so tests only assert that SOME config
    /// reached the run context.
    fn sample_airlock() -> AirlockConfig {
        AirlockConfig::self_host(
            &provider_host::ResolvedCredential {
                name: "jess-fable-1".into(),
                kind: provider_host::CredentialKind::Claude,
                authority: "airlock.owner.duck".into(),
                via: "http://127.0.0.1:0".into(),
                seal_pk: [7u8; 32],
            },
            provider_host::WorkRef::Direct,
        )
    }

    #[tokio::test]
    async fn a_credential_envelope_resolves_into_the_run_context() {
        let (providers, probes) = slow_providers(Duration::from_millis(5), false);
        let (resolver, seen) = FixedResolver::ok(sample_airlock());
        let (pool, mut rx) = pool_with(providers, 1);
        let pool = pool.with_credential_resolver(resolver);
        let payload =
            crate::envelope::compose_headless("sched\u{1f}d1", "hi", Some("jess-fable-1"))
                .into_bytes();
        pool.run(&effect_with_payload("s1", 0, Some(b"me"), &payload))
            .await
            .unwrap();
        let (_, _, outcome) = next_result(&mut rx).await;
        outcome.expect("the resolved run completes");
        assert_eq!(seen.load(Ordering::SeqCst), 1, "the resolver was consulted");
        let (_input, ctx) = probes.last_run.lock().unwrap().clone().unwrap();
        assert!(
            ctx.airlock.is_some(),
            "the resolved airlock reached the run"
        );
    }

    #[tokio::test]
    async fn a_resolver_refusal_fails_the_run_without_spawning() {
        let (providers, probes) = slow_providers(Duration::from_millis(5), false);
        let (resolver, _seen) = FixedResolver::err("credential_not_granted");
        let (pool, mut rx) = pool_with(providers, 1);
        let pool = pool.with_credential_resolver(resolver);
        let payload =
            crate::envelope::compose_headless("sched\u{1f}d2", "hi", Some("missing")).into_bytes();
        pool.run(&effect_with_payload("s2", 0, Some(b"me"), &payload))
            .await
            .unwrap();
        let (_, _, outcome) = next_result(&mut rx).await;
        assert!(
            outcome.unwrap_err().contains("credential_not_granted"),
            "the refusal is the attempt's result"
        );
        assert_eq!(
            probes.executions.load(Ordering::SeqCst),
            0,
            "a refused credential never launches a provider"
        );
    }

    #[tokio::test]
    async fn a_credential_with_no_resolver_wired_fails_the_run() {
        // a node that wired no resolver cannot honor a credential name; it must
        // fail loudly rather than run the guest on the host's own source.
        let (providers, probes) = slow_providers(Duration::from_millis(5), false);
        let (pool, mut rx) = pool_with(providers, 1);
        let payload =
            crate::envelope::compose_headless("sched\u{1f}d3", "hi", Some("jess")).into_bytes();
        pool.run(&effect_with_payload("s3", 0, Some(b"me"), &payload))
            .await
            .unwrap();
        let (_, _, outcome) = next_result(&mut rx).await;
        assert!(outcome.unwrap_err().contains("no credential resolver"));
        assert_eq!(probes.executions.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_credential_less_run_never_consults_the_resolver() {
        let (providers, probes) = slow_providers(Duration::from_millis(5), false);
        let (resolver, seen) = FixedResolver::ok(sample_airlock());
        let (pool, mut rx) = pool_with(providers, 1);
        let pool = pool.with_credential_resolver(resolver);
        // an ordinary (credential-less) envelope.
        pool.run(&effect_with_payload(
            "s4",
            0,
            Some(b"me"),
            &envelope_payload(),
        ))
        .await
        .unwrap();
        let (_, _, outcome) = next_result(&mut rx).await;
        outcome.expect("an ordinary run completes untouched");
        assert_eq!(
            seen.load(Ordering::SeqCst),
            0,
            "the resolver is not consulted"
        );
        let (_input, ctx) = probes.last_run.lock().unwrap().clone().unwrap();
        assert!(ctx.airlock.is_none(), "no credential ⇒ no airlock override");
    }

    #[test]
    fn run_key_for_dispatch_saga_uses_last_segment() {
        assert_eq!(run_key_for("dispatch\x1fruns\x1fd1"), "d1");
    }

    #[test]
    fn run_key_for_unnamespaced_saga_returns_whole_id() {
        assert_eq!(run_key_for("s1"), "s1");
    }

    async fn next_result(
        rx: &mut futures::channel::mpsc::UnboundedReceiver<Msg>,
    ) -> (String, u32, Result<Vec<u8>, String>) {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), rx.next())
                .await
                .expect("a result within budget")
                .expect("the lane stays open");
            assert_eq!(msg.target, "saga");
            match decode_saga_msg(&msg.payload).expect("a saga msg") {
                SagaMsg::OracleResult {
                    saga_id,
                    attempt,
                    outcome,
                    ..
                } => return (saga_id, attempt, outcome),
                SagaMsg::RenewLease { .. } => continue,
                other => panic!("expected an OracleResult, got {other:?}"),
            }
        }
    }

    async fn no_oracle_result(
        rx: &mut futures::channel::mpsc::UnboundedReceiver<Msg>,
        budget: Duration,
    ) -> bool {
        tokio::time::timeout(budget, async {
            loop {
                let Some(msg) = rx.next().await else {
                    return;
                };
                if matches!(
                    decode_saga_msg(&msg.payload),
                    Ok(SagaMsg::OracleResult { .. })
                ) {
                    panic!("a cancelled attempt delivered an OracleResult");
                }
            }
        })
        .await
        .is_err()
    }

    #[tokio::test]
    async fn fail_fast_occupied_settles_without_spawn_provider_or_provisioning() {
        let providers = Arc::new(ProviderSet::assemble(
            provider_host::SpecSet::from_specs(vec![spec_toml("alpha")]),
            Vec::new(),
        ));
        let spawn_count = Arc::new(AtomicUsize::new(0));
        let counted = spawn_count.clone();
        let spawn: SpawnFn = Arc::new(move |_, _| {
            counted.fetch_add(1, Ordering::SeqCst);
        });
        let deliver: DeliverFn = Arc::new(|_| Box::pin(async {}));
        let capacity = BTreeMap::from([("cores".to_string(), 1)]);
        let (provisioned, committed, cleaned) = flags();
        let provisioner: SharedProvisioner = Arc::new(MockProvisioner {
            provisioned: provisioned.clone(),
            committed,
            cleaned,
            fail_commit: None,
        });
        let pool = DispatchPool::with_limit(
            providers,
            b"me".to_vec(),
            spawn,
            deliver,
            1,
            capacity.clone(),
            provisioner,
        );
        let occupied = pool
            .ledger
            .try_reserve("occupied", &capacity)
            .expect("occupy the node");

        let event = effect_with_admission(
            "nested",
            3,
            Some(b"me"),
            capacity.clone(),
            AdmissionPolicy::FailFast,
        );
        let WorkOutcome::Handled(Some(msg)) = pool.run(&event).await.unwrap() else {
            panic!("occupied fail-fast attempt settles inline")
        };
        let SagaMsg::OracleResult {
            saga_id,
            attempt,
            outcome,
            usage,
        } = decode_saga_msg(&msg.payload).unwrap()
        else {
            panic!("expected an OracleResult")
        };
        assert_eq!(saga_id, "nested");
        assert_eq!(attempt, 3);
        assert_eq!(outcome, Ok(RESOURCE_UNAVAILABLE_RESULT.to_vec()));
        assert_eq!(usage, None);
        assert_eq!(spawn_count.load(Ordering::SeqCst), 0);
        assert!(!provisioned.load(Ordering::SeqCst));
        assert_eq!(pool.in_flight(), 0);
        drop(occupied);
        assert!(
            pool.ledger.fits(&capacity),
            "failed admission leaves no reservation"
        );
    }

    #[tokio::test]
    async fn successful_fail_fast_admission_keeps_reservation_until_settlement() {
        let (providers, probes) = slow_providers(Duration::from_millis(100), false);
        let capacity = BTreeMap::from([("cores".to_string(), 1)]);
        let (pool, mut rx) = pool_with_capacity(providers, 1, capacity.clone());
        let event = effect_with_admission(
            "nested",
            0,
            Some(b"me"),
            capacity.clone(),
            AdmissionPolicy::FailFast,
        );
        assert!(matches!(
            pool.run(&event).await.unwrap(),
            WorkOutcome::Handled(None)
        ));
        assert!(
            !pool.ledger.fits(&capacity),
            "successful admission reserves once"
        );
        let (saga_id, attempt, outcome) = next_result(&mut rx).await;
        assert_eq!((saga_id.as_str(), attempt), ("nested", 0));
        assert_eq!(
            response_text(outcome.unwrap()),
            "answer to: GENERIC\n\nCONTRACT\n\nCONVERSATION"
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while pool.in_flight() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(probes.executions.load(Ordering::SeqCst), 1);
        assert!(
            pool.ledger.fits(&capacity),
            "settlement releases reservation"
        );
    }

    #[tokio::test]
    async fn worker_control_cancels_only_the_matching_node_and_suppresses_late_result() {
        let (providers, probes) = slow_providers(Duration::from_secs(5), false);
        let (pool, mut rx) = pool_with(providers, 1);
        let work = effect_for("s1", 2, Some(b"me"));
        pool.run(&work).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while probes.executions.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        pool.run(&cancel_effect("s1", 2, b"other")).await.unwrap();
        assert_eq!(pool.in_flight(), 1, "foreign control is a claimed no-op");

        pool.run(&cancel_effect("s1", 2, b"me")).await.unwrap();
        // Redelivery while Cancelling is latched, never a second provider.
        pool.run(&work).await.unwrap();
        assert_eq!(probes.executions.load(Ordering::SeqCst), 1);
        tokio::time::timeout(Duration::from_secs(1), async {
            while pool.in_flight() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(no_oracle_result(&mut rx, Duration::from_millis(350)).await);
    }

    #[tokio::test]
    async fn cancellation_waits_for_provider_termination_before_releasing_admission() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let finished = Arc::new(AtomicBool::new(false));
        let providers = Arc::new(ProviderSet::assemble(
            provider_host::SpecSet::from_specs(vec![spec_toml("alpha")]),
            vec![Box::new(FailClosedProvider {
                entered: entered.clone(),
                release: release.clone(),
                finished: finished.clone(),
            })],
        ));
        let demands = BTreeMap::from([("cores".to_string(), 1)]);
        let (pool, mut rx) = pool_with_capacity(providers, 1, demands.clone());
        pool.run(&effect_with_demands("s1", 0, Some(b"me"), demands.clone()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("provider starts");

        pool.run(&cancel_effect("s1", 0, b"me")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(pool.in_flight(), 1);
        assert!(!pool.ledger.fits(&demands));
        assert!(!finished.load(Ordering::SeqCst));

        release.notify_one();
        tokio::time::timeout(Duration::from_secs(1), async {
            while pool.in_flight() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("provider settles before admission releases");
        assert!(finished.load(Ordering::SeqCst));
        assert!(pool.ledger.fits(&demands));
        assert!(no_oracle_result(&mut rx, Duration::from_millis(350)).await);
    }

    #[tokio::test]
    async fn cancellation_holds_resources_until_provider_teardown_and_workspace_cleanup() {
        let (providers, probes) = slow_providers(Duration::from_secs(5), false);
        let (provisioned, committed, cleaned) = flags();
        let provisioner: SharedProvisioner = Arc::new(MockProvisioner {
            provisioned,
            committed,
            cleaned: cleaned.clone(),
            fail_commit: None,
        });
        let capacity = BTreeMap::from([("cores".to_string(), 2)]);
        let (pool, _rx) = pool_with_capacity_and_provisioner(providers, 2, capacity, provisioner);
        let demands = BTreeMap::from([("cores".to_string(), 2)]);
        let parent = effect_with_demands("parent", 0, Some(b"me"), demands.clone());
        pool.run(&parent).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while probes.executions.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        pool.run(&cancel_effect("parent", 0, b"me")).await.unwrap();
        assert!(
            !pool.ledger.fits(&demands),
            "cancel retains the full-capacity reservation during teardown"
        );
        let child = effect_with_demands("child", 0, Some(b"me"), demands);
        pool.run(&child).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            probes.executions.load(Ordering::SeqCst),
            1,
            "the child stays in the resource queue during parent teardown"
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while probes.executions.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the child starts after parent teardown releases resources");
        assert!(
            cleaned.load(Ordering::SeqCst),
            "workspace cleanup happens before the child starts"
        );
        pool.run(&cancel_effect("child", 0, b"me")).await.unwrap();
    }

    #[tokio::test]
    async fn queued_attempt_cancellation_never_starts_the_provider() {
        let (providers, probes) = slow_providers(Duration::from_millis(300), false);
        let (pool, mut rx) = pool_with(providers, 1);
        pool.run(&effect_for("running", 0, Some(b"me")))
            .await
            .unwrap();
        pool.run(&effect_for("queued", 0, Some(b"me")))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while probes.executions.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        pool.run(&cancel_effect("queued", 0, b"me")).await.unwrap();

        let (saga_id, _, outcome) = next_result(&mut rx).await;
        assert_eq!(saga_id, "running");
        assert!(outcome.is_ok());
        tokio::time::timeout(Duration::from_secs(1), async {
            while pool.in_flight() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            probes.executions.load(Ordering::SeqCst),
            1,
            "the queued attempt never reaches the provider"
        );
    }

    #[tokio::test]
    async fn queued_attempts_do_not_allocate_teardown_owners() {
        let (providers, probes) = slow_providers(Duration::from_millis(300), false);
        let owners = Arc::new(AtomicUsize::new(0));
        let spawn: SpawnFn = Arc::new({
            let owners = owners.clone();
            move |kind, future| {
                if kind == SpawnKind::TeardownOwner {
                    owners.fetch_add(1, Ordering::SeqCst);
                }
                tokio::spawn(future);
            }
        });
        let deliver: DeliverFn = Arc::new(|_| Box::pin(async {}));
        let pool = DispatchPool::with_limit(
            providers,
            b"me".to_vec(),
            spawn,
            deliver,
            1,
            Default::default(),
            mock_provisioner(),
        );

        pool.run(&effect_for("running", 0, Some(b"me")))
            .await
            .unwrap();
        pool.run(&effect_for("queued", 0, Some(b"me")))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while probes.executions.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the admitted run starts");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(owners.load(Ordering::SeqCst), 1);

        pool.run(&cancel_effect("queued", 0, b"me")).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while pool.in_flight() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("running attempt finishes and queued cancellation prunes");
        assert_eq!(owners.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn hung_commits_cap_teardown_owners_after_one_overlap() {
        let (providers, probes) = slow_providers(Duration::from_millis(5), false);
        let owners = Arc::new(AtomicUsize::new(0));
        let spawn: SpawnFn = Arc::new({
            let owners = owners.clone();
            move |kind, future| {
                if kind == SpawnKind::TeardownOwner {
                    owners.fetch_add(1, Ordering::SeqCst);
                }
                tokio::spawn(future);
            }
        });
        let deliver: DeliverFn = Arc::new(|_| Box::pin(async {}));
        let cleaned = Arc::new(AtomicBool::new(false));
        let pool = DispatchPool::with_limit(
            providers,
            b"me".to_vec(),
            spawn,
            deliver,
            1,
            Default::default(),
            Arc::new(HungCommitProvisioner { cleaned }),
        );

        pool.run(&effect_for("hung-commit-1", 0, Some(b"me")))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while probes.executions.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the first provider starts");
        tokio::time::sleep(workspace_step_timeout() + Duration::from_millis(50)).await;

        pool.run(&effect_for("hung-commit-2", 0, Some(b"me")))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while probes.executions.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the second provider overlaps the first commit tail");
        tokio::time::sleep(workspace_step_timeout() + Duration::from_millis(50)).await;

        pool.run(&effect_for("hung-commit-3", 0, Some(b"me")))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(75)).await;
        assert_eq!(owners.load(Ordering::SeqCst), 2);
        assert_eq!(
            probes.executions.load(Ordering::SeqCst),
            2,
            "released provider permits must not bypass the bounded overlap"
        );
    }

    #[tokio::test]
    async fn blocked_deliveries_cap_teardown_owners_after_one_overlap() {
        let (providers, probes) = slow_providers(Duration::from_millis(1), false);
        let owners = Arc::new(AtomicUsize::new(0));
        let spawn: SpawnFn = Arc::new({
            let owners = owners.clone();
            move |kind, future| {
                if kind == SpawnKind::TeardownOwner {
                    owners.fetch_add(1, Ordering::SeqCst);
                }
                tokio::spawn(future);
            }
        });
        let delivery_started = Arc::new(tokio::sync::Notify::new());
        let deliver: DeliverFn = Arc::new({
            let delivery_started = delivery_started.clone();
            move |_| {
                let delivery_started = delivery_started.clone();
                Box::pin(async move {
                    delivery_started.notify_one();
                    std::future::pending::<()>().await;
                })
            }
        });
        let pool = DispatchPool::with_limit(
            providers,
            b"me".to_vec(),
            spawn,
            deliver,
            1,
            Default::default(),
            mock_provisioner(),
        );

        pool.run(&effect_for("blocked-delivery-1", 0, Some(b"me")))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), delivery_started.notified())
            .await
            .expect("the first owner reaches delivery");
        pool.run(&effect_for("blocked-delivery-2", 0, Some(b"me")))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), delivery_started.notified())
            .await
            .expect("the second owner overlaps the first blocked delivery");
        pool.run(&effect_for("blocked-delivery-3", 0, Some(b"me")))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(75)).await;

        assert_eq!(owners.load(Ordering::SeqCst), 2);
        assert_eq!(
            probes.executions.load(Ordering::SeqCst),
            2,
            "blocked deliveries must retain the bounded owner slots"
        );
    }

    #[tokio::test]
    async fn forced_owner_drop_keeps_the_runtime_responsive_and_admission_held() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let cleanup_entered = Arc::new(AtomicBool::new(false));
        let cleanup_release = Arc::new((Mutex::new(false), Condvar::new()));
        let providers = Arc::new(ProviderSet::assemble(
            provider_host::SpecSet::from_specs(vec![spec_toml("alpha")]),
            vec![Box::new(BlockingDropProvider {
                entered: entered.clone(),
                cleanup_entered: cleanup_entered.clone(),
                cleanup_release: cleanup_release.clone(),
            })],
        ));
        let (abort_tx, abort_rx) = futures::channel::oneshot::channel::<()>();
        let abort_rx = Arc::new(Mutex::new(Some(abort_rx)));
        let owner_thread = Arc::new(Mutex::new(None));
        let spawn: SpawnFn = Arc::new({
            let abort_rx = abort_rx.clone();
            let owner_thread = owner_thread.clone();
            move |kind, future| match kind {
                SpawnKind::Queued => {
                    tokio::spawn(future);
                }
                SpawnKind::TeardownOwner => {
                    let abort = abort_rx
                        .lock()
                        .expect("abort receiver lock")
                        .take()
                        .expect("one teardown owner");
                    let handle = std::thread::spawn(move || {
                        tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .expect("owner runtime")
                            .block_on(async move {
                                tokio::select! {
                                    _ = future => {}
                                    _ = abort => {}
                                }
                            });
                    });
                    *owner_thread.lock().expect("owner thread lock") = Some(handle);
                }
            }
        });
        let deliver: DeliverFn = Arc::new(|_| Box::pin(async {}));
        let demands = BTreeMap::from([("cores".to_string(), 1)]);
        let pool = DispatchPool::with_limit(
            providers,
            b"me".to_vec(),
            spawn,
            deliver,
            1,
            demands.clone(),
            mock_provisioner(),
        );

        pool.run(&effect_with_demands(
            "forced-drop",
            0,
            Some(b"me"),
            demands.clone(),
        ))
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("provider starts on its teardown owner");

        let watchdog_release = cleanup_release.clone();
        let watchdog = std::thread::spawn(move || {
            let (lock, ready) = &*watchdog_release;
            let released = lock.lock().expect("watchdog release lock");
            let (mut released, _) = ready
                .wait_timeout_while(released, Duration::from_secs(2), |done| !*done)
                .expect("watchdog wait");
            if !*released {
                *released = true;
                ready.notify_all();
            }
        });
        let responsive_since = std::time::Instant::now();
        abort_tx.send(()).expect("force owner future destruction");
        tokio::time::timeout(Duration::from_millis(500), async {
            while !cleanup_entered.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cleanup Drop runs off the shared runtime");
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(responsive_since.elapsed() < Duration::from_millis(500));
        assert_eq!(pool.in_flight(), 1, "the owner still owns the attempt");
        assert!(
            !pool.ledger.fits(&demands),
            "cleanup must finish before admission is released"
        );

        {
            let (lock, ready) = &*cleanup_release;
            *lock.lock().expect("cleanup release lock") = true;
            ready.notify_all();
        }
        tokio::task::spawn_blocking(move || {
            owner_thread
                .lock()
                .expect("owner thread lock")
                .take()
                .expect("owner thread exists")
                .join()
                .expect("owner thread joins");
            watchdog.join().expect("watchdog joins");
        })
        .await
        .expect("join helper");
        assert_eq!(pool.in_flight(), 0);
        assert!(pool.ledger.fits(&demands));
    }

    #[tokio::test]
    async fn discarding_a_spawn_future_prunes_its_attempt() {
        let (providers, probes) = slow_providers(Duration::ZERO, false);
        let deliver: DeliverFn = Arc::new(|_| Box::pin(async {}));
        let pool = DispatchPool::with_limit(
            providers,
            b"me".to_vec(),
            Arc::new(|_, future| drop(future)),
            deliver,
            1,
            Default::default(),
            mock_provisioner(),
        );

        pool.run(&effect_for("discarded", 0, Some(b"me")))
            .await
            .unwrap();
        assert_eq!(pool.in_flight(), 0);
        assert_eq!(probes.executions.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn completion_wins_once_before_a_late_cancel() {
        let (providers, _) = slow_providers(Duration::ZERO, false);
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let deliveries = Arc::new(AtomicUsize::new(0));
        let deliver: DeliverFn = Arc::new({
            let entered = entered.clone();
            let release = release.clone();
            let deliveries = deliveries.clone();
            move |msg| {
                let entered = entered.clone();
                let release = release.clone();
                let deliveries = deliveries.clone();
                Box::pin(async move {
                    if matches!(
                        decode_saga_msg(&msg.payload),
                        Ok(SagaMsg::OracleResult { .. })
                    ) {
                        deliveries.fetch_add(1, Ordering::SeqCst);
                        entered.notify_one();
                        release.notified().await;
                    }
                })
            }
        });
        let pool = DispatchPool::with_limit(
            providers,
            b"me".to_vec(),
            Arc::new(|_, future| {
                tokio::spawn(future);
            }),
            deliver,
            1,
            Default::default(),
            mock_provisioner(),
        );

        pool.run(&effect_for("completed", 0, Some(b"me")))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("completion reaches delivery");
        assert!(!pool.attempt_control().cancel("completed", 0, b"me"));
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(1), async {
            while pool.in_flight() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("delivery finishes");
        assert_eq!(deliveries.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_slow_run_renews_its_lease() {
        let (providers, _) = slow_providers(Duration::from_millis(100), false);
        let (pool, mut rx) = pool_with(providers, 1);
        pool.run(&effect_for("s1", 3, Some(b"me"))).await.unwrap();
        let msg = tokio::time::timeout(Duration::from_secs(1), rx.next())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            decode_saga_msg(&msg.payload).unwrap(),
            SagaMsg::RenewLease { saga_id, attempt: 3 } if saga_id == "s1"
        ));
    }

    #[tokio::test]
    async fn provider_usage_rides_the_oracle_result() {
        let executions = Arc::new(AtomicUsize::new(0));
        let provider = SlowProvider {
            tag: "alpha".into(),
            delay: Duration::ZERO,
            executions: executions.clone(),
            current: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
            last_run: Arc::new(Mutex::new(None)),
            fail: false,
            usage: Some(provider_host::TokenUsage {
                input_tokens: 100,
                cached_input_tokens: 60,
                cache_write_input_tokens: 0,
                output_tokens: 20,
                reasoning_output_tokens: 7,
            }),
        };
        let providers = Arc::new(ProviderSet::assemble(
            provider_host::SpecSet::from_specs(vec![spec_toml("alpha")]),
            vec![Box::new(provider)],
        ));
        let (pool, mut rx) = pool_with(providers, 1);
        pool.run(&effect_for("s1", 0, Some(b"me"))).await.unwrap();
        let msg = rx.next().await.unwrap();
        let SagaMsg::OracleResult { usage, .. } = decode_saga_msg(&msg.payload).unwrap() else {
            panic!("expected OracleResult");
        };
        assert_eq!(usage.unwrap().input_tokens, 100);
        assert_eq!(executions.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn two_slow_runs_execute_concurrently_and_never_block_the_offer() {
        let (providers, probes) = slow_providers(Duration::from_millis(300), false);
        let (pool, mut rx) = pool_with(providers, 4);

        let offered = std::time::Instant::now();
        for saga in ["s1", "s2"] {
            match pool.run(&effect_for(saga, 0, Some(b"me"))).await.unwrap() {
                WorkOutcome::Handled(None) => {}
                other => panic!("an executable lease spawns and claims, got {other:?}"),
            }
        }
        // the headline property: offering returned in offer time, not in
        // provider time.
        assert!(
            offered.elapsed() < Duration::from_millis(200),
            "the offer step must not await the provider (took {:?})",
            offered.elapsed()
        );

        let mut ids = vec![next_result(&mut rx).await.0, next_result(&mut rx).await.0];
        ids.sort();
        assert_eq!(ids, ["s1", "s2"], "both runs completed");
        assert_eq!(
            probes.peak.load(Ordering::SeqCst),
            2,
            "the two runs overlapped in real time"
        );
    }

    #[tokio::test]
    async fn the_cap_queues_over_limit_runs_without_dropping_them() {
        let (providers, probes) = slow_providers(Duration::from_millis(100), false);
        let (pool, mut rx) = pool_with(providers, 1);

        for saga in ["s1", "s2", "s3"] {
            pool.run(&effect_for(saga, 0, Some(b"me"))).await.unwrap();
        }
        let mut ids: Vec<String> = Vec::new();
        for _ in 0..3 {
            ids.push(next_result(&mut rx).await.0);
        }
        ids.sort();
        assert_eq!(ids, ["s1", "s2", "s3"], "queued runs still complete");
        assert_eq!(
            probes.peak.load(Ordering::SeqCst),
            1,
            "a cap of 1 serializes execution"
        );
        assert_eq!(probes.executions.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_redelivered_request_for_an_in_flight_attempt_is_skipped() {
        let (providers, probes) = slow_providers(Duration::from_millis(300), false);
        let (pool, mut rx) = pool_with(providers, 4);

        let eff = effect_for("s1", 0, Some(b"me"));
        pool.run(&eff).await.unwrap();
        // the redelivery, while the first child is still running: a claimed
        // skip, no second spawn.
        match pool.run(&eff).await.unwrap() {
            WorkOutcome::Handled(None) => {}
            other => panic!("an in-flight redelivery must be a skip, got {other:?}"),
        }
        assert_eq!(pool.in_flight(), 1, "one attempt in flight, not two");

        let (saga_id, attempt, outcome) = next_result(&mut rx).await;
        assert_eq!((saga_id.as_str(), attempt), ("s1", 0));
        assert_eq!(
            response_text(outcome.unwrap()),
            "answer to: GENERIC\n\nCONTRACT\n\nCONVERSATION"
        );
        assert_eq!(
            probes.executions.load(Ordering::SeqCst),
            1,
            "exactly one child for the attempt"
        );

        // ... and the key prunes on completion, so a LATER redelivery (e.g.
        // after a restart elsewhere re-leases) may execute again — the
        // stateless pre-pool semantics, with the saga deduping the result.
        while pool.in_flight() > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        pool.run(&eff).await.unwrap();
        let (saga_id, _, _) = next_result(&mut rx).await;
        assert_eq!(saga_id, "s1");
        assert_eq!(probes.executions.load(Ordering::SeqCst), 2);
    }

    /// a DIFFERENT attempt of the same saga is new work, never deduped —
    /// that is the retry lane.
    #[tokio::test]
    async fn a_new_attempt_of_the_same_saga_executes() {
        let (providers, probes) = slow_providers(Duration::from_millis(100), false);
        let (pool, mut rx) = pool_with(providers, 4);

        pool.run(&effect_for("s1", 0, Some(b"me"))).await.unwrap();
        pool.run(&effect_for("s1", 1, Some(b"me"))).await.unwrap();
        let mut attempts = vec![next_result(&mut rx).await.1, next_result(&mut rx).await.1];
        attempts.sort_unstable();
        assert_eq!(attempts, [0, 1]);
        assert_eq!(probes.executions.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn provider_failures_still_deliver_the_error_result() {
        let (providers, probes) = slow_providers(Duration::from_millis(20), true);
        let (pool, mut rx) = pool_with(providers, 4);

        pool.run(&effect_for("s1", 0, Some(b"me"))).await.unwrap();
        let (saga_id, attempt, outcome) = next_result(&mut rx).await;
        assert_eq!((saga_id.as_str(), attempt), ("s1", 0));
        let err = outcome.unwrap_err();
        assert!(err.contains("provider exploded"), "got: {err}");
        assert_eq!(probes.executions.load(Ordering::SeqCst), 1);
        // pruned: the failed attempt does not squat the in-flight set (the
        // saga's retry re-leases the SAME saga id with a new attempt, but a
        // same-key redelivery must also be runnable again).
        while pool.in_flight() > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// a v1 duckfs run envelope — the shape the composer emits for every run.
    fn envelope_payload() -> Vec<u8> {
        serde_json::json!({
            "ducktape_run": 1,
            "agent_id": "bot",
            "run_id": "chat\u{1f}general\u{1f}7\u{1f}bot",
            "agent_display_name": "BOT",
            "instructions": "GENERIC",
            "contract": "CONTRACT",
            "conversation": "CONVERSATION",
            "workspace": {
                "kind": "duckfs",
                "source_prefix": "/shared/agent-workspaces/bot",
                "source_snapshot": "aa".repeat(32)
            },
            "skills": [
                {"name":"persona","source_prefix":"/shared/skills/persona","always": true},
                {"name":"release","source_prefix":"/shared/skills/release","source_snapshot": "bb".repeat(32), "always": false}
            ],
            "result_contract": {"ducktape_runner_result": 1}
        })
        .to_string()
        .into_bytes()
    }

    #[tokio::test]
    async fn envelope_payloads_reach_the_provider_assembled_with_run_context() {
        let (providers, probes) = slow_providers(Duration::from_millis(5), false);
        let (pool, mut rx) = pool_with(providers, 4);

        let eff = effect_with_payload("s1", 0, Some(b"me"), &envelope_payload());
        pool.run(&eff).await.unwrap();
        let (_, _, outcome) = next_result(&mut rx).await;
        assert_eq!(
            response_text(outcome.unwrap()),
            "answer to: GENERIC\n\nCONTRACT\n\nCONVERSATION",
            "assembly order: prompt-or-instructions, contract, conversation"
        );
        let (input, ctx) = probes.last_run.lock().unwrap().clone().unwrap();
        assert!(
            !input.contains("ducktape_run"),
            "the provider never sees envelope JSON"
        );
        assert_eq!(ctx.agent_id.as_deref(), Some("bot"));
    }

    #[tokio::test]
    async fn provider_context_carries_dispatch_id_run_key() {
        let (providers, probes) = slow_providers(Duration::from_millis(5), false);
        let (pool, mut rx) = pool_with(providers, 4);

        let saga_id = "dispatch\x1fruns\x1fd1";
        let eff = effect_for(saga_id, 0, Some(b"me"));
        pool.run(&eff).await.unwrap();
        let (result_saga_id, _, outcome) = next_result(&mut rx).await;
        assert_eq!(result_saga_id, saga_id);
        assert_eq!(
            response_text(outcome.unwrap()),
            "answer to: GENERIC\n\nCONTRACT\n\nCONVERSATION"
        );
        let (_, ctx) = probes.last_run.lock().unwrap().clone().unwrap();
        assert_eq!(ctx.run_key.as_deref(), Some("d1"));
        let expected_node = provider_host::execution_node_id(b"me");
        assert_eq!(ctx.executing_node.as_deref(), Some(expected_node.as_str()));
        assert!(ctx.cancellation.is_some());
    }

    // ---- the portable provisioning bracket ----------------------------------

    /// the same v1 duckfs envelope, named for the provisioning-bracket tests.
    fn v1_envelope_payload() -> Vec<u8> {
        envelope_payload()
    }

    /// a forge-sourced v1 run envelope — the EXACT byte shapes task 1's
    /// composer emits (task-1 report §"Exact final serde shapes"): tagged
    /// forge workspace, `context`, requested-Pr sink WITHOUT title/body keys.
    fn forge_envelope_payload() -> Vec<u8> {
        serde_json::json!({
            "ducktape_run": 1,
            "agent_id": "bot",
            "run_id": "chat\u{1f}forge:app:7\u{1f}2\u{1f}bot",
            "agent_display_name": "BOT",
            "instructions": "GENERIC",
            "contract": "CONTRACT",
            "conversation": "CONVERSATION",
            "context": "Forge item context — you are working this item as a session.\nrepo: app\nitem: issue #7 (open)",
            "workspace": {
                "kind": "forge",
                "repo": "app",
                "item_title": "Fix the gate",
                "commit": "d0".repeat(20),
                "branch": "agent/item-7",
                "branch_born": false
            },
            "skills": [],
            "result_contract": {
                "ducktape_runner_result": 1,
                "sink": {"mode":"pr","repo":"app","source_branch":"agent/item-7","target_branch":"main"}
            }
        })
        .to_string()
        .into_bytes()
    }

    /// the winning-attempt bytes a portable run assembles: the mock stand-in
    /// for a real duckfs checkout. records that provision/commit/cleanup fired,
    /// binds a deterministic mount + env the provider observes, and (on commit)
    /// mints a fake output_ref — or fails the commit when `fail_commit` is set.
    struct MockProvisioner {
        provisioned: Arc<AtomicBool>,
        committed: Arc<AtomicBool>,
        cleaned: Arc<AtomicBool>,
        fail_commit: Option<String>,
    }

    #[async_trait::async_trait]
    impl crate::provision::WorkspaceProvisioner for MockProvisioner {
        async fn provision(
            &self,
            spec: &WorkspaceSpec,
        ) -> Result<Box<dyn ProvisionedWorkspace>, String> {
            self.provisioned.store(true, Ordering::SeqCst);
            let dir =
                std::env::temp_dir().join(format!("mock-ws-{}", spec.run_id.replace(':', "_")));
            let mut env = BTreeMap::new();
            env.insert("DUCKTAPE_RUN_WORKSPACE".into(), dir.display().to_string());
            // kind-agnostic: echo whatever source the spec carries, exactly
            // like a real receipt would (duckfs prefix/pin or forge:<repo>).
            let (src, snap) = spec.source.receipt_coords();
            Ok(Box::new(MockWs {
                dir,
                context_doc: Some("# persona\nYou are Bot.".into()),
                src,
                snap,
                env,
                committed: self.committed.clone(),
                cleaned: self.cleaned.clone(),
                fail_commit: self.fail_commit.clone(),
            }))
        }
    }

    struct MockWs {
        dir: PathBuf,
        /// the assembled soul the real provisioner hands back — the pool must
        /// carry it onto the RunContext, or the agent runs without its persona.
        context_doc: Option<String>,
        src: String,
        snap: Option<String>,
        env: BTreeMap<String, String>,
        committed: Arc<AtomicBool>,
        cleaned: Arc<AtomicBool>,
        /// `Some` makes commit() fail with this error — the commit-mechanism
        /// failure seam.
        fail_commit: Option<String>,
    }

    #[async_trait::async_trait]
    impl ProvisionedWorkspace for MockWs {
        fn workdir(&self) -> PathBuf {
            self.dir.clone()
        }
        fn env(&self) -> BTreeMap<String, String> {
            self.env.clone()
        }
        fn context_doc(&self) -> Option<String> {
            self.context_doc.clone()
        }
        async fn commit(
            &self,
            _audit_message: &str,
            _proposal: Option<&str>,
        ) -> Result<WorkspaceReceipt, String> {
            self.committed.store(true, Ordering::SeqCst);
            if let Some(err) = &self.fail_commit {
                return Err(err.clone());
            }
            Ok(WorkspaceReceipt {
                source_prefix: self.src.clone(),
                source_snapshot: self.snap.clone(),
                output_snapshot: Some("cc".repeat(32)),
                commit_height: Some(9),
                rebased: false,
                no_changes: false,
                commit_error: None,
                branch: None,
                output_commit: None,
            })
        }
        async fn cleanup(&self) {
            self.cleaned.store(true, Ordering::SeqCst);
        }
    }

    struct PhaseProvisioner {
        provision_entered: Option<Arc<tokio::sync::Notify>>,
        provision_release: Option<Arc<tokio::sync::Notify>>,
        commit_entered: Option<Arc<tokio::sync::Notify>>,
        commit_release: Option<Arc<tokio::sync::Notify>>,
        commits: Arc<AtomicUsize>,
        cleanups: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::provision::WorkspaceProvisioner for PhaseProvisioner {
        async fn provision(
            &self,
            spec: &WorkspaceSpec,
        ) -> Result<Box<dyn ProvisionedWorkspace>, String> {
            if let Some(entered) = &self.provision_entered {
                entered.notify_one();
            }
            if let Some(release) = &self.provision_release {
                release.notified().await;
            }
            let (source_prefix, source_snapshot) = spec.source.receipt_coords();
            Ok(Box::new(PhaseWs {
                dir: std::env::temp_dir()
                    .join(format!("phase-ws-{}", spec.run_id.replace(':', "_"))),
                source_prefix,
                source_snapshot,
                commit_entered: self.commit_entered.clone(),
                commit_release: self.commit_release.clone(),
                commits: self.commits.clone(),
                cleanups: self.cleanups.clone(),
            }))
        }
    }

    struct PhaseWs {
        dir: PathBuf,
        source_prefix: String,
        source_snapshot: Option<String>,
        commit_entered: Option<Arc<tokio::sync::Notify>>,
        commit_release: Option<Arc<tokio::sync::Notify>>,
        commits: Arc<AtomicUsize>,
        cleanups: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ProvisionedWorkspace for PhaseWs {
        fn workdir(&self) -> PathBuf {
            self.dir.clone()
        }

        fn env(&self) -> BTreeMap<String, String> {
            BTreeMap::new()
        }

        async fn commit(
            &self,
            _audit_message: &str,
            _proposal: Option<&str>,
        ) -> Result<WorkspaceReceipt, String> {
            if let Some(entered) = &self.commit_entered {
                entered.notify_one();
            }
            if let Some(release) = &self.commit_release {
                release.notified().await;
            }
            self.commits.fetch_add(1, Ordering::SeqCst);
            Ok(WorkspaceReceipt {
                source_prefix: self.source_prefix.clone(),
                source_snapshot: self.source_snapshot.clone(),
                output_snapshot: Some("dd".repeat(32)),
                commit_height: Some(10),
                rebased: false,
                no_changes: false,
                commit_error: None,
                branch: None,
                output_commit: None,
            })
        }

        async fn cleanup(&self) {
            self.cleanups.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn flags() -> (Arc<AtomicBool>, Arc<AtomicBool>, Arc<AtomicBool>) {
        (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        )
    }

    fn mock_provisioner() -> SharedProvisioner {
        let (provisioned, committed, cleaned) = flags();
        Arc::new(MockProvisioner {
            provisioned,
            committed,
            cleaned,
            fail_commit: None,
        })
    }

    fn pool_with_provisioner(
        providers: Arc<ProviderSet>,
        provisioner: SharedProvisioner,
    ) -> (DispatchPool, futures::channel::mpsc::UnboundedReceiver<Msg>) {
        pool_with_capacity_and_provisioner(providers, 4, Default::default(), provisioner)
    }

    fn pool_with_capacity_and_provisioner(
        providers: Arc<ProviderSet>,
        limit: usize,
        capacity: BTreeMap<String, u64>,
        provisioner: SharedProvisioner,
    ) -> (DispatchPool, futures::channel::mpsc::UnboundedReceiver<Msg>) {
        let (tx, rx) = futures::channel::mpsc::unbounded::<Msg>();
        let spawn: SpawnFn = Arc::new(|_, fut| {
            tokio::spawn(fut);
        });
        let deliver: DeliverFn = Arc::new(move |msg| {
            let tx = tx.clone();
            Box::pin(async move {
                let _ = tx.unbounded_send(msg);
            })
        });
        (
            DispatchPool::with_limit(
                providers,
                b"me".to_vec(),
                spawn,
                deliver,
                limit,
                capacity,
                provisioner,
            ),
            rx,
        )
    }

    #[tokio::test]
    async fn provider_phase_cancel_cleans_workspace_without_commit_or_result() {
        let (providers, probes) = slow_providers(Duration::from_secs(5), false);
        let (provisioned, committed, cleaned) = flags();
        let provisioner: SharedProvisioner = Arc::new(MockProvisioner {
            provisioned: provisioned.clone(),
            committed: committed.clone(),
            cleaned: cleaned.clone(),
            fail_commit: None,
        });
        let (pool, mut rx) = pool_with_provisioner(providers, provisioner);
        let work = effect_with_payload("s1", 0, Some(b"me"), &v1_envelope_payload());
        pool.run(&work).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !provisioned.load(Ordering::SeqCst)
                || probes.executions.load(Ordering::SeqCst) == 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        pool.run(&cancel_effect("s1", 0, b"me")).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !cleaned.load(Ordering::SeqCst) || pool.in_flight() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!committed.load(Ordering::SeqCst));
        assert!(no_oracle_result(&mut rx, Duration::from_millis(350)).await);
    }

    #[tokio::test]
    async fn provision_timeout_holds_admission_until_late_cleanup_finishes() {
        let (providers, probes) = slow_providers(Duration::ZERO, false);
        let provision_entered = Arc::new(tokio::sync::Notify::new());
        let provision_release = Arc::new(tokio::sync::Notify::new());
        let commits = Arc::new(AtomicUsize::new(0));
        let cleanups = Arc::new(AtomicUsize::new(0));
        let provisioner: SharedProvisioner = Arc::new(PhaseProvisioner {
            provision_entered: Some(provision_entered.clone()),
            provision_release: Some(provision_release.clone()),
            commit_entered: None,
            commit_release: None,
            commits: commits.clone(),
            cleanups: cleanups.clone(),
        });
        let demands = BTreeMap::from([("cores".to_string(), 1)]);
        let (pool, mut rx) =
            pool_with_capacity_and_provisioner(providers, 1, demands.clone(), provisioner);
        pool.run(&effect_with_demands("s1", 0, Some(b"me"), demands.clone()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), provision_entered.notified())
            .await
            .expect("provision starts");

        tokio::time::sleep(workspace_step_timeout() + Duration::from_millis(25)).await;
        assert_eq!(cleanups.load(Ordering::SeqCst), 0);
        assert_eq!(pool.in_flight(), 1, "late provision retains the attempt");
        assert!(
            !pool.ledger.fits(&demands),
            "late provision retains aggregate admission"
        );
        provision_release.notify_one();
        let (_, _, outcome) = next_result(&mut rx).await;
        assert!(outcome.unwrap_err().contains("workspace provision"));
        tokio::time::timeout(Duration::from_secs(1), async {
            while cleanups.load(Ordering::SeqCst) != 1 || pool.in_flight() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("late workspace is collected and reaped");
        assert!(pool.ledger.fits(&demands));
        assert_eq!(probes.executions.load(Ordering::SeqCst), 0);
        assert_eq!(commits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn provision_cancel_holds_admission_until_late_cleanup_finishes() {
        let (providers, probes) = slow_providers(Duration::ZERO, false);
        let provision_entered = Arc::new(tokio::sync::Notify::new());
        let provision_release = Arc::new(tokio::sync::Notify::new());
        let commits = Arc::new(AtomicUsize::new(0));
        let cleanups = Arc::new(AtomicUsize::new(0));
        let provisioner: SharedProvisioner = Arc::new(PhaseProvisioner {
            provision_entered: Some(provision_entered.clone()),
            provision_release: Some(provision_release.clone()),
            commit_entered: None,
            commit_release: None,
            commits: commits.clone(),
            cleanups: cleanups.clone(),
        });
        let demands = BTreeMap::from([("cores".to_string(), 1)]);
        let (pool, mut rx) =
            pool_with_capacity_and_provisioner(providers, 1, demands.clone(), provisioner);
        pool.run(&effect_with_demands("s1", 0, Some(b"me"), demands.clone()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), provision_entered.notified())
            .await
            .expect("provision starts");

        pool.run(&cancel_effect("s1", 0, b"me")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(pool.in_flight(), 1, "cancelled provision stays owned");
        assert!(
            !pool.ledger.fits(&demands),
            "cancelled provision retains aggregate admission"
        );
        assert_eq!(cleanups.load(Ordering::SeqCst), 0);
        provision_release.notify_one();
        tokio::time::timeout(Duration::from_secs(1), async {
            while cleanups.load(Ordering::SeqCst) != 1 || pool.in_flight() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("late workspace is cleaned exactly once");
        assert!(pool.ledger.fits(&demands));
        assert_eq!(probes.executions.load(Ordering::SeqCst), 0);
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert!(no_oracle_result(&mut rx, Duration::from_millis(350)).await);
    }

    #[tokio::test]
    async fn commit_timeout_holds_attempt_until_late_cleanup_finishes() {
        let (providers, _) = slow_providers(Duration::ZERO, false);
        let commit_entered = Arc::new(tokio::sync::Notify::new());
        let commit_release = Arc::new(tokio::sync::Notify::new());
        let commits = Arc::new(AtomicUsize::new(0));
        let cleanups = Arc::new(AtomicUsize::new(0));
        let provisioner: SharedProvisioner = Arc::new(PhaseProvisioner {
            provision_entered: None,
            provision_release: None,
            commit_entered: Some(commit_entered.clone()),
            commit_release: Some(commit_release.clone()),
            commits: commits.clone(),
            cleanups: cleanups.clone(),
        });
        let demands = BTreeMap::from([("cores".to_string(), 1)]);
        let (pool, mut rx) =
            pool_with_capacity_and_provisioner(providers, 1, demands.clone(), provisioner);
        pool.run(&effect_with_demands("s1", 0, Some(b"me"), demands.clone()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), commit_entered.notified())
            .await
            .expect("commit starts");

        tokio::time::sleep(workspace_step_timeout() + Duration::from_millis(25)).await;
        assert_eq!(pool.in_flight(), 1, "late commit retains the attempt");
        assert!(
            !pool.ledger.fits(&demands),
            "late commit retains aggregate admission"
        );
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert_eq!(cleanups.load(Ordering::SeqCst), 0);

        commit_release.notify_one();
        let (_, _, outcome) = next_result(&mut rx).await;
        let bytes = outcome.expect("provider answer survives a commit timeout");
        let result: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(result["status"], "degraded");
        tokio::time::timeout(Duration::from_secs(1), async {
            while commits.load(Ordering::SeqCst) != 1
                || cleanups.load(Ordering::SeqCst) != 1
                || pool.in_flight() != 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("late commit is collected then cleaned exactly once");
        assert!(pool.ledger.fits(&demands));
    }

    #[tokio::test]
    async fn commits_do_not_hold_provider_permits_and_cancel_cleans_late_work_once() {
        let (providers, probes) = slow_providers(Duration::ZERO, false);
        let commit_entered = Arc::new(tokio::sync::Notify::new());
        let commit_release = Arc::new(tokio::sync::Notify::new());
        let commits = Arc::new(AtomicUsize::new(0));
        let cleanups = Arc::new(AtomicUsize::new(0));
        let provisioner: SharedProvisioner = Arc::new(PhaseProvisioner {
            provision_entered: None,
            provision_release: None,
            commit_entered: Some(commit_entered.clone()),
            commit_release: Some(commit_release.clone()),
            commits: commits.clone(),
            cleanups: cleanups.clone(),
        });
        let (pool, mut rx) =
            pool_with_capacity_and_provisioner(providers, 1, Default::default(), provisioner);
        pool.run(&effect_with_payload(
            "s1",
            0,
            Some(b"me"),
            &v1_envelope_payload(),
        ))
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(1), commit_entered.notified())
            .await
            .expect("commit starts");

        pool.run(&effect_with_payload(
            "s2",
            0,
            Some(b"me"),
            &v1_envelope_payload(),
        ))
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_millis(100), async {
            while probes.executions.load(Ordering::SeqCst) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("second provider starts while first commit is pending");
        pool.run(&cancel_effect("s1", 0, b"me")).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), commit_entered.notified())
            .await
            .expect("second commit starts");

        pool.run(&cancel_effect("s2", 0, b"me")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            pool.in_flight(),
            2,
            "cancelled commits remain owned until storage cleanup"
        );
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert_eq!(cleanups.load(Ordering::SeqCst), 0);
        commit_release.notify_waiters();
        tokio::time::timeout(Duration::from_secs(1), async {
            while cleanups.load(Ordering::SeqCst) != 2 || pool.in_flight() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("commit and cleanup complete after cancellation");
        assert_eq!(commits.load(Ordering::SeqCst), 2);
        assert!(no_oracle_result(&mut rx, Duration::from_millis(350)).await);
    }

    #[tokio::test]
    async fn native_turn_refuses_a_provisioner_that_omits_native_artifacts() {
        let (providers, probes) = slow_providers(Duration::ZERO, false);
        let (provisioned, committed, cleaned) = flags();
        let provisioner: SharedProvisioner = Arc::new(MockProvisioner {
            provisioned: provisioned.clone(),
            committed: committed.clone(),
            cleaned: cleaned.clone(),
            fail_commit: None,
        });
        let (pool, mut rx) = pool_with_provisioner(providers, provisioner);
        let mut envelope: serde_json::Value =
            serde_json::from_slice(&v1_envelope_payload()).unwrap();
        envelope["native_conversation"] = serde_json::json!({
            "conversation_id":"resident-1", "turn_id":"1", "revision":0,
            "history_prefix":"/shared/conversations/resident-1", "history_snapshot":null,
            "session_path":"session.jsonl", "packages":[], "events":[],
        });
        let effect = effect_with_payload(
            "native-1", 0, Some(b"me"), &serde_json::to_vec(&envelope).unwrap(),
        );
        pool.run(&effect).await.unwrap();
        let (_, _, outcome) = next_result(&mut rx).await;
        assert!(outcome.unwrap_err().contains("did not restore"));
        assert!(provisioned.load(Ordering::SeqCst));
        assert!(cleaned.load(Ordering::SeqCst));
        assert!(!committed.load(Ordering::SeqCst));
        assert!(probes.last_run.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn a_v1_run_with_a_provisioner_wired_provisions_binds_commits_and_wraps_the_result() {
        let (providers, probes) = slow_providers(Duration::from_millis(5), false);
        let (provisioned, committed, cleaned) = flags();
        let provisioner: SharedProvisioner = Arc::new(MockProvisioner {
            provisioned: provisioned.clone(),
            committed: committed.clone(),
            cleaned: cleaned.clone(),
            fail_commit: None,
        });
        let (pool, mut rx) = pool_with_provisioner(providers, provisioner);

        let eff = effect_with_payload("s1", 0, Some(b"me"), &v1_envelope_payload());
        pool.run(&eff).await.unwrap();
        let (saga_id, attempt, outcome) = next_result(&mut rx).await;
        assert_eq!((saga_id.as_str(), attempt), ("s1", 0));

        // the delivered bytes are a host-assembled RunnerResult, NOT raw text.
        let bytes = outcome.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["ducktape_runner_result"], 1);
        assert_eq!(
            v["response_text"],
            "answer to: GENERIC\n\nCONTRACT\n\nCONVERSATION"
        );
        assert_eq!(v["workspace_receipt"]["output_snapshot"], "cc".repeat(32));
        assert_eq!(v["workspace_receipt"]["commit_height"], 9);
        assert_eq!(
            v["workspace_receipt"]["source_prefix"],
            "/shared/agent-workspaces/bot"
        );
        assert_eq!(v["workspace_receipt"]["no_changes"], false);
        assert!(
            !v.as_object().unwrap().contains_key("sink"),
            "a chain-sink run keeps the sink key skip-serialized"
        );

        // the full lifecycle fired, and the provider ran INSIDE the mount.
        assert!(provisioned.load(Ordering::SeqCst), "provision was called");
        assert!(committed.load(Ordering::SeqCst), "commit ran on success");
        assert!(cleaned.load(Ordering::SeqCst), "cleanup always runs (W5)");
        let (_, ctx) = probes.last_run.lock().unwrap().clone().unwrap();
        let expected = std::env::temp_dir().join("mock-ws-s1_0");
        assert_eq!(
            ctx.workdir_override.as_deref(),
            Some(expected.as_path()),
            "the provider observed the bound mount as its cwd"
        );
        assert_eq!(
            ctx.env.get("DUCKTAPE_RUN_WORKSPACE").map(String::as_str),
            Some(expected.display().to_string().as_str()),
            "the run-scoped workspace env was applied"
        );
        // the SOUL crosses provisioner → RunContext. it is assembled from the
        // MATERIALIZED skill mounts (only the provisioner can read them), so
        // this hop is the only way the persona ever reaches the model —
        // the provider then picks the door (the CLI's auto-load file, or a
        // prepend to the stdin prompt).
        assert_eq!(
            ctx.context_doc.as_deref(),
            Some("# persona\nYou are Bot."),
            "the provisioned context doc must ride into the run"
        );
    }

    /// a probe provisioner that captures the [`WorkspaceSpec::ro_mounts`] it is
    /// handed — proving the skills seam composer → WireSkill → PortablePlan.skills
    /// → WorkspaceSpec.ro_mounts (critic #5).
    struct RoMountProbe {
        captured: Arc<Mutex<Vec<crate::provision::RoMount>>>,
    }

    #[async_trait::async_trait]
    impl crate::provision::WorkspaceProvisioner for RoMountProbe {
        async fn provision(
            &self,
            spec: &WorkspaceSpec,
        ) -> Result<Box<dyn ProvisionedWorkspace>, String> {
            *self.captured.lock().unwrap() = spec.ro_mounts.clone();
            Ok(Box::new(ProbeWs))
        }
    }

    struct ProbeWs;

    #[async_trait::async_trait]
    impl ProvisionedWorkspace for ProbeWs {
        fn workdir(&self) -> PathBuf {
            std::env::temp_dir().join("probe-ws")
        }
        fn env(&self) -> BTreeMap<String, String> {
            BTreeMap::new()
        }
        async fn commit(
            &self,
            _audit_message: &str,
            _proposal: Option<&str>,
        ) -> Result<WorkspaceReceipt, String> {
            Ok(WorkspaceReceipt {
                source_prefix: String::new(),
                source_snapshot: None,
                output_snapshot: None,
                commit_height: None,
                rebased: false,
                no_changes: true,
                commit_error: None,
                branch: None,
                output_commit: None,
            })
        }
        async fn cleanup(&self) {}
    }

    #[tokio::test]
    async fn a_v1_runs_skills_reach_the_spec_as_ro_mounts() {
        let (providers, _probes) = slow_providers(Duration::from_millis(5), false);
        let captured: Arc<Mutex<Vec<crate::provision::RoMount>>> = Arc::new(Mutex::new(Vec::new()));
        let provisioner: SharedProvisioner = Arc::new(RoMountProbe {
            captured: captured.clone(),
        });
        let (pool, mut rx) = pool_with_provisioner(providers, provisioner);

        let eff = effect_with_payload("s1", 0, Some(b"me"), &v1_envelope_payload());
        pool.run(&eff).await.unwrap();
        let _ = next_result(&mut rx).await;

        // both composed skills become ro mounts, IN CURATION ORDER, each
        // carrying its load mode: the provisioner assembles the run's soul from
        // exactly this — inline the `always` bodies, index the rest — so an
        // order or a mode dropped here is a different agent.
        let mounts = captured.lock().unwrap().clone();
        assert_eq!(mounts.len(), 2, "both composed skills became ro mounts");
        assert_eq!(mounts[0].mount_subpath, "persona");
        assert!(mounts[0].always, "the persona skill inlines");
        assert_eq!(mounts[1].mount_subpath, "release");
        assert_eq!(mounts[1].source_prefix, "/shared/skills/release");
        assert!(!mounts[1].always, "an unmarked skill is on-demand");
        assert_eq!(
            mounts[1].source_snapshot.as_deref(),
            Some("bb".repeat(32).as_str())
        );
    }

    /// a probe provisioner that captures the WHOLE [`WorkspaceSpec`] — how the
    /// forge tests observe that the tagged source crossed envelope →
    /// PortablePlan → WorkspaceSpec intact.
    struct SpecProbe {
        captured: Arc<Mutex<Option<WorkspaceSpec>>>,
    }

    struct CommitMessageProbe {
        captured: Arc<Mutex<Option<String>>>,
    }

    #[async_trait::async_trait]
    impl crate::provision::WorkspaceProvisioner for CommitMessageProbe {
        async fn provision(
            &self,
            _spec: &WorkspaceSpec,
        ) -> Result<Box<dyn ProvisionedWorkspace>, String> {
            Ok(Box::new(CommitMessageWs {
                captured: self.captured.clone(),
            }))
        }
    }

    struct CommitMessageWs {
        captured: Arc<Mutex<Option<String>>>,
    }

    #[async_trait::async_trait]
    impl ProvisionedWorkspace for CommitMessageWs {
        fn workdir(&self) -> PathBuf {
            std::env::temp_dir().join("commit-message-probe")
        }
        fn env(&self) -> BTreeMap<String, String> {
            BTreeMap::new()
        }
        async fn commit(
            &self,
            audit_message: &str,
            proposal: Option<&str>,
        ) -> Result<WorkspaceReceipt, String> {
            *self.captured.lock().unwrap() = Some(
                proposal
                    .map(str::to_owned)
                    .unwrap_or_else(|| audit_message.to_owned()),
            );
            Ok(WorkspaceReceipt {
                source_prefix: "forge:app".into(),
                source_snapshot: Some("d0".repeat(20)),
                output_snapshot: None,
                commit_height: None,
                rebased: false,
                no_changes: true,
                commit_error: None,
                branch: None,
                output_commit: None,
            })
        }
        async fn cleanup(&self) {}
    }

    #[async_trait::async_trait]
    impl crate::provision::WorkspaceProvisioner for SpecProbe {
        async fn provision(
            &self,
            spec: &WorkspaceSpec,
        ) -> Result<Box<dyn ProvisionedWorkspace>, String> {
            *self.captured.lock().unwrap() = Some(spec.clone());
            Ok(Box::new(ProbeWs))
        }
    }

    #[tokio::test]
    async fn a_forge_runs_pinned_source_reaches_the_provisioners_spec() {
        // the forge half of the skills-reach-the-spec pattern: the tagged
        // source survives envelope → PortablePlan → WorkspaceSpec verbatim,
        // so task 3's provisioner sees exactly the committed pin.
        let (providers, _probes) = slow_providers(Duration::from_millis(5), false);
        let captured: Arc<Mutex<Option<WorkspaceSpec>>> = Arc::new(Mutex::new(None));
        let provisioner: SharedProvisioner = Arc::new(SpecProbe {
            captured: captured.clone(),
        });
        let (pool, mut rx) = pool_with_provisioner(providers, provisioner);

        let eff = effect_with_payload("s1", 7, Some(b"me"), &forge_envelope_payload());
        pool.run(&eff).await.unwrap();
        let _ = next_result(&mut rx).await;

        let spec = captured
            .lock()
            .unwrap()
            .clone()
            .expect("the spec was captured");
        assert_eq!(spec.run_id, "s1:7");
        assert_eq!(spec.agent.as_ref().unwrap().attempt, 7);
        assert_eq!(
            spec.agent.as_ref().map(|agent| agent.agent_id.as_str()),
            Some("bot")
        );
        assert_eq!(
            spec.agent.as_ref().map(|agent| agent.display_name.as_str()),
            Some("BOT")
        );
        assert_eq!(
            spec.source,
            crate::workspace_source::WorkspaceSource::Forge {
                repo: "app".into(),
                item_title: "Fix the gate".into(),
                commit: "d0".repeat(20),
                branch: "agent/item-7".into(),
                branch_born: false,
            }
        );
    }

    #[tokio::test]
    async fn portable_workspace_commit_preserves_the_run_audit_message() {
        let (providers, _probes) = slow_providers(Duration::from_millis(5), false);
        let captured = Arc::new(Mutex::new(None));
        let provisioner: SharedProvisioner = Arc::new(CommitMessageProbe {
            captured: captured.clone(),
        });
        let (pool, mut rx) = pool_with_provisioner(providers, provisioner);
        let saga_id = "dispatch\u{1f}runs\u{1f}private-hash";

        let eff = effect_with_payload(saga_id, 7, Some(b"me"), &v1_envelope_payload());
        pool.run(&eff).await.unwrap();
        let _ = next_result(&mut rx).await;

        let message = captured.lock().unwrap().clone().expect("commit called");
        assert_eq!(message, format!("agent run {saga_id}:7"));
    }

    #[tokio::test]
    async fn response_commit_message_reaches_the_workspace_commit_boundary_exactly() {
        let response = r#"{"reply_blocks":[],"actions":[],"commit_message":"fix: exact subject\n\nExact body."}"#;
        let providers = Arc::new(ProviderSet::assemble(
            provider_host::SpecSet::from_specs(vec![spec_toml("alpha")]),
            vec![Box::new(FixedProvider(response))],
        ));
        let captured = Arc::new(Mutex::new(None));
        let provisioner: SharedProvisioner = Arc::new(CommitMessageProbe {
            captured: captured.clone(),
        });
        let (pool, mut rx) = pool_with_provisioner(providers, provisioner);

        pool.run(&effect_for("s1", 0, Some(b"me"))).await.unwrap();
        let _ = next_result(&mut rx).await;

        assert_eq!(
            captured.lock().unwrap().as_deref(),
            Some("fix: exact subject\n\nExact body.")
        );
    }

    #[tokio::test]
    async fn the_requested_sink_is_echoed_on_the_runner_result() {
        // O1/O2 threading: the plan's requested Pr sink rides the assembled
        // RunnerResult — with title/body as PRESENT empty keys (runs' decode
        // keeps title required; delivery derives the real text, contract §3).
        let (providers, _probes) = slow_providers(Duration::from_millis(5), false);
        let (provisioned, committed, cleaned) = flags();
        let provisioner: SharedProvisioner = Arc::new(MockProvisioner {
            provisioned: provisioned.clone(),
            committed: committed.clone(),
            cleaned: cleaned.clone(),
            fail_commit: None,
        });
        let (pool, mut rx) = pool_with_provisioner(providers, provisioner);

        let eff = effect_with_payload("s1", 0, Some(b"me"), &forge_envelope_payload());
        pool.run(&eff).await.unwrap();
        let (_, _, outcome) = next_result(&mut rx).await;

        let bytes = outcome.unwrap();
        let raw = String::from_utf8(bytes.clone()).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["sink"]["mode"], "pr", "the requested sink is echoed: {v}");
        assert_eq!(v["sink"]["repo"], "app");
        assert_eq!(v["sink"]["source_branch"], "agent/item-7");
        assert_eq!(v["sink"]["target_branch"], "main");
        // the interop obligation: empty title/body are PRESENT keys, never
        // skipped — runs' WireSink keeps title required on decode.
        assert!(
            raw.contains(r#""title":"""#) && raw.contains(r#""body":"""#),
            "empty title/body must be present keys: {raw}"
        );
        // and the forge receipt coords rode along (§5).
        assert_eq!(v["workspace_receipt"]["source_prefix"], "forge:app");
        assert_eq!(v["workspace_receipt"]["source_snapshot"], "d0".repeat(20));
    }

    #[tokio::test]
    async fn a_hung_commit_on_the_forge_path_keeps_the_attempt_fail_closed() {
        // Forge follows the same aggregate-resource boundary as duckfs: a
        // commit that never settles cannot release or deliver the attempt.
        let (providers, _probes) = slow_providers(Duration::from_millis(5), false);
        let cleaned = Arc::new(AtomicBool::new(false));
        let provisioner: SharedProvisioner = Arc::new(HungCommitProvisioner {
            cleaned: cleaned.clone(),
        });
        let (pool, mut rx) = pool_with_provisioner(providers, provisioner);

        let eff = effect_with_payload("s1", 0, Some(b"me"), &forge_envelope_payload());
        pool.run(&eff).await.unwrap();
        assert!(no_oracle_result(&mut rx, Duration::from_millis(350)).await);
        assert_eq!(pool.in_flight(), 1, "the hung commit remains owned");
        assert!(
            !cleaned.load(Ordering::SeqCst),
            "cleanup must not race a commit that has never settled"
        );
    }

    #[tokio::test]
    async fn a_panicking_provider_on_the_forge_path_still_cleans_up() {
        // the unwind guard covers the forge path exactly as duckfs: no commit,
        // no leaked per-run dir, the attempt settles as a failure.
        let providers = Arc::new(ProviderSet::assemble(
            provider_host::SpecSet::from_specs(vec![spec_toml("alpha")]),
            vec![Box::new(PanicProvider {
                tag: "alpha".into(),
            })],
        ));
        let (provisioned, committed, cleaned) = flags();
        let provisioner: SharedProvisioner = Arc::new(MockProvisioner {
            provisioned: provisioned.clone(),
            committed: committed.clone(),
            cleaned: cleaned.clone(),
            fail_commit: None,
        });
        let (pool, mut rx) = pool_with_provisioner(providers, provisioner);

        let eff = effect_with_payload("s1", 0, Some(b"me"), &forge_envelope_payload());
        pool.run(&eff).await.unwrap();
        let (_, _, outcome) = next_result(&mut rx).await;

        let err = outcome.expect_err("a panic settles the attempt as a failure");
        assert!(err.contains("panicked"), "got {err}");
        assert!(provisioned.load(Ordering::SeqCst));
        assert!(
            !committed.load(Ordering::SeqCst),
            "a panicked run commits NOTHING"
        );
        assert!(
            cleaned.load(Ordering::SeqCst),
            "cleanup runs past the panic (W5)"
        );
    }

    #[tokio::test]
    async fn a_failed_v1_run_cleans_up_without_committing_and_delivers_the_error() {
        let (providers, _probes) = slow_providers(Duration::from_millis(5), true);
        let (provisioned, committed, cleaned) = flags();
        let provisioner: SharedProvisioner = Arc::new(MockProvisioner {
            provisioned: provisioned.clone(),
            committed: committed.clone(),
            cleaned: cleaned.clone(),
            fail_commit: None,
        });
        let (pool, mut rx) = pool_with_provisioner(providers, provisioner);

        let eff = effect_with_payload("s1", 0, Some(b"me"), &v1_envelope_payload());
        pool.run(&eff).await.unwrap();
        let (_, _, outcome) = next_result(&mut rx).await;

        let err = outcome.unwrap_err();
        assert!(
            err.contains("provider exploded"),
            "the run's error surfaces: {err}"
        );
        assert!(
            provisioned.load(Ordering::SeqCst),
            "the mount was materialized"
        );
        assert!(
            !committed.load(Ordering::SeqCst),
            "a failed run commits NOTHING — no output_ref for a discarded attempt"
        );
        assert!(
            cleaned.load(Ordering::SeqCst),
            "cleanup still runs on failure (W5)"
        );
    }

    #[tokio::test]
    async fn a_commit_mechanism_failure_degrades_the_receipt_never_fakes_a_clean_tree() {
        // THE silent-data-loss guard: a conflict/transport/rejection during the
        // workspace commit must surface as `commit_error` + a degraded status —
        // never as `no_changes: true` with an Ok status, which would report the
        // agent's lost writes as a clean working copy.
        let (providers, _probes) = slow_providers(Duration::from_millis(5), false);
        let (provisioned, committed, cleaned) = flags();
        let provisioner: SharedProvisioner = Arc::new(MockProvisioner {
            provisioned: provisioned.clone(),
            committed: committed.clone(),
            cleaned: cleaned.clone(),
            fail_commit: Some("commit conflict: head moved".into()),
        });
        let (pool, mut rx) = pool_with_provisioner(providers, provisioner);

        let eff = effect_with_payload("s1", 0, Some(b"me"), &v1_envelope_payload());
        pool.run(&eff).await.unwrap();
        let (_, _, outcome) = next_result(&mut rx).await;

        // the run's answer still delivers (R4) — wrapped, with the failure on
        // the receipt and the status degraded.
        let bytes = outcome.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["ducktape_runner_result"], 1);
        assert_eq!(
            v["response_text"],
            "answer to: GENERIC\n\nCONTRACT\n\nCONVERSATION"
        );
        assert_eq!(v["status"], "degraded", "a failed capture degrades the run");
        assert_eq!(
            v["workspace_receipt"]["commit_error"], "commit conflict: head moved",
            "the receipt records the real failure for the audit lane (I4)"
        );
        assert_eq!(
            v["workspace_receipt"]["no_changes"], false,
            "a failed capture must NEVER masquerade as a clean tree"
        );
        assert!(v["workspace_receipt"]["output_snapshot"].is_null());
        assert!(cleaned.load(Ordering::SeqCst), "cleanup still runs (W5)");
    }

    /// a workspace whose commit never resolves — the hung-actor-lane probe
    /// for the #298 bracket timeout.
    struct HungCommitProvisioner {
        cleaned: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl crate::provision::WorkspaceProvisioner for HungCommitProvisioner {
        async fn provision(
            &self,
            _spec: &WorkspaceSpec,
        ) -> Result<Box<dyn ProvisionedWorkspace>, String> {
            Ok(Box::new(HungCommitWs {
                cleaned: self.cleaned.clone(),
            }))
        }
    }

    struct HungCommitWs {
        cleaned: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl ProvisionedWorkspace for HungCommitWs {
        fn workdir(&self) -> PathBuf {
            std::env::temp_dir()
        }
        fn env(&self) -> BTreeMap<String, String> {
            BTreeMap::new()
        }
        async fn commit(
            &self,
            _audit_message: &str,
            _proposal: Option<&str>,
        ) -> Result<WorkspaceReceipt, String> {
            std::future::pending::<()>().await;
            unreachable!("a pending future never resolves")
        }
        async fn cleanup(&self) {
            self.cleaned.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn a_hung_commit_releases_the_provider_permit_but_keeps_the_attempt() {
        // the #298 bracket bound: commit blocks on the daemon actor lane, and
        // a stalled lane must not pin a pool permit until the saga deadline.
        // The provider permit is released before commit, but aggregate resource
        // admission remains fail-closed because the commit never settles.
        let (providers, _probes) = slow_providers(Duration::from_millis(5), false);
        let cleaned = Arc::new(AtomicBool::new(false));
        let provisioner: SharedProvisioner = Arc::new(HungCommitProvisioner {
            cleaned: cleaned.clone(),
        });
        let (pool, mut rx) = pool_with_provisioner(providers, provisioner);

        let eff = effect_with_payload("s1", 0, Some(b"me"), &v1_envelope_payload());
        pool.run(&eff).await.unwrap();
        assert!(no_oracle_result(&mut rx, Duration::from_millis(350)).await);
        assert_eq!(pool.in_flight(), 1, "the hung commit remains owned");
        assert!(
            !cleaned.load(Ordering::SeqCst),
            "cleanup must not race a commit that has never settled"
        );
    }

    /// a provider that panics mid-run — the unwind probe for the cleanup
    /// guard (a leaked per-run dir was #298's second bracket finding).
    struct PanicProvider {
        tag: String,
    }

    #[async_trait::async_trait]
    impl provider_host::Provider for PanicProvider {
        fn capability(&self) -> &str {
            &self.tag
        }
        async fn run(
            &self,
            _prompt: &str,
            _ctx: &provider_host::RunContext,
        ) -> Result<String, String> {
            panic!("provider crashed hard")
        }
    }

    #[tokio::test]
    async fn a_panicking_provider_still_cleans_up_and_fails_the_attempt() {
        let providers = Arc::new(ProviderSet::assemble(
            provider_host::SpecSet::from_specs(vec![spec_toml("alpha")]),
            vec![Box::new(PanicProvider {
                tag: "alpha".into(),
            })],
        ));
        let (provisioned, committed, cleaned) = flags();
        let provisioner: SharedProvisioner = Arc::new(MockProvisioner {
            provisioned: provisioned.clone(),
            committed: committed.clone(),
            cleaned: cleaned.clone(),
            fail_commit: None,
        });
        let (pool, mut rx) = pool_with_provisioner(providers, provisioner);

        let eff = effect_with_payload("s1", 0, Some(b"me"), &v1_envelope_payload());
        pool.run(&eff).await.unwrap();
        let (_, _, outcome) = next_result(&mut rx).await;

        let err = outcome.expect_err("a panic settles the attempt as a failure");
        assert!(
            err.contains("panicked") && err.contains("provider crashed hard"),
            "the panic payload surfaces in the saga error: {err}"
        );
        assert!(
            provisioned.load(Ordering::SeqCst),
            "the mount was materialized"
        );
        assert!(
            !committed.load(Ordering::SeqCst),
            "a panicked run commits NOTHING"
        );
        assert!(
            cleaned.load(Ordering::SeqCst),
            "cleanup runs even past a panicking provider — no leaked per-run dir"
        );
    }

    #[tokio::test]
    async fn untagged_and_wrong_marker_payloads_fail_the_saga_loudly() {
        let (providers, probes) = slow_providers(Duration::from_millis(5), false);
        let (pool, mut rx) = pool_with(providers, 4);

        pool.run(&effect_with_payload(
            "s1",
            0,
            Some(b"me"),
            b"the entire input",
        ))
        .await
        .unwrap();
        let (_, _, outcome) = next_result(&mut rx).await;
        let err = outcome.unwrap_err();
        assert!(err.contains("no ducktape_run envelope marker"), "got {err}");

        let mut unknown: serde_json::Value = serde_json::from_slice(&envelope_payload()).unwrap();
        unknown["ducktape_run"] = serde_json::json!(2);
        pool.run(&effect_with_payload(
            "s2",
            0,
            Some(b"me"),
            unknown.to_string().as_bytes(),
        ))
        .await
        .unwrap();
        let (_, _, outcome) = next_result(&mut rx).await;
        let err = outcome.unwrap_err();
        assert!(err.contains("marker 2"), "got {err}");
        assert_eq!(
            probes.executions.load(Ordering::SeqCst),
            0,
            "the provider is never invoked on a rejected payload"
        );
    }

    #[tokio::test]
    async fn an_oversized_prose_result_still_fits_the_saga_cap() {
        // THE wedge fix: the host-assembled RunnerResult is the saga Ok
        // payload, and the saga aborts any Ok over its result cap —
        // an uncapped assembly could then never land and the run would wedge
        // until deadline. an oversized prose answer must deliver TRUNCATED
        // (receipt intact) instead.
        let providers = Arc::new(ProviderSet::assemble(
            provider_host::SpecSet::from_specs(vec![spec_toml("alpha")]),
            vec![Box::new(HugeProvider {
                tag: "alpha".into(),
            })],
        ));
        let (provisioned, committed, cleaned) = flags();
        let provisioner: SharedProvisioner = Arc::new(MockProvisioner {
            provisioned: provisioned.clone(),
            committed: committed.clone(),
            cleaned: cleaned.clone(),
            fail_commit: None,
        });
        let (pool, mut rx) = pool_with_provisioner(providers, provisioner);

        let eff = effect_with_payload("s1", 0, Some(b"me"), &v1_envelope_payload());
        pool.run(&eff).await.unwrap();
        let (_, _, outcome) = next_result(&mut rx).await;

        let bytes = outcome.expect("the oversized run still delivers Ok");
        assert!(
            bytes.len() <= crate::module_contracts::MAX_RESULT_BYTES,
            "the delivered result must fit the saga cap ({} > {})",
            bytes.len(),
            crate::module_contracts::MAX_RESULT_BYTES
        );
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["ducktape_runner_result"], 1);
        let text = v["response_text"].as_str().unwrap();
        assert!(
            text.ends_with("bytes)]") && text.contains("[output truncated ("),
            "the truncated prose names its original size: …{}",
            &text[text.len().saturating_sub(60)..]
        );
        // the receipt survives truncation — the artifact facet is never lost.
        assert_eq!(v["workspace_receipt"]["output_snapshot"], "cc".repeat(32));
        assert!(cleaned.load(Ordering::SeqCst));
    }

    /// a provider whose answer alone exceeds the saga result cap.
    struct HugeProvider {
        tag: String,
    }

    #[async_trait::async_trait]
    impl provider_host::Provider for HugeProvider {
        fn capability(&self) -> &str {
            &self.tag
        }
        async fn run(
            &self,
            _prompt: &str,
            _ctx: &provider_host::RunContext,
        ) -> Result<String, String> {
            Ok("x".repeat(crate::module_contracts::MAX_RESULT_BYTES + 64 * 1024))
        }
    }

    /// pin the assembled wire shape against the runs producer field-for-field
    /// (a mirror of the consumer's Deserialize). a rename in EITHER crate must
    /// fail THIS test, never production — the receipt round-trips through
    /// decoder.
    #[test]
    fn assembled_runner_result_matches_the_runs_deserialize_contract() {
        // a mirror of runs' faceted Deserialize — a rename in EITHER crate must
        // fail THIS test. deny_unknown_fields mirrors runs: an assembled key
        // runs does not know is drift and must fail HERE, not in delivery.
        // facet fields carry serde defaults so the minimal shape still decodes.
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RunsRunnerResult {
            ducktape_runner_result: u32,
            response_text: String,
            #[serde(default)]
            native_input_handled: bool,
            #[serde(default)]
            native_cancelled: bool,
            workspace_receipt: RunsWorkspaceReceipt,
            #[serde(default)]
            sink: RunsSink,
            #[serde(default)]
            status: RunsStatus,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        #[allow(dead_code)]
        struct RunsWorkspaceReceipt {
            source_prefix: String,
            source_snapshot: Option<String>,
            output_snapshot: Option<String>,
            commit_height: Option<u64>,
            rebased: bool,
            no_changes: bool,
            #[serde(default)]
            commit_error: Option<String>,
            #[serde(default)]
            branch: Option<String>,
            #[serde(default)]
            output_commit: Option<String>,
        }
        #[derive(serde::Deserialize, Default, PartialEq, Debug)]
        #[serde(tag = "mode", rename_all = "snake_case")]
        enum RunsSink {
            #[default]
            Chain,
            Pr {
                repo: String,
                source_branch: String,
                target_branch: String,
                title: String,
                body: String,
            },
        }
        #[derive(serde::Deserialize, Default, PartialEq, Debug)]
        #[serde(rename_all = "snake_case")]
        enum RunsStatus {
            #[default]
            Ok,
            Degraded,
            Failed,
        }

        let receipt = WorkspaceReceipt {
            source_prefix: "/shared/agent-workspaces/bot".into(),
            source_snapshot: Some("aa".repeat(32)),
            output_snapshot: Some("cc".repeat(32)),
            commit_height: Some(9),
            rebased: true,
            no_changes: false,
            commit_error: None,
            branch: None,
            output_commit: None,
        };

        use crate::provision::{Sink, Status};

        // (1) the minimal shape (empty facets) still decodes and still yields
        //     response_text via the runs contract.
        let minimal = assemble_runner_result("the answer", &receipt, Sink::Chain, Status::Ok);
        let parsed: RunsRunnerResult = serde_json::from_slice(&minimal)
            .expect("minimal bytes deserialize into the runs contract");
        assert_eq!(parsed.ducktape_runner_result, 1);
        assert_eq!(parsed.response_text, "the answer");
        assert!(!parsed.native_input_handled);
        let handled = crate::provision::assemble_handled_input_result(&receipt, Sink::Chain, Status::Ok);
        let handled: RunsRunnerResult = serde_json::from_slice(&handled).unwrap();
        assert!(handled.native_input_handled);
        assert!(handled.response_text.is_empty());
        assert!(!handled.native_cancelled);
        let cancelled = crate::provision::assemble_cancelled_result(&receipt, Sink::Chain, Status::Ok);
        let cancelled: RunsRunnerResult = serde_json::from_slice(&cancelled).unwrap();
        assert!(cancelled.native_cancelled);
        assert!(!cancelled.native_input_handled);
        assert!(cancelled.response_text.is_empty());
        assert_eq!(
            parsed.workspace_receipt.output_snapshot,
            Some("cc".repeat(32))
        );
        assert_eq!(
            parsed.workspace_receipt.source_prefix,
            "/shared/agent-workspaces/bot"
        );
        assert_eq!(parsed.workspace_receipt.commit_height, Some(9));
        assert!(parsed.workspace_receipt.rebased);
        assert!(!parsed.workspace_receipt.no_changes);
        assert_eq!(parsed.workspace_receipt.commit_error, None);
        assert_eq!(parsed.sink, RunsSink::Chain);
        assert_eq!(parsed.status, RunsStatus::Ok);

        // (2) the facets Dispatch still owns round-trip field-for-field.
        let full = assemble_runner_result(
            "prose",
            &receipt,
            Sink::Pr {
                repo: "app".into(),
                source_branch: "agent/run".into(),
                target_branch: "main".into(),
                title: "PR".into(),
                body: "body".into(),
            },
            Status::Degraded,
        );
        let parsed: RunsRunnerResult = serde_json::from_slice(&full)
            .expect("faceted bytes deserialize into the runs contract");
        assert_eq!(parsed.status, RunsStatus::Degraded);
        assert_eq!(
            parsed.sink,
            RunsSink::Pr {
                repo: "app".into(),
                source_branch: "agent/run".into(),
                target_branch: "main".into(),
                title: "PR".into(),
                body: "body".into(),
            }
        );

        // (3) the REQUESTED-sink echo (contract §3): a Pr sink whose
        //     title/body are empty must still serialize them as PRESENT keys
        //     (runs keeps title REQUIRED on decode — the mirror has no serde
        //     default on it), and a forge receipt carrying the §5 fields must
        //     decode into runs' mirror field-for-field — both mirrors are
        //     strict, so every assembled key must be one runs knows.
        let forge_receipt = WorkspaceReceipt {
            source_prefix: "forge:app".into(),
            source_snapshot: Some("d0".repeat(20)),
            output_snapshot: None,
            commit_height: None,
            rebased: false,
            no_changes: false,
            commit_error: None,
            branch: Some("agent/item-7".into()),
            output_commit: Some("e1".repeat(20)),
        };
        let echoed = assemble_runner_result(
            "prose",
            &forge_receipt,
            Sink::Pr {
                repo: "app".into(),
                source_branch: "agent/item-7".into(),
                target_branch: "main".into(),
                title: String::new(),
                body: String::new(),
            },
            Status::Ok,
        );
        let raw = String::from_utf8(echoed.clone()).unwrap();
        assert!(
            raw.contains(r#""title":"""#) && raw.contains(r#""body":"""#),
            "empty title/body stay PRESENT keys on the wire: {raw}"
        );
        let parsed: RunsRunnerResult = serde_json::from_slice(&echoed)
            .expect("a requested-sink echo deserializes into the runs contract");
        assert_eq!(
            parsed.sink,
            RunsSink::Pr {
                repo: "app".into(),
                source_branch: "agent/item-7".into(),
                target_branch: "main".into(),
                title: String::new(),
                body: String::new(),
            }
        );
        assert_eq!(parsed.workspace_receipt.source_prefix, "forge:app");
    }

    /// the pool's gate is the shared one: announcements claim (Accept) or
    /// skip inline, foreign leases skip, unresolvable own leases error
    /// inline — none of it spawns.
    #[tokio::test]
    async fn gate_verdicts_stay_inline_and_spawn_nothing() {
        let (providers, probes) = slow_providers(Duration::from_millis(20), false);
        let (pool, _rx) = pool_with(providers, 4);

        // a servable announcement: an immediate Accept claim.
        match pool.run(&effect_for("s1", 0, None)).await.unwrap() {
            WorkOutcome::Handled(Some(msg)) => match decode_saga_msg(&msg.payload).unwrap() {
                SagaMsg::Accept { saga_id, attempt } => {
                    assert_eq!((saga_id.as_str(), attempt), ("s1", 0));
                }
                other => panic!("expected an Accept claim, got {other:?}"),
            },
            other => panic!("a servable announcement must claim, got {other:?}"),
        }

        // a foreign lease: a claimed skip.
        match pool.run(&effect_for("s2", 0, Some(b"peer"))).await.unwrap() {
            WorkOutcome::Handled(None) => {}
            other => panic!("a foreign lease must be a skip, got {other:?}"),
        }

        assert_eq!(pool.in_flight(), 0, "gate verdicts never enter the pool");
        assert_eq!(probes.executions.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_env_cap_parses_positive_integers_only() {
        // no env manipulation (parallel tests share the process): assert the
        // default directly and the parse rule via with_limit's clamp.
        assert_eq!(DEFAULT_MAX_CONCURRENT_RUNS, 4);
        let (providers, _probes) = slow_providers(Duration::from_millis(1), false);
        let (tx, _rx) = futures::channel::mpsc::unbounded::<Msg>();
        let deliver: DeliverFn = Arc::new(move |msg| {
            let tx = tx.clone();
            Box::pin(async move {
                let _ = tx.unbounded_send(msg);
            })
        });
        let pool = DispatchPool::with_limit(
            providers,
            b"me".to_vec(),
            Arc::new(|_, _fut| {}),
            deliver,
            0,
            Default::default(),
            mock_provisioner(),
        );
        // a zero cap would deadlock every run; the pool clamps to 1.
        assert_eq!(pool.semaphore.available_permits(), 1);
    }
}
