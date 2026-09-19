//! the push lane (`PushRefs`): a git-faithful ref update over consensus.
//!
//! forge's committed HEAD is decoupled from the on-disk libgit2 objects —
//! `root() = sha256(<HEAD oid>)`, a pure function of the oid alone. a push
//! exploits exactly that: the ONLY consensus effect is a compare-and-swap on
//! the committed HEAD; the git objects ride out-of-band in a NODE-LOCAL
//! packfile (fetched from the files blob store by digest) and are installed
//! lazily by `materialize`, never influencing root/accept-reject.
//!
//! the load-bearing test is
//! [`determinism_a_pushed_root_is_identical_without_the_pack`]: a forge whose
//! blob store LACKS the pack reaches the SAME root as one that has it. that is
//! the fork-safety invariant — pack possession is per-node, root is not.

use std::path::{Path, PathBuf};

use forge::Forge;
use forge::{
    ForgeMsg, ForgeQuery, ForgeReply, RefUpdate, TagCreate, decode_reply, encode_msg, encode_query,
};
use sdk::{Error, Module, Msg, StateRoot};

/// the module's canonical branch — the ref a materialized push moves.
const MAIN_REF: &str = "refs/heads/main";
/// the raw width of a sha1 oid (a push `prev_oid`/`new_oid` field).
const OID_LEN: usize = 20;
/// every op here targets the default repo, whose git dir is `base/default`.
const DEFAULT_REPO: &str = "default";

use sdk_testkit::TestCtx;

// forge's execute reads env (consensus_time / origin); me/height are cosmetic.
// a push binds a PRINCIPAL, so the origin must be an authenticated external
// key; with no `identity` handler registered it resolves to itself, and the
// same key births and then owns every repo here.
fn at(consensus_time: u64) -> TestCtx {
    TestCtx::with_env(sdk::Env {
        height: 0,
        consensus_time,
        origin: sdk::Origin::External(vec![1u8; 32]),
        me: "forge".into(),
        cause: sdk::Cause::Direct,
    })
}

