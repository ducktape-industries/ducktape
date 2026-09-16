//! Provider output interpretation and live run presentation.

use crate::host::{LiveActivity, LiveRun, LiveRunHint};

/// Select chat-anchored runs and cache the deployed product's agent names.
pub(crate) async fn discover(
    mut labels: std::collections::BTreeMap<String, String>,
) -> Result<serde_json::Value, String> {
    use serde_json::json;
    let pending = crate::host::ask(
        "rpc.query",
        &json!({"target":"runs","query":"pending_runs"}),
    )
    .await?;
    let records: Vec<_> = pending["pending_runs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|record| !record["channel_id"].as_str().unwrap_or_default().is_empty())
        .cloned()
        .collect();
    let unnamed = records
        .iter()
        .any(|record| !labels.contains_key(record["agent_id"].as_str().unwrap_or_default()));
    if unnamed {
        let roster = crate::host::ask(
            "rpc.query",
            &json!({"target":"runs","query":{"model":{"query":"agents"}}}),
        )
        .await;
        if let Ok(roster) = roster {
            for agent in roster["model"]["agents"].as_array().into_iter().flatten() {
                let Some(id) = agent["agent_id"].as_str() else {
                    continue;
                };
                let label = agent["display_name"].as_str().unwrap_or(id);
                labels.insert(id.into(), label.into());
            }
        }
        // A missing or deleted agent gets a stable fallback, avoiding a
        // second roster read on every poll of the same run.
        for record in &records {
            let id = record["agent_id"].as_str().unwrap_or_default();
            labels.entry(id.into()).or_insert_with(|| id.into());
        }
    }
    Ok(json!({"records":records,"labels":labels}))
}

/// Read one public session snapshot and each requested run's delegations.
pub(crate) async fn progress(runs: Vec<String>) -> serde_json::Value {
    use serde_json::json;
    let sessions = crate::host::ask(
        "rpc.query",
        &json!({"target":"runs","query":"agent_sessions"}),
    )
    .await
    .ok();
    let rows = futures::future::join_all(runs.into_iter().map(|run_id| {
        let sessions = sessions.as_ref();
        async move {
            let delegations = crate::host::ask(
                "rpc.query",
                &json!({"target":"runs","query":{"delegations":{"caller_run_id":run_id}}}),
            )
            .await
            .ok();
            let progress = public_run_progress(&run_id, sessions, delegations.as_ref());
            (run_id, progress)
        }
    }))
    .await;
    serde_json::Value::Object(rows.into_iter().collect())
}

/// Forward only committed progress facts, excluding session keys and results.
fn public_run_progress(
    run_id: &str,
    sessions: Option<&serde_json::Value>,
    delegations: Option<&serde_json::Value>,
) -> serde_json::Value {
    let sessions = sessions
        .and_then(|reply| reply["agent_sessions"].as_array())
        .map(|rows| {
            rows.iter()
                .filter(|row| row["run_id"] == run_id)
                .map(|row| serde_json::json!({"run_id": run_id, "actions": row["actions"]}))
                .collect::<Vec<_>>()
        });
    let delegations = delegations
        .and_then(|reply| reply["delegations"].as_array())
        .map(|rows| {
            rows.iter()
                .map(|row| serde_json::json!({"status": row["status"]}))
                .collect::<Vec<_>>()
        });
    serde_json::json!({
        "sessions": {"agent_sessions": sessions},
        "delegations": {"delegations": delegations},
    })
}

/// Interpret the authorized output in the deployed view, not in the host.
///
/// The hint is taken apart here and only what this reads of it goes on: the
/// destructuring is what makes a field added to the payload a field this
/// function has to decide about, rather than one that rides into the state.
pub(crate) fn project(hint: LiveRunHint) -> LiveRun {
    let LiveRunHint {
        public_progress,
        output,
        output_error,
        channel_id,
        anchor_seq,
        thread_root,
        run_id,
        dispatch_id,
        agent,
        status,
        activity,
        answer_preview,
    } = hint;
    let mut row = LiveRun {
        channel_id,
        anchor_seq,
        thread_root,
        run_id,
        dispatch_id,
        agent,
        status,
        activity,
        answer_preview,
    };
    // A reader the run's output is not addressed to is told what it
    // committed instead, and that is the whole status.
    if let Some(progress) = public_progress {
        row.status = public_run_status(
            &row.run_id,
            progress.get("sessions"),
            progress.get("delegations"),
        );
        return row;
    }
    if row.status.is_empty() {
        row.status = "Starting".into();
    }
    for (id, line) in output.iter().enumerate() {
        // Pending runs do not identify the worker. The parser recognizes
        // distinct provider shapes; Claude enables its assistant/result forms.
        if let Some(event) = provider_output_event("claude", line, id as i64) {
            row = live_row_apply(row, &event);
        }
    }
    let failed = !output_error.is_empty();
    if failed {
        row.status = output_error;
    }
    row
}

/// Only committed public facts, never provider text, prompts or tool arguments.
fn public_run_status(
    run_id: &str,
    sessions: Option<&serde_json::Value>,
    delegations: Option<&serde_json::Value>,
) -> String {
    let calls = delegations.and_then(|reply| reply["delegations"].as_array());
    let peer_pending =
        calls.is_some_and(|calls| calls.iter().any(|call| call["status"] == "pending"));
    if peer_pending {
        return "Working · peer call pending".into();
    }
    let peer_delivered =
        calls.is_some_and(|calls| calls.iter().any(|call| call["status"] == "delivered"));
    if peer_delivered {
        return "Working · peer reply received".into();
    }
    let Some(sessions) = sessions.and_then(|reply| reply["agent_sessions"].as_array()) else {
        return "Working".into();
    };
    let Some(session) = sessions.iter().find(|session| session["run_id"] == run_id) else {
        return "Starting".into();
    };
    match session["actions"].as_u64().unwrap_or_default() {
        0 => "Working".into(),
        1 => "Working · 1 action recorded".into(),
        count => format!("Working · {count} actions recorded"),
    }
}

#[derive(Clone, Debug, Hash, PartialEq)]
struct AgentChatEvent {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub detail: String,
    pub status: String,
    pub answer: String,
    pub saga_id: String,
}

fn provider_output_event(provider: &str, line: &str, id: i64) -> Option<AgentChatEvent> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    // Live output does not identify its provider. Pi's authoritative event
    // types are distinct from Claude messages and Codex items.
    let pi_event = provider == "pi"
        || matches!(
            value["type"].as_str(),
            Some("message_end" | "tool_execution_start" | "tool_execution_end")
        );
    if pi_event {
        return pi_output_event(&value, id);
    }
    if provider == "claude" && value["type"].as_str() == Some("result") {
        let answer = value["result"].as_str()?.to_string();
        return Some(chat_preview(id, answer));
    }
    let claude_assistant = provider == "claude" && value["type"] == "assistant";
    if claude_assistant {
        // Tool names describe observed activity without copying arguments,
        // tool output, or thinking blocks into the chat status.
        let blocks = value["message"]["content"].as_array()?;
        let tool = blocks
            .iter()
            .rev()
            .find(|block| block["type"] == "tool_use")?;
        let name = tool["name"].as_str()?;
        return Some(AgentChatEvent {
            id,
            kind: "status".into(),
            title: format!("Using {name}"),
            detail: String::new(),
            status: String::new(),
            answer: String::new(),
            saga_id: String::new(),
        });
    }
    let claude_tool_reply = provider == "claude" && value["type"] == "user";
    if claude_tool_reply {
        let blocks = value["message"]["content"].as_array()?;
        let result = blocks
            .iter()
            .rev()
            .find(|block| block["type"] == "tool_result")?;
        let failed = result["is_error"] == true;
        let title = if failed {
            "Tool failed · waiting for agent"
        } else {
            "Tool finished · waiting for agent"
        };
        return Some(AgentChatEvent {
            id,
            kind: "status".into(),
            title: title.into(),
            detail: String::new(),
            status: String::new(),
            answer: String::new(),
            saga_id: String::new(),
        });
    }
    let event_type = value["type"].as_str().unwrap_or_default();
    let item = &value["item"];
    let item_type = item["type"]
        .as_str()
        .or_else(|| item["item_type"].as_str())
        .unwrap_or_default();
    if item_type == "agent_message" {
        let answer = item["text"]
            .as_str()
            .or_else(|| item["message"].as_str())?
            .to_string();
        return Some(chat_preview(id, answer));
    }
    let completed = event_type.ends_with("completed");
    let status = if completed { "done" } else { "running" };
    let (title, detail) = match item_type {
        "reasoning" => (
            "Reasoning".to_string(),
            json_text(item.get("text").or_else(|| item.get("summary"))),
        ),
        "command_execution" => (
            "Command".to_string(),
            json_text(
                item.get("command")
                    .or_else(|| item.get("aggregated_output")),
            ),
        ),
        "mcp_tool_call" => {
            let server = item["server"].as_str().unwrap_or("tool");
            let tool = item["tool"].as_str().unwrap_or("call");
            (
                format!("{server} · {tool}"),
                json_text(item.get("arguments")),
            )
        }
        "web_search" => ("Web search".to_string(), json_text(item.get("query"))),
        _ => return None,
    };
    Some(AgentChatEvent {
        id,
        kind: "activity".into(),
        title,
        detail,
        status: status.into(),
        answer: String::new(),
        saga_id: String::new(),
    })
}

