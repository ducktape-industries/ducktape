//! the `tasks` work module -- ONE consensus module hosting TWO boards:
//!
//! * a **task board** (assigned-list kind): ordered human task lists, and
//! * a **job board** (first-claim kind): a consensus-native work board where
//!   exactly one worker claim wins by consensus order.
//!
//! both are QMDB-BACKED: pure logic over a host-injected [`sdk::MerkleStore`]
//! with the shared [`StagedStore`] overlay in front of it. every record is its
//! OWN store key, so an op touches only the keys it names, `root()` is the
//! store's cached merkle root (never a re-serialization of the whole board),
//! and state-sync rides the store's resolver lane rather than a byte snapshot
//! whose size grew with every job ever submitted.
//!
//! ## the key space
//!
//! | logical key | value |
//! |---|---|
//! | `t/{task_id}` | one [`Task`] record (json) |
//! | `t#` | the task-id enumeration index (a json `BTreeSet<String>`) |
//! | `j/{job_id}` | one [`Job`] record (json) |
//! | `j#` | the live job count (u64 LE) -- what [`MAX_JOBS`] is checked against |
//! | `j@{submitter}` | that submitter's live job count (u64 LE) -- what [`MAX_LIVE_JOBS_PER_SUBMITTER`] is checked against |
//! | `w#` | the registered worker set (a json `BTreeSet<ModuleId>`) |
//!
//! the store hashes each logical key (`sdk::store_key`) and cannot enumerate,
//! so the task board carries `t#`: [`TaskQuery::List`] answers one PAGE of the
//! id order and something has to hold that order. the job board needs no such
//! index -- its only dispatch read is the by-id `Get`, and board enumeration is
//! the index guest's job on the derived tier.
//!
//! ops and queries ride ONE wire envelope ([`WorkMsg`]/[`WorkQuery`]): the
//! module's single `execute`/`query` decodes the envelope and routes to the
//! matching board. see `interface` for the wire surface.

// the wire surface: this module's shared types, flattened at the crate root.
mod interface;
pub use interface::*;

// the wasm-guest port: the dispatch shell that adapts this module to the
// ducktape:module world. compiled only by the guest-builder's synthesized
// wasm32 cdylib workspace (feature `guest`), never by the native build.
#[cfg(feature = "guest")]
mod guest;

mod job_board;
mod task_board;

pub use job_board::{
    ATTEMPTS_EXHAUSTED_RESULT, MAX_ATTEMPTS, MAX_CONTROL_ACKNOWLEDGEMENTS,
    MAX_JOB_COMMENT_TEXT_BYTES, MAX_JOB_COMMENTS, MAX_JOB_CONTROLS, MAX_JOB_ID, MAX_JOBS, MAX_KIND,
    MAX_LEASE_VIEWS, MAX_LIVE_JOBS_PER_SUBMITTER, MAX_NATIVE_RUN_ID_BYTES, MAX_PAYLOAD, MAX_SPEC,
    MAX_WORKER_EXECUTIONS, MAX_WORKER_MODULE_ID, MAX_WORKER_REPORTS, MAX_WORKER_TEXT_BYTES,
    MAX_WORKERS, MIN_LEASE_VIEWS,
};
pub use task_board::{MAX_LIST_LIMIT, MAX_OPEN_TASKS_PER_OWNER, MAX_TASK_ID, MAX_TASKS};

// the derived-tier materialized view over the task board: the PURE decision
// core (fold + view over index_guest::StateRead), compiled everywhere and
// unit-tested natively. the engine shell that runs it inside the module's
// index database is `index_guest` below.
pub mod index;

// the wasm index-mapper shell: wires the pure core into the fluent31 engine.
// compiled only by `guest-builder --index`'s synthesized wasm32 workspace
// (feature `index-guest`), never by the native build.
#[cfg(feature = "index-guest")]
mod index_guest;

use sha2::{Digest, Sha256};

use sdk::{
    Ctx, Error, MerkleStore, Module, ModuleId, Msg, Origin, ResolverSyncTarget, StagedStore,
    StateRoot, StateSyncHandle,
};

