//! delivery requests, receipts and expiry — everything that moves a chat
//! message toward a provider session.
//!
//! the split the spec insists on holds here: [`deliver`] records that ONE
//! recipient was asked to carry an already-posted chat message,
//! [`acknowledge`] moves that recipient's SEPARATE delivery record, and neither
//! claims a task or asserts that a model read anything.

use sdk::{Ctx, Error, Origin, StagedStore};

use crate::bindings::{self, Advanced, live_binding};
use crate::interface::{
    BLOB_HEX_LEN, COMMIT_HEX_LEN, Credential, DUCK_SCHEME, DeliverRequest, Delivery,
    DeliveryState, EventBody, MAX_MAILBOX_QUEUED_BYTES, MAX_MAILBOX_UNDELIVERED,
    MAX_REFERENCE_BYTES, MAX_REFERENCES, MAX_UNDELIVERED_PER_SENDER, MessageKind, Party,
    Reference, TaskRef,
};
use crate::store;

/// longest stable reason token an acknowledgement may carry.
pub const MAX_REASON_BYTES: usize = 64;

/// the stable token a capacity refusal carries, so a caller can tell "retry
/// later, the queue is full" from "this message is wrong".
pub const QUEUE_FULL: &str = "queue_full";

/// what [`deliver`] settled on. an idempotent repeat answers with the record
/// the original request created and stages nothing.
pub struct Requested {
    pub delivery: Delivery,
    pub seq: u64,
    pub replayed: bool,
}

fn lowercase_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// each arm is checked STRUCTURALLY, so nothing outside the closed set can be
/// spelled at all: no local scheme, no mutable branch alias, no free-form URL.
fn check_reference(reference: &Reference) -> Result<(), Error> {
    match reference {
        Reference::Commit { repo, commit } => {
            bindings::check_id("reference repo", repo)?;
            if !lowercase_hex(commit, COMMIT_HEX_LEN) {
                return Err(Error::Module(format!(
                    "a commit reference is {COMMIT_HEX_LEN} lowercase hex characters; a branch or tag name moves and is not a reference"
                )));
            }
            Ok(())
        }
        Reference::Blob { hash } => {
            if !lowercase_hex(hash, BLOB_HEX_LEN) {
                return Err(Error::Module(format!(
                    "a blob reference is {BLOB_HEX_LEN} lowercase hex characters"
                )));
            }
            Ok(())
        }
        Reference::Duck { url } => {
            let shaped = url.starts_with(DUCK_SCHEME)
                && url.len() > DUCK_SCHEME.len()
                && url.len() <= MAX_REFERENCE_BYTES
                && !url.bytes().any(|b| b.is_ascii_whitespace() || b < 0x20);
            if !shaped {
                return Err(Error::Module(format!(
                    "a link reference is a {DUCK_SCHEME} url of at most {MAX_REFERENCE_BYTES} bytes with no whitespace"
                )));
            }
            Ok(())
        }
    }
}

/// the kind/task/reply invariants the spec states, as ONE match over the
/// discriminant rather than a ladder of ifs at the call site. a reply is a
/// chat thread reply, so `replies` is the message's own `thread`.
fn check_kind(kind: MessageKind, task: Option<&TaskRef>, replies: bool) -> Result<(), Error> {
    let satisfied = match kind {
        MessageKind::TaskUpdate => task.is_some(),
        MessageKind::Result => task.is_some() || replies,
        // a question or a task request can be purely conversational, and a
        // notice never requires either.
        MessageKind::Notice | MessageKind::Question | MessageKind::TaskRequest => true,
    };
    if satisfied {
        return Ok(());
    }
    Err(Error::Module(match kind {
        MessageKind::TaskUpdate => "a task_update must name its task".into(),
        _ => "a result must name its task or answer a thread".into(),
    }))
}

fn check_shape(request: &DeliverRequest) -> Result<(), Error> {
    bindings::check_id("channel_id", &request.channel_id)?;
    bindings::check_id("message_id", &request.message_id)?;
    bindings::check_participant("recipient", &request.recipient)?;
    if request.references.len() > MAX_REFERENCES {
        return Err(Error::Module(format!(
            "{} references, over the {MAX_REFERENCES} cap",
            request.references.len()
        )));
    }
    for reference in &request.references {
        check_reference(reference)?;
    }
    if let Some(task) = &request.task {
        bindings::check_id("task id", &task.id)?;
    }
    Ok(())
}

