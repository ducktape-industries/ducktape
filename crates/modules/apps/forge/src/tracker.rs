//! the tracker — forge's issue / pull-request / review state.
//!
//! GitHub-shaped consensus state living INSIDE the forge module (a new module
//! would change the genesis module set and fork existing networks; extending
//! forge rides the established flag-day/upgrade path). items are keyed by a
//! per-repo shared number space (`#42` is an issue OR a PR, never both), each
//! item owns a hidden chat discussion channel (`forge:<repo>:<n>`, created via
//! an `emit_msg` follow-up in the SAME block as the opening op), and reviews
//! are the batched GitHub flow: one verdict + line-anchored diff comments.
//!
//! ## determinism + persistence
//!
//! the tracker is PURE consensus state, but forge's substrate is git-on-disk,
//! not qmdb — so the tracker persists the same way the refs do: a canonical
//! binary encoding ([`Tracker::canonical_bytes`], the [`crate::codec`] layout)
//! written to `<base>/.tracker.bin` at every mutating `commit_block`, re-adopted
//! at construction, carried verbatim inside the state-sync snapshot container,
//! and folded into `root()` (sha256 of the canonical bytes under a domain tag).
//! same bytes on every validator — the encoding never touches node-local data.
//!
//! ## staging
//!
//! the tracker follows the host-lent staging pattern by CLONE-ON-WRITE: the
//! first tracker mutation in a block clones the committed [`Tracker`] into a
//! block-scratch copy (team-scale state, cheap), further ops in the block see
//! read-your-writes, `commit_block` swaps it in, `abort_block` drops it.

use std::collections::BTreeMap;

use crate::chat_contract::{self as chat, Party};
use sdk::{Error, Msg};

use crate::codec::{self, Reader};
use crate::oid::{OID_RAW_LEN, Oid};
use crate::tracker_iface::{
    DiffSide, ItemDetail, ItemKind, ItemState, ItemSummary, MAX_BODY_BYTES,
    MAX_OPEN_ITEMS_PER_ACTOR, MAX_OPEN_ITEMS_PER_REPO, MAX_PATH_BYTES, MAX_REVIEW_COMMENT_BYTES,
    MAX_REVIEW_COMMENTS, MAX_REVIEWS_PER_ITEM, MAX_TITLE_BYTES, ReviewComment, ReviewVerdict,
    ReviewView, channel_id_for,
};

/// the canonical-bytes header: magic + layout version. the disk file, the
/// snapshot section, and the root preimage all carry the same self-identifying
/// bytes.
const TRACKER_MAGIC: &[u8; 4] = b"TRK\x01";

/// one item — an issue, or a PR with its branches / merge / reviews.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub number: u64,
    pub kind: ItemKind,
    pub title: String,
    pub body: String,
    pub author: Party,
    pub state: ItemState,
    pub created_at: u64,
    pub updated_at: u64,
    /// monotonic per-item counter minting deterministic system-message ids
    /// (`forge:<repo>:<n>:sys:<seq>`) for the state-change lines forge posts
    /// into the item's discussion channel.
    pub sys_seq: u64,
    /// PR-only: source branch short name.
    pub source_branch: Option<String>,
    /// PR-only: target branch short name.
    pub target_branch: Option<String>,
    /// PR-only: the merge commit once merged.
    pub merge_oid: Option<Oid>,
    /// PR-only: submitted reviews, in submission order. `commit_oid` inside is
    /// stored NORMALIZED (parsed then re-hexed) so the bytes are canonical.
    pub reviews: Vec<ReviewView>,
}

impl Item {
    fn summary(&self) -> ItemSummary {
        ItemSummary {
            number: self.number,
            kind: self.kind,
            title: self.title.clone(),
            state: self.state,
            author: self.author.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }

    fn detail(&self, repo: &str) -> ItemDetail {
        ItemDetail {
            summary: self.summary(),
            body: self.body.clone(),
            channel_id: channel_id_for(repo, self.number),
            source_branch: self.source_branch.clone(),
            target_branch: self.target_branch.clone(),
            merge_oid: self.merge_oid.map(|o| o.to_string()),
            reviews: self.reviews.clone(),
        }
    }
}

/// one repo's tracker: the shared number space and its items.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct RepoTracker {
    /// the LAST assigned number; 0 = none yet. the next item gets `+1`.
    pub last_number: u64,
    /// how many items in [`Self::items`] are currently `Open` — what
    /// [`crate::tracker_iface::MAX_OPEN_ITEMS_PER_REPO`] is checked against.
    /// maintained alongside every state transition rather than recomputed by
    /// scanning `items`, which only ever grows (there is no delete op).
    pub open_count: u64,
    /// each author's share of [`Self::open_count`] — what
    /// [`crate::tracker_iface::MAX_OPEN_ITEMS_PER_ACTOR`] is checked against.
    /// an entry is removed once its count reaches zero, so this map never
    /// outgrows `open_count`.
    pub open_by_author: BTreeMap<Party, u64>,
    pub items: BTreeMap<u64, Item>,
}

