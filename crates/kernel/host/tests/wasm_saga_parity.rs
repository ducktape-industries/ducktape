//! the STORE-BACKED cutover-continuity proof for the saga ledger: the `saga`
//! guest component (the NATIVE `saga` crate compiled to wasm behind
//! `ducktape-module-sdk`) over `WasmModule::with_store(QmdbStore)` and the native
//! `SagaModule` over the same store shape are ROOT-CONTINUOUS — the same op
//! sequence commits the IDENTICAL qmdb merkle root after every block, not
//! merely lockstep-moving distinct roots. both roots ARE the store's root;
//! qmdb's batch canonicalizes mutations by hashed key, so the native logical-key
//! commit order and the wasm hashed-key drain order produce the same op log.
//! this executor swap changes not one committed byte — including the
//! byte-identical NO-OP blocks (a crank that finds nothing expired, a duplicate
//! trigger) that stage nothing on either side.
//!
//! saga is the deterministic half of the async engine, so this proof leans on
//! three surfaces beyond the usual reply/root matrix:
//!
//! * WORKER EVENTS: trigger / retry / accept emits a [`WorkerRequest`], while
//!   terminal transitions may emit a cancellation control. the host-side
//!   worker seam decodes both out of `BlockOutcome::events`. both runtimes must
//!   surface the byte-identical event stream, or the reactor would feed workers
//!   differently across the cutover.
//! * P6 CALLBACKS: every terminal transition with a `reply_to` emits a
//!   same-block [`SagaCallback`] msg. a native recorder module ("req") folds
//!   every callback it receives into its root on BOTH hosts, so a missing,
//!   duplicated, or reordered callback diverges the recorder roots.
//! * SIBLING-READ ASSIGNMENT: the production constructor
//!   (`SagaModule::with_assignment("saga", "valset", "capability",
//!   LeasePolicy::Strict)` — `bin/node/src/host_state.rs`) resolves every
//!   attempt's assignee through valset/capability queries; on the wasm side
//!   those are host-routed `query-module` reads resolved through the
//!   runtime's memoized replay against the REAL native siblings both hosts
//!   carry.

use capability::{CapabilityMsg, CapabilityRegistry, encode_msg as capability_encode_msg};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use host::{BlockContext, BlockOutcome, Host, MemberOutcome, SubmitError};
use saga::{
    LeasePolicy, MAX_CAPABILITY_BYTES, MAX_ERROR_BYTES, MAX_REPLY_PAYLOAD_BYTES, MAX_RESULT_BYTES,
    MAX_RETAINED_TERMINAL, SagaModule, SagaMsg, SagaQuery, SagaReply, SagaStatus, WorkerRequest,
    decode_reply, decode_worker_control, decode_worker_request, encode_msg, encode_query,
};
use sdk::{Ctx, Error, Module, ModuleId, Msg, Origin, StateRoot};
use statesync::qmdb::QmdbStore;
use std::collections::BTreeMap;
use valset::Valset;
use wasm_host::WasmModule;

/// GENERATED artifact — built from the `saga` module's guest port by
/// guest-builder (`make wasm-modules`); committed so this proof is self-contained.
const SAGA_WASM: &[u8] = include_bytes!("fixtures/saga.component.wasm");

fn wasm_saga(store: Box<dyn sdk::MerkleStore>) -> WasmModule {
    WasmModule::with_store("saga", SAGA_WASM, store).expect("load component")
}

/// the production wiring, verbatim (`bin/node/src/host_state.rs`).
fn native_saga(store: Box<dyn sdk::MerkleStore>) -> SagaModule {
    SagaModule::with_assignment("saga", store, "valset", "capability", LeasePolicy::Strict)
}

/// a 32-byte member key. the ordered lane hands modules verified ed25519 ids;
/// the parity claim only needs them distinct, non-empty, and identically
/// byte-compared by valset membership, capability announcements, and saga's
/// strict lease gate — no signatures cross this proof.
fn key(tag: u8) -> Vec<u8> {
    vec![tag; 32]
}

async fn seeded_valset(members: &[Vec<u8>]) -> Valset {
    let mut valset = Valset::new(
        "valset",
        Box::new(sdk_testkit::MemStore::new()),
        "governance",
    );
    for m in members {
        valset.seed(m.clone()).await.expect("seed valset");
    }
    valset.finish_seed().await.expect("seed valset");
    valset
}

/// the requester recorder: a native module both hosts carry under the id the
/// triggers name as `reply_to`. it commits to the byte-concatenation of every
/// saga callback it receives, so a callback that diverged (or went missing)
/// between the runtimes diverges the recorder roots. staged/committed split
/// keeps the block boundary honest (an aborted block leaves no trace here
/// either).
struct Recorder {
    staged: Vec<Vec<u8>>,
    committed: Vec<u8>,
}

