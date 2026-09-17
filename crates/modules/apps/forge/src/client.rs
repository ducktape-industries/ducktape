//! forge's CLIENT view model — the rendered tracker item (reviews, merge box
//! tallies) and the op-refresh classification a feed-following UI scopes its
//! reloads with. module-owned beside `tracker_iface.rs` (same wire, same
//! vocabulary), pure data-in/data-out, ui.wasm-portable like `chat::client`.
//! the NATIVE half of the merge lane (mirror fetch, scratch merge, pack build)
//! is deliberately NOT here — that is a host capability and lives with the
//! shell, exactly as the review/merge WIRE stays in `interface.rs`.

use crate::interface::PrDiff;
use crate::tracker_iface::{ItemDetail, ItemState, ReviewVerdict};
use crate::{ForgeMsg, decode_msg};
use chat::client::{ChatBlock, NameDirectory, author_display, author_handle, paragraph_blocks};

/// One rendered line-anchored review comment. `anchor` is display-ready
/// (`src/main.rs:14 (new)`), so the view never re-derives diff vocabulary.
#[derive(Clone, Debug, Hash, PartialEq, Default, serde::Serialize)]
pub struct ReviewCommentRow {
    pub anchor: String,
    pub body: String,
    /// the body through the chat tokenizer — see [`body_blocks`].
    pub blocks: Vec<ChatBlock>,
}

/// One submitted review, rendered.
#[derive(Clone, Debug, Hash, PartialEq, Default, serde::Serialize)]
pub struct ReviewRow {
    /// the rendered author handle (`user:{hex}`, ...) — avatar identity.
    pub author: String,
    /// the display name derived from the handle.
    pub author_name: String,
    /// `approve` | `request_changes` | `comment` (the wire's snake_case).
    pub verdict: String,
    pub body: String,
    /// the body through the chat tokenizer — see [`body_blocks`].
    pub blocks: Vec<ChatBlock>,
    /// the short source-head oid the review pinned.
    pub commit: String,
    /// the source branch moved past `commit` — early-GitHub "outdated".
    pub outdated: bool,
    /// the block height the review landed at.
    pub created_at: i64,
    pub comments: Vec<ReviewCommentRow>,
}

/// The item pane's full view model: one tracker item with its reviews, diff
/// facts, and merge-box tallies, every field display-ready.
#[derive(Clone, Debug, Hash, PartialEq, Default)]
pub struct ItemView {
    pub number: i64,
    /// `issue` | `pr` (the wire's snake_case).
    pub kind: String,
    /// `open` | `closed` | `merged`.
    pub state: String,
    pub title: String,
    pub body: String,
    /// the body through the chat tokenizer — see [`body_blocks`].
    pub blocks: Vec<ChatBlock>,
    pub author: String,
    pub author_name: String,
    /// the block height the item was opened at.
    pub created_at: i64,
    /// the item's hidden discussion channel (`forge:<repo>:<n>`).
    pub channel_id: String,
    /// PR-only; empty for issues.
    pub source_branch: String,
    pub target_branch: String,
    /// the merge commit hex once merged; empty until then.
    pub merge_oid: String,
    /// the diff's pinned heads — empty when no diff was available.
    pub source_oid: String,
    pub target_oid: String,
    pub diff: String,
    pub diff_truncated: bool,
    pub files_changed: i64,
    pub additions: i64,
    pub deletions: i64,
    pub reviews: Vec<ReviewRow>,
    /// merge-box tallies: the LATEST verdict per reviewer counts (an approval
    /// supersedes the same reviewer's earlier request-changes), `comment`
    /// verdicts never tally. advisory only — the wire never gates MergePr.
    pub approvals: i64,
    pub change_requests: i64,
}

/// Which committed surfaces one applied forge op invalidates. `number == 0`
/// means no single item; `refs_moved` marks branch-head movement (an open
/// PR's diff and merge box on that repo are stale even though no tracker
/// record changed).
#[derive(Clone, Debug, Hash, PartialEq, Default)]
pub struct ForgeRefresh {
    pub repo: String,
    pub number: i64,
    pub refs_moved: bool,
}