fn tmp_repo(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("ducktape-forge-push-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

/// the default repo's git dir under a forge base.
fn repo_dir(base: &Path) -> PathBuf {
    base.join(DEFAULT_REPO)
}

/// parse forge's multi-branch snapshot container into `(name, main_oid, pack)`
/// entries — the test-side inverse of `Forge::snapshot` (per repo: name,
/// branch_count, per-branch name+oid, tag_count, per-tag name+oid, pending,
/// pack; the trailing tracker section is irrelevant here).
fn parse_container(bytes: &[u8]) -> Vec<(String, Vec<u8>, Vec<u8>)> {
    fn u32_at(bytes: &[u8], p: &mut usize) -> usize {
        let v = u32::from_le_bytes(bytes[*p..*p + 4].try_into().unwrap()) as usize;
        *p += 4;
        v
    }
    let mut p = 4; // skip the 4-byte "FGv1" container magic
    let count = u32_at(bytes, &mut p);
    let mut out = Vec::new();
    for _ in 0..count {
        let nl = u32_at(bytes, &mut p);
        let name = String::from_utf8(bytes[p..p + nl].to_vec()).unwrap();
        p += nl;
        let ref_count = u32_at(bytes, &mut p);
        let mut main_oid = Vec::new();
        for _ in 0..ref_count {
            let bl = u32_at(bytes, &mut p);
            let branch = String::from_utf8(bytes[p..p + bl].to_vec()).unwrap();
            p += bl;
            let oid = bytes[p..p + OID_LEN].to_vec();
            p += OID_LEN;
            if branch == "main" {
                main_oid = oid;
            }
        }
        let tag_count = u32_at(bytes, &mut p);
        for _ in 0..tag_count {
            let tl = u32_at(bytes, &mut p);
            p += tl + OID_LEN;
        }
        // the pending section: refs whose objects the sender did not hold.
        let pending_count = u32_at(bytes, &mut p);
        for _ in 0..pending_count {
            let bl = u32_at(bytes, &mut p);
            p += bl + OID_LEN + 32;
        }
        let pl = u32_at(bytes, &mut p);
        let pack = bytes[p..p + pl].to_vec();
        p += pl;
        out.push((name, main_oid, pack));
    }
    out
}

/// a captured push input: a real commit's head oid, the packfile of its full
/// object closure (exactly what `snapshot()` carries after its 20-byte oid
/// header — the same closure `install` consumes, so materialize's git plumbing
/// is proven the way state sync is), and the source `root()` that head produced
/// (== `sha256(head oid)`, the value a faithful push must reproduce).
struct Captured {
    head: Vec<u8>,
    pack: Vec<u8>,
    root: StateRoot,
}
impl Captured {
    /// the head oid as a git2 `Oid`.
    fn oid(&self) -> git2::Oid {
        git2::Oid::from_bytes(&self.head).unwrap()
    }
    /// store the pack in a blob handle and return the digest to push with.
    fn stash(&self, blobs: &blobstore::BlobHandle) -> Vec<u8> {
        blobs.put_chunk(self.pack.clone()).to_vec()
    }
}

/// capture the current committed head of `src` (oid + full-closure pack + root).
/// the head + pack come straight out of `snapshot()` (parsed from the single
/// default-repo container) so materialize's git plumbing is proven the same way
/// state sync is.
fn capture(src: &Forge) -> Captured {
    let snap = src.snapshot().unwrap();
    let mut entries = parse_container(&snap);
    assert_eq!(entries.len(), 1, "a single born default repo");
    let (name, head, pack) = entries.remove(0);
    assert_eq!(name, DEFAULT_REPO);
    assert!(!pack.is_empty(), "a born head must carry a pack");
    Captured {
        head,
        pack,
        root: src.root(),
    }
}

/// Build Git history off-chain, then drive every resulting head through the
/// real PushRefs path. This is the production consensus/data-plane split in
/// miniature.
fn source_history(
    tag: &str,
    changes: &[(u64, &str, &str, &str)],
) -> (PathBuf, Forge, Vec<Captured>) {
    let commits = forge::testkit::history(tag, changes);
    let blobs = blobstore::BlobHandle::default();
    let digests = commits
        .iter()
        .map(|commit| blobs.put_chunk(commit.pack.clone()).to_vec())
        .collect::<Vec<_>>();
    let dir = tmp_repo(tag);
    let mut forge = Forge::with_blobs("forge", dir.clone(), blobs).unwrap();
    let mut prev = None;
    let mut captures = Vec::with_capacity(commits.len());
    for (commit, digest) in commits.iter().zip(digests) {
        push(&mut forge, prev.as_deref(), &commit.head, &digest);
        captures.push(capture(&forge));
        prev = Some(commit.head.clone());
    }
    (dir, forge, captures)
}

/// build a single-update `PushRefs` on `branch` of the default repo (empty `repo`).
fn push_branch_msg(branch: &str, prev: Option<&[u8]>, new: &[u8], digest: &[u8]) -> Msg {
    Msg {
        target: "forge".into(),
        payload: encode_msg(&ForgeMsg::PushRefs {
            repo: String::new(),
            updates: vec![RefUpdate {
                ref_name: branch.into(),
                prev_oid: prev.map(<[u8]>::to_vec),
                new_oid: Some(new.to_vec()),
            }],
            pack_digest: Some(digest.to_vec()),
            tags: Vec::new(),
            cert: None,
        }),
    }
}

/// build a single-update `PushRefs` on `main` of the default repo (empty `repo`).
fn push_msg(prev: Option<&[u8]>, new: &[u8], digest: &[u8]) -> Msg {
    push_branch_msg("main", prev, new, digest)
}

/// execute a Push and publish the block — the happy path.
fn push(forge: &mut Forge, prev: Option<&[u8]>, new: &[u8], digest: &[u8]) {
    futures::executor::block_on(forge.execute(&mut at(0), &push_msg(prev, new, digest))).unwrap();
    futures::executor::block_on(forge.commit_block()).unwrap();
}

/// execute a Push WITHOUT publishing — used to observe a rejection in isolation
/// (a rejected op never stages, so there is nothing to commit).
fn try_push(
    forge: &mut Forge,
    prev: Option<&[u8]>,
    new: &[u8],
    digest: &[u8],
) -> Result<(), Error> {
    futures::executor::block_on(forge.execute(&mut at(0), &push_msg(prev, new, digest)))
}

/// every `(name, head)` pair `query(ListRepos)` reports.
fn repo_heads(forge: &Forge) -> Vec<(String, Option<String>)> {
    let reply =
        futures::executor::block_on(forge.query(&encode_query(&ForgeQuery::ListRepos))).unwrap();
    match decode_reply(&reply).unwrap() {
        ForgeReply::Repos(repos) => repos.into_iter().map(|r| (r.name, r.head)).collect(),
        other => panic!("expected Repos, got {other:?}"),
    }
}

/// the current head hex reported by `query(Head)`.
fn head_query(forge: &Forge) -> Option<String> {
    let reply = futures::executor::block_on(forge.query(&encode_query(&ForgeQuery::Head))).unwrap();
    match decode_reply(&reply).unwrap() {
        ForgeReply::Head(h) => h,
        other => panic!("expected Head, got {other:?}"),
    }
}

/// the on-disk `MAIN_REF` oid of the default repo, or `None` if unborn (or the
/// repo dir doesn't exist yet) — the independent oracle that materialization
/// really moved (or did not move) the real ref.
fn on_disk_head(base: &Path) -> Option<git2::Oid> {
    git2::Repository::open(repo_dir(base))
        .ok()?
        .refname_to_id(MAIN_REF)
        .ok()
}

/// read a file's bytes out of the default repo's materialized head tree.
fn read_blob(base: &Path, name: &str) -> Vec<u8> {
    let repo = git2::Repository::open(repo_dir(base)).unwrap();
    let head = repo.refname_to_id(MAIN_REF).unwrap();
    let tree = repo.find_commit(head).unwrap().tree().unwrap();
    let entry = tree
        .get_name(name)
        .unwrap_or_else(|| panic!("{name} missing from materialized tree"));
    repo.find_blob(entry.id()).unwrap().content().to_vec()
}

/// a single-commit source module + its captured push input.
fn source_one(tag: &str) -> (PathBuf, Forge, Captured) {
    let (dir, src, mut captures) = source_history(tag, &[(1, "a.txt", "hello", "first")]);
    let cap = captures.remove(0);
    (dir, src, cap)
}

// ---------------------------------------------------------------------------

#[test]
fn push_to_unborn_moves_head_and_materializes_content() {
    let (src_dir, _src, cap) = source_one("unborn-src");
    assert_ne!(cap.root, StateRoot::ZERO, "source has real state");

    // a blob store that HOLDS the pack (the submitter's situation).
    let blobs = blobstore::BlobHandle::default();
    let digest = cap.stash(&blobs);

    let dst_dir = tmp_repo("unborn-dst");
    let mut dst = Forge::with_blobs("forge", dst_dir.clone(), blobs).unwrap();
    assert_eq!(dst.root(), StateRoot::ZERO, "unborn remote");

    push(&mut dst, None, &cap.head, &digest);

    // (a) root moved to sha256(new_oid) == the source root.
    assert_eq!(dst.root(), cap.root, "root becomes sha256(new_oid)");
    // (b) Head query returns the new oid hex.
    assert_eq!(head_query(&dst), Some(cap.oid().to_string()));
    // (c) content materialized: the REAL on-disk ref moved and the blob reads back.
    assert_eq!(on_disk_head(&dst_dir), Some(cap.oid()), "ref materialized");
    assert_eq!(read_blob(&dst_dir, "a.txt"), b"hello".to_vec());

    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&dst_dir);
}

#[test]
fn fast_forward_push_advances_the_head() {
    // a source that advances twice, captured after each pushed commit.
    let (src_dir, _src, mut captures) = source_history(
        "ff-src",
        &[(1, "a.txt", "one", "c1"), (2, "b.txt", "two", "c2")],
    );
    let c1 = captures.remove(0);
    let c2 = captures.remove(0);

    let blobs = blobstore::BlobHandle::default();
    let d1 = c1.stash(&blobs);
    let d2 = c2.stash(&blobs);

    let dst_dir = tmp_repo("ff-dst");
    let mut dst = Forge::with_blobs("forge", dst_dir.clone(), blobs).unwrap();

    push(&mut dst, None, &c1.head, &d1);
    assert_eq!(dst.root(), c1.root);
    assert_eq!(on_disk_head(&dst_dir), Some(c1.oid()));

    // fast-forward: prev == the current committed head.
    push(&mut dst, Some(&c1.head), &c2.head, &d2);
    assert_eq!(dst.root(), c2.root, "head advanced to the second commit");
    assert_eq!(on_disk_head(&dst_dir), Some(c2.oid()), "ref fast-forwarded");
    assert_eq!(read_blob(&dst_dir, "b.txt"), b"two".to_vec());
    // history came through the closure pack, not just the tip.
    let repo = git2::Repository::open(repo_dir(&dst_dir)).unwrap();
    assert_eq!(
        repo.find_commit(c2.oid()).unwrap().parent(0).unwrap().id(),
        c1.oid(),
        "the pushed tip descends from the prior head"
    );

    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&dst_dir);
}

