//! the ROOT-CONTINUITY proof for files (duckfs): the files guest component over
//! `WasmModule::with_odb(FilesOdbBacking)` and the native `Files` module over the
//! same disk substrate are BYTE-IDENTICAL block-by-block. unlike the whole-state
//! adapter ports (whose root representations differ), files' root is
//! `sha256(encode_refs)` on BOTH runtimes — the cutover changes the executor, not
//! one committed byte — and this proof pins that: the same
//! op stream commits the identical files root after EVERY block from genesis, the
//! same query replies, the same committed-only mid-block reads, the same watch
//! fan-out, and the same object-possession serve bytes.
//!
//! each block is a single `block_on(host.submit_at(..))` (or `submit_block` for a
//! multi-dispatch block); `submit_at` awaits execute AND the disk-persisting
//! `commit_block` internally, so no helper nests a second `block_on`. the wasm
//! `Files` tenant is runtime-agnostic (`wasm-host` pulls only `wasmtime`), so the
//! plain futures executor drives both sides, exactly as `files`' own host_e2e does.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use futures::executor::block_on;

use files::objects::object_id;
use files::{
    CHUNK_SIZE, Change, Content, Files, FilesMsg, FilesQuery, FilesReply, FilesSyncReq, Kind,
    MAX_CHANGES_PER_COMMIT, MAX_INLINE_COMMIT_BYTES, MAX_OBJECT_READS_PER_OP, MAX_READ_BYTES,
    encode_msg, encode_putblob, encode_query, encode_sync_req, to_hex,
};
use files_odb::FilesOdbBacking;
use host::{BlockContext, Host, MemberOutcome, SubmitError};
use sdk::{Ctx, Error, Module, ModuleId, Msg, Origin, StateRoot};
use sha2::{Digest as _, Sha256};
use wasm_host::WasmModule;

const FILES: &str = "files";

/// GENERATED artifact — built from the module crate's guest port by
/// guest-builder (`make wasm-modules`); committed so this proof is self-contained (the same fixture the
/// test pins).
const FILES_WASM: &[u8] = include_bytes!("fixtures/files.component.wasm");

// ---- the two runtimes over their own tempdirs -------------------------------

/// a native `Files` over `dir`, plus the two sibling modules the parity matrix
/// needs beside it: a [`Recorder`] (the watch-notification target) and a
/// [`QueryProbe`] (the mid-block committed-read prober). genesis only REGISTERS,
/// so a fresh dir starts at the empty refs root.
fn native_host(dir: &tempfile::TempDir) -> Host {
    Host::genesis(vec![
        Box::new(Files::open(FILES, dir.path().to_path_buf()).expect("open native files")),
        Box::new(Recorder::new("recorder")),
        Box::new(QueryProbe::new()),
        Box::new(identity::Identity::new(
            "identity",
            Box::new(sdk_testkit::MemStore::new()),
            "files-parity".into(),
        )),
        Box::new(attribution::AttributionModule::new(
            "attribution",
            Box::new(sdk_testkit::MemStore::new()),
        )),
    ])
    .expect("native genesis")
}

/// the wasm `files` tenant: the files guest component over a `FilesOdbBacking`
/// on `dir` — the exact `WasmModule::with_odb` composition bin/node uses — beside
/// the SAME two native siblings (kept native on both hosts so the emitted
/// follow-ups land identically and only the files cutover is under test).
fn wasm_host(dir: &tempfile::TempDir) -> Host {
    let backing = FilesOdbBacking::open(FILES, dir.path().to_path_buf()).expect("open odb backing");
    Host::genesis(vec![
        Box::new(
            WasmModule::with_odb(FILES, FILES_WASM, Box::new(backing)).expect("load component"),
        ),
        Box::new(Recorder::new("recorder")),
        Box::new(QueryProbe::new()),
        Box::new(identity::Identity::new(
            "identity",
            Box::new(sdk_testkit::MemStore::new()),
            "files-parity".into(),
        )),
        Box::new(attribution::AttributionModule::new(
            "attribution",
            Box::new(sdk_testkit::MemStore::new()),
        )),
    ])
    .expect("wasm genesis")
}

/// the consensus context for one block: both runtimes must see the identical env.
fn block(height: u64, origin: Origin) -> BlockContext {
    BlockContext {
        height,
        consensus_time: 1_000 + height,
        origin,
    }
}

// ---- root + reply comparison seams ------------------------------------------

/// every registered module's root, in registry order — the per-module equality
/// that is the whole claim (folds files + recorder + probe). identical registry
/// order on both hosts makes this a byte-for-byte cross-runtime compare.
fn all_roots(h: &Host) -> Vec<(ModuleId, StateRoot)> {
    h.module_roots()
}

fn files_root(h: &Host) -> StateRoot {
    h.module_root(FILES).expect("files registered")
}

/// the read matrix: every query family, including the `None`/absent shapes, plus
/// a body-reading `Read` (served host-side off the disk odb) and the staging
/// probe. byte-identical replies are the read-surface half of root continuity.
fn replies(h: &Host) -> Vec<Vec<u8>> {
    let note_chunk = to_hex(&object_id(Kind::Chunk, b"hello inline"));
    let queries = [
        FilesQuery::Stat {
            path: "/shared/f0".into(),
            snapshot: None,
        },
        FilesQuery::Stat {
            path: "/shared/note.txt".into(),
            snapshot: None,
        },
        FilesQuery::Stat {
            path: "/absent".into(),
            snapshot: None,
        },
        FilesQuery::Ls {
            path: "/shared".into(),
            snapshot: None,
            after: None,
            limit: 256,
        },
        FilesQuery::Read {
            path: "/shared/note.txt".into(),
            snapshot: None,
            offset: 0,
            len: MAX_READ_BYTES,
        },
        FilesQuery::Find {
            prefix: "/shared".into(),
            snapshot: None,
            after: None,
            limit: 256,
        },
        FilesQuery::History { limit: 64 },
        FilesQuery::Refs {},
        FilesQuery::HasChunks {
            ids: vec![note_chunk],
        },
    ];
    // fold BOTH outcomes into comparable bytes: a query on a not-yet-existing
    // path is a deterministic `Err` (identical code on both lanes — native
    // `Fs::query` vs the backing's `Fs::query`), so error PARITY is as much the
    // claim as reply parity. `.expect` here would panic on the early blocks whose
    // paths are absent; instead the error string rides into the compared vector.
    queries
        .iter()
        .map(|q| match block_on(h.query(FILES, &encode_query(q))) {
            Ok(bytes) => bytes,
            Err(e) => format!("ERR:{e}").into_bytes(),
        })
        .collect()
}

/// the committed head snapshot hex — deterministic, so identical across the two
/// runtimes and a threadable pin/commit base.
fn head(h: &Host) -> String {
    let reply = block_on(h.query(FILES, &encode_query(&FilesQuery::Refs {}))).expect("refs query");
    match files::decode_reply(&reply).expect("decode refs") {
        FilesReply::Refs(info) => info.head.expect("head present"),
        other => panic!("expected Refs, got {other:?}"),
    }
}

// ---- op builders (mirroring files/tests/host_e2e.rs) ------------------------

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
    to_hex(&object_id(Kind::Chunk, bytes))
}

// ---- sibling modules (kept native on both hosts) ----------------------------

/// the watch-notification target: a module that appends every follow-up payload
/// it receives to its committed log, so its root MOVES exactly when a
/// `duckfs_notify` was delivered. registered in BOTH hosts — if the wasm guest
/// failed to emit the notification the native emits, this module's root would
/// diverge and the block-by-block check would fire. length-prefixed concat so two
/// payloads can never alias one.
struct Recorder {
    id: ModuleId,
    committed: Vec<Vec<u8>>,
    staged: Vec<Vec<u8>>,
}

