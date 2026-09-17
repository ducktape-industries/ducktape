//! Conversation payloads and attachment uploads run in the guest.
use super::{AttachmentState, Send};
use ducktape_view_guest::host;
use serde::{Deserialize, Serialize};

pub use ducktape_view_files::SelectedFile;
/// Re-exported so a composer site converts a refusal with the same
/// `host::said` its neighbours use, rather than spelling the guest crate out.
pub use ducktape_view_guest::host::said;
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Clipboard {
    pub text: String,
    pub files: Vec<SelectedFile>,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Target {
    Post {
        channel: String,
        thread: Option<u64>,
    },
    Edit {
        channel: String,
        seq: u64,
        base_rev: u32,
    },
}

pub async fn pick() -> Result<Vec<SelectedFile>, host::Refusal> {
    let bytes = host::request("fs.pick", b"{}").await?;
    serde_json::from_slice(&bytes).map_err(|error| host::malformed(error.to_string()))
}
pub async fn clipboard() -> Result<Clipboard, host::Refusal> {
    let bytes = host::request("clipboard.read", b"{}").await?;
    serde_json::from_slice(&bytes).map_err(|error| host::malformed(error.to_string()))
}
pub async fn copy(text: &str) -> Result<(), host::Refusal> {
    host::request("clipboard.write", text.as_bytes())
        .await
        .map(|_| ())
}
pub async fn release(token: &str) {
    let _ = host::request("fs.release", token.as_bytes()).await;
}
pub async fn id() -> Result<String, host::Refusal> {
    let bytes = host::request("host.id", b"message").await?;
    String::from_utf8(bytes).map_err(|error| host::malformed(error.to_string()))
}

pub fn body(send: &Send) -> String {
    let mut lines: Vec<String> = send.body.lines().map(str::to_owned).collect();
    for file in &send.attachments {
        if let AttachmentState::Ready { uri } = &file.state {
            lines.push(format!("[{}]({uri})", safe_name(&file.name)));
        }
    }
    lines.join("\n")
}
fn safe_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | '/') {
                '_'
            } else {
                c
            }
        })
        .collect()
}

pub async fn submit(id: String, send: &Send, target: &Target) -> Result<(), host::Refusal> {
    let body = body(send);
    let invalid_body = body.is_empty() || body.len() > 16 * 1024;
    if invalid_body {
        return Err(host::Refusal::new(
            "invalid_body",
            "Message must contain between 1 byte and 16 KiB",
        ));
    }
    let blocks = chat_message::parse_message(&body);
    let payload = match target {
        Target::Post { channel, thread } => {
            serde_json::json!({"post_message": {"channel_id":channel, "message_id":id, "blocks":blocks, "thread":thread}})
        }
        Target::Edit {
            channel,
            seq,
            base_rev,
        } => {
            serde_json::json!({"edit_message": {"channel_id":channel,"seq":seq,"blocks":blocks,"base_rev":base_rev}})
        }
    };
    let ask = serde_json::json!({"target":"chat", "payload":payload});
    host::request(
        "op.submit",
        &serde_json::to_vec(&ask).expect("message envelope"),
    )
    .await
    .map(|_| ())
}

pub async fn upload(file: SelectedFile) -> Result<String, host::Refusal> {
    let attachment_id = String::from_utf8(host::request("host.id", b"attachment").await?)
        .map_err(|error| host::malformed(error.to_string()))?;
    let path = format!(
        "/shared/attachments/{attachment_id}/{}",
        safe_name(&file.name)
    );
    ducktape_view_files::upload(file, path).await
}
