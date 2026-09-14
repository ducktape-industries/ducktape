use super::*;
use crate::BellTarget;

#[derive(Clone, Debug, Hash, PartialEq)]
pub struct BellData {
    pub unread: i64,
    pub items: Vec<BellItem>,
    pub presentations: Vec<BellPresentation>,
}

/// Load the current account's attribution notifications and unread count.
pub async fn load_bell(rpc: String, expected_account: String) -> Result<BellData, AppError> {
    async {
        let rpc = rpc_client(&rpc)?;
        let Some(account) = local_account(&rpc).await? else {
            return Ok(BellData {
                unread: 0,
                items: Vec::new(),
                presentations: Vec::new(),
            });
        };
        ensure_bell_account(&expected_account, account)?;
        let items = load_bell_items(&rpc, account).await?;
        let key = local_user_key()
            .await
            .map(|key| hex_encode(&key))
            .unwrap_or_default();
        let account = account.to_string();
        let unread = bell_unread_count(&items, &account, &key);
        let visible = bell_visible_items(&items, &account, &key);
        let presentations = enrich_bell(&rpc, visible).await;
        Ok(BellData {
            unread,
            items,
            presentations,
        })
    }
    .await
    .map_err(app_error)
}

async fn load_bell_items(rpc: &RpcClient, account: u64) -> Result<Vec<BellItem>, String> {
    // The index pages oldest first. Walk its bounded retained queue before
    // taking the latest window; reversing the first page hides new items.
    let mut rows = Vec::new();
    let mut from_seq = 0;
    for _ in 0..=inbox::MAX_ITEMS_PER_ACCOUNT / 256 {
        let listed: inbox::index::InboxViewReply = rpc
            .view(
                "inbox",
                &inbox::index::InboxViewQuery::List {
                    account,
                    from_seq,
                    limit: Some(256),
                },
            )
            .await?;
        let inbox::index::InboxViewReply::Items(page) = listed else {
            return Err("the inbox returned the wrong list reply".to_string());
        };
        let Some(last) = page.last() else { break };
        let next = last.seq.checked_add(1).ok_or("inbox sequence overflow")?;
        if next <= from_seq {
            return Err("inbox pagination did not advance".into());
        }
        from_seq = next;
        let complete = page.len() < 256;
        rows.extend(page);
        if rows.len() > inbox::MAX_ITEMS_PER_ACCOUNT {
            return Err("inbox changed while loading; try again".into());
        }
        if complete {
            break;
        }
    }
    let items: Vec<_> = rows
        .iter()
        .rev()
        .map(inbox::client::bell_item_from_row)
        .collect();
    Ok(items)
}

fn ensure_bell_account(expected: &str, actual: u64) -> Result<(), String> {
    if expected.parse::<u64>().ok() == Some(actual) {
        Ok(())
    } else {
        Err("The active account changed. Reopen alerts and try again.".into())
    }
}

/// Mark everything at or below `up_to_seq` read (signed by the local user).
pub async fn mark_bell_read(
    rpc: String,
    password: String,
    expected_account: String,
    up_to_seq: i64,
) -> Result<BellDelta, AppError> {
    async {
        if up_to_seq <= 0 {
            return Ok(());
        }
        let rpc = rpc_client(&rpc)?;
        let account = local_account(&rpc)
            .await?
            .ok_or_else(|| "this key is on no account".to_string())?;
        ensure_bell_account(&expected_account, account)?;
        let up_to_seq = u64::try_from(up_to_seq).unwrap_or(0);
        signed_write(
            &rpc,
            "inbox",
            inbox::encode_msg(&inbox::InboxMsg::MarkRead { account, up_to_seq }),
            password,
        )
        .await
        .map(|_height| ())
    }
    .await
    .map_err(app_error)?;
    Ok(BellDelta {
        kind: "read".into(),
        up_to_seq,
        ..BellDelta::default()
    })
}

