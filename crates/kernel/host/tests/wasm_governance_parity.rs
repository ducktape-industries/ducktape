//! the STORE-BACKED cutover-continuity proof for governance: the `governance`
//! guest component over `WasmModule::with_store(QmdbStore)` and the native
//! `Governance` over the same store shape are ROOT-CONTINUOUS — the same op
//! sequence commits the IDENTICAL qmdb merkle root after every block (both
//! roots ARE the store's root; qmdb's batch canonicalizes mutations by hashed
//! key, so the native logical-key commit order and the wasm hashed-key drain
//! order produce the same op log).
//!
//! governance's per-network parameter is the INVITE BINDING, which travels as
//! GENESIS CONFIG — a `__config` RECORD seeded into the qmdb store under
//! `sdk::store_key` (the production `seed_store_config` seam), read back by
//! the guest's `load_store_config` per dispatch. BOTH runtimes' stores carry
//! the identical record here (root-continuity demands it; the native twin
//! reads its binding from the builder and simply carries the record in its
//! root). the config-in-the-root and config-governs-the-guest pins ride at
//! the end of this file.
//!
//! governance is the richest tenant the runtime hosts: proposals, ballots and
//! the deterministic tally read the valset sibling; account-share mode reads
//! the identity sibling; and a PASSING proposal acts by EMITTING follow-up
//! msgs the host drains in the same block — into valset (membership) and
//! the modules registry (wasm code swaps). the wasm host carries the REAL NATIVE
//! siblings, so every read resolves through the runtime's memoized replay and
//! every follow-up re-publishes through the host ctx — including the
//! UpdateModule flow proving a WASM governance can still drive the code
//! registry that live-updates the other wasm tenants.

use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::PrivateKey;
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use governance::invite::{INVITE_GRANT_NAMESPACE, INVITE_NONCE_LEN, InviteToken, sign_join_proof};
use governance::{
    GovAction, GovMsg, GovQuery, Governance, ShareAllocation, encode_msg, encode_query,
};
use host::{BlockContext, Host, MemberOutcome, SubmitError};
use identity::{Identity, IdentityMsg, KeyScheme, account_principal};
use modules::{
    Modules, ModulesQuery, ModulesReply, decode_reply as modules_decode_reply,
    encode_query as modules_encode_query,
};
use sdk::{Error, MerkleStore as _, Msg, Origin, StateRoot};
use statesync::qmdb::QmdbStore;
use valset::{
    Valset, ValsetQuery, ValsetReply, decode_reply as valset_decode_reply,
    encode_query as valset_encode_query,
};
use wasm_host::WasmModule;

/// GENERATED artifact — built from the `governance` module's guest port by
/// guest-builder (`make wasm-modules`); committed so this proof is self-contained.
const GOVERNANCE_WASM: &[u8] = include_bytes!("fixtures/governance.component.wasm");

/// the network binding invite tokens sign over — BOTH runtimes are wired with
/// it (natively via `with_invite_binding`, on the wasm side as genesis config).
const INVITE: &[u8] = b"parity-net#feedface";

/// the chain id the identity SIBLING runs under (identity is native on both
/// hosts here; governance itself carries no chain id).
const CHAIN_ID: &str = "test-chain";

/// the account numbers the two founders receive, in founding order.
const ACCOUNT_A: u64 = 1;
const ACCOUNT_B: u64 = 2;

/// a fresh qmdb store carrying the seeded `__config` invite-binding record —
/// exactly the production genesis seam (`bin/node/src/host_state.rs`
/// `seed_store_config`). BOTH runtimes' stores get the identical record:
/// root-continuity demands it, and the guest reads its binding from it.
/// `label` doubles as the store id: the deterministic runtime keys storage
/// partitions by id alone (child labels do not namespace them), so a shared
/// id would make the second store REPLAY the first's journal and re-append
/// the config. the id is not part of the qmdb root (the sync tests pin
/// root-equality across ids), so distinct ids cost nothing.
async fn gov_store(
    context: &deterministic::Context,
    label: &'static str,
    invite: &[u8],
) -> QmdbStore<deterministic::Context> {
    let mut store = QmdbStore::init(context.child(label), label).await;
    let config = sdk::genesis_config::encode_config(&[("invite", invite)]);
    store
        .commit_batch(vec![(
            sdk::store_key(sdk::genesis_config::CONFIG_KEY),
            Some(config),
        )])
        .await
        .expect("seed genesis config");
    store
}

/// the wasm governance over the host-constructed (config-seeded) store —
/// exactly the production construction (`bin/node/src/host_state.rs`).
fn wasm_governance(store: Box<dyn sdk::MerkleStore>) -> WasmModule {
    WasmModule::with_store("governance", GOVERNANCE_WASM, store).expect("load component")
}

