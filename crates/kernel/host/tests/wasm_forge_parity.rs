//! the ROOT-CONTINUITY proof for forge: the forge guest component over
//! `WasmModule::with_odb(ForgeOdbBacking)` and the native `Forge` module over
//! the same git substrate are BYTE-IDENTICAL block-by-block. forge's root is
//! the composition over born branches + tracker on BOTH runtimes — the cutover
//! changes the executor, not one committed byte — and this proof pins that:
//! the same op stream commits the identical forge root after EVERY block from
//! genesis, the same query replies (including the odb-reading browse lane),
//! the same rejections, the same committed-only mid-block sibling reads, the
//! same chat follow-ups, and snapshot containers that install across runtimes.
//!
//! packs ride a shared blob store on both sides, so materialization (the
//! host-side half of the wasm tenant, driven from the block's ref targets)
//! is under test too: the on-disk objects behind a pushed head must arrive on
//! the wasm substrate exactly as the native `commit_block` installs them.

use futures::executor::block_on;

use forge::testkit::{PackedCommit, history};
use forge::{
    Forge, ForgeMsg, ForgeOdbBacking, ForgeQuery, RefUpdate, ReviewVerdict, encode_msg,
    encode_query,
};
use host::{BlockContext, CapturePayloads, Host, MemberOutcome};
use sdk::{Ctx, Error, Module, ModuleId, Msg, Origin, StateRoot, StateSyncHandle};
use sha2::{Digest as _, Sha256};
use wasm_host::{CompiledModule, WasmModule};

const FORGE: &str = "forge";

/// the chain id BOTH runtimes are constructed with — natively as a
/// `with_chain_id` builder call, on the wasm side through the odb-served
/// genesis config (`crate::guest`'s `shape().config`, #1773).
const CHAIN_ID: &str = "test-chain";

/// forge's SYNC surface as the host publishes it at a boundary: one
/// self-contained container, never a resolver lane. independent of the disk
/// cohort (`block_durable_ids`), which forge is also in — the whole point of
/// the recovery fix is that neither answer implies the other.
fn sync_surface_is_snapshot_bytes(host: &Host) -> bool {
    let (snapshot, _) =
        host.capture_current_snapshot(0, CapturePayloads::All, || std::time::Duration::ZERO);
    snapshot
        .module(FORGE)
        .expect("forge is registered")
        .state_sync
        .has_snapshot_bytes()
}
const CHAT: &str = "chat";
const REPO: &str = "demo";

/// GENERATED artifact — built from the module crate's guest port by
/// guest-builder (`make wasm-modules`); committed so this proof is
/// self-contained; production loads its module artifact from the module set.
const FORGE_WASM: &[u8] = include_bytes!("fixtures/forge.component.wasm");

// ---- the two runtimes over their own git substrates ------------------------

/// a native `Forge` over `dir`, beside the siblings the parity matrix
/// needs: a [`Recorder`] registered as `chat` (the discussion-channel
/// follow-up target) and a [`QueryProbe`] (the mid-block committed-read
/// prober). genesis only REGISTERS, so a fresh dir starts at `StateRoot::ZERO`.
fn native_host(dir: &tempfile::TempDir, blobs: blobstore::BlobHandle) -> Host {
    let forge = Forge::with_blobs(FORGE, dir.path().join(FORGE), blobs)
        .expect("open native forge")
        .with_chat(CHAT)
        .with_attribution("attribution")
        .with_chain_id(CHAIN_ID);
    Host::genesis(vec![
        Box::new(forge),
        identity(),
        attribution(),
        Box::new(Recorder::new(CHAT)),
        Box::new(QueryProbe::new()),
    ])
    .expect("native genesis")
}

/// the wasm `forge` tenant: the forge guest over a `ForgeOdbBacking` on `dir`
/// — the exact `WasmModule::over_odb` composition `noded::compose` uses,
/// carrying the SAME chain id `noded::compose`'s `odb_genesis_config` would
/// resolve from the network's bindings — beside the SAME native siblings.
fn wasm_forge(dir: &tempfile::TempDir, blobs: blobstore::BlobHandle) -> WasmModule {
    let backing =
        ForgeOdbBacking::open(FORGE, dir.path().join(FORGE), blobs).expect("open odb backing");
    let config = sdk::genesis_config::encode_config(&[("chain_id", CHAIN_ID.as_bytes())]);
    CompiledModule::compile(FORGE_WASM)
        .expect("compile component")
        .over_odb(FORGE, Box::new(backing), Some(config))
        .expect("load component")
}

