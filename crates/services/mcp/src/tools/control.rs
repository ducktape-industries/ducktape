use serde_json::Value;

use super::{Tool, arg_str, schema};
use crate::identity::Run;
use crate::node::{NodeError, Result};

pub(super) fn tools() -> Vec<Tool> {
    vec![Tool {
        name: "ducktape_extend_provider_idle",
        description: "Request more silent time for only this live provider invocation. The host returns a bounded synchronous granted or denied result; reuse request_id only with identical seconds.",
        schema: || {
            let mut value = schema(&[
                (
                    "request_id",
                    "string",
                    true,
                    "An idempotency key for this request.",
                ),
                (
                    "requested_secs",
                    "integer",
                    true,
                    "Silent time requested from now; the host applies its own limits and hard cap.",
                ),
            ]);
            value["additionalProperties"] = Value::Bool(false);
            value
        },
        handler: extend_provider_idle,
    }]
}

fn extend_provider_idle(run: &Run, args: &Value) -> Result<Value> {
    let object = args
        .as_object()
        .ok_or_else(|| NodeError::Rejected("this tool needs an object argument".into()))?;
    if object
        .keys()
        .any(|key| key != "request_id" && key != "requested_secs")
    {
        return Err(NodeError::Rejected(
            "this tool accepts only request_id and requested_secs".into(),
        ));
    }
    let requested_secs = args
        .get("requested_secs")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            NodeError::Rejected("this tool needs an integer \"requested_secs\" argument".into())
        })?;
    run.extend_provider_idle(arg_str(args, "request_id")?, requested_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_accepts_only_request_identity_and_seconds() {
        let schema = (tools()[0].schema)();
        let properties = schema["properties"].as_object().unwrap();
        assert_eq!(properties.len(), 2);
        assert!(properties.contains_key("request_id"));
        assert!(properties.contains_key("requested_secs"));
        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    fn handler_rejects_caller_supplied_identity_or_credentials() {
        for extra in ["run_id", "attempt", "credential", "control_token"] {
            let mut args = serde_json::json!({"request_id":"r", "requested_secs":1});
            args[extra] = Value::String("other".into());
            let error = extend_provider_idle(&Run::from_env(), &args).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("only request_id and requested_secs")
            );
        }
    }

    #[test]
    fn unavailable_controller_is_a_typed_denial() {
        let reply = extend_provider_idle(
            &Run::from_env(),
            &serde_json::json!({"request_id":"r", "requested_secs":1}),
        )
        .unwrap();
        assert_eq!(reply["status"], "denied");
        assert_eq!(reply["reason"], "unavailable");
    }
}
