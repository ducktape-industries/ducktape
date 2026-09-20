//! the NATIVE forge module: [`Forge`] is the block-spanning `sdk::Module` over
//! the on-disk git substrate — the daemon, sim, and demo lanes compose it, and
//! the wasm tenant's host-side substrate (`forge_odb::ForgeOdbBacking`)
//! wraps it for the substrate half (root, browse/diff reads, snapshot packing,
//! materialization). the accept/reject logic is NOT here: `execute` delegates
//! to the shared [`ForgeState`] core, so this file owns only what touches disk
//! or reads a node-local object database.
//!
//! ## the host-lent staging seam (per repo + tracker)
//!
//! `execute` stages every change WITHOUT moving refs or the committed tracker
//! (`root()` reads committed state only); `commit_block` publishes staged
//! branches (or records node-local materialization targets) and swaps the
//! staged tracker in (persisting `<base>/.tracker.bin`); `abort_block` drops
//! everything staged.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use sdk::{Ctx, Error, Module, ModuleId, Msg, StateRoot, StateSyncHandle};

use crate::refs::RepoState;
use crate::state::{ForgeState, RefTarget};
use crate::tracker::Tracker;
use crate::*;
#[cfg(test)]
use crate::{
    query::{MAX_BROWSE_TREE_DEPTH, browse_path},
    refs::MAIN_BRANCH,
};

/// the node-local file the committed tracker persists to under `base` —
/// canonical bytes, rewritten atomically at every mutating `commit_block`,
/// re-adopted at construction (the tracker analogue of the on-disk git refs).
/// never a valid repo dir name (repos are directories; this is a file).
const TRACKER_FILE: &str = ".tracker.bin";

/// the node-local file the per-repo CATCH-UP MAP persists to under `base`.
///
/// a branch's committed head is CONSENSUS state; the on-disk git ref is a
/// node-local cache that legitimately lags it whenever the pack has not
/// arrived (that decoupling IS forge's fork-safety invariant). so the
/// committed map may NOT be re-derived from the ref cache alone at boot —
/// doing that silently rewinds this node's forge root, and recovery then
/// fail-stops on a root-hash recompose, bricking a node that was healthy.
/// this file carries exactly the gap: `repo -> branch -> (head, pack digest)`.
/// rewritten atomically at every commit, removed once nothing is outstanding.
const PENDING_FILE: &str = ".pending.bin";

/// the node-local, ADVISORY list of pending refs this node is stuck on
/// (see [`refs::RepoState::stuck_refs`]) — one `<repo>\t<full refname>` line
/// each, rewritten with [`PENDING_FILE`].
///
/// it carries no consensus state, so unlike the pending map a missing or
/// unreadable file is not fail-stop: it only leaves [`compact_repos`] as
/// conservative as it was before. ponytail: a plain line list, because that
/// is all a hint needs; give it a section in `.pending.bin` if it ever has to
/// carry more than two names.
const STUCK_FILE: &str = ".stuck.txt";

/// the node-local snapshot memo. recovery checkpoints already persist the
/// same bytes elsewhere; this copy exists only so reopening the git substrate
/// does not re-pack an unchanged object closure on the validator's command
/// loop before it can answer reads.
pub(crate) const SNAPSHOT_CACHE_FILE: &str = ".snapshot-cache.bin";

/// the 4-byte magic the pending file leads with.
const FORGE_PENDING_MAGIC: &[u8; 4] = b"FGP1";

/// the 4-byte magic every forge snapshot container leads with.
pub(crate) const FORGE_SNAPSHOT_MAGIC: &[u8; 4] = b"FGv1";

/// parse the pending file: `FGP1 ++ u32(repo_count) ++ (name, catch-up map)*`.
/// the bytes are untrusted (a tampered file), so every field is bounds-checked.
fn decode_pending(bytes: &[u8]) -> Result<BTreeMap<String, refs::PendingMap>, Error> {
    let body = bytes
        .strip_prefix(FORGE_PENDING_MAGIC.as_slice())
        .ok_or_else(|| {
            Error::module(
                "pending_decode",
                "forge pending file: missing the FGP1 magic",
            )
        })?;
    let mut r = codec::Reader::new(body);
    let count = r.u32()?;
    let mut out = BTreeMap::new();
    for _ in 0..count {
        let name = norm_repo(&r.str_()?)?;
        if out
            .insert(name.clone(), refs::take_pending(&mut r)?)
            .is_some()
        {
            return Err(Error::module(
                "pending_decode",
                format!("forge pending file: duplicate repo {name}"),
            ));
        }
    }
    if !r.done() {
        return Err(Error::module(
            "pending_decode",
            "forge pending file: trailing bytes after the map",
        ));
    }
    Ok(out)
}

/// the per-repo catch-up map on disk, decoded WITHOUT opening the module — an
/// absent file is an empty map. [`pending_digests`], [`compact_repos`], and
/// [`Forge::init`]'s re-adopt all read the file through here.
fn read_pending(base: &std::path::Path) -> Result<BTreeMap<String, refs::PendingMap>, Error> {
    let bytes = match std::fs::read(base.join(PENDING_FILE)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => {
            return Err(Error::module(
                "pending_file_read",
                format!("forge: read pending file: {e}"),
            ));
        }
    };
    decode_pending(&bytes)
}

/// packs one repo may accumulate before [`compact_repos`] collapses them —
/// git's own `gc.autoPackLimit` default. the ceiling is load-bearing on THIS
/// substrate: libgit2 ships no gc, so nothing collapses packs on its own (see
/// [`git::compact`] for the measured cost of letting them pile up).
pub const COMPACT_PACK_LIMIT: usize = 50;

/// the advisory stuck list as `repo -> full refnames`. an absent or unreadable
/// file is an empty map — the hint is never authority.
fn read_stuck(base: &std::path::Path) -> BTreeMap<String, BTreeSet<String>> {
    let Ok(text) = std::fs::read_to_string(base.join(STUCK_FILE)) else {
        return BTreeMap::new();
    };
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for line in text.lines() {
        let Some((repo, refname)) = line.split_once('\t') else {
            continue;
        };
        out.entry(repo.to_string())
            .or_default()
            .insert(refname.to_string());
    }
    out
}

/// collapse the packfiles every repo under `base` has accumulated, and return
/// how many packs that reclaimed. the node's maintenance handle, with the same
/// out-of-band standing as [`pending_digests`]: it never opens the module,
/// never moves a ref, and can never reach a root.
///
/// a repo whose branches are still WAITING on their objects is skipped — its
/// on-disk refs run behind the committed heads, so the closure kept here is
/// not the closure the repo is about to need. it compacts on a later tick,
/// once the node's blob sweep has caught it up.
///
/// a STUCK branch (its pack is installed and the ref move was still refused —
/// a push naming a head no pack closes) is not waiting for anything, and
/// letting one disable a repo's compaction forever is how a single bad push
/// costs that repo 1001 packs. those branches do not hold compaction back.
pub fn compact_repos(base: &std::path::Path, min_packs: usize) -> Result<usize, Error> {
    let pending = read_pending(base)?;
    let stuck = read_stuck(base);
    let entries = match std::fs::read_dir(base) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => {
            return Err(Error::module(
                "repo_base_read",
                format!("forge: read repo base: {e}"),
            ));
        }
    };
    let mut reclaimed = 0;
    for entry in entries {
        let dir = entry
            .map_err(|e| Error::module("repo_base_read", format!("forge: read repo base: {e}")))?
            .path();
        let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let is_repo = dir.join(".git").exists();
        let no_ref_stuck = BTreeSet::new();
        let stuck_here = stuck.get(name).unwrap_or(&no_ref_stuck);
        let waiting = pending
            .get(name)
            .is_some_and(|refs| refs.keys().any(|r| !stuck_here.contains(&r.full())));
        if !is_repo || waiting {
            continue;
        }
        let repo = git::open(&dir).map_err(|e| {
            Error::module("git_open_repo", format!("forge: open repo {name:?}: {e}"))
        })?;
        let packs = git::compact(&repo, min_packs).map_err(|e| {
            Error::module("git_compact", format!("forge: compact repo {name:?}: {e}"))
        })?;
        if packs == 0 {
            continue;
        }
        tracing::info!(
            target: "ducktape::forge",
            repo = %name,
            packs,
            "compacted a repo's packfiles into one"
        );
        reclaimed += packs;
    }
    Ok(reclaimed)
}

/// build the pack a peer needs to reach `head` in `repo`, bounded by the
/// `bases` it says it already holds — the SERVE half of the object catch-up
/// lane, and the reason a head stays recoverable after the pack that pushed
/// it is gone from every store.
///
/// `None` when this node cannot answer: no such repo here, or `head` is not
/// one of the branch or tag tips it holds. that guard is the whole
/// anti-amplifier: a peer can only make this node pack history it has itself
/// materialized, never an arbitrary walk of its object database.
pub fn build_objects(
    base: &std::path::Path,
    repo: &str,
    head: Oid,
    bases: &[Oid],
) -> Result<Option<Vec<u8>>, Error> {
    let name = norm_repo(repo)?;
    let dir = base.join(&name);
    if !dir.join(".git").exists() {
        return Ok(None);
    }
    let repo = git::open(&dir)
        .map_err(|e| Error::module("git_open_repo", format!("forge: open {name:?}: {e}")))?;
    let want: git2::Oid = head.into();
    let serves_head = git::ref_tips(&repo)
        .map_err(|e| {
            Error::module(
                "git_list_branches",
                format!("forge: read refs of {name:?}: {e}"),
            )
        })?
        .contains(&want);
    if !serves_head {
        return Ok(None);
    }
    // a base this node never saw cannot bound the walk (and must not error) —
    // the same filter the git fetch lane applies to a client's haves.
    let known: Vec<git2::Oid> = bases
        .iter()
        .map(|base| git2::Oid::from(*base))
        .filter(|base| repo.find_commit(*base).is_ok())
        .collect();
    let pack = match known.is_empty() {
        true => git::pack_closure_many(&repo, &[want]),
        false => git::pack_delta(&repo, &[want], &known),
    };
    pack.map(Some)
        .map_err(|e| Error::module("git_pack_closure", format!("forge: pack {name:?}: {e}")))
}

/// this node's own branch heads for `repo` — what a catch-up request sends as
/// its bases, so the answer carries only what actually moved. a repo nothing
/// has materialized here yet simply has none.
pub fn on_disk_heads(base: &std::path::Path, repo: &str) -> Result<Vec<Oid>, Error> {
    let name = norm_repo(repo)?;
    let dir = base.join(&name);
    if !dir.join(".git").exists() {
        return Ok(Vec::new());
    }
    let repo = git::open(&dir)
        .map_err(|e| Error::module("git_open_repo", format!("forge: open {name:?}: {e}")))?;
    Ok(git::list_branches(&repo)
        .map_err(|e| {
            Error::module(
                "git_list_branches",
                format!("forge: read refs of {name:?}: {e}"),
            )
        })?
        .into_iter()
        .map(|(_, oid)| Oid::from(oid))
        .collect())
}

/// install objects a peer built, and prove they close `head` — the RECEIVE
/// half of the same lane.
///
/// no trust attaches to which peer answered: indexing re-hashes every object
/// in the pack, and the closure check pins the result to an oid consensus has
/// already committed. this NEVER moves a ref — forge's own `materialize` does
/// that at its next block boundary, once the closure is there.
pub fn install_objects(
    base: &std::path::Path,
    repo: &str,
    head: Oid,
    pack: &[u8],
) -> Result<(), Error> {
    let name = norm_repo(repo)?;
    let repo = refs::open_or_init_repo(base, &name)?;
    git::install_pack(&repo, pack).map_err(|e| {
        Error::module(
            "git_install_pack",
            format!("forge: install objects for {name:?}: {e}"),
        )
    })?;
    git::verify_closure(&repo, head.into()).map_err(|e| {
        Error::module(
            "git_verify_closure",
            format!("forge: objects do not close {head}: {e}"),
        )
    })
}

/// one ref — a branch or a tag — a forge workspace is still waiting on.
///
/// the digest is the pack the push named — exact, and the cheap route while
/// some node still holds those bytes. the head is what makes the ref
/// recoverable WITHOUT them: any peer that materialized it can rebuild the
/// objects, and the requester verifies the result against this very oid,
/// which consensus already committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingBranch {
    pub repo: String,
    /// the FULL refname (`refs/heads/main`, `refs/tags/v1`).
    pub refname: String,
    pub head: Oid,
    pub digest: [u8; 32],
}

/// every ref a forge workspace is still waiting on, read from
/// [`PENDING_FILE`] WITHOUT opening the module.
///
/// this is the node's pull handle. the catch-up map is node-local possession,
/// never consensus state, so it deliberately does NOT ride the deterministic
/// `Module` surface — a block that could read it would fork. the node's blob
/// plane sweeps this out of band, fetches the objects, and forge picks them up
/// on its next `commit_block`; nothing here mutates.
///
/// a workspace with nothing outstanding has no file, which is `Ok(vec![])`.
pub fn pending_branches(base: &std::path::Path) -> Result<Vec<PendingBranch>, Error> {
    Ok(read_pending(base)?
        .into_iter()
        .flat_map(|(repo, pending)| {
            pending
                .into_iter()
                .map(move |(name, (head, digest))| PendingBranch {
                    repo: repo.clone(),
                    refname: name.full(),
                    head,
                    digest,
                })
        })
        .collect())
}

pub struct Forge {
    id: ModuleId,
    /// node-local container dir — NOT consensus state (the path may differ per
    /// node); only the committed state under it is. each named repo lives at
    /// `base/<name>` and is opened per-call.
    pub(crate) base: PathBuf,
    /// the node-local body store push packfiles are fetched from by digest —
    /// the SAME plane the files module serves. NEVER read by
    /// `root()`/`execute`/`commit_block`'s accept path.
    pub(crate) blobs: blobstore::BlobHandle,
    /// the consensus core: the repo namespace (seeded at construction from the
    /// on-disk repos + the pending file, grown lazily on first write), the
    /// COMMITTED tracker (persisted to [`TRACKER_FILE`]), and the block-scratch
    /// tracker.
    pub(crate) state: ForgeState,
    /// where issue/PR discussion-channel follow-ups go (`emit_msg` target).
    /// `None` (tests / minimal deployments without chat) emits nothing.
    chat_target: Option<String>,
    attribution_target: Option<String>,
    /// this network's chain id — the wasm tenant's genesis-config twin
    /// (`crate::guest`'s `shape().config`, #1773). empty for a native `Forge`
    /// that never carries a push certificate (every unit test but the
    /// pushcert ones); `with_chain_id` sets the real one. a certificate whose
    /// nonce disagrees with it is refused in `push_principal`.
    chain_id: String,
    /// the expensive per-repo pack payloads used to assemble snapshots.
    ///
    /// `snapshot()` packs the object closure of every branch head, and the
    /// node checkpoints by calling it every `checkpoint_blocks` blocks. On the
    /// demo workspace that measured **60.2 s of a 60.5 s capture — every one of
    /// the other 19 modules was 0 ms** — and the capture runs on the validator's
    /// select loop, so for those 60 s no other arm of that loop was polled:
    /// `/v1/query` went unserviced and even SIGTERM waited (issue #1018). An
    /// idle forge was re-packing a byte-identical 61 MB repo every 32 blocks.
    ///
    /// Each pack is keyed only by that repo's committed refs + pending map.
    /// Tracker-only writes then reserialize the cheap tracker tail around the
    /// resident packs instead of re-packing every Git object. Objects behind an
    /// unchanged head oid cannot change because Git is content-addressed; the
    /// one case where they arrive later is a missing closure, which is exactly
    /// what `pending` records in the key.
    ///
    /// ponytail: holds one pack per born repo resident (61 MB total here). Swap
    /// for memory-mapped pack slices if a node's repos outgrow its memory.
    pub(crate) snapshot_cache: std::cell::RefCell<Option<SnapshotCache>>,
}

/// [`Forge::snapshot`]'s expensive per-repo payload memo.
#[derive(Default)]
pub(crate) struct SnapshotCache {
    pub(crate) packs: BTreeMap<String, CachedPack>,
    /// Keys known to be present in the atomically-published cache file. This
    /// advances only after a successful rename, so a failed rewrite remains
    /// dirty and the next checkpoint retries it.
    pub(crate) persisted_keys: Option<Vec<(String, [u8; 32])>>,
}

pub(crate) struct CachedPack {
    /// sha256 of this repo's committed refs + node-local pending map.
    pub(crate) key: [u8; 32],
    pub(crate) bytes: Vec<u8>,
}

impl Forge {
    /// genesis wiring with a private, default (empty) blob store — useful for
    /// pack-less determinism tests and tracker-only deployments.
    pub fn init(id: impl Into<ModuleId>, base_dir: impl Into<PathBuf>) -> Result<Self, Error> {
        Self::with_blobs(id, base_dir, blobstore::BlobHandle::default())
    }

