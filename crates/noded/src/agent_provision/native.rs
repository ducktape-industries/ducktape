//! Native history has its own host-owned Files publication lane. The child can
//! propose JSONL bytes, never a path, snapshot, signing key, or revision.
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::runs::{
    ConversationHistory, ConversationStatus, ConversationTurnPhase, ConversationView,
};
use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Response, StatusCode};
use commonware_cryptography::{Signer as _, ed25519};
use compute_service::{WorkspaceSource, WorkspaceSpec};
use duckfs_client::api::NodeApi;
use duckfs_client::checkout::checkout;
use duckfs_client::commit::{CommitError, commit};
use futures::StreamExt as _;
use provider_host::{NativeConversationContext, NativeConversationEvent, NativePackage};
use serde::Deserialize;
use serde_json::Value;

use super::{
    ACTION_HEADER, ActionEvents, ActionState, RUNS_MODULE, action_events, action_json,
    action_response,
};
use crate::node_link::NodeLink;

#[path = "native_controls.rs"]
mod controls;

#[path = "native_projection.rs"]
mod projection;
#[path = "native_reports.rs"]
mod reports;

pub(super) struct NativeState {
    pub(super) context: NativeConversationContext,
    configuration: ConversationView,
    attempt: u32,
    private: Arc<PrivateDirectory>,
    checkpoint: tokio::sync::Mutex<HistoryReceipts>,
}

/// A fresh directory outside the child's mounted workspace. Its lifetime covers
/// blocking I/O too: cancelling a request cannot remove another task's checkout.
struct PrivateDirectory(PathBuf);

impl PrivateDirectory {
    fn create(parent: &Path) -> Result<Self, String> {
        let mut random = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut random);
        let path = parent.join(format!(".ducktape-native-{}", duckfs_core::to_hex(&random)));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|error| format!("create private native staging: {error}"))?;
        Ok(Self(path))
    }
}

impl Drop for PrivateDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) async fn prepare(
    node: &NodeLink,
    spec: &WorkspaceSpec,
    signer: &ed25519::PrivateKey,
    workdir: &Path,
) -> Result<Option<NativeState>, String> {
    let Some(agent) = &spec.agent else {
        return Ok(None);
    };
    let Some(descriptor) = &agent.native_conversation else {
        return Ok(None);
    };
    descriptor.validate()?;
    let view = conversation(node, &descriptor.conversation_id).await?;
    let packages_match = view.packages.len() == descriptor.packages.len()
        && view
            .packages
            .iter()
            .zip(&descriptor.packages)
            .all(|(view, descriptor)| {
                view.name == descriptor.name
                    && view.source_prefix == descriptor.source_prefix
                    && view.source_snapshot == descriptor.source_snapshot
            });
    let same_configuration = view.conversation_id == descriptor.conversation_id
        && view.agent_id == agent.agent_id
        && view.active_turn.as_ref().is_some_and(|turn| {
            descriptor.turn_id
                == crate::runs::conversation_turn_id(turn.from_cursor, turn.through_cursor)
        })
        && view.history_prefix == descriptor.history_prefix
        && view.session_path == descriptor.session_path
        && packages_match;
    if !same_configuration {
        return Err("native conversation configuration mismatch".into());
    }
    validate_configuration(&view, spec)?;
    validate_active(&view, &view, &agent.run_id)?;
    let events = frozen_events(node, &view).await?;
    if events != descriptor.events {
        return Err("native conversation events differ from the committed active turn".into());
    }
    require_lease(node, signer, &agent.run_id, agent.attempt).await?;
    let revision = latest_history(&view).map_or(0, |history| history.revision);
    if descriptor.revision > revision {
        return Err("native conversation revision is ahead of committed history".into());
    }
    let node_copy = node.clone();
    let view_copy = view.clone();
    let workdir = workdir.to_path_buf();
    let (private, mut context, receipts) =
        tokio::task::spawn_blocking(move || materialize(&node_copy.files(), &view_copy, &workdir))
            .await
            .map_err(|_| "native materialization task panicked".to_string())??;
    // Checkout can span blocks. Never start a child after the lease, active turn,
    // or authoritative history moved while we were hydrating its artifacts.
    let current = conversation(node, &view.conversation_id).await?;
    validate_active(&current, &view, &agent.run_id)?;
    require_lease(node, signer, &agent.run_id, agent.attempt).await?;
    if latest_history(&current) != latest_history(&view) {
        return Err("native history advanced during materialization".into());
    }
    context.events = events;
    Ok(Some(NativeState {
        context,
        configuration: view,
        attempt: agent.attempt,
        private: Arc::new(private),
        checkpoint: tokio::sync::Mutex::new(receipts),
    }))
}

