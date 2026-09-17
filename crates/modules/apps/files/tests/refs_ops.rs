//! the four refs-mutating verbs — pin / unpin / watch / unwatch — over the real
//! module surface: every test drives `Files::execute` with a `FilesMsg` op, then
//! the async `commit_block` (or `abort_block`), and reads back committed refs by
//! decoding the snapshot image (`decode_refs(f.snapshot())`) — an honest codec
//! round-trip, since the `Refs` query is not part of this task's read surface.
//!
//! covers the origin rules, the honest cap boundaries (1024 pins, 256
//! watches), and segment-boundary watch matching at both ends (`/shared`
//! fires for `/shared/x`, never for `/sharedsecret/x`).
//!
//! each async call is `block_on`'d at the top level; `commit_block`/`abort_block`
//! get their own `block_on` (nesting trips futures' LocalPool re-entry guard).

mod harness;
use harness::*;
use sdk::Module as _;

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use files::{Change, Content, FilesMsg, Refs, decode_refs, encode_msg, to_hex};

// ---- op builders ------------------------------------------------------------

fn pin_op(snapshot: &str, name: &str) -> sdk::Msg {
    msg(FilesMsg::Pin {
        snapshot: snapshot.into(),
        name: name.into(),
    })
}

fn unpin_op(name: &str) -> sdk::Msg {
    msg(FilesMsg::Unpin { name: name.into() })
}

fn watch_op(prefix: &str, module_id: &str) -> sdk::Msg {
    msg(FilesMsg::Watch {
        prefix: prefix.into(),
        module_id: module_id.into(),
    })
}

fn unwatch_op(prefix: &str, module_id: &str) -> sdk::Msg {
    msg(FilesMsg::Unwatch {
        prefix: prefix.into(),
        module_id: module_id.into(),
    })
}

fn msg(m: FilesMsg) -> sdk::Msg {
    sdk::Msg {
        target: "files".into(),
        payload: encode_msg(&m),
    }
}

// ---- drivers ----------------------------------------------------------------

/// drive one op through the module surface. returns the `TestCtx` on success (so
/// emitted watch notifications are observable), the `sdk::Error` on reject.
fn exec(
    f: &mut files::Files,
    origin: sdk::Origin,
    h: u64,
    op: sdk::Msg,
) -> Result<TestCtx, sdk::Error> {
    let mut ctx = test_ctx(origin, h);
    futures::executor::block_on(f.execute(&mut ctx, &op))?;
    Ok(ctx)
}

fn commit(
    f: &mut files::Files,
    origin: sdk::Origin,
    h: u64,
    base: Option<&str>,
    changes: Vec<Change>,
) -> Result<TestCtx, sdk::Error> {
    exec(
        f,
        origin,
        h,
        msg(FilesMsg::Commit {
            base_snapshot: base.map(Into::into),
            message: "c".into(),
            changes,
        }),
    )
}

fn commit_block(f: &mut files::Files) {
    futures::executor::block_on(f.commit_block()).unwrap();
}

fn abort_block(f: &mut files::Files) {
    futures::executor::block_on(f.abort_block()).unwrap();
}

/// seed one committed snapshot (system write under `/shared`), adopt it, and
/// return its hex id — the resolvable base a pin threads.
fn seed_head(f: &mut files::Files, h: u64) -> String {
    commit(
        f,
        sdk::Origin::System,
        h,
        None,
        vec![put_inline("/shared/seed", b"s")],
    )
    .expect("seed commits");
    commit_block(f);
    f.committed_head_for_test().expect("head after seed")
}

/// committed refs, decoded from the snapshot image — the honest read side for
/// pins/watches (the `Refs` query lands in a later task).
fn decoded_refs(f: &files::Files) -> Refs {
    decode_refs(&f.snapshot()).expect("snapshot image decodes")
}

fn put_inline(path: &str, bytes: &[u8]) -> Change {
    Change::Put {
        path: path.into(),
        exec: false,
        meta: BTreeMap::new(),
        content: Content::Inline {
            b64: STANDARD.encode(bytes),
        },
    }
}

fn md(name: &str) -> sdk::Origin {
    sdk::Origin::Module(name.into())
}

fn ext(who: &[u8]) -> sdk::Origin {
    sdk::Origin::External(who.to_vec())
}

