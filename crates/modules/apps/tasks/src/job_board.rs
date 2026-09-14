//! the job board (first-claim kind): a deterministic work board over the
//! module's qmdb store.
//!
//! # what the job board is (and what it is NOT)
//!
//! it is a PRODUCT-level work board for on-platform actors: humans and agent
//! fleets submitting ordinary signed ops over RPC. a submitter posts a job; any
//! worker claims it; **exactly one claim wins by consensus order** -- there is
//! no distributed lock because the total order IS the lock (the same first-claim
//! discipline the agent module uses for run-id turn claims). the claimant does
//! the work off-platform and reports a result; result singularity is enforced
//! in-state. jobs are open-ended work items with a human-visible lifecycle:
//! pending -> processing (claimed, leased) -> finalized (done/failed), plus
//! release, permissionless lease-expiry reclaim, cancel, and prune.
//!
//! the job board is DISTINCT from `saga`. `saga` is the platform's async-RPC
//! ledger for REACTOR effects -- off-consensus work the node's own worker loop
//! performs, addressed by module code. the job board never touches saga or the
//! reactor: workers act by submitting ordinary signed ops, and every "identity"
//! (`submitter`, claim `worker`) is derived from the dispatch [`Origin`] inside
//! this board, never read off the wire.
//!
//! # why a losing claim fails the block
//!
//! every transition guard produces an [`Error::Module`] rejection with a precise
//! message, which fails the submitter's block. that is correct and load-bearing:
//! a claim that lost the race must fail loudly and deterministically on every
//! node -- the rejection IS the product's race-resolution signal.
//!
//! # state
//!
//! one record per job (`j/{job_id}`), the live-job census in `j#`, the
//! per-submitter census in `j@{submitter}` (so no ONE account can fill a
//! shared board -- [`MAX_LIVE_JOBS_PER_SUBMITTER`], the task board's
//! `MAX_OPEN_TASKS_PER_OWNER` shape), and the
//! registered worker set in `w#`. a transition reads ONE record, rewrites it,
//! and stages the result -- no board walk. Each execution is also retained in
//! `ja/`, with a bounded conversation index in `wc/` and latest-ID pointer in
//! `jh/`. `Prune` deletes only the board record, not conversation history.
//! The census is a
//! counter because [`MAX_JOBS`] is checked per submit and the store cannot
//! enumerate.
//!
//! transition guards read through the staged overlay, so in-block effects (a
//! first claim) are visible to later ops in the same block. the `Get` query
//! answers from COMMITTED state only, so a read never leaks a staged write that
//! a block abort would take back.

use std::collections::BTreeSet;

use sdk::{Ctx, Error, ModuleId, Msg, Origin, StagedStore};
use sha2::{Digest, Sha256};

use crate::{
    Claim, ControlAcknowledgement, Job, JobComment, JobControl, JobControlInput, JobExecution,
    JobResult, JobStatus, JobsEvent, JobsMsg, JobsQuery, JobsReply, NativeHistoryHead, Party,
    WorkerHistory, WorkerReport, WorkerReportKind, controls, encode_job_event, stage_record,
};

/// max bytes of a `job_id` (non-empty).
pub const MAX_JOB_ID: usize = 256;
/// max bytes of a `kind` (non-empty).
pub const MAX_KIND: usize = 64;
/// max bytes of a job `spec`.
pub const MAX_SPEC: usize = 64 * 1024;
/// max bytes of a finalize `payload`.
pub const MAX_PAYLOAD: usize = 64 * 1024;
/// Bounded discussion per execution, retained with its conversation.
pub const MAX_JOB_COMMENTS: usize = 64;
pub const MAX_JOB_COMMENT_TEXT_BYTES: usize = 4096;
/// max distinct live job ids on the board.
pub const MAX_JOBS: usize = 65536;
/// max live job records ONE submitter may hold at once, well under
/// [`MAX_JOBS`]: no single account can fill a shared board with its own
/// [`MAX_SPEC`]-sized records (at most 64 MiB of spec bytes per submitter,
/// plus bounded record metadata). [`JobsMsg::Prune`] lets a submitter recede.
/// "live" means the same thing the board census means: the RECORD exists.
/// finalizing or cancelling a job does not free the slot, because the record
/// (and its spec bytes) is still on the board -- only a prune drops it.
pub const MAX_LIVE_JOBS_PER_SUBMITTER: usize = 1024;
/// lower clamp for a claim lease, in views.
pub const MIN_LEASE_VIEWS: u64 = 10;
/// upper clamp for a claim lease, in views.
pub const MAX_LEASE_VIEWS: u64 = 10_000;
/// after this many claims, an expired reclaim fails the job instead of requeuing.
pub const MAX_ATTEMPTS: u64 = 8;
/// the result payload a reclaim writes when it gives up on a job. the index
/// mapper folds the same string, so the two tiers cannot drift.
pub const ATTEMPTS_EXHAUSTED_RESULT: &str = "attempts exhausted";
/// max registered worker modules notified on each successful submit.
pub const MAX_WORKERS: usize = 16;
/// max bytes of a worker module id.
pub const MAX_WORKER_MODULE_ID: usize = 256;
/// Bounds are admission limits, never automatic archive deletion.
pub const MAX_JOB_CONTROLS: usize = 32;
pub const MAX_CONTROL_ACKNOWLEDGEMENTS: usize = 64;
pub const MAX_WORKER_REPORTS: usize = 32;
pub const MAX_WORKER_TEXT_BYTES: usize = 4096;
pub const MAX_WORKER_EXECUTIONS: usize = 64;
/// Native run keys are opaque and may contain Runs' internal separators.
pub const MAX_NATIVE_RUN_ID_BYTES: usize = 1024;

