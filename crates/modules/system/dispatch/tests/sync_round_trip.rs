//! state-sync round trip over the REAL store: a joiner reconstructs a
//! byte-identical qmdb root by pulling the source store's operation range
//! through commonware's qmdb sync, then wraps a fresh [`DispatchModule`] around
//! the injected store — the sync lane that REPLACED this module's byte snapshot.
//!
//! the source drives ops through the real module so the op log is what a
//! validator produces, and it deliberately carries every shape a naive "export
//! live records and re-apply sorted" could not reproduce:
//!
//! * record OVERWRITES — a recipe update, a dispatch walking
//!   AwaitingResult → AwaitingDelivery → Delivered, and a call walking
//!   Queued → Completed → Delivered,
//! * record DELETES — a removed recipe, and the mailbox entries the host's
//!   acknowledgments retire,
//! * the DELIVERED receipts whose outcome bytes were dropped at delivery (the
//!   retention rule) sitting next to a still-PENDING delivery whose outcome is
//!   still in its record and whose mailbox entry still rides the cursor, and a
//!   still-QUEUED call whose claim and record ride the call cursor.
//!
//! a [`DispatchModule`] consumes its injected store, so the handoff-as-resolver
//! form is only reachable on the raw store: REOPEN the committed partitions
//! under the same id (exactly the recovery path a restarting node takes — the
//! deterministic runtime shares storage across child contexts).

use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use dispatch::identity_contract::{
    AccountView, Control, IdentityQuery, IdentityReply, ProgramStanding,
};
use dispatch::saga_contract::{SagaCallback, SagaOutcome, encode_callback};
use dispatch::{
    AdmissionPolicy, CallOutcome, CallStatus, CallView, DispatchModule, DispatchMsg, DispatchQuery,
    DispatchReply, DispatchStatus, DispatchView, OutputContract, PendingCall, Recipe, Routing,
    decode_reply, encode_msg, encode_query,
};
use sdk::{
    Ack, CallId, Cause, DeliveryOutcome, Env, Error, MerkleStore as _, Module, Msg, Origin,
    StateRoot, StateSyncHandle,
};
use sdk_testkit::TestCtx;
use statesync::qmdb::QmdbStore;

const DISPATCH: &str = "dispatch";
const SAGA: &str = "saga";
const IDENTITY: &str = "identity";
/// the module that queues calls, and the executor identity names for the
/// program account they run as.
const RUNS: &str = "runs";
const PROGRAM: u64 = 7;

fn ctx(height: u64, origin: Origin) -> TestCtx {
    TestCtx::with_env(Env {
        height,
        consensus_time: 1_000 + height,
        origin,
        me: DISPATCH.into(),
        cause: Cause::Direct,
    })
}

/// the requester's ctx: identity serves [`PROGRAM`] as an active program
/// executed by `runs`, which a `Call` is admitted against.
fn requester_ctx(height: u64) -> TestCtx {
    ctx(height, Origin::Module(RUNS.into())).on_query(IDENTITY, |req| {
        let IdentityQuery::Get { number } = dispatch::identity_contract::decode_query(req)
            .map_err(|e| Error::module("codec", e))?;
        let account = (number == PROGRAM).then(|| AccountView {
            number,
            name: "program".into(),
            control: Control::Program {
                controller: 1,
                executor: RUNS.into(),
                generation: 0,
                standing: ProgramStanding::Active,
            },
            keys: Vec::new(),
            avatar: None,
            bio: None,
            updated_at: 0,
        });
        Ok(dispatch::identity_contract::encode_reply(
            &IdentityReply::Account(account),
        ))
    })
}

/// drive one op through the REAL module path: execute + commit_block (one op
/// per block height), so the committed op log is what a validator produces.
async fn apply(module: &mut DispatchModule, height: u64, origin: Origin, payload: Vec<u8>) {
    apply_with(module, ctx(height, origin), payload).await;
}

async fn apply_with(module: &mut DispatchModule, mut ctx: TestCtx, payload: Vec<u8>) {
    let msg = Msg {
        target: DISPATCH.into(),
        payload,
    };
    module.execute(&mut ctx, &msg).await.expect("op applies");
    module.commit_block().await.expect("commit");
}

/// the host's between-block pump for one boundary: read the committed mailbox
/// head, acknowledge every item as applied, commit the delivery unit.
async fn deliver_all(module: &mut DispatchModule, height: u64) -> usize {
    let items = module.pending_items().await.expect("a well-formed mailbox");
    for item in &items {
        module
            .acknowledge(
                &mut ctx(height, Origin::System),
                &Ack {
                    item: item.item,
                    target: item.target.clone(),
                    outcome: DeliveryOutcome::Applied,
                },
            )
            .await
            .expect("ack");
    }
    module.commit_block().await.expect("commit");
    items.len()
}

fn call_id(step: u64) -> CallId {
    CallId {
        requester: RUNS.into(),
        invocation: "run-1".into(),
        step,
    }
}