/// the production wiring, verbatim (`bin/node/src/host_state.rs` — where the
/// production tenant is the wasm twin; the native builder chain is what the
/// guest compiles in).
fn native_governance(store: Box<dyn sdk::MerkleStore>) -> Governance {
    Governance::new("governance", store, "valset", "identity")
        .with_invite_binding(INVITE)
        .with_code_registry("modules")
}

/// the native identity SIBLING over a MemStore double — the store backend is
/// irrelevant here (both hosts carry the same-shaped native sibling; only
/// same-backend cross-host equality is asserted).
fn native_identity() -> Identity {
    Identity::new(
        "identity",
        Box::new(sdk_testkit::MemStore::new()),
        CHAIN_ID.to_string(),
    )
}

async fn seeded_valset(validators: &[Vec<u8>]) -> Valset {
    let mut valset = Valset::new(
        "valset",
        Box::new(sdk_testkit::MemStore::new()),
        "governance",
    );
    for v in validators {
        valset.seed(v.clone()).await.expect("seed valset");
    }
    valset.finish_seed().await.expect("seed valset");
    valset
}

/// a code registry with one seeded tenant, so UpdateModule has a module to
/// re-code (mirrors bin/node's genesis-seeded registry).
async fn seeded_registry() -> Modules {
    let mut registry = Modules::new(
        "modules",
        Box::new(sdk_testkit::MemStore::new()),
        "valset",
        "governance",
    );
    registry
        .seed("hello", modules::Kind::Module, vec![0xAA; 32], Vec::new())
        .await
        .expect("seed stages");
    registry.finish_seed().await.expect("seed commits");
    registry
}

/// the full native sibling set both hosts carry: valset (membership), identity
/// (account shares), the modules registry (node upgrades + code swaps).
async fn siblings(validators: &[Vec<u8>]) -> Vec<Box<dyn sdk::Module>> {
    vec![
        Box::new(seeded_valset(validators).await),
        Box::new(native_identity()),
        Box::new(seeded_registry().await),
    ]
}

async fn native_host(context: &deterministic::Context, validators: &[Vec<u8>]) -> Host {
    let store = gov_store(context, "native_gov", INVITE).await;
    let mut modules = siblings(validators).await;
    modules.push(Box::new(native_governance(Box::new(store))));
    Host::genesis(modules).expect("genesis")
}

async fn wasm_host_(context: &deterministic::Context, validators: &[Vec<u8>]) -> Host {
    let store = gov_store(context, "wasm_gov", INVITE).await;
    let mut modules = siblings(validators).await;
    modules.push(Box::new(wasm_governance(Box::new(store))));
    Host::genesis(modules).expect("genesis")
}

type Ed = PrivateKey;

fn ed(seed: u64) -> Ed {
    Ed::from_seed(seed)
}
fn ed_pub(k: &Ed) -> Vec<u8> {
    k.public_key().as_ref().to_vec()
}

fn gov(m: &GovMsg) -> Msg {
    Msg {
        target: "governance".into(),
        payload: encode_msg(m),
    }
}

fn propose(id: &str, action: GovAction, period: u64) -> Msg {
    gov(&GovMsg::Propose {
        proposal_id: id.into(),
        action,
        voting_period: period,
    })
}

fn vote(id: &str, approve: bool) -> Msg {
    gov(&GovMsg::Vote {
        proposal_id: id.into(),
        approve,
    })
}

fn execute(id: &str) -> Msg {
    gov(&GovMsg::Execute {
        proposal_id: id.into(),
    })
}

/// mint an invite token over `binding` and wrap it (with `joiner`'s
/// proof-of-possession) into the Redeem op — deterministic: the nonce is
/// caller-chosen and ed25519 signing is deterministic.
fn redeem(issuer: &Ed, nonce: [u8; INVITE_NONCE_LEN], binding: &[u8], joiner: &Ed) -> Msg {
    // bearer, far-future expiry — the current grant preimage
    // (binding || nonce || expires_le; no role, no kind, no target).
    let expires: u64 = 4_102_444_800; // 2100-01-01 — far future, wall-clock-shaped
    let token_msg = [binding, nonce.as_slice(), &expires.to_le_bytes()].concat();
    let token_sig = issuer.sign(INVITE_GRANT_NAMESPACE, &token_msg);
    let token = InviteToken {
        issuer: issuer.public_key(),
        nonce,
        expires_unix_secs: expires,
        sig: token_sig.clone(),
    };
    let proof = sign_join_proof(joiner, binding, &token);
    gov(&GovMsg::Redeem {
        issuer: ed_pub(issuer),
        nonce: nonce.to_vec(),
        token_sig: token_sig.as_ref().to_vec(),
        joiner: ed_pub(joiner),
        proof: proof.as_ref().to_vec(),
        expires_unix_secs: expires,
    })
}