#[test]
fn stale_prev_oid_is_rejected_and_head_is_unchanged() {
    let (src_dir, _src, mut captures) = source_history(
        "stale-src",
        &[(1, "a.txt", "one", "c1"), (2, "b.txt", "two", "c2")],
    );
    let c1 = captures.remove(0);
    let c2 = captures.remove(0);

    let blobs = blobstore::BlobHandle::default();
    let d1 = c1.stash(&blobs);
    let d2 = c2.stash(&blobs);

    let dst_dir = tmp_repo("stale-dst");
    let mut dst = Forge::with_blobs("forge", dst_dir.clone(), blobs).unwrap();
    push(&mut dst, None, &c1.head, &d1);
    let pinned = dst.root();

    // (a) prev = None but the head is born -> non-fast-forward.
    let err = try_push(&mut dst, None, &c2.head, &d2).unwrap_err();
    assert!(matches!(err, Error::Module { ref reason, .. } if reason == "non_fast_forward"));
    assert_eq!(dst.root(), pinned, "a rejected push must not move the head");

    // (b) prev = a wrong 20-byte oid -> also non-fast-forward.
    let bogus = [0x11u8; OID_LEN];
    let err = try_push(&mut dst, Some(&bogus), &c2.head, &d2).unwrap_err();
    assert!(matches!(err, Error::Module { ref reason, .. } if reason == "non_fast_forward"));
    assert_eq!(dst.root(), pinned);
    assert_eq!(
        on_disk_head(&dst_dir),
        Some(c1.oid()),
        "on-disk ref pinned too"
    );

    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&dst_dir);
}

