//! Native/guest parity for programmable model users: real account mentions,
//! deferred program calls, bounded context composition, isolated rejections,
//! leased interactive sessions and job finalization. Both hosts run the same
//! native siblings; only runs changes runtime. Every block compares events,
//! decoded queries and sibling roots. Runs roots use different physical layouts.
use agent::AgentModule;
use attribution::AttributionModule;
use capability::{CapabilityMsg, CapabilityRegistry};
use chat::{
    Block, Chat, ChatMsg, ChatQuery, ChatReply, Mark, Party, PostPolicy, Span,
    decode_reply as chat_decode_reply, encode_msg as chat_encode_msg,
    encode_query as chat_encode_query,
};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use dispatch::{
    DispatchModule, DispatchQuery, DispatchReply, DispatchStatus,
    decode_reply as dispatch_decode_reply, encode_query as dispatch_encode_query,
};
use files::Files;
use host::{BlockContext, Host, MemberOutcome, SubmitError};
use pages::Pages;
use runs::{ActionEnvelope, AgentResponse, OP_TASKS_CREATE, ReplyBlock, SkillRef, encode_response};
use runs::{
    RunsModule, RunsMsg, RunsQuery, RunsReply, decode_reply as runs_decode_reply, dispatch_id_for,
    encode_msg as runs_encode_msg, encode_query as runs_encode_query, reply_message_id,
};
use saga::{
    SagaMsg, SagaQuery, SagaReply, decode_reply as saga_decode_reply, decode_worker_request,
    encode_msg as saga_encode_msg, encode_query as saga_encode_query,
};
use sdk::{Error, Event, Msg, Origin, StateRoot};
use futures::executor::block_on;
use sdk::MerkleStore as _;
use sdk_testkit::MemStore;
use statesync::qmdb::QmdbStore;
use tasks::{JobsMsg, encode_job_msg as jobs_encode_msg};
use tasks::{
    TaskQuery, TaskReply, Tasks, decode_task_reply as tasks_decode_reply,
    encode_task_query as tasks_encode_query,
};
use valset::Valset;
use wasm_host::WasmModule;

/// GENERATED artifact — built from the `runs` module's guest port by
/// guest-builder (`make wasm-modules`); committed so this proof is self-contained.
const RUNS_WASM: &[u8] = include_bytes!("fixtures/runs.component.wasm");

/// the chain id both hosts run on — the genesis `__config` parameter the
/// composer seeds into this store tenant, and the network every `duck://`
/// link the injector renders names. Both sides take it, or the two disagree
/// on the `?net=` of a rendered page link.
const PARITY_CHAIN_ID: &str = "parity#d0cdf950";

fn wasm_runs() -> WasmModule {
    WasmModule::with_store("runs", RUNS_WASM, Box::new(seeded_runs_store()))
        .expect("load component")
}

/// exactly what `noded::compose` seeds a STORE-backed network-bound tenant
/// with at genesis (`seed_store_config`); without it the guest refuses every
/// dispatch. The record sits at the digest of `__config`, like every other
/// record in a merkle store.
fn seeded_runs_store() -> MemStore {
    let config = sdk::genesis_config::encode_config(&[
        ("chain_id", PARITY_CHAIN_ID.as_bytes()),
        (
            sdk::genesis_config::TIME_UNIT,
            sdk::genesis_config::TimeUnit::Height.encode(),
        ),
    ]);
    let mut store = MemStore::new();
    block_on(store.commit_batch(vec![(
        sdk::store_key(sdk::genesis_config::CONFIG_KEY),
        Some(config),
    )]))
    .expect("seed genesis config");
    store
}

/// the production wiring, verbatim (`bin/node/src/host_state.rs`) — the exact
/// constructor chain the guest compiles in.
fn native_runs() -> RunsModule {
    RunsModule::new(
        "runs",
        "chat",
        "saga",
        "attribution",
        "dispatch",
        "agent",
        Some("tasks".into()),
        Some("tasks".into()),
    )
    .with_files_module("files")
    .with_sink_forge("forge")
    .with_pages_module("pages")
    .with_collaboration_module("collaboration")
    .with_chain_id(PARITY_CHAIN_ID)
    .with_time_unit(sdk::genesis_config::TimeUnit::Height)
}

/// the shared native sibling set under the production ids. `chat_label` /
/// `pages_label` / `agent_label` keep the two hosts' qmdb runtime children
/// distinct; `files_dir` is each host's own (empty) odb dir — the files root
/// is content-derived, so the two empty instances agree from genesis.
async fn siblings(
    context: &deterministic::Context,
    chat_label: &'static str,
    pages_label: &'static str,
    agent_label: &'static str,
    files_dir: std::path::PathBuf,
    saga: saga::SagaModule,
    assignment_members: Option<&[Vec<u8>]>,
) -> Vec<Box<dyn sdk::Module>> {
    let chat_store = QmdbStore::init(context.child(chat_label), "chat").await;
    let pages_store = QmdbStore::init(context.child(pages_label), "pages").await;
    let agent_store = QmdbStore::init(context.child(agent_label), "agent").await;
    let mut modules: Vec<Box<dyn sdk::Module>> = vec![
        Box::new(
            Chat::new("chat", Box::new(chat_store))
                .with_identity("identity")
                .with_attribution("attribution"),
        ),
        Box::new(
            Pages::new("pages", Box::new(pages_store))
                .with_identity("identity")
                .with_attribution("attribution")
                .with_files("files"),
        ),
        Box::new(
            AttributionModule::new("attribution", Box::new(sdk_testkit::MemStore::new()))
                .with_subscribers(["agent"]),
        ),
        Box::new(identity::Identity::new(
            "identity",
            Box::new(sdk_testkit::MemStore::new()),
            PARITY_CHAIN_ID.into(),
        )),
        Box::new(saga),
        Box::new(DispatchModule::new(
            "dispatch",
            "saga",
            "identity",
            Box::new(sdk_testkit::MemStore::new()),
        )),
        Box::new(AgentModule::new(
            "agent",
            Box::new(agent_store),
            agent::Siblings {
                identity: "identity".into(),
                attribution: "attribution".into(),
                dispatch: "dispatch".into(),
            },
        )),
        Box::new(Tasks::new(
            "tasks",
            "identity",
            "attribution",
            Box::new(sdk_testkit::MemStore::new()),
        )),
        Box::new(Files::open("files", files_dir).expect("files open")),
    ];
    if let Some(members) = assignment_members {
        let mut valset = Valset::new(
            "valset",
            Box::new(sdk_testkit::MemStore::new()),
            "governance",
        );
        for member in members {
            valset.seed(member.clone()).await.expect("seed valset");
        }
        valset.finish_seed().await.expect("seed valset");
        modules.push(Box::new(valset));
        modules.push(Box::new(CapabilityRegistry::new(
            "capability",
            Box::new(sdk_testkit::MemStore::new()),
            Some("valset".into()),
        )));
    }
    modules
}

