use super::*;
use serde_json::json;
use std::os::unix::fs::symlink;

// Exercise the production checkout/commit engines against the real Files
// module, using its existing deterministic NodeApi test adapter.
#[path = "../../../duckfs/client/tests/support/mod.rs"]
mod files_node;

#[path = "native_events_tests.rs"]
mod events;

#[path = "native_boundary_tests.rs"]
mod boundary;

const RUN_ID: &str = "chat\u{1f}room\u{1f}1\u{1f}resident";
const LOGICAL_TURN_ID: &str = "events/0/1";

pub(super) fn view() -> ConversationView {
    serde_json::from_value(json!({
        "conversation_id": "resident", "agent_id": "resident", "account": 7,
        "source": "detached", "history_prefix": "/resident/resident/history",
        "session_path": "sessions/resident.jsonl", "packages": [], "status": "active",
        "source_cursor": 1, "admitted_cursor": 1, "completed_cursor": 0, "next_turn": 2,
        "history": null,
        "active_turn": {"turn": 1, "run_id": RUN_ID, "from_cursor": 0, "through_cursor": 1,
            "phase": "running", "checkpoint": null, "actions": [], "drained_actions": 0, "outcome": null}
    })).unwrap()
}

pub(super) fn history() -> String {
    [
        json!({"type":"session", "version":3, "id":"native-session", "timestamp":"now", "cwd":"/workspace"}),
        json!({"type":"message", "id":"user-1", "parentId":null, "timestamp":"now",
            "message":{"role":"user", "content":[{"type":"text", "text":"hello"}], "timestamp":1}}),
        json!({"type":"custom", "id":"delivery-1", "parentId":"user-1", "timestamp":"now",
            "customType":"ducktape.turn_delivery", "data":{"conversation_id":"resident", "turn_id":LOGICAL_TURN_ID, "message_id":"user-1"}}),
    ].into_iter().map(|entry| format!("{entry}\n")).collect()
}

fn extended_history() -> String {
    format!(
        "{}{}\n",
        history(),
        json!({"type":"message", "id":"assistant-1", "parentId":"delivery-1", "timestamp":"now",
        "message":{"role":"assistant", "content":[{"type":"thinking", "thinking":"opaque", "thinkingSignature":"exact-signed-payload"},
            {"type":"toolCall", "id":"call-1", "name":"read", "arguments":{"path":"a"}}], "stopReason":"toolUse"}})
    )
}

fn native(parent: &Path) -> NativeState {
    NativeState {
        context: NativeConversationContext {
            conversation_id: "resident".into(),
            turn_id: LOGICAL_TURN_ID.into(),
            revision: 0,
            session_path: ".ducktape-run/native/history/sessions/resident.jsonl".into(),
            packages: vec![],
            events: vec![],
            job_reporting: false,
            system_prompt: String::new(),
        },
        configuration: view(),
        attempt: 1,
        private: Arc::new(PrivateDirectory::create(parent).unwrap()),
        checkpoint: tokio::sync::Mutex::new(HistoryReceipts::default()),
    }
}

fn worker_controls_fixture(job_id: &str, operation_id: &str, text: &str) -> runs::WorkerControls {
    serde_json::from_value(json!({"job_id":job_id, "job_attempt":1, "job_status":"processing", "result":null, "reports":[], "controls":[{
        "operation_id":operation_id, "input":{"steer":{"text":text}}, "author":{"module":"tasks"}, "height":1, "acknowledgements":[]
    }]})).unwrap()
}

fn control_history(job_id: &str, operation_id: &str, text: &str) -> String {
    let id = controls::qualified_id(job_id, operation_id);
    format!(
        "{}{}\n{}\n",
        history(),
        json!({"type":"message", "id":"control-user", "parentId":"delivery-1", "timestamp":"now",
            "message":{"role":"user", "content":[{"type":"text", "text":text}], "timestamp":2, "ducktape_control_id":id}}),
        json!({"type":"custom", "id":"control-receipt", "parentId":"control-user", "timestamp":"now",
            "customType":"ducktape.control_delivery", "data":{"id":id, "message_id":"control-user"}})
    )
}

