//! What the view asks of the host kernel, and the readings the screen folds
//! off the repositories it reads for itself.
//!
//! The kernel pushes only session facts (`forge.props`: connected, dark, the
//! network's name and chain id, the connected endpoint, and the `duck://`
//! link the app last routed here). Everything else is the view's own: this
//! node's standing off `rpc.status` and the valset, the
//! repo namespace, one repo's branches and tracker, one item with its patch
//! and reviews, the code browse's listing and file, and the item's
//! discussion all come from `rpc.query` / `rpc.view`, re-read on every
//! `rpc.live` hit for the forge plane. A review leaves as `op.submit`
//! carrying the module's own `ForgeMsg`, signed by the kernel with the
//! seated key — the view never sees the key, the endpoint or the password.
//!
//! Merge calculation belongs to the installed service named by this deployment's
//! `service.json` asset, reached through the generic authenticated Gateway.
//! Native picture decoding remains a host device capability.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use ducktape_view_guest::host;
use futures::{Stream, StreamExt, stream};
use serde::{Deserialize, Serialize};

pub use crate::blocks::{ChatBlock, ChatSpan};
use crate::blocks::{
    Names, author_display, avatar_initial, avatar_kind, blocks_of_json, blocks_view, body_blocks,
    deleted_block, party_handle, party_of_json,
};

/// The module this view reads and writes: its query target, its live plane,
/// and the git route the merge door fetches from.
const FORGE: &str = "forge";
/// The chat module the item discussions live in.
const CHAT: &str = "chat";

/// The newest run of a discussion the screen holds — the chat window's own
/// hot limit (`chat::client::CHAT_HOT_WINDOW_LIMIT`).
const DISCUSSION_LIMIT: i64 = 256;
/// One identity roster page (`identity::MAX_QUERY_LIMIT`).
const ACCOUNT_PAGE: u64 = 256;
/// The module's per-review line-comment cap
/// (`forge::tracker_iface::MAX_REVIEW_COMMENTS`).
const MAX_REVIEW_COMMENTS: usize = 64;
/// One line comment's body cap (`forge::MAX_REVIEW_COMMENT_BYTES`).
const MAX_REVIEW_COMMENT_BYTES: usize = 16 * 1024;
/// One comment anchor's path cap (`forge::MAX_PATH_BYTES`).
const MAX_PATH_BYTES: usize = 512;
/// One blob page (`forge::MAX_BLOB_PAGE_BYTES`).
const MAX_BLOB_PAGE_BYTES: u64 = 1024 * 1024;
/// The preview limit a picture past which draws the binary plate — the
/// app's own `picture::MAX_PICTURE_BYTES`.
const MAX_PICTURE_BYTES: usize = 16 * 1024 * 1024;
/// The picture surface this view parks its decoded pictures under — the
/// app's `picture::FORGE_SURFACE`.
const PICTURE_SURFACE: &str = "forge";

// ---------- the screen's rows ----------

/// One repository card: its name and the head the last push landed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ForgeRepo {
    pub name: String,
    pub head: String,
}

/// One born branch of the open repo: its name at the commit its head stood
/// on when the view read the repo.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ForgeBranch {
    pub name: String,
    pub head: String,
}

/// One tracker row. `kind` is `issue` | `pr`; `state` is `open` | `closed`
/// | `merged`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ForgeItem {
    pub number: i64,
    pub kind: String,
    pub state: String,
    pub title: String,
    pub author: String,
    pub author_name: String,
}

/// One discussion note, in the fields this screen draws.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub seq: i64,
    pub author: String,
    pub meta: String,
    pub blocks: Vec<ChatBlock>,
    pub initial: String,
    pub avatar_kind: String,
}

/// One line comment a review carried, anchored `path:line (side)`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ForgeReviewComment {
    pub anchor: String,
    pub body: String,
    pub blocks: Vec<ChatBlock>,
}

/// One submitted review, rendered.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ForgeReview {
    pub author: String,
    pub author_name: String,
    pub verdict: String,
    pub body: String,
    pub blocks: Vec<ChatBlock>,
    pub commit: String,
    pub outdated: bool,
    pub created_at: i64,
    pub comments: Vec<ForgeReviewComment>,
}

/// One line comment staged for the next review.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ForgeDraftComment {
    pub anchor: String,
    pub path: String,
    pub line: String,
    pub side: String,
    pub body: String,
}

/// One entry of the open directory. `kind` is `dir` | `file`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TreeEntry {
    pub name: String,
    pub path: String,
    pub kind: String,
}

/// One painted row of the unified patch. `kind` is `file` (one per changed
/// file, `text` naming it) | `hunk` | `add` | `del` | `ctx` | `note` (the
/// `\ No newline at end of file` remark).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DiffLine {
    pub key: i64,
    pub kind: String,
    pub old_no: String,
    pub new_no: String,
    pub sign: String,
    pub text: String,
    pub path: String,
    pub side: String,
}

// ---------- the session ----------

/// The session facts the kernel pushes, one item per change: nothing here
/// is forge's, and all of it is what the titlebar already holds.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub connected: bool,
    pub dark: bool,
    /// the network's name — the repo crumb's root and the empty state's hero
    pub org: String,
    /// this account's bio, as the empty state introduces the network
    pub about: String,
    pub network_chain_id: String,
    pub connected_rpc: String,
    /// the `duck://forge/...` address the app's open plane last routed here
    pub link: String,
    /// moves once per routed link, so the same address twice still lands
    pub link_tick: i64,
}

/// One item of the session subscription: the facts, or why not.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionItem {
    pub next: Session,
    pub error: String,
}

/// The session now, and again on every change the kernel sees.
pub fn session() -> ducktape_view_guest::Subscription<SessionItem> {
    ducktape_view_guest::Subscription::run(|| {
        host::subscribe("forge.props", &[]).map(|answer| {
            let read = answer.and_then(|bytes| {
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())
            });
            match read {
                Ok(next) => SessionItem {
                    next,
                    error: String::new(),
                },
                Err(error) => SessionItem {
                    next: Session::default(),
                    error: failure("Could not read the session", &error),
                },
            }
        })
    })
}

/// A refusal as the screen says it: what we were doing, then the kernel's
/// own words. Every error a reader hands the view passes through here, so
/// no strip ever shows a bare token.
fn failure(doing: &str, error: &str) -> String {
    format!("{doing}: {error}")
}

/// The serial every read subscription is keyed by: it moves when the
/// session comes up, so a reconnect reads the forge afresh.
pub fn connection_serial_after(was_connected: bool, connected: bool, serial: i64) -> i64 {
    let came_up = connected && !was_connected;
    match came_up {
        true => serial + 1,
        false => serial,
    }
}

// ---------- the node ----------

async fn query(target: &str, query: serde_json::Value) -> Result<serde_json::Value, String> {
    let ask = serde_json::json!({ "target": target, "query": query });
    let reply = host::request("rpc.query", &serde_json::to_vec(&ask).expect("encodes")).await?;
    serde_json::from_slice(&reply).map_err(|error| error.to_string())
}

async fn view(target: &str, query: serde_json::Value) -> Result<serde_json::Value, String> {
    let ask = serde_json::json!({ "target": target, "query": query });
    let reply = host::request("rpc.view", &serde_json::to_vec(&ask).expect("encodes")).await?;
    serde_json::from_slice(&reply).map_err(|error| error.to_string())
}

/// Every identity account, paged the way the module serves them: the one
/// read the display names are folded off.
async fn read_names() -> Names {
    let mut names = Names::default();
    let mut from = 0u64;
    loop {
        let ask = serde_json::json!({ "all": { "from": from, "limit": ACCOUNT_PAGE } });
        let Ok(reply) = query("identity", ask).await else {
            return names;
        };
        let page = &reply["accounts"];
        let rows = page.as_array().map_or(0, Vec::len);
        names.extend(Names::of_accounts(page));
        let last = page
            .as_array()
            .and_then(|page| page.last())
            .and_then(|account| account["number"].as_u64());
        let page_is_last = rows < ACCOUNT_PAGE as usize;
        match last {
            Some(last) if !page_is_last => from = last + 1,
            Some(_) | None => return names,
        }
    }
}

/// One item per forge block, then one read: the stream every slice below
/// re-reads on.
fn on_forge_blocks<Item, Load, Next>(load: Load) -> impl Stream<Item = Item>
where
    Load: Fn() -> Next + 'static,
    Next: Future<Output = Item> + 'static,
{
    let live = host::subscribe("rpc.live", FORGE.as_bytes());
    stream::once(load()).chain(live.then(move |_| load()))
}