/// an identity Create founding an account for the submitting (founder) key —
/// seeds the identity sibling for the account-share flow.
fn create(name: &str) -> Msg {
    Msg {
        target: "identity".into(),
        payload: identity::encode_msg(&IdentityMsg::Create {
            name: name.into(),
            scheme: KeyScheme::Ed25519,
        }),
    }
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
    h.module_root("governance").expect("governance registered")
}

const SIBLING_IDS: [&str; 3] = ["valset", "modules", "identity"];

/// the read matrix: the roster-served proposal listing, per-id gets (present
/// and absent), redemption point reads (a spent nonce, an unspent one), and
/// the share registry.
async fn replies(h: &Host, ids: &[&str]) -> Vec<Vec<u8>> {
    let mut queries = vec![
        encode_query(&GovQuery::Proposals),
        encode_query(&GovQuery::Redemption {
            nonce: vec![7; INVITE_NONCE_LEN],
        }),
        encode_query(&GovQuery::Redemption {
            nonce: vec![0; INVITE_NONCE_LEN],
        }),
        encode_query(&GovQuery::Shares),
        encode_query(&GovQuery::Proposal {
            proposal_id: "absent".into(),
        }),
    ];
    for id in ids {
        queries.push(encode_query(&GovQuery::Proposal {
            proposal_id: (*id).into(),
        }));
    }
    let mut out = Vec::new();
    for q in &queries {
        out.push(h.query("governance", q).await.expect("query"));
    }
    out
}

/// submit one accepted op to BOTH hosts and assert the parity invariants:
/// identical replies, per-sibling cross-host agreement, lockstep governance
/// root movement, and — where the op's follow-ups act on a sibling — that the
/// named sibling's root MOVED on both hosts (the emitted msg really landed).
/// (one argument per invariant knob; a builder would only obscure the matrix.)
#[allow(clippy::too_many_arguments)]
async fn roundtrip(
    native: &mut Host,
    wasm: &mut Host,
    ids: &[&str],
    height: u64,
    origin: Origin,
    m: Msg,
    moves: bool,
    sibling_moves: &[&str],
) {
    let (n_before, w_before) = (root_of(native), root_of(wasm));
    let sibling_before: Vec<_> = sibling_moves
        .iter()
        .map(|s| (native.module_root(s), wasm.module_root(s)))
        .collect();
    native
        .submit_at(block(height, origin.clone()), m.clone())
        .await
        .expect("native submit");
    wasm.submit_at(block(height, origin), m)
        .await
        .expect("wasm submit");
    assert_eq!(
        replies(native, ids).await,
        replies(wasm, ids).await,
        "replies diverge after block {height}"
    );
    for sibling in SIBLING_IDS {
        assert_eq!(
            native.module_root(sibling),
            wasm.module_root(sibling),
            "the native {sibling} sibling diverged at {height}"
        );
    }
    for (sibling, (n_sib, w_sib)) in sibling_moves.iter().zip(sibling_before) {
        assert_ne!(
            native.module_root(sibling),
            n_sib,
            "the {sibling} follow-up did not land natively at {height}"
        );
        assert_ne!(
            wasm.module_root(sibling),
            w_sib,
            "the {sibling} follow-up did not land through the wasm runtime at {height}"
        );
    }
    if moves {
        assert_ne!(root_of(native), n_before, "native root stuck at {height}");
        assert_ne!(root_of(wasm), w_before, "wasm root stuck at {height}");
    } else {
        assert_eq!(root_of(native), n_before, "native root moved at {height}");
        assert_eq!(root_of(wasm), w_before, "wasm root moved at {height}");
    }
    // THE continuity property: both roots ARE the same store root.
    assert_eq!(
        root_of(native),
        root_of(wasm),
        "the two runtimes diverged at {height}"
    );
}

/// submit one REJECTED op to both hosts: reasons carry the same needle, and
/// both governance roots (and all siblings) are byte-identical to pre-block.
async fn reject_roundtrip(
    native: &mut Host,
    wasm: &mut Host,
    ids: &[&str],
    height: u64,
    origin: Origin,
    m: Msg,
    needle: &str,
) {
    let (n_before, w_before) = (root_of(native), root_of(wasm));
    let n_err = native
        .submit_at(block(height, origin.clone()), m.clone())
        .await
        .expect_err("native must reject");
    let w_err = wasm
        .submit_at(block(height, origin), m)
        .await
        .expect_err("wasm must reject");
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
    assert_eq!(root_of(native), n_before, "native root moved on reject");
    assert_eq!(root_of(wasm), w_before, "wasm root moved on reject");
    for sibling in SIBLING_IDS {
        assert_eq!(native.module_root(sibling), wasm.module_root(sibling));
    }
    assert_eq!(replies(native, ids).await, replies(wasm, ids).await);
}

