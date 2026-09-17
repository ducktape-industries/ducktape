//! the tracker's public wire surface — issue / pull-request / review types.
//!
//! forge's tracker is GitHub-shaped: issues and PRs share ONE per-repo number
//! space (so `#42` is unambiguous), a PR's source is a real branch in the SAME
//! repo (no forks), and a review is the batched GitHub flow — one verdict, an
//! optional body, and zero or more line-anchored diff comments submitted
//! together. every item owns a hidden chat channel (`forge:<repo>:<number>`)
//! that carries its free-form discussion; the item's BODY lives HERE, on the
//! record, so authorship stays origin-derived (a chat follow-up would be
//! attributed to the forge module, not the opening user).
//!
//! authorship reuses chat's [`Party`] so the app renders forge authors
//! through the exact same display-name path as chat messages.

use chat::Party;
use serde::{Deserialize, Serialize};

// ---- write-time caps (consensus constants) ---------------------------------
// enforced deterministically BEFORE staging, so an oversized op rejects
// identically on every validator. shared here so clients can pre-validate.

/// item / review titles.
pub const MAX_TITLE_BYTES: usize = 256;
/// item bodies and review bodies (markdown).
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// a branch short name ("feature/x") — also a consensus-visible key.
pub const MAX_BRANCH_BYTES: usize = 128;
/// line comments per submitted review.
pub const MAX_REVIEW_COMMENTS: usize = 64;
/// one line comment's body.
pub const MAX_REVIEW_COMMENT_BYTES: usize = 16 * 1024;
/// a diff comment's file path.
pub const MAX_PATH_BYTES: usize = 512;
/// ref updates in one atomic push op.
pub const MAX_REFS_PER_PUSH: usize = 32;
/// reviews per PR; further submissions are rejected.
pub const MAX_REVIEWS_PER_ITEM: usize = 256;
/// OPEN issues + PRs a repo may hold at once (they share one number space, so
/// one ceiling covers both). closing or merging an item frees its slot —
/// there is no delete op, so this is the whole of the defense against an
/// unbounded number of live items.
pub const MAX_OPEN_ITEMS_PER_REPO: usize = 4096;
/// one actor's share of [`MAX_OPEN_ITEMS_PER_REPO`]: no single account may
/// hold more than this many OPEN items in one repo, so the repo cap cannot be
/// filled by one account crowding out everyone else.
pub const MAX_OPEN_ITEMS_PER_ACTOR: usize = 256;
/// branches one repo may hold at once. deletes are always allowed, so this is
/// a ceiling on live branches, not on a repo's history.
pub const MAX_BRANCHES_PER_REPO: usize = 1024;

/// an item's lifecycle state. `Merged` is PR-only and terminal.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ItemState {
    Open,
    Closed,
    Merged,
}

/// what an item IS — issues and PRs share the number space, so listings carry
/// the kind explicitly.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ItemKind {
    Issue,
    Pr,
}

/// the reviewer's overall verdict — GitHub's three review outcomes. approvals
/// are ADVISORY: they render in the merge box but never gate `MergePr` (branch
/// protection is future work).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ReviewVerdict {
    Approve,
    RequestChanges,
    Comment,
}

/// which side of the diff a line comment anchors to.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DiffSide {
    /// the deletion (base) side.
    Old,
    /// the addition (head) side.
    New,
}

/// one line-anchored diff comment inside a review. anchors are (path, line,
/// side) against the diff at the review's `commit_oid` — if the branch moves
/// past that commit the comment renders as "outdated" (no position tracking
/// across force-pushes; early-GitHub semantics).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewComment {
    pub path: String,
    pub line: u32,
    pub side: DiffSide,
    pub body: String,
}

/// one submitted review, immutable once staged.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewView {
    pub author: Party,
    pub verdict: ReviewVerdict,
    pub body: String,
    /// the PR source head hex (40-char sha1) the review was made against —
    /// the outdated-detection anchor.
    pub commit_oid: String,
    pub comments: Vec<ReviewComment>,
    pub created_at: u64,
}

/// one item row in a listing.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ItemSummary {
    pub number: u64,
    pub kind: ItemKind,
    pub title: String,
    pub state: ItemState,
    pub author: Party,
    pub created_at: u64,
    pub updated_at: u64,
}

