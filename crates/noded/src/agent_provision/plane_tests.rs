//! the agent TOOL PLANE both lanes hand every run: the node base its lane
//! serves the MCP catalog on (`DUCKTAPE_NODE`), the identity it acts under
//! (`DUCKTAPE_RUN_AGENT`), and the run-scoped endpoint its writes go through.
//!
//! the duckfs-lane cases here run the REAL `checkout_with` engine against a
//! stand-in files actor on the `NodeCommand` lane ([`spawn_files_actor`], which
//! the forge-lane W6 tests share) — the mount bracket is exercised for real,
//! without booting a node.

use crate::NodeHandle;
use std::collections::BTreeMap;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use compute_service::{RoMount, WorkspaceProvisioner as _, WorkspaceSource, WorkspaceSpec};
use duckfs_client::chunk::{chunk_ids, file_object_id};
use duckfs_core::{
    EntryInfo, EntryKindWire, FilesQuery, FilesReply, RefsInfo, decode_query, encode_reply,
};
use futures::StreamExt as _;

use super::*;
use crate::NodeCommand;

/// the skill subtree every W6 test mounts, and the file it must materialize.
pub(super) const SKILL_PREFIX: &str = "/shared/skills/qa";
pub(super) const SKILL_FILE: &str = "SKILL.md";
pub(super) const SKILL_BODY: &str = "always quack twice\n";

/// the in-memory duckfs the stand-in actor serves: one skill subtree.
pub(super) fn skill_tree() -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([(
        format!("{SKILL_PREFIX}/{SKILL_FILE}"),
        SKILL_BODY.as_bytes().to_vec(),
    )])
}

/// the W6 mount every skill test pins (`<ro-root>/qa` ⇐ the skill subtree).
/// on-demand by default: the `always` (inlined-persona) half is exercised by
/// [`super::tests`]' soul assembly, which is where a body read can fail a run.
pub(super) fn skill_mount() -> RoMount {
    RoMount {
        source_prefix: SKILL_PREFIX.into(),
        source_snapshot: None,
        mount_subpath: "qa".into(),
        always: false,
    }
}

/// a stand-in files actor on the `NodeCommand` lane: answers the three queries
/// a checkout makes (`refs` / `find` / `read`) over one in-memory tree, so the
/// REAL engine production runs (`checkout_with` through `ActorNodeApi`) drives
/// the mount bracket without a booted node.
///
/// `reject_reads` fails every `read` AFTER the enumeration succeeded — the
/// shape of a mid-checkout failure (a transport drop, an unavailable chunk):
/// the mount dirs are already on disk when the error fires, so the W5 unwind
/// has real debris to remove.
pub(super) fn spawn_files_actor(
    mut rx: futures::channel::mpsc::Receiver<NodeCommand>,
    tree: BTreeMap<String, Vec<u8>>,
    reject_reads: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(cmd) = rx.next().await {
            match cmd {
                // the ONE write a provision makes: the agent session bind, which
                // this actor is the assignee for and so commits. the refusing
                // case is [`spawn_session_actor`].
                NodeCommand::Submit { reply, .. } => {
                    let _ = reply.send(Ok(committed_block()));
                }
                NodeCommand::Query { req, reply, .. } => {
                    let _ = reply.send(files_reply(&tree, reject_reads, &req));
                }
                // the mount lane otherwise only ever reads.
                _ => panic!("the stand-in files actor got an unexpected command"),
            }
        }
    })
}

/// every session bind this actor saw, in order — the ops the provisioner ACTUALLY
/// submitted, so a test asserts against the wire and not against its own belief
/// about it.
pub(super) type SessionBinds = std::sync::Arc<std::sync::Mutex<Vec<runs::RunsMsg>>>;
type SessionActions = std::sync::Arc<std::sync::Mutex<Vec<(sdk::Origin, runs::RunsMsg)>>>;

