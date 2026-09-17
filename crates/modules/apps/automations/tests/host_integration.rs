//! integration: the real host routes a chat hook follow-up into automations with
//! `Origin::Module("chat")`, a rule fires, and its `CreateTask` follow-up lands
//! in the real tasks module — all atomically within one block.

use automations::Automations;
use automations::{
    Action, AutomationsMsg, AutomationsQuery, AutomationsReply, RunRecord, Trigger, decode_reply,
    encode_msg, encode_query,
};
use chat::{ChatEvent, Party, encode_event};
use futures::executor::block_on;
use host::{BlockContext, Host};
use sdk::{Ctx, Error, Module, ModuleId, Msg, Origin, StateRoot};
use tasks::Tasks;
use tasks::{
    TaskQuery, TaskReply, decode_task_reply as tasks_decode_reply,
    encode_task_query as tasks_encode_query,
};

const AUTO: &str = "automations";
const CHAT: &str = "chat";
const TASKS: &str = "tasks";
const IDENTITY: &str = "identity";
const ATTRIBUTION: &str = "attribution";

/// a stand-in for chat that relays its payload to automations as a hook
/// follow-up. because it is registered under the id "chat", the host stamps the
/// follow-up with `Origin::Module("chat")` — exactly what automations trusts.
struct RelayChat;

#[async_trait::async_trait(?Send)]
impl Module for RelayChat {
    fn id(&self) -> ModuleId {
        CHAT.into()
    }
    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }
    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        ctx.emit_msg(Msg {
            target: AUTO.into(),
            payload: msg.payload.clone(),
        });
        Ok(())
    }
    /// the relay serves no read: a task-creating rule asks chat nothing
    /// before it fires.
    async fn query(&self, _: &[u8]) -> Result<Vec<u8>, Error> {
        Err(Error::QueryUnsupported)
    }
}

fn from_user(payload: Msg) -> (BlockContext, Msg) {
    (
        BlockContext {
            height: 1,
            consensus_time: 100,
            origin: Origin::External(vec![9; 32]),
        },
        payload,
    )
}

fn create_rule_msg(rule_id: &str, trigger: Trigger, action: Action) -> Msg {
    Msg {
        target: AUTO.into(),
        payload: encode_msg(&AutomationsMsg::CreateRule {
            rule_id: rule_id.into(),
            trigger,
            action,
        }),
    }
}

fn chat_event_msg(channel: &str, seq: u64, author: Party) -> Msg {
    Msg {
        target: CHAT.into(),
        payload: encode_event(&ChatEvent::MessagePosted {
            channel_id: channel.into(),
            seq,
            thread_root: None,
            author,
            mentions: Vec::new(),
        }),
    }
}

async fn tasks_of(host: &Host) -> Vec<tasks::Task> {
    let req = tasks_encode_query(&TaskQuery::List {
        limit: tasks::MAX_LIST_LIMIT,
        after: None,
    });
    let bytes = host.query(TASKS, &req).await.expect("query");
    let TaskReply::Tasks(tasks) = tasks_decode_reply(&bytes).expect("reply") else {
        panic!("a list answers a page");
    };
    tasks
}

async fn run_history(host: &Host, rule_id: &str) -> Vec<RunRecord> {
    let bytes = host
        .query(
            AUTO,
            &encode_query(&AutomationsQuery::RunHistory {
                rule_id: rule_id.into(),
                limit: 16,
            }),
        )
        .await
        .expect("query");
    match decode_reply(&bytes).expect("reply") {
        AutomationsReply::History(records) => records,
        other => panic!("expected History, got {other:?}"),
    }
}

async fn genesis() -> Host {
    let auto = Automations::new(
        AUTO,
        Box::new(sdk_testkit::MemStore::new()),
        CHAT,
        TASKS,
        IDENTITY,
        ATTRIBUTION,
    );
    let mut host = Host::genesis(vec![
        Box::new(identity::Identity::new(
            "identity",
            Box::new(sdk_testkit::MemStore::new()),
            "test".into(),
        )),
        Box::new(attribution::AttributionModule::new(
            "attribution",
            Box::new(sdk_testkit::MemStore::new()),
        )),
        Box::new(Tasks::new(
            TASKS,
            "identity",
            "attribution",
            Box::new(sdk_testkit::MemStore::new()),
        )),
        Box::new(RelayChat),
        Box::new(auto),
    ])
    .expect("genesis");
    host.submit_at(
        BlockContext {
            height: 0,
            consensus_time: 0,
            origin: Origin::External(vec![9; 32]),
        },
        Msg {
            target: "identity".into(),
            payload: identity::encode_msg(&identity::IdentityMsg::Create {
                name: "Operator".into(),
                scheme: identity::KeyScheme::Ed25519,
            }),
        },
    )
    .await
    .expect("found owner");
    host
}

