//! forge's CONSENSUS core — the branch map of every repo plus the tracker, and
//! the block-scoped staging every [`ForgeMsg`] decides against. this is the ONE
//! implementation of forge's accept/reject logic: the native module drives it
//! over its disk substrate and the wasm guest drives it over the host state
//! lane, so both runtimes stage the identical fate for the identical op.
//!
//! nothing here touches a git object database, a file, or a clock: a decision
//! is a pure function of (committed state, block scratch, env, op). the module
//! also owns the byte contracts the two runtimes meet at:
//!
//! * the **state image** — born repos' branch and tag maps + the tracker, the whole of
//!   committed consensus state as one value. the guest reads it under the host
//!   state lane and writes the chained image back after every dispatch; the
//!   host substrate adopts the block's final image at commit.
//! * the **block scratch** — the fates staged so far this block, each with the
//!   committed head it shadows. a per-dispatch runtime rebuilds the native
//!   mid-block [`RepoState`] (committed refs + staged fates) from image +
//!   scratch, so the one-fate-per-branch rule and every committed-only check
//!   read exactly as they do on the block-spanning native struct.
//! * the **ref target** — one packed head's `(repo, ref, head, pack)`,
//!   staged as a block-scoped object so the host substrate learns which pack
//!   materializes a head the image only names. deletes carry no target.

use std::collections::{BTreeMap, BTreeSet};

use attribution::{Actor, AttributionMsg, AttributionUpdate, ObjectRef, Reason, Relation};
use chat::Party;
use identity::{IdentityQuery, IdentityReply};
use sdk::{Ctx, Error, Origin, StateRoot};
use sha2::{Digest, Sha256};

use crate::codec::{self, Reader};
use crate::oid::{OID_RAW_LEN, Oid};
use crate::refs::{INTEGRATION_BRANCH, RefName, RepoRefs, RepoState, StagedRef, norm_branch};
use crate::tracker::{self, Tracker, parse_hex_oid};
use crate::{
    ForgeMsg, ItemKind, MAX_BRANCHES_PER_REPO, MAX_REFS_PER_PUSH, MAX_TAGS_PER_REPO, PushCert,
    RefUpdate, ReviewVerdict, decode_msg, norm_repo,
};

/// one repo's committed branch or tag map, `short_name -> oid`.
type RefMap = BTreeMap<String, Oid>;

/// the Identity module's genesis-constant id — the account registry every
/// forge principal resolves through. mirrors `bin/node/src/host_state.rs`'s
/// `IDENTITY_MODULE_ID`; it is not a per-network choice, so it is not a knob.
const IDENTITY_MODULE: &str = "identity";

/// Call completions retain these exact bytes. A typed result fixes field order
/// even when native and guest builds select different serde_json map features.
#[derive(serde::Serialize)]
struct CreatedItem<'a> {
    number: u64,
    repo: &'a str,
}

/// the domain tag folding the tracker's canonical-bytes hash into the root
/// preimage — separates it from the branch material.
const TRACKER_ROOT_DOMAIN: &[u8] = b"ducktape.forge.tracker.v1\x00";

/// the domain tag forge's root preimage is separated under — a fixed constant
/// hashed over the folded preimage in [`compose_state_root`].
const FORGE_ROOT_DOMAIN: &[u8] = b"ducktape.forge.multirepo.v1\x00";

/// the 4-byte magic the state image leads with.
const IMAGE_MAGIC: &[u8; 4] = b"FGI1";

/// the 4-byte magic the block scratch leads with.
const BLOCK_SCRATCH_MAGIC: &[u8; 4] = b"FGB1";

/// the object-plane kind tag of a [`RefTarget`] — the only object kind forge
/// stages. a host substrate handed any other tag is wired to the wrong guest.
pub const REF_TARGET_KIND: u8 = 1;

/// the composition [`StateRoot`] over the whole forge state: every born repo's
/// branches, then its tags (callers pass repos SORTED by name; both maps are
/// sorted `BTreeMap`s) folded with the tracker's canonical-bytes hash, then
/// domain-separated under [`FORGE_ROOT_DOMAIN`]. the empty state ->
/// [`StateRoot::ZERO`] (the empty-genesis root). see the composition invariant
/// in the crate doc.
pub fn compose_state_root<'a>(
    repos: impl Iterator<Item = (&'a str, &'a RefMap, &'a RefMap)>,
    tracker: &Tracker,
) -> StateRoot {
    let mut h = Sha256::new();
    let mut any = false;
    for (name, branches, tags) in repos {
        let born = !branches.is_empty() || !tags.is_empty();
        if !born {
            continue;
        }
        any = true;
        // name/ref lengths are cap-bounded (64 / 128 bytes), so the u32
        // casts never truncate.
        h.update((name.len() as u32).to_le_bytes());
        h.update(name.as_bytes());
        for refs in [branches, tags] {
            h.update((refs.len() as u32).to_le_bytes());
            for (short, oid) in refs {
                h.update((short.len() as u32).to_le_bytes());
                h.update(short.as_bytes());
                h.update(oid.as_bytes()); // 20 raw sha1 bytes
            }
        }
    }
    if !tracker.is_empty() {
        any = true;
        h.update(TRACKER_ROOT_DOMAIN);
        h.update(Sha256::digest(tracker.canonical_bytes()));
    }
    if !any {
        return StateRoot::ZERO;
    }
    let inner: [u8; 32] = h.finalize().into();
    let mut outer = Sha256::new();
    outer.update(FORGE_ROOT_DOMAIN);
    outer.update(inner);
    StateRoot(outer.finalize().into())
}

/// parse exactly `OID_RAW_LEN` (20) raw sha1 bytes into an `Oid`, with a
/// deterministic module error naming the field on any other length.
fn parse_oid(bytes: &[u8], field: &str) -> Result<Oid, Error> {
    if bytes.len() != OID_RAW_LEN {
        return Err(Error::module(
            "bad_oid",
            format!(
                "forge: {field} must be {OID_RAW_LEN} bytes, got {}",
                bytes.len()
            ),
        ));
    }
    Oid::from_bytes(bytes)
}

/// parse a 32-byte pack digest from raw wire bytes.
fn parse_digest(bytes: &[u8]) -> Result<[u8; 32], Error> {
    bytes.try_into().map_err(|_| {
        Error::module(
            "bad_pack_digest",
            format!("forge: pack_digest must be 32 bytes, got {}", bytes.len()),
        )
    })
}

/// parse a 64-char sha256 hex digest (the app-facing MergePr lane).
fn parse_hex_digest(s: &str) -> Result<[u8; 32], Error> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::module(
            "bad_pack_digest",
            "forge: pack_digest must be 64 hex chars",
        ));
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|e| Error::module("bad_pack_digest", e.to_string()))?;
    }
    Ok(out)
}

/// one update's `(prev, new)` oids off the wire, each exactly 20 bytes.
fn update_oids(u: &RefUpdate) -> Result<(Option<Oid>, Option<Oid>), Error> {
    let prev = u
        .prev_oid
        .as_deref()
        .map(|b| parse_oid(b, "prev_oid"))
        .transpose()?;
    let new = u
        .new_oid
        .as_deref()
        .map(|b| parse_oid(b, "new_oid"))
        .transpose()?;
    Ok((prev, new))
}