/// Only references are kept together; each retained execution has its own key.
/// Continuing never rewrites the terminal predecessor's bytes.
#[derive(serde::Serialize, serde::Deserialize)]
struct Conversation {
    conversation_id: String,
    executions: Vec<(String, u64)>,
}

fn conversation_key(conversation_id: &str) -> Vec<u8> {
    [b"wc/", conversation_id.as_bytes()].concat()
}

fn archive_key(job_id: &str, revision: u64) -> Vec<u8> {
    [b"ja/".as_slice(), &sdk::wire::encode(&(job_id, revision))].concat()
}

fn latest_key(job_id: &str) -> Vec<u8> {
    [b"jh/", job_id.as_bytes()].concat()
}

async fn retained(staged: &StagedStore, job_id: &str) -> Result<Option<Job>, Error> {
    let Some(key) = staged.get(&latest_key(job_id)).await? else {
        return Ok(None);
    };
    let Some(bytes) = staged.get(&key).await? else {
        return Err(Error::Module("retained execution missing".into()));
    };
    decode_job(&bytes).map(Some)
}

async fn conversation(staged: &StagedStore, id: &str) -> Result<Conversation, Error> {
    let Some(bytes) = staged.get(&conversation_key(id)).await? else {
        return Err(Error::Module("worker conversation missing".into()));
    };
    sdk::wire::decode(&bytes).map_err(Error::Module)
}

/// one job record per id.
const RECORD_PREFIX: &[u8] = b"j/";
/// the live-job census (u64 LE) -- what [`MAX_JOBS`] is checked against.
const COUNT_KEY: &[u8] = b"j#";
/// the per-submitter live-job census (u64 LE), one record per submitter --
/// what [`MAX_LIVE_JOBS_PER_SUBMITTER`] is checked against. a zero count drops
/// the key, the same rule [`stage_count`] follows.
/// The key encodes the stored Party: account keys share one counter; old
/// key-owned records retain theirs after admission, just like their authority.
const SUBMITTER_COUNT_PREFIX: &[u8] = b"j@";
/// the registered worker set (a json `BTreeSet<ModuleId>`, at most
/// [`MAX_WORKERS`] entries).
const WORKERS_KEY: &[u8] = b"w#";

fn record_key(job_id: &str) -> Vec<u8> {
    let mut key = RECORD_PREFIX.to_vec();
    key.extend_from_slice(job_id.as_bytes());
    key
}

fn decode_job(bytes: &[u8]) -> Result<Job, Error> {
    sdk::wire::decode(bytes).map_err(|e| Error::Module(format!("job record decode: {e}")))
}

// ---- overlay-aware reads (execute-internal ONLY) ---------------------------
//
// transition guards must see in-block effects -- a second claim in the same
// block has to observe the first claim's staged `Processing` -- so these read
// through the overlay. the `Get` query does NOT.

/// the live view of a single job, reading through the staged overlay.
pub(crate) async fn load(staged: &StagedStore, job_id: &str) -> Result<Option<Job>, Error> {
    let Some(bytes) = staged.get(&record_key(job_id)).await? else {
        return Ok(None);
    };
    decode_job(&bytes).map(Some)
}

/// the live job or a precise not-found rejection.
async fn require(staged: &StagedStore, job_id: &str) -> Result<Job, Error> {
    load(staged, job_id)
        .await?
        .ok_or_else(|| Error::Module(format!("job not found: {job_id}")))
}

/// one census counter, reading through the staged overlay. an absent key is 0.
async fn read_census(staged: &StagedStore, key: &[u8]) -> Result<u64, Error> {
    let Some(bytes) = staged.get(key).await? else {
        return Ok(0);
    };
    let raw: [u8; 8] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| Error::Module("job census record is not a u64".into()))?;
    Ok(u64::from_le_bytes(raw))
}

/// count of distinct live job ids, reading through the staged overlay.
async fn live_count(staged: &StagedStore) -> Result<u64, Error> {
    read_census(staged, COUNT_KEY).await
}

fn submitter_count_key(submitter: &Party) -> Vec<u8> {
    let mut key = SUBMITTER_COUNT_PREFIX.to_vec();
    key.extend_from_slice(&sdk::wire::encode(submitter));
    key
}

/// count of one submitter's live job records, through the staged overlay.
async fn submitter_count(staged: &StagedStore, submitter: &Party) -> Result<u64, Error> {
    read_census(staged, &submitter_count_key(submitter)).await
}

/// stage a submitter's census (see [`SUBMITTER_COUNT_PREFIX`] on the zero case).
fn stage_submitter_count(staged: &mut StagedStore, submitter: &Party, count: u64) {
    let key = submitter_count_key(submitter);
    if count == 0 {
        staged.delete(key);
        return;
    }
    staged.stage(key, count.to_le_bytes().to_vec());
}