impl Recorder {
    fn new() -> Self {
        Self {
            staged: Vec::new(),
            committed: Vec::new(),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Module for Recorder {
    fn id(&self) -> ModuleId {
        "req".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot(sha2::Sha256::digest(&self.committed).into())
    }
    async fn execute(&mut self, _ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        self.staged.push(msg.payload.clone());
        Ok(())
    }
    async fn commit_block(&mut self) -> Result<(), Error> {
        for payload in self.staged.drain(..) {
            self.committed.extend_from_slice(&payload);
        }
        Ok(())
    }
    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged.clear();
        Ok(())
    }
}

use sha2::Digest as _;

/// the two hosts are handed SEPARATE qmdb stores under the same store id: the
/// root-continuity claim is that two independent stores driven by the two
/// executors commit the same root, which a shared store would trivially fake.
async fn native_host(
    context: &deterministic::Context,
    label: &'static str,
    members: &[Vec<u8>],
) -> Host {
    Host::genesis(vec![
        Box::new(native_saga(Box::new(
            QmdbStore::init(context.child(label), "saga").await,
        ))),
        Box::new(seeded_valset(members).await),
        Box::new(CapabilityRegistry::new(
            "capability",
            Box::new(sdk_testkit::MemStore::new()),
            Some("valset".into()),
        )),
        Box::new(Recorder::new()),
    ])
    .expect("genesis")
}

async fn wasm_host_(
    context: &deterministic::Context,
    label: &'static str,
    members: &[Vec<u8>],
) -> Host {
    Host::genesis(vec![
        Box::new(wasm_saga(Box::new(
            QmdbStore::init(context.child(label), "saga").await,
        ))),
        Box::new(seeded_valset(members).await),
        Box::new(CapabilityRegistry::new(
            "capability",
            Box::new(sdk_testkit::MemStore::new()),
            Some("valset".into()),
        )),
        Box::new(Recorder::new()),
    ])
    .expect("genesis")
}

/// the only saga id a given member may trigger: saga's id space is namespaced
/// per trigger origin ([`saga::namespaced_id`]), which is what stops one
/// member from squatting an id another principal derives.
fn sid(who: &[u8], id: &str) -> String {
    saga::namespaced_id(&Origin::External(who.to_vec()), id)
}

/// one block's agreed context. consensus_time == height, as on the real
/// validator network — saga compares lease/deadline views against
/// consensus_time, so keeping them equal keeps the sweep arithmetic honest.
fn block(height: u64, origin: Origin) -> BlockContext {
    BlockContext {
        height,
        consensus_time: height,
        origin,
    }
}

fn saga_op(m: &SagaMsg) -> Msg {
    Msg {
        target: "saga".into(),
        payload: encode_msg(m),
    }
}

/// trigger parameters as a STRUCT (enum variants take no functional-record
/// updates), so call sites override exactly the fields a scenario is about.
struct Trig {
    saga_id: String,
    spec: Vec<u8>,
    reply_to: Option<String>,
    reply_payload: Vec<u8>,
    deadline: Option<u64>,
    max_attempts: u32,
    lease_views: Option<u64>,
    capability: Option<String>,
    demands: BTreeMap<String, u64>,
    pinned_assignee: Option<Vec<u8>>,
}

impl From<Trig> for SagaMsg {
    fn from(t: Trig) -> Self {
        SagaMsg::Trigger {
            saga_id: t.saga_id,
            spec: t.spec,
            reply_to: t.reply_to,
            reply_payload: t.reply_payload,
            deadline: t.deadline,
            max_attempts: t.max_attempts,
            lease_views: t.lease_views,
            capability: t.capability,
            demands: t.demands,
            pinned_assignee: t.pinned_assignee,
        }
    }
}

/// a trigger with fire-and-forget defaults; call sites override fields inline.
fn trigger(id: &str) -> Trig {
    Trig {
        saga_id: id.into(),
        spec: format!("work:{id}").into_bytes(),
        reply_to: Some("req".into()),
        reply_payload: format!("corr:{id}").into_bytes(),
        deadline: None,
        max_attempts: 1,
        lease_views: None,
        capability: None,
        demands: BTreeMap::new(),
        pinned_assignee: None,
    }
}

fn oracle(id: &str, attempt: u32, outcome: Result<Vec<u8>, String>) -> Msg {
    saga_op(&SagaMsg::OracleResult {
        saga_id: id.into(),
        attempt,
        outcome,
        usage: None,
    })
}

fn announce(capabilities: &[&str], resources: &[(&str, u64)]) -> Msg {
    Msg {
        target: "capability".into(),
        payload: capability_encode_msg(&CapabilityMsg::Announce {
            capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
            resources: resources.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        }),
    }
}

/// the read matrix: per-id Get (present and absent ids alike), the crank
/// pump's NextExpiry, and both members' AssignedPending projections (the
/// resident worker pump's read — reconstructed WorkerRequests must agree).
async fn replies(h: &Host, ids: &[String], members: &[&[u8]]) -> Vec<Vec<u8>> {
    let mut queries = vec![encode_query(&SagaQuery::NextExpiry)];
    for id in ids {
        queries.push(encode_query(&SagaQuery::Get {
            saga_id: id.clone(),
        }));
    }
    queries.push(encode_query(&SagaQuery::Get {
        saga_id: "absent".into(),
    }));
    for m in members {
        queries.push(encode_query(&SagaQuery::AssignedPending {
            assignee: m.to_vec(),
            // the first page: these fixtures hold far fewer than one page of
            // live sagas, so it is the whole projection.
            after: None,
        }));
    }
    let mut out = Vec::new();
    for q in &queries {
        out.push(h.query("saga", q).await.expect("query"));
    }
    out
}

fn root_of(h: &Host) -> StateRoot {
    h.module_root("saga").expect("saga registered")
}

const SIBLING_IDS: [&str; 3] = ["valset", "capability", "req"];

/// the block's event trace as comparable tuples, and the saga work orders
/// decoded from it — the exact surface the host-side worker seam consumes.
fn event_tuples(out: &BlockOutcome) -> Vec<(String, Vec<u8>)> {
    out.events
        .iter()
        .map(|e| (e.source.clone(), e.payload.clone()))
        .collect()
}

fn worker_requests(out: &BlockOutcome) -> Vec<WorkerRequest> {
    out.events
        .iter()
        .filter(|e| e.source == "saga")
        .filter_map(|e| match decode_worker_request(&e.payload) {
            Ok(request) => Some(request),
            Err(_) => {
                decode_worker_control(&e.payload)
                    .expect("saga events are worker requests or controls");
                None
            }
        })
        .collect()
}

/// submit one ACCEPTED op to both hosts and assert the full parity claim:
/// identical replies, identical event traces (and decoded work orders),
/// per-sibling cross-host agreement (the recorder root carries the callback
/// lane), and lockstep saga-root movement.
/// (one argument per invariant knob; a builder would only obscure the matrix.)
#[allow(clippy::too_many_arguments)]
async fn roundtrip(
    native: &mut Host,
    wasm: &mut Host,
    ids: &[String],
    members: &[&[u8]],
    height: u64,
    origin: Origin,
    m: Msg,
    moves: bool,
) -> (Vec<WorkerRequest>, Vec<WorkerRequest>) {
    let (n_before, w_before) = (root_of(native), root_of(wasm));
    let n_out = native
        .submit_at(block(height, origin.clone()), m.clone())
        .await
        .expect("native submit");
    let w_out = wasm
        .submit_at(block(height, origin), m)
        .await
        .expect("wasm submit");
    assert_eq!(
        event_tuples(&n_out),
        event_tuples(&w_out),
        "event traces diverge at {height}"
    );
    let (n_reqs, w_reqs) = (worker_requests(&n_out), worker_requests(&w_out));
    assert_eq!(
        n_reqs, w_reqs,
        "decoded work orders diverge at {height} — the reactor would feed workers differently"
    );
    assert_eq!(
        replies(native, ids, members).await,
        replies(wasm, ids, members).await,
        "replies diverge after block {height}"
    );
    for sibling in SIBLING_IDS {
        assert_eq!(
            native.module_root(sibling),
            wasm.module_root(sibling),
            "the {sibling} sibling diverged at {height}"
        );
    }
    // THE continuity claim: one root, two executors.
    assert_eq!(
        root_of(native),
        root_of(wasm),
        "saga roots diverge after block {height}"
    );
    if moves {
        assert_ne!(root_of(native), n_before, "native root stuck at {height}");
        assert_ne!(root_of(wasm), w_before, "wasm root stuck at {height}");
    } else {
        assert_eq!(root_of(native), n_before, "native root moved at {height}");
        assert_eq!(root_of(wasm), w_before, "wasm root moved at {height}");
    }
    (n_reqs, w_reqs)
}

/// submit one REJECTED op to both hosts: reasons carry the same needle, and
/// the saga roots (and every sibling) are byte-identical to pre-block — the
/// abort lane leaves no trace.
/// (one argument per invariant knob; a builder would only obscure the matrix.)
#[allow(clippy::too_many_arguments)]
async fn reject_roundtrip(
    native: &mut Host,
    wasm: &mut Host,
    ids: &[String],
    members: &[&[u8]],
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
    assert_eq!(root_of(native), n_before, "native root moved on reject");
    assert_eq!(root_of(wasm), w_before, "wasm root moved on reject");
    assert_eq!(root_of(native), root_of(wasm), "roots diverge on reject");
    for sibling in SIBLING_IDS {
        assert_eq!(native.module_root(sibling), wasm.module_root(sibling));
    }
    assert_eq!(
        replies(native, ids, members).await,
        replies(wasm, ids, members).await
    );
}

/// the (cross-host-agreed) live view of one saga on the wasm host — used to
/// read the strict lease's current assignee so results can be submitted from
/// the origin the policy accepts.
async fn view_of(h: &Host, id: &str) -> saga::SagaView {
    let reply = h
        .query(
            "saga",
            &encode_query(&SagaQuery::Get { saga_id: id.into() }),
        )
        .await
        .expect("get");
    match decode_reply(&reply).expect("decode") {
        SagaReply::Saga(Some(v)) => v,
        other => panic!("expected a live saga, got {other:?}"),
    }
}

#[test]
fn same_ops_same_events_same_replies_roots_in_lockstep() {
    deterministic::Runner::default().start(same_ops_inner);
}

async fn same_ops_inner(context: deterministic::Context) {
    let (a, b) = (key(0xAA), key(0xBB));
    let members = vec![a.clone(), b.clone()];
    let member_keys: [&[u8]; 2] = [&a, &b];
    let ids = [
        sid(&a, "t-open"),
        sid(&b, "t-cap"),
        sid(&a, "t-ann"),
        sid(&a, "t-pin"),
        sid(&a, "t-cxl"),
        sid(&a, "t-renew"),
        sid(&a, "t-re"),
    ];

    let mut native = native_host(&context, "same_ops_native", &members).await;
    let mut wasm = wasm_host_(&context, "same_ops_wasm", &members).await;

    // both roots ARE the qmdb store's root over two independently opened
    // stores, so they start equal and — the claim of this file — stay equal
    // after every committed block below.
    assert_eq!(
        root_of(&native),
        root_of(&wasm),
        "store-backed tenant: genesis roots coincide"
    );

    // ---- capability announcements (sibling-only blocks hold the saga root).
    let (n0, w0) = (root_of(&native), root_of(&wasm));
    for host in [&mut native, &mut wasm] {
        host.submit_at(
            block(1, Origin::External(a.clone())),
            announce(&["llm"], &[("cores", 8)]),
        )
        .await
        .expect("A announces");
        host.submit_at(
            block(2, Origin::External(b.clone())),
            announce(&["llm", "gpu"], &[("cores", 4)]),
        )
        .await
        .expect("B announces");
    }
    assert_eq!(root_of(&native), n0, "sibling blocks hold the native root");
    assert_eq!(root_of(&wasm), w0, "sibling blocks hold the wasm root");

    // ---- t-open: an UNTAGGED trigger draws its pool from the valset sibling
    // (a host-routed read resolved through memoized replay on the wasm side).
    let (n_reqs, _) = roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        3,
        Origin::External(a.clone()),
        saga_op(
            &Trig {
                deadline: Some(500),
                max_attempts: 2,
                ..trigger(&sid(&a, "t-open"))
            }
            .into(),
        ),
        true,
    )
    .await;
    assert_eq!(n_reqs.len(), 1, "one work order per trigger");
    let open_assignee = n_reqs[0].assignee.clone().expect("valset pool assigns");

    // strict lease: a NON-assignee's result is a deterministic no-op on both
    // runtimes (an accepted op that stages nothing and emits nothing).
    let stranger = members
        .iter()
        .find(|m| **m != open_assignee)
        .expect("two members")
        .clone();
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        4,
        Origin::External(stranger),
        oracle(&sid(&a, "t-open"), 0, Ok(b"stolen".to_vec())),
        false,
    )
    .await;

