//! the network state-sync property at the crate level: a joiner rebuilds a
//! REAL qmdb-backed module purely through the wire protocol — manifest fetch,
//! manifest-pinned target adoption, proof-carrying op-range fetches through the remote
//! resolver — and lands on the source's exact root, with byzantine responses
//! rejected by verification rather than trust.
//!
//! the transport here is an in-process request/response channel pair: the
//! same bytes, framing, and client code a p2p channel carries, minus sockets.
//! (the real-mesh run lives in the node binary's demo script; the DETERMINISTIC
//! proof of the protocol lives here.)

use futures::channel::{mpsc, oneshot};
use futures::{SinkExt as _, StreamExt as _};
use host::{FinalizedBlock, Host};
use kv::Kv;
use kv::{KvMsg, encode as kv_encode};
use sdk::{Ctx, Error, Module, ModuleId, Msg, StateRoot, StateSyncHandle};
use statesync::qmdb::{QmdbStore, RemoteQmdbSource};
use statesync::{
    CHUNK_LEN, ManifestEntry, PayloadKind, SyncClient, SyncError, SyncRequest, SyncResponse,
    SyncServer, decode_response, encode_request, fetch_manifest, fetch_snapshot,
};

use commonware_runtime::{Runner as _, Supervisor as _, deterministic};

// ============================================================================
// the in-process transport: Send client futures, local server loop.
// ============================================================================

type RpcPair = (Vec<u8>, oneshot::Sender<Vec<u8>>);

#[derive(Clone)]
struct ChannelClient {
    tx: mpsc::Sender<RpcPair>,
}

fn pinned_target(entry: &ManifestEntry) -> statesync::qmdb::SyncTarget {
    entry
        .resolver_target
        .as_ref()
        .expect("resolver entry carries pinned target")
        .to_sync_target()
        .expect("pinned target range is non-empty")
}

impl SyncClient for ChannelClient {
    fn request(
        &self,
        req: SyncRequest,
    ) -> impl std::future::Future<Output = Result<SyncResponse, SyncError>> + Send {
        let mut tx = self.tx.clone();
        async move {
            let (reply_tx, reply_rx) = oneshot::channel();
            tx.send((encode_request(&req), reply_tx))
                .await
                .map_err(|e| SyncError::Transport(format!("request channel closed: {e}")))?;
            let bytes = reply_rx
                .await
                .map_err(|_| SyncError::Transport("server dropped the reply".into()))?;
            Ok(decode_response(&bytes)?)
        }
    }
}

/// run the server side over `host` until every client sender is dropped.
async fn serve_until_closed(
    server: &mut SyncServer,
    host: &Host,
    finalized: FinalizedBlock,
    mut rx: mpsc::Receiver<RpcPair>,
) {
    // fixed coordinates: these tests exercise the module payload lanes; the
    // epoch fields just have to round-trip through the manifest.
    let coords = statesync::BoundaryCoords {
        epoch: 0,
        view_base: 0,
        participants: vec![],
        floor_cert: None,
        ..Default::default()
    };
    while let Some((frame, reply)) = rx.next().await {
        let resp = server
            .handle_frame(host, Some(finalized), &coords, &frame)
            .await;
        // a dropped reply receiver just means the client gave up — server-side
        // that is not an error.
        let _ = reply.send(resp);
    }
}

// ============================================================================
// a snapshot-lane module with a payload bigger than one chunk.
// ============================================================================

struct BigSnapshot {
    bytes: Vec<u8>,
}

impl BigSnapshot {
    fn new() -> Self {
        // deterministic, incompressible-ish pattern spanning ~2.7 chunks so the
        // chunk loop takes multiple round trips and an exact tail slice.
        let len = CHUNK_LEN * 2 + CHUNK_LEN / 2 + 7;
        let bytes = (0..len).map(|i| (i * 31 + 7) as u8).collect();
        Self { bytes }
    }
}

#[async_trait::async_trait(?Send)]
impl Module for BigSnapshot {
    fn id(&self) -> ModuleId {
        "bigsnap".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot([0xBB; 32])
    }
    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        Ok(StateSyncHandle::SnapshotBytes(self.bytes.clone()))
    }
    async fn execute(&mut self, _c: &mut dyn Ctx, _m: &Msg) -> Result<(), Error> {
        Ok(())
    }
}