/// When assignment_members is given, saga is
/// wired with `with_assignment` (a real valset + capability registry,
/// seeded with those members) instead of the bare `new` — `Accept`'s
/// standing/capability gate needs a real valset to admit a claim against.
/// the lease policy stays `Open` regardless: this proof's oracle step
/// submits under a different origin than the assignee (see the session-lane
/// test), which only `Open` tolerates.
async fn native_host_with_assignment(
    context: &deterministic::Context,
    files_dir: std::path::PathBuf,
    assignment_members: Option<&[Vec<u8>]>,
) -> Host {
    let saga = match assignment_members {
        Some(_) => saga::SagaModule::with_assignment(
            "saga",
            Box::new(sdk_testkit::MemStore::new()),
            "valset",
            "capability",
            saga::LeasePolicy::Open,
        ),
        None => saga::SagaModule::new("saga", Box::new(sdk_testkit::MemStore::new())),
    };
    let mut modules = siblings(
        context,
        "native_chat",
        "native_pages",
        "native_agent",
        files_dir,
        saga,
        assignment_members,
    )
    .await;
    modules.push(Box::new(native_runs()));
    Host::genesis(modules).expect("genesis")
}

/// The guest host carries the same assignment registry as its native twin.
async fn wasm_host_with_assignment(
    context: &deterministic::Context,
    files_dir: std::path::PathBuf,
    assignment_members: Option<&[Vec<u8>]>,
) -> Host {
    let saga = match assignment_members {
        Some(_) => saga::SagaModule::with_assignment(
            "saga",
            Box::new(sdk_testkit::MemStore::new()),
            "valset",
            "capability",
            saga::LeasePolicy::Open,
        ),
        None => saga::SagaModule::new("saga", Box::new(sdk_testkit::MemStore::new())),
    };
    let mut modules = siblings(
        context,
        "wasm_chat",
        "wasm_pages",
        "wasm_agent",
        files_dir,
        saga,
        assignment_members,
    )
    .await;
    modules.push(Box::new(wasm_runs()));
    Host::genesis(modules).expect("genesis")
}

/// one block's agreed context. consensus_time == height, as on the real
/// validator network (runs stamps `created_at`/`opened_at` from it).
fn block(height: u64, origin: Origin) -> BlockContext {
    BlockContext {
        height,
        consensus_time: height,
        origin,
    }
}

fn alice() -> Origin {
    Origin::External(vec![0xA1; 32])
}

fn runs_op(m: &RunsMsg) -> Msg {
    Msg {
        target: "runs".into(),
        payload: runs_encode_msg(m),
    }
}

fn quackbot_ref() -> Party {
    Party::Account(2)
}

fn register_agent(account: u64, agent_id: &str, skills: Option<Vec<SkillRef>>) -> Msg {
    runs_op(&RunsMsg::ConfigureModel {
        operation: runs::ModelMsg::RegisterModel {
            account,
            agent_id: agent_id.into(),
            display_name: agent_id.into(),
            capability: "mock-llm-1".into(),
            recipe_hash: None,
            skills,
        },
    })
}

fn create_channel(channel_id: &str) -> Msg {
    Msg {
        target: "chat".into(),
        payload: chat_encode_msg(&ChatMsg::CreateChannel {
            channel_id: channel_id.into(),
            name: "General".into(),
            post_policy: PostPolicy::Open,
        }),
    }
}

/// a user post mentioning quackbot — what fires the engagement intake.
fn mention_post(channel_id: &str, message_id: &str) -> Msg {
    Msg {
        target: "chat".into(),
        payload: chat_encode_msg(&ChatMsg::PostMessage {
            channel_id: channel_id.into(),
            message_id: message_id.into(),
            blocks: vec![Block::Paragraph(vec![
                Span::plain("hey "),
                Span {
                    text: "@quackbot".into(),
                    marks: vec![Mark::Mention(quackbot_ref())],
                },
                Span::plain(" can you pick this up?"),
            ])],
            thread: None,
        }),
    }
}

/// a mention-free user post — an anchor message that engages nobody.
fn plain_post(channel_id: &str, message_id: &str) -> Msg {
    Msg {
        target: "chat".into(),
        payload: chat_encode_msg(&ChatMsg::PostMessage {
            channel_id: channel_id.into(),
            message_id: message_id.into(),
            blocks: vec![Block::paragraph("an anchor, nothing more")],
            thread: None,
        }),
    }
}

/// One all-policy trigger whose reference set consumes most of Runs' shared
/// sibling-read ledger on the first compose. Later composes may replay those
/// exact reads for free; only their agent and turn lookups are new.
fn reference_post(channel_id: &str, message_id: &str, accounts: std::ops::Range<u64>) -> Msg {
    let mut text = (0..50)
        .map(|index| format!("[page-{index}](duck://page/page-{index})"))
        .collect::<Vec<_>>()
        .join(" ");
    text.push_str(" [notes](duck://files/shared/attachments/u/notes.md)");
    Msg {
        target: "chat".into(),
        payload: chat_encode_msg(&ChatMsg::PostMessage {
            channel_id: channel_id.into(),
            message_id: message_id.into(),
            blocks: vec![Block::Paragraph(vec![Span {
                text,
                marks: accounts
                    .map(|account| Mark::Mention(Party::Account(account)))
                    .collect(),
            }])],
            thread: None,
        }),
    }
}

/// the minimal host-assembled runner-result wrapper the oracle ALWAYS
/// delivers around the model's raw text (the collaboration-loop shape).
fn wrap_runner(prose: Vec<u8>) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "ducktape_runner_result": 1,
        "response_text": String::from_utf8(prose).expect("utf-8 prose"),
        "workspace_receipt": {
            "source_prefix": "/shared/agent-workspaces/bot",
            "output_snapshot": null,
            "commit_height": null,
            "rebased": false,
            "no_changes": true
        }
    }))
    .expect("wrapper serializes")
}

// ---- catalog envelopes ---------------------------------------------------------

fn text_content(text: &str) -> serde_json::Value {
    serde_json::json!({"content": [{"type": "text", "text": text}]})
}

fn reply(text: &str) -> ActionEnvelope {
    ActionEnvelope::new(runs::OP_REPLY, None, text_content(text))
}

fn create_task(task_id: &str, title: &str) -> ActionEnvelope {
    ActionEnvelope::new(
        OP_TASKS_CREATE,
        None,
        serde_json::json!({"task_id": task_id, "title": title}),
    )
}

fn set_page_checked(block_id: &str, checked: bool) -> ActionEnvelope {
    ActionEnvelope::new(
        runs::OP_PAGES_SET_CHECKED,
        Some(serde_json::json!({"block_id": block_id})),
        serde_json::json!({"checked": checked}),
    )
}

/// the model's RAW text: a strict AgentResponse JSON the in-consensus
/// normalization accepts as-is — one reply paragraph + one task action.
fn canned_response(run_id: &str) -> Vec<u8> {
    encode_response(&AgentResponse {
        reply_blocks: vec![ReplyBlock {
            kind: "paragraph".into(),
            text: format!("quack: handling {run_id}"),
            lang: None,
        }],
        actions: vec![create_task("task-1", "follow up on the mention")],
        commit_message: None,
    })
}