    // the assignee's result lands: Done + the P6 callback commits into the
    // recorder IN THIS BLOCK (the recorder root moves, identically, on both).
    let (n_req_before, w_req_before) = (native.module_root("req"), wasm.module_root("req"));
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        5,
        Origin::External(open_assignee),
        oracle(&sid(&a, "t-open"), 0, Ok(br#"{"answer":42}"#.to_vec())),
        true,
    )
    .await;
    assert_ne!(
        native.module_root("req"),
        n_req_before,
        "callback landed natively"
    );
    assert_ne!(
        wasm.module_root("req"),
        w_req_before,
        "callback landed through wasm"
    );

    // ---- t-cap: a TAGGED trigger with demands draws from CapableProviders —
    // {cores: 8} narrows the llm pool to A alone, so the assignee is A on both
    // runtimes by construction.
    let (n_reqs, _) = roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        6,
        Origin::External(b.clone()),
        saga_op(
            &Trig {
                max_attempts: 2,
                capability: Some("llm".into()),
                demands: BTreeMap::from([("cores".to_string(), 8u64)]),
                ..trigger(&sid(&b, "t-cap"))
            }
            .into(),
        ),
        true,
    )
    .await;
    assert_eq!(
        n_reqs[0].assignee.as_deref(),
        Some(a.as_slice()),
        "the demand filter narrows the pool to A"
    );

