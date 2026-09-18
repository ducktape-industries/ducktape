//! pack state-sync: a fresh forge reconstructs a source namespace's committed
//! state from SELF-CONTAINED snapshot bytes — a repo-count then, per repo, its
//! name, 20-byte head oid, and the head's full object closure as a packfile —
//! and lands on the identical composed root() with the real commit content
//! intact. the bytes are the whole story: nothing here assumes the two nodes
//! share a filesystem, a remote, or a `git` binary.
//!
//! the snapshot is also the module's byzantine surface: install() consumes bytes
//! a malicious peer chose. the negative tests pin the two layers of the defense —
//! the composed head oids must rehash to the expected root BEFORE any byte is
//! written, and each pack must hash-verify object by object BEFORE any ref moves
//! — by asserting the rejected module's root AND its on-disk repos stay untouched.
//!
//! these tests drive the SINGLE default repo (`repo: ""` is its canonical
//! address); the multi-repo container is exercised end-to-end in `multi_repo.rs`.

use std::path::{Path, PathBuf};

use forge::Forge;
use forge::{ForgeMsg, RefUpdate, encode_msg};
use sdk::{Error, Module, Msg, StateRoot};

/// the module's canonical branch — the ref install must (and may only) move.
const MAIN_REF: &str = "refs/heads/main";

/// the snapshot's per-repo head-oid width (a raw sha1 oid).
const OID_LEN: usize = 20;

/// the default repo — its git dir is `base/default`.
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

fn tmp_base(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("ducktape-forge-sync-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

/// the default repo's git dir under a forge base.
fn repo_dir(base: &Path) -> PathBuf {
    base.join(DEFAULT_REPO)
}

fn push(forge: &mut Forge, prev: Option<&[u8]>, new: &[u8], digest: &[u8]) {
    let msg = Msg {
        target: "forge".into(),
        payload: encode_msg(&ForgeMsg::PushRefs {
            repo: String::new(),
            updates: vec![RefUpdate {
                ref_name: "main".into(),
                prev_oid: prev.map(<[u8]>::to_vec),
                new_oid: Some(new.to_vec()),
            }],
            pack_digest: Some(digest.to_vec()),
            tags: Vec::new(),
            cert: None,
        }),
    };
    futures::executor::block_on(forge.execute(&mut at(0), &msg)).unwrap();
    futures::executor::block_on(forge.commit_block()).unwrap();
}

/// a two-commit source module — history matters (the pack must ship the whole
/// closure, not just the tip).
fn source(tag: &str) -> (PathBuf, Forge) {
    let base = tmp_base(tag);
    let commits =
        forge::testkit::history(tag, &[(1, "a.txt", "one", "c1"), (2, "b.txt", "two", "c2")]);
    let blobs = blobstore::BlobHandle::default();
    let digests = commits
        .iter()
        .map(|commit| blobs.put_chunk(commit.pack.clone()).to_vec())
        .collect::<Vec<_>>();
    let mut forge = Forge::with_blobs("forge", base.clone(), blobs).unwrap();
    push(&mut forge, None, &commits[0].head, &digests[0]);
    push(
        &mut forge,
        Some(&commits[0].head),
        &commits[1].head,
        &digests[1],
    );
    (base, forge)
}

/// byte offset of the FIRST repo's head oid in a container: `magic` (4) +
/// `count` (4) + `name_len` (4) + `name`. valid only for a container with
/// >= 1 repo.
fn first_oid_offset(bytes: &[u8]) -> usize {
    let name_len = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    // magic(4) count(4) name_len(4) name ref_count(4) branch_len(4) branch [oid]
    let p = 12 + name_len + 4;
    let branch_len = u32::from_le_bytes(bytes[p..p + 4].try_into().unwrap()) as usize;
    p + 4 + branch_len
}

/// byte offset of the FIRST repo's pack in a one-branch, tag-less container
/// with nothing pending: after its oid, the tag count (4), the pending count
/// (4) and the pack length (4).
fn first_pack_offset(bytes: &[u8]) -> usize {
    first_oid_offset(bytes) + OID_LEN + 12
}

/// assemble a one-repo, one-branch (`main`) container with an EMPTY tracker
/// section: `FGv1 [count=1][name][branch_count=1]["main" oid][tag_count=0]
/// [pending_count=0][pack_len pack][tracker]`.
fn build_container(name: &str, oid: &[u8], pack: &[u8]) -> Vec<u8> {
    let mut out = b"FGv1".to_vec();
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&4u32.to_le_bytes());
    out.extend_from_slice(b"main");
    out.extend_from_slice(oid);
    out.extend_from_slice(&0u32.to_le_bytes()); // tag count: none
    out.extend_from_slice(&0u32.to_le_bytes()); // pending-map count: none
    out.extend_from_slice(&(pack.len() as u32).to_le_bytes());
    out.extend_from_slice(pack);
    out.extend_from_slice(&empty_tracker_section());
    out
}

/// the trailing tracker section of a tracker-less snapshot: length, magic,
/// zero source revision and zero repo count.
fn empty_tracker_section() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(b"TRK\x01");
    out.extend_from_slice(&0u64.to_le_bytes()); // source revision
    out.extend_from_slice(&0u32.to_le_bytes()); // repo count
    out
}