fn cancellation_history(job_id: &str, operation_id: &str) -> String {
    let header = history().lines().next().unwrap().to_string();
    format!(
        "{header}\n{}\n",
        json!({"type":"custom", "id":"cancel-receipt", "parentId":null, "timestamp":"now",
        "customType":"ducktape.cancel_delivery", "data":{"conversation_id":"resident", "turn_id":LOGICAL_TURN_ID, "id":controls::qualified_id(job_id, operation_id)}})
    )
}

fn checkpoint_record(history: ConversationHistory) -> runs::ConversationCheckpoint {
    serde_json::from_value(json!({"run_id":RUN_ID, "attempt":1, "operation_id":"checkpoint-1", "history":history, "delivery":true})).unwrap()
}

#[test]
fn delivery_requires_this_turn_receipt_and_an_earlier_real_user_entry() {
    let jsonl = history();
    validate_jsonl(&jsonl, &[], Some(("resident", LOGICAL_TURN_ID, &[]))).unwrap();
    for (conversation, turn) in [("other", LOGICAL_TURN_ID), ("resident", "events/1/2")] {
        assert!(validate_jsonl(&jsonl, &[], Some((conversation, turn, &[]))).is_err());
    }
    for invalid in [
        jsonl.replace("\"message_id\":\"user-1\"", "\"message_id\":\"missing\""),
        jsonl.replace("\"role\":\"user\"", "\"role\":\"assistant\""),
        jsonl
            .lines()
            .take(2)
            .map(|line| format!("{line}\n"))
            .collect(),
    ] {
        assert!(
            validate_jsonl(&invalid, &[], Some(("resident", LOGICAL_TURN_ID, &[]))).is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn handled_delivery_requires_the_exact_ordered_frozen_event_identities_without_a_user() {
    let events: Vec<NativeConversationEvent> = serde_json::from_value(json!([
        {"sequence":4,"operation_id":"report-4","actor":{"module":"tasks"},"input":{"event":{"kind":"report","content":{}}},"admitted_at":10},
        {"sequence":5,"operation_id":"report-5","actor":{"module":"tasks"},"input":{"event":{"kind":"report","content":{}}},"admitted_at":11}
    ])).unwrap();
    let header = history().lines().next().unwrap().to_string();
    let make = |identities: Value| {
        format!(
            "{header}\n{}\n",
            json!({"type":"custom", "id":"handled-1", "parentId":null,
        "timestamp":"now", "customType":"ducktape.input_handled", "data":{"conversation_id":"resident", "turn_id":LOGICAL_TURN_ID, "events":identities}})
        )
    };
    let identities =
        json!([{"sequence":4,"operation_id":"report-4"},{"sequence":5,"operation_id":"report-5"}]);
    validate_jsonl(
        &make(identities.clone()),
        &[],
        Some(("resident", LOGICAL_TURN_ID, &events)),
    )
    .unwrap();
    assert!(
        validate_jsonl(
            &make(identities),
            &[],
            Some(("resident", "other-turn", &events))
        )
        .is_err()
    );
    for invalid in [
        json!([]),
        json!([{"sequence":4,"operation_id":"report-4"}]),
        json!([{"sequence":5,"operation_id":"report-5"},{"sequence":4,"operation_id":"report-4"}]),
        json!([{"sequence":4,"operation_id":"wrong"},{"sequence":5,"operation_id":"report-5"}]),
        json!(["report-4", "report-5"]),
    ] {
        assert!(
            validate_jsonl(
                &make(invalid),
                &[],
                Some(("resident", LOGICAL_TURN_ID, &events))
            )
            .is_err()
        );
    }
}

#[test]
fn native_tree_validation_rejects_config_truncation_duplicates_and_forward_parents() {
    let jsonl = history();
    for invalid in [
        "{\"apiKey\":\"not-history\"}\n".to_string(),
        jsonl.trim_end().to_string(),
        format!("{jsonl}\n"),
        jsonl.replace("\"type\":\"custom\"", "\"type\":\"config\""),
        jsonl.replace("\"id\":\"delivery-1\"", "\"id\":\"user-1\""),
        jsonl.replace("\"parentId\":\"user-1\"", "\"parentId\":\"future\""),
        jsonl.replace("\"role\":\"user\"", "\"role\":\"system\""),
    ] {
        assert!(validate_jsonl(&invalid, &[], None).is_err(), "{invalid}");
    }
    validate_jsonl(&extended_history(), &[], None).unwrap();
}

#[test]
fn host_credentials_cannot_hide_in_json_escaping() {
    let jsonl = history();
    let secret = "secret-token";
    for leaked in [
        jsonl.replace("hello", secret),
        jsonl.replace("hello", "\\u0073ecret-token"),
    ] {
        assert!(validate_jsonl(&leaked, &[secret.into()], None).is_err());
    }
    validate_jsonl(&jsonl, &[secret.into()], None).unwrap();
}

#[test]
fn checkpoint_ids_are_stable_and_bound_to_full_bytes_turn_attempt_and_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let mut native = native(dir.path());
    let id = operation_id(&native, &history(), true, false);
    assert_eq!(id.len(), 64);
    assert!(id.len() <= runs::MAX_REQUEST_ID_BYTES);
    assert_eq!(id, operation_id(&native, &history(), true, false));
    assert_ne!(id, operation_id(&native, &extended_history(), true, false));
    assert_ne!(id, operation_id(&native, &history(), false, false));
    assert_ne!(id, operation_id(&native, &history(), true, true));
    native.attempt += 1;
    assert_ne!(id, operation_id(&native, &history(), true, false));
    native.attempt -= 1;
    native.context.turn_id = "events/1/2".into();
    assert_ne!(id, operation_id(&native, &history(), true, false));
    native.context.turn_id = LOGICAL_TURN_ID.into();
    native.configuration.active_turn.as_mut().unwrap().run_id =
        runs::run_id_for("room", 2, "resident");
    assert_ne!(
        id,
        operation_id(&native, &history(), true, false),
        "execution identity remains separate from logical turn identity"
    );
}

#[test]
fn history_tree_is_only_the_session_and_never_symlinks_or_config() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("sessions")).unwrap();
    std::fs::write(root.join("sessions/resident.jsonl"), history()).unwrap();
    validate_history_tree(root, "sessions/resident.jsonl").unwrap();
    std::fs::write(root.join("auth.json"), "{}").unwrap();
    assert!(validate_history_tree(root, "sessions/resident.jsonl").is_err());
    std::fs::remove_file(root.join("auth.json")).unwrap();
    std::fs::remove_file(root.join("sessions/resident.jsonl")).unwrap();
    symlink("../outside.jsonl", root.join("sessions/resident.jsonl")).unwrap();
    assert!(validate_history_tree(root, "sessions/resident.jsonl").is_err());
}

