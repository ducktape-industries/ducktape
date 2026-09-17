//! the STORE-BACKED cutover-continuity proof for identity: the `identity`
//! guest component over `WasmModule::with_store(QmdbStore)` and the native
//! `Identity` over the same store shape are ROOT-CONTINUOUS — the same op
//! sequence commits the IDENTICAL qmdb merkle root after every block (both
//! roots ARE the store's root; qmdb's batch canonicalizes mutations by hashed
//! key, so the native logical-key commit order and the wasm hashed-key drain
//! order produce the same op log).
//!
//! identity's per-network parameter is the CHAIN ID every add-key consent
//! folds in, which travels as GENESIS CONFIG — a `__config` RECORD seeded
//! into the qmdb store under `sdk::store_key` (the production
//! `seed_store_config` seam), read back by the guest's
//! `store_genesis_chain_id` per dispatch. BOTH runtimes' stores carry the
//! identical record here (root-continuity demands it; the native twin reads
//! its chain id from the constructor and simply carries the record in its
//! root). the config-in-the-root and config-governs-the-guest pins ride at
//! the end of this file.
//!
//! identity reads NO sibling: accounts are founded by the frame ORIGIN and
//! keys are admitted by an existing member's consent, so the hosts carry the
//! tenant alone. the consent verifies run IN the guest — ed25519 and the
//! WebAuthn passkey envelope (deterministic pure-Rust p256 on wasm32) — and
//! must answer byte-identically to the native module.

use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::PrivateKey;
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use host::{BlockContext, Host, MemberOutcome, SubmitError};
use identity::{
    Authorizer, IDENTITY_ADD_KEY_NS, Identity, IdentityMsg, IdentityQuery, KeyScheme,
    MAX_QUERY_LIMIT, add_key_preimage, encode_msg, encode_query,
};
use sdk::{Error, MerkleStore as _, Msg, Origin, StateRoot};
use statesync::qmdb::QmdbStore;
use wasm_host::WasmModule;

/// GENERATED artifact — built from the `identity` module's guest port by
/// guest-builder (`make wasm-modules`); committed so this proof is self-contained.
const IDENTITY_WASM: &[u8] = include_bytes!("fixtures/identity.component.wasm");

/// the chain id BOTH runtimes are constructed with — natively as a constructor
/// argument, on the wasm side through the store-seeded genesis config.
const CHAIN_ID: &str = "test-chain";

/// the WebAuthn relying party the passkey assertions are minted for.
const RP_ID: &str = "ducktape";

/// a fresh qmdb store carrying the seeded `__config` chain-id record —
/// exactly the production genesis seam (`bin/node/src/host_state.rs`
/// `seed_store_config`). BOTH runtimes' stores get the identical record:
/// root-continuity demands it, and the guest reads its chain id from it.
/// `label` doubles as the store id: the deterministic runtime keys storage
/// partitions by id alone (child labels do not namespace them), so a shared
/// id would make the second store REPLAY the first's journal. the id is not
/// part of the qmdb root, so distinct ids cost nothing.
async fn identity_store(
    context: &deterministic::Context,
    label: &'static str,
    chain_id: &str,
) -> QmdbStore<deterministic::Context> {
    let mut store = QmdbStore::init(context.child(label), label).await;
    let config = sdk::genesis_config::encode_config(&[("chain_id", chain_id.as_bytes())]);
    store
        .commit_batch(vec![(
            sdk::store_key(sdk::genesis_config::CONFIG_KEY),
            Some(config),
        )])
        .await
        .expect("seed genesis config");
    store
}

/// the wasm identity over the host-constructed (config-seeded) store —
/// exactly the production construction (`bin/node/src/host_state.rs`).
fn wasm_identity(store: Box<dyn sdk::MerkleStore>) -> WasmModule {
    WasmModule::with_store("identity", IDENTITY_WASM, store).expect("load component")
}

/// the production wiring, verbatim: identity under this network's chain id
/// (the native builder chain is what the guest compiles in).
fn native_identity(store: Box<dyn sdk::MerkleStore>) -> Identity {
    Identity::new("identity", store, CHAIN_ID.to_string())
}

async fn native_host(context: &deterministic::Context) -> Host {
    let store = identity_store(context, "native_id", CHAIN_ID).await;
    Host::genesis(vec![Box::new(native_identity(Box::new(store)))]).expect("genesis")
}