/// the full item: the summary row plus body, discussion channel, and — for a
/// PR — branches, merge oid, and reviews.
///
/// `deny_unknown_fields` composes with the flattened summary: the flat-map
/// buffer only hands [`ItemSummary`] the keys IT names, and the leftovers are
/// what the deny checks — so an unknown key is refused at whichever level it
/// belongs to, not silently absorbed by the flatten.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ItemDetail {
    #[serde(flatten)]
    pub summary: ItemSummary,
    pub body: String,
    /// the item's hidden discussion channel id (`forge:<repo>:<number>`).
    pub channel_id: String,
    /// PR-only: the source branch short name.
    pub source_branch: Option<String>,
    /// PR-only: the target branch short name (normally "dev").
    pub target_branch: Option<String>,
    /// PR-only: the merge commit hex once merged.
    pub merge_oid: Option<String>,
    pub reviews: Vec<ReviewView>,
}

/// one born branch in a [`crate::ForgeReply::Refs`] listing.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RefHead {
    /// the branch SHORT name ("main", "feature/x").
    pub name: String,
    /// the branch head oid as 40-char sha1 hex.
    pub head: String,
}

/// one ref command inside an atomic [`crate::ForgeMsg::PushRefs`]: a per-ref
/// compare-and-swap. `new_oid: None` deletes the branch (never "main");
/// `prev_oid: None` requires the branch to be unborn. raw 20-byte sha1 oids.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RefUpdate {
    /// the branch SHORT name ("main", "feature/x") — never a full refname.
    pub ref_name: String,
    pub prev_oid: Option<Vec<u8>>,
    pub new_oid: Option<Vec<u8>>,
}

/// the hidden discussion channel id every item owns. the `:` separator is the
/// chat module's reserved-namespace marker (external users cannot create such
/// ids; a module may only create ids under its own `"{module}:"` prefix), so
/// forge can rely on the id being unsquattable.
pub fn channel_id_for(repo: &str, number: u64) -> String {
    format!("forge:{repo}:{number}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// one detail record carrying every nested wire type on this surface.
    fn detail() -> ItemDetail {
        ItemDetail {
            summary: ItemSummary {
                number: 7,
                kind: ItemKind::Pr,
                title: "t".into(),
                state: ItemState::Open,
                author: Party::Key(vec![1; 4]),
                created_at: 10,
                updated_at: 11,
            },
            body: "b".into(),
            channel_id: channel_id_for("demo", 7),
            source_branch: Some("feat".into()),
            target_branch: Some("main".into()),
            merge_oid: None,
            reviews: vec![ReviewView {
                author: Party::Key(vec![2; 4]),
                verdict: ReviewVerdict::Approve,
                body: "lgtm".into(),
                commit_oid: "a".repeat(40),
                comments: vec![ReviewComment {
                    path: "src/lib.rs".into(),
                    line: 3,
                    side: DiffSide::New,
                    body: "nit".into(),
                }],
                created_at: 12,
            }],
        }
    }

    /// the detail's json with one unknown key spliced into the object at
    /// `pointer` (`""` is the record itself — the level the summary flattens
    /// onto).
    fn junked(pointer: &str) -> Value {
        let mut v = serde_json::to_value(detail()).expect("detail serializes");
        let obj = v
            .pointer_mut(pointer)
            .and_then(Value::as_object_mut)
            .expect("pointer names an object");
        obj.insert("junk".into(), json!(1));
        v
    }

    #[test]
    fn detail_refuses_unknown_fields_at_every_level() {
        let clean = serde_json::to_value(detail()).expect("detail serializes");
        assert_eq!(
            serde_json::from_value::<ItemDetail>(clean).expect("clean detail decodes"),
            detail(),
            "the record round-trips unchanged"
        );
        // "" is the flatten level: the summary claims only the keys it names,
        // and the leftover is what the record's deny refuses.
        for pointer in ["", "/reviews/0", "/reviews/0/comments/0"] {
            let decoded = serde_json::from_value::<ItemDetail>(junked(pointer));
            assert!(
                decoded.is_err(),
                "unknown field at {pointer:?} must be refused"
            );
        }
    }

    #[test]
    fn ref_records_refuse_unknown_fields() {
        let update = json!({
            "ref_name": "main",
            "prev_oid": null,
            "new_oid": [1],
            "junk": 1,
        });
        assert!(
            serde_json::from_value::<RefUpdate>(update).is_err(),
            "a push's per-ref record is strict"
        );
        let head = json!({ "name": "main", "head": "a".repeat(40), "junk": 1 });
        assert!(
            serde_json::from_value::<RefHead>(head).is_err(),
            "a refs listing row is strict"
        );
    }
}