async fn dispatch_saga_id(h: &Host, run_id: &str) -> String {
    let reply = h
        .query(
            "dispatch",
            &dispatch_encode_query(&DispatchQuery::Dispatch {
                receiver: "runs".into(),
                dispatch_id: dispatch_id_for(run_id),
            }),
        )
        .await
        .expect("dispatch query");
    match dispatch_decode_reply(&reply).expect("decode") {
        DispatchReply::Dispatch(Some(view)) => match view.status {
            DispatchStatus::AwaitingResult { saga_id } => saga_id,
            other => panic!("the run's dispatch must await its saga, got {other:?}"),
        },
        other => panic!("expected the run's dispatch, got {other:?}"),
    }
}

/// the run's committed lease-holder — saga's `assignee`, read directly off the
/// saga the dispatch names (identical on both hosts).
async fn dispatch_assignee(h: &Host, run_id: &str) -> Option<Vec<u8>> {
    let saga_id = dispatch_saga_id(h, run_id).await;
    let reply = h
        .query("saga", &saga_encode_query(&SagaQuery::Get { saga_id }))
        .await
        .expect("saga query");
    match saga_decode_reply(&reply).expect("decode") {
        SagaReply::Saga(view) => view.and_then(|v| v.assignee),
        other => panic!("expected the run's saga, got {other:?}"),
    }
}

async fn oracle_op(h: &Host, run_id: &str, raw: Vec<u8>) -> Msg {
    let saga_id = dispatch_saga_id(h, run_id).await;
    let bytes = h
        .query(
            "saga",
            &saga_encode_query(&SagaQuery::Get {
                saga_id: saga_id.clone(),
            }),
        )
        .await
        .unwrap();
    let SagaReply::Saga(Some(saga)) = saga_decode_reply(&bytes).unwrap() else {
        panic!("the run must have a saga");
    };
    Msg {
        target: "saga".into(),
        payload: saga_encode_msg(&SagaMsg::OracleResult {
            saga_id,
            attempt: saga.attempt,
            outcome: Ok(raw),
            usage: None,
        }),
    }
}

async fn chat_message(h: &Host, message_id: &str) -> Option<chat::MessageView> {
    let reply = h
        .query(
            "chat",
            &chat_encode_query(&ChatQuery::Message {
                message_id: message_id.into(),
            }),
        )
        .await
        .expect("chat query");
    match chat_decode_reply(&reply).expect("decode") {
        ChatReply::Message(view) => view,
        other => panic!("unexpected chat reply: {other:?}"),
    }
}

async fn task_ids(h: &Host) -> Vec<String> {
    let reply = h
        .query(
            "tasks",
            &tasks_encode_query(&TaskQuery::List {
                limit: tasks::MAX_LIST_LIMIT,
                after: None,
            }),
        )
        .await
        .expect("tasks query");
    let TaskReply::Tasks(tasks) = tasks_decode_reply(&reply).expect("decode") else {
        panic!("a list answers a page");
    };
    tasks.into_iter().map(|t| t.id).collect()
}

async fn recent_runs(h: &Host) -> Vec<runs::RunRecord> {
    let reply = h
        .query("runs", &runs_encode_query(&RunsQuery::RecentRuns))
        .await
        .expect("runs query");
    match runs_decode_reply(&reply).expect("decode") {
        RunsReply::RecentRuns(records) => records,
        other => panic!("unexpected runs reply: {other:?}"),
    }
}

async fn pending_run_ids(h: &Host) -> Vec<String> {
    let reply = h
        .query("runs", &runs_encode_query(&RunsQuery::PendingRuns))
        .await
        .expect("runs query");
    match runs_decode_reply(&reply).expect("decode") {
        RunsReply::PendingRuns(runs) => runs.into_iter().map(|p| p.run_id).collect(),
        other => panic!("unexpected runs reply: {other:?}"),
    }
}

async fn agent_sessions(h: &Host) -> Vec<runs::AgentSession> {
    let reply = h
        .query("runs", &runs_encode_query(&RunsQuery::AgentSessions))
        .await
        .expect("runs query");
    match runs_decode_reply(&reply).expect("decode") {
        RunsReply::AgentSessions(sessions) => sessions,
        other => panic!("unexpected runs reply: {other:?}"),
    }
}

const SIBLINGS: &[&str] = &[
    "identity",
    "chat",
    "pages",
    "attribution",
    "saga",
    "dispatch",
    "agent",
    "tasks",
    "files",
    "valset",
    "capability",
];
const SESSION_KEY: [u8; 32] = [0x11; 32];
const WORKER_NODE: [u8; 32] = [0x77; 32];

macro_rules! op {
    ($target:expr, $value:expr $(,)?) => {
        Msg {
            target: $target.into(),
            payload: sdk::wire::encode($value),
        }
    };
}

fn request(agent_id: &str, channel_id: &str, anchor_seq: u64) -> Msg {
    runs_op(&RunsMsg::RequestRun {
        agent_id: agent_id.into(),
        channel_id: channel_id.into(),
        anchor_seq,
        demands: Default::default(),
        skills: Vec::new(),
    })
}

fn event_tuples(events: &[Event]) -> Vec<(String, Vec<u8>)> {
    events
        .iter()
        .map(|event| (event.source.clone(), event.payload.clone()))
        .collect()
}

async fn replies(host: &Host) -> Vec<Vec<u8>> {
    let mut replies = Vec::new();
    for query in [
        RunsQuery::PendingRuns,
        RunsQuery::AgentSessions,
        RunsQuery::RecentRuns,
        RunsQuery::Model {
            query: runs::ModelQuery::Agents,
        },
    ] {
        replies.push(
            host.query("runs", &runs_encode_query(&query))
                .await
                .unwrap(),
        );
    }
    replies
}