async fn residents_of(h: &Host) -> Vec<Vec<u8>> {
    let reply = h
        .query("valset", &valset_encode_query(&ValsetQuery::Residents))
        .await
        .expect("residents query");
    match valset_decode_reply(&reply).expect("decode") {
        ValsetReply::Residents(r) => r,
        other => panic!("expected Residents, got {other:?}"),
    }
}

#[test]
fn same_ops_same_replies_roots_in_lockstep_follow_ups_land() {
    deterministic::Runner::default().start(|context| async move {
        same_ops_inner(&context).await;
    });
}

async fn same_ops_inner(context: &deterministic::Context) {
    let (a, b, c, joiner, joiner2) = (ed(1), ed(2), ed(3), ed(40), ed(41));
    let (a_pub, b_pub, c_pub) = (ed_pub(&a), ed_pub(&b), ed_pub(&c));
    let validators = vec![a_pub.clone(), b_pub.clone()];
    let ids = ["p-add-res", "p-sig", "p-upg", "p-code"];

    let mut native = native_host(context, &validators).await;
    let mut wasm = wasm_host_(context, &validators).await;

    // ROOT-CONTINUITY from GENESIS: both roots are the store's merkle root,
    // and the store already carries the seeded `__config` record — a real
    // (non-sentinel) root, identical across the runtimes.
    assert_ne!(
        root_of(&native),
        StateRoot::ZERO,
        "the config record is in the root from block zero"
    );
    assert_eq!(
        root_of(&native),
        root_of(&wasm),
        "genesis roots must be continuous across the runtimes"
    );

    // ---- the membership loop: proposal → ballots → tally → valset follow-up.
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        1,
        Origin::External(a_pub.clone()),
        propose(
            "p-add-res",
            GovAction::AddResident { key: c_pub.clone() },
            500,
        ),
        true,
        &[],
    )
    .await;
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        2,
        Origin::External(a_pub.clone()),
        vote("p-add-res", true),
        true,
        &[],
    )
    .await;
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        3,
        Origin::External(b_pub.clone()),
        vote("p-add-res", true),
        true,
        &[],
    )
    .await;
    // the tally passes (2 of 2) and the Grant follow-up lands on the valset
    // sibling IN THIS BLOCK — emitted from inside the wasm guest, republished
    // by the runtime, drained by the host.
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        4,
        Origin::External(b_pub.clone()),
        execute("p-add-res"),
        true,
        &["valset"],
    )
    .await;
    assert!(
        residents_of(&wasm).await.contains(&c_pub),
        "the wasm-driven Grant admitted C"
    );

    // ---- a failing proposal settles Rejected at its deadline (no follow-up).
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        5,
        Origin::External(b_pub.clone()),
        propose(
            "p-sig",
            GovAction::Signal {
                text: "quack?".into(),
            },
            3,
        ),
        true,
        &[],
    )
    .await;
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        6,
        Origin::External(a_pub.clone()),
        vote("p-sig", false),
        true,
        &[],
    )
    .await;
    // height 8: consensus_time 1008 reaches the deadline (1005 + 3) — the
    // settle is an ACCEPTED op that moves the root (status: Rejected).
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        8,
        Origin::External(a_pub.clone()),
        execute("p-sig"),
        true,
        &[],
    )
    .await;

    // ---- UpdateModule: a WASM governance drives the CODE REGISTRY — the
    // passing authorization emits ModulesMsg::ScheduleSwap into the native modules registry
    // sibling, scheduling a height-gated code swap for the "hello" tenant.
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        13,
        Origin::External(b_pub.clone()),
        propose(
            "p-code",
            GovAction::UpdateModule {
                name: "hello-replacement".into(),
                module_id: "hello".into(),
                // execute lands at height 16 below; 16 + 484 = 500, matching
                // the pending.activation_height assertion further down.
                activation_lead: 484,
                code_hash: vec![0xBB; 32],
            },
            500,
        ),
        true,
        &[],
    )
    .await;
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        14,
        Origin::External(a_pub.clone()),
        vote("p-code", true),
        true,
        &[],
    )
    .await;
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        15,
        Origin::External(b_pub.clone()),
        vote("p-code", true),
        true,
        &[],
    )
    .await;
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        16,
        Origin::External(a_pub.clone()),
        execute("p-code"),
        true,
        &["modules"],
    )
    .await;
    // decoded proof on BOTH hosts: the registry now carries the pending swap.
    for host in [&native, &wasm] {
        let reply = host
            .query(
                "modules",
                &modules_encode_query(&ModulesQuery::ModuleStatus),
            )
            .await
            .expect("modules registry status");
        let ModulesReply::ModuleStatus { modules } = modules_decode_reply(&reply).expect("decode")
        else {
            panic!("expected Status reply");
        };
        let hello = modules
            .iter()
            .find(|m| m.module_id == "hello")
            .expect("hello is registered");
        let pending = hello.pending.as_ref().expect("a swap is scheduled");
        assert_eq!(pending.name, "hello-replacement");
        assert_eq!(pending.activation_height, 500);
        assert_eq!(pending.code_hash, vec![0xBB; 32]);
    }

    // ---- Redeem: in-consensus invite admission, exactly-once. the token
    // verifies against the CONFIGURED binding inside the guest, and the Grant
    // follow-up lands on valset in the same block.
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        17,
        Origin::External(ed_pub(&joiner)),
        redeem(&a, [7; INVITE_NONCE_LEN], INVITE, &joiner),
        true,
        &["valset"],
    )
    .await;
    assert!(
        residents_of(&wasm).await.contains(&ed_pub(&joiner)),
        "the redeemed joiner holds resident standing"
    );

    // the SAME token again (even for a different joiner key) is single-use.
    reject_roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        18,
        Origin::External(ed_pub(&joiner2)),
        redeem(&a, [7; INVITE_NONCE_LEN], INVITE, &joiner2),
        "already redeemed",
    )
    .await;
    // and a FRESH token for a joiner who already holds standing settles too.
    reject_roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        19,
        Origin::External(ed_pub(&joiner)),
        redeem(&a, [8; INVITE_NONCE_LEN], INVITE, &joiner),
        "already holds resident standing",
    )
    .await;

    // queries are read-only on the wasm side too.
    let settled = root_of(&wasm);
    let _ = replies(&wasm, &ids).await;
    assert_eq!(root_of(&wasm), settled, "a query moved the wasm root");
}