async fn wasm_host_(context: &deterministic::Context) -> Host {
    let store = identity_store(context, "wasm_id", CHAIN_ID).await;
    Host::genesis(vec![Box::new(wasm_identity(Box::new(store)))]).expect("genesis")
}

#[test]
fn compiled_reference_batches_are_positional_bounded_and_read_only() {
    deterministic::Runner::default().start(|context| async move {
        use identity::{AccountRef, IdentityReply};
        let mut native = native_host(&context).await;
        let mut wasm = wasm_host_(&context).await;
        for host in [&mut native, &mut wasm] {
            for number in 1..=2 {
                host.submit_at(
                    block(number, Origin::External(ed_pub(&ed(number)))),
                    create(&format!("person-{number}")),
                )
                .await
                .unwrap();
            }
        }
        let references = vec![
            AccountRef::Key(ed_pub(&ed(2))),
            AccountRef::Account(1),
            AccountRef::Account(0),
            AccountRef::Key(ed_pub(&ed(99))),
            AccountRef::Account(99),
            AccountRef::Key(ed_pub(&ed(1))),
            AccountRef::Account(2),
        ];
        let expected = vec![Some(2), Some(1), None, None, None, Some(1), Some(2)];
        for host in [&native, &wasm] {
            let settled = root_of(host);
            for (references, expected) in [
                (references.clone(), expected.clone()),
                (Vec::new(), Vec::new()),
                (
                    vec![AccountRef::Key(ed_pub(&ed(2))); MAX_QUERY_LIMIT as usize],
                    vec![Some(2); MAX_QUERY_LIMIT as usize],
                ),
            ] {
                let bytes = host
                    .query(
                        "identity",
                        &encode_query(&IdentityQuery::Resolve { references }),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    identity::decode_reply(&bytes).unwrap(),
                    IdentityReply::Resolved(expected)
                );
            }
            let error = host
                .query(
                    "identity",
                    &encode_query(&IdentityQuery::Resolve {
                        references: vec![AccountRef::Account(1); MAX_QUERY_LIMIT as usize + 1],
                    }),
                )
                .await
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("reference count exceeds query limit")
            );
            assert_eq!(root_of(host), settled);
        }
        assert_eq!(root_of(&native), root_of(&wasm));
    });
}

// ---- key builders (the shapes identity's own tests use) --------------------

type Ed = PrivateKey;

fn ed(seed: u64) -> Ed {
    Ed::from_seed(seed)
}
fn ed_pub(k: &Ed) -> Vec<u8> {
    k.public_key().as_ref().to_vec()
}
/// every consent in this file admits into account 1 (founder A's), and dies
/// well past the handful of heights driven here — `consensus_time` is
/// `1_000 + height`.
const ACCOUNT_A: u64 = 1;
const EXPIRES: u64 = 100_000;

/// an ed25519 member's consent to admit `new_key` (of `scheme`) at `gen`.
fn ed_consent(member: &Ed, scheme: KeyScheme, new_key: &[u8], generation: u64) -> Authorizer {
    ed_consent_for(member, scheme, new_key, generation, ACCOUNT_A)
}

fn ed_consent_for(
    member: &Ed,
    scheme: KeyScheme,
    new_key: &[u8],
    generation: u64,
    account: u64,
) -> Authorizer {
    let preimage = add_key_preimage(CHAIN_ID, scheme, new_key, generation, account, EXPIRES);
    Authorizer {
        key: ed_pub(member),
        account,
        expires_at: EXPIRES,
        proof: keyscheme::testkit::ed25519_proof(member, IDENTITY_ADD_KEY_NS, &preimage),
    }
}