    /// genesis wiring over an EXISTING node-local blob store. `base_dir` is the
    /// CONTAINER for the repo namespace; construction creates it and RE-ADOPTS
    /// every existing per-repo git dir (seeding each repo's committed branches
    /// from its on-disk refs) and the persisted tracker file — so `root()` is
    /// correct immediately after a restart. a fresh container starts at
    /// [`StateRoot::ZERO`].
    pub fn with_blobs(
        id: impl Into<ModuleId>,
        base_dir: impl Into<PathBuf>,
        blobs: blobstore::BlobHandle,
    ) -> Result<Self, Error> {
        let base = base_dir.into();
        std::fs::create_dir_all(&base).map_err(|e| {
            Error::module("base_dir_create", format!("forge: create base dir: {e}"))
        })?;

        let mut repos = BTreeMap::new();
        for entry in std::fs::read_dir(&base)
            .map_err(|e| Error::module("base_dir_read", format!("forge: scan base dir: {e}")))?
        {
            let entry = entry.map_err(|e| Error::module("base_dir_read", e.to_string()))?;
            if !entry
                .file_type()
                .map_err(|e| Error::module("base_dir_read", e.to_string()))?
                .is_dir()
            {
                continue;
            }
            // a subdir that is not a valid repo slug, or not a git repo, cannot
            // be one this module created — ignore it.
            let Some(dir_name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(name) = norm_repo(&dir_name) else {
                continue;
            };
            let dir = base.join(&name);
            if !dir.join(".git").exists() {
                continue;
            }
            let repo =
                git::open(&dir).map_err(|e| Error::module("git_open_repo", e.to_string()))?;
            let adopt = |listed: Vec<(String, git2::Oid)>| -> BTreeMap<String, Oid> {
                listed
                    .into_iter()
                    .map(|(short, oid)| (short, Oid::from(oid)))
                    .collect()
            };
            let committed = refs::RepoRefs {
                branches: adopt(
                    git::list_branches(&repo)
                        .map_err(|e| Error::module("git_list_branches", e.to_string()))?,
                ),
                tags: adopt(
                    git::list_tags(&repo)
                        .map_err(|e| Error::module("git_list_tags", e.to_string()))?,
                ),
            };
            repos.insert(name, RepoState::with_committed(committed));
        }

        // re-adopt the catch-up map BEFORE the tracker: it carries the refs
        // whose committed head runs ahead of the ref cache the loop above just
        // read, so it is the authority wherever the two disagree. a corrupt
        // file is FAIL-STOP for the same reason the tracker is — booting on a
        // rewound branch map composes a wrong root.
        for (name, pending) in read_pending(&base)? {
            repos.entry(name).or_default().adopt_pending(pending);
        }

        // re-adopt the persisted tracker. a corrupt file is FAIL-STOP (like a
        // corrupt repo): booting with a silently-empty tracker would compose a
        // wrong root and fork this node at its first root-hash check anyway.
        let tracker_path = base.join(TRACKER_FILE);
        let tracker = if tracker_path.exists() {
            let bytes = std::fs::read(&tracker_path).map_err(|e| {
                Error::module(
                    "tracker_file_read",
                    format!("forge: read tracker file: {e}"),
                )
            })?;
            Tracker::decode(&bytes)?
        } else {
            Tracker::default()
        };

        let forge = Self {
            id: id.into(),
            base,
            blobs,
            state: ForgeState {
                repos,
                tracker,
                staged_tracker: None,
            },
            chat_target: None,
            attribution_target: None,
            chain_id: String::new(),
            snapshot_cache: std::cell::RefCell::new(None),
        };
        let restored_cache = forge.restore_snapshot_cache();
        *forge.snapshot_cache.borrow_mut() = restored_cache;
        Ok(forge)
    }

    /// route issue/PR discussion follow-ups at the given chat module. the node
    /// binaries wire `"chat"`; without it forge stays fully functional but
    /// opens no discussion channels.
    pub fn with_chat(mut self, target: impl Into<String>) -> Self {
        self.chat_target = Some(target.into());
        self
    }

    /// publish source relationships in the same unit as the accepted mutation.
    pub fn with_attribution(mut self, target: impl Into<String>) -> Self {
        self.attribution_target = Some(target.into());
        self
    }

    /// this network's chain id — the genesis-config parameter a push
    /// certificate's nonce is checked against (`pushcert::signer`).
    pub fn with_chain_id(mut self, chain_id: impl Into<String>) -> Self {
        self.chain_id = chain_id.into();
        self
    }

    /// node-local catch-up across ALL repos (see [`refs::RepoState::materialize`]).
    pub fn materialize(&mut self) -> Result<(), Error> {
        let base = &self.base;
        let blobs = &self.blobs;
        for (name, state) in self.state.repos.iter_mut() {
            state.materialize(base, name, blobs)?;
        }
        self.persist_pending()
    }

    /// atomically persist the COMMITTED tracker to [`TRACKER_FILE`].
    pub(crate) fn persist_tracker(&self) -> Result<(), Error> {
        let path = self.base.join(TRACKER_FILE);
        let tmp = self.base.join(".tracker.bin.tmp");
        std::fs::write(&tmp, self.state.tracker.canonical_bytes()).map_err(|e| {
            Error::module(
                "tracker_file_write",
                format!("forge: write tracker file: {e}"),
            )
        })?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            Error::module(
                "tracker_file_write",
                format!("forge: publish tracker file: {e}"),
            )
        })?;
        Ok(())
    }

    /// atomically publish the advisory [`STUCK_FILE`], or remove it once no
    /// ref is stuck. read only by [`compact_repos`].
    fn persist_stuck(&self) -> Result<(), Error> {
        let path = self.base.join(STUCK_FILE);
        let mut out = String::new();
        for (name, state) in &self.state.repos {
            for stuck in state.stuck_refs() {
                out.push_str(name);
                out.push('\t');
                out.push_str(&stuck.full());
                out.push('\n');
            }
        }
        if out.is_empty() {
            return match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(Error::module(
                    "stuck_file_write",
                    format!("forge: clear stuck file: {e}"),
                )),
            };
        }
        let tmp = self.base.join(".stuck.txt.tmp");
        std::fs::write(&tmp, &out).map_err(|e| {
            Error::module("stuck_file_write", format!("forge: write stuck file: {e}"))
        })?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            Error::module(
                "stuck_file_write",
                format!("forge: publish stuck file: {e}"),
            )
        })?;
        Ok(())
    }

    /// atomically persist the per-repo catch-up map to [`PENDING_FILE`], or
    /// remove the file once every branch has caught up — a stale file would
    /// re-adopt heads the ref cache has since overtaken.
    pub(crate) fn persist_pending(&self) -> Result<(), Error> {
        self.persist_stuck()?;
        let path = self.base.join(PENDING_FILE);
        let outstanding: Vec<(&str, &refs::PendingMap)> = self
            .state
            .repos
            .iter()
            .map(|(name, state)| (name.as_str(), state.pending()))
            .filter(|(_, pending)| !pending.is_empty())
            .collect();
        if outstanding.is_empty() {
            return match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(Error::module(
                    "pending_file_write",
                    format!("forge: clear pending file: {e}"),
                )),
            };
        }
        let mut out = FORGE_PENDING_MAGIC.to_vec();
        codec::put_u32(&mut out, outstanding.len() as u32);
        for (name, pending) in outstanding {
            codec::put_str(&mut out, name);
            refs::put_pending(&mut out, pending);
        }
        let tmp = self.base.join(".pending.bin.tmp");
        std::fs::write(&tmp, &out).map_err(|e| {
            Error::module(
                "pending_file_write",
                format!("forge: write pending file: {e}"),
            )
        })?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            Error::module(
                "pending_file_write",
                format!("forge: publish pending file: {e}"),
            )
        })?;
        Ok(())
    }

    /// this repo's COMMITTED `main` head hex (the single-repo Head surface) —
    /// committed-only like every other read arm, so a mid-block sibling read
    /// answers identically on every runtime.
    #[cfg(test)]
    fn read_head(&self, name: &str) -> Option<String> {
        self.state
            .repos
            .get(name)
            .and_then(|s| s.refs.get(MAIN_BRANCH))
            .map(|oid| oid.to_string())
    }
}

#[async_trait::async_trait(?Send)]
impl Module for Forge {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    /// 2: the root domain + snapshot magic reset to v1 tags with the
    /// no-versioning sweep — same layout, different preimage bytes.
    /// the composed state root — pure, no IO. see the composition invariant.
    fn root(&self) -> StateRoot {
        self.state.root()
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        Ok(StateSyncHandle::SnapshotBytes(self.snapshot()?))
    }

    /// forge's committed refs, packs and tracker land on its OWN disk at every
    /// block boundary (`publish_block`) and reopen at that tip, so it belongs to
    /// recovery's per-block-durable cohort — even though its sync surface is one
    /// self-contained container rather than a resolver lane. the default
    /// `block_durable` reads the sync handle and would answer `false`, leaving
    /// forge unplaceable at any height but its last change.
    fn block_durable(&self) -> bool {
        true
    }

    /// apply one write op through the shared consensus core. Git writes stage
    /// pure per-branch CAS updates; tracker ops mutate the block-scratch
    /// tracker and emit chat follow-ups that commit atomically with the block.
    /// execute never opens a Git repo.
    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        self.state
            .apply(
                ctx,
                &msg.payload,
                self.chat_target.as_deref(),
                self.attribution_target.as_deref(),
                &self.chain_id,
            )
            .await
    }

    /// Read committed projections. Metadata comes from the resident maps;
    /// Tree/Blob perform bounded reads against the node-local object database.
    /// No query fetches or mutates Git state.
    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        self.query_committed(req)
    }

    /// publish everything staged: packed head publications + materialization
    /// targets and deletes, then the block-scratch tracker (persisted to disk).
    async fn commit_block(&mut self) -> Result<(), Error> {
        self.publish_block()
    }

    /// discard everything staged — no ref moved, tracker unchanged, `root()`
    /// unchanged.
    async fn abort_block(&mut self) -> Result<(), Error> {
        self.state.abort();
        Ok(())
    }
}

impl Forge {
    /// Native tests use the same product policy as the deployed guest.
    pub(crate) fn query_committed(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        let image = crate::state::decode_image(&self.state.committed_image())?;
        crate::query::Reader {
            image: &image,
            git: crate::query::NativeGit(&self.base),
        }
        .query(req)
    }

    /// the bounded object read the product policy makes, exposed because the
    /// host-side substrate (`forge-odb`) answers the kernel's import with it.
    pub fn git_object_read(
        &self,
        repository: &str,
        oid: &[u8],
        max_bytes: u64,
    ) -> Result<git_primitives::GitObject, Error> {
        crate::query::read_object(&self.base, repository, oid, max_bytes)
    }
    pub fn git_diff_read(
        &self,
        repository: &str,
        target: &[u8],
        source: &[u8],
        path: Option<&str>,
        budget: git_primitives::GitDiffBudget,
    ) -> Result<git_primitives::GitDiff, git_primitives::GitDiffError> {
        crate::query::read_diff(&self.base, repository, target, source, path, budget)
    }

    /// publish everything staged: packed head publications + materialization
    /// targets and deletes, then the block-scratch tracker (persisted to disk).
    /// the block-boundary half shared by `Module::commit_block` and the wasm
    /// tenant's host-side backing (which stages the block's fates onto the
    /// core first, then publishes through here).
    pub(crate) fn publish_block(&mut self) -> Result<(), Error> {
        let base = &self.base;
        let blobs = &self.blobs;
        for (name, state) in self.state.repos.iter_mut() {
            state.publish(base, name, blobs)?;
        }
        // publish both grows the catch-up map (a head whose pack has not
        // arrived) and drains it (materialize caught one up) — either way the
        // durable copy must land in the SAME commit as the heads it describes.
        self.persist_pending()?;
        if self.state.commit_tracker() {
            self.persist_tracker()?;
        }
        Ok(())
    }

    /// adopt a block's final refs image on behalf of the wasm tenant: rebuild
    /// the per-branch fates from the image plus the packed-head records the
    /// guest staged, put them on the core, and run the SAME publish
    /// `Module::commit_block` runs.
    ///
    /// it lives here rather than in the substrate for the reason files' does:
    /// the durability ordering is the load-bearing part, and one copy of it is
    /// what makes a wasm tenant's disk and root byte-identical to native
    /// forge's. the substrate owns only the shape the kernel drives — buffering
    /// the records and splitting the flush from the adopt.
    pub fn adopt_refs(&mut self, bytes: &[u8], targets: Vec<RefTarget>) -> Result<(), Error> {
        let image = crate::state::decode_image(bytes)?;
        let fates = self.state.fates_for_image(&image, targets)?;
        for (name, staged) in fates {
            self.state.repos.entry(name).or_default().staged = staged;
        }
        self.state.staged_tracker = Some(image.tracker);
        self.publish_block()
    }

    /// the committed refs image — the `root()` preimage and the snapshot bytes.
    pub fn committed_image(&self) -> Vec<u8> {
        self.state.committed_image()
    }