    // an Err consumes the attempt and RE-LEASES: the retry's work order is
    // re-emitted (attempt 1) on both runtimes.
    let (n_reqs, _) = roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        7,
        Origin::External(a.clone()),
        oracle(&sid(&b, "t-cap"), 0, Err("transient".into())),
        true,
    )
    .await;
    assert_eq!(n_reqs.len(), 1, "the retry re-emits one work order");
    assert_eq!(n_reqs[0].attempt, 1);
    let retry_assignee = view_of(&wasm, &sid(&b, "t-cap"))
        .await
        .assignee
        .expect("re-leased");

    // the FINAL attempt's Err lands Failed + callback (no further work order).
    let (n_reqs, _) = roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        8,
        Origin::External(retry_assignee),
        oracle(&sid(&b, "t-cap"), 1, Err("fatal".into())),
        true,
    )
    .await;
    assert!(n_reqs.is_empty(), "a terminal failure emits no work order");
    assert_eq!(
        view_of(&wasm, &sid(&b, "t-cap")).await.status,
        SagaStatus::Failed
    );

    // ---- t-ann: a tag NOBODY announced assigns nobody — the emitted work
    // order is an ANNOUNCEMENT (assignee None), and under Strict no result
    // can land until a node claims it.
    let (n_reqs, _) = roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        9,
        Origin::External(a.clone()),
        saga_op(
            &Trig {
                capability: Some("nobody-has-this".into()),
                ..trigger(&sid(&a, "t-ann"))
            }
            .into(),
        ),
        true,
    )
    .await;
    assert_eq!(n_reqs[0].assignee, None, "an unprovided tag assigns nobody");

    // a result for the unclaimed announcement is a strict no-op...
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        10,
        Origin::External(a.clone()),
        oracle(&sid(&a, "t-ann"), 0, Ok(b"premature".to_vec())),
        false,
    )
    .await;
    // B announces itself as a "nobody-has-this" provider — Accept's
    // capability gate needs this on top of B's valset standing (a member
    // key already has); an unannounced tag has no eligible claimant at all.
    // Announce REPLACES the submitter's whole tag set, so this re-states B's
    // earlier "llm"/"gpu" announcement too — the later t-re/t-cap scenarios
    // still need B in those pools.
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        11,
        Origin::External(b.clone()),
        announce(&["llm", "gpu", "nobody-has-this"], &[("cores", 4)]),
        false,
    )
    .await;
    // ...B claims it via Accept — the re-emitted work order names the winner —
    let (n_reqs, _) = roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        12,
        Origin::External(b.clone()),
        saga_op(&SagaMsg::Accept {
            saga_id: sid(&a, "t-ann"),
            attempt: 0,
        }),
        true,
    )
    .await;
    assert_eq!(n_reqs[0].assignee.as_deref(), Some(b.as_slice()));
    // ...a LATE accept (already assigned) is a no-op...
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        13,
        Origin::External(a.clone()),
        saga_op(&SagaMsg::Accept {
            saga_id: sid(&a, "t-ann"),
            attempt: 0,
        }),
        false,
    )
    .await;
    // ...and the WINNER's result lands.
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        14,
        Origin::External(b.clone()),
        oracle(&sid(&a, "t-ann"), 0, Ok(b"claimed-and-done".to_vec())),
        true,
    )
    .await;

    // ---- t-pin: a static binding leases every attempt to the pinned key —
    // no pool query at all.
    let (n_reqs, _) = roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        15,
        Origin::External(a.clone()),
        saga_op(
            &Trig {
                pinned_assignee: Some(b.clone()),
                ..trigger(&sid(&a, "t-pin"))
            }
            .into(),
        ),
        true,
    )
    .await;
    assert_eq!(n_reqs[0].assignee.as_deref(), Some(b.as_slice()));
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        16,
        Origin::External(b.clone()),
        oracle(&sid(&a, "t-pin"), 0, Ok(b"pinned-done".to_vec())),
        true,
    )
    .await;

    // ---- t-cxl: cancel is gated to the recorded trigger origin — a foreign
    // cancel is a no-op, the owner's cancel lands Cancelled + callback.
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        17,
        Origin::External(a.clone()),
        saga_op(
            &Trig {
                max_attempts: 3,
                ..trigger(&sid(&a, "t-cxl"))
            }
            .into(),
        ),
        true,
    )
    .await;
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        18,
        Origin::External(b.clone()),
        saga_op(&SagaMsg::Cancel {
            saga_id: sid(&a, "t-cxl"),
        }),
        false,
    )
    .await;
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        19,
        Origin::External(a.clone()),
        saga_op(&SagaMsg::Cancel {
            saga_id: sid(&a, "t-cxl"),
        }),
        true,
    )
    .await;

    // ---- t-renew: only the current assignee may renew, and only inside the
    // renewal window. the lease view moves identically on both runtimes.
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        20,
        Origin::External(a.clone()),
        saga_op(
            &Trig {
                lease_views: Some(4),
                capability: Some("llm".into()),
                ..trigger(&sid(&a, "t-renew"))
            }
            .into(),
        ),
        true,
    )
    .await;
    let renew_holder = view_of(&wasm, &sid(&a, "t-renew"))
        .await
        .assignee
        .expect("leased");
    let stranger = members
        .iter()
        .find(|m| **m != renew_holder)
        .expect("two members")
        .clone();
    // a non-holder renew is a no-op...
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        22,
        Origin::External(stranger),
        saga_op(&SagaMsg::RenewLease {
            saga_id: sid(&a, "t-renew"),
            attempt: 0,
        }),
        false,
    )
    .await;
    // ...the holder's renew inside the half-window extends the lease (the
    // updated_at bump alone moves the root even when the expiry holds).
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        23,
        Origin::External(renew_holder),
        saga_op(&SagaMsg::RenewLease {
            saga_id: sid(&a, "t-renew"),
            attempt: 0,
        }),
        true,
    )
    .await;

    // ---- t-re: the trigger origin reassigns — the incremented attempt is
    // rendezvous-assigned over the pool MINUS the old holder, so it lands on
    // the other member, and the re-emitted work order names it.
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        24,
        Origin::External(a.clone()),
        saga_op(
            &Trig {
                max_attempts: 3,
                capability: Some("llm".into()),
                ..trigger(&sid(&a, "t-re"))
            }
            .into(),
        ),
        true,
    )
    .await;
    let first_holder = view_of(&wasm, &sid(&a, "t-re"))
        .await
        .assignee
        .expect("leased");
    // a foreign reassign is a no-op (the origin gate reads the recorded
    // trigger origin, folded state on the wasm side)...
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        25,
        Origin::External(b.clone()),
        saga_op(&SagaMsg::Reassign {
            saga_id: sid(&a, "t-re"),
            attempt: 0,
        }),
        false,
    )
    .await;
    let (n_reqs, _) = roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        26,
        Origin::External(a.clone()),
        saga_op(&SagaMsg::Reassign {
            saga_id: sid(&a, "t-re"),
            attempt: 0,
        }),
        true,
    )
    .await;
    assert_eq!(n_reqs.len(), 1);
    assert_eq!(n_reqs[0].attempt, 1);
    let second_holder = n_reqs[0].assignee.clone().expect("reassigned");
    assert_ne!(second_holder, first_holder, "the old holder is excluded");

    // ---- prune: terminal-only, origin-gated GC. a foreign prune and a
    // non-terminal prune skip as no-ops; the owner's prune of a terminal saga
    // removes it (the root moves, and the Get view empties identically).
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        27,
        Origin::External(b.clone()),
        saga_op(&SagaMsg::Prune {
            saga_ids: vec![sid(&a, "t-open"), sid(&a, "t-re")],
        }),
        false,
    )
    .await;
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        28,
        Origin::External(a.clone()),
        saga_op(&SagaMsg::Prune {
            saga_ids: vec![sid(&a, "t-open"), sid(&a, "t-re")],
        }),
        true,
    )
    .await;
    let reply = wasm
        .query(
            "saga",
            &encode_query(&SagaQuery::Get {
                saga_id: sid(&a, "t-open"),
            }),
        )
        .await
        .expect("get");
    assert_eq!(
        decode_reply(&reply).expect("decode"),
        SagaReply::Saga(None),
        "the pruned saga is gone"
    );

    // and after the WHOLE script the two stores hold byte-identical roots.
    assert_eq!(root_of(&native), root_of(&wasm));

    // queries are read-only on the wasm side too: the root is stable across
    // the whole read matrix.
    let settled = root_of(&wasm);
    let _ = replies(&wasm, &ids, &member_keys).await;
    assert_eq!(root_of(&wasm), settled, "a query moved the wasm root");
}