fn pi_output_event(value: &serde_json::Value, id: i64) -> Option<AgentChatEvent> {
    let title = match value["type"].as_str()? {
        "message_end" => {
            let message = &value["message"];
            let completed_assistant = message["role"] == "assistant"
                && matches!(
                    message["stopReason"].as_str(),
                    Some("stop" | "length" | "toolUse")
                );
            if !completed_assistant {
                return None;
            }
            // Only the authoritative text blocks belong in chat. Thinking and
            // tool calls remain private; turn_end/agent_end repeat this message.
            let answer = message["content"]
                .as_array()?
                .iter()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let empty_answer = answer.trim().is_empty();
            if empty_answer {
                return None;
            }
            return Some(chat_preview(id, answer));
        }
        "tool_execution_start" => {
            let name = value["toolName"].as_str()?;
            format!("Using {name}")
        }
        "tool_execution_end" => {
            let failed = value["isError"].as_bool()?;
            if failed {
                "Tool failed · waiting for agent".into()
            } else {
                "Tool finished · waiting for agent".into()
            }
        }
        _ => return None,
    };
    Some(AgentChatEvent {
        id,
        kind: "status".into(),
        title,
        detail: String::new(),
        status: String::new(),
        answer: String::new(),
        saga_id: String::new(),
    })
}