// ---------- this node's seat ----------

/// One item of the seat subscription: this node's standing word, or why
/// the valset could not say.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SeatItem {
    /// `validator` | `resident` | `guest`, or "" while the valset is silent
    pub tier: String,
    pub error: String,
}

/// This node's standing now and after every valset block — the word the
/// org head wears. The view folds it off `rpc.status` and the valset
/// itself; no session prop carries it, so swapping this view changes what
/// the badge calls this device.
pub fn seat(connection: i64) -> ducktape_view_guest::Subscription<SeatItem> {
    ducktape_view_guest::Subscription::run_with(connection, |_| {
        let live = host::subscribe("rpc.live", b"valset");
        stream::once(load_seat()).chain(live.then(|_| load_seat()))
    })
}

async fn load_seat() -> SeatItem {
    match read_seat().await {
        Ok(tier) => SeatItem {
            tier,
            error: String::new(),
        },
        Err(error) => SeatItem {
            tier: String::new(),
            error: failure("Could not read this node's standing", &error),
        },
    }
}

async fn read_seat() -> Result<String, String> {
    let status = host::request("rpc.status", b"{}").await?;
    let status: serde_json::Value =
        serde_json::from_slice(&status).map_err(|error| error.to_string())?;
    let node_key = status["public_key"].as_str().unwrap_or_default().to_owned();
    let validators = query("valset", serde_json::json!("validators")).await?;
    let residents = query("valset", serde_json::json!("residents")).await?;
    Ok(fold_tier(
        &node_key,
        &seat_keys(&validators, "validators"),
        &seat_keys(&residents, "residents"),
    ))
}

/// A valset key list — `{"validators": [[byte, …], …]}` — as hex.
fn seat_keys(reply: &serde_json::Value, seat: &str) -> Vec<String> {
    reply[seat]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|key| {
            key.as_array()
                .map(|bytes| {
                    bytes
                        .iter()
                        .filter_map(|byte| byte.as_u64())
                        .map(|byte| format!("{:02x}", byte as u8))
                        .collect::<String>()
                })
                .unwrap_or_default()
        })
        .collect()
}

/// This node's seat word, as the two valset lists name it.
///
/// AN UNANSWERED VALSET IS NOT A GUEST: an answered valset always carries
/// the chain's own validators, so an empty pair of lists is silence and
/// reads `""` — which is what keeps the badge off the head entirely rather
/// than calling a validator's device a guest.
pub fn fold_tier(node_key: &str, validators: &[String], residents: &[String]) -> String {
    let silent = validators.is_empty() && residents.is_empty();
    let seated = validators.iter().any(|key| key == node_key);
    let resident = residents.iter().any(|key| key == node_key);
    match (silent, seated, resident) {
        (true, _, _) => String::new(),
        (false, true, _) => "validator".into(),
        (false, false, true) => "resident".into(),
        (false, false, false) => "guest".into(),
    }
}

// ---------- the repo namespace ----------

/// One item of the repo-list subscription: the cards, or why not.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RepoListItem {
    pub repos: Vec<ForgeRepo>,
    pub error: String,
}

/// The repo namespace now and after every forge block.
pub fn repos(connection: i64) -> ducktape_view_guest::Subscription<RepoListItem> {
    ducktape_view_guest::Subscription::run_with(connection, |_| on_forge_blocks(load_repos))
}

async fn load_repos() -> RepoListItem {
    match query(FORGE, serde_json::json!("list_repos")).await {
        Ok(reply) => RepoListItem {
            repos: fold_repos(&reply),
            error: String::new(),
        },
        Err(error) => RepoListItem {
            repos: Vec::new(),
            error: failure("Could not read the repositories", &error),
        },
    }
}

/// The committed repo cards: the module's name and head, the head shortened
/// the way every oid on this screen is.
pub fn fold_repos(reply: &serde_json::Value) -> Vec<ForgeRepo> {
    reply["repos"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|repo| ForgeRepo {
            name: repo["name"].as_str().unwrap_or_default().to_owned(),
            head: short_digest(repo["head"].as_str().unwrap_or("(unborn)")),
        })
        .collect()
}

/// An oid as the cards wear it: the first 12 characters.
fn short_digest(digest: &str) -> String {
    digest.chars().take(12).collect()
}

// ---------- one repo ----------

/// One item of the open repo's subscription: its branches and tracker.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RepoItem {
    pub repo: String,
    pub branches: Vec<ForgeBranch>,
    pub items: Vec<ForgeItem>,
    pub error: String,
}

/// The open repo's branches and tracker items, re-read on every forge
/// block. No repo open reads nothing.
pub fn repo(connection: i64, repo: String) -> ducktape_view_guest::Subscription<RepoItem> {
    ducktape_view_guest::Subscription::run_with((connection, repo), |(_, repo)| {
        let repo = repo.clone();
        on_forge_blocks(move || load_repo(repo.clone()))
    })
}

async fn load_repo(repo: String) -> RepoItem {
    if repo.is_empty() {
        return RepoItem::default();
    }
    match read_repo(&repo).await {
        Ok(item) => item,
        Err(error) => RepoItem {
            repo,
            error: failure("Could not read this repository", &error),
            ..RepoItem::default()
        },
    }
}

async fn read_repo(repo: &str) -> Result<RepoItem, String> {
    let refs = query(FORGE, serde_json::json!({ "list_refs": { "repo": repo } })).await?;
    let items = query(FORGE, serde_json::json!({ "list_items": { "repo": repo } })).await?;
    let names = read_names().await;
    Ok(RepoItem {
        repo: repo.to_owned(),
        branches: fold_branches(&refs),
        items: fold_items(&items, &names),
        error: String::new(),
    })
}

pub fn fold_branches(reply: &serde_json::Value) -> Vec<ForgeBranch> {
    reply["refs"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|head| ForgeBranch {
            name: head["name"].as_str().unwrap_or_default().to_owned(),
            head: head["head"].as_str().unwrap_or_default().to_owned(),
        })
        .collect()
}

/// Listing rows from the committed summaries: the wire lists ascending by
/// number, the tracker renders newest first.
pub fn fold_items(reply: &serde_json::Value, names: &Names) -> Vec<ForgeItem> {
    reply["items"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .rev()
        .map(|item| {
            let handle = party_handle(&party_of_json(&item["author"]));
            ForgeItem {
                number: item["number"].as_i64().unwrap_or(0),
                kind: item["kind"].as_str().unwrap_or_default().to_owned(),
                state: item["state"].as_str().unwrap_or_default().to_owned(),
                title: item["title"].as_str().unwrap_or_default().to_owned(),
                author_name: author_display(&handle, names),
                author: handle,
            }
        })
        .collect()
}

// ---------- one item ----------

/// One item of the open item's subscription: the detail pane's whole model.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ItemItem {
    pub repo: String,
    pub number: i64,
    pub kind: String,
    pub title: String,
    pub state: String,
    pub author: String,
    pub branches: String,
    pub body: String,
    pub blocks: Vec<ChatBlock>,
    pub channel_id: String,
    pub source_branch: String,
    pub source_oid: String,
    pub target_oid: String,
    pub merge_oid: String,
    pub diff_rows: Vec<DiffLine>,
    pub diff_truncated: bool,
    pub files_changed: i64,
    pub additions: i64,
    pub deletions: i64,
    pub reviews: Vec<ForgeReview>,
    pub approvals: i64,
    pub change_requests: i64,
    pub error: String,
}

/// The open item in full, re-read on every forge block.
pub fn item(
    connection: i64,
    repo: String,
    number: i64,
) -> ducktape_view_guest::Subscription<ItemItem> {
    ducktape_view_guest::Subscription::run_with((connection, repo, number), |(_, repo, number)| {
        let (repo, number) = (repo.clone(), *number);
        on_forge_blocks(move || load_item(repo.clone(), number))
    })
}

async fn load_item(repo: String, number: i64) -> ItemItem {
    let asked = !repo.is_empty() && number > 0;
    if !asked {
        return ItemItem::default();
    }
    match read_item(&repo, number).await {
        Ok(item) => item,
        Err(error) => ItemItem {
            repo,
            number,
            error: failure("Could not read this item", &error),
            ..ItemItem::default()
        },
    }
}