#[test]
fn user_post_fires_rule_and_creates_task_atomically() {
    block_on(async {
        let mut host = genesis().await;

        // register a CreateTask rule.
        let (ctx, msg) = from_user(create_rule_msg(
            "capture",
            Trigger {
                channel_id: Some("general".into()),
                mention: None,
                text_contains: None,
            },
            Action::CreateTask {
                task_id_prefix: "auto".into(),
                title_template: "post {seq} in {channel}".into(),
            },
        ));
        host.submit_at(ctx, msg).await.expect("create rule");

        // a user post in "general" flows chat -> automations -> tasks in one block.
        let app_before = host.root_hash();
        let out = host
            .submit_at(
                BlockContext {
                    height: 2,
                    consensus_time: 200,
                    origin: Origin::External(b"poster".to_vec()),
                },
                chat_event_msg("general", 5, Party::Key(vec![1; 4])),
            )
            .await
            .expect("hook fires");
        assert_ne!(out.root_hash, app_before, "the fire moved the root-hash");

        let tasks = tasks_of(&host).await;
        assert_eq!(tasks.len(), 1, "the rule created exactly one task");
        assert_eq!(
            tasks[0].id, "auto-capture-general-5",
            "deterministic task id, the firing rule named in it"
        );
        assert_eq!(tasks[0].title, "post 5 in general");

        let recs = run_history(&host, "capture").await;
        assert_eq!(recs.len(), 1);
        assert!(recs[0].action_ok);
        assert_eq!(recs[0].seq, 5);
    });
}

#[test]
fn module_authored_post_does_not_fire() {
    block_on(async {
        let mut host = genesis().await;
        let (ctx, msg) = from_user(create_rule_msg(
            "capture",
            Trigger {
                channel_id: None,
                mention: None,
                text_contains: None,
            },
            Action::CreateTask {
                task_id_prefix: "auto".into(),
                title_template: "T".into(),
            },
        ));
        host.submit_at(ctx, msg).await.expect("create rule");

        // a module-authored post (loop-prevention target) must not fire.
        host.submit_at(
            BlockContext {
                height: 2,
                consensus_time: 200,
                origin: Origin::External(b"poster".to_vec()),
            },
            chat_event_msg("general", 1, Party::Module("automations".into())),
        )
        .await
        .expect("no-fail arm");

        assert!(
            tasks_of(&host).await.is_empty(),
            "no task from a module post"
        );
    });
}

#[test]
fn squatted_task_id_is_caught_by_probe_and_block_commits() {
    block_on(async {
        let mut host = genesis().await;
        let (ctx, msg) = from_user(create_rule_msg(
            "capture",
            Trigger {
                channel_id: None,
                mention: None,
                text_contains: None,
            },
            Action::CreateTask {
                task_id_prefix: "auto".into(),
                title_template: "T".into(),
            },
        ));
        host.submit_at(ctx, msg).await.expect("create rule");

        // squat the deterministic id the next fire will compose: without the
        // probe, tasks would reject the duplicate and abort the posting block.
        host.submit_at(
            BlockContext {
                height: 2,
                consensus_time: 200,
                origin: Origin::External(b"squatter".to_vec()),
            },
            Msg {
                target: TASKS.into(),
                payload: tasks::encode_task_msg(&tasks::TaskMsg::CreateTask {
                    task_id: "auto-capture-general-5".into(),
                    title: "squatted".into(),
                    owner: None,
                }),
            },
        )
        .await
        .expect("squat the id");

        // the fire is downgraded to a run record; the user's post commits.
        host.submit_at(
            BlockContext {
                height: 3,
                consensus_time: 300,
                origin: Origin::External(b"poster".to_vec()),
            },
            chat_event_msg("general", 5, Party::Key(vec![1; 4])),
        )
        .await
        .expect("the squatted fire must not abort the block");
        assert_eq!(tasks_of(&host).await.len(), 1, "only the squatter's task");
        let recs = run_history(&host, "capture").await;
        assert_eq!(recs.len(), 1);
        assert!(!recs[0].action_ok);
        assert!(recs[0].detail.contains("already exists"));
    });
}

