//! Pure, network-persisted installation identity and the explicit initializer.
use std::collections::BTreeMap;

use crate::wire::{agent, runs};
use agent::{Continuation, Decode, Predicate, Program, Step, Value};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Plan {
    pub chief_id: String,
    pub controller: u64,
    pub namespace: String,
    pub channel_id: String,
    pub existing_channel: bool,
    pub home_page_id: String,
    pub board_page_id: String,
    pub inbox_page_id: String,
    pub worker_agent_id: String,
}

impl Plan {
    pub fn new(
        controller: u64,
        chief_id: &str,
        channel: Option<&str>,
        worker: &str,
    ) -> Result<Self, String> {
        let valid_id = !chief_id.is_empty()
            && chief_id.len() <= 48
            && chief_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
        if !valid_id {
            return Err("chief-id must be 1..48 ASCII letters, digits, '-' or '_'".into());
        }
        let valid_worker = !worker.trim().is_empty() && !worker.chars().any(char::is_control);
        if !valid_worker {
            return Err("an existing worker model id is required".into());
        }
        let digest = Sha256::digest(sdk::wire::encode(&(controller, chief_id)));
        let namespace = format!("chief-{}", hex(&digest[..16]));
        if worker == namespace {
            return Err("Chief cannot be its own worker model".into());
        }
        if let Some(channel) = channel {
            let valid_channel =
                !channel.trim().is_empty() && !channel.chars().any(char::is_control);
            if !valid_channel {
                return Err("invalid existing channel id".into());
            }
        }
        Ok(Self {
            chief_id: chief_id.into(),
            controller,
            channel_id: channel.unwrap_or(&namespace).into(),
            existing_channel: channel.is_some(),
            home_page_id: format!("{namespace}-home"),
            board_page_id: format!("{namespace}-board"),
            inbox_page_id: format!("{namespace}-inbox"),
            worker_agent_id: worker.into(),
            namespace,
        })
    }
    pub fn root(&self) -> String {
        format!("/home/{}/chief/{}", self.controller, self.chief_id)
    }
    pub fn config(&self) -> serde_json::Value {
        serde_json::json!({"agentId": self.namespace, "conversationId": self.namespace,
            "homePageId": self.home_page_id, "boardPageId": self.board_page_id,
            "inboxPageId": self.inbox_page_id, "workerAgentId": self.worker_agent_id})
    }
    pub fn pages(&self) -> [(&str, &str); 3] {
        [
            (&self.home_page_id, "Chief"),
            (&self.board_page_id, "Chief board"),
            (&self.inbox_page_id, "Chief inbox"),
        ]
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Control {
    Pause,
    Resume,
}

pub(crate) fn control_messages(
    state: &runs::ConversationView,
    action: Control,
    operation: &str,
) -> Result<Vec<runs::RunsMsg>, String> {
    match action {
        Control::Pause => pause_messages(state, operation),
        Control::Resume => resume_messages(state, operation),
    }
}
fn activation(state: &runs::ConversationView, operation: &str, active: bool) -> runs::RunsMsg {
    runs::RunsMsg::ActivateConversation {
        conversation_id: state.conversation_id.clone(),
        operation_id: format!("{operation}-activate"),
        active,
    }
}
fn pause_messages(
    state: &runs::ConversationView,
    operation: &str,
) -> Result<Vec<runs::RunsMsg>, String> {
    Ok(vec![activation(state, operation, false)])
}
fn resume_messages(
    state: &runs::ConversationView,
    operation: &str,
) -> Result<Vec<runs::RunsMsg>, String> {
    let Some(turn) = &state.active_turn else {
        return Ok(vec![activation(state, operation, true)]);
    };
    match turn.phase {
        runs::ConversationTurnPhase::Draining => resume_draining(state, turn, operation),
        runs::ConversationTurnPhase::AwaitingProgram => resume_awaiting_program(state, operation),
        runs::ConversationTurnPhase::Queued | runs::ConversationTurnPhase::Running => {
            Ok(vec![activation(state, operation, true)])
        }
        runs::ConversationTurnPhase::Settled => {
            Err("Chief retains an invalid settled active turn".into())
        }
    }
}
fn resume_awaiting_program(
    state: &runs::ConversationView,
    operation: &str,
) -> Result<Vec<runs::RunsMsg>, String> {
    let reaction_stopped = matches!(state.status, runs::ConversationStatus::Paused { .. });
    if reaction_stopped {
        return Err("Chief program reaction is paused before native execution; repair that reaction before resuming".into());
    }
    Ok(vec![activation(state, operation, true)])
}
fn resume_draining(
    state: &runs::ConversationView,
    turn: &runs::ConversationTurn,
    operation: &str,
) -> Result<Vec<runs::RunsMsg>, String> {
    let interrupted = turn.outcome == Some(runs::RunOutcome::Failed)
        || matches!(state.status, runs::ConversationStatus::Paused { .. });
    if !interrupted {
        return Ok(vec![activation(state, operation, true)]);
    }
    let effects_settled = turn.drained_actions == turn.actions.len() as u64;
    if !effects_settled {
        return Err(
            "Chief still has unsettled execution effects; resume after their receipts settle"
                .into(),
        );
    }
    // The explicit user resume authorizes recovery. Runs carries forward the
    // last committed checkpoint and fences the old turn; the CLI never writes
    // native history, consumes its input cursor, or reconfigures the identity.
    Ok(vec![
        runs::RunsMsg::RetryConversationTurn {
            conversation_id: state.conversation_id.clone(),
            operation_id: format!("{operation}-retry"),
        },
        activation(state, operation, true),
    ])
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub(crate) fn literal(value: serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(v) => Value::Bool(v),
        serde_json::Value::Number(v) => Value::Number(
            v.as_i64()
                .map(i128::from)
                .unwrap_or_else(|| i128::from(v.as_u64().expect("integer wire"))),
        ),
        serde_json::Value::String(v) => Value::Text(v),
        serde_json::Value::Array(v) => Value::List(v.into_iter().map(literal).collect()),
        serde_json::Value::Object(v) => {
            Value::Map(v.into_iter().map(|(k, v)| (k, literal(v))).collect())
        }
    }
}
fn reference(path: &[&str]) -> Value {
    Value::Ref(path.iter().map(|p| (*p).into()).collect())
}
fn object(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Map(
        fields
            .into_iter()
            .map(|(k, v)| (k.into(), v))
            .collect::<BTreeMap<_, _>>(),
    )
}
fn call(module: &str, msg: Value, bind: &str) -> Step {
    Step::Call {
        module: module.into(),
        msg,
        bind: bind.into(),
        decode: Decode::Json,
        on_failure: Continuation::Unhandled,
    }
}
fn ensure(
    steps: &mut Vec<Step>,
    module: &str,
    query: serde_json::Value,
    bind: &str,
    test: Predicate,
    msg: Value,
) {
    steps.push(Step::Query {
        module: module.into(),
        query: literal(query),
        bind: bind.into(),
    });
    let at = steps.len() as u64;
    steps.push(Step::Branch {
        test,
        then: at + 2,
        or: at + 1,
    });
    steps.push(call(module, msg, &format!("{bind}_created")));
}
fn equals(left: Value, right: Value) -> Predicate {
    Predicate::Equals { left, right }
}

/// Only Agent's explicit initialization attribution enters this prefix. Every
/// call waits for its actual outcome. The final activation uses a fixed Runs
/// operation identity, so a later initialization never undoes a human pause.
pub(crate) fn program(plan: &Plan, package: &run_envelope::ConversationPackage) -> (Program, u64) {
    let mut steps = vec![Step::Branch {
        test: Predicate::All(vec![
            equals(
                reference(&["change", "source", "module"]),
                Value::Text("agent".into()),
            ),
            equals(
                reference(&["change", "source", "kind"]),
                Value::Text(agent::INITIALIZATION_KIND.into()),
            ),
            equals(reference(&["change", "kind"]), Value::Text("added".into())),
            equals(
                reference(&["change", "reason"]),
                object([("defined", Value::Text(agent::INITIALIZATION_REASON.into()))]),
            ),
        ]),
        then: 1,
        or: 0,
    }];
    steps.push(Step::Query {
        module: "runs".into(),
        query: literal(serde_json::json!({"conversation":{"conversation_id":plan.namespace}})),
        bind: "chief_existing_conversation".into(),
    });
    steps.push(Step::Branch {
        test: Predicate::Defined(reference(&["chief_existing_conversation", "conversation"])),
        then: 0,
        or: 3,
    });
    if !plan.existing_channel {
        let owner = reference(&["chief_channel", "channel", "owner", "account"]);
        let channel_exists = Predicate::All(vec![
            Predicate::Defined(owner.clone()),
            equals(owner, reference(&["account"])),
        ]);
        ensure(
            &mut steps,
            "chat",
            serde_json::json!({"channel":{"channel_id":plan.channel_id}}),
            "chief_channel",
            channel_exists,
            literal(
                serde_json::json!({"create_channel":{"channel_id":plan.channel_id,"name":"Chief","post_policy":"open"}}),
            ),
        );
    }
    let workspace_guidance = "Request work changes in the shared Chat channel bound to this Chief. The Chief board contains task, ask, and rule pages. Answer an ask with a fresh comment on its page. Chief writes managed page bodies; worker completion is a claim until Chief records acceptance.";
    for (index, (page, title)) in plan.pages().into_iter().enumerate() {
        let bind = format!("chief_page_{index}");
        ensure(
            &mut steps,
            "pages",
            serde_json::json!({"get_block":{"block_id":page}}),
            &bind,
            Predicate::Defined(reference(&[&bind, "block"])),
            literal(serde_json::json!({"create_page":{
                "page_id":page,"title":title,"blocks":[{
                    "id":format!("{page}-guide"),"kind":"paragraph","text":workspace_guidance
                }]
            }})),
        );
        // This owner-checked idempotent call also refuses an existing foreign
        // collection. Never skip it merely because someone occupied our ID.
        steps.push(call(
            "pages",
            literal(serde_json::json!({"create_record_collection":{
                "page_id":page,"request_id":format!("{}-collection-{index}",plan.namespace)
            }})),
            &format!("chief_collection_{index}"),
        ));
    }
    ensure(
        &mut steps,
        "runs",
        serde_json::to_value(runs::RunsQuery::Model {
            query: runs::ModelQuery::Agent {
                agent_id: plan.namespace.clone(),
            },
        })
        .expect("model query wire"),
        "chief_model",
        Predicate::All(vec![
            Predicate::Defined(reference(&["chief_model", "model", "agent"])),
            equals(
                reference(&["chief_model", "model", "agent", "account"]),
                reference(&["account"]),
            ),
        ]),
        object([(
            "configure_model",
            object([(
                "operation",
                object([(
                    "register_model",
                    object([
                        ("account", reference(&["account"])),
                        ("agent_id", Value::Text(plan.namespace.clone())),
                        ("display_name", Value::Text("Chief".into())),
                        ("capability", Value::Text("pi".into())),
                    ]),
                )]),
            )]),
        )]),
    );
    let configure = steps.len() as u64;
    let Step::Branch { then, .. } = &mut steps[2] else {
        unreachable!()
    };
    *then = configure;
    steps.push(call("runs", literal(serde_json::json!({"configure_conversation":{
        "conversation_id":plan.namespace,"agent_id":plan.namespace,"source":{"channel":{"channel_id":plan.channel_id}},
        "history_prefix":format!("/shared/conversations/{}/history",plan.namespace),"session_path":"session.jsonl","packages":[package]
    }})), "chief_conversation"));
    steps.push(call("runs", literal(serde_json::json!({"activate_conversation":{
        "conversation_id":plan.namespace,"operation_id":format!("{}-activate",plan.namespace),"active":true
    }})), "chief_activation"));
    steps.push(Step::Finish);
    let offset = steps.len() as u64;
    let Step::Branch { or, .. } = &mut steps[0] else {
        unreachable!()
    };
    *or = offset;
    let mut model = runs::conversation_program(&plan.namespace);
    relocate(&mut model, offset);
    steps.extend(model.steps);
    (Program { steps }, offset)
}

/// Relocate both VM continuations AND the Runs action protocol's absolute call
/// positions. Changing only branches leaves claims pointing into the initializer.
pub(crate) fn relocate(program: &mut Program, offset: u64) {
    for step in &mut program.steps {
        match step {
            Step::Branch { then, or, .. } => {
                *then += offset;
                *or += offset;
            }
            Step::Call {
                msg, on_failure, ..
            } => {
                relocate_failure(on_failure, offset);
                relocate_field(msg, &["claim_action_request", "target_step"], offset);
                relocate_field(msg, &["complete_action_request", "call", "step"], offset);
            }
            Step::Dispatch { on_failure, .. } => relocate_failure(on_failure, offset),
            Step::Query { .. } | Step::Report { .. } | Step::Finish => {}
        }
    }
}
fn relocate_field(value: &mut Value, path: &[&str], offset: u64) {
    let Some((key, rest)) = path.split_first() else {
        if let Value::Number(index) = value {
            *index += i128::from(offset);
        }
        return;
    };
    let Value::Map(fields) = value else {
        return;
    };
    let Some(child) = fields.get_mut(*key) else {
        return;
    };
    relocate_field(child, rest, offset);
}
fn relocate_failure(continuation: &mut Continuation, offset: u64) {
    if let Continuation::Step(index) = continuation {
        *index += offset;
    }
}