impl Recorder {
    fn new(id: &str) -> Self {
        Self {
            id: id.into(),
            committed: Vec::new(),
            staged: Vec::new(),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Module for Recorder {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }
    fn root(&self) -> StateRoot {
        let mut h = Sha256::new();
        for payload in &self.committed {
            h.update((payload.len() as u64).to_le_bytes());
            h.update(payload);
        }
        StateRoot(h.finalize().into())
    }
    async fn execute(&mut self, _ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        self.staged.push(msg.payload.clone());
        Ok(())
    }
    async fn commit_block(&mut self) -> Result<(), Error> {
        self.committed.append(&mut self.staged);
        Ok(())
    }
    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged.clear();
        Ok(())
    }
}

/// a mid-block query prober (the `runs` sibling-read pattern): `execute`
/// host-routes its payload as a files query (`Ctx::query` → the committed-only
/// backing/query lane) and STAGES the reply; `commit_block` commits it, so the
/// probe's root commits to the exact bytes it saw mid-block. registered in BOTH
/// hosts — a divergent committed-only read would diverge the probe roots.
struct QueryProbe {
    staged: Option<Vec<u8>>,
    committed: Vec<u8>,
}

impl QueryProbe {
    fn new() -> Self {
        Self {
            staged: None,
            committed: Vec::new(),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Module for QueryProbe {
    fn id(&self) -> ModuleId {
        "probe".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot(Sha256::digest(&self.committed).into())
    }
    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        self.staged = Some(ctx.query(FILES, &msg.payload).await?);
        Ok(())
    }
    async fn commit_block(&mut self) -> Result<(), Error> {
        if let Some(reply) = self.staged.take() {
            self.committed = reply;
        }
        Ok(())
    }
    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged = None;
        Ok(())
    }
}

// ============================================================================
// CASE 1/2/5(pin)/11: the full happy-path matrix, block-by-block root equality
// ============================================================================

/// putblob → commit(Chunks) round trip [1], an inline commit [2], pin/unpin [5],
/// and the object-possession serve surface [11] — driven through BOTH runtimes,
/// asserting the files root is byte-identical after EVERY block from genesis and
/// every query reply matches.
#[test]
fn happy_path_matrix_roots_identical_block_by_block() {
    let dir_n = tempfile::tempdir().unwrap();
    let dir_w = tempfile::tempdir().unwrap();
    let mut native = native_host(&dir_n);
    let mut wasm = wasm_host(&dir_w);
    let owner = Origin::External(b"tester".to_vec());

    // ROOT CONTINUITY from block zero: both sides commit to the SAME empty refs,
    // so — unlike the whole-state ports — the roots are EQUAL.
    assert_eq!(
        all_roots(&native),
        all_roots(&wasm),
        "genesis roots diverge"
    );
    // recovery's disk cohort sees the wasm tenant exactly as it saw native
    // files: the duckfs-odb lane commits its own objects per block.
    assert!(native.block_durable_ids().contains(FILES));
    assert!(wasm.block_durable_ids().contains(FILES));

    let c0 = vec![0x11u8; 1000];
    let c1 = vec![0x22u8; 2000];

    // each op is one block; `moves` marks the blocks that must advance the files
    // root (every one here does — putblob stages into refs, commits/pins mutate).
    let head_hex = {
        let ops: Vec<(u64, Origin, Msg)> = vec![
            (1, owner.clone(), putblob_op(&c0)),
            (2, owner.clone(), putblob_op(&c1)),
            (
                3,
                owner.clone(),
                commit_op(
                    None,
                    "genesis commit",
                    vec![
                        put_chunks("/shared/f0", c0.len() as u64, &[chunk_hex(&c0)]),
                        put_chunks("/shared/f1", c1.len() as u64, &[chunk_hex(&c1)]),
                        put_inline("/shared/note.txt", b"hello inline"),
                        Change::Mkdir {
                            path: "/shared/dir".into(),
                        },
                        Change::Symlink {
                            path: "/shared/link".into(),
                            target: "/shared/f0".into(),
                        },
                    ],
                ),
            ),
        ];
        for (height, origin, msg) in ops {
            let before = files_root(&native);
            block_on(native.submit_at(block(height, origin.clone()), msg.clone())).expect("native");
            block_on(wasm.submit_at(block(height, origin), msg)).expect("wasm");
            assert_eq!(
                all_roots(&native),
                all_roots(&wasm),
                "roots diverge after block {height}"
            );
            assert_ne!(files_root(&native), before, "files root stuck at {height}");
            assert_eq!(
                replies(&native),
                replies(&wasm),
                "replies diverge at {height}"
            );
        }
        head(&native)
    };
    assert_eq!(
        head_hex,
        head(&wasm),
        "hosts disagree on the committed head"
    );

    // pin [5] then unpin the head; the same signer threads both.
    for (height, msg) in [
        (4, pin_op(&head_hex, "release")),
        (5, {
            Msg {
                target: FILES.into(),
                payload: encode_msg(&FilesMsg::Unpin {
                    name: "release".into(),
                }),
            }
        }),
    ] {
        let before = files_root(&native);
        block_on(native.submit_at(block(height, owner.clone()), msg.clone())).expect("native pin");
        block_on(wasm.submit_at(block(height, owner.clone()), msg)).expect("wasm pin");
        assert_eq!(
            all_roots(&native),
            all_roots(&wasm),
            "pin block {height} diverges"
        );
        assert_ne!(files_root(&native), before, "pin/unpin must move the root");
    }

    // [11] the object-possession serve surface is byte-identical on identical
    // committed state: GetRefs (the refs image a joiner installs) and GetObjects
    // (the chunk bodies it fetches) must serve the same bytes from either executor.
    let get_refs = encode_sync_req(&FilesSyncReq::GetRefs);
    assert_eq!(
        block_on(native.serve_sync(FILES, &get_refs)).expect("native GetRefs"),
        block_on(wasm.serve_sync(FILES, &get_refs)).expect("wasm GetRefs"),
        "GetRefs serve bytes diverge"
    );
    let get_objs = encode_sync_req(&FilesSyncReq::GetObjects {
        ids: vec![
            chunk_hex(&c0),
            chunk_hex(&c1),
            to_hex(&object_id(Kind::Chunk, b"hello inline")),
        ],
    });
    assert_eq!(
        block_on(native.serve_sync(FILES, &get_objs)).expect("native GetObjects"),
        block_on(wasm.serve_sync(FILES, &get_objs)).expect("wasm GetObjects"),
        "GetObjects serve bytes diverge"
    );

    // a query never moves a root on either side.
    let settled = all_roots(&wasm);
    let _ = replies(&wasm);
    assert_eq!(all_roots(&wasm), settled, "a query moved a root");
}

// ============================================================================
// CASE 3: same-block cross-dispatch faces (the Task-3-fix parity cases)
// ============================================================================

/// an inline chunk produced by an earlier commit in the SAME block, referenced by
/// a later `Content::Chunks` commit [face 1] and de-duped by a later putblob of
/// the same bytes [face 2]. native carries the block-local object index in-memory;
/// the guest reconstructs it through `__block_objects`. BOTH must accept with
/// byte-identical roots — the fixed divergence, pinned on the real fixture.
#[test]
fn same_block_faces_match_native() {
    let content = b"small inline body";
    let chunk = chunk_hex(content);

    // face 1: inline /a, then Chunks /b referencing /a's inline chunk — one block.
    {
        let dir_n = tempfile::tempdir().unwrap();
        let dir_w = tempfile::tempdir().unwrap();
        let mut native = native_host(&dir_n);
        let mut wasm = wasm_host(&dir_w);
        let batch = vec![
            (
                Origin::System,
                commit_op(None, "inline", vec![put_inline("/a", content)]),
            ),
            (
                Origin::System,
                commit_op(
                    None,
                    "chunks",
                    vec![put_chunks(
                        "/b",
                        content.len() as u64,
                        std::slice::from_ref(&chunk),
                    )],
                ),
            ),
        ];
        let n_out =
            block_on(native.submit_block(block(1, Origin::System), batch.clone())).expect("native");
        let w_out = block_on(wasm.submit_block(block(1, Origin::System), batch)).expect("wasm");
        for out in [&n_out, &w_out] {
            assert!(
                out.members
                    .iter()
                    .all(|m| matches!(m, MemberOutcome::Applied { .. })),
                "both same-block members must apply (face 1): {:?}",
                out.members
            );
        }
        assert_eq!(all_roots(&native), all_roots(&wasm), "face 1 roots diverge");
        assert_eq!(replies(&native), replies(&wasm), "face 1 replies diverge");
    }

    // face 2: inline /a, then putblob the same bytes — the putblob must DEDUP
    // against the block index (no phantom staging entry), identically on both.
    {
        let dir_n = tempfile::tempdir().unwrap();
        let dir_w = tempfile::tempdir().unwrap();
        let mut native = native_host(&dir_n);
        let mut wasm = wasm_host(&dir_w);
        let batch = vec![
            (
                Origin::System,
                commit_op(None, "inline", vec![put_inline("/a", content)]),
            ),
            (Origin::System, putblob_op(content)),
        ];
        let n_out =
            block_on(native.submit_block(block(1, Origin::System), batch.clone())).expect("native");
        let w_out = block_on(wasm.submit_block(block(1, Origin::System), batch)).expect("wasm");
        for out in [&n_out, &w_out] {
            assert!(
                out.members
                    .iter()
                    .all(|m| matches!(m, MemberOutcome::Applied { .. })),
                "both same-block members must apply (face 2): {:?}",
                out.members
            );
        }
        assert_eq!(all_roots(&native), all_roots(&wasm), "face 2 roots diverge");
        // the dedup is observable: refs.staging is empty (no phantom entry), so
        // the Refs reply is byte-identical to native's.
        assert_eq!(replies(&native), replies(&wasm), "face 2 replies diverge");
    }
}

// ============================================================================
// CASE 4/6/7: rejection verdicts match, and an aborted block leaves no trace
// ============================================================================

/// distinct deterministic rejection families [4 CAS, 6 resource cap], each proving
/// the same aborted-block invariant [7]: both runtimes reject with the native
/// reason, and NEITHER root nor the object-possession serve surface moves.
#[test]
fn rejections_match_and_leave_roots_and_odb_unmoved() {
    let dir_n = tempfile::tempdir().unwrap();
    let dir_w = tempfile::tempdir().unwrap();
    let mut native = native_host(&dir_n);
    let mut wasm = wasm_host(&dir_w);

    // setup: one committed file so the CAS class has real state to collide with.
    let setup = commit_op(None, "v0", vec![put_inline("/shared/x", b"v0")]);
    block_on(native.submit_at(block(1, Origin::System), setup.clone())).expect("native setup");
    block_on(wasm.submit_at(block(1, Origin::System), setup)).expect("wasm setup");
    assert_eq!(all_roots(&native), all_roots(&wasm), "setup roots diverge");

    // the rejection matrix. needles are verified against `fs.rs` error strings
    // (non-vacuous): each must appear in BOTH the native sentence and the wasm
    // sentence (the guest carries the sentence verbatim, so containment holds).
    //
    // CASE-6 ADAPTATION: the real STAGING_QUOTA_BYTES is 1 GiB and the
    // per-owner-entry caps are 4096 — neither reachable cheaply through the op
    // boundary, and the `set_*_for_tests` seams live only on native `Files`, not
    // on the wasm tenant (`WasmModule` exposes no test seam). so the staging-quota
    // slot uses the MAX_CHANGES_PER_COMMIT cap — a cheap deterministic
    // resource-cap rejection reachable purely via op shape, the brief's named
    // fallback. (fs.rs:889 "commit exceeds the change cap".)
    let over_cap: Vec<Change> = (0..=MAX_CHANGES_PER_COMMIT)
        .map(|i| Change::Mkdir {
            path: format!("/c{i}"),
        })
        .collect();
    let rejects: Vec<(u64, Msg, &str)> = vec![
        // [4] per-path CAS: re-create /shared/x on the empty base while it exists.
        (
            2,
            commit_op(None, "cas", vec![put_inline("/shared/x", b"beta")]),
            "changed since base",
        ),
        // [6] resource cap (staging-quota substitute): one over the change cap.
        (
            3,
            commit_op(None, "flood", over_cap),
            "commit exceeds the change cap",
        ),
        // a staging-path reject too: a chunk one byte over CHUNK_SIZE.
        (
            4,
            putblob_op(&vec![0u8; CHUNK_SIZE as usize + 1]),
            "chunk exceeds CHUNK_SIZE",
        ),
    ];

    let get_refs = encode_sync_req(&FilesSyncReq::GetRefs);
    for (height, msg, needle) in rejects {
        let before = all_roots(&native);
        assert_eq!(before, all_roots(&wasm), "pre-reject roots diverge");
        let serve_before = block_on(native.serve_sync(FILES, &get_refs)).expect("serve before");

        let n_err = block_on(native.submit_at(block(height, Origin::System), msg.clone()))
            .expect_err("native rejects");
        let w_err =
            block_on(wasm.submit_at(block(height, Origin::System), msg)).expect_err("wasm rejects");
        assert_module_reject("native", height, &n_err, needle);
        assert_module_reject("wasm", height, &w_err, needle);
        // [7] aborted block leaves no trace: roots byte-identical to pre-block and
        // still equal, and the object-possession serve surface is unmoved.
        assert_eq!(all_roots(&native), before, "native root moved on reject");
        assert_eq!(all_roots(&wasm), before, "wasm root moved on reject");
        assert_eq!(
            block_on(native.serve_sync(FILES, &get_refs)).expect("serve after"),
            serve_before,
            "aborted block moved the odb/refs serve surface"
        );
    }
}

/// A project-sized workload exercises successful writes and reads with the
/// production guest fuel and object limits: 128 distinct documents, a 2 MiB
/// chunked asset, a 32-document edit, historical reads, and durable reopen.
/// The import spends the whole per-op object-read budget (128 documents × a
/// chunk and a fileobj each), so the asset rides the next commit — one commit
/// is one op, and one op gets [`MAX_OBJECT_READS_PER_OP`] committed reads.
#[test]
fn project_workload_succeeds_with_production_guest_limits() {
    let dir_n = tempfile::tempdir().unwrap();
    let dir_w = tempfile::tempdir().unwrap();
    let mut native = native_host(&dir_n);
    let mut wasm = wasm_host(&dir_w);
    let chunks = [
        vec![0x41; CHUNK_SIZE as usize],
        vec![0x42; CHUNK_SIZE as usize],
    ];
    for (index, chunk) in chunks.iter().enumerate() {
        let height = index as u64 + 1;
        let operation = putblob_op(chunk);
        block_on(native.submit_at(block(height, Origin::System), operation.clone())).unwrap();
        block_on(wasm.submit_at(block(height, Origin::System), operation)).unwrap();
        assert_eq!(all_roots(&native), all_roots(&wasm));
    }
    let documents: Vec<Vec<u8>> = (0..128)
        .map(|index| {
            let mut bytes = format!("document {index:03}\n").into_bytes();
            bytes.resize(1024, b'x');
            bytes
        })
        .collect();
    let changes: Vec<_> = documents
        .iter()
        .enumerate()
        .map(|(index, bytes)| put_inline(&format!("/shared/project/docs/{index:03}.txt"), bytes))
        .collect();
    let operation = commit_op(None, "import project", changes);
    block_on(native.submit_at(block(3, Origin::System), operation.clone())).unwrap();
    block_on(wasm.submit_at(block(3, Origin::System), operation)).unwrap();
    assert_eq!(all_roots(&native), all_roots(&wasm));
    let snapshot = head(&wasm);
    let operation = commit_op(
        Some(&snapshot),
        "add the chunked asset",
        vec![put_chunks(
            "/shared/project/asset.bin",
            2 * CHUNK_SIZE,
            &chunks
                .iter()
                .map(|chunk| chunk_hex(chunk))
                .collect::<Vec<_>>(),
        )],
    );
    block_on(native.submit_at(block(4, Origin::System), operation.clone())).unwrap();
    block_on(wasm.submit_at(block(4, Origin::System), operation)).unwrap();
    assert_eq!(all_roots(&native), all_roots(&wasm));
    let operation = commit_op(
        Some(&head(&wasm)),
        "edit 32 documents",
        (0..32)
            .map(|index| put_inline(&format!("/shared/project/docs/{index:03}.txt"), b"edited"))
            .collect(),
    );
    block_on(native.submit_at(block(5, Origin::System), operation.clone())).unwrap();
    block_on(wasm.submit_at(block(5, Origin::System), operation)).unwrap();
    assert_eq!(all_roots(&native), all_roots(&wasm));
    let root = files_root(&wasm);
    drop(wasm);
    let wasm = wasm_host(&dir_w);
    // Attribution is an in-memory sibling in this fixture; Files owns disk recovery.
    assert_eq!(
        files_root(&wasm),
        root,
        "durable reopen preserves Files root"
    );
    for (index, expected) in documents.iter().enumerate() {
        let query = FilesQuery::Read {
            path: format!("/shared/project/docs/{index:03}.txt"),
            snapshot: Some(snapshot.clone()),
            offset: 0,
            len: MAX_READ_BYTES,
        };
        let guest = block_on(wasm.query(FILES, &encode_query(&query))).unwrap();
        assert_eq!(
            guest,
            block_on(native.query(FILES, &encode_query(&query))).unwrap()
        );
        let FilesReply::Read { b64, .. } = files::decode_reply(&guest).unwrap() else {
            panic!("read reply");
        };
        assert_eq!(STANDARD.decode(b64).unwrap(), *expected);
    }
    for (index, expected) in chunks.iter().enumerate() {
        let query = FilesQuery::Read {
            path: "/shared/project/asset.bin".into(),
            snapshot: None,
            offset: index as u64 * CHUNK_SIZE,
            len: CHUNK_SIZE,
        };
        let guest = block_on(wasm.query(FILES, &encode_query(&query))).unwrap();
        assert_eq!(
            guest,
            block_on(native.query(FILES, &encode_query(&query))).unwrap()
        );
        let FilesReply::Read { b64, .. } = files::decode_reply(&guest).unwrap() else {
            panic!("read reply");
        };
        assert_eq!(STANDARD.decode(b64).unwrap(), *expected);
    }
    let query = FilesQuery::Ls {
        path: "/shared/project/docs".into(),
        snapshot: None,
        after: None,
        limit: 256,
    };
    let guest = block_on(wasm.query(FILES, &encode_query(&query))).unwrap();
    assert_eq!(
        guest,
        block_on(native.query(FILES, &encode_query(&query))).unwrap()
    );
    let FilesReply::Ls { entries, .. } = files::decode_reply(&guest).unwrap() else {
        panic!("listing reply");
    };
    assert_eq!(entries.len(), 128);
}

// ============================================================================
// CASE 13: the per-op object-read consensus cap — ONE BOUND, BOTH RUNTIMES
// ============================================================================

/// one commit of `count` distinct inline documents of `body` bytes each, named
/// `first..first + count` under `/shared/<dir>/`. a row picks a directory of its
/// own, or a `first` past what a previous row wrote, so no row collides with
/// another row's paths and refuses on the per-path CAS instead of the budget
/// under test. each distinct body stages a chunk and a fileobj — two distinct
/// `object-stat` probes — and the rewritten trees are staged, not probed, so the
/// op charges `2 * count` for its documents, plus [`HEAD_SPINE_READS`] when
/// `base`/the effective head is a real snapshot rather than the empty tree.
fn import_of(base: Option<&str>, dir: &str, first: usize, count: usize, body: usize) -> Msg {
    let changes = (first..first + count)
        .map(|index| {
            let mut bytes = format!("document {index:06}\n").into_bytes();
            bytes.resize(body, b'x');
            put_inline(&format!("/shared/{dir}/{index:06}.txt"), &bytes)
        })
        .collect();
    commit_op(base, "import", changes)
}

/// what a commit spends on the tree it lands on, BEFORE it charges a single
/// document: the effective head snapshot object, then one `object-get` per
/// pre-existing directory on the spine of `/shared/<dir>/<name>` — the root
/// tree, `/shared`, and `/shared/<dir>`. the base snapshot and its spine resolve
/// to those same ids and dedupe, and the count does not move with the document
/// count or with how many entries the directory already holds, so it is a flat
/// per-commit constant on top of the 2 reads each document costs.
const HEAD_SPINE_READS: usize = 4;

/// the distinct-object-read cap ([`MAX_OBJECT_READS_PER_OP`]) is a FILES
/// CONSENSUS RULE single-sourced in `duckfs-core`, and a cap is only one bound
/// if it binds in BOTH directions: a commit that spends the whole cap must be
/// ACCEPTED by both runtimes, and every commit past it REJECTED by both, with
/// neither root moving. the cap counts distinct committed-store `object-get`
/// (tree-walk) AND `object-stat` (`stage_object` presence probe) reads in ONE
/// bound, mirroring the kernel.
///
/// the accept row is the whole point and the fragile half. the guest spends one
/// `wasm_host::DEFAULT_FUEL` on the whole dispatch, so a cap set above what that
/// fuel admits is not a cap at all — it is a band in which native accepts what
/// the guest traps on, and nothing below the band notices. so the accept row is
/// the most expensive commit the other
/// budgets admit AT the cap: cap/2 documents whose bodies together fill
/// [`MAX_INLINE_COMMIT_BYTES`] exactly. it sits on both ceilings at once.
///
/// there are TWO accept rows because there are two shapes, and the one users
/// meet is the second. cap/2 documents is what an EMPTY tree admits: there is no
/// spine to walk, so the whole budget goes to documents. every commit after the
/// first lands on an existing head and pays [`HEAD_SPINE_READS`] off the top, so
/// its bound is `(cap - HEAD_SPINE_READS) / 2` = 126 documents — asserted here on
/// the head the genesis row just created, together with the reject one document
/// past it. without that pair the accept half of the matrix only ever holds for
/// the one commit in a network's life that has no head.
///
/// the reject rows climb from one read over the cap to 16x it — the band where
/// native used to accept alone. all of them are refused by both runtimes with
/// the SAME reason and leave both roots exactly where the accepted commit left
/// them: the core charges a read BEFORE issuing it, so even the 16x row is
/// refused on the read that breaches the cap rather than out of fuel much later.
#[test]
fn object_read_cap_is_one_bound_on_both_runtimes() {
    let at_cap = MAX_OBJECT_READS_PER_OP / 2;
    let saturating_body = MAX_INLINE_COMMIT_BYTES / at_cap;

    let dir_n = tempfile::tempdir().unwrap();
    let dir_w = tempfile::tempdir().unwrap();
    let mut native = native_host(&dir_n);
    let mut wasm = wasm_host(&dir_w);
    let genesis = all_roots(&native);
    assert_eq!(genesis, all_roots(&wasm), "genesis roots diverge");

    // [accept] exactly the cap, with the inline budget spent too. base=None onto
    // an empty tree: the genesis shape, where nothing but documents is charged.
    let at_cap_op = import_of(None, "at-cap", 0, at_cap, saturating_body);
    block_on(native.submit_at(block(1, Origin::System), at_cap_op.clone()))
        .expect("native commits the whole object-read budget");
    block_on(wasm.submit_at(block(1, Origin::System), at_cap_op))
        .expect("the guest must REACH the documented cap, not trap on fuel below it");
    let committed = all_roots(&native);
    assert_eq!(committed, all_roots(&wasm), "at-cap roots diverge");
    assert_ne!(committed, genesis, "the at-cap commit must actually land");

    // [reject] one read over the cap, then across the band to 16x it. every row
    // is the same op on both runtimes at the same height.
    for (index, count) in [at_cap + 1, 2 * at_cap, 4 * at_cap, 8 * at_cap, 16 * at_cap]
        .into_iter()
        .enumerate()
    {
        let height = index as u64 + 2;
        // 64-byte bodies keep even the 16x row inside MAX_INLINE_COMMIT_BYTES,
        // so the object-read cap is the bound under test, not the inline budget.
        let op = import_of(None, &format!("over-cap-{count}"), 0, count, 64);
        let native_err = block_on(native.submit_at(block(height, Origin::System), op.clone()))
            .expect_err("native rejects past the object-read cap");
        let wasm_err = block_on(wasm.submit_at(block(height, Origin::System), op))
            .expect_err("wasm rejects past the object-read cap");
        assert_module_reject("native", height, &native_err, "object-read budget");
        assert_module_reject("wasm", height, &wasm_err, "object-read budget");
        assert_eq!(
            all_roots(&native),
            committed,
            "native root moved on reject at {count} documents"
        );
        assert_eq!(
            all_roots(&wasm),
            committed,
            "wasm root moved on reject at {count} documents"
        );
    }

    // [accept] the same cap onto an EXISTING head — the shape every commit after
    // the first one has. the head snapshot and the directory spine this commit
    // rewrites are charged before a single document is, so the SAME cap carries
    // fewer documents here than at genesis: 126 into a directory that already
    // holds some, not 128. the bodies spend the inline budget too, so this row
    // sits on both ceilings exactly as the genesis row does.
    let onto_head = (MAX_OBJECT_READS_PER_OP - HEAD_SPINE_READS) / 2;
    let onto_head_body = MAX_INLINE_COMMIT_BYTES / onto_head;
    let head_hex = head(&native);
    assert_eq!(
        head_hex,
        head(&wasm),
        "heads diverge before the onto-head row"
    );
    let onto_head_op = import_of(Some(&head_hex), "at-cap", at_cap, onto_head, onto_head_body);
    block_on(native.submit_at(block(7, Origin::System), onto_head_op.clone()))
        .expect("native commits the whole object-read budget onto an existing head");
    block_on(wasm.submit_at(block(7, Origin::System), onto_head_op))
        .expect("the guest must REACH the cap onto an existing head, not only onto an empty tree");
    let extended = all_roots(&native);
    assert_eq!(extended, all_roots(&wasm), "onto-head roots diverge");
    assert_ne!(
        extended, committed,
        "the onto-head commit must actually land"
    );

    // [reject] one document past that bound is one read past the cap, refused by
    // both runtimes. 64-byte bodies keep it well inside MAX_INLINE_COMMIT_BYTES,
    // so the object-read cap is what refuses it and not the inline budget.
    let over_bound_op = import_of(
        Some(&head(&native)),
        "at-cap",
        at_cap + onto_head,
        onto_head + 1,
        64,
    );
    let native_err = block_on(native.submit_at(block(8, Origin::System), over_bound_op.clone()))
        .expect_err("native rejects one document past the onto-head bound");
    let wasm_err = block_on(wasm.submit_at(block(8, Origin::System), over_bound_op))
        .expect_err("wasm rejects one document past the onto-head bound");
    assert_module_reject("native", 8, &native_err, "object-read budget");
    assert_module_reject("wasm", 8, &wasm_err, "object-read budget");
    assert_eq!(
        all_roots(&native),
        extended,
        "native root moved on the onto-head reject"
    );
    assert_eq!(
        all_roots(&wasm),
        extended,
        "wasm root moved on the onto-head reject"
    );
}

/// a deterministic module rejection whose sentence CONTAINS `needle` — the wasm
/// runtime wraps the sentence in its wit-error rendering then unwraps it
/// verbatim, so the parity claim is containment, not string equality (same as
/// pages parity). The token itself is what `files`'s own tests pin.
fn assert_module_reject(who: &str, height: u64, err: &SubmitError, needle: &str) {
    let SubmitError::Rejected(Error::Module { sentence, .. }) = err else {
        panic!("{who} rejection shape at {height}: {err:?}");
    };
    assert!(
        sentence.contains(needle),
        "{who} sentence at {height}: {sentence}"
    );
}

// ============================================================================
// CASE 5(watch): watch-notification delivery parity
// ============================================================================

/// register a watch for the `recorder` sibling, then commit under the watched
/// prefix: the files module emits a `duckfs_notify` follow-up that the host drains
/// to `recorder` IN-BLOCK. the guest must emit the byte-identical notification the
/// native module does, or `recorder`'s root diverges.
#[test]
fn watch_notification_delivery_parity() {
    let dir_n = tempfile::tempdir().unwrap();
    let dir_w = tempfile::tempdir().unwrap();
    let mut native = native_host(&dir_n);
    let mut wasm = wasm_host(&dir_w);

    // system may register a watch for any module_id (the arbitrary-authority
    // origin); recorder is a real registered module, so the notification lands.
    let reg = watch_op("/watched", "recorder");
    block_on(native.submit_at(block(1, Origin::System), reg.clone())).expect("native watch");
    block_on(wasm.submit_at(block(1, Origin::System), reg)).expect("wasm watch");
    assert_eq!(
        all_roots(&native),
        all_roots(&wasm),
        "watch-reg roots diverge"
    );

    // recorder is still empty (nothing delivered yet) — proves the next block is
    // what moves it.
    let rec_before = native.module_root("recorder").expect("recorder");
    assert_eq!(rec_before, wasm.module_root("recorder").expect("recorder"));

    // commit under /watched → the watch fires → recorder receives duckfs_notify.
    let fire = commit_op(None, "notify", vec![put_inline("/watched/note", b"ring")]);
    block_on(native.submit_at(block(2, Origin::System), fire.clone())).expect("native fire");
    block_on(wasm.submit_at(block(2, Origin::System), fire)).expect("wasm fire");

    assert_eq!(
        all_roots(&native),
        all_roots(&wasm),
        "post-notify roots diverge"
    );
    let rec_after = native.module_root("recorder").expect("recorder");
    assert_ne!(
        rec_after, rec_before,
        "recorder must have received the notification"
    );
    assert_eq!(
        rec_after,
        wasm.module_root("recorder").expect("recorder"),
        "the wasm guest must emit the byte-identical duckfs_notify the native module does"
    );
    assert_eq!(
        replies(&native),
        replies(&wasm),
        "post-notify replies diverge"
    );
}

// ============================================================================
// CASE 9: mid-block sibling probe reads COMMITTED refs (committed-only lane)
// ============================================================================

/// the `runs` in-block read: a sibling `ctx.query("files", Refs)` mid-block sees
/// COMMITTED refs, not a same-block staged commit — on BOTH runtimes. drive a
/// block whose first dispatch STAGES a new commit and whose second dispatch probes
/// files: the probe must reply the pre-block committed refs (non-vacuous — there
/// IS a staged change it correctly ignores), byte-identical across runtimes.
#[test]
fn mid_block_sibling_probe_serves_committed_refs() {
    let dir_n = tempfile::tempdir().unwrap();
    let dir_w = tempfile::tempdir().unwrap();
    let mut native = native_host(&dir_n);
    let mut wasm = wasm_host(&dir_w);

    // block 1: commit a file so committed refs are non-empty and have a head.
    let seed = commit_op(None, "seed", vec![put_inline("/shared/a", b"first")]);
    block_on(native.submit_at(block(1, Origin::System), seed.clone())).expect("native seed");
    block_on(wasm.submit_at(block(1, Origin::System), seed)).expect("wasm seed");
    let committed_refs =
        block_on(native.query(FILES, &encode_query(&FilesQuery::Refs {}))).expect("refs");

    // block 2: a new commit (op0, staged) then the probe (op1). the probe must see
    // block-1's COMMITTED refs, never op0's staged /late.
    let batch = vec![
        (
            Origin::System,
            commit_op(None, "late", vec![put_inline("/shared/late", b"staged")]),
        ),
        (
            Origin::System,
            Msg {
                target: "probe".into(),
                payload: encode_query(&FilesQuery::Refs {}),
            },
        ),
    ];
    let n_out =
        block_on(native.submit_block(block(2, Origin::System), batch.clone())).expect("native");
    let w_out = block_on(wasm.submit_block(block(2, Origin::System), batch)).expect("wasm");
    for out in [&n_out, &w_out] {
        assert!(
            out.members
                .iter()
                .all(|m| matches!(m, MemberOutcome::Applied { .. })),
            "all members must apply: {:?}",
            out.members
        );
    }
    assert_eq!(
        all_roots(&native),
        all_roots(&wasm),
        "post-probe roots diverge"
    );
    // the probe committed the mid-block reply it saw — identical on both runtimes.
    assert_eq!(
        native.module_root("probe"),
        wasm.module_root("probe"),
        "mid-block committed-read replies diverge"
    );
    // and it was the COMMITTED refs (block 1), not the staged /late commit — the
    // committed-only lane, proven by matching the pre-block-2 committed image.
    assert_eq!(
        native.module_root("probe"),
        Some(StateRoot(Sha256::digest(&committed_refs).into())),
        "probe must serve committed-only refs, not the same-block staged commit"
    );
}

// ============================================================================
// CASE 8: gc after the history window slides — root stays equal, gc is neutral
// ============================================================================

/// drive past the real HISTORY_WINDOW (1024) so the bounded window slides, and
/// past the GC_PERIOD_BLOCKS (1024) boundary so gc actually fires at height 1024.
/// gc removes only unreachable objects (never touches refs), so the files root
/// must stay byte-identical to native across every block — including the gc block.
/// the window/gc caps are consensus constants (no wasm-side test seam), so this
/// exercises them at their real size.
#[test]
fn gc_after_window_slide_stays_root_equal() {
    let dir_n = tempfile::tempdir().unwrap();
    let dir_w = tempfile::tempdir().unwrap();
    let mut native = native_host(&dir_n);
    let mut wasm = wasm_host(&dir_w);

    // 1030 tiny commits: one new top-level dir per block (base=None, distinct
    // path, no CAS conflict). height 1..=1030 crosses the 1024 gc boundary; the
    // 1024-deep window slides after commit 1025. asserting roots every block is
    // cheap beside the commit; the gc block is not special-cased — it must stay
    // equal like every other.
    for height in 1..=1030u64 {
        let msg = commit_op(
            None,
            "w",
            vec![Change::Mkdir {
                path: format!("/g{height}"),
            }],
        );
        block_on(native.submit_at(block(height, Origin::System), msg.clone())).expect("native gc");
        block_on(wasm.submit_at(block(height, Origin::System), msg)).expect("wasm gc");
        // per-block check narrows to files_root: only files moves in this lane (no
        // watch fires, so recorder/probe stay put), and gc lives entirely inside
        // files — so files_root is the sole signal, and folding all roots every one
        // of 1030 blocks would only add cost. the full matrix is asserted once below.
        assert_eq!(
            files_root(&native),
            files_root(&wasm),
            "files root diverges at gc-lane block {height}"
        );
    }
    // final full-matrix equality after gc has run and the window has slid.
    assert_eq!(
        all_roots(&native),
        all_roots(&wasm),
        "post-gc roots diverge"
    );
    assert_eq!(replies(&native), replies(&wasm), "post-gc replies diverge");
}

#[test]
fn sync_handle_matches_native() {
    let dir_n = tempfile::tempdir().unwrap();
    let dir_w = tempfile::tempdir().unwrap();
    let native = Files::open(FILES, dir_n.path().to_path_buf()).expect("open native");
    let wasm = WasmModule::with_odb(
        FILES,
        FILES_WASM,
        Box::new(FilesOdbBacking::open(FILES, dir_w.path().to_path_buf()).expect("open backing")),
    )
    .expect("load component");

    assert_eq!(
        native.state_sync_handle().expect("native handle"),
        wasm.state_sync_handle().expect("wasm handle"),
        "sync handles diverge"
    );
    // genesis roots equal directly at the module level (both empty refs).
    assert_eq!(
        Module::root(&native),
        Module::root(&wasm),
        "genesis module roots diverge"
    );
}

// ============================================================================
// CASE 12: reopen from disk — roots still equal (durable-restart parity)
// ============================================================================

/// build committed state on both runtimes, DROP both hosts (releasing disk
/// handles), reopen fresh hosts over the SAME dirs, and assert the files roots are
/// still byte-identical (and unchanged from before the drop). native `Files::open`
/// re-adopts committed refs from its envelope; the wasm tenant re-adopts through a
/// reopened `FilesOdbBacking` — the same durable-restart path, byte-for-byte.
#[test]
fn reopen_preserves_equal_roots() {
    let dir_n = tempfile::tempdir().unwrap();
    let dir_w = tempfile::tempdir().unwrap();

    let c0 = vec![0x33u8; 1500];
    let ops: Vec<(u64, Origin, Msg)> = vec![
        (1, Origin::System, putblob_op(&c0)),
        (
            2,
            Origin::System,
            commit_op(
                None,
                "b2",
                vec![
                    put_chunks("/shared/f0", c0.len() as u64, &[chunk_hex(&c0)]),
                    put_inline("/shared/note.txt", b"hello inline"),
                ],
            ),
        ),
        (
            3,
            Origin::External(b"tester".to_vec()),
            commit_op(None, "b3", vec![put_inline("/shared/more", b"tail")]),
        ),
    ];

    let (before_n, before_w) = {
        let mut native = native_host(&dir_n);
        let mut wasm = wasm_host(&dir_w);
        for (height, origin, msg) in &ops {
            block_on(native.submit_at(block(*height, origin.clone()), msg.clone()))
                .expect("native");
            block_on(wasm.submit_at(block(*height, origin.clone()), msg.clone())).expect("wasm");
            assert_eq!(
                all_roots(&native),
                all_roots(&wasm),
                "pre-drop block {height} diverges"
            );
        }
        (all_roots(&native), all_roots(&wasm))
        // both hosts drop here, releasing the disk handles.
    };
    assert_eq!(before_n, before_w, "pre-drop roots must be equal");

    // reopen over the SAME dirs — genesis only registers the reopened modules.
    let native2 = native_host(&dir_n);
    let wasm2 = wasm_host(&dir_w);
    assert_eq!(
        files_root(&native2),
        before_n.iter().find(|(id, _)| id == FILES).unwrap().1,
        "native reopen must re-adopt the committed files root"
    );
    assert_eq!(
        files_root(&wasm2),
        files_root(&native2),
        "wasm reopen root must equal native reopen root"
    );
    assert_eq!(
        replies(&native2),
        replies(&wasm2),
        "post-reopen replies diverge"
    );
}

// ============================================================================
// MEASUREMENT HARNESS (issue #2238) — ignored by default
// ============================================================================
//
// These print a table instead of asserting a budget: they measure the cost of
// NORMAL Files work and the shape of the curve up to the rejection boundary.
// A correctness run stays a correctness run.
//
// `cargo test -p host --test wasm_files_parity -- --ignored --nocapture`
//
// Not measured here: guest fuel and the per-op object-read counter. Neither
// crosses the `Host` API, and instrumenting the kernel to export them is a
// change to the execution engine, not a measurement of it. An object read is
// answered inside its import (`wasm_host`'s module docs), so one dispatch runs
// the guest ONCE and issues one read call per read: the wall time below is
// linear in the objects an operation touches.

/// Resident and peak-resident kibibytes, or zeroes where procfs is absent.
fn measured_memory() -> (u64, u64) {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return (0, 0);
    };
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_default()
    };
    (field("VmRSS:"), field("VmHWM:"))
}