// a WebAuthn passkey, synthesized exactly as an authenticator would produce
// it (keyscheme's testkit recipe). p256 signing is RFC-6979 deterministic, so
// the proof bytes are identical on every run — no OS randomness in this proof.
fn wa_key(seed: u8) -> p256::ecdsa::SigningKey {
    keyscheme::testkit::passkey(seed)
}
fn wa_pub(k: &p256::ecdsa::SigningKey) -> Vec<u8> {
    keyscheme::testkit::passkey_pubkey(k)
}
/// a passkey member's consent (a full WebAuthn assertion envelope) to admit
/// `new_key` (of `scheme`) at `gen`.
fn wa_consent(
    member: &p256::ecdsa::SigningKey,
    scheme: KeyScheme,
    new_key: &[u8],
    generation: u64,
) -> Authorizer {
    let preimage = add_key_preimage(CHAIN_ID, scheme, new_key, generation, ACCOUNT_A, EXPIRES);
    Authorizer {
        key: wa_pub(member),
        account: ACCOUNT_A,
        expires_at: EXPIRES,
        proof: keyscheme::testkit::passkey_proof(
            member,
            RP_ID,
            IDENTITY_ADD_KEY_NS,
            &preimage,
            true,
        ),
    }
}

// ---- op + query helpers -----------------------------------------------------

fn msg(m: &IdentityMsg) -> Msg {
    Msg {
        target: "identity".into(),
        payload: encode_msg(m),
    }
}

/// found an account for the ORIGIN (an ed25519 key).
fn create(name: &str) -> Msg {
    msg(&IdentityMsg::Create {
        name: name.into(),
        scheme: KeyScheme::Ed25519,
    })
}

/// admit the ORIGIN (of `scheme`) under `authorizer`'s consent.
fn add_key(scheme: KeyScheme, label: Option<&str>, authorizer: Authorizer) -> Msg {
    msg(&IdentityMsg::AddKey {
        scheme,
        label: label.map(str::to_string),
        authorizer,
    })
}

fn remove_key(key: &[u8]) -> Msg {
    msg(&IdentityMsg::RemoveKey { key: key.to_vec() })
}

fn set_name(name: &str) -> Msg {
    msg(&IdentityMsg::SetName { name: name.into() })
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
    h.module_root("identity").expect("identity registered")
}

/// the read matrix: every query family — the numbered listing (full, a
/// window, an empty window), per-account gets (present + absent), and both
/// per-key resolvers (the ownership index and the admission counter) over
/// every key the test knows about plus an absent one.
async fn replies(h: &Host, numbers: &[u64], keys: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut queries = vec![
        encode_query(&IdentityQuery::All {
            from: 0,
            limit: MAX_QUERY_LIMIT,
        }),
        encode_query(&IdentityQuery::All { from: 2, limit: 1 }),
        encode_query(&IdentityQuery::All { from: 0, limit: 0 }),
        encode_query(&IdentityQuery::Get { number: 99 }),
        encode_query(&IdentityQuery::OfKey {
            key: b"absent".to_vec(),
        }),
        encode_query(&IdentityQuery::KeyGen {
            key: b"absent".to_vec(),
        }),
    ];
    for n in numbers {
        queries.push(encode_query(&IdentityQuery::Get { number: *n }));
    }
    for k in keys {
        queries.push(encode_query(&IdentityQuery::OfKey { key: k.clone() }));
        queries.push(encode_query(&IdentityQuery::KeyGen { key: k.clone() }));
    }
    let mut out = Vec::new();
    for q in &queries {
        out.push(h.query("identity", q).await.expect("query"));
    }
    out
}

/// the fixed keyset one world uses; shared by the matrix helpers. founder A
/// and founder B each found an account (numbers 1 and 2); the joiner, the
/// passkey and the device are admitted into A's.
struct World {
    founder_a: Ed,
    founder_b: Ed,
    joiner: Ed,
    device: Ed,
    passkey: p256::ecdsa::SigningKey,
}

impl World {
    fn new() -> Self {
        Self {
            founder_a: ed(11),
            founder_b: ed(12),
            joiner: ed(21),
            device: ed(22),
            passkey: wa_key(0x42),
        }
    }

    fn numbers(&self) -> Vec<u64> {
        vec![1, 2]
    }
    fn keys(&self) -> Vec<Vec<u8>> {
        vec![
            ed_pub(&self.founder_a),
            ed_pub(&self.founder_b),
            ed_pub(&self.joiner),
            ed_pub(&self.device),
            wa_pub(&self.passkey),
        ]
    }
    fn a(&self) -> Origin {
        Origin::External(ed_pub(&self.founder_a))
    }
    fn b(&self) -> Origin {
        Origin::External(ed_pub(&self.founder_b))
    }
}

