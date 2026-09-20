//! Signed people -> the committed Chat/Runs/Agent guests -> independent Jobs and
//! protected Pages.
//! The shipping initializer and Agent program execute on the real Host queue.
//! Model decisions and native history bytes are deterministic fixtures: this
//! does not execute Chief's TypeScript package, Pi, a VM, or a network transport.
#[allow(dead_code)]
#[path = "../src/chief_cli/plan.rs"]
mod plan;
#[path = "../src/wire.rs"]
mod wire;

use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use futures::executor::block_on;
use host::{BlockContext, Host};
use noded::testkit::committed_module;
use sdk::{Msg, Origin};
use sha2::{Digest as _, Sha256};
use wire::{agent, chat, pages, runs, tasks};

/// this composition's identity chain id — and the value the `runs` guest reads
/// out of its genesis `__config` record.
const CHAIN_ID: &str = "chief-test";
const CHIEF: u64 = 4;
const WORKER: u64 = 3;
const JOB: &str = "chief-independent-job";
const TASK: &str = "chief-reviewed-task";
const REPORTED_JOB: &str = "chief-provider-reported-job";

// Resolver-backed production modules retain only committed store bytes here.
// Restart freezes and copies those bytes into NEW stores, not live module clones.
#[derive(Clone, Default)]
struct Store(Rc<RefCell<BTreeMap<Vec<u8>, Vec<u8>>>>);

#[async_trait::async_trait(?Send)]
impl sdk::MerkleStore for Store {
    async fn get(&self, key: &[u8; sdk::ROOT_LEN]) -> Result<Option<Vec<u8>>, sdk::Error> {
        Ok(self.0.borrow().get(key.as_slice()).cloned())
    }
    async fn commit_batch(
        &mut self,
        writes: Vec<([u8; sdk::ROOT_LEN], Option<Vec<u8>>)>,
    ) -> Result<(), sdk::Error> {
        let mut map = self.0.borrow_mut();
        for (key, value) in writes {
            match value {
                Some(value) => {
                    map.insert(key.to_vec(), value);
                }
                None => {
                    map.remove(key.as_slice());
                }
            }
        }
        Ok(())
    }
    fn root(&self) -> sdk::StateRoot {
        sdk::StateRoot(Sha256::digest(sdk::hash::encode_pairs(&self.0.borrow())).into())
    }
    async fn sync_target(&self) -> Result<sdk::ResolverSyncTarget, sdk::Error> {
        Err(sdk::Error::QueryUnsupported)
    }
    async fn serve_sync(&self, _: &[u8]) -> Result<Vec<u8>, sdk::Error> {
        Err(sdk::Error::QueryUnsupported)
    }
}

type Stores = BTreeMap<&'static str, Store>;
fn store(stores: &mut Stores, name: &'static str) -> Box<dyn sdk::MerkleStore> {
    Box::new(stores.entry(name).or_default().clone())
}

// `runs-wire` exposes this reply with its own dispatch-wire dependency. The
// node test consumes only the stable status shape, so keep that nested type at
// this boundary instead of coupling the test to the producer's wire crate.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ActionReceiptReply {
    ActionRequest(Option<ActionReceipt>),
}