/// a stand-in actor for the SESSION lane: it records every op the provisioner
/// submits and answers it with `bind` — `Ok` for the node that holds the run's
/// lease, `Err(detail)` for one that does not (the module's own refusal string).
/// files queries are answered over an empty tree, so the rw checkout materializes
/// an empty dir and the provision reaches the session either way.
fn spawn_session_actor(
    mut rx: futures::channel::mpsc::Receiver<NodeCommand>,
    bind: Result<(), &'static str>,
) -> (tokio::task::JoinHandle<()>, SessionBinds, SessionActions) {
    let binds: SessionBinds = Default::default();
    let seen = binds.clone();
    let actions: SessionActions = Default::default();
    let seen_actions = actions.clone();
    let actor = tokio::spawn(async move {
        while let Some(cmd) = rx.next().await {
            match cmd {
                NodeCommand::Submit {
                    target,
                    payload,
                    reply,
                    ..
                } => {
                    assert_eq!(target, "runs", "the only op a provision submits");
                    seen.lock()
                        .unwrap()
                        .push(runs::decode_msg(&payload).expect("a runs op"));
                    let _ = reply.send(
                        bind.map(|()| committed_block())
                            .map_err(|said| crate::Refused::new("module", said)),
                    );
                }
                NodeCommand::Query {
                    target, req, reply, ..
                } => {
                    let result = if target == "runs" {
                        let result = match runs::decode_query(&req).unwrap() {
                            runs::RunsQuery::AgentSessions => {
                                runs::RunsReply::AgentSessions(vec![runs::AgentSession {
                                    run_id: consensus_run_id(),
                                    agent_id: "quackbot".into(),
                                    session_key: Vec::new(),
                                    lease: runs::ExecutionLease {
                                        holder: Vec::new(),
                                        attempt: 0,
                                    },
                                    opened_at: 0,
                                    actions: 0,
                                }])
                            }
                            runs::RunsQuery::ActionRequest { request_id } => {
                                assert_eq!(
                                    seen_actions.lock().unwrap().len(),
                                    1,
                                    "receipt follows admitted action"
                                );
                                runs::RunsReply::ActionRequest(Some(runs::ActionRequestView {
                                    request_id,
                                    account: 2,
                                    generation: 0,
                                    run_id: consensus_run_id(),
                                    operation: "tasks.create".into(),
                                    result: serde_json::Value::Null,
                                    target: "tasks".into(),
                                    payload: serde_json::Value::Null,
                                    status: runs::ActionStatus::Completed {
                                        call: sdk::CallId {
                                            requester: "agent".into(),
                                            invocation: "2/1".into(),
                                            step: 1,
                                        },
                                        outcome: dispatch::CallOutcomeSummary::Applied {
                                            output_digest: [0; 32],
                                            assigned: Vec::new(),
                                        },
                                    },
                                }))
                            }
                            query => panic!("unexpected session query {query:?}"),
                        };
                        Ok(runs::encode_reply(&result))
                    } else {
                        files_reply(&BTreeMap::new(), false, &req)
                    };
                    let _ = reply.send(result);
                }
                NodeCommand::SubmitFrame { frame, reply } => {
                    let (origin, msg) = node::decode_frame(&frame).expect("a valid action frame");
                    assert_eq!(msg.target, "runs");
                    seen_actions.lock().unwrap().push((
                        origin,
                        runs::decode_msg(&msg.payload).expect("a runs action"),
                    ));
                    let _ = reply.send(Ok(committed_block()));
                }
                // the provisioner reads as the NODE (`Query`), never as a
                // principal. One arriving here would mean this plane had grown
                // an authenticated read path, which is a change to assert
                // about deliberately rather than absorb silently.
                NodeCommand::QueryAs { target, .. } => {
                    panic!("the session lane received an authenticated read for {target}")
                }
            }
        }
    });
    (actor, binds, actions)
}

pub(super) fn committed_block() -> crate::BlockSummary {
    crate::BlockSummary {
        height: 1,
        root_hash: "ab".repeat(32),
    }
}

