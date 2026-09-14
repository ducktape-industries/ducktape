//! Pi's JSON event contract and run-local configuration. Pi is a harness;
//! the credential's vendor selects the model API, not a new credential kind.

use std::path::Path;

use serde_json::{Value, json};

use crate::{ProviderOutput, TokenUsage, json_lines};

pub(crate) const TOOL_EXTENSION: &str = "ducktape.ts";

/// Only a fresh config home and an opaque per-run capability reach Pi. Override
/// the selected built-in provider rather than inventing a model catalog: Pi's
/// installed catalog still owns model IDs, token limits, and API options.
pub(crate) fn configure(
    config: &Path,
    endpoint: &crate::broker::BrokerEndpoint,
    mut set: impl FnMut(&str, String),
) -> Result<(), String> {
    let provider = match endpoint.kind {
        crate::CredentialKind::Claude => "anthropic",
        crate::CredentialKind::Codex => "openai-codex",
        crate::CredentialKind::AppleCodesign => {
            return Err(
                "credential kind apple-codesign is a signing identity, not a model lane".into(),
            );
        }
    };
    let models = json!({"providers": {provider: {
        "baseUrl": endpoint.base_url,
        "apiKey": format!("${}", crate::BROKER_TOKEN_ENV),
    }}});
    let settings = json!({
        "defaultProvider": provider,
        "transport": "sse",
        "enableInstallTelemetry": false,
    });
    for (name, value) in [("models.json", models), ("settings.json", settings)] {
        std::fs::write(config.join(name), value.to_string())
            .map_err(|error| format!("write Pi {name}: {error}"))?;
    }
    set(crate::BROKER_TOKEN_ENV, endpoint.run_bearer.clone());
    set("PI_OFFLINE", "1".into());
    set(
        crate::PROVIDER_CONTROL_URL_ENV,
        endpoint.control_url.clone(),
    );
    set(
        crate::PROVIDER_CONTROL_TOKEN_ENV,
        endpoint.control_token.clone(),
    );
    Ok(())
}

pub(crate) fn stage_tools(config: &Path) -> Result<(), String> {
    std::fs::write(config.join(TOOL_EXTENSION), include_str!("pi/ducktape.ts"))
        .map_err(|error| format!("stage Pi Ducktape tool extension: {error}"))
}

/// Count authoritative assistant completions once, never the duplicate messages
/// in `turn_end` / `agent_end` or the cumulative streaming usage snapshots.
/// A failed final turn must not turn an earlier tool preamble into an answer.
pub(crate) fn parse_output(stdout: &str) -> Result<ProviderOutput, String> {
    let mut last = None;
    let mut usage: Option<TokenUsage> = None;
    for event in json_lines(stdout) {
        let is_assistant_end =
            event["type"] == "message_end" && event["message"]["role"] == "assistant";
        if !is_assistant_end {
            continue;
        }
        let message = &event["message"];
        if let Some(counts) = message.get("usage").and_then(Value::as_object) {
            let read = |key: &str| counts.get(key).and_then(Value::as_u64).unwrap_or(0);
            let total = usage.get_or_insert_with(TokenUsage::default);
            let cached = read("cacheRead");
            let written = read("cacheWrite");
            total.input_tokens = total
                .input_tokens
                .saturating_add(read("input").saturating_add(cached).saturating_add(written));
            total.cached_input_tokens = total.cached_input_tokens.saturating_add(cached);
            total.cache_write_input_tokens = total.cache_write_input_tokens.saturating_add(written);
            total.output_tokens = total.output_tokens.saturating_add(read("output"));
            total.reasoning_output_tokens = total
                .reasoning_output_tokens
                .saturating_add(read("reasoning"));
        }
        last = Some(message.clone());
    }
    let Some(message) = last else {
        return Err("pi emitted no completed assistant message".into());
    };
    match message["stopReason"].as_str() {
        Some("stop" | "length") => {}
        Some("error" | "aborted") => {
            return Err(format!(
                "pi reported a failed assistant turn: {}",
                crate::excerpt(
                    message["errorMessage"]
                        .as_str()
                        .unwrap_or("no error detail")
                ),
            ));
        }
        _ => return Err("pi exited without a final assistant answer".into()),
    }
    let text = message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("");
    let text = crate::parse_text_output(&text)?;
    Ok(ProviderOutput { text, usage })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assistant(text: &str, reason: &str) -> Value {
        json!({"role":"assistant", "stopReason":reason, "content":[
            {"type":"thinking", "thinking":"private"},
            {"type":"text", "text":text}
        ], "usage":{"input":10,"cacheRead":20,"cacheWrite":3,"output":4,"reasoning":2}})
    }

    #[test]
    fn final_answer_and_usage_ignore_duplicate_events_and_snapshots() {
        let first = assistant("I will read the file", "toolUse");
        let last = assistant("The answer", "stop");
        let events = [
            json!({"type":"message_end","message":first}),
            json!({"type":"turn_end","message":first}),
            json!({"type":"message_update","usage":{"input":9999}}),
            json!({"type":"message_end","message":{"role":"toolResult","content":"not an answer"}}),
            json!({"type":"message_end","message":last}),
            json!({"type":"agent_end","messages":[first,last]}),
        ];
        let stream = events
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let output = parse_output(&stream).unwrap();
        assert_eq!(output.text, "The answer");
        assert_eq!(
            output.usage,
            Some(TokenUsage {
                input_tokens: 66,
                cached_input_tokens: 40,
                cache_write_input_tokens: 6,
                output_tokens: 8,
                reasoning_output_tokens: 4,
            })
        );
    }

    #[test]
    fn failed_or_unfinished_turn_never_returns_an_earlier_preamble() {
        for reason in ["error", "aborted", "toolUse", "pending", "deferred"] {
            let first = json!({"type":"message_end","message":assistant("Earlier", "stop")});
            let last = json!({"type":"message_end","message":assistant("", reason)});
            assert!(
                parse_output(&format!("{first}\n{last}")).is_err(),
                "{reason}"
            );
        }
        assert!(parse_output("banner\n{}").is_err());
        let empty = json!({"type":"message_end","message":assistant("", "stop")});
        assert!(parse_output(&empty.to_string()).is_err());
    }
}