#[test]
fn inline_page_and_block_mentions_preserve_source_and_program_reply_parity() {
    let directory = tempfile::tempdir().unwrap();
    deterministic::Runner::default().start(|context| async move {
        let mut pair = Pair::new(&context, directory.path()).await;
        pair.provision(2, "quackbot").await;
        pair.submit(
            alice(),
            op!(
                "pages",
                &pages::PageMsg::CreatePage {
                    page_id: "inline".into(),
                    title: "Quackbot review this".into(),
                    blocks: Vec::new(),
                }
            ),
        )
        .await;
        pair.submit(
            alice(),
            op!(
                "pages",
                &pages::PageMsg::InsertBlock {
                    parent: "inline".into(),
                    after: None,
                    block: pages::NewBlock {
                        id: "inline-todo".into(),
                        kind: pages::BlockKind::Todo,
                        text: "Quackbot review the todo".into(),
                        marks: Vec::new()
                    },
                }
            ),
        )
        .await;
        pair.drain().await;
        for target in ["inline", "inline-todo"] {
            let checked = target == "inline";
            pair.submit(
                alice(),
                op!(
                    "pages",
                    &pages::PageMsg::SetSpanMark {
                        block_id: target.into(),
                        start: 0,
                        end: 8,
                        kind: pages::InlineMark::Mention(2),
                        active: true,
                    }
                ),
            )
            .await;
            assert!(pending_run_ids(&pair.native).await.is_empty());
            pair.drain().await;
            let pending = pending_run_ids(&pair.native).await;
            assert_eq!(pending.len(), 1);
            let run = &pending[0];
            pair.accept(run).await;
            pair.submit(
                Origin::External(WORKER_NODE.to_vec()),
                runs_op(&RunsMsg::OpenAgentSession {
                    attempt: 0,
                    run_id: run.clone(),
                    session_key: SESSION_KEY.to_vec(),
                }),
            )
            .await;
            pair.submit(
                Origin::External(SESSION_KEY.to_vec()),
                runs_op(&RunsMsg::AgentAction {
                    run_id: run.clone(),
                    request_id: "tick".into(),
                    action: set_page_checked("inline-todo", checked),
                }),
            )
            .await;
            pair.drain().await;
            assert!(matches!(
                pair.action(&runs::action_request_id(run, "tick"))
                    .await
                    .status,
                runs::ActionStatus::Completed {
                    outcome: dispatch::CallOutcomeSummary::Applied { .. },
                    ..
                }
            ));
            pair.submit(
                Origin::External(SESSION_KEY.to_vec()),
                runs_op(&RunsMsg::AgentAction {
                    run_id: run.clone(),
                    request_id: "underway".into(),
                    action: reply("Review underway."),
                }),
            )
            .await;
            pair.drain().await;
            pair.settle(run, b"Reviewed this inline mention.".to_vec())
                .await;
            let query = pages::encode_query(&pages::PageQuery::CommentThread {
                thread_id: format!("agent/{}/thread/reply", dispatch_id_for(run)),
            });
            let native = pair.native.query("pages", &query).await.unwrap();
            let wasm = pair.wasm.query("pages", &query).await.unwrap();
            assert_eq!(native, wasm);
            let pages::PageReply::CommentThread(Some(thread)) = pages::decode_reply(&wasm).unwrap()
            else {
                panic!("actual program reply");
            };
            assert_eq!(thread.thread.target, target);
            assert_eq!(thread.thread.opener, pages::Party::Account(2));
            assert_eq!(thread.comments[0].author, pages::Party::Account(2));
            assert_eq!(thread.comments[0].text, "Review underway.");
            assert_eq!(thread.comments[1].text, "Reviewed this inline mention.");
            assert_eq!(thread.comments[1].author, pages::Party::Account(2));
            let query = pages::encode_query(&pages::PageQuery::GetBlock {
                block_id: "inline-todo".into(),
            });
            let bytes = pair.wasm.query("pages", &query).await.unwrap();
            let pages::PageReply::Block(Some(todo)) = pages::decode_reply(&bytes).unwrap() else {
                panic!("source todo");
            };
            assert_eq!(todo.author, pages::Party::Account(1));
            assert_eq!(todo.checked, checked);
            assert!(pending_run_ids(&pair.native).await.is_empty());
        }
    });
}

fn root_of(host: &Host) -> StateRoot {
    host.module_root("runs").unwrap()
}

struct Pair {
    native: Host,
    wasm: Host,
    height: u64,
    requests: Vec<saga::WorkerRequest>,
}
impl Pair {
    async fn new(context: &deterministic::Context, directory: &std::path::Path) -> Self {
        let members = [WORKER_NODE.to_vec()];
        let native =
            native_host_with_assignment(context, directory.join("native"), Some(&members)).await;
        let wasm = wasm_host_with_assignment(context, directory.join("wasm"), Some(&members)).await;
        let mut pair = Self {
            native,
            wasm,
            height: 0,
            requests: Vec::new(),
        };
        pair.submit(
            alice(),
            op!(
                "identity",
                &identity::IdentityMsg::Create {
                    name: "Alice".into(),
                    scheme: identity::KeyScheme::Ed25519,
                },
            ),
        )
        .await;
        pair.submit(
            Origin::External(WORKER_NODE.to_vec()),
            op!(
                "capability",
                &CapabilityMsg::Announce {
                    capabilities: vec!["mock-llm-1".into()],
                    resources: Default::default(),
                },
            ),
        )
        .await;
        pair.submit(alice(), create_channel("general")).await;
        pair
    }
    async fn parity(&self) {
        assert_eq!(
            replies(&self.native).await,
            replies(&self.wasm).await,
            "queries at {}",
            self.height
        );
        for sibling in SIBLINGS {
            assert_eq!(
                self.native.module_root(sibling),
                self.wasm.module_root(sibling),
                "{sibling} at {}",
                self.height
            );
        }
        assert_ne!(
            root_of(&self.native),
            root_of(&self.wasm),
            "physical storage differs"
        );
    }
    async fn batch(&mut self, messages: Vec<(Origin, Msg)>) -> Vec<MemberOutcome> {
        self.height += 1;
        let native_before = root_of(&self.native);
        let wasm_before = root_of(&self.wasm);
        let native = self
            .native
            .submit_block(block(self.height, Origin::System), messages.clone())
            .await
            .unwrap();
        let wasm = self
            .wasm
            .submit_block(block(self.height, Origin::System), messages)
            .await
            .unwrap();
        assert_eq!(
            event_tuples(&native.events),
            event_tuples(&wasm.events),
            "events at {}",
            self.height
        );
        assert_eq!(
            format!("{:?}", native.members),
            format!("{:?}", wasm.members),
            "members at {}",
            self.height
        );
        self.requests.extend(
            native
                .events
                .iter()
                .filter_map(|event| decode_worker_request(&event.payload).ok()),
        );
        self.parity().await;
        assert_eq!(
            root_of(&self.native) != native_before,
            root_of(&self.wasm) != wasm_before,
            "root movement at {}",
            self.height
        );
        native.members
    }
    async fn submit(&mut self, origin: Origin, message: Msg) {
        let outcomes = self.batch(vec![(origin, message)]).await;
        assert!(
            matches!(outcomes[0], MemberOutcome::Applied { .. }),
            "{outcomes:?}"
        );
    }
    async fn drain(&mut self) {
        // Advance only while committed work exists; every iteration consumes
        // a real host queue batch, with no wall-clock polling.
        while self.native.has_pending_work().await.unwrap() {
            assert!(self.wasm.has_pending_work().await.unwrap());
            self.batch(Vec::new()).await;
        }
        assert!(!self.wasm.has_pending_work().await.unwrap());
    }
    async fn provision(&mut self, account: u64, id: &str) {
        self.submit(
            alice(),
            op!(
                "agent",
                &agent::AgentMsg::Provision {
                    request_id: id.into(),
                    name: id.into(),
                    program: runs::model_program(id),
                },
            ),
        )
        .await;
        self.submit(alice(), register_agent(account, id, None))
            .await;
    }
    async fn mention_run(&mut self) -> String {
        self.submit(alice(), mention_post("general", "mention"))
            .await;
        assert!(
            pending_run_ids(&self.native).await.is_empty(),
            "source commits before its reaction"
        );
        self.drain().await;
        let runs = pending_run_ids(&self.native).await;
        assert_eq!(runs.len(), 1);
        runs[0].clone()
    }
    async fn accept(&mut self, run: &str) {
        let saga_id = dispatch_saga_id(&self.native, run).await;
        self.submit(
            Origin::External(WORKER_NODE.to_vec()),
            op!(
                "saga",
                &SagaMsg::Accept {
                    saga_id,
                    attempt: 0,
                },
            ),
        )
        .await;
    }
    async fn settle(&mut self, run: &str, response: Vec<u8>) {
        let oracle = oracle_op(&self.native, run, wrap_runner(response)).await;
        self.submit(Origin::External(WORKER_NODE.to_vec()), oracle)
            .await;
        self.drain().await;
    }
    async fn rejected(&mut self, origin: Origin, message: Msg, needle: &str) {
        let before = (root_of(&self.native), root_of(&self.wasm));
        self.height += 1;
        let n = self
            .native
            .submit_at(block(self.height, origin.clone()), message.clone())
            .await
            .unwrap_err();
        let w = self
            .wasm
            .submit_at(block(self.height, origin), message)
            .await
            .unwrap_err();
        let SubmitError::Rejected(Error::Module(n)) = n else {
            panic!("{n:?}");
        };
        let SubmitError::Rejected(Error::Module(w)) = w else {
            panic!("{w:?}");
        };
        assert!(n.contains(needle), "{n}");
        assert!(w.contains(needle), "{w}");
        assert_eq!(before, (root_of(&self.native), root_of(&self.wasm)));
        self.parity().await;
    }
    async fn action(&self, id: &str) -> runs::ActionRequestView {
        let query = runs_encode_query(&RunsQuery::ActionRequest {
            request_id: id.into(),
        });
        let native = self.native.query("runs", &query).await.unwrap();
        let wasm = self.wasm.query("runs", &query).await.unwrap();
        assert_eq!(native, wasm);
        let RunsReply::ActionRequest(Some(view)) = runs_decode_reply(&native).unwrap() else {
            panic!("action receipt");
        };
        view
    }
}