/// write-time cap on ONE stored record. the concrete store's codec bounds a
/// stored value at 1 MiB AT DECODE TIME (`statesync::qmdb::store_config`): an
/// oversized value would COMMIT fine and then panic every later read on every
/// validator -- a poison pill. the 4 KiB margin below the codec bound covers
/// the serialized operation's framing (32-byte hashed key, varint length
/// prefix, operation tag), exactly as `kv::MAX_VALUE_LEN` reasons.
///
/// this is the ONE guard the storage swap adds: an op frame may carry up to
/// `node::MAX_FRAME_BYTES` (1 MiB + 16 KiB), so an unbounded `title`/`task_id`
/// was reachable and would now poison the store.
pub const MAX_RECORD_BYTES: usize = (1 << 20) - 4 * 1024;

/// a qmdb-backed work module: the task board and the job board over ONE store.
pub struct Tasks {
    id: ModuleId,
    identity: ModuleId,
    attribution: ModuleId,
    /// the host-injected authenticated store plus this block's staging overlay
    /// (read-your-writes; folded into `root()` at `commit_block`).
    staged: StagedStore,
}

impl Tasks {
    /// wrap the host-constructed store under module identity `id`. sync -- the
    /// store arrives already opened (or already synced to a verified root).
    pub fn new(
        id: impl Into<ModuleId>,
        identity: impl Into<ModuleId>,
        attribution: impl Into<ModuleId>,
        store: Box<dyn MerkleStore>,
    ) -> Self {
        Self {
            id: id.into(),
            identity: identity.into(),
            attribution: attribution.into(),
            staged: StagedStore::new(store),
        }
    }
}

/// refuse a value the store's codec would later panic decoding. `what` names
/// the record in the rejection. an op that writes SEVERAL records checks them
/// all before staging any, so a refused op leaves no overlay entry at all.
///
/// that ordering — CHECK everything, THEN stage everything — is a root
/// invariant, not a style preference. natively this `Tasks` keeps `staged`
/// across every dispatch in a block; the wasm guest rebuilds the module per
/// dispatch and flushes its overlay only on a SUCCESSFUL execute. so a path
/// that stages a write and then returns `Err` leaves residue on one side and
/// none on the other, and the two ports diverge on the root. no path does that
/// today (`task_board::create` checks both records before staging either, and
/// every other transition stages last); keep it that way when adding one.
pub(crate) fn check_record(value: &[u8], what: &str) -> Result<(), Error> {
    if value.len() > MAX_RECORD_BYTES {
        return Err(Error::Module(format!(
            "{what} is {} bytes, over the {MAX_RECORD_BYTES}-byte store record cap",
            value.len()
        )));
    }
    Ok(())
}

/// [`check_record`] then stage — the single-record writer's shape.
pub(crate) fn stage_record(
    staged: &mut StagedStore,
    key: Vec<u8>,
    value: Vec<u8>,
    what: &str,
) -> Result<(), Error> {
    check_record(&value, what)?;
    staged.stage(key, value);
    Ok(())
}

/// The source actor is an account when identity knows it, otherwise the
/// authenticated key or module remains a distinct non-account principal.
async fn actor_from_origin(ctx: &dyn Ctx, identity: &str) -> Result<Party, Error> {
    match &ctx.env().origin {
        Origin::Program(account) => {
            let account = require_account(ctx, identity, *account).await?;
            let is_active_program = matches!(
                account.control,
                identity::Control::Program {
                    standing: identity::ProgramStanding::Active,
                    ..
                }
            );
            if !is_active_program {
                return Err(Error::Module("program account is not active".into()));
            }
            Ok(Party::Account(account.number))
        }
        Origin::External(key) => {
            if key.is_empty() {
                return Err(Error::Module(
                    "external origin must carry a non-empty submitter id".into(),
                ));
            }
            let query = identity::IdentityQuery::OfKey { key: key.clone() };
            let reply = identity_reply(ctx, identity, query).await?;
            let identity::IdentityReply::Account(account) = reply else {
                return Err(Error::Module(
                    "identity returned an unexpected reply".into(),
                ));
            };
            Ok(account.map_or_else(
                || Party::Key(key.clone()),
                |account| Party::Account(account.number),
            ))
        }
        Origin::Module(module) => Ok(Party::Module(module.clone())),
        Origin::System => Ok(Party::System),
    }
}

/// whether the actor holds a work lease (a job claim's worker): the one
/// consent the boards keep, since a claimant's in-flight work is theirs to
/// finalize or release. a key-held lease stays with that signer after
/// account admission; an account-held lease is the resolved account's,
/// including its other keys. admission never silently transfers a lease.
pub(crate) fn controls(holder: &Party, actor: &Party, origin: &Origin) -> bool {
    match holder {
        Party::Key(key) => matches!(origin, Origin::External(signer) if signer == key),
        Party::Account(_) | Party::Module(_) | Party::System => holder == actor,
    }
}