#[test]
fn crank_times_out_and_expires_leases_in_lockstep() {
    deterministic::Runner::default().start(crank_inner);
}

async fn crank_inner(context: deterministic::Context) {
    let (a, b) = (key(0xAA), key(0xBB));
    let members = vec![a.clone(), b.clone()];
    let member_keys: [&[u8]; 2] = [&a, &b];
    let ids = [sid(&a, "t-dead"), sid(&a, "t-lease")];

    let mut native = native_host(&context, "crank_native", &members).await;
    let mut wasm = wasm_host_(&context, "crank_wasm", &members).await;

    // a saga bounded by an absolute deadline, and one bounded by short leases.
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        1,
        Origin::External(a.clone()),
        saga_op(
            &Trig {
                deadline: Some(4),
                ..trigger(&sid(&a, "t-dead"))
            }
            .into(),
        ),
        true,
    )
    .await;
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        2,
        Origin::External(a.clone()),
        saga_op(
            &Trig {
                lease_views: Some(2),
                max_attempts: 2,
                ..trigger(&sid(&a, "t-lease"))
            }
            .into(),
        ),
        true,
    )
    .await;

    // a crank BEFORE anything expired stages nothing (both roots hold).
    roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        3,
        Origin::External(b.clone()),
        saga_op(&SagaMsg::Crank {}),
        false,
    )
    .await;

    // by view 5 the deadline (4) has passed and t-lease's first lease (2+2)
    // expired: ONE permissionless crank times t-dead out (callback) and
    // re-leases t-lease (a fresh work order for attempt 1) — identically.
    let (n_reqs, _) = roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        5,
        Origin::External(b.clone()),
        saga_op(&SagaMsg::Crank {}),
        true,
    )
    .await;
    assert_eq!(n_reqs.len(), 1, "the expired lease re-emits one work order");
    assert_eq!(n_reqs[0].saga_id, sid(&a, "t-lease"));
    assert_eq!(n_reqs[0].attempt, 1);
    assert_eq!(
        view_of(&wasm, &sid(&a, "t-dead")).await.status,
        SagaStatus::TimedOut
    );

    // the SECOND lease expires with attempts exhausted: the next crank lands
    // Failed + callback (no further work order).
    let (n_reqs, _) = roundtrip(
        &mut native,
        &mut wasm,
        &ids,
        &member_keys,
        9,
        Origin::External(b.clone()),
        saga_op(&SagaMsg::Crank {}),
        true,
    )
    .await;
    assert!(n_reqs.is_empty());
    let view = view_of(&wasm, &sid(&a, "t-lease")).await;
    assert_eq!(view.status, SagaStatus::Failed);
    assert_eq!(view.error.as_deref(), Some("lease attempts exhausted"));
}