// ============================================================================
// the tests
// ============================================================================

#[test]
fn joiner_rebuilds_kv_over_the_wire_protocol() {
    deterministic::Runner::default().start(|context| async move {
        // ---- SOURCE: real committed kv content through the host op path ----
        // built the way a host does: concrete store first, injected as a box.
        let kv = Kv::new(
            "kv",
            Box::new(QmdbStore::init(context.child("source_kv"), "kv").await),
        );
        let mut host =
            Host::genesis(vec![Box::new(kv), Box::new(BigSnapshot::new())]).expect("genesis");

        let set = |k: &[u8], v: &[u8]| Msg {
            target: "kv".into(),
            payload: kv_encode(&KvMsg::Set {
                key: k.to_vec(),
                value: v.to_vec(),
            }),
        };
        host.submit(set(b"greeting", b"hello world"))
            .await
            .expect("op 1");
        host.submit(set(b"motd", b"draft")).await.expect("op 2");
        // overwrite: op-log order matters — only real sync reproduces the root.
        host.submit(set(b"motd", b"final")).await.expect("op 3");

        let finalized = FinalizedBlock {
            height: 3,
            root_hash: host.root_hash(),
        };
        let src_kv_root = host.module_root("kv").expect("kv registered");

        // ---- the wire: server loop + channel client -------------------------
        let (tx, rx) = mpsc::channel::<RpcPair>(16);
        let client = ChannelClient { tx };
        let mut server = SyncServer::new();

        let joiner_ctx = context.child("joiner_kv");
        let client_for_join = client.clone();
        let join_side = async move {
            // manifest: the captured boundary describes every module.
            let manifest = fetch_manifest(&client_for_join).await.expect("manifest");
            assert_eq!(manifest.height, 3);
            assert_eq!(manifest.root_hash, finalized.root_hash);
            let kv_entry = manifest.entry("kv").expect("kv in manifest");
            assert_eq!(kv_entry.kind, PayloadKind::Resolver);
            assert_eq!(kv_entry.root, src_kv_root);

            // pinned target from the manifest, gated on the manifest root.
            let resolver =
                RemoteQmdbSource::new(client_for_join.clone(), manifest.boundary_id(), "kv");
            let target = pinned_target(kv_entry);
            assert_eq!(
                StateRoot(target.root.0),
                kv_entry.root,
                "pinned target root equals the manifest root"
            );

            // rebuild ENTIRELY through the wire: every op batch crosses the
            // channel as proof-carrying bytes and is merkle-verified. the
            // synced store then backs a fresh module, the joiner-host shape.
            let store = QmdbStore::sync_from(joiner_ctx, "kv-rebuilt", target, resolver)
                .await
                .expect("sync_from");
            let rebuilt = Kv::new("kv-rebuilt", Box::new(store));
            assert_eq!(rebuilt.root(), src_kv_root, "synced root == source root");
            assert_eq!(
                rebuilt.get(b"motd").await.as_deref(),
                Some(b"final".as_ref()),
                "overwrite history replayed in op-log order"
            );
            assert_eq!(
                rebuilt.get(b"greeting").await.as_deref(),
                Some(b"hello world".as_ref())
            );

            // multi-chunk snapshot lane: bytes reassemble exactly.
            let snap = fetch_snapshot(&client_for_join, manifest.boundary_id(), "bigsnap")
                .await
                .expect("big snapshot");
            assert_eq!(
                snap,
                BigSnapshot::new().bytes,
                "chunked payload reassembles"
            );
            drop(client_for_join); // close the channel so the server loop ends.
        };

        let server_side = serve_until_closed(&mut server, &host, finalized, rx);
        // the extra `client` clone must drop too or the server loop never ends.
        drop(client);
        futures::join!(join_side, server_side);
    });
}