async fn identity_reply(
    ctx: &dyn Ctx,
    identity: &str,
    query: identity::IdentityQuery,
) -> Result<identity::IdentityReply, Error> {
    let bytes = ctx.query(identity, &identity::encode_query(&query)).await?;
    identity::decode_reply(&bytes).map_err(Error::Module)
}

async fn require_account(
    ctx: &dyn Ctx,
    identity: &str,
    number: u64,
) -> Result<identity::AccountView, Error> {
    let reply = identity_reply(ctx, identity, identity::IdentityQuery::Get { number }).await?;
    let identity::IdentityReply::Account(Some(account)) = reply else {
        return Err(Error::Module("task owner account does not exist".into()));
    };
    Ok(account)
}

/// The revision record is retained when its board object is removed.
async fn next_revision(
    staged: &StagedStore,
    kind: &str,
    object: &str,
) -> Result<(Vec<u8>, u64), Error> {
    let key = sdk::wire::encode(&("attribution_revision", kind, object));
    let revision = match staged.get(&key).await? {
        Some(bytes) => u64::from_le_bytes(
            bytes
                .try_into()
                .map_err(|_| Error::Module("invalid attribution revision".into()))?,
        ),
        None => 0,
    };
    let next = revision
        .checked_add(1)
        .ok_or_else(|| Error::Module("attribution revision exhausted".into()))?;
    Ok((key, next))
}

fn relation(
    relations: &mut Vec<attribution::Relation>,
    party: &Party,
    reason: attribution::Reason,
    detail: Vec<u8>,
) {
    let Party::Account(recipient) = party else {
        return;
    };
    relations.push(attribution::Relation {
        recipient: *recipient,
        reason,
        detail,
    });
}

fn task_relations(task: &Task) -> Vec<attribution::Relation> {
    let mut relations = Vec::new();
    relation(
        &mut relations,
        &task.owner,
        attribution::Reason::Ownership,
        sdk::wire::encode(&task.status),
    );
    relations
}

fn job_relations(job: &Job) -> Vec<attribution::Relation> {
    let mut relations = Vec::new();
    let detail = sdk::wire::encode(&job.status);
    relation(
        &mut relations,
        &job.submitter,
        attribution::Reason::Authorship,
        detail.clone(),
    );
    relation(
        &mut relations,
        &job.submitter,
        attribution::Reason::Ownership,
        detail.clone(),
    );
    if let Some(claim) = &job.claim {
        relation(
            &mut relations,
            &claim.worker,
            attribution::Reason::Assignment,
            detail.clone(),
        );
    }
    if job.result.is_some() {
        relation(
            &mut relations,
            &job.submitter,
            attribution::Reason::Result,
            detail,
        );
    }
    relations
}

/// Reasons remain static; operation identity belongs to the immutable source,
/// not to a dynamic reason name or a detail-only update of a retained relation.
fn worker_event_identity(msg: &JobsMsg) -> Option<(&'static str, &str)> {
    match msg {
        JobsMsg::Submit { .. } | JobsMsg::SubmitConversation { .. } => {
            Some(("worker_submitted", ""))
        }
        JobsMsg::Continue { operation_id, .. } => Some(("worker_continued", operation_id)),
        JobsMsg::Control { operation_id, .. } => Some(("worker_control", operation_id)),
        JobsMsg::AcknowledgeControl { operation_id, .. } => {
            Some(("worker_control_acknowledged", operation_id))
        }
        JobsMsg::Checkpoint {
            kind, operation_id, ..
        } => match kind {
            WorkerReportKind::Checkpoint => Some(("worker_checkpoint", operation_id)),
            WorkerReportKind::Report => Some(("worker_report", operation_id)),
        },
        JobsMsg::SettleCancellation { operation_id, .. } => {
            Some(("worker_cancelled", operation_id))
        }
        JobsMsg::Cancel { .. } => Some(("worker_cancelled", "")),
        JobsMsg::Finalize { .. } => Some(("worker_settled", "")),
        JobsMsg::Comment { .. }
        | JobsMsg::Claim { .. }
        | JobsMsg::Release { .. }
        | JobsMsg::Reclaim { .. }
        | JobsMsg::Prune { .. }
        | JobsMsg::RegisterWorker { .. }
        | JobsMsg::UnregisterWorker { .. }
        | JobsMsg::CheckpointNativeHistory { .. } => None,
    }
}