#[test]
fn rejections_match_and_leave_no_trace() {
    deterministic::Runner::default().start(rejections_inner);
}

async fn rejections_inner(context: deterministic::Context) {
    let (a, b) = (key(0xAA), key(0xBB));
    let members = vec![a.clone(), b.clone()];
    let member_keys: [&[u8]; 2] = [&a, &b];
    let ids = [sid(&a, "live"), sid(&a, "solo"), sid(&a, "pinned")];

    let mut native = native_host(&context, "reject_native", &members).await;
    let mut wasm = wasm_host_(&context, "reject_wasm", &members).await;

    // seed: a lone provider for "solo" (reassignment has no alternate), a live
    // assigned saga (the oversized-outcome seam is gated to its assignee), a
    // single-attempt saga, and a pinned saga.
    for host in [&mut native, &mut wasm] {
        host.submit_at(
            block(1, Origin::External(a.clone())),
            announce(&["solo"], &[]),
        )
        .await
        .expect("announce solo");
        host.submit_at(
            block(2, Origin::External(a.clone())),
            saga_op(
                &Trig {
                    max_attempts: 3,
                    capability: Some("solo".into()),
                    ..trigger(&sid(&a, "live"))
                }
                .into(),
            ),
        )
        .await
        .expect("trigger live");
        host.submit_at(
            block(3, Origin::External(a.clone())),
            saga_op(
                &Trig {
                    capability: Some("solo".into()),
                    ..trigger(&sid(&a, "solo"))
                }
                .into(),
            ),
        )
        .await
        .expect("trigger solo");
        host.submit_at(
            block(4, Origin::External(a.clone())),
            saga_op(
                &Trig {
                    max_attempts: 3,
                    pinned_assignee: Some(b.clone()),
                    ..trigger(&sid(&a, "pinned"))
                }
                .into(),
            ),
        )
        .await
        .expect("trigger pinned");
    }
    // "solo"'s only provider is A, so both live sagas lease to A.
    assert_eq!(
        view_of(&wasm, &sid(&a, "live")).await.assignee.as_deref(),
        Some(a.as_slice())
    );

    // every distinct refusal family the native module implements. (the
    // 12 MiB spec cap shares its code path with the reply_payload cap below
    // and is exercised by saga's own unit tests — a 44 MiB JSON op would
    // slow this proof for no additional coverage.)
    let rejects: Vec<(Origin, Msg, &str)> = vec![
        (
            Origin::External(a.clone()),
            saga_op(
                &Trig {
                    max_attempts: 0,
                    ..trigger(&sid(&a, "r1"))
                }
                .into(),
            ),
            "trigger max_attempts must be >= 1",
        ),
        (
            Origin::External(a.clone()),
            saga_op(
                &Trig {
                    reply_payload: vec![7; MAX_REPLY_PAYLOAD_BYTES + 1],
                    ..trigger(&sid(&a, "r2"))
                }
                .into(),
            ),
            "trigger reply_payload is",
        ),
        (
            Origin::External(a.clone()),
            saga_op(
                &Trig {
                    capability: Some(String::new()),
                    ..trigger(&sid(&a, "r3"))
                }
                .into(),
            ),
            "trigger capability must be non-empty",
        ),
        (
            Origin::External(a.clone()),
            saga_op(
                &Trig {
                    capability: Some("x".repeat(MAX_CAPABILITY_BYTES + 1)),
                    ..trigger(&sid(&a, "r4"))
                }
                .into(),
            ),
            "trigger capability is",
        ),
        // the shared validate_resources invariant: zero values and non-tag
        // dimension keys reject at trigger time.
        (
            Origin::External(a.clone()),
            saga_op(
                &Trig {
                    capability: Some("llm".into()),
                    demands: BTreeMap::from([("cores".to_string(), 0u64)]),
                    ..trigger(&sid(&a, "r5"))
                }
                .into(),
            ),
            "is zero",
        ),
        (
            Origin::External(a.clone()),
            saga_op(
                &Trig {
                    capability: Some("llm".into()),
                    demands: BTreeMap::from([("NOT A TAG".to_string(), 1u64)]),
                    ..trigger(&sid(&a, "r6"))
                }
                .into(),
            ),
            "invalid characters",
        ),
        (
            Origin::External(a.clone()),
            saga_op(
                &Trig {
                    pinned_assignee: Some(Vec::new()),
                    ..trigger(&sid(&a, "r7"))
                }
                .into(),
            ),
            "pinned_assignee must be non-empty",
        ),
        // the callback-poison rule: a self-targeting or unknown reply_to
        // rejects at trigger time (the unknown-module check is a module-root
        // sibling read — resolved through the runtime on the wasm side).
        (
            Origin::External(a.clone()),
            saga_op(
                &Trig {
                    reply_to: Some("saga".into()),
                    ..trigger(&sid(&a, "r8"))
                }
                .into(),
            ),
            "must not target the saga module itself",
        ),
        (
            Origin::External(a.clone()),
            saga_op(
                &Trig {
                    reply_to: Some("ghost".into()),
                    ..trigger(&sid(&a, "r9"))
                }
                .into(),
            ),
            "targets unknown module ghost",
        ),
        // oversized outcomes ABORT rather than commit into the root preimage —
        // submitted from the assignee so the size check is what rejects.
        (
            Origin::External(a.clone()),
            oracle(&sid(&a, "live"), 0, Ok(vec![7; MAX_RESULT_BYTES + 1])),
            "oracle result is",
        ),
        (
            Origin::External(a.clone()),
            oracle(&sid(&a, "live"), 0, Err("e".repeat(MAX_ERROR_BYTES + 1))),
            "oracle error is",
        ),
        // the Accept origin gates.
        (
            Origin::System,
            saga_op(&SagaMsg::Accept {
                saga_id: sid(&a, "live"),
                attempt: 0,
            }),
            "Accept requires an external origin",
        ),
        (
            Origin::External(Vec::new()),
            saga_op(&SagaMsg::Accept {
                saga_id: sid(&a, "live"),
                attempt: 0,
            }),
            "non-empty submitter id",
        ),
        // reassignment door checks: pinned sagas never reassign, a lone
        // provider has no alternate, and a single-attempt saga has no attempt
        // left to burn.
        (
            Origin::External(a.clone()),
            saga_op(&SagaMsg::Reassign {
                saga_id: sid(&a, "pinned"),
                attempt: 0,
            }),
            "pinned saga cannot be reassigned",
        ),
        (
            Origin::External(a.clone()),
            saga_op(&SagaMsg::Reassign {
                saga_id: sid(&a, "solo"),
                attempt: 0,
            }),
            "reassignment attempts exhausted",
        ),
        (
            Origin::External(a.clone()),
            saga_op(&SagaMsg::Reassign {
                saga_id: sid(&a, "live"),
                attempt: 0,
            }),
            "no alternate assignee is available",
        ),
        // the ID-SQUAT seam: the saga id space is OWNED per origin, so B
        // cannot trigger into A's namespace, nor into the `dispatch` module's
        // — the predictable `dispatch{SEP}{receiver}{SEP}{id}` shape a
        // squatter would race a producer for. this is the one refusal here
        // decided purely on `env().origin`, which is exactly the input that
        // can cross the WIT boundary and do nothing inside the guest.
        (
            Origin::External(b.clone()),
            saga_op(
                &Trig {
                    ..trigger(&sid(&a, "squat"))
                }
                .into(),
            ),
            "own namespace",
        ),
        (
            Origin::External(b.clone()),
            saga_op(
                &Trig {
                    ..trigger(&saga::namespaced_id(
                        &Origin::Module("dispatch".into()),
                        "chat\u{1f}run-7",
                    ))
                }
                .into(),
            ),
            "own namespace",
        ),
        // the decode seam.
        (
            Origin::External(a.clone()),
            Msg {
                target: "saga".into(),
                payload: b"definitely-not-json".to_vec(),
            },
            "expected value",
        ),
    ];

    for (height, (origin, m, needle)) in rejects.into_iter().enumerate() {
        let height = height as u64 + 5;
        reject_roundtrip(
            &mut native,
            &mut wasm,
            &ids,
            &member_keys,
            height,
            origin,
            m,
            needle,
        )
        .await;
    }
}