/// Stage the census. An empty board drops the counter entirely; source
/// revision records remain so a recreated job cannot reuse an attribution.
fn stage_count(staged: &mut StagedStore, count: u64) {
    if count == 0 {
        staged.delete(COUNT_KEY.to_vec());
        return;
    }
    staged.stage(COUNT_KEY.to_vec(), count.to_le_bytes().to_vec());
}

/// the registered worker set, reading through the staged overlay. `BTreeSet`
/// serializes ASCENDING, so the record bytes are canonical.
async fn load_workers(staged: &StagedStore) -> Result<BTreeSet<ModuleId>, Error> {
    let Some(bytes) = staged.get(WORKERS_KEY).await? else {
        return Ok(BTreeSet::new());
    };
    sdk::wire::decode(&bytes).map_err(|e| Error::Module(format!("worker set decode: {e}")))
}

/// stage the worker set — an EMPTY set drops the key (see [`stage_count`]).
fn stage_workers(staged: &mut StagedStore, workers: &BTreeSet<ModuleId>) -> Result<(), Error> {
    if workers.is_empty() {
        staged.delete(WORKERS_KEY.to_vec());
        return Ok(());
    }
    stage_record(
        staged,
        WORKERS_KEY.to_vec(),
        sdk::wire::encode(workers),
        "worker set",
    )
}

fn stage_job(staged: &mut StagedStore, job: &Job) -> Result<(), Error> {
    let bytes = sdk::wire::encode(job);
    crate::check_record(&bytes, "job record")?;
    let key = archive_key(&job.job_id, job.created_at_revision);
    staged.stage(record_key(&job.job_id), bytes.clone());
    staged.stage(key.clone(), bytes);
    staged.stage(latest_key(&job.job_id), key);
    Ok(())
}

fn worker_module_from_origin(origin: &Origin, module_id: &ModuleId) -> Result<ModuleId, Error> {
    let Origin::Module(worker) = origin else {
        return Err(Error::Module(
            "worker registration requires a module origin".into(),
        ));
    };
    if worker.is_empty() {
        return Err(Error::Module("worker module_id must not be empty".into()));
    }
    if worker.len() > MAX_WORKER_MODULE_ID {
        return Err(Error::Module(format!(
            "worker module_id exceeds {MAX_WORKER_MODULE_ID} bytes"
        )));
    }
    if worker == module_id {
        return Err(Error::Module(
            "the work module cannot register itself as a worker".into(),
        ));
    }
    Ok(worker.clone())
}

async fn register_worker(
    staged: &mut StagedStore,
    origin: &Origin,
    module_id: &ModuleId,
) -> Result<(), Error> {
    let worker = worker_module_from_origin(origin, module_id)?;
    let mut workers = load_workers(staged).await?;
    if workers.contains(&worker) {
        return Ok(());
    }
    if workers.len() >= MAX_WORKERS {
        return Err(Error::Module("worker cap reached".into()));
    }
    workers.insert(worker);
    stage_workers(staged, &workers)
}

async fn unregister_worker(
    staged: &mut StagedStore,
    origin: &Origin,
    module_id: &ModuleId,
) -> Result<(), Error> {
    let worker = worker_module_from_origin(origin, module_id)?;
    let mut workers = load_workers(staged).await?;
    if !workers.remove(&worker) {
        return Ok(());
    }
    stage_workers(staged, &workers)
}

// ---- transitions (each fails the block on any guard violation) -------------

enum Creation {
    Fresh {
        kind: String,
        execution: JobExecution,
    },
    Continuation {
        previous: Box<Job>,
        operation_id: String,
        kind: String,
    },
}