fn prefixes_overlap(left: &str, right: &str) -> bool {
    let left = left.trim_end_matches('/');
    let right = right.trim_end_matches('/');
    left == right
        || left.starts_with(&format!("{right}/"))
        || right.starts_with(&format!("{left}/"))
}

fn validate_configuration(view: &ConversationView, spec: &WorkspaceSpec) -> Result<(), String> {
    let reserved_component = view.session_path.split('/').any(|part| {
        [
            ".duckfs",
            ".git",
            "auth.json",
            "settings.json",
            "models.json",
        ]
        .iter()
        .any(|reserved| part.eq_ignore_ascii_case(reserved))
    });
    if reserved_component {
        return Err("native session path names provider configuration".into());
    }
    let workspace_overlap = match &spec.source {
        WorkspaceSource::Duckfs { source_prefix, .. } => {
            prefixes_overlap(&view.history_prefix, source_prefix)
        }
        WorkspaceSource::Forge { .. } => false,
    };
    let package_overlap = view
        .packages
        .iter()
        .any(|package| prefixes_overlap(&view.history_prefix, &package.source_prefix));
    if workspace_overlap || package_overlap {
        return Err("native history must have a dedicated Files subtree".into());
    }
    Ok(())
}

fn latest_history(view: &ConversationView) -> Option<&ConversationHistory> {
    view.active_turn
        .as_ref()
        .and_then(|turn| turn.checkpoint.as_ref())
        .map(|checkpoint| &checkpoint.history)
        .or(view.history.as_ref())
}

fn validate_active(
    view: &ConversationView,
    expected: &ConversationView,
    run_id: &str,
) -> Result<(), String> {
    let same_configuration = view.conversation_id == expected.conversation_id
        && view.agent_id == expected.agent_id
        && view.account == expected.account
        && view.history_prefix == expected.history_prefix
        && view.session_path == expected.session_path
        && view.packages == expected.packages;
    if !same_configuration {
        return Err("native conversation configuration changed".into());
    }
    if view.status != ConversationStatus::Active {
        return Err("native conversation is not active".into());
    }
    let Some(turn) = &view.active_turn else {
        return Err("native conversation has no active turn".into());
    };
    let same_range = expected.active_turn.as_ref().is_some_and(|expected| {
        turn.from_cursor == expected.from_cursor && turn.through_cursor == expected.through_cursor
    });
    let current =
        turn.run_id == run_id && turn.phase == ConversationTurnPhase::Running && same_range;
    if !current {
        return Err("native conversation turn is no longer running".into());
    }
    Ok(())
}

async fn conversation(node: &NodeLink, id: &str) -> Result<ConversationView, String> {
    let bytes = node
        .query(
            RUNS_MODULE,
            &crate::runs::encode_query(&crate::runs::RunsQuery::Conversation {
                conversation_id: id.into(),
            }),
        )
        .await?;
    let crate::runs::RunsReply::Conversation(Some(view)) = crate::runs::decode_reply(&bytes)?
    else {
        return Err("native conversation not found in committed Runs state".into());
    };
    Ok(view)
}

async fn frozen_events(
    node: &NodeLink,
    view: &ConversationView,
) -> Result<Vec<NativeConversationEvent>, String> {
    let Some(turn) = &view.active_turn else {
        return Err("native conversation has no active turn".into());
    };
    let mut cursor = turn.from_cursor;
    let mut result = Vec::new();
    while cursor < turn.through_cursor {
        let from = cursor
            .checked_add(1)
            .ok_or_else(|| "native event cursor exhausted".to_string())?;
        let limit = (turn.through_cursor - cursor).min(64);
        let bytes = node
            .query(
                RUNS_MODULE,
                &crate::runs::encode_query(&crate::runs::RunsQuery::ConversationEvents {
                    conversation_id: view.conversation_id.clone(),
                    from,
                    limit,
                }),
            )
            .await?;
        let crate::runs::RunsReply::ConversationEvents(events) = crate::runs::decode_reply(&bytes)?
        else {
            return Err("unexpected native conversation events reply".into());
        };
        let contiguous = events.len() as u64 == limit
            && events
                .iter()
                .enumerate()
                .all(|(index, event)| event.sequence == from + index as u64);
        if !contiguous {
            return Err("native conversation frozen events are missing or reordered".into());
        }
        let value = serde_json::to_value(events)
            .map_err(|error| format!("project native events: {error}"))?;
        let events: Vec<NativeConversationEvent> = serde_json::from_value(value)
            .map_err(|error| format!("project native event schema: {error}"))?;
        result.extend(events);
        cursor += limit;
    }
    Ok(result)
}