fn assert_module_err(err: &sdk::Error, needle: &str) {
    match err {
        sdk::Error::Module { reason, sentence } => {
            assert!(
                reason.starts_with("files_"),
                "expected a files step token, got {reason:?}"
            );
            assert!(
                sentence.contains(needle),
                "expected error to contain {needle:?}, got {sentence:?}"
            );
        }
        other => panic!("expected a module refusal carrying {needle:?}, got {other:?}"),
    }
}

// ---- pin --------------------------------------------------------------------

#[test]
fn pin_happy_path_moves_root_and_records_owner() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let head = seed_head(&mut f, 1);
    let root_after_seed = f.root();

    // staging a pin edits the pending overlay only — the committed root stays put.
    exec(&mut f, sdk::Origin::System, 2, pin_op(&head, "v1")).expect("pin stages");
    assert_eq!(
        f.root(),
        root_after_seed,
        "pin stages, committed root unmoved"
    );
    assert!(
        decoded_refs(&f).pins.is_empty(),
        "pin not visible in committed refs before commit_block"
    );

    commit_block(&mut f);
    assert_ne!(
        f.root(),
        root_after_seed,
        "commit_block adopts the pin, root moves"
    );
    let refs = decoded_refs(&f);
    let pin = refs.pins.get("v1").expect("pin recorded under its name");
    assert_eq!(to_hex(&pin.snapshot), head, "pin protects the seeded head");
    assert_eq!(
        pin.owner,
        files::Actor::System,
        "owner is the acting origin, not the payload"
    );
}

#[test]
fn pin_duplicate_name_rejects() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let head = seed_head(&mut f, 1);
    exec(&mut f, sdk::Origin::System, 2, pin_op(&head, "v1")).expect("first pin");
    commit_block(&mut f);
    let err =
        exec(&mut f, sdk::Origin::System, 3, pin_op(&head, "v1")).expect_err("duplicate name");
    assert_module_err(&err, "pin name already exists");
    abort_block(&mut f);
}

#[test]
fn pin_unresolvable_snapshot_rejects() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_head(&mut f, 1); // a non-empty refs, but we pin a never-seen id
    let random = "ab".repeat(32); // 64 valid hex chars, never committed
    let err =
        exec(&mut f, sdk::Origin::System, 2, pin_op(&random, "v1")).expect_err("unresolvable");
    assert_module_err(&err, "snapshot not resolvable");
    abort_block(&mut f);
}

#[test]
fn pin_empty_name_rejects() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let head = seed_head(&mut f, 1);
    let err = exec(&mut f, sdk::Origin::System, 2, pin_op(&head, "")).expect_err("empty name");
    assert_module_err(&err, "pin name must not be empty");
    abort_block(&mut f);
}

#[test]
fn pin_name_too_long_rejects() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let head = seed_head(&mut f, 1);
    let long = "n".repeat(files::MAX_PIN_NAME_BYTES + 1);
    let err = exec(&mut f, sdk::Origin::System, 2, pin_op(&head, &long)).expect_err("over cap");
    assert_module_err(&err, "pin name exceeds the byte cap");
    abort_block(&mut f);
}

#[test]
fn pin_table_full_at_max_pins() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let head = seed_head(&mut f, 1);
    // fill the pin table honestly in ONE block: MAX_PINS distinct names, all
    // pointing at the resolvable head (a cheap in-memory BTreeMap fill), spread
    // across MAX_PINS / MAX_PINS_PER_OWNER owners so the per-owner cap (#1801)
    // never trips before the global one this test targets.
    let owners_needed = files::MAX_PINS / files::MAX_PINS_PER_OWNER;
    for i in 0..files::MAX_PINS {
        let owner = md(&format!("owner{}", i % owners_needed));
        exec(&mut f, owner, 2, pin_op(&head, &format!("p{i}")))
            .expect("pin fits under the global and per-owner caps");
    }
    // the table is now exactly full: even a FRESH owner (nowhere near its own
    // per-owner share) is refused by the global cap.
    let err =
        exec(&mut f, md("owner-fresh"), 2, pin_op(&head, "overflow")).expect_err("cap reached");
    assert_module_err(&err, "pin table is full");
    abort_block(&mut f);
}

/// the per-owner share of the global table (#1801): the `MAX_PINS_PER_OWNER`th
/// pin from one owner lands, the next from the SAME owner is refused with a
/// stable reason distinct from the global cap, and a DIFFERENT owner is
/// unaffected.
#[test]
fn pin_per_owner_cap_is_independent_of_other_owners() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let head = seed_head(&mut f, 1);

    for i in 0..files::MAX_PINS_PER_OWNER {
        exec(&mut f, md("alice"), 2, pin_op(&head, &format!("a{i}")))
            .expect("alice's pin fits under her share");
    }
    let err = exec(&mut f, md("alice"), 2, pin_op(&head, "one-too-many"))
        .expect_err("alice hit her per-owner cap");
    assert_module_err(&err, "pin quota exceeded");

    // bob's share is untouched by alice filling hers.
    exec(&mut f, md("bob"), 2, pin_op(&head, "b0")).expect("bob still pins");
    abort_block(&mut f);
}