async fn verify_receipt_snapshots(pair: &Pair, request_id: &str) {
    use sdk::Module as _;
    let capture = |host: &Host| {
        let (snapshot, _) =
            host.capture_current_snapshot(pair.height, host::CapturePayloads::All, || {
                std::time::Duration::ZERO
            });
        let module = snapshot.module("runs").unwrap();
        let sdk::StateSyncHandle::SnapshotBytes(bytes) = &module.state_sync else {
            panic!("runs snapshot");
        };
        (bytes.clone(), module.root)
    };
    let (bytes, root) = capture(&pair.native);
    let mut native = native_runs();
    native.install(&bytes, root).unwrap();
    let (bytes, root) = capture(&pair.wasm);
    let mut wasm = wasm_runs();
    wasm.install(&bytes, root).unwrap();
    let query = runs_encode_query(&RunsQuery::ActionPlan {
        request_id: request_id.into(),
    });
    assert_eq!(
        native.query(&query).await.unwrap(),
        pair.native.query("runs", &query).await.unwrap()
    );
    assert_eq!(
        wasm.query(&query).await.unwrap(),
        pair.wasm.query("runs", &query).await.unwrap()
    );
    assert_eq!(
        native.pending_items().await.unwrap(),
        wasm.pending_items().await.unwrap()
    );
}

#[test]
fn the_collaboration_loop_lands_identically_on_both_runtimes() {
    let directory = tempfile::tempdir().unwrap();
    deterministic::Runner::default().start(|context| async move {
        let mut pair = Pair::new(&context, directory.path()).await;
        pair.provision(2, "quackbot").await;
        let run = pair.mention_run().await;
        assert_eq!(pair.requests.len(), 1, "one real work request");
        pair.accept(&run).await;
        let oracle = oracle_op(&pair.native, &run, wrap_runner(canned_response(&run))).await;
        pair.submit(Origin::External(WORKER_NODE.to_vec()), oracle)
            .await;
        assert!(
            chat_message(&pair.wasm, &reply_message_id(&run))
                .await
                .is_none()
        );
        pair.drain().await;
        assert!(pending_run_ids(&pair.wasm).await.is_empty());
        let reply = chat_message(&pair.wasm, &reply_message_id(&run))
            .await
            .unwrap();
        assert_eq!(reply.head.author, quackbot_ref());
        assert_eq!(reply.head.origin, Origin::Program(2));
        assert_eq!(
            reply.head.blocks,
            vec![Block::paragraph(format!("quack: handling {run}"))]
        );
        assert_eq!(task_ids(&pair.wasm).await, vec!["task-1"]);
        assert_eq!(
            recent_runs(&pair.native).await,
            recent_runs(&pair.wasm).await
        );
        assert_eq!(recent_runs(&pair.wasm).await[0].run_id, run);
        let settled = root_of(&pair.wasm);
        replies(&pair.wasm).await;
        assert_eq!(root_of(&pair.wasm), settled, "queries do not write");

        // The model result can be accepted before its separately authorized
        // reply executes. A competing committed post then makes that call fail.
        pair.submit(alice(), plain_post("general", "manual-after-mention"))
            .await;
        pair.submit(alice(), request("quackbot", "general", 3))
            .await;
        pair.drain().await;
        let manual = pending_run_ids(&pair.wasm).await.pop().unwrap();
        assert_eq!(manual, runs::run_id_for("general", 3, "quackbot"));
        pair.accept(&manual).await;
        let response = encode_response(&AgentResponse {
            reply_blocks: vec![ReplyBlock {
                kind: "paragraph".into(),
                text: "answer".into(),
                lang: None,
            }],
            actions: Vec::new(),
            commit_message: None,
        });
        let oracle = oracle_op(&pair.native, &manual, wrap_runner(response)).await;
        pair.submit(Origin::External(WORKER_NODE.to_vec()), oracle)
            .await;
        while !recent_runs(&pair.wasm)
            .await
            .iter()
            .any(|record| record.run_id == manual)
        {
            assert!(pair.wasm.has_pending_work().await.unwrap());
            pair.batch(Vec::new()).await;
        }
        assert_eq!(
            recent_runs(&pair.wasm).await[0].outcome,
            runs::RunOutcome::ResultAccepted
        );
        pair.submit(alice(), plain_post("general", &reply_message_id(&manual)))
            .await;
        pair.drain().await;
        let receipt = pair
            .action(&format!("result/{}/0", dispatch_id_for(&manual)))
            .await;
        let runs::ActionStatus::Completed {
            outcome: dispatch::CallOutcomeSummary::Rejected { reason },
            ..
        } = receipt.status
        else {
            panic!("the actual target must reject the occupied reply id: {receipt:?}");
        };
        assert!(!reason.is_empty());
        assert_eq!(
            recent_runs(&pair.wasm).await[0].outcome,
            runs::RunOutcome::ActionRejected
        );
        assert_eq!(
            chat_message(&pair.wasm, &reply_message_id(&manual))
                .await
                .unwrap()
                .head
                .author,
            Party::Account(1)
        );
    });
}