async fn create_execution(
    staged: &mut StagedStore,
    job_id: String,
    spec: String,
    creation: Creation,
    actor: &Party,
    height: u64,
) -> Result<JobsEvent, Error> {
    let (kind, execution, submitter, previous_job_id, continuation_operation_id, mut history) =
        match creation {
            Creation::Fresh { kind, execution } => {
                (kind, execution, actor.clone(), None, None, None)
            }
            Creation::Continuation {
                previous,
                operation_id,
                kind,
            } => {
                let history = conversation(staged, &previous.conversation_id).await?;
                let at_capacity = history.executions.len() >= MAX_WORKER_EXECUTIONS;
                if at_capacity {
                    return Err(Error::Module(
                        "worker conversation execution cap reached".into(),
                    ));
                }
                (
                    kind,
                    previous.execution,
                    previous.submitter,
                    Some(previous.job_id),
                    Some(operation_id),
                    Some(history),
                )
            }
        };
    // enforce every size cap HERE, at execute time, with rejection -- so
    // oversized bytes never reach a committed record (the repo's poison-value
    // lesson).
    if job_id.is_empty() {
        return Err(Error::Module("job_id must not be empty".into()));
    }
    if job_id.len() > MAX_JOB_ID {
        return Err(Error::Module(format!("job_id exceeds {MAX_JOB_ID} bytes")));
    }
    if kind.is_empty() {
        return Err(Error::Module("kind must not be empty".into()));
    }
    if kind.len() > MAX_KIND {
        return Err(Error::Module(format!("kind exceeds {MAX_KIND} bytes")));
    }
    if spec.len() > MAX_SPEC {
        return Err(Error::Module(format!("spec exceeds {MAX_SPEC} bytes")));
    }
    if load(staged, &job_id).await?.is_some() {
        return Err(Error::Module(format!("job already exists: {job_id}")));
    }
    let count = live_count(staged).await?;
    if count >= MAX_JOBS as u64 {
        return Err(Error::Module(format!(
            "job board full: {MAX_JOBS} live jobs"
        )));
    }

    let submitter_live = submitter_count(staged, &submitter).await?;
    if submitter_live >= MAX_LIVE_JOBS_PER_SUBMITTER as u64 {
        return Err(Error::Module(format!(
            "job submitter at cap: {MAX_LIVE_JOBS_PER_SUBMITTER} live jobs"
        )));
    }
    let spec_hash = Sha256::digest(spec.as_bytes()).to_vec();
    let (_, created_at_revision) = crate::next_revision(staged, "job", &job_id).await?;
    let history = history.get_or_insert_with(|| Conversation {
        conversation_id: format!("{job_id}:{created_at_revision}"),
        executions: Vec::new(),
    });
    history
        .executions
        .push((job_id.clone(), created_at_revision));
    let history_bytes = sdk::wire::encode(history);
    crate::check_record(&history_bytes, "worker conversation")?;
    stage_job(
        staged,
        &Job {
            job_id: job_id.clone(),
            execution,
            conversation_id: history.conversation_id.clone(),
            previous_job_id,
            continuation_operation_id,
            controls: Vec::new(),
            reports: Vec::new(),
            native_history: None,
            kind: kind.clone(),
            spec: spec.clone(),
            submitter: submitter.clone(),
            status: JobStatus::Pending,
            attempt: 0,
            claim: None,
            result: None,
            comments: Vec::new(),
            created_at_revision,
            created_at_height: height,
            updated_at_height: height,
        },
    )?;
    staged.stage(conversation_key(&history.conversation_id), history_bytes);
    stage_count(staged, count + 1);
    stage_submitter_count(staged, &submitter, submitter_live + 1);
    Ok(JobsEvent::Submitted {
        job_id,
        kind,
        submitter,
        spec,
        spec_hash,
    })
}

async fn notify_workers(
    staged: &StagedStore,
    ctx: &mut dyn Ctx,
    event: &JobsEvent,
) -> Result<(), Error> {
    for worker in load_workers(staged).await? {
        ctx.emit_msg(Msg {
            target: worker,
            payload: encode_job_event(event),
        });
    }
    Ok(())
}

async fn submit(
    staged: &mut StagedStore,
    ctx: &mut dyn Ctx,
    job_id: String,
    kind: String,
    spec: String,
    execution: JobExecution,
    actor: &Party,
) -> Result<(), Error> {
    let event = create_execution(
        staged,
        job_id,
        spec,
        Creation::Fresh { kind, execution },
        actor,
        ctx.env().height,
    )
    .await?;
    notify_workers(staged, ctx, &event).await
}

async fn continue_worker(
    staged: &mut StagedStore,
    ctx: &mut dyn Ctx,
    msg: JobsMsg,
    actor: &Party,
) -> Result<(), Error> {
    let JobsMsg::Continue {
        previous_job_id,
        job_id,
        operation_id,
        kind,
        spec,
    } = msg
    else {
        return Err(Error::Module("expected continuation".into()));
    };
    sdk::validate_id("operation_id", &operation_id, MAX_JOB_ID)?;
    let Some(previous) = retained(staged, &previous_job_id).await? else {
        return Err(Error::Module("previous execution not found".into()));
    };
    let is_conversation = previous.execution == JobExecution::Conversation;
    if !is_conversation {
        return Err(Error::Module(
            "continue requires a conversation execution".into(),
        ));
    }
    if !previous.status.is_terminal() {
        return Err(Error::Module(
            "continue requires a terminal execution".into(),
        ));
    }
    let history = conversation(staged, &previous.conversation_id).await?;
    let previous_ref = (previous.job_id.clone(), previous.created_at_revision);
    let is_latest = history.executions.last() == Some(&previous_ref);
    if !is_latest {
        // A retry must match the original continuation exactly, never launch twice.
        let Some(next) = retained(staged, &job_id).await? else {
            return Err(Error::Module(
                "continue requires the latest execution".into(),
            ));
        };
        let same_continuation = next.conversation_id == previous.conversation_id
            && next.previous_job_id.as_deref() == Some(previous_job_id.as_str())
            && next.continuation_operation_id.as_deref() == Some(operation_id.as_str())
            && next.kind == kind
            && next.spec == spec;
        if same_continuation {
            return Ok(());
        }
        return Err(Error::Module(
            "continue conflicts with an existing continuation".into(),
        ));
    }
    let reused_id = retained(staged, &job_id).await?.is_some();
    if reused_id {
        return Err(Error::Module("continuation requires a fresh job_id".into()));
    }
    // Operation IDs are unique within a conversation, not just one predecessor.
    for (id, revision) in &history.executions {
        let Some(bytes) = staged.get(&archive_key(id, *revision)).await? else {
            return Err(Error::Module("retained execution missing".into()));
        };
        let execution = decode_job(&bytes)?;
        let reused_operation =
            execution.continuation_operation_id.as_deref() == Some(operation_id.as_str());
        if reused_operation {
            return Err(Error::Module(
                "continuation operation_id already exists".into(),
            ));
        }
    }
    let event = create_execution(
        staged,
        job_id,
        spec,
        Creation::Continuation {
            previous: Box::new(previous),
            operation_id,
            kind,
        },
        actor,
        ctx.env().height,
    )
    .await?;
    notify_workers(staged, ctx, &event).await
}

