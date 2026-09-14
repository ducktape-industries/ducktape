#![allow(dead_code)]
//! Real host regression: mentions reach a keyless model program, and external
//! session credentials propose work whose actual target runs as that account.
use host::{BlockContext, Host};
use sdk::{Msg, Origin};
use sdk_testkit::MemStore;

pub fn store() -> Box<dyn sdk::MerkleStore> {
    Box::new(MemStore::new())
}
pub fn member() -> Origin {
    Origin::External(vec![1; 32])
}
pub fn provider() -> Origin {
    Origin::External(vec![8; 32])
}
pub fn session() -> Origin {
    Origin::External(vec![9; 32])
}
fn context(height: u64, origin: Origin) -> BlockContext {
    BlockContext {
        height,
        consensus_time: height,
        origin,
    }
}
pub fn msg<T: serde::Serialize>(target: &str, payload: &T) -> Msg {
    Msg {
        target: target.into(),
        payload: sdk::wire::encode(payload),
    }
}

pub struct Network {
    pub host: Host,
    pub height: u64,
    pub events: Vec<sdk::Event>,
}
impl Network {
    pub async fn new() -> Self {
        let mut valset = valset::Valset::new("valset", store(), "governance");
        valset.seed(vec![8; 32]).await.unwrap();
        valset.seed(vec![7; 32]).await.unwrap();
        valset.finish_seed().await.unwrap();
        let host = Host::genesis(vec![
            Box::new(identity::Identity::new(
                "identity",
                store(),
                "runs-test".into(),
            )),
            Box::new(
                attribution::AttributionModule::new("attribution", store())
                    .with_subscribers(["agent"]),
            ),
            Box::new(agent::AgentModule::new(
                "agent",
                store(),
                agent::Siblings {
                    identity: "identity".into(),
                    attribution: "attribution".into(),
                    dispatch: "dispatch".into(),
                },
            )),
            Box::new(
                chat::Chat::new("chat", store())
                    .with_identity("identity")
                    .with_attribution("attribution"),
            ),
            Box::new(
                pages::Pages::new("pages", store())
                    .with_identity("identity")
                    .with_attribution("attribution"),
            ),
            Box::new(valset),
            Box::new(capability::CapabilityRegistry::new(
                "capability",
                store(),
                Some("valset".into()),
            )),
            Box::new(saga::SagaModule::with_assignment(
                "saga",
                store(),
                "valset",
                "capability",
                saga::LeasePolicy::Open,
            )),
            Box::new(dispatch::DispatchModule::new(
                "dispatch",
                "saga",
                "identity",
                store(),
            )),
            Box::new(tasks::Tasks::new(
                "tasks",
                "identity",
                "attribution",
                store(),
            )),
            Box::new(
                runs::RunsModule::new(
                    "runs",
                    "chat",
                    "saga",
                    "attribution",
                    "dispatch",
                    "agent",
                    Some("tasks".into()),
                    Some("tasks".into()),
                )
                .with_pages_module("pages")
                .with_sink_forge("forge"),
            ),
        ])
        .unwrap();
        Self {
            host,
            height: 0,
            events: Vec::new(),
        }
    }
    pub async fn submit(&mut self, origin: Origin, message: Msg) {
        self.height += 1;
        let outcome = self
            .host
            .submit_at(context(self.height, origin), message)
            .await
            .unwrap();
        self.events.extend(outcome.events);
    }
    pub async fn step(&mut self) {
        self.height += 1;
        let outcome = self
            .host
            .submit_block(context(self.height, Origin::System), Vec::new())
            .await
            .unwrap();
        self.events.extend(outcome.events);
    }
    pub async fn drain(&mut self) {
        // Every step executes the host's next committed queue batch. There is
        // no clock or external worker to poll in this deterministic drain.
        while self.host.has_pending_work().await.unwrap() {
            self.step().await;
        }
    }
    pub async fn runs(&self) -> Vec<runs::PendingRun> {
        let bytes = self
            .host
            .query("runs", &runs::encode_query(&runs::RunsQuery::PendingRuns))
            .await
            .unwrap();
        let runs::RunsReply::PendingRuns(runs) = runs::decode_reply(&bytes).unwrap() else {
            panic!("pending runs reply");
        };
        runs
    }
    pub async fn action(&self, id: &str) -> runs::ActionRequestView {
        let bytes = self
            .host
            .query(
                "runs",
                &runs::encode_query(&runs::RunsQuery::ActionRequest {
                    request_id: id.into(),
                }),
            )
            .await
            .unwrap();
        let runs::RunsReply::ActionRequest(Some(request)) = runs::decode_reply(&bytes).unwrap()
        else {
            panic!("action request reply");
        };
        request
    }
    pub async fn task(&self, id: &str) -> Option<tasks::Task> {
        let bytes = self
            .host
            .query(
                "tasks",
                &tasks::encode_task_query(&tasks::TaskQuery::Get { task_id: id.into() }),
            )
            .await
            .unwrap();
        let tasks::TaskReply::Task(task) = tasks::decode_task_reply(&bytes).unwrap() else {
            panic!("task reply");
        };
        task
    }
    pub async fn provision(&mut self) -> String {
        self.provision_program(runs::model_program("builder")).await
    }
    pub async fn provision_program(&mut self, program: agent::Program) -> String {
        self.submit(
            provider(),
            msg(
                "capability",
                &capability::CapabilityMsg::Announce {
                    capabilities: vec!["model-1".into()],
                    resources: Default::default(),
                },
            ),
        )
        .await;
        self.submit(
            member(),
            msg(
                "identity",
                &identity::IdentityMsg::Create {
                    name: "Alice".into(),
                    scheme: identity::KeyScheme::Ed25519,
                },
            ),
        )
        .await;
        self.submit(
            member(),
            msg(
                "agent",
                &agent::AgentMsg::Provision {
                    request_id: "builder".into(),
                    name: "Builder".into(),
                    program,
                },
            ),
        )
        .await;
        self.submit(
            member(),
            msg(
                "runs",
                &runs::RunsMsg::ConfigureModel {
                    operation: runs::ModelMsg::RegisterModel {
                        account: 2,
                        agent_id: "builder".into(),
                        display_name: "Builder".into(),
                        capability: "model-1".into(),
                        recipe_hash: None,
                        skills: None,
                    },
                },
            ),
        )
        .await;
        self.submit(
            member(),
            msg(
                "chat",
                &chat::ChatMsg::CreateChannel {
                    channel_id: "general".into(),
                    name: "General".into(),
                    post_policy: chat::PostPolicy::Open,
                },
            ),
        )
        .await;
        self.submit(
            member(),
            msg(
                "chat",
                &chat::ChatMsg::PostMessage {
                    channel_id: "general".into(),
                    message_id: "mention".into(),
                    thread: None,
                    blocks: vec![chat::Block::Paragraph(vec![chat::Span {
                        text: "Builder, create a task".into(),
                        marks: vec![chat::Mark::Mention(chat::Party::Account(2))],
                    }])],
                },
            ),
        )
        .await;
        assert!(
            self.runs().await.is_empty(),
            "the source post commits before its reaction"
        );
        self.drain().await;
        let pending = self.runs().await;
        assert_eq!(
            pending.len(),
            1,
            "the real default program requested model work"
        );
        let run = &pending[0];
        let bytes = self
            .host
            .query(
                "dispatch",
                &dispatch::encode_query(&dispatch::DispatchQuery::Dispatch {
                    receiver: "runs".into(),
                    dispatch_id: run.dispatch_id.clone(),
                }),
            )
            .await
            .unwrap();
        let dispatch::DispatchReply::Dispatch(Some(dispatched)) =
            dispatch::decode_reply(&bytes).unwrap()
        else {
            panic!("dispatch reply");
        };
        let dispatch::DispatchStatus::AwaitingResult { saga_id } = dispatched.status else {
            panic!("work was not announced");
        };
        self.submit(
            provider(),
            msg(
                "saga",
                &saga::SagaMsg::Accept {
                    saga_id,
                    attempt: 0,
                },
            ),
        )
        .await;
        self.submit(
            provider(),
            msg(
                "runs",
                &runs::RunsMsg::OpenAgentSession {
                    attempt: 0,
                    run_id: run.run_id.clone(),
                    session_key: vec![9; 32],
                },
            ),
        )
        .await;
        run.run_id.clone()
    }
}