/// answer one files query over `tree` — the read half both stand-ins share.
///
/// `reject_reads` fails every `read` AFTER the enumeration succeeded (see
/// [`spawn_files_actor`]).
pub(super) fn files_reply(
    tree: &BTreeMap<String, Vec<u8>>,
    reject_reads: bool,
    req: &[u8],
) -> Result<Vec<u8>, crate::Refused> {
    match decode_query(req).expect("a files query") {
        FilesQuery::Refs {} => Ok(encode_reply(&FilesReply::Refs(RefsInfo {
            head: None,
            pins: BTreeMap::new(),
            window_len: 0,
        }))),
        FilesQuery::Find { prefix, .. } => Ok(encode_reply(&FilesReply::Find {
            entries: tree
                .iter()
                .filter(|(path, _)| path.starts_with(&prefix))
                .map(|(path, bytes)| file_entry(path, bytes))
                .collect(),
            next: None,
        })),
        FilesQuery::Read { path, .. } if !reject_reads => {
            let bytes = tree.get(&path).cloned().unwrap_or_default();
            Ok(encode_reply(&FilesReply::Read {
                b64: STANDARD.encode(&bytes),
                eof: true,
            }))
        }
        // the verbatim module contract string the engine's taxonomy keys on.
        FilesQuery::Read { .. } => Err(crate::Refused::new(
            "module",
            "files: chunk not available",
        )),
        other => panic!("the checkout asked for {other:?}"),
    }
}

/// one file entry, content-addressed EXACTLY as the module stores it — the
/// checkout re-derives this id over the bytes it assembled and refuses a
/// mismatch, so a sloppy stand-in would fail the verify, not the test.
fn file_entry(path: &str, bytes: &[u8]) -> EntryInfo {
    let meta = BTreeMap::new();
    let size = bytes.len() as u64;
    EntryInfo {
        path: path.into(),
        kind: EntryKindWire::File,
        size,
        exec: false,
        object: duckfs_core::to_hex(&file_object_id(size, &chunk_ids(bytes), &meta)),
        meta,
    }
}

/// the run id CONSENSUS knows these runs by — minted with the SAME `run_id_for`
/// the runs module keys its pending map with, never hand-written. the spec's own
/// `run_id` (`{saga_id}:{attempt}`) is the host's dir key and names no run in
/// consensus, so a test that asserted the session bind carried THAT would pass
/// on an id `runs` can never resolve — which is exactly how the write plane
/// shipped dead. the end-to-end proof that these two ids stay distinct and the
/// right one is bound lives in [`super::session_boundary_tests`].
fn consensus_run_id() -> String {
    runs::run_id_for("general", 1, "quackbot")
}

/// a duckfs-source spec: the agent's own workspace prefix (empty in the
/// stand-in tree — the rw checkout materializes nothing) plus the skill mounts.
fn duckfs_spec(agent: Option<&str>, mounts: Vec<RoMount>) -> WorkspaceSpec {
    WorkspaceSpec {
        run_id: "s1:0".into(),
        agent: agent.map(|id| compute_service::AgentExecution {
            native_conversation: None,
            run_id: consensus_run_id(),
            attempt: 0,
            agent_id: id.into(),
            display_name: id.into(),
        }),
        source: WorkspaceSource::Duckfs {
            source_prefix: "/shared/agent-workspaces/quackbot".into(),
            source_snapshot: None,
        },
        ro_mounts: mounts,
    }
}

// ---- the node base -------------------------------------------------------

#[test]
fn the_node_base_is_the_bare_http_root_the_forge_lane_hangs_off() {
    // a wildcard bind must become a CONNECTABLE loopback in the SAME family (a
    // bindv6only [::] listener refuses 127.0.0.1) — a run dials this back.
    assert_eq!(
        node_http_base(Some("0.0.0.0:8844")).as_deref(),
        Some("http://127.0.0.1:8844")
    );
    assert_eq!(
        node_http_base(Some("[::]:9001")).as_deref(),
        Some("http://[::1]:9001")
    );
    // an explicit host passes through untouched (the operator chose it).
    assert_eq!(
        node_http_base(Some("10.0.0.5:8844")).as_deref(),
        Some("http://10.0.0.5:8844")
    );
    assert_eq!(
        node_http_base(Some("localhost:8844")).as_deref(),
        Some("http://localhost:8844")
    );
    // no http surface ⇒ nothing to dial ⇒ nothing to hand the run.
    assert_eq!(node_http_base(None), None);

    // DUCKTAPE_NODE is the node ROOT: the /forge mount is the FORGE lane's
    // suffix on top of it, and must never leak into the tool plane's base.
    let base = node_http_base(Some("0.0.0.0:8844")).unwrap();
    assert!(
        !base.contains("/forge"),
        "the node base carries no suffix: {base}"
    );
}

