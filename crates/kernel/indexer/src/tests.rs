//! store tests. the fold path runs the REAL pipeline — the committed testmap
//! fixture (crates/kernel/index-guest/testmap, refreshed by `make
//! wasm-modules`) installed in a real engine, folded by the engine's trigger
//! runner. every wait is on the database's own subscription stream, never a
//! poll: a missing event fails the test at the deadline instead of flaking
//! early. guest-failure queue-holding semantics (a failing fold retains its
//! events and backs off) are the engine's own tested contract
//! (fluent31/tests/trigger_robustness.rs) and are not re-proven here; the
//! synchronous view path covers `Fail` surfacing.

use super::*;

use std::collections::BTreeSet;
use std::time::Duration;

use fluent31::StreamEvent;

/// the reference mapper: a committed index guest whose derived key space is
/// `seen/{height}/{seq}` and `count`. Its source ships from ducktape-sdk; the
/// bytes are pinned here beside the three suites that fold through them.
const TESTMAP: &[u8] = include_bytes!("../tests/fixtures/testmap.index.wasm");
/// how long a fold may take before the test calls it stuck. generous: the
/// wait is event-driven, so healthy runs never sit it out.
const RECV_DEADLINE: Duration = Duration::from_secs(60);

/// a hand-assembled CORE module — `(module (func (export "on_apply")
/// (result i32) i32.const 0))` — exporting a role entry point but no
/// `memory`. Readiness (`role list contains on_apply or query`) alone would
/// pass this; fluent31's own `install_module` (rev 76ba1867,
/// `crates/fluent31/src/wasm/mod.rs:181-203`) additionally requires a
/// `memory` export, so this is exactly the shape #1839 describes: loadable
/// by readiness, refused by install. Assembled by hand rather than pulling
/// in a `wat` dependency this crate doesn't otherwise need.
#[rustfmt::skip]
const NO_MEMORY_MAPPER: &[u8] = &[
    0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00, // magic, version
    // type section: one type, () -> i32
    0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7F,
    // function section: one function, type 0
    0x03, 0x02, 0x01, 0x00,
    // export section: "on_apply" -> func 0
    0x07, 0x0C, 0x01, 0x08,
    0x6F, 0x6E, 0x5F, 0x61, 0x70, 0x70, 0x6C, 0x79, // "on_apply"
    0x00, 0x00,
    // code section: one body, i32.const 0; end
    0x0A, 0x06, 0x01, 0x04, 0x00, 0x41, 0x00, 0x0B,
];

fn bare_store(dir: &Path) -> IndexStore {
    let modules = [IndexModule::bare("chat"), IndexModule::bare("tasks")];
    IndexStore::open(dir, &modules).expect("open store")
}

fn mapped_store(dir: &Path) -> IndexStore {
    let modules = [
        IndexModule {
            id: "chat",
            guest: Some(TESTMAP),
        },
        IndexModule::bare("tasks"),
    ];
    IndexStore::open(dir, &modules).expect("open store")
}

fn chat_op(payload: &[u8]) -> AppliedOp {
    AppliedOp {
        module: "chat".into(),
        origin: OriginTag::external("jess"),
        payload: payload.to_vec(),
        assigned: Vec::new(),
    }
}

fn tasks_op() -> AppliedOp {
    AppliedOp {
        module: "tasks".into(),
        origin: OriginTag::module("chat"),
        payload: br#"{"create":"t"}"#.to_vec(),
        assigned: Vec::new(),
    }
}

fn block(height: u64, ops: Vec<AppliedOp>) -> BlockOps {
    BlockOps {
        height,
        time: 1_000 + height,
        ops,
        record: None,
    }
}