fn wasm_host(dir: &tempfile::TempDir, blobs: blobstore::BlobHandle) -> Host {
    Host::genesis(vec![
        Box::new(wasm_forge(dir, blobs)),
        identity(),
        attribution(),
        Box::new(Recorder::new(CHAT)),
        Box::new(QueryProbe::new()),
    ])
    .expect("wasm genesis")
}

fn identity() -> Box<dyn Module> {
    Box::new(identity::Identity::new(
        "identity",
        Box::new(sdk_testkit::MemStore::new()),
        CHAIN_ID.into(),
    ))
}

fn attribution() -> Box<dyn Module> {
    Box::new(attribution::AttributionModule::new(
        "attribution",
        Box::new(sdk_testkit::MemStore::new()),
    ))
}

/// the consensus context for one block: both runtimes must see the identical env.
fn block(height: u64, origin: Origin) -> BlockContext {
    BlockContext {
        height,
        consensus_time: 1_000 + height,
        origin,
    }
}

fn founder() -> Origin {
    Origin::External(vec![0xA1; 32])
}

fn another_member() -> Origin {
    Origin::External(vec![0xB2; 32])
}

// ---- root + reply comparison seams ------------------------------------------

fn all_roots(h: &Host) -> Vec<(ModuleId, StateRoot)> {
    h.module_roots()
}

fn forge_root(h: &Host) -> StateRoot {
    h.module_root(FORGE).expect("forge registered")
}

/// the read matrix: every query family, including the absent shapes and the
/// odb-reading browse lane (served host-side off the git substrate on the wasm
/// side). a query on a not-yet-existing item is a deterministic `Err`, so error
/// PARITY is as much the claim as reply parity.
fn replies(h: &Host) -> Vec<Vec<u8>> {
    let queries = [
        ForgeQuery::Head,
        ForgeQuery::HeadOf { repo: REPO.into() },
        ForgeQuery::HeadOf {
            repo: "absent".into(),
        },
        ForgeQuery::ListRepos,
        ForgeQuery::ListRefs { repo: REPO.into() },
        ForgeQuery::ListItems { repo: REPO.into() },
        ForgeQuery::GetItem {
            repo: REPO.into(),
            number: 1,
        },
        ForgeQuery::GetItem {
            repo: REPO.into(),
            number: 2,
        },
        ForgeQuery::GetItem {
            repo: REPO.into(),
            number: 9,
        },
        ForgeQuery::PrDiff {
            repo: REPO.into(),
            number: 2,
        },
        ForgeQuery::Tree {
            repo: REPO.into(),
            rev: String::new(),
            path: String::new(),
        },
        ForgeQuery::Blob {
            repo: REPO.into(),
            rev: String::new(),
            path: "README.md".into(),
        },
        ForgeQuery::BlobBytes {
            repo: REPO.into(),
            rev: String::new(),
            path: "feature.txt".into(),
            offset: 0,
            len: 64,
        },
    ];
    queries
        .iter()
        .map(|q| match block_on(h.query(FORGE, &encode_query(q))) {
            Ok(bytes) => bytes,
            Err(e) => format!("ERR:{e}").into_bytes(),
        })
        .collect()
}

fn assert_lockstep(native: &Host, wasm: &Host, at: &str) {
    assert_eq!(all_roots(native), all_roots(wasm), "roots diverge {at}");
    assert_eq!(replies(native), replies(wasm), "replies diverge {at}");
}

// ---- op builders ----------------------------------------------------------------

fn op(msg: &ForgeMsg) -> Msg {
    Msg {
        target: FORGE.into(),
        payload: encode_msg(msg),
    }
}

fn update(branch: &str, prev: Option<&PackedCommit>, new: Option<&PackedCommit>) -> RefUpdate {
    RefUpdate {
        ref_name: branch.into(),
        prev_oid: prev.map(|c| c.head.clone()),
        new_oid: new.map(|c| c.head.clone()),
    }
}