fn require_claim_attempt(
    job: &Job,
    actor: &Party,
    origin: &Origin,
    attempt: u64,
) -> Result<(), Error> {
    let current_claim = job.status == JobStatus::Processing
        && job.attempt == attempt
        && job
            .claim
            .as_ref()
            .is_some_and(|claim| controls(&claim.worker, actor, origin));
    if !current_claim {
        return Err(Error::Module(
            "operation requires the current claimant and attempt".into(),
        ));
    }
    Ok(())
}

fn require_worker_text(text: &str) -> Result<(), Error> {
    let valid = !text.trim().is_empty() && text.len() <= MAX_WORKER_TEXT_BYTES;
    if !valid {
        return Err(Error::Module(
            "worker input requires non-empty text within the byte cap".into(),
        ));
    }
    Ok(())
}

async fn control(
    staged: &mut StagedStore,
    ctx: &dyn Ctx,
    job_id: String,
    operation_id: String,
    input: JobControlInput,
    actor: &Party,
) -> Result<(), Error> {
    sdk::validate_id("operation_id", &operation_id, MAX_JOB_ID)?;
    if let JobControlInput::Steer { text } = &input {
        require_worker_text(text)?;
    }
    let mut job = require(staged, &job_id).await?;
    if let Some(existing) = job
        .controls
        .iter()
        .find(|control| control.operation_id == operation_id)
    {
        let identical = existing.input == input && existing.author == *actor;
        if identical {
            return Ok(());
        }
        return Err(Error::Module("control operation_id already exists".into()));
    }
    if job.status.is_terminal() {
        return Err(Error::Module(
            "controls require a nonterminal execution".into(),
        ));
    }
    let at_capacity = job.controls.len() >= MAX_JOB_CONTROLS;
    if at_capacity {
        return Err(Error::Module("job control cap reached".into()));
    }
    let report_id = job
        .reports
        .iter()
        .any(|report| report.operation_id == operation_id);
    if report_id {
        return Err(Error::Module(
            "operation_id already used by a report".into(),
        ));
    }
    job.controls.push(JobControl {
        operation_id,
        input,
        author: actor.clone(),
        height: ctx.env().height,
        acknowledgements: Vec::new(),
    });
    job.updated_at_height = ctx.env().height;
    stage_job(staged, &job)
}

async fn acknowledge_control(
    staged: &mut StagedStore,
    ctx: &dyn Ctx,
    job_id: String,
    operation_id: String,
    attempt: u64,
    actor: &Party,
) -> Result<(), Error> {
    let mut job = require(staged, &job_id).await?;
    require_claim_attempt(&job, actor, &ctx.env().origin, attempt)?;
    let Some(control) = job
        .controls
        .iter_mut()
        .find(|control| control.operation_id == operation_id)
    else {
        return Err(Error::Module("control not found".into()));
    };
    let already_acknowledged = control
        .acknowledgements
        .iter()
        .any(|ack| ack.attempt == attempt);
    if already_acknowledged {
        return Ok(());
    }
    let at_capacity = control.acknowledgements.len() >= MAX_CONTROL_ACKNOWLEDGEMENTS;
    if at_capacity {
        return Err(Error::Module("control acknowledgement cap reached".into()));
    }
    control.acknowledgements.push(ControlAcknowledgement {
        worker: actor.clone(),
        attempt,
        height: ctx.env().height,
    });
    job.updated_at_height = ctx.env().height;
    stage_job(staged, &job)
}

async fn settle_cancellation(
    staged: &mut StagedStore,
    ctx: &dyn Ctx,
    job_id: String,
    operation_id: String,
    attempt: u64,
    payload: String,
    actor: &Party,
) -> Result<(), Error> {
    let mut job = require(staged, &job_id).await?;
    require_claim_attempt(&job, actor, &ctx.env().origin, attempt)?;
    let acknowledged_cancel = job.controls.iter().any(|control| {
        control.operation_id == operation_id
            && control.input == JobControlInput::Cancel
            && control
                .acknowledgements
                .iter()
                .any(|ack| ack.attempt == attempt)
    });
    if !acknowledged_cancel {
        return Err(Error::Module(
            "cancellation requires acknowledgement by the current attempt".into(),
        ));
    }
    let oversized = payload.len() > MAX_PAYLOAD;
    if oversized {
        return Err(Error::Module(format!(
            "payload exceeds {MAX_PAYLOAD} bytes"
        )));
    }
    job.status = JobStatus::Cancelled;
    job.result = Some(JobResult {
        ok: false,
        payload: payload.clone(),
    });
    // One reserved final report links settlement to its exact control request.
    job.reports.push(WorkerReport {
        operation_id,
        worker: actor.clone(),
        attempt,
        height: ctx.env().height,
        kind: WorkerReportKind::Report,
        payload,
    });
    job.updated_at_height = ctx.env().height;
    stage_job(staged, &job)
}