#[test]
fn determinism_a_pushed_root_is_identical_without_the_pack() {
    // THE load-bearing invariant: a validator WITHOUT the pack reaches the same
    // root as one WITH it. pack possession is per-node; root is not.
    let (src_dir, _src, cap) = source_one("det-src");

    // node WITH the pack.
    let with_blobs = blobstore::BlobHandle::default();
    let digest = cap.stash(&with_blobs);
    let with_dir = tmp_repo("det-with");
    let mut with = Forge::with_blobs("forge", with_dir.clone(), with_blobs).unwrap();
    push(&mut with, None, &cap.head, &digest);

    // node WITHOUT the pack: an EMPTY blob store, but the SAME op (same digest).
    let without_blobs = blobstore::BlobHandle::default();
    assert!(
        !without_blobs.has_chunk(&<[u8; 32]>::try_from(digest.as_slice()).unwrap()),
        "this store must not hold the pack"
    );
    let without_dir = tmp_repo("det-without");
    let mut without = Forge::with_blobs("forge", without_dir.clone(), without_blobs).unwrap();
    push(&mut without, None, &cap.head, &digest);

    // identical roots — the fork-safety property.
    assert_eq!(
        without.root(),
        cap.root,
        "root is sha256(new_oid), pack-free"
    );
    assert_eq!(
        without.root(),
        with.root(),
        "the pack-less node MUST match the pack-holding node's root"
    );
    // and Head reports the new oid on both.
    assert_eq!(head_query(&without), Some(cap.oid().to_string()));
    assert_eq!(head_query(&without), head_query(&with));

    // the difference is ONLY node-local: content is not yet materialized on the
    // pack-less node (its on-disk ref stays unborn), while the pack-holder has it.
    assert_eq!(
        on_disk_head(&without_dir),
        None,
        "no pack -> ref stays behind"
    );
    assert_eq!(
        on_disk_head(&with_dir),
        Some(cap.oid()),
        "pack -> materialized"
    );

    // a later materialize with the pack STILL absent is a safe no-op: root stays
    // correct, ref stays behind, no error.
    without.materialize().unwrap();
    assert_eq!(without.root(), cap.root);
    assert_eq!(on_disk_head(&without_dir), None);

    // once the pack arrives, a retry catches the on-disk ref up — root unchanged.
    // no second push is involved: the reopened node ALREADY holds the committed
    // head (the catch-up map is durable) and knows the digest it is waiting on,
    // so materialize alone closes the gap. re-pushing here would be a
    // non-fast-forward, exactly as it would be against any other node.
    let arrived = blobstore::BlobHandle::default();
    let d2 = cap.stash(&arrived); // digest is identical (content-addressed)
    assert_eq!(d2, digest, "content-addressed digest is stable");
    let mut caught_up = Forge::with_blobs("forge", without_dir.clone(), arrived).unwrap();
    assert_eq!(
        caught_up.root(),
        cap.root,
        "the reopened node keeps the committed head"
    );
    caught_up.materialize().unwrap();
    assert_eq!(caught_up.root(), cap.root, "root still sha256(new_oid)");
    assert_eq!(
        on_disk_head(&without_dir),
        Some(cap.oid()),
        "the pack now present -> ref materialized"
    );

    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&with_dir);
    let _ = std::fs::remove_dir_all(&without_dir);
}

#[test]
fn malformed_push_fields_are_rejected_deterministically() {
    let dst_dir = tmp_repo("malformed-dst");
    let mut dst = Forge::init("forge", dst_dir.clone()).unwrap();

    let ok_oid = [0u8; OID_LEN];
    let ok_digest = [0u8; 32];

    // new_oid too short.
    let err = try_push(&mut dst, None, &[0u8; 19], &ok_digest).unwrap_err();
    assert!(
        matches!(err, Error::Module { ref reason, ref sentence } if reason == "bad_oid" && sentence.contains("new_oid"))
    );

    // new_oid too long.
    let err = try_push(&mut dst, None, &[0u8; 21], &ok_digest).unwrap_err();
    assert!(
        matches!(err, Error::Module { ref reason, ref sentence } if reason == "bad_oid" && sentence.contains("new_oid"))
    );

    // pack_digest not 32 bytes.
    let err = try_push(&mut dst, None, &ok_oid, &[0u8; 31]).unwrap_err();
    assert!(matches!(err, Error::Module { ref reason, .. } if reason == "bad_pack_digest"));

    // prev_oid present but wrong length.
    let err = try_push(&mut dst, Some(&[0u8; 10]), &ok_oid, &ok_digest).unwrap_err();
    assert!(
        matches!(err, Error::Module { ref reason, ref sentence } if reason == "bad_oid" && sentence.contains("prev_oid"))
    );

    // every rejection left the module untouched (still unborn, never staged).
    assert_eq!(dst.root(), StateRoot::ZERO);
    assert_eq!(head_query(&dst), None);
    assert_eq!(on_disk_head(&dst_dir), None);

    let _ = std::fs::remove_dir_all(&dst_dir);
}