#[test]
fn rejections_match_and_leave_no_trace() {
    deterministic::Runner::default().start(|context| async move {
        rejections_inner(&context).await;
    });
}

async fn rejections_inner(context: &deterministic::Context) {
    let (a, b) = (ed(1), ed(2));
    let (a_pub, b_pub) = (ed_pub(&a), ed_pub(&b));
    let d_pub = ed_pub(&ed(4));
    let outsider = ed(99);
    let outsider_pub = ed_pub(&outsider);
    let validators = vec![a_pub.clone(), b_pub.clone()];
    let ids = ["p", "stale", "done"];

    let mut native = native_host(context, &validators).await;
    let mut wasm = wasm_host_(context, &validators).await;

    // seed three proposals: a long-lived Open one, one whose deadline will
    // lapse un-executed, and one settled at its deadline.
    for host in [&mut native, &mut wasm] {
        host.submit_at(
            block(1, Origin::External(a_pub.clone())),
            propose("p", GovAction::AddValidator { key: d_pub.clone() }, 500),
        )
        .await
        .expect("seed p");
        host.submit_at(
            block(2, Origin::External(a_pub.clone())),
            propose("stale", GovAction::Signal { text: "s".into() }, 3),
        )
        .await
        .expect("seed stale");
        host.submit_at(
            block(3, Origin::External(b_pub.clone())),
            propose("done", GovAction::Signal { text: "d".into() }, 3),
        )
        .await
        .expect("seed done");
        // time 1007 ≥ done's deadline (1003 + 3): settles Rejected (no votes).
        host.submit_at(block(7, Origin::External(a_pub.clone())), execute("done"))
            .await
            .expect("settle done");
    }

    // every distinct refusal family: proposal grammar (id, period, key shape,
    // upgrade/module-update door checks), electorate gating (decided by the
    // valset sibling reads inside the wasm runtime), ballot/tally lifecycle
    // (unknown, settled, deadline, undecidable), invite verification, origin
    // shapes, and the decode seam.
    let rejects: Vec<(Origin, Msg, &str)> = vec![
        (
            Origin::External(a_pub.clone()),
            propose("", GovAction::Signal { text: "x".into() }, 5),
            "proposal_id must not be empty",
        ),
        (
            Origin::External(a_pub.clone()),
            propose("p2", GovAction::Signal { text: "x".into() }, 0),
            "voting_period must be in",
        ),
        (
            Origin::External(a_pub.clone()),
            propose("p2", GovAction::AddValidator { key: vec![1; 8] }, 5),
            "32-byte ed25519",
        ),
        (
            Origin::External(a_pub.clone()),
            propose(
                "p2",
                GovAction::UpdateModule {
                    name: "n".into(),
                    module_id: String::new(),
                    activation_lead: 500,
                    code_hash: vec![0xBB; 32],
                },
                5,
            ),
            "module_id must not be empty",
        ),
        (
            Origin::External(a_pub.clone()),
            propose(
                "p2",
                GovAction::UpdateModule {
                    name: "n".into(),
                    module_id: "hello".into(),
                    activation_lead: 500,
                    code_hash: vec![0xBB; 8],
                },
                5,
            ),
            "code_hash must be",
        ),
        (
            Origin::External(a_pub.clone()),
            propose("p", GovAction::Signal { text: "dup".into() }, 5),
            "proposal already exists",
        ),
        // the standing gate — in validator mode the submitter must ITSELF be
        // a member node (no identity read; a key never fans out to nodes).
        // decided by the valset sibling read.
        (
            Origin::External(outsider_pub.clone()),
            propose("p2", GovAction::Signal { text: "x".into() }, 5),
            "not a validator-set member node",
        ),
        (
            Origin::Module("saga".into()),
            propose("p2", GovAction::Signal { text: "x".into() }, 5),
            "external submitter",
        ),
        (
            Origin::External(a_pub.clone()),
            vote("nope", true),
            "no such proposal",
        ),
        (
            Origin::External(outsider_pub.clone()),
            vote("p", true),
            "voter is not in the frozen electorate",
        ),
        (
            Origin::External(a_pub.clone()),
            vote("done", true),
            "proposal is settled",
        ),
        // "stale" is still Open but its deadline (1005) has lapsed.
        (
            Origin::External(a_pub.clone()),
            vote("stale", true),
            "voting closed at the deadline",
        ),
        (
            Origin::External(a_pub.clone()),
            execute("nope"),
            "no such proposal",
        ),
        (
            Origin::External(a_pub.clone()),
            execute("done"),
            "proposal is settled",
        ),
        // "p" has no ballots and a far deadline: the tally is undecidable.
        (
            Origin::External(a_pub.clone()),
            execute("p"),
            "not decidable yet",
        ),
        // invite verification, in-guest: a malformed nonce, a token for
        // ANOTHER network, a non-member issuer, an already-admitted joiner.
        (
            Origin::External(a_pub.clone()),
            {
                let mut m = redeem(&a, [7; INVITE_NONCE_LEN], INVITE, &ed(50));
                let GovMsg::Redeem {
                    issuer,
                    token_sig,
                    joiner,
                    proof,
                    ..
                } = governance::decode_msg(&m.payload).expect("decode")
                else {
                    panic!("redeem shape");
                };
                m.payload = encode_msg(&GovMsg::Redeem {
                    issuer,
                    nonce: vec![7; 8],
                    token_sig,
                    joiner: joiner.clone(),
                    proof,
                    expires_unix_secs: 4_102_444_800,
                });
                m
            },
            "nonce must be",
        ),
        (
            Origin::External(a_pub.clone()),
            redeem(&a, [9; INVITE_NONCE_LEN], b"other-net", &ed(50)),
            "does not verify for this network",
        ),
        (
            Origin::External(a_pub.clone()),
            redeem(&outsider, [9; INVITE_NONCE_LEN], INVITE, &ed(50)),
            "no longer part of this network",
        ),
        (
            Origin::External(a_pub.clone()),
            redeem(&a, [9; INVITE_NONCE_LEN], INVITE, &b),
            "already a validator",
        ),
        (
            Origin::External(a_pub.clone()),
            Msg {
                target: "governance".into(),
                payload: b"definitely-not-json".to_vec(),
            },
            "expected value",
        ),
    ];

    for (height, (origin, m, needle)) in rejects.into_iter().enumerate() {
        // heights start past the seeding blocks; consensus_time stays past
        // "stale"'s deadline for every entry.
        let height = height as u64 + 8;
        reject_roundtrip(&mut native, &mut wasm, &ids, height, origin, m, needle).await;
    }
}