#[test]
fn stale_capture_requests_are_refused_not_mis_served() {
    deterministic::Runner::default().start(|context| async move {
        let kv = Kv::new(
            "kv",
            Box::new(QmdbStore::init(context.child("kv"), "kv").await),
        );
        let mut host = Host::genesis(vec![Box::new(kv)]).expect("genesis");
        host.submit(Msg {
            target: "kv".into(),
            payload: kv_encode(&KvMsg::Set {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            }),
        })
        .await
        .expect("op");
        let finalized = FinalizedBlock {
            height: 1,
            root_hash: host.root_hash(),
        };

        let mut server = SyncServer::new();
        // a chunk request against a height never captured must error cleanly.
        let resp = server
            .handle(
                &host,
                Some(finalized),
                &statesync::BoundaryCoords::default(),
                SyncRequest::Chunk {
                    boundary: statesync::BoundaryId {
                        height: 999,
                        root_hash: StateRoot([9u8; 32]),
                    },
                    module_id: "kv".into(),
                    offset: 0,
                },
            )
            .await;
        assert!(
            matches!(resp, SyncResponse::Error(ref e) if e.contains("not leased")),
            "unknown boundary must be a clean protocol error, got {resp:?}"
        );

        // manifest against a WRONG finalized boundary (host has advanced past
        // it) is refused by the host's root-hash gate, not served stale.
        let wrong = FinalizedBlock {
            height: 0,
            root_hash: StateRoot([1u8; 32]),
        };
        let resp = server
            .handle(
                &host,
                Some(wrong),
                &statesync::BoundaryCoords::default(),
                SyncRequest::Manifest,
            )
            .await;
        assert!(
            matches!(resp, SyncResponse::Error(ref e) if e.contains("capture failed")),
            "a mismatched boundary must refuse, got {resp:?}"
        );
    });
}

#[test]
fn byzantine_op_batches_fail_verification_not_installation() {
    // a lying server can transport-tamper exactly one thing: response bytes.
    // decode-level tampering (truncation, trailing bytes, forged counts) must
    // reject at the wire layer; this pins the decode side. (proof-level lies —
    // valid encoding, wrong ops — are rejected by the sync engine's merkle
    // verification against the target root, proven by commonware's own suite.)
    use statesync::qmdb::{decode_qmdb_resp, encode_qmdb_req, ops_request};

    deterministic::Runner::default().start(|context| async move {
        let kv = Kv::new(
            "kv",
            Box::new(QmdbStore::init(context.child("kv"), "kv").await),
        );
        let mut host = Host::genesis(vec![Box::new(kv)]).expect("genesis");
        host.submit(Msg {
            target: "kv".into(),
            payload: kv_encode(&KvMsg::Set {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            }),
        })
        .await
        .expect("op");

        // real op coordinates come from the store's own target — the log
        // carries commit-floor ops beyond the user writes, and serving below
        // the pruned floor is refused.
        let target = host
            .resolver_sync_target("kv")
            .await
            .expect("resolver target");

        // an honest ops envelope straight off the serve lane...
        let body = host
            .serve_sync(
                "kv",
                &encode_qmdb_req(&ops_request(target.op_count, target.start, 16)),
            )
            .await
            .expect("serve ops");
        assert!(decode_qmdb_resp(&body).is_ok(), "honest reply decodes");

        // ...rejects when truncated,
        assert!(decode_qmdb_resp(&body[..body.len() - 1]).is_err());
        // ...when carrying trailing garbage,
        let mut trailing = body.clone();
        trailing.push(0);
        assert!(decode_qmdb_resp(&trailing).is_err());
        // ...and when its variant tag names a reply shape that does not exist.
        let mut forged = body.clone();
        forged[0] = 0xFF;
        assert!(decode_qmdb_resp(&forged).is_err());
    });
}

// ============================================================================
// the index backfill lane: real fluent31 databases round-trip the wire.
// ============================================================================