/// `list_repos` answers the INTEGRATION head: a main-only repo falls back to
/// the main head, but once `dev` is born the listing reports dev's oid —
/// the branch every browse surface reads, so a dev-only repo must never list
/// as unborn to a remote client.
#[test]
fn list_repos_reports_the_integration_head() {
    let (src_dir, _src, mut captures) = source_history(
        "listrepos-src",
        &[(1, "a.txt", "one", "c1"), (2, "b.txt", "two", "c2")],
    );
    let c1 = captures.remove(0);
    let c2 = captures.remove(0);

    let dst_dir = tmp_repo("listrepos-dst");
    let blobs = blobstore::BlobHandle::default();
    let d1 = c1.stash(&blobs);
    let d2 = c2.stash(&blobs);
    let mut dst = Forge::with_blobs("forge", dst_dir.clone(), blobs).unwrap();
    push(&mut dst, None, &c1.head, &d1);
    push(&mut dst, Some(&c1.head), &c2.head, &d2);
    let main_head = on_disk_head(&dst_dir).unwrap();

    // main-only: the listing falls back to the main head.
    assert_eq!(
        repo_heads(&dst),
        vec![("default".into(), Some(main_head.to_string()))]
    );

    // dev born at c1 (≠ main's c2): the integration branch owns the listing.
    futures::executor::block_on(
        dst.execute(&mut at(3), &push_branch_msg("dev", None, &c1.head, &d1)),
    )
    .unwrap();
    futures::executor::block_on(dst.commit_block()).unwrap();
    assert_ne!(c1.oid(), main_head, "the two branches must diverge");
    assert_eq!(
        repo_heads(&dst),
        vec![("default".into(), Some(c1.oid().to_string()))]
    );

    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&dst_dir);
}

#[test]
fn a_restart_without_the_pack_keeps_the_committed_head() {
    // the committed head is CONSENSUS state; the on-disk git ref is a NODE-LOCAL
    // cache that legitimately lags whenever the pack has not arrived. re-deriving
    // the committed map from that lagging cache at boot silently rewinds this
    // node's forge root — and recovery then fail-stops on
    // "recomposed root_hash != sealed tip", bricking a node that was healthy.
    let (src_dir, _src, cap) = source_one("restart-src");

    let dir = tmp_repo("restart-nopack");
    let blobs = blobstore::BlobHandle::default();
    let digest = cap.stash(&blobstore::BlobHandle::default()); // digest only; NOT in `blobs`
    let mut node = Forge::with_blobs("forge", dir.clone(), blobs).unwrap();
    push(&mut node, None, &cap.head, &digest);
    assert_eq!(
        node.root(),
        cap.root,
        "committed head is live before the restart"
    );
    assert_eq!(on_disk_head(&dir), None, "no pack -> the ref cache lags");
    drop(node);

    // RESTART over the same directory, pack still absent.
    let reopened =
        Forge::with_blobs("forge", dir.clone(), blobstore::BlobHandle::default()).unwrap();
    assert_eq!(
        reopened.root(),
        cap.root,
        "a restart must not rewind the committed head to the lagging ref cache",
    );
    assert_eq!(
        head_query(&reopened),
        Some(cap.oid().to_string()),
        "the committed head survives the restart",
    );

    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_pending_branch_still_snapshots() {
    // `snapshot()` feeds BOTH recovery checkpointing and serving a joiner. a head
    // whose objects this node does not hold is a state forge MODELS on purpose
    // (that is the fork-safety invariant above) — so it must be serializable,
    // not an error that stops this node checkpointing and admitting members.
    let (src_dir, _src, cap) = source_one("snap-pending-src");

    let dir = tmp_repo("snap-pending");
    let digest = cap.stash(&blobstore::BlobHandle::default());
    let mut node =
        Forge::with_blobs("forge", dir.clone(), blobstore::BlobHandle::default()).unwrap();
    push(&mut node, None, &cap.head, &digest);

    let snap = node
        .snapshot()
        .expect("a committed head whose objects are absent must still serialize");

    // and it round-trips onto a fresh namespace at the same root — the joiner
    // lands in the SAME node-local state the server is in, pending and all.
    let rt = tmp_repo("snap-pending-rt");
    let mut fresh =
        Forge::with_blobs("forge", rt.clone(), blobstore::BlobHandle::default()).unwrap();
    fresh.install(&snap, cap.root).expect("install");
    assert_eq!(fresh.root(), cap.root);
    assert_eq!(on_disk_head(&rt), None, "the receiver is pending too");

    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&rt);
}

