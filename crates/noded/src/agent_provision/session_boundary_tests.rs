//! THE BOUNDARY: the id a run is named by, from the composer that mints it to
//! the session `runs` binds — crossed in ONE piece, with nothing fabricated.
//!
//! every other test on this path fakes the seam it exists to prove. the
//! provisioner tests hand `WorkspaceSpec` a hardcoded `"s1:0"` and assert the
//! bind op carries `"s1:0"` back: that asserts FORWARDING, and it passes for any
//! string on earth — including the one production actually sent, which `runs`
//! could never resolve. the `runs` tests build their run ids with `run_id_for`:
//! the right ids, from a composer the provisioner never talks to. between the
//! two, the write plane shipped DEAD — `OpenAgentSession` named
//! `{saga_id}:{attempt}`, `pending_entry` missed on every lookup, no session ever
//! opened, and every agent write answered "this run has no agent session". a
//! silent degrade, by design; nothing anywhere went red.
//!
//! so this test owns the seam and fakes NO id. it drives a real chat mention
//! through the real modules until a run is in flight, hands the saga's real
//! `WorkerRequest` to the REAL [`DispatchPool`] — which decodes the composed
//! envelope and builds the `WorkspaceSpec` itself, exactly as production does —
//! lets the REAL [`NodedProvisioner`] open the session off that spec, and routes
//! the resulting op into the REAL `runs` module as the node holding the lease.
//! then it asserts what only an end-to-end run can: the session BOUND to the run
//! the pending map is actually keyed by, and the agent got a scoped endpoint for
//! THAT id without receiving the private signer.
//!
//! the duckfs checkout and the workspace commit are not the subject: their actor
//! traffic is answered by the same stand-in the plane tests use. the `runs` ops
//! are the ones that reach real consensus.

use crate::NodeHandle;
use std::path::Path;
use std::collections::BTreeMap;
use std::sync::Arc;

use attribution::AttributionModule;
use capability::CapabilityMsg;
use crate::chat::{Block, ChatMsg, Mark, PostPolicy, Span};
use commonware_runtime::{Runner as _, Supervisor as _};
use compute_service::{DeliverFn, DispatchPool, SpawnFn};
use dispatch::DispatchModule;
use futures::StreamExt as _;
use host::worker::{WorkOutcome, Worker as _};
use host::{BlockContext, Host};
use crate::runs::ModelMsg;
use saga::SagaModule;
use sdk::{Event, Msg, Origin};

use super::plane_tests::{committed_block, files_reply};
use super::*;
use crate::NodeCommand;
use crate::testkit::committed_module;
use statesync::qmdb::QmdbStore;

/// the agent under test, and the node that will claim its run's lease. the node
/// key IS the pool's identity — the same bytes `runs` checks the bind's origin
/// against — so there is nothing to line up by hand.
const AGENT: &str = "quackbot";
const CAPABILITY: &str = "mock-llm-1";
/// 32 bytes: `Accept`'s standing gate requires a real valset-shaped key, and
/// this is seeded into that valset (see `genesis`).
pub(in crate::agent_provision) const WORKER_NODE: &[u8] = &[0x77; 32];
const CHANNEL: &str = "general";

