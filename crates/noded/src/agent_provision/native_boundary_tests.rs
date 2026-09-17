//! Cross the real queue/composer/compute/native-provision boundary. Neither the
//! execution ID nor the native descriptor is authored by this test.
use super::*;
use crate::agent_provision::session_boundary_tests::{WORKER_NODE, genesis};
use commonware_runtime::Runner as _;
use host::{BlockContext, Host};
use sdk::{Event, Msg, Origin};

#[path = "native_worker_boundary_tests.rs"]
mod worker;

const RESIDENT: &str = "resident";
const CONVERSATION: &str = "native-boundary";
const CAPABILITY: &str = "native-test";

fn at(height: u64, origin: Origin) -> BlockContext {
    BlockContext {
        height,
        consensus_time: height,
        origin,
    }
}

fn controller() -> Origin {
    Origin::External(vec![1; 32])
}

async fn apply(host: &mut Host, height: &mut u64, origin: Origin, msg: Msg) -> Vec<Event> {
    *height += 1;
    let mut events = host
        .submit_at(at(*height, origin), msg)
        .await
        .unwrap()
        .events;
    // Drain the host's real queue, not a clock or a synthetic response list.
    while host.has_pending_work().await.unwrap() {
        *height += 1;
        events.extend(
            host.submit_block(at(*height, Origin::System), vec![])
                .await
                .unwrap()
                .events,
        );
    }
    events
}

fn runs_msg(message: runs::RunsMsg) -> Msg {
    Msg {
        target: "runs".into(),
        payload: runs::encode_msg(&message),
    }
}

fn worker_request(events: Vec<Event>) -> saga::WorkerRequest {
    let mut requests = events
        .into_iter()
        .filter_map(|event| saga::decode_worker_request(&event.payload).ok());
    let request = requests
        .next()
        .expect("real Runs queue emitted a worker request");
    assert!(
        requests.next().is_none(),
        "one frozen input range queues one execution"
    );
    assert_eq!(request.assignee.as_deref(), Some(WORKER_NODE));
    request
}

async fn conversation(host: &Host) -> ConversationView {
    read_conversation(host, CONVERSATION).await
}

async fn read_conversation(host: &Host, id: &str) -> ConversationView {
    let reply = host
        .query(
            "runs",
            &runs::encode_query(&runs::RunsQuery::Conversation {
                conversation_id: id.into(),
            }),
        )
        .await
        .unwrap();
    let runs::RunsReply::Conversation(Some(view)) = runs::decode_reply(&reply).unwrap() else {
        panic!("the real committed conversation must exist");
    };
    view
}

fn workspace(request: &saga::WorkerRequest) -> WorkspaceSpec {
    let work = dispatch::decode_work_spec(&request.spec).unwrap();
    let prepared =
        compute_service::envelope::prepare(std::str::from_utf8(&work.payload).unwrap()).unwrap();
    WorkspaceSpec {
        run_id: format!("{}:{}", request.saga_id, request.attempt),
        agent: Some(compute_service::AgentExecution {
            run_id: prepared.workspace.consensus_run_id,
            native_conversation: prepared.workspace.native_conversation,
            attempt: request.attempt,
            agent_id: RESIDENT.into(),
            display_name: prepared.workspace.agent_display_name,
        }),
        source: prepared.workspace.source,
        ro_mounts: prepared.workspace.skills,
    }
}

async fn provision_native(
    host: &mut Host,
    height: &mut u64,
    spec: &WorkspaceSpec,
    workdir: &Path,
) -> NativeState {
    let signer = ed25519::PrivateKey::from_seed(*height);
    let execution = spec.agent.as_ref().unwrap();
    apply(
        host,
        height,
        Origin::External(WORKER_NODE.to_vec()),
        runs_msg(runs::RunsMsg::OpenAgentSession {
            run_id: execution.run_id.clone(),
            attempt: execution.attempt,
            session_key: signer.public_key().to_vec(),
        }),
    )
    .await;
    let committed = read_conversation(
        host,
        &execution
            .native_conversation
            .as_ref()
            .unwrap()
            .conversation_id,
    )
    .await;
    let (handle, mut commands, _hub) = crate::NodeHandle::channel();
    let link = crate::agent_provision::test_link(handle).await;
    std::fs::create_dir(workdir).unwrap();
    let preparation = prepare(&link, spec, &signer, workdir);
    tokio::pin!(preparation);
    let native = loop {
        tokio::select! {
            result = &mut preparation => break result.expect("real queued descriptor is accepted by native host").unwrap(),
            command = commands.next() => {
                let crate::NodeCommand::Query { target, req, reply, .. } = command.expect("native query actor remains live") else {
                    panic!("fresh native materialization only reads committed state");
                };
                let result = host.query(&target, &req).await.map_err(|error| crate::Refused::of(&error));
                let _ = reply.send(result);
            }
        }
    };
    assert_eq!(native.configuration, committed);
    let descriptor = execution.native_conversation.as_ref().unwrap();
    assert_eq!(native.context.turn_id, descriptor.turn_id);
    assert_eq!(native.context.events, descriptor.events);
    assert!(native.context.session_path.is_relative());
    native
}