// ---- what a run actually gets ---------------------------------------------

#[tokio::test]
async fn a_run_gets_the_node_base_its_agent_id_and_the_tool_bin_dir_on_path() {
    let tmp = tempfile::tempdir().unwrap();
    let (handle, rx, _hub) = NodeHandle::channel();
    let _actor = spawn_files_actor(rx, skill_tree(), false);
    let prov = NodedProvisioner::new(crate::agent_provision::test_link(handle).await, tmp.path())
        .with_node_url(Some("http://127.0.0.1:8844".into()));

    let ws = prov
        .provision(&duckfs_spec(Some("quackbot"), vec![skill_mount()]))
        .await
        .expect("provision");
    let dir = ws.workdir();
    let env = ws.env();

    // the tool plane's two halves: WHERE the node is …
    assert_eq!(
        env.get("DUCKTAPE_NODE").map(String::as_str),
        Some("http://127.0.0.1:8844")
    );
    // … and WHO the run acts for. the model's record is deliberately NOT
    // here: it is read back from the committed registry by this id, so it can
    // never drift from the record.
    assert_eq!(
        env.get("DUCKTAPE_RUN_AGENT").map(String::as_str),
        Some("quackbot")
    );
    // the workspace + skill roots (the skill tree is the -ro SIBLING).
    assert_eq!(
        env.get("DUCKTAPE_RUN_WORKSPACE").map(String::as_str),
        Some(dir.display().to_string().as_str())
    );
    let ro = PathBuf::from(env.get("DUCKTAPE_RUN_SKILLS").expect("skills root"));
    assert_eq!(ro, PathBuf::from(format!("{}-ro", dir.display())));
    assert_eq!(
        std::fs::read_to_string(ro.join("qa").join(SKILL_FILE)).unwrap(),
        SKILL_BODY
    );

    ws.cleanup().await;
    assert!(
        !dir.exists() && !ro.exists(),
        "W5: both trees are the run's debris"
    );
}

#[tokio::test]
async fn an_unreachable_node_or_an_anonymous_run_omits_the_var_rather_than_guessing() {
    let tmp = tempfile::tempdir().unwrap();
    let (handle, rx, _hub) = NodeHandle::channel();
    let _actor = spawn_files_actor(rx, skill_tree(), false);
    // no with_node_url (a node serving no http surface) and no agent_id.
    let ws = NodedProvisioner::new(crate::agent_provision::test_link(handle).await, tmp.path())
        .provision(&duckfs_spec(None, Vec::new()))
        .await
        .expect("provision");

    let env = ws.env();
    assert!(
        !env.contains_key("DUCKTAPE_NODE"),
        "a guessed base is worse than an absent one"
    );
    assert!(!env.contains_key("DUCKTAPE_RUN_AGENT"));
    assert!(
        !env.contains_key("DUCKTAPE_RUN_SKILLS"),
        "no mounts, no root"
    );
    // the run still runs — only the tool plane is missing, never the workspace.
    assert!(env.contains_key("DUCKTAPE_RUN_WORKSPACE"));
    ws.cleanup().await;
}

// ---- the agent session key --------------------------------------------------