/// a base whose default repo is still pristine: either the repo dir was never
/// created, or it has no born ref AND no indexed pack — the on-disk oracle that a
/// rejected install wrote NOTHING.
fn assert_repo_untouched(base: &Path) {
    let dir = repo_dir(base);
    if !dir.exists() {
        return; // never created — trivially untouched
    }
    let repo = git2::Repository::open(&dir).unwrap();
    assert!(
        repo.refname_to_id(MAIN_REF).is_err(),
        "ref must not have moved"
    );
    let packs = std::fs::read_dir(dir.join(".git/objects/pack"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(packs, 0, "no pack may have been indexed");
}

#[test]
fn snapshot_reconstructs_root_and_content_on_a_fresh_module() {
    let (src_base, src) = source("rt-src");
    let root = src.root();
    assert_ne!(root, StateRoot::ZERO, "source must have real state");
    let bytes = src.snapshot().unwrap();

    let dst_base = tmp_base("rt-dst");
    let mut dst = Forge::init("forge", dst_base.clone()).unwrap();
    assert_eq!(dst.root(), StateRoot::ZERO);
    dst.install(&bytes, root).unwrap();

    // THE PROPERTY: identical composed root — the root-hash linkage a joiner needs.
    assert_eq!(
        dst.root(),
        root,
        "installed root must equal the source root"
    );

    // content oracle: read the installed default repo with git2 directly — the
    // head commit, its message, both blobs, and the parent link all came through.
    let repo = git2::Repository::open(repo_dir(&dst_base)).unwrap();
    let head = repo.refname_to_id(MAIN_REF).unwrap();
    let commit = repo.find_commit(head).unwrap();
    assert_eq!(commit.message(), Some("c2"));
    let tree = commit.tree().unwrap();
    let blob = |name: &str| {
        let entry = tree
            .get_name(name)
            .unwrap_or_else(|| panic!("{name} missing from installed tree"));
        repo.find_blob(entry.id()).unwrap().content().to_vec()
    };
    assert_eq!(blob("a.txt"), b"one".to_vec());
    assert_eq!(blob("b.txt"), b"two".to_vec());
    assert_eq!(
        commit.parent(0).unwrap().message(),
        Some("c1"),
        "history must ship, not just the tip"
    );

    let _ = std::fs::remove_dir_all(&src_base);
    let _ = std::fs::remove_dir_all(&dst_base);
}

#[test]
fn tampered_head_oid_is_rejected_before_anything_is_written() {
    let (src_base, src) = source("tamper-src");
    let root = src.root();
    let mut bytes = src.snapshot().unwrap();
    // corrupt the default repo's head oid so the composed root no longer matches.
    let off = first_oid_offset(&bytes);
    bytes[off] ^= 0xff;

    let dst_base = tmp_base("tamper-dst");
    let mut dst = Forge::init("forge", dst_base.clone()).unwrap();
    let err = dst.install(&bytes, root).unwrap_err();
    assert!(matches!(err, Error::Module { ref reason, .. } if reason == "snapshot_root_mismatch"));
    assert_eq!(
        dst.root(),
        StateRoot::ZERO,
        "a rejected snapshot must leave the root untouched"
    );
    assert_repo_untouched(&dst_base);

    let _ = std::fs::remove_dir_all(&src_base);
    let _ = std::fs::remove_dir_all(&dst_base);
}

#[test]
fn corrupt_or_garbage_pack_bytes_are_rejected() {
    let (src_base, src) = source("garbage-src");
    let root = src.root();
    let bytes = src.snapshot().unwrap();

    let dst_base = tmp_base("garbage-dst");
    let mut dst = Forge::init("forge", dst_base.clone()).unwrap();

    // an honest header (count/name/oid) over pure garbage instead of a pack.
    let mut garbage = bytes.clone();
    let pack_start = first_pack_offset(&bytes);
    for b in &mut garbage[pack_start..] {
        *b = 0xab;
    }
    assert!(
        dst.install(&garbage, root).is_err(),
        "garbage pack must be rejected"
    );

    // a real pack with its trailer checksum flipped.
    let mut flipped = bytes.clone();
    *flipped.last_mut().unwrap() ^= 0xff;
    assert!(
        dst.install(&flipped, root).is_err(),
        "corrupted pack must be rejected"
    );

    // truncation below even the container header.
    assert!(
        dst.install(&bytes[..2], root).is_err(),
        "truncated snapshot must be rejected"
    );

    // failed packs may strand junk in the odb, but no ref moved and root is
    // byte-identical to before every attempt.
    assert_eq!(dst.root(), StateRoot::ZERO);
    assert_eq!(on_disk_head(&dst_base), None, "ref must not have moved");

    let _ = std::fs::remove_dir_all(&src_base);
    let _ = std::fs::remove_dir_all(&dst_base);
}

/// the default repo's on-disk head oid, or `None` if unborn / not created.
fn on_disk_head(base: &Path) -> Option<git2::Oid> {
    git2::Repository::open(repo_dir(base))
        .ok()?
        .refname_to_id(MAIN_REF)
        .ok()
}

#[test]
fn empty_snapshot_round_trips_the_unborn_state() {
    let src_base = tmp_base("empty-src");
    let src = Forge::init("forge", src_base.clone()).unwrap();
    let bytes = src.snapshot().unwrap();
    let mut expected = b"FGv1".to_vec();
    expected.extend_from_slice(&[0u8; 4]); // zero repo count
    expected.extend_from_slice(&empty_tracker_section());
    assert_eq!(
        bytes, expected,
        "an empty namespace must serialize as the magic + zero-count marker + empty tracker"
    );

    let dst_base = tmp_base("empty-dst");
    let mut dst = Forge::init("forge", dst_base.clone()).unwrap();
    dst.install(&bytes, StateRoot::ZERO).unwrap();
    assert_eq!(dst.root(), StateRoot::ZERO);

    // the marker binds to ZERO the way root() does — any other expectation fails…
    assert!(
        dst.install(&bytes, StateRoot([1u8; sdk::ROOT_LEN]))
            .is_err()
    );
    // …and it must not smuggle trailing bytes.
    let mut padded = bytes.clone();
    padded.push(0);
    assert!(dst.install(&padded, StateRoot::ZERO).is_err());

    let _ = std::fs::remove_dir_all(&src_base);
    let _ = std::fs::remove_dir_all(&dst_base);
}

#[test]
fn install_replaces_a_divergent_head() {
    let (src_base, src) = source("replace-src");
    let root = src.root();
    let bytes = src.snapshot().unwrap();

    let dst_base = tmp_base("replace-dst");
    let mut divergent =
        forge::testkit::history("replace-divergent", &[(9, "z.txt", "other", "unrelated")]);
    let divergent = divergent.remove(0);
    let blobs = blobstore::BlobHandle::default();
    let digest = blobs.put_chunk(divergent.pack).to_vec();
    let mut dst = Forge::with_blobs("forge", dst_base.clone(), blobs).unwrap();
    push(&mut dst, None, &divergent.head, &digest);
    assert_ne!(
        dst.root(),
        root,
        "the destination starts on an unrelated head"
    );

    // install is a replacement, not a merge: the unrelated (non-fast-forward)
    // head is overwritten.
    dst.install(&bytes, root).unwrap();
    assert_eq!(dst.root(), root);

    let _ = std::fs::remove_dir_all(&src_base);
    let _ = std::fs::remove_dir_all(&dst_base);
}

#[test]
fn a_partial_closure_pack_is_rejected_before_the_ref_moves() {
    let (src_base, src) = source("partial-src");
    let expected = src.root();
    let full = src.snapshot().unwrap();
    let head = git2::Oid::from_bytes(&full[first_oid_offset(&full)..][..OID_LEN]).unwrap();

    // byzantine pack: the GENUINE head commit and its root tree, nothing else.
    // every carried object hash-checks and the oid composes to `expected`, but
    // the blobs and the parent commit are missing — only a closure walk between
    // pack indexing and the ref move can catch it.
    let repo = git2::Repository::open(repo_dir(&src_base)).unwrap();
    let head_commit = repo.find_commit(head).unwrap();
    let mut pb = repo.packbuilder().unwrap();
    pb.insert_object(head, None).unwrap();
    pb.insert_object(head_commit.tree_id(), None).unwrap();
    let mut buf = git2::Buf::new();
    pb.write_buf(&mut buf).unwrap();
    let bytes = build_container(DEFAULT_REPO, head.as_bytes(), &buf);

    let dst_base = tmp_base("partial-dst");
    let mut dst = Forge::init("forge", dst_base.clone()).unwrap();
    let err = dst.install(&bytes, expected).unwrap_err();
    assert!(
        matches!(err, Error::Module { ref reason, .. } if reason == "git_verify_closure"),
        "an incomplete pack is refused by the closure check: {err:?}"
    );
    assert_eq!(dst.root(), StateRoot::ZERO, "the ref never moved");
    assert_eq!(on_disk_head(&dst_base), None, "the ref never moved");

    let _ = std::fs::remove_dir_all(&src_base);
}

#[test]
fn empty_state_installed_over_a_born_repo_unbinds_it_durably() {
    // an empty source: its snapshot is the zero-count marker.
    let empty_base = tmp_base("unbind-empty-src");
    let empty = Forge::init("forge", empty_base).unwrap();
    let marker = empty.snapshot().unwrap();

    // a BORN destination with real history.
    let (dst_base, mut dst) = source("born-dst");
    assert_ne!(dst.root(), StateRoot::ZERO, "destination starts born");

    dst.install(&marker, StateRoot::ZERO).unwrap();
    assert_eq!(
        dst.root(),
        StateRoot::ZERO,
        "in-memory root returns to ZERO"
    );

    // durability: a re-init re-adopts refs from DISK — if install had only
    // cleared the cached head and left the default repo's ref, the old root
    // would resurrect here and the root-hash would diverge from ZERO.
    drop(dst);
    let reopened = Forge::init("forge", dst_base).unwrap();
    assert_eq!(reopened.root(), StateRoot::ZERO, "the on-disk ref is gone");
}