#[test]
fn multi_dispatch_block_reads_prior_writes_and_isolates_rejections() {
    deterministic::Runner::default().start(|context| async move {
        multi_dispatch_inner(&context).await;
    });
}

async fn multi_dispatch_inner(context: &deterministic::Context) {
    let (a, b) = (ed(1), ed(2));
    let (a_pub, b_pub) = (ed_pub(&a), ed_pub(&b));
    let e_pub = ed_pub(&ed(5));
    let validators = vec![a_pub.clone(), b_pub.clone()];
    let ids = ["batch", "iso"];

    let mut native = native_host(context, &validators).await;
    let mut wasm = wasm_host_(context, &validators).await;

    // ONE block, FOUR ops: propose → two ballots → execute. every later
    // dispatch reads the earlier dispatches' STAGED writes (the ballots read
    // the staged proposal, the tally reads the staged ballots) — on the wasm
    // side through the host's outer staged store overlay — and
    // the passing tally emits the Grant follow-up from the FOURTH dispatch of
    // the same block.
    let (n_valset, w_valset) = (native.module_root("valset"), wasm.module_root("valset"));
    let batch = vec![
        (
            Origin::External(a_pub.clone()),
            propose("batch", GovAction::AddResident { key: e_pub.clone() }, 500),
        ),
        (Origin::External(a_pub.clone()), vote("batch", true)),
        (Origin::External(b_pub.clone()), vote("batch", true)),
        (Origin::External(a_pub.clone()), execute("batch")),
    ];
    let n_out = native
        .submit_block(block(1, Origin::External(a_pub.clone())), batch.clone())
        .await
        .expect("native block");
    let w_out = wasm
        .submit_block(block(1, Origin::External(a_pub.clone())), batch)
        .await
        .expect("wasm block");
    for out in [&n_out, &w_out] {
        assert!(
            out.members
                .iter()
                .all(|m| matches!(m, MemberOutcome::Applied { .. })),
            "all four members must apply: {:?}",
            out.members
        );
    }
    assert_eq!(replies(&native, &ids).await, replies(&wasm, &ids).await);
    assert_ne!(native.module_root("valset"), n_valset, "the Grant landed");
    assert_ne!(wasm.module_root("valset"), w_valset, "the Grant landed");
    assert_eq!(native.module_root("valset"), wasm.module_root("valset"));
    assert!(residents_of(&wasm).await.contains(&e_pub));

    // ONE block where the middle member rejects AGAINST THE STAGE (a duplicate
    // of the proposal staged by the first member — read-your-writes is what
    // decides the rejection), while the third member votes on that same staged
    // proposal: [Applied, Rejected, Applied].
    let (n_before, w_before) = (root_of(&native), root_of(&wasm));
    let batch = vec![
        (
            Origin::External(a_pub.clone()),
            propose("iso", GovAction::Signal { text: "one".into() }, 500),
        ),
        (
            Origin::External(b_pub.clone()),
            propose("iso", GovAction::Signal { text: "two".into() }, 500),
        ),
        (Origin::External(a_pub.clone()), vote("iso", true)),
    ];
    let n_out = native
        .submit_block(block(2, Origin::External(a_pub.clone())), batch.clone())
        .await
        .expect("native block");
    let w_out = wasm
        .submit_block(block(2, Origin::External(a_pub.clone())), batch)
        .await
        .expect("wasm block");
    for out in [&n_out, &w_out] {
        assert!(matches!(out.members[0], MemberOutcome::Applied { .. }));
        assert!(
            matches!(out.members[1], MemberOutcome::Rejected { .. }),
            "the duplicate must reject against the SAME-BLOCK stage: {:?}",
            out.members
        );
        assert!(matches!(out.members[2], MemberOutcome::Applied { .. }));
    }
    assert_ne!(root_of(&native), n_before);
    assert_ne!(root_of(&wasm), w_before);
    assert_eq!(replies(&native, &ids).await, replies(&wasm, &ids).await);
}