/// AgentSessions alone is insufficient: the saga can move the attempt before
/// a replacement child opens a session. Mirror the committed execution proof.
async fn require_lease(
    node: &NodeLink,
    signer: &ed25519::PrivateKey,
    run_id: &str,
    attempt: u32,
) -> Result<(), String> {
    let bytes = node
        .query(
            RUNS_MODULE,
            &crate::runs::encode_query(&crate::runs::RunsQuery::AgentSessions),
        )
        .await?;
    let crate::runs::RunsReply::AgentSessions(sessions) = crate::runs::decode_reply(&bytes)? else {
        return Err("unexpected native session reply".into());
    };
    let Some(session) = sessions.iter().find(|session| session.run_id == run_id) else {
        return Err("native conversation session is no longer bound".into());
    };
    let bound =
        session.lease.attempt == attempt && session.session_key == signer.public_key().as_ref();
    if !bound {
        return Err("native conversation session attempt is stale".into());
    }
    let bytes = node
        .query(
            "dispatch",
            &dispatch::encode_query(&dispatch::DispatchQuery::Dispatch {
                receiver: RUNS_MODULE.into(),
                dispatch_id: crate::runs::dispatch_id_for(run_id),
            }),
        )
        .await?;
    let dispatch::DispatchReply::Dispatch(Some(dispatch)) = dispatch::decode_reply(&bytes)? else {
        return Err("native conversation has no dispatch".into());
    };
    let dispatch::DispatchStatus::AwaitingResult { saga_id } = dispatch.status else {
        return Err("native conversation has no execution lease".into());
    };
    let bytes = node
        .query(
            "saga",
            &saga::encode_query(&saga::SagaQuery::Get { saga_id }),
        )
        .await?;
    let saga::SagaReply::Saga(Some(saga)) = saga::decode_reply(&bytes)? else {
        return Err("native conversation has no saga".into());
    };
    let current = !saga.status.is_terminal()
        && saga.attempt == attempt
        && saga.assignee.as_ref() == Some(&session.lease.holder);
    if !current {
        return Err("native conversation execution lease moved".into());
    }
    Ok(())
}

fn materialize(
    api: &dyn NodeApi,
    view: &ConversationView,
    workdir: &Path,
) -> Result<(PrivateDirectory, NativeConversationContext, HistoryReceipts), String> {
    let parent = workdir
        .parent()
        .ok_or_else(|| "native workspace has no staging parent".to_string())?;
    let private = PrivateDirectory::create(parent)?;
    let runtime = workdir.join(provider_host::RUN_RUNTIME_DIR);
    // Source artifacts cannot preinstall a symlink or a config in the reserved
    // runtime root; start with a fresh host-owned directory before any child.
    match std::fs::symlink_metadata(&runtime) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            std::fs::remove_dir_all(&runtime)
        }
        Ok(_) => std::fs::remove_file(&runtime),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
    .map_err(|error| format!("clear native runtime root: {error}"))?;
    let relative_root = Path::new(provider_host::RUN_RUNTIME_DIR).join("native");
    let session_path = relative_root.join("history").join(&view.session_path);
    let destination = workdir.join(&session_path);
    std::fs::create_dir_all(destination.parent().expect("native session has a parent"))
        .map_err(|error| format!("create native history directory: {error}"))?;
    let receipts = if let Some(history) = latest_history(view) {
        let restored = private.0.join("restore");
        checkout(
            api,
            &restored,
            &view.history_prefix,
            Some(&history.snapshot),
        )
        .map_err(|error| format!("restore committed native history: {error}"))?;
        validate_history_tree(&restored, &view.session_path)?;
        let jsonl = std::fs::read_to_string(restored.join(&view.session_path))
            .map_err(|error| format!("read committed native session: {error}"))?;
        let receipts = validate_jsonl(&jsonl, &[], None)?;
        std::fs::write(&destination, jsonl)
            .map_err(|error| format!("materialize native session: {error}"))?;
        receipts
    } else {
        HistoryReceipts::default()
    };
    let mut packages = Vec::new();
    for package in &view.packages {
        let path = relative_root.join("packages").join(&package.name);
        let destination = workdir.join(&path);
        // Separate pinned packages must not alias on a case-insensitive host.
        // The runtime root is fresh, so an existing target belongs to another
        // package, never to a resumable checkout of this one.
        let already_materialized = destination
            .try_exists()
            .map_err(|error| format!("inspect native package destination: {error}"))?;
        if already_materialized {
            return Err("native package destinations collide".into());
        }
        checkout(
            api,
            &destination,
            &package.source_prefix,
            Some(&package.source_snapshot),
        )
        .map_err(|error| format!("materialize pinned native package: {error}"))?;
        std::fs::remove_dir_all(destination.join(".duckfs"))
            .map_err(|error| format!("remove native package checkout metadata: {error}"))?;
        packages.push(NativePackage {
            name: package.name.clone(),
            path,
        });
    }
    let turn = view
        .active_turn
        .as_ref()
        .ok_or_else(|| "native conversation has no turn".to_string())?;
    Ok((
        private,
        NativeConversationContext {
            conversation_id: view.conversation_id.clone(),
            turn_id: crate::runs::conversation_turn_id(turn.from_cursor, turn.through_cursor),
            revision: latest_history(view).map_or(0, |history| history.revision),
            session_path,
            packages,
            events: Vec::new(),
            job_reporting: matches!(view.source, crate::runs::ConversationSource::Job { .. }),
            system_prompt: String::new(),
        },
        receipts,
    ))
}