async fn read_item(repo: &str, number: i64) -> Result<ItemItem, String> {
    let ask = serde_json::json!({ "get_item": { "repo": repo, "number": number } });
    let reply = query(FORGE, ask).await?;
    let detail = &reply["item"];
    if detail.is_null() {
        return Err("item was not found".to_owned());
    }
    let is_pr = detail["kind"].as_str() == Some("pr");
    let diff = match is_pr {
        false => serde_json::Value::Null,
        true => {
            let ask = serde_json::json!({ "pr_diff": { "repo": repo, "number": number } });
            query(FORGE, ask)
                .await
                .map(|reply| reply["pr_diff"].clone())
                .unwrap_or(serde_json::Value::Null)
        }
    };
    let names = read_names().await;
    Ok(fold_item(repo.to_owned(), detail, &diff, &names))
}

/// The item pane's model from the committed detail plus its pinned diff.
pub fn fold_item(
    repo: String,
    detail: &serde_json::Value,
    diff: &serde_json::Value,
    names: &Names,
) -> ItemItem {
    let source_oid = diff["source_oid"].as_str().unwrap_or_default().to_owned();
    let author = party_handle(&party_of_json(&detail["author"]));
    let reviews = fold_reviews(&detail["reviews"], &source_oid, names);
    let source_branch = detail["source_branch"].as_str().unwrap_or_default();
    let target_branch = detail["target_branch"].as_str().unwrap_or_default();
    let branches = match source_branch.is_empty() {
        true => String::new(),
        false => format!("{source_branch} into {target_branch}"),
    };
    let body = detail["body"].as_str().unwrap_or_default().to_owned();
    ItemItem {
        repo,
        number: detail["number"].as_i64().unwrap_or(0),
        kind: detail["kind"].as_str().unwrap_or_default().to_owned(),
        title: detail["title"].as_str().unwrap_or_default().to_owned(),
        state: detail["state"].as_str().unwrap_or_default().to_owned(),
        author: author_display(&author, names),
        branches,
        blocks: body_blocks(&body),
        body,
        channel_id: detail["channel_id"].as_str().unwrap_or_default().to_owned(),
        source_branch: source_branch.to_owned(),
        target_oid: diff["target_oid"].as_str().unwrap_or_default().to_owned(),
        merge_oid: detail["merge_oid"].as_str().unwrap_or_default().to_owned(),
        diff_rows: diff_lines(diff["patch"].as_str().unwrap_or_default()),
        diff_truncated: diff["truncated"].as_bool().unwrap_or(false),
        files_changed: diff["files_changed"].as_i64().unwrap_or(0),
        additions: diff["additions"].as_i64().unwrap_or(0),
        deletions: diff["deletions"].as_i64().unwrap_or(0),
        approvals: tally(&reviews).0,
        change_requests: tally(&reviews).1,
        reviews,
        source_oid,
        error: String::new(),
    }
}

/// The merge box's tallies — approvals and change requests — count the
/// LATEST verdict per author, so a reviewer who approved and then asked for
/// changes counts once. A plain comment is neither.
fn tally(reviews: &[ForgeReview]) -> (i64, i64) {
    let mut latest: Vec<(&str, &str)> = Vec::new();
    for review in reviews {
        if review.verdict == "comment" {
            continue;
        }
        match latest
            .iter_mut()
            .find(|(author, _)| *author == review.author)
        {
            Some(slot) => slot.1 = &review.verdict,
            None => latest.push((&review.author, &review.verdict)),
        }
    }
    let approvals = latest.iter().filter(|(_, v)| *v == "approve").count();
    let change_requests = latest.len() - approvals;
    (
        i64::try_from(approvals).unwrap_or(i64::MAX),
        i64::try_from(change_requests).unwrap_or(i64::MAX),
    )
}

fn fold_reviews(reviews: &serde_json::Value, source_oid: &str, names: &Names) -> Vec<ForgeReview> {
    reviews
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|review| {
            let handle = party_handle(&party_of_json(&review["author"]));
            let commit_oid = review["commit_oid"].as_str().unwrap_or_default();
            let body = review["body"].as_str().unwrap_or_default().to_owned();
            ForgeReview {
                author_name: author_display(&handle, names),
                author: handle,
                verdict: review["verdict"].as_str().unwrap_or_default().to_owned(),
                blocks: body_blocks(&body),
                body,
                commit: short_oid(commit_oid),
                outdated: !source_oid.is_empty() && commit_oid != source_oid,
                created_at: review["created_at"].as_i64().unwrap_or(0),
                comments: review["comments"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|comment| {
                        let body = comment["body"].as_str().unwrap_or_default().to_owned();
                        ForgeReviewComment {
                            anchor: comment_anchor(
                                comment["path"].as_str().unwrap_or_default(),
                                &comment["line"].as_i64().unwrap_or(0).to_string(),
                                comment["side"].as_str().unwrap_or_default(),
                            ),
                            blocks: body_blocks(&body),
                            body,
                        }
                    })
                    .collect(),
            }
        })
        .collect()
}

fn short_oid(oid: &str) -> String {
    oid.chars().take(8).collect()
}

// ---------- the discussion ----------

/// One member of the discussion channel, as the composer's mention menu
/// lists them: the label it shows and the identity a pick writes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatMember {
    pub label: String,
    pub key: String,
}

/// One item of the open item's discussion: the notes and the composer's
/// mention vocabulary.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DiscussionItem {
    pub channel_id: String,
    pub messages: Vec<ChatMessage>,
    pub choices: Vec<ducktape_view_composer::MentionChoice>,
    /// Older notes than the window holds: the list says so rather than
    /// pretending the discussion starts where it does.
    pub clipped: bool,
    pub error: String,
}

/// The item's hidden `forge:<repo>:<n>` channel, re-read on every chat
/// block — a note posted anywhere lands here the same way it does in Chat.
pub fn discussion(
    connection: i64,
    channel_id: String,
) -> ducktape_view_guest::Subscription<DiscussionItem> {
    ducktape_view_guest::Subscription::run_with((connection, channel_id), |(_, channel_id)| {
        let channel_id = channel_id.clone();
        let live = host::subscribe("rpc.live", CHAT.as_bytes());
        let load = move || load_discussion(channel_id.clone());
        stream::once(load()).chain(live.then(move |_| load()))
    })
}

async fn load_discussion(channel_id: String) -> DiscussionItem {
    if channel_id.is_empty() {
        return DiscussionItem::default();
    }
    match read_discussion(&channel_id).await {
        Ok(item) => item,
        Err(error) => DiscussionItem {
            channel_id,
            error: failure("Could not read the discussion", &error),
            ..DiscussionItem::default()
        },
    }
}

async fn read_discussion(channel_id: &str) -> Result<DiscussionItem, String> {
    let ask = serde_json::json!({ "roots": {
        "channel_id": channel_id,
        "before_seq": serde_json::Value::Null,
        "limit": DISCUSSION_LIMIT,
    }});
    let roots = view(CHAT, ask).await?;
    let ask = serde_json::json!({ "members": {
        "channel_id": channel_id,
        "after": serde_json::Value::Null,
        "limit": serde_json::Value::Null,
    }});
    let members = view(CHAT, ask).await?;
    let names = read_names().await;
    Ok(DiscussionItem {
        channel_id: channel_id.to_owned(),
        messages: fold_messages(&roots["roots"]["roots"], &names),
        choices: names.composer_choices(&fold_members(&members["members"]["members"], &names)),
        clipped: roots["roots"]["has_more"].as_bool().unwrap_or(false),
        error: String::new(),
    })
}

/// The timeline rows the screen draws, oldest first as the page answers.
pub fn fold_messages(roots: &serde_json::Value, names: &Names) -> Vec<ChatMessage> {
    roots
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|row| fold_message(row, names))
        .collect()
}

fn fold_message(row: &serde_json::Value, names: &Names) -> ChatMessage {
    let seq = row["seq"].as_i64().unwrap_or(0);
    let edited = row["rev"].as_i64().unwrap_or(0) > 0;
    let deleted = row["deleted"].as_bool().unwrap_or(false);
    let author = row["author"].as_str().unwrap_or_default();
    let wire = blocks_of_json(&row["blocks"]);
    let blocks = match deleted {
        true => vec![deleted_block()],
        false => blocks_view(&wire, names),
    };
    let meta = match edited {
        true => format!("#{seq} (edited)"),
        false => format!("#{seq}"),
    };
    ChatMessage {
        seq,
        author: author_display(author, names),
        meta,
        blocks,
        initial: avatar_initial(author, names),
        avatar_kind: avatar_kind(author, names),
    }
}