#[test]
fn multiple_programs_compose_bounded_references_inside_the_real_wasm_budget() {
    let directory = tempfile::tempdir().unwrap();
    deterministic::Runner::default().start(|context| async move {
        let mut pair = Pair::new(&context, directory.path()).await;
        for index in 0..20 {
            pair.provision(index + 2, &format!("bot-{index:02}")).await;
        }
        pair.submit(alice(), reference_post("general", "bounded", 2..22))
            .await;
        pair.drain().await;
        // Each account runs its own bounded program invocation. The removed
        // channel watch no longer shares one implicit compose across models.
        assert_eq!(pair.requests.len(), 20);
        assert_eq!(pending_run_ids(&pair.wasm).await.len(), 20);
        assert_eq!(
            pending_run_ids(&pair.native).await,
            pending_run_ids(&pair.wasm).await
        );
    });
}

#[test]
fn rejections_match_and_leave_no_trace() {
    let directory = tempfile::tempdir().unwrap();
    deterministic::Runner::default().start(|context| async move {
        let mut pair = Pair::new(&context, directory.path()).await;
        pair.provision(2, "quackbot").await;
        let rejects = [
            (
                alice(),
                request("ghost", "general", 1),
                "unknown agent: ghost",
            ),
            (
                alice(),
                request("quackbot", "bad\u{1f}channel", 1),
                "reserved unit separator",
            ),
            (
                Origin::External(Vec::new()),
                request("ghost", "general", 1),
                "non-empty submitter",
            ),
            (
                alice(),
                runs_op(&RunsMsg::CancelRun {
                    run_id: "nope".into(),
                }),
                "unknown run: nope",
            ),
            (
                alice(),
                runs_op(&RunsMsg::OpenAgentSession {
                    attempt: 0,
                    run_id: "nope".into(),
                    session_key: vec![7; 8],
                }),
                "32 bytes",
            ),
            (
                Origin::System,
                runs_op(&RunsMsg::OpenAgentSession {
                    attempt: 0,
                    run_id: "nope".into(),
                    session_key: vec![7; 32],
                }),
                "only the node",
            ),
            (
                alice(),
                runs_op(&RunsMsg::OpenAgentSession {
                    attempt: 0,
                    run_id: "nope".into(),
                    session_key: vec![7; 32],
                }),
                "not in flight",
            ),
            (
                alice(),
                runs_op(&RunsMsg::AgentAction {
                    run_id: "nope".into(),
                    request_id: "t".into(),
                    action: create_task("t", "t"),
                }),
                "run is not in flight",
            ),
            (
                alice(),
                runs_op(&RunsMsg::ClaimActionRequest {
                    request_id: "unknown".into(),
                    target_step: 1,
                }),
                "unknown action request",
            ),
            (
                alice(),
                Msg {
                    target: "runs".into(),
                    payload: b"definitely-not-json".to_vec(),
                },
                "expected value",
            ),
        ];
        for (origin, message, reason) in rejects {
            pair.rejected(origin, message, reason).await;
        }
    });
}

#[test]
fn multi_dispatch_reads_prior_writes_and_isolates_rejected_control_and_receipts() {
    let directory = tempfile::tempdir().unwrap();
    deterministic::Runner::default().start(|context| async move {
        let mut pair = Pair::new(&context, directory.path()).await;
        pair.provision(2, "quackbot").await;
        // Program calls are authenticated by the host seam. Two identical
        // calls in a block must observe the first call's staged turn claim.
        pair.submit(alice(), plain_post("general", "anchor")).await;
        let call = request("quackbot", "general", 1);
        let outcomes = pair
            .batch(vec![
                (Origin::Program(2), call.clone()),
                (Origin::Program(2), call),
            ])
            .await;
        assert!(
            outcomes
                .iter()
                .all(|outcome| matches!(outcome, MemberOutcome::Applied { .. }))
        );
        assert_eq!(pending_run_ids(&pair.wasm).await.len(), 1);
        let run = pending_run_ids(&pair.wasm).await.pop().unwrap();
        pair.accept(&run).await;
        pair.submit(
            Origin::External(WORKER_NODE.to_vec()),
            runs_op(&RunsMsg::OpenAgentSession {
                attempt: 0,
                run_id: run.clone(),
                session_key: SESSION_KEY.to_vec(),
            }),
        )
        .await;
        let action = |id: &str| {
            runs_op(&RunsMsg::AgentAction {
                run_id: run.clone(),
                request_id: id.into(),
                action: create_task(id, id),
            })
        };
        let outcomes = pair
            .batch(vec![
                (Origin::External(SESSION_KEY.to_vec()), action("first")),
                (alice(), action("forged")),
                (Origin::External(SESSION_KEY.to_vec()), action("second")),
            ])
            .await;
        assert!(matches!(outcomes[0], MemberOutcome::Applied { .. }));
        assert!(matches!(outcomes[1], MemberOutcome::Rejected { .. }));
        assert!(matches!(outcomes[2], MemberOutcome::Applied { .. }));
        assert_eq!(agent_sessions(&pair.wasm).await[0].actions, 2);
        assert!(
            task_ids(&pair.wasm).await.is_empty(),
            "admission precedes program writes"
        );
        pair.drain().await;
        assert_eq!(task_ids(&pair.wasm).await, vec!["first", "second"]);
        for id in ["first", "second"] {
            assert!(matches!(
                pair.action(&runs::action_request_id(&run, id)).await.status,
                runs::ActionStatus::Completed {
                    outcome: dispatch::CallOutcomeSummary::Applied { .. },
                    ..
                }
            ));
        }

        pair.submit(alice(), plain_post("general", "manual-anchor"))
            .await;
        let caller = Origin::External(vec![3; 32]);
        let manual = request("quackbot", "general", 2);
        let outcomes = pair
            .batch(vec![
                (caller.clone(), manual.clone()),
                (caller.clone(), manual),
            ])
            .await;
        assert!(
            outcomes
                .iter()
                .all(|outcome| matches!(outcome, MemberOutcome::Applied { .. }))
        );
        pair.drain().await;
        let manual_id = runs::run_id_for("general", 2, "quackbot");
        let bytes = pair
            .wasm
            .query("runs", &runs_encode_query(&RunsQuery::PendingRuns))
            .await
            .unwrap();
        let RunsReply::PendingRuns(pending) = runs_decode_reply(&bytes).unwrap() else {
            panic!("pending");
        };
        let requested: Vec<_> = pending
            .iter()
            .filter(|run| run.run_id == manual_id)
            .collect();
        assert_eq!(requested.len(), 1);
        assert_eq!(requested[0].requester, caller);
        pair.submit(
            caller,
            runs_op(&RunsMsg::CancelRun {
                run_id: manual_id.clone(),
            }),
        )
        .await;
        pair.drain().await;
        assert!(!pending_run_ids(&pair.wasm).await.contains(&manual_id));
    });
}

