//! the multi-repo namespace: forge addresses a NAMED set of repos, and its
//! `root()` is a canonical sorted hash over each repo's committed HEAD.
//!
//! the load-bearing properties pinned here:
//!   - COMPOSITION + ORDER-INDEPENDENCE: two repos compose into one root, and
//!     the same heads reached in either order compose to the SAME root.
//!   - PER-REPO CAS ISOLATION: a stale push on one repo is rejected without
//!     touching any other repo.
//!   - REPO DEFAULTING: a `{commit:{path,content,message}}` with no `repo` key
//!     targets the "default" repo; `"head"`/`list_repos` see it.
//!   - NAME VALIDATION: a bad slug is rejected deterministically.
//!   - SNAPSHOT ROUND-TRIP + PACK-LESS DETERMINISM: N repos snapshot -> install
//!     to an identical composed root, and a node WITHOUT a push's pack still
//!     composes the SAME root (the phase-1 determinism invariant, now per-repo).

use std::path::{Path, PathBuf};

use forge::Forge;
use forge::{
    ForgeMsg, ForgeQuery, ForgeReply, RefUpdate, RepoHead, decode_reply, encode_msg, encode_query,
};
use futures::executor::block_on;
use sdk::{Error, Module, Msg, StateRoot};

const MAIN_REF: &str = "refs/heads/main";

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
    p.push(format!("ducktape-forge-multi-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn oid_hex(raw: &[u8]) -> String {
    git2::Oid::from_bytes(raw).unwrap().to_string()
}

fn push_msg(repo: &str, prev: Option<&[u8]>, new: &[u8], digest: &[u8]) -> Msg {
    Msg {
        target: "forge".into(),
        payload: encode_msg(&ForgeMsg::PushRefs {
            repo: repo.into(),
            updates: vec![RefUpdate {
                ref_name: "main".into(),
                prev_oid: prev.map(<[u8]>::to_vec),
                new_oid: Some(new.to_vec()),
            }],
            pack_digest: Some(digest.to_vec()),
            cert: None,
        }),
    }
}

/// push a captured head to a named repo (execute + publish).
fn push(forge: &mut Forge, repo: &str, prev: Option<&[u8]>, new: &[u8], digest: &[u8]) {
    block_on(forge.execute(&mut at(0), &push_msg(repo, prev, new, digest))).unwrap();
    block_on(forge.commit_block()).unwrap();
}

/// execute a push WITHOUT publishing — to observe a rejection in isolation.
fn try_push(
    forge: &mut Forge,
    repo: &str,
    prev: Option<&[u8]>,
    new: &[u8],
    digest: &[u8],
) -> Result<(), Error> {
    block_on(forge.execute(&mut at(0), &push_msg(repo, prev, new, digest)))
}

fn head_query(forge: &Forge) -> Option<String> {
    match decode_reply(&block_on(forge.query(&encode_query(&ForgeQuery::Head))).unwrap()).unwrap() {
        ForgeReply::Head(h) => h,
        other => panic!("expected Head, got {other:?}"),
    }
}

fn head_of(forge: &Forge, repo: &str) -> Option<String> {
    let q = encode_query(&ForgeQuery::HeadOf { repo: repo.into() });
    match decode_reply(&block_on(forge.query(&q)).unwrap()).unwrap() {
        ForgeReply::Head(h) => h,
        other => panic!("expected Head, got {other:?}"),
    }
}

fn list_repos(forge: &Forge) -> Vec<RepoHead> {
    let q = encode_query(&ForgeQuery::ListRepos);
    match decode_reply(&block_on(forge.query(&q)).unwrap()).unwrap() {
        ForgeReply::Repos(r) => r,
        other => panic!("expected Repos, got {other:?}"),
    }
}

/// the on-disk `MAIN_REF` oid of `base/<repo>`, or `None` if unborn / absent.
fn on_disk_head(base: &Path, repo: &str) -> Option<git2::Oid> {
    git2::Repository::open(base.join(repo))
        .ok()?
        .refname_to_id(MAIN_REF)
        .ok()
}

/// generate a real commit closure (head oid + full-closure pack) via a throwaway
/// source forge's default repo. a pushed head is just an oid + a pack of git
/// objects, so the source repo NAME is irrelevant to the closure.
fn make_closure(tag: &str, t: u64, path: &str, content: &str, message: &str) -> (Vec<u8>, Vec<u8>) {
    let mut commits = forge::testkit::history(tag, &[(t, path, content, message)]);
    let commit = commits.remove(0);
    (commit.head, commit.pack)
}

// ---------------------------------------------------------------------------

#[test]
fn two_repos_compose_and_are_order_independent() {
    let (ha, pa) = make_closure("ca", 1, "x.txt", "aaa", "ca");
    let (hb, pb) = make_closure("cb", 2, "y.txt", "bbb", "cb");

    // forge 1: push "a" then "b".
    let base1 = tmp_base("ab");
    let blobs1 = blobstore::BlobHandle::default();
    let da1 = blobs1.put_chunk(pa.clone()).to_vec();
    let db1 = blobs1.put_chunk(pb.clone()).to_vec();
    let mut f1 = Forge::with_blobs("forge", base1.clone(), blobs1).unwrap();
    assert_eq!(f1.root(), StateRoot::ZERO, "empty namespace -> ZERO");

    push(&mut f1, "a", None, &ha, &da1);
    let root_a_only = f1.root();
    assert_ne!(root_a_only, StateRoot::ZERO, "push to 'a' moved the root");
    assert_eq!(head_of(&f1, "b"), None, "'b' absent until pushed");

    push(&mut f1, "b", None, &hb, &db1);
    let root_both = f1.root();
    assert_ne!(root_both, root_a_only, "push to 'b' moved the root again");

    // forge 2: push "b" then "a" (reverse order).
    let base2 = tmp_base("ba");
    let blobs2 = blobstore::BlobHandle::default();
    let da2 = blobs2.put_chunk(pa.clone()).to_vec();
    let db2 = blobs2.put_chunk(pb.clone()).to_vec();
    let mut f2 = Forge::with_blobs("forge", base2.clone(), blobs2).unwrap();
    push(&mut f2, "b", None, &hb, &db2);
    push(&mut f2, "a", None, &ha, &da2);

    // THE property: a sorted composition -> identical root regardless of order.
    assert_eq!(
        f1.root(),
        f2.root(),
        "root is order-independent (sorted composition)"
    );
    assert_eq!(head_of(&f1, "a"), Some(oid_hex(&ha)));
    assert_eq!(head_of(&f1, "b"), Some(oid_hex(&hb)));
    assert_eq!(head_of(&f1, "a"), head_of(&f2, "a"));
    assert_eq!(head_of(&f1, "b"), head_of(&f2, "b"));

    let _ = std::fs::remove_dir_all(&base1);
    let _ = std::fs::remove_dir_all(&base2);
}

#[test]
fn stale_push_on_one_repo_does_not_touch_another() {
    let (ha, pa) = make_closure("cas-a", 1, "a.txt", "a", "ca");
    let (hb, pb) = make_closure("cas-b", 2, "b.txt", "b", "cb");
    let (hc, _pc) = make_closure("cas-c", 3, "c.txt", "c", "cc");

    let base = tmp_base("cas");
    let blobs = blobstore::BlobHandle::default();
    let da = blobs.put_chunk(pa).to_vec();
    let db = blobs.put_chunk(pb).to_vec();
    let mut f = Forge::with_blobs("forge", base.clone(), blobs).unwrap();
    push(&mut f, "a", None, &ha, &da);
    push(&mut f, "b", None, &hb, &db);

    let pinned = f.root();
    let head_a = head_of(&f, "a");
    let head_b = head_of(&f, "b");

    // a stale push to "a" (prev = None but "a" is born) — non-fast-forward. the
    // CAS is per-repo and fires BEFORE any IO, so a bogus digest is irrelevant.
    let err = try_push(&mut f, "a", None, &hc, &[0u8; 32]).unwrap_err();
    assert!(matches!(err, Error::Module { ref reason, .. } if reason == "non_fast_forward"));

    // nothing moved: not "a", and — the isolation property — not "b" either.
    assert_eq!(f.root(), pinned, "a rejected push must not move any root");
    assert_eq!(head_of(&f, "a"), head_a, "'a' head unchanged");
    assert_eq!(
        head_of(&f, "b"),
        head_b,
        "'b' head untouched by 'a's rejection"
    );
    assert_eq!(head_a, Some(oid_hex(&ha)));

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn empty_repo_targets_default_and_is_addressable() {
    let base = tmp_base("compat");
    let (head, pack) = make_closure("compat-commit", 5, "a.txt", "hi", "m");
    let blobs = blobstore::BlobHandle::default();
    let digest = blobs.put_chunk(pack).to_vec();
    let mut f = Forge::with_blobs("forge", base.clone(), blobs).unwrap();

    // the single-repo wire: a push with an explicit empty `repo` slug, which
    // the module maps to the default repo.
    push(&mut f, "", None, &head, &digest);

    // the unit Head query answers the default repo.
    let head = head_query(&f);
    assert!(
        head.is_some(),
        "Head must see the default repo's pushed commit"
    );
    // HeadOf("default") and HeadOf("") resolve to the same repo.
    assert_eq!(head_of(&f, "default"), head);
    assert_eq!(head_of(&f, ""), head);
    // ListRepos shows exactly the default repo.
    assert_eq!(
        list_repos(&f),
        vec![RepoHead {
            name: "default".into(),
            head: head.clone(),
        }]
    );
    // and it materialized on disk at base/default.
    assert!(
        base.join("default/.git").exists(),
        "the default repo's git dir lives at base/default"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn head_of_reads_named_repos_and_bad_slugs_are_rejected() {
    let base = tmp_base("headof");
    let (head, pack) = make_closure("headof-commit", 1, "r.md", "hello", "c");
    let blobs = blobstore::BlobHandle::default();
    let digest = blobs.put_chunk(pack).to_vec();
    let mut f = Forge::with_blobs("forge", base.clone(), blobs).unwrap();
    push(&mut f, "docs", None, &head, &digest);

    let on_disk = on_disk_head(&base, "docs").unwrap().to_string();
    assert_eq!(head_of(&f, "docs"), Some(on_disk), "HeadOf reads the repo");
    assert_eq!(head_of(&f, "missing"), None, "an absent repo is None");

    // a bad slug in a write is rejected DETERMINISTICALLY at execute.
    for bad in ["BAD", "a/b", "..", "with space"] {
        let err =
            block_on(f.execute(&mut at(2), &push_msg(bad, None, &head, &digest))).unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "bad_repo_name"),
            "{bad:?} must reject"
        );
    }
    // a bad slug in a HeadOf query also errs (never a silent None).
    let q = encode_query(&ForgeQuery::HeadOf { repo: "..".into() });
    assert!(block_on(f.query(&q)).is_err());

    // the rejections left the namespace as it was: only "docs".
    assert_eq!(
        list_repos(&f)
            .into_iter()
            .map(|r| r.name)
            .collect::<Vec<_>>(),
        vec!["docs".to_string()]
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn multi_repo_snapshot_round_trips_and_pack_less_node_composes_the_same_root() {
    let (ha, pa) = make_closure("rt-alpha", 1, "a.txt", "AAA", "ca");
    let (hb, pb) = make_closure("rt-beta", 2, "b.txt", "BBB", "cb");
    let (hp, pp) = make_closure("rt-push", 9, "p.txt", "pushed", "cp");

    // The source holds all three off-chain packs, so every pushed repo
    // materializes before it serves the snapshot.
    let src_base = tmp_base("rt-src");
    let src_blobs = blobstore::BlobHandle::default();
    let da = src_blobs.put_chunk(pa).to_vec();
    let db = src_blobs.put_chunk(pb).to_vec();
    let dp = src_blobs.put_chunk(pp.clone()).to_vec();
    let mut src = Forge::with_blobs("forge", src_base.clone(), src_blobs).unwrap();
    push(&mut src, "alpha", None, &ha, &da);
    push(&mut src, "beta", None, &hb, &db);
    push(&mut src, "gamma", None, &hp, &dp);
    let src_root = src.root();
    assert_ne!(src_root, StateRoot::ZERO);

    // snapshot -> fresh install: identical composed root, all three repos.
    let bytes = src.snapshot().unwrap();
    let dst_base = tmp_base("rt-dst");
    let mut dst = Forge::init("forge", dst_base.clone()).unwrap();
    dst.install(&bytes, src_root).unwrap();
    assert_eq!(
        dst.root(),
        src_root,
        "installed composed root matches the source"
    );
    for r in ["alpha", "beta", "gamma"] {
        assert_eq!(head_of(&dst, r), head_of(&src, r), "repo {r} head matches");
    }
    // content oracle: the pushed repo's blob came through the closure pack.
    let gamma = git2::Repository::open(dst_base.join("gamma")).unwrap();
    let ghead = gamma.refname_to_id(MAIN_REF).unwrap();
    let gtree = gamma.find_commit(ghead).unwrap().tree().unwrap();
    let gblob = gtree.get_name("p.txt").unwrap().id();
    assert_eq!(gamma.find_blob(gblob).unwrap().content(), b"pushed");

    // determinism carry-over: replay the SAME ops on a node whose blob store
    // LACKS the push pack. root must still compose to src_root — pack possession
    // is per-node, root is not.
    let nopack_base = tmp_base("rt-nopack");
    let nopack_blobs = blobstore::BlobHandle::default();
    let mut nopack = Forge::with_blobs("forge", nopack_base.clone(), nopack_blobs).unwrap();
    push(&mut nopack, "alpha", None, &ha, &da);
    push(&mut nopack, "beta", None, &hb, &db);
    push(&mut nopack, "gamma", None, &hp, &dp);

    assert_eq!(
        nopack.root(),
        src_root,
        "a pack-less node composes the SAME root (per-repo P1 determinism)"
    );
    assert_eq!(head_of(&nopack, "gamma"), Some(oid_hex(&hp)));
    // The only difference is node-local: no repo can materialize without its
    // pack, while all three consensus heads and the composed root still match.
    for repo in ["alpha", "beta", "gamma"] {
        assert_eq!(
            on_disk_head(&nopack_base, repo),
            None,
            "no pack -> {repo}'s on-disk ref stays behind"
        );
    }

    let _ = std::fs::remove_dir_all(&src_base);
    let _ = std::fs::remove_dir_all(&dst_base);
    let _ = std::fs::remove_dir_all(&nopack_base);
}