/// The composer's mention vocabulary: the channel's members by the label
/// each renders under and the key a pick writes.
pub fn fold_members(members: &serde_json::Value, names: &Names) -> Vec<ChatMember> {
    members
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|member| {
            let party = member["party"].as_str().unwrap_or_default();
            let key = party.strip_prefix("user:").unwrap_or(party);
            ChatMember {
                label: names.member_label(key),
                key: key.to_owned(),
            }
        })
        .collect()
}

/// The note a deep link's `#seq` names, as a list of at most one — the card
/// above the discussion shows it even when the window does not hold it.
pub fn note_at_seq(discussion: &[ChatMessage], seq: i64) -> Vec<ChatMessage> {
    discussion
        .iter()
        .find(|message| message.seq == seq)
        .cloned()
        .into_iter()
        .collect()
}

// ---------- the code browse ----------

/// One item of the tree subscription: one directory at one commit.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TreeItem {
    pub repo: String,
    pub rev: String,
    pub path: String,
    /// Whether the repo has at least one branch: an empty listing can also
    /// be a real empty commit, so the screen must not infer "unborn" from
    /// the rows alone.
    pub born: bool,
    pub entries: Vec<TreeEntry>,
    pub truncated: bool,
    pub error: String,
}

/// One directory listing, pinned to the commit the root listing answered
/// with. Keyed by what it names, so a move re-reads and nothing else does.
pub fn tree(
    connection: i64,
    repo: String,
    rev: String,
    path: String,
) -> ducktape_view_guest::Subscription<TreeItem> {
    ducktape_view_guest::Subscription::run_with(
        (connection, repo, rev, path),
        |(_, repo, rev, path)| stream::once(load_tree(repo.clone(), rev.clone(), path.clone())),
    )
}

async fn load_tree(repo: String, rev: String, path: String) -> TreeItem {
    if repo.is_empty() {
        return TreeItem::default();
    }
    let ask = serde_json::json!({ "tree": { "repo": &repo, "rev": &rev, "path": &path } });
    let read = query(FORGE, ask).await.and_then(|reply| {
        let tree = &reply["tree"];
        match tree.is_null() {
            true => Err("the repository tree was not found".to_owned()),
            false => Ok(tree.clone()),
        }
    });
    match read {
        Ok(tree) => TreeItem {
            repo,
            rev: tree["rev"].as_str().unwrap_or_default().to_owned(),
            path,
            born: tree["born"].as_bool().unwrap_or(false),
            entries: fold_entries(&tree["entries"]),
            truncated: tree["truncated"].as_bool().unwrap_or(false),
            error: String::new(),
        },
        Err(error) => TreeItem {
            repo,
            rev,
            path,
            error: failure("Could not read the repository tree", &error),
            ..TreeItem::default()
        },
    }
}

pub fn fold_entries(entries: &serde_json::Value) -> Vec<TreeEntry> {
    entries
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|entry| TreeEntry {
            name: entry["name"].as_str().unwrap_or_default().to_owned(),
            path: entry["path"].as_str().unwrap_or_default().to_owned(),
            kind: entry["kind"].as_str().unwrap_or_default().to_owned(),
        })
        .collect()
}

/// One item of the blob subscription: one file at the tree's exact commit.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BlobItem {
    pub repo: String,
    pub path: String,
    pub text: String,
    pub truncated: bool,
    pub binary: bool,
    /// The blob is a decoded picture parked under the picture surface,
    /// drawn at `width` × `height`; `text` is empty.
    pub picture: bool,
    pub width: i64,
    pub height: i64,
    pub note: String,
    pub error: String,
}

/// One file read at one commit. A picture pages its bytes in and is parked
/// with the host's picture store; anything else is the bounded text
/// preview, and a Markdown one asks the host to park what it embeds.
pub fn blob(
    connection: i64,
    repo: String,
    rev: String,
    path: String,
    net: String,
) -> ducktape_view_guest::Subscription<BlobItem> {
    ducktape_view_guest::Subscription::run_with(
        (connection, repo, rev, path, net),
        |(_, repo, rev, path, net)| {
            stream::once(load_blob(
                repo.clone(),
                rev.clone(),
                path.clone(),
                net.clone(),
            ))
        },
    )
}

async fn load_blob(repo: String, rev: String, path: String, net: String) -> BlobItem {
    let asked = !repo.is_empty() && !path.is_empty();
    if !asked {
        return BlobItem::default();
    }
    let read = match picture_path(&path) {
        true => read_picture(&repo, &rev, &path).await,
        false => read_text(&repo, &rev, &path, &net).await,
    };
    match read {
        Ok(item) => item,
        Err(error) => BlobItem {
            repo,
            path,
            error: failure("Could not load this file", &error),
            ..BlobItem::default()
        },
    }
}

async fn read_text(repo: &str, rev: &str, path: &str, net: &str) -> Result<BlobItem, String> {
    let ask = serde_json::json!({ "blob": { "repo": repo, "rev": rev, "path": path } });
    let reply = query(FORGE, ask).await?;
    let blob = &reply["blob"];
    if blob.is_null() {
        return Err("the requested file was not found".to_owned());
    }
    let text = blob["text"].as_str().unwrap_or_default().to_owned();
    let binary = blob["binary"].as_bool().unwrap_or(false);
    let item = BlobItem {
        repo: repo.to_owned(),
        path: blob["path"].as_str().unwrap_or(path).to_owned(),
        truncated: blob["truncated"].as_bool().unwrap_or(false),
        binary,
        picture: false,
        width: 0,
        height: 0,
        note: String::new(),
        error: String::new(),
        text,
    };
    let illustrated = markdown_path(&item.path) && !binary;
    if illustrated {
        let rev = blob["rev"].as_str().unwrap_or(rev);
        park_inline_pictures(&item, repo, rev, net).await;
    }
    Ok(item)
}

/// The pictures a Markdown document embeds, parked under it by the host —
/// the addresses are `duck://` refs, repo-relative paths and web URLs, and
/// only the app's one open plane knows how to fetch each kind.
async fn park_inline_pictures(item: &BlobItem, repo: &str, rev: &str, net: &str) {
    let base = format!(
        "duck://forge/{repo}/blob/{}@{rev}{}",
        item.path,
        net_query(net)
    );
    let ask = serde_json::json!({
        "doc": &item.path,
        "source": &item.text,
        "base": base,
    });
    let _ = host::request(
        "picture.inline",
        &serde_json::to_vec(&ask).expect("encodes"),
    )
    .await;
}

/// A picture blob: page it in, hand it to the host's picture store, draw
/// the slot. An over-cap object and a body that does not decode land on the
/// binary plate with the reason as its line, never a failed load.
async fn read_picture(repo: &str, rev: &str, path: &str) -> Result<BlobItem, String> {
    let plate = |note: String| BlobItem {
        repo: repo.to_owned(),
        path: path.to_owned(),
        text: String::new(),
        truncated: false,
        binary: true,
        picture: false,
        width: 0,
        height: 0,
        note,
        error: String::new(),
    };
    let Some(pages) = read_blob_pages(repo, rev, path).await? else {
        return Ok(plate(format!(
            "This picture is larger than the {} MiB preview limit.",
            MAX_PICTURE_BYTES >> 20
        )));
    };
    let ask = serde_json::json!({ "surface": PICTURE_SURFACE, "path": path, "pages": pages });
    let parked = host::request("picture.put", &serde_json::to_vec(&ask).expect("encodes")).await;
    let parked = parked.and_then(|reply| {
        serde_json::from_slice::<serde_json::Value>(&reply).map_err(|error| error.to_string())
    });
    match parked {
        Ok(size) => Ok(BlobItem {
            repo: repo.to_owned(),
            path: path.to_owned(),
            text: String::new(),
            truncated: false,
            binary: false,
            picture: true,
            width: size["width"].as_i64().unwrap_or(0),
            height: size["height"].as_i64().unwrap_or(0),
            note: String::new(),
            error: String::new(),
        }),
        Err(reason) => Ok(plate(format!("This picture did not decode: {reason}."))),
    }
}