#[test]
fn multi_dispatch_block_reads_prior_writes_and_isolates_rejections() {
    deterministic::Runner::default().start(multi_dispatch_inner);
}

async fn multi_dispatch_inner(context: deterministic::Context) {
    // ONE member: the valset pool is [A], so every rendezvous assigns A and
    // the strict gate accepts A's results — deterministic by construction.
    let a = key(0xAA);
    let members = vec![a.clone()];
    let member_keys: [&[u8]; 1] = [&a];
    let ids = [sid(&a, "s1"), sid(&a, "s2")];

    let mut native = native_host(&context, "multi_native", &members).await;
    let mut wasm = wasm_host_(&context, "multi_wasm", &members).await;

    // ONE block, three ops: the result reads the STAGED trigger (and its
    // staged lease), and the prune reads the STAGED terminal state — on the
    // wasm side each later dispatch reloads the prior dispatch's staged
    // `__state` (the read-your-writes seam the adapter relies on). the P6
    // callback still fires from the middle dispatch.
    let batch = vec![
        (
            Origin::External(a.clone()),
            saga_op(&trigger(&sid(&a, "s1")).into()),
        ),
        (
            Origin::External(a.clone()),
            oracle(&sid(&a, "s1"), 0, Ok(b"same-block".to_vec())),
        ),
        (
            Origin::External(a.clone()),
            saga_op(&SagaMsg::Prune {
                saga_ids: vec![sid(&a, "s1")],
            }),
        ),
    ];
    let n_out = native
        .submit_block(block(1, Origin::External(a.clone())), batch.clone())
        .await
        .expect("native block");
    let w_out = wasm
        .submit_block(block(1, Origin::External(a.clone())), batch)
        .await
        .expect("wasm block");
    for out in [&n_out, &w_out] {
        assert!(
            out.members
                .iter()
                .all(|m| matches!(m, MemberOutcome::Applied { .. })),
            "all members must apply: {:?}",
            out.members
        );
    }
    let tuples = |events: &[sdk::Event]| -> Vec<(String, Vec<u8>)> {
        events
            .iter()
            .map(|e| (e.source.clone(), e.payload.clone()))
            .collect()
    };
    assert_eq!(tuples(&n_out.events), tuples(&w_out.events));
    assert_eq!(
        replies(&native, &ids, &member_keys).await,
        replies(&wasm, &ids, &member_keys).await
    );
    // triggered, resolved, and pruned within one block: nothing remains.
    let reply = wasm
        .query(
            "saga",
            &encode_query(&SagaQuery::Get {
                saga_id: sid(&a, "s1"),
            }),
        )
        .await
        .expect("get");
    assert_eq!(decode_reply(&reply).expect("decode"), SagaReply::Saga(None));
    // ...but the callback COMMITTED (the recorder left its empty root).
    let empty_recorder = StateRoot(sha2::Sha256::digest([]).into());
    assert_ne!(
        native.module_root("req"),
        Some(empty_recorder),
        "the same-block callback committed"
    );
    assert_eq!(native.module_root("req"), wasm.module_root("req"));

    // ONE block where the SECOND member rejects: the runtime aborts the
    // staged overlay and replays the accepted member — committed state equals
    // the accepted subset alone, on both runtimes.
    let (n_before, w_before) = (root_of(&native), root_of(&wasm));
    let batch = vec![
        (
            Origin::External(a.clone()),
            saga_op(&trigger(&sid(&a, "s2")).into()),
        ),
        (
            Origin::External(a.clone()),
            saga_op(
                &Trig {
                    max_attempts: 0,
                    ..trigger(&sid(&a, "s3"))
                }
                .into(),
            ),
        ),
    ];
    let n_out = native
        .submit_block(block(2, Origin::External(a.clone())), batch.clone())
        .await
        .expect("native block");
    let w_out = wasm
        .submit_block(block(2, Origin::External(a.clone())), batch)
        .await
        .expect("wasm block");
    for out in [&n_out, &w_out] {
        assert!(matches!(out.members[0], MemberOutcome::Applied { .. }));
        assert!(matches!(out.members[1], MemberOutcome::Rejected { .. }));
    }
    assert_ne!(root_of(&native), n_before, "the accepted member landed");
    assert_ne!(root_of(&wasm), w_before, "the accepted member landed");
    assert_eq!(
        replies(&native, &ids, &member_keys).await,
        replies(&wasm, &ids, &member_keys).await
    );
    assert!(matches!(
        decode_reply(
            &wasm
                .query(
                    "saga",
                    &encode_query(&SagaQuery::Get {
                        saga_id: sid(&a, "s2")
                    })
                )
                .await
                .expect("get")
        )
        .expect("decode"),
        SagaReply::Saga(Some(_))
    ));
}