#[test]
fn private_candidate_round_trips_full_jsonl_without_pinning_the_ordinary_workspace() {
    let api = files_node::ModuleNode::new();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    checkout(&api, &workspace, "/ordinary", None).unwrap();
    std::fs::write(workspace.join("user.txt"), "ordinary output").unwrap();
    let ordinary = commit(&api, &workspace, "ordinary").unwrap();
    let private = PrivateDirectory::create(root.path()).unwrap();
    let full = extended_history();
    let snapshot = persist(&api, &private.0, &view(), None, &full, "operation").unwrap();
    let path = "/resident/resident/history/sessions/resident.jsonl";
    assert_eq!(
        api.read(path, Some(&snapshot), 0, full.len() as u64)
            .unwrap()
            .0,
        full.as_bytes()
    );
    assert_eq!(
        api.stat("/ordinary/user.txt", Some(&snapshot)).unwrap(),
        api.stat("/ordinary/user.txt", Some(&ordinary.snapshot))
            .unwrap()
    );
    assert!(api.refs().unwrap().pins.is_empty());
    assert_eq!(
        private.0.metadata().unwrap().permissions().mode() & 0o777,
        0o700
    );
    let prior = ConversationHistory {
        revision: 1,
        snapshot: snapshot.clone(),
    };
    assert_eq!(
        persist(
            &api,
            &private.0,
            &view(),
            Some(&prior),
            &full,
            "different-final-boundary"
        )
        .unwrap(),
        snapshot
    );
    assert!(
        api.refs().unwrap().pins.is_empty(),
        "checkpoint candidates never use ordinary pins"
    );
    let calls = api.commit_calls.get();
    assert!(
        persist(
            &api,
            &private.0,
            &view(),
            Some(&prior),
            &history(),
            "rollback"
        )
        .is_err()
    );
    assert_eq!(
        api.commit_calls.get(),
        calls,
        "a truncated checkpoint must not reach Files commit"
    );
}