#[test]
fn pin_quota_follows_the_account_share_and_exact_key_admission() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let head = seed_head(&mut f, 1);
    let old_key = files::Authority::External {
        key: vec![1],
        account: None,
    };
    let admitted = files::Authority::External {
        key: vec![1],
        account: Some(1),
    };
    let sibling = files::Authority::External {
        key: vec![2],
        account: Some(1),
    };
    let store = duckfs_disk::DiskStore::open(d.path().join("objects")).unwrap();
    let mut core = files::Fs::new(store, decoded_refs(&f));
    core.pin(&old_key, 2, head.clone(), "before-admission".into())
        .unwrap();
    for i in 0..files::MAX_PINS_PER_OWNER - 1 {
        let signer = match i % 2 {
            0 => &admitted,
            _ => &sibling,
        };
        core.pin(signer, 2, head.clone(), format!("account-{i}"))
            .unwrap();
    }

    // Admission must not erase the actual signer's earlier quota usage.
    let before = core.pending_refs().clone();
    assert_eq!(
        core.pin(&admitted, 2, head.clone(), "over-quota".into()),
        Err("files: pin quota exceeded".into())
    );
    assert_eq!(core.pending_refs(), &before);

    // The other member draws on the account's share, not on the earlier key
    // pin's. Its one remaining account slot is shared by every account member,
    // and any member releases the earlier key pin.
    core.pin(&sibling, 2, head.clone(), "last-account-slot".into())
        .unwrap();
    let before = core.pending_refs().clone();
    assert_eq!(
        core.pin(&sibling, 2, head.clone(), "account-over-quota".into()),
        Err("files: pin quota exceeded".into())
    );
    assert_eq!(core.pending_refs(), &before);
    core.unpin(&sibling, 2, "before-admission".into()).unwrap();

    // A keyless program has its own account share and can release its own pin
    // to regain capacity, with the same revision/rollback discipline.
    let program = files::Authority::Program(2);
    for i in 0..files::MAX_PINS_PER_OWNER {
        core.pin(&program, 2, head.clone(), format!("program-{i}"))
            .unwrap();
    }
    let before = core.pending_refs().clone();
    assert_eq!(
        core.pin(&program, 2, head.clone(), "program-over-quota".into()),
        Err("files: pin quota exceeded".into())
    );
    assert_eq!(core.pending_refs(), &before);
    core.unpin(&program, 2, "program-0".into()).unwrap();
    core.pin(&program, 2, head, "program-replacement".into())
        .unwrap();
}

// ---- unpin ------------------------------------------------------------------

#[test]
fn any_authority_unpins_and_an_absent_pin_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let head = seed_head(&mut f, 1);

    // alice (a module) creates the pin: its owner for attribution and the
    // per-owner pin share, not for removal.
    exec(&mut f, md("alice"), 2, pin_op(&head, "a")).expect("alice pins");
    commit_block(&mut f);

    // bob, another module, removes alice's pin.
    exec(&mut f, md("bob"), 3, unpin_op("a")).expect("bob unpins");
    commit_block(&mut f);
    assert!(decoded_refs(&f).pins.is_empty(), "bob removed alice's pin");

    // re-pin; alice removes her own.
    exec(&mut f, md("alice"), 4, pin_op(&head, "a")).expect("re-pin");
    commit_block(&mut f);
    exec(&mut f, md("alice"), 5, unpin_op("a")).expect("alice unpins");
    commit_block(&mut f);
    assert!(decoded_refs(&f).pins.is_empty(), "alice's pin removed");

    // re-pin; system removes it too.
    exec(&mut f, md("alice"), 6, pin_op(&head, "a")).expect("re-pin");
    commit_block(&mut f);
    exec(&mut f, sdk::Origin::System, 7, unpin_op("a")).expect("system unpins");
    commit_block(&mut f);
    assert!(decoded_refs(&f).pins.is_empty(), "system removed the pin");

    // unpin of an absent name → not found, whoever asks.
    let err = exec(&mut f, md("bob"), 8, unpin_op("ghost")).expect_err("absent");
    assert_module_err(&err, "pin not found");
    abort_block(&mut f);
}