/// A history subtree contains exactly the configured session and its ancestor
/// directories, never provider homes, config, symlinks, or arbitrary files.
fn validate_history_tree(root: &Path, session: &str) -> Result<(), String> {
    let session = Path::new(session);
    if duckfs_client::index::Index::path(root).exists() {
        let index = duckfs_client::index::Index::load(root)
            .map_err(|error| format!("read private native checkout index: {error}"))?;
        for path in index.entries.keys() {
            let relative = path
                .strip_prefix(&format!("{}/", index.prefix))
                .ok_or_else(|| "native checkout index escaped history subtree".to_string())?;
            let allowed =
                session == Path::new(relative) || session.starts_with(Path::new(relative));
            if !allowed {
                return Err("native history snapshot contains a non-session artifact".into());
            }
        }
    }
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory)
            .map_err(|error| format!("inspect native history: {error}"))?
        {
            let entry = entry.map_err(|error| format!("inspect native history entry: {error}"))?;
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|_| "native history path escaped".to_string())?;
            if relative == Path::new(".duckfs") {
                continue;
            }
            let kind = entry
                .file_type()
                .map_err(|error| format!("inspect native history kind: {error}"))?;
            let allowed_file = kind.is_file() && relative == session;
            let allowed_directory =
                kind.is_dir() && relative != session && session.starts_with(relative);
            if allowed_file {
                continue;
            }
            if !allowed_directory {
                return Err("native history subtree contains a non-session artifact".into());
            }
            pending.push(path);
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Request {
    Checkpoint {
        jsonl: String,
        delivery: bool,
        complete: bool,
        delivered_control_ids: Vec<String>,
    },
    Control {},
    Report {
        operation_id: String,
        report_kind: crate::tasks::WorkerReportKind,
        payload: String,
    },
}

pub(super) async fn route(
    State(state): State<Arc<ActionState>>,
    headers: HeaderMap,
    Json(request): Json<Request>,
) -> Response<Body> {
    let authorized = headers
        .get(ACTION_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|token| crate::services::token_matches(token, &state.token));
    if !authorized {
        return action_response(StatusCode::UNAUTHORIZED, "action token rejected");
    }
    let Some(native) = &state.native else {
        return action_response(StatusCode::FORBIDDEN, "run has no native conversation");
    };
    match dispatch_request(&state, native, request).await {
        Ok(value) => action_json(StatusCode::OK, value),
        Err(error) => action_response(StatusCode::BAD_REQUEST, &error),
    }
}

async fn dispatch_request(
    state: &ActionState,
    native: &NativeState,
    request: Request,
) -> Result<Value, String> {
    match request {
        Request::Checkpoint {
            jsonl,
            delivery,
            complete,
            delivered_control_ids,
        } => {
            checkpoint(
                state,
                native,
                jsonl,
                delivery,
                complete,
                delivered_control_ids,
            )
            .await
        }
        Request::Control {} => control(state, native).await,
        Request::Report {
            operation_id,
            report_kind,
            payload,
        } => reports::report(state, native, operation_id, report_kind, payload).await,
    }
}

async fn control(state: &ActionState, native: &NativeState) -> Result<Value, String> {
    let view = conversation(&state.node, &native.context.conversation_id).await?;
    let current_turn = view.active_turn.as_ref().is_some_and(|turn| {
        turn.run_id == state.run_id && turn.phase == ConversationTurnPhase::Running
    });
    if !current_turn {
        return Ok(serde_json::json!({"control": "abort", "messages": []}));
    }
    require_lease(&state.node, &state.signer, &state.run_id, native.attempt).await?;
    let control = match view.status {
        ConversationStatus::Active => "continue",
        ConversationStatus::Paused { .. } => "pause",
        ConversationStatus::Inactive => "abort",
    };
    if control != "continue" {
        return Ok(serde_json::json!({"control": control, "messages": []}));
    }
    let approved = native.checkpoint.lock().await;
    controls::poll(state, native, &approved).await
}