/// the whole tracker state, keyed by normalized repo slug (sorted — the
/// canonical encoding iterates in key order).
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct Tracker {
    /// Source revisions survive ref deletion, image re-entry, and recovery.
    pub source_revision: u64,
    pub repos: BTreeMap<String, RepoTracker>,
}

fn check_len(field: &str, s: &str, max: usize) -> Result<(), Error> {
    if s.len() > max {
        return Err(Error::module(
            "too_long",
            format!("forge: {field} too long ({} bytes, max {max})", s.len()),
        ));
    }
    Ok(())
}

fn check_title(title: &str) -> Result<(), Error> {
    if title.trim().is_empty() {
        return Err(Error::module(
            "empty_title",
            "forge: title must not be empty",
        ));
    }
    check_len("title", title, MAX_TITLE_BYTES)
}

/// parse a 40-char sha1 hex string from a tracker op into an [`Oid`],
/// deterministically.
pub fn parse_hex_oid(s: &str, field: &str) -> Result<Oid, Error> {
    if s.len() != 40 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::module(
            "bad_oid",
            format!("forge: {field} must be 40 hex chars, got {:?}", s.len()),
        ));
    }
    Oid::from_hex(&s.to_ascii_lowercase())
}

/// refuse an open/reopen once the repo or the author is at its OPEN-item
/// ceiling. named so the two caps this crate enforces (a repo cap, and an
/// author's share of it) read as one decision at every call site.
fn require_open_slot(rt: &RepoTracker, author: &Party, repo: &str) -> Result<(), Error> {
    if rt.open_count as usize >= MAX_OPEN_ITEMS_PER_REPO {
        return Err(Error::module(
            "repo_open_item_cap",
            format!(
                "forge: repo {repo:?} is at its open-item cap ({MAX_OPEN_ITEMS_PER_REPO}); close \
                 or merge one first"
            ),
        ));
    }
    let actor_open = rt.open_by_author.get(author).copied().unwrap_or(0);
    if actor_open as usize >= MAX_OPEN_ITEMS_PER_ACTOR {
        return Err(Error::module(
            "author_open_item_cap",
            format!(
                "forge: you already have {MAX_OPEN_ITEMS_PER_ACTOR} open items in repo {repo:?}; \
                 close or merge one first"
            ),
        ));
    }
    Ok(())
}

/// record that `author` just opened (or reopened) an item — the
/// [`RepoTracker::open_by_author`] half of [`require_open_slot`]'s ceiling.
fn note_author_opened(rt: &mut RepoTracker, author: &Party) {
    *rt.open_by_author.entry(author.clone()).or_insert(0) += 1;
}

/// record that `author`'s item just closed or merged, pruning the entry at
/// zero so the map never outgrows the live open count.
fn note_author_closed(rt: &mut RepoTracker, author: &Party) {
    use std::collections::btree_map::Entry;
    if let Entry::Occupied(mut e) = rt.open_by_author.entry(author.clone()) {
        *e.get_mut() -= 1;
        if *e.get() == 0 {
            e.remove();
        }
    }
}

impl Tracker {
    /// empty is what `compose_state_root` skips: no item was ever numbered
    /// and no source revision published.
    pub fn is_empty(&self) -> bool {
        self.source_revision == 0
            && self
                .repos
                .values()
                .all(|r| r.items.is_empty() && r.last_number == 0)
    }

    fn item(&self, repo: &str, number: u64) -> Result<&Item, Error> {
        self.repos
            .get(repo)
            .and_then(|r| r.items.get(&number))
            .ok_or_else(|| {
                Error::module(
                    "no_such_item",
                    format!("forge: no item #{number} in repo {repo}"),
                )
            })
    }

    fn item_mut(&mut self, repo: &str, number: u64) -> Result<&mut Item, Error> {
        self.repos
            .get_mut(repo)
            .and_then(|r| r.items.get_mut(&number))
            .ok_or_else(|| {
                Error::module(
                    "no_such_item",
                    format!("forge: no item #{number} in repo {repo}"),
                )
            })
    }