// ---- catalog envelopes ---------------------------------------------------------
// the operations tests submit, built the way an agent builds them: an
// operation name, an optional target, an input — never a typed variant.

pub fn envelope(
    operation: &str,
    target: Option<serde_json::Value>,
    input: serde_json::Value,
) -> runs::ActionEnvelope {
    runs::ActionEnvelope::new(operation, target, input)
}

pub fn text_content(text: impl Into<String>) -> serde_json::Value {
    serde_json::json!({"content": [{"type": "text", "text": text.into()}]})
}

pub fn reply(text: impl Into<String>) -> runs::ActionEnvelope {
    envelope(runs::OP_REPLY, None, text_content(text))
}

pub fn post_message(
    channel_id: impl Into<String>,
    text: impl Into<String>,
    thread: Option<u64>,
) -> runs::ActionEnvelope {
    let mut target = serde_json::json!({"channel_id": channel_id.into()});
    if let Some(root) = thread {
        target["thread"] = root.into();
    }
    envelope(runs::OP_CHAT_POST_MESSAGE, Some(target), text_content(text))
}

pub fn page_comment(target: impl Into<String>, text: impl Into<String>) -> runs::ActionEnvelope {
    envelope(
        runs::OP_PAGES_COMMENT,
        Some(serde_json::json!({"target": target.into()})),
        text_content(text),
    )
}