/// the attempt fence: a task reference must name the job's CURRENT attempt.
/// this module reads `tasks`; it never creates, claims or schedules work.
async fn check_attempt(
    ctx: &dyn Ctx,
    tasks_id: &str,
    task: &TaskRef,
    what: &str,
) -> Result<(), Error> {
    let request = tasks::encode_job_query(&tasks::JobsQuery::Get {
        job_id: task.id.clone(),
    });
    let bytes = ctx.query(tasks_id, &request).await?;
    let tasks::JobsReply::Job(job) = tasks::decode_job_reply(&bytes).map_err(Error::Module)? else {
        return Err(Error::Module("unexpected reply to task lookup".into()));
    };
    let job = job.ok_or_else(|| Error::Module(format!("no task {}", task.id)))?;
    let stale_attempt = job.attempt != task.expected_attempt;
    if stale_attempt {
        return Err(Error::Module(format!(
            "{what}: task {} is on attempt {}, not the expected {}",
            task.id, job.attempt, task.expected_attempt
        )));
    }
    Ok(())
}

/// the metadata a repeat must match byte for byte to be the same request.
fn same_request(existing: &Delivery, request: &DeliverRequest) -> bool {
    existing.kind == request.kind
        && existing.task == request.task
        && existing.references == request.references
        && existing.expires_at == request.expires_at
}

#[allow(clippy::too_many_arguments)]
pub async fn deliver(
    staged: &mut StagedStore,
    ctx: &dyn Ctx,
    actor: &Party,
    origin: &Origin,
    now: u64,
    max_delivery_ttl: u64,
    tasks_id: &str,
    chat_id: &str,
    request: DeliverRequest,
) -> Result<Requested, Error> {
    check_shape(&request)?;

    // THE MESSAGE IS CHAT'S, and so is its author. the caller must be the
    // origin that posted it — compared against the exact origin chat recorded,
    // so a service key delivers what that key posted and a program account
    // what its run posted, and nobody delivers somebody else's words.
    let message = crate::chat_message(ctx, chat_id, &request.message_id)
        .await?
        .ok_or_else(|| Error::Module(format!("no chat message {}", request.message_id)))?;
    if message.channel_id != request.channel_id {
        return Err(Error::Module(format!(
            "message {} is on channel {}, not {}",
            request.message_id, message.channel_id, request.channel_id
        )));
    }
    if &message.head.origin != origin {
        return Err(Error::Module(format!(
            "message {} was not posted by this origin",
            request.message_id
        )));
    }
    if message.head.deleted {
        return Err(Error::Module(format!(
            "message {} is deleted",
            request.message_id
        )));
    }
    let seq = message.seq;
    let sender = actor.clone();
    bindings::check_participant("sender", &sender)?;
    if sender == request.recipient {
        return Err(Error::Module("a message is not delivered to its sender".into()));
    }
    check_kind(request.kind, request.task.as_ref(), message.head.thread.is_some())?;

    // an identical repeat answers the existing record and stages nothing; a
    // different one under the same (message, recipient) is refused, never a
    // second admission.
    if let Some(existing) =
        store::delivery(staged, &request.channel_id, seq, &request.recipient).await?
    {
        if !same_request(&existing, &request) {
            return Err(Error::Module(format!(
                "message {} was already requested for this recipient with different metadata",
                request.message_id
            )));
        }
        return Ok(Requested {
            delivery: existing,
            seq: 0,
            replayed: true,
        });
    }

    // the recipient must be able to read the channel: chat's gate, asked of
    // chat, so this module never carries a second copy of the admission rule.
    let access = crate::chat_access(ctx, chat_id, &request.channel_id, &request.recipient).await?;
    if !access.may_read {
        return Err(Error::Module(format!(
            "recipient may not read channel {}",
            request.channel_id
        )));
    }
    if let Some(task) = &request.task {
        check_attempt(ctx, tasks_id, task, "stale task target").await?;
    }
    if request.expires_at <= now {
        return Err(Error::Module(format!(
            "expires_at {} is not after the block's agreed time {now}",
            request.expires_at
        )));
    }
    if request.expires_at - now > max_delivery_ttl {
        return Err(Error::Module(format!(
            "expires_at {} is more than {max_delivery_ttl} time units out",
            request.expires_at
        )));
    }

    let delivery = Delivery {
        channel_id: request.channel_id.clone(),
        seq,
        message_id: request.message_id.clone(),
        sender: sender.clone(),
        recipient: request.recipient.clone(),
        kind: request.kind,
        task: request.task.clone(),
        references: request.references.clone(),
        expires_at: request.expires_at,
        requested_at: now,
        state: DeliveryState::Stored,
        advanced_by: 0,
        reason: None,
        updated_at: now,
    };
    let encoded = sdk::wire::encode(&delivery);
    store::check_record(&encoded, "delivery")?;
    let queued_bytes = encoded.len() as u64;

    // the caps apply at admission INCLUDING while the recipient is
    // disconnected, and replacing a binding does not reset the accounting.
    let mut usage = store::mailbox(staged, &request.recipient).await?;
    if usage.undelivered >= MAX_MAILBOX_UNDELIVERED {
        return Err(Error::Module(format!(
            "{QUEUE_FULL}: the recipient holds the {MAX_MAILBOX_UNDELIVERED} undelivered message cap"
        )));
    }
    if usage.queued_bytes + queued_bytes > MAX_MAILBOX_QUEUED_BYTES {
        return Err(Error::Module(format!(
            "{QUEUE_FULL}: the recipient would exceed the {MAX_MAILBOX_QUEUED_BYTES}-byte queued payload cap"
        )));
    }
    let quota = store::sender_quota(staged, &request.recipient, &sender).await?;
    if quota >= MAX_UNDELIVERED_PER_SENDER {
        return Err(Error::Module(format!(
            "{QUEUE_FULL}: the recipient already holds {MAX_UNDELIVERED_PER_SENDER} undelivered messages from this sender"
        )));
    }
    usage.undelivered += 1;
    usage.queued_bytes += queued_bytes;

    // CHECK everything, THEN stage everything: a path that stages a write and
    // then returns Err leaves overlay residue on the native side and none in
    // the wasm port, and the two roots diverge.
    let event_seq = store::append_event(
        staged,
        &request.channel_id,
        now,
        EventBody::DeliveryRequested {
            message_seq: seq,
            sender: sender.clone(),
            recipient: request.recipient.clone(),
            kind: request.kind,
        },
    )
    .await?;
    staged.stage(
        store::delivery_key(&request.channel_id, seq, &request.recipient),
        encoded,
    );
    store::put(
        staged,
        store::mailbox_key(&request.recipient),
        &usage,
        "mailbox",
    )?;
    store::put_counter(
        staged,
        store::sender_quota_key(&request.recipient, &sender),
        quota + 1,
    );
    Ok(Requested {
        delivery,
        seq: event_seq,
        replayed: false,
    })
}