    /// The immutable author lookup used by ForgeState's item-management
    /// authorization gate. Keeping the lookup here preserves the tracker's
    /// existing missing-item refusal and keeps the state core free of tracker
    /// map details.
    pub(crate) fn item_author(&self, repo: &str, number: u64) -> Result<&Party, Error> {
        Ok(&self.item(repo, number)?.author)
    }

    /// open an issue or PR: assign the next number in the repo's shared space
    /// and insert the record. PR branch existence is the CALLER's check (it
    /// owns the refs); this stays a pure map mutation.
    #[allow(clippy::too_many_arguments)]
    pub fn open_item(
        &mut self,
        repo: &str,
        kind: ItemKind,
        title: String,
        body: String,
        author: Party,
        now: u64,
        branches: Option<(String, String)>,
    ) -> Result<u64, Error> {
        check_title(&title)?;
        check_len("body", &body, MAX_BODY_BYTES)?;
        debug_assert_eq!(matches!(kind, ItemKind::Pr), branches.is_some());
        let rt = self.repos.entry(repo.to_string()).or_default();
        require_open_slot(rt, &author, repo)?;
        let number = rt.last_number + 1;
        rt.last_number = number;
        let (source_branch, target_branch) = match branches {
            Some((s, t)) => (Some(s), Some(t)),
            None => (None, None),
        };
        rt.open_count += 1;
        note_author_opened(rt, &author);
        rt.items.insert(
            number,
            Item {
                number,
                kind,
                title,
                body,
                author,
                state: ItemState::Open,
                created_at: now,
                updated_at: now,
                sys_seq: 0,
                source_branch,
                target_branch,
                merge_oid: None,
                reviews: Vec::new(),
            },
        );
        Ok(number)
    }

    /// edit title and/or body — AUTHOR-ONLY, mirroring chat's edit posture.
    pub fn edit_item(
        &mut self,
        repo: &str,
        number: u64,
        title: Option<String>,
        body: Option<String>,
        now: u64,
    ) -> Result<(), Error> {
        let item = self.item_mut(repo, number)?;
        if let Some(t) = title {
            check_title(&t)?;
            item.title = t;
        }
        if let Some(b) = body {
            check_len("body", &b, MAX_BODY_BYTES)?;
            item.body = b;
        }
        item.updated_at = now;
        Ok(())
    }

