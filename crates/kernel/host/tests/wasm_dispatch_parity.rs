//! the STORE-BACKED cutover-continuity proof for the `dispatch` task plane: the
//! dispatch guest component over `WasmModule::with_store(QmdbStore)` and the
//! native [`DispatchModule`] over the same store shape are ROOT-CONTINUOUS —
//! the same op sequence commits the IDENTICAL qmdb merkle root after every
//! block. both roots ARE the store's root; qmdb's batch canonicalizes mutations
//! by hashed key, so the native logical-key commit order and the wasm hashed-key
//! drain order produce the same op log. this executor swap changes not one
//! committed byte — including the byte-identical NO-OP blocks (a duplicate
//! `Dispatch`, a `Nudge`, a correlation-mismatched saga callback) that stage
//! nothing on either side.
//!
//! ## what the matrix drives
//!
//! the WHOLE op family, not just the admin surface: register / update / remove
//! a recipe, dispatch under a module origin, the saga's terminal callback (the
//! contract judgement + mailbox enqueue), the System-origin `DeliverPending`
//! sweep (the payload drop), cancel, reassign and nudge. query replies match
//! after every block over the whole read matrix, rejections carry the native
//! reason and leave both roots byte-identical, and the two stand-in siblings
//! record identical routing — the saga trigger/cancel/reassign lane and the
//! receiver's `ResultEvent` lane.
//!
//! ## why there are no sibling reads on the accept path
//!
//! dispatch is a SELF-CONTAINED plane: its `execute` reads only `ctx.env()` and
//! EMITS follow-ups (a saga `Trigger`, event breadcrumbs); it makes no
//! cross-module `query-module` reads, so — unlike attribution/runs — the guest needs
//! no memoized sibling replay.
//!
//! ## PART 2: the committed-only query lane
//!
//! dispatch answers `Module::query` from COMMITTED state alone regardless of
//! caller, so a same-block staged write never leaks into the host's delivery
//! injection or runs' `turn_taken` read. on the wasm side the component
//! declares it (`shape().committed_queries`), and the host drops the outer
//! staged overlay for a query round so `WitStore` serves the native module's
//! `get_committed` reads exactly as the native store does.

use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use dispatch::{
    DispatchModule, DispatchMsg, DispatchQuery, DispatchReply, DispatchStatus, OutputContract,
    Routing, decode_reply, encode_msg, encode_query,
};
use host::{BlockContext, Host, MemberOutcome, SubmitError};
use saga::{SagaCallback, SagaOutcome, encode_callback};
use sdk::{Ctx, Error, Event, Module, ModuleId, Msg, Origin, StateRoot, StateSyncHandle};
use sha2::{Digest, Sha256};
use statesync::qmdb::QmdbStore;
use wasm_host::WasmModule;

/// GENERATED artifact — built from the `dispatch` module's guest port by
/// guest-builder (`make wasm-modules`); committed so this proof is self-contained.
const DISPATCH_WASM: &[u8] = include_bytes!("fixtures/dispatch.component.wasm");

/// EXACTLY the production wiring in bin/node's host state: the saga collaborator
/// id is genesis config (not committed state), so both runtimes — and the guest
/// itself — must wire the same id or the routing forks.
async fn native_dispatch(context: &deterministic::Context, label: &'static str) -> DispatchModule {
    DispatchModule::new(
        "dispatch",
        "saga",
        "identity",
        Box::new(QmdbStore::init(context.child(label), "dispatch").await),
    )
}

async fn wasm_dispatch(context: &deterministic::Context, label: &'static str) -> WasmModule {
    WasmModule::with_store(
        "dispatch",
        DISPATCH_WASM,
        Box::new(QmdbStore::init(context.child(label), "dispatch").await),
    )
    // dispatch's query surface is committed-only regardless of caller (the
    // native contract): a same-block staged write must never leak into a
    // mid-block sibling read. the component declares it (`shape`), so the
    // load applies it — nothing to wire here.
    .expect("load component")
}