/// CAS every branch update and stage every tag creation of ONE atomic push
/// onto `state`. detached from the repo map so the caller decides whether a
/// birthing repo's entry survives a refusal.
fn stage_updates(
    state: &mut RepoState,
    updates: &[RefUpdate],
    tags: &[RefUpdate],
    digest: Option<[u8; 32]>,
) -> Result<(), Error> {
    for u in updates {
        let (prev, new) = update_oids(u)?;
        state.stage_update(
            &u.ref_name,
            prev,
            new,
            new.is_some().then(|| digest.unwrap()),
        )?;
    }
    for u in tags {
        let (prev, new) = update_oids(u)?;
        state.stage_tag(&u.ref_name, prev, new, digest)?;
    }
    // the ceilings read the maps this push would PUBLISH, so a delete always
    // passes and a push that both deletes and creates is judged on its net
    // effect. the maps ARE the counts — no counter to persist.
    let published = state.published();
    if published.branches.len() > MAX_BRANCHES_PER_REPO {
        return Err(Error::module(
            "branch_cap",
            format!(
                "forge: repo is at its branch cap ({MAX_BRANCHES_PER_REPO}); \
                 delete a branch first"
            ),
        ));
    }
    if published.tags.len() > MAX_TAGS_PER_REPO {
        return Err(Error::module(
            "tag_cap",
            format!("forge: repo holds its most tags ({MAX_TAGS_PER_REPO})"),
        ));
    }
    Ok(())
}

/// the consensus state plus this block's staging: the repo namespace (keyed by
/// normalized slug, SORTED so `root()` composes order-independently), the
/// COMMITTED tracker, and the block-scratch tracker (clone-on-write on the
/// first tracker mutation of a block; swapped in at commit, dropped at abort).
#[derive(Clone, Default)]
pub struct ForgeState {
    pub repos: BTreeMap<String, RepoState>,
    pub tracker: Tracker,
    pub staged_tracker: Option<Tracker>,
}

fn attributed_actor(party: &Party) -> Actor {
    match party {
        Party::Account(account) => Actor::Account(*account),
        Party::Key(key) => Actor::Key(key.clone()),
        Party::Module(module) => Actor::Module(module.clone()),
        Party::System => Actor::System,
    }
}

fn party_relation(party: &Party, reason: Reason, detail: Vec<u8>) -> Option<Relation> {
    party.account().map(|recipient| Relation {
        recipient,
        reason,
        detail,
    })
}

fn source_object(kind: &str, address: impl serde::Serialize) -> ObjectRef {
    ObjectRef {
        kind: kind.into(),
        object: serde_json::to_string(&address).expect("source address serializes"),
    }
}

impl ForgeState {
    /// the composed state root over COMMITTED state — pure, no IO.
    pub fn root(&self) -> StateRoot {
        let entries = self
            .repos
            .iter()
            .map(|(n, s)| (n.as_str(), &s.refs, &s.tags));
        compose_state_root(entries, &self.tracker)
    }

    /// the tracker as THIS BLOCK sees it (read-your-writes).
    pub fn tracker_view(&self) -> &Tracker {
        self.staged_tracker.as_ref().unwrap_or(&self.tracker)
    }

    /// clone-on-write access to the block-scratch tracker.
    fn staged_tracker_mut(&mut self) -> &mut Tracker {
        self.staged_tracker
            .get_or_insert_with(|| self.tracker.clone())
    }

    /// swap the block-scratch tracker in as committed; `true` when a tracker
    /// mutation was staged this block (the caller persists it).
    pub fn commit_tracker(&mut self) -> bool {
        match self.staged_tracker.take() {
            Some(staged) => {
                self.tracker = staged;
                true
            }
            None => false,
        }
    }

    /// discard everything staged — no ref moved, tracker unchanged, `root()`
    /// unchanged.
    pub fn abort(&mut self) {
        for state in self.repos.values_mut() {
            state.abort();
        }
        self.staged_tracker = None;
    }

    /// apply one write op. Git writes stage pure per-branch CAS updates;
    /// tracker ops mutate the block-scratch tracker and emit chat follow-ups
    /// (at `chat_target`; `None` opens no discussion channels) that commit
    /// atomically with the block. never opens a Git repo.
    pub async fn apply(
        &mut self,
        ctx: &mut dyn Ctx,
        payload: &[u8],
        chat_target: Option<&str>,
        attribution_target: Option<&str>,
        chain_id: &str,
    ) -> Result<(), Error> {
        let msg = decode_msg(payload).map_err(|e| Error::module("codec", e))?;
        let party = match &msg {
            ForgeMsg::PushRefs {
                repo,
                updates,
                tags,
                cert,
                ..
            } => {
                let name = norm_repo(repo)?;
                Self::push_party(ctx, chain_id, &name, cert.as_ref(), updates, tags).await?
            }
            ForgeMsg::MergePr { .. } => Self::ref_party(ctx).await?,
            _ => Self::party_of_origin(ctx).await?,
        };
        let before = self.clone();
        let applied = async {
            self.apply_op(ctx, msg, chat_target, &party).await?;
            self.publish_attribution(ctx, attribution_target, &before, &party)
        }
        .await;
        if applied.is_err() {
            *self = before;
        }
        applied
    }