    /// close / reopen. merged PRs are terminal. an unchanged state is a
    /// deterministic no-op (`Ok(None)`); a real transition returns the system
    /// line to post into the discussion channel.
    pub fn set_state(
        &mut self,
        repo: &str,
        number: u64,
        open: bool,
        now: u64,
    ) -> Result<Option<&'static str>, Error> {
        let rt = self.repos.get_mut(repo).ok_or_else(|| {
            Error::module(
                "no_such_item",
                format!("forge: no item #{number} in repo {repo}"),
            )
        })?;
        let (current_state, author) = {
            let item = rt.items.get(&number).ok_or_else(|| {
                Error::module(
                    "no_such_item",
                    format!("forge: no item #{number} in repo {repo}"),
                )
            })?;
            (item.state, item.author.clone())
        };
        if current_state == ItemState::Merged {
            return Err(Error::module(
                "pr_already_merged",
                "forge: a merged pull request cannot change state",
            ));
        }
        let target = if open {
            ItemState::Open
        } else {
            ItemState::Closed
        };
        if current_state == target {
            return Ok(None);
        }
        // reopening claims a slot back exactly like a fresh `OpenIssue`/
        // `OpenPr` — the same two caps apply, so a closed item cannot reopen
        // past a repo or an author's ceiling.
        if open {
            require_open_slot(rt, &author, repo)?;
        }
        let item = rt.items.get_mut(&number).expect("checked above");
        item.state = target;
        item.updated_at = now;
        if open {
            rt.open_count += 1;
            note_author_opened(rt, &author);
        } else {
            rt.open_count -= 1;
            note_author_closed(rt, &author);
        }
        Ok(Some(if open { "reopened" } else { "closed" }))
    }

    /// mark an open PR merged, recording the merge commit. the ref CAS (target
    /// and source head checks) is the caller's job — it owns the refs.
    pub fn merge_pr(&mut self, repo: &str, number: u64, merge: Oid, now: u64) -> Result<(), Error> {
        let rt = self.repos.get_mut(repo).ok_or_else(|| {
            Error::module(
                "no_such_item",
                format!("forge: no item #{number} in repo {repo}"),
            )
        })?;
        let item = rt.items.get_mut(&number).ok_or_else(|| {
            Error::module(
                "no_such_item",
                format!("forge: no item #{number} in repo {repo}"),
            )
        })?;
        if item.kind != ItemKind::Pr {
            return Err(Error::module(
                "not_a_pull_request",
                format!("forge: item #{number} is an issue, not a pull request"),
            ));
        }
        if item.state != ItemState::Open {
            return Err(Error::module(
                "pr_not_open",
                format!("forge: pull request #{number} is not open"),
            ));
        }
        item.state = ItemState::Merged;
        item.merge_oid = Some(merge);
        item.updated_at = now;
        let author = item.author.clone();
        rt.open_count -= 1;
        note_author_closed(rt, &author);
        Ok(())
    }

    /// append a batched review to a PR. reviews are immutable once submitted;
    /// reviewing a closed/merged PR is allowed (GitHub permits it and it is
    /// harmless). `commit_oid` is normalized before storage.
    // the arity mirrors the SubmitReview wire op's fields one-to-one.
    #[allow(clippy::too_many_arguments)]
    pub fn submit_review(
        &mut self,
        repo: &str,
        number: u64,
        author: Party,
        verdict: ReviewVerdict,
        body: String,
        commit_oid: &str,
        comments: Vec<ReviewComment>,
        now: u64,
    ) -> Result<(), Error> {
        check_len("review body", &body, MAX_BODY_BYTES)?;
        if comments.len() > MAX_REVIEW_COMMENTS {
            return Err(Error::module(
                "review_comment_cap",
                format!(
                    "forge: too many review comments ({}, max {MAX_REVIEW_COMMENTS})",
                    comments.len()
                ),
            ));
        }
        for c in &comments {
            check_len("comment path", &c.path, MAX_PATH_BYTES)?;
            check_len("comment body", &c.body, MAX_REVIEW_COMMENT_BYTES)?;
            if c.body.trim().is_empty() {
                return Err(Error::module(
                    "empty_review_comment",
                    "forge: a review comment body must not be empty",
                ));
            }
        }
        if body.trim().is_empty() && comments.is_empty() {
            return Err(Error::module(
                "empty_review",
                "forge: a review needs a body or at least one comment",
            ));
        }
        let commit = parse_hex_oid(commit_oid, "commit_oid")?;
        let item = self.item_mut(repo, number)?;
        if item.kind != ItemKind::Pr {
            return Err(Error::module(
                "not_a_pull_request",
                format!("forge: item #{number} is an issue, not a pull request"),
            ));
        }
        if item.reviews.len() >= MAX_REVIEWS_PER_ITEM {
            return Err(Error::module(
                "review_cap",
                format!("forge: review cap reached ({MAX_REVIEWS_PER_ITEM})"),
            ));
        }
        item.reviews.push(ReviewView {
            author,
            verdict,
            body,
            commit_oid: commit.to_string(),
            comments,
            created_at: now,
        });
        item.updated_at = now;
        Ok(())
    }

    /// the (source, target) branches of an OPEN PR — the merge path's
    /// tracker-side gate.
    pub fn pr_branches(&self, repo: &str, number: u64) -> Result<(String, String), Error> {
        let item = self.item(repo, number)?;
        if item.kind != ItemKind::Pr {
            return Err(Error::module(
                "not_a_pull_request",
                format!("forge: item #{number} is an issue, not a pull request"),
            ));
        }
        if item.state != ItemState::Open {
            return Err(Error::module(
                "pr_not_open",
                format!("forge: pull request #{number} is not open"),
            ));
        }
        Ok((
            item.source_branch.clone().unwrap_or_default(),
            item.target_branch.clone().unwrap_or_default(),
        ))
    }

    /// mint the next system-message id for an item's discussion channel.
    pub fn next_sys_message_id(&mut self, repo: &str, number: u64) -> Result<String, Error> {
        let item = self.item_mut(repo, number)?;
        item.sys_seq += 1;
        Ok(format!("forge:{repo}:{number}:sys:{}", item.sys_seq))
    }

    // ---- read projections ---------------------------------------------------

    pub fn list(&self, repo: &str) -> Vec<ItemSummary> {
        self.repos
            .get(repo)
            .map(|r| r.items.values().map(Item::summary).collect())
            .unwrap_or_default()
    }

    pub fn get(&self, repo: &str, number: u64) -> Option<ItemDetail> {
        self.repos
            .get(repo)
            .and_then(|r| r.items.get(&number))
            .map(|i| i.detail(repo))
    }

    // ---- canonical codec -----------------------------------------------------
    // the SINGLE deterministic byte layout behind the root fold, the snapshot
    // section, and the disk file. every string is cap-bounded at stage time, so
    // the u32 length prefixes never truncate.

    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = TRACKER_MAGIC.to_vec();
        codec::put_u64(&mut out, self.source_revision);
        codec::put_u32(&mut out, self.repos.len() as u32);
        for (repo, rt) in &self.repos {
            codec::put_str(&mut out, repo);
            codec::put_u64(&mut out, rt.last_number);
            codec::put_u32(&mut out, rt.items.len() as u32);
            for item in rt.items.values() {
                encode_item(&mut out, item);
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let body = bytes
            .strip_prefix(TRACKER_MAGIC.as_slice())
            .ok_or_else(|| {
                Error::module(
                    "tracker_decode",
                    "forge tracker: bad magic (not a TRK1 container)",
                )
            })?;
        let mut r = Reader::new(body);
        let source_revision = r.u64()?;
        let repo_count = r.u32()?;
        let mut repos = BTreeMap::new();
        for _ in 0..repo_count {
            let name = r.str_()?;
            let last_number = r.u64()?;
            let item_count = r.u32()?;
            let mut items = BTreeMap::new();
            for _ in 0..item_count {
                let item = decode_item(&mut r)?;
                if item.number > last_number {
                    return Err(Error::module(
                        "tracker_decode",
                        "forge tracker: item numbered past the counter",
                    ));
                }
                if items.insert(item.number, item).is_some() {
                    return Err(Error::module(
                        "tracker_decode",
                        "forge tracker: duplicate item number",
                    ));
                }
            }
            // `open_count`/`open_by_author` are not wire fields: they are a
            // pure function of `items`' states, derived here rather than
            // trusted from bytes so the two can never drift apart.
            let mut open_count = 0u64;
            let mut open_by_author: BTreeMap<Party, u64> = BTreeMap::new();
            for item in items.values() {
                if item.state == ItemState::Open {
                    open_count += 1;
                    *open_by_author.entry(item.author.clone()).or_insert(0) += 1;
                }
            }
            if repos
                .insert(
                    name,
                    RepoTracker {
                        last_number,
                        open_count,
                        open_by_author,
                        items,
                    },
                )
                .is_some()
            {
                return Err(Error::module(
                    "tracker_decode",
                    "forge tracker: duplicate repo",
                ));
            }
        }
        if !r.done() {
            return Err(Error::module(
                "tracker_decode",
                "forge tracker: trailing bytes after the container",
            ));
        }
        Ok(Self {
            source_revision,
            repos,
        })
    }
}

