//! Semantic worker reports are Tasks provenance, never native machine history.
use super::*;

fn job_id(native: &NativeState) -> Result<&str, String> {
    let runs_wire::ConversationSource::Job { job_id } = &native.configuration.source else {
        return Err("native conversation is not a worker".into());
    };
    if !native.context.job_reporting {
        return Err("native job reporting is unavailable".into());
    }
    Ok(job_id)
}

async fn current_worker(
    state: &ActionState,
    native: &NativeState,
    expected: Option<&runs_wire::WorkerControls>,
) -> Result<runs_wire::WorkerControls, String> {
    let job_id = job_id(native)?;
    let view = conversation(&state.node, &native.context.conversation_id).await?;
    validate_active(&view, &native.configuration, &state.run_id)?;
    if view.source != native.configuration.source {
        return Err("native worker binding changed".into());
    }
    require_lease(&state.node, &state.signer, &state.run_id, native.attempt).await?;
    // Runs resolves this through the immutable WorkerExecutionBinding, including
    // retained admission identity after board pruning or public job-ID reuse.
    let worker = controls::worker_controls(state)
        .await?
        .ok_or_else(|| "native worker job is unavailable".to_string())?;
    let current = worker.job_id == job_id && worker.job_status == tasks::JobStatus::Processing;
    if !current {
        return Err("native worker job is no longer processing".into());
    }
    let same_claim = expected.is_none_or(|prior| {
        worker.job_id == prior.job_id && worker.job_attempt == prior.job_attempt
    });
    if !same_claim {
        return Err("native worker report claim changed".into());
    }
    Ok(worker)
}

fn matching_report(
    worker: &runs_wire::WorkerControls,
    operation_id: &str,
    kind: &tasks::WorkerReportKind,
    payload: &str,
) -> Result<Option<tasks::WorkerReport>, String> {
    let Some(report) = worker
        .reports
        .iter()
        .find(|report| report.operation_id == operation_id)
    else {
        return Ok(None);
    };
    let exact = report.worker == tasks::Party::Module(RUNS_MODULE.into())
        && report.attempt == worker.job_attempt
        && report.kind == *kind
        && report.payload == payload;
    if !exact {
        return Err("native worker report conflicts with immutable receipt".into());
    }
    Ok(Some(report.clone()))
}

pub(super) async fn report(
    state: &ActionState,
    native: &NativeState,
    operation_id: String,
    kind: tasks::WorkerReportKind,
    payload: String,
) -> Result<Value, String> {
    sdk::validate_id("operation_id", &operation_id, tasks::MAX_JOB_ID)
        .map_err(|error| format!("{error:?}"))?;
    let valid_payload =
        !payload.trim().is_empty() && payload.len() <= tasks::MAX_WORKER_TEXT_BYTES;
    if !valid_payload {
        return Err("worker report requires bounded nonempty text".into());
    }
    let mut secrets = vec![state.token.clone()];
    if let Some(token) = state.node.operator_token() {
        secrets.push(token);
    }
    reject_decoded_secrets(&Value::String(payload.clone()), &secrets)
        .map_err(|_| "worker report contains a host credential".to_string())?;
    if let Ok(value) = serde_json::from_str::<Value>(&payload) {
        reject_decoded_secrets(&value, &secrets)
            .map_err(|_| "worker report contains a host credential".to_string())?;
    }
    job_id(native)?;
    let mut seq = state.seq.lock().await;
    let mut events = action_events(&state.node).await?;
    let expected = current_worker(state, native, None).await?;
    matching_report(&expected, &operation_id, &kind, &payload)?;
    // Even duplicates cross Runs authorization: a public Tasks row alone does
    // not prove this execution still owns the claim or program generation.
    // Runs' durable operation receipt suppresses duplicate Tasks emits/charges.
    let message = sdk::Msg {
        target: RUNS_MODULE.into(),
        payload: runs_wire::encode_msg(&runs_wire::RunsMsg::ReportJob {
            run_id: state.run_id.clone(),
            attempt: native.attempt,
            operation_id: operation_id.clone(),
            kind: kind.clone(),
            payload: payload.clone(),
        }),
    };
    let frame = node::encode_frame(&state.signer, *seq, &message);
    *seq = seq
        .checked_add(1)
        .ok_or_else(|| "action signer sequence exhausted".to_string())?;
    // The daemon replies only after consensus Applied/Rejected, not enqueue.
    // A committed rejection or an uncertain transport failure is never an ACK.
    state.node.submit_frame(frame).await?;
    let mut observed_height = None;
    loop {
        let worker = current_worker(state, native, Some(&expected)).await?;
        if let Some(report) = matching_report(&worker, &operation_id, &kind, &payload)? {
            return Ok(serde_json::json!({"report":report}));
        }
        controls::next_block(&mut events, &mut observed_height).await?;
    }
}