    async fn apply_op(
        &mut self,
        ctx: &mut dyn Ctx,
        msg: ForgeMsg,
        chat_target: Option<&str>,
        party: &Party,
    ) -> Result<(), Error> {
        let now = ctx.env().consensus_time;
        match msg {
            ForgeMsg::PushRefs {
                repo,
                updates,
                tags,
                pack_digest,
                cert: _,
            } => {
                let name = norm_repo(&repo)?;
                self.stage_push_refs(&name, updates, tags, pack_digest)
            }
            ForgeMsg::OpenIssue { repo, title, body } => {
                let name = norm_repo(&repo)?;
                let author = party.clone();
                let number = self.staged_tracker_mut().open_item(
                    &name,
                    ItemKind::Issue,
                    title,
                    body,
                    author,
                    now,
                    None,
                )?;
                if let Some(chat) = chat_target {
                    ctx.emit_msg(tracker::create_channel_msg(chat, &name, number));
                }
                ctx.set_output(sdk::wire::encode(&CreatedItem {
                    number,
                    repo: &name,
                }));
                Ok(())
            }
            ForgeMsg::OpenPr {
                repo,
                title,
                body,
                source_branch,
                target_branch,
            } => {
                let name = norm_repo(&repo)?;
                let author = party.clone();
                let target = if target_branch.is_empty() {
                    INTEGRATION_BRANCH.to_string()
                } else {
                    target_branch
                };
                norm_branch(&source_branch)?;
                norm_branch(&target)?;
                if source_branch == target {
                    return Err(Error::module(
                        "bad_pull_request",
                        "forge: a pull request needs distinct source and target branches",
                    ));
                }
                // both branches must be BORN in committed state — a PR from a
                // branch nobody pushed is meaningless, and the checks read
                // agreed state only.
                let state = self.repos.get(&name).ok_or_else(|| {
                    Error::module("unknown_repo", format!("forge: no repo {name:?}"))
                })?;
                for (label, branch) in [("source", &source_branch), ("target", &target)] {
                    if !state.refs.contains_key(branch.as_str()) {
                        return Err(Error::module(
                            "unborn_branch",
                            format!(
                                "forge: {label} branch {branch:?} is not born in repo {name:?}"
                            ),
                        ));
                    }
                }
                let number = self.staged_tracker_mut().open_item(
                    &name,
                    ItemKind::Pr,
                    title,
                    body,
                    author,
                    now,
                    Some((source_branch, target)),
                )?;
                if let Some(chat) = chat_target {
                    ctx.emit_msg(tracker::create_channel_msg(chat, &name, number));
                }
                ctx.set_output(sdk::wire::encode(&CreatedItem {
                    number,
                    repo: &name,
                }));
                Ok(())
            }
            ForgeMsg::EditItem {
                repo,
                number,
                title,
                body,
            } => {
                let name = norm_repo(&repo)?;
                self.staged_tracker_mut()
                    .edit_item(&name, number, title, body, now)
            }
            ForgeMsg::SetItemState { repo, number, open } => {
                let name = norm_repo(&repo)?;
                // DELIBERATELY open to any authenticated member: closing and
                // reopening is triage, `Merged` is terminal and refused below,
                // and the inverse op is one message away. apply has already
                // authenticated the origin before any state is staged.
                if let Some(verb) = self
                    .staged_tracker_mut()
                    .set_state(&name, number, open, now)?
                {
                    self.emit_system_line(
                        ctx,
                        chat_target,
                        &name,
                        number,
                        &format!("{verb} this"),
                    )?;
                }
                Ok(())
            }
            ForgeMsg::MergePr {
                repo,
                number,
                prev_target_oid,
                expected_source_oid,
                merge_oid,
                pack_digest,
            } => {
                let name = norm_repo(&repo)?;
                let prev_target = parse_hex_oid(&prev_target_oid, "prev_target_oid")?;
                let expected_source = parse_hex_oid(&expected_source_oid, "expected_source_oid")?;
                let merge = parse_hex_oid(&merge_oid, "merge_oid")?;
                let digest = parse_hex_digest(&pack_digest)?;

                // the PR must be an open PR; pull its branches.
                let (source, target) = self.tracker_view().pr_branches(&name, number)?;

                // double CAS on COMMITTED refs: the target must not have moved
                // under the merger, and the merge must have been computed
                // against the CURRENT source head (a force-push between compute
                // and submit rejects deterministically).
                let state = self.repos.get_mut(&name).ok_or_else(|| {
                    Error::module("unknown_repo", format!("forge: no repo {name:?}"))
                })?;
                if state.refs.get(&source).copied() != Some(expected_source) {
                    return Err(Error::module(
                        "source_branch_moved",
                        "forge: pull request source branch moved; recompute the merge",
                    ));
                }
                state.stage_update(&target, Some(prev_target), Some(merge), Some(digest))?;
                self.staged_tracker_mut()
                    .merge_pr(&name, number, merge, now)?;
                self.emit_system_line(ctx, chat_target, &name, number, "merged this pull request")?;
                Ok(())
            }
            ForgeMsg::SubmitReview {
                repo,
                number,
                verdict,
                body,
                commit_oid,
                comments,
            } => {
                let name = norm_repo(&repo)?;
                let author = party.clone();
                self.staged_tracker_mut().submit_review(
                    &name,
                    number,
                    author,
                    verdict,
                    body,
                    &commit_oid,
                    comments,
                    now,
                )?;
                let line = match verdict {
                    ReviewVerdict::Approve => Some("approved these changes"),
                    ReviewVerdict::RequestChanges => Some("requested changes"),
                    ReviewVerdict::Comment => None,
                };
                if let Some(text) = line {
                    self.emit_system_line(ctx, chat_target, &name, number, text)?;
                }
                Ok(())
            }
        }
    }

    /// emit a system line into an item's discussion channel (no-op without a
    /// chat target). the message id is minted from the item's own monotonic
    /// counter, so it is deterministic and collision-free.
    fn emit_system_line(
        &mut self,
        ctx: &mut dyn Ctx,
        chat_target: Option<&str>,
        repo: &str,
        number: u64,
        text: &str,
    ) -> Result<(), Error> {
        let Some(chat) = chat_target else {
            return Ok(());
        };
        let message_id = self
            .staged_tracker_mut()
            .next_sys_message_id(repo, number)?;
        ctx.emit_msg(tracker::system_line_msg(
            chat, repo, number, message_id, text,
        ));
        Ok(())
    }

    /// the Identity ACCOUNT number a key belongs to, or `None`.
    ///
    /// a host with no identity module at all (the minimal test hosts) has no
    /// accounts, so nothing resolves — the only tolerated query failures are
    /// exactly "that module is not here".
    async fn identity_account(ctx: &dyn Ctx, key: &[u8]) -> Result<Option<u64>, Error> {
        let query = IdentityQuery::OfKey { key: key.to_vec() };
        let reply = match ctx
            .query(IDENTITY_MODULE, &identity::encode_query(&query))
            .await
        {
            Ok(bytes) => bytes,
            Err(Error::UnknownModule(_) | Error::QueryUnsupported) => return Ok(None),
            Err(other) => return Err(other),
        };
        match identity::decode_reply(&reply)
            .map_err(|e| Error::module("identity_reply_decode", e))?
        {
            IdentityReply::Account(account) => Ok(account.map(|a| a.number)),
            other => Err(Error::module(
                "unexpected_identity_reply",
                format!("forge: identity answered an account query with {other:?}"),
            )),
        }
    }

    async fn party_of_key(ctx: &dyn Ctx, key: Vec<u8>) -> Result<Party, Error> {
        if key.is_empty() {
            return Err(Error::module(
                "external_origin_required",
                "forge: operations require an authenticated origin",
            ));
        }
        let account = Self::identity_account(ctx, &key).await?;
        Ok(account.map_or_else(|| Party::Key(key), Party::Account))
    }

    async fn party_of_origin(ctx: &dyn Ctx) -> Result<Party, Error> {
        let origin = &ctx.env().origin;
        match origin {
            Origin::External(key) => Self::party_of_key(ctx, key.clone()).await,
            Origin::Program(account) => Ok(Party::Account(*account)),
            Origin::Module(module) => Ok(Party::Module(module.clone())),
            Origin::System => Err(Error::module(
                "external_origin_required",
                "forge: tracker ops require an authenticated origin",
            )),
        }
    }

    async fn ref_party(ctx: &dyn Ctx) -> Result<Party, Error> {
        match &ctx.env().origin {
            Origin::External(_) | Origin::Program(_) => Self::party_of_origin(ctx).await,
            Origin::Module(_) | Origin::System => Err(Error::module(
                "external_origin_required",
                "forge: a ref-moving op requires an authenticated person",
            )),
        }
    }

    /// Certificate possession proves the signer authorized these exact refs,
    /// repo and network nonce: the signer is the party, never its relay origin.
    async fn push_party(
        ctx: &dyn Ctx,
        chain_id: &str,
        repo: &str,
        cert: Option<&PushCert>,
        updates: &[RefUpdate],
        tags: &[RefUpdate],
    ) -> Result<Party, Error> {
        let Some(cert) = cert else {
            return Self::ref_party(ctx).await;
        };
        let signer = crate::pushcert::signer(cert, chain_id, repo, updates, tags)
            .map_err(|reason| Error::module("bad_push_cert", format!("forge: {reason}")))?;
        Self::party_of_key(ctx, signer).await
    }

