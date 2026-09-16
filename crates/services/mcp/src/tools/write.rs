//! The one write entry point. `ducktape_action` carries a catalog envelope —
//! an operation name, an optional target, an input — and the caller's
//! `request_id` through this run's scoped host endpoint. Runs decodes the
//! envelope against the schemas it owns, validates the session and lease, and
//! the account's program executes the prepared target message; the endpoint
//! waits for the committed receipt before returning it. The private session
//! signer stays on the host, and this binary interprets nothing: adding an
//! operation to the runs catalog needs no change here, and the `submit`
//! operation carries any module's own message without one.

use serde_json::{Value, json};

use runs::ActionEnvelope;

use super::{Tool, arg_str};
use crate::identity::Run;
use crate::node::{NodeError, Result};

pub(super) fn tools() -> Vec<Tool> {
    vec![Tool {
        name: "ducktape_action",
        description: "Propose one write as this run's program account. operation names an entry \
                      of the catalog ducktape_actions lists (reply, react, unreact, \
                      chat.post_message, tasks.create, tasks.update_status, pages.comment, \
                      pages.set_checked, pages.post, jobs.comment, duckfs.write_text, \
                      collaboration.deliver, collaboration.acknowledge, agent.call, submit); \
                      target and input follow that entry's schemas — \
                      reply, react and unreact take no target and act on the message this run \
                      was called from. submit carries ANY module's own message: target is \
                      {\"module\": id} and input is the message exactly as that module's wire \
                      spells it (an object with exactly one key, or a bare string), so \
                      whatever a member may submit to a module, you may. request_id is your \
                      idempotency key within this run: the same id with the same bytes returns \
                      the same receipt, the same id with different bytes is refused. Ducktape \
                      validates the message on every validator and returns the committed \
                      receipt; a refusal is the target module's own words about what it could \
                      not accept.",
        schema: action_schema,
        handler: action,
    }]
}

fn action_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "operation": {
                "type": "string",
                "description": "A catalog operation name from ducktape_actions.",
            },
            "target": {
                "type": "object",
                "description": "The destination the operation acts on, per its catalog target schema. Omit for operations that take none.",
            },
            "input": {
                "type": ["object", "string"],
                "description": "The operation's input, per its catalog input schema; for submit, the module's own message (an object with one key, or a bare string).",
            },
            "request_id": {
                "type": "string",
                "description": "Your idempotency key for this write within the run.",
            },
        },
        "required": ["operation", "input", "request_id"],
        "additionalProperties": false,
    })
}

/// The envelope exactly as the caller shaped it. Nothing is decoded here: the
/// catalog is the module's, and its refusal names the field it could not read.
fn envelope(args: &Value) -> Result<ActionEnvelope> {
    let operation = arg_str(args, "operation")?;
    let target = match args.get("target") {
        None | Some(Value::Null) => None,
        Some(target @ Value::Object(_)) => Some(target.clone()),
        Some(_) => {
            return Err(NodeError::Rejected(
                "this tool needs an object \"target\" argument when one is given".into(),
            ));
        }
    };
    let input = match args.get("input") {
        Some(input @ (Value::Object(_) | Value::String(_))) => input.clone(),
        _ => {
            return Err(NodeError::Rejected(
                "this tool needs an object or string \"input\" argument".into(),
            ));
        }
    };
    Ok(ActionEnvelope::new(operation, target, input))
}

fn action(run: &Run, args: &Value) -> Result<Value> {
    let request_id = arg_str(args, "request_id")?;
    run.act(request_id, envelope(args)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_envelope_is_carried_opaque_with_an_optional_target() {
        let with_target = envelope(&json!({
            "operation": "chat.post_message",
            "target": {"channel_id": "general", "thread": 3},
            "input": {"content": [{"type": "text", "text": "hi"}]},
            "request_id": "r1",
        }))
        .unwrap();
        assert_eq!(with_target.operation, "chat.post_message");
        assert_eq!(
            with_target.target,
            Some(json!({"channel_id": "general", "thread": 3}))
        );
        // an operation this binary has never heard of still travels: the
        // catalog that decides is the module's.
        let unknown = envelope(&json!({
            "operation": "future.op",
            "input": {"anything": true},
            "request_id": "r2",
        }))
        .unwrap();
        assert_eq!(unknown.target, None);
        assert_eq!(unknown.input, json!({"anything": true}));
        // a bare string is a module message too (a unit variant on its wire).
        let bare = envelope(&json!({
            "operation": runs::OP_SUBMIT,
            "target": {"module": "runs"},
            "input": "pending_runs",
            "request_id": "r3",
        }))
        .unwrap();
        assert_eq!(bare.input, json!("pending_runs"));
    }

    #[test]
    fn malformed_arguments_are_refused_by_name() {
        for (args, needle) in [
            (json!({"input": {}, "request_id": "r"}), "operation"),
            (json!({"operation": "reply", "request_id": "r"}), "input"),
            (
                json!({"operation": "reply", "input": {}, "target": "general", "request_id": "r"}),
                "target",
            ),
        ] {
            let error = envelope(&args).unwrap_err();
            assert!(
                matches!(&error, NodeError::Rejected(m) if m.contains(needle)),
                "{args} -> {error:?}"
            );
        }
        let error = action(
            &Run::from_env(),
            &json!({"operation": "reply", "input": {}}),
        )
        .unwrap_err();
        assert!(matches!(&error, NodeError::Rejected(m) if m.contains("request_id")));
    }

    #[test]
    fn the_schema_requires_exactly_the_envelope_and_its_key() {
        let schema = action_schema();
        assert_eq!(
            schema["required"],
            json!(["operation", "input", "request_id"])
        );
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["target"]["type"], "object");
        // the operation is free text on purpose: an enum here would be a second
        // catalog that drifts from the one consensus enforces.
        assert!(schema["properties"]["operation"].get("enum").is_none());
    }
}