// ---- watch: origin gate + caps ----------------------------------------------

#[test]
fn watch_external_origin_rejects() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    // an external submitter is not a module → cannot register a watch at all.
    let err = exec(&mut f, ext(b"someone"), 1, watch_op("/shared", "someone"))
        .expect_err("external blocked");
    assert_module_err(&err, "watch registration is module-origin only");
    abort_block(&mut f);
}

#[test]
fn watch_foreign_module_rejects() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    // "automations" may not register a watch on behalf of "chat".
    let err = exec(&mut f, md("automations"), 1, watch_op("/shared", "chat"))
        .expect_err("foreign module");
    assert_module_err(&err, "a module may only watch for itself");
    abort_block(&mut f);
}

#[test]
fn watch_self_ok_moves_root_and_records() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let root0 = f.root();
    // "chat" registers a watch for itself.
    exec(&mut f, md("chat"), 1, watch_op("/shared", "chat")).expect("self-watch ok");
    assert_eq!(f.root(), root0, "watch stages, committed root unmoved");
    commit_block(&mut f);
    assert_ne!(f.root(), root0, "commit_block adopts the watch, root moves");
    assert!(
        decoded_refs(&f)
            .watches
            .contains(&("/shared".to_string(), "chat".to_string())),
        "watch recorded under its (prefix, module_id) key"
    );
}

#[test]
fn watch_system_may_register_for_any_module() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    exec(&mut f, sdk::Origin::System, 1, watch_op("/shared", "chat")).expect("system for chat");
    commit_block(&mut f);
    assert!(
        decoded_refs(&f)
            .watches
            .contains(&("/shared".to_string(), "chat".to_string())),
        "system registered a watch on chat's behalf"
    );
}

#[test]
fn watch_duplicate_pair_rejects() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    exec(&mut f, md("chat"), 1, watch_op("/shared", "chat")).expect("first");
    commit_block(&mut f);
    let err = exec(&mut f, md("chat"), 2, watch_op("/shared", "chat")).expect_err("duplicate pair");
    assert_module_err(&err, "watch already registered");
    abort_block(&mut f);
}

#[test]
fn watch_table_full_at_max_watches() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    // fill the watch table honestly in ONE block: MAX_WATCHES distinct pairs
    // (system may register for any module), then one more overflows the cap.
    for i in 0..files::MAX_WATCHES {
        exec(
            &mut f,
            sdk::Origin::System,
            1,
            watch_op("/shared", &format!("m{i}")),
        )
        .expect("watch fits under the cap");
    }
    let err = exec(
        &mut f,
        sdk::Origin::System,
        1,
        watch_op("/shared", "overflow"),
    )
    .expect_err("cap reached");
    assert_module_err(&err, "watch table is full");
    abort_block(&mut f);
}

#[test]
fn watch_prefix_must_be_canonical() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    // a non-absolute prefix is rejected by `canonical`.
    let err = exec(&mut f, sdk::Origin::System, 1, watch_op("shared", "chat"))
        .expect_err("non-absolute prefix");
    assert_module_err(&err, "absolute");
    abort_block(&mut f);
    // a trailing slash surfaces as an empty segment and is rejected.
    let err = exec(&mut f, sdk::Origin::System, 1, watch_op("/shared/", "chat"))
        .expect_err("trailing slash");
    assert_module_err(&err, "empty or dot segment");
    abort_block(&mut f);
}

// ---- unwatch ----------------------------------------------------------------

#[test]
fn unwatch_gate_and_absent() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    exec(&mut f, md("chat"), 1, watch_op("/shared", "chat")).expect("register");
    commit_block(&mut f);

    // external cannot unwatch.
    let err =
        exec(&mut f, ext(b"x"), 2, unwatch_op("/shared", "chat")).expect_err("external blocked");
    assert_module_err(&err, "watch registration is module-origin only");
    abort_block(&mut f);

    // a foreign module cannot unwatch chat's watch.
    let err = exec(&mut f, md("automations"), 3, unwatch_op("/shared", "chat"))
        .expect_err("foreign module");
    assert_module_err(&err, "a module may only watch for itself");
    abort_block(&mut f);

    // chat, the registrant, can — and the watch is gone.
    exec(&mut f, md("chat"), 4, unwatch_op("/shared", "chat")).expect("chat unwatches");
    commit_block(&mut f);
    assert!(decoded_refs(&f).watches.is_empty(), "watch removed");

    // unwatch of an absent pair → not found.
    let err = exec(&mut f, md("chat"), 5, unwatch_op("/shared", "chat")).expect_err("absent");
    assert_module_err(&err, "watch not found");
    abort_block(&mut f);
}