    /// stage an atomic multi-ref push: validate both lists, then CAS every
    /// branch and stage every tag creation. PURE and deterministic — no repo
    /// opened, nothing installed, no ref moves (see [`RepoState::stage_update`]
    /// and [`RepoState::stage_tag`]).
    ///
    /// No member owns a repo: any authenticated member births one and moves
    /// any of its branches under the per-branch CAS. Consensus cannot check
    /// ref descendancy (a validator may not hold the objects), so what keeps
    /// `main`/`dev` coherent on disk is materialize's fast-forward rule for a
    /// protected branch and the refusal to delete one — a head that does not
    /// descend from the on-disk ref is held, never installed.
    fn stage_push_refs(
        &mut self,
        name: &str,
        updates: Vec<RefUpdate>,
        tags: Vec<RefUpdate>,
        pack_digest: Option<Vec<u8>>,
    ) -> Result<(), Error> {
        let ref_count = updates.len() + tags.len();
        if ref_count == 0 {
            return Err(Error::module(
                "empty_push",
                "forge: push carries no ref updates",
            ));
        }
        if ref_count > MAX_REFS_PER_PUSH {
            return Err(Error::module(
                "push_cap",
                format!("forge: too many ref updates ({ref_count}, max {MAX_REFS_PER_PUSH})"),
            ));
        }
        let branches = updates.iter().map(|u| RefName::Branch(u.ref_name.clone()));
        let tag_names = tags.iter().map(|u| RefName::Tag(u.ref_name.clone()));
        let mut seen = BTreeSet::new();
        for refname in branches.chain(tag_names) {
            norm_branch(refname.short())?;
            let full = refname.full();
            if !seen.insert(refname) {
                return Err(Error::module(
                    "duplicate_ref_update",
                    format!("forge: duplicate ref update for {full:?}"),
                ));
            }
        }
        let digest = pack_digest.as_deref().map(parse_digest).transpose()?;
        let sets_a_head = updates.iter().chain(&tags).any(|u| u.new_oid.is_some());
        if sets_a_head && digest.is_none() {
            return Err(Error::module(
                "missing_pack_digest",
                "forge: a push that sets heads needs a pack_digest",
            ));
        }

        // a repo the push BIRTHS is only inserted once EVERY CAS succeeded —
        // `abort_block` drops staged fates but never a map entry, so inserting
        // first would leave a phantom repo behind a rejected push (visible to
        // `ListRepos`, and gone again after a restart re-adopt).
        match self.repos.remove(name) {
            Some(mut state) => {
                let staged = stage_updates(&mut state, &updates, &tags, digest);
                self.repos.insert(name.to_string(), state);
                staged
            }
            None => {
                let mut state = RepoState::default();
                stage_updates(&mut state, &updates, &tags, digest)?;
                self.repos.insert(name.to_string(), state);
                Ok(())
            }
        }
    }

    /// Reports are derived from the accepted source mutation, before any
    /// publication leaves its atomic unit. The revision is inside the tracker
    /// image, so ODB adoption, per-dispatch reload and snapshots preserve it.
    fn publish_attribution(
        &mut self,
        ctx: &mut dyn Ctx,
        target: Option<&str>,
        before: &Self,
        party: &Party,
    ) -> Result<(), Error> {
        let Some(target) = target else {
            return Ok(());
        };
        let mut reports = Vec::new();
        for (repo, tracker) in &self.tracker_view().repos {
            let previous = before.tracker_view().repos.get(repo);
            for (number, item) in &tracker.items {
                let previous_item = previous.and_then(|tracker| tracker.items.get(number));
                if previous_item == Some(item) {
                    continue;
                }
                let mut relations: Vec<_> =
                    party_relation(&item.author, Reason::Authorship, Vec::new())
                        .into_iter()
                        .collect();
                let reviewers: BTreeSet<_> = item
                    .reviews
                    .iter()
                    .filter_map(|review| review.author.account())
                    .collect();
                relations.extend(reviewers.into_iter().map(|recipient| Relation {
                    recipient,
                    reason: Reason::Credit,
                    detail: Vec::new(),
                }));
                reports.push((source_object("item", (repo, number)), relations));
                let old_reviews = previous_item.map_or(0, |item| item.reviews.len());
                for (index, review) in item.reviews.iter().enumerate().skip(old_reviews) {
                    let relations = party_relation(&review.author, Reason::Authorship, Vec::new())
                        .into_iter()
                        .collect();
                    reports.push((
                        source_object("review", (repo, number, index + 1)),
                        relations,
                    ));
                }
            }
        }
        for (repo, state) in &self.repos {
            for (refname, fate) in &state.staged {
                let already_staged = before
                    .repos
                    .get(repo)
                    .is_some_and(|state| state.staged.contains_key(refname));
                if already_staged {
                    continue;
                }
                let relations = match fate {
                    StagedRef::Packed(head, _) => party_relation(
                        party,
                        Reason::Defined("ref_writer".into()),
                        head.as_bytes().to_vec(),
                    )
                    .into_iter()
                    .collect(),
                    StagedRef::Delete => Vec::new(),
                };
                let object = match refname {
                    RefName::Branch(branch) => source_object("ref", (repo, branch)),
                    RefName::Tag(tag) => source_object("tag", (repo, tag)),
                };
                reports.push((object, relations));
            }
        }
        if reports.is_empty() {
            return Ok(());
        }
        let revision = self
            .tracker_view()
            .source_revision
            .checked_add(1)
            .ok_or_else(|| {
                Error::module("revision_exhausted", "forge: source revision exhausted")
            })?;
        self.staged_tracker_mut().source_revision = revision;
        let updates = reports
            .into_iter()
            .map(|(object, relations)| AttributionUpdate {
                object,
                revision,
                actor: attributed_actor(party),
                relations,
                transfers: Vec::new(),
            })
            .collect();
        ctx.emit_msg(sdk::Msg {
            target: target.into(),
            payload: attribution::encode_msg(&AttributionMsg::AttributeBatch { updates }),
        });
        Ok(())
    }

    // ---- the state image ----------------------------------------------------

    /// the COMMITTED state as one image — the substrate's view of the block
    /// boundary, and what a per-dispatch runtime reads at a block's first
    /// dispatch.
    pub fn committed_image(&self) -> Vec<u8> {
        encode_image(
            self.repos
                .iter()
                .map(|(n, s)| (n.as_str(), &s.refs, &s.tags)),
            &self.tracker,
        )
    }

    /// the state as it will read once this block publishes — every staged
    /// fate on the ref maps and the block's tracker view — as one image.
    /// what a per-dispatch runtime chains to the next dispatch and, at the
    /// block boundary, hands the substrate to adopt.
    pub fn published_image(&self) -> Vec<u8> {
        let published: BTreeMap<&str, RepoRefs> = self
            .repos
            .iter()
            .map(|(n, s)| (n.as_str(), s.published()))
            .collect();
        encode_image(
            published
                .iter()
                .map(|(n, refs)| (*n, &refs.branches, &refs.tags)),
            self.tracker_view(),
        )
    }