/// Page one blob's bytes in (1 MiB pages to eof) as the base64 runs the
/// node answered with, ONE PER PAGE: each run is padded on its own, so the
/// host decodes them separately and joins the bytes — concatenating the
/// text would re-frame the stream at the first page boundary.
///
/// Page 1 asks by the caller's rev; every later page asks by the exact oid
/// page 1 answered, so a branch that moves mid-read cannot hand back pages
/// of two different commits. `None`: the object is past the preview cap —
/// by the size the node announces, or by what arrived.
async fn read_blob_pages(repo: &str, rev: &str, path: &str) -> Result<Option<Vec<String>>, String> {
    let mut pages: Vec<String> = Vec::new();
    let mut rev = rev.to_owned();
    let mut read = 0u64;
    loop {
        let ask = serde_json::json!({ "blob_bytes": {
            "repo": repo,
            "rev": &rev,
            "path": path,
            "offset": read,
            "len": MAX_BLOB_PAGE_BYTES,
        }});
        let reply = query(FORGE, ask).await?;
        let page = &reply["blob_bytes"];
        if page.is_null() {
            return Err("the requested file was not found".to_owned());
        }
        rev = page["rev"].as_str().unwrap_or_default().to_owned();
        let chunk = page["b64"].as_str().unwrap_or_default().to_owned();
        read += base64_len(&chunk);
        let announced_past_cap = page["size"].as_i64().unwrap_or(0) > MAX_PICTURE_BYTES as i64;
        let past_cap = announced_past_cap || read > MAX_PICTURE_BYTES as u64;
        if past_cap {
            return Ok(None);
        }
        let done = page["eof"].as_bool().unwrap_or(true) || chunk.is_empty();
        pages.push(chunk);
        if done {
            return Ok(Some(pages));
        }
    }
}

/// How many bytes a padded base64 run carries.
fn base64_len(b64: &str) -> u64 {
    let padding = b64.bytes().rev().take_while(|byte| *byte == b'=').count() as u64;
    (b64.len() as u64 / 4) * 3 - padding
}

/// The `?net=` a produced `duck://` link carries — the minted digest after
/// the chain id's last `#` — or "" when the producer has no chain id.
fn net_query(chain_id: &str) -> String {
    let digest = chain_id.rsplit_once('#').map(|(_, hex)| hex).unwrap_or("");
    match digest.is_empty() {
        true => String::new(),
        false => format!("?net={digest}"),
    }
}

// ---------- the writes ----------

/// One finished write: which act it was, what the merge produced, and the
/// refusal if any.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActItem {
    /// `review` | `merge`
    pub kind: String,
    pub merge_oid: String,
    pub conflicts: Vec<String>,
    pub error: String,
}

type Act = Pin<Box<dyn Future<Output = ActItem>>>;

#[derive(Default)]
struct Acts {
    pending: Vec<Act>,
    waker: Option<Waker>,
}

thread_local! {
    // One per thread = one per driver, like the guest's request registry: a
    // wasm module has one thread, and every native test drives its own app
    // on its own thread.
    static ACTS: RefCell<Acts> = RefCell::default();
}

fn start(act: impl Future<Output = ActItem> + 'static) -> bool {
    ACTS.with_borrow_mut(|acts| {
        acts.pending.push(Box::pin(act));
        if let Some(waker) = acts.waker.take() {
            waker.wake();
        }
    });
    true
}

/// Every write's outcome, as the kernel answers it.
pub fn acts() -> ducktape_view_guest::Subscription<ActItem> {
    ducktape_view_guest::Subscription::run(|| ActStream)
}

struct ActStream;

impl Stream for ActStream {
    type Item = ActItem;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<ActItem>> {
        ACTS.with_borrow_mut(|acts| {
            let mut finished = None;
            for (index, act) in acts.pending.iter_mut().enumerate() {
                if let Poll::Ready(item) = act.as_mut().poll(cx) {
                    finished = Some((index, item));
                    break;
                }
            }
            let Some((index, item)) = finished else {
                acts.waker = Some(cx.waker().clone());
                return Poll::Pending;
            };
            drop(acts.pending.remove(index));
            Poll::Ready(Some(item))
        })
    }
}

async fn submit(message: serde_json::Value) -> Result<(), String> {
    submit_with_blob(message, None).await
}

async fn submit_with_blob(
    message: serde_json::Value,
    required_blob: Option<&str>,
) -> Result<(), String> {
    let mut op = serde_json::json!({ "target": FORGE, "payload": message });
    if let Some(digest) = required_blob {
        op["required_blob"] = serde_json::Value::String(digest.to_owned());
    }
    host::request("op.submit", &serde_json::to_vec(&op).expect("encodes"))
        .await
        .map(|_| ())
}

/// Submit a batched review pinned to the source head the reviewer saw. A
/// review and its line comments are ONE transaction on the wire.
pub fn review_submit(
    repo: String,
    number: i64,
    verdict: String,
    body: String,
    commit_oid: String,
    comments: Vec<ForgeDraftComment>,
) -> bool {
    start(async move {
        let refusal = match commit_oid.is_empty() {
            true => Some("the pull request diff has not loaded yet".to_owned()),
            false => None,
        };
        let error = match refusal {
            Some(refusal) => refusal,
            None => {
                let message = serde_json::json!({ "submit_review": {
                    "repo": repo,
                    "number": number,
                    "verdict": verdict,
                    "body": body,
                    "commit_oid": commit_oid,
                    "comments": review_comments(&comments),
                }});
                submit(message).await.err().unwrap_or_default()
            }
        };
        let error = match error.is_empty() {
            true => error,
            false => failure("The review was not sent", &error),
        };
        ActItem {
            kind: "review".to_owned(),
            error,
            ..ActItem::default()
        }
    })
}

/// The staged drafts as the module's own `ReviewComment`. The stage gate
/// already refuses an unusable draft, so nothing is dropped here that the
/// composer would ever have taken.
fn review_comments(comments: &[ForgeDraftComment]) -> Vec<serde_json::Value> {
    comments
        .iter()
        .filter_map(|draft| {
            let line = draft.line.parse::<u32>().ok()?;
            Some(serde_json::json!({
                "path": draft.path,
                "line": line,
                "side": draft.side,
                "body": draft.body,
            }))
        })
        .collect()
}

/// Merge an open PR the way the wire demands it: the merge commit is
/// computed by the configured installed service from its read-only store.
/// The service lands the minimal pack, and the double-CAS'd
/// `MergePr` goes out over that. A local conflict submits NOTHING.
pub fn merge(
    repo: String,
    number: i64,
    source_branch: String,
    expected_source_oid: String,
    prev_target_oid: String,
) -> bool {
    start(async move {
        match merge_pr(
            repo,
            number,
            source_branch,
            expected_source_oid,
            prev_target_oid,
        )
        .await
        {
            Ok(item) => item,
            Err(error) => ActItem {
                kind: "merge".to_owned(),
                error: failure("The merge did not go through", &error),
                ..ActItem::default()
            },
        }
    })
}

async fn merge_pr(
    repo: String,
    number: i64,
    source_branch: String,
    expected_source_oid: String,
    prev_target_oid: String,
) -> Result<ActItem, String> {
    let pinned = !expected_source_oid.is_empty() && !prev_target_oid.is_empty();
    if !pinned {
        return Err("the pull request diff has not loaded yet".to_owned());
    }
    let ask = serde_json::json!({
        "repo": &repo,
        "ours": &prev_target_oid,
        "theirs": &expected_source_oid,
        "message": format!("Merge pull request #{number} from {source_branch}"),
    });
    let config = host::request("asset.read", b"service.json")
        .await
        .map_err(|error| format!("Forge merge service is not configured: {error}"))?;
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Service {
        account: u64,
        route: String,
    }
    let service: Service = serde_json::from_slice(&config)
        .map_err(|error| format!("invalid service.json: {error}"))?;
    let configured = service.account > 0 && !service.route.is_empty();
    if !configured {
        return Err("service.json must name the Forge service account and route".into());
    }
    let request = serde_json::json!({"account":service.account,"route":service.route,"method":"post","path":"/merge",
        "headers":[{"name":"content-type","value":"application/json"}],"body":serde_json::to_vec(&ask).expect("merge request")});
    let reply = host::request(
        "net.request",
        &serde_json::to_vec(&request).expect("application request"),
    )
    .await?;
    let reply: serde_json::Value =
        serde_json::from_slice(&reply).map_err(|error| error.to_string())?;
    use base64::Engine as _;
    let body = base64::engine::general_purpose::STANDARD
        .decode(
            reply["body_b64"]
                .as_str()
                .ok_or("merge service returned no body")?,
        )
        .map_err(|error| error.to_string())?;
    let built: serde_json::Value =
        serde_json::from_slice(&body).map_err(|error| error.to_string())?;
    if reply["head"]["status"].as_u64() != Some(200) {
        return Err(built["error"]
            .as_str()
            .unwrap_or("merge service refused request")
            .into());
    }
    let conflicts: Vec<String> = built["conflicts"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|path| path.as_str().unwrap_or_default().to_owned())
        .collect();
    if !conflicts.is_empty() {
        return Ok(ActItem {
            kind: "merge".to_owned(),
            merge_oid: String::new(),
            conflicts,
            error: String::new(),
        });
    }
    let merge_oid = built["merge_oid"].as_str().unwrap_or_default().to_owned();
    let pack = base64::engine::general_purpose::STANDARD
        .decode(
            built["pack_b64"]
                .as_str()
                .ok_or("merge service returned no pack")?,
        )
        .map_err(|error| error.to_string())?;
    let digest = host::request("blob.put", &pack).await?;
    let digest = String::from_utf8(digest).map_err(|error| error.to_string())?;
    let message = serde_json::json!({ "merge_pr": {
        "repo": repo,
        "number": number,
        "prev_target_oid": prev_target_oid,
        "expected_source_oid": expected_source_oid,
        "merge_oid": &merge_oid,
        "pack_digest": &digest,
    }});
    submit_with_blob(message, Some(&digest)).await?;
    Ok(ActItem {
        kind: "merge".to_owned(),
        merge_oid,
        conflicts: Vec::new(),
        error: String::new(),
    })
}