#[test]
fn the_session_lane_matches_lease_budget_and_close_out() {
    let directory = tempfile::tempdir().unwrap();
    deterministic::Runner::default().start(|context| async move {
        let mut pair = Pair::new(&context, directory.path()).await;
        pair.provision(2, "quackbot").await;
        pair.submit(alice(), plain_post("general", "manual-anchor"))
            .await;
        pair.submit(alice(), request("quackbot", "general", 1))
            .await;
        pair.drain().await;
        let run = pending_run_ids(&pair.wasm).await.pop().unwrap();
        assert_eq!(run, runs::run_id_for("general", 1, "quackbot"));
        assert!(run.contains('\u{1f}'));
        pair.accept(&run).await;
        assert_eq!(
            dispatch_assignee(&pair.wasm, &run).await,
            Some(WORKER_NODE.to_vec())
        );
        let open = runs_op(&RunsMsg::OpenAgentSession {
            attempt: 0,
            run_id: run.clone(),
            session_key: SESSION_KEY.to_vec(),
        });
        pair.rejected(alice(), open.clone(), "lease").await;
        pair.submit(Origin::External(WORKER_NODE.to_vec()), open.clone())
            .await;
        pair.submit(Origin::External(WORKER_NODE.to_vec()), open)
            .await;
        pair.rejected(
            Origin::External(WORKER_NODE.to_vec()),
            runs_op(&RunsMsg::OpenAgentSession {
                attempt: 0,
                run_id: run.clone(),
                session_key: vec![0x66; 32],
            }),
            "already has an open",
        )
        .await;
        let post = runs_op(&RunsMsg::AgentAction {
            run_id: run.clone(),
            request_id: "working".into(),
            action: reply("working"),
        });
        pair.rejected(
            Origin::External(WORKER_NODE.to_vec()),
            post.clone(),
            "only the bound session key",
        )
        .await;
        pair.submit(Origin::External(SESSION_KEY.to_vec()), post)
            .await;
        assert_eq!(agent_sessions(&pair.wasm).await[0].actions, 1);
        assert!(matches!(
            pair.action(&runs::action_request_id(&run, "working"))
                .await
                .status,
            runs::ActionStatus::AwaitingProgram
        ));
        pair.drain().await;
        let receipt = pair.action(&runs::action_request_id(&run, "working")).await;
        assert!(matches!(
            receipt.status,
            runs::ActionStatus::Completed {
                outcome: dispatch::CallOutcomeSummary::Applied { .. },
                ..
            }
        ));
        let message = chat_message(&pair.wasm, &runs::post_message_id(&run, "s0"))
            .await
            .unwrap();
        assert_eq!(message.head.author, Party::Account(2));
        assert_eq!(message.head.origin, Origin::Program(2));
        assert_eq!(message.head.thread, Some(1));
        verify_receipt_snapshots(&pair, &runs::action_request_id(&run, "working")).await;
        // An admitted action cannot borrow the next attempt's lease, even
        // when the same node wins it. Completed receipts remain reportable,
        // and the rejected proposal keeps its request_id: the retry under the
        // new lease is new work with a new key.
        let reply_action = runs_op(&RunsMsg::AgentAction {
            run_id: run.clone(),
            request_id: "next".into(),
            action: reply("next update"),
        });
        let retry_action = runs_op(&RunsMsg::AgentAction {
            run_id: run.clone(),
            request_id: "next-retry".into(),
            action: reply("next update"),
        });
        pair.submit(Origin::External(SESSION_KEY.to_vec()), reply_action.clone())
            .await;
        let saga_id = dispatch_saga_id(&pair.native, &run).await;
        pair.submit(
            Origin::External(WORKER_NODE.to_vec()),
            op!(
                "saga",
                &SagaMsg::OracleResult {
                    saga_id: saga_id.clone(),
                    attempt: 0,
                    outcome: Err("interrupted provider".into()),
                    usage: None,
                }
            ),
        )
        .await;
        pair.submit(
            Origin::External(WORKER_NODE.to_vec()),
            op!(
                "saga",
                &SagaMsg::Accept {
                    saga_id,
                    attempt: 1,
                }
            ),
        )
        .await;
        pair.rejected(
            Origin::External(SESSION_KEY.to_vec()),
            reply_action.clone(),
            "lease has moved",
        )
        .await;
        pair.submit(
            Origin::External(WORKER_NODE.to_vec()),
            runs_op(&RunsMsg::OpenAgentSession {
                run_id: run.clone(),
                attempt: 1,
                session_key: vec![0x77; 32],
            }),
        )
        .await;
        assert_eq!(agent_sessions(&pair.wasm).await[0].actions, 2);
        assert_eq!(agent_sessions(&pair.wasm).await[0].lease.attempt, 1);
        pair.submit(Origin::External(vec![0x77; 32]), retry_action)
            .await;
        pair.drain().await;
        assert!(matches!(
            pair.action(&runs::action_request_id(&run, "next"))
                .await
                .status,
            runs::ActionStatus::Rejected { .. }
        ));
        assert!(
            chat_message(&pair.wasm, &runs::post_message_id(&run, "s1"))
                .await
                .is_none()
        );
        let retried = chat_message(&pair.wasm, &runs::post_message_id(&run, "s2"))
            .await
            .unwrap();
        assert_eq!(retried.head.author, Party::Account(2));
        assert_eq!(retried.head.thread, Some(1));
        verify_receipt_snapshots(&pair, &runs::action_request_id(&run, "next-retry")).await;
        pair.settle(&run, canned_response(&run)).await;
        let reply = chat_message(&pair.wasm, &reply_message_id(&run))
            .await
            .unwrap();
        assert_eq!(reply.head.author, Party::Account(2));
        assert_eq!(reply.head.origin, Origin::Program(2));
        assert_eq!(reply.head.thread, Some(1));
        assert_eq!(
            recent_runs(&pair.wasm).await[0].outcome,
            runs::RunOutcome::ResultAccepted
        );
        assert!(agent_sessions(&pair.wasm).await.is_empty());
        pair.rejected(
            Origin::External(SESSION_KEY.to_vec()),
            runs_op(&RunsMsg::AgentAction {
                run_id: run,
                request_id: "late".into(),
                action: create_task("late", "late"),
            }),
            "run is not in flight",
        )
        .await;
    });
}