    /// re-enter a block mid-way: the chained image (committed ⊕ the fates so
    /// far) and the block scratch (those fates with the committed heads they
    /// shadow) rebuild the native mid-block shape — committed refs with every
    /// staged ref reverted to its committed head, the fates staged on top,
    /// and the tracker as the block sees it. a repo the scratch names but the
    /// image omits is one whose last ref is staged for deletion.
    pub fn from_lane(image: Image, scratch: BlockScratch) -> Self {
        let Image {
            repos: mut images,
            tracker,
        } = image;
        let mut repos = BTreeMap::new();
        for (name, fates) in scratch {
            let mut committed = images.remove(&name).unwrap_or_default();
            let mut staged = BTreeMap::new();
            for (refname, (prev, fate)) in fates {
                committed.set(&refname, prev);
                staged.insert(refname, fate);
            }
            repos.insert(name, RepoState::staged_over(committed, staged));
        }
        for (name, committed) in images {
            repos.insert(name, RepoState::with_committed(committed));
        }
        Self {
            repos,
            tracker,
            staged_tracker: None,
        }
    }

    /// this block's scratch: every staged fate with the committed head it
    /// shadows (`None` = unborn), per repo. only repos with a staged fate
    /// appear.
    pub fn block_scratch(&self) -> BlockScratch {
        self.repos
            .iter()
            .filter(|(_, state)| !state.staged.is_empty())
            .map(|(name, state)| {
                let fates = state
                    .staged
                    .iter()
                    .map(|(refname, fate)| {
                        (refname.clone(), (state.committed_head(refname), *fate))
                    })
                    .collect();
                (name.clone(), fates)
            })
            .collect()
    }

    /// the packed heads staged since `before` (an earlier scratch of this
    /// block) — the ref targets ONE dispatch adds. the one-fate-per-ref rule
    /// makes every staged ref appear in exactly one dispatch's set.
    pub fn ref_targets_since(&self, before: &BlockScratch) -> Vec<RefTarget> {
        let mut targets = Vec::new();
        for (name, state) in &self.repos {
            for (refname, fate) in &state.staged {
                let already_staged = before
                    .get(name)
                    .is_some_and(|fates| fates.contains_key(refname));
                if already_staged {
                    continue;
                }
                if let StagedRef::Packed(head, pack) = fate {
                    targets.push(RefTarget {
                        repo: name.clone(),
                        name: refname.clone(),
                        head: *head,
                        pack: *pack,
                    });
                }
            }
        }
        targets
    }

    /// the fates a block's final image + its ref targets stage over the
    /// COMMITTED state — how the substrate turns an adopted image back into the
    /// per-ref publish it would have run natively. every target IS a packed
    /// fate (its head must be the image's head for that ref); every
    /// committed ref the image drops is a delete; a head the image moves
    /// without a target is a runtime bug and refuses deterministically.
    pub fn fates_for_image(
        &self,
        image: &Image,
        targets: Vec<RefTarget>,
    ) -> Result<BTreeMap<String, BTreeMap<RefName, StagedRef>>, Error> {
        let mut fates: BTreeMap<String, BTreeMap<RefName, StagedRef>> = BTreeMap::new();
        for target in targets {
            let image_head = image
                .repos
                .get(&target.repo)
                .and_then(|refs| refs.head(&target.name));
            if image_head != Some(target.head) {
                return Err(Error::module(
                    "ref_target_mismatch",
                    format!(
                        "forge: ref target {}/{} names head {} but the image commits {:?}",
                        target.repo,
                        target.name.full(),
                        target.head,
                        image_head
                    ),
                ));
            }
            fates
                .entry(target.repo)
                .or_default()
                .insert(target.name, StagedRef::Packed(target.head, target.pack));
        }
        for (name, state) in &self.repos {
            let new_refs = image.repos.get(name);
            for (refname, head) in state.committed().iter() {
                let new_head = new_refs.and_then(|refs| refs.head(&refname));
                let has_target = fates
                    .get(name)
                    .is_some_and(|repo| repo.contains_key(&refname));
                let Some(new_head) = new_head else {
                    fates
                        .entry(name.clone())
                        .or_default()
                        .insert(refname, StagedRef::Delete);
                    continue;
                };
                let moved_without_target = new_head != head && !has_target;
                if moved_without_target {
                    return Err(Error::module(
                        "missing_ref_target",
                        format!(
                            "forge: image moves {name}/{} without a ref target",
                            refname.full()
                        ),
                    ));
                }
            }
        }
        for (name, refs) in &image.repos {
            for (refname, _) in refs.iter() {
                let committed = self
                    .repos
                    .get(name)
                    .is_some_and(|state| state.committed_head(&refname).is_some());
                let has_target = fates
                    .get(name)
                    .is_some_and(|repo| repo.contains_key(&refname));
                let born_without_target = !committed && !has_target;
                if born_without_target {
                    return Err(Error::module(
                        "missing_ref_target",
                        format!(
                            "forge: image births {name}/{} without a ref target",
                            refname.full()
                        ),
                    ));
                }
            }
        }
        Ok(fates)
    }
}

/// the decoded state image: born repos' branch and tag maps + the tracker.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Image {
    pub repos: BTreeMap<String, RepoRefs>,
    pub tracker: Tracker,
}

impl Image {
    /// the composed root the image commits to — the same fold the substrate
    /// computes over its resident maps.
    pub fn root(&self) -> StateRoot {
        compose_state_root(
            self.repos
                .iter()
                .map(|(n, refs)| (n.as_str(), &refs.branches, &refs.tags)),
            &self.tracker,
        )
    }
}

/// encode a state image: `FGI1 ++ u32(repo_count) ++ per BORN repo sorted by
/// name (u32 name_len ++ name ++ branches ++ tags) ++ u32(tracker_len) ++
/// tracker`, where each of branches and tags is `u32 count ++ per ref sorted
/// (u32 short_len ++ short ++ oid[20])`. only born repos (any ref at all) are
/// carried — exactly the root's preimage material.
pub fn encode_image<'a>(
    repos: impl Iterator<Item = (&'a str, &'a RefMap, &'a RefMap)>,
    tracker: &Tracker,
) -> Vec<u8> {
    let born: Vec<(&str, &RefMap, &RefMap)> = repos
        .filter(|(_, branches, tags)| !branches.is_empty() || !tags.is_empty())
        .collect();
    let mut out = IMAGE_MAGIC.to_vec();
    codec::put_u32(&mut out, born.len() as u32);
    for (name, branches, tags) in born {
        codec::put_str(&mut out, name);
        put_ref_map(&mut out, branches);
        put_ref_map(&mut out, tags);
    }
    codec::put_bytes(&mut out, &tracker.canonical_bytes());
    out
}

/// append one repo's branch or tag map: `u32 count ++ per ref sorted (u32
/// short_len ++ short ++ oid[20])` — the image's and the snapshot's shared
/// encoding.
pub(crate) fn put_ref_map(out: &mut Vec<u8>, refs: &RefMap) {
    codec::put_u32(out, refs.len() as u32);
    for (short, oid) in refs {
        codec::put_str(out, short);
        out.extend_from_slice(oid.as_bytes());
    }
}

/// read one repo's branch or tag map from UNTRUSTED bytes: every name
/// re-validated, no zero oid, no duplicate. `reason` names the container.
pub(crate) fn take_ref_map(r: &mut Reader, reason: &str, repo: &str) -> Result<RefMap, Error> {
    let count = r.u32()?;
    let mut refs = RefMap::new();
    for _ in 0..count {
        let short = r.str_()?;
        norm_branch(&short)?;
        let oid = Oid::from_bytes(r.take(OID_RAW_LEN)?)?;
        if oid.is_zero() {
            return Err(Error::module(
                reason,
                format!("forge: ref {short} of {repo} carries a zero oid"),
            ));
        }
        if refs.insert(short, oid).is_some() {
            return Err(Error::module(
                reason,
                format!("forge: duplicate ref in repo {repo}"),
            ));
        }
    }
    Ok(refs)
}