fn digest(bytes: &[u8]) -> String {
    duckfs_core::to_hex(&duckfs_core::objects::object_id(
        duckfs_core::Kind::Chunk,
        bytes,
    ))
}

fn operation_id(native: &NativeState, jsonl: &str, delivery: bool, complete: bool) -> String {
    let run_id = &native
        .configuration
        .active_turn
        .as_ref()
        .expect("native state binds an active turn")
        .run_id;
    let coordinates = serde_json::to_vec(&(
        &native.context.conversation_id,
        &native.context.turn_id,
        run_id,
        native.attempt,
        digest(jsonl.as_bytes()),
        delivery,
        complete,
    ))
    .expect("native checkpoint coordinates serialize");
    // The complete digest fits Runs' 64-byte operation ID budget; a display
    // prefix would make every otherwise-valid checkpoint unpublishable.
    digest(&coordinates)
}

async fn checkpoint(
    state: &ActionState,
    native: &NativeState,
    jsonl: String,
    delivery: bool,
    complete: bool,
    delivered_control_ids: Vec<String>,
) -> Result<Value, String> {
    let mut secrets = vec![state.token.clone()];
    if let Some(token) = state.node.operator_token() {
        secrets.push(token);
    }
    let receipt = delivery.then_some((
        &native.context.conversation_id[..],
        &native.context.turn_id[..],
        native.context.events.as_slice(),
    ));
    let receipts = validate_jsonl(&jsonl, &secrets, receipt)?;
    controls::validate_claims(&delivered_control_ids, &receipts)?;
    let operation_id = operation_id(native, &jsonl, delivery, complete);
    // Separate from the signer sequence lock: a long-running agent action must
    // not allow two history CAS operations to race through artifact staging.
    let mut approved = native.checkpoint.lock().await;
    let view = conversation(&state.node, &native.context.conversation_id).await?;
    validate_active(&view, &native.configuration, &state.run_id)?;
    require_lease(&state.node, &state.signer, &state.run_id, native.attempt).await?;
    let worker = controls::worker_controls(state).await?;
    let acknowledgements = controls::plan(&receipts, &approved, worker.as_ref())?;
    let cancellation =
        controls::cancellation_plan(&receipts, &approved, worker.as_ref(), &native.context)?;
    let unfinished_cancel = cancellation.is_some() && !complete;
    if unfinished_cancel {
        return Err("native cancellation requires a final checkpoint".into());
    }
    let prior = view
        .active_turn
        .as_ref()
        .and_then(|turn| turn.checkpoint.as_ref());
    if let Some(committed) = prior.filter(|checkpoint| {
        checkpoint.operation_id == operation_id
            && checkpoint.run_id == state.run_id
            && checkpoint.attempt == native.attempt
            && (!delivery || checkpoint.delivery)
    }) {
        *approved = receipts;
        controls::complete_boundary(state, native, &acknowledgements, cancellation.as_ref())
            .await?;
        return Ok(serde_json::json!({"revision": committed.history.revision}));
    }
    let revision = latest_history(&view)
        .map_or(0, |history| history.revision)
        .checked_add(1)
        .ok_or_else(|| "native history revision exhausted".to_string())?;
    let node = state.node.clone();
    let private = native.private.clone();
    let config = native.configuration.clone();
    let checkpoint_id = operation_id.clone();
    let previous = latest_history(&view).cloned();
    let projection_jsonl = jsonl.clone();
    let candidate = tokio::task::spawn_blocking(move || {
        persist(
            &node.files(),
            &private.0,
            &config,
            previous.as_ref(),
            &jsonl,
            &checkpoint_id,
        )
    })
    .await
    .map_err(|_| "native checkpoint task panicked".to_string())??;
    let snapshot = projection::publish(
        &state.node,
        &candidate,
        &native.configuration,
        &projection_jsonl,
    )
    .await?;
    let history = ConversationHistory { revision, snapshot };
    let mut seq = state.seq.lock().await;
    let events = action_events(&state.node).await?;
    // Staging an orphan artifact confers no authority. Recheck after Files I/O,
    // and Runs independently checks the signing session's current saga lease.
    let current = conversation(&state.node, &native.context.conversation_id).await?;
    validate_active(&current, &native.configuration, &state.run_id)?;
    require_lease(&state.node, &state.signer, &state.run_id, native.attempt).await?;
    if latest_history(&current) != latest_history(&view) {
        return Err("native history checkpoint lost its revision fence".into());
    }
    let message = crate::runs::RunsMsg::CheckpointConversation {
        conversation_id: native.context.conversation_id.clone(),
        run_id: state.run_id.clone(),
        attempt: native.attempt,
        operation_id: operation_id.clone(),
        history: history.clone(),
        delivery,
    };
    let message = sdk::Msg {
        target: RUNS_MODULE.into(),
        payload: crate::runs::encode_msg(&message),
    };
    let frame = node::encode_frame(&state.signer, *seq, &message);
    *seq = seq
        .checked_add(1)
        .ok_or_else(|| "action signer sequence exhausted".to_string())?;
    state.node.submit_frame(frame).await?;
    await_checkpoint(state, native, &operation_id, &history, delivery, events).await?;
    drop(seq);
    *approved = receipts;
    controls::complete_boundary(state, native, &acknowledgements, cancellation.as_ref()).await?;
    // `complete` forces this final publication but never releases the queue;
    // only the normal run-result/action-drain state machine does that.
    Ok(serde_json::json!({"revision": revision}))
}