#[test]
fn restore_uses_active_checkpoint_not_envelope_or_files_head_and_paths_are_relative() {
    let api = files_node::ModuleNode::new();
    let root = tempfile::tempdir().unwrap();
    let private = PrivateDirectory::create(root.path()).unwrap();
    let mut configuration = view();
    let first = persist(&api, &private.0, &configuration, None, &history(), "first").unwrap();
    let second = persist(
        &api,
        &private.0,
        &configuration,
        None,
        &extended_history(),
        "second",
    )
    .unwrap();
    // An orphan Files head from an unaccepted attempt is never restore authority.
    persist(&api, &private.0, &configuration, None, &history(), "orphan").unwrap();
    configuration.history = Some(ConversationHistory {
        revision: 1,
        snapshot: first,
    });
    configuration.active_turn.as_mut().unwrap().checkpoint =
        Some(checkpoint_record(ConversationHistory {
            revision: 2,
            snapshot: second,
        }));
    let workdir = root.path().join("workspace");
    std::fs::create_dir(&workdir).unwrap();
    let (staging, context, _) = materialize(&api, &configuration, &workdir).unwrap();
    assert_eq!(context.revision, 2);
    assert!(!context.session_path.is_absolute());
    assert_eq!(
        context.session_path,
        Path::new(".ducktape-run/native/history/sessions/resident.jsonl")
    );
    assert_eq!(
        std::fs::read_to_string(workdir.join(context.session_path)).unwrap(),
        extended_history()
    );
    assert!(!staging.0.starts_with(&workdir));
    assert!(
        context.system_prompt.is_empty(),
        "compute fills the bound stable system prefix"
    );
}

#[test]
fn retry_preserves_logical_delivery_while_the_opaque_execution_id_changes() {
    assert_eq!(RUN_ID, runs::run_id_for("room", 1, "resident"));
    assert!(RUN_ID.contains('\u{1f}'));
    let api = files_node::ModuleNode::new();
    let root = tempfile::tempdir().unwrap();
    let private = PrivateDirectory::create(root.path()).unwrap();
    let mut original = view();
    let snapshot = persist(&api, &private.0, &original, None, &history(), "delivered").unwrap();
    original.history = Some(ConversationHistory {
        revision: 1,
        snapshot,
    });
    let original_dir = root.path().join("original");
    std::fs::create_dir(&original_dir).unwrap();
    let (_, first, _) = materialize(&api, &original, &original_dir).unwrap();
    let mut retry = original.clone();
    let retry_run = runs::run_id_for("room", 2, "resident");
    retry.active_turn.as_mut().unwrap().run_id = retry_run.clone();
    retry.active_turn.as_mut().unwrap().turn = 2;
    retry.next_turn = 3;
    let retry_dir = root.path().join("retry");
    std::fs::create_dir(&retry_dir).unwrap();
    let (_, resumed, _) = materialize(&api, &retry, &retry_dir).unwrap();
    assert_eq!(first.turn_id, runs::conversation_turn_id(0, 1));
    assert_eq!(first.turn_id, resumed.turn_id);
    assert_ne!(resumed.turn_id, retry_run);
    let restored = std::fs::read_to_string(retry_dir.join(&resumed.session_path)).unwrap();
    validate_jsonl(
        &restored,
        &[],
        Some((&resumed.conversation_id, &resumed.turn_id, &[])),
    )
    .unwrap();
    validate_active(&retry, &retry, &retry_run).unwrap();
    assert!(
        validate_active(&retry, &retry, RUN_ID).is_err(),
        "logical delivery never authorizes the old execution"
    );
}

