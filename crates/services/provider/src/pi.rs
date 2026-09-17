//! Pi's JSON event contract and run-local configuration. Pi is a harness;
//! the credential's vendor selects the model API, not a new credential kind.

use std::path::Path;

use base64::Engine as _;
use serde_json::{Value, json};

use crate::{ProviderOutput, TokenUsage, json_lines};

pub(crate) const TOOL_EXTENSION: &str = "ducktape.ts";
pub(crate) const CONVERSATION_EXTENSION: &str = "conversation.ts";
const CONVERSATION_MANIFEST: &str = "conversation.json";

/// Only the conversation workflow loads the SDK bootstrap. Keeping selection
/// in the fresh config home also makes host and microVM argv identical.
pub(crate) fn extension(config: &Path) -> &'static str {
    if config.join(CONVERSATION_MANIFEST).is_file() {
        CONVERSATION_EXTENSION
    } else {
        TOOL_EXTENSION
    }
}

pub(crate) fn prepare_conversation(
    config: &Path,
    conversation: &crate::NativeConversationContext,
    prompt: &str,
) -> Result<String, String> {
    let artifact_path = |path: &Path| {
        path.starts_with(".ducktape-run/native")
            && path
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
    };
    let isolated_artifacts = artifact_path(&conversation.session_path)
        && conversation
            .packages
            .iter()
            .all(|package| artifact_path(&package.path));
    if !isolated_artifacts {
        return Err("native conversation artifacts must be below the private workspace-relative native root".into());
    }
    let manifest = serde_json::to_vec(conversation)
        .map_err(|error| format!("encode native conversation manifest: {error}"))?;
    std::fs::write(config.join(CONVERSATION_MANIFEST), manifest)
        .map_err(|error| format!("write native conversation manifest: {error}"))?;
    std::fs::write(
        config.join(CONVERSATION_EXTENSION),
        include_str!("pi/conversation.ts"),
    )
    .map_err(|error| format!("stage native conversation transport: {error}"))?;
    Ok(format!(
        "/ducktape-conversation {}",
        base64::engine::general_purpose::STANDARD.encode(prompt)
    ))
}

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

enum Completion {
    Assistant(Value),
    InputHandled,
    Cancelled,
}