fn persist(
    api: &dyn NodeApi,
    parent: &Path,
    config: &ConversationView,
    previous: Option<&ConversationHistory>,
    jsonl: &str,
    operation_id: &str,
) -> Result<String, String> {
    if let Some(previous) = previous {
        let restored = PrivateDirectory::create(parent)?;
        checkout(
            api,
            &restored.0,
            &config.history_prefix,
            Some(&previous.snapshot),
        )
        .map_err(|error| format!("verify prior native checkpoint: {error}"))?;
        validate_history_tree(&restored.0, &config.session_path)?;
        let prior = std::fs::read_to_string(restored.0.join(&config.session_path))
            .map_err(|error| format!("read prior native checkpoint: {error}"))?;
        if !jsonl.starts_with(&prior) {
            return Err("native checkpoint would discard or rewrite committed history".into());
        }
    }
    let stage = PrivateDirectory::create(parent)?;
    // HEAD is only the CAS base for Files, never the restore authority. A prior
    // attempt may have left an orphan here; the full accepted JSONL replaces it.
    let index = checkout(api, &stage.0, &config.history_prefix, None)
        .map_err(|error| format!("checkout private native staging: {error}"))?;
    validate_history_tree(&stage.0, &config.session_path)?;
    let session = stage.0.join(&config.session_path);
    std::fs::create_dir_all(session.parent().expect("native session has parent"))
        .map_err(|error| format!("create private native session parent: {error}"))?;
    std::fs::write(&session, jsonl)
        .map_err(|error| format!("write private native session: {error}"))?;
    std::fs::set_permissions(&session, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("set private native session permissions: {error}"))?;
    let snapshot = match commit(
        api,
        &stage.0,
        &format!("native conversation history {operation_id}"),
    ) {
        Ok(summary) => summary.snapshot,
        Err(CommitError::Nothing) => index
            .base_snapshot
            .ok_or_else(|| "native checkpoint has no Files snapshot".to_string())?,
        Err(error) => return Err(format!("commit native history: {error}")),
    };
    Ok(snapshot)
}

async fn checkpoint_result(
    state: &ActionState,
    native: &NativeState,
    operation_id: &str,
    history: &ConversationHistory,
    delivery: bool,
) -> Result<bool, String> {
    let view = conversation(&state.node, &native.context.conversation_id).await?;
    validate_active(&view, &native.configuration, &state.run_id)?;
    require_lease(&state.node, &state.signer, &state.run_id, native.attempt).await?;
    let checkpoint = view
        .active_turn
        .as_ref()
        .and_then(|turn| turn.checkpoint.as_ref());
    let committed = checkpoint.is_some_and(|checkpoint| {
        checkpoint.run_id == state.run_id
            && checkpoint.attempt == native.attempt
            && checkpoint.operation_id == operation_id
            && checkpoint.history == *history
            && (!delivery || checkpoint.delivery)
    });
    if committed {
        return Ok(true);
    }
    let superseded =
        latest_history(&view).is_some_and(|current| current.revision >= history.revision);
    if superseded {
        return Err("native checkpoint was superseded".into());
    }
    Ok(false)
}