#[test]
fn real_runs_queue_and_explicit_retry_cross_the_native_host_with_one_logical_turn() {
    let root = tempfile::tempdir().unwrap();
    let cfg = commonware_runtime::tokio::Config::default()
        .with_storage_directory(root.path().join("storage"));
    commonware_runtime::tokio::Runner::new(cfg).start(|context| async move {
        let mut host = genesis(context, root.path()).await;
        let mut height = 0;
        let setup = [
            Msg { target:"identity".into(), payload:identity::encode_msg(&identity::IdentityMsg::Create {
                name:"Alice".into(), scheme:identity::KeyScheme::Ed25519,
            }) },
            Msg { target:"agent".into(), payload:agent::encode_msg(&agent::AgentMsg::Provision {
                request_id:RESIDENT.into(), name:"Resident".into(), program:runs::model_program(RESIDENT),
            }) },
            runs_msg(runs::RunsMsg::ConfigureModel { operation:runs::ModelMsg::RegisterModel {
                account:2, agent_id:RESIDENT.into(), display_name:"Resident".into(), capability:CAPABILITY.into(), recipe_hash:None, skills:None,
            } }),
            runs_msg(runs::RunsMsg::ConfigureConversation {
                conversation_id:CONVERSATION.into(), agent_id:RESIDENT.into(), source:runs::ConversationSource::Detached,
                history_prefix:"/resident/native-boundary/history".into(), session_path:"sessions/resident.jsonl".into(), packages:vec![],
            }),
            runs_msg(runs::RunsMsg::AppendConversationInput {
                conversation_id:CONVERSATION.into(), operation_id:"first-input".into(), input:runs::ConversationInput::Control {content:"hello".into()},
            }),
        ];
        for msg in setup { apply(&mut host, &mut height, controller(), msg).await; }
        apply(&mut host, &mut height, Origin::External(WORKER_NODE.to_vec()), Msg {
            target:"capability".into(), payload:capability::encode_msg(&capability::CapabilityMsg::Announce {
                capabilities:vec![CAPABILITY.into()], resources:Default::default(),
            }),
        }).await;
        let first = worker_request(apply(&mut host, &mut height, controller(), runs_msg(runs::RunsMsg::ActivateConversation {
            conversation_id:CONVERSATION.into(), operation_id:"activate".into(), active:true,
        })).await);
        let first_spec = workspace(&first);
        let first_execution = first_spec.agent.as_ref().unwrap();
        let first_descriptor = first_execution.native_conversation.as_ref().unwrap();
        assert_eq!(first_descriptor.events[0].sequence, 1);
        assert_eq!(first_descriptor.turn_id, "events/0/1", "the producer uses the exclusive committed start cursor, not the first event's inclusive sequence");
        assert_ne!(first_descriptor.turn_id, first_execution.run_id);
        let original = provision_native(&mut host, &mut height, &first_spec, &root.path().join("first")).await;
        let first_view = conversation(&host).await;
        assert_eq!(first_view.active_turn.as_ref().unwrap().run_id, first_execution.run_id);
        assert_eq!((first_view.active_turn.as_ref().unwrap().from_cursor, first_view.active_turn.as_ref().unwrap().through_cursor), (0, 1));

        // End the real execution without a native delivery checkpoint. Runs
        // pauses at its drain boundary instead of consuming the input; explicit
        // retry queues that same frozen range under a newly minted run ID.
        apply(&mut host, &mut height, Origin::External(WORKER_NODE.to_vec()), Msg {
            target:"saga".into(), payload:saga::encode_msg(&saga::SagaMsg::OracleResult {
                saga_id:first.saga_id.clone(), attempt:first.attempt, usage:None,
                outcome:Ok(serde_json::to_vec(&json!({
                    "ducktape_runner_result":1, "response_text":"{\"reply_blocks\":[],\"actions\":[]}",
                    "workspace_receipt":compute_service::WorkspaceReceipt::no_changes(&first_spec),
                })).unwrap()),
            }),
        }).await;
        let ended = conversation(&host).await;
        assert_eq!(ended.completed_cursor, 0);
        assert_eq!(ended.active_turn.as_ref().unwrap().phase, ConversationTurnPhase::Draining);
        let retry = worker_request(apply(&mut host, &mut height, controller(), runs_msg(runs::RunsMsg::RetryConversationTurn {
            conversation_id:CONVERSATION.into(), operation_id:"explicit-retry".into(),
        })).await);
        let retry_spec = workspace(&retry);
        let retry_execution = retry_spec.agent.as_ref().unwrap();
        let retry_descriptor = retry_execution.native_conversation.as_ref().unwrap();
        assert_ne!(retry_execution.run_id, first_execution.run_id);
        assert_eq!(retry_descriptor.turn_id, first_descriptor.turn_id);
        assert_eq!(retry_descriptor.events, first_descriptor.events);
        let resumed = provision_native(&mut host, &mut height, &retry_spec, &root.path().join("retry")).await;
        assert_eq!(resumed.context.turn_id, original.context.turn_id);
        assert_eq!(resumed.context.events, original.context.events);
        assert!(validate_active(&resumed.configuration, &resumed.configuration, &first_execution.run_id).is_err());
    });
}