fn check_reason(reason: Option<&String>) -> Result<(), Error> {
    let Some(reason) = reason else {
        return Ok(());
    };
    // a stable snake_case token: greppable and countable, never prose and
    // never somewhere a path or a credential could hide.
    let shaped = !reason.is_empty()
        && reason.len() <= MAX_REASON_BYTES
        && reason
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if shaped {
        return Ok(());
    }
    Err(Error::Module(format!(
        "reason must be a snake_case token of at most {MAX_REASON_BYTES} bytes"
    )))
}

/// release the recipient's queue accounting once a record reaches a terminal
/// state. saturating on purpose: an accounting record that somehow lags must
/// not wedge the block.
async fn release(staged: &mut StagedStore, delivery: &Delivery, encoded_len: u64) -> Result<(), Error> {
    let mut usage = store::mailbox(staged, &delivery.recipient).await?;
    usage.undelivered = usage.undelivered.saturating_sub(1);
    usage.queued_bytes = usage.queued_bytes.saturating_sub(encoded_len);
    let quota = store::sender_quota(staged, &delivery.recipient, &delivery.sender).await?;
    store::put(
        staged,
        store::mailbox_key(&delivery.recipient),
        &usage,
        "mailbox",
    )?;
    store::put_counter(
        staged,
        store::sender_quota_key(&delivery.recipient, &delivery.sender),
        quota.saturating_sub(1),
    );
    Ok(())
}

async fn load_delivery(
    staged: &StagedStore,
    channel_id: &str,
    seq: u64,
    recipient: &Party,
) -> Result<Delivery, Error> {
    store::delivery(staged, channel_id, seq, recipient)
        .await?
        .ok_or_else(|| Error::Module(format!("no delivery of {seq} on {channel_id} for this recipient")))
}

/// move a record and log the transition as its own committed event, so a
/// consumer resuming from a cursor sees `Held`/`AdapterAccepted` too.
async fn advance(
    staged: &mut StagedStore,
    mut delivery: Delivery,
    now: u64,
    state: DeliveryState,
    reason: Option<String>,
    advanced_by: Credential,
) -> Result<Advanced, Error> {
    let was_open = !delivery.state.is_terminal();
    // the queue accounting charged the record AS ADMITTED; release exactly
    // that many bytes, not the moved record's.
    let admitted_len = sdk::wire::encode(&Delivery {
        state: DeliveryState::Stored,
        advanced_by: 0,
        reason: None,
        updated_at: delivery.requested_at,
        ..delivery.clone()
    })
    .len() as u64;
    delivery.state = state;
    delivery.advanced_by = advanced_by;
    delivery.reason = reason.clone();
    delivery.updated_at = now;
    let bytes = sdk::wire::encode(&delivery);
    store::check_record(&bytes, "delivery")?;

    let seq = store::append_event(
        staged,
        &delivery.channel_id,
        now,
        EventBody::DeliveryAdvanced {
            message_seq: delivery.seq,
            recipient: delivery.recipient.clone(),
            state,
            reason,
        },
    )
    .await?;
    staged.stage(
        store::delivery_key(&delivery.channel_id, delivery.seq, &delivery.recipient),
        bytes,
    );
    if was_open && state.is_terminal() {
        release(staged, &delivery, admitted_len).await?;
    }
    Ok(Advanced { seq })
}