/// Each conversation event adds a fresh relation, so every checkpoint and receipt
/// is delivered even when its recipient and reason match an earlier event.
/// One-shot jobs retain only their ordinary lifecycle and Result attribution.
fn job_event_report(
    job: &Job,
    actor: &Party,
    height: u64,
    operation: JobsMsg,
) -> Option<attribution::AttributionMsg> {
    let is_conversation = job.execution == JobExecution::Conversation;
    if !is_conversation {
        return None;
    }
    let Party::Account(recipient) = &job.submitter else {
        return None;
    };
    let (reason, operation_id) = worker_event_identity(&operation)?;
    let identity = sdk::wire::encode(&(
        &job.job_id,
        job.created_at_revision,
        reason,
        operation_id,
        job.attempt,
    ));
    let object = format!("{:x}", Sha256::digest(identity));
    let detail = JobEventDetail {
        job_id: job.job_id.clone(),
        conversation_id: job.conversation_id.clone(),
        job_kind: job.kind.clone(),
        created_at_revision: job.created_at_revision,
        job_attempt: job.attempt,
        submitter: job.submitter.clone(),
        actor: actor.clone(),
        height,
        operation,
    };
    Some(attribution::AttributionMsg::Attribute {
        object: attribution::ObjectRef {
            kind: "job_event".into(),
            object,
        },
        revision: 1,
        actor: actor.clone(),
        relations: vec![attribution::Relation {
            recipient: *recipient,
            reason: attribution::Reason::Defined(reason.into()),
            detail: sdk::wire::encode(&detail),
        }],
        transfers: Vec::new(),
    })
}

impl Tasks {
    fn publish(
        &mut self,
        ctx: &mut dyn Ctx,
        actor: Party,
        object: attribution::ObjectRef,
        revision: (Vec<u8>, u64),
        relations: Vec<attribution::Relation>,
    ) {
        self.staged
            .stage(revision.0, revision.1.to_le_bytes().to_vec());
        ctx.emit_msg(Msg {
            target: self.attribution.clone(),
            payload: attribution::encode_msg(&attribution::AttributionMsg::Attribute {
                object,
                revision: revision.1,
                actor,
                relations,
                transfers: Vec::new(),
            }),
        });
    }

    async fn on_task(&mut self, ctx: &mut dyn Ctx, msg: TaskMsg) -> Result<(), Error> {
        let actor = actor_from_origin(ctx, &self.identity).await?;
        let task_id = match &msg {
            TaskMsg::CreateTask { task_id, owner, .. } => {
                if let Some(owner) = owner {
                    require_account(ctx, &self.identity, *owner).await?;
                }
                task_id.clone()
            }
            TaskMsg::UpdateStatus { task_id, .. } | TaskMsg::DeleteTask { task_id } => {
                task_id.clone()
            }
        };
        let revision = next_revision(&self.staged, "task", &task_id).await?;
        let before = task_board::load(&self.staged, &task_id).await?;
        task_board::execute(&mut self.staged, &actor, msg, ctx.env().consensus_time).await?;
        let after = task_board::load(&self.staged, &task_id).await?;
        let owner = after
            .as_ref()
            .or(before.as_ref())
            .map(|task| task.owner.clone())
            .unwrap_or(actor.clone());
        ctx.set_assigned(encode_assigned(&WorkAssigned::Task {
            actor: actor.clone(),
            owner,
        }));
        ctx.set_output(encode_task_reply(&TaskReply::Task(after.clone())));
        if before == after {
            return Ok(());
        }
        let relations = after.as_ref().map(task_relations).unwrap_or_default();
        self.publish(
            ctx,
            actor,
            attribution::ObjectRef {
                kind: "task".into(),
                object: task_id,
            },
            revision,
            relations,
        );
        Ok(())
    }

    /// Machine heads participate in consensus and retention, not in source
    /// attribution: a snapshot boundary is not a semantic report to an account.
    async fn on_native_history(
        &mut self,
        ctx: &mut dyn Ctx,
        job_id: String,
        msg: JobsMsg,
        actor: &Party,
    ) -> Result<(), Error> {
        job_board::execute(&mut self.staged, ctx, msg, actor, &self.id).await?;
        let after = job_board::load(&self.staged, &job_id).await?;
        ctx.set_assigned(encode_assigned(&WorkAssigned::Job {
            actor: actor.clone(),
        }));
        ctx.set_output(encode_job_reply(&JobsReply::Job(after)));
        Ok(())
    }