fn call_op(step: u64) -> Vec<u8> {
    encode_msg(&DispatchMsg::Call {
        invocation: "run-1".into(),
        step,
        account: PROGRAM,
        target: "chat".into(),
        payload: b"the call input".to_vec(),
    })
}

/// the saga's terminal callback for `key` — the intake that records the checked
/// outcome and enqueues the delivery.
async fn callback(module: &mut DispatchModule, height: u64, key: &str, outcome: SagaOutcome) {
    apply(
        module,
        height,
        Origin::Module(SAGA.into()),
        encode_callback(&SagaCallback {
            saga_id: format!("dispatch\x1f{key}"),
            payload: key.as_bytes().to_vec(),
            outcome,
        }),
    )
    .await;
}

fn register(recipe_id: &str, capability: &str) -> Vec<u8> {
    encode_msg(&DispatchMsg::RegisterRecipe {
        recipe_id: recipe_id.into(),
        description: "round trip".into(),
        capability: capability.into(),
        routing: Routing::Pinned(vec![7u8; 32]),
        output_contract: OutputContract::Text,
        max_attempts: 2,
        deadline_views: Some(100),
        lease_views: None,
    })
}

fn dispatch_op(dispatch_id: &str) -> Vec<u8> {
    encode_msg(&DispatchMsg::Dispatch {
        dispatch_id: dispatch_id.into(),
        recipe_id: "summarize".into(),
        payload: b"the entire input".to_vec(),
        demands: Default::default(),
        admission: AdmissionPolicy::Queue,
    })
}

async fn recipe(module: &DispatchModule, recipe_id: &str) -> Option<Recipe> {
    let reply = module
        .query(&encode_query(&DispatchQuery::Recipe {
            recipe_id: recipe_id.into(),
        }))
        .await
        .expect("recipe");
    match decode_reply(&reply).expect("decode") {
        DispatchReply::Recipe(r) => r,
        other => panic!("expected Recipe, got {other:?}"),
    }
}

async fn view(module: &DispatchModule, dispatch_id: &str) -> Option<DispatchView> {
    let reply = module
        .query(&encode_query(&DispatchQuery::Dispatch {
            receiver: "caller".into(),
            dispatch_id: dispatch_id.into(),
        }))
        .await
        .expect("dispatch");
    match decode_reply(&reply).expect("decode") {
        DispatchReply::Dispatch(v) => v,
        other => panic!("expected Dispatch, got {other:?}"),
    }
}

async fn pending(module: &DispatchModule) -> u64 {
    let reply = module
        .query(&encode_query(&DispatchQuery::PendingDeliveries))
        .await
        .expect("pending");
    match decode_reply(&reply).expect("decode") {
        DispatchReply::PendingDeliveries(n) => n,
        other => panic!("expected PendingDeliveries, got {other:?}"),
    }
}

async fn pending_calls(module: &DispatchModule) -> Vec<PendingCall> {
    let reply = module
        .query(&encode_query(&DispatchQuery::PendingCalls))
        .await
        .expect("pending calls");
    match decode_reply(&reply).expect("decode") {
        DispatchReply::PendingCalls(calls) => calls,
        other => panic!("expected PendingCalls, got {other:?}"),
    }
}

async fn call(module: &DispatchModule, step: u64) -> Option<CallView> {
    let reply = module
        .query(&encode_query(&DispatchQuery::Call { id: call_id(step) }))
        .await
        .expect("call");
    match decode_reply(&reply).expect("decode") {
        DispatchReply::Call(view) => view,
        other => panic!("expected Call, got {other:?}"),
    }
}

