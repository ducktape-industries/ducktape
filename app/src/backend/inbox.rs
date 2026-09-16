//! What the rail paints beside the bell, asked of the deployed `inbox` view.
//!
//! THE APP FOLDS NO INBOX. Which notifications exist, which of them are
//! noise, how each is worded and where its `duck://` door leads are the
//! view's, read through the kernel's doors and re-read on its own `rpc.live`
//! hits. The one thing the app draws itself is a number on its own chrome, so
//! the one thing it asks for is that number — a headless run of the same
//! view, through the session lane every other background errand uses.

use super::AppError;

/// The seated account's unread count, by the view's own unread rule.
pub async fn load_bell_unread(rpc: String, account: String) -> Result<i64, AppError> {
    let props = serde_json::to_vec(&serde_json::json!({
        "background": { "unread": { "account": account } }
    }))
    .map_err(|error| error.to_string())?;
    let bytes = crate::module_view::background::request("inbox", props, &rpc).await?;
    let answer: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if let Some(error) = answer["error"].as_str() {
        return Err(error.to_owned().into());
    }
    Ok(answer["unread"].as_i64().unwrap_or_default())
}

/// A count the rail can apply, or nothing. A failed errand keeps the number
/// already on the chrome: a badge that blinks to zero because a socket
/// dropped reads as "everything is read", which is the one thing it must
/// never say wrongly. The next block asks again.
pub fn bell_count(result: Result<i64, AppError>) -> Option<i64> {
    match result {
        Ok(unread) => Some(unread),
        Err(error) => {
            tracing::warn!(
                target: "ducktape::app",
                reason = "inbox_unread_unanswered",
                error = %error.message,
                "bell count left as it was"
            );
            None
        }
    }
}