// ---------- the OS doors ----------

/// `forge.open_link` — a link a body or the reader's document carried.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub url: String,
}

/// `forge.copy` — the host puts `text` on the clipboard and toasts `label`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Copy {
    pub text: String,
    pub label: String,
}

pub fn open_link(url: &str) -> bool {
    notify("forge.open_link", &Link { url: url.into() })
}

pub fn copy(text: &str, label: &str) -> bool {
    notify(
        "forge.copy",
        &Copy {
            text: text.into(),
            label: label.into(),
        },
    )
}

fn notify<T: Serialize>(operation: &str, payload: &T) -> bool {
    let bytes = serde_json::to_vec(payload).expect("an intent encodes");
    host::notify(operation, &bytes);
    true
}

// ---------- the deep link ----------

/// What a `duck://forge/…` address the app routed here names: a repo, an
/// item (with the discussion note its `#seq` lands on), or a file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ForgeLink {
    pub repo: String,
    pub number: i64,
    pub seq: i64,
    pub path: String,
    pub rev: String,
}

/// `duck://forge/<repo>[/<number>[#<seq>]]` and
/// `duck://forge/<repo>/blob/<path>[@<rev>]`, as the app's open plane
/// spells them. Anything else names no repo and opens nothing.
pub fn forge_link(url: &str) -> ForgeLink {
    let Some(rest) = url.strip_prefix("duck://forge/") else {
        return ForgeLink::default();
    };
    let rest = rest.split('?').next().unwrap_or_default();
    let (repo, rest) = match rest.split_once('/') {
        Some((repo, rest)) => (repo, rest),
        None => (rest, ""),
    };
    let link = ForgeLink {
        repo: repo.to_owned(),
        ..ForgeLink::default()
    };
    if rest.is_empty() {
        return link;
    }
    if let Some(blob) = rest.strip_prefix("blob/") {
        let (path, rev) = match blob.rsplit_once('@') {
            Some((path, rev)) => (path, rev),
            None => (blob, ""),
        };
        return ForgeLink {
            path: path.to_owned(),
            rev: rev.to_owned(),
            ..link
        };
    }
    let (number, seq) = match rest.split_once('#') {
        Some((number, seq)) => (number, seq.parse::<i64>().unwrap_or(0)),
        None => (rest, 0),
    };
    ForgeLink {
        number: number.parse::<i64>().unwrap_or(0),
        seq,
        ..link
    }
}

// ---------- the readings ----------

pub fn plural(count: i64, one: &str, many: &str) -> String {
    let noun = if count == 1 { one } else { many };
    format!("{count} {noun}")
}

/// The tracker's Pull requests / Issues split; the Code seat lists nothing.
pub fn filter_forge_items(items: &[ForgeItem], tab: &str, side: &str, query: &str) -> Vec<ForgeItem> {
    let kind = match tab {
        "pulls" => "pr",
        "issues" => "issue",
        _ => return Vec::new(),
    };
    let wanted = query.trim().to_lowercase();
    items
        .iter()
        .filter(|item| item.kind == kind)
        .filter(|item| item_is_open(item) == (side == "open"))
        .filter(|item| item_matches(item, &wanted))
        .cloned()
        .collect()
}

/// Whether an item stands on the Open side of its tracker. One reading for
/// the list and the tab chip both, so the count can never disagree with the
/// rows under it — and a closed pull request is closed, which is what
/// `state != "merged"` used to deny.
pub fn item_is_open(item: &ForgeItem) -> bool {
    item.state == "open"
}

/// Whether a row survives what the reader typed: its title or its number.
/// An empty filter keeps everything.
fn item_matches(item: &ForgeItem, wanted: &str) -> bool {
    if wanted.is_empty() {
        return true;
    }
    let titled = item.title.to_lowercase().contains(wanted);
    let numbered = format!("#{}", item.number).contains(wanted);
    titled || numbered
}

/// How many rows each side of a tracker holds, as its switch shows them.
pub fn forge_side_count(items: &[ForgeItem], kind: &str, side: &str) -> i64 {
    let counted = items
        .iter()
        .filter(|item| item.kind == kind)
        .filter(|item| item_is_open(item) == (side == "open"))
        .count();
    i64::try_from(counted).unwrap_or(i64::MAX)
}

/// The tab count chips — the open work on each tracker.
pub fn forge_open_count(items: &[ForgeItem], kind: &str) -> i64 {
    forge_side_count(items, kind, "open")
}

/// The seat an open item belongs to, by its kind: a pull request lights the
/// Pull requests tab, an issue the Issues tab. A kind the tracker has no
/// seat for leaves the code browse lit.
pub fn kind_tab(kind: &str) -> String {
    match kind {
        "pr" => "pulls".to_owned(),
        "issue" => "issues".to_owned(),
        _ => "code".to_owned(),
    }
}

/// The merged-state banner: the short merge oid plus the branch line.
pub fn forge_merge_note(merge_oid: &str, branches: &str) -> String {
    let short: String = merge_oid.chars().take(8).collect();
    match branches.is_empty() {
        true => format!("Merged as {short}."),
        false => format!("Merged {branches} as {short}."),
    }
}

/// A state or seat word as a badge reads it: `open` → `Open`.
pub fn state_label(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The file pane's line under a failed read: the loader's note, or the
/// refusal as a sentence.
pub fn blob_note(note: &str, error: &str) -> String {
    match error.is_empty() {
        true => note.to_owned(),
        false => error.to_owned(),
    }
}

/// A review verdict key as its timeline verb.
pub fn verdict_label(verdict: &str) -> String {
    match verdict {
        "approve" => "approved".into(),
        "request_changes" => "requested changes".into(),
        _ => "commented".into(),
    }
}

pub fn tree_width_after_delta(width: f64, delta: f64, viewport: f64) -> f64 {
    let maximum = (viewport * 0.45).clamp(180.0, 480.0);
    (width + delta).clamp(180.0, maximum)
}

/// A Markdown document reads through the document surface; forge carries no
/// language field, so the path is the one discriminator.
pub fn markdown_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".markdown")
}

/// Whether a committed path is drawn as a picture rather than read as text.
/// The extension is the path's call — the wires only say binary-or-text.
pub fn picture_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or_default();
    let Some((_, extension)) = name.rsplit_once('.') else {
        return false;
    };
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg"
    )
}

/// `1024 × 768` — the caption under a drawn picture.
pub fn picture_caption(width: i64, height: i64) -> String {
    format!("{width} × {height}")
}

/// The binary plate's line: the loader's reason when it gave one, else the
/// generic one.
pub fn binary_note(text: &str) -> String {
    match text.is_empty() {
        true => "This is not text — the reader shows no preview for it.".to_owned(),
        false => text.to_owned(),
    }
}

/// `duck://forge/<repo>/<number>?net=…` — one issue or PR.
pub fn duck_forge_item_link(repo: &str, number: i64, chain_id: &str) -> String {
    format!("duck://forge/{repo}/{number}{}", net_query(chain_id))
}

