//! A queued steer is not delivered until its native user/receipt is in the
//! accepted history. Job-scoped opaque ids survive worker retries without
//! colliding with another job's operation names.
use super::*;

#[derive(Clone)]
pub(super) struct ControlReceipt {
    pub(super) text: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CancellationReceipt {
    pub(super) conversation_id: String,
    pub(super) turn_id: String,
    id: String,
}

pub(super) struct Cancellation {
    acknowledgement: Acknowledgement,
    id: String,
}

#[derive(Clone)]
pub(super) struct Acknowledgement {
    job_id: String,
    job_attempt: u64,
    operation_id: String,
}

pub(super) fn qualified_id(job_id: &str, operation_id: &str) -> String {
    serde_json::to_string(&(job_id, operation_id)).expect("job control identity serializes")
}

fn receipt_id(data: &Value) -> Result<&str, String> {
    let Some(id) = data.get("id").and_then(Value::as_str) else {
        return Err("native control receipt has no id".into());
    };
    let identity: (String, String) = serde_json::from_str(id)
        .map_err(|_| "native control id is not a qualified job operation".to_string())?;
    let canonical = !identity.0.is_empty()
        && !identity.1.is_empty()
        && qualified_id(&identity.0, &identity.1) == id;
    if !canonical {
        return Err("native control id is not canonical".into());
    }
    Ok(id)
}

pub(super) fn cancellation_receipt(data: &Value) -> Result<(String, CancellationReceipt), String> {
    let id = receipt_id(data)?;
    let receipt = serde_json::from_value(data.clone())
        .map_err(|_| "native cancellation receipt has invalid identity".to_string())?;
    Ok((id.into(), receipt))
}

pub(super) fn receipt(
    data: &Value,
    users: &BTreeMap<String, Value>,
) -> Result<(String, ControlReceipt), String> {
    let id = receipt_id(data)?;
    let message = data
        .get("message_id")
        .and_then(Value::as_str)
        .and_then(|id| users.get(id))
        .ok_or_else(|| "native control receipt has no actual user entry".to_string())?;
    if message.get("ducktape_control_id").and_then(Value::as_str) != Some(id) {
        return Err("native control receipt does not match its user entry".into());
    }
    let text = match &message["content"] {
        Value::String(text) => Some(text.as_str()),
        Value::Array(parts) if parts.len() == 1 && parts[0]["type"] == "text" => {
            parts[0]["text"].as_str()
        }
        _ => None,
    }
    .ok_or_else(|| "native control user has no exact steer text".to_string())?;
    Ok((id.into(), ControlReceipt { text: text.into() }))
}

pub(super) fn validate_claims(ids: &[String], receipts: &HistoryReceipts) -> Result<(), String> {
    let requested: BTreeSet<_> = ids.iter().collect();
    let recorded: BTreeSet<_> = receipts
        .controls
        .keys()
        .chain(receipts.cancellations.keys())
        .collect();
    let exact = requested.len() == ids.len()
        && requested == recorded
        && recorded.len() == receipts.controls.len() + receipts.cancellations.len();
    if !exact {
        return Err("native control claims do not match durable native receipts".into());
    }
    Ok(())
}

pub(super) async fn worker_controls(
    state: &ActionState,
) -> Result<Option<runs_wire::WorkerControls>, String> {
    let bytes = state
        .node
        .query(
            RUNS_MODULE,
            &runs_wire::encode_query(&runs_wire::RunsQuery::WorkerControls {
                run_id: state.run_id.clone(),
            }),
        )
        .await?;
    let runs_wire::RunsReply::WorkerControls(controls) = runs_wire::decode_reply(&bytes)? else {
        return Err("unexpected worker controls reply".into());
    };
    Ok(controls)
}

fn acknowledged(control: &tasks::JobControl, job_attempt: u64) -> bool {
    control.acknowledgements.iter().any(|ack| {
        ack.attempt == job_attempt && ack.worker == tasks::Party::Module(RUNS_MODULE.into())
    })
}

pub(super) fn plan(
    receipts: &HistoryReceipts,
    approved: &HistoryReceipts,
    worker: Option<&runs_wire::WorkerControls>,
) -> Result<Vec<Acknowledgement>, String> {
    let mut result = Vec::new();
    for (id, receipt) in &receipts.controls {
        let current = worker.and_then(|worker| {
            worker
                .controls
                .iter()
                .find(|control| qualified_id(&worker.job_id, &control.operation_id) == *id)
                .map(|control| (worker, control))
        });
        let Some((worker, control)) = current else {
            if !approved.controls.contains_key(id) {
                return Err("native control receipt names an unknown job control".into());
            }
            continue;
        };
        let tasks::JobControlInput::Steer { text } = &control.input else {
            return Err("native cancellation requires a cancellation boundary receipt".into());
        };
        if text != &receipt.text {
            return Err("native steer differs from the committed control text".into());
        }
        if acknowledged(control, worker.job_attempt) {
            continue;
        }
        result.push(Acknowledgement {
            job_id: worker.job_id.clone(),
            job_attempt: worker.job_attempt,
            operation_id: control.operation_id.clone(),
        });
    }
    Ok(result)
}

pub(super) fn cancellation_plan(
    receipts: &HistoryReceipts,
    approved: &HistoryReceipts,
    worker: Option<&runs_wire::WorkerControls>,
    context: &NativeConversationContext,
) -> Result<Option<Cancellation>, String> {
    let mut cancellations = Vec::new();
    for (id, receipt) in &receipts.cancellations {
        let current = worker.and_then(|worker| {
            worker
                .controls
                .iter()
                .find(|control| qualified_id(&worker.job_id, &control.operation_id) == *id)
                .map(|control| (worker, control))
        });
        let Some((worker, control)) = current else {
            if !approved.cancellations.contains_key(id) {
                return Err("native cancellation names an unknown job control".into());
            }
            continue;
        };
        let bound = receipt.conversation_id == context.conversation_id
            && receipt.turn_id == context.turn_id
            && receipt.id == *id
            && control.input == tasks::JobControlInput::Cancel;
        if !bound {
            return Err("native cancellation receipt is outside this turn's cancel scope".into());
        }
        cancellations.push(Cancellation {
            id: id.clone(),
            acknowledgement: Acknowledgement {
                job_id: worker.job_id.clone(),
                job_attempt: worker.job_attempt,
                operation_id: control.operation_id.clone(),
            },
        });
    }
    if cancellations.len() > 1 {
        return Err("native history has multiple current cancellation boundaries".into());
    }
    Ok(cancellations.pop())
}

pub(super) async fn complete_boundary(
    state: &ActionState,
    native: &NativeState,
    acknowledgements: &[Acknowledgement],
    cancellation: Option<&Cancellation>,
) -> Result<(), String> {
    acknowledge(state, native, acknowledgements).await?;
    let Some(cancellation) = cancellation else {
        return Ok(());
    };
    acknowledge(
        state,
        native,
        std::slice::from_ref(&cancellation.acknowledgement),
    )
    .await?;
    settle_cancellation(state, native, cancellation).await
}

pub(super) async fn poll(
    state: &ActionState,
    native: &NativeState,
    approved: &HistoryReceipts,
) -> Result<Value, String> {
    let current = worker_controls(state).await?;
    // A recovered snapshot can already contain the user marker even when the
    // old host died before the Jobs acknowledgement. ACK it without steering
    // the same input into the resumed native tree again.
    let pending_ack = plan(approved, approved, current.as_ref())?;
    let cancellation = cancellation_plan(approved, approved, current.as_ref(), &native.context)?;
    complete_boundary(state, native, &pending_ack, cancellation.as_ref()).await?;
    if let Some(cancellation) = cancellation {
        return Ok(
            serde_json::json!({"control":"continue", "messages":[], "cancel":{"id":cancellation.id}}),
        );
    }
    let current = worker_controls(state).await?;
    let Some(worker) = current else {
        return Ok(serde_json::json!({"control":"continue", "messages":[]}));
    };
    let cancel = worker
        .controls
        .iter()
        .find(|control| control.input == tasks::JobControlInput::Cancel);
    if let Some(cancel) = cancel {
        return Ok(
            serde_json::json!({"control":"continue", "messages":[], "cancel":{"id":qualified_id(&worker.job_id, &cancel.operation_id)}}),
        );
    }
    let mut messages = Vec::new();
    for control in &worker.controls {
        if acknowledged(control, worker.job_attempt) {
            continue;
        }
        match &control.input {
            tasks::JobControlInput::Steer { text } => messages.push(serde_json::json!({
                "id":qualified_id(&worker.job_id, &control.operation_id), "text":text, "kind":"steer",
            })),
            tasks::JobControlInput::Cancel => return Err("native cancellation escaped boundary selection".into()),
        }
    }
    Ok(serde_json::json!({"control":"continue", "messages":messages}))
}

async fn acknowledgement_result(
    state: &ActionState,
    native: &NativeState,
    expected: &Acknowledgement,
) -> Result<bool, String> {
    require_lease(&state.node, &state.signer, &state.run_id, native.attempt).await?;
    let current = worker_controls(state).await?.ok_or_else(|| {
        "native worker job disappeared before control acknowledgement".to_string()
    })?;
    let same_claim =
        current.job_id == expected.job_id && current.job_attempt == expected.job_attempt;
    if !same_claim {
        return Err("native job control claim moved".into());
    }
    let control = current
        .controls
        .iter()
        .find(|control| control.operation_id == expected.operation_id)
        .ok_or_else(|| "native job control disappeared".to_string())?;
    let complete = acknowledged(control, current.job_attempt);
    let exhausted =
        !complete && control.acknowledgements.len() >= tasks_module::MAX_CONTROL_ACKNOWLEDGEMENTS;
    if exhausted {
        return Err("native job control acknowledgement cap reached".into());
    }
    Ok(complete)
}

pub(super) async fn acknowledge(
    state: &ActionState,
    native: &NativeState,
    acknowledgements: &[Acknowledgement],
) -> Result<(), String> {
    for acknowledgement in acknowledgements {
        let mut seq = state.seq.lock().await;
        let mut events = action_events(&state.node).await?;
        if acknowledgement_result(state, native, acknowledgement).await? {
            continue;
        }
        let message = sdk::Msg {
            target: RUNS_MODULE.into(),
            payload: runs_wire::encode_msg(&runs_wire::RunsMsg::AcknowledgeJobControl {
                run_id: state.run_id.clone(),
                attempt: native.attempt,
                operation_id: acknowledgement.operation_id.clone(),
            }),
        };
        let frame = node::encode_frame(&state.signer, *seq, &message);
        *seq = seq
            .checked_add(1)
            .ok_or_else(|| "action signer sequence exhausted".to_string())?;
        state.node.submit_frame(frame).await?;
        let mut observed_height = None;
        loop {
            if acknowledgement_result(state, native, acknowledgement).await? {
                break;
            }
            next_block(&mut events, &mut observed_height).await?;
        }
    }
    Ok(())
}

fn cancellation_payload(native: &NativeState, cancellation: &Cancellation) -> String {
    serde_json::json!({"native_cancelled":true, "conversation_id":native.context.conversation_id,
        "turn_id":native.context.turn_id, "control_id":cancellation.id})
    .to_string()
}

async fn settlement_result(
    state: &ActionState,
    native: &NativeState,
    cancellation: &Cancellation,
    payload: &str,
) -> Result<bool, String> {
    require_lease(&state.node, &state.signer, &state.run_id, native.attempt).await?;
    let current = worker_controls(state)
        .await?
        .ok_or_else(|| "native cancellation job disappeared".to_string())?;
    let same_claim = current.job_id == cancellation.acknowledgement.job_id
        && current.job_attempt == cancellation.acknowledgement.job_attempt;
    if !same_claim {
        return Err("native cancellation claim moved".into());
    }
    match current.job_status {
        tasks::JobStatus::Processing => Ok(false),
        tasks::JobStatus::Cancelled => {
            let exact = current
                .result
                .as_ref()
                .is_some_and(|result| !result.ok && result.payload == payload);
            if !exact {
                return Err("native cancellation committed a different result".into());
            }
            Ok(true)
        }
        tasks::JobStatus::Pending | tasks::JobStatus::Done | tasks::JobStatus::Failed => {
            Err("native cancellation job is no longer processing".into())
        }
    }
}

async fn settle_cancellation(
    state: &ActionState,
    native: &NativeState,
    cancellation: &Cancellation,
) -> Result<(), String> {
    let payload = cancellation_payload(native, cancellation);
    let mut seq = state.seq.lock().await;
    let mut events = action_events(&state.node).await?;
    if settlement_result(state, native, cancellation, &payload).await? {
        return Ok(());
    }
    let message = sdk::Msg {
        target: RUNS_MODULE.into(),
        payload: runs_wire::encode_msg(&runs_wire::RunsMsg::SettleJobCancellation {
            run_id: state.run_id.clone(),
            attempt: native.attempt,
            operation_id: cancellation.acknowledgement.operation_id.clone(),
            payload: payload.clone(),
        }),
    };
    let frame = node::encode_frame(&state.signer, *seq, &message);
    *seq = seq
        .checked_add(1)
        .ok_or_else(|| "action signer sequence exhausted".to_string())?;
    state.node.submit_frame(frame).await?;
    let mut observed_height = None;
    loop {
        if settlement_result(state, native, cancellation, &payload).await? {
            return Ok(());
        }
        next_block(&mut events, &mut observed_height).await?;
    }
}

pub(super) async fn next_block(
    events: &mut ActionEvents,
    observed_height: &mut Option<u64>,
) -> Result<(), String> {
    while let Some(frame) = events.next().await {
        let frame = frame.map_err(|error| format!("native boundary readback event: {error}"))?;
        let tokio_tungstenite::tungstenite::Message::Text(text) = frame else {
            continue;
        };
        let value: Value = serde_json::from_str(&text)
            .map_err(|error| format!("decode native boundary event: {error}"))?;
        match value.get("type").and_then(Value::as_str) {
            Some("heartbeat") => {}
            Some("error") => {
                return Err("native boundary readback event stream refused".into());
            }
            _ => continue,
        }
        let Some(height) = value.get("height").and_then(Value::as_u64) else {
            continue;
        };
        if *observed_height == Some(height) {
            continue;
        }
        *observed_height = Some(height);
        return Ok(());
    }
    Err("node disconnected before native boundary committed".into())
}