/// decode a state image from bytes the host lane carried: every field is
/// bounds-checked and every name/ref re-validated, so a corrupt lane fails
/// closed instead of re-genesis-ing the module.
pub fn decode_image(bytes: &[u8]) -> Result<Image, Error> {
    let body = bytes
        .strip_prefix(IMAGE_MAGIC.as_slice())
        .ok_or_else(|| Error::module("image_decode", "forge image: missing the FGI1 magic"))?;
    let mut r = Reader::new(body);
    let count = r.u32()?;
    let mut repos = BTreeMap::new();
    for _ in 0..count {
        let name = norm_repo(&r.str_()?)?;
        let refs = RepoRefs {
            branches: take_ref_map(&mut r, "image_decode", &name)?,
            tags: take_ref_map(&mut r, "image_decode", &name)?,
        };
        if refs.is_empty() {
            return Err(Error::module(
                "image_decode",
                format!("forge image: repo {name} carries no refs"),
            ));
        }
        if repos.insert(name.clone(), refs).is_some() {
            return Err(Error::module(
                "image_decode",
                format!("forge image: duplicate repo {name}"),
            ));
        }
    }
    let tracker_len = r.u32()? as usize;
    let tracker = Tracker::decode(r.take(tracker_len)?)?;
    if !r.done() {
        return Err(Error::module(
            "image_decode",
            "forge image: trailing bytes after the container",
        ));
    }
    Ok(Image { repos, tracker })
}

// ---- the block scratch --------------------------------------------------------

/// per repo, per staged ref: the committed head it shadows (`None` =
/// unborn) and the staged fate.
pub type BlockScratch = BTreeMap<String, BTreeMap<RefName, (Option<Oid>, StagedRef)>>;

/// encode a block scratch: `FGB1 ++ u32(repo_count) ++ per repo (u32 name_len
/// ++ name ++ u32 fate_count ++ per ref (u32 refname_len ++ FULL refname ++
/// prev tag (0 | 1 ++ oid[20]) ++ fate tag (0 delete | 1 ++ oid[20] ++
/// digest[32])))`.
pub fn encode_block_scratch(scratch: &BlockScratch) -> Vec<u8> {
    let mut out = BLOCK_SCRATCH_MAGIC.to_vec();
    codec::put_u32(&mut out, scratch.len() as u32);
    for (name, fates) in scratch {
        codec::put_str(&mut out, name);
        codec::put_u32(&mut out, fates.len() as u32);
        for (refname, (prev, fate)) in fates {
            codec::put_str(&mut out, &refname.full());
            match prev {
                None => codec::put_u8(&mut out, 0),
                Some(oid) => {
                    codec::put_u8(&mut out, 1);
                    out.extend_from_slice(oid.as_bytes());
                }
            }
            match fate {
                StagedRef::Delete => codec::put_u8(&mut out, 0),
                StagedRef::Packed(oid, digest) => {
                    codec::put_u8(&mut out, 1);
                    out.extend_from_slice(oid.as_bytes());
                    out.extend_from_slice(digest);
                }
            }
        }
    }
    out
}

/// decode a block scratch the host lane carried back — the same fail-closed
/// posture as [`decode_image`].
pub fn decode_block_scratch(bytes: &[u8]) -> Result<BlockScratch, Error> {
    let body = bytes
        .strip_prefix(BLOCK_SCRATCH_MAGIC.as_slice())
        .ok_or_else(|| {
            Error::module(
                "scratch_decode",
                "forge block scratch: missing the FGB1 magic",
            )
        })?;
    let mut r = Reader::new(body);
    let count = r.u32()?;
    let mut scratch = BlockScratch::new();
    for _ in 0..count {
        let name = norm_repo(&r.str_()?)?;
        let fate_count = r.u32()?;
        let mut fates = BTreeMap::new();
        for _ in 0..fate_count {
            let refname = RefName::parse(&r.str_()?)?;
            let prev = match r.u8()? {
                0 => None,
                1 => Some(Oid::from_bytes(r.take(OID_RAW_LEN)?)?),
                t => {
                    return Err(Error::module(
                        "scratch_decode",
                        format!("forge block scratch: bad prev tag {t}"),
                    ));
                }
            };
            let fate = match r.u8()? {
                0 => StagedRef::Delete,
                1 => {
                    let oid = Oid::from_bytes(r.take(OID_RAW_LEN)?)?;
                    let digest: [u8; 32] = r
                        .take(32)?
                        .try_into()
                        .expect("take(32) yields exactly 32 bytes");
                    StagedRef::Packed(oid, digest)
                }
                t => {
                    return Err(Error::module(
                        "scratch_decode",
                        format!("forge block scratch: bad fate tag {t}"),
                    ));
                }
            };
            if fates.insert(refname, (prev, fate)).is_some() {
                return Err(Error::module(
                    "scratch_decode",
                    format!("forge block scratch: duplicate ref in repo {name}"),
                ));
            }
        }
        if scratch.insert(name.clone(), fates).is_some() {
            return Err(Error::module(
                "scratch_decode",
                format!("forge block scratch: duplicate repo {name}"),
            ));
        }
    }
    if !r.done() {
        return Err(Error::module(
            "scratch_decode",
            "forge block scratch: trailing bytes after the container",
        ));
    }
    Ok(scratch)
}

// ---- the ref target -----------------------------------------------------------

/// one packed head a block stages: the pack that materializes it, keyed by the
/// ref it moves or creates. the object-plane record a per-dispatch runtime
/// hands the substrate at the block boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefTarget {
    pub repo: String,
    pub name: RefName,
    pub head: Oid,
    pub pack: [u8; 32],
}

/// `u32 repo_len ++ repo ++ u32 refname_len ++ FULL refname ++ oid[20] ++
/// digest[32]`.
pub fn encode_ref_target(target: &RefTarget) -> Vec<u8> {
    let mut out = Vec::new();
    codec::put_str(&mut out, &target.repo);
    codec::put_str(&mut out, &target.name.full());
    out.extend_from_slice(target.head.as_bytes());
    out.extend_from_slice(&target.pack);
    out
}

