//! host-level end-to-end + the cross-host determinism suite — the phase-1
//! capstone. every test stands a REAL `Files` module (over a tempdir) on the
//! deterministic `host::Host` runtime and drives it exactly as the ordered
//! consensus lane does: one op per `submit_at` block, the host owning the
//! `commit_block`/`abort_block` boundary. this is the only layer that proves the
//! whole module behaves identically across nodes under the real kernel host —
//! the unit/module tests drive the module in isolation; here the root-hash is
//! composed over the registry (`state::global_root`), so equality across hosts
//! is the real cross-node gate.
//!
//! each block is a single top-level `block_on(host.submit_at(..))` (or
//! `host.query(..)`): `submit_at` awaits execute AND the disk-persisting
//! `commit_block` internally, so nesting a second `block_on` inside would trip
//! futures' LocalPool re-entry guard. no helper nests.

mod harness;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use futures::executor::block_on;

use files::{
    CHUNK_SIZE, Change, Content, EntryKindWire, FilesMsg, FilesQuery, FilesReply, Kind,
    MAX_READ_BYTES, decode_reply, encode_msg, encode_putblob, encode_query, to_hex,
};
use host::{BlockContext, Host, SubmitError};
use sdk::{Error, Msg, Origin, StateRoot};

const FILES: &str = "files";

// ---- host + block/query drivers ---------------------------------------------

/// a host wrapping a single fresh `Files` module over `dir`. genesis performs no
/// module writes — it only registers — so a fresh dir starts at the empty root.
fn open_host(dir: &tempfile::TempDir) -> Host {
    open_host_with_attribution(dir, harness::SharedStore::default())
}

fn open_host_with_attribution(dir: &tempfile::TempDir, attribution: harness::SharedStore) -> Host {
    let m = files::Files::open(FILES, dir.path().to_path_buf()).expect("open files");
    Host::genesis(vec![
        Box::new(m),
        Box::new(identity_module::Identity::new(
            "identity",
            Box::new(sdk_testkit::MemStore::new()),
            "test".into(),
        )),
        Box::new(attribution_module::AttributionModule::new(
            "attribution",
            Box::new(attribution),
        )),
    ])
    .expect("genesis")
}

/// the block-constant consensus context: height doubles as the agreed logical
/// clock.
fn bctx(height: u64, origin: Origin) -> BlockContext {
    BlockContext {
        height,
        consensus_time: height,
        origin,
    }
}

/// submit one op as a block, returning the committed root-hash on success.
fn submit(
    host: &mut Host,
    height: u64,
    origin: Origin,
    msg: Msg,
) -> Result<StateRoot, SubmitError> {
    block_on(host.submit_at(bctx(height, origin), msg)).map(|o| o.root_hash)
}

fn query_bytes(host: &Host, q: &FilesQuery) -> Vec<u8> {
    block_on(host.query(FILES, &encode_query(q))).expect("query ok")
}

fn query(host: &Host, q: &FilesQuery) -> FilesReply {
    decode_reply(&query_bytes(host, q)).expect("decode reply")
}

/// the committed head snapshot hex — deterministic, so it is identical across
/// converged hosts and a threadable pin/commit base.
fn head(host: &Host) -> String {
    match query(host, &FilesQuery::Refs {}) {
        FilesReply::Refs(info) => info.head.expect("head present"),
        other => panic!("expected Refs, got {other:?}"),
    }
}

/// read a whole file back through the host query path, paging the `MAX_READ_BYTES`
/// window until EOF — the only way to reassemble a multi-chunk file over the wire.
fn read_all(host: &Host, path: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut offset = 0u64;
    loop {
        match query(
            host,
            &FilesQuery::Read {
                path: path.into(),
                snapshot: None,
                offset,
                len: MAX_READ_BYTES,
            },
        ) {
            FilesReply::Read { b64, eof } => {
                let bytes = STANDARD.decode(b64.as_bytes()).expect("b64 read page");
                out.extend_from_slice(&bytes);
                offset += bytes.len() as u64;
                // progress is guaranteed (a below-cap read reaches eof; an at/past
                // eof read is empty+eof), but guard against a 0-byte non-eof loop.
                if eof || bytes.is_empty() {
                    break;
                }
            }
            other => panic!("expected Read, got {other:?}"),
        }
    }
    out
}