fn encode_author(out: &mut Vec<u8>, a: &Party) {
    match a {
        Party::Key(id) => {
            codec::put_u8(out, 0);
            codec::put_bytes(out, id);
        }
        Party::Account(account) => {
            codec::put_u8(out, 1);
            codec::put_u64(out, *account);
        }
        Party::Module(m) => {
            codec::put_u8(out, 2);
            codec::put_str(out, m);
        }
        Party::System => codec::put_u8(out, 3),
    }
}

fn decode_author(r: &mut Reader) -> Result<Party, Error> {
    Ok(match r.u8()? {
        0 => {
            let len = r.u32()? as usize;
            Party::Key(r.take(len)?.to_vec())
        }
        1 => Party::Account(r.u64()?),
        2 => Party::Module(r.str_()?),
        3 => Party::System,
        t => {
            return Err(Error::module(
                "tracker_decode",
                format!("forge tracker: bad author tag {t}"),
            ));
        }
    })
}

fn encode_item(out: &mut Vec<u8>, item: &Item) {
    codec::put_u64(out, item.number);
    codec::put_u8(
        out,
        match item.kind {
            ItemKind::Issue => 0,
            ItemKind::Pr => 1,
        },
    );
    codec::put_str(out, &item.title);
    codec::put_str(out, &item.body);
    encode_author(out, &item.author);
    codec::put_u8(
        out,
        match item.state {
            ItemState::Open => 0,
            ItemState::Closed => 1,
            ItemState::Merged => 2,
        },
    );
    codec::put_u64(out, item.created_at);
    codec::put_u64(out, item.updated_at);
    codec::put_u64(out, item.sys_seq);
    if item.kind == ItemKind::Pr {
        codec::put_str(out, item.source_branch.as_deref().unwrap_or_default());
        codec::put_str(out, item.target_branch.as_deref().unwrap_or_default());
        match item.merge_oid {
            Some(oid) => {
                codec::put_u8(out, 1);
                out.extend_from_slice(oid.as_bytes());
            }
            None => codec::put_u8(out, 0),
        }
        codec::put_u32(out, item.reviews.len() as u32);
        for review in &item.reviews {
            encode_author(out, &review.author);
            codec::put_u8(
                out,
                match review.verdict {
                    ReviewVerdict::Approve => 0,
                    ReviewVerdict::RequestChanges => 1,
                    ReviewVerdict::Comment => 2,
                },
            );
            codec::put_str(out, &review.body);
            // commit_oid is stored normalized 40-hex; re-encode raw.
            let oid = Oid::from_hex(&review.commit_oid).expect("stored normalized");
            out.extend_from_slice(oid.as_bytes());
            codec::put_u64(out, review.created_at);
            codec::put_u32(out, review.comments.len() as u32);
            for c in &review.comments {
                codec::put_str(out, &c.path);
                codec::put_u32(out, c.line);
                codec::put_u8(
                    out,
                    match c.side {
                        DiffSide::Old => 0,
                        DiffSide::New => 1,
                    },
                );
                codec::put_str(out, &c.body);
            }
        }
    }
}

