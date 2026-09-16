use super::*;

pub(crate) async fn load_workspace(
    rpc: &RpcClient,
    channel_id: Option<&str>,
    generation: i64,
) -> Result<WorkspaceData, String> {
    // Two independent reads. Serialized, chat's first paint waited on a status
    // probe it does not render; concurrent, the console opens on the slower
    // leg rather than their sum.
    let (status, chat) = tokio::try_join!(
        async { rpc.status().await.map_err(String::from) },
        load_chat_data(rpc, channel_id)
    )?;
    let tip = tip_from_status(status)?;
    Ok(WorkspaceData {
        generation,
        rpc: rpc.origin().to_string(),
        status: tip.status,
        height: tip.height,
        channels: chat.channels,
        active_channel: chat.active_channel,
        active_channel_name: chat.active_channel_name,
        active_channel_archived: chat.active_channel_archived,
        huddle_roster: chat.huddle_roster,
    })
}

fn tip_from_status(status: NodeStatus) -> Result<Tip, String> {
    let height = i64::try_from(status.height).map_err(|_| "node height exceeds i64")?;
    Ok(Tip {
        height,
        status: format!("Connected · block {height}"),
    })
}

/// The deployed view chooses the landing room and projects its channel facts.
pub(crate) async fn load_chat_data(
    rpc: &RpcClient,
    requested: Option<&str>,
) -> Result<ChatData, String> {
    // Other native live readers still use the shared identity cache.
    if let Err(error) = refresh_names(rpc).await {
        tracing::debug!(target: "ducktape::chat", %error, "name directory unavailable");
    }
    let key = local_user_key()
        .await
        .map(|key| hex_encode(&key))
        .unwrap_or_default();
    let result = chat_background(
        rpc.origin(),
        serde_json::json!({"kind":"workspace","requested":requested,"key":key}),
    )
    .await
    .map_err(|error| error.message)?;
    let chat: ChatData = serde_json::from_value(result).map_err(|error| error.to_string())?;
    note_active_channel(&chat.active_channel);
    Ok(chat)
}

/// A switch returns only the refreshed row, or the view's cold-start fallback.
pub(crate) async fn load_channel_window_data(
    rpc: &RpcClient,
    channel_id: &str,
) -> Result<ChatData, String> {
    let key = local_user_key()
        .await
        .map(|key| hex_encode(&key))
        .unwrap_or_default();
    let result = chat_background(
        rpc.origin(),
        serde_json::json!({"kind":"window","channel":channel_id,"key":key}),
    )
    .await
    .map_err(|error| error.message)?;
    let chat: ChatData = serde_json::from_value(result).map_err(|error| error.to_string())?;
    note_active_channel(&chat.active_channel);
    Ok(chat)
}