// ---- op builders ------------------------------------------------------------

fn putblob_op(bytes: &[u8]) -> Msg {
    Msg {
        target: FILES.into(),
        payload: encode_putblob(bytes),
    }
}

fn commit_op(base: Option<&str>, message: &str, changes: Vec<Change>) -> Msg {
    Msg {
        target: FILES.into(),
        payload: encode_msg(&FilesMsg::Commit {
            base_snapshot: base.map(Into::into),
            message: message.into(),
            changes,
        }),
    }
}

fn pin_op(snapshot: &str, name: &str) -> Msg {
    Msg {
        target: FILES.into(),
        payload: encode_msg(&FilesMsg::Pin {
            snapshot: snapshot.into(),
            name: name.into(),
        }),
    }
}

fn watch_op(prefix: &str, module_id: &str) -> Msg {
    Msg {
        target: FILES.into(),
        payload: encode_msg(&FilesMsg::Watch {
            prefix: prefix.into(),
            module_id: module_id.into(),
        }),
    }
}

fn unwatch_op(prefix: &str, module_id: &str) -> Msg {
    Msg {
        target: FILES.into(),
        payload: encode_msg(&FilesMsg::Unwatch {
            prefix: prefix.into(),
            module_id: module_id.into(),
        }),
    }
}

fn put_inline(path: &str, bytes: &[u8]) -> Change {
    Change::Put {
        path: path.into(),
        exec: false,
        meta: Default::default(),
        content: Content::Inline {
            b64: STANDARD.encode(bytes),
        },
    }
}

fn put_chunks(path: &str, size: u64, chunk_hexes: &[String]) -> Change {
    Change::Put {
        path: path.into(),
        exec: false,
        meta: Default::default(),
        content: Content::Chunks {
            size,
            chunks: chunk_hexes.to_vec(),
        },
    }
}

/// the content id of a chunk, hex — the digest a `Chunks` change references.
fn chunk_hex(bytes: &[u8]) -> String {
    to_hex(&files::objects::object_id(Kind::Chunk, bytes))
}

// ---- test 1: the host flow --------------------------------------------------