#[test]
fn semantic_job_reporting_is_derived_only_from_the_committed_job_source() {
    let api = files_node::ModuleNode::new();
    let root = tempfile::tempdir().unwrap();
    for (name, source, expected) in [
        (
            "worker",
            runs::ConversationSource::Job {
                job_id: "job-a".into(),
            },
            true,
        ),
        (
            "channel",
            runs::ConversationSource::Channel {
                channel_id: "room".into(),
            },
            false,
        ),
        ("detached", runs::ConversationSource::Detached, false),
    ] {
        let mut configuration = view();
        configuration.source = source;
        let workdir = root.path().join(name);
        std::fs::create_dir(&workdir).unwrap();
        let (_, context, _) = materialize(&api, &configuration, &workdir).unwrap();
        assert_eq!(context.job_reporting, expected);
    }
}

#[test]
fn a_first_turn_never_restores_an_orphan_files_head() {
    let api = files_node::ModuleNode::new();
    let root = tempfile::tempdir().unwrap();
    let private = PrivateDirectory::create(root.path()).unwrap();
    persist(&api, &private.0, &view(), None, &history(), "orphan").unwrap();
    let workdir = root.path().join("workspace");
    std::fs::create_dir(&workdir).unwrap();
    let (_, context, _) = materialize(&api, &view(), &workdir).unwrap();
    assert_eq!(context.revision, 0);
    assert!(!workdir.join(context.session_path).exists());
}

#[test]
fn native_packages_restore_the_pin_inside_runtime_and_cannot_preinstall_runtime_symlinks() {
    let api = files_node::ModuleNode::new();
    let root = tempfile::tempdir().unwrap();
    let package = root.path().join("package");
    checkout(&api, &package, "/packages/resident", None).unwrap();
    std::fs::write(package.join("SKILL.md"), "pinned instructions").unwrap();
    let pin = commit(&api, &package, "package").unwrap().snapshot;
    std::fs::write(package.join("SKILL.md"), "newer instructions").unwrap();
    commit(&api, &package, "new package").unwrap();
    let mut configuration = view();
    configuration.packages = serde_json::from_value(
        json!([{"name":"resident", "source_prefix":"/packages/resident", "source_snapshot":pin}]),
    )
    .unwrap();
    let workdir = root.path().join("workspace");
    std::fs::create_dir(&workdir).unwrap();
    symlink(&package, workdir.join(provider_host::RUN_RUNTIME_DIR)).unwrap();
    let (_, context, _) = materialize(&api, &configuration, &workdir).unwrap();
    assert_eq!(
        context.packages[0].path,
        Path::new(".ducktape-run/native/packages/resident")
    );
    assert_eq!(
        std::fs::read_to_string(workdir.join(&context.packages[0].path).join("SKILL.md")).unwrap(),
        "pinned instructions"
    );
    assert_eq!(
        std::fs::read_to_string(package.join("SKILL.md")).unwrap(),
        "newer instructions"
    );
    assert!(
        !workdir
            .join(&context.packages[0].path)
            .join(".duckfs")
            .exists()
    );
    configuration
        .packages
        .push(configuration.packages[0].clone());
    assert!(
        materialize(&api, &configuration, &workdir).is_err(),
        "a second pin must not reuse an existing package directory"
    );
}