fn decode_item(r: &mut Reader) -> Result<Item, Error> {
    let number = r.u64()?;
    let kind = match r.u8()? {
        0 => ItemKind::Issue,
        1 => ItemKind::Pr,
        t => {
            return Err(Error::module(
                "tracker_decode",
                format!("forge tracker: bad kind tag {t}"),
            ));
        }
    };
    let title = r.str_()?;
    let body = r.str_()?;
    let author = decode_author(r)?;
    let state = match r.u8()? {
        0 => ItemState::Open,
        1 => ItemState::Closed,
        2 => ItemState::Merged,
        t => {
            return Err(Error::module(
                "tracker_decode",
                format!("forge tracker: bad state tag {t}"),
            ));
        }
    };
    let created_at = r.u64()?;
    let updated_at = r.u64()?;
    let sys_seq = r.u64()?;
    let mut item = Item {
        number,
        kind,
        title,
        body,
        author,
        state,
        created_at,
        updated_at,
        sys_seq,
        source_branch: None,
        target_branch: None,
        merge_oid: None,
        reviews: Vec::new(),
    };
    if kind == ItemKind::Pr {
        item.source_branch = Some(r.str_()?);
        item.target_branch = Some(r.str_()?);
        item.merge_oid = match r.u8()? {
            0 => None,
            1 => Some(Oid::from_bytes(r.take(OID_RAW_LEN)?)?),
            t => {
                return Err(Error::module(
                    "tracker_decode",
                    format!("forge tracker: bad merge tag {t}"),
                ));
            }
        };
        let n_reviews = r.u32()?;
        for _ in 0..n_reviews {
            let author = decode_author(r)?;
            let verdict = match r.u8()? {
                0 => ReviewVerdict::Approve,
                1 => ReviewVerdict::RequestChanges,
                2 => ReviewVerdict::Comment,
                t => {
                    return Err(Error::module(
                        "tracker_decode",
                        format!("forge tracker: bad verdict tag {t}"),
                    ));
                }
            };
            let body = r.str_()?;
            let commit = Oid::from_bytes(r.take(OID_RAW_LEN)?)?;
            let created_at = r.u64()?;
            let n_comments = r.u32()?;
            let mut comments = Vec::with_capacity(n_comments.min(1024) as usize);
            for _ in 0..n_comments {
                let path = r.str_()?;
                let line = r.u32()?;
                let side = match r.u8()? {
                    0 => DiffSide::Old,
                    1 => DiffSide::New,
                    t => {
                        return Err(Error::module(
                            "tracker_decode",
                            format!("forge tracker: bad side tag {t}"),
                        ));
                    }
                };
                let body = r.str_()?;
                comments.push(ReviewComment {
                    path,
                    line,
                    side,
                    body,
                });
            }
            item.reviews.push(ReviewView {
                author,
                verdict,
                body,
                commit_oid: commit.to_string(),
                comments,
                created_at,
            });
        }
    }
    Ok(item)
}