pub fn page_thread_comment(
    thread_id: impl Into<String>,
    text: impl Into<String>,
) -> runs::ActionEnvelope {
    envelope(
        runs::OP_PAGES_COMMENT,
        Some(serde_json::json!({"thread_id": thread_id.into()})),
        text_content(text),
    )
}

pub fn set_page_checked(block_id: impl Into<String>, checked: bool) -> runs::ActionEnvelope {
    envelope(
        runs::OP_PAGES_SET_CHECKED,
        Some(serde_json::json!({"block_id": block_id.into()})),
        serde_json::json!({"checked": checked}),
    )
}

pub fn job_comment(job_id: impl Into<String>, text: impl Into<String>) -> runs::ActionEnvelope {
    envelope(
        runs::OP_JOBS_COMMENT,
        Some(serde_json::json!({"job_id": job_id.into()})),
        text_content(text),
    )
}

pub fn create_task(task_id: impl Into<String>, title: impl Into<String>) -> runs::ActionEnvelope {
    envelope(
        runs::OP_TASKS_CREATE,
        None,
        serde_json::json!({"task_id": task_id.into(), "title": title.into()}),
    )
}

pub fn update_task_status(
    task_id: impl Into<String>,
    status: impl Into<String>,
) -> runs::ActionEnvelope {
    envelope(
        runs::OP_TASKS_UPDATE_STATUS,
        Some(serde_json::json!({"task_id": task_id.into()})),
        serde_json::json!({"status": status.into()}),
    )
}

pub fn duckfs_write_text(
    path: impl Into<String>,
    text: impl Into<String>,
    base_snapshot: Option<String>,
) -> runs::ActionEnvelope {
    let mut input = serde_json::json!({"text": text.into()});
    if let Some(base) = base_snapshot {
        input["base_snapshot"] = base.into();
    }
    envelope(
        runs::OP_DUCKFS_WRITE_TEXT,
        Some(serde_json::json!({"path": path.into()})),
        input,
    )
}

pub fn update_module(spec: runs::ModuleUpdateSpec) -> runs::ActionEnvelope {
    envelope(
        runs::OP_MODULES_UPDATE,
        None,
        serde_json::to_value(spec).expect("module update specs serialize"),
    )
}

pub fn agent_call(
    agent_id: impl Into<String>,
    instruction: impl Into<String>,
) -> runs::ActionEnvelope {
    envelope(
        runs::OP_AGENT_CALL,
        Some(serde_json::json!({"agent_id": agent_id.into()})),
        serde_json::json!({"instruction": instruction.into()}),
    )
}