fn documents(count: usize, bytes: usize) -> Vec<Vec<u8>> {
    (0..count)
        .map(|index| {
            let mut body = format!("document {index:06}\n").into_bytes();
            body.resize(bytes, b'x');
            body
        })
        .collect()
}

fn import_changes(bodies: &[Vec<u8>]) -> Vec<Change> {
    bodies
        .iter()
        .enumerate()
        .map(|(index, body)| put_inline(&format!("/shared/project/docs/{index:06}.txt"), body))
        .collect()
}

/// The three phases the issue separates: component load and compile, the warm
/// dispatches that do the work, and the first query after a durable reopen.
#[test]
#[ignore = "measurement harness"]
fn files_cold_load_warm_dispatch_and_reopen_first_query() {
    println!("== baseline production workload: 128 documents of 1 KiB + one 2 MiB asset ==");
    println!("phase\truntime\tms\tdetail");
    for repeat in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        let cold = std::time::Instant::now();
        let mut wasm = wasm_host(&dir);
        let cold = cold.elapsed();
        println!(
            "cold_load\twasm\t{:.1}\trepeat {repeat}",
            cold.as_secs_f64() * 1000.0
        );

        let chunks = [
            vec![0x41; CHUNK_SIZE as usize],
            vec![0x42; CHUNK_SIZE as usize],
        ];
        let staged = std::time::Instant::now();
        for (index, chunk) in chunks.iter().enumerate() {
            block_on(wasm.submit_at(block(index as u64 + 1, Origin::System), putblob_op(chunk)))
                .unwrap();
        }
        println!(
            "putblob_2x1MiB\twasm\t{:.1}\trepeat {repeat}",
            staged.elapsed().as_secs_f64() * 1000.0
        );

        // 128 documents is the whole per-op object-read budget (a chunk and a
        // fileobj each), so the asset rides its own commit.
        let bodies = documents(128, 1024);
        let import = std::time::Instant::now();
        block_on(wasm.submit_at(
            block(3, Origin::System),
            commit_op(None, "import", import_changes(&bodies)),
        ))
        .unwrap();
        println!(
            "commit_import_128\twasm\t{:.1}\trepeat {repeat}",
            import.elapsed().as_secs_f64() * 1000.0
        );
        block_on(wasm.submit_at(
            block(4, Origin::System),
            commit_op(
                Some(&head(&wasm)),
                "asset",
                vec![put_chunks(
                    "/shared/project/asset.bin",
                    2 * CHUNK_SIZE,
                    &chunks.iter().map(|chunk| chunk_hex(chunk)).collect::<Vec<_>>(),
                )],
            ),
        ))
        .unwrap();

        let snapshot = head(&wasm);
        let edit = std::time::Instant::now();
        block_on(
            wasm.submit_at(
                block(5, Origin::System),
                commit_op(
                    Some(&snapshot),
                    "edit 32",
                    (0..32)
                        .map(|index| {
                            put_inline(&format!("/shared/project/docs/{index:06}.txt"), b"edited")
                        })
                        .collect(),
                ),
            ),
        )
        .unwrap();
        println!(
            "commit_edit_32\twasm\t{:.1}\trepeat {repeat}",
            edit.elapsed().as_secs_f64() * 1000.0
        );

        let root = files_root(&wasm);
        drop(wasm);
        let reopen = std::time::Instant::now();
        let wasm = wasm_host(&dir);
        let reopen = reopen.elapsed();
        assert_eq!(files_root(&wasm), root, "durable reopen preserves the root");
        println!(
            "reopen\twasm\t{:.1}\trepeat {repeat}",
            reopen.as_secs_f64() * 1000.0
        );

        let first = std::time::Instant::now();
        let query = FilesQuery::Read {
            path: "/shared/project/docs/000000.txt".into(),
            snapshot: None,
            offset: 0,
            len: MAX_READ_BYTES,
        };
        block_on(wasm.query(FILES, &encode_query(&query))).unwrap();
        println!(
            "first_read_after_reopen\twasm\t{:.3}\trepeat {repeat}",
            first.elapsed().as_secs_f64() * 1000.0
        );
        let warm = std::time::Instant::now();
        for _ in 0..128 {
            block_on(wasm.query(FILES, &encode_query(&query))).unwrap();
        }
        println!(
            "warm_read_mean\twasm\t{:.3}\trepeat {repeat}",
            warm.elapsed().as_secs_f64() * 1000.0 / 128.0
        );
        let (rss, peak) = measured_memory();
        println!("memory\twasm\t0.0\trss_kib={rss} peak_kib={peak}");
    }
}