/// Native snapshots replace a bounded pointer; they never consume semantic
/// report slots. The claimant fence applies even to an otherwise exact replay.
async fn checkpoint_native_history(
    staged: &mut StagedStore,
    ctx: &dyn Ctx,
    msg: JobsMsg,
    actor: &Party,
) -> Result<(), Error> {
    let JobsMsg::CheckpointNativeHistory {
        job_id,
        attempt,
        run_id,
        execution_attempt,
        revision,
        snapshot,
    } = msg
    else {
        return Err(Error::Module("expected native history checkpoint".into()));
    };
    let valid_run_id = !run_id.is_empty() && run_id.len() <= MAX_NATIVE_RUN_ID_BYTES;
    if !valid_run_id {
        return Err(Error::Module(format!(
            "native run_id must be nonempty and at most {MAX_NATIVE_RUN_ID_BYTES} bytes"
        )));
    }
    sdk::validate_id("snapshot", &snapshot, MAX_JOB_ID)?;
    let positive_revision = revision > 0;
    if !positive_revision {
        return Err(Error::Module(
            "native history revision must be positive".into(),
        ));
    }
    let mut job = require(staged, &job_id).await?;
    require_claim_attempt(&job, actor, &ctx.env().origin, attempt)?;
    if let Some(head) = &job.native_history {
        let same_claim = head.job_attempt == attempt;
        let same_run = head.run_id == run_id;
        let same_execution = head.execution_attempt == execution_attempt;
        let identical = same_claim
            && same_run
            && same_execution
            && head.revision == revision
            && head.snapshot == snapshot;
        if identical {
            return Ok(());
        }
        let newer_revision = revision > head.revision;
        if !newer_revision {
            return Err(Error::Module(
                "native history revision is stale or conflicting".into(),
            ));
        }
        let changed_run_in_claim = same_claim && !same_run;
        if changed_run_in_claim {
            return Err(Error::Module(
                "native history run is fenced to the current job claim".into(),
            ));
        }
        let stale_execution = same_run && execution_attempt < head.execution_attempt;
        if stale_execution {
            return Err(Error::Module(
                "native history execution attempt is stale".into(),
            ));
        }
    }
    job.native_history = Some(NativeHistoryHead {
        job_attempt: attempt,
        worker: actor.clone(),
        run_id,
        execution_attempt,
        revision,
        snapshot,
        height: ctx.env().height,
    });
    job.updated_at_height = ctx.env().height;
    stage_job(staged, &job)
}

async fn checkpoint(
    staged: &mut StagedStore,
    ctx: &dyn Ctx,
    msg: JobsMsg,
    actor: &Party,
) -> Result<(), Error> {
    let JobsMsg::Checkpoint {
        job_id,
        operation_id,
        attempt,
        kind,
        payload,
    } = msg
    else {
        return Err(Error::Module("expected checkpoint".into()));
    };
    sdk::validate_id("operation_id", &operation_id, MAX_JOB_ID)?;
    require_worker_text(&payload)?;
    let mut job = require(staged, &job_id).await?;
    require_claim_attempt(&job, actor, &ctx.env().origin, attempt)?;
    if let Some(existing) = job
        .reports
        .iter()
        .find(|report| report.operation_id == operation_id)
    {
        let identical = existing.attempt == attempt
            && existing.worker == *actor
            && existing.kind == kind
            && existing.payload == payload;
        if identical {
            return Ok(());
        }
        return Err(Error::Module("report operation_id already exists".into()));
    }
    let control_id = job
        .controls
        .iter()
        .any(|control| control.operation_id == operation_id);
    if control_id {
        return Err(Error::Module(
            "operation_id already used by a control".into(),
        ));
    }
    let at_capacity = job.reports.len() >= MAX_WORKER_REPORTS;
    if at_capacity {
        return Err(Error::Module("worker report cap reached".into()));
    }
    job.reports.push(WorkerReport {
        operation_id,
        worker: actor.clone(),
        attempt,
        height: ctx.env().height,
        kind,
        payload,
    });
    job.updated_at_height = ctx.env().height;
    stage_job(staged, &job)
}

async fn claim(
    staged: &mut StagedStore,
    job_id: String,
    lease_views: u64,
    actor: &Party,
    height: u64,
) -> Result<(), Error> {
    let mut job = require(staged, &job_id).await?;
    if job.status != JobStatus::Pending {
        // the consensus order already picked the winner; this op lost the
        // race and fails deterministically on every node.
        return Err(Error::Module(format!(
            "job not claimable (status {:?}): {job_id}",
            job.status
        )));
    }
    let worker = actor.clone();
    job.status = JobStatus::Processing;
    job.attempt = job.attempt.saturating_add(1);
    job.claim = Some(Claim {
        worker,
        claimed_at_height: height,
        lease_views: lease_views.clamp(MIN_LEASE_VIEWS, MAX_LEASE_VIEWS),
    });
    job.updated_at_height = height;
    stage_job(staged, &job)
}

