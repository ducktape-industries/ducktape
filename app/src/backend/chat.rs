use super::*;

/// One participant of a channel's live huddle — the roster is consensus state
/// (`HuddleMember{user, node, joined_at}`), not a count.
#[derive(Clone, Debug, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HuddleParticipant {
    pub key: String,
    pub label: String,
    pub initials: String,
    pub is_agent: bool,
    pub is_you: bool,
    pub joined_at: i64,
    /// The member's NODE key (hex) — the overlay identity the call hub fans
    /// media out to; `call_recipients` steers with exactly these.
    pub node: String,
}

/// Adapt the view's roster seats for the native device session.
pub(crate) fn roster_of_seats(seats: &[HuddleSeat]) -> Vec<HuddleParticipant> {
    seats
        .iter()
        .map(|seat| HuddleParticipant {
            key: seat.node.clone(),
            label: seat.label.clone(),
            initials: seat.initials.clone(),
            is_agent: false,
            is_you: seat.is_you,
            joined_at: 0,
            node: seat.node.clone(),
        })
        .collect()
}

/// Am *I* in this huddle — the discriminant that splits the `Huddle` start
/// button from the LIVE pill with its ✕ Leave.
pub fn huddle_self(roster: Vec<HuddleParticipant>) -> bool {
    roster.iter().any(|participant| participant.is_you)
}

// The huddle's elapsed clock is a LOCAL session fact on a NATIVE `every 1s`
// subscription — ui-lang ships one, so this app has no tick stream of its own.

pub async fn join_huddle(rpc: String, password: String, channel_id: String) -> Result<bool, AppError> {
    guest_participation(rpc, password, serde_json::json!({"kind":"join","channel":channel_id})).await?;
    Ok(true)
}

pub async fn move_huddle(rpc: String, password: String, leaving: String, channel_id: String) -> Result<String, AppError> {
    guest_participation(rpc, password, serde_json::json!({"kind":"move","from":leaving,"channel":channel_id})).await
}

pub async fn leave_huddle(rpc: String, password: String, channel_id: String) -> Result<bool, AppError> {
    guest_participation(rpc, password, serde_json::json!({"kind":"leave","channel":channel_id})).await?;
    Ok(true)
}

/// The shell relays user intent; the deployed Chat component owns participation.
async fn guest_participation(rpc: String, password: String, intent: serde_json::Value) -> Result<String, AppError> {
    require_seated_signer(password).await?;
    let result = chat_background(&rpc, intent).await?;
    result["channel"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "invalid Chat participation result".to_owned().into())
}

pub(crate) async fn chat_background(
    rpc: &str,
    intent: serde_json::Value,
) -> Result<serde_json::Value, AppError> {
    let props = serde_json::to_vec(&serde_json::json!({"background":intent}))
        .map_err(|error| error.to_string())?;
    let bytes = crate::module_view::background::request("chat", props, rpc).await?;
    let result: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if let Some(error) = result.get("error") {
        return Err(AppError {message:error["message"].as_str().unwrap_or("Chat participation refused").into(),
            committed:error["committed"].as_bool().unwrap_or(false)});
    }
    Ok(result)
}

/// Hand a WEB link to the OS opener — the `DuckKind::Web` arm of the open
/// plane, and its only caller. Only http(s) leaves the app this way
/// (this passes a string to a shell command, and the scheme gate is the trust
/// boundary); every other scheme is the open plane's own to resolve.
pub async fn open_external_url(url: String) -> Result<bool, AppError> {
    async {
        // "" is the open plane's "not my turn" — nothing was asked, nothing
        // opens.
        if url.is_empty() {
            return Ok(false);
        }
        let is_web = url.starts_with("http://") || url.starts_with("https://");
        if !is_web {
            return Err("only web links are handed to the system browser".to_string());
        }
        let opener = match std::env::consts::OS {
            "macos" => "open",
            "windows" => "explorer",
            _ => "xdg-open",
        };
        tokio::process::Command::new(opener)
            .arg(&url)
            .spawn()
            .map_err(|error| format!("could not open the link: {error}"))?;
        Ok(true)
    }
    .await
    .map_err(app_error)
}