#[test]
fn the_catch_up_map_clears_on_arrival_and_a_corrupt_one_is_fail_stop() {
    let (src_dir, _src, cap) = source_one("pendfile-src");
    let dir = tmp_repo("pendfile");
    let pending_file = dir.join(".pending.bin");

    let digest = cap.stash(&blobstore::BlobHandle::default());
    let mut node =
        Forge::with_blobs("forge", dir.clone(), blobstore::BlobHandle::default()).unwrap();
    push(&mut node, None, &cap.head, &digest);
    assert!(
        pending_file.exists(),
        "an outstanding head must be durable, not RAM-only"
    );

    // a tampered map is FAIL-STOP, exactly like a corrupt tracker: booting on a
    // rewound or invented branch map composes a wrong root either way.
    let good = std::fs::read(&pending_file).unwrap();
    std::fs::write(&pending_file, &good[..good.len() - 1]).unwrap();
    assert!(
        Forge::with_blobs("forge", dir.clone(), blobstore::BlobHandle::default()).is_err(),
        "a truncated catch-up map must refuse to boot"
    );
    std::fs::write(&pending_file, b"XXXX").unwrap();
    assert!(
        Forge::with_blobs("forge", dir.clone(), blobstore::BlobHandle::default()).is_err(),
        "a catch-up map without the magic must refuse to boot"
    );

    // once the pack arrives nothing is outstanding, so the file goes away.
    std::fs::write(&pending_file, &good).unwrap();
    let arrived = blobstore::BlobHandle::default();
    cap.stash(&arrived);
    let mut caught_up = Forge::with_blobs("forge", dir.clone(), arrived).unwrap();
    caught_up.materialize().unwrap();
    assert_eq!(on_disk_head(&dir), Some(cap.oid()), "materialized");
    assert!(
        !pending_file.exists(),
        "nothing outstanding -> no stale file to re-adopt from"
    );
    assert_eq!(caught_up.root(), cap.root);

    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn materialize_releases_the_pack_once_the_objects_land() {
    // the pack is a courier. once its objects are in the odb every reader —
    // the git fetch lane, a PR diff, a peer's catch-up — takes them from
    // THERE, so a second copy in the blob store is pure duplication that
    // nothing ever released.
    let (src_dir, _src, cap) = source_one("release-src");
    let dir = tmp_repo("release");
    let blobs = blobstore::BlobHandle::default();
    let digest = cap.stash(&blobs);
    let staged: [u8; 32] = digest.clone().try_into().unwrap();
    assert!(blobs.has_chunk(&staged), "staged for the push");

    let mut node = Forge::with_blobs("forge", dir.clone(), blobs.clone()).unwrap();
    push(&mut node, None, &cap.head, &digest);
    node.materialize().unwrap();

    assert!(
        forge::pending_branches(&dir).unwrap().is_empty(),
        "the branch materialized",
    );
    assert!(
        !blobs.has_chunk(&staged),
        "and the pack it no longer needs is released",
    );

    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pending_branches_is_the_node_side_pull_handle() {
    // the node's blob plane reads this WITHOUT opening the module: node-local
    // possession must never ride the deterministic `Module` surface, because a
    // block that could read it would fork.
    let (src_dir, _src, cap) = source_one("digests-src");
    let dir = tmp_repo("digests");

    assert!(
        forge::pending_branches(&dir).unwrap().is_empty(),
        "a workspace with no file is not an error",
    );

    let digest = cap.stash(&blobstore::BlobHandle::default());
    let mut node =
        Forge::with_blobs("forge", dir.clone(), blobstore::BlobHandle::default()).unwrap();
    push(&mut node, None, &cap.head, &digest);

    let want: [u8; 32] = digest.clone().try_into().unwrap();
    let outstanding = forge::pending_branches(&dir).unwrap();
    assert_eq!(outstanding.len(), 1, "one branch is waiting");
    assert_eq!(
        (
            outstanding[0].digest,
            outstanding[0].head.as_bytes().as_slice()
        ),
        (want, cap.head.as_slice()),
        "the outstanding pack AND the head it explains are both visible from \
         outside the module",
    );

    // and it goes quiet once the objects land.
    let arrived = blobstore::BlobHandle::default();
    cap.stash(&arrived);
    let mut caught_up = Forge::with_blobs("forge", dir.clone(), arrived).unwrap();
    caught_up.materialize().unwrap();
    assert!(forge::pending_branches(&dir).unwrap().is_empty());

    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&dir);
}

/// the packfiles a repo's object dir holds.
fn pack_count(base: &Path) -> usize {
    let dir = repo_dir(base).join(".git").join("objects").join("pack");
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
                .count()
        })
        .unwrap_or(0)
}