/// Document count in ONE commit, held against everything else. Each distinct
/// inline body stages a chunk and a fileobj, so the op accrues two distinct
/// object reads per document against `MAX_OBJECT_READS_PER_OP`. This is the
/// curve that decides the documents-per-commit ceiling, and where on it the cap
/// refuses.
#[test]
#[ignore = "measurement harness"]
fn files_commit_cost_by_document_count() {
    // 64-byte bodies keep the whole sweep inside MAX_INLINE_COMMIT_BYTES, so
    // the object-read cap is the constraint the curve runs into, not the
    // unrelated inline-bytes budget.
    println!(
        "== one import commit, 64-byte documents, scaling document count (inline budget {} KiB, object-read cap {MAX_OBJECT_READS_PER_OP}) ==",
        MAX_INLINE_COMMIT_BYTES / 1024
    );
    println!(
        "documents\tobject_reads\tcap_used_pct\tnative_ms\twasm_ms\twasm_over_native\tms_per_doc\trss_kib\toutcome"
    );
    for count in [16usize, 32, 64, 128, 256, 512, 1024, 2048, 2080] {
        let dir_n = tempfile::tempdir().unwrap();
        let dir_w = tempfile::tempdir().unwrap();
        let mut native = native_host(&dir_n);
        let mut wasm = wasm_host(&dir_w);
        let genesis = all_roots(&wasm);
        let bodies = documents(count, 64);
        let operation = commit_op(None, "import", import_changes(&bodies));

        let start = std::time::Instant::now();
        let native_outcome =
            block_on(native.submit_at(block(1, Origin::System), operation.clone()));
        let native_ms = start.elapsed().as_secs_f64() * 1000.0;
        let start = std::time::Instant::now();
        let wasm_outcome = block_on(wasm.submit_at(block(1, Origin::System), operation));
        let wasm_ms = start.elapsed().as_secs_f64() * 1000.0;

        // Each runtime's refusal must leave its own roots where they were; that
        // safety invariant is asserted. Whether the two AGREE is a measurement
        // at this scale, so it is reported as a column rather than asserted —
        // the fixed-workload parity proofs above still assert agreement.
        if native_outcome.is_err() {
            assert_eq!(all_roots(&native), genesis, "native root moved on reject");
        }
        if wasm_outcome.is_err() {
            assert_eq!(all_roots(&wasm), genesis, "wasm root moved on reject");
        }
        let reason = |outcome: &Result<host::BlockOutcome, SubmitError>| match outcome {
            Ok(_) => "accepted".to_string(),
            Err(error) => format!("{error:?}")
                .replace('\n', " ")
                .chars()
                .take(48)
                .collect(),
        };
        let agreed = native_outcome.is_ok() == wasm_outcome.is_ok();
        let roots_equal = all_roots(&native) == all_roots(&wasm);
        let outcome = format!(
            "native={} wasm={} agree={agreed} roots_equal={roots_equal}",
            reason(&native_outcome),
            reason(&wasm_outcome)
        );
        let (rss, _) = measured_memory();
        println!(
            "{count}\t{}\t{}\t{native_ms:.1}\t{wasm_ms:.1}\t{:.2}\t{:.3}\t{rss}\t{outcome}",
            count * 2,
            count * 200 / MAX_OBJECT_READS_PER_OP,
            wasm_ms / native_ms.max(0.001),
            wasm_ms / count as f64
        );
    }
}