/// the end-to-end happy path over the real host: putblob a 1.5-chunk file across
/// two blocks, commit it (chunks ref + inline + mkdir + symlink) in one block,
/// then pin the head. the root-hash MOVES on every successful block; every query
/// round-trips through `host.query`; the multi-chunk content reads back byte-exact;
/// the pin is visible via Refs and the author is the mapped external identity.
#[test]
fn host_flow() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = open_host(&dir);
    let tester = Origin::External(b"tester".to_vec());
    let owner = format!("ext:{}", to_hex(b"tester"));

    // a 1.5-chunk file: one FULL interior chunk (must be exactly CHUNK_SIZE) plus
    // a half-size tail. distinct fill bytes make the straddling readback exact.
    let chunk0 = vec![0xABu8; CHUNK_SIZE as usize];
    let tail_len = (CHUNK_SIZE / 2) as usize;
    let chunk1 = vec![0xCDu8; tail_len];
    let size = CHUNK_SIZE + tail_len as u64;
    let want: Vec<u8> = chunk0.iter().chain(chunk1.iter()).copied().collect();

    let mut prev = host.root_hash();

    // block 1: stage chunk0. staging is state (it lands in refs), so commit_block
    // adopts it and the root-hash moves even without any tree change.
    let h = submit(&mut host, 1, tester.clone(), putblob_op(&chunk0)).expect("putblob c0");
    assert_ne!(h, prev, "putblob chunk0 moves the root-hash");
    assert_eq!(
        h,
        host.root_hash(),
        "outcome hash matches the live host hash"
    );
    prev = h;

    // block 2: stage chunk1.
    let h = submit(&mut host, 2, tester.clone(), putblob_op(&chunk1)).expect("putblob c1");
    assert_ne!(h, prev, "putblob chunk1 moves the root-hash");
    prev = h;

    // block 3: one atomic commit — the staged chunks, an inline file, a dir, a link.
    let changes = vec![
        put_chunks(
            "/shared/big",
            size,
            &[chunk_hex(&chunk0), chunk_hex(&chunk1)],
        ),
        put_inline("/shared/note.txt", b"hello inline"),
        Change::Mkdir {
            path: "/shared/dir".into(),
        },
        Change::Symlink {
            path: "/shared/link".into(),
            target: "/shared/big".into(),
        },
    ];
    let h = submit(
        &mut host,
        3,
        tester.clone(),
        commit_op(None, "genesis commit", changes),
    )
    .expect("commit");
    assert_ne!(h, prev, "commit moves the root-hash");
    prev = h;

    // block 4: pin the now-committed head.
    let head_hex = head(&host);
    let h = submit(&mut host, 4, tester.clone(), pin_op(&head_hex, "release")).expect("pin");
    assert_ne!(h, prev, "pin moves the root-hash");

    // ---- queries round-trip through the host ----
    match query(
        &host,
        &FilesQuery::Stat {
            path: "/shared/big".into(),
            snapshot: None,
        },
    ) {
        FilesReply::Stat(Some(e)) => {
            assert_eq!(e.kind, EntryKindWire::File);
            assert_eq!(e.size, size, "stat reports the full 1.5-chunk size");
        }
        other => panic!("big stat: {other:?}"),
    }
    match query(
        &host,
        &FilesQuery::Stat {
            path: "/shared/dir".into(),
            snapshot: None,
        },
    ) {
        FilesReply::Stat(Some(e)) => assert_eq!(e.kind, EntryKindWire::Dir),
        other => panic!("dir stat: {other:?}"),
    }
    match query(
        &host,
        &FilesQuery::Stat {
            path: "/shared/link".into(),
            snapshot: None,
        },
    ) {
        FilesReply::Stat(Some(e)) => assert_eq!(e.kind, EntryKindWire::Symlink),
        other => panic!("link stat: {other:?}"),
    }

    // Ls lists the immediate children in sorted-name order.
    let entries = match query(
        &host,
        &FilesQuery::Ls {
            path: "/shared".into(),
            snapshot: None,
            after: None,
            limit: 256,
        },
    ) {
        FilesReply::Ls { entries, .. } => entries,
        other => panic!("ls: {other:?}"),
    };
    let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "/shared/big",
            "/shared/dir",
            "/shared/link",
            "/shared/note.txt"
        ],
        "ls returns every committed child",
    );

    // the multi-chunk content reads back byte-exact through the paged read path.
    assert_eq!(
        read_all(&host, "/shared/big"),
        want,
        "multi-chunk read is byte-exact end to end"
    );

    // the pin is visible via Refs, protecting the exact head it named.
    match query(&host, &FilesQuery::Refs {}) {
        FilesReply::Refs(info) => assert_eq!(
            info.pins.get("release").map(String::as_str),
            Some(head_hex.as_str()),
            "pin visible via Refs"
        ),
        other => panic!("refs: {other:?}"),
    }

    // the author is the mapped external identity `ext:<hex of "tester">`.
    match query(&host, &FilesQuery::History { limit: 8 }) {
        FilesReply::History(snaps) => {
            let latest = snaps.first().expect("one commit in history");
            assert_eq!(
                latest.author.to_string(),
                owner,
                "author recorded as ext:<hex>"
            );
        }
        other => panic!("history: {other:?}"),
    }
}

// ---- test 2: two hosts converge (the cross-node determinism gate) ------------