async fn comment(
    staged: &mut StagedStore,
    job_id: String,
    created_at_revision: u64,
    comment_id: String,
    text: String,
    actor: &Party,
    height: u64,
) -> Result<(), Error> {
    sdk::validate_id("comment_id", &comment_id, MAX_JOB_ID)?;
    let valid_text = !text.trim().is_empty() && text.len() <= MAX_JOB_COMMENT_TEXT_BYTES;
    if !valid_text {
        return Err(Error::Module(
            "job comments require non-empty text within the byte cap".into(),
        ));
    }
    let mut job = require(staged, &job_id).await?;
    let same_instance = job.created_at_revision == created_at_revision;
    if !same_instance {
        return Err(Error::Module(
            "job was replaced before the comment could be applied".into(),
        ));
    }
    let full = job.comments.len() >= MAX_JOB_COMMENTS;
    if full {
        return Err(Error::Module("job discussion is full".into()));
    }
    let duplicate = job.comments.iter().any(|comment| comment.id == comment_id);
    if duplicate {
        return Err(Error::Module("job comment id already exists".into()));
    }
    job.comments.push(JobComment {
        id: comment_id,
        author: actor.clone(),
        text,
        height,
    });
    job.updated_at_height = height;
    stage_job(staged, &job)
}

async fn finalize(
    staged: &mut StagedStore,
    job_id: String,
    ok: bool,
    payload: String,
    actor: &Party,
    origin: &Origin,
    height: u64,
) -> Result<(), Error> {
    let mut job = require(staged, &job_id).await?;
    // result singularity: a terminal job is not `Processing`, so this guard
    // rejects any second finalize.
    if job.status != JobStatus::Processing {
        return Err(Error::Module(format!(
            "job not in processing (status {:?}): {job_id}",
            job.status
        )));
    }
    let is_claimant = job
        .claim
        .as_ref()
        .is_some_and(|claim| controls(&claim.worker, actor, origin));
    if !is_claimant {
        return Err(Error::Module(format!(
            "only the current claimant may finalize: {job_id}"
        )));
    }
    if payload.len() > MAX_PAYLOAD {
        return Err(Error::Module(format!(
            "payload exceeds {MAX_PAYLOAD} bytes"
        )));
    }
    job.status = if ok {
        JobStatus::Done
    } else {
        JobStatus::Failed
    };
    job.result = Some(JobResult { ok, payload });
    job.updated_at_height = height;
    stage_job(staged, &job) // claim retained for the record
}

async fn release(
    staged: &mut StagedStore,
    job_id: String,
    actor: &Party,
    origin: &Origin,
    height: u64,
) -> Result<(), Error> {
    let mut job = require(staged, &job_id).await?;
    if job.status != JobStatus::Processing {
        return Err(Error::Module(format!(
            "job not in processing (status {:?}): {job_id}",
            job.status
        )));
    }
    let is_claimant = job
        .claim
        .as_ref()
        .is_some_and(|claim| controls(&claim.worker, actor, origin));
    if !is_claimant {
        return Err(Error::Module(format!(
            "only the current claimant may release: {job_id}"
        )));
    }
    job.status = JobStatus::Pending;
    job.claim = None; // attempt count kept
    job.updated_at_height = height;
    stage_job(staged, &job)
}

async fn reclaim(staged: &mut StagedStore, job_id: String, height: u64) -> Result<(), Error> {
    // PERMISSIONLESS (saga's permissionless-crank pattern): any origin may
    // reclaim, because the ONLY thing that authorizes it is a consensus fact
    // -- a deterministic deadline of heights, identical on every node.
    let mut job = require(staged, &job_id).await?;
    if job.status != JobStatus::Processing {
        return Err(Error::Module(format!(
            "reclaim only applies to processing jobs (status {:?}): {job_id}",
            job.status
        )));
    }
    let claim = job
        .claim
        .as_ref()
        .ok_or_else(|| Error::Module(format!("processing job missing claim: {job_id}")))?;
    let deadline = claim.claimed_at_height.saturating_add(claim.lease_views);
    if height <= deadline {
        return Err(Error::Module(format!(
            "lease not expired (height {height} <= deadline {deadline}): {job_id}"
        )));
    }
    if job.attempt >= MAX_ATTEMPTS {
        // give up: fail the job. the claim is retained for the record, the
        // same way a finalize-to-terminal keeps its claim.
        job.status = JobStatus::Failed;
        job.result = Some(JobResult {
            ok: false,
            payload: ATTEMPTS_EXHAUSTED_RESULT.into(),
        });
    } else {
        job.status = JobStatus::Pending;
        job.claim = None; // attempt count kept for the next claim to bump
    }
    job.updated_at_height = height;
    stage_job(staged, &job)
}

/// any member cancels a still-pending job: the submitter is attribution and
/// the census key, not a consent the cancel needs. a claim is a work lease,
/// and the lease does gate.
async fn cancel(staged: &mut StagedStore, job_id: String, height: u64) -> Result<(), Error> {
    let mut job = require(staged, &job_id).await?;
    // once claimed, the worker holds it until finalize/release/lease expiry.
    if job.status != JobStatus::Pending {
        return Err(Error::Module(format!(
            "cancel only applies to pending jobs (status {:?}): {job_id}",
            job.status
        )));
    }
    job.status = JobStatus::Cancelled;
    job.updated_at_height = height;
    stage_job(staged, &job)
}

/// any member prunes a terminal job's record; the slot freed is the
/// submitter's, whoever prunes.
async fn prune(staged: &mut StagedStore, job_id: String) -> Result<(), Error> {
    let job = require(staged, &job_id).await?;
    if !job.status.is_terminal() {
        return Err(Error::Module(format!(
            "prune only applies to terminal jobs (status {:?}): {job_id}",
            job.status
        )));
    }
    let count = live_count(staged).await?;
    staged.delete(record_key(&job_id));
    stage_count(staged, count.saturating_sub(1));
    // the prune is the ONLY path that frees a board slot -- both censuses
    // recede together, the record's disappearance being what each counts.
    let submitter_live = submitter_count(staged, &job.submitter).await?;
    stage_submitter_count(staged, &job.submitter, submitter_live.saturating_sub(1));
    Ok(())
}