/// Individual object size and query result size, each moved alone against a
/// fixed document count — the dimensions that grow bytes rather than reads.
#[test]
#[ignore = "measurement harness"]
fn files_cost_by_object_size_and_query_result_size() {
    println!(
        "== 64 inline documents, scaling the bytes in each (inline budget {} KiB per commit) ==",
        MAX_INLINE_COMMIT_BYTES / 1024
    );
    println!(
        "doc_bytes\ttotal_kib\twasm_commit_ms\twasm_read_ms\tread_reply_bytes\trss_kib\toutcome"
    );
    for doc_bytes in [256usize, 1024, 4096, 8192] {
        let dir = tempfile::tempdir().unwrap();
        let mut wasm = wasm_host(&dir);
        let genesis = all_roots(&wasm);
        let bodies = documents(64, doc_bytes);
        let start = std::time::Instant::now();
        let committed = block_on(wasm.submit_at(
            block(1, Origin::System),
            commit_op(None, "import", import_changes(&bodies)),
        ));
        let commit_ms = start.elapsed().as_secs_f64() * 1000.0;
        let Ok(_) = committed else {
            // The budget refusal must leave the committed root exactly as it was.
            assert_eq!(all_roots(&wasm), genesis, "root moved on an inline refusal");
            println!(
                "{doc_bytes}\t{}\t{commit_ms:.1}\t0.000\t0\t{}\trejected:inline_commit_budget",
                64 * doc_bytes / 1024,
                measured_memory().0
            );
            continue;
        };
        let query = FilesQuery::Read {
            path: "/shared/project/docs/000000.txt".into(),
            snapshot: None,
            offset: 0,
            len: MAX_READ_BYTES,
        };
        let start = std::time::Instant::now();
        let reply = block_on(wasm.query(FILES, &encode_query(&query))).unwrap();
        let read_ms = start.elapsed().as_secs_f64() * 1000.0;
        let (rss, _) = measured_memory();
        println!(
            "{doc_bytes}\t{}\t{commit_ms:.1}\t{read_ms:.3}\t{}\t{rss}\taccepted",
            64 * doc_bytes / 1024,
            reply.len()
        );
    }

    // Large objects take the chunked path, which is what carries real assets
    // past the inline budget: one putblob per chunk, then one referencing commit.
    println!("== one chunked asset, scaling its size ==");
    println!("asset_mib\tchunks\tputblob_ms\tcommit_ms\tread_1mib_ms\trss_kib");
    for chunk_count in [1usize, 2, 4, 8] {
        let dir = tempfile::tempdir().unwrap();
        let mut wasm = wasm_host(&dir);
        let chunks: Vec<Vec<u8>> = (0..chunk_count)
            .map(|index| vec![0x41 + index as u8; CHUNK_SIZE as usize])
            .collect();
        let start = std::time::Instant::now();
        for (index, chunk) in chunks.iter().enumerate() {
            block_on(wasm.submit_at(block(index as u64 + 1, Origin::System), putblob_op(chunk)))
                .unwrap();
        }
        let putblob_ms = start.elapsed().as_secs_f64() * 1000.0;
        let change = put_chunks(
            "/shared/project/asset.bin",
            chunk_count as u64 * CHUNK_SIZE,
            &chunks
                .iter()
                .map(|chunk| chunk_hex(chunk))
                .collect::<Vec<_>>(),
        );
        let start = std::time::Instant::now();
        block_on(wasm.submit_at(
            block(chunk_count as u64 + 1, Origin::System),
            commit_op(None, "import asset", vec![change]),
        ))
        .unwrap();
        let commit_ms = start.elapsed().as_secs_f64() * 1000.0;
        let query = FilesQuery::Read {
            path: "/shared/project/asset.bin".into(),
            snapshot: None,
            offset: 0,
            len: CHUNK_SIZE,
        };
        let start = std::time::Instant::now();
        block_on(wasm.query(FILES, &encode_query(&query))).unwrap();
        let read_ms = start.elapsed().as_secs_f64() * 1000.0;
        println!(
            "{chunk_count}\t{chunk_count}\t{putblob_ms:.1}\t{commit_ms:.1}\t{read_ms:.3}\t{}",
            measured_memory().0
        );
    }

    println!("== one listing, scaling the entries it returns ==");
    println!("entries\tlimit\twasm_ls_ms\tls_reply_bytes\treturned");
    let dir = tempfile::tempdir().unwrap();
    let mut wasm = wasm_host(&dir);
    // 256 entries take four commits: one op gets MAX_OBJECT_READS_PER_OP reads,
    // each distinct document costs two of them (chunk + fileobj), and every
    // commit past the first also spends HEAD_SPINE_READS on the head snapshot
    // and the path trees it rewrites — so its bound is 126 documents and a batch
    // of cap/2 overruns the cap by exactly those four reads.
    let per_commit = MAX_OBJECT_READS_PER_OP / 4;
    let mut base: Option<String> = None;
    for (batch, first) in (0..256usize).step_by(per_commit).enumerate() {
        let changes = (first..first + per_commit)
            .map(|index| {
                let mut body = format!("document {index:06}\n").into_bytes();
                body.resize(256, b'x');
                put_inline(&format!("/shared/project/docs/{index:06}.txt"), &body)
            })
            .collect();
        block_on(wasm.submit_at(
            block(batch as u64 + 1, Origin::System),
            commit_op(base.as_deref(), "import", changes),
        ))
        .unwrap();
        base = Some(head(&wasm));
    }
    for limit in [16u64, 64, 128, 256] {
        let query = FilesQuery::Ls {
            path: "/shared/project/docs".into(),
            snapshot: None,
            after: None,
            limit,
        };
        let start = std::time::Instant::now();
        let reply = block_on(wasm.query(FILES, &encode_query(&query))).unwrap();
        let ls_ms = start.elapsed().as_secs_f64() * 1000.0;
        let FilesReply::Ls { entries, .. } = files::decode_reply(&reply).unwrap() else {
            panic!("listing reply");
        };
        println!(
            "256\t{limit}\t{ls_ms:.3}\t{}\t{}",
            reply.len(),
            entries.len()
        );
    }
}