// ---- BINDING: segment-boundary watch matching (task 9 review ruling) --------

#[test]
fn watch_segment_boundary_does_not_leak_across_names() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    // system registers a watch on /shared for "indexer", committed.
    exec(
        &mut f,
        sdk::Origin::System,
        1,
        watch_op("/shared", "indexer"),
    )
    .expect("register");
    commit_block(&mut f);

    // a commit to /sharedsecret/x (system arbitrary-root write) must NOT notify:
    // "/shared" is a substring of "/sharedsecret/x" but NOT a segment prefix.
    let ctx = commit(
        &mut f,
        sdk::Origin::System,
        2,
        None,
        vec![put_inline("/sharedsecret/x", b"a")],
    )
    .expect("commit under a different top-level name");
    commit_block(&mut f);
    assert!(
        watch_msgs(&ctx).is_empty(),
        "no false-positive notification across the segment boundary"
    );

    // a commit to /shared/x DOES notify — it descends through a real "/" boundary.
    let ctx = commit(
        &mut f,
        sdk::Origin::System,
        3,
        None,
        vec![put_inline("/shared/x", b"b")],
    )
    .expect("commit under the watched prefix");
    commit_block(&mut f);
    assert_eq!(
        watch_msgs(&ctx).len(),
        1,
        "fires under the real segment prefix"
    );
    assert_eq!(watch_msgs(&ctx)[0].target, "indexer");
}

#[test]
fn watch_root_prefix_fires_for_everything() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    // the everything-watch "/" fires for any committed path.
    exec(&mut f, sdk::Origin::System, 1, watch_op("/", "indexer")).expect("register root watch");
    commit_block(&mut f);

    let ctx = commit(
        &mut f,
        sdk::Origin::System,
        2,
        None,
        vec![put_inline("/sharedsecret/x", b"a")],
    )
    .expect("commit");
    commit_block(&mut f);
    assert_eq!(
        watch_msgs(&ctx).len(),
        1,
        "root watch fires for /sharedsecret/x"
    );
    assert_eq!(watch_msgs(&ctx)[0].target, "indexer");

    let ctx = commit(
        &mut f,
        sdk::Origin::System,
        3,
        None,
        vec![put_inline("/shared/y", b"b")],
    )
    .expect("commit");
    commit_block(&mut f);
    assert_eq!(
        watch_msgs(&ctx).len(),
        1,
        "root watch fires for /shared/y too"
    );
    assert_eq!(watch_msgs(&ctx)[0].target, "indexer");
}

// ---- root-movement discipline: staged-then-abort is a no-op -----------------

#[test]
fn pin_stage_then_abort_leaves_root_and_refs_untouched() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let head = seed_head(&mut f, 1); // a durable refs file now exists
    let root0 = f.root();
    let refs_path = d.path().join("refs");
    let refs_before = std::fs::read(&refs_path).expect("refs file exists after commit_block");

    exec(&mut f, sdk::Origin::System, 2, pin_op(&head, "v1")).expect("pin stages");
    assert_eq!(f.root(), root0, "pin only stages the pending overlay");
    abort_block(&mut f);
    assert_eq!(f.root(), root0, "abort leaves the committed root put");
    assert_eq!(
        std::fs::read(&refs_path).unwrap(),
        refs_before,
        "abort never touched the refs file on disk"
    );
    assert!(
        decoded_refs(&f).pins.is_empty(),
        "the aborted pin was never committed"
    );
}

#[test]
fn watch_stage_then_abort_leaves_root_and_refs_untouched() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_head(&mut f, 1); // a durable refs file now exists
    let root0 = f.root();
    let refs_path = d.path().join("refs");
    let refs_before = std::fs::read(&refs_path).expect("refs file exists after commit_block");

    exec(&mut f, md("chat"), 2, watch_op("/shared", "chat")).expect("watch stages");
    assert_eq!(f.root(), root0, "watch only stages the pending overlay");
    abort_block(&mut f);
    assert_eq!(f.root(), root0, "abort leaves the committed root put");
    assert_eq!(
        std::fs::read(&refs_path).unwrap(),
        refs_before,
        "abort never touched the refs file on disk"
    );
    assert!(
        decoded_refs(&f).watches.is_empty(),
        "the aborted watch was never committed"
    );
}