#[test]
fn account_share_mode_resolves_identity_siblings_in_the_guest() {
    deterministic::Runner::default().start(|context| async move {
        share_mode_inner(&context).await;
    });
}

async fn share_mode_inner(context: &deterministic::Context) {
    let (a, b) = (ed(1), ed(2));
    let (a_pub, b_pub) = (ed_pub(&a), ed_pub(&b));
    let (founder_a, founder_b) = (ed(11), ed(12));
    let validators = vec![a_pub.clone(), b_pub.clone()];
    let ids = ["shares", "sig2"];

    let mut native = native_host(context, &validators).await;
    let mut wasm = wasm_host_(context, &validators).await;

    // seed the identity SIBLING: two USER keys (no validator standing) found
    // accounts 1 and 2 from their own origins. these blocks leave governance
    // alone on both runtimes.
    let (n0, w0) = (root_of(&native), root_of(&wasm));
    for host in [&mut native, &mut wasm] {
        host.submit_at(
            block(1, Origin::External(ed_pub(&founder_a))),
            create("alice"),
        )
        .await
        .expect("found A");
        host.submit_at(
            block(2, Origin::External(ed_pub(&founder_b))),
            create("bob"),
        )
        .await
        .expect("found B");
    }
    assert_eq!(root_of(&native), n0);
    assert_eq!(root_of(&wasm), w0);

    // AdoptShares (still validator mode: proposed and carried by the member
    // nodes): the door check resolves EVERY named account NUMBER against the
    // identity sibling — inside the guest those are host-routed query-module
    // reads through the memoized replay.
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        3,
        Origin::External(a_pub.clone()),
        propose(
            "shares",
            GovAction::AdoptShares {
                allocations: vec![
                    ShareAllocation {
                        account_id: ACCOUNT_A,
                        shares: 2,
                    },
                    ShareAllocation {
                        account_id: ACCOUNT_B,
                        shares: 1,
                    },
                ],
            },
            500,
        ),
        true,
        &[],
    )
    .await;
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        4,
        Origin::External(a_pub.clone()),
        vote("shares", true),
        true,
        &[],
    )
    .await;
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        5,
        Origin::External(b_pub.clone()),
        vote("shares", true),
        true,
        &[],
    )
    .await;
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        6,
        Origin::External(a_pub.clone()),
        execute("shares"),
        true,
        &[],
    )
    .await;

    // share mode now governs NEW proposals: the proposer and every ballot
    // resolve key → account through the identity sibling's `OfKey` (in-guest
    // reads), and the frozen electorate is the share registry. the USER keys
    // act — no validator standing anywhere.
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        7,
        Origin::External(ed_pub(&founder_a)),
        propose(
            "sig2",
            GovAction::Signal {
                text: "by-shares".into(),
            },
            500,
        ),
        true,
        &[],
    )
    .await;
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        8,
        Origin::External(ed_pub(&founder_a)),
        vote("sig2", true),
        true,
        &[],
    )
    .await;
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        9,
        Origin::External(ed_pub(&founder_b)),
        vote("sig2", false),
        true,
        &[],
    )
    .await;
    // ParticipatingMajority: participation 3 ≥ quorum 2, yes (2) > no (1),
    // irreversible early (yes > total - yes) — Passed on both runtimes.
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        10,
        Origin::External(ed_pub(&founder_b)),
        execute("sig2"),
        true,
        &[],
    )
    .await;

    // a NODE key never resolves to an account: in share mode the validator
    // that carried the adoption has no standing to propose — decided by the
    // identity sibling read inside the guest.
    reject_roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        11,
        Origin::External(a_pub.clone()),
        propose("sig3", GovAction::Signal { text: "x".into() }, 5),
        "belongs to no Identity account",
    )
    .await;

    // decoded spot check on the wasm side: account-keyed ballots, the frozen
    // share electorate, and the passed outcome.
    let reply = wasm
        .query(
            "governance",
            &encode_query(&GovQuery::Proposal {
                proposal_id: "sig2".into(),
            }),
        )
        .await
        .expect("proposal query");
    let governance::GovReply::Proposal(Some(view)) =
        governance::decode_reply(&reply).expect("decode")
    else {
        panic!("sig2 must exist");
    };
    assert_eq!(view.status, governance::ProposalStatus::Passed);
    assert_eq!(view.voter_kind, governance::VoterKind::Account);
    assert_eq!(
        view.proposer,
        account_principal(ACCOUNT_A),
        "proposer resolved to the ACCOUNT principal"
    );
    // ballots and the frozen electorate are keyed by ACCOUNT principal (8
    // bytes LE), served in ascending key order (the module's BTreeMap
    // projection).
    let mut expected_votes = vec![
        (account_principal(ACCOUNT_A), true),
        (account_principal(ACCOUNT_B), false),
    ];
    expected_votes.sort();
    assert_eq!(view.votes, expected_votes);
    let mut expected_powers = vec![
        (account_principal(ACCOUNT_A), 2),
        (account_principal(ACCOUNT_B), 1),
    ];
    expected_powers.sort();
    assert_eq!(view.electorate, expected_powers);
}