/// serve `IndexOps` off a real source store, everything else off the host.
/// mirrors bin/node's split exactly: the serve step names the state touch, the
/// state owner (here, the store) answers it.
fn serve_index_ops(
    source: &indexer::IndexStore,
    module: &str,
    after: Option<(u64, u32)>,
    boundary: u64,
    page_len: usize,
    corrupt: bool,
) -> SyncResponse {
    let cursor = after.map(|(h, s)| indexer::op_key(h, s));
    let page = source
        .scan(
            module,
            indexer::OP_PREFIX.as_bytes(),
            cursor.as_deref().map(str::as_bytes),
            page_len,
        )
        .expect("scan");
    let mut rows: Vec<(String, Vec<u8>)> = page
        .entries
        .iter()
        .map(|(k, v)| (String::from_utf8(k.clone()).unwrap(), v.clone()))
        .filter(|(k, _)| indexer::parse_op_key(k.as_bytes()).is_some_and(|(h, _)| h <= boundary))
        .collect();
    let has_more = rows.len() == page.entries.len() && page.has_more;
    if corrupt && let Some((_, value)) = rows.last_mut() {
        value.truncate(1); // a row that no longer borsh-decodes
    }
    SyncResponse::IndexOps {
        next_after: has_more
            .then(|| {
                rows.last()
                    .and_then(|(k, _)| indexer::parse_op_key(k.as_bytes()))
            })
            .flatten(),
        rows,
        source_floor: source.vouched_floor(module).expect("floor"),
        applied_height: source.applied_height(module).expect("watermark"),
    }
}

/// a client that answers `IndexOps` straight off a source store and refuses
/// everything else — the whole wire under test here is the one lane.
#[derive(Clone)]
struct IndexOpsClient {
    source: std::sync::Arc<indexer::IndexStore>,
    page_len: usize,
    /// corrupt the last row of every page: the byzantine-source arm.
    corrupt: bool,
}

impl SyncClient for IndexOpsClient {
    fn request(
        &self,
        req: SyncRequest,
    ) -> impl std::future::Future<Output = Result<SyncResponse, SyncError>> + Send {
        // the REAL codec on both sides: the page crosses as bytes, exactly as
        // a mesh frame would carry it.
        let source = self.source.clone();
        let (page_len, corrupt) = (self.page_len, self.corrupt);
        async move {
            let framed = statesync::encode_request(&req);
            let decoded = statesync::decode_request(&framed)?;
            let SyncRequest::IndexOps {
                boundary,
                module,
                after,
            } = decoded
            else {
                return Ok(SyncResponse::Error(
                    "only the IndexOps lane is served".into(),
                ));
            };
            let resp = serve_index_ops(&source, &module, after, boundary, page_len, corrupt);
            Ok(decode_response(&statesync::encode_response(&resp))?)
        }
    }
}

/// the reference mapper (`crates/kernel/indexer/tests/fixtures`). the wire
/// lane has to be proven against a REAL FOLD, not
/// just raw rows: paging, key order and folding are one mechanism, and a test
/// that only diffs `op/` bytes would pass with the trigger never firing —
/// while the user-visible symptom this whole lane exists to fix is an empty
/// DERIVED view.
const TESTMAP: &[u8] = include_bytes!("../../indexer/tests/fixtures/testmap.index.wasm");

fn store(dir: &std::path::Path) -> indexer::IndexStore {
    indexer::IndexStore::open(
        dir,
        &[
            indexer::IndexModule {
                id: "chat",
                guest: Some(TESTMAP),
            },
            indexer::IndexModule::bare("tasks"),
        ],
    )
    .expect("open store")
}