    async fn on_job(&mut self, ctx: &mut dyn Ctx, msg: JobsMsg) -> Result<(), Error> {
        let actor = actor_from_origin(ctx, &self.identity).await?;
        let job_id = match &msg {
            JobsMsg::CheckpointNativeHistory { job_id, .. } => {
                return self
                    .on_native_history(ctx, job_id.clone(), msg, &actor)
                    .await;
            }
            JobsMsg::Comment { job_id, .. }
            | JobsMsg::Submit { job_id, .. }
            | JobsMsg::SubmitConversation { job_id, .. }
            | JobsMsg::Continue { job_id, .. }
            | JobsMsg::Control { job_id, .. }
            | JobsMsg::AcknowledgeControl { job_id, .. }
            | JobsMsg::SettleCancellation { job_id, .. }
            | JobsMsg::Checkpoint { job_id, .. }
            | JobsMsg::Claim { job_id, .. }
            | JobsMsg::Finalize { job_id, .. }
            | JobsMsg::Release { job_id }
            | JobsMsg::Reclaim { job_id }
            | JobsMsg::Cancel { job_id }
            | JobsMsg::Prune { job_id } => job_id.clone(),
            JobsMsg::RegisterWorker {} | JobsMsg::UnregisterWorker {} => {
                return job_board::execute(&mut self.staged, ctx, msg, &actor, &self.id).await;
            }
        };
        let revision = next_revision(&self.staged, "job", &job_id).await?;
        let before = job_board::load(&self.staged, &job_id).await?;
        let worker_operation = worker_event_identity(&msg).map(|_| msg.clone());
        job_board::execute(&mut self.staged, ctx, msg, &actor, &self.id).await?;
        let after = job_board::load(&self.staged, &job_id).await?;
        ctx.set_assigned(encode_assigned(&WorkAssigned::Job {
            actor: actor.clone(),
        }));
        ctx.set_output(encode_job_reply(&JobsReply::Job(after.clone())));
        if before == after {
            return Ok(());
        }
        let relations = after.as_ref().map(job_relations).unwrap_or_default();
        let event = worker_operation.and_then(|operation| {
            job_event_report(after.as_ref()?, &actor, ctx.env().height, operation)
        });
        self.publish(
            ctx,
            actor,
            attribution::ObjectRef {
                kind: "job".into(),
                object: job_id,
            },
            revision,
            relations,
        );
        if let Some(event) = event {
            ctx.emit_msg(Msg {
                target: self.attribution.clone(),
                payload: attribution::encode_msg(&event),
            });
        }
        Ok(())
    }
}

#[async_trait::async_trait(?Send)]
impl Module for Tasks {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    /// the REAL merkle root over all committed records, cached by the store --
    /// never a re-serialization of the boards.
    fn root(&self) -> StateRoot {
        self.staged.root()
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        self.staged.state_sync_handle()
    }

    /// the network state-sync serve lane: answers the shared qmdb wire requests
    /// (historical proof-carrying op ranges) from committed state. read-only.
    async fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        self.staged.serve_sync(req).await
    }

    async fn resolver_sync_target(&self) -> Result<ResolverSyncTarget, Error> {
        self.staged.sync_target().await
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        match decode_work_msg(&msg.payload).map_err(Error::Module)? {
            WorkMsg::Task(msg) => self.on_task(ctx, msg).await,
            WorkMsg::Job(msg) => self.on_job(ctx, msg).await,
        }
    }

    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        match decode_work_query(req).map_err(Error::Module)? {
            WorkQuery::Task(task_query) => Ok(encode_work_reply(&WorkReply::Task(
                task_board::query(&self.staged, task_query).await?,
            ))),
            WorkQuery::Job(job_query) => Ok(encode_work_reply(&WorkReply::Job(
                job_board::query(&self.staged, job_query).await?,
            ))),
        }
    }

    /// publish the block's staged writes AND deletes in ONE store batch.
    async fn commit_block(&mut self) -> Result<(), Error> {
        self.staged.commit().await
    }

    /// discard the block's staged writes -- nothing reached the store, so
    /// `root()` is unchanged.
    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged.abort();
        Ok(())
    }
}