/// stage a push whose head NO pack closes: the CAS is pure, so consensus takes
/// it, and the branch is left pending on a verdict that can never change.
fn push_poison(forge: &mut Forge, branch: &str, digest: &[u8]) {
    let head = [7u8; OID_LEN];
    futures::executor::block_on(
        forge.execute(&mut at(0), &push_branch_msg(branch, None, &head, digest)),
    )
    .unwrap();
    futures::executor::block_on(forge.commit_block()).unwrap();
}

#[test]
fn a_head_no_pack_closes_is_installed_once_and_then_left_alone() {
    // consensus cannot open a pack, so a push may name a head its pack does not
    // contain. the node-local catch-up must not re-index that pack on every
    // block for the rest of the repo's life.
    let (src_dir, _src, cap) = source_one("poison-src");
    let dir = tmp_repo("poison");
    let blobs = blobstore::BlobHandle::default();
    let digest = cap.stash(&blobs);

    let mut node = Forge::with_blobs("forge", dir.clone(), blobs.clone()).unwrap();
    push_poison(&mut node, "poison", &digest);

    assert_eq!(pack_count(&dir), 1, "the pack installed once");
    assert_eq!(
        forge::pending_branches(&dir).unwrap().len(),
        1,
        "the closure is short, so the branch stays pending"
    );

    // wipe the evidence: a second install would put it straight back.
    let pack_dir = repo_dir(&dir).join(".git").join("objects").join("pack");
    std::fs::remove_dir_all(&pack_dir).unwrap();
    std::fs::create_dir_all(&pack_dir).unwrap();
    node.materialize().unwrap();
    node.materialize().unwrap();
    assert_eq!(pack_count(&dir), 0, "the futile pack is never re-installed");

    // a NEW push to the branch is the one thing that can change the verdict, so
    // it clears the mark — and this one names a head the same pack DOES close.
    let poison_head = vec![7u8; OID_LEN];
    futures::executor::block_on(node.execute(
        &mut at(0),
        &push_branch_msg("poison", Some(&poison_head), &cap.head, &digest),
    ))
    .unwrap();
    futures::executor::block_on(node.commit_block()).unwrap();
    assert_eq!(pack_count(&dir), 1, "the fresh push installs again");
    assert!(
        forge::pending_branches(&dir).unwrap().is_empty(),
        "and this head's closure verifies, so nothing is left waiting"
    );

    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_stuck_branch_does_not_disable_the_repos_compaction() {
    // one unresolvable head used to skip the WHOLE repo forever, and packs cost
    // 212 MB at 1001 where the same history costs 6 MB in one.
    // two unrelated histories on two branches, so one pack per branch and a
    // collapse that really has something to collapse.
    let (dir_a, _a, mut cap_a) = source_history("compact-a", &[(1, "a.txt", "one", "c1")]);
    let (dir_b, _b, mut cap_b) = source_history("compact-b", &[(2, "b.txt", "two", "c2")]);
    let main = cap_a.remove(0);
    let feat = cap_b.remove(0);

    // a node with both packs materializes both branches, and the pair is worth
    // collapsing — the baseline both halves below measure against.
    let two_branch_node = |tag: &str| {
        let dir = tmp_repo(tag);
        let blobs = blobstore::BlobHandle::default();
        let blobs_for_later = blobs.clone();
        let dm = main.stash(&blobs);
        let df = feat.stash(&blobs);
        let mut node = Forge::with_blobs("forge", dir.clone(), blobs).unwrap();
        push(&mut node, None, &main.head, &dm);
        futures::executor::block_on(
            node.execute(&mut at(0), &push_branch_msg("feat", None, &feat.head, &df)),
        )
        .unwrap();
        futures::executor::block_on(node.commit_block()).unwrap();
        assert_eq!(pack_count(&dir), 2, "one pack per materialized branch");
        (dir, node, blobs_for_later)
    };

    // a branch still WAITING on its objects keeps holding compaction back: its
    // closure is the one the repo is about to need.
    let (waiting_dir, mut lagging, _) = two_branch_node("compact-waiting");
    let unheld = [9u8; 32];
    futures::executor::block_on(lagging.execute(
        &mut at(0),
        &push_branch_msg("later", None, &[7u8; OID_LEN], &unheld),
    ))
    .unwrap();
    futures::executor::block_on(lagging.commit_block()).unwrap();
    assert_eq!(
        forge::compact_repos(&waiting_dir, 0).unwrap(),
        0,
        "a repo whose branches are still in flight is left alone"
    );

    // a STUCK one does not: its pack is installed and refused, so nothing is
    // coming for it and the whole repo must not wait on it. (a materialized
    // push RELEASES its pack, so the poison push needs one of its own.)
    let (dir_c, _c, mut cap_c) = source_history("compact-c", &[(3, "c.txt", "three", "c3")]);
    let (dir, mut node, blobs) = two_branch_node("compact-stuck");
    let dc = cap_c.remove(0).stash(&blobs);
    push_poison(&mut node, "poison", &dc);
    assert!(
        forge::compact_repos(&dir, 0).unwrap() > 0,
        "the repo still compacts"
    );
    assert_eq!(pack_count(&dir), 1, "collapsed into one pack");

    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
    let _ = std::fs::remove_dir_all(&dir_c);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&waiting_dir);
}