pub fn decode_ref_target(bytes: &[u8]) -> Result<RefTarget, Error> {
    let mut r = Reader::new(bytes);
    let repo = norm_repo(&r.str_()?)?;
    let name = RefName::parse(&r.str_()?)?;
    let head = Oid::from_bytes(r.take(OID_RAW_LEN)?)?;
    let pack: [u8; 32] = r
        .take(32)?
        .try_into()
        .expect("take(32) yields exactly 32 bytes");
    if !r.done() {
        return Err(Error::module(
            "ref_target_decode",
            "forge ref target: trailing bytes after the record",
        ));
    }
    Ok(RefTarget {
        repo,
        name,
        head,
        pack,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker_iface::ItemKind;
    use chat::Party;

    fn oid(c: char) -> Oid {
        Oid::from_hex(&c.to_string().repeat(40)).unwrap()
    }

    fn branch(name: &str) -> RefName {
        RefName::Branch(name.into())
    }

    fn tag(name: &str) -> RefName {
        RefName::Tag(name.into())
    }

    fn refs(branches: &[(&str, Oid)], tags: &[(&str, Oid)]) -> RepoRefs {
        let map = |pairs: &[(&str, Oid)]| pairs.iter().map(|(n, o)| (n.to_string(), *o)).collect();
        RepoRefs {
            branches: map(branches),
            tags: map(tags),
        }
    }

    fn state_with(repos: &[(&str, &[(&str, Oid)])]) -> ForgeState {
        let mut state = ForgeState::default();
        for (name, branches) in repos {
            state.repos.insert(
                name.to_string(),
                RepoState::with_committed(refs(branches, &[])),
            );
        }
        state
    }

    #[test]
    fn image_round_trips_and_carries_only_born_repos() {
        let mut state = state_with(&[
            ("alpha", &[("main", oid('a')), ("dev", oid('b'))]),
            ("unborn", &[]),
        ]);
        state
            .tracker
            .open_item(
                "alpha",
                ItemKind::Issue,
                "t".into(),
                String::new(),
                Party::Key(vec![1]),
                1,
                None,
            )
            .unwrap();
        let image = decode_image(&state.committed_image()).unwrap();
        assert_eq!(image.repos.len(), 1, "unborn repos are not carried");
        assert_eq!(image.repos["alpha"].branches["main"], oid('a'));
        assert_eq!(image.tracker, state.tracker);
        assert_eq!(
            image.root(),
            state.root(),
            "the image commits to the resident root"
        );
        assert!(decode_image(b"nope").is_err());
        let mut truncated = state.committed_image();
        truncated.pop();
        assert!(decode_image(&truncated).is_err());
    }

    #[test]
    fn lane_re_entry_rebuilds_the_native_mid_block_shape() {
        // block start: alpha/main = a committed; dispatch 1 pushes main a->b and
        // births beta/main = c, deletes alpha/feat.
        let mut state = state_with(&[("alpha", &[("main", oid('a')), ("feat", oid('f'))])]);
        let before = state.block_scratch();
        assert!(before.is_empty());
        state
            .repos
            .get_mut("alpha")
            .unwrap()
            .stage_update("main", Some(oid('a')), Some(oid('b')), Some([1; 32]))
            .unwrap();
        state
            .repos
            .get_mut("alpha")
            .unwrap()
            .stage_update("feat", Some(oid('f')), None, None)
            .unwrap();
        state
            .repos
            .entry("beta".into())
            .or_default()
            .stage_update("main", None, Some(oid('c')), Some([2; 32]))
            .unwrap();
        let targets = state.ref_targets_since(&before);
        assert_eq!(
            targets.len(),
            2,
            "one target per packed head, none for the delete"
        );

        // the lane carries the chained image + the scratch to dispatch 2.
        let image = decode_image(&state.published_image()).unwrap();
        assert_eq!(image.repos["alpha"].branches["main"], oid('b'));
        assert!(!image.repos["alpha"].branches.contains_key("feat"));
        assert_eq!(image.repos["beta"].branches["main"], oid('c'));
        let scratch = decode_block_scratch(&encode_block_scratch(&state.block_scratch())).unwrap();
        let reentered = ForgeState::from_lane(image, scratch);

        // committed refs are the pre-block ones; the fates sit on top.
        let alpha = &reentered.repos["alpha"];
        assert_eq!(alpha.refs["main"], oid('a'));
        assert_eq!(alpha.refs["feat"], oid('f'));
        assert_eq!(
            alpha.staged[&branch("main")],
            StagedRef::Packed(oid('b'), [1; 32])
        );
        assert_eq!(alpha.staged[&branch("feat")], StagedRef::Delete);
        let beta = &reentered.repos["beta"];
        assert!(beta.refs.is_empty(), "a birthed repo has no committed refs");
        assert_eq!(
            beta.staged[&branch("main")],
            StagedRef::Packed(oid('c'), [2; 32])
        );
        // the one-fate rule holds across the re-entry, exactly as natively.
        assert!(
            reentered.repos["alpha"].refs.get("main").copied() == Some(oid('a'))
                && ForgeState::from_lane(
                    decode_image(&reentered.published_image()).unwrap(),
                    reentered.block_scratch()
                )
                .repos["alpha"]
                    .staged
                    .contains_key(&branch("main"))
        );
        // nothing new was staged since the scratch was taken.
        assert!(
            reentered
                .ref_targets_since(&reentered.block_scratch())
                .is_empty()
        );
        // and the chained image is stable across re-entry.
        assert_eq!(reentered.published_image(), state.published_image());
    }

    #[test]
    fn fates_for_image_mirror_the_native_publish() {
        let committed = state_with(&[("alpha", &[("main", oid('a')), ("feat", oid('f'))])]);
        let image = Image {
            repos: BTreeMap::from([
                ("alpha".to_string(), refs(&[("main", oid('b'))], &[])),
                ("beta".to_string(), refs(&[("main", oid('c'))], &[])),
            ]),
            tracker: Tracker::default(),
        };
        let targets = vec![
            RefTarget {
                repo: "alpha".into(),
                name: branch("main"),
                head: oid('b'),
                pack: [1; 32],
            },
            RefTarget {
                repo: "beta".into(),
                name: branch("main"),
                head: oid('c'),
                pack: [2; 32],
            },
        ];
        let fates = committed.fates_for_image(&image, targets.clone()).unwrap();
        assert_eq!(
            fates["alpha"][&branch("main")],
            StagedRef::Packed(oid('b'), [1; 32])
        );
        assert_eq!(fates["alpha"][&branch("feat")], StagedRef::Delete);
        assert_eq!(
            fates["beta"][&branch("main")],
            StagedRef::Packed(oid('c'), [2; 32])
        );

        // a moved head with no target, a birthed branch with no target, and a
        // target disagreeing with the image all refuse.
        assert!(committed.fates_for_image(&image, Vec::new()).is_err());
        let mut wrong = targets.clone();
        wrong[0].head = oid('9');
        assert!(committed.fates_for_image(&image, wrong).is_err());
        assert!(
            committed
                .fates_for_image(&image, targets[..1].to_vec())
                .is_err()
        );

        // a same-head re-push is a packed fate too (native re-records the pack).
        let same = Image {
            repos: BTreeMap::from([(
                "alpha".to_string(),
                refs(&[("main", oid('a')), ("feat", oid('f'))], &[]),
            )]),
            tracker: Tracker::default(),
        };
        let repush = vec![RefTarget {
            repo: "alpha".into(),
            name: branch("main"),
            head: oid('a'),
            pack: [3; 32],
        }];
        let fates = committed.fates_for_image(&same, repush).unwrap();
        assert_eq!(
            fates["alpha"][&branch("main")],
            StagedRef::Packed(oid('a'), [3; 32])
        );
        assert!(!fates["alpha"].contains_key(&branch("feat")));

        // a created tag is a packed fate like a branch head; one with no
        // target is refused the same way.
        let tagged = Image {
            repos: BTreeMap::from([(
                "alpha".to_string(),
                refs(
                    &[("main", oid('a')), ("feat", oid('f'))],
                    &[("v1", oid('a'))],
                ),
            )]),
            tracker: Tracker::default(),
        };
        let tag_target = vec![RefTarget {
            repo: "alpha".into(),
            name: tag("v1"),
            head: oid('a'),
            pack: [4; 32],
        }];
        let fates = committed.fates_for_image(&tagged, tag_target).unwrap();
        assert_eq!(fates["alpha"].len(), 1, "only the tag moved");
        assert_eq!(
            fates["alpha"][&tag("v1")],
            StagedRef::Packed(oid('a'), [4; 32])
        );
        assert!(committed.fates_for_image(&tagged, Vec::new()).is_err());
    }

    #[test]
    fn ref_target_round_trips() {
        for name in [branch("feature/x"), tag("v1.0")] {
            let target = RefTarget {
                repo: "alpha".into(),
                name,
                head: oid('d'),
                pack: [9; 32],
            };
            assert_eq!(
                decode_ref_target(&encode_ref_target(&target)).unwrap(),
                target
            );
            let mut extra = encode_ref_target(&target);
            extra.push(0);
            assert!(decode_ref_target(&extra).is_err());
        }
    }

    #[test]
    fn the_image_carries_tags_beside_branches_and_the_root_folds_them() {
        let mut state = state_with(&[("alpha", &[("main", oid('a'))])]);
        let untagged_root = state.root();
        let alpha = state.repos.get_mut("alpha").unwrap();
        alpha.tags.insert("v1".into(), oid('a'));
        // a repo holding only a tag is born: it is carried and it roots.
        state.repos.insert(
            "tags-only".into(),
            RepoState::with_committed(refs(&[], &[("v0", oid('c'))])),
        );
        let image = decode_image(&state.committed_image()).unwrap();
        assert_eq!(
            image.repos["alpha"],
            refs(&[("main", oid('a'))], &[("v1", oid('a'))])
        );
        assert_eq!(image.repos["tags-only"], refs(&[], &[("v0", oid('c'))]));
        assert_eq!(image.root(), state.root());
        assert_ne!(state.root(), untagged_root, "a tag is committed state");

        // the same oid as a branch and as a tag are different preimages.
        let as_branch = state_with(&[("alpha", &[("main", oid('a')), ("v1", oid('a'))])]);
        let mut as_tag = state_with(&[("alpha", &[("main", oid('a'))])]);
        as_tag
            .repos
            .get_mut("alpha")
            .unwrap()
            .tags
            .insert("v1".into(), oid('a'));
        assert_ne!(as_branch.root(), as_tag.root());
    }

    #[test]
    fn a_staged_tag_rides_the_lane_like_a_branch_head() {
        let mut state = state_with(&[("alpha", &[("main", oid('a'))])]);
        let before = state.block_scratch();
        state
            .stage_push_refs(
                "alpha",
                update("main", Some(oid('a')), Some(oid('b'))),
                update("v1", None, Some(oid('b'))),
                Some(vec![5; 32]),
            )
            .unwrap();
        let targets = state.ref_targets_since(&before);
        assert_eq!(
            targets.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
            vec![branch("main"), tag("v1")],
            "one target per packed ref, branches first"
        );

        let image = decode_image(&state.published_image()).unwrap();
        assert_eq!(image.repos["alpha"].tags["v1"], oid('b'));
        let scratch = decode_block_scratch(&encode_block_scratch(&state.block_scratch())).unwrap();
        let mut reentered = ForgeState::from_lane(image, scratch);
        let alpha = &reentered.repos["alpha"];
        assert!(alpha.tags.is_empty(), "the tag is staged, not committed");
        assert_eq!(
            alpha.staged[&tag("v1")],
            StagedRef::Packed(oid('b'), [5; 32])
        );
        // the create-once rule holds across the re-entry.
        let again = reentered
            .stage_push_refs(
                "alpha",
                Vec::new(),
                update("v1", None, Some(oid('c'))),
                Some(vec![6; 32]),
            )
            .unwrap_err();
        assert!(again.to_string().contains("tag_immutable"), "{again}");
    }

    #[test]
    fn a_push_names_each_ref_once_and_counts_tags_against_its_cap() {
        let mut state = ForgeState::default();
        let digest = Some(vec![7u8; 32]);
        let twice = [
            update("v1", None, Some(oid('a'))),
            update("v1", None, Some(oid('b'))),
        ]
        .concat();
        let refused = state
            .stage_push_refs("alpha", Vec::new(), twice, digest.clone())
            .unwrap_err();
        assert!(
            refused.to_string().contains("duplicate_ref_update"),
            "{refused}"
        );
        // a branch and a tag of the same name are two refs, not a duplicate.
        state
            .stage_push_refs(
                "alpha",
                update("v1", None, Some(oid('a'))),
                update("v1", None, Some(oid('a'))),
                digest.clone(),
            )
            .unwrap();
        let no_pack = ForgeState::default()
            .stage_push_refs(
                "alpha",
                Vec::new(),
                update("v2", None, Some(oid('a'))),
                None,
            )
            .unwrap_err();
        assert!(
            no_pack.to_string().contains("missing_pack_digest"),
            "{no_pack}"
        );

        let over: Vec<RefUpdate> = (0..=MAX_REFS_PER_PUSH / 2)
            .flat_map(|i| update(&format!("r{i}"), None, Some(oid('a'))))
            .collect();
        let refused = ForgeState::default()
            .stage_push_refs("alpha", over.clone(), over, digest.clone())
            .unwrap_err();
        assert!(refused.to_string().contains("push_cap"), "{refused}");

        let full: BTreeMap<String, Oid> = (0..MAX_TAGS_PER_REPO)
            .map(|i| (format!("t{i}"), oid('a')))
            .collect();
        let mut capped = ForgeState::default();
        capped.repos.insert(
            "alpha".into(),
            RepoState::with_committed(RepoRefs {
                branches: BTreeMap::new(),
                tags: full,
            }),
        );
        let refused = capped
            .stage_push_refs(
                "alpha",
                Vec::new(),
                update("new", None, Some(oid('c'))),
                digest,
            )
            .unwrap_err();
        assert!(refused.to_string().contains("tag_cap"), "{refused}");
    }

    /// a single-branch create/delete push, the shape both cap tests drive.
    fn update(branch: &str, prev: Option<Oid>, new: Option<Oid>) -> Vec<RefUpdate> {
        vec![RefUpdate {
            ref_name: branch.into(),
            prev_oid: prev.map(|o| o.as_bytes().to_vec()),
            new_oid: new.map(|o| o.as_bytes().to_vec()),
        }]
    }

    #[test]
    fn a_push_may_not_grow_a_repo_past_its_branch_cap() {
        let full = RepoRefs {
            branches: (0..MAX_BRANCHES_PER_REPO)
                .map(|i| (format!("b{i}"), oid('a')))
                .collect(),
            ..Default::default()
        };
        let mut state = ForgeState::default();
        state
            .repos
            .insert("alpha".into(), RepoState::with_committed(full));
        let digest = Some(vec![7u8; 32]);

        let refused = state
            .stage_push_refs(
                "alpha",
                update("new", None, Some(oid('c'))),
                Vec::new(),
                digest.clone(),
            )
            .unwrap_err();
        assert!(
            refused.to_string().contains("branch cap"),
            "the cap+1-th branch is refused: {refused}"
        );
        // the host drops every staged fate of a rejected block (`abort_block`).
        state.repos.get_mut("alpha").unwrap().abort();
        // a delete is always allowed — it is the only way back under the cap.
        state
            .stage_push_refs(
                "alpha",
                update("b0", Some(oid('a')), None),
                Vec::new(),
                None,
            )
            .unwrap();
        state
            .stage_push_refs(
                "alpha",
                update("new", None, Some(oid('c'))),
                Vec::new(),
                digest,
            )
            .unwrap();
    }
}