/// apply one op to BOTH hosts and assert their root-hashes stay equal after the
/// block — a rejection must land IDENTICALLY on both, and never move either hash.
fn step(a: &mut Host, b: &mut Host, height: u64, origin: Origin, msg: Msg, expect_reject: bool) {
    let ra = block_on(a.submit_at(bctx(height, origin.clone()), msg.clone()));
    let rb = block_on(b.submit_at(bctx(height, origin), msg));
    match (&ra, &rb) {
        (Ok(oa), Ok(ob)) => {
            assert!(
                !expect_reject,
                "block {height}: expected a rejection, both accepted"
            );
            assert_eq!(
                oa.root_hash, ob.root_hash,
                "block {height}: outcome root-hash equal"
            );
        }
        (Err(ea), Err(eb)) => {
            assert!(
                expect_reject,
                "block {height}: unexpected rejection: {ea:?}"
            );
            // the deterministic-rejection contract: byte-for-byte the same error.
            assert_eq!(ea, eb, "block {height}: hosts reject identically");
        }
        _ => panic!("block {height}: host divergence a={ra:?} b={rb:?}"),
    }
    assert_eq!(
        a.root_hash(),
        b.root_hash(),
        "block {height}: committed root-hash equal after the block"
    );
}

/// two hosts over different tempdirs fed the IDENTICAL op stream — interleaved
/// putblobs by two owners, commits, a mid-sequence CAS-conflict rejection, a
/// module-origin watch + unwatch, a pin, and a run of commits that exercise the
/// bounded history window push — must agree on the root-hash after EVERY block and
/// serve byte-identical replies at the end. this is the whole crate's cross-node
/// determinism gate.
#[test]
fn two_hosts_converge() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let mut a = open_host(&dir_a);
    let mut b = open_host(&dir_b);
    assert_eq!(a.root_hash(), b.root_hash(), "fresh hosts start equal");

    let owner_a = Origin::External(b"owner-a".to_vec());
    let owner_b = Origin::External(b"owner-b".to_vec());
    let chat = Origin::Module("chat".into());

    let c1 = vec![0x11u8; 200];
    let c2 = vec![0x22u8; 300];

    // interleaved putblobs by two independent owners.
    step(&mut a, &mut b, 1, owner_a.clone(), putblob_op(&c1), false);
    step(&mut a, &mut b, 2, owner_b.clone(), putblob_op(&c2), false);

    // a commit referencing the first staged chunk, an inline file, and a dir.
    step(
        &mut a,
        &mut b,
        3,
        owner_a.clone(),
        commit_op(
            None,
            "c1",
            vec![
                put_chunks("/shared/f1", 200, &[chunk_hex(&c1)]),
                put_inline("/shared/a.txt", b"alpha"),
                Change::Mkdir {
                    path: "/shared/dir".into(),
                },
            ],
        ),
        false,
    );
    // a commit referencing the second staged chunk.
    step(
        &mut a,
        &mut b,
        4,
        owner_b.clone(),
        commit_op(
            None,
            "c2",
            vec![put_chunks("/shared/f2", 300, &[chunk_hex(&c2)])],
        ),
        false,
    );

    // a CAS-conflict rejection mid-sequence: re-creating /shared/a.txt on the empty
    // base while it already exists at head is a per-path CAS conflict — both hosts
    // reject identically and neither hash moves.
    step(
        &mut a,
        &mut b,
        5,
        owner_a.clone(),
        commit_op(None, "conflict", vec![put_inline("/shared/a.txt", b"beta")]),
        true,
    );

    // a module-origin watch registration and its removal. "chat" is not a
    // registered module here, so the watched prefix is deliberately one no later
    // commit touches — a fired notification would re-dispatch to an unknown module
    // and fail the block. registration + removal alone exercise the ops' state
    // determinism; the notification fan-out is proven in the module tests.
    step(
        &mut a,
        &mut b,
        6,
        chat.clone(),
        watch_op("/watched", "chat"),
        false,
    );
    step(
        &mut a,
        &mut b,
        7,
        chat.clone(),
        unwatch_op("/watched", "chat"),
        false,
    );

    // a pin over the (converged, deterministic) head.
    let head_hex = head(&a);
    assert_eq!(head_hex, head(&b), "hosts agree on the head hex");
    step(
        &mut a,
        &mut b,
        8,
        owner_a.clone(),
        pin_op(&head_hex, "rel"),
        false,
    );

    // a run of commits to exercise repeated history-window pushes.
    for i in 0..6u64 {
        let path = format!("/shared/w{i}");
        step(
            &mut a,
            &mut b,
            9 + i,
            owner_b.clone(),
            commit_op(None, "w", vec![put_inline(&path, b"x")]),
            false,
        );
    }

    assert_eq!(a.root_hash(), b.root_hash(), "final root-hash equal");

    // every read reply is byte-identical across the two hosts.
    let queries = [
        FilesQuery::Stat {
            path: "/shared/f1".into(),
            snapshot: None,
        },
        FilesQuery::Ls {
            path: "/shared".into(),
            snapshot: None,
            after: None,
            limit: 256,
        },
        FilesQuery::Read {
            path: "/shared/f1".into(),
            snapshot: None,
            offset: 0,
            len: MAX_READ_BYTES,
        },
        FilesQuery::History { limit: 64 },
        FilesQuery::Refs {},
    ];
    for q in &queries {
        assert_eq!(
            query_bytes(&a, q),
            query_bytes(&b, q),
            "reply byte-identical across hosts: {q:?}"
        );
    }
}