/// which of `ids` a host still answers a saga for — the retention set, named
/// so a divergence says WHICH receipt went instead of just "roots differ".
async fn retained(h: &Host, ids: &[String]) -> Vec<String> {
    let mut kept = Vec::new();
    for id in ids {
        let reply = h
            .query(
                "saga",
                &encode_query(&SagaQuery::Get {
                    saga_id: id.clone(),
                }),
            )
            .await
            .expect("get");
        if matches!(
            decode_reply(&reply).expect("decode"),
            SagaReply::Saga(Some(_))
        ) {
            kept.push(id.clone());
        }
    }
    kept
}

#[test]
fn a_block_that_crosses_the_retention_cap_evicts_identically_on_both_runtimes() {
    deterministic::Runner::default().start(retention_parity_inner);
}

async fn retention_parity_inner(context: deterministic::Context) {
    // the hazard the per-op trim exists FOR. the wasm shell calls the guest's
    // inner `commit_block` once per OP, the native module once per BLOCK, so a
    // trim that read the committed map at the boundary would evict at a
    // different point under the two — and this is the block that would expose
    // it: one block settling MAX_RETAINED_TERMINAL + 1 sagas, i.e. crossing
    // the cap MID-block. staged inside the op, both runtimes must drop the
    // same id and answer identically afterwards.
    let a = key(0xAA);
    let members = vec![a.clone()];
    let member_keys: [&[u8]; 1] = [&a];

    let mut native = native_host(&context, "retention_native", &members).await;
    let mut wasm = wasm_host_(&context, "retention_wasm", &members).await;
    let (n_before, w_before) = (root_of(&native), root_of(&wasm));

    let ids: Vec<String> = (0..=MAX_RETAINED_TERMINAL)
        .map(|i| sid(&a, &format!("s{i:04}")))
        .collect();
    let mut batch: Vec<(Origin, Msg)> = ids
        .iter()
        .flat_map(|id| {
            [
                (Origin::External(a.clone()), saga_op(&trigger(id).into())),
                (
                    Origin::External(a.clone()),
                    oracle(id, 0, Ok(b"agreed".to_vec())),
                ),
            ]
        })
        .collect();
    // the op that MAKES the mid-block eviction observable: re-trigger the id
    // the crossing just freed, in the same block. once evicted it is new work
    // (a fresh pending saga and its work order); left in place until the
    // boundary it is the duplicate NO-OP instead — so a trim that moved to
    // `commit_block` diverges the two runtimes here, in events and in state.
    batch.push((
        Origin::External(a.clone()),
        saga_op(&trigger(&ids[0]).into()),
    ));

    let n_out = native
        .submit_block(block(1, Origin::External(a.clone())), batch.clone())
        .await
        .expect("native block");
    let w_out = wasm
        .submit_block(block(1, Origin::External(a.clone())), batch)
        .await
        .expect("wasm block");
    for out in [&n_out, &w_out] {
        assert!(
            out.members
                .iter()
                .all(|m| matches!(m, MemberOutcome::Applied { .. })),
            "all members must apply: {:?}",
            out.members
        );
    }
    let tuples = |events: &[sdk::Event]| -> Vec<(String, Vec<u8>)> {
        events
            .iter()
            .map(|e| (e.source.clone(), e.payload.clone()))
            .collect()
    };
    assert_eq!(
        tuples(&n_out.events),
        tuples(&w_out.events),
        "event traces diverge across the cap"
    );
    assert_eq!(
        retained(&native, &ids).await,
        retained(&wasm, &ids).await,
        "the runtimes retained different sagas"
    );
    assert_eq!(
        replies(&native, &ids, &member_keys).await,
        replies(&wasm, &ids, &member_keys).await
    );
    // both roots are the same store's root, so the cap-crossing block must
    // leave them byte-identical — a trim that bit at a different point under
    // the two runtimes would show up right here.
    assert_eq!(
        root_of(&native),
        root_of(&wasm),
        "the runtimes committed different roots across the cap"
    );
    for sibling in SIBLING_IDS {
        assert_eq!(
            native.module_root(sibling),
            wasm.module_root(sibling),
            "the {sibling} sibling diverged across the cap"
        );
    }
    assert_ne!(root_of(&native), n_before, "native root stuck");
    assert_ne!(root_of(&wasm), w_before, "wasm root stuck");

    // and the trim actually bit MID-block on both: one consensus_time for the
    // whole block, so the id breaks every tie and the LOWEST one goes — which
    // is why its re-trigger landed as live work instead of a duplicate no-op.
    for h in [&native, &wasm] {
        assert_eq!(
            view_of(h, &ids[0]).await.status,
            SagaStatus::Pending,
            "the freed id must be new work to the rest of the block"
        );
        assert_eq!(
            view_of(h, ids.last().expect("ids")).await.status,
            SagaStatus::Done,
            "the newest receipt always survives"
        );
    }
}