/// submit one accepted op to BOTH hosts and assert the parity invariants:
/// identical replies, lockstep root movement, and THE continuity property —
/// both roots are the same store root after every block.
async fn roundtrip(
    native: &mut Host,
    wasm: &mut Host,
    w: &World,
    height: u64,
    origin: Origin,
    m: Msg,
    moves: bool,
) {
    let (n_before, w_before) = (root_of(native), root_of(wasm));
    native
        .submit_at(block(height, origin.clone()), m.clone())
        .await
        .expect("native submit");
    wasm.submit_at(block(height, origin), m)
        .await
        .expect("wasm submit");
    assert_eq!(
        replies(native, &w.numbers(), &w.keys()).await,
        replies(wasm, &w.numbers(), &w.keys()).await,
        "replies diverge after block {height}"
    );
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

#[test]
fn same_ops_same_replies_roots_in_lockstep_and_continuous() {
    deterministic::Runner::default().start(|context| async move {
        same_ops_inner(&context).await;
    });
}

async fn same_ops_inner(context: &deterministic::Context) {
    let w = World::new();

    let mut native = native_host(context).await;
    let mut wasm = wasm_host_(context).await;

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
    assert_eq!(
        replies(&native, &w.numbers(), &w.keys()).await,
        replies(&wasm, &w.numbers(), &w.keys()).await,
        "empty registries answer identically"
    );

    // h1: founder A founds account 1 from its own key (the frame signature is
    // the possession proof; no sibling read anywhere).
    roundtrip(&mut native, &mut wasm, &w, 1, w.a(), create("alice"), true).await;

    // h2: founder B founds account 2 — names trim, numbers are monotonic.
    roundtrip(&mut native, &mut wasm, &w, 2, w.b(), create("  bob "), true).await;

    // h3: the joiner admits ITSELF into A under founder A's consent at the
    // joiner's generation 0 (the ed25519 consent verifies IN the guest).
    roundtrip(
        &mut native,
        &mut wasm,
        &w,
        3,
        Origin::External(ed_pub(&w.joiner)),
        add_key(
            KeyScheme::Ed25519,
            Some("laptop"),
            ed_consent(&w.founder_a, KeyScheme::Ed25519, &ed_pub(&w.joiner), 0),
        ),
        true,
    )
    .await;

    // h4: a WebAuthn PASSKEY joins A — a Secp256r1 origin under the founder's
    // ed25519 consent over the passkey's scheme tag.
    roundtrip(
        &mut native,
        &mut wasm,
        &w,
        4,
        Origin::External(wa_pub(&w.passkey)),
        add_key(
            KeyScheme::Secp256r1,
            Some("phone"),
            ed_consent(&w.founder_a, KeyScheme::Secp256r1, &wa_pub(&w.passkey), 0),
        ),
        true,
    )
    .await;

    // h5: the PASSKEY authorizes a device key — the raw ECDSA-P256 assertion
    // envelope (authData ‖ SHA256(clientDataJSON)) verifies inside the wasm
    // guest, byte-identically to the native module.
    roundtrip(
        &mut native,
        &mut wasm,
        &w,
        5,
        Origin::External(ed_pub(&w.device)),
        add_key(
            KeyScheme::Ed25519,
            None,
            wa_consent(&w.passkey, KeyScheme::Ed25519, &ed_pub(&w.device), 0),
        ),
        true,
    )
    .await;

    // h6: a non-founding member (the passkey, admitted at h4) evicts the
    // device it admitted at h5 — membership, not founding, is the authority,
    // and a key only drops one admitted no earlier than itself. the device's
    // generation stays at 1.
    roundtrip(
        &mut native,
        &mut wasm,
        &w,
        6,
        Origin::External(wa_pub(&w.passkey)),
        remove_key(&ed_pub(&w.device)),
        true,
    )
    .await;

    // h7: a member renames the account (origin-gated, no proof — updated_at
    // moves, so the root moves).
    roundtrip(
        &mut native,
        &mut wasm,
        &w,
        7,
        Origin::External(wa_pub(&w.passkey)),
        set_name("  Kim  "),
        true,
    )
    .await;

    // decoded spot check on the wasm side: the surviving association is the
    // founder + the passkey + the joiner, the name trimmed, the device
    // unlinked at generation 1.
    let reply = wasm
        .query("identity", &encode_query(&IdentityQuery::Get { number: 1 }))
        .await
        .expect("get 1");
    let identity::IdentityReply::Account(Some(acc)) =
        identity::decode_reply(&reply).expect("decode")
    else {
        panic!("account 1 must exist");
    };
    assert_eq!(acc.number, 1);
    assert_eq!(acc.name, "Kim");
    let mut survivors = vec![ed_pub(&w.founder_a), ed_pub(&w.joiner), wa_pub(&w.passkey)];
    survivors.sort();
    let keys: Vec<Vec<u8>> = acc.keys.iter().map(|k| k.pubkey.clone()).collect();
    assert_eq!(keys, survivors, "ascending by public key");
    let reply = wasm
        .query(
            "identity",
            &encode_query(&IdentityQuery::OfKey {
                key: ed_pub(&w.device),
            }),
        )
        .await
        .expect("of_key device");
    assert_eq!(
        identity::decode_reply(&reply).expect("decode"),
        identity::IdentityReply::Account(None)
    );
    let reply = wasm
        .query(
            "identity",
            &encode_query(&IdentityQuery::KeyGen {
                key: ed_pub(&w.device),
            }),
        )
        .await
        .expect("key_gen device");
    assert_eq!(
        identity::decode_reply(&reply).expect("decode"),
        identity::IdentityReply::Gen(1),
        "removal keeps the admission counter"
    );

    // error-shaped queries reject identically too (needle containment — the
    // wasm runtime wraps the native reason in its wit-error rendering).
    let junk = b"definitely-not-json".to_vec();
    let n_err = native
        .query("identity", &junk)
        .await
        .expect_err("native rejects");
    let w_err = wasm
        .query("identity", &junk)
        .await
        .expect_err("wasm rejects");
    let Error::Module {
        reason: n_reason,
        sentence: n_msg,
    } = n_err
    else {
        panic!("native query error shape: {n_err:?}");
    };
    let Error::Module {
        reason: w_reason,
        sentence: w_msg,
    } = w_err
    else {
        panic!("wasm query error shape: {w_err:?}");
    };
    assert!(n_msg.contains("expected value"), "native reason: {n_msg}");
    assert_eq!(n_reason, w_reason, "wasm token must match the native token");
    assert!(w_msg.contains("expected value"), "wasm reason: {w_msg}");

    // queries are read-only on the wasm side too.
    let settled = root_of(&wasm);
    let _ = replies(&wasm, &w.numbers(), &w.keys()).await;
    assert_eq!(root_of(&wasm), settled, "a query moved the wasm root");
}

#[test]
fn rejections_match_and_leave_no_trace() {
    deterministic::Runner::default().start(|context| async move {
        rejections_inner(&context).await;
    });
}

async fn rejections_inner(context: &deterministic::Context) {
    let w = World::new();
    let stranger = ed(99);
    let unfounded = ed(31);

    let mut native = native_host(context).await;
    let mut wasm = wasm_host_(context).await;

    // seed both worlds identically: A is founded, the joiner admitted at
    // generation 0 and evicted again — its counter is now 1 and A is back to
    // its single founding key.
    let spent = ed_consent(&w.founder_a, KeyScheme::Ed25519, &ed_pub(&w.joiner), 0);
    for host in [&mut native, &mut wasm] {
        host.submit_at(block(1, w.a()), create("alice"))
            .await
            .expect("found A");
        host.submit_at(
            block(2, Origin::External(ed_pub(&w.joiner))),
            add_key(KeyScheme::Ed25519, None, spent.clone()),
        )
        .await
        .expect("admit the joiner");
        host.submit_at(block(3, w.a()), remove_key(&ed_pub(&w.joiner)))
            .await
            .expect("evict the joiner");
    }

    // the rejection matrix: every distinct refusal family — the single-use
    // consent (replayed after eviction), scheme pinning, authorizer standing,
    // single ownership, membership invariants, member gating, name caps,
    // origin shapes, and the decode seam. each rejected block must leave BOTH
    // roots byte-identical (abort: no trace).
    let rejects: Vec<(Origin, Msg, &str)> = vec![
        // the spent consent: minted at generation 0, the joiner is at 1.
        (
            Origin::External(ed_pub(&w.joiner)),
            add_key(KeyScheme::Ed25519, None, spent),
            "consent does not verify",
        ),
        // a consent minted for a different scheme of the same bytes is a
        // forgery.
        (
            Origin::External(ed_pub(&unfounded)),
            add_key(
                KeyScheme::Ed25519,
                None,
                ed_consent(&w.founder_a, KeyScheme::Secp256r1, &ed_pub(&unfounded), 0),
            ),
            "consent does not verify",
        ),
        // a consent naming an account its author is not on: the account is
        // signature-covered, so naming another one is not a shortcut either.
        (
            Origin::External(ed_pub(&unfounded)),
            add_key(
                KeyScheme::Ed25519,
                None,
                ed_consent_for(&w.founder_a, KeyScheme::Ed25519, &ed_pub(&unfounded), 0, 2),
            ),
            "consent names account 2",
        ),
        // an authorizer on no account has nothing to admit into.
        (
            Origin::External(ed_pub(&unfounded)),
            add_key(
                KeyScheme::Ed25519,
                None,
                ed_consent(&stranger, KeyScheme::Ed25519, &ed_pub(&unfounded), 0),
            ),
            "authorizer belongs to no account",
        ),
        // single ownership: a member key cannot be admitted again ...
        (
            w.a(),
            add_key(
                KeyScheme::Ed25519,
                None,
                ed_consent(&w.founder_a, KeyScheme::Ed25519, &ed_pub(&w.founder_a), 0),
            ),
            "already belongs to an account",
        ),
        // ... nor found a second account.
        (w.a(), create("again"), "already belongs to an account"),
        // a malformed founding key for its declared scheme: 32 bytes as a
        // wallet, and 5 bytes as ed25519.
        (
            w.b(),
            msg(&IdentityMsg::Create {
                name: "x".into(),
                scheme: KeyScheme::Secp256k1,
            }),
            "founding key is malformed",
        ),
        (
            Origin::External(vec![7; 5]),
            create("x"),
            "founding key is malformed",
        ),
        // membership invariant: the last key can never be removed.
        (
            w.a(),
            remove_key(&ed_pub(&w.founder_a)),
            "cannot remove the last key",
        ),
        // member gating: a stranger renames nothing.
        (
            Origin::External(ed_pub(&stranger)),
            set_name("x"),
            "origin key belongs to no account",
        ),
        // an over-limit name from a member.
        (
            w.a(),
            set_name(&"x".repeat(65)),
            "exceeds the 64-byte limit",
        ),
        // origin shapes: system and empty-external submitters.
        (
            Origin::System,
            create("sys"),
            "origin-gated to external submitters",
        ),
        (
            Origin::External(Vec::new()),
            create("nobody"),
            "non-empty submitter",
        ),
        // the decode seam (from a member origin).
        (
            w.a(),
            Msg {
                target: "identity".into(),
                payload: b"definitely-not-json".to_vec(),
            },
            "expected value",
        ),
    ];

    for (height, (origin, m, needle)) in rejects.into_iter().enumerate() {
        let height = height as u64 + 4;
        let (n_before, w_before) = (root_of(&native), root_of(&wasm));

        let n_err = native
            .submit_at(block(height, origin.clone()), m.clone())
            .await
            .expect_err("native must reject");
        let w_err = wasm
            .submit_at(block(height, origin), m)
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
        assert_eq!(root_of(&native), root_of(&wasm));
        assert_eq!(
            replies(&native, &w.numbers(), &w.keys()).await,
            replies(&wasm, &w.numbers(), &w.keys()).await
        );
    }
}

#[test]
fn multi_dispatch_block_reads_prior_writes_and_isolates_rejections() {
    deterministic::Runner::default().start(|context| async move {
        multi_dispatch_inner(&context).await;
    });
}

async fn multi_dispatch_inner(context: &deterministic::Context) {
    let w = World::new();

    let mut native = native_host(context).await;
    let mut wasm = wasm_host_(context).await;

    // ONE block, two ops: the create founds account 1, and the SetName from
    // the SAME key reads that founding STAGED IN THIS BLOCK — on the wasm side
    // dispatch 2 reads dispatch 1's staged store writes through the host's
    // outer staged overlay (the read-your-writes seam).
    let batch = vec![(w.a(), create("alice")), (w.a(), set_name("quack"))];
    let n_out = native
        .submit_block(block(1, w.a()), batch.clone())
        .await
        .expect("native block");
    let w_out = wasm
        .submit_block(block(1, w.a()), batch)
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
    assert_eq!(root_of(&native), root_of(&wasm));
    assert_eq!(
        replies(&native, &w.numbers(), &w.keys()).await,
        replies(&wasm, &w.numbers(), &w.keys()).await
    );

    // ONE block, three members: B founds account 2, the joiner is admitted
    // into A, and the SAME admission replays — the replay is DECIDED by
    // read-your-writes (the staged key index already owns the joiner). the
    // runtime aborts the rejected member's overlay and replays the accepted
    // ones — committed state must equal the accepted subset alone, on both
    // runtimes.
    let (n_before, w_before) = (root_of(&native), root_of(&wasm));
    let consent = ed_consent(&w.founder_a, KeyScheme::Ed25519, &ed_pub(&w.joiner), 0);
    let batch = vec![
        (w.b(), create("bob")),
        (
            Origin::External(ed_pub(&w.joiner)),
            add_key(KeyScheme::Ed25519, None, consent.clone()),
        ),
        (
            Origin::External(ed_pub(&w.joiner)),
            add_key(KeyScheme::Ed25519, None, consent),
        ),
    ];
    let n_out = native
        .submit_block(block(2, w.b()), batch.clone())
        .await
        .expect("native block");
    let w_out = wasm
        .submit_block(block(2, w.b()), batch)
        .await
        .expect("wasm block");
    for out in [&n_out, &w_out] {
        assert!(matches!(out.members[0], MemberOutcome::Applied { .. }));
        assert!(matches!(out.members[1], MemberOutcome::Applied { .. }));
        assert!(
            matches!(out.members[2], MemberOutcome::Rejected { .. }),
            "the replayed admission must reject against the SAME-BLOCK stage: {:?}",
            out.members
        );
    }
    assert_ne!(root_of(&native), n_before);
    assert_ne!(root_of(&wasm), w_before);
    assert_eq!(root_of(&native), root_of(&wasm));
    assert_eq!(
        replies(&native, &w.numbers(), &w.keys()).await,
        replies(&wasm, &w.numbers(), &w.keys()).await
    );
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
    let here = identity_store(context, "cfg_here", CHAIN_ID).await;
    let same = identity_store(context, "cfg_same", CHAIN_ID).await;
    let other = identity_store(context, "cfg_other", "other-chain").await;
    assert_eq!(here.root(), same.root(), "same config, same genesis root");
    assert_ne!(
        here.root(),
        other.root(),
        "a different chain id IS a different genesis consensus state"
    );
    drop((here, same));

    // and the config GOVERNS the guest: the same add-key consent (minted for
    // CHAIN_ID) is accepted by the tenant configured with CHAIN_ID and
    // deterministically refused by the tenant configured with another chain —
    // proof the parameter actually reaches the ported logic, not just the root.
    let w = World::new();
    let mut wasm = wasm_host_(context).await;
    let mut other_host =
        Host::genesis(vec![Box::new(wasm_identity(Box::new(other)))]).expect("genesis");
    for host in [&mut wasm, &mut other_host] {
        host.submit_at(block(1, w.a()), create("alice"))
            .await
            .expect("founding reads no chain id");
    }

    let m = add_key(
        KeyScheme::Ed25519,
        None,
        ed_consent(&w.founder_a, KeyScheme::Ed25519, &ed_pub(&w.joiner), 0),
    );
    let joiner = Origin::External(ed_pub(&w.joiner));
    wasm.submit_at(block(2, joiner.clone()), m.clone())
        .await
        .expect("the configured chain accepts its own consent");
    let err = other_host
        .submit_at(block(2, joiner), m)
        .await
        .expect_err("a foreign chain id must refuse the consent");
    let SubmitError::Rejected(Error::Module { reason, sentence }) = err else {
        panic!("rejection shape: {err:?}");
    };
    assert_eq!(
        reason, "consent_unverified",
        "the chain-scoped preimage is what refuses"
    );
    assert!(
        sentence.contains("does not verify"),
        "the chain-scoped preimage is what refuses: {sentence}"
    );
}