fn block_with_record(height: u64, ops: Vec<AppliedOp>) -> BlockOps {
    BlockOps {
        record: Some(format!(r#"{{"height":{height}}}"#).into_bytes()),
        ..block(height, ops)
    }
}

/// block on the subscription until every expected key has streamed by.
fn wait_for_keys(sub: &mut fluent31::Subscription, expect: impl IntoIterator<Item = String>) {
    let mut expect: BTreeSet<Vec<u8>> = expect.into_iter().map(String::into_bytes).collect();
    while !expect.is_empty() {
        let event = sub
            .recv_timeout(RECV_DEADLINE)
            .expect("subscription stream healthy")
            .expect("expected derived write never streamed — the fold is stuck");
        let StreamEvent::Batch(entries) = event else {
            panic!("subscription lagged mid-test");
        };
        for entry in entries {
            expect.remove(&entry.key);
        }
    }
}

fn seen_key(height: u64, seq: u32) -> String {
    format!("seen/{height:016x}/{seq:04x}")
}

/// block on a `fold/` subscription until the tip reaches `target`, returning
/// every tip position that streamed by. one entry per fold INVOCATION, so the
/// list is also the transcript of how the engine cut the batch.
fn wait_for_tip(sub: &mut fluent31::Subscription, target: (u64, u32)) -> Vec<(u64, u32)> {
    let mut seen = Vec::new();
    loop {
        let event = sub
            .recv_timeout(RECV_DEADLINE)
            .expect("subscription stream healthy")
            .expect("the fold tip never streamed — the fold is stuck");
        let StreamEvent::Batch(entries) = event else {
            panic!("subscription lagged mid-test");
        };
        for entry in entries {
            assert_eq!(entry.key, FOLD_TIP.as_bytes(), "only the tip lives here");
            let value = entry.value.expect("a tip write always carries its value");
            seen.push(index_guest::decode_fold_tip(&value).expect("a well-formed tip"));
        }
        if seen.last() == Some(&target) {
            return seen;
        }
    }
}

// ----------------------------------------------------------------------------
// the host feed
// ----------------------------------------------------------------------------

#[test]
fn op_rows_land_per_module_in_drain_order() {
    let dir = tempfile::tempdir().unwrap();
    let store = bare_store(dir.path());

    store
        .apply_block(&block(
            1,
            vec![
                chat_op(br#"{"post":"hi"}"#),
                tasks_op(),
                chat_op(br#"{"post":"again"}"#),
            ],
        ))
        .expect("apply");

    let page = store.scan("chat", OP_PREFIX.as_bytes(), None, 10).unwrap();
    assert_eq!(page.entries.len(), 2);
    assert!(!page.has_more);
    // block-wide seq survives the per-module split: chat got 0 and 2.
    assert_eq!(page.entries[0].0, op_key(1, 0).into_bytes());
    assert_eq!(page.entries[1].0, op_key(1, 2).into_bytes());

    let row: OpRow = borsh::from_slice(&page.entries[0].1).unwrap();
    assert_eq!(row.height, 1);
    assert_eq!(row.seq, 0);
    assert_eq!(row.time, 1_001);
    assert_eq!(row.origin, OriginTag::external("jess"));
    assert_eq!(row.payload, br#"{"post":"hi"}"#.to_vec());

    assert_eq!(store.applied_height("chat").unwrap(), 1);
    assert_eq!(store.applied_height("tasks").unwrap(), 1);
}

#[test]
fn replay_is_idempotent_per_module() {
    let dir = tempfile::tempdir().unwrap();
    let store = bare_store(dir.path());

    let b1 = block(1, vec![chat_op(b"{}")]);
    store.apply_block(&b1).expect("first apply");
    store
        .apply_block(&b1)
        .expect("replay is a skip, not an error");

    let page = store.scan("chat", OP_PREFIX.as_bytes(), None, 10).unwrap();
    assert_eq!(page.entries.len(), 1, "no duplicate rows on replay");
    assert_eq!(store.applied_height("chat").unwrap(), 1);
}

#[test]
fn watermarks_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = bare_store(dir.path());
        store.apply_block(&block(1, vec![chat_op(b"{}")])).unwrap();
        store.apply_block(&block(2, vec![chat_op(b"{}")])).unwrap();
    }
    let store = bare_store(dir.path());
    assert_eq!(store.applied_height("chat").unwrap(), 2);
    assert_eq!(
        store.applied_height("tasks").unwrap(),
        2,
        "a quiet module's watermark advances with every block — watermark \
         lag must mean missing blocks, not missing ops"
    );
    assert_eq!(
        store.resume_height().unwrap(),
        2,
        "resume from the max watermark"
    );
    let page = store.scan("chat", OP_PREFIX.as_bytes(), None, 10).unwrap();
    assert_eq!(page.entries.len(), 2);
}

#[test]
fn scan_pages_with_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let store = bare_store(dir.path());
    for h in 1..=5 {
        store.apply_block(&block(h, vec![chat_op(b"{}")])).unwrap();
    }

    let first = store.scan("chat", OP_PREFIX.as_bytes(), None, 2).unwrap();
    assert_eq!(first.entries.len(), 2);
    assert!(first.has_more);
    let cursor = first.next_after.clone().expect("cursor when has_more");

    let second = store
        .scan("chat", OP_PREFIX.as_bytes(), Some(cursor.as_bytes()), 10)
        .unwrap();
    assert_eq!(second.entries.len(), 3, "resumes strictly after the cursor");
    assert!(!second.has_more);
    assert!(second.next_after.is_none());
    assert_eq!(second.entries[0].0, op_key(3, 0).into_bytes());
}

#[test]
fn scan_pages_by_byte_budget_not_just_row_count() {
    // #1809: a page well under MAX_SCAN_LIMIT rows can still be gigabytes —
    // duckfs stage chunks put megabyte payloads in op rows. three ~1.5 MiB
    // rows blow the 4 MiB page budget on the third row, so a `limit` of 10
    // still pages after 2 entries.
    let dir = tempfile::tempdir().unwrap();
    let store = bare_store(dir.path());
    let big_payload = vec![b'x'; 3 * 1024 * 1024 / 2];
    for h in 1..=3 {
        store
            .apply_block(&block(h, vec![chat_op(&big_payload)]))
            .unwrap();
    }

    let page = store.scan("chat", OP_PREFIX.as_bytes(), None, 10).unwrap();
    assert_eq!(
        page.entries.len(),
        2,
        "stops adding rows once the running total would exceed MAX_INDEX_PAGE_BYTES"
    );
    assert!(page.has_more);
    let cursor = page.next_after.clone().expect("cursor when has_more");

    let rest = store
        .scan("chat", OP_PREFIX.as_bytes(), Some(cursor.as_bytes()), 10)
        .unwrap();
    assert_eq!(
        rest.entries.len(),
        1,
        "the cursor resumes past the budget cut"
    );
    assert!(!rest.has_more);
}

#[test]
fn scan_never_wedges_on_a_single_row_over_the_byte_budget() {
    // the budget check only fires once a page already holds a row — a lone
    // oversized row must still page out instead of returning an empty page
    // forever.
    let dir = tempfile::tempdir().unwrap();
    let store = bare_store(dir.path());
    let huge_payload = vec![b'x'; 5 * 1024 * 1024];
    store
        .apply_block(&block(1, vec![chat_op(&huge_payload)]))
        .unwrap();

    let page = store.scan("chat", OP_PREFIX.as_bytes(), None, 10).unwrap();
    assert_eq!(page.entries.len(), 1, "at least one row always ships");
    assert!(!page.has_more);
}

#[test]
fn an_undeclared_modules_ops_are_skipped_not_poisoned() {
    // a module admitted after boot (modules register) has no database
    // here: its ops write nothing, every declared module's watermark still
    // advances past the block, and the store keeps taking writes.
    let dir = tempfile::tempdir().unwrap();
    let store = bare_store(dir.path());
    let admitted = block(
        1,
        vec![AppliedOp {
            module: "ghost".into(),
            origin: OriginTag::system(),
            payload: b"{}".to_vec(),
            assigned: Vec::new(),
        }],
    );
    store.apply_block(&admitted).unwrap();
    assert!(!store.is_poisoned());
    assert_eq!(
        store.applied_height("chat").unwrap(),
        1,
        "quiet declared modules advance past the admitted module's block"
    );
    store.apply_block(&block(2, vec![chat_op(b"{}")])).unwrap();
    assert_eq!(store.applied_height("chat").unwrap(), 2);
    // asked for BY NAME the undeclared id is still its own error.
    assert!(matches!(
        store.view("ghost", b"count"),
        Err(Error::UnknownModule(_))
    ));
}

// ----------------------------------------------------------------------------
// the guest fold + view
// ----------------------------------------------------------------------------

#[test]
fn guest_folds_ops_and_serves_the_view() {
    let dir = tempfile::tempdir().unwrap();
    let store = mapped_store(dir.path());

    // subscribe BEFORE the write so no derived event can slip past.
    let mut sub = store.subscribe("chat", b"seen/", Some(b"seen0")).unwrap();
    store
        .apply_block(&block(
            1,
            vec![
                chat_op(br#"{"post":"hi"}"#),
                tasks_op(),
                chat_op(br#"{"post":"again"}"#),
            ],
        ))
        .unwrap();
    wait_for_keys(&mut sub, [seen_key(1, 0), seen_key(1, 2)]);

    // the fold's transaction committed rows and counter atomically.
    assert_eq!(
        store.view("chat", b"count").unwrap(),
        2u64.to_be_bytes().to_vec()
    );
    assert_eq!(
        store.view("chat", seen_key(1, 0).as_bytes()).unwrap(),
        br#"{"post":"hi"}"#.to_vec()
    );

    // a guest Fail is the module refusing the REQUEST — message intact.
    let Err(Error::View(msg)) = store.view("chat", b"boom") else {
        panic!("poison view request must surface as Error::View");
    };
    assert!(
        msg.contains("poison view request"),
        "guest message travels: {msg}"
    );

    // no guest (or no query role) → no materialized view; unknown module is
    // its own error.
    assert!(matches!(
        store.view("tasks", b"count"),
        Err(Error::ViewUnsupported)
    ));
    assert!(matches!(
        store.view("ghost", b"count"),
        Err(Error::UnknownModule(_))
    ));

    // fold health is observable exactly where a fold exists.
    assert!(store.fold_status("chat").unwrap().is_some());
    assert!(store.fold_status("tasks").unwrap().is_none());
}

/// THE TIP IS A ROW POSITION, NOT A BLOCK NUMBER. The shared shell records
/// `(height, seq)` of the last op row it CONSUMED, which is what makes
/// "is my op in the view yet" answerable: a height alone cannot tell a block
/// that folded whole from one the engine is halfway through.
#[test]
fn the_fold_tip_records_the_last_op_row_consumed() {
    let dir = tempfile::tempdir().unwrap();
    let store = mapped_store(dir.path());

    // a database that never folded reports UNKNOWN, never a zero position.
    assert_eq!(store.fold_tip("chat").unwrap(), None);

    let mut sub = store
        .subscribe("chat", FOLD_PREFIX.as_bytes(), Some(b"fold0"))
        .unwrap();
    // the block's LAST dispatch is tasks', so chat's tip must be chat's own
    // last row (1, 2) — the block-wide seq, kept exactly as the feed spells it.
    store
        .apply_block(&block(
            1,
            vec![chat_op(b"one"), tasks_op(), chat_op(b"two")],
        ))
        .unwrap();
    assert_eq!(wait_for_tip(&mut sub, (1, 2)), vec![(1, 2)]);
    assert_eq!(store.fold_tip("chat").unwrap(), Some((1, 2)));

    // a module with no folding guest has no tip to report — absent, and the
    // op FEED watermark says nothing about it either way.
    assert_eq!(store.fold_tip("tasks").unwrap(), None);
    assert_eq!(store.applied_height("tasks").unwrap(), 1);

    // a quiet block advances the feed watermark and leaves the tip alone —
    // the honest shape: the tip vouches for folded ROWS, not for blocks.
    store.apply_block(&block(2, vec![tasks_op()])).unwrap();
    assert_eq!(store.applied_height("chat").unwrap(), 2);
    assert_eq!(store.fold_tip("chat").unwrap(), Some((1, 2)));
}

/// A BLOCK THE ENGINE CUTS MID-BATCH NEVER OVER-CLAIMS. fluent31's
/// `trigger_batch` hands a fold at most 512 events per invocation, so a block
/// past that folds in several transactions — and the tip after each one has to
/// name where the cut FELL, not where the block ends. This is the whole reason
/// the record is `(height, seq)`.
#[test]
fn a_batch_cut_mid_block_parks_the_tip_at_the_cut() {
    /// past fluent31's `trigger_batch` (512), so the engine must cut.
    const OPS: u32 = 600;

    let dir = tempfile::tempdir().unwrap();
    let store = mapped_store(dir.path());
    let mut sub = store
        .subscribe("chat", FOLD_PREFIX.as_bytes(), Some(b"fold0"))
        .unwrap();

    let ops = (0..OPS).map(|n| chat_op(format!("op{n}").as_bytes()));
    store.apply_block(&block(1, ops.collect())).unwrap();

    let tips = wait_for_tip(&mut sub, (1, OPS - 1));
    assert!(
        tips.len() > 1,
        "{OPS} ops cannot fit one invocation — the cut is what this pins: {tips:?}"
    );
    assert!(
        tips.windows(2).all(|pair| pair[0] < pair[1]),
        "the tip only ever moves forward: {tips:?}"
    );
    // the intermediate tip is a REAL row of this block, strictly inside it: a
    // shell that stamped the block's end at the first invocation would claim
    // rows it had not folded yet.
    let cut = tips[0];
    assert_eq!(cut.0, 1);
    assert!(
        cut.1 < OPS - 1,
        "the first invocation stopped short: {cut:?}"
    );
    assert!(
        store
            .get("chat", seen_key(cut.0, cut.1).as_bytes())
            .unwrap()
            .is_some(),
        "the tip names a row whose derived write is committed"
    );
    assert_eq!(store.fold_tip("chat").unwrap(), Some((1, OPS - 1)));
}

/// A MAPPER ARRIVING OVER AN EXISTING FEED DERIVES IT, NOT JUST WHAT COMES
/// NEXT. A changes-mode trigger delivers writes committed AFTER registration,
/// so registering one over a populated `op/` range replays nothing: the rows
/// already in the feed would stay forever underived while the views claimed to
/// serve them. `converge_guest` re-writes the feed so the fold consumes it.
#[test]
fn a_new_mapper_folds_the_op_feed_the_database_already_held() {
    let dir = tempfile::tempdir().unwrap();
    {
        // no guest: the feed accumulates with nothing folding behind it.
        let store = bare_store(dir.path());
        store.apply_block(&block(1, vec![chat_op(b"one")])).unwrap();
        store
            .apply_block(&block(2, vec![chat_op(b"two"), chat_op(b"three")]))
            .unwrap();
        assert!(
            store
                .scan("chat", b"seen/", None, 10)
                .unwrap()
                .entries
                .is_empty()
        );
    }
    let store = mapped_store(dir.path());
    // NO BARRIER HERE, ON PURPOSE. `open` waits the refold out itself, and
    // that is the property under test: `replay_op_feed` only stages the
    // re-writes, so an `open` that returned before the runner drained them
    // would hand a node a read model that is the freshly CLEARED keyspace —
    // every view answering "no such row" while the feed re-derives behind it.
    // the last row of the pre-existing feed is (2, 1) — the refold walks the
    // whole thing in key order, which for op keys IS block-and-drain order.
    assert_eq!(store.fold_tip("chat").unwrap(), Some((2, 1)));
    assert_eq!(
        store.view("chat", b"count").unwrap(),
        3u64.to_be_bytes().to_vec(),
        "all three pre-existing ops folded"
    );
    for (height, seq) in [(1, 0), (2, 0), (2, 1)] {
        assert!(
            store
                .get("chat", seen_key(height, seq).as_bytes())
                .unwrap()
                .is_some(),
            "row ({height}, {seq}) was derived from the feed already on disk"
        );
    }
    assert_eq!(
        store.applied_height("chat").unwrap(),
        2,
        "the feed watermark is untouched"
    );
}

/// A MAPPER SWAP RE-DERIVES ITS ROWS INSTEAD OF INHERITING THE PREVIOUS
/// MAPPER'S. Derived rows are the OUTPUT of a mapper, so after a swap the ones
/// on disk are the old mapper's — and the fold tip vouches for them, which is
/// the hazard `indexable-spec.md` §3.2.4 names. The clear is the half that
/// makes the replay honest: the fixture's `count` is a read-modify-write, so a
/// replay onto UNCLEARED rows would double it.
#[test]
fn a_mapper_swap_clears_and_refolds_the_read_model() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = mapped_store(dir.path());
        let mut sub = store
            .subscribe("chat", FOLD_PREFIX.as_bytes(), Some(b"fold0"))
            .unwrap();
        store
            .apply_block(&block(1, vec![chat_op(b"one"), chat_op(b"two")]))
            .unwrap();
        wait_for_tip(&mut sub, (1, 1));
        assert_eq!(
            store.view("chat", b"count").unwrap(),
            2u64.to_be_bytes().to_vec()
        );
    }
    // the SAME mapper carrying one extra custom section: different bytes,
    // identical behaviour, so what changes across the reopen is the hash the
    // marker compares — the upgrade signal itself, with the fold's answer held
    // constant so the assertions below can be exact.
    let restamped = restamped_testmap();
    assert_ne!(
        restamped, TESTMAP,
        "the swap must actually change the bytes"
    );
    let store = IndexStore::open(
        dir.path(),
        &[
            IndexModule {
                id: "chat",
                guest: Some(&restamped),
            },
            IndexModule::bare("tasks"),
        ],
    )
    .expect("open store");

    // again with no barrier: the swap's replay is finished by the time `open`
    // answers, or these rows are not there to read.
    assert_eq!(
        store.view("chat", b"count").unwrap(),
        2u64.to_be_bytes().to_vec(),
        "re-derived from zero: a replay onto uncleared rows would read 4"
    );
    let seen = store.scan("chat", b"seen/", None, 10).unwrap();
    assert_eq!(seen.entries.len(), 2, "every row is back, none duplicated");
    assert_eq!(
        store.fold_tip("chat").unwrap(),
        Some((1, 1)),
        "the tip is re-established over rows the INSTALLED mapper wrote"
    );
    // the feed and its bookkeeping are the host's, and a mapper change says
    // nothing about them.
    assert_eq!(store.applied_height("chat").unwrap(), 1);
    assert_eq!(
        store
            .scan("chat", OP_PREFIX.as_bytes(), None, 10)
            .unwrap()
            .entries
            .len(),
        2,
        "the op feed is what the refold READ — it is never wiped"
    );
}

/// THE OPEN WAITS THE REFOLD OUT — BUT A GUEST THAT CANNOT FOLD ITS OWN FEED
/// NO LONGER TAKES THE WHOLE NODE DOWN WITH IT (#1725/#1726).
///
/// `replay_op_feed` only STAGES its re-writes — the engine folds them on a
/// background runner — so `open` still waits `drain_fold` out: an open that
/// returned early would publish a store whose read model is the keyspace
/// `clear_derived` just emptied, every view answering "no such row" for the
/// length of the whole feed. But once that wait resolves to `FoldStuck` (a
/// poisoned op the guest refuses forever, never draining), the derived tier
/// is node-local and rebuildable, so ONE module's guest rejecting its own
/// feed must not refuse the whole `open`: `converge_guest` warns and still
/// writes the marker, leaving the module exactly as stuck as `drain_fold`
/// found it — visible through `fold_status`, never through `is_poisoned`
/// (that flag is for HOST-write failures, and a guest-fold error never sets
/// it) — while `open` succeeds and every other module indexes normally.
#[test]
fn a_guest_stuck_at_converge_does_not_refuse_the_open() {
    let dir = tempfile::tempdir().unwrap();
    {
        // no guest, so the feed takes the poison op with nothing folding: the
        // failure belongs to the REFOLD, not to the write that fed it.
        let store = bare_store(dir.path());
        store
            .apply_block(&block(1, vec![chat_op(b"one"), chat_op(b"boom")]))
            .unwrap();
    }

    let store = IndexStore::open(
        dir.path(),
        &[
            IndexModule {
                id: "chat",
                guest: Some(TESTMAP),
            },
            IndexModule::bare("tasks"),
        ],
    )
    .expect("a stuck guest fold must not refuse the open");

    let status = store
        .fold_status("chat")
        .unwrap()
        .expect("chat still folds — its guest just cannot clear its own backlog");
    assert!(status.pending > 0, "the poison op is still queued");
    assert!(
        !store.is_poisoned(),
        "a guest-fold failure must never poison the host-write path"
    );
    // "tasks" shares nothing with chat's stuck fold: its feed watermark still
    // advances on every block, ops or not (every module's does).
    assert_eq!(store.applied_height("tasks").unwrap(), 1);
}

/// #1725: A FAILED REFOLD MUST RESTORE ITS MARKER TOO, NOT ONLY ON SUCCESS.
/// Before this fix `refold` dropped `meta/guest` first and put it back only
/// after `refold_feed` returned `Ok`, so a live guest failure left the
/// marker permanently absent — the next `open` would find none, replay the
/// WHOLE feed from scratch to rebuild it, hit the identical failure, and
/// (pre-#1726 too) refuse to open at all. Restoring the marker on the error
/// path is what lets a warm boot recognize its own guest and skip that
/// replay outright instead of re-triggering the same brick.
#[test]
fn a_failed_refold_restores_its_marker() {
    let dir = tempfile::tempdir().unwrap();
    let store = mapped_store(dir.path());
    store.apply_block(&block(1, vec![chat_op(b"one")])).unwrap();
    store.wait_folds_drained().unwrap();
    assert_eq!(store.fold_tip("chat").unwrap(), Some((1, 0)));

    // a poisoned row lands in the feed the same way a join-seam backfill's
    // would (indexable spec §7) before `repair_read_model` calls `refold`.
    store
        .apply_block(&block(2, vec![chat_op(b"boom")]))
        .unwrap();

    let db = store.db("chat").unwrap();
    let marker_before = db.get(META_GUEST.as_bytes()).unwrap();
    assert!(marker_before.is_some(), "converge already wrote one");

    let err = store.refold("chat").unwrap_err();
    assert!(
        matches!(err, Error::FoldStuck(_)),
        "the guest's own Fail is what refold surfaces: {err}"
    );

    let marker_after = db.get(META_GUEST.as_bytes()).unwrap();
    assert_eq!(
        marker_before, marker_after,
        "the marker must come back even though the refold failed"
    );
}

/// The reference mapper with one empty custom section appended: a wasm module
/// stays valid under any trailing custom section, so this is byte-different
/// and behaviour-identical — exactly the upgrade shape a swap test needs and
/// cannot get from a second fixture without also changing what it folds.
fn restamped_testmap() -> Vec<u8> {
    const NAME: &[u8] = b"ducktape.refold-test";
    let mut wasm = TESTMAP.to_vec();
    let mut section = vec![NAME.len() as u8];
    section.extend_from_slice(NAME);
    wasm.push(0); // custom section id
    wasm.push(section.len() as u8); // section size (a single LEB128 byte)
    wasm.extend_from_slice(&section);
    wasm
}

#[test]
fn backfill_wipes_derived_state_and_recreates_the_fold() {
    let dir = tempfile::tempdir().unwrap();
    let store = mapped_store(dir.path());

    let mut sub = store.subscribe("chat", b"seen/", Some(b"seen0")).unwrap();
    store.apply_block(&block(1, vec![chat_op(b"one")])).unwrap();
    wait_for_keys(&mut sub, [seen_key(1, 0)]);
    assert_eq!(store.fold_tip("chat").unwrap(), Some((1, 0)));

    store.mark_backfilled("chat", 5).unwrap();
    assert_eq!(store.applied_height("chat").unwrap(), 5);
    assert_eq!(store.backfill_height("chat").unwrap(), Some(5));
    // THE TIP GOES WITH THE ROWS IT VOUCHED FOR. `clear_db` wipes it like any
    // other derived key, so a boundary stamp reports UNKNOWN rather than a
    // position whose derived state no longer exists — which is exactly why a
    // client waiting on the tip must escape by timeout instead of blocking.
    assert_eq!(store.fold_tip("chat").unwrap(), None);
    let seen = store.scan("chat", b"seen/", None, 10).unwrap();
    assert!(seen.entries.is_empty(), "derived rows wiped");
    let ops = store.scan("chat", OP_PREFIX.as_bytes(), None, 10).unwrap();
    assert!(
        ops.entries.is_empty(),
        "op feed wiped — it honestly starts at the boundary"
    );

    // the fold trigger was re-registered: new blocks fold from a clean slate,
    // and no pre-wipe event ever resurrects a wiped row.
    let mut sub = store.subscribe("chat", b"seen/", Some(b"seen0")).unwrap();
    store.apply_block(&block(6, vec![chat_op(b"two")])).unwrap();
    wait_for_keys(&mut sub, [seen_key(6, 0)]);
    assert_eq!(
        store.view("chat", b"count").unwrap(),
        1u64.to_be_bytes().to_vec(),
        "the counter re-derives from zero — nothing pre-wipe survived"
    );
    let seen = store.scan("chat", b"seen/", None, 10).unwrap();
    assert_eq!(seen.entries.len(), 1);
}

#[test]
fn reopen_without_a_guest_converges_the_database() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = mapped_store(dir.path());
        let mut sub = store.subscribe("chat", b"seen/", Some(b"seen0")).unwrap();
        store.apply_block(&block(1, vec![chat_op(b"one")])).unwrap();
        wait_for_keys(&mut sub, [seen_key(1, 0)]);
    }
    {
        // the module stopped shipping an index guest: install + trigger go.
        let store = bare_store(dir.path());
        assert!(matches!(
            store.view("chat", b"count"),
            Err(Error::ViewUnsupported)
        ));
        assert!(store.fold_status("chat").unwrap().is_none());
        // Removing a mapper removes its output; the feed remains available
        // for any later mapper to derive a fresh view.
        let seen = store.scan("chat", b"seen/", None, 10).unwrap();
        assert!(seen.entries.is_empty());
        assert_eq!(
            store
                .scan("chat", OP_PREFIX.as_bytes(), None, 10)
                .unwrap()
                .entries
                .len(),
            1
        );
    }
    // and shipping one again REFOLDS: the guest is new to this database, so
    // the rows the previous one left are cleared and re-derived from the feed
    // before the next block is ever applied. `count` reads 2 for both ops, not
    // 3 (a replay onto the surviving row) and not 1 (no replay at all).
    let store = mapped_store(dir.path());
    let mut sub = store.subscribe("chat", b"seen/", Some(b"seen0")).unwrap();
    store.apply_block(&block(2, vec![chat_op(b"two")])).unwrap();
    wait_for_keys(&mut sub, [seen_key(2, 0)]);
    assert_eq!(
        store.view("chat", b"count").unwrap(),
        2u64.to_be_bytes().to_vec()
    );
}