/// a stand-in sibling that records every follow-up `Msg` delivered to it — under
/// the native staging contract, so an aborted block leaves no trace here either
/// — and serves its committed log via its query surface. two of these stand
/// beside dispatch: `"saga"` (the trigger / cancel / reassign lane) and
/// `"caller"` (the receiver a `ResultEvent` is delivered to). comparing the logs
/// across runtimes is the routing-parity claim.
struct Recorder {
    id: ModuleId,
    committed: Vec<(String, Vec<u8>)>,
    staged: Vec<(String, Vec<u8>)>,
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
        if self.committed.is_empty() {
            return StateRoot::ZERO;
        }
        let mut h = Sha256::new();
        for (who, payload) in &self.committed {
            h.update((who.len() as u64).to_le_bytes());
            h.update(who.as_bytes());
            h.update((payload.len() as u64).to_le_bytes());
            h.update(payload);
        }
        StateRoot(h.finalize().into())
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        self.staged
            .push((ctx.env().origin.actor_string(), msg.payload.clone()));
        Ok(())
    }

    async fn query(&self, _req: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(serde_json::to_vec(&self.committed).expect("serializable"))
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

/// a sibling that probes dispatch's query surface MID-BLOCK (PART 2): on delivery
/// it issues `ctx.query("dispatch", <payload>)` — the payload IS the encoded
/// [`DispatchQuery`] — and records the raw reply. dispatch answers its query
/// surface from COMMITTED state alone regardless of caller, so a recipe
/// registered earlier in the SAME block must read back `Recipe(None)` here; this
/// probe is how the same-block matrix observes that, identically on both runtimes.
struct RecipeProbe {
    id: ModuleId,
    committed: Vec<Vec<u8>>,
    staged: Vec<Vec<u8>>,
}

impl RecipeProbe {
    fn new(id: &str) -> Self {
        Self {
            id: id.into(),
            committed: Vec::new(),
            staged: Vec::new(),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Module for RecipeProbe {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    fn root(&self) -> StateRoot {
        if self.committed.is_empty() {
            return StateRoot::ZERO;
        }
        let mut h = Sha256::new();
        for reply in &self.committed {
            h.update((reply.len() as u64).to_le_bytes());
            h.update(reply);
        }
        StateRoot(h.finalize().into())
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        // the mid-block sibling read: dispatch answers committed-only, so a
        // recipe registered earlier in THIS block is invisible here.
        let reply = ctx.query("dispatch", &msg.payload).await?;
        self.staged.push(reply);
        Ok(())
    }

    async fn query(&self, _req: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(serde_json::to_vec(&self.committed).expect("serializable"))
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

async fn native_host(context: &deterministic::Context, label: &'static str) -> Host {
    Host::genesis(vec![
        Box::new(native_dispatch(context, label).await),
        Box::new(Recorder::new("saga")),
        Box::new(Recorder::new("caller")),
    ])
    .expect("genesis")
}

async fn wasm_host_(context: &deterministic::Context, label: &'static str) -> Host {
    Host::genesis(vec![
        Box::new(wasm_dispatch(context, label).await),
        Box::new(Recorder::new("saga")),
        Box::new(Recorder::new("caller")),
    ])
    .expect("genesis")
}

fn op(m: &DispatchMsg) -> Msg {
    Msg {
        target: "dispatch".into(),
        payload: encode_msg(m),
    }
}

fn external(key: &[u8]) -> Origin {
    Origin::External(key.to_vec())
}

fn caller() -> Origin {
    Origin::Module("caller".into())
}

/// the composite state key dispatch namespaces a receiver's dispatch under, and
/// the saga id it derives from that key — both re-derived here so the callback
/// correlates exactly as the module's own derivation does.
fn key_of(receiver: &str, dispatch_id: &str) -> String {
    format!("{receiver}\x1f{dispatch_id}")
}

fn callback_op(key: &str, outcome: SagaOutcome) -> Msg {
    Msg {
        target: "dispatch".into(),
        payload: encode_callback(&SagaCallback {
            saga_id: format!("dispatch\x1f{key}"),
            payload: key.as_bytes().to_vec(),
            outcome,
        }),
    }
}

/// one block's agreed context: both runtimes see the identical env. `consensus_time`
/// is height-derived, so recipe `created_at`/`updated_at` match across runtimes.
fn block(height: u64, origin: Origin) -> BlockContext {
    BlockContext {
        height,
        consensus_time: 1_000 + height,
        origin,
    }
}

fn root_of(h: &Host) -> StateRoot {
    h.module_root("dispatch").expect("dispatch registered")
}

/// a stand-in's committed delivery log, decoded — the plane's observable routing
/// output. equal across runtimes after every block.
async fn deliveries(h: &Host, who: &str) -> Vec<(String, Vec<u8>)> {
    let reply = h.query(who, &[]).await.expect("recorder query");
    serde_json::from_slice(&reply).expect("recorder log decodes")
}

/// the whole committed read matrix: a recipe hit and an absent point read, both
/// dispatch records, and the mailbox census the host's delivery injection keys
/// on.
async fn replies(h: &Host) -> Vec<Vec<u8>> {
    let queries = [
        encode_query(&DispatchQuery::Recipe {
            recipe_id: "summarize".into(),
        }),
        encode_query(&DispatchQuery::Recipe {
            recipe_id: "absent".into(),
        }),
        encode_query(&DispatchQuery::Dispatch {
            receiver: "caller".into(),
            dispatch_id: "d1".into(),
        }),
        encode_query(&DispatchQuery::Dispatch {
            receiver: "caller".into(),
            dispatch_id: "d2".into(),
        }),
        encode_query(&DispatchQuery::Dispatch {
            receiver: "caller".into(),
            dispatch_id: "absent".into(),
        }),
        encode_query(&DispatchQuery::PendingDeliveries),
    ];
    let mut out = Vec::new();
    for q in &queries {
        out.push(h.query("dispatch", q).await.expect("query"));
    }
    out
}

async fn dispatch_view(h: &Host, dispatch_id: &str) -> Option<dispatch::DispatchView> {
    let reply = h
        .query(
            "dispatch",
            &encode_query(&DispatchQuery::Dispatch {
                receiver: "caller".into(),
                dispatch_id: dispatch_id.into(),
            }),
        )
        .await
        .expect("query");
    match decode_reply(&reply).expect("decode") {
        DispatchReply::Dispatch(v) => v,
        other => panic!("expected a Dispatch reply, got {other:?}"),
    }
}

fn event_tuples(events: &[Event]) -> Vec<(String, Vec<u8>)> {
    events
        .iter()
        .map(|e| (e.source.clone(), e.payload.clone()))
        .collect()
}

/// a recipe with every field populated — the registration the matrix opens on.
fn full_recipe() -> DispatchMsg {
    DispatchMsg::RegisterRecipe {
        recipe_id: "summarize".into(),
        description: "summarize a thread".into(),
        capability: "alpha".into(),
        routing: Routing::Pinned(vec![7u8; 32]),
        output_contract: OutputContract::Json,
        max_attempts: 3,
        deadline_views: Some(100),
        lease_views: Some(20),
    }
}

fn dispatch_op(dispatch_id: &str) -> DispatchMsg {
    DispatchMsg::Dispatch {
        dispatch_id: dispatch_id.into(),
        recipe_id: "summarize".into(),
        payload: b"the entire input".to_vec(),
        demands: Default::default(),
        admission: dispatch::AdmissionPolicy::Queue,
    }
}

/// submit one op to BOTH hosts at `height`, require BOTH accept, and assert the
/// full between-blocks parity: identical dispatch traces + events, identical
/// stand-in deliveries, roots that move-or-hold together per `moves` — and land
/// on the SAME value, because the port is root-continuous.
async fn accept(
    native: &mut Host,
    wasm: &mut Host,
    height: u64,
    origin: Origin,
    msg: Msg,
    moves: bool,
) {
    let (n_before, w_before) = (root_of(native), root_of(wasm));
    let n_out = native
        .submit_at(block(height, origin.clone()), msg.clone())
        .await
        .expect("native must accept");
    let w_out = wasm
        .submit_at(block(height, origin), msg)
        .await
        .expect("wasm must accept");

    assert_eq!(
        n_out.dispatches, w_out.dispatches,
        "dispatch traces diverge at block {height}"
    );
    assert_eq!(
        event_tuples(&n_out.events),
        event_tuples(&w_out.events),
        "events diverge at block {height}"
    );
    for who in ["saga", "caller"] {
        assert_eq!(
            deliveries(native, who).await,
            deliveries(wasm, who).await,
            "{who} deliveries diverge after block {height}"
        );
    }
    assert_eq!(
        replies(native).await,
        replies(wasm).await,
        "replies diverge after block {height}"
    );
    if moves {
        assert_ne!(root_of(native), n_before, "native root stuck at {height}");
        assert_ne!(root_of(wasm), w_before, "wasm root stuck at {height}");
    } else {
        assert_eq!(root_of(native), n_before, "native root moved at {height}");
        assert_eq!(root_of(wasm), w_before, "wasm root moved at {height}");
    }
    assert_eq!(
        root_of(native),
        root_of(wasm),
        "roots diverge after block {height} — the port must be root-continuous"
    );
}

/// submit one op to BOTH hosts at `height`, require BOTH reject with the native
/// module's reason (the wasm runtime wraps it in its wit-error rendering, so the
/// claim is containment), and assert the abort left NO trace: both roots
/// byte-identical to pre-block, the stand-ins unchanged on both.
async fn reject(
    native: &mut Host,
    wasm: &mut Host,
    height: u64,
    origin: Origin,
    msg: Msg,
    needle: &str,
) {
    let (n_before, w_before) = (root_of(native), root_of(wasm));
    let saga_before = deliveries(native, "saga").await;

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

    // the abort path: staged writes discarded, no trace on either root.
    assert_eq!(root_of(native), n_before, "native root moved on reject");
    assert_eq!(root_of(wasm), w_before, "wasm root moved on reject");
    // and no delivery escaped the aborted block, on either runtime.
    for (host, label) in [(&*native, "native"), (&*wasm, "wasm")] {
        assert_eq!(
            deliveries(host, "saga").await,
            saga_before,
            "{label} saga log moved on reject"
        );
    }
    assert_eq!(
        root_of(native),
        root_of(wasm),
        "roots diverge on a rejected block {height}"
    );
}

#[test]
fn same_ops_same_replies_and_roots_stay_continuous() {
    deterministic::Runner::default().start(|context| async move {
        same_ops_inner(&context).await;
    });
}

async fn same_ops_inner(context: &deterministic::Context) {
    let mut native = native_host(context, "same_native").await;
    let mut wasm = wasm_host_(context, "same_wasm").await;
    let alice = external(b"alice");
    let bob = external(b"bob");

    // ROOT CONTINUITY starts at genesis: both roots ARE the (empty) qmdb
    // store's merkle root, so the cutover moves nothing.
    assert_eq!(
        root_of(&native),
        root_of(&wasm),
        "genesis roots must match — the port is root-continuous"
    );

    // the admin surface: register (all fields), a duplicate rejection, an
    // update by the registrant, an update by anyone else.
    accept(
        &mut native,
        &mut wasm,
        1,
        alice.clone(),
        op(&full_recipe()),
        true,
    )
    .await;
    reject(
        &mut native,
        &mut wasm,
        2,
        alice.clone(),
        op(&full_recipe()),
        "already exists",
    )
    .await;
    let update = DispatchMsg::UpdateRecipe {
        recipe_id: "summarize".into(),
        description: Some("summarize, tersely".into()),
        capability: None,
        routing: None,
        output_contract: None,
        max_attempts: Some(5),
    };
    accept(&mut native, &mut wasm, 3, alice.clone(), op(&update), true).await;
    let by_anyone = DispatchMsg::UpdateRecipe {
        recipe_id: "summarize".into(),
        description: Some("summarize, terser still".into()),
        capability: None,
        routing: None,
        output_contract: None,
        max_attempts: None,
    };
    accept(&mut native, &mut wasm, 4, bob.clone(), op(&by_anyone), true).await;

    // the run surface, under a MODULE origin (the receiver of the result):
    // two dispatches, a duplicate that is a deterministic NO-OP (roots hold on
    // both), a reassign and a cancel that route to saga without touching state.
    accept(
        &mut native,
        &mut wasm,
        5,
        caller(),
        op(&dispatch_op("d1")),
        true,
    )
    .await;
    accept(
        &mut native,
        &mut wasm,
        6,
        caller(),
        op(&dispatch_op("d2")),
        true,
    )
    .await;
    accept(
        &mut native,
        &mut wasm,
        7,
        caller(),
        op(&dispatch_op("d1")),
        false,
    )
    .await;
    accept(
        &mut native,
        &mut wasm,
        8,
        caller(),
        op(&DispatchMsg::ReassignDispatch {
            dispatch_id: "d1".into(),
            attempt: 1,
        }),
        false,
    )
    .await;
    accept(
        &mut native,
        &mut wasm,
        9,
        caller(),
        op(&DispatchMsg::CancelDispatch {
            dispatch_id: "d2".into(),
        }),
        false,
    )
    .await;

    // the saga's terminal callbacks: `d1` passes the Json contract, `d2`'s
    // cancellation lands as an Err. each enqueues a mailbox delivery.
    //
    // the never-pop-stack rule runs itself here: the HOST injects one
    // System-origin `DeliverPending` per block whose PRE-block committed
    // mailbox is non-empty. so block 10's enqueue is delivered by block 11's
    // injection (with d2's, staged earlier in that same block), the outcome
    // payloads are dropped, and the mailbox cursor key disappears with them.
    let saga = Origin::Module("saga".into());
    accept(
        &mut native,
        &mut wasm,
        10,
        saga.clone(),
        callback_op(
            &key_of("caller", "d1"),
            SagaOutcome::Done(br#"{"ok":1}"#.to_vec()),
        ),
        true,
    )
    .await;
    assert!(
        deliveries(&wasm, "caller").await.is_empty(),
        "a result must never reach its receiver in the block that agreed on it"
    );
    accept(
        &mut native,
        &mut wasm,
        11,
        saga.clone(),
        callback_op(&key_of("caller", "d2"), SagaOutcome::Cancelled),
        true,
    )
    .await;
    assert_eq!(
        deliveries(&wasm, "caller").await.len(),
        1,
        "the second result cannot deliver in its own callback block"
    );
    // a callback for an UNKNOWN key is a deterministic no-op on both runtimes —
    // while the second result is delivered from the previous committed boundary.
    accept(
        &mut native,
        &mut wasm,
        12,
        saga.clone(),
        callback_op(&key_of("caller", "ghost"), SagaOutcome::Done(b"x".to_vec())),
        true,
    )
    .await;
    // An idle pump must leave the already-drained mailbox unchanged.
    accept(
        &mut native,
        &mut wasm,
        13,
        Origin::System,
        op(&DispatchMsg::Nudge {}),
        false,
    )
    .await;
    // the permissionless pump stages nothing at all.
    accept(
        &mut native,
        &mut wasm,
        14,
        alice.clone(),
        op(&DispatchMsg::Nudge {}),
        false,
    )
    .await;
    // and the recipe removal drops both its record AND (the last id) its index.
    accept(
        &mut native,
        &mut wasm,
        15,
        alice.clone(),
        op(&DispatchMsg::RemoveRecipe {
            recipe_id: "summarize".into(),
        }),
        true,
    )
    .await;

    // decoded spot checks on the wasm side (the native side is asserted equal
    // reply-for-reply after every block above).
    let d1 = dispatch_view(&wasm, "d1")
        .await
        .expect("the receipt survives");
    assert_eq!(
        d1.status,
        DispatchStatus::Delivered {
            delivery: sdk::DeliveryOutcome::Applied
        }
    );
    assert_eq!(d1.outcome, None, "delivery drops this module's copy");
    assert_eq!(d1.created_at, 1_005);
    assert_eq!(d1.updated_at, 1_011, "delivered by block 11's injection");
    let d2 = dispatch_view(&wasm, "d2")
        .await
        .expect("the receipt survives");
    assert_eq!(
        d2.status,
        DispatchStatus::Delivered {
            delivery: sdk::DeliveryOutcome::Applied
        }
    );

    // the receiver got BOTH results, in mailbox (FIFO) order, with every byte.
    let received = deliveries(&wasm, "caller").await;
    assert_eq!(received.len(), 2, "one ResultEvent per dispatch");
    let dispatch::Delivery::Result(first) =
        dispatch::decode_delivery(&received[0].1).expect("decode")
    else {
        panic!("result delivery")
    };
    assert_eq!(first.dispatch_id, "d1");
    assert_eq!(first.recipe_id, "summarize");
    assert_eq!(first.outcome, Ok(br#"{"ok":1}"#.to_vec()));
    let dispatch::Delivery::Result(second) =
        dispatch::decode_delivery(&received[1].1).expect("decode")
    else {
        panic!("result delivery")
    };
    assert_eq!(second.dispatch_id, "d2");
    assert_eq!(second.outcome, Err("cancelled".into()));

    // the removed recipe is gone from the point read.
    let reply = wasm
        .query(
            "dispatch",
            &encode_query(&DispatchQuery::Recipe {
                recipe_id: "summarize".into(),
            }),
        )
        .await
        .expect("query");
    assert_eq!(
        decode_reply(&reply).expect("decode"),
        DispatchReply::Recipe(None),
        "the removed recipe record is gone"
    );

    // queries are read-only on the wasm side too: the root is STABLE across the
    // whole read matrix.
    let settled = root_of(&wasm);
    let _ = replies(&wasm).await;
    assert_eq!(root_of(&wasm), settled, "a query moved the wasm root");
}

#[test]
fn rejections_match_and_leave_no_trace() {
    deterministic::Runner::default().start(|context| async move {
        rejections_inner(&context).await;
    });
}

async fn rejections_inner(context: &deterministic::Context) {
    let mut native = native_host(context, "rej_native").await;
    let mut wasm = wasm_host_(context, "rej_wasm").await;
    let alice = external(b"alice");

    // seed one recipe so the run-surface guards have live state.
    accept(
        &mut native,
        &mut wasm,
        1,
        alice.clone(),
        op(&full_recipe()),
        true,
    )
    .await;

    let rejects: Vec<(Origin, Msg, &str)> = vec![
        (
            alice.clone(),
            op(&DispatchMsg::RegisterRecipe {
                recipe_id: String::new(),
                description: String::new(),
                capability: "alpha".into(),
                routing: Routing::Rendezvous,
                output_contract: OutputContract::Text,
                max_attempts: 1,
                deadline_views: None,
                lease_views: None,
            }),
            "recipe_id must be non-empty",
        ),
        (
            alice.clone(),
            op(&DispatchMsg::RegisterRecipe {
                recipe_id: "bad-tag".into(),
                description: String::new(),
                capability: "NOT A TAG".into(),
                routing: Routing::Rendezvous,
                output_contract: OutputContract::Text,
                max_attempts: 1,
                deadline_views: None,
                lease_views: None,
            }),
            "invalid characters",
        ),
        (
            alice.clone(),
            op(&DispatchMsg::RegisterRecipe {
                recipe_id: "no-pin".into(),
                description: String::new(),
                capability: "alpha".into(),
                routing: Routing::Pinned(Vec::new()),
                output_contract: OutputContract::Text,
                max_attempts: 1,
                deadline_views: None,
                lease_views: None,
            }),
            "Pinned key",
        ),
        (
            // the pin cap: a `Routing::Pinned` key is saga's `pinned_assignee`
            // verbatim, so registration refuses what saga would refuse at
            // trigger time. both ports must refuse it IDENTICALLY.
            alice.clone(),
            op(&DispatchMsg::RegisterRecipe {
                recipe_id: "huge-pin".into(),
                description: String::new(),
                capability: "alpha".into(),
                routing: Routing::Pinned(vec![7u8; saga::MAX_ASSIGNEE_BYTES + 1]),
                output_contract: OutputContract::Text,
                max_attempts: 1,
                deadline_views: None,
                lease_views: None,
            }),
            "routing Pinned key is",
        ),
        (
            alice.clone(),
            op(&DispatchMsg::RemoveRecipe {
                recipe_id: "ghost".into(),
            }),
            "unknown recipe",
        ),
        (
            // Dispatch is module-origin only: an external submitter has no
            // execute intake, so nothing could ever receive its result.
            alice.clone(),
            op(&dispatch_op("nope")),
            "module-origin only",
        ),
        (
            caller(),
            op(&DispatchMsg::Dispatch {
                dispatch_id: "unknown-recipe".into(),
                recipe_id: "ghost".into(),
                payload: b"x".to_vec(),
                demands: Default::default(),
                admission: dispatch::AdmissionPolicy::Queue,
            }),
            "unknown recipe",
        ),
        (
            // a payload that is not a dispatch op at all: both sides reject
            // with the same serde rendering (decode runs inside the guest too).
            alice.clone(),
            Msg {
                target: "dispatch".into(),
                payload: br#"{"no_such_arm":{}}"#.to_vec(),
            },
            "unknown variant",
        ),
    ];

    for (height, (origin, msg, needle)) in rejects.into_iter().enumerate() {
        reject(
            &mut native,
            &mut wasm,
            height as u64 + 2,
            origin,
            msg,
            needle,
        )
        .await;
    }
}

#[test]
fn multi_dispatch_block_reads_prior_writes_and_isolates_rejections() {
    deterministic::Runner::default().start(|context| async move {
        multi_dispatch_inner(&context).await;
    });
}

async fn multi_dispatch_inner(context: &deterministic::Context) {
    let mut native = native_host(context, "multi_native").await;
    let mut wasm = wasm_host_(context, "multi_wasm").await;
    let alice = external(b"alice");

    // ONE block, two ops: the second op's recipe lookup READS the first op's
    // staged write (the recipe only exists in this block's overlay). on the
    // wasm side that read falls through `WitStore::get` to the host's OUTER
    // staged overlay — the read-your-writes seam the adapter relies on, since
    // the guest rebuilds the module (and its inner overlay) per dispatch.
    let batch = vec![
        (alice.clone(), op(&full_recipe())),
        (caller(), op(&dispatch_op("d1"))),
    ];
    let n_out = native
        .submit_block(block(1, alice.clone()), batch.clone())
        .await
        .expect("native block");
    let w_out = wasm
        .submit_block(block(1, alice.clone()), batch)
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
    assert_eq!(replies(&native).await, replies(&wasm).await);
    assert_eq!(root_of(&native), root_of(&wasm), "roots diverge");

    // ONE block where the SECOND member rejects: the runtime aborts the staged
    // overlay and replays the accepted member — committed state must equal the
    // accepted subset alone, on both runtimes.
    let (n_before, w_before) = (root_of(&native), root_of(&wasm));
    let batch = vec![
        (caller(), op(&dispatch_op("d2"))),
        (
            alice.clone(),
            op(&DispatchMsg::RemoveRecipe {
                recipe_id: "ghost".into(),
            }),
        ),
    ];
    let n_out = native
        .submit_block(block(2, alice.clone()), batch.clone())
        .await
        .expect("native block");
    let w_out = wasm
        .submit_block(block(2, alice), batch)
        .await
        .expect("wasm block");
    for out in [&n_out, &w_out] {
        assert!(matches!(out.members[0], MemberOutcome::Applied { .. }));
        assert!(matches!(out.members[1], MemberOutcome::Rejected { .. }));
    }
    assert_ne!(root_of(&native), n_before);
    assert_ne!(root_of(&wasm), w_before);
    assert_eq!(root_of(&native), root_of(&wasm), "roots diverge");
    assert_eq!(replies(&native).await, replies(&wasm).await);
    for host in [&native, &wasm] {
        let view = dispatch_view(host, "d2")
            .await
            .expect("the accepted member");
        assert!(matches!(view.status, DispatchStatus::AwaitingResult { .. }));
    }
}

/// the store-backed sync surface: the ported guest advertises EXACTLY what the
/// native module does — no byte snapshot, the store's resolver lane.
#[test]
fn sync_handle_matches_native() {
    deterministic::Runner::default().start(|context| async move {
        let native = native_dispatch(&context, "handle_native").await;
        let wasm = wasm_dispatch(&context, "handle_wasm").await;

        let n_handle = native.state_sync_handle().expect("native handle");
        let w_handle = wasm.state_sync_handle().expect("wasm handle");
        assert_eq!(n_handle, w_handle, "sync handles diverge");
        assert!(
            matches!(w_handle, StateSyncHandle::ResolverBacked { ref backend, .. } if backend == "qmdb"),
            "store-backed tenant must stay resolver-backed: {w_handle:?}"
        );
        assert!(
            native.snapshot_bytes().is_none(),
            "a store-backed module ships no byte snapshot"
        );
    });
}

/// PART 2: the committed-only query lane — the load-bearing contract of the
/// port. dispatch answers its query surface from COMMITTED state ALONE
/// regardless of caller, so a recipe registered earlier in the SAME block is
/// invisible to a mid-block sibling read. runs' consensus-visible sibling reads
/// (turn-taken, lease-holder) rely on this; without the component's
/// `committed_queries` declaration the guest's `WitStore` would serve the
/// host's staged overlay and leak the same-block write. this matrix pins the
/// contract: op 1 registers a recipe
/// (staged, uncommitted) and op 2 — a sibling in the SAME block — reads it back
/// through dispatch's query surface, and BOTH runtimes must answer
/// `Recipe(None)`.
#[test]
fn same_block_sibling_query_reads_dispatch_committed_only() {
    deterministic::Runner::default().start(|context| async move {
        same_block_inner(&context).await;
    });
}

async fn same_block_inner(context: &deterministic::Context) {
    let mut native = Host::genesis(vec![
        Box::new(native_dispatch(context, "probe_native").await),
        Box::new(RecipeProbe::new("probe")),
    ])
    .expect("genesis");
    let mut wasm = Host::genesis(vec![
        Box::new(wasm_dispatch(context, "probe_wasm").await),
        Box::new(RecipeProbe::new("probe")),
    ])
    .expect("genesis");
    let alice = external(b"alice");

    // ONE block, two ops: op 1 registers "summarize" on dispatch (staged, not yet
    // committed); op 2 delivers to the probe, whose execute queries dispatch for
    // that very recipe MID-BLOCK. the committed-only contract means the probe must
    // read `Recipe(None)` — the same-block registration is invisible on the query
    // surface, on native by construction and on wasm via the committed-only lane.
    let probe_req = encode_query(&DispatchQuery::Recipe {
        recipe_id: "summarize".into(),
    });
    let batch = vec![
        (alice.clone(), op(&full_recipe())),
        (
            alice.clone(),
            Msg {
                target: "probe".into(),
                payload: probe_req,
            },
        ),
    ];
    let n_out = native
        .submit_block(block(1, alice.clone()), batch.clone())
        .await
        .expect("native block");
    let w_out = wasm
        .submit_block(block(1, alice), batch)
        .await
        .expect("wasm block");
    for out in [&n_out, &w_out] {
        assert!(
            out.members
                .iter()
                .all(|m| matches!(m, MemberOutcome::Applied { .. })),
            "both ops must apply: {:?}",
            out.members
        );
    }

    let expected: Vec<DispatchReply> = vec![DispatchReply::Recipe(None)];
    for (host, label) in [(&native, "native"), (&wasm, "wasm")] {
        let raw = host.query("probe", &[]).await.expect("probe query");
        let logs: Vec<Vec<u8>> = serde_json::from_slice(&raw).expect("probe log decodes");
        let replies: Vec<DispatchReply> = logs
            .iter()
            .map(|b| decode_reply(b).expect("reply decodes"))
            .collect();
        assert_eq!(
            replies, expected,
            "{label} dispatch must answer the mid-block sibling query from committed state only"
        );
    }
}