/// Count authoritative assistant completions once, never the duplicate messages
/// in `turn_end` / `agent_end` or the cumulative streaming usage snapshots.
/// A failed final turn must not turn an earlier tool preamble into an answer.
pub(crate) fn parse_output(stdout: &str) -> Result<ProviderOutput, String> {
    let mut last = None;
    let mut usage: Option<TokenUsage> = None;
    for event in json_lines(stdout) {
        if event["type"] == "native_conversation_cancelled" {
            let already_terminal =
                matches!(last, Some(Completion::InputHandled | Completion::Cancelled));
            if already_terminal {
                return Err("pi mixed cancellation with another native terminal result".into());
            }
            last = Some(Completion::Cancelled);
            continue;
        }
        if event["type"] == "native_conversation_handled" {
            if last.is_some() {
                return Err("pi mixed handled input with another completion".into());
            }
            last = Some(Completion::InputHandled);
            continue;
        }
        let is_assistant_end =
            event["type"] == "message_end" && event["message"]["role"] == "assistant";
        if !is_assistant_end {
            continue;
        }
        if matches!(last, Some(Completion::InputHandled | Completion::Cancelled)) {
            return Err("pi emitted a model answer after a native terminal result".into());
        }
        let message = &event["message"];
        // A retry may replay the already-checkpointed final answer without a
        // model request. Preserve that native message, but never charge its
        // historical counters as new consumption by this invocation.
        let newly_generated = event["restored"] != true;
        if let Some(counts) = message
            .get("usage")
            .and_then(Value::as_object)
            .filter(|_| newly_generated)
        {
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
        last = Some(Completion::Assistant(message.clone()));
    }
    let message = match last {
        Some(Completion::Assistant(message)) => message,
        Some(Completion::InputHandled) => {
            return Ok(ProviderOutput {
                text: String::new(),
                usage: None,
                disposition: crate::OutputDisposition::InputHandled,
            });
        }
        Some(Completion::Cancelled) => {
            return Ok(ProviderOutput {
                text: String::new(),
                usage,
                disposition: crate::OutputDisposition::Cancelled,
            });
        }
        None => return Err("pi emitted no completed assistant message".into()),
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
    Ok(ProviderOutput {
        text,
        usage,
        disposition: crate::OutputDisposition::Answer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// pi speaks no MCP: the extension IS the client, so the route it dials
    /// and the route the lane serves are two literals in two languages. Pin
    /// them together here — nothing else compiles the TypeScript.
    #[test]
    fn the_pi_extension_dials_the_route_the_lane_serves() {
        const SOURCE: &str = include_str!("pi/ducktape.ts");
        assert!(
            SOURCE.contains(&format!("const MCP_PATH = {:?};", crate::MCP_PATH)),
            "pi/ducktape.ts does not spell MCP_PATH as {:?}",
            crate::MCP_PATH
        );
        // and it reaches it over the run's own lane, holding nothing: a guest
        // that execs a server or carries a write token is the design this
        // replaced.
        assert!(
            SOURCE.contains("env.DUCKTAPE_NODE"),
            "pi/ducktape.ts does not read the run's node url"
        );
        for banned in ["child_process", "DUCKTAPE_RUN_ACTION_TOKEN"] {
            assert!(!SOURCE.contains(banned), "pi/ducktape.ts names {banned}");
        }
    }

    fn native_context() -> crate::NativeConversationContext {
        crate::NativeConversationContext {
            conversation_id: "resident-1".into(),
            turn_id: "turn-2".into(),
            revision: 7,
            session_path: ".ducktape-run/native/history/session.jsonl".into(),
            packages: vec![crate::NativePackage {
                name: "resident".into(),
                path: ".ducktape-run/native/packages/resident".into(),
            }],
            events: Vec::new(),
            job_reporting: false,
            system_prompt: "Stable resident instructions".into(),
        }
    }

    #[test]
    fn native_bootstrap_is_explicit_and_keeps_only_current_input_in_command() {
        let config = tempfile::tempdir().unwrap();
        assert_eq!(extension(config.path()), TOOL_EXTENSION);
        let context = native_context();
        let prompt = "next turn\nwith unicode 🦆 and /commands";
        let command = prepare_conversation(config.path(), &context, prompt).unwrap();
        let encoded = command.strip_prefix("/ducktape-conversation ").unwrap();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
            prompt.as_bytes()
        );
        assert_eq!(extension(config.path()), CONVERSATION_EXTENSION);
        let manifest: Value = serde_json::from_slice(
            &std::fs::read(config.path().join(CONVERSATION_MANIFEST)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            manifest["session_path"],
            ".ducktape-run/native/history/session.jsonl"
        );
        assert_eq!(manifest["revision"], 7);
        assert_eq!(manifest["system_prompt"], context.system_prompt);
        assert_eq!(manifest["job_reporting"], false);
        assert_eq!(manifest.as_object().unwrap().len(), 8);
        assert!(config.path().join(CONVERSATION_EXTENSION).is_file());
    }

    #[test]
    fn native_bootstrap_refuses_artifacts_outside_reserved_history_root() {
        for path in [
            "/host/session.jsonl",
            "../session.jsonl",
            ".ducktape-run/native/../provider-config/auth.json",
            "workspace/session.jsonl",
        ] {
            let config = tempfile::tempdir().unwrap();
            let mut context = native_context();
            context.session_path = path.into();
            assert!(prepare_conversation(config.path(), &context, "turn").is_err());
            assert_eq!(extension(config.path()), TOOL_EXTENSION);
        }
    }

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
    fn handled_input_is_explicit_without_assistant_or_model_usage() {
        let handled = r#"{"type":"native_conversation_handled"}"#;
        let output = parse_output(handled).unwrap();
        assert_eq!(output.disposition, crate::OutputDisposition::InputHandled);
        assert!(output.text.is_empty());
        assert_eq!(output.usage, None);
        let answer = json!({"type":"message_end", "message":assistant("answer", "stop")});
        assert!(parse_output(&format!("{handled}\n{answer}")).is_err());
        assert!(parse_output(&format!("{answer}\n{handled}")).is_err());
        assert!(parse_output(&format!("{handled}\n{handled}")).is_err());
    }

    #[test]
    fn cancellation_has_no_answer_but_keeps_real_usage_before_the_boundary() {
        let cancelled = r#"{"type":"native_conversation_cancelled"}"#;
        let untouched = parse_output(cancelled).unwrap();
        assert_eq!(untouched.disposition, crate::OutputDisposition::Cancelled);
        assert!(untouched.text.is_empty());
        assert_eq!(untouched.usage, None);
        let preamble =
            json!({"type":"message_end", "message":assistant("I will use a tool", "toolUse")});
        let output = parse_output(&format!("{preamble}\n{cancelled}")).unwrap();
        assert_eq!(output.disposition, crate::OutputDisposition::Cancelled);
        assert!(output.text.is_empty());
        assert_eq!(output.usage.unwrap().output_tokens, 4);
        assert!(parse_output(&format!("{cancelled}\n{preamble}")).is_err());
        assert!(parse_output(&format!("{cancelled}\n{cancelled}")).is_err());
    }

    #[test]
    fn restored_native_answer_does_not_report_new_provider_usage() {
        let event = json!({"type":"message_end", "message":assistant("Committed answer", "stop"), "restored":true});
        let output = parse_output(&event.to_string()).unwrap();
        assert_eq!(output.text, "Committed answer");
        assert_eq!(output.usage, None);
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