// ----------------------------------------------------------------------------
// the blocks database
// ----------------------------------------------------------------------------

#[test]
fn block_records_serve_newest_first_tail_oldest_first() {
    let dir = tempfile::tempdir().unwrap();
    let store = bare_store(dir.path());
    for h in 1..=5 {
        store
            .apply_block(&block_with_record(h, vec![chat_op(b"{}")]))
            .unwrap();
    }

    let rows = store.recent_block_rows(3).unwrap();
    assert_eq!(rows.len(), 3);
    // the newest 3 (heights 3..=5), oldest-first — the ring's contract.
    assert_eq!(rows[0], br#"{"height":3}"#.to_vec());
    assert_eq!(rows[2], br#"{"height":5}"#.to_vec());
    assert_eq!(store.recent_block_rows(100).unwrap().len(), 5);
    assert_eq!(store.blocks_height().unwrap(), 5);
}

#[test]
fn block_record_lands_without_ops_and_advances_resume() {
    let dir = tempfile::tempdir().unwrap();
    let store = bare_store(dir.path());
    // a block whose op stream is empty for the index (e.g. a finalized-
    // but-rejected frame) still shows in the explorer.
    store
        .apply_block(&block_with_record(9, Vec::new()))
        .unwrap();

    assert_eq!(store.recent_block_rows(10).unwrap().len(), 1);
    // every module's watermark advances — quiet is not stale — but no op
    // rows were written.
    assert_eq!(store.applied_height("chat").unwrap(), 9);
    let ops = store.scan("chat", OP_PREFIX.as_bytes(), None, 10).unwrap();
    assert!(ops.entries.is_empty(), "no op rows");
    assert_eq!(store.resume_height().unwrap(), 9, "blocks watermark counts");
}

#[test]
fn block_record_without_feed_leaves_module_watermarks_alone() {
    let dir = tempfile::tempdir().unwrap();
    let store = bare_store(dir.path());
    // a boundary follower's write: the explorer row lands, the blocks
    // watermark advances, and NO module watermark moves — read models
    // answer to the boundary stamp, not to this row.
    store
        .apply_block_record(7, br#"{"height":7}"#.to_vec())
        .unwrap();

    assert_eq!(
        store.recent_block_rows(10).unwrap(),
        vec![br#"{"height":7}"#.to_vec()]
    );
    assert_eq!(store.blocks_height().unwrap(), 7);
    assert_eq!(store.applied_height("chat").unwrap(), 0);
    assert_eq!(store.applied_height("tasks").unwrap(), 0);

    // idempotent at or below the blocks watermark, exactly like the feed.
    store
        .apply_block_record(7, br#"{"height":"dup"}"#.to_vec())
        .unwrap();
    store
        .apply_block_record(3, br#"{"height":3}"#.to_vec())
        .unwrap();
    assert_eq!(
        store.recent_block_rows(10).unwrap(),
        vec![br#"{"height":7}"#.to_vec()]
    );
}

#[test]
fn block_records_are_idempotent_and_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = bare_store(dir.path());
        let b = block_with_record(1, vec![chat_op(b"{}")]);
        store.apply_block(&b).unwrap();
        store.apply_block(&b).expect("replay is a skip");
    }
    let store = bare_store(dir.path());
    let rows = store.recent_block_rows(10).unwrap();
    assert_eq!(rows.len(), 1, "no duplicate rows; rows survive reopen");
    assert_eq!(store.blocks_height().unwrap(), 1);
}

#[test]
fn prefix_successor_edges() {
    assert_eq!(prefix_successor(b"op/"), Some(b"op0".to_vec()));
    assert_eq!(prefix_successor(&[0x01, 0xff]), Some(vec![0x02]));
    assert_eq!(prefix_successor(&[0xff, 0xff]), None);
    assert_eq!(prefix_successor(b""), None);
}

// ----------------------------------------------------------------------------
// the joiner's op-row backfill
// ----------------------------------------------------------------------------

/// THE BACKFILL IS INDISTINGUISHABLE FROM HAVING SEEN THE BLOCKS. A store
/// stamped at a boundary and then fed the source's op rows in ASCENDING key
/// order must land on the same views, the same fold tip, and the same rows as
/// a twin that watched every block go by through `apply_block`.
///
/// The order is the whole argument: the fold trigger is changes-mode, so it
/// folds in COMMIT order — writing ascending makes commit order key order,
/// which is exactly block-and-drain order. Nothing here refolds.
#[test]
fn a_backfilled_store_matches_a_store_that_saw_the_blocks() {
    let live_dir = tempfile::tempdir().unwrap();
    let joiner_dir = tempfile::tempdir().unwrap();
    let live = mapped_store(live_dir.path());
    let joiner = mapped_store(joiner_dir.path());

    const BOUNDARY: u64 = 6;
    for h in 1..=BOUNDARY {
        live.apply_block(&block(h, vec![chat_op(b"payload"), tasks_op()]))
            .unwrap();
    }
    live.wait_folds_drained().unwrap();

    // the joiner's join seam: canonical state jumped to the boundary, so the
    // heal stamps — feed and views begin empty at BOUNDARY.
    joiner.mark_backfilled("chat", BOUNDARY).unwrap();
    assert_eq!(
        joiner
            .scan("chat", b"seen/", None, 100)
            .unwrap()
            .entries
            .len(),
        0
    );

    // ...then the source's rows land below it, ascending, one page at a time.
    let rows = live.scan("chat", OP_PREFIX.as_bytes(), None, 100).unwrap();
    assert_eq!(rows.entries.len(), BOUNDARY as usize);
    for page in rows.entries.chunks(2) {
        let page: Vec<(String, Vec<u8>)> = page
            .iter()
            .map(|(k, v)| (String::from_utf8(k.clone()).unwrap(), v.clone()))
            .collect();
        joiner.write_backfill_rows("chat", &page).unwrap();
    }
    joiner.wait_folds_drained().unwrap();
    joiner.set_backfill_floor("chat", None).unwrap();

    // parity: rows, derived views, and the fold tip. `meta/` is deliberately
    // out — it is host bookkeeping about the FEED, and the boundary stamp
    // already reset it (`meta/guest` included, so the next open re-converges).
    let derived = |s: &IndexStore| {
        s.scan("chat", b"", None, 1024)
            .unwrap()
            .entries
            .into_iter()
            .filter(|(k, _)| !k.starts_with(META_PREFIX.as_bytes()))
            .collect::<Vec<_>>()
    };
    assert_eq!(derived(&live), derived(&joiner), "every key byte-identical");
    assert_eq!(
        joiner.view("chat", b"count").unwrap(),
        live.view("chat", b"count").unwrap()
    );
    assert_eq!(
        joiner.fold_tip("chat").unwrap(),
        live.fold_tip("chat").unwrap()
    );
    assert_eq!(joiner.fold_tip("chat").unwrap(), Some((BOUNDARY, 0)));
    assert_eq!(
        joiner.backfill_height("chat").unwrap(),
        None,
        "floor cleared"
    );
}

/// A FEED EXTENDED DOWNWARD IS ONLY HONEST AFTER A REFOLD. The floored-module
/// seam appends rows BELOW the ones the fold already consumed — out of order
/// by construction, because nothing may be wiped ahead of a pull that might
/// fail — which walks the fold tip BACKWARDS: a caller that wrote at height 8
/// would read a tip of 5 and wait forever for its own op. Re-driving the whole
/// feed in key order is what puts the read model back in agreement with it,
/// byte-identical to a store that watched every block go by.
#[test]
fn a_refold_rebuilds_the_read_model_over_a_feed_extended_downward() {
    let live_dir = tempfile::tempdir().unwrap();
    let joiner_dir = tempfile::tempdir().unwrap();
    let live = mapped_store(live_dir.path());
    let joiner = mapped_store(joiner_dir.path());

    const FLOOR: u64 = 5;
    const TIP: u64 = 10;
    for h in 1..=TIP {
        live.apply_block(&block(h, vec![chat_op(b"payload")]))
            .unwrap();
    }
    live.wait_folds_drained().unwrap();

    // the restarted resident: stamped at a checkpoint, then the journal replay
    // folded the suffix back on top — watermark at the tip, floor at 5.
    joiner.mark_backfilled("chat", FLOOR).unwrap();
    for h in (FLOOR + 1)..=TIP {
        joiner
            .apply_block(&block(h, vec![chat_op(b"payload")]))
            .unwrap();
    }
    joiner.wait_folds_drained().unwrap();

    // ...and the seam pulls the history below the floor in UNDER it.
    let below: Vec<(String, Vec<u8>)> = live
        .scan("chat", OP_PREFIX.as_bytes(), None, 100)
        .unwrap()
        .entries
        .iter()
        .map(|(k, v)| (String::from_utf8(k.clone()).unwrap(), v.clone()))
        .filter(|(k, _)| parse_op_key(k.as_bytes()).is_some_and(|(h, _)| h <= FLOOR))
        .collect();
    assert_eq!(below.len(), FLOOR as usize);
    joiner.write_backfill_rows("chat", &below).unwrap();
    joiner.wait_folds_drained().unwrap();
    joiner.set_backfill_floor("chat", None).unwrap();
    assert_eq!(
        joiner.fold_tip("chat").unwrap(),
        Some((FLOOR, 0)),
        "the out-of-order fold left the tip below the feed it has"
    );

    joiner.refold("chat").unwrap();

    let derived = |s: &IndexStore| {
        s.scan("chat", b"", None, 1024)
            .unwrap()
            .entries
            .into_iter()
            .filter(|(k, _)| !k.starts_with(META_PREFIX.as_bytes()))
            .collect::<Vec<_>>()
    };
    assert_eq!(derived(&live), derived(&joiner), "every key byte-identical");
    assert_eq!(
        joiner.view("chat", b"count").unwrap(),
        live.view("chat", b"count").unwrap()
    );
    assert_eq!(
        joiner.fold_tip("chat").unwrap(),
        Some((TIP, 0)),
        "and the tip vouches for the whole feed again"
    );
}

/// A REFOLD MUST NOT LEAVE A MARKER VOUCHING FOR A KEYSPACE IT IS STILL
/// REBUILDING. `converge_guest` writes `meta/guest` LAST for this reason: it
/// says "this database is converged on that artifact", and a warm open that
/// finds it matching returns early without looking at the read model at all.
/// A refold that cleared and then crashed with the marker still in place would
/// therefore be adopted forever — a truncated read model with a fold tip below
/// its own feed, and nothing on the boot path to notice.
///
/// Pinned as a source shape because no behavioural test can see it: the window
/// exists only between two writes inside one call, and both orders leave the
/// same store behind when nothing crashes.
#[test]
fn the_refold_drops_its_guest_marker_before_clearing_and_writes_it_back_last() {
    const SRC: &str = include_str!("lib.rs");
    let body = SRC
        .split("pub fn refold(")
        .nth(1)
        .expect("refold is declared")
        .split("\n    /// ")
        .next()
        .expect("refold body");
    let dropped = body
        .find("delete(META_GUEST")
        .expect("the refold drops the marker");
    let cleared = body
        .find("clear_derived(")
        .expect("the refold clears the derived keyspace");
    // `refold_feed` is the replay AND the drain — the shared tail with
    // `converge_guest`.
    let drained = body.find("refold_feed(").expect("the refold drains");
    let written = body
        .find("put(META_GUEST")
        .expect("the refold writes the marker back");
    assert!(
        dropped < cleared,
        "the marker must come down BEFORE the keyspace it vouches for"
    );
    assert!(
        drained < written,
        "and go back up only AFTER the fold has caught up with the feed"
    );
}

/// ...AND AN INTERRUPTED ONE IS RE-RUN WHOLE. The payoff of the order above:
/// the state a crashed refold leaves — derived keyspace cleared, no marker —
/// is exactly the state `IndexStore::open` rebuilds from, so the truncation
/// heals at the next boot instead of being adopted.
#[test]
fn an_interrupted_refold_is_re_run_whole_by_the_next_open() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = mapped_store(dir.path());
        for h in 1..=5 {
            store
                .apply_block(&block(h, vec![chat_op(b"payload")]))
                .unwrap();
        }
        store.wait_folds_drained().unwrap();
        assert_eq!(store.fold_tip("chat").unwrap(), Some((5, 0)));

        // the wreckage of a refold that died after its clear.
        let db = &store.db("chat").expect("chat is open");
        db.delete(META_GUEST).unwrap();
        clear_derived(db).unwrap();
        assert_eq!(
            store.fold_tip("chat").unwrap(),
            None,
            "the read model is gone, and the feed still holds every row"
        );
        assert_eq!(
            store
                .scan("chat", OP_PREFIX.as_bytes(), None, 100)
                .unwrap()
                .entries
                .len(),
            5
        );
    }

    let reopened = mapped_store(dir.path());
    assert_eq!(
        reopened.fold_tip("chat").unwrap(),
        Some((5, 0)),
        "the next open refolded it whole"
    );
    assert_eq!(
        reopened
            .scan("chat", b"seen/", None, 100)
            .unwrap()
            .entries
            .len(),
        5,
        "every derived row is back"
    );
}

/// THE FLOOR IS THE ONLY THING A BACKFILL MOVES. `write_backfill_rows` must
/// never touch the watermark (the heal already stamped it at the boundary, and
/// it vouches for feed contiguity from the floor up), and the floor setter
/// must both compose a source's own truncation and clear it outright.
#[test]
fn backfill_writes_leave_the_watermark_and_only_the_floor_moves() {
    let dir = tempfile::tempdir().unwrap();
    let store = bare_store(dir.path());
    store.mark_backfilled("chat", 9).unwrap();
    assert_eq!(store.applied_height("chat").unwrap(), 9);
    assert_eq!(store.backfill_height("chat").unwrap(), Some(9));

    let row = borsh::to_vec(&OpRow {
        height: 3,
        seq: 0,
        time: 1_003,
        origin: OriginTag::external("jess"),
        payload: b"{}".to_vec(),
        assigned: Vec::new(),
    })
    .unwrap();
    store
        .write_backfill_rows("chat", &[(op_key(3, 0), row)])
        .unwrap();
    assert_eq!(
        store.applied_height("chat").unwrap(),
        9,
        "a backfill write never moves the watermark"
    );
    assert_eq!(
        store
            .scan("chat", OP_PREFIX.as_bytes(), None, 10)
            .unwrap()
            .entries
            .len(),
        1
    );

    // compose a late-joined source's own truncation, then clear it.
    store.set_backfill_floor("chat", Some(2)).unwrap();
    assert_eq!(store.backfill_height("chat").unwrap(), Some(2));
    store.set_backfill_floor("chat", None).unwrap();
    assert_eq!(store.backfill_height("chat").unwrap(), None);
    assert_eq!(
        store.applied_height("chat").unwrap(),
        9,
        "clearing the floor never moves the watermark either"
    );
}

// ----------------------------------------------------------------------------
// rendering
// ----------------------------------------------------------------------------

#[test]
fn user_handle_renders_names_and_keys() {
    assert_eq!(user_handle(b"jess"), "jess");
    assert_eq!(user_handle(&[0xab, 0xcd]), "abcd");
    assert_eq!(user_handle(b""), "");
    assert_eq!(user_handle(b"line\nbreak"), "6c696e650a627265616b");
}

// ----------------------------------------------------------------------------
// #1726: the drain's own progress/stall decision, pulled out of `drain_fold`
// so it is testable without driving fluent31's real retry/backoff timing.
// ----------------------------------------------------------------------------

#[test]
fn drain_verdict_calls_a_shrinking_backlog_progress() {
    assert_eq!(
        drain_verdict(3, 5, Duration::from_secs(999), Duration::from_secs(60)),
        DrainVerdict::Progress,
        "pending below every count seen so far is progress, no matter how long it took"
    );
}

#[test]
fn drain_verdict_waits_out_a_flat_backlog_within_the_stall_budget() {
    // this is the whole bug: a trigger that just recorded an error but has
    // not yet had time to retry (fluent31's own 100ms-to-6.4s backoff) looks
    // exactly like this — flat pending, short elapsed. it must NOT be Stuck.
    assert_eq!(
        drain_verdict(4, 4, Duration::from_millis(50), Duration::from_secs(60)),
        DrainVerdict::Waiting
    );
}

#[test]
fn drain_verdict_is_stuck_only_once_the_stall_budget_elapses() {
    assert_eq!(
        drain_verdict(4, 4, Duration::from_secs(60), Duration::from_secs(60)),
        DrainVerdict::Stuck,
        "flat pending for the whole budget is stuck — a permanent guest Fail \
         the engine retries forever must still surface, just not instantly"
    );
}

#[test]
fn drain_verdict_never_treats_equal_pending_as_progress() {
    // a boundary check: `<`, not `<=` — an unchanged backlog is not shrinking.
    assert_eq!(
        drain_verdict(4, 4, Duration::from_millis(1), Duration::from_secs(60)),
        DrainVerdict::Waiting
    );
}

#[test]
fn a_live_mapper_replacement_refolds_before_reads_resume() {
    let dir = tempfile::tempdir().unwrap();
    let store = mapped_store(dir.path());
    store
        .apply_block(&block(1, vec![chat_op(b"one"), chat_op(b"two")]))
        .unwrap();
    store.wait_folds_drained().unwrap();
    let restamped = restamped_testmap();
    store
        .converge(&[IndexModule {
            id: "chat",
            guest: Some(&restamped),
        }])
        .unwrap();
    assert_eq!(store.view("chat", b"count").unwrap(), 2u64.to_be_bytes());
    assert_eq!(
        store
            .scan("chat", b"seen/", None, 10)
            .unwrap()
            .entries
            .len(),
        2
    );
    assert_eq!(store.fold_tip("chat").unwrap(), Some((1, 1)));
    store.converge(&[IndexModule::bare("chat")]).unwrap();
    assert!(matches!(
        store.view("chat", b"count"),
        Err(Error::ViewUnsupported)
    ));
    assert!(
        store
            .scan("chat", b"seen/", None, 10)
            .unwrap()
            .entries
            .is_empty()
    );
}

#[test]
fn a_failed_mapper_replacement_refuses_only_its_own_module() {
    let dir = tempfile::tempdir().unwrap();
    let store = mapped_store(dir.path());
    assert!(
        store
            .converge(&[IndexModule {
                id: "chat",
                guest: Some(b"not wasm")
            }])
            .is_err()
    );
    // bad GUEST BYTES are this module's problem alone: the store stays
    // usable, and the op feed (chat's included) keeps advancing — a mapper
    // change never affects what the feed saw, only what it means.
    assert!(
        !store.is_poisoned(),
        "a rejected guest must not poison the whole store"
    );
    assert!(store.view("chat", b"count").is_err());
    assert!(store.get("chat", b"count").is_err());
    assert!(store.scan("chat", b"seen/", None, 10).is_err());
    store
        .apply_block(&block(1, vec![chat_op(b"one"), tasks_op()]))
        .expect("the op feed is not this module's poison to hold");
    assert_eq!(store.applied_height("chat").unwrap(), 1);
    assert_eq!(store.applied_height("tasks").unwrap(), 1);
}

#[test]
fn validate_guest_requires_memory_export_like_install_does() {
    let dir = tempfile::tempdir().unwrap();
    let store = bare_store(dir.path());
    // readiness (role coverage alone) would pass this; pin it against the
    // actual activation path so the two can never drift apart again.
    assert!(
        store.validate_guest(NO_MEMORY_MAPPER).is_err(),
        "readiness must refuse a mapper with no `memory` export"
    );
    assert!(
        store
            .converge(&[IndexModule {
                id: "chat",
                guest: Some(NO_MEMORY_MAPPER),
            }])
            .is_err(),
        "install would refuse the same bytes readiness now also refuses"
    );
}

#[test]
fn an_unchanged_rejection_short_circuits_without_recompiling() {
    let dir = tempfile::tempdir().unwrap();
    let store = bare_store(dir.path());
    let spec = || IndexModule {
        id: "chat",
        guest: Some(NO_MEMORY_MAPPER),
    };
    let first = store.converge(&[spec()]).unwrap_err();
    assert!(
        first.to_string().contains("memory"),
        "the first attempt reports the real reason: {first}"
    );
    // the identical candidate reconverged (`converge_host_modules` retries a
    // stuck deployment every block): a second wasmtime compile of the same
    // doomed bytes is wasted work, so this must be refused WITHOUT paying
    // for it — proven by the distinct short-circuit message, since a real
    // recompile would report "memory" again, not this one.
    let second = store.converge(&[spec()]).unwrap_err();
    assert!(
        second.to_string().contains("previously rejected"),
        "a repeat of the same rejected bytes must short-circuit: {second}"
    );
}

#[test]
fn a_changed_candidate_after_a_rejection_is_recompiled() {
    let dir = tempfile::tempdir().unwrap();
    let store = bare_store(dir.path());
    store
        .converge(&[IndexModule {
            id: "chat",
            guest: Some(NO_MEMORY_MAPPER),
        }])
        .unwrap_err();
    // different bytes after a rejection must NOT hit the short-circuit —
    // only an EXACT repeat of the doomed candidate skips recompiling.
    let different = store.converge(&[IndexModule {
        id: "chat",
        guest: Some(TESTMAP),
    }]);
    assert!(
        different.is_ok(),
        "a real fix must still converge: {different:?}"
    );
}

#[test]
fn a_rejected_mapper_leaves_every_other_module_folding() {
    let dir = tempfile::tempdir().unwrap();
    let store = IndexStore::open(
        dir.path(),
        &[
            IndexModule::bare("chat"),
            IndexModule {
                id: "tasks",
                guest: Some(TESTMAP),
            },
        ],
    )
    .unwrap();
    assert!(
        store
            .converge(&[IndexModule {
                id: "chat",
                guest: Some(NO_MEMORY_MAPPER),
            }])
            .is_err()
    );
    assert!(!store.is_poisoned());
    store
        .apply_block(&block(1, vec![chat_op(b"one"), tasks_op()]))
        .unwrap();
    store.wait_folds_drained().unwrap();
    assert_eq!(store.applied_height("chat").unwrap(), 1);
    assert_eq!(store.applied_height("tasks").unwrap(), 1);
    assert_eq!(store.view("tasks", b"count").unwrap(), 1u64.to_be_bytes());
    assert!(store.view("chat", b"count").is_err());
}

/// The two engine reads take separate snapshots. Their order and shared
/// deployment guard prevent a watermark from vouching for unseen rows.
#[test]
fn a_view_reads_its_advisory_tip_before_query_under_one_deployment_guard() {
    let body = include_str!("lib.rs")
        .split("pub fn view_with_tip(")
        .nth(1)
        .unwrap()
        .split("\n    /// ")
        .next()
        .unwrap();
    let guard = body.find("let _deployment = m.read_deployment()?").unwrap();
    let tip = body.find("let folded =").unwrap();
    let view = body.find("let bytes = m.query(req)?").unwrap();
    assert!(guard < tip && tip < view);
    assert!(!body[tip..view].contains('?'), "tip errors are advisory");
    assert!(
        !body[guard..view].contains("drop("),
        "the guard spans both reads"
    );
}

#[test]
fn an_unchanged_deployment_converges_while_a_view_holds_its_read_guard() {
    let dir = tempfile::tempdir().unwrap();
    let store = mapped_store(dir.path());
    let spec = IndexModule {
        id: "chat",
        guest: Some(TESTMAP),
    };
    let code_hash = sha2::Sha256::digest(TESTMAP);
    store.converge_deployment(&spec, &code_hash).unwrap();
    let module = store.module("chat").unwrap();
    let _view = module.read_deployment().unwrap();
    // A write lock here would wait for this very reader. The unchanged
    // deployment must complete without taking one or touching mapper bytes.
    store.converge_deployment(&spec, &code_hash).unwrap();
}
