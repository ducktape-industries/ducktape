//! the STORE-BACKED cutover-continuity proof for capability: the `capability`
//! guest component over `WasmModule::with_store(QmdbStore)` and the native
//! `CapabilityRegistry` over the same store shape are ROOT-CONTINUOUS — the
//! same op sequence commits the IDENTICAL qmdb merkle root after every block
//! (both roots ARE the store's root; qmdb's batch canonicalizes mutations by
//! hashed key, so the native logical-key commit order and the wasm hashed-key
//! drain order produce the same op log). capability carries no per-network
//! genesis config, so both stores start empty and the genesis roots are
//! already equal.
//!
//! capability is MEMBER-GATED against a sibling, which is the point of this
//! tenant: every announce queries the valset module's live Validators AND
//! Residents projections through `ctx.query` before it may stage anything —
//! inside the guest those resolve through the wasm runtime's memoized-replay
//! machinery under real dispatch. both hosts therefore carry a REAL
//! `valset::Valset` seeded with test validators, and the matrix covers an
//! acceptance (member), an acceptance the RESIDENT union arm admits, and a
//! rejection (outsider) — all decided by sibling reads on the wasm side.

use capability::{
    CapabilityMsg, CapabilityQuery, CapabilityRegistry, CapabilityReply, decode_reply, encode_msg,
    encode_query,
};
use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::PrivateKey;
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use host::{BlockContext, Host, MemberOutcome, SubmitError};
use sdk::{Error, Msg, Origin, StateRoot};
use statesync::qmdb::QmdbStore;
use valset::{Valset, ValsetMsg, encode_msg as valset_encode_msg};
use wasm_host::WasmModule;

/// GENERATED artifact — built from the `capability` module's guest port by
/// guest-builder (`make wasm-modules`); committed so this proof is self-contained.
const CAPABILITY_WASM: &[u8] = include_bytes!("fixtures/capability.component.wasm");

/// a fresh qmdb store. `label` doubles as the store id: the deterministic
/// runtime keys storage partitions by id alone (child labels do not namespace
/// them), so a shared id would make the second store REPLAY the first's
/// journal. the id is not part of the qmdb root, so distinct ids cost nothing.
async fn cap_store(
    context: &deterministic::Context,
    label: &'static str,
) -> QmdbStore<deterministic::Context> {
    QmdbStore::init(context.child(label), label).await
}

/// the wasm capability over the host-constructed store — exactly the
/// production construction (`bin/node/src/host_state.rs`).
fn wasm_capability(store: Box<dyn sdk::MerkleStore>) -> WasmModule {
    WasmModule::with_store("capability", CAPABILITY_WASM, store).expect("load component")
}

/// EXACTLY the production wiring in bin/node's host state — the valset id is
/// genesis wiring, so both runtimes (and the guest itself) must gate against
/// the same sibling or the member gate forks.
fn native_capability(store: Box<dyn sdk::MerkleStore>) -> CapabilityRegistry {
    CapabilityRegistry::new("capability", store, Some("valset".into()))
}

/// a REAL valset sibling, genesis-seeded with `validators` — the module whose
/// live Validators/Residents projections every announce is gated on.
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

async fn native_host(context: &deterministic::Context, validators: &[Vec<u8>]) -> Host {
    let store = cap_store(context, "native_cap").await;
    Host::genesis(vec![
        Box::new(native_capability(Box::new(store))),
        Box::new(seeded_valset(validators).await),
    ])
    .expect("genesis")
}

async fn wasm_host_(context: &deterministic::Context, validators: &[Vec<u8>]) -> Host {
    let store = cap_store(context, "wasm_cap").await;
    Host::genesis(vec![
        Box::new(wasm_capability(Box::new(store))),
        Box::new(seeded_valset(validators).await),
    ])
    .expect("genesis")
}