#[test]
fn the_jobs_lane_claims_dispatches_and_finalizes_identically() {
    let directory = tempfile::tempdir().unwrap();
    deterministic::Runner::default().start(|context| async move {
        let mut pair = Pair::new(&context, directory.path()).await;
        pair.provision(2, "quackbot").await;
        pair.submit(
            alice(),
            runs_op(&RunsMsg::EnableJobWorker { enabled: true }),
        )
        .await;
        pair.submit(
            alice(),
            Msg {
                target: "tasks".into(),
                payload: jobs_encode_msg(&JobsMsg::Submit {
                    job_id: "job-1".into(),
                    kind: "agent/quackbot".into(),
                    spec: "summarize this work item".into(),
                }),
            },
        )
        .await;
        assert!(
            pending_run_ids(&pair.wasm).await.is_empty(),
            "source job commits before reaction"
        );
        pair.drain().await;
        let run = pending_run_ids(&pair.wasm).await.pop().unwrap();
        assert_eq!(pair.requests.len(), 1);
        pair.accept(&run).await;
        pair.submit(
            Origin::External(WORKER_NODE.to_vec()),
            runs_op(&RunsMsg::OpenAgentSession {
                run_id: run.clone(),
                attempt: 0,
                session_key: SESSION_KEY.to_vec(),
            }),
        )
        .await;
        pair.submit(
            Origin::External(SESSION_KEY.to_vec()),
            runs_op(&RunsMsg::AgentAction {
                run_id: run.clone(),
                request_id: "underway".into(),
                action: reply("Job underway."),
            }),
        )
        .await;
        pair.drain().await;
        let response = encode_response(&AgentResponse {
            reply_blocks: vec![ReplyBlock {
                kind: "paragraph".into(),
                text: "Job complete.".into(),
                lang: None,
            }],
            actions: vec![create_task("job-task", "complete job")],
            commit_message: None,
        });
        pair.settle(&run, response).await;
        assert!(pending_run_ids(&pair.wasm).await.is_empty());
        assert_eq!(task_ids(&pair.wasm).await, vec!["job-task"]);
        let bytes = pair
            .wasm
            .query(
                "tasks",
                &tasks::encode_job_query(&tasks::JobsQuery::Get {
                    job_id: "job-1".into(),
                }),
            )
            .await
            .unwrap();
        let tasks::JobsReply::Job(Some(job)) = tasks::decode_job_reply(&bytes).unwrap() else {
            panic!("job");
        };
        assert_eq!(
            job.comments
                .iter()
                .map(|comment| comment.text.as_str())
                .collect::<Vec<_>>(),
            ["Job underway.", "Job complete."]
        );
        assert!(
            job.comments
                .iter()
                .all(|comment| comment.author == tasks::Party::Account(2))
        );
        assert_eq!(job.status, tasks::JobStatus::Done);
        assert!(job.result.unwrap().ok);
    });
}

#[test]
fn a_submit_carries_a_module_message_to_its_module_on_both_runtimes() {
    let directory = tempfile::tempdir().unwrap();
    deterministic::Runner::default().start(|context| async move {
        let mut pair = Pair::new(&context, directory.path()).await;
        pair.provision(2, "quackbot").await;
        pair.submit(alice(), plain_post("general", "anchor")).await;
        pair.submit(alice(), request("quackbot", "general", 1))
            .await;
        pair.drain().await;
        let run = pending_run_ids(&pair.wasm).await.pop().unwrap();
        pair.accept(&run).await;
        pair.submit(
            Origin::External(WORKER_NODE.to_vec()),
            runs_op(&RunsMsg::OpenAgentSession {
                attempt: 0,
                run_id: run.clone(),
                session_key: SESSION_KEY.to_vec(),
            }),
        )
        .await;
        // the floor under the catalog: a chat post spelled as chat's own
        // message, carried verbatim and executed by the program account.
        let message = serde_json::to_value(ChatMsg::PostMessage {
            channel_id: "general".into(),
            message_id: "via-submit".into(),
            blocks: vec![Block::paragraph("any message a member may send")],
            thread: None,
        })
        .unwrap();
        pair.submit(
            Origin::External(SESSION_KEY.to_vec()),
            runs_op(&RunsMsg::AgentAction {
                run_id: run.clone(),
                request_id: "any-message".into(),
                action: ActionEnvelope::new(
                    runs::OP_SUBMIT,
                    Some(serde_json::json!({"module": "chat"})),
                    message.clone(),
                ),
            }),
        )
        .await;
        let receipt = pair
            .action(&runs::action_request_id(&run, "any-message"))
            .await;
        assert_eq!(receipt.target, "chat");
        assert_eq!(receipt.payload, message, "carried verbatim");
        assert!(matches!(
            receipt.status,
            runs::ActionStatus::AwaitingProgram
        ));
        pair.drain().await;
        let receipt = pair
            .action(&runs::action_request_id(&run, "any-message"))
            .await;
        assert!(matches!(
            receipt.status,
            runs::ActionStatus::Completed {
                outcome: dispatch::CallOutcomeSummary::Applied { .. },
                ..
            }
        ));
        let posted = chat_message(&pair.wasm, "via-submit").await.unwrap();
        assert_eq!(posted.head.author, Party::Account(2));
        assert_eq!(posted.head.origin, Origin::Program(2));
    });
}

#[test]
fn a_live_task_update_retains_its_attempt_on_both_runtimes() {
    let directory = tempfile::tempdir().unwrap();
    deterministic::Runner::default().start(|context| async move {
        let mut pair = Pair::new(&context, directory.path()).await;
        pair.provision(2, "quackbot").await;
        pair.submit(alice(), plain_post("general", "anchor")).await;
        pair.submit(alice(), request("quackbot", "general", 1))
            .await;
        pair.drain().await;
        let run = pending_run_ids(&pair.wasm).await.pop().unwrap();
        pair.accept(&run).await;
        pair.submit(
            Origin::External(WORKER_NODE.to_vec()),
            runs_op(&RunsMsg::OpenAgentSession {
                attempt: 0,
                run_id: run.clone(),
                session_key: SESSION_KEY.to_vec(),
            }),
        )
        .await;
        pair.submit(
            Origin::External(SESSION_KEY.to_vec()),
            runs_op(&RunsMsg::AgentAction {
                run_id: run.clone(),
                request_id: "task-update".into(),
                action: ActionEnvelope::new(
                    runs::OP_COLLABORATION_DELIVER,
                    Some(serde_json::json!({"channel_id":"review"})),
                    serde_json::json!({"message_id":"m3","recipient":"acct:7",
                    "kind":"task_update","expires_at":900,
                    "task":{"id":"review-task","expected_attempt":7}}),
                ),
            }),
        )
        .await;
        let receipt = pair
            .action(&runs::action_request_id(&run, "task-update"))
            .await;
        assert_eq!(receipt.target, "collaboration");
        let request: collaboration::Request = serde_json::from_value(receipt.payload).unwrap();
        let collaboration::CollaborationMsg::Deliver(send) = request.op else {
            panic!("expected Deliver")
        };
        assert_eq!(send.kind, collaboration::MessageKind::TaskUpdate);
        assert_eq!(
            send.task,
            Some(collaboration::TaskRef {
                id: "review-task".into(),
                expected_attempt: 7
            })
        );
    });
}

#[test]
fn deployed_registration_program_query_matches_native_and_preserves_state() {
    use sdk::Module as _;
    futures::executor::block_on(async {
        let guest = wasm_runs();
        let native = native_runs();
        let before = guest.root();
        for id in ["builder", "another-agent"] {
            let query = runs_encode_query(&RunsQuery::ModelProgram {
                agent_id: id.into(),
            });
            let actual = guest.query(&query).await.unwrap();
            assert_eq!(actual, native.query(&query).await.unwrap());
            assert_eq!(
                runs_decode_reply(&actual).unwrap(),
                RunsReply::ModelProgram(runs::model_program(id))
            );
        }
        let invalid = runs_encode_query(&RunsQuery::ModelProgram {
            agent_id: "Invalid ID".into(),
        });
        assert!(guest.query(&invalid).await.is_err());
        assert_eq!(guest.root(), before);
    });
}