/// One prose body through the CHAT tokenizer — the same grammar a chat row
/// is rendered with, so a `duck://` ref, a `[label](url)` and a bare
/// `https://` in an issue, a PR description or a review comment become the
/// same `Mark::Link` span the app opens through its one open plane. Forge
/// prose is markdown wherever it is typed; it does not get a second dialect
/// just because it was typed on this surface. No roster here, so a `@handle`
/// stays plain ink (a forge body is not addressed to a channel).
fn body_blocks(body: &str) -> Vec<ChatBlock> {
    paragraph_blocks(body)
}

/// Build the item pane's view model from the committed detail + its pinned
/// diff (PRs with a locally-computable patch; `None` for issues and for
/// diff-query misses).
pub fn item_view(detail: &ItemDetail, diff: Option<&PrDiff>, names: &NameDirectory) -> ItemView {
    let source_oid = diff.map(|d| d.source_oid.clone()).unwrap_or_default();
    let author = author_handle(&detail.summary.author);
    let reviews: Vec<ReviewRow> = detail
        .reviews
        .iter()
        .map(|review| {
            let handle = author_handle(&review.author);
            ReviewRow {
                author_name: author_display(&handle, names),
                author: handle,
                verdict: verdict_key(review.verdict).into(),
                blocks: body_blocks(&review.body),
                body: review.body.clone(),
                commit: short_oid(&review.commit_oid),
                outdated: !source_oid.is_empty() && review.commit_oid != source_oid,
                created_at: i64::try_from(review.created_at).unwrap_or(i64::MAX),
                comments: review
                    .comments
                    .iter()
                    .map(|comment| ReviewCommentRow {
                        anchor: format!(
                            "{}:{} ({})",
                            comment.path,
                            comment.line,
                            match comment.side {
                                crate::tracker_iface::DiffSide::Old => "old",
                                crate::tracker_iface::DiffSide::New => "new",
                            }
                        ),
                        blocks: body_blocks(&comment.body),
                        body: comment.body.clone(),
                    })
                    .collect(),
            }
        })
        .collect();
    let (approvals, change_requests) = tally(&reviews);
    ItemView {
        number: i64::try_from(detail.summary.number).unwrap_or(i64::MAX),
        kind: kind_key(detail.summary.kind).into(),
        state: state_key(detail.summary.state).into(),
        title: detail.summary.title.clone(),
        blocks: body_blocks(&detail.body),
        body: detail.body.clone(),
        author_name: author_display(&author, names),
        author,
        created_at: i64::try_from(detail.summary.created_at).unwrap_or(i64::MAX),
        channel_id: detail.channel_id.clone(),
        source_branch: detail.source_branch.clone().unwrap_or_default(),
        target_branch: detail.target_branch.clone().unwrap_or_default(),
        merge_oid: detail.merge_oid.clone().unwrap_or_default(),
        source_oid,
        target_oid: diff.map(|d| d.target_oid.clone()).unwrap_or_default(),
        diff: diff.map(|d| d.patch.clone()).unwrap_or_default(),
        diff_truncated: diff.is_some_and(|d| d.truncated),
        files_changed: diff.map_or(0, |d| i64::try_from(d.files_changed).unwrap_or(i64::MAX)),
        additions: diff.map_or(0, |d| i64::try_from(d.additions).unwrap_or(i64::MAX)),
        deletions: diff.map_or(0, |d| i64::try_from(d.deletions).unwrap_or(i64::MAX)),
        reviews,
        approvals,
        change_requests,
    }
}

/// Classify one applied forge op into the surfaces it invalidates. `Err` =
/// undecodable — the caller reloads everything it shows.
pub fn refresh_from_op(payload: &[u8]) -> Result<ForgeRefresh, String> {
    let msg = decode_msg(payload)?;
    let refresh = match msg {
        ForgeMsg::PushRefs { repo, .. } => ForgeRefresh {
            repo,
            number: 0,
            refs_moved: true,
        },
        ForgeMsg::OpenIssue { repo, .. } | ForgeMsg::OpenPr { repo, .. } => ForgeRefresh {
            repo,
            number: 0,
            refs_moved: false,
        },
        ForgeMsg::EditItem { repo, number, .. }
        | ForgeMsg::SetItemState { repo, number, .. }
        | ForgeMsg::SubmitReview { repo, number, .. } => ForgeRefresh {
            repo,
            number: i64::try_from(number).unwrap_or(i64::MAX),
            refs_moved: false,
        },
        ForgeMsg::MergePr { repo, number, .. } => ForgeRefresh {
            repo,
            number: i64::try_from(number).unwrap_or(i64::MAX),
            // the merge moves the target branch under the same op.
            refs_moved: true,
        },
    };
    Ok(refresh)
}