// ---- test 3: rejects never move the root-hash --------------------------------

/// submit one op that MUST be rejected, and assert (1) the host surfaces a
/// deterministic module rejection — never a node-local fatal boundary fault — and
/// (2) the root-hash is byte-identical before and after. the kernel contract is
/// that an execute error aborts the WHOLE block (host/src/lib.rs), so no staged
/// write survives.
fn assert_rejected(host: &mut Host, height: u64, origin: Origin, msg: Msg, label: &str) {
    let before = host.root_hash();
    let err = block_on(host.submit_at(bctx(height, origin), msg)).expect_err(label);
    // a rejection is the deterministic no-op every honest node computes; a Fatal
    // would mean this node's state went indeterminate, which no rejection may.
    assert!(
        err.rejected().is_some(),
        "{label}: must be a deterministic rejection, got {err:?}"
    );
    assert!(
        matches!(err.rejected(), Some(Error::Module(_))),
        "{label}: module-level rejection, got {err:?}"
    );
    assert_eq!(
        host.root_hash(),
        before,
        "{label}: root-hash byte-identical after the rejected block"
    );
}

/// every rejection class from the verb surface, submitted through the host: each
/// proves the module error surfaces AND the root-hash is untouched.
#[test]
fn rejects_never_move_root_hash() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = open_host(&dir);

    // setup: a two-commit history (S1 then S2 over /shared/x) and an existing pin,
    // so the duplicate-pin and CAS-conflict classes have real state to collide with.
    submit(
        &mut host,
        1,
        Origin::System,
        commit_op(None, "v0", vec![put_inline("/shared/x", b"v0")]),
    )
    .expect("setup S1");
    let s1 = head(&host);
    submit(
        &mut host,
        2,
        Origin::System,
        commit_op(Some(&s1), "va", vec![put_inline("/shared/x", b"va")]),
    )
    .expect("advance to S2");
    let s2 = head(&host);
    submit(&mut host, 3, Origin::System, pin_op(&s2, "keep")).expect("seed pin");

    // 1) oversized putblob — a chunk one byte over CHUNK_SIZE.
    assert_rejected(
        &mut host,
        4,
        Origin::System,
        putblob_op(&vec![0u8; CHUNK_SIZE as usize + 1]),
        "oversized putblob",
    );

    // 2) bad-authority commit — ext:bob writing outside /home and /shared.
    assert_rejected(
        &mut host,
        5,
        Origin::External(b"bob".to_vec()),
        commit_op(None, "x", vec![put_inline("/etc/passwd", b"x")]),
        "bad-authority commit",
    );

    // 3) duplicate pin name — "keep" is already taken by the seed pin.
    assert_rejected(
        &mut host,
        6,
        Origin::System,
        pin_op(&s2, "keep"),
        "duplicate pin name",
    );

    // 4) CAS conflict — committing /shared/x on the now-stale base S1.
    assert_rejected(
        &mut host,
        7,
        Origin::System,
        commit_op(Some(&s1), "vb", vec![put_inline("/shared/x", b"vb")]),
        "cas conflict",
    );

    // 5) unresolvable base — a never-committed snapshot id.
    let random = "bb".repeat(32);
    assert_rejected(
        &mut host,
        8,
        Origin::System,
        commit_op(Some(&random), "x", vec![put_inline("/shared/y", b"y")]),
        "unresolvable base",
    );

    // 6) non-module watch — an external submitter may not register a watch.
    assert_rejected(
        &mut host,
        9,
        Origin::External(b"someone".to_vec()),
        watch_op("/shared", "someone"),
        "non-module watch",
    );
}