fn chat_preview(id: i64, answer: String) -> AgentChatEvent {
    AgentChatEvent {
        id,
        kind: "preview".into(),
        title: "Writing the answer".into(),
        detail: String::new(),
        status: String::new(),
        answer,
        saga_id: String::new(),
    }
}

fn json_text(value: Option<&serde_json::Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Fold one parsed output event into the row. Status lines replace the status;
/// activities upsert by label and mark done; previews and answers replace the
/// preview; errors become the status.
fn live_row_apply(mut row: LiveRun, event: &AgentChatEvent) -> LiveRun {
    match event.kind.as_str() {
        "status" => row.status = event.title.clone(),
        "activity" => {
            let label = if event.detail.is_empty() {
                event.title.clone()
            } else {
                format!("{}: {}", event.title, event.detail)
            };
            let done = event.status == "done";
            match row.activity.iter_mut().find(|act| act.label == label) {
                Some(act) => act.done |= done,
                None => row.activity.push(LiveActivity { label, done }),
            }
            row.status = event.title.clone();
        }
        "preview" | "answer" => {
            // A provider's last item can be its structured payload (a JSON
            // block list) before the words: not a preview anyone reads, and
            // it flashed in the card for a poll before the reply landed.
            let raw_payload = matches!(event.answer.trim_start().chars().next(), Some('{' | '['));
            if !raw_payload {
                row.answer_preview = event.answer.clone();
            }
            row.status = if event.kind == "answer" {
                "Done".into()
            } else {
                "Answering".into()
            };
        }
        "error" => {
            row.status = if event.answer.is_empty() {
                event.title.clone()
            } else {
                event.answer.clone()
            };
        }
        _ => {}
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_output_is_presented_by_the_guest_and_transport_errors_remain_visible() {
        let line = serde_json::json!({"type": "result", "result": "answer"}).to_string();
        let observed = LiveRunHint {
            status: "Starting".into(),
            output: vec![line],
            ..Default::default()
        };
        let rendered = project(observed.clone());
        assert_eq!(rendered.answer_preview, "answer");
        assert_eq!(rendered.status, "Answering");
        let failed = project(LiveRunHint {
            output_error: "connection failed".into(),
            ..observed
        });
        assert_eq!(failed.answer_preview, "answer");
        assert_eq!(failed.status, "connection failed");
    }

    #[test]
    fn public_progress_uses_committed_counts_not_private_content() {
        use serde_json::json;
        let sessions = json!({"agent_sessions": [
            {"run_id": "mine", "actions": 3, "session_key": "private-looking-key"},
            {"run_id": "other", "actions": 99}
        ]});
        assert_eq!(public_run_status("mine", None, None), "Working");
        assert_eq!(
            public_run_status("missing", Some(&sessions), None),
            "Starting"
        );
        assert_eq!(
            public_run_status("mine", Some(&sessions), None),
            "Working · 3 actions recorded"
        );
        let calls = json!({"delegations": [{
            "status": "pending", "delegation_id": "long-private-looking-id",
            "result": {"text": "SECRET provider result"}
        }]});
        assert_eq!(
            public_run_status("mine", Some(&sessions), Some(&calls)),
            "Working · peer call pending"
        );
        let delivered =
            json!({"delegations": [{"status": "delivered", "result": {"text": "SECRET"}}]});
        assert_eq!(
            public_run_status("mine", Some(&sessions), Some(&delivered)),
            "Working · peer reply received"
        );
        let failed = json!({"delegations": [{"status": "failed", "result": {"text": "SECRET"}}]});
        assert_eq!(
            public_run_status("mine", Some(&sessions), Some(&failed)),
            "Working · 3 actions recorded"
        );
    }

    fn event(kind: &str, title: &str, status: &str) -> AgentChatEvent {
        AgentChatEvent {
            id: 1,
            kind: kind.into(),
            title: title.into(),
            detail: String::new(),
            status: status.into(),
            answer: String::new(),
            saga_id: String::new(),
        }
    }

    #[test]
    fn output_events_fold_into_a_status_line_and_a_checklist() {
        let row = live_row_apply(LiveRun::default(), &event("status", "Thinking", ""));
        assert_eq!(row.status, "Thinking");
        let row = live_row_apply(row, &event("activity", "Command", "running"));
        assert_eq!(row.activity.len(), 1);
        assert!(!row.activity[0].done);
        let row = live_row_apply(row, &event("activity", "Command", "done"));
        assert_eq!(row.activity.len(), 1, "the same title upserts");
        assert!(row.activity[0].done);
        let row = live_row_apply(
            row,
            &AgentChatEvent {
                answer: "the reply".into(),
                ..event("answer", "", "")
            },
        );
        assert_eq!(row.answer_preview, "the reply");
        assert_eq!(row.status, "Done");
    }

    #[test]
    fn a_row_preserves_activity_and_answer_output() {
        let mut row = LiveRun::default();
        for i in 0..40 {
            row = live_row_apply(row, &event("activity", &format!("Step {i}"), "done"));
        }
        assert_eq!(row.activity.len(), 40);
        assert_eq!(row.activity[0].label, "Step 0");
        let long = "x".repeat(10_000);
        let row = live_row_apply(
            row,
            &AgentChatEvent {
                answer: long.clone(),
                ..event("preview", "", "")
            },
        );
        assert_eq!(row.answer_preview, long);
    }

    #[test]
    fn claude_tool_progress_names_the_observed_tool_without_private_content() {
        let line = serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                { "type": "thinking", "thinking": "SECRET reasoning" },
                { "type": "tool_use", "name": "Read", "input": { "file_path": "SECRET path" } }
            ] }
        })
        .to_string();
        let event = provider_output_event("claude", &line, 8).unwrap();
        assert_eq!(event.kind, "status");
        assert_eq!(event.title, "Using Read");
        assert!(event.detail.is_empty());
        assert!(event.answer.is_empty());
        let result = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"SECRET output"}]}}"#;
        let event = provider_output_event("claude", result, 9).unwrap();
        assert_eq!(event.title, "Tool finished · waiting for agent");
        assert!(event.detail.is_empty());
        assert!(event.answer.is_empty());
        let thought = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"SECRET"}]}}"#;
        assert!(provider_output_event("claude", thought, 9).is_none());
    }

    #[test]
    fn pi_events_reach_the_existing_live_output_caller() {
        for line in [
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"Answer"}]}}"#,
            r#"{"type":"tool_execution_start","toolName":"read","args":{"path":"SECRET"}}"#,
            r#"{"type":"tool_execution_end","isError":false,"result":"SECRET"}"#,
            r#"{"type":"tool_execution_end","isError":true,"result":"SECRET"}"#,
        ] {
            let expected = provider_output_event("pi", line, 15).unwrap();
            // watch_live_output passes Claude because PendingRun has no provider.
            let event = provider_output_event("claude", line, 15).unwrap();
            assert_eq!(event, expected);
        }
        for line in [
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"error","content":[{"type":"text","text":"Incomplete"}]}}"#,
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"aborted","content":[{"type":"text","text":"Incomplete"}]}}"#,
            r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"Answer"}}"#,
            r#"{"type":"turn_end","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"Answer"}]}}"#,
            r#"{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"Answer"}]}]}"#,
        ] {
            assert!(provider_output_event("claude", line, 15).is_none());
        }
    }

    #[test]
    fn pi_completed_assistant_projects_only_authoritative_text() {
        for stop_reason in ["stop", "length", "toolUse"] {
            let line = serde_json::json!({
                "type": "message_end",
                "message": {
                    "role": "assistant",
                    "stopReason": stop_reason,
                    "content": [
                        { "type": "thinking", "thinking": "SECRET reasoning" },
                        { "type": "text", "text": "First paragraph" },
                        { "type": "toolCall", "name": "read", "arguments": { "path": "SECRET path" } },
                        { "type": "text", "text": "Second paragraph" }
                    ]
                }
            });
            let event = provider_output_event("pi", &line.to_string(), 10).unwrap();
            assert_eq!(
                event,
                chat_preview(10, "First paragraph\nSecond paragraph".into())
            );
        }
    }

    #[test]
    fn pi_tool_progress_never_copies_arguments_or_results() {
        for (event_type, is_error, title) in [
            ("tool_execution_start", false, "Using read"),
            (
                "tool_execution_end",
                false,
                "Tool finished · waiting for agent",
            ),
            (
                "tool_execution_end",
                true,
                "Tool failed · waiting for agent",
            ),
        ] {
            let line = serde_json::json!({
                "type": event_type,
                "toolCallId": "call-1",
                "toolName": "read",
                "args": { "path": "SECRET path" },
                "result": { "content": [{ "type": "text", "text": "SECRET output" }] },
                "isError": is_error
            });
            let event = provider_output_event("pi", &line.to_string(), 11).unwrap();
            assert_eq!(event.id, 11);
            assert_eq!(event.kind, "status");
            assert_eq!(event.title, title);
            assert!(event.detail.is_empty());
            assert!(event.status.is_empty());
            assert!(event.answer.is_empty());
            assert!(event.saga_id.is_empty());
        }
    }

    #[test]
    fn pi_failed_or_unfinished_assistant_is_not_a_preview() {
        for stop_reason in ["error", "aborted", "pending", "deferred", "unknown"] {
            let line = serde_json::json!({
                "type": "message_end",
                "message": {
                    "role": "assistant", "stopReason": stop_reason,
                    "content": [{ "type": "text", "text": "Incomplete answer" }],
                    "errorMessage": "SECRET diagnostic"
                }
            });
            assert!(provider_output_event("pi", &line.to_string(), 12).is_none());
        }
    }

    #[test]
    fn pi_streaming_and_duplicate_lifecycle_events_are_ignored() {
        let message = serde_json::json!({
            "role": "assistant", "stopReason": "stop",
            "content": [{ "type": "text", "text": "Answer" }]
        });
        for event_type in [
            "session",
            "message_start",
            "message_update",
            "turn_end",
            "agent_end",
            "tool_execution_update",
            "unknown",
        ] {
            let line = serde_json::json!({
                "type": event_type,
                "message": message,
                "messages": [message],
                "assistantMessageEvent": { "type": "text_delta", "delta": "Answer" },
                "partialResult": { "content": [{ "type": "text", "text": "SECRET output" }] }
            });
            assert!(provider_output_event("pi", &line.to_string(), 13).is_none());
        }
        // Pi must not fall through to the Codex item parser.
        let codex = r#"{"type":"item.completed","item":{"type":"agent_message","text":"Answer"}}"#;
        assert!(provider_output_event("pi", codex, 13).is_none());
    }

    #[test]
    fn pi_non_assistant_empty_and_malformed_messages_are_ignored() {
        for role in ["user", "toolResult", "custom", "compactionSummary"] {
            let line = serde_json::json!({
                "type": "message_end",
                "message": { "role": role, "stopReason": "stop", "content": [{ "type": "text", "text": "SECRET" }] }
            });
            assert!(provider_output_event("pi", &line.to_string(), 14).is_none());
        }
        for content in [
            serde_json::json!([]),
            serde_json::json!([{ "type": "text", "text": " \n" }]),
            serde_json::json!([{ "type": "thinking", "thinking": "SECRET" }]),
            serde_json::json!([{ "type": "toolCall", "name": "read", "arguments": { "path": "SECRET" } }]),
        ] {
            let line = serde_json::json!({
                "type": "message_end",
                "message": { "role": "assistant", "stopReason": "stop", "content": content }
            });
            assert!(provider_output_event("pi", &line.to_string(), 14).is_none());
        }
        for line in [
            "not json",
            "{}",
            r#"{"type":"message_end"}"#,
            r#"{"type":"tool_execution_start"}"#,
            r#"{"type":"tool_execution_end"}"#,
        ] {
            assert!(provider_output_event("pi", line, 14).is_none());
        }
    }

    #[test]
    fn provider_output_projects_known_events_not_raw_json() {
        let line = serde_json::json!({
            "type": "item.completed",
            "item": { "type": "agent_message", "text": "answer" }
        })
        .to_string();
        let event = provider_output_event("codex", &line, 7).unwrap();
        assert_eq!(event.kind, "preview");
        assert_eq!(event.answer, "answer");
        assert!(provider_output_event("codex", r#"{"type":"unknown","secret":"no"}"#, 8).is_none());
    }
}

#[cfg(test)]
mod progress_tests {
    use super::*;
    #[test]
    fn public_progress_excludes_other_runs_and_private_fields() {
        let progress = public_run_progress(
            "mine",
            Some(&serde_json::json!({"agent_sessions": [
                {"run_id": "mine", "actions": 3, "session_key": "SECRET"},
                {"run_id": "other", "actions": 99}
            ]})),
            Some(&serde_json::json!({"delegations": [
                {"status": "pending", "result": "SECRET", "delegation_id": "SECRET"}
            ]})),
        );
        assert_eq!(
            progress,
            serde_json::json!({
                "sessions": {"agent_sessions": [{"run_id": "mine", "actions": 3}]},
                "delegations": {"delegations": [{"status": "pending"}]},
            })
        );
    }
}