// ---- dispatch --------------------------------------------------------------

pub(crate) async fn execute(
    staged: &mut StagedStore,
    ctx: &mut dyn Ctx,
    msg: JobsMsg,
    actor: &Party,
    module_id: &ModuleId,
) -> Result<(), Error> {
    let env = ctx.env();
    let (origin, height) = (env.origin.clone(), env.height);
    match msg {
        JobsMsg::Comment {
            job_id,
            created_at_revision,
            comment_id,
            text,
        } => {
            comment(
                staged,
                job_id,
                created_at_revision,
                comment_id,
                text,
                actor,
                height,
            )
            .await
        }
        JobsMsg::Submit { job_id, kind, spec } => {
            submit(
                staged,
                ctx,
                job_id,
                kind,
                spec,
                JobExecution::OneShot,
                actor,
            )
            .await
        }
        JobsMsg::SubmitConversation { job_id, kind, spec } => {
            submit(
                staged,
                ctx,
                job_id,
                kind,
                spec,
                JobExecution::Conversation,
                actor,
            )
            .await
        }
        msg @ JobsMsg::Continue { .. } => continue_worker(staged, ctx, msg, actor).await,
        JobsMsg::Control {
            job_id,
            operation_id,
            input,
        } => control(staged, ctx, job_id, operation_id, input, actor).await,
        JobsMsg::AcknowledgeControl {
            job_id,
            operation_id,
            attempt,
        } => acknowledge_control(staged, ctx, job_id, operation_id, attempt, actor).await,
        JobsMsg::SettleCancellation {
            job_id,
            operation_id,
            attempt,
            payload,
        } => settle_cancellation(staged, ctx, job_id, operation_id, attempt, payload, actor).await,
        msg @ JobsMsg::Checkpoint { .. } => checkpoint(staged, ctx, msg, actor).await,
        msg @ JobsMsg::CheckpointNativeHistory { .. } => {
            checkpoint_native_history(staged, ctx, msg, actor).await
        }
        JobsMsg::Claim {
            job_id,
            lease_views,
        } => claim(staged, job_id, lease_views, actor, height).await,
        JobsMsg::Finalize {
            job_id,
            ok,
            payload,
        } => finalize(staged, job_id, ok, payload, actor, &origin, height).await,
        JobsMsg::Release { job_id } => release(staged, job_id, actor, &origin, height).await,
        JobsMsg::Reclaim { job_id } => reclaim(staged, job_id, height).await,
        JobsMsg::Cancel { job_id } => cancel(staged, job_id, height).await,
        JobsMsg::Prune { job_id } => prune(staged, job_id).await,
        JobsMsg::RegisterWorker {} => register_worker(staged, &origin, module_id).await,
        JobsMsg::UnregisterWorker {} => unregister_worker(staged, &origin, module_id).await,
    }
}

async fn get_committed_job(staged: &StagedStore, job_id: &str) -> Result<JobsReply, Error> {
    let Some(bytes) = staged.get_committed(&record_key(job_id)).await? else {
        return Ok(JobsReply::Job(None));
    };
    decode_job(&bytes).map(|job| JobsReply::Job(Some(job)))
}

async fn get_worker(staged: &StagedStore, conversation_id: String) -> Result<JobsReply, Error> {
    let Some(bytes) = staged
        .get_committed(&conversation_key(&conversation_id))
        .await?
    else {
        return Ok(JobsReply::Worker(None));
    };
    let history: Conversation = sdk::wire::decode(&bytes).map_err(Error::Module)?;
    let mut executions = Vec::with_capacity(history.executions.len());
    for (job_id, revision) in history.executions {
        let Some(bytes) = staged
            .get_committed(&archive_key(&job_id, revision))
            .await?
        else {
            return Err(Error::Module("retained execution missing".into()));
        };
        executions.push(decode_job(&bytes)?);
    }
    Ok(JobsReply::Worker(Some(WorkerHistory {
        conversation_id,
        executions,
    })))
}

async fn get_controls(staged: &StagedStore, job_id: &str) -> Result<JobsReply, Error> {
    let Some(key) = staged.get_committed(&latest_key(job_id)).await? else {
        return Ok(JobsReply::Controls(Vec::new()));
    };
    let Some(bytes) = staged.get_committed(&key).await? else {
        return Err(Error::Module("retained execution missing".into()));
    };
    Ok(JobsReply::Controls(decode_job(&bytes)?.controls))
}

/// the read projection answers from COMMITTED state only -- never the staged
/// overlay. a query must not observe a write that a block abort would take
/// back; transition guards keep their own overlay-aware view above.
pub(crate) async fn query(staged: &StagedStore, q: JobsQuery) -> Result<JobsReply, Error> {
    match q {
        JobsQuery::Get { job_id } => get_committed_job(staged, &job_id).await,
        JobsQuery::GetWorker { conversation_id } => get_worker(staged, conversation_id).await,
        JobsQuery::Controls { job_id } => get_controls(staged, &job_id).await,
    }
}