/// a provider that answers instantly and RECORDS the run context it was handed —
/// the child's env among it. that env is what the agent's process would really
/// see, so `DUCKTAPE_RUN_ID` here is the id the MCP server would stamp onto every
/// mid-run write.
struct RecordingProvider {
    seen: Arc<std::sync::Mutex<Option<provider_host::RunContext>>>,
    ready: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl provider_host::Provider for RecordingProvider {
    fn capability(&self) -> &str {
        CAPABILITY
    }
    async fn run(&self, _prompt: &str, ctx: &provider_host::RunContext) -> Result<String, String> {
        *self.seen.lock().unwrap() = Some(ctx.clone());
        self.ready.notify_one();
        Ok(r#"{"reply_blocks":[],"actions":[]}"#.to_string())
    }
}

fn provider_set(
    seen: Arc<std::sync::Mutex<Option<provider_host::RunContext>>>,
    ready: Arc<tokio::sync::Notify>,
) -> Arc<provider_host::ProviderSet> {
    let spec = provider_host::CapabilitySpec::parse(
        &format!(
            r#"
spec = 1
[capability]
tag = "{CAPABILITY}"
[detect]
bin = "{CAPABILITY}-cli"
[invoke]
args = []
prompt = "stdin"
[output]
format = "text"
"#
        ),
        "test",
    )
    .expect("the mock capability spec parses");
    Arc::new(provider_host::ProviderSet::assemble(
        provider_host::SpecSet::from_specs(vec![spec]),
        vec![Box::new(RecordingProvider { seen, ready })],
    ))
}

fn at(height: u64, origin: Origin) -> BlockContext {
    BlockContext {
        height,
        consensus_time: height,
        origin,
    }
}

fn alice() -> Origin {
    Origin::External(vec![1; 32])
}

/// the chain id this composition's identity plane is scoped to — the same
/// value the `runs` guest reads out of its genesis `__config` record.
const CHAIN_ID: &str = "session-boundary";

/// the genesis set the collaboration loop runs on — chat + the attribution plane +
/// the dispatch plane + the registry + runs.
/// `files_root` is where this composition's Files module keeps its objects:
/// runs is wired to `files`, so every genesis that runs a conversation needs
/// the module that wiring names.
///
/// chat, agent, runs and tasks are the COMMITTED guests this repo ships
/// (`crates/modules/apps/<id>/component.wasm`) — their source lives in
/// ducktape-modules, and the wiring a native constructor used to take as
/// arguments is compiled into each guest, which is why none is passed here.
/// The system modules and files are still core-resident and still native.
pub(in crate::agent_provision) async fn genesis(
    context: commonware_runtime::tokio::Context,
    files_root: &Path,
) -> Host {
    let chat = committed_module(
        "chat",
        Box::new(QmdbStore::init(context.child("chat"), "chat").await),
        CHAIN_ID,
    )
    .await;
    // `Accept`'s standing gate needs a real valset to admit a claim against,
    // and a tagged saga (this run's agent carries `CAPABILITY`) needs the
    // capability registry too — see PR #1738. `WORKER_NODE` is the only node
    // that ever claims a lease in this test, so it is the only genesis
    // validator.
    let mut valset = valset::Valset::new(
        "valset",
        Box::new(sdk_testkit::MemStore::new()),
        "governance",
    );
    valset
        .seed(WORKER_NODE.to_vec())
        .await
        .expect("seed valset");
    valset.finish_seed().await.expect("seed valset");
    Host::genesis(vec![
        Box::new(chat),
        Box::new(identity::Identity::new(
            "identity",
            Box::new(sdk_testkit::MemStore::new()),
            CHAIN_ID.into(),
        )),
        Box::new(
            AttributionModule::new("attribution", Box::new(sdk_testkit::MemStore::new()))
                .with_subscribers(["agent"]),
        ),
        Box::new(valset),
        Box::new(capability::CapabilityRegistry::new(
            "capability",
            Box::new(sdk_testkit::MemStore::new()),
            Some("valset".into()),
        )),
        Box::new(SagaModule::with_assignment(
            "saga",
            Box::new(sdk_testkit::MemStore::new()),
            "valset",
            "capability",
            saga::LeasePolicy::Open,
        )),
        Box::new(DispatchModule::new(
            "dispatch",
            "saga",
            "identity",
            Box::new(sdk_testkit::MemStore::new()),
        )),
        Box::new(committed_module("agent", Box::new(sdk_testkit::MemStore::new()), CHAIN_ID).await),
        Box::new(committed_module("runs", Box::new(sdk_testkit::MemStore::new()), CHAIN_ID).await),
        Box::new(committed_module("tasks", Box::new(sdk_testkit::MemStore::new()), CHAIN_ID).await),
        Box::new(files::Files::open("files", files_root.join("files")).expect("files module")),
    ])
    .expect("genesis")
}

/// A human mention commits before the programmable user's reaction. Draining
/// the actual queued attribution and calls creates the run and its announcement
/// event for the compute pool.
async fn mention_run(host: &mut Host) -> Event {
    let operations = [
        Msg {
            target: "identity".into(),
            payload: identity::encode_msg(&identity::IdentityMsg::Create {
                name: "Alice".into(),
                scheme: identity::KeyScheme::Ed25519,
            }),
        },
        Msg {
            target: "agent".into(),
            payload: crate::agent::encode_msg(&crate::agent::AgentMsg::Provision {
                request_id: AGENT.into(),
                name: "Quackbot".into(),
                program: crate::runs::model_program(AGENT),
            }),
        },
        Msg {
            target: "runs".into(),
            payload: crate::runs::encode_msg(&crate::runs::RunsMsg::ConfigureModel {
                operation: ModelMsg::RegisterModel {
                    account: 2,
                    agent_id: AGENT.into(),
                    display_name: "Quackbot".into(),
                    capability: CAPABILITY.into(),
                    recipe_hash: None,
                    skills: None,
                },
            }),
        },
        Msg {
            target: "chat".into(),
            payload: crate::chat::encode_msg(&ChatMsg::CreateChannel {
                channel_id: CHANNEL.into(),
                name: "General".into(),
                post_policy: PostPolicy::Open,
            }),
        },
        Msg {
            target: "chat".into(),
            payload: crate::chat::encode_msg(&ChatMsg::PostMessage {
                channel_id: CHANNEL.into(),
                message_id: "m1".into(),
                thread: None,
                blocks: vec![Block::Paragraph(vec![Span {
                    text: "Please help".into(),
                    marks: vec![Mark::Mention(crate::chat::Party::Account(2))],
                }])],
            }),
        },
    ];
    let mut height = 0;
    let mut events = Vec::new();
    for operation in operations {
        height += 1;
        events.extend(
            host.submit_at(at(height, alice()), operation)
                .await
                .unwrap()
                .events,
        );
    }
    while host.has_pending_work().await.unwrap() {
        height += 1;
        events.extend(
            host.submit_block(at(height, Origin::System), Vec::new())
                .await
                .unwrap()
                .events,
        );
    }
    let mut requests: Vec<_> = events
        .into_iter()
        .filter(|event| saga::decode_worker_request(&event.payload).is_ok())
        .collect();
    assert_eq!(requests.len(), 1);
    requests.remove(0)
}

/// serve ONE actor command the live run made.
///
/// `runs` ops go to the REAL host, framed with the node key — precisely what the
/// daemon does (it discards the caller-supplied origin and signs with its own
/// identity, which is the assignee `runs` authorizes against). everything else is
/// the duckfs lane's checkout/commit traffic, answered by the plane tests'
/// stand-in: the workspace is not this test's subject, consensus is.
async fn serve(host: &mut Host, height: u64, cmd: NodeCommand) -> Option<crate::runs::RunsMsg> {
    match cmd {
        NodeCommand::Submit {
            target,
            payload,
            reply,
            ..
        } if target == "runs" => {
            let op = crate::runs::decode_msg(&payload).expect("a runs op");
            let outcome = host
                .submit_at(
                    at(height, Origin::External(WORKER_NODE.to_vec())),
                    Msg { target, payload },
                )
                .await;
            let _ = reply.send(
                outcome
                    .map(|_| committed_block())
                    .map_err(|error| crate::Refused::of_submit(&error)),
            );
            Some(op)
        }
        NodeCommand::Submit { reply, .. } => {
            let _ = reply.send(Ok(committed_block()));
            None
        }
        NodeCommand::Query { req, reply, .. } => {
            let _ = reply.send(files_reply(&BTreeMap::new(), false, &req));
            None
        }
        _ => panic!("the run made an unexpected actor call"),
    }
}

#[test]
fn the_id_the_provisioner_binds_is_the_id_runs_resolves_the_run_by() {
    let tmp = tempfile::tempdir().unwrap();
    // a REAL tokio runtime with a commonware context: the modules need the
    // context (chat is storage-backed), the pool needs the reactor.
    let cfg = commonware_runtime::tokio::Config::default()
        .with_storage_directory(tmp.path().join("storage"));
    commonware_runtime::tokio::Runner::new(cfg).start(|context| async move {
        let runs_root = tmp.path().join("runs");
        let mut host = genesis(context, tmp.path()).await;

        // ---- a real run, in flight -----------------------------------------
        let announce = mention_run(&mut host).await;
        // THE ID CONSENSUS KNOWS. nothing below is allowed to invent it; every
        // assertion measures against this.
        let run_id = pending_runs(&host).await.into_iter().next().unwrap().run_id;
        assert!(
            pending_runs(&host).await.iter().any(|p| p.run_id == run_id),
            "the run is in flight, keyed by the id runs minted"
        );

        // ---- the real pool, the real provisioner ----------------------------
        let (handle, mut rx, _hub) = NodeHandle::channel();
        let seen = Arc::new(std::sync::Mutex::new(None));
        let ready = Arc::new(tokio::sync::Notify::new());
        // the pool RETURNS its claim/no-op op to the caller and pushes only the
        // run's terminal result down the deliver lane; the node submits both. we
        // never read the lane — the run's result is another test's subject.
        let (deliver_tx, _deliver_rx) = futures::channel::mpsc::unbounded::<Msg>();
        let spawn: SpawnFn = Arc::new(|_, fut| {
            tokio::spawn(fut);
        });
        let deliver: DeliverFn = Arc::new(move |msg| {
            let tx = deliver_tx.clone();
            Box::pin(async move {
                let _ = tx.unbounded_send(msg);
            })
        });
        let pool = DispatchPool::with_limit(
            provider_set(seen.clone(), ready.clone()),
            WORKER_NODE.to_vec(),
            spawn,
            deliver,
            1,
            // no announced capacity: this bed is about the session boundary, and
            // a bare node's ledger fits the demandless jobs it dispatches.
            Default::default(),
            Arc::new(
                NodedProvisioner::new(crate::agent_provision::test_link(handle).await, &runs_root)
                    .with_node_url(Some("http://127.0.0.1:8844".into())),
            ),
        );

        // WORKER_NODE announces itself as a `CAPABILITY` provider — Accept's
        // capability gate needs this on top of the valset standing seeded in
        // `genesis`, since the run's saga carries the agent's own capability
        // tag. it lands AFTER the trigger `mention_run` staged, so the pool
        // above still saw an empty provider pool and stayed an unassigned
        // announcement.
        host.submit_at(
            at(100, Origin::External(WORKER_NODE.to_vec())),
            Msg {
                target: "capability".into(),
                payload: capability::encode_msg(&CapabilityMsg::Announce {
                    capabilities: vec![CAPABILITY.into()],
                    resources: Default::default(),
                }),
            },
        )
        .await
        .expect("capability announce block");

        // the announcement is an OFFER: the pool claims it with Accept, and the
        // saga re-emits the request naming the winner. the lease this creates is
        // the SAME committed lease `runs` authorizes the bind against.
        let accept = match pool.run(&announce).await.expect("the pool gates the offer") {
            WorkOutcome::Handled(Some(op)) => op,
            other => panic!("the pool must CLAIM a servable announcement, got {other:?}"),
        };
        let assigned: Vec<Event> = host
            .submit_at(at(101, Origin::External(WORKER_NODE.to_vec())), accept)
            .await
            .expect("accept block")
            .events
            .into_iter()
            .filter(|e| saga::decode_worker_request(&e.payload).is_ok())
            .collect();
        assert_eq!(
            assigned.len(),
            1,
            "the accepted attempt re-announces to its holder"
        );

        // EXECUTE. the pool decodes the composed envelope and builds the
        // WorkspaceSpec itself — `run_id` = "{saga_id}:{attempt}",
        // `consensus_run_id` = whatever the envelope carried — and the
        // provisioner opens the session off it. no id in this test is written by
        // hand; this is the production path, running.
        pool.run(&assigned[0])
            .await
            .expect("the pool executes its lease");

        // serve the run's actor traffic until its session bind reaches consensus.
        let bound = loop {
            let cmd = rx.next().await.expect("the actor lane stays open");
            if let Some(crate::runs::RunsMsg::OpenAgentSession {
                run_id,
                attempt,
                session_key,
            }) = serve(&mut host, 102, cmd).await
            {
                assert_eq!(attempt, 0);
                break (run_id, session_key);
            }
        };

        // ---- THE ASSERTION: the bind LANDED, on the run that exists ----------
        assert_eq!(
            bound.0, run_id,
            "the provisioner must name the run in the id space runs resolves — \
             NOT its own {{saga_id}}:{{attempt}} workspace key"
        );
        let sessions = agent_sessions(&host).await;
        assert_eq!(
            sessions.len(),
            1,
            "the session BOUND: consensus holds exactly one live session for this run"
        );
        assert_eq!(sessions[0].run_id, run_id);
        assert_eq!(
            sessions[0].agent_id, AGENT,
            "identity comes from the run's committed entry, never the payload"
        );
        assert_eq!(
            sessions[0].session_key, bound.1,
            "the bound key is the one the provisioner minted"
        );

        // The agent was told the same id: the MCP server stamps this var onto
        // every RunsMsg::AgentAction, so a host-local id here would make every
        // mid-run write name a run that does not exist. the bind is the LAST
        // actor call a provision makes, so the model call is still landing — wait
        // for the context it was handed rather than racing it.
        ready.notified().await;
        let ctx = seen
            .lock()
            .unwrap()
            .clone()
            .expect("provider context recorded");
        assert_eq!(
            ctx.env.get("DUCKTAPE_RUN_ID").map(String::as_str),
            Some(run_id.as_str()),
            "the run id in the agent's environment is the consensus one"
        );
        assert!(
            !ctx.env.contains_key("DUCKTAPE_RUN_SESSION_KEY"),
            "the child never receives the private session key"
        );
        assert!(
            ctx.env
                .get("DUCKTAPE_RUN_ACTION_URL")
                .is_some_and(|url| url.ends_with("/v1/run-action")),
            "the child receives only the narrow action endpoint"
        );
        assert_eq!(
            ctx.env.get("DUCKTAPE_RUN_ACTION_TOKEN").map(String::len),
            Some(64)
        );
    });
}

// ---- read the module back through its own query surface ---------------------

async fn pending_runs(host: &Host) -> Vec<crate::runs::PendingRun> {
    let reply = host
        .query("runs", &crate::runs::encode_query(&crate::runs::RunsQuery::PendingRuns))
        .await
        .unwrap();
    match crate::runs::decode_reply(&reply).unwrap() {
        crate::runs::RunsReply::PendingRuns(runs) => runs,
        other => panic!("unexpected reply: {other:?}"),
    }
}

async fn agent_sessions(host: &Host) -> Vec<crate::runs::AgentSession> {
    let reply = host
        .query("runs", &crate::runs::encode_query(&crate::runs::RunsQuery::AgentSessions))
        .await
        .unwrap();
    match crate::runs::decode_reply(&reply).unwrap() {
        crate::runs::RunsReply::AgentSessions(sessions) => sessions,
        other => panic!("unexpected reply: {other:?}"),
    }
}