/// The installed service supplies the authenticated Git endpoint.
pub fn forge_push_instructions() -> String {
    "Push a repository through your network’s installed Git service. Ask its operator for the authenticated Git endpoint and signing configuration.".into()
}

/// The label a picked-but-unstaged line wears above the composer, empty
/// when no line is picked — the composer keys its whole visibility on this.
pub fn forge_comment_target(path: &str, line: &str, side: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    comment_anchor(path, line, side)
}

/// What the branch selector reads while no branch stands at the browse's
/// commit: the commit itself abbreviated, or the root read still in flight.
pub fn commit_label(rev: &str) -> String {
    if rev.is_empty() {
        return "…".to_owned();
    }
    rev.chars().take(12).collect()
}

/// The repository switcher's options: the forge's repositories by name.
pub fn repo_names(repos: &[ForgeRepo]) -> Vec<String> {
    repos.iter().map(|repo| repo.name.clone()).collect()
}

/// The branch selector's options: the open repo's born branches by name.
pub fn branch_names(branches: &[ForgeBranch]) -> Vec<String> {
    branches.iter().map(|branch| branch.name.clone()).collect()
}

/// The commit a branch's head stood on when the repo slice was read, or
/// empty for a name the slice does not hold.
pub fn forge_branch_head(branches: &[ForgeBranch], name: &str) -> String {
    branches
        .iter()
        .find(|branch| branch.name == name)
        .map(|branch| branch.head.clone())
        .unwrap_or_default()
}

/// The branch the code browse's pinned commit is the head of. The picked
/// branch wins while it still stands there, then `dev`, then `main`, then
/// the first branch at that commit; empty when none does.
pub fn forge_tree_branch(branches: &[ForgeBranch], picked: &str, rev: &str) -> String {
    if rev.is_empty() {
        return String::new();
    }
    let standing_at_rev = |name: &str| {
        branches
            .iter()
            .any(|branch| branch.name == name && branch.head == rev)
    };
    let preferred = [picked, "dev", "main"]
        .into_iter()
        .find(|name| standing_at_rev(name));
    let first_at_rev = || {
        branches
            .iter()
            .find(|branch| branch.head == rev)
            .map(|branch| branch.name.as_str())
    };
    preferred
        .or_else(first_at_rev)
        .unwrap_or_default()
        .to_owned()
}

/// The directory a committed path sits in, in the forge tree's own
/// spelling: the repository root is `""`, never `/`.
pub fn forge_parent(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir.to_owned(),
        None => String::new(),
    }
}

/// The reader header's path, gated on the revision the file was opened
/// under: a preview opened at an older commit was retired by the branch
/// move. Unfolding another directory in the tree retires nothing.
pub fn forge_file_header(opened_rev: &str, rev: &str, path: &str) -> String {
    let same_commit = opened_rev == rev;
    match same_commit {
        true => path.to_owned(),
        false => String::new(),
    }
}

/// The file a repository lands on when it opens — the README at its root,
/// preferring `README.md` over any other spelling, as every forge does.
/// Empty when the root carries none, which leaves the reader's own empty
/// state on screen.
pub fn readme_of(entries: &[TreeEntry]) -> String {
    let rank = |name: &str| {
        let name = name.to_ascii_lowercase();
        let (stem, extension) = match name.rsplit_once('.') {
            Some((stem, extension)) => (stem.to_owned(), extension.to_owned()),
            None => (name, String::new()),
        };
        if stem != "readme" {
            return None;
        }
        match extension.as_str() {
            "md" => Some(0),
            "markdown" => Some(1),
            "" => Some(2),
            _ => Some(3),
        }
    };
    entries
        .iter()
        .filter(|entry| entry.kind == "file")
        .filter_map(|entry| rank(&entry.name).map(|rank| (rank, entry)))
        // the listing is sorted, so the first of equal rank is stable
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, entry)| entry.path.to_owned())
        .unwrap_or_default()
}

/// One breadcrumb per path segment: the directory each one stands for, and
/// the name it shows. The last crumb is the file itself — it stands for no
/// directory, which is what makes it the one that does not press.
pub fn crumbs_of(path: &str) -> Vec<(String, String)> {
    let mut walked = String::new();
    let names = path.split('/').filter(|segment| !segment.is_empty());
    let mut crumbs: Vec<(String, String)> = names
        .map(|name| {
            if !walked.is_empty() {
                walked.push('/');
            }
            walked.push_str(name);
            (walked.to_owned(), name.to_owned())
        })
        .collect();
    if let Some(last) = crumbs.last_mut() {
        last.0 = String::new();
    }
    crumbs
}

/// The PR stats line: `3 files, +12 −4`.
pub fn forge_stats(files: i64, additions: i64, deletions: i64) -> String {
    format!(
        "{}, +{additions} −{deletions}",
        plural(files, "file", "files")
    )
}

/// Where a line comment sits, as a person reads it: `main.rs:12`, and the
/// side only when it is the removed line.
pub fn comment_anchor(path: &str, line: &str, side: &str) -> String {
    match side {
        "old" => format!("{path}:{line} (removed line)"),
        _ => format!("{path}:{line}"),
    }
}

/// Stage one line comment, or replace the one already on that line.
///
/// Returns `staged` UNCHANGED when the anchor or body is not usable — an
/// empty path (a deleted file's rows), a blank body, a line number that is
/// not a positive `u32`, or a full list.
pub fn stage_forge_comment(
    staged: Vec<ForgeDraftComment>,
    path: String,
    line: String,
    side: String,
    body: String,
) -> Vec<ForgeDraftComment> {
    let anchored = !path.is_empty() && line.parse::<u32>().is_ok_and(|no| no > 0);
    let sided = side == "new" || side == "old";
    let usable_body = !body.trim().is_empty() && body.len() <= MAX_REVIEW_COMMENT_BYTES;
    if !anchored || !sided || !usable_body || path.len() > MAX_PATH_BYTES {
        return staged;
    }
    let comment = ForgeDraftComment {
        anchor: comment_anchor(&path, &line, &side),
        path,
        line,
        side,
        body,
    };
    let mut staged = staged;
    match staged.iter().position(|row| row.anchor == comment.anchor) {
        Some(at) => staged[at] = comment,
        None if staged.len() < MAX_REVIEW_COMMENTS => staged.push(comment),
        None => {}
    }
    staged
}

/// Drop the comment staged at one anchor. A miss leaves the list alone.
pub fn drop_forge_comment(staged: Vec<ForgeDraftComment>, anchor: &str) -> Vec<ForgeDraftComment> {
    let mut staged = staged;
    staged.retain(|row| row.anchor != anchor);
    staged
}

/// The staged list is at the module's per-review cap, so the composer must
/// refuse to take another.
pub fn forge_comment_cap_reached(staged: &[ForgeDraftComment]) -> bool {
    staged.len() >= MAX_REVIEW_COMMENTS
}

/// The PR's source head moved under the open item. What was written against
/// the old diff — the staged comments, the line comment being typed —
/// cannot be carried across: the anchor is `(path, line, side)` into a
/// specific patch, and the review would be submitted pinning the NEW head.
pub fn forge_branch_moved(next_oid: &str, current_oid: &str) -> bool {
    !next_oid.is_empty() && !current_oid.is_empty() && next_oid != current_oid
}

/// Discarded work is never silent. This says WHY the staged comments
/// vanished, and only when there were some to lose.
pub fn staged_comment_drop_note(dropped: bool) -> String {
    match dropped {
        false => String::new(),
        true => "The branch moved while comments were staged. They anchored to lines in the old diff, so they were discarded rather than posted against the new one.".to_owned(),
    }
}

/// The draft, or nothing once the act that consumed it landed.
pub fn keep_draft(consumed: bool, draft: &str) -> String {
    match consumed {
        true => String::new(),
        false => draft.to_owned(),
    }
}

/// The staged comments, or none once the branch moved under them.
pub fn keep_staged(dropped: bool, staged: Vec<ForgeDraftComment>) -> Vec<ForgeDraftComment> {
    match dropped {
        true => Vec::new(),
        false => staged,
    }
}

/// What a parked deep link names, or what the browse already stood on when
/// the link named nothing there.
pub fn keep_focus(focused: String, current: &str) -> String {
    match focused.is_empty() {
        true => current.to_owned(),
        false => focused,
    }
}

