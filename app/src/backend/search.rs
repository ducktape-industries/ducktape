use super::*;

#[derive(Clone, Debug, Hash, PartialEq)]
pub struct PaletteSearchData {
    pub chat_hits: Vec<ChatSearchHit>,
    pub page_hits: Vec<PageSearchHit>,
}

/// The command palette's per-keystroke search: one debounced call covering
/// chat and pages together. Typing a word used to issue two RPC round trips
/// per keystroke. The palette task lane owns cancellation and stale delivery;
/// dropping a superseded task during this sleep prevents its RPCs from firing.
pub async fn palette_search(rpc: String, text: String) -> Result<PaletteSearchData, AppError> {
    tokio::time::sleep(Duration::from_millis(250)).await;
    let (chat, pages) = tokio::join!(
        search_chat(rpc.clone(), String::new(), text.clone()),
        search_pages(rpc, String::new(), text)
    );
    let both_failed = chat.is_err() && pages.is_err();
    if both_failed {
        return Err(AppError {
            message: "Search did not reach the node. Retry in a moment.".into(),
            committed: false,
        });
    }
    Ok(PaletteSearchData {
        chat_hits: chat.map(|data| data.hits).unwrap_or_default(),
        page_hits: pages.map(|data| data.hits).unwrap_or_default(),
    })
}
