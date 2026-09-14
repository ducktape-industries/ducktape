//! The default model workflow is an ordinary program that the controller may replace.
use agent::{Continuation, Decode, Predicate, Program, Step, Value};
use std::collections::BTreeMap;

fn object(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Map(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect::<BTreeMap<_, _>>(),
    )
}
fn reference(path: &[&str]) -> Value {
    Value::Ref(path.iter().map(|value| (*value).into()).collect())
}
fn text(value: &str) -> Value {
    Value::Text(value.into())
}
fn equals(left: Value, right: Value) -> Predicate {
    Predicate::Equals { left, right }
}
fn operation(name: &'static str, fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    object([(name, object(fields))])
}
fn call(module: &str, msg: Value, bind: &str, failure: u64) -> Step {
    Step::Call {
        module: module.into(),
        msg,
        bind: bind.into(),
        decode: Decode::Json,
        on_failure: Continuation::Step(failure),
    }
}

/// React to a mention by requesting model work, and to a run's tool request by
/// executing its exact prepared message as this user. Every action, including its
/// failure, reaches a verified runs receipt. Reports are the controller's choice.
pub fn model_program(agent_id: &str) -> Program {
    program(agent_id, Intake::Mention)
}

/// Resident coordination admits every authenticated source change into Runs'
/// durable queue. Source identity, idempotency and self-reply suppression stay
/// in Runs; the program carries no domain-specific event policy.
pub fn conversation_program(agent_id: &str) -> Program {
    program(agent_id, Intake::Conversation)
}

enum Intake {
    Mention,
    Conversation,
}

fn program(agent_id: &str, intake: Intake) -> Program {
    let request = || reference(&["change", "source", "object"]);
    let mut steps = vec![
        Step::Branch {
            test: Predicate::All(vec![
                equals(reference(&["change", "source", "module"]), text("runs")),
                equals(
                    reference(&["change", "source", "kind"]),
                    text("action_request"),
                ),
                equals(reference(&["change", "kind"]), text("added")),
            ]),
            then: 1,
            or: 0,
        },
        Step::Query {
            module: "runs".into(),
            query: operation("action_plan", [("request_id", request())]),
            bind: "proposal".into(),
        },
    ];
    for module in ["chat", "pages", "tasks", "files", "forge", "runs"] {
        let route = steps.len() as u64;
        let claim = route + 1;
        let target = route + 2;
        let complete = route + 3;
        let finish = route + 4;
        steps.push(Step::Branch {
            test: equals(
                reference(&["proposal", "action_request", "target"]),
                text(module),
            ),
            then: claim,
            or: finish + 1,
        });
        steps.push(call(
            "runs",
            operation(
                "claim_action_request",
                [
                    ("request_id", request()),
                    ("target_step", Value::Number(target.into())),
                ],
            ),
            "plan",
            finish,
        ));
        steps.push(call(
            module,
            reference(&["plan", "applied", "output", "payload"]),
            "effect",
            complete,
        ));
        steps.push(call(
            "runs",
            operation(
                "complete_action_request",
                [
                    ("request_id", request()),
                    ("result", reference(&["effect"])),
                    (
                        "call",
                        object([
                            (
                                "requester",
                                reference(&["plan", "applied", "output", "requester"]),
                            ),
                            (
                                "invocation",
                                reference(&["plan", "applied", "output", "invocation"]),
                            ),
                            ("step", Value::Number(target.into())),
                        ]),
                    ),
                ],
            ),
            "receipt",
            finish,
        ));
        steps.push(Step::Finish);
    }
    let unsupported = steps.len() as u64;
    steps.push(call(
        "runs",
        operation(
            "reject_action_request",
            [
                ("request_id", request()),
                (
                    "reason",
                    text("the model program has no route for this action target"),
                ),
            ],
        ),
        "rejection",
        unsupported + 1,
    ));
    steps.push(Step::Finish);
    let mention = steps.len() as u64;
    let mention_intake = Predicate::All(vec![
        equals(reference(&["change", "kind"]), text("added")),
        Predicate::Any(vec![
            Predicate::All(vec![
                equals(reference(&["change", "reason"]), text("mention")),
                Predicate::Any(vec![
                    equals(reference(&["change", "source", "module"]), text("chat")),
                    equals(reference(&["change", "source", "module"]), text("pages")),
                ]),
            ]),
            Predicate::All(vec![
                equals(reference(&["change", "source", "module"]), text("runs")),
                equals(
                    reference(&["change", "source", "kind"]),
                    text("run_request"),
                ),
            ]),
        ]),
    ]);
    let admission = match intake {
        Intake::Mention => mention_intake,
        Intake::Conversation => Predicate::Defined(reference(&["change", "seq"])),
    };
    steps.push(Step::Branch {
        test: admission,
        then: mention + 1,
        or: mention + 2,
    });
    steps.push(call(
        "runs",
        operation(
            "request_attributed_run",
            [
                ("agent_id", text(agent_id)),
                ("change_seq", reference(&["change", "seq"])),
            ],
        ),
        "run",
        mention + 2,
    ));
    steps.push(Step::Finish);
    if let Step::Branch { or, .. } = &mut steps[0] {
        *or = mention;
    }
    Program { steps }
}