#[allow(clippy::too_many_arguments)]
pub async fn acknowledge(
    staged: &mut StagedStore,
    ctx: &dyn Ctx,
    now: u64,
    tasks_id: &str,
    chat_id: &str,
    channel_id: String,
    seq: u64,
    recipient: Party,
    binding_credential: Credential,
    state: DeliveryState,
    reason: Option<String>,
) -> Result<Advanced, Error> {
    check_reason(reason.as_ref())?;
    let delivery = load_delivery(staged, &channel_id, seq, &recipient).await?;

    // permission is rechecked at DELIVERY time, not only at admission: a
    // participant removed from the channel since must not keep writing
    // receipts. chat is the roster, so chat is asked.
    let access = crate::chat_access(ctx, chat_id, &channel_id, &recipient).await?;
    if !access.may_read {
        return Err(Error::Module(format!(
            "the recipient may no longer read channel {channel_id}"
        )));
    }

    let binding = store::binding(staged, &channel_id, &recipient)
        .await?
        .ok_or_else(|| Error::Module(format!("the recipient has no binding on {channel_id}")))?;
    // ONLY the currently authorized binding advances the record. a stale
    // service may report history for inspection; it cannot overwrite this.
    // the credential names the binding the record is for, not the caller:
    // any authenticated member reports under the live one.
    if binding.detached || binding.credential != binding_credential {
        return Err(Error::Module(format!(
            "binding credential {binding_credential} is stale; the current one is {}{}",
            binding.credential,
            if binding.detached { " (detached)" } else { "" }
        )));
    }
    // EXPIRY IS THE DEADLINE'S, NOT A REPORTER'S. `expire` is permissionless
    // precisely because it checks the clock — every caller asking gets the same
    // answer. Reaching the same terminal state through an acknowledgement would
    // route around that check: a bound service could declare a still-live
    // message expired, settle it, and free its queue slot before its deadline.
    //
    // A LATE report is a different thing and stays admissible: the record
    // logs what a bound service observed, and a provider that really did
    // accept the input said so. The deadline bounds the resource, not the
    // truth — anyone may sweep an unsettled record once it passes.
    if state == DeliveryState::Expired {
        return Err(Error::Module(format!(
            "expiry is not reported: {seq} on {channel_id} expires at {} by the deadline alone",
            delivery.expires_at
        )));
    }
    if !delivery.state.may_advance_to(state) {
        return Err(Error::Module(format!(
            "delivery cannot move from {} to {}",
            delivery.state.as_str(),
            state.as_str()
        )));
    }
    // a previous attempt cannot publish as the current one after returning:
    // the task's attempt is rechecked HERE, not only at admission.
    if let Some(task) = &delivery.task {
        check_attempt(ctx, tasks_id, task, "stale attempt acknowledgement").await?;
    }
    advance(staged, delivery, now, state, reason, binding_credential).await
}

/// permissionless: the deadline is agreed network time, so every caller asking
/// gets the same answer and none of them needs authority to ask.
pub async fn expire(
    staged: &mut StagedStore,
    now: u64,
    channel_id: String,
    seq: u64,
    recipient: Party,
) -> Result<Advanced, Error> {
    let delivery = load_delivery(staged, &channel_id, seq, &recipient).await?;
    if now < delivery.expires_at {
        return Err(Error::Module(format!(
            "delivery of {seq} on {channel_id} expires at {}, not yet at {now}",
            delivery.expires_at
        )));
    }
    if delivery.state.is_terminal() {
        return Err(Error::Module(format!(
            "delivery of {seq} on {channel_id} already settled as {}",
            delivery.state.as_str()
        )));
    }
    let advanced_by = delivery.advanced_by;
    advance(
        staged,
        delivery,
        now,
        DeliveryState::Expired,
        Some("deadline_passed".into()),
        advanced_by,
    )
    .await
}

/// the live binding a recipient's records are carried under, if any — what
/// eligibility asks before saying "deliver".
pub async fn carrier(
    staged: &StagedStore,
    channel_id: &str,
    recipient: &Party,
) -> Result<bool, Error> {
    Ok(live_binding(staged, channel_id, recipient).await?.is_some())
}