#[tokio::test]
async fn an_agent_run_gets_a_scoped_endpoint_while_the_private_key_stays_host_side() {
    let tmp = tempfile::tempdir().unwrap();
    let (handle, rx, _hub) = NodeHandle::channel();
    let (_actor, binds, actions) = spawn_session_actor(rx, Ok(()));

    let ws = NodedProvisioner::new(crate::agent_provision::test_link(handle).await, tmp.path())
        .provision(&duckfs_spec(Some("quackbot"), Vec::new()))
        .await
        .expect("provision");
    let env = ws.env();

    assert!(
        !env.contains_key("DUCKTAPE_RUN_SESSION_KEY"),
        "the private session key must never enter the child environment"
    );
    assert_eq!(
        env.get("DUCKTAPE_RUN_ID").map(String::as_str),
        Some(consensus_run_id().as_str()),
        "the run the AGENT names is the one runs resolves, never the spec's dir key"
    );
    let action_url = env
        .get("DUCKTAPE_RUN_ACTION_URL")
        .expect("an agent run gets a scoped action endpoint");
    let action_token = env
        .get("DUCKTAPE_RUN_ACTION_TOKEN")
        .expect("an agent run gets a scoped action token");
    assert!(action_url.starts_with("http://127.0.0.1:"));
    assert!(action_url.ends_with("/v1/run-action"));
    assert_eq!(action_token.len(), 64);

    let bound = match binds.lock().unwrap().as_slice() {
        [
            runs::RunsMsg::OpenAgentSession {
                run_id,
                attempt,
                session_key,
            },
        ] => {
            assert_eq!(
                run_id,
                &consensus_run_id(),
                "the bind names the run in the id space runs can resolve"
            );
            assert_eq!(*attempt, 0);
            session_key.clone()
        }
        other => panic!("expected exactly one session bind, got {other:?}"),
    };
    assert_eq!(bound.len(), runs::SESSION_KEY_LEN);

    let wrong_token = post_action(
        action_url,
        &"cd".repeat(32),
        &serde_json::json!({"message":{"agent_action":{
            "run_id": consensus_run_id(),
            "request_id": "task-0",
            "action":{"operation":"tasks.create","input":{"task_id":"task-0", "title":"no"}},
        }}}),
    )
    .await;
    assert_eq!(wrong_token, 401);

    let wrong_run = post_action(
        action_url,
        action_token,
        &serde_json::json!({"message":{"agent_action":{
            "run_id": "another-run",
            "request_id": "task-0",
            "action":{"operation":"tasks.create","input":{"task_id":"task-0", "title":"no"}},
        }}}),
    )
    .await;
    assert_eq!(wrong_run, 403);

    let out_of_scope = post_action(
        action_url,
        action_token,
        &serde_json::json!({"message":{"open_agent_session":{
            "attempt": 0,
            "run_id": consensus_run_id(),
            "session_key": vec![0; runs::SESSION_KEY_LEN],
        }}}),
    )
    .await;
    assert_eq!(out_of_scope, 403);
    assert!(actions.lock().unwrap().is_empty());

    let action = runs::ActionEnvelope::new(
        runs::OP_TASKS_CREATE,
        None,
        serde_json::json!({"task_id": "task-1", "title": "scoped"}),
    );
    let accepted = post_action(
        action_url,
        action_token,
        &serde_json::json!({"message":{"agent_action":{
            "run_id": consensus_run_id(),
            "request_id": "task-1",
            "action": action,
        }}}),
    )
    .await;
    assert_eq!(accepted, 200);
    {
        let submitted = actions.lock().unwrap();
        assert_eq!(submitted.len(), 1);
        assert_eq!(
            submitted[0].0,
            sdk::Origin::External(bound),
            "the host signs with the public key consensus bound"
        );
        assert!(matches!(
            &submitted[0].1,
            runs::RunsMsg::AgentAction { run_id, .. } if run_id == &consensus_run_id()
        ));
    }

    // Neither the node key nor the run's private session key enters the child;
    // it receives only the run-scoped endpoint and bearer token.
    assert!(!env.contains_key("DUCKTAPE_NODE_KEY"));
    ws.cleanup().await;
}