fn push(updates: Vec<RefUpdate>, pack: Option<[u8; 32]>) -> Msg {
    op(&ForgeMsg::PushRefs {
        repo: REPO.into(),
        updates,
        pack_digest: pack.map(|d| d.to_vec()),
        cert: None,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// drive one single-op block through BOTH hosts and require the same verdict.
/// a rejection's native reason must appear in the wasm reason — the wasm
/// runtime wraps the reason in its wit-error rendering, so containment (not
/// equality) is the cross-runtime claim.
fn submit_both(native: &mut Host, wasm: &mut Host, height: u64, origin: Origin, msg: Msg) -> bool {
    let n = block_on(native.submit_at(block(height, origin.clone()), msg.clone()));
    let w = block_on(wasm.submit_at(block(height, origin), msg));
    match (n, w) {
        (Ok(native), Ok(wasm)) => {
            assert_eq!(
                native.dispatches, wasm.dispatches,
                "dispatch bytes at block {height}"
            );
            true
        }
        (Err(n), Err(w)) => {
            assert_reason_contained(&reason_of(n), &reason_of(w), height);
            false
        }
        (n, w) => panic!("verdicts diverge at block {height}: native {n:?} vs wasm {w:?}"),
    }
}

/// Program calls persist the result bytes in dispatch receipts and agent
/// bindings. Native serde_json/preserve_order must not change those bytes.
#[test]
fn program_issue_and_pr_results_match_native_and_wasm() {
    use agent::{
        AgentModule, AgentMsg, Continuation, Decode, Predicate, Program, Siblings, Step, Value,
    };
    use commonware_cryptography::{Signer as _, ed25519::PrivateKey};

    fn call(operation: &str, fields: &[(&str, &str)], bind: &str) -> Step {
        Step::Call {
            module: FORGE.into(),
            msg: Value::Map(
                [(
                    operation.into(),
                    Value::Map(
                        fields
                            .iter()
                            .map(|(key, value)| ((*key).into(), Value::Text((*value).into())))
                            .collect(),
                    ),
                )]
                .into(),
            ),
            bind: bind.into(),
            decode: Decode::Text,
            on_failure: Continuation::Unhandled,
        }
    }

    let dir_n = tempfile::tempdir().unwrap();
    let dir_w = tempfile::tempdir().unwrap();
    let blobs_n = blobstore::BlobHandle::default();
    let blobs_w = blobstore::BlobHandle::default();
    let commits = history("program-results", &[(1, "README.md", "base\n", "birth")]);
    let commit = &commits[0];
    let pack = blobs_n.put_chunk(commit.pack.clone());
    assert_eq!(pack, blobs_w.put_chunk(commit.pack.clone()));
    let mut native = native_host(&dir_n, blobs_n);
    let mut wasm = wasm_host(&dir_w, blobs_w);
    for host in [&mut native, &mut wasm] {
        host.register(Box::new(dispatch::DispatchModule::new(
            "dispatch",
            "saga",
            "identity",
            Box::new(sdk_testkit::MemStore::new()),
        )));
        host.register(Box::new(AgentModule::new(
            "agent",
            Box::new(sdk_testkit::MemStore::new()),
            Siblings {
                identity: "identity".into(),
                attribution: "attribution".into(),
                dispatch: "dispatch".into(),
            },
        )));
    }
    let controller = Origin::External(PrivateKey::from_seed(1).public_key().as_ref().to_vec());
    assert!(submit_both(
        &mut native,
        &mut wasm,
        1,
        controller.clone(),
        Msg {
            target: "identity".into(),
            payload: identity::encode_msg(&identity::IdentityMsg::Create {
                name: "controller".into(),
                scheme: identity::KeyScheme::Ed25519,
            }),
        }
    ));
    assert!(submit_both(
        &mut native,
        &mut wasm,
        2,
        founder(),
        push(
            vec![
                update("dev", None, Some(commit)),
                update("feature", None, Some(commit))
            ],
            Some(pack),
        )
    ));
    let program = Program {
        steps: vec![
            Step::Branch {
                test: Predicate::Equals {
                    left: Value::Ref(vec!["change".into(), "source".into(), "module".into()]),
                    right: Value::Text("test-source".into()),
                },
                then: 1,
                or: 3,
            },
            call(
                "open_issue",
                &[("repo", REPO), ("title", "program issue"), ("body", "body")],
                "issue",
            ),
            call(
                "open_pr",
                &[
                    ("repo", REPO),
                    ("title", "program PR"),
                    ("body", "body"),
                    ("source_branch", "feature"),
                    ("target_branch", "dev"),
                ],
                "pr",
            ),
            Step::Finish,
        ],
    };
    assert!(submit_both(
        &mut native,
        &mut wasm,
        3,
        controller,
        Msg {
            target: "agent".into(),
            payload: agent::encode_msg(&AgentMsg::Provision {
                request_id: "forge-program".into(),
                name: "forge-program".into(),
                program
            }),
        }
    ));
    assert!(submit_both(
        &mut native,
        &mut wasm,
        4,
        Origin::Module("test-source".into()),
        Msg {
            target: "attribution".into(),
            payload: attribution::encode_msg(&attribution::AttributionMsg::Attribute {
                object: attribution::ObjectRef {
                    kind: "trigger".into(),
                    object: "open-items".into()
                },
                revision: 1,
                actor: attribution::Actor::Account(1),
                relations: vec![attribution::Relation {
                    recipient: 2,
                    reason: attribution::Reason::Mention,
                    detail: vec![]
                }],
                transfers: vec![],
            }),
        }
    ));

    let mut outputs = Vec::new();
    // Drive the deterministic delivery/call schedule, including completion
    // and the program's immediate stop on its own authorship attributions.
    for height in 5..=10 {
        let n = block_on(native.submit_block(block(height, Origin::System), vec![])).unwrap();
        let w = block_on(wasm.submit_block(block(height, Origin::System), vec![])).unwrap();
        assert_eq!(n.calls, w.calls, "call receipts at block {height}");
        assert_eq!(
            n.deliveries, w.deliveries,
            "delivery receipts at block {height}"
        );
        assert_eq!(
            all_roots(&native),
            all_roots(&wasm),
            "all committed roots at block {height}"
        );
        for call in n.calls {
            for dispatch in call
                .dispatches
                .into_iter()
                .filter(|entry| entry.module == FORGE)
            {
                assert_eq!(dispatch.origin, Origin::Program(2));
                outputs.push(dispatch.output.expect("forge call result"));
            }
        }
    }
    assert_eq!(
        outputs,
        vec![
            br#"{"number":1,"repo":"demo"}"#.to_vec(),
            br#"{"number":2,"repo":"demo"}"#.to_vec()
        ]
    );
    let query = agent::encode_query(&agent::AgentQuery::Invocation { account: 2, seq: 1 });
    let n = block_on(native.query("agent", &query)).unwrap();
    let w = block_on(wasm.query("agent", &query)).unwrap();
    assert_eq!(n, w, "persisted invocation response bytes");
    let agent::AgentReply::Invocation(Some(invocation)) = agent::decode_reply(&n).unwrap() else {
        panic!("the program handled the trigger");
    };
    assert!(matches!(invocation.status, agent::Status::Finished { .. }));
    for (bind, expected) in ["issue", "pr"].into_iter().zip(outputs) {
        let agent::CallResult::Applied { output, .. } =
            serde_json::from_value(invocation.bindings[bind].clone()).unwrap()
        else {
            panic!("the {bind} call applied");
        };
        assert_eq!(
            output,
            serde_json::Value::String(String::from_utf8(expected).unwrap())
        );
    }
}

fn reason_of(err: host::SubmitError) -> String {
    match err {
        host::SubmitError::Rejected(Error::Module(reason)) => reason,
        other => panic!("expected a module rejection, got {other:?}"),
    }
}

fn assert_reason_contained(native: &str, wasm: &str, height: u64) {
    // the wit rendering is a debug-escaped string: undo the quote escaping so
    // a reason that names a repo (`"demo"`) is comparable; a batch member's
    // reason additionally carries the `Module(..)` rendering on both sides,
    // so the native needle is its inner text.
    let wasm = wasm.replace("\\\"", "\"");
    let native = native
        .strip_prefix("Module(")
        .and_then(|inner| inner.strip_suffix(')'))
        .unwrap_or(native);
    assert!(
        wasm.contains(native),
        "rejections diverge at block {height}: native {native:?} vs wasm {wasm:?}"
    );
}

// ---- sibling modules (kept native on both hosts) ----------------------------

/// the discussion-channel follow-up target: appends every payload it receives
/// to its committed log, so its root MOVES exactly when forge emitted a chat
/// follow-up. registered as `chat` in BOTH hosts — a missing or different
/// emission on the wasm side diverges this module's root.
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

/// a mid-block query prober: `execute` host-routes its payload as a forge
/// query (`Ctx::query` → the committed-only lane) and STAGES the reply;
/// `commit_block` commits it, so the probe's root commits to the exact bytes
/// it saw mid-block. a divergent committed-only read diverges the probe roots.
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
        self.staged = Some(ctx.query(FORGE, &msg.payload).await?);
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

fn probe_op(q: &ForgeQuery) -> Msg {
    Msg {
        target: "probe".into(),
        payload: encode_query(q),
    }
}

// ============================================================================
// the full matrix, block-by-block root equality
// ============================================================================

/// pushes (birth, advance, multi-branch, delete), the tracker (issue, PR,
/// review, merge, edit, close), protected-branch + CAS rejections, a multi-op block
/// with a same-branch conflict, and a mid-block sibling read — driven through
/// BOTH runtimes, asserting the forge root is byte-identical after EVERY block
/// from genesis and every query reply matches.
#[test]
fn full_matrix_roots_identical_block_by_block() {
    let dir_n = tempfile::tempdir().unwrap();
    let dir_w = tempfile::tempdir().unwrap();
    // the SAME packs on both sides, in a store EACH: possession is node-local,
    // and materialize RELEASES a pack once its objects are in the odb — one
    // shared handle would let whichever substrate ran first take the bytes away
    // from the other. both must materialize the same objects.
    let blobs_n = blobstore::BlobHandle::default();
    let blobs_w = blobstore::BlobHandle::default();
    let commits = history(
        "parity",
        &[
            (1, "README.md", "hello\n", "birth"),
            (2, "README.md", "hello again\n", "advance"),
            (3, "feature.txt", "feature body\n", "feature"),
            (4, "merged.txt", "merged\n", "merge"),
        ],
    );
    let packs: Vec<[u8; 32]> = commits
        .iter()
        .map(|c| {
            let digest = blobs_n.put_chunk(c.pack.clone());
            assert_eq!(digest, blobs_w.put_chunk(c.pack.clone()));
            digest
        })
        .collect();
    let (c1, c2, c3, c4) = (&commits[0], &commits[1], &commits[2], &commits[3]);

    let mut native = native_host(&dir_n, blobs_n);
    let mut wasm = wasm_host(&dir_w, blobs_w);

    // ROOT CONTINUITY from block zero: both sides commit to the SAME empty
    // namespace, so the roots are EQUAL (and ZERO).
    assert_eq!(forge_root(&native), StateRoot::ZERO);
    assert_lockstep(&native, &wasm, "at genesis");
    // the host sees the wasm tenant exactly as it saw native forge, on BOTH
    // answers — which are independent, and conflating them is what left forge
    // unplaceable at restart. SYNC SURFACE: snapshot bytes, one self-contained
    // container, never a resolver lane.
    assert!(sync_surface_is_snapshot_bytes(&native), "native forge");
    assert!(sync_surface_is_snapshot_bytes(&wasm), "wasm forge");
    // DURABILITY: per-block on its own disk, so it is in recovery's disk
    // cohort on both sides despite that snapshot-shaped surface.
    assert!(native.block_durable_ids().contains(FORGE));
    assert!(wasm.block_durable_ids().contains(FORGE));

    // the accepted stream — every block here moves the forge root.
    let accepted: Vec<(u64, Origin, Msg)> = vec![
        // birth: main at c1.
        (
            1,
            founder(),
            push(vec![update("main", None, Some(c1))], Some(packs[0])),
        ),
        // advance main c1 → c2.
        (
            2,
            founder(),
            push(vec![update("main", Some(c1), Some(c2))], Some(packs[1])),
        ),
        // one atomic multi-branch push: dev born at c2, feature born at c3.
        (
            3,
            founder(),
            push(
                vec![
                    update("dev", None, Some(c2)),
                    update("feature", None, Some(c3)),
                ],
                Some(packs[2]),
            ),
        ),
        // the tracker: an issue (#1) — emits the discussion-channel follow-up.
        (
            4,
            another_member(),
            op(&ForgeMsg::OpenIssue {
                repo: REPO.into(),
                title: "first issue".into(),
                body: "body".into(),
            }),
        ),
        // a PR (#2) feature → dev.
        (
            5,
            another_member(),
            op(&ForgeMsg::OpenPr {
                repo: REPO.into(),
                title: "review me".into(),
                body: String::new(),
                source_branch: "feature".into(),
                target_branch: "dev".into(),
            }),
        ),
        // a review on #2.
        (
            6,
            founder(),
            op(&ForgeMsg::SubmitReview {
                repo: REPO.into(),
                number: 2,
                verdict: ReviewVerdict::Approve,
                body: "lgtm".into(),
                commit_oid: hex(&c3.head),
                comments: Vec::new(),
            }),
        ),
        // the founder merges #2: dev c2 → c4 (the client-computed merge).
        (
            7,
            founder(),
            op(&ForgeMsg::MergePr {
                repo: REPO.into(),
                number: 2,
                prev_target_oid: hex(&c2.head),
                expected_source_oid: hex(&c3.head),
                merge_oid: hex(&c4.head),
                pack_digest: hex(&packs[3]),
            }),
        ),
        // any member edits + closes the issue.
        (
            8,
            another_member(),
            op(&ForgeMsg::EditItem {
                repo: REPO.into(),
                number: 1,
                title: Some("first issue, retitled".into()),
                body: None,
            }),
        ),
        (
            9,
            another_member(),
            op(&ForgeMsg::SetItemState {
                repo: REPO.into(),
                number: 1,
                open: false,
            }),
        ),
        // delete the merged feature branch (object-free: no pack).
        (
            10,
            founder(),
            push(vec![update("feature", Some(c3), None)], None),
        ),
    ];
    for (height, origin, msg) in accepted {
        let before = forge_root(&native);
        assert!(
            submit_both(&mut native, &mut wasm, height, origin, msg),
            "block {height} must apply"
        );
        assert_ne!(forge_root(&native), before, "forge root stuck at {height}");
        assert_lockstep(&native, &wasm, &format!("after block {height}"));
    }

    // the REJECTION matrix: every verdict identical, no root movement.
    let rejected: Vec<(u64, Origin, Msg)> = vec![
        // deleting a protected branch.
        (
            11,
            another_member(),
            push(vec![update("main", Some(c2), None)], None),
        ),
        // a stale CAS on main.
        (
            12,
            founder(),
            push(vec![update("main", Some(c1), Some(c3))], Some(packs[2])),
        ),
        // re-merging a merged PR.
        (
            13,
            founder(),
            op(&ForgeMsg::MergePr {
                repo: REPO.into(),
                number: 2,
                prev_target_oid: hex(&c4.head),
                expected_source_oid: hex(&c3.head),
                merge_oid: hex(&c4.head),
                pack_digest: hex(&packs[3]),
            }),
        ),
        // a PR from a deleted (unborn) branch.
        (
            14,
            founder(),
            op(&ForgeMsg::OpenPr {
                repo: REPO.into(),
                title: "dangling".into(),
                body: String::new(),
                source_branch: "feature".into(),
                target_branch: "dev".into(),
            }),
        ),
    ];
    for (height, origin, msg) in rejected {
        let before = forge_root(&native);
        assert!(
            !submit_both(&mut native, &mut wasm, height, origin, msg),
            "block {height} must reject"
        );
        assert_eq!(
            forge_root(&native),
            before,
            "a rejection moved the root at {height}"
        );
        assert_lockstep(&native, &wasm, &format!("after rejected block {height}"));
    }

    // a MULTI-OP block: the block-scratch lane. member 0 re-births feature at
    // c3, member 1 moves main c2 → c3 (a different branch, applies), member 2
    // moves feature AGAIN in the same block (one fate per branch per block —
    // rejected), member 3 reads dev's head mid-block through the probe
    // (committed-only: c4 on both sides).
    let ops = vec![
        (
            founder(),
            push(vec![update("feature", None, Some(c3))], Some(packs[2])),
        ),
        (
            founder(),
            push(vec![update("main", Some(c2), Some(c3))], Some(packs[2])),
        ),
        (
            founder(),
            push(vec![update("feature", Some(c3), Some(c4))], Some(packs[3])),
        ),
        (
            founder(),
            probe_op(&ForgeQuery::HeadOf { repo: REPO.into() }),
        ),
    ];
    let n = block_on(native.submit_block(block(16, founder()), ops.clone())).expect("native block");
    let w = block_on(wasm.submit_block(block(16, founder()), ops)).expect("wasm block");
    for (i, (n, w)) in n.members.iter().zip(w.members.iter()).enumerate() {
        match (n, w) {
            (MemberOutcome::Applied { .. }, MemberOutcome::Applied { .. }) => {}
            (MemberOutcome::Rejected { reason: rn }, MemberOutcome::Rejected { reason: rw }) => {
                assert_reason_contained(rn, rw, 16 + i as u64);
            }
            (n, w) => panic!("member {i} verdicts diverge: native {n:?} vs wasm {w:?}"),
        }
    }
    assert!(matches!(n.members[0], MemberOutcome::Applied { .. }));
    assert!(matches!(n.members[1], MemberOutcome::Applied { .. }));
    assert!(matches!(n.members[2], MemberOutcome::Rejected { .. }));
    assert!(matches!(n.members[3], MemberOutcome::Applied { .. }));
    assert_lockstep(&native, &wasm, "after the multi-op block");

    // the browse lane read REAL objects: every pushed head materialized on
    // both substrates (the wasm side through the block's ref targets).
    let tree = block_on(native.query(
        FORGE,
        &encode_query(&ForgeQuery::Tree {
            repo: REPO.into(),
            rev: String::new(),
            path: String::new(),
        }),
    ))
    .expect("tree");
    let listing = String::from_utf8(tree).unwrap();
    assert!(
        listing.contains("merged.txt"),
        "dev head not materialized: {listing}"
    );

    // SNAPSHOT INTERCHANGE: the wasm tenant's container installs into a fresh
    // native module and the native container into a fresh wasm tenant, each
    // landing at the shared root with identical replies.
    let (n_snap, _) =
        native.capture_current_snapshot(16, CapturePayloads::All, || std::time::Duration::ZERO);
    let (w_snap, _) =
        wasm.capture_current_snapshot(16, CapturePayloads::All, || std::time::Duration::ZERO);
    let root = forge_root(&native);
    let n_bytes = snapshot_bytes(&n_snap.module(FORGE).expect("forge entry").state_sync);
    let w_bytes = snapshot_bytes(&w_snap.module(FORGE).expect("forge entry").state_sync);
    let empty = blobstore::BlobHandle::default();

    let dir_fresh_n = tempfile::tempdir().unwrap();
    let mut fresh_native = Forge::with_blobs(FORGE, dir_fresh_n.path().join(FORGE), empty.clone())
        .expect("fresh native");
    fresh_native
        .install(&w_bytes, root)
        .expect("the wasm container installs natively");
    assert_eq!(fresh_native.root(), root);

    let dir_fresh_w = tempfile::tempdir().unwrap();
    let mut fresh_wasm = wasm_forge(&dir_fresh_w, empty);
    fresh_wasm
        .install(&n_bytes, root)
        .expect("the native container installs into the wasm tenant");
    assert_eq!(fresh_wasm.root(), root);
    let q = encode_query(&ForgeQuery::ListRefs { repo: REPO.into() });
    assert_eq!(
        block_on(fresh_native.query(&q)).expect("native refs"),
        block_on(fresh_wasm.query(&q)).expect("wasm refs"),
        "replies diverge over the installed snapshots"
    );
    // a container under the wrong root is refused whole.
    let mut bogus = wasm_forge(
        &tempfile::tempdir().unwrap(),
        blobstore::BlobHandle::default(),
    );
    assert!(bogus.install(&n_bytes, StateRoot::ZERO).is_err());
    assert_eq!(bogus.root(), StateRoot::ZERO);

    // REOPEN: the wasm substrate re-adopts its on-disk state at the same root.
    drop(wasm);
    let reopened = wasm_host(&dir_w, blobstore::BlobHandle::default());
    assert_eq!(
        forge_root(&reopened),
        root,
        "the reopened wasm tenant rewound"
    );
    assert_eq!(replies(&native), replies(&reopened));
}

fn snapshot_bytes(handle: &StateSyncHandle) -> Vec<u8> {
    match handle {
        StateSyncHandle::SnapshotBytes(b) => b.clone(),
        other => panic!("expected snapshot bytes, got {other:?}"),
    }
}

// ============================================================================
// pack possession is per-node, root is not
// ============================================================================

/// a wasm tenant whose blob store LACKS the pack commits the SAME root as one
/// that holds it (the fork-safety invariant), records the head as a catch-up
/// target, and materializes it once the pack arrives on a later forge block.
#[test]
fn a_wasm_tenant_without_the_pack_reaches_the_same_root_then_catches_up() {
    let dir_full = tempfile::tempdir().unwrap();
    let dir_bare = tempfile::tempdir().unwrap();
    let full = blobstore::BlobHandle::default();
    let bare = blobstore::BlobHandle::default();
    let commits = history("possession", &[(1, "README.md", "hello\n", "birth")]);
    let c1 = &commits[0];
    let pack = full.put_chunk(c1.pack.clone());

    let mut with_pack = wasm_host(&dir_full, full);
    let mut without = wasm_host(&dir_bare, bare.clone());
    let birth = push(vec![update("main", None, Some(c1))], Some(pack));
    block_on(with_pack.submit_at(block(1, founder()), birth.clone())).expect("with pack");
    block_on(without.submit_at(block(1, founder()), birth)).expect("without pack");
    assert_eq!(
        forge_root(&with_pack),
        forge_root(&without),
        "pack possession leaked into the root"
    );
    assert_ne!(forge_root(&without), StateRoot::ZERO);

    // the catch-up map is the node's pull handle — readable without the module.
    // it names the pack to pull AND the head that pack explains.
    let outstanding = forge::pending_branches(&dir_bare.path().join(FORGE)).unwrap();
    assert_eq!(
        outstanding
            .iter()
            .map(|p| (p.digest, p.head.as_bytes().to_vec()))
            .collect::<Vec<_>>(),
        vec![(pack, c1.head.clone())]
    );
    assert!(
        forge::pending_branches(&dir_full.path().join(FORGE))
            .unwrap()
            .is_empty()
    );
    let tree = ForgeQuery::Tree {
        repo: REPO.into(),
        rev: String::new(),
        path: String::new(),
    };
    let not_yet = block_on(without.query(FORGE, &encode_query(&tree)));
    let materialized = block_on(with_pack.query(FORGE, &encode_query(&tree))).expect("tree");
    assert_ne!(
        not_yet.ok(),
        Some(materialized.clone()),
        "objects appeared from nowhere"
    );

    // the pack arrives out of band; the next forge block catches the ref up.
    bare.put_chunk(c1.pack.clone());
    let touch = op(&ForgeMsg::OpenIssue {
        repo: REPO.into(),
        title: "nudge".into(),
        body: String::new(),
    });
    block_on(without.submit_at(block(2, founder()), touch)).expect("nudge");
    assert!(
        forge::pending_branches(&dir_bare.path().join(FORGE))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        block_on(without.query(FORGE, &encode_query(&tree))).expect("tree"),
        materialized,
        "the caught-up substrate serves the same tree"
    );
}

// ============================================================================
// #1773 — the push-certificate chain check, on both runtimes
// ============================================================================

/// a `git push --signed` certificate is checked against THIS network's chain
/// id identically whether forge runs native or wasm: a certificate minted for
/// a different chain is refused on both, and one minted for this network's
/// own id is accepted on both — proof the odb-served genesis config actually
/// reaches [`forge::pushcert::signer`] through the guest, not just the root.
#[test]
fn a_push_certificate_checks_the_chain_id_identically_on_both_runtimes() {
    use forge::PushCert;
    use keyscheme::sshsig::GIT_SSH_NS;
    use keyscheme::testkit::{ssh_key, sshsig};

    let dir_n = tempfile::tempdir().unwrap();
    let dir_w = tempfile::tempdir().unwrap();
    let mut native = native_host(&dir_n, blobstore::BlobHandle::default());
    let mut wasm = wasm_host(&dir_w, blobstore::BlobHandle::default());

    let sk = ssh_key(1);
    let certified = |chain_id: &str| {
        let updates = vec![RefUpdate {
            ref_name: "main".into(),
            prev_oid: None,
            new_oid: Some(vec![7u8; 20]),
        }];
        let cert_text =
            forge::pushcert::certificate(&forge::pushcert::nonce(chain_id, REPO), &updates);
        op(&ForgeMsg::PushRefs {
            repo: REPO.into(),
            updates,
            pack_digest: Some(vec![9u8; 32]),
            cert: Some(PushCert {
                sshsig: sshsig(&sk, GIT_SSH_NS, &cert_text),
                cert: cert_text,
            }),
        })
    };

    // a certificate minted for a DIFFERENT chain is refused on both runtimes.
    let rejected = certified("some-other-chain");
    assert!(!submit_both(
        &mut native,
        &mut wasm,
        1,
        another_member(),
        rejected
    ));

    // this network's own chain id is accepted on both.
    let accepted = certified(CHAIN_ID);
    assert!(submit_both(
        &mut native,
        &mut wasm,
        2,
        another_member(),
        accepted
    ));
}
