//! What the palette asks of the host KERNEL, and every fold the screen is.
//!
//! The kernel pushes SESSION FACTS ONLY (`palette.props`: connected, dark and
//! the chain id a link is spelled for). WHICH KEY OPENS THIS, what a query
//! searches, how a hit reads and where it leads are folded here: the chord is
//! claimed through `host.chord`, the messages are read through `rpc.view`
//! against `chat`, the pages through `rpc.view` against `pages`, and a hit's
//! door out is `host.open_link` carrying a `duck://` address.
//!
//! Nothing here links a module crate: every request and every reply is
//! spelled as the JSON the module's wire already is.

use ducktape_view_guest::host;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

/// The chord this palette answers to. One string, claimed at the kernel and
/// spelled the way the kernel spells a press, so a claim and a press cannot
/// disagree.
pub const CHORD: &str = "cmd-k";

/// How long a keystroke waits before the query leaves. A search per keystroke
/// is two node reads per keystroke; the pause is what makes typing one query.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(250);

/// The most hits of each kind one query asks for.
const HITS: usize = 25;

/// The session facts the kernel pushes this view.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub connected: bool,
    pub dark: bool,
    #[serde(default)]
    pub chain: String,
}

/// One item of the session subscription: the facts, or why not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionItem {
    pub next: Session,
    pub error: String,
}

pub fn session() -> ducktape_view_guest::Subscription<SessionItem> {
    ducktape_view_guest::Subscription::run(|| {
        host::subscribe("palette.props", &[]).map(|answer| {
            let read = answer.and_then(|bytes| {
                serde_json::from_slice::<Session>(&bytes).map_err(|error| error.to_string())
            });
            match read {
                Ok(next) => SessionItem {
                    next,
                    error: String::new(),
                },
                Err(error) => SessionItem {
                    error,
                    ..SessionItem::default()
                },
            }
        })
    })
}

/// Every press of this view's chord. A claim the kernel refused — because a
/// seated view asked for the same chord first — ends the stream instead of
/// carrying a press, so a palette whose claim lost simply never opens. Who
/// holds it is the kernel's to log, not this screen's to word.
pub fn chord() -> ducktape_view_guest::Subscription<()> {
    ducktape_view_guest::Subscription::run(|| {
        host::subscribe("host.chord", CHORD.as_bytes())
            .filter_map(|answer| std::future::ready(answer.ok().map(|_| ())))
    })
}

/// One message hit, as the palette draws it.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatHit {
    pub channel_id: String,
    pub seq: i64,
    pub author: String,
    pub text: String,
}

/// One page hit, as the palette draws it.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageHit {
    pub page_id: String,
    pub block_id: String,
    pub title: String,
    pub text: String,
}

/// One answer to one query: what was found, for what, and what went wrong.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchItem {
    pub query: String,
    pub chat: Vec<ChatHit>,
    pub pages: Vec<PageHit>,
    /// both lanes refused; one lane refusing is silence, not an error
    pub error: String,
}

/// The reader's query, searched once it has settled.
///
/// KEYED ON THE QUERY, so a keystroke drops the reading before it starts:
/// the subscription waits one [`SETTLE`] tick before it asks, and a key
/// pressed inside that window replaces the subscription rather than racing
/// it. Nothing debounces on the host side — a stale answer cannot arrive,
/// because the request that would have carried it was never sent.
pub fn search(query: String, serial: i64) -> ducktape_view_guest::Subscription<SearchItem> {
    ducktape_view_guest::Subscription::run_with((query, serial), |(query, _)| {
        let query = query.clone();
        ducktape_view_guest::ticks(SETTLE)
            .take(1)
            .then(move |()| read(query.clone()))
    })
}

async fn read(query: String) -> SearchItem {
    let (chat, pages) = futures::join!(read_chat(&query), read_pages(&query));
    let both_refused = chat.is_err() && pages.is_err();
    if both_refused {
        return SearchItem {
            query,
            chat: Vec::new(),
            pages: Vec::new(),
            error: "Search did not reach the node. Retry in a moment.".into(),
        };
    }
    SearchItem {
        query,
        chat: chat.unwrap_or_default(),
        pages: pages.unwrap_or_default(),
        error: String::new(),
    }
}

async fn read_chat(query: &str) -> Result<Vec<ChatHit>, String> {
    let ask = serde_json::json!({
        "search": { "text": query, "channel_id": null, "limit": HITS },
    });
    let reply = view("chat", ask).await?;
    Ok(reply["hits"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|hit| ChatHit {
            channel_id: text(&hit["channel_id"]),
            seq: hit["seq"].as_i64().unwrap_or_default(),
            author: text(&hit["author"]),
            text: text(&hit["text"]),
        })
        .collect())
}

async fn read_pages(query: &str) -> Result<Vec<PageHit>, String> {
    let ask = serde_json::json!({
        "search": { "text": query, "page_id": null, "limit": HITS },
    });
    let reply = view("pages", ask).await?;
    let hits: Vec<_> = reply["hits"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|hit| PageHit {
            page_id: text(&hit["page_id"]),
            block_id: text(&hit["block_id"]),
            title: String::new(),
            text: text(&hit["text"]),
        })
        .collect();
    if hits.is_empty() {
        return Ok(hits);
    }
    // A TITLE IS DECORATION AND MUST NOT DESTROY THE PAYLOAD: an index that
    // refuses leaves every hit on the fallback an unknown page already
    // takes, rather than throwing away a search the node answered.
    let titles = page_titles().await.unwrap_or_default();
    Ok(hits
        .into_iter()
        .map(|mut hit| {
            hit.title = titles
                .get(&hit.page_id)
                .cloned()
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| "Untitled".into());
            hit
        })
        .collect())
}

async fn page_titles() -> Result<std::collections::BTreeMap<String, String>, String> {
    let reply = view("pages", serde_json::json!({ "index": {} })).await?;
    Ok(reply["pages"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|page| (text(&page["id"]), text(&page["title"])))
        .collect())
}

async fn view(target: &str, query: serde_json::Value) -> Result<serde_json::Value, String> {
    let ask = serde_json::json!({ "target": target, "query": query });
    let reply = host::request(
        "rpc.view",
        &serde_json::to_vec(&ask).expect("a request encodes"),
    )
    .await?;
    serde_json::from_slice(&reply).map_err(|error| error.to_string())
}

fn text(value: &serde_json::Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

/// A message's address, for the chain this session is on. The `?net=` half
/// is the chain id's digest — the whole id cannot ride a URI.
pub fn chat_link(hit: &ChatHit, chain: &str) -> String {
    format!(
        "duck://channel/{}{}#{}",
        hit.channel_id,
        net_query(chain),
        hit.seq
    )
}

/// A page block's address, for the chain this session is on.
pub fn page_link(hit: &PageHit, chain: &str) -> String {
    let block = match hit.block_id.is_empty() {
        true => String::new(),
        false => format!("#{}", hit.block_id),
    };
    format!("duck://page/{}{}{}", hit.page_id, net_query(chain), block)
}

/// `?net=<digest>` for a chain id of the form `<name>#<digest>`, or nothing
/// when this session is on no named chain.
fn net_query(chain: &str) -> String {
    match chain.rsplit_once('#') {
        Some((_, digest)) if !digest.is_empty() => format!("?net={digest}"),
        _ => String::new(),
    }
}

/// The address of a hit, handed to the kernel's ONE open door, which routes
/// a `duck://` link to whatever owns it.
pub fn open_link(link: &str) {
    ducktape_view_guest::host::open_link(link);
}