/// a tag push round trip. an ANNOTATED tag rides one push beside the branch it
/// tags, in the one pack `git push` sends; it lands in consensus, materializes
/// as `refs/tags/<name>` with the tag object itself (not only the commit it
/// peels to), is listed by `ListTags`, and reaches a fresh node through the
/// snapshot — whose pack must carry the tag object too — and a restart.
#[test]
fn an_annotated_tag_pushes_materializes_and_syncs() {
    let src_dir = tmp_repo("tag-src");
    let src = git2::Repository::init(&src_dir).unwrap();
    let sig =
        git2::Signature::new("ducktape", "ducktape@localhost", &git2::Time::new(1, 0)).unwrap();
    let blob = src.blob(b"one").unwrap();
    let mut tree = src.treebuilder(None).unwrap();
    tree.insert("a.txt", blob, 0o100644).unwrap();
    let tree = src.find_tree(tree.write().unwrap()).unwrap();
    let commit = src
        .commit(Some(MAIN_REF), &sig, &sig, "one", &tree, &[])
        .unwrap();
    let target = src.find_object(commit, None).unwrap();
    let tag = src.tag("v1", &target, &sig, "release one", false).unwrap();
    // what git sends: the commit's closure plus the tag object.
    let mut packer = src.packbuilder().unwrap();
    let mut walk = src.revwalk().unwrap();
    walk.push(commit).unwrap();
    for object in walk {
        packer.insert_commit(object.unwrap()).unwrap();
    }
    packer.insert_object(tag, None).unwrap();
    let mut pack = git2::Buf::new();
    packer.write_buf(&mut pack).unwrap();

    let blobs = blobstore::BlobHandle::default();
    let digest = blobs.put_chunk(pack.to_vec()).to_vec();
    let base = tmp_repo("tag-dst");
    let mut node = Forge::with_blobs("forge", base.clone(), blobs).unwrap();
    let born = |oid: git2::Oid, name: &str| RefUpdate {
        ref_name: name.into(),
        prev_oid: None,
        new_oid: Some(oid.as_bytes().to_vec()),
    };
    let msg = Msg {
        target: "forge".into(),
        payload: encode_msg(&ForgeMsg::PushRefs {
            repo: String::new(),
            updates: vec![born(commit, "main")],
            tags: vec![TagCreate {
                name: "v1".into(),
                oid: tag.as_bytes().to_vec(),
            }],
            pack_digest: Some(digest),
            cert: None,
        }),
    };
    futures::executor::block_on(node.execute(&mut at(1), &msg)).unwrap();
    futures::executor::block_on(node.commit_block()).unwrap();

    let on_disk = git2::Repository::open(repo_dir(&base)).unwrap();
    assert_eq!(on_disk.refname_to_id(MAIN_REF).unwrap(), commit);
    assert_eq!(on_disk.refname_to_id("refs/tags/v1").unwrap(), tag);
    assert_eq!(on_disk.find_tag(tag).unwrap().target_id(), commit);
    let listed = futures::executor::block_on(node.query(&encode_query(&ForgeQuery::ListTags {
        repo: String::new(),
    })))
    .unwrap();
    assert_eq!(
        decode_reply(&listed).unwrap(),
        ForgeReply::Tags(vec![forge::TagRef {
            name: "v1".into(),
            oid: tag.to_string(),
        }])
    );

    let fresh_base = tmp_repo("tag-fresh");
    let mut fresh = Forge::init("forge", fresh_base.clone()).unwrap();
    fresh
        .install(&node.snapshot().unwrap(), node.root())
        .unwrap();
    assert_eq!(fresh.root(), node.root());
    let fresh_repo = git2::Repository::open(repo_dir(&fresh_base)).unwrap();
    assert_eq!(fresh_repo.refname_to_id("refs/tags/v1").unwrap(), tag);
    assert!(
        fresh_repo.find_tag(tag).is_ok(),
        "the snapshot pack carries the tag object"
    );
    drop(fresh);
    let reopened = Forge::init("forge", fresh_base.clone()).unwrap();
    assert_eq!(
        reopened.root(),
        node.root(),
        "a restart re-adopts the tag off disk"
    );

    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&fresh_base);
}
