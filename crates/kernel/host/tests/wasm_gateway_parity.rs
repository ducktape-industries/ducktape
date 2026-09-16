//! the STORE-BACKED cutover-continuity proof for the MERGED gateway module:
//! the `gateway` guest component over `WasmModule::with_store(QmdbStore)` and
//! the native `Gateway` over the same store shape are ROOT-CONTINUOUS — the
//! same op sequence commits the IDENTICAL qmdb merkle root after every block
//! (both roots ARE the store's root).
//!
//! gateway now owns the WHOLE `.duck` name → AccountId → route pipeline: BOTH
//! the route plane AND the `.duck` handle plane absorbed from the retired
//! `duckdns` module (which had its own `duckdns` guest + parity proof —
//! both merged here). the route-plane tests below cover SetRoute; the
//! handle-plane test covers SetHandle / Resolve on the SAME merged tenant.
//!
//! like identity, gateway's constructor takes the PER-NETWORK chain id, which
//! travels as GENESIS CONFIG — a `__config` RECORD seeded into the qmdb store
//! under `sdk::store_key` (the production `seed_store_config` seam), read
//! back by the guest's `store_genesis_chain_id` per dispatch. BOTH runtimes'
//! stores carry the identical record; the config-in-the-root and
//! config-governs-the-guest pins ride at the end of this file.
//!
//! every gateway execute depends on ONE sibling read: the identity `OfKey`
//! resolution of the origin (a USER key) to its account, plus the
//! current-member signer check over that account's keys. both hosts therefore
//! carry the REAL native `identity::Identity` wired exactly as production, so
//! on the wasm side every acceptance and every gating rejection resolves
//! through the runtime's memoized replay. the WASM host carries NATIVE
//! identity: each parity proof isolates ONE wasm tenant. no valset: a node
//! key never resolves to an account, and standing is not a gateway concern.

use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::PrivateKey;
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use gateway::{
    DuckDnsName, GATEWAY_ROUTE_NS, Gateway, GatewayMsg, GatewayQuery, GatewayReply,
    MAX_QUERY_LIMIT, MemberAuthorization, ResolvedAccount, RouteAudience, RouteDefinition,
    RouteMethod, RouteName, RoutePolicy, RouteStatement, RouteTarget, decode_reply, encode_msg,
    encode_query, route_signing_preimage,
};
use host::{BlockContext, Host, MemberOutcome, SubmitError};
use identity::{Identity, IdentityMsg, KeyScheme};
use sdk::{Error, MerkleStore as _, Msg, Origin, StateRoot};
use statesync::qmdb::QmdbStore;
use wasm_host::WasmModule;

/// GENERATED artifact — built from the `gateway` module's guest port by
/// guest-builder (`make wasm-modules`); committed so this proof is self-contained.
const GATEWAY_WASM: &[u8] = include_bytes!("fixtures/gateway.component.wasm");

/// the chain id BOTH runtimes are constructed with — natively as a constructor
/// argument, on the wasm side as the host-installed genesis config. the
/// identity sibling runs under the same id (one network, one chain id).
const CHAIN_ID: &str = "test-chain";

/// the account numbers the two founders receive, in founding order.
const ACCOUNT_A: u64 = 1;
const ACCOUNT_B: u64 = 2;
/// a number no founding ever reaches in these worlds.
const ABSENT_ACCOUNT: u64 = 99;