#[test]
fn genesis_config_is_consensus_state_and_governs_the_guest() {
    deterministic::Runner::default().start(|context| async move {
        genesis_config_inner(&context).await;
    });
}

async fn genesis_config_inner(context: &deterministic::Context) {
    // the config record is IN the store root: per-network genesis roots,
    // honestly.
    let here = gov_store(context, "cfg_here", INVITE).await;
    let same = gov_store(context, "cfg_same", INVITE).await;
    let other = gov_store(context, "cfg_other", b"other-net").await;
    assert_eq!(here.root(), same.root(), "same config, same genesis root");
    assert_ne!(
        here.root(),
        other.root(),
        "a different invite binding IS a different genesis consensus state"
    );
    drop((here, same));

    // and the config GOVERNS the guest: the same Redeem op (token minted for
    // INVITE) is accepted by the tenant configured with INVITE and
    // deterministically refused by the tenant configured with another binding
    // — the guest read its binding from the seeded store record alone.
    let (a, joiner) = (ed(1), ed(40));
    let validators = vec![ed_pub(&a)];
    let mut wasm = wasm_host_(context, &validators).await;
    let mut other_host = {
        let mut modules = siblings(&validators).await;
        modules.push(Box::new(wasm_governance(Box::new(other))));
        Host::genesis(modules).expect("genesis")
    };

    let m = redeem(&a, [7; INVITE_NONCE_LEN], INVITE, &joiner);
    wasm.submit_at(block(1, Origin::External(ed_pub(&joiner))), m.clone())
        .await
        .expect("the configured binding accepts its own token");
    let err = other_host
        .submit_at(block(1, Origin::External(ed_pub(&joiner))), m)
        .await
        .expect_err("a foreign binding must refuse the token");
    let SubmitError::Rejected(Error::Module(reason)) = err else {
        panic!("rejection shape: {err:?}");
    };
    assert!(
        reason.contains("does not verify for this network"),
        "the binding is what refuses: {reason}"
    );
}