/// The app retains the complete bounded inbox so its filtered badge and read
/// watermark agree even when the visible window only contains fifty rows.
pub fn apply_bell(mut items: Vec<BellItem>, delta: BellDelta) -> Vec<BellItem> {
    match delta.kind.as_str() {
        "delivered" => {
            if !items.iter().any(|item| item.seq == delta.item.seq) {
                items.push(delta.item);
            }
            items.sort_unstable_by_key(|item| std::cmp::Reverse(item.seq));
            items.truncate(inbox::MAX_ITEMS_PER_ACCOUNT);
        }
        "read" => {
            for item in &mut items {
                if item.seq <= delta.up_to_seq {
                    item.read = true;
                }
            }
        }
        "cleared" => items.retain(|item| item.seq > delta.up_to_seq),
        _ => {}
    }
    items
}

fn bell_noise(item: &BellItem, account: &str, key: &str) -> bool {
    let own_actor = (!account.is_empty() && item.actor == format!("account:{account}"))
        || (!key.is_empty() && item.actor == format!("key:{key}"));
    own_actor && matches!(item.reason.as_str(), "authorship" | "ownership")
}

pub fn bell_visible_items(items: &[BellItem], account: &str, key: &str) -> Vec<BellItem> {
    items
        .iter()
        .filter(|item| !bell_noise(item, account, key))
        .take(50)
        .cloned()
        .collect()
}

pub fn bell_unread_count(items: &[BellItem], account: &str, key: &str) -> i64 {
    items
        .iter()
        .filter(|item| !item.read && !bell_noise(item, account, key))
        .count() as i64
}

pub fn merge_bell_loaded(
    current: Vec<BellItem>,
    loaded: Vec<BellItem>,
    read: i64,
    cleared: i64,
) -> Vec<BellItem> {
    let mut rows: std::collections::BTreeMap<_, _> =
        loaded.into_iter().map(|item| (item.seq, item)).collect();
    for item in current {
        rows.entry(item.seq)
            .and_modify(|row| row.read |= item.read)
            .or_insert(item);
    }
    rows.into_values()
        .rev()
        .filter(|item| item.seq > cleared)
        .take(inbox::MAX_ITEMS_PER_ACCOUNT)
        .map(|mut item| {
            item.read |= item.seq <= read;
            item
        })
        .collect()
}

pub fn bell_head(items: Vec<BellItem>) -> i64 {
    inbox::client::bell_head_seq(&items)
}

#[derive(Clone, Debug, Hash, PartialEq)]
pub struct BellPresentation {
    pub seq: i64,
    pub title: String,
    pub detail: String,
    pub target: BellTarget,
    pub object: String,
    pub number: i64,
    pub anchor: String,
}

impl Default for BellPresentation {
    fn default() -> Self {
        Self {
            seq: 0,
            title: String::new(),
            detail: String::new(),
            target: BellTarget::Unavailable,
            object: String::new(),
            number: 0,
            anchor: String::new(),
        }
    }
}

pub fn bell_account_items(items: Vec<BellItem>, old: &str, new: &str) -> Vec<BellItem> {
    if old == new { items } else { Vec::new() }
}

fn bell_actor(item: &BellItem, names: &NameDirectory) -> String {
    let handle = match item.actor.split_once(':') {
        Some(("account", id)) => format!("acct:{id}"),
        Some(("key", id)) => format!("user:{id}"),
        Some(("module", id)) => return bell_title(id),
        _ => return "System".into(),
    };
    names.member_label(&handle)
}

fn bell_summary(item: &BellItem, actor: &str) -> BellPresentation {
    let title = match (item.kind.as_str(), item.reason.as_str()) {
        (kind, reason) if kind != "added" => format!("{} changed · {actor}", bell_title(reason)),
        (_, "mention") => format!("{actor} mentioned you"),
        (_, "assignment") => format!("Assignment activity · {actor}"),
        (_, "authorship") => format!("Activity on your post · {actor}"),
        (_, "ownership") => format!("Activity on an item you own · {actor}"),
        (_, "result") => format!("{actor} shared a result"),
        (_, "report") => format!("{actor} shared a report"),
        (_, "credit") => format!("{actor} credited you"),
        (_, other) => format!("{} · {actor}", bell_title(other)),
    };
    BellPresentation {
        seq: item.seq,
        title,
        detail: match item.source.split('/').next().unwrap_or("") {
            "chat" => "Chat activity",
            "pages" => "Page activity",
            "forge" => "Repository activity",
            "runs" => "Agent action",
            "tasks" => "Task activity",
            _ => "Activity details are not available in this app",
        }
        .into(),
        ..BellPresentation::default()
    }
}