#[test]
fn dedicated_subtree_and_bound_configuration_are_required() {
    let mut configuration = view();
    let spec = WorkspaceSpec {
        run_id: "host-1".into(),
        agent: None,
        ro_mounts: vec![],
        source: WorkspaceSource::Duckfs {
            source_prefix: "/ordinary".into(),
            source_snapshot: None,
        },
    };
    validate_configuration(&configuration, &spec).unwrap();
    configuration.history_prefix = "/ordinary/history".into();
    assert!(validate_configuration(&configuration, &spec).is_err());
    configuration = view();
    configuration.session_path = ".duckfs/session.jsonl".into();
    assert!(validate_configuration(&configuration, &spec).is_err());
    configuration.session_path = ".DUCKFS/session.jsonl".into();
    assert!(validate_configuration(&configuration, &spec).is_err());
    configuration = view();
    configuration.active_turn.as_mut().unwrap().run_id = "other-run".into();
    assert!(validate_active(&configuration, &view(), RUN_ID).is_err());
    configuration = view();
    configuration.active_turn.as_mut().unwrap().phase = ConversationTurnPhase::Draining;
    assert!(validate_active(&configuration, &view(), RUN_ID).is_err());
}

#[test]
fn history_overlap_normalizes_root_and_trailing_separators_without_prefix_collisions() {
    let mut configuration = view();
    configuration.history_prefix = "/shared/native/history".into();
    for source_prefix in ["/", "/shared/", "/shared", "/shared/native/history/"] {
        let spec = WorkspaceSpec {
            run_id: "host-1".into(),
            agent: None,
            ro_mounts: vec![],
            source: WorkspaceSource::Duckfs {
                source_prefix: source_prefix.into(),
                source_snapshot: None,
            },
        };
        assert!(
            validate_configuration(&configuration, &spec).is_err(),
            "{source_prefix}"
        );
    }
    assert!(!prefixes_overlap(
        "/shared/native/history",
        "/shared/ordinary/"
    ));
    assert!(!prefixes_overlap("/shared-native/history", "/shared/"));
    assert!(prefixes_overlap(
        "/shared/native/history",
        "/shared/native/history/child"
    ));
}