async fn post_action(url: &str, token: &str, value: &serde_json::Value) -> u16 {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let address = url
        .strip_prefix("http://")
        .and_then(|rest| rest.split_once('/'))
        .map(|(address, _)| address)
        .expect("loopback action url");
    let body = value.to_string();
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect action endpoint");
    stream
        .write_all(
            format!(
                "POST /v1/run-action HTTP/1.1\r\nhost: {address}\r\ncontent-type: application/json\r\n\
                 x-ducktape-run-action: {token}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .expect("write action request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read action response");
    String::from_utf8_lossy(&response)
        .split_whitespace()
        .nth(1)
        .and_then(|status| status.parse().ok())
        .expect("http status")
}

#[tokio::test]
async fn a_run_with_no_agent_opens_no_session_and_submits_no_bind() {
    let tmp = tempfile::tempdir().unwrap();
    let (handle, rx, _hub) = NodeHandle::channel();
    let (_actor, binds, _actions) = spawn_session_actor(rx, Ok(()));

    let ws = NodedProvisioner::new(crate::agent_provision::test_link(handle).await, tmp.path())
        .provision(&duckfs_spec(None, Vec::new()))
        .await
        .expect("provision");

    let env = ws.env();
    assert!(!env.contains_key("DUCKTAPE_RUN_AGENT"));
    assert!(!env.contains_key("DUCKTAPE_RUN_ID"));
    assert!(!env.contains_key("DUCKTAPE_RUN_SESSION_KEY"));
    assert!(!env.contains_key(session::ENV_ACTION_URL));
    assert!(
        binds.lock().unwrap().is_empty(),
        "a workspace nobody acts for asks consensus for nothing"
    );
    ws.cleanup().await;
}

#[tokio::test]
async fn a_refused_bind_fails_provision_and_removes_the_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let (handle, rx, _hub) = NodeHandle::channel();
    let (_actor, binds, _actions) = spawn_session_actor(rx, Err("runs: not the run's assignee"));
    let result = NodedProvisioner::new(crate::agent_provision::test_link(handle).await, tmp.path())
        .provision(&duckfs_spec(Some("quackbot"), Vec::new()))
        .await;
    let Err(error) = result else {
        panic!("the model must not start with a refused session");
    };
    assert!(error.contains("open agent session"), "{error}");
    assert!(error.contains("not the run's assignee"), "{error}");
    assert_eq!(binds.lock().unwrap().len(), 1);
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
}

type ReceiptState = std::sync::Arc<std::sync::Mutex<runs::ActionStatus>>;

fn spawn_receipt_actor(
    mut commands: futures::channel::mpsc::Receiver<NodeCommand>,
) -> (
    tokio::task::JoinHandle<()>,
    ReceiptState,
    tokio::sync::mpsc::UnboundedReceiver<()>,
) {
    let status: ReceiptState =
        std::sync::Arc::new(std::sync::Mutex::new(runs::ActionStatus::AwaitingProgram));
    let stored = status.clone();
    let (observed, queries) = tokio::sync::mpsc::unbounded_channel();
    let actor = tokio::spawn(async move {
        while let Some(command) = commands.next().await {
            match command {
                NodeCommand::Submit { reply, .. } | NodeCommand::SubmitFrame { reply, .. } => {
                    let _ = reply.send(Ok(committed_block()));
                }
                NodeCommand::Query {
                    target, req, reply, ..
                } => {
                    assert_eq!(target, "runs");
                    let response = match runs::decode_query(&req).unwrap() {
                        runs::RunsQuery::AgentSessions => {
                            runs::RunsReply::AgentSessions(vec![runs::AgentSession {
                                run_id: consensus_run_id(),
                                agent_id: "quackbot".into(),
                                session_key: Vec::new(),
                                lease: runs::ExecutionLease {
                                    holder: Vec::new(),
                                    attempt: 0,
                                },
                                opened_at: 0,
                                actions: 0,
                            }])
                        }
                        runs::RunsQuery::ActionRequest { request_id } => {
                            let _ = observed.send(());
                            runs::RunsReply::ActionRequest(Some(runs::ActionRequestView {
                                request_id,
                                account: 2,
                                generation: 0,
                                run_id: consensus_run_id(),
                                operation: "tasks.create".into(),
                                result: serde_json::Value::Null,
                                target: "tasks".into(),
                                payload: serde_json::Value::Null,
                                status: stored.lock().unwrap().clone(),
                            }))
                        }
                        query => panic!("unexpected receipt query {query:?}"),
                    };
                    let _ = reply.send(Ok(runs::encode_reply(&response)));
                }
                // as above: the receipt lane reads as the NODE, never as a
                // principal.
                NodeCommand::QueryAs { target, .. } => {
                    panic!("the receipt lane received an authenticated read for {target}")
                }
            }
        }
    });
    (actor, status, queries)
}

fn request_tool_action(session: &super::session::RunSession) -> tokio::task::JoinHandle<u16> {
    let url = session.action_url.clone();
    let token = session.action_token.clone();
    tokio::spawn(async move {
        post_action(&url, &token, &serde_json::json!({"message":{"agent_action":{
            "run_id":consensus_run_id(), "request_id":"committed",
            "action":{"operation":"tasks.create","input":{"task_id":"committed", "title":"wait for target"}}
        }}})).await
    })
}

#[tokio::test]
async fn tool_http_waits_for_the_actual_committed_outcome_and_surfaces_target_failure() {
    for (outcome, expected) in [
        (
            dispatch::CallOutcomeSummary::Applied {
                output_digest: [1; 32],
                assigned: Vec::new(),
            },
            200,
        ),
        (
            dispatch::CallOutcomeSummary::Rejected {
                reason: "the target rejected the write".into(),
            },
            400,
        ),
    ] {
        let (handle, commands, hub) = NodeHandle::channel();
        let (actor, state, mut observed) = spawn_receipt_actor(commands);
        let link = test_link(handle).await;
        let workdir = tempfile::tempdir().unwrap();
        let session = super::session::open(
            &link, &duckfs_spec(Some("quackbot"), Vec::new()), workdir.path(),
        )
        .await
        .unwrap()
        .expect("agent session");
        let request = request_tool_action(&session);
        observed
            .recv()
            .await
            .expect("the admitted action queried its still-pending receipt");
        assert!(
            !request.is_finished(),
            "admission must not return tool success"
        );
        *state.lock().unwrap() = runs::ActionStatus::Completed {
            call: sdk::CallId {
                requester: "agent".into(),
                invocation: "2/1".into(),
                step: 1,
            },
            outcome,
        };
        hub.publish_block(2, "cd".repeat(32), crate::stream::BlockWake::TipOnly);
        assert_eq!(request.await.unwrap(), expected);
        drop(session);
        actor.abort();
    }
}

#[tokio::test]
async fn disconnecting_the_registered_receipt_stream_fails_the_pending_tool_request() {
    let directory = tempfile::tempdir().unwrap();
    let token = crate::admin::mint_operator_token(directory.path()).unwrap();
    let (handle, commands, _) = NodeHandle::channel();
    let handle = handle.with_admin(crate::AdminConfig {
        operator_token: Some(token),
        ..Default::default()
    });
    let (actor, _, mut observed) = spawn_receipt_actor(commands);
    let (disconnect, signal) = tokio::sync::oneshot::channel::<()>();
    let signal = std::sync::Arc::new(tokio::sync::Mutex::new(Some(signal)));
    // Only the stream is controlled here; admission and receipt queries use
    // the real node router and actor boundary.
    let app = axum::Router::new()
        .route(
            "/v1/ws",
            axum::routing::get(move |upgrade: axum::extract::ws::WebSocketUpgrade| {
                let signal = signal.clone();
                async move {
                    let signal = signal.lock().await.take().unwrap();
                    upgrade.on_upgrade(move |mut socket| async move {
                        socket.recv().await.expect("subscription").unwrap();
                        socket
                            .send(axum::extract::ws::Message::Text(
                                "{\"type\":\"subscribed\"}".into(),
                            ))
                            .await
                            .unwrap();
                        signal.await.unwrap();
                        socket
                            .send(axum::extract::ws::Message::Close(None))
                            .await
                            .unwrap();
                    })
                }
            }),
        )
        .fallback_service(crate::router(handle));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let link =
        NodeLink::new(format!("http://{address}")).with_workspace_credential(directory.path());
    let session = super::session::open(
        &link, &duckfs_spec(Some("quackbot"), Vec::new()), directory.path(),
    )
    .await
    .unwrap()
    .expect("agent session");
    let request = request_tool_action(&session);
    observed.recv().await.unwrap();
    assert!(!request.is_finished());
    disconnect.send(()).unwrap();
    assert_eq!(request.await.unwrap(), 400);
    drop(session);
    actor.abort();
    server.abort();
}