#[test]
fn synced_store_reconstructs_source_root_and_every_read() {
    deterministic::Runner::default().start(|context| async move {
        let mut src = DispatchModule::new(
            DISPATCH,
            SAGA,
            IDENTITY,
            Box::new(QmdbStore::init(context.child("src"), "src").await),
        );
        let owner = Origin::External(b"owner".to_vec());
        let caller = Origin::Module("caller".into());

        // recipes: two registrations, one update that OVERWRITES a record, one
        // removal that DELETES one.
        apply(&mut src, 1, owner.clone(), register("summarize", "alpha")).await;
        apply(&mut src, 2, owner.clone(), register("classify", "beta")).await;
        apply(
            &mut src,
            3,
            owner.clone(),
            encode_msg(&DispatchMsg::UpdateRecipe {
                recipe_id: "summarize".into(),
                description: Some("updated".into()),
                capability: None,
                routing: None,
                output_contract: None,
                max_attempts: Some(5),
            }),
        )
        .await;
        apply(
            &mut src,
            4,
            owner.clone(),
            encode_msg(&DispatchMsg::RemoveRecipe {
                recipe_id: "classify".into(),
            }),
        )
        .await;

        // two dispatches and two calls. `done` and call 0 run the FULL
        // lifecycle and end Delivered — their outcome bytes dropped, their
        // mailbox entries deleted. `live` stops at AwaitingDelivery, so its
        // outcome IS in the record and its mailbox entry rides the committed
        // cursor; call 1 stays Queued, so its claim and record ride the call
        // cursor.
        apply(&mut src, 5, caller.clone(), dispatch_op("done")).await;
        apply(&mut src, 6, caller.clone(), dispatch_op("live")).await;
        apply_with(&mut src, requester_ctx(7), call_op(0)).await;
        apply_with(&mut src, requester_ctx(8), call_op(1)).await;
        callback(
            &mut src,
            9,
            "caller\x1fdone",
            SagaOutcome::Done(b"the whole result".to_vec()),
        )
        .await;
        apply(
            &mut src,
            10,
            Origin::System,
            encode_msg(&DispatchMsg::CompleteCall {
                enqueued: 0,
                id: call_id(0),
                outcome: CallOutcome::Applied {
                    output: b"the call output".to_vec(),
                    assigned: b"stamp".to_vec(),
                },
            }),
        )
        .await;
        assert_eq!(deliver_all(&mut src, 11).await, 2);
        callback(
            &mut src,
            12,
            "caller\x1flive",
            SagaOutcome::Failed("provider died".into()),
        )
        .await;

        // the module is resolver-backed: there is NO byte snapshot to ship.
        match src.state_sync_handle().expect("handle") {
            StateSyncHandle::ResolverBacked { backend, .. } => assert_eq!(backend, "qmdb"),
            other => panic!("expected ResolverBacked, got {other:?}"),
        }
        let src_root = src.root();
        assert_ne!(src_root, StateRoot::ZERO, "source must have a real root");
        let src_summarize = recipe(&src, "summarize").await.expect("the kept recipe");
        assert_eq!(src_summarize.description, "updated");
        assert_eq!(src_summarize.max_attempts, 5);
        assert_eq!(recipe(&src, "classify").await, None, "removed at height 4");
        let src_done = view(&src, "done").await.expect("the receipt survives");
        let src_live = view(&src, "live").await.expect("the pending delivery");
        assert_eq!(pending(&src).await, 1);
        let src_call_done = call(&src, 0).await.expect("the call receipt survives");
        let src_call_queued = call(&src, 1).await.expect("the queued call");
        let src_pending_calls = pending_calls(&src).await;
        assert_eq!(src_pending_calls.len(), 1);

        // the module consumed its store, so REOPEN the committed partitions as
        // a bare store for the handoff (drop first — one owner at a time).
        drop(src);
        let src_store = QmdbStore::init(context.child("src_serve"), "src").await;
        assert_eq!(
            src_store.root(),
            src_root,
            "reopened store must recover the committed root"
        );
        let target = src_store.sync_boundary_target().await;
        let resolver = src_store.into_resolver();

        // JOINER: rebuild on a FRESH namespace by pulling the proven op range,
        // then wrap the module around the injected store.
        let store = QmdbStore::sync_from(context.child("dst"), "dst", target, resolver)
            .await
            .expect("sync_from");
        let synced = DispatchModule::new(DISPATCH, SAGA, IDENTITY, Box::new(store));

        // THE PROPERTY: identical qmdb root — the root-hash linkage a joiner
        // needs at the boundary height.
        assert_eq!(
            synced.root(),
            src_root,
            "synced store root must equal the source root"
        );

        // and every read answers exactly like the source: the updated record
        // rode the sync verbatim, the removed one is still gone.
        assert_eq!(recipe(&synced, "summarize").await, Some(src_summarize));
        assert_eq!(
            recipe(&synced, "classify").await,
            None,
            "the removed recipe is gone"
        );

        // the DELIVERED receipt rode the sync with its payload dropped...
        assert_eq!(view(&synced, "done").await, Some(src_done.clone()));
        assert_eq!(
            src_done.status,
            DispatchStatus::Delivered {
                delivery: DeliveryOutcome::Applied
            }
        );
        assert_eq!(
            src_done.outcome, None,
            "delivery dropped this module's copy"
        );

        // ...and the still-PENDING delivery kept its outcome AND its mailbox
        // entry, so the joiner's host will deliver exactly one more item.
        assert_eq!(view(&synced, "live").await, Some(src_live.clone()));
        assert_eq!(src_live.status, DispatchStatus::AwaitingDelivery);
        assert_eq!(src_live.outcome, Some(Err("provider died".into())));
        assert_eq!(pending(&synced).await, 1);

        assert_eq!(view(&synced, "never-dispatched").await, None);

        // the call queue rode the sync whole: the delivered call's receipt
        // (outcome reduced to its summary), the queued call with its claim,
        // and the committed head batch the joiner's host will run next.
        assert_eq!(call(&synced, 0).await, Some(src_call_done.clone()));
        assert!(
            matches!(
                src_call_done.status,
                CallStatus::Delivered {
                    delivery: DeliveryOutcome::Applied,
                    ..
                }
            ),
            "got {:?}",
            src_call_done.status
        );
        assert_eq!(call(&synced, 1).await, Some(src_call_queued.clone()));
        assert_eq!(src_call_queued.status, CallStatus::Queued);
        assert_eq!(pending_calls(&synced).await, src_pending_calls);
        assert_eq!(call(&synced, 2).await, None);
    });
}
