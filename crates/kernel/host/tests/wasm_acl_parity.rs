//! the STORE-BACKED cutover-continuity proof for acl: the `acl` guest
//! component over `WasmModule::with_store(QmdbStore)` and the native `Acl`
//! over the same store shape are ROOT-CONTINUOUS — the same op sequence
//! commits the IDENTICAL qmdb merkle root after every block (both roots ARE
//! the store's root). acl carries no per-network genesis config, so both
//! stores start empty (= allow-all) and the genesis roots are already equal.
//!
//! acl's read surface is CONSENSUS-CRITICAL: the kernel drain consults
//! `AclQuery::PolicyFor` before every external op reaches its target, through
//! the ordinary module query lane this proof exercises. byte-identical
//! `PolicyFor` replies across the runtimes therefore ARE the gate-parity
//! claim — the drain decides from nothing else.

use acl_module::{
    Acl, AclMsg, AclQuery, MAX_TARGET_LEN, Standing, WILDCARD_TARGET, encode_msg, encode_query,
};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use host::{BlockContext, Host, MemberOutcome, SubmitError};
use sdk::{Error, Msg, Origin, StateRoot};
use statesync::qmdb::QmdbStore;
use wasm_host::WasmModule;

/// GENERATED artifact — built from the `acl` module's guest port by
/// guest-builder (`make wasm-modules`); committed so this proof is self-contained.
const ACL_WASM: &[u8] = include_bytes!("fixtures/acl.component.wasm");

/// a fresh qmdb store. `label` doubles as the store id (the deterministic
/// runtime keys storage partitions by id alone).
async fn acl_store(
    context: &deterministic::Context,
    label: &'static str,
) -> QmdbStore<deterministic::Context> {
    QmdbStore::init(context.child(label), label).await
}

fn wasm_acl(store: Box<dyn sdk::MerkleStore>) -> WasmModule {
    WasmModule::with_store("acl", ACL_WASM, store).expect("load component")
}

async fn native_host(context: &deterministic::Context) -> Host {
    let store = acl_store(context, "native_acl").await;
    Host::genesis(vec![Box::new(Acl::new(
        "acl",
        Box::new(store),
        "governance",
    ))])
    .expect("genesis")
}

async fn wasm_host_(context: &deterministic::Context) -> Host {
    let store = acl_store(context, "wasm_acl").await;
    Host::genesis(vec![Box::new(wasm_acl(Box::new(store)))]).expect("genesis")
}

fn set(target: &str, standing: Option<Standing>) -> Msg {
    Msg {
        target: "acl".into(),
        payload: encode_msg(&AclMsg::SetPolicy {
            target: target.into(),
            standing,
        }),
    }
}

fn gov() -> Origin {
    Origin::Module("governance".into())
}

/// one block's agreed context: both runtimes must see the identical env.
fn block(height: u64, origin: Origin) -> BlockContext {
    BlockContext {
        height,
        consensus_time: 1_000 + height,
        origin,
    }
}

fn root_of(h: &Host) -> StateRoot {
    h.module_root("acl").expect("acl registered")
}

/// the read matrix: the full table plus the drain gate's exact per-target
/// read (`PolicyFor`), including the wildcard fallback and the open default.
async fn replies(h: &Host) -> Vec<Vec<u8>> {
    let mut queries = vec![encode_query(&AclQuery::Policy)];
    for target in ["valset", "chat", "pages", WILDCARD_TARGET, "absent"] {
        queries.push(encode_query(&AclQuery::PolicyFor {
            target: target.into(),
        }));
    }
    let mut out = Vec::new();
    for q in queries {
        out.push(h.query("acl", &q).await.expect("acl query"));
    }
    out
}

#[test]
fn same_ops_same_policy_roots_in_lockstep_and_continuous() {
    deterministic::Runner::default().start(|context| async move {
        same_ops_inner(&context).await;
    });
}

async fn same_ops_inner(context: &deterministic::Context) {
    let mut native = native_host(context).await;
    let mut wasm = wasm_host_(context).await;

    // ROOT-CONTINUITY from GENESIS: both roots are the (empty) store's merkle
    // root — the allow-all shape — identical across the runtimes.
    let genesis_root = root_of(&native);
    assert_eq!(
        genesis_root,
        root_of(&wasm),
        "genesis roots must be continuous across the runtimes"
    );

    // every op family, in one deterministic sequence. `moves` says whether the
    // op changes committed state — root movement must agree on BOTH sides.
    let ops: Vec<(Origin, Msg, bool)> = vec![
        // h1: governance tightens one target.
        (gov(), set("valset", Some(Standing::Validator)), true),
        // h2: an idempotent re-set stages nothing.
        (gov(), set("valset", Some(Standing::Validator)), false),
        // h3: the wildcard fallback entry.
        (gov(), set(WILDCARD_TARGET, Some(Standing::User)), true),
        // h4: an update in place (same target, different standing).
        (gov(), set("valset", Some(Standing::Node)), true),
        // h5: SYSTEM origin is the other authorized author.
        (Origin::System, set("chat", Some(Standing::Open)), true),
        // h6: clearing an absent entry is a documented staged no-op.
        (gov(), set("ghost", None), false),
        // h7: clearing a present entry moves the root.
        (gov(), set("chat", None), true),
    ];
    for (height, (origin, msg, moves)) in ops.into_iter().enumerate() {
        let height = height as u64 + 1;
        let (n_before, w_before) = (root_of(&native), root_of(&wasm));
        native
            .submit_at(block(height, origin.clone()), msg.clone())
            .await
            .expect("native submit");
        wasm.submit_at(block(height, origin), msg)
            .await
            .expect("wasm submit");
        // the read matrix — the drain gate's decision inputs — is identical
        // after every block.
        assert_eq!(
            replies(&native).await,
            replies(&wasm).await,
            "policy replies diverge after block {height}"
        );
        // roots move in LOCKSTEP: a state-changing op moves both commit
        // boundaries, a staged no-op holds both...
        if moves {
            assert_ne!(root_of(&native), n_before, "native root stuck at {height}");
            assert_ne!(root_of(&wasm), w_before, "wasm root stuck at {height}");
        } else {
            assert_eq!(root_of(&native), n_before, "native root moved at {height}");
            assert_eq!(root_of(&wasm), w_before, "wasm root moved at {height}");
        }
        // THE continuity property: both roots ARE the same store root.
        assert_eq!(
            root_of(&native),
            root_of(&wasm),
            "the two runtimes diverged at {height}"
        );
    }
}