/// Merge-box tallies over rendered reviews: the latest verdict per reviewer,
/// `comment` verdicts skipped.
fn tally(reviews: &[ReviewRow]) -> (i64, i64) {
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

fn verdict_key(verdict: ReviewVerdict) -> &'static str {
    match verdict {
        ReviewVerdict::Approve => "approve",
        ReviewVerdict::RequestChanges => "request_changes",
        ReviewVerdict::Comment => "comment",
    }
}

fn kind_key(kind: crate::tracker_iface::ItemKind) -> &'static str {
    match kind {
        crate::tracker_iface::ItemKind::Issue => "issue",
        crate::tracker_iface::ItemKind::Pr => "pr",
    }
}

fn state_key(state: ItemState) -> &'static str {
    match state {
        ItemState::Open => "open",
        ItemState::Closed => "closed",
        ItemState::Merged => "merged",
    }
}

/// A 40-hex oid down to the 8-char short form every git surface renders.
fn short_oid(oid: &str) -> String {
    oid.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker_iface::{
        DiffSide, ItemKind, ItemSummary, ReviewComment, ReviewView, channel_id_for,
    };
    use chat::Party;

    fn detail(reviews: Vec<ReviewView>) -> ItemDetail {
        ItemDetail {
            summary: ItemSummary {
                number: 1,
                kind: ItemKind::Pr,
                title: "Greeting feature".into(),
                state: ItemState::Open,
                author: Party::Key(vec![0xbe; 32]),
                created_at: 3250,
                updated_at: 3250,
            },
            body: "Switches main.rs to the friendly greeting.".into(),
            channel_id: channel_id_for("lab", 1),
            source_branch: Some("feature/greeting".into()),
            target_branch: Some("dev".into()),
            merge_oid: None,
            reviews,
        }
    }

    fn review(author: u8, verdict: ReviewVerdict, commit: &str) -> ReviewView {
        ReviewView {
            author: Party::Key(vec![author; 32]),
            verdict,
            body: "looked at it".into(),
            commit_oid: commit.into(),
            comments: vec![ReviewComment {
                path: "main.rs".into(),
                line: 1,
                side: DiffSide::New,
                body: "nice".into(),
            }],
            created_at: 3300,
        }
    }

    fn diff() -> PrDiff {
        PrDiff {
            source_oid: "1993a2e33fbf0f44a03fdddd213710957cffddf1".into(),
            target_oid: "dbcfdc52503c3d7cb5bc4e5ba1818e90f51e026d".into(),
            files_changed: 1,
            additions: 1,
            deletions: 1,
            patch: "diff --git a/main.rs b/main.rs".into(),
            truncated: false,
        }
    }

    #[test]
    fn item_view_renders_reviews_and_pins() {
        let current = "1993a2e33fbf0f44a03fdddd213710957cffddf1";
        let view = item_view(
            &detail(vec![
                review(0xaa, ReviewVerdict::Approve, current),
                review(
                    0xbb,
                    ReviewVerdict::RequestChanges,
                    "0000000000000000000000000000000000000000",
                ),
            ]),
            Some(&diff()),
            &NameDirectory::default(),
        );
        assert_eq!(view.kind, "pr");
        assert_eq!(view.state, "open");
        assert_eq!(view.channel_id, "forge:lab:1");
        assert_eq!(view.source_branch, "feature/greeting");
        assert_eq!(view.source_oid, current);
        assert_eq!(view.reviews.len(), 2);
        assert!(!view.reviews[0].outdated, "review at the head is current");
        assert!(
            view.reviews[1].outdated,
            "review behind the head is outdated"
        );
        assert_eq!(view.reviews[0].comments[0].anchor, "main.rs:1 (new)");
        assert_eq!(view.approvals, 1);
        assert_eq!(view.change_requests, 1);
    }

    /// Every forge prose surface — the item body, a review body, a review
    /// comment — carries link spans, so the app opens what a body points at
    /// instead of drawing it as dead ink.
    #[test]
    fn every_forge_body_tokenizes_its_links() {
        let mut detail = detail(vec![review(
            0xaa,
            ReviewVerdict::Approve,
            "1993a2e33fbf0f44a03fdddd213710957cffddf1",
        )]);
        detail.body = "see [the plan](duck://page/plan?net=d0cdf950)".into();
        detail.reviews[0].body = "context at https://ducktape.industries/x".into();
        detail.reviews[0].comments[0].body = "also [#58](duck://forge/lab/58)".into();
        let view = item_view(&detail, None, &NameDirectory::default());

        let links = |blocks: &[ChatBlock]| -> Vec<String> {
            blocks
                .iter()
                .flat_map(|block| block.spans.iter())
                .filter(|span| !span.link.is_empty())
                .map(|span| span.link.clone())
                .collect()
        };
        assert_eq!(links(&view.blocks), ["duck://page/plan?net=d0cdf950"]);
        assert_eq!(
            links(&view.reviews[0].blocks),
            ["https://ducktape.industries/x"]
        );
        assert_eq!(
            links(&view.reviews[0].comments[0].blocks),
            ["duck://forge/lab/58"]
        );
    }

    #[test]
    fn tallies_take_the_latest_verdict_per_reviewer() {
        let head = "1993a2e33fbf0f44a03fdddd213710957cffddf1";
        let view = item_view(
            &detail(vec![
                review(0xaa, ReviewVerdict::RequestChanges, head),
                review(0xaa, ReviewVerdict::Approve, head),
                review(0xbb, ReviewVerdict::Comment, head),
            ]),
            Some(&diff()),
            &NameDirectory::default(),
        );
        assert_eq!(view.approvals, 1, "the re-review supersedes");
        assert_eq!(view.change_requests, 0);
    }

    #[test]
    fn issue_view_has_no_diff_or_branch_facts() {
        let mut plain = detail(Vec::new());
        plain.summary.kind = ItemKind::Issue;
        plain.source_branch = None;
        plain.target_branch = None;
        let view = item_view(&plain, None, &NameDirectory::default());
        assert_eq!(view.kind, "issue");
        assert!(view.source_branch.is_empty());
        assert!(view.source_oid.is_empty());
        assert!(view.diff.is_empty());
        assert_eq!(view.files_changed, 0);
    }

    #[test]
    fn refresh_classifies_ops_by_surface() {
        let push = crate::encode_msg(&ForgeMsg::PushRefs {
            repo: "lab".into(),
            updates: Vec::new(),
            pack_digest: None,
            cert: None,
        });
        let refreshed = refresh_from_op(&push).expect("push classifies");
        assert_eq!(refreshed.repo, "lab");
        assert_eq!(refreshed.number, 0);
        assert!(refreshed.refs_moved);

        let review = crate::encode_msg(&ForgeMsg::SubmitReview {
            repo: "lab".into(),
            number: 1,
            verdict: ReviewVerdict::Approve,
            body: String::new(),
            commit_oid: "1993a2e33fbf0f44a03fdddd213710957cffddf1".into(),
            comments: Vec::new(),
        });
        let refreshed = refresh_from_op(&review).expect("review classifies");
        assert_eq!(refreshed.number, 1);
        assert!(!refreshed.refs_moved);

        let merge = crate::encode_msg(&ForgeMsg::MergePr {
            repo: "lab".into(),
            number: 1,
            prev_target_oid: "dbcfdc52503c3d7cb5bc4e5ba1818e90f51e026d".into(),
            expected_source_oid: "1993a2e33fbf0f44a03fdddd213710957cffddf1".into(),
            merge_oid: "0000000000000000000000000000000000000000".into(),
            pack_digest: "00".repeat(32),
        });
        let refreshed = refresh_from_op(&merge).expect("merge classifies");
        assert_eq!(refreshed.number, 1);
        assert!(refreshed.refs_moved, "the merge moves the target branch");
    }

    #[test]
    fn undecodable_payload_is_an_error_not_a_guess() {
        assert!(refresh_from_op(&[0xff, 0xfe]).is_err());
    }
}