/// The address a freshly routed link names, or "" when the app routed
/// nothing new — the same link twice is two landings, so the count decides,
/// never the text.
pub fn routed_link(fresh: bool, url: &str) -> String {
    match fresh {
        true => url.to_owned(),
        false => String::new(),
    }
}

/// The seq a fresh landing names, or 0 when nothing new landed.
pub fn landed_seq_of(fresh: bool, seq: i64) -> i64 {
    if fresh { seq } else { 0 }
}

/// A read's phase from what it answered: a refusal is `failed`, an answer
/// is `ready`.
pub fn phase_of(error: &str) -> String {
    match error.is_empty() {
        true => "ready".to_owned(),
        false => "failed".to_owned(),
    }
}

/// Which act a finished write was, as the handler matches on it.
pub(crate) fn act_of(kind: &str) -> crate::Act {
    match kind {
        "merge" => crate::Act::Merge,
        _ => crate::Act::Review,
    }
}

/// The composer's scope: the app keys the document it edits on this, and
/// splits the channel back out of it when a note is sent
/// (`backend::composer_scope`).
pub fn composer_scope(endpoint: &str, channel_id: &str) -> String {
    match channel_id.is_empty() {
        true => String::new(),
        false => format!("{endpoint}\u{1f}{channel_id}"),
    }
}

// ---------- the patch, painted ----------

/// A unified patch as the rows the diff pane draws.
pub fn diff_lines(diff: &str) -> Vec<DiffLine> {
    // A patch line has no durable id. Namespace the whole row set by the
    // exact patch: unchanged rebuilds retain identity; any patch edit
    // deliberately drops row state instead of transferring it.
    use std::hash::{Hash as _, Hasher as _};

    let mut patch_hasher = std::collections::hash_map::DefaultHasher::new();
    diff.hash(&mut patch_hasher);
    let patch_key = patch_hasher.finish() as i64;
    let mut rows = Vec::new();
    let mut old_no = 0i64;
    let mut new_no = 0i64;
    // The path every following code row is anchored to, taken from the
    // patch's own `+++ b/…` header: a comment cannot be authored from a row
    // that does not know its file. The `--- a/…` side is held only to name
    // a deleted file, whose head side is `/dev/null`.
    let mut path = String::new();
    let mut old_path = String::new();
    // What the open hunk still owes on each side. A hunk header DECLARES how
    // many lines its body covers, and while either side is still owed one,
    // every line is body content — never a header. That budget is the ONLY
    // thing separating a real `+++ b/<path>` header from a source line
    // reading `++ x`, which a patch writes as `+++ x`.
    let mut old_left = 0i64;
    let mut new_left = 0i64;
    for line in diff.lines() {
        let inside_hunk_body = old_left > 0 || new_left > 0;
        if !inside_hunk_body {
            if let Some(source) = removed_side_path(line) {
                old_path = source;
                continue;
            }
            if let Some(target) = added_side_path(line) {
                path = target;
                rows.push(file_row(&old_path, &path));
                old_path.clear();
                continue;
            }
            if let Some(name) = binary_file(line) {
                rows.push(named_file_row(&format!("{name} (binary)")));
                continue;
            }
            if let Some(span) = hunk_span(line) {
                old_no = span.old_start;
                new_no = span.new_start;
                old_left = span.old_len;
                new_left = span.new_len;
                rows.push(diff_row(
                    "hunk",
                    String::new(),
                    String::new(),
                    "",
                    line,
                    "",
                    "",
                ));
                continue;
            }
            // `diff --git`, `index`, a mode or rename line: git's own
            // bookkeeping between files, which the one file row already says
            continue;
        }
        // `\ No newline at end of file` is a note ABOUT the previous line. It
        // holds no position on either side, so it consumes neither a line
        // number nor the hunk's budget.
        if line.starts_with('\\') {
            rows.push(diff_row(
                "note",
                String::new(),
                String::new(),
                "",
                line.trim_start_matches('\\').trim(),
                "",
                "",
            ));
            continue;
        }
        match line.chars().next() {
            Some('+') => {
                rows.push(diff_row(
                    "add",
                    String::new(),
                    new_no.to_string(),
                    "+",
                    &line[1..],
                    &path,
                    "new",
                ));
                new_no += 1;
                new_left -= 1;
            }
            Some('-') => {
                rows.push(diff_row(
                    "del",
                    old_no.to_string(),
                    String::new(),
                    "-",
                    &line[1..],
                    &path,
                    "old",
                ));
                old_no += 1;
                old_left -= 1;
            }
            Some(_) | None => {
                let text = line.strip_prefix(' ').unwrap_or(line);
                rows.push(diff_row(
                    "ctx",
                    old_no.to_string(),
                    new_no.to_string(),
                    "",
                    text,
                    &path,
                    "new",
                ));
                old_no += 1;
                new_no += 1;
                old_left -= 1;
                new_left -= 1;
            }
        }
    }
    for (index, row) in rows.iter_mut().enumerate() {
        row.key = patch_key.wrapping_add(i64::try_from(index).unwrap_or(i64::MAX));
    }
    rows
}

/// The one row that opens a changed file: its name, and whether it is new
/// or gone. Not a commentable position, so it carries no path or side.
fn file_row(old_path: &str, new_path: &str) -> DiffLine {
    let name = match (old_path.is_empty(), new_path.is_empty()) {
        (true, false) => format!("{new_path} (new file)"),
        (false, false) => new_path.to_owned(),
        (false, true) => format!("{old_path} (deleted)"),
        (true, true) => "(unnamed file)".to_owned(),
    };
    named_file_row(&name)
}

fn named_file_row(name: &str) -> DiffLine {
    diff_row("file", String::new(), String::new(), "", name, "", "")
}

/// The head-side path a `+++ b/<path>` header names. A pure deletion writes
/// `+++ /dev/null`, which names no file on the head side and yields an
/// empty path — its rows are then uncommentable, which is correct.
fn added_side_path(line: &str) -> Option<String> {
    header_path(line.strip_prefix("+++ ")?, "b/")
}

/// The base-side path a `--- a/<path>` header names; a new file's is empty.
fn removed_side_path(line: &str) -> Option<String> {
    header_path(line.strip_prefix("--- ")?, "a/")
}

fn header_path(target: &str, marker: &str) -> Option<String> {
    if target == "/dev/null" {
        return Some(String::new());
    }
    // git writes `a/<path>` / `b/<path>`; a patch produced without prefixes
    // writes the path bare, so strip the marker only when it is there.
    Some(target.strip_prefix(marker).unwrap_or(target).to_owned())
}

/// The head-side name a `Binary files a/<x> and b/<x> differ` line carries:
/// git writes no `+++` for one, so this is its only file row.
fn binary_file(line: &str) -> Option<String> {
    let rest = line.strip_prefix("Binary files ")?;
    let (_, head) = rest.split_once(" and ")?;
    let head = head.strip_suffix(" differ")?;
    header_path(head, "b/")
}

fn diff_row(
    kind: &str,
    old_no: String,
    new_no: String,
    sign: &str,
    text: &str,
    path: &str,
    side: &str,
) -> DiffLine {
    DiffLine {
        key: 0,
        kind: kind.into(),
        old_no,
        new_no,
        path: path.into(),
        side: side.into(),
        sign: sign.into(),
        text: text.to_owned(),
    }
}

/// A hunk header's two starting line numbers and the two line counts its
/// body covers.
struct HunkSpan {
    old_start: i64,
    new_start: i64,
    old_len: i64,
    new_len: i64,
}

/// `@@ -138,9 +138,12 @@ …` → the starts and the lengths. A range written
/// without a comma covers exactly one line (`@@ -1 +1 @@`).
fn hunk_span(line: &str) -> Option<HunkSpan> {
    let body = line.strip_prefix("@@ ")?;
    let (ranges, _) = body.split_once(" @@")?;
    let (old, new) = ranges.split_once(' ')?;
    let range = |range: &str| -> Option<(i64, i64)> {
        let digits = range.trim_start_matches(['-', '+']);
        let (start, len) = match digits.split_once(',') {
            Some((start, len)) => (start, len.parse().ok()?),
            None => (digits, 1),
        };
        Some((start.parse().ok()?, len))
    };
    let (old_start, old_len) = range(old)?;
    let (new_start, new_len) = range(new)?;
    Some(HunkSpan {
        old_start,
        new_start,
        old_len,
        new_len,
    })
}