/// a REAL ed25519 public key — valset's Grant path validates the curve point,
/// so the resident admission needs genuine keys (and the rest use them for
/// uniformity).
fn key(seed: u64) -> Vec<u8> {
    PrivateKey::from_seed(seed).public_key().as_ref().to_vec()
}

fn ext(who: &[u8]) -> Origin {
    Origin::External(who.to_vec())
}

fn announce(tags: &[&str]) -> Msg {
    Msg {
        target: "capability".into(),
        payload: encode_msg(&CapabilityMsg::Announce {
            capabilities: tags.iter().map(|t| t.to_string()).collect(),
            resources: Default::default(),
        }),
    }
}

/// a System-origin valset Grant — how the resident tier is populated (genesis
/// orchestration shape), targeting the SIBLING, not the module under test.
fn grant(key: &[u8]) -> Msg {
    Msg {
        target: "valset".into(),
        payload: valset_encode_msg(&ValsetMsg::Grant { key: key.to_vec() }),
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

/// a module-claimed capability class — the classed-address router's write op.
fn claim(class: &str) -> Msg {
    Msg {
        target: "capability".into(),
        payload: encode_msg(&CapabilityMsg::ClaimClass {
            class: class.into(),
        }),
    }
}

/// the read matrix: every query family, including the absent shapes, plus a
/// per-key Node read for every key the test knows about. class reads ride in
/// every matrix pass, so ResolveClass/Classes equality is asserted per block
/// across ALL parity tests, not just the class-focused ones.
async fn replies(h: &Host, keys: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut queries = vec![
        encode_query(&CapabilityQuery::Providers {
            capability: "codex".into(),
        }),
        encode_query(&CapabilityQuery::Providers {
            capability: "claude".into(),
        }),
        encode_query(&CapabilityQuery::Providers {
            capability: "absent".into(),
        }),
        encode_query(&CapabilityQuery::All),
        encode_query(&CapabilityQuery::ResolveClass {
            class: "agent".into(),
        }),
        encode_query(&CapabilityQuery::ResolveClass { class: "ai".into() }),
        encode_query(&CapabilityQuery::ResolveClass {
            class: "absent".into(),
        }),
        encode_query(&CapabilityQuery::Classes),
    ];
    for k in keys {
        queries.push(encode_query(&CapabilityQuery::Node { node: k.clone() }));
    }
    let mut out = Vec::new();
    for q in &queries {
        out.push(h.query("capability", q).await.expect("query"));
    }
    out
}

async fn node_tags(h: &Host, node: &[u8]) -> Vec<String> {
    let reply = h
        .query(
            "capability",
            &encode_query(&CapabilityQuery::Node {
                node: node.to_vec(),
            }),
        )
        .await
        .expect("node query");
    match decode_reply(&reply).expect("decode") {
        CapabilityReply::Node(tags) => tags,
        other => panic!("expected Node reply, got {other:?}"),
    }
}

async fn class_owner(h: &Host, class: &str) -> Option<String> {
    let reply = h
        .query(
            "capability",
            &encode_query(&CapabilityQuery::ResolveClass {
                class: class.into(),
            }),
        )
        .await
        .expect("resolve-class query");
    match decode_reply(&reply).expect("decode") {
        CapabilityReply::ClassOwner(owner) => owner,
        other => panic!("expected ClassOwner reply, got {other:?}"),
    }
}

async fn classes_of(h: &Host) -> Vec<(String, String)> {
    let reply = h
        .query("capability", &encode_query(&CapabilityQuery::Classes))
        .await
        .expect("classes query");
    match decode_reply(&reply).expect("decode") {
        CapabilityReply::Classes(classes) => classes,
        other => panic!("expected Classes reply, got {other:?}"),
    }
}

async fn providers(h: &Host, capability: &str) -> Vec<Vec<u8>> {
    let reply = h
        .query(
            "capability",
            &encode_query(&CapabilityQuery::Providers {
                capability: capability.into(),
            }),
        )
        .await
        .expect("providers query");
    match decode_reply(&reply).expect("decode") {
        CapabilityReply::Providers(p) => p,
        other => panic!("expected Providers reply, got {other:?}"),
    }
}

fn root_of(h: &Host) -> StateRoot {
    h.module_root("capability").expect("capability registered")
}

#[test]
fn same_ops_same_replies_roots_in_lockstep_schema_break_pinned() {
    deterministic::Runner::default().start(|context| async move {
        same_ops_inner(&context).await;
    });
}

async fn same_ops_inner(context: &deterministic::Context) {
    let (m1, m2, resident, outsider) = (key(1), key(2), key(3), key(4));
    let validators = [m1.clone(), m2.clone()];
    let mut native = native_host(context, &validators).await;
    let mut wasm = wasm_host_(context, &validators).await;
    let keys = [m1.clone(), m2.clone(), resident.clone(), outsider.clone()];

    // ROOT-CONTINUITY from GENESIS: both roots are the (empty) store's merkle
    // root, identical across the runtimes and distinct from the ZERO sentinel
    // convention only in representation — the property below is equality.
    let genesis = root_of(&wasm);
    assert_eq!(
        root_of(&native),
        genesis,
        "genesis roots must be continuous across the runtimes"
    );
    // the SIBLING is byte-identical on both hosts, before and (asserted per
    // block below) after every op — it is native on both sides.
    assert_eq!(native.module_root("valset"), wasm.module_root("valset"));

    // every op family, in one deterministic sequence. `moves` says whether the
    // op changes committed CAPABILITY state — root movement must agree on BOTH
    // sides. every gated announce resolves the Validators AND Residents
    // sibling queries through the wasm runtime's memoized replay.
    let ops: Vec<(Origin, Msg, bool)> = vec![
        // h1/h2: member announces — acceptance DEPENDS on the sibling reads.
        (ext(&m1), announce(&["codex", "claude"]), true),
        (ext(&m2), announce(&["codex"]), true),
        // h3: re-announcing the current set is an idempotent no-op state-wise.
        (ext(&m1), announce(&["codex", "claude"]), false),
        // h4: a DECLARATIVE REPLACE — "codex" is gone for m1, not kept.
        (ext(&m1), announce(&["claude"]), true),
        // h5: the sibling's own membership moves (a resident is granted);
        // capability's committed state is untouched by a valset-targeted op.
        (Origin::System, grant(&resident), false),
        // h6: the RESIDENT union arm admits — this acceptance flips on the
        // Residents sibling query alone (the key holds no validator seat).
        (ext(&resident), announce(&["claude"]), true),
        // h7: an empty set removes the node.
        (ext(&m2), announce(&[]), true),
        // h8: duplicate tags collapse (set semantics), replacing m1's set.
        (ext(&m1), announce(&["dup", "dup"]), true),
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

        // replies identical after every block (the whole read matrix), and the
        // sibling stayed byte-identical across hosts.
        assert_eq!(
            replies(&native, &keys).await,
            replies(&wasm, &keys).await,
            "replies diverge after block {height}"
        );
        assert_eq!(native.module_root("valset"), wasm.module_root("valset"));
        // roots move in LOCKSTEP: a state-changing op moves both commit
        // boundaries, a no-op holds both...
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

    // decoded spot check on the wasm side: the replace and the collapse both
    // landed, and the resident's announce is served next to the members'.
    assert_eq!(node_tags(&wasm, &m1).await, vec!["dup"]);
    assert!(node_tags(&wasm, &m2).await.is_empty(), "m2 removed");
    assert_eq!(node_tags(&wasm, &resident).await, vec!["claude"]);
    assert_eq!(providers(&wasm, "claude").await, vec![resident.clone()]);
    assert!(providers(&wasm, "codex").await.is_empty());

    // empty the registry: the record set returns to its never-announced shape
    // and BOTH runtimes stay continuous (the op-log root moves — deletes are
    // ops — but moves IDENTICALLY on both sides).
    for (height, who) in [(9u64, resident.clone()), (10u64, m1.clone())] {
        for host in [&mut native, &mut wasm] {
            host.submit_at(block(height, ext(&who)), announce(&[]))
                .await
                .expect("emptying announce");
        }
        assert_eq!(
            replies(&native, &keys).await,
            replies(&wasm, &keys).await,
            "replies diverge after block {height}"
        );
    }
    assert_ne!(root_of(&wasm), genesis, "the deletes are committed ops");
    assert_eq!(root_of(&native), root_of(&wasm));

    // queries are read-only on the wasm side too: the root is STABLE across
    // the whole read matrix.
    let settled = root_of(&wasm);
    let _ = replies(&wasm, &keys).await;
    assert_eq!(root_of(&wasm), settled, "a query moved the wasm root");
}

#[test]
fn rejections_match_and_leave_no_trace() {
    deterministic::Runner::default().start(|context| async move {
        rejections_inner(&context).await;
    });
}

async fn rejections_inner(context: &deterministic::Context) {
    let (m1, outsider) = (key(1), key(4));
    let validators = [m1.clone()];
    let mut native = native_host(context, &validators).await;
    let mut wasm = wasm_host_(context, &validators).await;
    let keys = [m1.clone(), outsider.clone()];

    for host in [&mut native, &mut wasm] {
        host.submit_at(block(1, ext(&m1)), announce(&["codex"]))
            .await
            .expect("seed announce");
    }

    let too_long = "x".repeat(65);
    let too_many: Vec<String> = (0..=64).map(|i| format!("cap{i}")).collect();
    let too_many: Vec<&str> = too_many.iter().map(String::as_str).collect();

    // the rejection matrix: every distinct refusal family the native module
    // implements. the FIRST is the member gate — a rejection DECIDED by the
    // valset sibling queries resolving through the wasm runtime's memoized
    // replay; the tag-shape and decode rejections after it come from a MEMBER,
    // so they pass the gate (two sibling reads) and then still leave no trace.
    // each rejected block must leave BOTH roots byte-identical (the abort
    // path: staged writes discarded, no trace).
    let rejects: Vec<(Origin, Msg, &str)> = vec![
        (ext(&outsider), announce(&["codex"]), "no current standing"),
        (
            Origin::Module("agent".into()),
            announce(&["codex"]),
            "external submitter",
        ),
        (Origin::System, announce(&["codex"]), "external submitter"),
        (
            Origin::External(Vec::new()),
            announce(&["codex"]),
            "key is empty",
        ),
        (ext(&m1), announce(&[""]), "must be non-empty"),
        (ext(&m1), announce(&["Codex"]), "invalid characters"),
        (ext(&m1), announce(&[too_long.as_str()]), "exceeds 64 bytes"),
        (ext(&m1), announce(&too_many), "too many capabilities"),
        (
            ext(&m1),
            Msg {
                target: "capability".into(),
                payload: b"definitely-not-json".to_vec(),
            },
            "expected value",
        ),
    ];

    for (height, (origin, msg, needle)) in rejects.into_iter().enumerate() {
        let height = height as u64 + 2;
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
        let SubmitError::Rejected(Error::Module {
            reason: n_reason,
            sentence: n_msg,
        }) = n_err
        else {
            panic!("native rejection shape: {n_err:?}");
        };
        let SubmitError::Rejected(Error::Module {
            reason: w_reason,
            sentence: w_msg,
        }) = w_err
        else {
            panic!("wasm rejection shape: {w_err:?}");
        };
        assert!(n_msg.contains(needle), "native reason: {n_msg}");
        assert_eq!(n_reason, w_reason, "wasm token must match the native token");
        assert!(
            w_msg.contains(needle),
            "wasm reason must carry the native reason: {w_msg}"
        );

        // abort leaves no trace: both roots byte-identical to pre-block.
        assert_eq!(root_of(&native), n_before, "native root moved on reject");
        assert_eq!(root_of(&wasm), w_before, "wasm root moved on reject");
        assert_eq!(root_of(&native), root_of(&wasm));
        assert_eq!(replies(&native, &keys).await, replies(&wasm, &keys).await);
    }
}

#[test]
fn multi_dispatch_block_reads_prior_writes_and_isolates_rejections() {
    deterministic::Runner::default().start(|context| async move {
        multi_dispatch_inner(&context).await;
    });
}

async fn multi_dispatch_inner(context: &deterministic::Context) {
    let (m1, m2, outsider) = (key(1), key(2), key(4));
    let validators = [m1.clone(), m2.clone()];
    let mut native = native_host(context, &validators).await;
    let mut wasm = wasm_host_(context, &validators).await;
    let keys = [m1.clone(), m2.clone(), outsider.clone()];

    // ONE block, two announces from DIFFERENT nodes: on the wasm side the
    // second dispatch reloads the first dispatch's staged `__state`, so its
    // whole-state save must CARRY m1's entry — the read-your-writes seam the
    // adapter relies on (were it broken, m1's announce would vanish) — while
    // each dispatch also resolves its two sibling queries through the
    // memoized replay.
    let batch = vec![
        (ext(&m1), announce(&["codex"])),
        (ext(&m2), announce(&["claude"])),
    ];
    let n_out = native
        .submit_block(block(1, ext(&m1)), batch.clone())
        .await
        .expect("native block");
    let w_out = wasm
        .submit_block(block(1, ext(&m1)), batch)
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
    assert_eq!(replies(&native, &keys).await, replies(&wasm, &keys).await);
    for host in [&native, &wasm] {
        assert_eq!(node_tags(host, &m1).await, vec!["codex"], "m1 survived");
        assert_eq!(node_tags(host, &m2).await, vec!["claude"]);
    }

    // ONE block, two announces from the SAME node: the last staged replace
    // wins — on both runtimes.
    let batch = vec![(ext(&m2), announce(&["a"])), (ext(&m2), announce(&["b"]))];
    let n_out = native
        .submit_block(block(2, ext(&m2)), batch.clone())
        .await
        .expect("native block");
    let w_out = wasm
        .submit_block(block(2, ext(&m2)), batch)
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
    assert_eq!(replies(&native, &keys).await, replies(&wasm, &keys).await);
    for host in [&native, &wasm] {
        assert_eq!(node_tags(host, &m2).await, vec!["b"], "last replace wins");
    }

    // ONE block where the SECOND member rejects — and the rejection is DECIDED
    // by the sibling reads (the outsider holds no standing in the valset the
    // wasm runtime queried): the runtime aborts the staged overlay and replays
    // the accepted member — committed state must equal the accepted subset
    // alone, on both runtimes.
    let (n_before, w_before) = (root_of(&native), root_of(&wasm));
    let batch = vec![
        (ext(&m1), announce(&["codex", "extra"])),
        (ext(&outsider), announce(&["claude"])),
    ];
    let n_out = native
        .submit_block(block(3, ext(&m1)), batch.clone())
        .await
        .expect("native block");
    let w_out = wasm
        .submit_block(block(3, ext(&m1)), batch)
        .await
        .expect("wasm block");
    for out in [&n_out, &w_out] {
        assert!(matches!(out.members[0], MemberOutcome::Applied { .. }));
        assert!(matches!(out.members[1], MemberOutcome::Rejected { .. }));
    }
    // the accepted member landed (roots moved), the rejected one left nothing.
    assert_ne!(root_of(&native), n_before);
    assert_ne!(root_of(&wasm), w_before);
    assert_eq!(root_of(&native), root_of(&wasm));
    assert_eq!(replies(&native, &keys).await, replies(&wasm, &keys).await);
    for host in [&native, &wasm] {
        assert_eq!(
            node_tags(host, &m1).await,
            vec!["codex", "extra"],
            "the accepted member survived the batch replay"
        );
        assert!(
            node_tags(host, &outsider).await.is_empty(),
            "a rejected member must leave no trace"
        );
    }
}

#[test]
fn class_claims_apply_identically_and_roots_move_in_lockstep() {
    deterministic::Runner::default().start(|context| async move {
        class_claims_inner(&context).await;
    });
}

async fn class_claims_inner(context: &deterministic::Context) {
    let m1 = key(1);
    let validators = [m1.clone()];
    let mut native = native_host(context, &validators).await;
    let mut wasm = wasm_host_(context, &validators).await;
    let keys = [m1.clone()];

    // every class-op family, in one deterministic sequence. `moves` says
    // whether the op changes committed capability state — root movement must
    // agree on BOTH sides, and the read matrix (which now carries
    // ResolveClass and Classes) must answer identically after every block.
    let ops: Vec<(Origin, Msg, bool)> = vec![
        // h1: a MODULE claims a class — the claimant is the verified module
        // origin threading through the wasm runtime's env.
        (Origin::Module("agent-app".into()), claim("agent"), true),
        // h2: a re-claim by the OWNING module is an idempotent no-op — the
        // root must hold on BOTH runtimes (nothing is staged).
        (Origin::Module("agent-app".into()), claim("agent"), false),
        // h3: a second class from a different module lands beside the first.
        (Origin::Module("saga".into()), claim("ai"), true),
        // h4: announcements and claims share the registry — a member announce
        // moves the same root the claims move.
        (ext(&m1), announce(&["codex"]), true),
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

        assert_eq!(
            replies(&native, &keys).await,
            replies(&wasm, &keys).await,
            "replies diverge after block {height}"
        );
        if moves {
            assert_ne!(root_of(&native), n_before, "native root stuck at {height}");
            assert_ne!(root_of(&wasm), w_before, "wasm root stuck at {height}");
        } else {
            assert_eq!(root_of(&native), n_before, "native root moved at {height}");
            assert_eq!(root_of(&wasm), w_before, "wasm root moved at {height}");
        }
        // THE continuity property: both roots ARE the same store root.
        assert_eq!(root_of(&native), root_of(&wasm));
    }

    // decoded spot checks on BOTH sides: the router view is identical.
    for host in [&native, &wasm] {
        assert_eq!(
            class_owner(host, "agent").await.as_deref(),
            Some("agent-app")
        );
        assert_eq!(class_owner(host, "ai").await.as_deref(), Some("saga"));
        assert_eq!(class_owner(host, "absent").await, None);
        assert_eq!(
            classes_of(host).await,
            vec![
                ("agent".to_string(), "agent-app".to_string()),
                ("ai".to_string(), "saga".to_string()),
            ],
            "Classes enumerates sorted"
        );
        assert_eq!(node_tags(host, &m1).await, vec!["codex"], "announce landed");
    }
}

#[test]
fn class_claim_rejections_match_and_leave_no_trace() {
    deterministic::Runner::default().start(|context| async move {
        class_claim_rejections_inner(&context).await;
    });
}

async fn class_claim_rejections_inner(context: &deterministic::Context) {
    let m1 = key(1);
    let validators = [m1.clone()];
    let mut native = native_host(context, &validators).await;
    let mut wasm = wasm_host_(context, &validators).await;
    let keys = [m1.clone()];

    // seed a committed claim so the rival case has a class to collide with.
    for host in [&mut native, &mut wasm] {
        host.submit_at(block(1, Origin::Module("agent-app".into())), claim("agent"))
            .await
            .expect("seed claim");
    }

    let too_long = "c".repeat(33);
    // the class rejection matrix: the rival-module claim (first claim wins),
    // both non-module origins ("a class is claimed by the module that serves
    // it"), and every class-shape refusal — the separators `:` and `/` are
    // rejected as characters, so a claim can never smuggle address structure.
    let rejects: Vec<(Origin, Msg, &str)> = vec![
        (
            Origin::Module("rival".into()),
            claim("agent"),
            "already claimed",
        ),
        (ext(&m1), claim("fresh"), "module that serves it"),
        (Origin::System, claim("fresh"), "module that serves it"),
        (
            Origin::Module("agent-app".into()),
            claim(""),
            "must be non-empty",
        ),
        (
            Origin::Module("agent-app".into()),
            claim("Agent"),
            "invalid characters",
        ),
        (
            Origin::Module("agent-app".into()),
            claim("agent:sub"),
            "invalid characters",
        ),
        (
            Origin::Module("agent-app".into()),
            claim("agent/sub"),
            "invalid characters",
        ),
        (
            Origin::Module("agent-app".into()),
            claim(too_long.as_str()),
            "exceeds 32 bytes",
        ),
    ];

    for (height, (origin, msg, needle)) in rejects.into_iter().enumerate() {
        let height = height as u64 + 2;
        let (n_before, w_before) = (root_of(&native), root_of(&wasm));

        let n_err = native
            .submit_at(block(height, origin.clone()), msg.clone())
            .await
            .expect_err("native must reject");
        let w_err = wasm
            .submit_at(block(height, origin), msg)
            .await
            .expect_err("wasm must reject");

        let SubmitError::Rejected(Error::Module {
            reason: n_reason,
            sentence: n_msg,
        }) = n_err
        else {
            panic!("native rejection shape: {n_err:?}");
        };
        let SubmitError::Rejected(Error::Module {
            reason: w_reason,
            sentence: w_msg,
        }) = w_err
        else {
            panic!("wasm rejection shape: {w_err:?}");
        };
        assert!(n_msg.contains(needle), "native reason: {n_msg}");
        assert_eq!(n_reason, w_reason, "wasm token must match the native token");
        assert!(
            w_msg.contains(needle),
            "wasm reason must carry the native reason: {w_msg}"
        );

        assert_eq!(root_of(&native), n_before, "native root moved on reject");
        assert_eq!(root_of(&wasm), w_before, "wasm root moved on reject");
        assert_eq!(replies(&native, &keys).await, replies(&wasm, &keys).await);
    }

    // the seeded claim survived every rejection, untouched, on both sides.
    for host in [&native, &wasm] {
        assert_eq!(
            class_owner(host, "agent").await.as_deref(),
            Some("agent-app")
        );
        assert_eq!(class_owner(host, "fresh").await, None);
    }
}

#[test]
fn class_claims_multi_dispatch_block_reads_prior_claims() {
    deterministic::Runner::default().start(|context| async move {
        class_multi_dispatch_inner(&context).await;
    });
}

async fn class_multi_dispatch_inner(context: &deterministic::Context) {
    let m1 = key(1);
    let validators = [m1.clone()];
    let mut native = native_host(context, &validators).await;
    let mut wasm = wasm_host_(context, &validators).await;
    let keys = [m1.clone()];

    // ONE block, three dispatches: alpha claims "agent"; beta's rival claim on
    // the SAME class must be rejected by reading alpha's claim STAGED IN THIS
    // BLOCK (on the wasm side dispatch 2 reloads dispatch 1's staged
    // `__state` — read-your-writes is what decides the rejection); beta's
    // claim on a free class then applies, reading through the same overlay.
    let (n_before, w_before) = (root_of(&native), root_of(&wasm));
    let batch = vec![
        (Origin::Module("alpha".into()), claim("agent")),
        (Origin::Module("beta".into()), claim("agent")),
        (Origin::Module("beta".into()), claim("ai")),
    ];
    let n_out = native
        .submit_block(block(1, Origin::Module("alpha".into())), batch.clone())
        .await
        .expect("native block");
    let w_out = wasm
        .submit_block(block(1, Origin::Module("alpha".into())), batch)
        .await
        .expect("wasm block");
    for out in [&n_out, &w_out] {
        assert!(
            matches!(out.members[0], MemberOutcome::Applied { .. }),
            "alpha's claim must apply: {:?}",
            out.members
        );
        assert!(
            matches!(out.members[1], MemberOutcome::Rejected { .. }),
            "beta's rival claim must reject against the SAME-BLOCK stage: {:?}",
            out.members
        );
        assert!(
            matches!(out.members[2], MemberOutcome::Applied { .. }),
            "beta's free-class claim must apply: {:?}",
            out.members
        );
    }

    // the accepted claims landed (roots moved), the rejected rival left no
    // trace, and both runtimes serve the identical router view.
    assert_ne!(root_of(&native), n_before);
    assert_ne!(root_of(&wasm), w_before);
    assert_eq!(root_of(&native), root_of(&wasm));
    assert_eq!(replies(&native, &keys).await, replies(&wasm, &keys).await);
    for host in [&native, &wasm] {
        assert_eq!(class_owner(host, "agent").await.as_deref(), Some("alpha"));
        assert_eq!(class_owner(host, "ai").await.as_deref(), Some("beta"));
        assert_eq!(
            classes_of(host).await,
            vec![
                ("agent".to_string(), "alpha".to_string()),
                ("ai".to_string(), "beta".to_string()),
            ]
        );
    }
}

#[test]
fn an_empty_wasm_roster_without_valset_answers_queries_and_refuses_announcements() {
    deterministic::Runner::default().start(|context| async move {
        let native = native_capability(Box::new(cap_store(&context, "empty_native").await));
        let wasm = wasm_capability(Box::new(cap_store(&context, "empty_wasm").await));
        for module in [Box::new(native) as Box<dyn sdk::Module>, Box::new(wasm)] {
            let mut host = Host::genesis(vec![module]).unwrap();
            assert!(providers(&host, "codex").await.is_empty());
            let query = encode_query(&CapabilityQuery::CapableProviders {
                capability: "codex".into(),
                demands: Default::default(),
            });
            assert_eq!(
                decode_reply(&host.query("capability", &query).await.unwrap()).unwrap(),
                CapabilityReply::Providers(vec![])
            );
            assert!(
                host.submit_at(block(1, ext(&[7; 32])), announce(&["codex"]))
                    .await
                    .is_err()
            );
        }
    });
}

#[test]
fn wasm_provider_queries_exclude_a_member_who_left_after_announcing() {
    deterministic::Runner::default().start(|context| async move {
        let departing = key(1);
        let validators = [departing.clone(), key(2)];
        let native = native_host(&context, &validators).await;
        let wasm = wasm_host_(&context, &validators).await;
        for mut host in [native, wasm] {
            host.submit_at(block(1, ext(&departing)), announce(&["codex"]))
                .await
                .unwrap();
            assert_eq!(providers(&host, "codex").await, vec![departing.clone()]);
            let root = root_of(&host);
            host.submit_at(
                block(2, Origin::System),
                Msg {
                    target: "valset".into(),
                    payload: valset_encode_msg(&ValsetMsg::Leave {
                        key: departing.clone(),
                    }),
                },
            )
            .await
            .unwrap();
            assert_eq!(root_of(&host), root, "the old announcement remains stored");
            assert!(providers(&host, "codex").await.is_empty());
            let query = encode_query(&CapabilityQuery::CapableProviders {
                capability: "codex".into(),
                demands: Default::default(),
            });
            assert_eq!(
                decode_reply(&host.query("capability", &query).await.unwrap()).unwrap(),
                CapabilityReply::Providers(vec![])
            );
        }
    });
}