#[test]
fn rejections_match_and_leave_no_trace() {
    deterministic::Runner::default().start(|context| async move {
        rejections_inner(&context).await;
    });
}

async fn rejections_inner(context: &deterministic::Context) {
    let mut native = native_host(context).await;
    let mut wasm = wasm_host_(context).await;

    // the rejection matrix: the governance-only origin gate, every target-
    // shape violation, and undecodable bytes. each rejected block must leave
    // BOTH roots byte-identical (the abort path: staged writes discarded).
    // (the MAX_POLICY_ENTRIES cap rejection needs a 256-entry fill and is
    // pinned by the native crate's own tests; the cap check itself is compiled
    // into the guest unchanged.)
    let rejects: Vec<(Origin, Msg, &str)> = vec![
        (
            Origin::External(vec![7u8; 32]),
            set("chat", Some(Standing::Validator)),
            "only via governance",
        ),
        (gov(), set("", Some(Standing::Open)), "non-empty"),
        (gov(), set(" chat", Some(Standing::Open)), "untrimmed"),
        (
            gov(),
            set(&"x".repeat(MAX_TARGET_LEN + 1), Some(Standing::Open)),
            "exceeds",
        ),
        (
            gov(),
            Msg {
                target: "acl".into(),
                payload: b"definitely-not-json".to_vec(),
            },
            "expected value",
        ),
    ];
    for (i, (origin, msg, needle)) in rejects.into_iter().enumerate() {
        let height = i as u64 + 1;
        let (n_before, w_before) = (root_of(&native), root_of(&wasm));
        let n_err = native
            .submit_at(block(height, origin.clone()), msg.clone())
            .await
            .expect_err("native must reject");
        let w_err = wasm
            .submit_at(block(height, origin), msg)
            .await
            .expect_err("wasm must reject");
        // both reject DETERMINISTICALLY with the native module's reason. the
        // wasm runtime wraps the reason in its wit-error rendering, so the
        // parity claim is containment, not string equality.
        let SubmitError::Rejected(Error::Module(n_msg)) = n_err else {
            panic!("native rejection shape: {n_err:?}");
        };
        let SubmitError::Rejected(Error::Module(w_msg)) = w_err else {
            panic!("wasm rejection shape: {w_err:?}");
        };
        assert!(n_msg.contains(needle), "native reason: {n_msg}");
        assert!(
            w_msg.contains(needle),
            "wasm reason must carry the native reason: {w_msg}"
        );
        assert_eq!(root_of(&native), n_before, "native root moved on reject");
        assert_eq!(root_of(&wasm), w_before, "wasm root moved on reject");
        assert_eq!(replies(&native).await, replies(&wasm).await);
    }
}

#[test]
fn multi_dispatch_block_reads_prior_staged_writes() {
    deterministic::Runner::default().start(|context| async move {
        multi_dispatch_inner(&context).await;
    });
}

async fn multi_dispatch_inner(context: &deterministic::Context) {
    let mut native = native_host(context).await;
    let mut wasm = wasm_host_(context).await;

    // ONE block, two ops: the clear only lands because the table read sees the
    // SAME block's staged entry — on the wasm side that is the outer staged
    // overlay being reloaded by the second dispatch. a staged-read failure
    // would leave the clear a documented no-op-on-absent and the entry would
    // survive the commit, so the COMMITTED table being empty IS the
    // read-your-writes claim. (the store root still moves — a qmdb root is an
    // op-log root, so an emptied record set does not rewind it — identically
    // on both runtimes.)
    let batch = vec![
        (gov(), set("chat", Some(Standing::User))),
        (gov(), set("chat", None)),
    ];
    let n_out = native
        .submit_block(block(1, gov()), batch.clone())
        .await
        .expect("native block");
    let w_out = wasm
        .submit_block(block(1, gov()), batch)
        .await
        .expect("wasm block");
    for out in [&n_out, &w_out] {
        assert!(
            out.members
                .iter()
                .all(|m| matches!(m, MemberOutcome::Applied { .. })),
            "both members must apply: {:?}",
            out.members
        );
    }
    for host in [&native, &wasm] {
        let reply = host
            .query("acl", &encode_query(&AclQuery::Policy))
            .await
            .expect("policy query");
        let acl::AclReply::Policy(table) = acl::decode_reply(&reply).expect("policy decodes")
        else {
            panic!("expected a Policy reply");
        };
        assert!(
            table.is_empty(),
            "the same-block clear must land: staged reads are broken"
        );
    }
    assert_eq!(
        root_of(&native),
        root_of(&wasm),
        "continuity after the batch"
    );
    assert_eq!(replies(&native).await, replies(&wasm).await);
}