async fn await_checkpoint(
    state: &ActionState,
    native: &NativeState,
    operation_id: &str,
    history: &ConversationHistory,
    delivery: bool,
    mut events: ActionEvents,
) -> Result<(), String> {
    if checkpoint_result(state, native, operation_id, history, delivery).await? {
        return Ok(());
    }
    let mut observed_height = None;
    while let Some(frame) = events.next().await {
        let frame = frame.map_err(|error| format!("native checkpoint event stream: {error}"))?;
        let tokio_tungstenite::tungstenite::Message::Text(text) = frame else {
            continue;
        };
        let value: Value = serde_json::from_str(&text)
            .map_err(|error| format!("decode native checkpoint event: {error}"))?;
        match value.get("type").and_then(Value::as_str) {
            Some("heartbeat") => {}
            Some("error") => return Err("native checkpoint event stream refused".into()),
            _ => continue,
        }
        let Some(height) = value.get("height").and_then(Value::as_u64) else {
            continue;
        };
        if observed_height == Some(height) {
            continue;
        }
        observed_height = Some(height);
        if checkpoint_result(state, native, operation_id, history, delivery).await? {
            return Ok(());
        }
    }
    Err("node disconnected before native checkpoint committed".into())
}

/// Validate native entry structure, not model content: opaque signatures and
/// tool payloads are retained verbatim. Delivery requires this turn's durable
/// marker to reference an actual earlier user entry, never just any old user.
fn validate_jsonl(
    jsonl: &str,
    secrets: &[String],
    delivery: Option<(&str, &str, &[NativeConversationEvent])>,
) -> Result<HistoryReceipts, String> {
    let complete = !jsonl.is_empty() && jsonl.ends_with('\n');
    if !complete {
        return Err("native history must be complete JSONL".into());
    }
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        let escaped = serde_json::to_string(secret).expect("secret serializes");
        let leaked =
            jsonl.contains(secret.as_str()) || jsonl.contains(&escaped[1..escaped.len() - 1]);
        if leaked {
            return Err("native history contains a host credential".into());
        }
    }
    let mut entries = jsonl.lines();
    let header: Value = serde_json::from_str(entries.next().expect("nonempty history"))
        .map_err(|_| "native history has invalid JSON".to_string())?;
    let valid_header = header.get("type").and_then(Value::as_str) == Some("session")
        && header.get("version").and_then(Value::as_u64) == Some(3)
        && header.get("timestamp").and_then(Value::as_str).is_some()
        && header.get("cwd").and_then(Value::as_str).is_some()
        && header
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty());
    if !valid_header {
        return Err("native history has no session header".into());
    }
    reject_decoded_secrets(&header, secrets)?;
    let mut ids = BTreeSet::new();
    let mut users = BTreeMap::new();
    let mut controls = BTreeMap::new();
    let mut cancellations = BTreeMap::new();
    let mut receipts = BTreeSet::new();
    let mut handled_receipts = BTreeMap::new();
    for line in entries {
        let entry: Value = serde_json::from_str(line)
            .map_err(|_| "native history has invalid JSON".to_string())?;
        reject_decoded_secrets(&entry, secrets)?;
        let Some(kind) = entry.get("type").and_then(Value::as_str) else {
            return Err("native history has an untyped entry".into());
        };
        let native_kind = matches!(
            kind,
            "message"
                | "thinking_level_change"
                | "model_change"
                | "compaction"
                | "branch_summary"
                | "custom"
                | "custom_message"
                | "label"
                | "session_info"
        );
        if !native_kind {
            return Err("native history contains a non-native entry".into());
        }
        validate_entry_payload(&entry, kind)?;
        let Some(id) = entry
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return Err("native history entry has no id".into());
        };
        let valid_parent = match entry.get("parentId") {
            Some(Value::Null) => true,
            Some(Value::String(parent)) => ids.contains(parent),
            _ => false,
        };
        if !valid_parent || !ids.insert(id.to_string()) {
            return Err("native history has an invalid entry tree".into());
        }
        if kind == "message" {
            let message = &entry["message"];
            let Some(role) = message.get("role").and_then(Value::as_str) else {
                return Err("native history has an invalid message".into());
            };
            let native_role = matches!(
                role,
                "user"
                    | "assistant"
                    | "toolResult"
                    | "bashExecution"
                    | "custom"
                    | "branchSummary"
                    | "compactionSummary"
            );
            if !native_role {
                return Err("native history has a non-native message role".into());
            }
            if role == "user" {
                users.insert(id.to_string(), message.clone());
            }
        }
        let turn_receipt = kind == "custom"
            && entry.get("customType").and_then(Value::as_str) == Some("ducktape.turn_delivery");
        if turn_receipt {
            let data = &entry["data"];
            let identity = (
                data.get("conversation_id").and_then(Value::as_str),
                data.get("turn_id").and_then(Value::as_str),
            );
            let (Some(conversation), Some(turn)) = identity else {
                return Err("native delivery receipt has no identity".into());
            };
            let user_exists = data
                .get("message_id")
                .and_then(Value::as_str)
                .is_some_and(|id| users.contains_key(id));
            if !user_exists {
                return Err("native delivery receipt has no committed user entry".into());
            }
            receipts.insert((conversation.to_string(), turn.to_string()));
        }
        let control_receipt = kind == "custom"
            && entry.get("customType").and_then(Value::as_str) == Some("ducktape.control_delivery");
        if control_receipt {
            let receipt = controls::receipt(&entry["data"], &users)?;
            if controls.insert(receipt.0, receipt.1).is_some() {
                return Err("native history repeats a control delivery receipt".into());
            }
        }
        let cancellation_receipt = kind == "custom"
            && entry.get("customType").and_then(Value::as_str) == Some("ducktape.cancel_delivery");
        if cancellation_receipt {
            let receipt = controls::cancellation_receipt(&entry["data"])?;
            if cancellations.insert(receipt.0, receipt.1).is_some() {
                return Err("native history repeats a cancellation receipt".into());
            }
        }
        let handled_receipt = kind == "custom"
            && entry.get("customType").and_then(Value::as_str) == Some("ducktape.input_handled");
        if handled_receipt {
            let data = &entry["data"];
            let identity = (
                data.get("conversation_id").and_then(Value::as_str),
                data.get("turn_id").and_then(Value::as_str),
            );
            let (Some(conversation), Some(turn)) = identity else {
                return Err("native handled receipt has no identity".into());
            };
            let events: Vec<HandledEvent> = serde_json::from_value(data["events"].clone())
                .map_err(|_| "native handled receipt has invalid event identities".to_string())?;
            handled_receipts.insert((conversation.to_string(), turn.to_string()), events);
        }
    }
    if let Some((conversation, turn, events)) = delivery {
        let identity = (conversation.into(), turn.into());
        let user_delivered = receipts.contains(&identity);
        let expected: Vec<HandledEvent> = events
            .iter()
            .map(|event| HandledEvent {
                sequence: event.sequence,
                operation_id: event.operation_id.clone(),
            })
            .collect();
        let events_handled =
            !expected.is_empty() && handled_receipts.get(&identity) == Some(&expected);
        if !user_delivered && !events_handled {
            return Err("native history has no delivery receipt for this turn".into());
        }
    }
    Ok(HistoryReceipts {
        controls,
        cancellations,
    })
}

