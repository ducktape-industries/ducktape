use super::*;
use gpui_kit::base::text::{TextView, TextViewState};
use gpui_kit::{
    App, AppContext, ClickEvent, Context, Entity, EventEmitter, ImageCache, ImageCacheError,
    IntoElement, MouseButton, ParentElement, Render, RenderImage, Resource, Styled, Window, div,
};

const LINK_TOKEN_BYTES: u64 = 4 * 1024;

#[derive(Clone, Debug, Hash, PartialEq)]
pub struct AgentChatEvent {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub detail: String,
    pub status: String,
    pub answer: String,
    pub saga_id: String,
}

pub(crate) fn provider_output_event(provider: &str, line: &str, id: i64) -> Option<AgentChatEvent> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
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
        let name = clip_text(tool["name"].as_str()?, 80);
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
        detail: clip_text(&detail, 1_200),
        status: status.into(),
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

pub(crate) fn agent_ws_url(rpc: &str) -> String {
    let base = if let Some(rest) = rpc.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = rpc.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        rpc.to_string()
    };
    format!("{}/v1/ws", base.trim_end_matches('/'))
}

pub(crate) fn read_link_token(workspace: &Path) -> Result<String, String> {
    let path = workspace.join("service-link.token");
    let metadata = std::fs::metadata(&path).map_err(|_| {
        "The node's agent event token is not available in its workspace.".to_string()
    })?;
    if metadata.len() > LINK_TOKEN_BYTES {
        return Err("The node's agent event token is unexpectedly large.".into());
    }
    let token = std::fs::read_to_string(path)
        .map_err(|_| "The node's agent event token could not be read.".to_string())?;
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err("The node's agent event token is empty.".into());
    }
    Ok(token)
}

pub(crate) fn subscription_refusal(value: &serde_json::Value) -> Option<String> {
    let is_refusal =
        value["type"].as_str() == Some("refused") || value["type"].as_str() == Some("error");
    if !is_refusal {
        return None;
    }
    Some(
        value["detail"]
            .as_str()
            .or_else(|| value["error"].as_str())
            .unwrap_or("The node refused the agent event stream.")
            .to_string(),
    )
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

pub(crate) fn clip_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let end = text
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= limit)
        .last()
        .unwrap_or(0);
    format!("{}…", &text[..end])
}

/// The Markdown surface owns its selection and parse across host frames.
pub struct MarkdownSurface {
    source: String,
    doc: String,
    dark: bool,
    text: Entity<TextViewState>,
    pictures: Entity<ParkedPictures>,
}
impl MarkdownSurface {
    pub fn new(source: String, doc: String, dark: bool, cx: &mut Context<Self>) -> Self {
        let text = cx.new(|cx| TextViewState::markdown(&source, cx));
        let pictures = cx.new(|_| ParkedPictures { doc: doc.clone() });
        Self {
            source,
            doc,
            dark,
            text,
            pictures,
        }
    }
    pub fn replace(&mut self, source: String, doc: String, dark: bool, cx: &mut Context<Self>) {
        if self.source != source {
            self.text
                .update(cx, |state, cx| state.set_text(&source, cx));
            self.source = source;
        }
        self.doc = doc.clone();
        self.dark = dark;
        self.pictures.update(cx, |pictures, _| pictures.doc = doc);
        cx.notify();
    }
}
impl EventEmitter<ui_lang_wire::SurfaceValue> for MarkdownSurface {}
impl Render for MarkdownSurface {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        div().w_full().image_cache(self.pictures.clone()).child(
            TextView::new(&self.text)
                .selectable(true)
                .scrollable(false)
                .on_link_click(move |url, event, _, cx| {
                    let opens = match event {
                        ClickEvent::Mouse(click) => {
                            matches!(click.up.button, MouseButton::Left | MouseButton::Middle)
                        }
                        ClickEvent::Keyboard(_) => true,
                        ClickEvent::Touch(click) => !click.long_press,
                    };
                    if opens {
                        let _ = view.update(cx, |_, cx| {
                            cx.emit(ui_lang_wire::SurfaceValue::Str(url.to_string()))
                        });
                    }
                }),
        )
    }
}

/// Markdown never fetches URLs itself. Only images already admitted by the
/// repository loader can be read, preserving its address and byte limits.
struct ParkedPictures {
    doc: String,
}
impl ImageCache for ParkedPictures {
    fn load(
        &mut self,
        resource: &Resource,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        let picture = match resource {
            Resource::Uri(uri) => super::picture::inline_picture(&self.doc, uri.as_ref()),
            _ => None,
        };
        let Some(picture) = picture else {
            return Some(Err(ImageCacheError::Asset("Image unavailable".into())));
        };
        match picture.handle {
            super::picture::PictureHandle::Raster(image) => Some(Ok(image)),
            super::picture::PictureHandle::Vector(image) => {
                image.use_render_image(window, cx).map(Ok)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