pub fn bell_openable(item: &BellItem, presentations: &[BellPresentation]) -> bool {
    presentations.iter().any(|entry| {
        entry.seq == item.seq && entry.target != BellTarget::Unavailable && !entry.object.is_empty()
    })
}

pub fn bell_presentation(item: &BellItem, presentations: &[BellPresentation]) -> BellPresentation {
    presentations
        .iter()
        .find(|entry| entry.seq == item.seq)
        .cloned()
        .unwrap_or_else(|| bell_summary(item, &bell_actor(item, ReaderFacts::cached().names())))
}

pub fn merge_bell_presentations(
    items: Vec<BellItem>,
    current: Vec<BellPresentation>,
    loaded: Vec<BellPresentation>,
) -> Vec<BellPresentation> {
    items
        .iter()
        .filter_map(|item| {
            loaded
                .iter()
                .find(|entry| entry.seq == item.seq)
                .or_else(|| current.iter().find(|entry| entry.seq == item.seq))
                .cloned()
        })
        .collect()
}

fn bell_preview(text: &str) -> String {
    text.chars()
        .take(240)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn bell_link(entry: &BellPresentation, chain: String) -> String {
    match entry.target {
        BellTarget::Message => duck_channel_message_link(entry.object.clone(), entry.number, chain),
        BellTarget::Page => duck_page_link(entry.object.clone(), chain),
        BellTarget::Forge => duck_forge_item_link(entry.object.clone(), entry.number, chain),
        BellTarget::Repo => duck_forge_repo_link(entry.object.clone(), chain),
        BellTarget::Run => duck_run_link(entry.object.clone(), chain),
        BellTarget::Unavailable => String::new(),
    }
}

/// The page a bell entry names, read off pages' VIEW lane: the block's page
/// opens and the block anchors, titled by the page's opening line.
async fn bell_page(
    rpc: &RpcClient,
    block_id: String,
    entry: &mut BellPresentation,
) -> Result<(), String> {
    let Some(block) = view_block(rpc, &block_id).await? else {
        entry.detail = "Page content not found".into();
        return Ok(());
    };
    if block.block_id != block_id {
        return Err("wrong page block".into());
    }
    entry.detail = bell_preview(&block.text);
    entry.target = BellTarget::Page;
    entry.object = block.page_id.clone();
    entry.anchor = block.block_id.clone();
    if block.page_id != block.block_id
        && let Some(root) = view_block(rpc, &block.page_id).await?
        && root.block_id == block.page_id
    {
        entry.detail = format!("{} · {}", bell_preview(&root.text), entry.detail);
    }
    Ok(())
}

/// One block off pages' view lane; `None` for an id the index does not hold.
async fn view_block(
    rpc: &RpcClient,
    block_id: &str,
) -> Result<Option<::pages::index::PageBlockRow>, String> {
    let reply: ::pages::index::PagesViewReply = rpc
        .view(
            "pages",
            &::pages::index::PagesViewQuery::GetBlock {
                block_id: block_id.to_owned(),
            },
        )
        .await?;
    match reply {
        ::pages::index::PagesViewReply::Block(block) => Ok(block),
        _ => Ok(None),
    }
}

async fn bell_source(
    rpc: &RpcClient,
    item: &BellItem,
    entry: &mut BellPresentation,
) -> Result<(), String> {
    let mut source = item.source.splitn(3, '/');
    let (module, kind, object) = (
        source.next().unwrap_or(""),
        source.next().unwrap_or(""),
        source.next().unwrap_or(""),
    );
    match (module, kind) {
        ("chat", "message") => {
            let reply: ChatViewReply = rpc
                .view(
                    "chat",
                    &ChatViewQuery::Message {
                        message_id: object.into(),
                    },
                )
                .await?;
            match reply {
                ChatViewReply::Message(Some(row)) if row.message_id == object && !row.deleted => {
                    entry.detail = bell_preview(&row.text);
                    if entry.detail.is_empty() {
                        entry.detail = "Message attachment".into();
                    }
                    entry.target = BellTarget::Message;
                    entry.object = row.channel_id;
                    entry.number =
                        i64::try_from(row.seq).map_err(|_| "message sequence overflow")?;
                }
                ChatViewReply::Message(Some(row)) if row.deleted => {
                    entry.detail = "Message deleted".into()
                }
                ChatViewReply::Message(None) => entry.detail = "Message not found".into(),
                _ => return Err("wrong message reply".into()),
            }
        }
        ("pages", "block") => bell_page(rpc, object.into(), entry).await?,
        ("pages", "comment") => {
            // The comment's thread, off pages' VIEW lane: the comment's
            // text is in it and the thread's target is the page to open.
            let reply: ::pages::index::PagesViewReply = rpc
                .view(
                    "pages",
                    &::pages::index::PagesViewQuery::ThreadOfComment {
                        comment_id: object.into(),
                    },
                )
                .await?;
            let ::pages::index::PagesViewReply::Thread(Some(thread)) = reply else {
                entry.detail = "Comment not found".into();
                return Ok(());
            };
            let Some(comment) = thread.comments.iter().find(|comment| comment.id == object) else {
                return Err("wrong comment".into());
            };
            if comment.deleted {
                entry.detail = "Comment deleted".into();
                return Ok(());
            }
            bell_page(rpc, thread.target.clone(), entry).await?;
            entry.detail = bell_preview(&comment.text);
        }
        ("forge", "item" | "review") => {
            let (repo, number): (String, u64) = if kind == "review" {
                let (repo, number, _review): (String, u64, u64) =
                    serde_json::from_str(object).map_err(|e| e.to_string())?;
                (repo, number)
            } else {
                serde_json::from_str(object).map_err(|e| e.to_string())?
            };
            let reply: ::forge::ForgeReply = rpc
                .query(
                    "forge",
                    &::forge::ForgeQuery::GetItem {
                        repo: repo.clone(),
                        number,
                    },
                )
                .await?;
            let ::forge::ForgeReply::Item(Some(detail)) = reply else {
                entry.detail = "Issue or pull request not found".into();
                return Ok(());
            };
            if detail.summary.number != number {
                return Err("wrong forge item".into());
            }
            let status = match detail.summary.state {
                ::forge::ItemState::Open => "Open",
                ::forge::ItemState::Closed => "Closed",
                ::forge::ItemState::Merged => "Merged",
            };
            entry.detail = format!(
                "{} #{} · {} · {status}",
                repo,
                number,
                bell_preview(&detail.summary.title)
            );
            entry.target = BellTarget::Forge;
            entry.object = repo;
            entry.number = i64::try_from(number).map_err(|_| "item number overflow")?;
        }
        ("forge", "repo" | "ref") => {
            let (repo, branch): (String, String) = if kind == "ref" {
                serde_json::from_str(object).map_err(|e| e.to_string())?
            } else {
                (
                    serde_json::from_str(object).map_err(|e| e.to_string())?,
                    String::new(),
                )
            };
            let reply: ::forge::ForgeReply = rpc
                .query("forge", &::forge::ForgeQuery::HeadOf { repo: repo.clone() })
                .await?;
            if !matches!(reply, ::forge::ForgeReply::Head(_)) {
                return Err("wrong repository reply".into());
            }
            entry.detail = if branch.is_empty() {
                format!("Repository {repo}")
            } else {
                format!("{repo} · branch {branch}")
            };
            entry.target = BellTarget::Repo;
            entry.object = repo;
        }
        ("tasks", "task") => {
            let reply: ::tasks::WorkReply = rpc
                .query(
                    "tasks",
                    &::tasks::WorkQuery::Task(::tasks::TaskQuery::Get {
                        task_id: object.into(),
                    }),
                )
                .await?;
            let ::tasks::WorkReply::Task(::tasks::TaskReply::Task(Some(task))) = reply else {
                entry.detail = "Task not found".into();
                return Ok(());
            };
            if task.id != object {
                return Err("wrong task".into());
            }
            let status = match task.status {
                ::tasks::TaskStatus::Open => "Open",
                ::tasks::TaskStatus::InProgress => "In progress",
                ::tasks::TaskStatus::Done => "Done",
            };
            entry.detail = format!("{} · {status}", bell_preview(&task.title));
        }
        ("tasks", "job") => {
            let reply: ::tasks::WorkReply = rpc
                .query(
                    "tasks",
                    &::tasks::WorkQuery::Job(::tasks::JobsQuery::Get {
                        job_id: object.into(),
                    }),
                )
                .await?;
            let ::tasks::WorkReply::Job(::tasks::JobsReply::Job(Some(job))) = reply else {
                entry.detail = "Job not found".into();
                return Ok(());
            };
            if job.job_id != object {
                return Err("wrong job".into());
            }
            let status = match job.status {
                ::tasks::JobStatus::Pending => "Pending",
                ::tasks::JobStatus::Processing => "In progress",
                ::tasks::JobStatus::Done => "Done",
                ::tasks::JobStatus::Failed => "Failed",
                ::tasks::JobStatus::Cancelled => "Cancelled",
            };
            entry.detail = format!("{} · {status}", bell_title(&job.kind));
        }
        ("runs", "action_request") => {
            let reply: ::runs::RunsReply = rpc
                .query(
                    "runs",
                    &::runs::RunsQuery::ActionRequest {
                        request_id: object.into(),
                    },
                )
                .await?;
            let ::runs::RunsReply::ActionRequest(Some(request)) = reply else {
                entry.detail = "Agent action not found".into();
                return Ok(());
            };
            if request.request_id != object {
                return Err("wrong action request".into());
            }
            let status = match request.status {
                ::runs::ActionStatus::AwaitingProgram => "Awaiting program",
                ::runs::ActionStatus::Claimed { .. } => "In progress",
                ::runs::ActionStatus::Completed { .. } => "Completed",
                ::runs::ActionStatus::Rejected { .. } => "Rejected",
            };
            entry.detail = format!(
                "{} · {} · {status}",
                bell_title(&request.operation),
                request.target
            );
            entry.target = BellTarget::Run;
            entry.object = ::runs::dispatch_id_for(&request.run_id);
        }
        _ => {}
    }
    Ok(())
}

async fn enrich_bell(rpc: &RpcClient, items: Vec<BellItem>) -> Vec<BellPresentation> {
    use futures::{StreamExt, stream};
    let facts = ReaderFacts::current().await;
    stream::iter(items)
        .map(|item| {
            let names = facts.names();
            async move {
                let mut entry = bell_summary(&item, &bell_actor(&item, names));
                if bell_source(rpc, &item, &mut entry).await.is_err() {
                    entry.detail =
                        "Couldn't load this alert's details. Reopen alerts to retry.".into();
                    entry.target = BellTarget::Unavailable;
                    entry.object.clear();
                }
                entry
            }
        })
        .buffered(4)
        .collect()
        .await
}

pub fn bell_missing_items(items: Vec<BellItem>, contexts: &[BellPresentation]) -> Vec<BellItem> {
    items
        .into_iter()
        .filter(|item| !contexts.iter().any(|entry| entry.seq == item.seq))
        .collect()
}

pub async fn load_bell_presentations(
    rpc: String,
    items: Vec<BellItem>,
) -> Result<Vec<BellPresentation>, AppError> {
    let client = rpc_client(&rpc).map_err(app_error)?;
    Ok(enrich_bell(&client, items).await)
}

/// The relation reason as words, including source-defined reason names.
pub fn bell_title(kind: &str) -> String {
    let words = kind.replace('_', " ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => words,
    }
}

#[cfg(test)]
#[path = "bell_tests.rs"]
mod tests;