#[tokio::test]
async fn native_route_is_capability_scoped_and_unavailable_to_non_native_runs() {
    let signer = ed25519::PrivateKey::from_seed(1);
    let session = super::super::start_action_server(
        NodeLink::new("http://127.0.0.1:0"),
        signer,
        RUN_ID.into(),
        None,
    )
    .await
    .unwrap();
    let url = session
        .action_url
        .replace("/v1/run-action", "/v1/native-conversation");
    let client = reqwest::Client::new();
    let unauthorized = client
        .post(&url)
        .json(&json!({"kind":"control"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let non_native = client
        .post(&url)
        .header(ACTION_HEADER, &session.action_token)
        .json(&json!({"kind":"control"}))
        .send()
        .await
        .unwrap();
    assert_eq!(non_native.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn unproven_control_claim_is_refused_before_network_publication() {
    let root = tempfile::tempdir().unwrap();
    let native = native(root.path());
    let state = ActionState {
        node: NodeLink::new("http://127.0.0.1:0"),
        signer: ed25519::PrivateKey::from_seed(1),
        run_id: RUN_ID.into(),
        token: "host-secret".into(),
        seq: tokio::sync::Mutex::new(0),
        native: None,
    };
    let error = checkpoint(
        &state,
        &native,
        history(),
        true,
        false,
        vec!["queue-only-id".into()],
    )
    .await
    .unwrap_err();
    assert!(error.contains("do not match durable native receipts"));
}

#[test]
fn forty_history_candidates_never_consume_the_ordinary_pin_quota() {
    let api = files_node::ModuleNode::new();
    let root = tempfile::tempdir().unwrap();
    let private = PrivateDirectory::create(root.path()).unwrap();
    let configuration = view();
    let mut previous = None;
    let mut jsonl = history();
    for revision in 1..=40 {
        jsonl.push_str(&format!("{}\n", json!({"type":"custom", "id":format!("checkpoint-{revision}"),
            "parentId":"delivery-1", "timestamp":"now", "customType":"native.test", "data":revision})));
        let snapshot = persist(
            &api,
            &private.0,
            &configuration,
            previous.as_ref(),
            &jsonl,
            &format!("operation-{revision}"),
        )
        .unwrap();
        previous = Some(ConversationHistory { revision, snapshot });
        assert!(api.refs().unwrap().pins.is_empty());
    }
}

#[test]
fn steer_receipts_are_qualified_actual_users_and_match_the_current_control() {
    let jsonl = control_history("job-a", "steer-1", "focus here");
    let proof = validate_jsonl(&jsonl, &[], None).unwrap();
    let id = controls::qualified_id("job-a", "steer-1");
    controls::validate_claims(std::slice::from_ref(&id), &proof).unwrap();
    assert!(controls::validate_claims(&[], &proof).is_err());
    assert!(controls::validate_claims(&[id.clone(), id], &proof).is_err());
    let worker = worker_controls_fixture("job-a", "steer-1", "focus here");
    assert_eq!(
        controls::plan(&proof, &HistoryReceipts::default(), Some(&worker))
            .unwrap()
            .len(),
        1
    );
    let other_text = worker_controls_fixture("job-a", "steer-1", "different instruction");
    assert!(controls::plan(&proof, &HistoryReceipts::default(), Some(&other_text)).is_err());
    let later_job = worker_controls_fixture("job-b", "steer-1", "focus here");
    assert!(controls::plan(&proof, &HistoryReceipts::default(), Some(&later_job)).is_err());
    assert!(
        controls::plan(&proof, &proof, Some(&later_job))
            .unwrap()
            .is_empty(),
        "historical markers do not ACK the next job's same operation name"
    );
    assert!(
        validate_jsonl(
            &jsonl.replace(
                "\"message_id\":\"control-user\"",
                "\"message_id\":\"user-1\""
            ),
            &[],
            None
        )
        .is_err()
    );
}

#[test]
fn cancellation_requires_this_turns_current_job_cancel_and_is_not_input_delivery() {
    let root = tempfile::tempdir().unwrap();
    let native = native(root.path());
    let jsonl = cancellation_history("job-a", "cancel-1");
    let proof = validate_jsonl(&jsonl, &[], None).unwrap();
    controls::validate_claims(&[controls::qualified_id("job-a", "cancel-1")], &proof).unwrap();
    assert!(validate_jsonl(&jsonl, &[], Some(("resident", LOGICAL_TURN_ID, &[]))).is_err());
    let mut worker = worker_controls_fixture("job-a", "cancel-1", "not a cancellation");
    assert!(
        controls::cancellation_plan(
            &proof,
            &HistoryReceipts::default(),
            Some(&worker),
            &native.context
        )
        .is_err()
    );
    worker.controls[0].input = tasks::JobControlInput::Cancel;
    assert!(
        controls::cancellation_plan(
            &proof,
            &HistoryReceipts::default(),
            Some(&worker),
            &native.context
        )
        .unwrap()
        .is_some()
    );
    let wrong_turn =
        validate_jsonl(&jsonl.replace(LOGICAL_TURN_ID, "events/1/2"), &[], None).unwrap();
    assert!(
        controls::cancellation_plan(
            &wrong_turn,
            &HistoryReceipts::default(),
            Some(&worker),
            &native.context
        )
        .is_err()
    );
    worker.job_id = "job-b".into();
    assert!(
        controls::cancellation_plan(
            &proof,
            &HistoryReceipts::default(),
            Some(&worker),
            &native.context
        )
        .is_err()
    );
    assert!(
        controls::cancellation_plan(&proof, &proof, Some(&worker), &native.context)
            .unwrap()
            .is_none()
    );
}

#[test]
fn request_cannot_choose_config_paths_revisions_or_signer_messages() {
    for value in [
        json!({"kind":"control", "delivered_control_ids":["id"]}),
        json!({"kind":"checkpoint", "jsonl":history(), "delivery":true, "complete":false, "delivered_control_ids":[], "revision":99}),
        json!({"kind":"checkpoint", "jsonl":history(), "delivery":true, "complete":false, "delivered_control_ids":[], "session_path":"auth.json"}),
        json!({"kind":"config", "auth":{}}),
    ] {
        assert!(serde_json::from_value::<Request>(value).is_err());
    }
}