#[derive(Debug, serde::Deserialize)]
struct ActionReceipt {
    status: ActionReceiptStatus,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ActionReceiptStatus {
    Completed { outcome: ActionReceiptOutcome },
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ActionReceiptOutcome {
    Applied {
        #[serde(rename = "output_digest")]
        _output_digest: [u8; 32],
        #[serde(rename = "assigned")]
        _assigned: Vec<u8>,
    },
}

fn signer(seed: u64) -> PrivateKey {
    PrivateKey::from_seed(seed)
}
fn key(seed: u64) -> Vec<u8> {
    signer(seed).public_key().as_ref().to_vec()
}
fn msg(target: &str, payload: impl serde::Serialize) -> Msg {
    Msg {
        target: target.into(),
        payload: sdk::wire::encode(&payload),
    }
}
fn context(height: u64, origin: Origin) -> BlockContext {
    BlockContext {
        height,
        consensus_time: height,
        origin,
    }
}

struct Network {
    host: Host,
    stores: Stores,
    directory: tempfile::TempDir,
    height: u64,
    trace: Vec<host::DispatchRecord>,
}
/// how one composition comes up. Block zero seeds the validator set; a restart
/// finds it — and every other module's committed state — in the store it
/// reopens, so it seeds nothing.
enum Boot {
    Genesis,
    Restart,
}

impl Network {
    /// agent, chat, pages, tasks and runs are the COMMITTED guests this repo
    /// ships (`crates/modules/apps/<id>/component.wasm`): their source lives in
    /// ducktape-modules, and the sibling wiring a native constructor used to
    /// take as arguments is compiled into each guest. The system modules and
    /// files are core-resident and still native.
    async fn compose(stores: &mut Stores, directory: &std::path::Path, boot: Boot) -> Host {
        let mut validators = valset::Valset::new("valset", store(stores, "valset"), "governance");
        match boot {
            Boot::Genesis => {
                validators.seed(key(8)).await.unwrap();
                validators.seed(key(7)).await.unwrap();
                validators.finish_seed().await.unwrap();
            }
            Boot::Restart => {}
        }
        Host::genesis(vec![
            Box::new(identity::Identity::new(
                "identity",
                store(stores, "identity"),
                CHAIN_ID.into(),
            )),
            Box::new(
                attribution::AttributionModule::new("attribution", store(stores, "attribution"))
                    .with_subscribers(["agent"]),
            ),
            Box::new(committed_module("agent", store(stores, "agent"), CHAIN_ID).await),
            Box::new(committed_module("chat", store(stores, "chat"), CHAIN_ID).await),
            Box::new(committed_module("pages", store(stores, "pages"), CHAIN_ID).await),
            Box::new(validators),
            Box::new(capability::CapabilityRegistry::new(
                "capability",
                store(stores, "capability"),
                Some("valset".into()),
            )),
            Box::new(saga::SagaModule::with_assignment(
                "saga",
                store(stores, "saga"),
                "valset",
                "capability",
                saga::LeasePolicy::Open,
            )),
            Box::new(dispatch::DispatchModule::new(
                "dispatch",
                "saga",
                "identity",
                store(stores, "dispatch"),
            )),
            Box::new(committed_module("tasks", store(stores, "tasks"), CHAIN_ID).await),
            Box::new(files::Files::open("files", directory.to_path_buf()).unwrap()),
            Box::new(committed_module("runs", store(stores, "runs"), CHAIN_ID).await),
        ])
        .unwrap()
    }
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut stores = Stores::new();
        let host = Self::compose(&mut stores, directory.path(), Boot::Genesis).await;
        Self {
            host,
            stores,
            directory,
            height: 0,
            trace: Vec::new(),
        }
    }
    async fn submit(&mut self, seed: u64, target: &str, payload: impl serde::Serialize) {
        self.try_submit(seed, target, payload).await.unwrap();
    }
    async fn try_submit(
        &mut self,
        seed: u64,
        target: &str,
        payload: impl serde::Serialize,
    ) -> Result<(), String> {
        self.height += 1;
        let frame = node::encode_frame(&signer(seed), self.height, &msg(target, payload));
        let (origin, message) = node::decode_frame(&frame).expect("real signed frame verifies");
        let outcome = self
            .host
            .submit_at(context(self.height, origin), message)
            .await
            .map_err(|e| format!("{e:?}"))?;
        self.trace.extend(outcome.dispatches);
        Ok(())
    }
    async fn step(&mut self) {
        self.height += 1;
        let outcome = self
            .host
            .submit_block(context(self.height, Origin::System), Vec::new())
            .await
            .unwrap();
        self.trace.extend(outcome.internal_dispatches());
    }
    async fn drain(&mut self) {
        // Execute committed queue batches; this is not time-based polling.
        while self.host.has_pending_work().await.unwrap() {
            self.step().await;
        }
    }
    async fn query<T: serde::de::DeserializeOwned>(
        &self,
        target: &str,
        query: impl serde::Serialize,
    ) -> T {
        let bytes = self
            .host
            .query(target, &sdk::wire::encode(&query))
            .await
            .unwrap();
        sdk::wire::decode(&bytes).unwrap()
    }
    async fn restart(self) -> Self {
        let (snapshot, _) =
            self.host
                .capture_current_snapshot(self.height, host::CapturePayloads::All, || {
                    std::time::Duration::ZERO
                });
        let mut stores = self
            .stores
            .iter()
            .map(|(name, store)| {
                (
                    *name,
                    Store(Rc::new(RefCell::new(store.0.borrow().clone()))),
                )
            })
            .collect();
        let pending = self.host.has_pending_work().await.unwrap();
        drop(self.host);
        let mut host = Self::compose(&mut stores, self.directory.path(), Boot::Restart).await;
        host.restore_committed(self.height, self.height);
        assert_eq!(
            host.root_hash(),
            snapshot.root_hash,
            "every module recovered its committed state"
        );
        assert_eq!(host.has_pending_work().await.unwrap(), pending);
        Self {
            host,
            stores,
            directory: self.directory,
            height: self.height,
            trace: self.trace,
        }
    }
    async fn conversation(&self, id: &str) -> runs::ConversationView {
        let runs::RunsReply::Conversation(Some(state)) = self
            .query(
                "runs",
                runs::RunsQuery::Conversation {
                    conversation_id: id.into(),
                },
            )
            .await
        else {
            panic!("conversation")
        };
        state
    }
    async fn events(&self, id: &str) -> Vec<runs::ConversationEvent> {
        let runs::RunsReply::ConversationEvents(events) = self
            .query(
                "runs",
                runs::RunsQuery::ConversationEvents {
                    conversation_id: id.into(),
                    from: 1,
                    limit: 64,
                },
            )
            .await
        else {
            panic!("conversation events")
        };
        events
    }
    async fn runs(&self) -> Vec<runs::PendingRun> {
        let runs::RunsReply::PendingRuns(runs) =
            self.query("runs", runs::RunsQuery::PendingRuns).await
        else {
            panic!("runs")
        };
        runs
    }
    async fn run(&self, id: &str) -> runs::PendingRun {
        self.runs()
            .await
            .into_iter()
            .find(|run| run.run_id == id)
            .expect("pending run")
    }
    async fn saga(&self, run: &runs::PendingRun) -> String {
        let dispatch::DispatchReply::Dispatch(Some(view)) = self
            .query(
                "dispatch",
                dispatch::DispatchQuery::Dispatch {
                    receiver: "runs".into(),
                    dispatch_id: run.dispatch_id.clone(),
                },
            )
            .await
        else {
            panic!("dispatch")
        };
        let dispatch::DispatchStatus::AwaitingResult { saga_id } = view.status else {
            panic!("awaiting result: {:?}", view.status)
        };
        saga_id
    }
    fn native_payload(&self, run: &runs::PendingRun) -> run_envelope::NativeConversation {
        self.trace
            .iter()
            .rev()
            .find_map(|entry| {
                let from_runs =
                    entry.module == "dispatch" && entry.origin == Origin::Module("runs".into());
                if !from_runs {
                    return None;
                }
                let Ok(dispatch::DispatchMsg::Dispatch {
                    dispatch_id,
                    payload,
                    ..
                }) = dispatch::decode_msg(&entry.payload)
                else {
                    return None;
                };
                if dispatch_id != run.dispatch_id {
                    return None;
                }
                let value: serde_json::Value = sdk::wire::decode(&payload).unwrap();
                Some(serde_json::from_value(value["native_conversation"].clone()).unwrap())
            })
            .expect("real Runs dispatch carries a native conversation manifest")
    }
    async fn accept(&mut self, run: &runs::PendingRun, session: u64) {
        let saga_id = self.saga(run).await;
        self.submit(
            8,
            "saga",
            saga::SagaMsg::Accept {
                saga_id,
                attempt: 0,
            },
        )
        .await;
        self.submit(
            8,
            "runs",
            runs::RunsMsg::OpenAgentSession {
                run_id: run.run_id.clone(),
                attempt: 0,
                session_key: key(session),
            },
        )
        .await;
    }
    async fn action(
        &mut self,
        session: u64,
        run: &runs::PendingRun,
        request: &str,
        target: &str,
        operation: impl serde::Serialize,
    ) {
        self.submit(
            session,
            "runs",
            runs::RunsMsg::AgentAction {
                run_id: run.run_id.clone(),
                request_id: request.into(),
                action: runs::ActionEnvelope::new(
                    runs::OP_SUBMIT,
                    Some(serde_json::json!({"module":target})),
                    serde_json::to_value(operation).unwrap(),
                ),
            },
        )
        .await;
        self.drain().await;
        let ActionReceiptReply::ActionRequest(Some(receipt)) = self
            .query(
                "runs",
                runs::RunsQuery::ActionRequest {
                    request_id: runs::action_request_id(&run.run_id, request),
                },
            )
            .await
        else {
            panic!("action receipt")
        };
        assert!(
            matches!(
                receipt.status,
                ActionReceiptStatus::Completed {
                    outcome: ActionReceiptOutcome::Applied { .. },
                }
            ),
            "{request}: {receipt:?}"
        );
    }
    async fn checkpoint(
        &mut self,
        run: &runs::PendingRun,
        id: &str,
        revision: u64,
    ) -> runs::RunsMsg {
        let state = self.conversation(id).await;
        let files::FilesReply::Refs(refs) = self.query("files", files::FilesQuery::Refs {}).await
        else {
            panic!("Files refs")
        };
        self.submit(
            1,
            "files",
            files::FilesMsg::Commit {
                base_snapshot: refs.head,
                message: "deterministic native history fixture".into(),
                changes: vec![files::Change::Put {
                    path: format!("{}/{}", state.history_prefix, state.session_path),
                    exec: false,
                    meta: Default::default(),
                    content: files::Content::Inline {
                        b64: STANDARD.encode(format!(
                            "{{\"type\":\"session\",\"revision\":{revision}}}\n"
                        )),
                    },
                }],
            },
        )
        .await;
        let files::FilesReply::Refs(refs) = self.query("files", files::FilesQuery::Refs {}).await
        else {
            panic!("committed history")
        };
        let head = refs.head.expect("committed history snapshot");
        self.submit(
            8,
            "files",
            files::FilesMsg::ProjectSnapshot {
                snapshot: head.clone(),
                path: state.history_prefix.clone(),
            },
        )
        .await;
        let snapshot = self
            .trace
            .iter()
            .rev()
            .find_map(|entry| {
                let output = entry.output.as_ref()?;
                let Ok(files::FilesWriteOutput {
                    outcome: files::WriteOutcome::ProjectSnapshot { snapshot },
                    ..
                }) = files::decode_write_output(output)
                else {
                    return None;
                };
                Some(snapshot)
            })
            .expect("Files returns the committed scoped history snapshot");
        assert_ne!(
            snapshot, head,
            "history retention uses a subtree projection, not the whole workspace head"
        );
        let checkpoint = runs::RunsMsg::CheckpointConversation {
            conversation_id: id.into(),
            run_id: run.run_id.clone(),
            attempt: 0,
            operation_id: format!(
                "checkpoint-{}-{revision}",
                state.active_turn.as_ref().unwrap().turn
            ),
            history: runs::ConversationHistory { revision, snapshot },
            delivery: true,
        };
        self.submit(8, "runs", &checkpoint).await;
        let (retention_key, replacement) = self
            .trace
            .iter()
            .rev()
            .find_map(|entry| {
                let from_runs =
                    entry.module == "files" && entry.origin == Origin::Module("runs".into());
                if !from_runs {
                    return None;
                }
                let Ok(files::FilesMsg::CompareExchangeRetention {
                    key, replacement, ..
                }) = files::decode_msg(&entry.payload)
                else {
                    return None;
                };
                Some((key, replacement))
            })
            .expect("checkpoint executes module-owned Files retention CAS");
        let retained: files::FilesReply = self
            .query(
                "files",
                files::FilesQuery::Retention {
                    module_id: "runs".into(),
                    key: retention_key,
                },
            )
            .await;
        assert_eq!(retained, files::FilesReply::Retention(replacement));
        checkpoint
    }
    async fn finish(&mut self, run: &runs::PendingRun) {
        let saga_id = self.saga(run).await;
        self.submit(8, "saga", saga::SagaMsg::OracleResult {
            saga_id, attempt: 0, usage: None,
            outcome: Ok(sdk::wire::encode(&serde_json::json!({
                "ducktape_runner_result":1,"response_text":"","native_input_handled":true,
                "workspace_receipt":{"source_prefix":format!("/shared/agent-workspaces/{}",run.agent_id),"output_snapshot":null,"commit_height":null,"rebased":false,"no_changes":true}
            }))),
        }).await;
        self.drain().await;
        assert!(
            !self
                .runs()
                .await
                .iter()
                .any(|pending| pending.run_id == run.run_id),
            "execution must settle"
        );
    }
    async fn job(&self) -> tasks::Job {
        let tasks::WorkReply::Job(tasks::JobsReply::Job(Some(job))) = self
            .query(
                "tasks",
                tasks::WorkQuery::Job(tasks::JobsQuery::Get { job_id: JOB.into() }),
            )
            .await
        else {
            panic!("job")
        };
        job
    }
    async fn post(&mut self, seed: u64, channel: &str, id: &str, text: &str) {
        self.submit(
            seed,
            "chat",
            chat::ChatMsg::PostMessage {
                channel_id: channel.into(),
                message_id: id.into(),
                blocks: vec![chat::Block::paragraph(text)],
                thread: None,
            },
        )
        .await;
    }
}

async fn bootstrap(network: &mut Network) -> plan::Plan {
    for (seed, name) in [(1, "Alice"), (2, "Bob")] {
        network
            .submit(
                seed,
                "identity",
                identity::IdentityMsg::Create {
                    name: name.into(),
                    scheme: identity::KeyScheme::Ed25519,
                },
            )
            .await;
    }
    network
        .submit(
            8,
            "capability",
            capability::CapabilityMsg::Announce {
                capabilities: vec!["pi".into()],
                resources: Default::default(),
            },
        )
        .await;
    network
        .submit(
            1,
            "agent",
            agent::AgentMsg::Provision {
                request_id: "worker".into(),
                name: "Worker".into(),
                program: runs::model_program("worker"),
            },
        )
        .await;
    network.drain().await;
    network
        .submit(
            1,
            "runs",
            runs::RunsMsg::ConfigureModel {
                operation: runs::ModelMsg::RegisterModel {
                    account: WORKER,
                    agent_id: "worker".into(),
                    display_name: "Worker".into(),
                    capability: "pi".into(),
                    recipe_hash: None,
                    skills: None,
                },
            },
        )
        .await;
    network
        .submit(1, "runs", runs::RunsMsg::EnableJobWorker { enabled: true })
        .await;
    let plan = plan::Plan::new(1, "chief", None, "worker").unwrap();
    let source_prefix = format!("{}/package", plan.root());
    network
        .submit(
            1,
            "files",
            files::FilesMsg::Commit {
                base_snapshot: None,
                message: "resident package boundary fixture".into(),
                changes: vec![files::Change::Put {
                    path: format!("{source_prefix}/index.ts"),
                    exec: false,
                    meta: Default::default(),
                    content: files::Content::Inline {
                        b64: STANDARD.encode("export default () => {};\n"),
                    },
                }],
            },
        )
        .await;
    let files::FilesReply::Refs(refs) = network.query("files", files::FilesQuery::Refs {}).await
    else {
        panic!("package snapshot")
    };
    let package = run_envelope::ConversationPackage {
        name: "chief".into(),
        source_prefix,
        source_snapshot: refs
            .head
            .expect("package bytes are actually committed in Files"),
    };
    let (program, _) = plan::program(&plan, &package);
    network
        .submit(
            1,
            "agent",
            agent::AgentMsg::Provision {
                request_id: plan.namespace.clone(),
                name: "Chief".into(),
                program,
            },
        )
        .await;
    network.drain().await;
    let runs::RunsReply::Conversation(before) = network
        .query(
            "runs",
            runs::RunsQuery::Conversation {
                conversation_id: plan.namespace.clone(),
            },
        )
        .await
    else {
        panic!("before bootstrap")
    };
    assert!(
        before.is_none(),
        "provisioning alone must not initialize Chief"
    );
    network
        .submit(
            1,
            "agent",
            agent::AgentMsg::Initialize {
                account: CHIEF,
                request_id: "explicit-bootstrap".into(),
            },
        )
        .await;
    network.drain().await;
    let agent::AgentReply::Invocations(invocations) = network
        .query(
            "agent",
            agent::AgentQuery::Invocations {
                account: CHIEF,
                after: 0,
                limit: 64,
            },
        )
        .await
    else {
        panic!("initializer invocations")
    };
    let failures = invocations
        .iter()
        .filter(|entry| matches!(entry.invocation.status, agent::Status::Failed { .. }))
        .map(|entry| &entry.invocation.status)
        .collect::<Vec<_>>();
    assert!(failures.is_empty(), "initializer call failed: {failures:?}");
    assert_eq!(
        network.conversation(&plan.namespace).await.status,
        runs::ConversationStatus::Active
    );
    assert!(
        network.runs().await.is_empty(),
        "initialization is not a model turn"
    );
    plan
}

fn job_event(input: &runs::ConversationInput) -> Option<tasks::JobEventDetail> {
    let runs::ConversationInput::Event { content, .. } = input else {
        return None;
    };
    let change: attribution::Change =
        serde_json::from_value(content["attribution"].clone()).unwrap();
    let from_jobs = change.source.module == "tasks" && change.source.kind == "job_event";
    if !from_jobs {
        return None;
    }
    Some(sdk::wire::decode(&change.detail).expect("immutable native Jobs event detail"))
}

fn record(plan: &plan::Plan, revision: u64, status: &str) -> pages::PageMsg {
    pages::PageMsg::CommitRecords {
        page_id: plan.board_page_id.clone(),
        expected_revision: revision,
        request_id: format!("board-{revision}"),
        changes: vec![pages::RecordChange::Upsert {
            record_id: TASK.into(),
            data: serde_json::json!({"status":status,"job":JOB}),
            document: pages::RecordDocument {
                title: "Independent work awaiting Chief review".into(),
                blocks: Vec::new(),
            },
        }],
        state_changes: vec![pages::RecordStateChange::Put {
            key: "review".into(),
            value: serde_json::json!({"accepted":false,"job":JOB}),
        }],
        metadata: None,
        artifacts: Vec::new(),
    }
}

#[test]
fn signed_people_resident_turns_independent_jobs_and_snapshot_restart() {
    block_on(async {
        let mut network = Network::new().await;
        let plan = bootstrap(&mut network).await;
        network
            .post(
                1,
                &plan.channel_id,
                "alice-request",
                "Please organize the work",
            )
            .await;
        assert!(
            network.host.has_pending_work().await.unwrap(),
            "source commit leaves real queued work"
        );
        network = network.restart().await;
        network.drain().await;
        let first = network.conversation(&plan.namespace).await;
        let turn = first.active_turn.as_ref().unwrap();
        assert_eq!(turn.phase, runs::ConversationTurnPhase::Running);
        assert_eq!((turn.from_cursor, turn.through_cursor), (0, 1));
        let run = network.run(&turn.run_id).await;
        network.accept(&run, 9).await;
        network
            .post(
                2,
                &plan.channel_id,
                "bob-request",
                "Include my constraints too",
            )
            .await;
        network.drain().await;
        let two = network.conversation(&plan.namespace).await;
        assert_eq!(
            two.active_turn, first.active_turn,
            "second person queues behind one active resident execution"
        );
        assert_eq!(two.admitted_cursor, 2);
        assert_eq!(network.runs().await.len(), 1);
        let admitted = network.events(&plan.namespace).await;
        for (event, seed, text) in [
            (&admitted[0], 1, "Please organize the work"),
            (&admitted[1], 2, "Include my constraints too"),
        ] {
            assert_eq!(event.actor, Origin::External(key(seed)));
            let runs::ConversationInput::Chat { message } = &event.input else {
                panic!("authenticated source snapshot")
            };
            assert_eq!(message.head.origin, Origin::External(key(seed)));
            assert_eq!(message.head.author, chat::Party::Account(seed));
            assert_eq!(message.head.blocks, vec![chat::Block::paragraph(text)]);
        }
        network
            .submit(
                2,
                "chat",
                chat::ChatMsg::EditMessage {
                    channel_id: plan.channel_id.clone(),
                    seq: 1,
                    blocks: vec![chat::Block::paragraph("rewritten after admission")],
                    base_rev: None,
                },
            )
            .await;
        network
            .submit(
                1,
                "identity",
                identity::IdentityMsg::SetName {
                    name: "Renamed Alice".into(),
                },
            )
            .await;
        network.drain().await;
        assert_eq!(
            network.events(&plan.namespace).await,
            admitted,
            "edits and identity changes do not rewrite admitted actors or prompts"
        );
        network
            .action(
                9,
                &run,
                "chief-reply",
                "chat",
                chat::ChatMsg::PostMessage {
                    channel_id: plan.channel_id.clone(),
                    message_id: "chief-self-reply".into(),
                    blocks: vec![chat::Block::paragraph("I will coordinate both requests")],
                    thread: None,
                },
            )
            .await;
        assert_eq!(
            network.events(&plan.namespace).await,
            admitted,
            "own ordinary-channel response cannot feed itself"
        );
        network
            .action(9, &run, "chief-board", "pages", record(&plan, 0, "working"))
            .await;
        network
            .submit(
                2,
                "pages",
                pages::PageMsg::AddComment {
                    thread_id: "chief-task-discussion".into(),
                    comment_id: "bob-plain-comment".into(),
                    target: TASK.into(),
                    text: "Please verify this before acceptance".into(),
                    anchor: None,
                    mentions: Vec::new(),
                },
            )
            .await;
        network.drain().await;
        let discussion = network
            .events(&plan.namespace)
            .await
            .into_iter()
            .find(|event| {
                let runs::ConversationInput::Event { content, .. } = &event.input else {
                    return false;
                };
                content["source"]["comment"]["id"] == "bob-plain-comment"
            })
            .expect("plain managed-Page comment reaches resident queue without a mention");
        let runs::ConversationInput::Event { content, .. } = &discussion.input else {
            unreachable!()
        };
        let source: pages::ManagedDiscussionSnapshot =
            serde_json::from_value(content["source"].clone()).unwrap();
        assert_eq!(source.comment.author, pages::Party::Account(2));
        assert!(source.comment.mentions.is_empty());
        assert_eq!(source.comment.text, "Please verify this before acceptance");
        network
            .submit(
                2,
                "pages",
                pages::PageMsg::EditComment {
                    comment_id: "bob-plain-comment".into(),
                    text: "Changed after admission".into(),
                    mentions: Vec::new(),
                },
            )
            .await;
        network.drain().await;
        assert_eq!(
            network
                .events(&plan.namespace)
                .await
                .iter()
                .find(|event| event.sequence == discussion.sequence),
            Some(&discussion),
            "managed discussion snapshot is immutable too"
        );
        assert!(
            network
                .try_submit(2, "pages", record(&plan, 1, "accepted"))
                .await
                .is_err(),
            "ordinary discussion access cannot mutate Chief's protected records"
        );
        network
            .action(
                9,
                &run,
                "chief-worker",
                "tasks",
                tasks::WorkMsg::Job(tasks::JobsMsg::SubmitConversation {
                    job_id: JOB.into(),
                    kind: "agent/worker".into(),
                    spec: "Produce evidence for review, not acceptance".into(),
                }),
            )
            .await;
        let job = network.job().await;
        assert_eq!(job.execution, tasks::JobExecution::Conversation);
        assert_eq!(job.submitter, tasks::Party::Account(CHIEF));
        assert_eq!(job.status, tasks::JobStatus::Processing);
        let worker = network
            .runs()
            .await
            .into_iter()
            .find(|run| run.job_id.as_deref() == Some(JOB))
            .unwrap();
        assert_ne!(worker.run_id, run.run_id);
        assert_eq!(
            worker.requester,
            Origin::Program(WORKER),
            "the worker program requests its own independent execution"
        );
        network.accept(&worker, 10).await;
        // A separately authenticated provider reports semantic progress through
        // the real Jobs board. This does not pretend a model session holds the
        // Runs module's claim or fabricate that module's origin.
        network
            .action(
                9,
                &run,
                "chief-reported-worker",
                "tasks",
                tasks::WorkMsg::Job(tasks::JobsMsg::SubmitConversation {
                    job_id: REPORTED_JOB.into(),
                    kind: "fixture/external-provider".into(),
                    spec: "Report independently".into(),
                }),
            )
            .await;
        network
            .submit(
                8,
                "tasks",
                tasks::WorkMsg::Job(tasks::JobsMsg::Claim {
                    job_id: REPORTED_JOB.into(),
                    lease_views: 1000,
                }),
            )
            .await;
        let checkpoint = network.checkpoint(&run, &plan.namespace, 1).await;
        let worker_conversation = network.native_payload(&worker).conversation_id;
        network.checkpoint(&worker, &worker_conversation, 1).await;
        let checkpointed = network.conversation(&plan.namespace).await;
        assert!(
            network.job().await.native_history.is_some(),
            "Files checkpoint propagates through Runs to Jobs"
        );
        let events_before_restart = network.events(&plan.namespace).await;
        network = network.restart().await;
        assert_eq!(network.conversation(&plan.namespace).await, checkpointed);
        assert_eq!(network.events(&plan.namespace).await, events_before_restart);
        network.submit(8, "runs", &checkpoint).await;
        assert_eq!(
            network.conversation(&plan.namespace).await,
            checkpointed,
            "checkpoint replay remains idempotent after restart"
        );
        let runs::RunsMsg::CheckpointConversation { history, .. } = &checkpoint else {
            unreachable!()
        };
        let files::FilesReply::Read { b64, eof } = network
            .query(
                "files",
                files::FilesQuery::Read {
                    path: format!("{}/session.jsonl", checkpointed.history_prefix),
                    snapshot: Some(history.snapshot.clone()),
                    offset: 0,
                    len: 1024,
                },
            )
            .await
        else {
            panic!("restored history bytes")
        };
        assert!(eof);
        assert_eq!(
            STANDARD.decode(b64).unwrap(),
            b"{\"type\":\"session\",\"revision\":1}\n"
        );
        network.finish(&run).await;
        assert_eq!(
            network.job().await.status,
            tasks::JobStatus::Processing,
            "Chief completion must not cancel independent worker lifetime"
        );
        assert!(
            network
                .runs()
                .await
                .iter()
                .any(|pending| pending.run_id == worker.run_id)
        );
        let resumed = network.conversation(&plan.namespace).await;
        assert_eq!(resumed.completed_cursor, 1);
        assert_eq!(resumed.history.as_ref(), Some(history));
        let tasks::WorkReply::Job(tasks::JobsReply::Job(Some(independent))) = network
            .query(
                "tasks",
                tasks::WorkQuery::Job(tasks::JobsQuery::Get {
                    job_id: REPORTED_JOB.into(),
                }),
            )
            .await
        else {
            panic!("independent provider job")
        };
        assert_eq!(
            independent.status,
            tasks::JobStatus::Processing,
            "both Jobs survive coordinator completion"
        );
        network
            .submit(
                8,
                "tasks",
                tasks::WorkMsg::Job(tasks::JobsMsg::Checkpoint {
                    job_id: REPORTED_JOB.into(),
                    operation_id: "verified-progress".into(),
                    attempt: independent.attempt,
                    kind: tasks::WorkerReportKind::Checkpoint,
                    payload: "Evidence gathered; acceptance belongs to Chief".into(),
                }),
            )
            .await;
        network.drain().await;
        let progress = network
            .events(&plan.namespace)
            .await
            .into_iter()
            .filter_map(|event| job_event(&event.input))
            .find(|event| {
                event.job_id == REPORTED_JOB
                    && matches!(event.operation, tasks::JobsMsg::Checkpoint { .. })
            })
            .expect("authenticated Tasks checkpoint reaches Chief's native queue without mentions");
        assert_eq!(progress.actor, tasks::Party::Key(key(8)));
        assert_eq!(progress.job_attempt, independent.attempt);
        let tasks::JobsMsg::Checkpoint { payload, .. } = &progress.operation else {
            unreachable!()
        };
        assert_eq!(payload, "Evidence gathered; acceptance belongs to Chief");
        let second = resumed.active_turn.as_ref().unwrap();
        assert_eq!(second.from_cursor, 1);
        assert_ne!(second.run_id, run.run_id);
        assert!(
            network.try_submit(8, "runs", &checkpoint).await.is_err(),
            "settled attempt cannot replay its checkpoint as new authority"
        );
        network.finish(&worker).await;
        let succeeded = network.job().await;
        assert_eq!(succeeded.status, tasks::JobStatus::Done);
        assert!(succeeded.result.as_ref().unwrap().ok);
        let events = network.events(&plan.namespace).await;
        let reports_success = events
            .iter()
            .filter_map(|event| job_event(&event.input))
            .any(|event| {
                event.job_id == JOB
                    && matches!(event.operation, tasks::JobsMsg::Finalize { ok: true, .. })
            });
        assert!(
            reports_success,
            "real Jobs attribution queues completion evidence to the resident, not an invented model event"
        );
        let pages::PageReply::Record(Some(managed_record)) = network
            .query(
                "pages",
                pages::PageQuery::Record {
                    page_id: plan.board_page_id.clone(),
                    record_id: TASK.into(),
                },
            )
            .await
        else {
            panic!("managed task")
        };
        assert_eq!(
            managed_record.data["status"], "working",
            "worker success cannot make a Chief acceptance decision"
        );
        let pages::PageReply::RecordState(Some(state)) = network
            .query(
                "pages",
                pages::PageQuery::RecordState {
                    page_id: plan.board_page_id.clone(),
                    key: "review".into(),
                },
            )
            .await
        else {
            panic!("protected review state")
        };
        assert_eq!(state.value["accepted"], false);
        let terminal_event = events
            .iter()
            .find(|event| {
                job_event(&event.input).is_some_and(|job| {
                    job.job_id == JOB
                        && matches!(job.operation, tasks::JobsMsg::Finalize { ok: true, .. })
                })
            })
            .unwrap();
        let progress_event = events
            .iter()
            .find(|event| job_event(&event.input).as_ref() == Some(&progress))
            .unwrap();
        assert!(terminal_event.sequence > discussion.sequence);
        assert!(terminal_event.sequence > progress_event.sequence);
        // Runs deliberately delivers one frozen event per turn. Execute the
        // finite committed prefix, not an assumed batch size or a timed wait.
        // This includes Bob, the managed Page discussion and worker progress.
        for expected in events
            .iter()
            .filter(|event| event.sequence > 1 && event.sequence < terminal_event.sequence)
        {
            let state = network.conversation(&plan.namespace).await;
            let turn = state.active_turn.as_ref().unwrap();
            assert_eq!(
                (turn.from_cursor, turn.through_cursor),
                (expected.sequence - 1, expected.sequence)
            );
            let pending = network.run(&turn.run_id).await;
            let native = network.native_payload(&pending);
            // The logical turn names its committed interval: the cursor it
            // starts AFTER and the one it ends ON.
            assert_eq!(
                native.turn_id,
                runs::conversation_turn_id(turn.from_cursor, turn.through_cursor)
            );
            assert_eq!(native.events.len(), 1);
            assert_eq!(
                native.events[0].input,
                serde_json::to_value(&expected.input).unwrap()
            );
            assert_eq!(
                native.events[0].actor,
                serde_json::to_value(&expected.actor).unwrap()
            );
            assert_eq!(
                native.history_snapshot.as_ref(),
                state.history.as_ref().map(|history| &history.snapshot)
            );
            let revision = native.revision + 1;
            network.accept(&pending, 100 + expected.sequence).await;
            network
                .checkpoint(&pending, &plan.namespace, revision)
                .await;
            network.finish(&pending).await;
        }
        let review_turn = network
            .conversation(&plan.namespace)
            .await
            .active_turn
            .unwrap();
        let review_run = network.run(&review_turn.run_id).await;
        let native_review = network.native_payload(&review_run);
        assert_eq!(
            native_review.events[0].input,
            serde_json::to_value(&terminal_event.input).unwrap(),
            "worker success reaches a native turn only as evidence"
        );
        network.accept(&review_run, 12).await;
        network
            .action(
                12,
                &review_run,
                "chief-review-needed",
                "pages",
                record(&plan, 1, "ready_for_review"),
            )
            .await;
        let old_attempt = network
            .checkpoint(&review_run, &plan.namespace, native_review.revision + 1)
            .await;
        network
            .submit(
                7,
                "capability",
                capability::CapabilityMsg::Announce {
                    capabilities: vec!["pi".into()],
                    resources: Default::default(),
                },
            )
            .await;
        network
            .submit(
                1,
                "runs",
                runs::RunsMsg::ReassignRun {
                    run_id: review_run.run_id.clone(),
                    attempt: 0,
                },
            )
            .await;
        assert!(
            network.try_submit(8, "runs", &old_attempt).await.is_err(),
            "lease reassignment fences even an exact checkpoint replay"
        );
        assert!(
            network
                .try_submit(
                    12,
                    "runs",
                    runs::RunsMsg::AgentAction {
                        run_id: review_run.run_id.clone(),
                        request_id: "stale-session-write".into(),
                        action: runs::ActionEnvelope::new(
                            runs::OP_SUBMIT,
                            Some(serde_json::json!({"module":"pages"})),
                            serde_json::to_value(record(&plan, 2, "accepted")).unwrap()
                        ),
                    }
                )
                .await
                .is_err(),
            "former attempt cannot make a Chief decision"
        );
        let pages::PageReply::Record(Some(reviewed)) = network
            .query(
                "pages",
                pages::PageQuery::Record {
                    page_id: plan.board_page_id.clone(),
                    record_id: TASK.into(),
                },
            )
            .await
        else {
            panic!("reviewed task")
        };
        assert_eq!(
            reviewed.data["status"], "ready_for_review",
            "only the current Chief decision landed, not the fenced acceptance"
        );
        // The trace is produced by the Host's execution authorization, never
        // by injecting Origin::Program into a test submission.
        for target in ["chat", "tasks", "pages", "runs"] {
            assert!(
                network
                    .trace
                    .iter()
                    .any(|entry| entry.module == target && entry.origin == Origin::Program(CHIEF)),
                "missing real Chief call to {target}"
            );
        }
        assert!(
            network
                .trace
                .iter()
                .any(|entry| entry.module == "runs" && entry.origin == Origin::Program(WORKER))
        );
    });
}

#[test]
fn current_controller_pause_resume_preserves_backlog_and_refuses_former_controller() {
    block_on(async {
        let mut network = Network::new().await;
        let plan = bootstrap(&mut network).await;
        network
            .submit(
                1,
                "identity",
                identity::IdentityMsg::TransferControl {
                    account: CHIEF,
                    to: 2,
                },
            )
            .await;
        let pause = runs::RunsMsg::ActivateConversation {
            conversation_id: plan.namespace.clone(),
            operation_id: "pause".into(),
            active: false,
        };
        assert!(network.try_submit(1, "runs", &pause).await.is_err());
        network.submit(2, "runs", &pause).await;
        network
            .post(
                1,
                &plan.channel_id,
                "paused-alice",
                "Wait for the controller",
            )
            .await;
        network
            .post(2, &plan.channel_id, "paused-bob", "Resume this request too")
            .await;
        network.drain().await;
        assert!(network.runs().await.is_empty());
        let paused = network.conversation(&plan.namespace).await;
        assert_ne!(paused.status, runs::ConversationStatus::Active);
        network = network.restart().await;
        assert_eq!(network.conversation(&plan.namespace).await, paused);
        let resume = runs::RunsMsg::ActivateConversation {
            conversation_id: plan.namespace.clone(),
            operation_id: "resume".into(),
            active: true,
        };
        assert!(network.try_submit(1, "runs", &resume).await.is_err());
        network.submit(2, "runs", &resume).await;
        network.drain().await;
        let active = network.conversation(&plan.namespace).await;
        assert_eq!(active.status, runs::ConversationStatus::Active);
        assert_eq!(active.admitted_cursor, 2);
        assert_eq!(network.runs().await.len(), 1);
        let frozen_events = network.events(&plan.namespace).await;
        network.submit(2, "runs", &resume).await;
        network.drain().await;
        assert_eq!(
            network.events(&plan.namespace).await,
            frozen_events,
            "replayed resume must not redeliver backlog"
        );
        assert_eq!(network.runs().await.len(), 1);
    });
}