    /// drop everything staged this block: the sync twin of
    /// `Module::abort_block`, which is what the kernel's discard hook needs.
    pub fn abort_staged(&mut self) {
        self.state.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_contract as chat;
    use crate::{decode_reply, encode_msg, encode_query};

    use sdk_testkit::TestCtx;

    /// a branch's full refname, as the on-disk repo stores it.
    fn heads_ref(branch: &str) -> String {
        refs::RefName::Branch(branch.into()).full()
    }

    // forge's execute reads only env (consensus_time / origin) and CAPTURES
    // emitted follow-ups; the shared TestCtx captures them (read via `msgs()`).
    // no `identity` handler is registered, so a principal resolves to the
    // origin key itself — the identity-less host path.
    fn ctx_at(consensus_time: u64) -> TestCtx {
        ctx_with_origin(consensus_time, user_origin(1))
    }
    fn ctx_with_origin(consensus_time: u64, origin: sdk::Origin) -> TestCtx {
        TestCtx::with_env(sdk::Env {
            height: 0,
            consensus_time,
            origin,
            me: "forge".into(),
            cause: sdk::Cause::Direct,
        })
    }

    fn tmp_base(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("ducktape-forge-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn exec(forge: &mut Forge, ctx: &mut TestCtx, m: &ForgeMsg) -> Result<(), Error> {
        let msg = Msg {
            target: "forge".into(),
            payload: encode_msg(m),
        };
        futures::executor::block_on(forge.execute(ctx, &msg))
    }

    fn exec_commit(forge: &mut Forge, ctx: &mut TestCtx, m: &ForgeMsg) {
        exec(forge, ctx, m).unwrap();
        futures::executor::block_on(forge.commit_block()).unwrap();
    }

    fn query_reply(forge: &Forge, query: ForgeQuery) -> Result<ForgeReply, Error> {
        let bytes = futures::executor::block_on(forge.query(&encode_query(&query)))?;
        decode_reply(&bytes).map_err(|e| Error::module("codec", e))
    }

    /// Test-fixture plumbing only: put real objects and a ref directly on disk
    /// so diff/snapshot tests can exercise libgit2 without restoring a
    /// consensus commit-building path.
    fn seed_materialized_commit(
        forge: &mut Forge,
        t: u64,
        repo: &str,
        path: &str,
        content: &str,
        message: &str,
    ) {
        let name = norm_repo(repo).unwrap();
        forge.state.repos.entry(name.clone()).or_default();
        let git_repo = refs::open_or_init_repo(&forge.base, &name).unwrap();
        let state = forge.state.repos.get_mut(&name).unwrap();
        let parent = state
            .refs
            .get(MAIN_BRANCH)
            .copied()
            .map(|oid| git_repo.find_commit(oid.into()).unwrap());
        let base_tree = parent.as_ref().map(|commit| commit.tree().unwrap());
        let blob = git_repo.blob(content.as_bytes()).unwrap();
        let tree_oid = git::build_tree(&git_repo, base_tree.as_ref(), path, blob).unwrap();
        let tree = git_repo.find_tree(tree_oid).unwrap();
        let oid = git::commit(&git_repo, &tree, parent.as_ref(), message, t).unwrap();
        git::update_ref(&git_repo, &heads_ref(MAIN_BRANCH), oid).unwrap();
        state.refs.insert(MAIN_BRANCH.to_string(), oid.into());
    }

    fn push_head(forge: &mut Forge, repo: &str, prev: Option<Oid>, new: Oid) {
        let mut ctx = ctx_at(0);
        exec_commit(
            forge,
            &mut ctx,
            &ForgeMsg::PushRefs {
                repo: repo.into(),
                updates: vec![RefUpdate {
                    ref_name: MAIN_BRANCH.into(),
                    prev_oid: prev.map(|oid| oid.as_bytes().to_vec()),
                    new_oid: Some(new.as_bytes().to_vec()),
                }],
                pack_digest: Some(vec![7u8; 32]),
                tags: Vec::new(),
                cert: None,
            },
        );
    }

    // read a repo's main oid via git2 directly — the independent oracle that
    // root() tracks the real refs, not just the cache.
    fn git_head_oid(base: &std::path::Path, repo: &str) -> Oid {
        git2::Repository::open(base.join(repo))
            .unwrap()
            .refname_to_id("refs/heads/main")
            .unwrap()
            .into()
    }

    fn oid(hexc: char) -> Oid {
        Oid::from_hex(&hexc.to_string().repeat(40)).unwrap()
    }

    fn materialized_pr(base: &std::path::Path, content: &[u8]) -> (Forge, Oid, Oid) {
        let mut forge = Forge::init("forge", base.to_path_buf()).unwrap();
        seed_materialized_commit(&mut forge, 1, "demo", "base.txt", "base\n", "base");
        let repo = git::open(&base.join("demo")).unwrap();
        let target = git_head_oid(base, "demo");
        let target_commit = repo.find_commit(target.into()).unwrap();
        let target_tree = target_commit.tree().unwrap();
        let blob = repo.blob(content).unwrap();
        let source_tree_oid =
            git::build_tree(&repo, Some(&target_tree), "feature.txt", blob).unwrap();
        let source_tree = repo.find_tree(source_tree_oid).unwrap();
        let source = git::commit(&repo, &source_tree, Some(&target_commit), "feature", 2).unwrap();
        git::update_ref(&repo, "refs/heads/dev", target.into()).unwrap();
        git::update_ref(&repo, "refs/heads/feature", source).unwrap();
        let state = forge.state.repos.get_mut("demo").unwrap();
        state.refs.insert("dev".into(), target);
        state.refs.insert("feature".into(), source.into());

        let mut ctx = ctx_with_origin(3, user_origin(1));
        exec_commit(
            &mut forge,
            &mut ctx,
            &ForgeMsg::OpenPr {
                repo: "demo".into(),
                title: "review me".into(),
                body: String::new(),
                source_branch: "feature".into(),
                target_branch: "dev".into(),
            },
        );
        (forge, source.into(), target)
    }

    fn replace_pr_source_with_files(
        forge: &mut Forge,
        base: &std::path::Path,
        target: Oid,
        files: usize,
        content: &[u8],
    ) -> Oid {
        let repo = git::open(&base.join("demo")).unwrap();
        let target_commit = repo.find_commit(target.into()).unwrap();
        let mut tree_oid = target_commit.tree_id();
        let blob = repo.blob(content).unwrap();
        for index in 0..files {
            let tree = repo.find_tree(tree_oid).unwrap();
            tree_oid =
                git::build_tree(&repo, Some(&tree), &format!("feature-{index:04}.txt"), blob)
                    .unwrap();
        }
        let tree = repo.find_tree(tree_oid).unwrap();
        let source = git::commit(&repo, &tree, Some(&target_commit), "feature", 2).unwrap();
        git::update_ref(&repo, "refs/heads/feature", source).unwrap();
        forge
            .state
            .repos
            .get_mut("demo")
            .unwrap()
            .refs
            .insert("feature".into(), source.into());
        source.into()
    }

    #[test]
    fn pr_diff_pins_oids_and_returns_a_reviewable_patch() {
        let base = tmp_base("pr-diff");
        let (forge, source, target) = materialized_pr(&base, b"reviewable\n");
        let bytes = futures::executor::block_on(forge.query(&encode_query(&ForgeQuery::PrDiff {
            repo: "demo".into(),
            number: 1,
        })))
        .unwrap();
        let ForgeReply::PrDiff(diff) = decode_reply(&bytes).unwrap() else {
            panic!("wrong reply")
        };
        assert_eq!(diff.source_oid, source.to_string());
        assert_eq!(diff.target_oid, target.to_string());
        assert_eq!(diff.files_changed, 1);
        assert_eq!(diff.additions, 1);
        assert_eq!(diff.deletions, 0);
        assert!(diff.patch.contains("+++ b/feature.txt"), "{}", diff.patch);
        assert!(diff.patch.contains("+reviewable"), "{}", diff.patch);
        assert!(!diff.truncated);
        assert!(diff.patch.len() <= MAX_PR_DIFF_BYTES);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pr_diff_caps_the_patch_and_reports_truncation() {
        let base = tmp_base("pr-diff-cap");
        let content = vec![b'x'; MAX_PR_DIFF_BYTES + 4096];
        let (forge, _, _) = materialized_pr(&base, &content);
        let bytes = futures::executor::block_on(forge.query(&encode_query(&ForgeQuery::PrDiff {
            repo: "demo".into(),
            number: 1,
        })))
        .unwrap();
        let ForgeReply::PrDiff(diff) = decode_reply(&bytes).unwrap() else {
            panic!("wrong reply")
        };
        assert!(diff.truncated);
        assert_eq!(diff.patch.len(), MAX_PR_DIFF_BYTES);
        assert_eq!(diff.files_changed, 1, "full stats survive truncation");
        assert_eq!(diff.additions, 1, "full stats survive truncation");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pr_diff_stops_patch_printing_at_the_response_cap() {
        let base = tmp_base("pr-diff-callback-stop");
        let (_, source, target) = materialized_pr(&base, &[b'x'; 4096]);
        let repo = git::open(&base.join("demo")).unwrap();
        let diff = git::bounded_diff(
            &repo,
            Some(target.into()),
            source.into(),
            64,
            MAX_PR_DIFF_FILES,
            MAX_PR_DIFF_BLOB_BYTES,
        )
        .expect("the deliberate libgit2 callback abort is truncation, not an error");
        assert!(diff.truncated);
        assert_eq!(diff.patch.len(), 64);
        assert_eq!(
            (diff.files_changed, diff.additions, diff.deletions),
            (1, 1, 0)
        );
        // the counts survive the clipped patch -- they come from the file's own
        // patch object, not from the text the byte ceiling cut.
        let [file] = &diff.files[..] else {
            panic!("one changed file, got {:?}", diff.files);
        };
        assert!(
            file.truncated,
            "the ceiling cut inside this file, so its row says so"
        );
        assert_eq!((file.additions, file.deletions), (Some(1), Some(0)));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pr_diff_returns_empty_when_trees_are_identical() {
        let base = tmp_base("pr-diff-identical-trees");
        let repo = git::init(&base.join("repo")).unwrap();
        let tree_oid = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let target = git::commit(&repo, &tree, None, "target", 1).unwrap();
        let parent = repo.find_commit(target).unwrap();
        let source = git::commit(&repo, &tree, Some(&parent), "source", 2).unwrap();

        let result = git::bounded_diff(
            &repo,
            Some(target),
            source,
            MAX_PR_DIFF_BYTES,
            MAX_PR_DIFF_FILES,
            MAX_PR_DIFF_BLOB_BYTES,
        )
        .unwrap();
        assert_eq!(result.patch, "");
        assert!(!result.truncated);
        assert_eq!(
            (result.files_changed, result.additions, result.deletions),
            (0, 0, 0)
        );
        assert!(result.files.is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pr_diff_rejects_an_oversized_commit_before_identical_tree_materialization() {
        let base = tmp_base("pr-diff-large-commit");
        let repo = git::init(&base.join("repo")).unwrap();
        let tree_oid = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let target = git::commit(&repo, &tree, None, "target", 1).unwrap();
        let mut raw = format!(
            "tree {tree_oid}\nparent {target}\nauthor agent <agent@agents.duck> 2 +0000\ncommitter node <node@nodes.duck> 2 +0000\n\n"
        )
        .into_bytes();
        raw.resize(raw.len() + MAX_PR_DIFF_COMMIT_BYTES + 1, b'x');
        let source = repo
            .odb()
            .unwrap()
            .write(git2::ObjectType::Commit, &raw)
            .unwrap();

        let result = git::bounded_diff(
            &repo,
            Some(target),
            source,
            MAX_PR_DIFF_BYTES,
            MAX_PR_DIFF_FILES,
            MAX_PR_DIFF_BLOB_BYTES,
        );
        assert!(
            matches!(
                result,
                Err(git::BoundedDiffError::TooLarge { commit_bytes, .. })
                    if commit_bytes > MAX_PR_DIFF_COMMIT_BYTES
            ),
            "commit headers must bound work before find_commit: {result:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pr_diff_preflight_handles_nested_add_delete_type_and_mode_changes() {
        let base = tmp_base("pr-diff-tree-walk");
        let repo = git::init(&base.join("repo")).unwrap();
        let old_blob = repo.blob(b"old\n").unwrap();
        let new_blob = repo.blob(b"new\n").unwrap();

        let mut old_nested = repo.treebuilder(None).unwrap();
        old_nested.insert("one.txt", old_blob, 0o100644).unwrap();
        old_nested.insert("two.txt", old_blob, 0o100644).unwrap();
        let old_nested_oid = old_nested.write().unwrap();
        let mut old_root = repo.treebuilder(None).unwrap();
        old_root.insert("node", old_nested_oid, 0o040000).unwrap();
        old_root.insert("mode.txt", old_blob, 0o100644).unwrap();
        old_root.insert("removed.txt", old_blob, 0o100644).unwrap();
        let old_tree = repo.find_tree(old_root.write().unwrap()).unwrap();
        let target = git::commit(&repo, &old_tree, None, "target", 1).unwrap();
        let target_commit = repo.find_commit(target).unwrap();

        let mut new_root = repo.treebuilder(None).unwrap();
        new_root.insert("node", new_blob, 0o100644).unwrap();
        new_root.insert("mode.txt", old_blob, 0o100755).unwrap();
        new_root.insert("added.txt", new_blob, 0o100644).unwrap();
        let new_tree = repo.find_tree(new_root.write().unwrap()).unwrap();
        let source = git::commit(&repo, &new_tree, Some(&target_commit), "source", 2).unwrap();

        let diff = git::bounded_diff(
            &repo,
            Some(target),
            source,
            MAX_PR_DIFF_BYTES,
            MAX_PR_DIFF_FILES,
            MAX_PR_DIFF_BLOB_BYTES,
        )
        .unwrap();
        let patch = &diff.patch;
        assert!(!diff.truncated);
        assert_eq!(diff.files_changed, 6);
        let paths = [
            "added.txt",
            "mode.txt",
            "node",
            "node/one.txt",
            "node/two.txt",
            "removed.txt",
        ];
        for path in paths {
            assert!(patch.contains(path), "missing {path} from {patch}");
        }
        // the index carries every one of them, ordered by path, and the
        // preflight's shapes survive into it.
        assert_eq!(
            diff.files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            paths
        );
        let status_of = |path: &str| {
            diff.files
                .iter()
                .find(|file| file.path == path)
                .map(|file| file.status)
        };
        assert_eq!(
            status_of("added.txt"),
            Some(git_primitives::GitFileStatus::Added)
        );
        assert_eq!(
            status_of("removed.txt"),
            Some(git_primitives::GitFileStatus::Deleted)
        );
        assert_eq!(
            status_of("node"),
            Some(git_primitives::GitFileStatus::Added),
            "a directory replaced by a file is an add at that path"
        );
        assert!(patch.contains("old mode 100644"), "{patch}");
        assert!(patch.contains("new mode 100755"), "{patch}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pr_diff_preflight_rejects_a_wide_mostly_unchanged_tree() {
        let base = tmp_base("pr-diff-wide-tree");
        let repo = git::init(&base.join("repo")).unwrap();
        let old_blob = repo.blob(b"old\n").unwrap();
        let new_blob = repo.blob(b"new\n").unwrap();
        let entries = MAX_PR_DIFF_TREE_ENTRIES / 2 + 1;
        let mut old_builder = repo.treebuilder(None).unwrap();
        for index in 0..entries {
            old_builder
                .insert(format!("entry-{index:05}.txt"), old_blob, 0o100644)
                .unwrap();
        }
        let old_tree = repo.find_tree(old_builder.write().unwrap()).unwrap();
        let target = git::commit(&repo, &old_tree, None, "target", 1).unwrap();
        let target_commit = repo.find_commit(target).unwrap();
        let mut new_builder = repo.treebuilder(Some(&old_tree)).unwrap();
        new_builder
            .insert("entry-00000.txt", new_blob, 0o100644)
            .unwrap();
        let new_tree = repo.find_tree(new_builder.write().unwrap()).unwrap();
        let source = git::commit(&repo, &new_tree, Some(&target_commit), "source", 2).unwrap();

        let result = git::bounded_diff(
            &repo,
            Some(target),
            source,
            MAX_PR_DIFF_BYTES,
            MAX_PR_DIFF_FILES,
            MAX_PR_DIFF_BLOB_BYTES,
        );
        assert!(
            matches!(
                result,
                Err(git::BoundedDiffError::TooLarge { tree_entries, .. })
                    if tree_entries > MAX_PR_DIFF_TREE_ENTRIES
            ),
            "wide tree traversal must stop at its own work bound: {result:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pr_diff_preflight_rejects_excessive_tree_depth() {
        let base = tmp_base("pr-diff-deep-tree");
        let repo = git::init(&base.join("repo")).unwrap();
        let empty_oid = repo.treebuilder(None).unwrap().write().unwrap();
        let empty_tree = repo.find_tree(empty_oid).unwrap();
        let target = git::commit(&repo, &empty_tree, None, "target", 1).unwrap();
        let target_commit = repo.find_commit(target).unwrap();
        let blob = repo.blob(b"deep\n").unwrap();
        let mut leaf_builder = repo.treebuilder(None).unwrap();
        leaf_builder.insert("leaf.txt", blob, 0o100644).unwrap();
        let mut tree_oid = leaf_builder.write().unwrap();
        for _ in 0..=MAX_PR_DIFF_TREE_DEPTH {
            let mut builder = repo.treebuilder(None).unwrap();
            builder.insert("nested", tree_oid, 0o040000).unwrap();
            tree_oid = builder.write().unwrap();
        }
        let source_tree = repo.find_tree(tree_oid).unwrap();
        let source = git::commit(&repo, &source_tree, Some(&target_commit), "source", 2).unwrap();

        let result = git::bounded_diff(
            &repo,
            Some(target),
            source,
            MAX_PR_DIFF_BYTES,
            MAX_PR_DIFF_FILES,
            MAX_PR_DIFF_BLOB_BYTES,
        );
        assert!(
            matches!(
                result,
                Err(git::BoundedDiffError::TooLarge { tree_depth, .. })
                    if tree_depth > MAX_PR_DIFF_TREE_DEPTH
            ),
            "deep tree traversal must stop before unbounded recursion: {result:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pr_diff_preflight_rejects_a_missing_changed_blob() {
        let base = tmp_base("pr-diff-missing-blob");
        let repo = git::init(&base.join("repo")).unwrap();
        let empty_tree_oid = repo.treebuilder(None).unwrap().write().unwrap();
        let empty_tree = repo.find_tree(empty_tree_oid).unwrap();
        let target = git::commit(&repo, &empty_tree, None, "target", 1).unwrap();
        let target_commit = repo.find_commit(target).unwrap();
        let blob = repo.blob(b"will disappear\n").unwrap();
        let mut source_builder = repo.treebuilder(None).unwrap();
        source_builder
            .insert("missing.txt", blob, 0o100644)
            .unwrap();
        let source_tree = repo.find_tree(source_builder.write().unwrap()).unwrap();
        let source = git::commit(&repo, &source_tree, Some(&target_commit), "source", 2).unwrap();
        let hex = blob.to_string();
        std::fs::remove_file(repo.path().join("objects").join(&hex[..2]).join(&hex[2..])).unwrap();

        let result = git::bounded_diff(
            &repo,
            Some(target),
            source,
            MAX_PR_DIFF_BYTES,
            MAX_PR_DIFF_FILES,
            MAX_PR_DIFF_BLOB_BYTES,
        );
        assert!(
            matches!(result, Err(git::BoundedDiffError::Git(_))),
            "missing changed blob must fail as an unavailable git object"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pr_diff_rejects_too_many_changed_files_before_patch_generation() {
        let base = tmp_base("pr-diff-many-files");
        let (mut forge, _, target) = materialized_pr(&base, b"initial\n");
        let source =
            replace_pr_source_with_files(&mut forge, &base, target, MAX_PR_DIFF_FILES + 1, b"x\n");
        let err = futures::executor::block_on(forge.query(&encode_query(&ForgeQuery::PrDiff {
            repo: "demo".into(),
            number: 1,
        })))
        .unwrap_err()
        .to_string();
        assert!(err.contains("diff is too large to serve"), "{err}");
        assert!(
            err.contains(&format!("{} changed files", MAX_PR_DIFF_FILES + 1)),
            "{err}"
        );
        assert!(err.contains(&source.to_string()), "{err}");
        assert!(err.contains(&target.to_string()), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// a pull request that pairs one oversized blob with one ordinary source
    /// file -- the shape that used to take the whole reply down with it.
    fn pr_with_a_huge_blob(base: &std::path::Path) -> Forge {
        let mut forge = Forge::init("forge", base.to_path_buf()).unwrap();
        seed_materialized_commit(&mut forge, 1, "demo", "base.txt", "base\n", "base");
        let repo = git::open(&base.join("demo")).unwrap();
        let target = git_head_oid(base, "demo");
        let target_commit = repo.find_commit(target.into()).unwrap();
        let target_tree = target_commit.tree().unwrap();
        let huge = repo.blob(&vec![b'x'; MAX_PR_DIFF_BLOB_BYTES + 1]).unwrap();
        let small = repo.blob(b"one small line\n").unwrap();
        let with_huge = git::build_tree(&repo, Some(&target_tree), "huge.bin", huge).unwrap();
        let with_huge = repo.find_tree(with_huge).unwrap();
        let source_tree = git::build_tree(&repo, Some(&with_huge), "small.txt", small).unwrap();
        let source_tree = repo.find_tree(source_tree).unwrap();
        let source = git::commit(&repo, &source_tree, Some(&target_commit), "feature", 2).unwrap();
        git::update_ref(&repo, "refs/heads/dev", target.into()).unwrap();
        git::update_ref(&repo, "refs/heads/feature", source).unwrap();
        let state = forge.state.repos.get_mut("demo").unwrap();
        state.refs.insert("dev".into(), target);
        state.refs.insert("feature".into(), source.into());
        let mut ctx = ctx_with_origin(3, user_origin(1));
        exec_commit(
            &mut forge,
            &mut ctx,
            &ForgeMsg::OpenPr {
                repo: "demo".into(),
                title: "review me".into(),
                body: String::new(),
                source_branch: "feature".into(),
                target_branch: "dev".into(),
            },
        );
        forge
    }

    #[test]
    fn a_blob_past_the_budget_loses_its_hunks_and_not_the_whole_reply() {
        let base = tmp_base("pr-diff-large-blob");
        let forge = pr_with_a_huge_blob(&base);
        let bytes = futures::executor::block_on(forge.query(&encode_query(&ForgeQuery::PrDiff {
            repo: "demo".into(),
            number: 1,
        })))
        .expect("one oversized blob does not refuse the change it is part of");
        let ForgeReply::PrDiff(diff) = decode_reply(&bytes).unwrap() else {
            panic!("expected a PrDiff reply");
        };
        // the index is COMPLETE: both paths, ordered by path.
        assert_eq!(
            diff.files
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            ["huge.bin", "small.txt"]
        );
        assert_eq!(diff.files_changed, 2);

        let huge = &diff.files[0];
        assert!(huge.truncated, "the budget could not afford this file");
        assert_eq!(
            (huge.additions, huge.deletions),
            (None, None),
            "absent, not zero -- nothing counted its lines"
        );
        assert!(!diff.patch.contains("huge.bin"), "{}", diff.patch);

        let small = &diff.files[1];
        assert!(!small.truncated, "the readable file is fully here");
        assert_eq!((small.additions, small.deletions), (Some(1), Some(0)));
        assert!(diff.patch.contains("small.txt"), "{}", diff.patch);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_scoped_read_reaches_the_file_the_aggregate_budget_skipped() {
        let base = tmp_base("pr-file-diff-large-blob");
        let forge = pr_with_a_huge_blob(&base);
        let bytes =
            futures::executor::block_on(forge.query(&encode_query(&ForgeQuery::PrFileDiff {
                repo: "demo".into(),
                number: 1,
                path: "huge.bin".into(),
            })))
            .expect("a scoped read is priced by its own file");
        let ForgeReply::PrFileDiff(diff) = decode_reply(&bytes).unwrap() else {
            panic!("expected a PrFileDiff reply");
        };
        let [file] = &diff.files[..] else {
            panic!("a scoped read answers about one file, got {:?}", diff.files);
        };
        assert_eq!(file.path, "huge.bin");
        assert_eq!(file.status, FileStatus::Added);
        assert_eq!(
            file.additions,
            Some(1),
            "the file the whole-change read could not count"
        );
        assert!(diff.patch.contains("huge.bin"), "the hunks are here now");
        assert!(diff.truncated, "8 MiB does not fit the scoped byte ceiling");
        // the pinned pair travels on the scoped reply too, so a view can refuse
        // a patch that no longer matches the branch it is rendering.
        assert_eq!(diff.source_oid.len(), 40);
        assert_eq!(diff.target_oid.len(), 40);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_blob_budget_is_spent_cheapest_file_first() {
        let base = tmp_base("pr-diff-budget-order");
        let mut forge = Forge::init("forge", base.to_path_buf()).unwrap();
        seed_materialized_commit(&mut forge, 1, "demo", "base.txt", "base\n", "base");
        let repo = git::open(&base.join("demo")).unwrap();
        let target = git_head_oid(&base, "demo");
        let target_commit = repo.find_commit(target.into()).unwrap();
        // two files, each affordable alone, together over the budget. the
        // cheaper one is the one a reader gets.
        let big = repo
            .blob(&vec![b'a'; MAX_PR_DIFF_BLOB_BYTES - 1024])
            .unwrap();
        let small = repo.blob(&vec![b'b'; 2048]).unwrap();
        let tree = git::build_tree(&repo, Some(&target_commit.tree().unwrap()), "big.txt", big)
            .and_then(|oid| {
                let tree = repo.find_tree(oid)?;
                git::build_tree(&repo, Some(&tree), "small.txt", small)
            })
            .unwrap();
        let tree = repo.find_tree(tree).unwrap();
        let source = git::commit(&repo, &tree, Some(&target_commit), "two files", 2).unwrap();

        let diff = git::bounded_diff(
            &repo,
            Some(target.into()),
            source,
            MAX_PR_DIFF_BYTES,
            MAX_PR_DIFF_FILES,
            MAX_PR_DIFF_BLOB_BYTES,
        )
        .unwrap();
        let by_path = |path: &str| {
            diff.files
                .iter()
                .find(|file| file.path == path)
                .unwrap_or_else(|| panic!("{path} missing from the index"))
        };
        assert!(
            !by_path("small.txt").truncated,
            "the cheap file is the one the budget buys"
        );
        assert!(
            by_path("big.txt").truncated,
            "and the expensive one keeps its row without its hunks"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    fn history_of(forge: &Forge, path: &str, after: &str, limit: u64) -> CommitPage {
        let bytes =
            futures::executor::block_on(forge.query(&encode_query(&ForgeQuery::ListCommits {
                repo: "demo".into(),
                rev: String::new(),
                path: path.into(),
                after: after.into(),
                limit,
            })))
            .expect("a history page");
        let ForgeReply::Commits(page) = decode_reply(&bytes).unwrap() else {
            panic!("expected a Commits reply");
        };
        page
    }

    fn summaries(page: &CommitPage) -> Vec<&str> {
        page.commits.iter().map(|c| c.summary.as_str()).collect()
    }

    /// five commits on `main`, each touching its own file plus `shared.txt`
    /// every other time, so a path filter has something to filter.
    fn five_commits(base: &std::path::Path) -> Forge {
        let mut forge = Forge::init("forge", base.to_path_buf()).unwrap();
        for n in 1..=5u64 {
            let touches_shared = n % 2 == 1;
            let path = if touches_shared {
                "shared.txt".to_string()
            } else {
                format!("f{n}.txt")
            };
            seed_materialized_commit(
                &mut forge,
                n,
                "demo",
                &path,
                &format!("body {n}\n"),
                &format!("commit {n}\n\nthe body of {n}"),
            );
        }
        forge
    }

    #[test]
    fn history_pages_newest_first_and_the_cursor_resumes_where_it_stopped() {
        let base = tmp_base("history-pages");
        let forge = five_commits(&base);

        let first = history_of(&forge, "", "", 2);
        assert_eq!(summaries(&first), ["commit 5", "commit 4"]);
        assert_eq!(first.rev.len(), 40, "the page names the head it walked");

        let second = history_of(&forge, "", first.next.as_deref().unwrap(), 2);
        assert_eq!(summaries(&second), ["commit 3", "commit 2"]);

        let third = history_of(&forge, "", second.next.as_deref().unwrap(), 2);
        assert_eq!(summaries(&third), ["commit 1"]);
        assert!(
            third.next.is_none(),
            "the root commit has no parent to resume from"
        );
        // the summary is the message's FIRST LINE; the body stays behind.
        assert_eq!(third.commits[0].summary, "commit 1");
        assert!(third.commits[0].committed_at > 0);
        assert!(third.commits[0].author.contains('<'), "an identity line");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_path_filter_keeps_only_the_commits_that_moved_it() {
        let base = tmp_base("history-filter");
        let forge = five_commits(&base);
        let page = history_of(&forge, "shared.txt", "", 10);
        assert_eq!(
            summaries(&page),
            ["commit 5", "commit 3", "commit 1"],
            "commits 2 and 4 never touched shared.txt"
        );
        assert!(page.next.is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_filtered_page_resumes_at_the_commit_after_the_last_match() {
        let base = tmp_base("history-filter-pages");
        let forge = five_commits(&base);
        // commits 5, 3 and 1 touch shared.txt; the cursor has to land on the
        // commit AFTER the last match, not after the last commit scanned, or
        // the skipped non-matching commits are walked twice or lost.
        let mut seen = Vec::new();
        let mut after = String::new();
        loop {
            let page = history_of(&forge, "shared.txt", &after, 1);
            seen.extend(page.commits.iter().map(|c| c.summary.clone()));
            let Some(next) = page.next else { break };
            after = next;
        }
        assert_eq!(seen, ["commit 5", "commit 3", "commit 1"]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_cursor_off_the_walked_chain_is_refused_rather_than_read() {
        let base = tmp_base("history-bad-cursor");
        let forge = five_commits(&base);
        let err =
            futures::executor::block_on(forge.query(&encode_query(&ForgeQuery::ListCommits {
                repo: "demo".into(),
                rev: String::new(),
                path: String::new(),
                // a syntactically valid oid that is not on this history.
                after: "0".repeat(40),
                limit: 2,
            })))
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not on the first-parent chain"), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn one_commit_carries_its_whole_message_and_its_patch_against_the_parent() {
        let base = tmp_base("commit-detail");
        let forge = five_commits(&base);
        let head = history_of(&forge, "", "", 1).commits[0].oid.clone();
        let bytes = futures::executor::block_on(forge.query(&encode_query(&ForgeQuery::Commit {
            repo: "demo".into(),
            rev: head.clone(),
        })))
        .expect("the head commit");
        let ForgeReply::Commit(Some(commit)) = decode_reply(&bytes).unwrap() else {
            panic!("expected a Commit reply");
        };
        assert_eq!(commit.oid, head);
        assert_eq!(commit.message, "commit 5\n\nthe body of 5");
        assert_eq!(commit.parents.len(), 1, "a linear history");
        // the patch is pinned to (first parent, this commit), the same shape a
        // pull request answers in.
        assert_eq!(commit.diff.target_oid, commit.parents[0]);
        assert_eq!(commit.diff.source_oid, head);
        assert_eq!(
            commit
                .diff
                .files
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            ["shared.txt"]
        );
        assert!(
            commit.diff.patch.contains("body 5"),
            "{}",
            commit.diff.patch
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// the substrate hands over git's own bytes and nothing else: what a commit
    /// MEANS is decided over `raw` by `git_primitives::parse_commit`, and a read
    /// the ceiling refused answers with the size alone.
    #[test]
    fn a_commit_object_crosses_raw_and_a_refused_read_keeps_its_size() {
        let base = tmp_base("object-raw");
        let forge = five_commits(&base);
        let head = git_head_oid(&base, "demo");

        let object = forge
            .git_object_read("demo", head.as_bytes(), 256 * 1024)
            .expect("the head commit object");
        assert_eq!(object.kind, git_primitives::KIND_COMMIT);
        assert_eq!(
            object.raw.len() as u64,
            object.size,
            "a body inside the ceiling crosses whole"
        );

        let commit = git_primitives::parse_commit(&object.raw).expect("a commit body");
        assert_eq!(commit.message, "commit 5\n\nthe body of 5");
        assert_eq!(commit.author, "ducktape <ducktape@localhost>");
        // the COMMITTER's epoch seconds, which `seed_materialized_commit` set
        // to the commit's own number.
        assert_eq!(commit.committed_at, 5);
        assert_eq!(commit.parents.len(), 1, "a linear history");
        assert_eq!(commit.tree.len(), 20, "an oid is 20 stored bytes");

        // one byte short of the body: the size is still the truth, and the body
        // is simply not there.
        let refused = forge
            .git_object_read("demo", head.as_bytes(), object.size - 1)
            .expect("a refused read is an answer, not an error");
        assert_eq!(refused.kind, object.kind);
        assert_eq!(refused.size, object.size);
        assert!(refused.raw.is_empty(), "a refused body crosses empty");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A root commit has no parent to diff against, and diffing it against
    /// ITSELF answers an empty diff -- which tells a reader the first commit
    /// changed nothing, while it in fact introduced every file in the tree.
    /// It diffs against nothing instead, the way `git show` does.
    #[test]
    fn the_root_commit_introduces_its_whole_tree_rather_than_diffing_against_itself() {
        let base = tmp_base("commit-root");
        let mut forge = Forge::init("forge", base.to_path_buf()).unwrap();
        // the ROOT itself carries TWO files, so "every file is Added" is an
        // assertion rather than an accident of there being one. Seeded by hand
        // because `seed_materialized_commit` writes one path per commit, and a
        // second commit would no longer be the root.
        let name = norm_repo("demo").unwrap();
        forge.state.repos.entry(name.clone()).or_default();
        let git_repo = refs::open_or_init_repo(&forge.base, &name).unwrap();
        let alpha = git_repo.blob(b"alpha\n").unwrap();
        let beta = git_repo.blob(b"beta\n").unwrap();
        let one = git_repo
            .find_tree(git::build_tree(&git_repo, None, "a.txt", alpha).unwrap())
            .unwrap();
        let tree_oid = git::build_tree(&git_repo, Some(&one), "b.txt", beta).unwrap();
        let tree = git_repo.find_tree(tree_oid).unwrap();
        let oid = git::commit(&git_repo, &tree, None, "the first commit", 1).unwrap();
        git::update_ref(&git_repo, &heads_ref(MAIN_BRANCH), oid).unwrap();
        forge
            .state
            .repos
            .get_mut(&name)
            .unwrap()
            .refs
            .insert(MAIN_BRANCH.to_string(), oid.into());

        let root = history_of(&forge, "", "", 10)
            .commits
            .pop()
            .expect("the oldest commit on the page");
        assert!(
            root.parents.is_empty(),
            "the fixture's only commit is a root"
        );

        let bytes = futures::executor::block_on(forge.query(&encode_query(&ForgeQuery::Commit {
            repo: "demo".into(),
            rev: root.oid.clone(),
        })))
        .expect("the root commit");
        let ForgeReply::Commit(Some(commit)) = decode_reply(&bytes).unwrap() else {
            panic!("expected a Commit reply");
        };

        assert_eq!(
            commit
                .diff
                .files
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            ["a.txt", "b.txt"],
            "the root commit's whole tree, not an empty diff"
        );
        assert!(
            commit
                .diff
                .files
                .iter()
                .all(|f| f.status == FileStatus::Added),
            "against nothing, every path is an addition: {:?}",
            commit.diff.files,
        );
        assert!(commit.diff.patch.contains("alpha"), "{}", commit.diff.patch);
        assert!(commit.diff.patch.contains("beta"), "{}", commit.diff.patch);
        assert_eq!(commit.diff.files_changed, 2);
        assert_eq!(commit.diff.additions, 2);
        assert_eq!(commit.diff.deletions, 0);
        // there is no target to pin to, and an empty string is a pin no view
        // can mistake for an oid.
        assert_eq!(commit.diff.target_oid, "");
        assert_eq!(commit.diff.source_oid, root.oid);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_commit_outside_the_committed_history_is_not_reachable() {
        let base = tmp_base("commit-unreachable");
        let forge = five_commits(&base);
        let err = futures::executor::block_on(forge.query(&encode_query(&ForgeQuery::Commit {
            repo: "demo".into(),
            rev: "0".repeat(40),
        })))
        .unwrap_err()
        .to_string();
        assert!(err.contains("is not reachable from any branch"), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pr_diff_fails_honestly_when_a_pinned_object_is_unavailable() {
        let base = tmp_base("pr-diff-missing");
        let (mut forge, _, target) = materialized_pr(&base, b"present\n");
        let missing = oid('f');
        forge
            .state
            .repos
            .get_mut("demo")
            .unwrap()
            .refs
            .insert("feature".into(), missing);
        let err = futures::executor::block_on(forge.query(&encode_query(&ForgeQuery::PrDiff {
            repo: "demo".into(),
            number: 1,
        })))
        .unwrap_err()
        .to_string();
        assert!(err.contains("not fully materialized"), "{err}");
        assert!(err.contains(&missing.to_string()), "{err}");
        assert!(err.contains(&target.to_string()), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pr_diff_rejects_an_issue_before_reading_git_objects() {
        let base = tmp_base("issue-diff");
        let mut forge = Forge::init("forge", base.clone()).unwrap();
        let mut ctx = ctx_with_origin(1, user_origin(1));
        exec_commit(
            &mut forge,
            &mut ctx,
            &ForgeMsg::OpenIssue {
                repo: "demo".into(),
                title: "not a pr".into(),
                body: String::new(),
            },
        );
        let err = futures::executor::block_on(forge.query(&encode_query(&ForgeQuery::PrDiff {
            repo: "demo".into(),
            number: 1,
        })))
        .unwrap_err()
        .to_string();
        assert!(err.contains("issue, not a pull request"), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }

    fn user_key(b: u8) -> Vec<u8> {
        vec![b; 8]
    }
    fn user_origin(b: u8) -> sdk::Origin {
        sdk::Origin::External(user_key(b))
    }

    /// `git push --signed`: the certificate's SSH signer is the party the
    /// push is attributed to — not the node that bridged it. A certificate
    /// authorizes exactly the moves it lists, on the repo its nonce names, by
    /// the key it embeds.
    #[test]
    fn a_signed_push_speaks_for_its_ssh_signer() {
        use crate::pushcert;
        use keyscheme::sshsig::GIT_SSH_NS;
        use keyscheme::testkit::{ssh_key, sshsig};
        let base = tmp_base("signed-push");
        let mut forge = Forge::init("forge", base.clone())
            .unwrap()
            .with_chain_id("chain-a");
        let signed = |seed: u8, updates: Vec<RefUpdate>| {
            let cert = pushcert::certificate(&pushcert::nonce("chain-a", "lab"), &updates, &[]);
            ForgeMsg::PushRefs {
                repo: "lab".into(),
                pack_digest: Some(vec![9u8; 32]),
                tags: Vec::new(),
                cert: Some(PushCert {
                    sshsig: sshsig(&ssh_key(seed), GIT_SSH_NS, &cert),
                    cert,
                }),
                updates,
            }
        };
        let main_to = |prev: Option<char>, new: char| RefUpdate {
            ref_name: "main".into(),
            prev_oid: prev.map(|c| oid(c).as_bytes().to_vec()),
            new_oid: Some(oid(new).as_bytes().to_vec()),
        };
        let refused = |forge: &mut Forge, t: u64, msg: &ForgeMsg| -> String {
            let err = exec(forge, &mut ctx_at(t), msg).unwrap_err();
            futures::executor::block_on(forge.abort_block()).unwrap();
            format!("{err:?}")
        };
        const ALICE: u8 = 5;

        // the node bridges alice's signed pushes.
        exec_commit(
            &mut forge,
            &mut ctx_at(1),
            &signed(ALICE, vec![main_to(None, 'a')]),
        );
        exec_commit(
            &mut forge,
            &mut ctx_at(2),
            &signed(ALICE, vec![main_to(Some('a'), 'b')]),
        );

        // a certificate authorizes only the moves it lists…
        let mut borrowed = signed(ALICE, vec![main_to(Some('b'), 'c')]);
        let ForgeMsg::PushRefs { updates, .. } = &mut borrowed else {
            unreachable!()
        };
        updates[0].new_oid = Some(oid('d').as_bytes().to_vec());
        assert!(refused(&mut forge, 5, &borrowed).contains("ref updates"));
        // …on the repo its nonce names…
        let mut elsewhere = signed(ALICE, vec![main_to(Some('b'), 'c')]);
        let ForgeMsg::PushRefs { repo, .. } = &mut elsewhere else {
            unreachable!()
        };
        *repo = "other".into();
        assert!(refused(&mut forge, 6, &elsewhere).contains("nonce"));
        // …on THIS chain: a certificate minted for a different ducktape
        // network is refused, never trusted as this network's first
        // certified push (#1773 — no more "pin whichever chain arrives
        // first").
        let updates = vec![main_to(Some('b'), 'c')];
        let cert =
            pushcert::certificate(&pushcert::nonce("some-other-chain", "lab"), &updates, &[]);
        let cross_chain = ForgeMsg::PushRefs {
            repo: "lab".into(),
            pack_digest: Some(vec![9u8; 32]),
            tags: Vec::new(),
            cert: Some(PushCert {
                sshsig: sshsig(&ssh_key(ALICE), GIT_SSH_NS, &cert),
                cert,
            }),
            updates,
        };
        assert!(refused(&mut forge, 6, &cross_chain).contains("nonce"));
        // …by the key it embeds (a flipped key byte is someone else's blob).
        let mut forged = signed(ALICE, vec![main_to(Some('b'), 'c')]);
        let ForgeMsg::PushRefs {
            cert: Some(cert), ..
        } = &mut forged
        else {
            unreachable!()
        };
        // byte 40 sits inside the embedded 32-byte key (after the 6-byte
        // magic, u32 version, and the two length-prefixed `ssh-ed25519` tags).
        cert.sshsig[40] ^= 1;
        assert!(refused(&mut forge, 7, &forged).contains("does not verify"));
        // alice's second push is the last accepted move.
        exec_commit(
            &mut forge,
            &mut ctx_at(8),
            &signed(ALICE, vec![main_to(Some('b'), 'c')]),
        );
        let refused_stale = signed(ALICE, vec![main_to(Some('b'), 'd')]);
        // the CAS names its own class: the token, not a prefix on the sentence.
        assert!(refused(&mut forge, 9, &refused_stale).contains("non_fast_forward"));
    }

    // #1773: forge learns its own chain id through the genesis-config seam
    // (`crate::guest`'s `shape().config`, `noded::compose`'s
    // `odb_genesis_config`), so `pushcert::signer` checks a certificate's
    // nonce against it directly — no pinning off whichever chain the first
    // cert-verified push happens to carry.
    #[test]
    fn a_certificate_from_a_different_chain_is_refused_even_on_an_unborn_repo() {
        use crate::pushcert;
        use keyscheme::sshsig::GIT_SSH_NS;
        use keyscheme::testkit::{ssh_key, sshsig};
        let base = tmp_base("cross-chain-replay");
        let mut forge = Forge::init("forge", base.clone())
            .unwrap()
            .with_chain_id("home");
        let signed = |chain: &str, repo: &str, seed: u8, updates: Vec<RefUpdate>| {
            let cert = pushcert::certificate(&pushcert::nonce(chain, repo), &updates, &[]);
            ForgeMsg::PushRefs {
                repo: repo.into(),
                pack_digest: Some(vec![9u8; 32]),
                tags: Vec::new(),
                cert: Some(PushCert {
                    sshsig: sshsig(&ssh_key(seed), GIT_SSH_NS, &cert),
                    cert,
                }),
                updates,
            }
        };
        let birth = |c: char| RefUpdate {
            ref_name: "main".into(),
            prev_oid: None,
            new_oid: Some(oid(c).as_bytes().to_vec()),
        };

        // the network's own chain accepts a certified birth.
        exec_commit(
            &mut forge,
            &mut ctx_at(1),
            &signed("home", "demo", 1, vec![birth('a')]),
        );

        // a certificate minted on a DIFFERENT chain, harvested and replayed
        // to squat an as-yet-unborn repo name on THIS network, is refused —
        // even though its nonce names the (unborn) repo correctly and the
        // signature verifies. no "first push wins" residual gap: consensus
        // checks the chain id it was seeded with from block one.
        let err = exec(
            &mut forge,
            &mut ctx_at(2),
            &signed("away", "stolen", 2, vec![birth('b')]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("nonce"), "{err}");
        futures::executor::block_on(forge.abort_block()).unwrap();
        assert!(
            forge.read_head("stolen").is_none(),
            "the refused birth claimed nothing"
        );

        // the home chain keeps working normally, on a different repo too.
        exec_commit(
            &mut forge,
            &mut ctx_at(3),
            &signed("home", "other", 3, vec![birth('c')]),
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn successive_pushes_move_the_root() {
        let base = tmp_base("second");
        let mut forge = Forge::init("forge", base.clone()).unwrap();
        let a = oid('a');
        let b = oid('b');
        push_head(&mut forge, "", None, a);
        let r1 = forge.root();
        push_head(&mut forge, "", Some(a), b);
        let r2 = forge.root();
        assert_ne!(r1, r2, "a second pushed head must advance the root");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn root_composes_into_global_root() {
        let base = tmp_base("compose");
        let mut forge = Forge::init("forge", base.clone()).unwrap();
        let before = host::global_root(&[&forge as &dyn Module]);
        push_head(&mut forge, "", None, oid('a'));
        let after = host::global_root(&[&forge as &dyn Module]);
        assert_ne!(before, after, "forge's root must move the global root-hash");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pushed_head_root_is_reproducible_across_namespaces() {
        let a = tmp_base("det-a");
        let b = tmp_base("det-b");
        let mut fa = Forge::init("forge", a.clone()).unwrap();
        let mut fb = Forge::init("forge", b.clone()).unwrap();
        push_head(&mut fa, "myrepo", None, oid('a'));
        push_head(&mut fb, "myrepo", None, oid('a'));
        assert_eq!(fa.root(), fb.root(), "same fixed oid -> identical root");
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    // multi-branch: an atomic PushRefs births branches, CASes per branch,
    // deletes non-main branches, and refuses main deletion — all reflected in
    // root() without any pack materialized (the determinism invariant).
    /// a tag is a ref forge tracks: created beside a branch in ONE atomic push,
    /// listed by `ListTags` while `ListRefs` stays the branches, part of the
    /// root, re-adopted across a restart — and refused BY NAME for every later
    /// move, delete or re-create.
    #[test]
    fn a_tag_is_created_once_listed_apart_from_branches_and_never_moves() {
        let base = tmp_base("tags");
        let mut forge = Forge::init("forge", base.clone()).unwrap();
        let update = |name: &str, prev: Option<char>, new: Option<char>| RefUpdate {
            ref_name: name.into(),
            prev_oid: prev.map(|c| oid(c).as_bytes().to_vec()),
            new_oid: new.map(|c| oid(c).as_bytes().to_vec()),
        };
        let create = |name: &str, c: char| TagCreate {
            name: name.into(),
            oid: oid(c).as_bytes().to_vec(),
        };
        let push = |updates: Vec<RefUpdate>, tags: Vec<TagCreate>| ForgeMsg::PushRefs {
            repo: "demo".into(),
            updates,
            tags,
            pack_digest: Some(vec![7u8; 32]),
            cert: None,
        };
        let branch_only = forge.root();
        exec_commit(
            &mut forge,
            &mut ctx_at(1),
            &push(
                vec![update("main", None, Some('a'))],
                vec![create("v1", 'a')],
            ),
        );
        let tagged = forge.root();
        assert_ne!(tagged, branch_only);

        let ForgeReply::Tags(tags) = query_reply(
            &forge,
            ForgeQuery::ListTags {
                repo: "demo".into(),
            },
        )
        .unwrap() else {
            panic!("wrong reply")
        };
        assert_eq!(
            tags,
            vec![TagRef {
                name: "v1".into(),
                oid: oid('a').to_string(),
            }]
        );
        let ForgeReply::Refs(refs) = query_reply(
            &forge,
            ForgeQuery::ListRefs {
                repo: "demo".into(),
            },
        )
        .unwrap() else {
            panic!("wrong reply")
        };
        assert_eq!(
            refs.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["main"],
            "ListRefs lists the branches and nothing else"
        );
        let ForgeReply::Tags(none) = query_reply(
            &forge,
            ForgeQuery::ListTags {
                repo: "nowhere".into(),
            },
        )
        .unwrap() else {
            panic!("wrong reply")
        };
        assert!(none.is_empty(), "an unknown repo has no tags");

        // a re-create at another oid (what a move would be) and at the same one.
        for (t, c) in [(2, 'b'), (3, 'a')] {
            let err = exec(
                &mut forge,
                &mut ctx_at(t),
                &push(Vec::new(), vec![create("v1", c)]),
            )
            .unwrap_err();
            futures::executor::block_on(forge.abort_block()).unwrap();
            assert!(
                matches!(&err, Error::Module { reason, .. } if reason == sdk::refusal::ALREADY_EXISTS),
                "{c}: {err:?}"
            );
        }
        assert_eq!(forge.root(), tagged, "no refusal moved the root");

        // no pack ever arrived, so the tag is re-adopted from the pending
        // file rather than from an on-disk ref.
        drop(forge);
        let reopened = Forge::init("forge", base.clone()).unwrap();
        assert_eq!(reopened.root(), tagged);
        assert_eq!(reopened.state.repos["demo"].tags["v1"], oid('a'));
    }

    /// one certificate can sign a branch move and a tag creation together:
    /// both apply. a signed re-create of a held tag is refused by name like
    /// any other.
    #[test]
    fn a_signed_push_creates_a_tag_beside_a_branch() {
        use crate::pushcert;
        use keyscheme::sshsig::GIT_SSH_NS;
        use keyscheme::testkit::{ssh_key, sshsig};
        let mut forge = Forge::init("forge", tmp_base("signed-tag"))
            .unwrap()
            .with_chain_id("chain-a");
        let update = |name: &str, prev: Option<char>, new: char| RefUpdate {
            ref_name: name.into(),
            prev_oid: prev.map(|c| oid(c).as_bytes().to_vec()),
            new_oid: Some(oid(new).as_bytes().to_vec()),
        };
        let create = |name: &str, c: char| TagCreate {
            name: name.into(),
            oid: oid(c).as_bytes().to_vec(),
        };
        let signed = |updates: Vec<RefUpdate>, tags: Vec<TagCreate>| {
            let cert = pushcert::certificate(&pushcert::nonce("chain-a", "lab"), &updates, &tags);
            ForgeMsg::PushRefs {
                repo: "lab".into(),
                updates,
                tags,
                pack_digest: Some(vec![9u8; 32]),
                cert: Some(PushCert {
                    sshsig: sshsig(&ssh_key(5), GIT_SSH_NS, &cert),
                    cert,
                }),
            }
        };
        exec_commit(
            &mut forge,
            &mut ctx_at(1),
            &signed(vec![update("main", None, 'a')], vec![create("v1", 'a')]),
        );
        let lab = &forge.state.repos["lab"];
        assert_eq!(lab.refs["main"], oid('a'));
        assert_eq!(lab.tags["v1"], oid('a'));

        let moved = exec(
            &mut forge,
            &mut ctx_at(2),
            &signed(Vec::new(), vec![create("v1", 'b')]),
        )
        .unwrap_err();
        assert!(
            matches!(&moved, Error::Module { reason, .. } if reason == sdk::refusal::ALREADY_EXISTS),
            "{moved:?}"
        );
    }

    #[test]
    fn push_refs_multi_branch_flow() {
        let base = tmp_base("multi-branch");
        let mut forge = Forge::init("forge", base.clone()).unwrap();
        let digest = vec![7u8; 32];

        // birth main + a feature branch in ONE atomic push.
        let mut ctx = ctx_at(1);
        exec_commit(
            &mut forge,
            &mut ctx,
            &ForgeMsg::PushRefs {
                repo: "demo".into(),
                updates: vec![
                    RefUpdate {
                        ref_name: "main".into(),
                        prev_oid: None,
                        new_oid: Some(oid('a').as_bytes().to_vec()),
                    },
                    RefUpdate {
                        ref_name: "feature/x".into(),
                        prev_oid: None,
                        new_oid: Some(oid('b').as_bytes().to_vec()),
                    },
                ],
                pack_digest: Some(digest.clone()),
                tags: Vec::new(),
                cert: None,
            },
        );
        let r1 = forge.root();
        assert_ne!(r1, StateRoot::ZERO);

        // ListRefs sees both branches.
        let reply =
            futures::executor::block_on(forge.query(&encode_query(&ForgeQuery::ListRefs {
                repo: "demo".into(),
            })))
            .unwrap();
        let ForgeReply::Refs(refs) = decode_reply(&reply).unwrap() else {
            panic!("wrong reply")
        };
        assert_eq!(
            refs.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["feature/x", "main"]
        );

        // stale CAS rejects; fresh CAS force-moves the feature branch.
        let mut ctx = ctx_at(2);
        assert!(
            exec(
                &mut forge,
                &mut ctx,
                &ForgeMsg::PushRefs {
                    repo: "demo".into(),
                    updates: vec![RefUpdate {
                        ref_name: "feature/x".into(),
                        prev_oid: Some(oid('a').as_bytes().to_vec()), // stale
                        new_oid: Some(oid('c').as_bytes().to_vec()),
                    }],
                    pack_digest: Some(digest.clone()),
                    tags: Vec::new(),
                    cert: None,
                },
            )
            .is_err()
        );
        futures::executor::block_on(forge.abort_block()).unwrap();

        let mut ctx = ctx_at(3);
        exec_commit(
            &mut forge,
            &mut ctx,
            &ForgeMsg::PushRefs {
                repo: "demo".into(),
                updates: vec![RefUpdate {
                    ref_name: "feature/x".into(),
                    prev_oid: Some(oid('b').as_bytes().to_vec()),
                    new_oid: Some(oid('c').as_bytes().to_vec()), // force-ish move
                }],
                pack_digest: Some(digest.clone()),
                tags: Vec::new(),
                cert: None,
            },
        );
        assert_ne!(forge.root(), r1, "branch move must move the root");

        // deleting main is refused; deleting the feature branch works and is
        // pack-free.
        let mut ctx = ctx_at(4);
        assert!(
            exec(
                &mut forge,
                &mut ctx,
                &ForgeMsg::PushRefs {
                    repo: "demo".into(),
                    updates: vec![RefUpdate {
                        ref_name: "main".into(),
                        prev_oid: Some(oid('a').as_bytes().to_vec()),
                        new_oid: None,
                    }],
                    pack_digest: None,
                    tags: Vec::new(),
                    cert: None,
                },
            )
            .is_err()
        );
        futures::executor::block_on(forge.abort_block()).unwrap();

        let mut ctx = ctx_at(5);
        exec_commit(
            &mut forge,
            &mut ctx,
            &ForgeMsg::PushRefs {
                repo: "demo".into(),
                updates: vec![RefUpdate {
                    ref_name: "feature/x".into(),
                    prev_oid: Some(oid('c').as_bytes().to_vec()),
                    new_oid: None,
                }],
                pack_digest: None,
                tags: Vec::new(),
                cert: None,
            },
        );
        let reply =
            futures::executor::block_on(forge.query(&encode_query(&ForgeQuery::ListRefs {
                repo: "demo".into(),
            })))
            .unwrap();
        let ForgeReply::Refs(refs) = decode_reply(&reply).unwrap() else {
            panic!("wrong reply")
        };
        assert_eq!(refs.len(), 1, "only main survives");

        let _ = std::fs::remove_dir_all(&base);
    }

    // the tracker flow: open issue -> hidden channel follow-up; open PR on a
    // born branch; review; merge via double CAS — refs AND item state move
    // atomically; system lines ride the item's own discussion channel.
    #[test]
    fn tracker_issue_pr_review_merge_flow() {
        let base = tmp_base("tracker");
        let mut forge = Forge::init("forge", base.clone())
            .unwrap()
            .with_chat("chat");
        let digest = vec![9u8; 32];

        // seed a repo with release main, integration dev, and a feature branch
        // (fabricated oids — packs never gate consensus). user 2 pushes it and
        // merges onto `dev` below.
        let mut ctx = ctx_with_origin(1, user_origin(2));
        exec_commit(
            &mut forge,
            &mut ctx,
            &ForgeMsg::PushRefs {
                repo: "demo".into(),
                updates: vec![
                    RefUpdate {
                        ref_name: "main".into(),
                        prev_oid: None,
                        new_oid: Some(oid('a').as_bytes().to_vec()),
                    },
                    RefUpdate {
                        ref_name: "dev".into(),
                        prev_oid: None,
                        new_oid: Some(oid('a').as_bytes().to_vec()),
                    },
                    RefUpdate {
                        ref_name: "feat".into(),
                        prev_oid: None,
                        new_oid: Some(oid('b').as_bytes().to_vec()),
                    },
                ],
                pack_digest: Some(digest.clone()),
                tags: Vec::new(),
                cert: None,
            },
        );

        // an issue: number 1, channel follow-up emitted.
        let mut ctx = ctx_with_origin(2, user_origin(1));
        exec(
            &mut forge,
            &mut ctx,
            &ForgeMsg::OpenIssue {
                repo: "demo".into(),
                title: "it breaks".into(),
                body: "details".into(),
            },
        )
        .unwrap();
        futures::executor::block_on(forge.commit_block()).unwrap();
        assert_eq!(ctx.msgs().len(), 1);
        assert_eq!(ctx.msgs()[0].target, "chat");
        let chat::ChatMsg::CreateChannel {
            channel_id, name, ..
        } = crate::chat_contract::decode_msg(&ctx.msgs()[0].payload).unwrap()
        else {
            panic!("expected CreateChannel")
        };
        assert_eq!(channel_id, "forge:demo:1");
        assert_eq!(name, "demo#1");

        // a PR from the born feature branch: shares the number space (#2).
        let mut ctx = ctx_with_origin(3, user_origin(2));
        exec(
            &mut forge,
            &mut ctx,
            &ForgeMsg::OpenPr {
                repo: "demo".into(),
                title: "fix it".into(),
                body: "the fix".into(),
                source_branch: "feat".into(),
                target_branch: String::new(),
            },
        )
        .unwrap();
        futures::executor::block_on(forge.commit_block()).unwrap();
        assert_eq!(ctx.msgs().len(), 1, "channel follow-up for the PR");

        // a PR from an unborn branch rejects.
        let mut ctx = ctx_with_origin(4, user_origin(2));
        assert!(
            exec(
                &mut forge,
                &mut ctx,
                &ForgeMsg::OpenPr {
                    repo: "demo".into(),
                    title: "nope".into(),
                    body: String::new(),
                    source_branch: "ghost".into(),
                    target_branch: String::new(),
                },
            )
            .is_err()
        );
        futures::executor::block_on(forge.abort_block()).unwrap();

        // review with a line comment + approval system line.
        let mut ctx = ctx_with_origin(5, user_origin(3));
        exec(
            &mut forge,
            &mut ctx,
            &ForgeMsg::SubmitReview {
                repo: "demo".into(),
                number: 2,
                verdict: ReviewVerdict::Approve,
                body: "ship it".into(),
                commit_oid: oid('b').to_string(),
                comments: vec![ReviewComment {
                    path: "src/lib.rs".into(),
                    line: 10,
                    side: DiffSide::New,
                    body: "nice".into(),
                }],
            },
        )
        .unwrap();
        futures::executor::block_on(forge.commit_block()).unwrap();
        assert_eq!(ctx.msgs().len(), 1, "approval line emitted");

        // merge: stale target CAS rejects; the real one moves dev AND marks
        // the PR merged in one block.
        let mut ctx = ctx_with_origin(6, user_origin(2));
        assert!(
            exec(
                &mut forge,
                &mut ctx,
                &ForgeMsg::MergePr {
                    repo: "demo".into(),
                    number: 2,
                    prev_target_oid: oid('f').to_string(), // stale
                    expected_source_oid: oid('b').to_string(),
                    merge_oid: oid('c').to_string(),
                    pack_digest: hex(&digest),
                },
            )
            .is_err()
        );
        futures::executor::block_on(forge.abort_block()).unwrap();

        let mut ctx = ctx_with_origin(7, user_origin(2));
        exec(
            &mut forge,
            &mut ctx,
            &ForgeMsg::MergePr {
                repo: "demo".into(),
                number: 2,
                prev_target_oid: oid('a').to_string(),
                expected_source_oid: oid('b').to_string(),
                merge_oid: oid('c').to_string(),
                pack_digest: hex(&digest),
            },
        )
        .unwrap();
        futures::executor::block_on(forge.commit_block()).unwrap();
        assert_eq!(ctx.msgs().len(), 1, "merged line emitted");

        // committed state: release main stays put, dev advances, and the PR is
        // merged with its review recorded.
        assert_eq!(forge.read_head("demo"), Some(oid('a').to_string()));
        let refs = futures::executor::block_on(forge.query(&encode_query(&ForgeQuery::ListRefs {
            repo: "demo".into(),
        })))
        .unwrap();
        let ForgeReply::Refs(refs) = decode_reply(&refs).unwrap() else {
            panic!("refs missing")
        };
        assert_eq!(
            refs.iter().find(|head| head.name == "dev").unwrap().head,
            oid('c').to_string()
        );
        let reply = futures::executor::block_on(forge.query(&encode_query(&ForgeQuery::GetItem {
            repo: "demo".into(),
            number: 2,
        })))
        .unwrap();
        let ForgeReply::Item(Some(item)) = decode_reply(&reply).unwrap() else {
            panic!("item missing")
        };
        assert_eq!(item.summary.state, ItemState::Merged);
        assert_eq!(
            item.merge_oid.as_deref(),
            Some(oid('c').to_string().as_str())
        );
        assert_eq!(item.reviews.len(), 1);
        assert_eq!(item.channel_id, "forge:demo:2");

        // a merged PR cannot merge/close again.
        let mut ctx = ctx_with_origin(8, user_origin(2));
        assert!(
            exec(
                &mut forge,
                &mut ctx,
                &ForgeMsg::SetItemState {
                    repo: "demo".into(),
                    number: 2,
                    open: false,
                },
            )
            .is_err()
        );
        futures::executor::block_on(forge.abort_block()).unwrap();

        // tracker survives restart via the persisted file.
        drop(forge);
        let reopened = Forge::init("forge", base.clone()).unwrap();
        let reply =
            futures::executor::block_on(reopened.query(&encode_query(&ForgeQuery::ListItems {
                repo: "demo".into(),
            })))
            .unwrap();
        let ForgeReply::Items(items) = decode_reply(&reply).unwrap() else {
            panic!("wrong reply")
        };
        assert_eq!(items.len(), 2, "issue + PR re-adopted from disk");

        let _ = std::fs::remove_dir_all(&base);
    }

    // two independent namespaces replaying the same ops (branches + tracker)
    // compose IDENTICAL roots — the tracker is deterministic consensus state.
    #[test]
    fn tracker_root_is_reproducible_across_namespaces() {
        let run = |tag: &str| {
            let base = tmp_base(tag);
            let mut forge = Forge::init("forge", base.clone())
                .unwrap()
                .with_chat("chat");
            let mut ctx = ctx_at(1);
            exec_commit(
                &mut forge,
                &mut ctx,
                &ForgeMsg::PushRefs {
                    repo: "demo".into(),
                    updates: vec![RefUpdate {
                        ref_name: "main".into(),
                        prev_oid: None,
                        new_oid: Some(oid('a').as_bytes().to_vec()),
                    }],
                    pack_digest: Some(vec![1u8; 32]),
                    tags: Vec::new(),
                    cert: None,
                },
            );
            let mut ctx = ctx_with_origin(2, user_origin(9));
            exec(
                &mut forge,
                &mut ctx,
                &ForgeMsg::OpenIssue {
                    repo: "demo".into(),
                    title: "same".into(),
                    body: "same".into(),
                },
            )
            .unwrap();
            futures::executor::block_on(forge.commit_block()).unwrap();
            let root = forge.root();
            let _ = std::fs::remove_dir_all(&base);
            root
        };
        assert_eq!(run("det-t-a"), run("det-t-b"));
    }

    // every snapshot leads with the container magic; install requires it.
    #[test]
    fn snapshot_leads_with_magic_and_install_requires_it() {
        let base = tmp_base("magic");
        let mut forge = Forge::init("forge", base.clone()).unwrap();
        seed_materialized_commit(&mut forge, 42, "docs", "a.txt", "x", "c");
        let root = forge.root();
        let snap = forge.snapshot().unwrap();
        assert!(snap.starts_with(FORGE_SNAPSHOT_MAGIC.as_slice()));

        let rt = tmp_base("magic-rt");
        let mut fresh = Forge::init("forge", rt.clone()).unwrap();
        assert!(
            fresh
                .install(&snap[FORGE_SNAPSHOT_MAGIC.len()..], root)
                .is_err(),
            "a container missing the magic must be rejected"
        );
        fresh.install(&snap, root).unwrap();
        assert_eq!(fresh.root(), root, "install reproduces the root");

        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&rt);
    }

    #[test]
    fn snapshot_cache_reuses_a_valid_pack_after_restart() {
        let base = tmp_base("snapshot-cache-restart");
        let mut forge = Forge::init("forge", base.clone()).unwrap();
        seed_materialized_commit(&mut forge, 1, "demo", "a.txt", "hello", "c1");

        let builds_before = snapshot::snapshot_pack_builds();
        let first = forge.snapshot().unwrap();
        let builds_after_seed = snapshot::snapshot_pack_builds();
        assert_eq!(builds_after_seed, builds_before + 1);
        assert!(base.join(SNAPSHOT_CACHE_FILE).is_file());
        drop(forge);

        let reopened = Forge::init("forge", base.clone()).unwrap();
        let second = reopened.snapshot().unwrap();
        assert_eq!(second, first, "restart changed an unchanged snapshot");
        assert_eq!(
            snapshot::snapshot_pack_builds(),
            builds_after_seed,
            "restart rebuilt a pack already persisted by snapshot()"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn snapshot_cache_reserializes_tracker_without_repacking() {
        let base = tmp_base("snapshot-cache-tracker");
        let mut forge = Forge::init("forge", base.clone()).unwrap();
        seed_materialized_commit(&mut forge, 1, "demo", "a.txt", "hello", "c1");
        let first = forge.snapshot().unwrap();
        let builds_after_seed = snapshot::snapshot_pack_builds();

        let mut ctx = ctx_with_origin(2, user_origin(7));
        exec_commit(
            &mut forge,
            &mut ctx,
            &ForgeMsg::OpenIssue {
                repo: "demo".into(),
                title: "fresh tracker tail".into(),
                body: "carried by the next snapshot".into(),
            },
        );
        let root = forge.root();
        let updated = forge.snapshot().unwrap();
        assert_ne!(updated, first, "tracker mutation was not serialized");
        assert_eq!(
            snapshot::snapshot_pack_builds(),
            builds_after_seed,
            "tracker-only state rebuilt an unchanged Git pack"
        );

        let roundtrip = tmp_base("snapshot-cache-tracker-roundtrip");
        let mut installed = Forge::init("forge", roundtrip.clone()).unwrap();
        installed.install(&updated, root).unwrap();
        let reply =
            futures::executor::block_on(installed.query(&encode_query(&ForgeQuery::ListItems {
                repo: "demo".into(),
            })))
            .unwrap();
        let ForgeReply::Items(items) = decode_reply(&reply).unwrap() else {
            panic!("wrong reply")
        };
        assert_eq!(items[0].title, "fresh tracker tail");

        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&roundtrip);
    }

    #[test]
    fn snapshot_cache_invalidates_changed_refs_and_pending() {
        let base = tmp_base("snapshot-cache-keys");
        let mut forge = Forge::init("forge", base.clone()).unwrap();
        seed_materialized_commit(&mut forge, 1, "demo", "a.txt", "hello", "c1");
        forge.snapshot().unwrap();
        let builds_after_seed = snapshot::snapshot_pack_builds();

        seed_materialized_commit(&mut forge, 2, "demo", "b.txt", "world", "c2");
        drop(forge);
        let mut reopened = Forge::init("forge", base.clone()).unwrap();
        reopened.snapshot().unwrap();
        let builds_after_ref = snapshot::snapshot_pack_builds();
        assert_eq!(
            builds_after_ref,
            builds_after_seed + 1,
            "a changed committed ref reused the prior pack"
        );

        reopened.state.repos.get_mut("demo").unwrap().adopt_pending(
            [(refs::RefName::Branch("feature".into()), (oid('f'), [9; 32]))]
                .into_iter()
                .collect(),
        );
        reopened.persist_pending().unwrap();
        drop(reopened);
        let reopened = Forge::init("forge", base.clone()).unwrap();
        reopened.snapshot().unwrap();
        assert_eq!(
            snapshot::snapshot_pack_builds(),
            builds_after_ref + 1,
            "a changed pending set reused the prior pack"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn snapshot_cache_digest_rejects_a_damaged_valid_file() {
        let base = tmp_base("snapshot-cache-digest");
        let mut forge = Forge::init("forge", base.clone()).unwrap();
        seed_materialized_commit(&mut forge, 1, "demo", "a.txt", "hello", "c1");
        forge.snapshot().unwrap();
        let builds_after_seed = snapshot::snapshot_pack_builds();

        let path = base.join(SNAPSHOT_CACHE_FILE);
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&path, bytes).unwrap();
        drop(forge);

        let reopened = Forge::init("forge", base.clone()).unwrap();
        assert!(
            reopened.snapshot_cache.borrow().is_none(),
            "a cache with a damaged digest was adopted"
        );
        reopened.snapshot().unwrap();
        assert_eq!(
            snapshot::snapshot_pack_builds(),
            builds_after_seed + 1,
            "a cache with a damaged digest avoided a required rebuild"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    // a snapshot carries branches AND tracker; install onto a fresh namespace
    // reproduces the root byte-for-byte.
    #[test]
    fn snapshot_round_trips_branches_and_tracker() {
        let base = tmp_base("snap");
        let mut forge = Forge::init("forge", base.clone())
            .unwrap()
            .with_chat("chat");

        // Real fixture objects on main, then a second branch on the same oid —
        // its objects exist, so the snapshot pack closes.
        seed_materialized_commit(&mut forge, 1, "demo", "a.txt", "hello", "c1");
        let head = git_head_oid(&base, "demo");
        let mut ctx = ctx_at(2);
        exec_commit(
            &mut forge,
            &mut ctx,
            &ForgeMsg::PushRefs {
                repo: "demo".into(),
                updates: vec![RefUpdate {
                    ref_name: "feat".into(),
                    prev_oid: None,
                    new_oid: Some(head.as_bytes().to_vec()),
                }],
                pack_digest: Some(vec![3u8; 32]),
                tags: Vec::new(),
                cert: None,
            },
        );
        let mut ctx = ctx_with_origin(3, user_origin(4));
        exec(
            &mut forge,
            &mut ctx,
            &ForgeMsg::OpenIssue {
                repo: "demo".into(),
                title: "carry me".into(),
                body: String::new(),
            },
        )
        .unwrap();
        futures::executor::block_on(forge.commit_block()).unwrap();

        let root = forge.root();
        let snap = forge.snapshot().unwrap();

        let rt = tmp_base("snap-rt");
        let mut fresh = Forge::init("forge", rt.clone()).unwrap();
        fresh.install(&snap, root).unwrap();
        assert_eq!(fresh.root(), root, "install reproduces the root");
        let reply =
            futures::executor::block_on(fresh.query(&encode_query(&ForgeQuery::ListItems {
                repo: "demo".into(),
            })))
            .unwrap();
        let ForgeReply::Items(items) = decode_reply(&reply).unwrap() else {
            panic!("wrong reply")
        };
        assert_eq!(items.len(), 1, "tracker rode the snapshot");

        // a tampered container is rejected by the root gate.
        let mut bad = snap.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        let mut fresh2 = Forge::init("forge", tmp_base("snap-bad")).unwrap();
        assert!(fresh2.install(&bad, root).is_err());

        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&rt);
    }

    // ---- pushes and merges --------------------------------------------------

    fn push(
        forge: &mut Forge,
        ctx: &mut TestCtx,
        repo: &str,
        branch: &str,
        prev: Option<Oid>,
        new: Oid,
    ) -> Result<(), Error> {
        let r = exec(
            forge,
            ctx,
            &ForgeMsg::PushRefs {
                repo: repo.into(),
                updates: vec![RefUpdate {
                    ref_name: branch.into(),
                    prev_oid: prev.map(|o| o.as_bytes().to_vec()),
                    new_oid: Some(new.as_bytes().to_vec()),
                }],
                pack_digest: Some(vec![7u8; 32]),
                tags: Vec::new(),
                cert: None,
            },
        );
        match &r {
            Ok(()) => futures::executor::block_on(forge.commit_block()).unwrap(),
            Err(_) => futures::executor::block_on(forge.abort_block()).unwrap(),
        }
        r
    }

    // ANY target accepts a merge from ANY member — a protected one included:
    // a stranger, a reviewer, the member who pushed the repo. reviews are
    // stored and shown and change nothing about who may merge.
    #[test]
    fn merging_onto_any_target_is_any_members() {
        let base = tmp_base("any-merge");
        let mut forge = Forge::init("forge", base.clone()).unwrap();
        let digest = vec![9u8; 32];

        // `one` merges onto the protected integration branch; the others onto
        // a feature branch.
        let target_of = |repo: &str| match repo {
            "one" => "dev",
            _ => "release",
        };
        let mut pusher = ctx_with_origin(1, user_origin(1));
        for repo in ["one", "two", "three"] {
            exec_commit(
                &mut forge,
                &mut pusher,
                &ForgeMsg::PushRefs {
                    repo: repo.into(),
                    updates: vec![
                        RefUpdate {
                            ref_name: target_of(repo).into(),
                            prev_oid: None,
                            new_oid: Some(oid('a').as_bytes().to_vec()),
                        },
                        RefUpdate {
                            ref_name: "feat".into(),
                            prev_oid: None,
                            new_oid: Some(oid('b').as_bytes().to_vec()),
                        },
                    ],
                    pack_digest: Some(digest.clone()),
                    tags: Vec::new(),
                    cert: None,
                },
            );
        }

        let open_pr = |forge: &mut Forge, repo: &str, author: u8| {
            let mut ctx = ctx_with_origin(2, user_origin(author));
            exec_commit(
                forge,
                &mut ctx,
                &ForgeMsg::OpenPr {
                    repo: repo.into(),
                    title: "fix it".into(),
                    body: String::new(),
                    source_branch: "feat".into(),
                    target_branch: target_of(repo).into(),
                },
            );
        };
        let merge_of = |repo: &str| ForgeMsg::MergePr {
            repo: repo.into(),
            number: 1,
            prev_target_oid: oid('a').to_string(),
            expected_source_oid: oid('b').to_string(),
            merge_oid: oid('c').to_string(),
            pack_digest: hex(&digest),
        };

        // a stranger — not the author, not a reviewer, not the pusher — merges
        // onto the protected branch.
        open_pr(&mut forge, "one", 2);
        let mut stranger = ctx_with_origin(3, user_origin(9));
        exec_commit(&mut forge, &mut stranger, &merge_of("one"));
        assert_eq!(forge.state.repos["one"].refs["dev"], oid('c'));

        // a reviewer's review is stored, and the reviewer merges like anyone.
        open_pr(&mut forge, "two", 2);
        let mut reviewer = ctx_with_origin(4, user_origin(3));
        exec_commit(
            &mut forge,
            &mut reviewer,
            &ForgeMsg::SubmitReview {
                repo: "two".into(),
                number: 1,
                verdict: ReviewVerdict::Approve,
                body: "ship it".into(),
                commit_oid: oid('b').to_string(),
                comments: vec![],
            },
        );
        exec_commit(&mut forge, &mut reviewer, &merge_of("two"));
        assert_eq!(forge.state.repos["two"].refs["release"], oid('c'));

        // the member who pushed the repo merges too.
        open_pr(&mut forge, "three", 2);
        exec_commit(&mut forge, &mut pusher, &merge_of("three"));
        assert_eq!(forge.state.repos["three"].refs["release"], oid('c'));

        let _ = std::fs::remove_dir_all(&base);
    }

    // SetItemState stays open ON PURPOSE — but it is still authenticated.
    #[test]
    fn any_member_closes_an_item_but_an_unauthenticated_origin_cannot() {
        let base = tmp_base("close-open");
        let mut forge = Forge::init("forge", base.clone()).unwrap();
        let mut author = ctx_with_origin(1, user_origin(1));
        exec_commit(
            &mut forge,
            &mut author,
            &ForgeMsg::OpenIssue {
                repo: "demo".into(),
                title: "triage me".into(),
                body: String::new(),
            },
        );
        let close = ForgeMsg::SetItemState {
            repo: "demo".into(),
            number: 1,
            open: false,
        };

        // the pre-consensus probe and the system origin are refused; a MODULE
        // is authenticated and is allowed as a tracker author.
        for origin in [sdk::Origin::External(Vec::new()), sdk::Origin::System] {
            let mut probe = ctx_with_origin(2, origin.clone());
            assert!(
                exec(&mut forge, &mut probe, &close).is_err(),
                "{origin:?} must not close an item"
            );
            futures::executor::block_on(forge.abort_block()).unwrap();
        }

        let mut stranger = ctx_with_origin(3, user_origin(9));
        exec_commit(&mut forge, &mut stranger, &close);
        let item = forge.state.tracker.get("demo", 1).expect("item");
        assert_eq!(
            item.summary.state,
            ItemState::Closed,
            "triage is open to all"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn code_queries_list_one_tree_and_keep_the_returned_revision_pinned() {
        let base = tmp_base("browse-pinned");
        let mut forge = Forge::init("forge", base.clone()).unwrap();
        let repo = refs::open_or_init_repo(&base, "demo").unwrap();
        let alpha = repo.blob(b"alpha\n").unwrap();
        let zebra = repo.blob(b"zebra\n").unwrap();
        let source = repo.blob(b"pub fn one() {}\n").unwrap();
        let mut src = repo.treebuilder(None).unwrap();
        src.insert("lib.rs", source, 0o100644).unwrap();
        let src_oid = src.write().unwrap();
        let mut root = repo.treebuilder(None).unwrap();
        root.insert("zebra.txt", zebra, 0o100644).unwrap();
        root.insert("src", src_oid, 0o040000).unwrap();
        root.insert("alpha.txt", alpha, 0o100644).unwrap();
        let root_oid = root.write().unwrap();
        let tree = repo.find_tree(root_oid).unwrap();
        let first = git::commit(&repo, &tree, None, "first", 1).unwrap();
        git::update_ref(&repo, &heads_ref(MAIN_BRANCH), first).unwrap();
        forge
            .state
            .repos
            .entry("demo".into())
            .or_default()
            .refs
            .insert(MAIN_BRANCH.into(), first.into());

        let ForgeReply::Tree(root) = query_reply(
            &forge,
            ForgeQuery::Tree {
                repo: "demo".into(),
                rev: String::new(),
                path: String::new(),
            },
        )
        .unwrap() else {
            panic!("wrong root tree reply")
        };
        assert_eq!(root.rev, first.to_string());
        assert!(root.born && !root.truncated);
        let rows: Vec<_> = root
            .entries
            .iter()
            .map(|entry| (entry.kind, entry.path.as_str()))
            .collect();
        assert_eq!(
            rows,
            vec![
                (TreeEntryKind::Dir, "src"),
                (TreeEntryKind::File, "alpha.txt"),
                (TreeEntryKind::File, "zebra.txt"),
            ]
        );

        let ForgeReply::Tree(src) = query_reply(
            &forge,
            ForgeQuery::Tree {
                repo: "demo".into(),
                rev: root.rev.clone(),
                path: "src".into(),
            },
        )
        .unwrap() else {
            panic!("wrong nested tree reply")
        };
        assert_eq!(src.entries[0].path, "src/lib.rs");
        let ForgeReply::Blob(blob) = query_reply(
            &forge,
            ForgeQuery::Blob {
                repo: "demo".into(),
                rev: root.rev.clone(),
                path: "src/lib.rs".into(),
            },
        )
        .unwrap() else {
            panic!("wrong blob reply")
        };
        assert_eq!(blob.text, "pub fn one() {}\n");
        assert!(!blob.binary && !blob.truncated);

        seed_materialized_commit(&mut forge, 2, "demo", "later.txt", "later\n", "later");
        let ForgeReply::Blob(pinned) = query_reply(
            &forge,
            ForgeQuery::Blob {
                repo: "demo".into(),
                rev: root.rev,
                path: "src/lib.rs".into(),
            },
        )
        .unwrap() else {
            panic!("wrong pinned blob reply")
        };
        assert_eq!(pinned.rev, first.to_string());
        assert_eq!(pinned.text, "pub fn one() {}\n");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Every born branch is browsable at its own head, merged into the
    /// integration branch or not: the code seat's branch selector pins the
    /// browse to the picked branch's head. A commit no branch reaches is
    /// still refused.
    #[test]
    fn code_queries_browse_any_born_branch_at_its_head() {
        let base = tmp_base("browse-any-branch");
        let mut forge = Forge::init("forge", base.clone()).unwrap();
        let repo = refs::open_or_init_repo(&base, "demo").unwrap();
        let readme = repo.blob(b"readme\n").unwrap();
        let mut root = repo.treebuilder(None).unwrap();
        root.insert("README.md", readme, 0o100644).unwrap();
        let main_tree = repo.find_tree(root.write().unwrap()).unwrap();
        let first = git::commit(&repo, &main_tree, None, "first", 1).unwrap();
        let poem = repo.blob(b"a poem\n").unwrap();
        let mut root = repo.treebuilder(Some(&main_tree)).unwrap();
        root.insert("POEM.md", poem, 0o100644).unwrap();
        let poem_tree = repo.find_tree(root.write().unwrap()).unwrap();
        let first_commit = repo.find_commit(first).unwrap();
        let on_branch = git::commit(&repo, &poem_tree, Some(&first_commit), "poem", 2).unwrap();
        let stray = repo.blob(b"stray\n").unwrap();
        let mut root = repo.treebuilder(Some(&main_tree)).unwrap();
        root.insert("STRAY.md", stray, 0o100644).unwrap();
        let stray_tree = repo.find_tree(root.write().unwrap()).unwrap();
        let on_no_branch =
            git::commit(&repo, &stray_tree, Some(&first_commit), "stray", 3).unwrap();
        git::update_ref(&repo, &heads_ref(MAIN_BRANCH), first).unwrap();
        git::update_ref(&repo, &heads_ref("agent/poem"), on_branch).unwrap();
        let state = forge.state.repos.entry("demo".into()).or_default();
        state.refs.insert(MAIN_BRANCH.into(), first.into());
        state.refs.insert("agent/poem".into(), on_branch.into());

        let tree_at = |rev: git2::Oid| {
            query_reply(
                &forge,
                ForgeQuery::Tree {
                    repo: "demo".into(),
                    rev: rev.to_string(),
                    path: String::new(),
                },
            )
        };
        let ForgeReply::Tree(branch_root) = tree_at(on_branch).unwrap() else {
            panic!("wrong branch tree reply")
        };
        assert_eq!(branch_root.rev, on_branch.to_string());
        let names: Vec<_> = branch_root
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        assert_eq!(names, ["POEM.md", "README.md"]);
        // the shared ancestor answers from either branch
        let ForgeReply::Tree(shared) = tree_at(first).unwrap() else {
            panic!("wrong shared tree reply")
        };
        assert_eq!(shared.rev, first.to_string());
        let refused = tree_at(on_no_branch).unwrap_err().to_string();
        assert!(
            refused.contains("not reachable from any branch"),
            "{refused}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The picture viewer's read: pages of `blob_bytes` concatenate to the
    /// exact object, `eof` fires on the page that reaches its end, a page past
    /// the end is empty-and-eof, and `size` always names the whole object.
    #[test]
    fn blob_bytes_pages_concatenate_to_the_whole_object() {
        use base64::Engine as _;
        let base = tmp_base("blob-bytes");
        let mut forge = Forge::init("forge", base.clone()).unwrap();
        // A NUL up front makes it binary for `blob`; `blob_bytes` does not care.
        let content = "\0PNG-ish bytes, not text, long enough to page\n";
        seed_materialized_commit(&mut forge, 1, "demo", "logo.png", content, "main");
        let rev = git_head_oid(&base, "demo").to_string();
        let page = |offset: u64, len: u64| {
            let ForgeReply::BlobBytes(page) = query_reply(
                &forge,
                ForgeQuery::BlobBytes {
                    repo: "demo".into(),
                    rev: rev.clone(),
                    path: "logo.png".into(),
                    offset,
                    len,
                },
            )
            .unwrap() else {
                panic!("wrong blob_bytes reply")
            };
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(page.b64)
                .unwrap();
            (bytes, page.eof, page.size)
        };
        let total = content.len() as u64;
        let (first, first_eof, size) = page(0, 10);
        let (rest, rest_eof, _) = page(10, 4096);
        assert_eq!(size, total as i64);
        assert!(!first_eof && rest_eof);
        assert_eq!([first, rest].concat(), content.as_bytes());
        let (past, past_eof, _) = page(total + 5, 10);
        assert!(
            past.is_empty() && past_eof,
            "a page past the end is empty and final"
        );

        let ForgeReply::Blob(blob) = query_reply(
            &forge,
            ForgeQuery::Blob {
                repo: "demo".into(),
                rev: rev.clone(),
                path: "logo.png".into(),
            },
        )
        .unwrap() else {
            panic!("wrong blob reply")
        };
        assert!(blob.binary, "the text lane still brands it binary");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn code_queries_reject_unsafe_or_uncommitted_revisions() {
        let base = tmp_base("browse-validation");
        let mut forge = Forge::init("forge", base.clone()).unwrap();
        seed_materialized_commit(&mut forge, 1, "demo", "a.txt", "a\n", "main");
        let repo = git::open(&base.join("demo")).unwrap();
        let head = repo
            .find_commit(git_head_oid(&base, "demo").into())
            .unwrap();
        let tree = repo.find_tree(head.tree_id()).unwrap();
        let orphan = git::commit(&repo, &tree, None, "orphan", 2).unwrap();

        for query in [
            ForgeQuery::Tree {
                repo: "demo".into(),
                rev: "main".into(),
                path: String::new(),
            },
            ForgeQuery::Tree {
                repo: "demo".into(),
                rev: orphan.to_string(),
                path: String::new(),
            },
            ForgeQuery::Tree {
                repo: "demo".into(),
                rev: String::new(),
                path: "../objects".into(),
            },
            ForgeQuery::Blob {
                repo: "demo".into(),
                rev: String::new(),
                path: "/a.txt".into(),
            },
        ] {
            assert!(query_reply(&forge, query).is_err());
        }

        let deep = (0..=MAX_BROWSE_TREE_DEPTH)
            .map(|_| "x")
            .collect::<Vec<_>>()
            .join("/");
        assert!(browse_path(&deep, true).is_err());

        let hidden = repo.blob(b"hidden").unwrap();
        let mut builder = repo.treebuilder(Some(&tree)).unwrap();
        builder.insert(&[0xff][..], hidden, 0o100644).unwrap();
        let tree_oid = builder.write().unwrap();
        let non_utf8_tree = repo.find_tree(tree_oid).unwrap();
        let visible_head = git::commit(&repo, &non_utf8_tree, Some(&head), "visible", 3).unwrap();
        git::update_ref(&repo, &heads_ref(MAIN_BRANCH), visible_head).unwrap();
        forge
            .state
            .repos
            .get_mut("demo")
            .unwrap()
            .refs
            .insert(MAIN_BRANCH.into(), visible_head.into());
        let ForgeReply::Tree(listing) = query_reply(
            &forge,
            ForgeQuery::Tree {
                repo: "demo".into(),
                rev: String::new(),
                path: String::new(),
            },
        )
        .unwrap() else {
            panic!("wrong non-utf8 tree reply")
        };
        assert!(listing.truncated, "the omitted non-utf8 entry is disclosed");
        assert_eq!(listing.entries[0].path, "a.txt");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn code_blob_query_bounds_text_and_marks_binary() {
        let base = tmp_base("browse-blob-bounds");
        let mut forge = Forge::init("forge", base.clone()).unwrap();
        seed_materialized_commit(&mut forge, 1, "demo", "binary.dat", "head\0tail", "binary");
        let huge = "x".repeat(MAX_BLOB_BYTES + 1);
        seed_materialized_commit(&mut forge, 2, "demo", "huge.txt", &huge, "huge");
        let rev = forge.state.repos["demo"].refs[MAIN_BRANCH].to_string();

        let ForgeReply::Blob(binary) = query_reply(
            &forge,
            ForgeQuery::Blob {
                repo: "demo".into(),
                rev: rev.clone(),
                path: "binary.dat".into(),
            },
        )
        .unwrap() else {
            panic!("wrong binary reply")
        };
        assert!(binary.binary && binary.text.is_empty());
        assert_eq!(binary.size, 9);

        let ForgeReply::Blob(huge) = query_reply(
            &forge,
            ForgeQuery::Blob {
                repo: "demo".into(),
                rev,
                path: "huge.txt".into(),
            },
        )
        .unwrap() else {
            panic!("wrong oversized reply")
        };
        assert!(huge.truncated && !huge.binary && huge.text.is_empty());
        assert_eq!(huge.size, (MAX_BLOB_BYTES + 1) as i64);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn code_tree_query_is_bounded_and_unborn_is_explicit() {
        let empty_base = tmp_base("browse-unborn");
        let empty = Forge::init("forge", empty_base.clone()).unwrap();
        let ForgeReply::Tree(unborn) = query_reply(
            &empty,
            ForgeQuery::Tree {
                repo: "demo".into(),
                rev: String::new(),
                path: String::new(),
            },
        )
        .unwrap() else {
            panic!("wrong unborn reply")
        };
        assert!(!unborn.born && unborn.rev.is_empty() && unborn.entries.is_empty());
        assert!(
            query_reply(
                &empty,
                ForgeQuery::Blob {
                    repo: "demo".into(),
                    rev: String::new(),
                    path: "a.txt".into(),
                },
            )
            .is_err()
        );

        let base = tmp_base("browse-wide");
        let mut forge = Forge::init("forge", base.clone()).unwrap();
        let repo = refs::open_or_init_repo(&base, "demo").unwrap();
        let blob = repo.blob(b"x").unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        for index in 0..MAX_TREE_ENTRIES + 2 {
            builder
                .insert(format!("f{index:04}.txt"), blob, 0o100644)
                .unwrap();
        }
        let tree_oid = builder.write().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let head = git::commit(&repo, &tree, None, "wide", 1).unwrap();
        git::update_ref(&repo, &heads_ref(MAIN_BRANCH), head).unwrap();
        forge
            .state
            .repos
            .entry("demo".into())
            .or_default()
            .refs
            .insert(MAIN_BRANCH.into(), head.into());
        let ForgeReply::Tree(wide) = query_reply(
            &forge,
            ForgeQuery::Tree {
                repo: "demo".into(),
                rev: String::new(),
                path: String::new(),
            },
        )
        .unwrap() else {
            panic!("wrong wide reply")
        };
        assert!(wide.truncated);
        assert_eq!(wide.entries.len(), MAX_TREE_ENTRIES);
        assert_eq!(wide.entries.first().unwrap().name, "f0000.txt");
        assert_eq!(wide.entries.last().unwrap().name, "f0999.txt");

        let _ = std::fs::remove_dir_all(&empty_base);
        let _ = std::fs::remove_dir_all(&base);
    }

    // a REFUSED push must leave no repo behind: `abort_block` drops staged
    // fates, never a map entry, so the entry can only be inserted on success.
    #[test]
    fn a_refused_push_creates_no_repo() {
        let base = tmp_base("phantom");
        let mut forge = Forge::init("forge", base.clone()).unwrap();
        let mut ctx = ctx_at(1);
        // a CAS against an unborn branch — the repo has never existed.
        push(
            &mut forge,
            &mut ctx,
            "ghost",
            "main",
            Some(oid('a')),
            oid('b'),
        )
        .expect_err("prev_oid on an unborn branch fails the CAS");
        assert!(!forge.state.repos.contains_key("ghost"), "no phantom repo");

        let reply = futures::executor::block_on(forge.query(&encode_query(&ForgeQuery::ListRepos)))
            .unwrap();
        let ForgeReply::Repos(repos) = decode_reply(&reply).unwrap() else {
            panic!("wrong reply")
        };
        assert!(repos.is_empty(), "ListRepos stays empty: {repos:?}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// installed packs under a repo — what compaction collapses.
    fn pack_count(repo_dir: &std::path::Path) -> usize {
        std::fs::read_dir(repo_dir.join(".git").join("objects").join("pack"))
            .expect("a repo that received a pack has a pack dir")
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "pack"))
            .count()
    }

    /// seed `base/demo` the way `materialize` does — one installed pack per
    /// push, objects that exist ONLY inside those packs (the history is built
    /// in a separate repo, so nothing lands loose) — and return its head.
    fn seed_pushed_packs(base: &std::path::Path, source_base: &std::path::Path) -> Oid {
        let source = git::init(source_base).expect("source repo");
        let dest = refs::open_or_init_repo(base, "demo").expect("dest repo");
        let mut head = None;
        for (t, content) in ["one", "two", "three"].iter().enumerate() {
            let blob = source.blob(content.as_bytes()).unwrap();
            let parent = head.map(|oid: Oid| source.find_commit(oid.into()).unwrap());
            let base_tree = parent.as_ref().map(|commit| commit.tree().unwrap());
            let tree_oid = git::build_tree(&source, base_tree.as_ref(), "a.txt", blob).unwrap();
            let tree = source.find_tree(tree_oid).unwrap();
            let oid = git::commit(&source, &tree, parent.as_ref(), content, t as u64).unwrap();
            // what a real push carries: the closure the client's common base
            // leaves out, and the WHOLE closure only when it has no base.
            let pack = match head {
                Some(prev) => git::pack_delta(&source, &[oid], &[prev.into()]).unwrap(),
                None => git::pack_closure_many(&source, &[oid]).unwrap(),
            };
            git::install_pack(&dest, &pack).unwrap();
            head = Some(oid.into());
        }
        let head = head.expect("three commits");
        git::update_ref(&dest, &heads_ref(MAIN_BRANCH), head.into()).unwrap();
        head
    }

    /// libgit2 implements no gc, so nothing but this collapses the pack a
    /// push installs. compaction must leave ONE pack that still closes the
    /// branch head, with the ref untouched.
    #[test]
    fn compaction_folds_the_packs_and_keeps_every_head_whole() {
        let base = tmp_base("compact");
        let source_base = tmp_base("compact-source");
        let head = seed_pushed_packs(&base, &source_base);
        assert_eq!(pack_count(&base.join("demo")), 3, "one pack per push");

        let reclaimed = compact_repos(&base, 2).expect("compaction runs");

        assert_eq!(reclaimed, 3, "every pack that predated the compacted one");
        assert_eq!(pack_count(&base.join("demo")), 1, "collapsed into one pack");
        let repo = refs::open_or_init_repo(&base, "demo").unwrap();
        git::verify_closure(&repo, head.into())
            .expect("the compacted pack still closes the branch head");
        assert_eq!(git_head_oid(&base, "demo"), head, "the ref never moved");
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&source_base);
    }

    /// the oldest commit reachable from `head` — the "behind" position a
    /// catch-up test starts a second workspace at.
    fn root_commit(base: &std::path::Path, head: Oid) -> git2::Oid {
        let repo = refs::open_or_init_repo(base, "demo").unwrap();
        let mut commit = repo.find_commit(head.into()).unwrap();
        while commit.parent_count() > 0 {
            commit = commit.parent(0).unwrap();
        }
        commit.id()
    }

    /// the objects can arrive by ANY route — the catch-up lane pulls them
    /// from a peer that never held the pushed pack. materialize must then
    /// advance the ref on the closure alone, with that digest nowhere.
    #[test]
    fn a_head_whose_objects_arrived_elsewhere_materializes_without_its_pack() {
        let base = tmp_base("materialize-any-route");
        let source_base = tmp_base("materialize-any-route-source");
        let head = seed_pushed_packs(&base, &source_base);
        // rewind the ref: every object is here, the branch just has not been
        // moved onto the committed head yet.
        let repo = refs::open_or_init_repo(&base, "demo").unwrap();
        let behind = repo
            .find_commit(head.into())
            .unwrap()
            .parent(0)
            .unwrap()
            .id();
        git::update_ref(&repo, &heads_ref(MAIN_BRANCH), behind).unwrap();

        let mut forge = Forge::init("forge", base.clone()).unwrap();
        forge
            .state
            .repos
            .get_mut("demo")
            .expect("the on-disk repo is adopted")
            .adopt_pending(refs::PendingMap::from([(
                refs::RefName::Branch(MAIN_BRANCH.into()),
                (head, [7u8; 32]),
            )]));
        // nothing in the blob store answers for that digest, and nothing ever will.
        forge.materialize().unwrap();

        assert_eq!(
            git_head_oid(&base, "demo"),
            head,
            "the ref advanced on the closure alone"
        );
        assert!(
            pending_branches(&base).unwrap().is_empty(),
            "the branch is no longer waiting on anything"
        );
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&source_base);
    }

    /// the lane end to end, without a mesh: a node holding the head builds the
    /// objects a behind node is missing, bounded by what that node already
    /// has, and the behind node installs them and proves the closure.
    #[test]
    fn a_holder_rebuilds_exactly_the_objects_a_behind_node_is_missing() {
        let base = tmp_base("objects-holder");
        let source_base = tmp_base("objects-holder-source");
        let behind = tmp_base("objects-behind");
        let head = seed_pushed_packs(&base, &source_base);

        // the behind node holds only the root commit.
        let first = root_commit(&base, head);
        let holder = refs::open_or_init_repo(&base, "demo").unwrap();
        let seed = git::pack_closure_many(&holder, &[first]).unwrap();
        let catching_up = refs::open_or_init_repo(&behind, "demo").unwrap();
        git::install_pack(&catching_up, &seed).unwrap();
        git::update_ref(&catching_up, &heads_ref(MAIN_BRANCH), first).unwrap();

        let bases = on_disk_heads(&behind, "demo").unwrap();
        assert_eq!(bases, vec![Oid::from(first)], "its own head is the base");
        let bounded = build_objects(&base, "demo", head, &bases)
            .unwrap()
            .expect("the holder serves a head it holds");
        let whole = build_objects(&base, "demo", head, &[]).unwrap().unwrap();
        assert!(
            bounded.len() < whole.len(),
            "the base bounds the answer ({} vs {} bytes)",
            bounded.len(),
            whole.len()
        );

        install_objects(&behind, "demo", head, &bounded).expect("the closure lands");

        // and the guard: a commit this node holds but does NOT serve as a
        // branch head is not packable — the lane is not a general odb reader.
        assert!(
            build_objects(&base, "demo", Oid::from(first), &[])
                .unwrap()
                .is_none(),
            "only branch heads are servable"
        );
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&source_base);
        let _ = std::fs::remove_dir_all(&behind);
    }

    /// a packfile is named for its contents, so when one installed pack
    /// ALREADY holds the whole closure, compaction re-installs that same file
    /// and has nothing new to keep. it must then leave every pack alone rather
    /// than unlink the one holding everything.
    #[test]
    fn compaction_keeps_the_pack_that_already_holds_the_whole_closure() {
        let base = tmp_base("compact-collide");
        let source_base = tmp_base("compact-collide-source");
        let head = seed_pushed_packs(&base, &source_base);
        // a client with no common base pushes the FULL closure — the same pack
        // compaction would write.
        let source = git::open(&source_base).unwrap();
        let dest = refs::open_or_init_repo(&base, "demo").unwrap();
        let whole = git::pack_closure_many(&source, &[head.into()]).unwrap();
        git::install_pack(&dest, &whole).unwrap();
        assert_eq!(pack_count(&base.join("demo")), 4);

        assert_eq!(compact_repos(&base, 2).expect("compaction runs"), 0);

        assert_eq!(pack_count(&base.join("demo")), 4, "nothing unlinked");
        git::verify_closure(&dest, head.into()).expect("the closure survives");
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&source_base);
    }

    /// a repo below the ceiling is left alone — the point of the ceiling is
    /// that repacking a healthy repo every tick is pure waste.
    #[test]
    fn compaction_leaves_a_repo_under_the_pack_ceiling_alone() {
        let base = tmp_base("compact-under");
        let source_base = tmp_base("compact-under-source");
        seed_pushed_packs(&base, &source_base);

        assert_eq!(compact_repos(&base, 3).expect("compaction runs"), 0);

        assert_eq!(pack_count(&base.join("demo")), 3, "packs untouched");
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&source_base);
    }

    /// a repo still waiting on objects keeps its packs: its on-disk refs run
    /// behind the committed heads, so the closure compaction would keep is not
    /// the closure the repo is about to need.
    #[test]
    fn compaction_skips_a_repo_still_waiting_on_its_objects() {
        let base = tmp_base("compact-pending");
        let source_base = tmp_base("compact-pending-source");
        let head = seed_pushed_packs(&base, &source_base);
        let mut forge = Forge::init("forge", base.clone()).unwrap();
        forge
            .state
            .repos
            .get_mut("demo")
            .expect("the on-disk repo is adopted")
            .adopt_pending(refs::PendingMap::from([(
                refs::RefName::Branch(MAIN_BRANCH.into()),
                (head, [7u8; 32]),
            )]));
        forge.persist_pending().unwrap();

        assert_eq!(compact_repos(&base, 2).expect("compaction runs"), 0);

        assert_eq!(pack_count(&base.join("demo")), 3, "packs untouched");
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&source_base);
    }

    #[test]
    fn norm_repo_maps_empty_to_default_and_validates_slugs() {
        assert_eq!(norm_repo("").unwrap(), DEFAULT_REPO);
        assert_eq!(norm_repo("docs").unwrap(), "docs");
        assert_eq!(norm_repo("a.b_c-1").unwrap(), "a.b_c-1");
        for bad in [
            "Docs",
            "a/b",
            "a b",
            ".",
            "..",
            "a\0b",
            ".tracker.bin",
            ".pending.bin",
            ".stuck.txt",
            ".snapshot-cache.bin",
            ".hidden",
        ] {
            assert!(norm_repo(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(norm_repo(&"a".repeat(65)).is_err(), "65 bytes too long");
        assert!(norm_repo(&"a".repeat(64)).is_ok(), "64 bytes ok");
    }

    // a repo named after forge's own dot-prefixed state file must never reach
    // the stage: it would collide with `<base>/.tracker.bin` on disk and fail
    // `commit_block` on every node forever. the first tracker write on "demo"
    // puts `.tracker.bin` in the base dir; birthing a repo literally called
    // ".tracker.bin" must be refused AT THE PushRefs STAGE (by norm_repo), so
    // commit_block/publish_block never even sees it and stays Ok.
    #[test]
    fn repo_named_like_forges_own_state_file_is_refused_at_stage() {
        let base = tmp_base("dotfile-collision");
        let mut forge = Forge::init("forge", base.clone()).unwrap();

        let mut pusher = ctx_with_origin(1, user_origin(1));
        push(&mut forge, &mut pusher, "demo", "main", None, oid('a')).expect("birthing demo");
        exec_commit(
            &mut forge,
            &mut pusher,
            &ForgeMsg::OpenIssue {
                repo: "demo".into(),
                title: "first tracker write".into(),
                body: String::new(),
            },
        );
        assert!(
            base.join(".tracker.bin").is_file(),
            "precondition: tracker persisted"
        );

        let mut attacker = ctx_with_origin(2, user_origin(2));
        let err = push(
            &mut forge,
            &mut attacker,
            ".tracker.bin",
            "main",
            None,
            oid('b'),
        )
        .expect_err("a repo named after forge's own state file must be refused");
        let _ = err;

        // the node is NOT bricked: a normal publish_block still succeeds.
        let mut pusher_again = ctx_with_origin(3, user_origin(1));
        push(&mut forge, &mut pusher_again, "demo", "dev", None, oid('c'))
            .expect("publish_block stays Ok after the refused push");

        let _ = std::fs::remove_dir_all(&base);
    }

    // forge::norm_repo is shared with the git smart-HTTP bridge (git_http.rs)
    // — the same reject must hold there, since that route never touches
    // consensus's own PushRefs staging.
    #[test]
    fn bridge_shares_norm_repo_and_refuses_the_same_dotfile_name() {
        assert!(norm_repo(".tracker.bin").is_err());
        assert!(norm_repo(".pending.bin").is_err());
    }
}