// ---- test 4: a restarted host replays and converges -------------------------

/// determinism hardening: a host that is DROPPED mid-sequence, REOPENED over its
/// data dir, and replays the remaining blocks must land on the same root-hash as a
/// host that ran the whole sequence straight through.
///
/// this is the host-layer equivalent of a node crash-and-recover across a block.
/// it works at the host layer because `Files::open` re-adopts the committed refs
/// (head, staging table, pins, window) and full odb from disk, and `Host::genesis`
/// only REGISTERS the restored module — it runs no genesis writes — so the wrapped
/// module's root is byte-identical to its value at the crash boundary. phase 2's
/// node-integration e2e will drive this through the real journal/statesync join
/// path; here the module-reopen-under-a-fresh-host construction is the phase-1
/// equivalent, and the intermediate assertion pins the boundary equality directly.
#[test]
fn restart_mid_sequence_converges() {
    // two staged chunks, referenced by later commits — so the reopen must restore
    // the staging table AND the odb, not just the head.
    let c1 = b"chunk-one-bytes".to_vec();
    let c2 = b"chunk-two-bytes".to_vec();
    let ops: Vec<(u64, Origin, Msg)> = vec![
        (1, Origin::External(b"a".to_vec()), putblob_op(&c1)),
        (2, Origin::External(b"b".to_vec()), putblob_op(&c2)),
        (
            3,
            Origin::External(b"a".to_vec()),
            commit_op(
                None,
                "b3",
                vec![
                    put_chunks("/shared/f1", c1.len() as u64, &[chunk_hex(&c1)]),
                    Change::Mkdir {
                        path: "/shared/dir".into(),
                    },
                ],
            ),
        ),
        (
            4,
            Origin::External(b"b".to_vec()),
            commit_op(
                None,
                "b4",
                vec![
                    put_chunks("/shared/f2", c2.len() as u64, &[chunk_hex(&c2)]),
                    Change::Symlink {
                        path: "/shared/link".into(),
                        target: "/shared/f1".into(),
                    },
                ],
            ),
        ),
        (
            5,
            Origin::System,
            commit_op(None, "b5", vec![put_inline("/shared/g", b"hi")]),
        ),
        (
            6,
            Origin::System,
            commit_op(None, "b6", vec![put_inline("/genesis/seed", b"s")]),
        ),
    ];

    // host A runs all six blocks straight through; capture its per-block hashes.
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = open_host(&dir_a);
    let mut a_hashes = Vec::new();
    for (h, o, m) in &ops {
        a_hashes.push(submit(&mut a, *h, o.clone(), m.clone()).expect("host A block"));
    }
    let a_after_3 = a_hashes[2];
    let final_a = *a_hashes.last().unwrap();
    drop(a);

    // host B runs blocks 1-3, is dropped (releasing disk handles), reopened, and
    // replays 4-6.
    let dir_b = tempfile::tempdir().unwrap();
    let attribution_b = harness::SharedStore::default();
    {
        let mut b = open_host_with_attribution(&dir_b, attribution_b.clone());
        for (h, o, m) in ops.iter().take(3) {
            submit(&mut b, *h, o.clone(), m.clone()).expect("host B pre-restart block");
        }
    } // b drops here.

    let mut b2 = open_host_with_attribution(&dir_b, attribution_b);
    // the reopened host re-adopts block 3's durable committed state (including the
    // still-staged second chunk) — the same module root host A held at block 3.
    assert_eq!(
        b2.root_hash(),
        a_after_3,
        "reopen re-adopts the block-3 committed state exactly"
    );
    for (h, o, m) in ops.iter().skip(3) {
        submit(&mut b2, *h, o.clone(), m.clone()).expect("host B post-restart block");
    }

    assert_eq!(
        b2.root_hash(),
        final_a,
        "a restarted host replaying 4-6 converges to the straight-through host"
    );
}