/// a fresh qmdb store carrying the seeded `__config` chain-id record —
/// exactly the production genesis seam (`bin/node/src/host_state.rs`
/// `seed_store_config`). BOTH runtimes' stores get the identical record:
/// root-continuity demands it, and the guest reads its chain id from it.
/// `label` doubles as the store id (the deterministic runtime keys storage
/// partitions by id alone).
async fn gw_store(
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

/// the wasm gateway over the host-constructed (config-seeded) store —
/// exactly the production construction (`bin/node/src/host_state.rs`).
fn wasm_gateway(store: Box<dyn sdk::MerkleStore>) -> WasmModule {
    WasmModule::with_store("gateway", GATEWAY_WASM, store).expect("load component")
}

/// the production wiring, verbatim (`bin/node/src/host_state.rs`).
fn native_gateway(store: Box<dyn sdk::MerkleStore>) -> Gateway {
    Gateway::new("gateway", store, "identity", CHAIN_ID)
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

async fn native_host(context: &deterministic::Context) -> Host {
    let store = gw_store(context, "native_gw", CHAIN_ID).await;
    Host::genesis(vec![
        Box::new(native_gateway(Box::new(store))),
        Box::new(native_identity()),
    ])
    .expect("genesis")
}

async fn wasm_host_(context: &deterministic::Context) -> Host {
    let store = gw_store(context, "wasm_gw", CHAIN_ID).await;
    Host::genesis(vec![
        Box::new(wasm_gateway(Box::new(store))),
        Box::new(native_identity()),
    ])
    .expect("genesis")
}

type Ed = PrivateKey;

fn ed(seed: u64) -> Ed {
    Ed::from_seed(seed)
}
fn ed_pub(k: &Ed) -> Vec<u8> {
    k.public_key().as_ref().to_vec()
}

/// an identity Create op founding an account for the submitting (founder)
/// key — the flow identity's tests drive, here to seed the REAL identity
/// sibling both gateways read through.
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

// ---- route builders ---------------------------------------------------------

/// a 64-char lowercase hex manifest digest, distinct per seed.
fn manifest_hex(seed: u8) -> String {
    format!("{seed:02x}").repeat(32)
}

/// a static-content route: GET+HEAD, bodyless, bounded response — the shape
/// `validate_route` demands of DuckFS targets.
fn content_route(seed: u8) -> RouteDefinition {
    RouteDefinition {
        target: RouteTarget::DuckFs {
            manifest_sha256: manifest_hex(seed),
        },
        policy: RoutePolicy {
            audience: RouteAudience::Network,
            methods: vec![RouteMethod::Get, RouteMethod::Head],
            max_request_bytes: Some(0),
            max_response_bytes: 1024 * 1024,
            allow_authorization: false,
            allow_upgrade: false,
        },
    }
}

/// a loopback-HTTP route with a body-bearing method, streaming responses and
/// websocket upgrade — the other target family.
fn loopback_route() -> RouteDefinition {
    RouteDefinition {
        target: RouteTarget::LoopbackHttp,
        policy: RoutePolicy {
            audience: RouteAudience::Owner,
            methods: vec![RouteMethod::Get, RouteMethod::Post],
            max_request_bytes: Some(1024),
            max_response_bytes: 0,
            allow_authorization: false,
            allow_upgrade: true,
        },
    }
}

fn statement(
    account_id: u64,
    label: Option<&str>,
    publisher: &[u8],
    revision: u64,
    route: Option<RouteDefinition>,
) -> RouteStatement {
    RouteStatement {
        chain_id: CHAIN_ID.into(),
        account_id,
        name: match label {
            Some(l) => RouteName::named(l),
            None => RouteName::apex(),
        },
        publisher_node: publisher.to_vec(),
        revision,
        route,
    }
}

/// a SetRoute msg whose statement is signed for real by `signer` (an Ed25519
/// account member) over the canonical preimage.
fn set_route(st: RouteStatement, signer: &Ed) -> Msg {
    let preimage = route_signing_preimage(&st).expect("statement validates");
    let authorization = MemberAuthorization {
        signer: ed_pub(signer),
        signature: signer.sign(GATEWAY_ROUTE_NS, &preimage).as_ref().to_vec(),
    };
    Msg {
        target: "gateway".into(),
        payload: encode_msg(&GatewayMsg::SetRoute {
            statement: st,
            authorization,
        }),
    }
}

/// a SetRoute msg with an EXPLICIT authorization — for statements the
/// canonical preimage refuses to encode (bad label, revision 0, a policy the
/// target forbids), where the module's own validation is the thing under test.
fn set_route_raw(st: RouteStatement, signer: Vec<u8>, signature: Vec<u8>) -> Msg {
    Msg {
        target: "gateway".into(),
        payload: encode_msg(&GatewayMsg::SetRoute {
            statement: st,
            authorization: MemberAuthorization { signer, signature },
        }),
    }
}

fn root_of(h: &Host) -> StateRoot {
    h.module_root("gateway").expect("gateway registered")
}

/// the read matrix: exact-name gets (present, tombstoned, absent) and the
/// management listings for both accounts plus an absent one.
async fn replies(h: &Host, accounts: &[u64]) -> Vec<Vec<u8>> {
    let mut queries = Vec::new();
    for a in accounts {
        for label in [None, Some("api"), Some("web"), Some("multi"), Some("iso")] {
            queries.push(encode_query(&GatewayQuery::Get {
                account_id: *a,
                name: match label {
                    Some(l) => RouteName::named(l),
                    None => RouteName::apex(),
                },
            }));
        }
        queries.push(encode_query(&GatewayQuery::List { account_id: *a }));
    }
    queries.push(encode_query(&GatewayQuery::Get {
        account_id: ABSENT_ACCOUNT,
        name: RouteName::apex(),
    }));
    queries.push(encode_query(&GatewayQuery::List {
        account_id: ABSENT_ACCOUNT,
    }));
    let mut out = Vec::new();
    for q in &queries {
        out.push(h.query("gateway", q).await.expect("query"));
    }
    out
}

/// the world both tests stand up: founder A founds account 1 and founder B
/// account 2 (every gateway op's ORIGIN is one of these USER keys); node A and
/// node B are the 32-byte node keys the statements name as publishers — an
/// account vouches for whichever node it names, no node is bound to anything.
struct World {
    node_a: Vec<u8>,
    node_b: Vec<u8>,
    founder_a: Ed,
    founder_b: Ed,
}

impl World {
    fn new() -> Self {
        Self {
            node_a: ed_pub(&ed(1)),
            node_b: ed_pub(&ed(2)),
            founder_a: ed(11),
            founder_b: ed(12),
        }
    }
    fn a_id(&self) -> u64 {
        ACCOUNT_A
    }
    fn b_id(&self) -> u64 {
        ACCOUNT_B
    }
    fn a(&self) -> Origin {
        Origin::External(ed_pub(&self.founder_a))
    }
    fn b(&self) -> Origin {
        Origin::External(ed_pub(&self.founder_b))
    }
    fn accounts(&self) -> Vec<u64> {
        vec![self.a_id(), self.b_id()]
    }

    /// stand up the shared sibling on one host. these blocks touch only the
    /// NATIVE identity — the gateway root must hold through all of them.
    async fn seed(&self, host: &mut Host) {
        host.submit_at(block(1, self.a()), create("alice"))
            .await
            .expect("found account A");
        host.submit_at(block(2, self.b()), create("bob"))
            .await
            .expect("found account B");
    }
}

/// submit one accepted op to BOTH hosts and assert the parity invariants.
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
        replies(native, &w.accounts()).await,
        replies(wasm, &w.accounts()).await,
        "replies diverge after block {height}"
    );
    assert_eq!(
        native.module_root("identity"),
        wasm.module_root("identity"),
        "the native identity sibling diverged"
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

    // sibling-only seeding (the two foundings) holds the gateway roots.
    let (n0, w0) = (root_of(&native), root_of(&wasm));
    w.seed(&mut native).await;
    w.seed(&mut wasm).await;
    assert_eq!(root_of(&native), n0, "sibling blocks hold the native root");
    assert_eq!(root_of(&wasm), w0, "sibling blocks hold the wasm root");

    // h5: founder A publishes its apex content route naming node A (the
    // identity OfKey + signer-membership gate resolves through the wasm
    // runtime's memoized replay; the route signature verifies IN the guest).
    roundtrip(
        &mut native,
        &mut wasm,
        &w,
        5,
        w.a(),
        set_route(
            statement(w.a_id(), None, &w.node_a, 1, Some(content_route(0x11))),
            &w.founder_a,
        ),
        true,
    )
    .await;

    // h6: founder B publishes a labeled loopback route on node B.
    roundtrip(
        &mut native,
        &mut wasm,
        &w,
        6,
        w.b(),
        set_route(
            statement(w.b_id(), Some("api"), &w.node_b, 1, Some(loopback_route())),
            &w.founder_b,
        ),
        true,
    )
    .await;

    // h7: a monotonic apex update (revision 2, new manifest).
    roundtrip(
        &mut native,
        &mut wasm,
        &w,
        7,
        w.a(),
        set_route(
            statement(w.a_id(), None, &w.node_a, 2, Some(content_route(0x22))),
            &w.founder_a,
        ),
        true,
    )
    .await;

    // h8: an authenticated TOMBSTONE (route None) — still advances the
    // revision stream, drops out of List, stays Get-able.
    roundtrip(
        &mut native,
        &mut wasm,
        &w,
        8,
        w.a(),
        set_route(statement(w.a_id(), None, &w.node_a, 3, None), &w.founder_a),
        true,
    )
    .await;

    // h9: republishing over the tombstone continues the same stream (rev 4).
    roundtrip(
        &mut native,
        &mut wasm,
        &w,
        9,
        w.a(),
        set_route(
            statement(w.a_id(), None, &w.node_a, 4, Some(loopback_route())),
            &w.founder_a,
        ),
        true,
    )
    .await;

    // h10: a second label under A lands beside the apex — served by node B:
    // the account vouches for whichever node it names, the origin is never
    // compared to the publisher.
    roundtrip(
        &mut native,
        &mut wasm,
        &w,
        10,
        w.a(),
        set_route(
            statement(
                w.a_id(),
                Some("web"),
                &w.node_b,
                1,
                Some(content_route(0x33)),
            ),
            &w.founder_a,
        ),
        true,
    )
    .await;

    // decoded spot check on the wasm side: the apex is back live at rev 4 as
    // loopback, "web" serves content, and B's listing carries exactly "api".
    let reply = wasm
        .query(
            "gateway",
            &encode_query(&GatewayQuery::List {
                account_id: w.a_id(),
            }),
        )
        .await
        .expect("list A");
    let gateway::GatewayReply::Routes(routes) = gateway::decode_reply(&reply).expect("decode")
    else {
        panic!("expected Routes reply");
    };
    assert_eq!(routes.len(), 2, "apex + web are live");
    assert_eq!(routes[0].name, RouteName::apex());
    assert_eq!(routes[0].revision, 4);
    assert_eq!(routes[0].target, "loopback_http");
    assert_eq!(routes[1].name, RouteName::named("web"));

    // error-shaped queries reject identically (needle containment).
    for (q, needle) in [
        (
            encode_query(&GatewayQuery::List { account_id: 0 }),
            "account number must be non-zero",
        ),
        (b"definitely-not-json".to_vec(), "expected value"),
    ] {
        let n_err = native
            .query("gateway", &q)
            .await
            .expect_err("native rejects");
        let w_err = wasm.query("gateway", &q).await.expect_err("wasm rejects");
        let Error::Module(n_msg) = n_err else {
            panic!("native query error shape: {n_err:?}");
        };
        let Error::Module(w_msg) = w_err else {
            panic!("wasm query error shape: {w_err:?}");
        };
        assert!(n_msg.contains(needle), "native query reason: {n_msg}");
        assert!(
            w_msg.contains(needle),
            "wasm query reason must carry the native reason: {w_msg}"
        );
    }

    // queries are read-only on the wasm side too.
    let settled = root_of(&wasm);
    let _ = replies(&wasm, &w.accounts()).await;
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
    let outsider = ed_pub(&ed(99));
    let outsider_signer = ed(99);
    let mut native = native_host(context).await;
    let mut wasm = wasm_host_(context).await;
    for host in [&mut native, &mut wasm] {
        w.seed(host).await;
        // one committed route so the revision matrix has a stream to violate.
        host.submit_at(
            block(5, w.a()),
            set_route(
                statement(w.a_id(), None, &w.node_a, 1, Some(content_route(0x11))),
                &w.founder_a,
            ),
        )
        .await
        .expect("seed route");
    }

    // a statement whose label the canonical grammar refuses (module-side
    // validation is under test, so the authorization is explicit junk).
    let bad_label = statement(
        w.a_id(),
        Some("Bad_Label"),
        &w.node_a,
        1,
        Some(loopback_route()),
    );
    let zero_revision = statement(w.a_id(), Some("api"), &w.node_a, 0, Some(loopback_route()));
    // a content route violating the signed content-policy shape (POST).
    let mut bad_content = content_route(0x44);
    bad_content.policy.methods = vec![RouteMethod::Get, RouteMethod::Post];
    bad_content.policy.max_request_bytes = Some(64);
    let bad_content_st = statement(w.a_id(), Some("api"), &w.node_a, 1, Some(bad_content));

    // the rejection matrix: the identity gate (an origin on no account — a
    // stranger's key AND a node key, which never resolves), chain scoping,
    // account/signer authority, signature verification, the monotonic
    // revision stream, statement grammar, origin shapes, and the decode
    // seam. each rejected block leaves BOTH roots byte-identical.
    let rejects: Vec<(Origin, Msg, &str)> = vec![
        // identity-gated: a key on no account — decided by the sibling read.
        (
            Origin::External(outsider.clone()),
            set_route(
                statement(ABSENT_ACCOUNT, None, &outsider, 1, Some(loopback_route())),
                &outsider_signer,
            ),
            "belongs to no Identity account",
        ),
        // identity-gated: a NODE key as origin never resolves to an account,
        // even the node A's routes name as publisher.
        (
            Origin::External(w.node_a.clone()),
            set_route(
                statement(w.a_id(), None, &w.node_a, 2, Some(loopback_route())),
                &w.founder_a,
            ),
            "belongs to no Identity account",
        ),
        // chain scoping: a statement for another network.
        (
            w.a(),
            set_route(
                RouteStatement {
                    chain_id: "other-chain".into(),
                    ..statement(w.a_id(), None, &w.node_a, 2, Some(loopback_route()))
                },
                &w.founder_a,
            ),
            "belongs to another chain",
        ),
        // account authority: the origin key belongs to A, the statement is B's.
        (
            w.a(),
            set_route(
                statement(w.b_id(), None, &w.node_a, 1, Some(loopback_route())),
                &w.founder_b,
            ),
            "route account is not the origin's account",
        ),
        // signer authority: a key outside the account's association.
        (
            w.a(),
            set_route(
                statement(w.a_id(), None, &w.node_a, 2, Some(loopback_route())),
                &outsider_signer,
            ),
            "signer is not a current account member",
        ),
        // signature verification: a member signer, a junk signature.
        (
            w.a(),
            set_route_raw(
                statement(w.a_id(), None, &w.node_a, 2, Some(loopback_route())),
                ed_pub(&w.founder_a),
                vec![9; 64],
            ),
            "signature does not verify",
        ),
        // the monotonic stream: a replayed revision...
        (
            w.a(),
            set_route(
                statement(w.a_id(), None, &w.node_a, 1, Some(content_route(0x55))),
                &w.founder_a,
            ),
            "route revision must be",
        ),
        // ...and a skipped one.
        (
            w.a(),
            set_route(
                statement(w.a_id(), None, &w.node_a, 7, Some(content_route(0x55))),
                &w.founder_a,
            ),
            "route revision must be",
        ),
        // statement grammar: label, zero revision, content-policy shape.
        (
            w.a(),
            set_route_raw(bad_label, ed_pub(&w.founder_a), vec![9; 64]),
            "invalid route label",
        ),
        (
            w.a(),
            set_route_raw(zero_revision, ed_pub(&w.founder_a), vec![9; 64]),
            "revision starts at 1",
        ),
        (
            w.a(),
            set_route_raw(bad_content_st, ed_pub(&w.founder_a), vec![9; 64]),
            "content routes require",
        ),
        // origin shapes: a system origin, and a non-key blob (no length rule
        // any more — it simply belongs to no account).
        (
            Origin::System,
            set_route(
                statement(w.a_id(), None, &w.node_a, 2, Some(loopback_route())),
                &w.founder_a,
            ),
            "origin must be an external key",
        ),
        (
            Origin::External(vec![7; 16]),
            set_route(
                statement(w.a_id(), None, &w.node_a, 2, Some(loopback_route())),
                &w.founder_a,
            ),
            "belongs to no Identity account",
        ),
        // the decode seam (from a fully-authorized origin).
        (
            w.a(),
            Msg {
                target: "gateway".into(),
                payload: b"definitely-not-json".to_vec(),
            },
            "expected value",
        ),
    ];

    for (height, (origin, m, needle)) in rejects.into_iter().enumerate() {
        let height = height as u64 + 6;
        let (n_before, w_before) = (root_of(&native), root_of(&wasm));

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

        assert_eq!(root_of(&native), n_before, "native root moved on reject");
        assert_eq!(root_of(&wasm), w_before, "wasm root moved on reject");
        assert_eq!(
            replies(&native, &w.accounts()).await,
            replies(&wasm, &w.accounts()).await
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
    let outsider = ed_pub(&ed(99));
    let outsider_signer = ed(99);
    let mut native = native_host(context).await;
    let mut wasm = wasm_host_(context).await;
    w.seed(&mut native).await;
    w.seed(&mut wasm).await;

    // ONE block, two revisions of the SAME route: the second dispatch's
    // monotonic check reads the first dispatch's STAGED revision — on the wasm
    // side that is the outer staged `__state` reloaded by dispatch 2 (the
    // read-your-writes seam), with the identity gate resolving through
    // memoized replay on each dispatch.
    let batch = vec![
        (
            w.a(),
            set_route(
                statement(
                    w.a_id(),
                    Some("multi"),
                    &w.node_a,
                    1,
                    Some(content_route(0x11)),
                ),
                &w.founder_a,
            ),
        ),
        (
            w.a(),
            set_route(
                statement(
                    w.a_id(),
                    Some("multi"),
                    &w.node_a,
                    2,
                    Some(loopback_route()),
                ),
                &w.founder_a,
            ),
        ),
    ];
    let n_out = native
        .submit_block(block(5, w.a()), batch.clone())
        .await
        .expect("native block");
    let w_out = wasm
        .submit_block(block(5, w.a()), batch)
        .await
        .expect("wasm block");
    for out in [&n_out, &w_out] {
        assert!(
            out.members
                .iter()
                .all(|m| matches!(m, MemberOutcome::Applied { .. })),
            "both revisions must apply: {:?}",
            out.members
        );
    }
    assert_eq!(
        replies(&native, &w.accounts()).await,
        replies(&wasm, &w.accounts()).await
    );

    // ONE block, three members: an accepted publish, a REPLAYED revision the
    // staged stream rejects (the rejection is DECIDED by read-your-writes),
    // and an accepted publish from the other account — committed state must
    // equal the accepted subset alone, on both runtimes.
    let (n_before, w_before) = (root_of(&native), root_of(&wasm));
    let batch = vec![
        (
            w.a(),
            set_route(
                statement(
                    w.a_id(),
                    Some("iso"),
                    &w.node_a,
                    1,
                    Some(content_route(0x22)),
                ),
                &w.founder_a,
            ),
        ),
        (
            w.a(),
            set_route(
                statement(
                    w.a_id(),
                    Some("iso"),
                    &w.node_a,
                    1,
                    Some(content_route(0x33)),
                ),
                &w.founder_a,
            ),
        ),
        (
            w.b(),
            set_route(
                statement(w.b_id(), None, &w.node_b, 1, Some(loopback_route())),
                &w.founder_b,
            ),
        ),
    ];
    let n_out = native
        .submit_block(block(6, w.a()), batch.clone())
        .await
        .expect("native block");
    let w_out = wasm
        .submit_block(block(6, w.a()), batch)
        .await
        .expect("wasm block");
    for out in [&n_out, &w_out] {
        assert!(matches!(out.members[0], MemberOutcome::Applied { .. }));
        assert!(
            matches!(out.members[1], MemberOutcome::Rejected { .. }),
            "the replayed revision must reject against the SAME-BLOCK stage: {:?}",
            out.members
        );
        assert!(matches!(out.members[2], MemberOutcome::Applied { .. }));
    }
    assert_ne!(root_of(&native), n_before);
    assert_ne!(root_of(&wasm), w_before);
    assert_eq!(root_of(&native), root_of(&wasm));
    assert_eq!(
        replies(&native, &w.accounts()).await,
        replies(&wasm, &w.accounts()).await
    );

    // one block where a key on NO account rejects after an acceptance — the
    // outsider's member is gate-rejected by the sibling read, leaving the
    // accepted subset only.
    let (n_before, w_before) = (root_of(&native), root_of(&wasm));
    let batch = vec![
        (
            w.a(),
            set_route(
                statement(w.a_id(), Some("api"), &w.node_a, 1, Some(loopback_route())),
                &w.founder_a,
            ),
        ),
        (
            Origin::External(outsider.clone()),
            set_route(
                statement(ABSENT_ACCOUNT, None, &outsider, 1, Some(loopback_route())),
                &outsider_signer,
            ),
        ),
    ];
    let n_out = native
        .submit_block(block(7, w.a()), batch.clone())
        .await
        .expect("native block");
    let w_out = wasm
        .submit_block(block(7, w.a()), batch)
        .await
        .expect("wasm block");
    for out in [&n_out, &w_out] {
        assert!(matches!(out.members[0], MemberOutcome::Applied { .. }));
        assert!(matches!(out.members[1], MemberOutcome::Rejected { .. }));
    }
    assert_ne!(root_of(&native), n_before);
    assert_ne!(root_of(&wasm), w_before);
    assert_eq!(root_of(&native), root_of(&wasm));
    assert_eq!(
        replies(&native, &w.accounts()).await,
        replies(&wasm, &w.accounts()).await
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
    let here = gw_store(context, "cfg_here", CHAIN_ID).await;
    let same = gw_store(context, "cfg_same", CHAIN_ID).await;
    let other = gw_store(context, "cfg_other", "other-chain").await;
    assert_eq!(here.root(), same.root(), "same config, same genesis root");
    assert_ne!(
        here.root(),
        other.root(),
        "a different chain id IS a different genesis consensus state"
    );
    drop((here, same));

    // and the config GOVERNS the guest: the same signed route statement
    // (scoped to CHAIN_ID) is accepted by the tenant configured with CHAIN_ID
    // and deterministically refused as another chain's by the tenant
    // configured differently.
    let w = World::new();
    let mut wasm = wasm_host_(context).await;
    let mut other_host = Host::genesis(vec![
        Box::new(wasm_gateway(Box::new(other))),
        Box::new(native_identity()),
    ])
    .expect("genesis");
    w.seed(&mut wasm).await;
    w.seed(&mut other_host).await;

    let m = set_route(
        statement(w.a_id(), None, &w.node_a, 1, Some(content_route(0x11))),
        &w.founder_a,
    );
    wasm.submit_at(block(5, w.a()), m.clone())
        .await
        .expect("the configured chain accepts its own statement");
    let err = other_host
        .submit_at(block(5, w.a()), m)
        .await
        .expect_err("a foreign chain id must refuse the statement");
    let SubmitError::Rejected(Error::Module(reason)) = err else {
        panic!("rejection shape: {err:?}");
    };
    assert!(
        reason.contains("belongs to another chain"),
        "the chain scope is what refuses: {reason}"
    );
}

// ---- handle plane (absorbed from duckdns) -----------------------------------

/// a SetHandle msg on the MERGED gateway module — the `.duck` human-name facet.
fn set_handle(handle: Option<&str>) -> Msg {
    Msg {
        target: "gateway".into(),
        payload: encode_msg(&GatewayMsg::SetHandle {
            handle: handle.map(Into::into),
        }),
    }
}

fn resolve_query(handle: &str) -> Vec<u8> {
    encode_query(&GatewayQuery::Resolve {
        name: DuckDnsName {
            handle: handle.into(),
        },
    })
}

/// the handle-plane read matrix: present/renamed/freed/absent resolves plus the
/// full registration listing — the whole surface duckdns's own parity pinned.
async fn handle_replies(h: &Host) -> Vec<Vec<u8>> {
    let queries = [
        resolve_query("orthory"),
        resolve_query("renamed"),
        resolve_query("quack-2"),
        resolve_query("absent"),
        encode_query(&GatewayQuery::Registrations {
            from: 0,
            limit: MAX_QUERY_LIMIT,
        }),
        encode_query(&GatewayQuery::Registrations { from: 1, limit: 1 }),
    ];
    let mut out = Vec::new();
    for q in &queries {
        out.push(h.query("gateway", q).await.expect("query"));
    }
    out
}

async fn resolved(h: &Host, handle: &str) -> Option<ResolvedAccount> {
    let reply = h
        .query("gateway", &resolve_query(handle))
        .await
        .expect("resolve");
    match decode_reply(&reply).expect("decode") {
        GatewayReply::Resolved(r) => r,
        other => panic!("expected Resolved, got {other:?}"),
    }
}

#[test]
fn handle_plane_ops_stay_in_lockstep_on_the_merged_tenant() {
    deterministic::Runner::default().start(|context| async move {
        handle_plane_inner(&context).await;
    });
}

async fn handle_plane_inner(context: &deterministic::Context) {
    let w = World::new();
    let mut native = native_host(context).await;
    let mut wasm = wasm_host_(context).await;

    // root-continuity from genesis: both roots carry the seeded `__config`.
    let genesis = root_of(&native);
    assert_eq!(genesis, root_of(&wasm), "genesis roots must be continuous");

    // sibling-only seed blocks leave the gateway root untouched on both sides.
    w.seed(&mut native).await;
    w.seed(&mut wasm).await;
    assert_eq!(root_of(&native), genesis, "seed holds the native root");

    // every handle op family in one deterministic sequence; `moves` says
    // whether committed state changes — root movement must agree on both sides.
    // founder A's key acts for account A; founder B's for account B.
    let ops: Vec<(Origin, Option<&str>, bool)> = vec![
        (w.a(), Some("orthory"), true),  // A registers
        (w.b(), Some("quack-2"), true),  // B registers
        (w.a(), Some("orthory"), false), // idempotent no-op
        (w.a(), Some("renamed"), true),  // atomic rename frees "orthory"
        (w.b(), None, true),             // unregister "quack-2"
        (w.b(), Some("orthory"), true),  // claim the freed name
    ];

    for (i, (who, handle, moves)) in ops.into_iter().enumerate() {
        let height = i as u64 + 5;
        let (n_before, w_before) = (root_of(&native), root_of(&wasm));
        native
            .submit_at(block(height, who.clone()), set_handle(handle))
            .await
            .expect("native submit");
        wasm.submit_at(block(height, who), set_handle(handle))
            .await
            .expect("wasm submit");

        assert_eq!(
            handle_replies(&native).await,
            handle_replies(&wasm).await,
            "handle replies diverge after block {height}"
        );
        if moves {
            assert_ne!(root_of(&native), n_before, "native stuck at {height}");
            assert_ne!(root_of(&wasm), w_before, "wasm stuck at {height}");
        } else {
            assert_eq!(root_of(&native), n_before, "native moved at {height}");
            assert_eq!(root_of(&wasm), w_before, "wasm moved at {height}");
        }
        assert_eq!(root_of(&native), root_of(&wasm), "continuity per block");
    }

    // resolution stops at the stable account NUMBER, never a key or a node.
    assert_eq!(
        resolved(&wasm, "renamed").await,
        Some(ResolvedAccount {
            account_id: w.a_id()
        }),
        "A's rename resolves to A's account number"
    );
    assert_eq!(
        resolved(&wasm, "orthory").await,
        Some(ResolvedAccount {
            account_id: w.b_id()
        }),
        "the freed handle now belongs to B's account"
    );
    assert_eq!(
        resolved(&wasm, "quack-2").await,
        None,
        "B's old name is gone"
    );

    // a reserved root label is refused identically on both runtimes, and the
    // reject leaves BOTH roots byte-identical to pre-block (abort, no trace).
    let (n_before, w_before) = (root_of(&native), root_of(&wasm));
    let n_err = native
        .submit_at(block(20, w.a()), set_handle(Some("net")))
        .await
        .expect_err("native rejects reserved");
    let w_err = wasm
        .submit_at(block(20, w.a()), set_handle(Some("net")))
        .await
        .expect_err("wasm rejects reserved");
    for err in [n_err, w_err] {
        let SubmitError::Rejected(Error::Module(reason)) = err else {
            panic!("rejection shape: {err:?}");
        };
        assert!(reason.contains("reserved"), "reason: {reason}");
    }
    assert_eq!(root_of(&native), n_before, "native moved on reject");
    assert_eq!(root_of(&wasm), w_before, "wasm moved on reject");
}