/// Two rules carrying the SAME task prefix fire on one post. The composed id
/// names its rule, so they are two distinct tasks and the post commits — it
/// used to be one id composed twice, the second create failing at tasks and
/// unwinding the poster's message.
#[test]
fn two_rules_sharing_a_prefix_both_fire_and_the_post_commits() {
    block_on(async {
        let mut host = genesis().await;
        for rule_id in ["r1", "r2"] {
            let (ctx, msg) = from_user(create_rule_msg(
                rule_id,
                Trigger {
                    channel_id: None,
                    mention: None,
                    text_contains: None,
                },
                Action::CreateTask {
                    task_id_prefix: "auto".into(),
                    title_template: "T".into(),
                },
            ));
            host.submit_at(ctx, msg).await.expect("create rule");
        }

        host.submit_at(
            BlockContext {
                height: 2,
                consensus_time: 200,
                origin: Origin::External(b"poster".to_vec()),
            },
            chat_event_msg("general", 5, Party::Key(vec![1; 4])),
        )
        .await
        .expect("two rules on one prefix must not abort the post");

        let ids: Vec<String> = tasks_of(&host).await.into_iter().map(|t| t.id).collect();
        assert_eq!(ids, ["auto-r1-general-5", "auto-r2-general-5"]);
        for rule_id in ["r1", "r2"] {
            let recs = run_history(&host, rule_id).await;
            assert_eq!(recs.len(), 1, "{rule_id} fired once");
            assert!(recs[0].action_ok, "{rule_id}: {}", recs[0].detail);
        }
    });
}

/// One task short of the owner's cap, two rules fire on one post. The census
/// each probe reads cannot see the other rule's create, so the reservation
/// is what keeps the second from emitting a create tasks would refuse —
/// which would unwind the post. The first takes the last slot; the second is
/// recorded as refused and the post commits.
#[test]
fn a_second_rule_refuses_the_slot_the_first_one_took() {
    block_on(async {
        let mut host = genesis().await;
        // the rule owner is the identity genesis founded (account 1), which
        // is also who `from_user` submits the rules as.
        let owner = 1u64;
        let last = tasks::MAX_OPEN_TASKS_PER_OWNER as u64 - 1;
        for n in 0..last {
            host.submit_at(
                BlockContext {
                    height: 1,
                    consensus_time: 100 + n,
                    origin: Origin::External(vec![9; 32]),
                },
                Msg {
                    target: TASKS.into(),
                    payload: tasks::encode_task_msg(&tasks::TaskMsg::CreateTask {
                        task_id: format!("fill-{n}"),
                        title: "filler".into(),
                        owner: Some(owner),
                    }),
                },
            )
            .await
            .expect("fill the owner's board");
        }

        for (rule_id, prefix) in [("r1", "one"), ("r2", "two")] {
            let (ctx, msg) = from_user(create_rule_msg(
                rule_id,
                Trigger {
                    channel_id: None,
                    mention: None,
                    text_contains: None,
                },
                Action::CreateTask {
                    task_id_prefix: prefix.into(),
                    title_template: "T".into(),
                },
            ));
            host.submit_at(ctx, msg).await.expect("create rule");
        }

        host.submit_at(
            BlockContext {
                height: 2,
                consensus_time: 200,
                origin: Origin::External(b"poster".to_vec()),
            },
            chat_event_msg("general", 5, Party::Key(vec![1; 4])),
        )
        .await
        .expect("the owner's full board must not abort the post");

        assert_eq!(
            tasks_of(&host).await.len() as u64,
            tasks::MAX_OPEN_TASKS_PER_OWNER as u64,
            "the board holds exactly its cap"
        );
        let first = run_history(&host, "r1").await;
        assert!(first[0].action_ok, "the first rule took the last slot");
        let second = run_history(&host, "r2").await;
        assert!(!second[0].action_ok, "the second rule found none left");
        assert!(
            second[0].detail.contains("at task cap"),
            "unexpected detail: {}",
            second[0].detail
        );
    });
}