// ---- chat follow-up payloads ------------------------------------------------
// the discussion-channel writes forge emits inside consensus. follow-ups carry
// `Origin::Module("forge")`, so chat's namespace guard admits the `forge:`
// prefix and the author renders as the forge module.

/// the CreateChannel follow-up an `OpenIssue`/`OpenPr` emits — atomic with the
/// item record (the whole block commits or rolls back together).
pub fn create_channel_msg(chat_target: &str, repo: &str, number: u64) -> Msg {
    Msg {
        target: chat_target.to_string(),
        payload: chat::encode_msg(&chat::ChatMsg::CreateChannel {
            channel_id: channel_id_for(repo, number),
            name: format!("{repo}#{number}"),
            post_policy: chat::PostPolicy::Open,
        }),
    }
}

/// a one-line system message into an item's discussion channel ("closed",
/// "reopened", "merged", "approved these changes", ...).
pub fn system_line_msg(
    chat_target: &str,
    repo: &str,
    number: u64,
    message_id: String,
    text: &str,
) -> Msg {
    Msg {
        target: chat_target.to_string(),
        payload: chat::encode_msg(&chat::ChatMsg::PostMessage {
            channel_id: channel_id_for(repo, number),
            message_id,
            blocks: vec![chat::Block::paragraph(text)],
            thread: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(b: u8) -> Party {
        Party::Key(vec![b; 4])
    }

    #[test]
    fn numbering_is_shared_and_sequential() {
        let mut t = Tracker::default();
        let a = t
            .open_item(
                "demo",
                ItemKind::Issue,
                "one".into(),
                String::new(),
                user(1),
                10,
                None,
            )
            .unwrap();
        let b = t
            .open_item(
                "demo",
                ItemKind::Pr,
                "two".into(),
                String::new(),
                user(2),
                11,
                Some(("feat".into(), "main".into())),
            )
            .unwrap();
        let c = t
            .open_item(
                "other",
                ItemKind::Issue,
                "own space".into(),
                String::new(),
                user(1),
                12,
                None,
            )
            .unwrap();
        assert_eq!(
            (a, b, c),
            (1, 2, 1),
            "shared per-repo space, per-repo counters"
        );
    }

    #[test]
    fn state_machine_close_reopen_merge() {
        let mut t = Tracker::default();
        t.open_item(
            "demo",
            ItemKind::Pr,
            "pr".into(),
            String::new(),
            user(1),
            1,
            Some(("feat".into(), "main".into())),
        )
        .unwrap();
        assert_eq!(t.set_state("demo", 1, false, 2).unwrap(), Some("closed"));
        assert_eq!(
            t.set_state("demo", 1, false, 3).unwrap(),
            None,
            "no-op repeat"
        );
        assert_eq!(t.set_state("demo", 1, true, 4).unwrap(), Some("reopened"));
        let merge = Oid::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        t.merge_pr("demo", 1, merge, 5).unwrap();
        assert!(
            t.set_state("demo", 1, true, 6).is_err(),
            "merged is terminal"
        );
        assert!(
            t.merge_pr("demo", 1, merge, 7).is_err(),
            "double merge rejected"
        );
        let d = t.get("demo", 1).unwrap();
        assert_eq!(d.summary.state, ItemState::Merged);
        assert_eq!(
            d.merge_oid.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn edit_item_rewrites_title_and_body() {
        let mut t = Tracker::default();
        t.open_item(
            "demo",
            ItemKind::Issue,
            "t".into(),
            "b".into(),
            user(1),
            1,
            None,
        )
        .unwrap();
        t.edit_item(
            "demo",
            1,
            Some("new title".into()),
            Some("new body".into()),
            3,
        )
        .unwrap();
        let d = t.get("demo", 1).unwrap();
        assert_eq!(d.summary.title, "new title");
        assert_eq!(d.body, "new body");
    }

    #[test]
    fn canonical_bytes_round_trip() {
        let mut t = Tracker::default();
        t.open_item(
            "demo",
            ItemKind::Issue,
            "issue".into(),
            "body".into(),
            user(1),
            1,
            None,
        )
        .unwrap();
        t.open_item(
            "demo",
            ItemKind::Pr,
            "pr".into(),
            "prbody".into(),
            Party::Module("runs".into()),
            2,
            Some(("feature/x".into(), "main".into())),
        )
        .unwrap();
        t.submit_review(
            "demo",
            2,
            user(3),
            ReviewVerdict::RequestChanges,
            "needs work".into(),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            vec![ReviewComment {
                path: "src/lib.rs".into(),
                line: 42,
                side: DiffSide::New,
                body: "off by one".into(),
            }],
            3,
        )
        .unwrap();
        t.set_state("demo", 1, false, 4).unwrap();
        t.next_sys_message_id("demo", 1).unwrap();

        let bytes = t.canonical_bytes();
        let back = Tracker::decode(&bytes).unwrap();
        assert_eq!(back, t, "decode(encode(t)) == t");
        assert_eq!(back.canonical_bytes(), bytes, "re-encode is byte-identical");

        // tampered bytes die deterministically, never panic.
        let mut bad = bytes.clone();
        let last = bad.len() - 1;
        bad.truncate(last);
        assert!(Tracker::decode(&bad).is_err());
    }

    #[test]
    fn empty_tracker_is_empty_and_round_trips() {
        let t = Tracker::default();
        assert!(t.is_empty());
        assert_eq!(Tracker::decode(&t.canonical_bytes()).unwrap(), t);
        // a repo whose items were all... never mind — items can't be deleted;
        // a counter-only repo still makes the tracker NON-empty (the number
        // space is consensus state: reusing #1 after a rebuild would relink
        // old channels).
        let mut t2 = Tracker::default();
        t2.open_item(
            "demo",
            ItemKind::Issue,
            "x".into(),
            String::new(),
            user(1),
            1,
            None,
        )
        .unwrap();
        assert!(!t2.is_empty());
    }

    // the per-repo cap: with no delete op, OPEN items would otherwise grow
    // unbounded. spread across many authors so the per-actor cap below never
    // trips first.
    #[test]
    fn repo_open_item_cap_refuses_until_one_closes() {
        let mut t = Tracker::default();
        for i in 0..MAX_OPEN_ITEMS_PER_REPO as u64 {
            t.open_item(
                "demo",
                ItemKind::Issue,
                format!("item {i}"),
                String::new(),
                user((i % 256) as u8),
                i,
                None,
            )
            .unwrap();
        }
        let err = t
            .open_item(
                "demo",
                ItemKind::Issue,
                "one too many".into(),
                String::new(),
                user(0),
                1_000_000,
                None,
            )
            .expect_err("repo is at its open-item cap");
        assert!(err.to_string().contains("open-item cap"), "{err}");

        // a different repo is unaffected — the cap is per-repo.
        t.open_item(
            "other",
            ItemKind::Issue,
            "fine".into(),
            String::new(),
            user(0),
            1_000_000,
            None,
        )
        .unwrap();

        // closing one item in "demo" frees exactly one slot.
        t.set_state("demo", 1, false, 1_000_001).unwrap();
        t.open_item(
            "demo",
            ItemKind::Issue,
            "fits now".into(),
            String::new(),
            user(0),
            1_000_002,
            None,
        )
        .unwrap();
        let err = t
            .open_item(
                "demo",
                ItemKind::Issue,
                "one too many again".into(),
                String::new(),
                user(0),
                1_000_003,
                None,
            )
            .expect_err("full again");
        assert!(err.to_string().contains("open-item cap"), "{err}");
    }

    // the per-actor cap: one account cannot crowd out the whole repo even
    // though the repo cap has room left.
    #[test]
    fn actor_open_item_cap_refuses_until_one_closes() {
        let mut t = Tracker::default();
        for i in 0..MAX_OPEN_ITEMS_PER_ACTOR as u64 {
            t.open_item(
                "demo",
                ItemKind::Issue,
                format!("item {i}"),
                String::new(),
                user(1),
                i,
                None,
            )
            .unwrap();
        }
        let err = t
            .open_item(
                "demo",
                ItemKind::Issue,
                "one too many".into(),
                String::new(),
                user(1),
                1_000_000,
                None,
            )
            .expect_err("this actor is at their share of the cap");
        assert!(err.to_string().contains("you already have"), "{err}");

        // a DIFFERENT actor still has room — the cap is per-actor, not global.
        t.open_item(
            "demo",
            ItemKind::Issue,
            "someone else".into(),
            String::new(),
            user(2),
            1_000_001,
            None,
        )
        .unwrap();

        // closing user(1)'s item #1 frees exactly their own slot back.
        t.set_state("demo", 1, false, 1_000_002).unwrap();
        t.open_item(
            "demo",
            ItemKind::Issue,
            "fits now".into(),
            String::new(),
            user(1),
            1_000_003,
            None,
        )
        .unwrap();
    }
}