fn feed(store: &indexer::IndexStore, heights: std::ops::RangeInclusive<u64>) {
    for h in heights {
        store
            .apply_block(&indexer::BlockOps {
                height: h,
                time: 1_000 + h,
                ops: vec![indexer::AppliedOp {
                    module: "chat".into(),
                    origin: indexer::OriginTag::external("jess"),
                    payload: format!(r#"{{"post":"hi {h}"}}"#).into_bytes(),
                    assigned: Vec::new(),
                }],
                record: Some(format!(r#"{{"height":{h}}}"#).into_bytes()),
            })
            .expect("fold");
    }
}

/// walk the wire into a joiner store, exactly as the node's join seam does —
/// `after` resumes above a feed the joiner already holds.
fn backfill_after(
    client: &IndexOpsClient,
    joiner: &indexer::IndexStore,
    boundary: u64,
    after: Option<(u64, u32)>,
) -> Result<Option<u64>, SyncError> {
    futures::executor::block_on(statesync::fetch_index_ops(
        client,
        "chat",
        boundary,
        after,
        |page| {
            joiner
                .write_backfill_rows("chat", page)
                .map_err(|e| e.to_string())
        },
    ))
}

/// the walk from the source's own beginning — the fresh joiner's seam.
fn backfill(
    client: &IndexOpsClient,
    joiner: &indexer::IndexStore,
    boundary: u64,
) -> Result<Option<u64>, SyncError> {
    backfill_after(client, joiner, boundary, None)
}

/// THE CRATE-LEVEL PROOF OF SPEC §7: a joiner owing a boundary pulls the
/// source's op rows below it OVER THE WIRE, page by page, and ends up with the
/// source's rows — feed and watermark included — instead of an empty view.
#[test]
fn a_joiner_owing_a_boundary_backfills_the_sources_op_rows_over_the_wire() {
    let src_dir = tempfile::tempdir().expect("src dir");
    let dst_dir = tempfile::tempdir().expect("dst dir");
    let source = std::sync::Arc::new(store(src_dir.path()));
    feed(&source, 1..=9);
    let joiner = store(dst_dir.path());
    joiner.owe("chat", 1, 9).expect("owe");
    assert_eq!(
        joiner
            .scan("chat", indexer::OP_PREFIX.as_bytes(), None, 100)
            .unwrap()
            .entries
            .len(),
        0,
        "the stamp is what leaves a joiner's views empty"
    );

    // page size 2: the cursor walk takes five round trips, so an off-by-one
    // in either the cursor or the ordering check shows up as a gap.
    let client = IndexOpsClient {
        source: source.clone(),
        page_len: 2,
        corrupt: false,
    };
    let floor = backfill(&client, &joiner, 9).expect("backfill");
    assert_eq!(floor, None, "a source that reaches genesis has no floor");
    // the repair's closing move, in its order: drain the fold the writes
    // triggered, THEN settle the debt over rows that are actually derived.
    joiner.wait_folds_drained().expect("joiner folds drain");
    source.wait_folds_drained().expect("source folds drain");
    joiner.settle_owed("chat", 1, 9).expect("settle");
    joiner.advance_watermark("chat", 9).expect("watermark");

    let rows = |s: &indexer::IndexStore| {
        s.scan("chat", indexer::OP_PREFIX.as_bytes(), None, 1024)
            .expect("scan")
            .entries
    };
    assert_eq!(rows(&source), rows(&joiner), "op rows byte-identical");
    assert_eq!(joiner.applied_height("chat").unwrap(), 9);
    assert_eq!(joiner.vouched_floor("chat").unwrap(), None);

    // THE POINT OF THE LANE: the rows that crossed the wire were FOLDED, so
    // the joiner's derived view answers for pre-boundary history exactly as
    // the source's does. `count` and the per-op `seen/` rows are the testmap's
    // derived key space; the tip is the fold's own progress mark, and it only
    // reaches (9, 0) if every page landed in ascending order.
    let derived = |s: &indexer::IndexStore| {
        s.scan("chat", b"seen/", None, 1024)
            .expect("scan")
            .entries
            .len()
    };
    assert_eq!(
        derived(&joiner),
        9,
        "every backfilled op derived a view row"
    );
    assert_eq!(derived(&source), derived(&joiner), "same derived rows");
    assert_eq!(
        joiner.view("chat", b"count").unwrap(),
        source.view("chat", b"count").unwrap(),
        "the derived view matches the source's"
    );
    assert_eq!(
        joiner.fold_tip("chat").unwrap(),
        source.fold_tip("chat").unwrap()
    );
    assert_eq!(joiner.fold_tip("chat").unwrap(), Some((9, 0)));
}

/// A SOURCE'S OWN DEBT TRAVELS AS ITS FLOOR. The source joined late too, so
/// it has no rows at or below what IT owes — the joiner settles only what
/// the source vouched for and keeps owing the rest, since claiming genesis
/// would be a lie the joiner told about content nobody has.
#[test]
fn the_sources_floor_bounds_what_the_joiner_may_settle() {
    let src_dir = tempfile::tempdir().expect("src dir");
    let dst_dir = tempfile::tempdir().expect("dst dir");
    let source = std::sync::Arc::new(store(src_dir.path()));
    // the source itself joined at 4, then folded 5..=9.
    source.owe("chat", 1, 4).expect("source owes");
    feed(&source, 5..=9);
    let joiner = store(dst_dir.path());
    joiner.owe("chat", 1, 9).expect("owe");

    let client = IndexOpsClient {
        source: source.clone(),
        page_len: 3,
        corrupt: false,
    };
    let floor = backfill(&client, &joiner, 9).expect("backfill");
    assert_eq!(floor, Some(4), "the source's truncation travels");
    joiner.settle_owed("chat", 5, 9).expect("settle what the source vouched for");
    assert_eq!(joiner.owed("chat").unwrap(), vec![(1, 4)]);
    assert_eq!(joiner.vouched_floor("chat").unwrap(), Some(4));
    assert_eq!(
        joiner
            .scan("chat", indexer::OP_PREFIX.as_bytes(), None, 100)
            .unwrap()
            .entries
            .len(),
        5,
        "exactly the rows the source actually held"
    );
}

/// A PAGE THAT FAILS STRUCTURAL VALIDATION ABORTS THE MODULE, AND THE DEBT
/// STANDS. These rows are unverifiable by design, so the one thing the trust
/// boundary can still refuse is garbage — and refusing it has to leave the
/// joiner exactly as honest as it was before it asked.
#[test]
fn a_corrupt_page_aborts_the_backfill_and_keeps_the_debt() {
    let src_dir = tempfile::tempdir().expect("src dir");
    let dst_dir = tempfile::tempdir().expect("dst dir");
    let source = std::sync::Arc::new(store(src_dir.path()));
    feed(&source, 1..=9);
    let joiner = store(dst_dir.path());
    joiner.owe("chat", 1, 9).expect("owe");

    let client = IndexOpsClient {
        source,
        page_len: 4,
        corrupt: true,
    };
    let err = backfill(&client, &joiner, 9).expect_err("a corrupt row must refuse");
    assert!(
        matches!(&err, SyncError::Module { reason, .. } if reason.contains("borsh")),
        "want a structural refusal, got {err}"
    );
    // the settle is never reached, so the debt stands — the joiner still
    // says, honestly, that everything below 9 is absent.
    assert_eq!(joiner.vouched_floor("chat").unwrap(), Some(9));
}

/// A SOURCE THAT FOLDED LESS THAN IT IS ASKED FOR IS REFUSED. Writing its rows
/// anyway would leave the joiner with a HOLE between the source's watermark
/// and its own boundary — rows below a floor that claims they are all there.
#[test]
fn a_source_behind_the_boundary_is_refused_before_a_hole_can_form() {
    let src_dir = tempfile::tempdir().expect("src dir");
    let dst_dir = tempfile::tempdir().expect("dst dir");
    let source = std::sync::Arc::new(store(src_dir.path()));
    feed(&source, 1..=4);
    let joiner = store(dst_dir.path());
    joiner.owe("chat", 1, 9).expect("owe");

    let client = IndexOpsClient {
        source,
        page_len: 8,
        corrupt: false,
    };
    let err = backfill(&client, &joiner, 9).expect_err("a lagging source must refuse");
    assert!(
        matches!(&err, SyncError::Module { reason, .. } if reason.contains("hole")),
        "want the hole refusal, got {err}"
    );
    assert_eq!(joiner.vouched_floor("chat").unwrap(), Some(9));
}

/// A SOURCE WHOSE FLOOR ROSE ABOVE THE BOUNDARY IS REFUSED, NOT COMPOSED. It
/// state-synced forward past the range it is being asked for, so it vouches
/// for none of it — and inheriting a floor of 20 under a watermark of 9 would
/// leave the joiner claiming more missing than it has. The debt stands.
#[test]
fn a_source_owing_past_the_boundary_is_refused_not_composed() {
    let src_dir = tempfile::tempdir().expect("src dir");
    let dst_dir = tempfile::tempdir().expect("dst dir");
    let source = std::sync::Arc::new(store(src_dir.path()));
    feed(&source, 1..=9);
    // the source jumped to a boundary ABOVE the one the joiner is asking about
    // and owes everything up to it: it vouches for none of what the joiner wants.
    source.owe("chat", 10, 20).expect("source owes");
    feed(&source, 21..=22);
    let joiner = store(dst_dir.path());
    joiner.owe("chat", 1, 9).expect("owe");

    let client = IndexOpsClient {
        source,
        page_len: 8,
        corrupt: false,
    };
    let err = backfill(&client, &joiner, 9).expect_err("a re-stamped source must refuse");
    assert!(
        matches!(&err, SyncError::Module { reason, .. } if reason.contains("rose above")),
        "want the risen-floor refusal, got {err}"
    );
    assert_eq!(joiner.vouched_floor("chat").unwrap(), Some(9));
}

/// A RESUMED WALK IS ANCHORED AT ITS CURSOR. `after` is not a hint the source
/// may ignore: the caller already holds every row at or below it (that is what
/// its own watermark vouches for), and re-writing those rows would re-fold ops
/// its views already carry. So the walk asks strictly above the cursor and
/// anchors its ascent check there — a source replaying its history from
/// genesis under a resume is REFUSED, not composed.
#[test]
fn a_resumed_walk_starts_at_its_cursor_and_refuses_a_replay_below_it() {
    let src_dir = tempfile::tempdir().expect("src dir");
    let source = std::sync::Arc::new(store(src_dir.path()));
    feed(&source, 1..=9);
    let client = IndexOpsClient {
        source,
        page_len: 2,
        corrupt: false,
    };

    // the honest resume: a caller whose feed reaches height 5 pulls 6..=9.
    let mut carried: Vec<(u64, u32)> = Vec::new();
    let floor = futures::executor::block_on(statesync::fetch_index_ops(
        &client,
        "chat",
        9,
        Some((5, u32::MAX)),
        |page| {
            carried.extend(
                page.iter()
                    .filter_map(|(key, _)| indexer::parse_op_key(key.as_bytes())),
            );
            Ok(())
        },
    ))
    .expect("the resumed walk completes");
    assert_eq!(floor, None, "the source covers the range from genesis");
    assert_eq!(
        carried,
        vec![(6, 0), (7, 0), (8, 0), (9, 0)],
        "only the rows above the cursor may cross"
    );

    /// answers every ask with the history from genesis, cursor or no cursor.
    #[derive(Clone)]
    struct Replayer;
    impl SyncClient for Replayer {
        fn request(
            &self,
            req: SyncRequest,
        ) -> impl std::future::Future<Output = Result<SyncResponse, SyncError>> + Send {
            let SyncRequest::IndexOps { .. } = req else {
                unreachable!("only the IndexOps lane is asked")
            };
            async move {
                let row = |height: u64| {
                    borsh::to_vec(&indexer::OpRow {
                        height,
                        seq: 0,
                        time: 1_000 + height,
                        origin: indexer::OriginTag::external("jess"),
                        payload: b"{}".to_vec(),
                        assigned: Vec::new(),
                    })
                    .expect("row encodes")
                };
                Ok(SyncResponse::IndexOps {
                    rows: vec![
                        (indexer::op_key(1, 0), row(1)),
                        (indexer::op_key(2, 0), row(2)),
                    ],
                    next_after: None,
                    source_floor: None,
                    applied_height: 9,
                })
            }
        }
    }
    let mut written = 0usize;
    let err = futures::executor::block_on(statesync::fetch_index_ops(
        &Replayer,
        "chat",
        9,
        Some((5, u32::MAX)),
        |page| {
            written += page.len();
            Ok(())
        },
    ))
    .expect_err("a replay below the resume cursor must refuse");
    assert!(
        matches!(&err, SyncError::Module { reason, .. } if reason.contains("does not ascend")),
        "want the ascent refusal, got {err}"
    );
    assert_eq!(written, 0, "nothing below the cursor reaches the store");
}

/// A CURSOR WITHOUT ROWS BEHIND IT IS THE WALK'S ONLY UNBOUNDED SHAPE, and it
/// has to be refused rather than re-asked. A source that serves a page, then
/// answers "no rows, but ask again from the same place", would otherwise spin
/// a joining node forever — inside the join seam, before it ever serves.
#[test]
fn an_empty_page_re_offering_its_cursor_is_refused_not_re_asked() {
    /// serves ONE real row, then empty pages that keep re-offering the cursor
    /// it already handed out.
    #[derive(Clone)]
    struct StuckSource;
    impl SyncClient for StuckSource {
        fn request(
            &self,
            req: SyncRequest,
        ) -> impl std::future::Future<Output = Result<SyncResponse, SyncError>> + Send {
            let SyncRequest::IndexOps { after, .. } = req else {
                unreachable!("only the IndexOps lane is asked")
            };
            async move {
                let row = borsh::to_vec(&indexer::OpRow {
                    height: 1,
                    seq: 0,
                    time: 1_001,
                    origin: indexer::OriginTag::external("jess"),
                    payload: b"{}".to_vec(),
                    assigned: Vec::new(),
                })
                .expect("row encodes");
                Ok(SyncResponse::IndexOps {
                    // the FIRST ask gets the row; every ask from its cursor
                    // gets nothing but the same cursor back.
                    rows: match after {
                        None => vec![(indexer::op_key(1, 0), row)],
                        Some(_) => Vec::new(),
                    },
                    next_after: Some((1, 0)),
                    source_floor: None,
                    applied_height: 9,
                })
            }
        }
    }
    let err = futures::executor::block_on(statesync::fetch_index_ops(
        &StuckSource,
        "chat",
        9,
        None,
        |_| Ok(()),
    ))
    .expect_err("an empty page re-offering its cursor must refuse");
    assert!(
        matches!(&err, SyncError::Module { reason, .. } if reason.contains("0 rows served")),
        "want the no-progress refusal, got {err}"
    );
}

/// A KEY THAT PARSES IS NOT A KEY THAT SORTS. `op/2/0` decodes to `(2, 0)` —
/// `from_str_radix` neither demands the canonical width nor rejects a leading
/// `+` — so a source can pass every ascent check while handing the joiner a
/// key that lands AFTER `op/0000000000000009/00000000` in the store. Key order is
/// the one invariant this whole lane rests on: the next `converge_guest`
/// refold replays `op/` in KEY order, so a mis-sorted row means every derived
/// view is rebuilt from history running backwards, silently. The boundary
/// therefore demands the byte-exact canonical rendering, not a parse.
#[test]
fn a_non_canonical_op_key_is_refused_even_though_it_parses_and_ascends() {
    /// one page, two rows, ASCENDING BY PARSED POSITION — and the first key is
    /// short-form, so its bytes sort after the second's.
    #[derive(Clone)]
    struct WidthLiar;
    impl SyncClient for WidthLiar {
        fn request(
            &self,
            req: SyncRequest,
        ) -> impl std::future::Future<Output = Result<SyncResponse, SyncError>> + Send {
            let SyncRequest::IndexOps { .. } = req else {
                unreachable!("only the IndexOps lane is asked")
            };
            async move {
                let row = |height: u64| {
                    borsh::to_vec(&indexer::OpRow {
                        height,
                        seq: 0,
                        time: 1_000 + height,
                        origin: indexer::OriginTag::external("jess"),
                        payload: b"{}".to_vec(),
                        assigned: Vec::new(),
                    })
                    .expect("row encodes")
                };
                Ok(SyncResponse::IndexOps {
                    rows: vec![
                        ("op/2/0".to_string(), row(2)),
                        (indexer::op_key(9, 0), row(9)),
                    ],
                    next_after: None,
                    source_floor: None,
                    applied_height: 9,
                })
            }
        }
    }
    // proof the row is otherwise impeccable: it parses, it ascends, its height
    // is under the boundary, and it borsh-decodes agreeing with its own key.
    assert_eq!(indexer::parse_op_key(b"op/2/0"), Some((2, 0)));
    assert!(
        "op/2/0" > indexer::op_key(9, 0).as_str(),
        "and it MIS-SORTS"
    );

    let mut written = 0usize;
    let err = futures::executor::block_on(statesync::fetch_index_ops(
        &WidthLiar,
        "chat",
        9,
        None,
        |page| {
            written += page.len();
            Ok(())
        },
    ))
    .expect_err("a non-canonical op key must refuse");
    assert!(
        matches!(&err, SyncError::Module { reason, .. } if reason.contains("canonical")),
        "want the canonical-shape refusal, got {err}"
    );
    assert_eq!(written, 0, "nothing from a lying page may reach the store");
}