#[derive(Clone, Default)]
struct HistoryReceipts {
    controls: BTreeMap<String, controls::ControlReceipt>,
    cancellations: BTreeMap<String, controls::CancellationReceipt>,
}

fn validate_entry_payload(entry: &Value, kind: &str) -> Result<(), String> {
    let text = |key: &str| entry.get(key).and_then(Value::as_str).is_some();
    let content = |value: &Value| value.is_string() || value.is_array();
    let payload_valid = match kind {
        "message" => match entry["message"]["role"].as_str() {
            Some("user" | "assistant" | "toolResult") => content(&entry["message"]["content"]),
            Some("bashExecution" | "custom" | "branchSummary" | "compactionSummary") => true,
            _ => false,
        },
        "thinking_level_change" => text("thinkingLevel"),
        "model_change" => text("provider") && text("modelId"),
        "compaction" => {
            text("summary") && text("firstKeptEntryId") && entry["tokensBefore"].is_number()
        }
        "branch_summary" => text("fromId") && text("summary"),
        "custom" => text("customType"),
        "custom_message" => {
            text("customType") && content(&entry["content"]) && entry["display"].is_boolean()
        }
        "label" => text("targetId"),
        "session_info" => entry.get("name").is_none() || text("name"),
        _ => false,
    };
    let native_entry = text("timestamp") && payload_valid;
    if !native_entry {
        return Err("native history has a malformed entry payload".into());
    }
    Ok(())
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct HandledEvent {
    sequence: u64,
    operation_id: String,
}

fn reject_decoded_secrets(value: &Value, secrets: &[String]) -> Result<(), String> {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::String(text) => {
                let leaked = secrets
                    .iter()
                    .filter(|secret| !secret.is_empty())
                    .any(|secret| text.contains(secret));
                if leaked {
                    return Err("native history contains a host credential".into());
                }
            }
            Value::Array(items) => pending.extend(items),
            Value::Object(object) => {
                let leaked_key = object.keys().any(|key| {
                    secrets
                        .iter()
                        .filter(|secret| !secret.is_empty())
                        .any(|secret| key.contains(secret))
                });
                if leaked_key {
                    return Err("native history contains a host credential".into());
                }
                pending.extend(object.values());
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "native_tests.rs"]
mod tests;
