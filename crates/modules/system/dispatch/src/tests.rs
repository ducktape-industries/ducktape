//! the dispatch plane's behaviour suite over an in-memory store.
//!
//! these tests assert BEHAVIOUR — the recipe admin surface, the saga callback
//! correlation gates, the call queue's admission and finalization, the
//! mailbox's between-block pump (`pending_items` + `acknowledge`), retention —
//! so the in-memory [`MemStore`] stands in for qmdb. the cross-node round trip
//! over the REAL store is `tests/sync_round_trip.rs`, and the wasm-vs-native
//! root continuity proof is the host crate's `wasm_dispatch_parity`.

use super::*;
use crate::identity_contract::{
    AccountView, decode_query as identity_decode_query, encode_reply as identity_encode_reply,
};
use crate::saga_contract::{decode_msg as saga_decode_msg, encode_callback};
use futures::executor::block_on;
use sdk::Env;
use sdk_testkit::{MemStore, TestCtx};

/// the requester module of every call here — the executor identity names.
const RUNS: &str = "runs";
/// the program account the requester executes.
const PROGRAM: AccountNumber = 7;

fn env(height: u64, origin: Origin, cause: Cause) -> Env {
    Env {
        height,
        consensus_time: height,
        origin,
        me: "dispatch".into(),
        cause,
    }
}

// dispatch's execute reads env and, for a Call, the identity sibling; the
// shared TestCtx serves that read and captures emitted msgs/events (read via
// msgs()/events()). module_root is never consulted.
fn mk_ctx(height: u64, origin: Origin) -> TestCtx {
    TestCtx::with_env(env(height, origin, Cause::Direct))
}

/// a ctx whose identity sibling answers `Get` from `accounts`.
fn with_identity(ctx: TestCtx, accounts: Vec<AccountView>) -> TestCtx {
    ctx.on_query("identity", move |req| {
        let IdentityQuery::Get { number } =
            identity_decode_query(req).map_err(|e| Error::module("codec", e))?;
        let account = accounts.iter().find(|a| a.number == number).cloned();
        Ok(identity_encode_reply(&IdentityReply::Account(account)))
    })
}

fn account(number: AccountNumber, control: Control) -> AccountView {
    AccountView {
        number,
        name: format!("account-{number}"),
        control,
        keys: Vec::new(),
        avatar: None,
        bio: None,
        updated_at: 0,
    }
}

fn program(
    number: AccountNumber,
    executor: &str,
    generation: u64,
    standing: ProgramStanding,
) -> AccountView {
    account(
        number,
        Control::Program {
            controller: 1,
            executor: executor.into(),
            generation,
            standing,
        },
    )
}

/// the requester's ctx: module origin `runs`, a Direct cause, identity
/// serving [`PROGRAM`] as an active program executed by `runs`.
fn requester_ctx(height: u64) -> TestCtx {
    with_identity(
        mk_ctx(height, Origin::Module(RUNS.into())),
        vec![program(PROGRAM, RUNS, 0, ProgramStanding::Active)],
    )
}

/// build the module the way a host does: concrete store first, injected as
/// `Box<dyn MerkleStore>`.
fn module() -> DispatchModule {
    DispatchModule::new("dispatch", "saga", "identity", Box::new(MemStore::new()))
}
fn exec(m: &mut DispatchModule, ctx: &mut TestCtx, payload: &DispatchMsg) -> Result<(), Error> {
    let msg = Msg {
        target: "dispatch".into(),
        payload: crate::encode_msg(payload),
    };
    block_on(m.execute(ctx, &msg))
}
fn commit(m: &mut DispatchModule) {
    block_on(m.commit_block()).unwrap();
}
fn register(kind: OutputContract, routing: Routing) -> DispatchMsg {
    DispatchMsg::RegisterRecipe {
        recipe_id: "summarize".into(),
        description: "test recipe".into(),
        capability: "alpha".into(),
        routing,
        output_contract: kind,
        max_attempts: 2,
        deadline_views: Some(100),
        lease_views: None,
    }
}
fn owner() -> Origin {
    Origin::External(b"owner".to_vec())
}
fn dispatch_op(id: &str, payload: &[u8]) -> DispatchMsg {
    DispatchMsg::Dispatch {
        dispatch_id: id.into(),
        recipe_id: "summarize".into(),
        payload: payload.to_vec(),
        demands: BTreeMap::new(),
        admission: AdmissionPolicy::Queue,
    }
}
/// run register (as the external owner) + dispatch (as module "caller"),
/// committed — the shared preamble of the callback/delivery tests.
fn registered_and_dispatched(m: &mut DispatchModule, kind: OutputContract) -> String {
    let mut ctx = mk_ctx(0, owner());
    exec(m, &mut ctx, &register(kind, Routing::Rendezvous)).unwrap();
    let mut ctx = mk_ctx(5, Origin::Module("caller".into()));
    exec(m, &mut ctx, &dispatch_op("d1", b"input")).unwrap();
    commit(m);
    dispatch_key("caller", "d1")
}
fn callback_for(
    m: &mut DispatchModule,
    at: u64,
    key: &str,
    outcome: SagaOutcome,
) -> Result<(), Error> {
    let mut ctx = mk_ctx(at, Origin::Module("saga".into()));
    let msg = Msg {
        target: "dispatch".into(),
        payload: encode_callback(&SagaCallback {
            saga_id: saga_id_for(key),
            payload: key.as_bytes().to_vec(),
            outcome,
        }),
    };
    block_on(m.execute(&mut ctx, &msg))
}
fn get_dispatch(m: &DispatchModule, key: &str) -> Option<DispatchView> {
    // the tests address dispatches by their composite state key; split it
    // back into the wire query's (receiver, local id) coordinates.
    let (receiver, dispatch_id) = key.split_once(SEP).expect("composite key");
    let reply = block_on(m.query(&crate::encode_query(&DispatchQuery::Dispatch {
        receiver: receiver.into(),
        dispatch_id: dispatch_id.into(),
    })))
    .unwrap();
    match crate::decode_reply(&reply).unwrap() {
        DispatchReply::Dispatch(v) => v,
        other => panic!("expected Dispatch reply, got {other:?}"),
    }
}
fn pending_deliveries(m: &DispatchModule) -> u64 {
    let reply = block_on(m.query(&crate::encode_query(&DispatchQuery::PendingDeliveries))).unwrap();
    match crate::decode_reply(&reply).unwrap() {
        DispatchReply::PendingDeliveries(n) => n,
        other => panic!("expected PendingDeliveries reply, got {other:?}"),
    }
}
fn recipe(m: &DispatchModule, recipe_id: &str) -> Option<Recipe> {
    let reply = block_on(m.query(&crate::encode_query(&DispatchQuery::Recipe {
        recipe_id: recipe_id.into(),
    })))
    .unwrap();
    match crate::decode_reply(&reply).unwrap() {
        DispatchReply::Recipe(r) => r,
        other => panic!("expected Recipe reply, got {other:?}"),
    }
}
/// the committed byte size of one dispatch record — what the retention pin
/// measures now that the state IS the store.
fn record_bytes(m: &DispatchModule, key: &str) -> usize {
    block_on(m.staged.get_committed(&dispatch_key_of(key)))
        .unwrap()
        .expect("the record survives")
        .len()
}

// ---- the call queue and the mailbox: helpers ----------------------------------

fn call_op(invocation: &str, step: u64, target: &str, payload: &[u8]) -> DispatchMsg {
    DispatchMsg::Call {
        invocation: invocation.into(),
        step,
        account: PROGRAM,
        target: target.into(),
        payload: payload.to_vec(),
    }
}
fn call_id(invocation: &str, step: u64) -> CallId {
    CallId {
        requester: RUNS.into(),
        invocation: invocation.into(),
        step,
    }
}
fn applied(output: &[u8]) -> CallOutcome {
    CallOutcome::Applied {
        output: output.to_vec(),
        assigned: b"stamp".to_vec(),
    }
}
/// the host's finalizer op for the call at `enqueued`.
fn complete(
    m: &mut DispatchModule,
    at: u64,
    enqueued: u64,
    id: CallId,
    outcome: CallOutcome,
) -> Result<(), Error> {
    let mut ctx = mk_ctx(at, Origin::System);
    exec(
        m,
        &mut ctx,
        &DispatchMsg::CompleteCall {
            enqueued,
            id,
            outcome,
        },
    )
}
fn pending_calls(m: &DispatchModule) -> Vec<PendingCall> {
    let reply = block_on(m.query(&crate::encode_query(&DispatchQuery::PendingCalls))).unwrap();
    match crate::decode_reply(&reply).unwrap() {
        DispatchReply::PendingCalls(calls) => calls,
        other => panic!("expected PendingCalls reply, got {other:?}"),
    }
}
fn call_view(m: &DispatchModule, id: &CallId) -> Option<CallView> {
    let reply = block_on(m.query(&crate::encode_query(&DispatchQuery::Call {
        id: id.clone(),
    })))
    .unwrap();
    match crate::decode_reply(&reply).unwrap() {
        DispatchReply::Call(view) => view,
        other => panic!("expected Call reply, got {other:?}"),
    }
}
/// the host's between-block read of the mailbox head.
fn pending_items(m: &DispatchModule) -> Result<Vec<PendingItem>, Error> {
    block_on(m.pending_items())
}
/// the host's acknowledgment of item `item`'s delivery to `target`, run in a
/// delivery unit at `at` and left STAGED (the caller commits or aborts).
fn ack(
    m: &mut DispatchModule,
    at: u64,
    item: u64,
    target: &str,
    outcome: DeliveryOutcome,
) -> Result<TestCtx, Error> {
    let mut ctx = mk_ctx(at, Origin::System);
    block_on(m.acknowledge(
        &mut ctx,
        &Ack {
            item,
            target: target.into(),
            outcome,
        },
    ))?;
    Ok(ctx)
}
/// deliver every committed mailbox item as `Applied` and commit — the host's
/// pump for one boundary, for tests that only need the queue moved on.
fn drain_applied(m: &mut DispatchModule, at: u64) -> Vec<PendingItem> {
    let items = pending_items(m).expect("a well-formed mailbox");
    for item in &items {
        ack(m, at, item.item, &item.target, DeliveryOutcome::Applied).expect("ack");
    }
    commit(m);
    items
}
fn delivered(payload: &[u8]) -> Delivery {
    crate::decode_delivery(payload).expect("a delivery envelope")
}

#[test]
fn recipe_registration_validates_and_any_origin_mutates() {
    let mut m = module();
    let mut ctx = mk_ctx(0, owner());
    exec(
        &mut m,
        &mut ctx,
        &register(OutputContract::Text, Routing::Rendezvous),
    )
    .unwrap();
    // duplicate id is an error.
    let err = exec(
        &mut m,
        &mut ctx,
        &register(OutputContract::Text, Routing::Rendezvous),
    )
    .unwrap_err();
    assert!(err.to_string().contains("already exists"), "got {err}");

    // shape rules: bad tag, empty pinned key, zero attempts.
    for (bad, needle) in [
        (
            DispatchMsg::RegisterRecipe {
                recipe_id: "r2".into(),
                description: String::new(),
                capability: "NOT A TAG".into(),
                routing: Routing::Rendezvous,
                output_contract: OutputContract::Text,
                max_attempts: 1,
                deadline_views: None,
                lease_views: None,
            },
            "invalid characters",
        ),
        (
            DispatchMsg::RegisterRecipe {
                recipe_id: "r2".into(),
                description: String::new(),
                capability: "alpha".into(),
                routing: Routing::Pinned(Vec::new()),
                output_contract: OutputContract::Text,
                max_attempts: 1,
                deadline_views: None,
                lease_views: None,
            },
            "Pinned key",
        ),
        (
            DispatchMsg::RegisterRecipe {
                recipe_id: "r2".into(),
                description: String::new(),
                capability: "alpha".into(),
                routing: Routing::Rendezvous,
                output_contract: OutputContract::Text,
                max_attempts: 0,
                deadline_views: None,
                lease_views: None,
            },
            "max_attempts",
        ),
    ] {
        let err = exec(&mut m, &mut ctx, &bad).unwrap_err();
        assert!(err.to_string().contains(needle), "wanted {needle} in {err}");
    }

    // a foreign origin's update lands: the owner is attribution, not consent,
    // and stays the registering origin.
    let mut foreign = mk_ctx(0, Origin::External(b"other".to_vec()));
    exec(
        &mut m,
        &mut foreign,
        &DispatchMsg::UpdateRecipe {
            recipe_id: "summarize".into(),
            description: Some("edited by a stranger".into()),
            capability: None,
            routing: None,
            output_contract: None,
            max_attempts: None,
        },
    )
    .unwrap();
    commit(&mut m);
    let edited = recipe(&m, "summarize").expect("recipe committed");
    assert_eq!(edited.description, "edited by a stranger");
    assert_eq!(edited.owner, SagaOrigin::External(b"owner".to_vec()));

    // an update is validated the same as a registration.
    let err = exec(
        &mut m,
        &mut ctx,
        &DispatchMsg::UpdateRecipe {
            recipe_id: "summarize".into(),
            description: None,
            capability: Some("ALSO NOT A TAG".into()),
            routing: None,
            output_contract: None,
            max_attempts: None,
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("invalid characters"), "got {err}");
    exec(
        &mut m,
        &mut ctx,
        &DispatchMsg::UpdateRecipe {
            recipe_id: "summarize".into(),
            description: None,
            capability: Some("beta".into()),
            routing: None,
            output_contract: None,
            max_attempts: Some(3),
        },
    )
    .unwrap();
    commit(&mut m);
    let committed = recipe(&m, "summarize").expect("recipe committed");
    assert_eq!(committed.capability, "beta");
    assert_eq!(committed.max_attempts, 3);
}

#[test]
fn agent_namespace_is_reserved_for_the_runs_module_origin() {
    let mut m = module();

    // an External account can never claim an `agent/` recipe id — squatting
    // it would block the model's ConfigureModel recipe registration.
    let mut mallory = mk_ctx(0, Origin::External(b"mallory".to_vec()));
    let err = exec(
        &mut m,
        &mut mallory,
        &DispatchMsg::RegisterRecipe {
            recipe_id: "agent/bot".into(),
            description: String::new(),
            capability: "alpha".into(),
            routing: Routing::Rendezvous,
            output_contract: OutputContract::Text,
            max_attempts: 1,
            deadline_views: None,
            lease_views: None,
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("reserved"), "got {err}");

    // the runs module's own module-origin registration succeeds — this is
    // the RegisterRecipe emitted by ConfigureModel.
    let mut runs = mk_ctx(0, Origin::Module("runs".into()));
    exec(
        &mut m,
        &mut runs,
        &DispatchMsg::RegisterRecipe {
            recipe_id: "agent/bot".into(),
            description: "runs for agent bot".into(),
            capability: "alpha".into(),
            routing: Routing::Rendezvous,
            output_contract: OutputContract::Text,
            max_attempts: 1,
            deadline_views: None,
            lease_views: None,
        },
    )
    .unwrap();
    commit(&mut m);
    assert!(recipe(&m, "agent/bot").is_some());

    // the reservation is id namespacing at registration only: once
    // registered, a foreign module removes the recipe like any other.
    let mut other = mk_ctx(0, Origin::Module("other".into()));
    exec(
        &mut m,
        &mut other,
        &DispatchMsg::RemoveRecipe {
            recipe_id: "agent/bot".into(),
        },
    )
    .unwrap();
    commit(&mut m);
    assert!(recipe(&m, "agent/bot").is_none());
}

#[test]
fn a_program_origin_reaches_the_admin_surface_and_is_refused_where_it_must_be() {
    // a program account acts only through calls its executor queued: it can
    // register no recipe, dispatch nothing, queue nothing — and a Nudge from
    // it is the same no-op it is from anyone.
    let mut m = module();
    let mut ctx = mk_ctx(0, Origin::Program(PROGRAM));
    let err = exec(
        &mut m,
        &mut ctx,
        &register(OutputContract::Text, Routing::Rendezvous),
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("cannot register recipes"),
        "got {err}"
    );
    let err = exec(&mut m, &mut ctx, &dispatch_op("d1", b"x")).unwrap_err();
    assert!(err.to_string().contains("module-origin only"), "got {err}");
    let err = exec(&mut m, &mut ctx, &call_op("run-1", 0, "chat", b"x")).unwrap_err();
    assert!(err.to_string().contains("module-origin only"), "got {err}");
    exec(&mut m, &mut ctx, &DispatchMsg::Nudge {}).unwrap();
    assert!(m.staged.is_empty(), "a nudge stages nothing");
}

#[test]
fn removing_every_recipe_returns_the_root_to_its_never_registered_value() {
    // a recipe's whole footprint is its own `r/{id}` record, so removing every
    // recipe must leave the plane hashing exactly like one that never
    // registered any — no residue key surviving the removals.
    let mut m = module();
    let empty_root = m.root();
    let mut ctx = mk_ctx(0, owner());
    for recipe_id in ["zeta", "alpha"] {
        exec(
            &mut m,
            &mut ctx,
            &DispatchMsg::RegisterRecipe {
                recipe_id: recipe_id.into(),
                description: String::new(),
                capability: "alpha".into(),
                routing: Routing::Rendezvous,
                output_contract: OutputContract::Text,
                max_attempts: 1,
                deadline_views: None,
                lease_views: None,
            },
        )
        .unwrap();
    }
    commit(&mut m);
    assert!(recipe(&m, "alpha").is_some());
    assert!(recipe(&m, "zeta").is_some());

    for recipe_id in ["alpha", "zeta"] {
        exec(
            &mut m,
            &mut ctx,
            &DispatchMsg::RemoveRecipe {
                recipe_id: recipe_id.into(),
            },
        )
        .unwrap();
    }
    commit(&mut m);
    assert!(recipe(&m, "alpha").is_none());
    assert!(recipe(&m, "zeta").is_none());
    assert_eq!(
        m.root(),
        empty_root,
        "a removed recipe must leave no residue key behind"
    );
}

#[test]
fn a_routing_pin_over_saga_s_assignee_cap_is_refused_at_registration() {
    // saga refuses a `pinned_assignee` over MAX_ASSIGNEE_BYTES at trigger
    // time, so a recipe admitted with a bigger pin would register fine and
    // then fail EVERY dispatch under it. registration is where the pin is
    // admitted, so registration is where it is capped.
    let mut m = module();
    let mut ctx = mk_ctx(0, owner());
    let err = exec(
        &mut m,
        &mut ctx,
        &DispatchMsg::RegisterRecipe {
            recipe_id: "huge".into(),
            description: String::new(),
            capability: "alpha".into(),
            routing: Routing::Pinned(vec![7u8; 300]),
            output_contract: OutputContract::Text,
            max_attempts: 1,
            deadline_views: None,
            lease_views: None,
        },
    )
    .unwrap_err();
    assert!(
        err.to_string()
            .contains(&format!("the cap is {MAX_ASSIGNEE_BYTES}")),
        "the refusal must name the cap; got {err}"
    );
    // and the refusal left NOTHING staged.
    commit(&mut m);
    assert!(recipe(&m, "huge").is_none());
}

#[test]
fn a_pin_at_the_cap_registers_and_its_trigger_carries_it() {
    // the boundary the other side of the refusal: a pin exactly at the cap is
    // admitted, and the dispatch it drives emits a trigger saga's own gate
    // accepts — the two caps are the same number, so this can never be
    // register-ok-then-dispatch-refused.
    let mut m = module();
    let pin = vec![7u8; MAX_ASSIGNEE_BYTES];
    let mut ctx = mk_ctx(0, owner());
    exec(
        &mut m,
        &mut ctx,
        &register(OutputContract::Text, Routing::Pinned(pin.clone())),
    )
    .unwrap();
    commit(&mut m);

    let mut caller = mk_ctx(5, Origin::Module("caller".into()));
    exec(&mut m, &mut caller, &dispatch_op("d1", b"input")).unwrap();
    assert_eq!(caller.msgs().len(), 1);
    let SagaMsg::Trigger {
        pinned_assignee, ..
    } = saga_decode_msg(&caller.msgs()[0].payload).unwrap()
    else {
        panic!("expected a trigger");
    };
    assert_eq!(pinned_assignee, Some(pin));
}

#[test]
fn dispatch_stages_trigger_with_recipe_routing_and_dedups_per_receiver() {
    let mut m = module();
    let mut ctx = mk_ctx(0, owner());
    exec(
        &mut m,
        &mut ctx,
        &register(OutputContract::Json, Routing::Pinned(vec![7u8; 32])),
    )
    .unwrap();
    commit(&mut m);

    // external dispatch is refused.
    let mut external = mk_ctx(0, owner());
    let err = exec(&mut m, &mut external, &dispatch_op("d1", b"x")).unwrap_err();
    assert!(err.to_string().contains("module-origin only"), "got {err}");

    // an unknown recipe is an error; an oversized payload is an error.
    let mut caller = mk_ctx(5, Origin::Module("caller".into()));
    let err = exec(
        &mut m,
        &mut caller,
        &DispatchMsg::Dispatch {
            dispatch_id: "d1".into(),
            recipe_id: "nope".into(),
            payload: b"x".to_vec(),
            demands: BTreeMap::new(),
            admission: AdmissionPolicy::Queue,
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("unknown recipe"), "got {err}");
    let err = exec(
        &mut m,
        &mut caller,
        &dispatch_op("d1", &vec![0u8; MAX_PAYLOAD_BYTES + 1]),
    )
    .unwrap_err();
    assert!(err.to_string().contains("payload is"), "got {err}");

    // the real dispatch stages exactly one trigger carrying the recipe's
    // capability, routing pin, deadline, and the work-spec payload.
    exec(&mut m, &mut caller, &dispatch_op("d1", b"input")).unwrap();
    assert_eq!(caller.msgs().len(), 1);
    assert_eq!(caller.msgs()[0].target, "saga");
    let SagaMsg::Trigger {
        saga_id,
        spec,
        reply_to,
        reply_payload,
        deadline,
        max_attempts,
        capability,
        pinned_assignee,
        ..
    } = saga_decode_msg(&caller.msgs()[0].payload).unwrap()
    else {
        panic!("expected a trigger");
    };
    let key = dispatch_key("caller", "d1");
    assert_eq!(saga_id, saga_id_for(&key));
    assert_eq!(reply_to.as_deref(), Some("dispatch"));
    assert_eq!(reply_payload, key.clone().into_bytes());
    assert_eq!(deadline, Some(5 + 100));
    assert_eq!(max_attempts, 2);
    assert_eq!(capability.as_deref(), Some("alpha"));
    assert_eq!(pinned_assignee, Some(vec![7u8; 32]));
    let work = crate::decode_work_spec(&spec).unwrap();
    assert_eq!(work.dispatch_id, "d1");
    assert_eq!(work.capability, "alpha");
    assert_eq!(work.payload, b"input");

    // a duplicate under the same receiver is a deterministic no-op...
    exec(&mut m, &mut caller, &dispatch_op("d1", b"other")).unwrap();
    assert_eq!(caller.msgs().len(), 1, "no second trigger");
    // ...but another receiver's identical id is a distinct dispatch.
    let mut other = mk_ctx(5, Origin::Module("other".into()));
    exec(&mut m, &mut other, &dispatch_op("d1", b"input")).unwrap();
    assert_eq!(other.msgs().len(), 1);
    commit(&mut m);
    assert!(get_dispatch(&m, &key).is_some());
    assert!(get_dispatch(&m, &dispatch_key("other", "d1")).is_some());
}

#[test]
fn dispatch_demands_reach_both_the_trigger_and_the_work_spec() {
    let mut m = module();
    let mut ctx = mk_ctx(0, owner());
    exec(
        &mut m,
        &mut ctx,
        &register(OutputContract::Text, Routing::Rendezvous),
    )
    .unwrap();
    commit(&mut m);

    let demands = BTreeMap::from([("cores".to_string(), 4u64)]);
    let mut caller = mk_ctx(5, Origin::Module("caller".into()));
    exec(
        &mut m,
        &mut caller,
        &DispatchMsg::Dispatch {
            dispatch_id: "d1".into(),
            recipe_id: "summarize".into(),
            payload: b"input".to_vec(),
            demands: demands.clone(),
            admission: AdmissionPolicy::FailFast,
        },
    )
    .unwrap();
    assert_eq!(caller.msgs().len(), 1);
    let SagaMsg::Trigger {
        spec,
        demands: trigger_demands,
        ..
    } = saga_decode_msg(&caller.msgs()[0].payload).unwrap()
    else {
        panic!("expected a trigger");
    };
    // ONE source: the trigger's demands and the work spec's demands agree
    // exactly with what was dispatched — no drift possible.
    assert_eq!(trigger_demands, demands);
    let work = crate::decode_work_spec(&spec).unwrap();
    assert_eq!(work.demands, demands);
    assert_eq!(work.admission, AdmissionPolicy::FailFast);
}

#[test]
fn work_spec_without_demands_field_is_rejected() {
    // FLAG DAY: demands is required — a spec that omits the key fails to
    // decode rather than silently defaulting to a demandless job.
    let no_demands =
        br#"{"kind":"dispatch-work-v1","dispatch_id":"d","capability":"c","payload":[]}"#;
    assert!(crate::decode_work_spec(no_demands).is_err());
}

#[test]
fn callbacks_judge_contracts_and_enqueue_deliveries() {
    // Json contract, JSON result: Ok flows to the mailbox.
    let mut m = module();
    let key = registered_and_dispatched(&mut m, OutputContract::Json);
    callback_for(
        &mut m,
        9,
        &key,
        SagaOutcome::Done(br#"{"ok":true}"#.to_vec()),
    )
    .unwrap();
    commit(&mut m);
    let view = get_dispatch(&m, &key).unwrap();
    assert_eq!(view.status, DispatchStatus::AwaitingDelivery);
    assert_eq!(view.outcome, Some(Ok(br#"{"ok":true}"#.to_vec())));
    assert_eq!(pending_deliveries(&m), 1);

    // Json contract, non-JSON result: the VIOLATION is the outcome.
    let mut m = module();
    let key = registered_and_dispatched(&mut m, OutputContract::Json);
    callback_for(&mut m, 9, &key, SagaOutcome::Done(b"not json".to_vec())).unwrap();
    commit(&mut m);
    let view = get_dispatch(&m, &key).unwrap();
    match view.outcome {
        Some(Err(e)) => assert!(e.contains("output contract violation"), "got {e}"),
        other => panic!("expected a contract violation, got {other:?}"),
    }
    assert_eq!(pending_deliveries(&m), 1, "violations are delivered too");

    // saga failure maps to the Err outcome verbatim.
    let mut m = module();
    let key = registered_and_dispatched(&mut m, OutputContract::Text);
    callback_for(&mut m, 9, &key, SagaOutcome::Failed("provider died".into())).unwrap();
    commit(&mut m);
    assert_eq!(
        get_dispatch(&m, &key).unwrap().outcome,
        Some(Err("provider died".into()))
    );

    // correlation gates: unknown key and stale saga id are no-ops.
    let mut m = module();
    let key = registered_and_dispatched(&mut m, OutputContract::Text);
    let before = m.root();
    callback_for(
        &mut m,
        9,
        "caller\x1fnope",
        SagaOutcome::Done(b"x".to_vec()),
    )
    .unwrap();
    let mut ctx = mk_ctx(0, Origin::Module("saga".into()));
    let msg = Msg {
        target: "dispatch".into(),
        payload: encode_callback(&SagaCallback {
            saga_id: "some-other-saga".into(),
            payload: key.as_bytes().to_vec(),
            outcome: SagaOutcome::Done(b"x".to_vec()),
        }),
    };
    block_on(m.execute(&mut ctx, &msg)).unwrap();
    commit(&mut m);
    assert_eq!(m.root(), before, "mismatched callbacks stage nothing");
}

#[test]
fn work_result_retains_the_admitted_chain_across_external_work() {
    let mut m = module();
    let mut ctx = mk_ctx(0, owner());
    exec(
        &mut m,
        &mut ctx,
        &register(OutputContract::Text, Routing::Rendezvous),
    )
    .unwrap();
    let root = Root::Change {
        source: "attribution".into(),
        seq: 17,
    };
    let cause = Cause::Chain {
        root: root.clone(),
        hop: Hop::Delivery(ItemRef {
            source: "attribution".into(),
            item: 29,
        }),
    };
    let mut caller = TestCtx::with_env(env(5, Origin::Module("caller".into()), cause.clone()));
    exec(&mut m, &mut caller, &dispatch_op("d1", b"input")).unwrap();
    commit(&mut m);
    let key = dispatch_key("caller", "d1");
    assert_eq!(get_dispatch(&m, &key).unwrap().cause, cause);
    // The callback arrives under Direct; its authority is saga's, while the
    // result's causal root still belongs to the original requesting chain.
    callback_for(&mut m, 9, &key, SagaOutcome::Done(b"answer".to_vec())).unwrap();
    commit(&mut m);
    let items = block_on(m.pending_items()).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].cause,
        Cause::Chain {
            root,
            hop: Hop::Delivery(ItemRef {
                source: "dispatch".into(),
                item: items[0].item
            }),
        }
    );
}

#[test]
fn pending_items_walks_the_committed_mailbox_head_in_order_and_bounded() {
    let mut m = module();
    let mut ctx = mk_ctx(0, owner());
    exec(
        &mut m,
        &mut ctx,
        &register(OutputContract::Text, Routing::Rendezvous),
    )
    .unwrap();
    // enqueue MAX + 1 outcomes across two receivers.
    for i in 0..=MAX_DELIVERIES_PER_BLOCK {
        let receiver = if i % 2 == 0 { "even" } else { "odd" };
        let mut caller = mk_ctx(5, Origin::Module(receiver.into()));
        exec(
            &mut m,
            &mut caller,
            &dispatch_op(&format!("d{i:03}"), b"in"),
        )
        .unwrap();
    }
    commit(&mut m);
    for i in 0..=MAX_DELIVERIES_PER_BLOCK {
        let receiver = if i % 2 == 0 { "even" } else { "odd" };
        let key = dispatch_key(receiver, &format!("d{i:03}"));
        callback_for(
            &mut m,
            9,
            &key,
            SagaOutcome::Done(format!("r{i}").into_bytes()),
        )
        .unwrap();
    }
    // COMMITTED only: nothing staged is a pending item yet.
    assert!(pending_items(&m).unwrap().is_empty());
    commit(&mut m);
    assert_eq!(
        pending_deliveries(&m) as usize,
        MAX_DELIVERIES_PER_BLOCK + 1
    );

    // the head batch is FIFO and bounded per block; reading it moves nothing.
    let items = pending_items(&m).unwrap();
    assert_eq!(items.len(), MAX_DELIVERIES_PER_BLOCK);
    assert_eq!(pending_items(&m).unwrap(), items, "a read is a read");
    let numbers: Vec<u64> = items.iter().map(|item| item.item).collect();
    assert_eq!(
        numbers,
        (0..MAX_DELIVERIES_PER_BLOCK as u64).collect::<Vec<_>>()
    );
    let Delivery::Result(first) = delivered(&items[0].payload) else {
        panic!("expected a Result delivery");
    };
    assert_eq!(first.dispatch_id, "d000");
    assert_eq!(first.outcome, Ok(b"r0".to_vec()));
    assert_eq!(items[0].target, "even");
    assert_eq!(items[1].target, "odd");
    let item_ref = ItemRef {
        source: "dispatch".into(),
        item: 0,
    };
    assert_eq!(
        items[0].cause,
        Cause::Chain {
            root: Root::Item(item_ref.clone()),
            hop: Hop::Delivery(item_ref),
        }
    );

    // the host acknowledges the batch; the remainder stays pending.
    for item in &items {
        ack(&mut m, 9, item.item, &item.target, DeliveryOutcome::Applied).unwrap();
    }
    commit(&mut m);
    assert_eq!(pending_deliveries(&m), 1, "the remainder stays pending");
    assert_eq!(
        get_dispatch(&m, &dispatch_key("even", "d000"))
            .unwrap()
            .status,
        DispatchStatus::Delivered {
            delivery: DeliveryOutcome::Applied
        }
    );

    // the next boundary drains the remainder, under its own number.
    let rest = pending_items(&m).unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].item, MAX_DELIVERIES_PER_BLOCK as u64);
    ack(
        &mut m,
        10,
        rest[0].item,
        &rest[0].target,
        DeliveryOutcome::Applied,
    )
    .unwrap();
    commit(&mut m);
    assert_eq!(pending_deliveries(&m), 0);
}

#[test]
fn admission_policy_does_not_change_committed_state_or_root() {
    fn dispatched(admission: AdmissionPolicy) -> DispatchModule {
        let mut m = module();
        let mut owner_ctx = mk_ctx(0, owner());
        exec(
            &mut m,
            &mut owner_ctx,
            &register(OutputContract::Json, Routing::Rendezvous),
        )
        .unwrap();
        commit(&mut m);
        let mut caller = mk_ctx(5, Origin::Module("caller".into()));
        exec(
            &mut m,
            &mut caller,
            &DispatchMsg::Dispatch {
                dispatch_id: "d1".into(),
                recipe_id: "summarize".into(),
                payload: b"input".to_vec(),
                demands: BTreeMap::new(),
                admission,
            },
        )
        .unwrap();
        commit(&mut m);
        m
    }

    let queue = dispatched(AdmissionPolicy::Queue);
    let fail_fast = dispatched(AdmissionPolicy::FailFast);
    // admission is host-local: it rides the work spec, never a record.
    assert_eq!(queue.root(), fail_fast.root());
}

#[test]
fn abort_discards_every_staged_write() {
    let mut m = module();
    let before = m.root();
    let mut ctx = mk_ctx(0, owner());
    exec(
        &mut m,
        &mut ctx,
        &register(OutputContract::Text, Routing::Rendezvous),
    )
    .unwrap();
    let mut caller = mk_ctx(0, Origin::Module("caller".into()));
    exec(&mut m, &mut caller, &dispatch_op("d1", b"in")).unwrap();
    block_on(m.abort_block()).unwrap();
    assert_eq!(m.root(), before, "aborted block leaves no trace");
    assert_eq!(pending_deliveries(&m), 0);
}

#[test]
fn cancel_is_receiver_scoped_and_idempotent() {
    let mut m = module();
    let key = registered_and_dispatched(&mut m, OutputContract::Text);
    let cancel = DispatchMsg::CancelDispatch {
        dispatch_id: "d1".into(),
    };

    // an external submitter has no cancel surface.
    let mut ctx = mk_ctx(0, owner());
    assert!(exec(&mut m, &mut ctx, &cancel).is_err());

    // a foreign module's cancel lands in its OWN receiver namespace —
    // an unknown key, a deterministic no-op.
    let mut ctx = mk_ctx(0, Origin::Module("other".into()));
    exec(&mut m, &mut ctx, &cancel).unwrap();
    assert!(ctx.msgs().is_empty());

    // the receiver's cancel tells exactly the dispatch's own saga.
    let mut ctx = mk_ctx(0, Origin::Module("caller".into()));
    exec(&mut m, &mut ctx, &cancel).unwrap();
    assert_eq!(ctx.msgs().len(), 1);
    assert_eq!(ctx.msgs()[0].target, "saga");
    match saga_decode_msg(&ctx.msgs()[0].payload).unwrap() {
        SagaMsg::Cancel { saga_id } => assert_eq!(saga_id, saga_id_for(&key)),
        other => panic!("expected a saga Cancel, got {other:?}"),
    }

    // once the Cancelled callback lands, the result flows the NORMAL
    // path (Err in the mailbox) and a repeat cancel is a no-op.
    callback_for(&mut m, 9, &key, SagaOutcome::Cancelled).unwrap();
    commit(&mut m);
    assert_eq!(pending_deliveries(&m), 1);
    let mut ctx = mk_ctx(0, Origin::Module("caller".into()));
    exec(&mut m, &mut ctx, &cancel).unwrap();
    assert!(ctx.msgs().is_empty());
}

#[test]
fn an_undecodable_saga_callback_is_swallowed_not_an_abort() {
    // the callback-poison rule: the callback intake runs inside a
    // finalized block, so a payload that fails to decode must be a
    // staged no-op plus a diagnostic event — an Err would abort the
    // block, which replays as a no-op and re-aborts forever.
    let mut m = module();
    let key = registered_and_dispatched(&mut m, OutputContract::Text);
    let before = m.root();

    let mut ctx = mk_ctx(0, Origin::Module("saga".into()));
    let msg = Msg {
        target: "dispatch".into(),
        payload: b"not a callback".to_vec(),
    };
    block_on(m.execute(&mut ctx, &msg)).expect("the poisoned callback must not abort");
    assert_eq!(ctx.events().len(), 1, "the swallow left a diagnostic event");
    assert!(
        String::from_utf8_lossy(&ctx.events()[0].payload).contains("undecodable"),
        "the event names the drop"
    );

    // the block applies and stages nothing: the dispatch still awaits its
    // result and the root is unmoved.
    commit(&mut m);
    assert_eq!(m.root(), before, "nothing was staged");
    assert!(matches!(
        get_dispatch(&m, &key).unwrap().status,
        DispatchStatus::AwaitingResult { .. }
    ));
}

#[test]
fn a_corrupt_mailbox_entry_fails_the_between_block_read_closed() {
    // the host refuses to run the block on a corrupt queue rather than skip
    // work: a garbage entry, and an entry pointing at a record that does not
    // exist, are both errors from `pending_items` — and the head cannot be
    // acknowledged past them either.
    let mut m = module();
    // neither shape can be built through the module's own transitions; plant
    // the records directly (same crate, test-only).
    m.staged.stage(mailbox_key(0), b"garbage".to_vec());
    stage_mailbox(&mut m.staged, Mailbox { head: 0, next: 1 });
    commit(&mut m);
    assert_eq!(pending_deliveries(&m), 1);
    let err = pending_items(&m).unwrap_err();
    assert!(
        err.to_string().contains("mailbox entry decode"),
        "got {err}"
    );
    let err = ack(&mut m, 0, 0, "anyone", DeliveryOutcome::Applied).unwrap_err();
    assert!(
        err.to_string().contains("mailbox entry decode"),
        "got {err}"
    );

    m.staged.stage(
        mailbox_key(0),
        encode_mail_entry(&MailEntry::Call { enqueued: 99 }),
    );
    commit(&mut m);
    let err = pending_items(&m).unwrap_err();
    assert!(err.to_string().contains("has no record"), "got {err}");
    let err = ack(&mut m, 0, 0, RUNS, DeliveryOutcome::Applied).unwrap_err();
    assert!(err.to_string().contains("has no record"), "got {err}");
    assert!(
        m.staged.is_empty(),
        "a refused acknowledgment stages nothing"
    );
}

#[test]
fn reassign_is_receiver_scoped_and_carries_the_expected_attempt() {
    let mut m = module();
    let key = registered_and_dispatched(&mut m, OutputContract::Text);
    let reassign = DispatchMsg::ReassignDispatch {
        dispatch_id: "d1".into(),
        attempt: 2,
    };
    let mut ctx = mk_ctx(0, Origin::Module("caller".into()));
    exec(&mut m, &mut ctx, &reassign).unwrap();
    assert_eq!(ctx.msgs().len(), 1);
    assert!(matches!(
        saga_decode_msg(&ctx.msgs()[0].payload).unwrap(),
        SagaMsg::Reassign { saga_id, attempt: 2 } if saga_id == saga_id_for(&key)
    ));
}

// ---- retention ---------------------------------------------------------

/// drive one dispatch all the way from `Dispatch` to `Delivered`, one
/// block per transition, with an `outcome_bytes`-sized result.
fn full_round(m: &mut DispatchModule, i: usize, outcome_bytes: usize) {
    let height = (i as u64 + 1) * 4;
    let dispatch_id = format!("d{i:04}");
    let key = dispatch_key("caller", &dispatch_id);

    let mut ctx = mk_ctx(height, Origin::Module("caller".into()));
    exec(m, &mut ctx, &dispatch_op(&dispatch_id, b"input")).unwrap();
    commit(m);

    callback_for(
        m,
        height + 1,
        &key,
        SagaOutcome::Done(vec![b'x'; outcome_bytes]),
    )
    .unwrap();
    commit(m);

    let items = drain_applied(m, height + 2);
    assert_eq!(items.len(), 1);
}

#[test]
fn delivery_hands_the_outcome_over_and_drops_this_module_s_copy() {
    let mut m = module();
    let mut ctx = mk_ctx(0, owner());
    exec(
        &mut m,
        &mut ctx,
        &register(OutputContract::Text, Routing::Rendezvous),
    )
    .unwrap();
    commit(&mut m);

    // the pre-delivery record still carries the bytes — the delivery is
    // what needs them.
    let key = dispatch_key("caller", "d0000");
    let mut ctx = mk_ctx(4, Origin::Module("caller".into()));
    exec(&mut m, &mut ctx, &dispatch_op("d0000", b"input")).unwrap();
    commit(&mut m);
    callback_for(&mut m, 9, &key, SagaOutcome::Done(b"result".to_vec())).unwrap();
    commit(&mut m);
    assert_eq!(
        get_dispatch(&m, &key).unwrap().outcome,
        Some(Ok(b"result".to_vec()))
    );

    let items = drain_applied(&mut m, 6);

    // the receiver got every byte...
    let Delivery::Result(event) = delivered(&items[0].payload) else {
        panic!("expected a Result delivery");
    };
    assert_eq!(event.outcome, Ok(b"result".to_vec()));
    // ...and the record kept none of them, while STAYING a record.
    let view = get_dispatch(&m, &key).expect("the delivered record survives");
    assert_eq!(
        view.status,
        DispatchStatus::Delivered {
            delivery: DeliveryOutcome::Applied
        }
    );
    assert_eq!(view.outcome, None, "the delivered copy is dropped");
}

#[test]
fn sustained_dispatch_traffic_keeps_the_state_bounded_but_never_forgets_a_run() {
    // the growth pin. every dispatch record is `runs`' PERMANENT turn
    // claim (runs::dispatch_flow::turn_taken) — evicting one re-opens a
    // settled run for a duplicate agent launch — so the record count grows 1:1
    // with traffic ON PURPOSE, and per-record keys make that free: an op
    // touches only the keys it names. what must NOT grow is the RECORD: the
    // outcome is up to MAX_RESULT_BYTES and delivery drops this module's copy.
    let mut m = module();
    let mut ctx = mk_ctx(0, owner());
    exec(
        &mut m,
        &mut ctx,
        &register(OutputContract::Text, Routing::Rendezvous),
    )
    .unwrap();
    commit(&mut m);

    const ROUNDS: usize = 64;
    const OUTCOME_BYTES: usize = 64 * 1024;
    for i in 0..ROUNDS {
        full_round(&mut m, i, OUTCOME_BYTES);
    }
    assert_eq!(pending_deliveries(&m), 0, "every result was delivered");

    // ROUNDS * 64 KiB of results passed through; every receipt survives and
    // each is a FIXED-SIZE record, not a second ledger of the bytes.
    let mut total = 0;
    for i in 0..ROUNDS {
        let key = dispatch_key("caller", &format!("d{i:04}"));
        let view = get_dispatch(&m, &key).expect("no receipt is ever evicted");
        assert_eq!(
            view.status,
            DispatchStatus::Delivered {
                delivery: DeliveryOutcome::Applied
            }
        );
        assert_eq!(view.outcome, None);
        total += record_bytes(&m, &key);
    }
    let ceiling = ROUNDS * 1024;
    assert!(
        total < ceiling,
        "the {ROUNDS} delivered records hold {total} bytes (ceiling {ceiling}); \
         delivered outcomes are being retained"
    );

    // the recipe is untouched.
    assert!(recipe(&m, "summarize").is_some());
}

// ---- the call queue ----------------------------------------------------

#[test]
fn a_call_is_admitted_with_its_claim_record_and_cursor_and_visible_once_committed() {
    let mut m = module();
    let mut ctx = requester_ctx(5);
    exec(&mut m, &mut ctx, &call_op("run-1", 0, "chat", b"hello")).unwrap();
    assert!(
        ctx.msgs().is_empty(),
        "a call emits nothing: the host runs it"
    );
    // COMMITTED only: the host's reads see nothing staged.
    assert!(pending_calls(&m).is_empty());
    assert_eq!(call_view(&m, &call_id("run-1", 0)), None);
    commit(&mut m);

    let batch = pending_calls(&m);
    assert_eq!(batch.len(), 1);
    let id = call_id("run-1", 0);
    assert_eq!(
        batch[0],
        PendingCall {
            enqueued: 0,
            id: id.clone(),
            account: PROGRAM,
            generation: 0,
            target: "chat".into(),
            payload: b"hello".to_vec(),
            // a Direct requester's call starts a chain of its own.
            cause: Cause::Chain {
                root: Root::Call(id.clone()),
                hop: Hop::Call(id.clone()),
            },
        }
    );
    assert_eq!(
        call_view(&m, &id),
        Some(CallView {
            enqueued: 0,
            id,
            account: PROGRAM,
            generation: 0,
            target: "chat".into(),
            payload_digest: sha2::Sha256::digest(b"hello").into(),
            cause: Cause::Direct,
            status: CallStatus::Queued,
        })
    );

    // the next call takes the next number; the batch stays in queue order.
    let mut ctx = requester_ctx(6);
    exec(&mut m, &mut ctx, &call_op("run-1", 1, "pages", b"again")).unwrap();
    commit(&mut m);
    let numbers: Vec<u64> = pending_calls(&m).iter().map(|c| c.enqueued).collect();
    assert_eq!(numbers, vec![0, 1]);
}

#[test]
fn a_call_under_a_chained_requester_inherits_the_chain_root() {
    // a requester running as a link of a chain (here: the delivery of mailbox
    // item 41) queues a call that stays in that chain — its execution root is
    // the chain's root, and the call itself is the hop.
    let root = Root::Item(ItemRef {
        source: "dispatch".into(),
        item: 41,
    });
    let requester_cause = Cause::Chain {
        root: root.clone(),
        hop: Hop::Delivery(ItemRef {
            source: "dispatch".into(),
            item: 41,
        }),
    };
    let mut m = module();
    let mut ctx = with_identity(
        TestCtx::with_env(env(5, Origin::Module(RUNS.into()), requester_cause)),
        vec![program(PROGRAM, RUNS, 0, ProgramStanding::Active)],
    );
    exec(&mut m, &mut ctx, &call_op("run-1", 0, "chat", b"x")).unwrap();
    commit(&mut m);
    assert_eq!(
        pending_calls(&m)[0].cause,
        Cause::Chain {
            root,
            hop: Hop::Call(call_id("run-1", 0)),
        }
    );
}

#[test]
fn a_call_is_refused_for_every_non_executable_account_and_malformed_request() {
    let mut m = module();
    let before = m.root();
    let op = call_op("run-1", 0, "chat", b"x");

    // only a module queues calls: an external key and a program account have
    // no call surface.
    let err = exec(&mut m, &mut mk_ctx(0, owner()), &op).unwrap_err();
    assert!(err.to_string().contains("module-origin only"), "got {err}");
    let err = exec(&mut m, &mut mk_ctx(0, Origin::Program(PROGRAM)), &op).unwrap_err();
    assert!(err.to_string().contains("module-origin only"), "got {err}");

    // the account's control record decides, one distinct refusal each.
    for (accounts, needle) in [
        (Vec::new(), "does not exist"),
        (vec![account(PROGRAM, Control::Keys)], "key-held"),
        (
            vec![program(PROGRAM, "other", 0, ProgramStanding::Active)],
            "executed by",
        ),
        (
            vec![program(PROGRAM, RUNS, 0, ProgramStanding::Suspended)],
            "suspended",
        ),
        (
            vec![account(PROGRAM, Control::Revoked { controller: 1 })],
            "revoked",
        ),
    ] {
        let mut ctx = with_identity(mk_ctx(0, Origin::Module(RUNS.into())), accounts);
        let err = exec(&mut m, &mut ctx, &op).unwrap_err();
        assert!(err.to_string().contains(needle), "wanted {needle} in {err}");
    }

    // the id: the separator can never spell another claim's key; an empty
    // target has no unit to run in.
    let mut ctx = requester_ctx(0);
    let err = exec(&mut m, &mut ctx, &call_op("run\x1f1", 0, "chat", b"x")).unwrap_err();
    assert!(err.to_string().contains("reserved separator"), "got {err}");
    let err = exec(&mut m, &mut ctx, &call_op("run-1", 0, "", b"x")).unwrap_err();
    assert!(err.to_string().contains("target"), "got {err}");

    // the payload: over the wire cap, and — the admission rule — under the
    // wire cap but unable to share a record with a maximal outcome.
    let err = exec(
        &mut m,
        &mut ctx,
        &call_op("run-1", 0, "chat", &vec![0u8; MAX_PAYLOAD_BYTES + 1]),
    )
    .unwrap_err();
    assert!(err.to_string().contains("payload is"), "got {err}");
    let err = exec(
        &mut m,
        &mut ctx,
        &call_op(
            "run-1",
            0,
            "chat",
            &vec![0u8; MAX_RECORD_BYTES - sdk::MAX_OUTPUT_BYTES],
        ),
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("completed call record"),
        "the refusal names the record the finalizer would write; got {err}"
    );

    // every refusal staged nothing.
    assert!(m.staged.is_empty());
    commit(&mut m);
    assert_eq!(m.root(), before);
    assert!(pending_calls(&m).is_empty());

    // and a payload that leaves room for the maximal outcome is admitted.
    let roomy = MAX_RECORD_BYTES - sdk::MAX_OUTPUT_BYTES - sdk::MAX_ASSIGNED_BYTES - 1024;
    exec(
        &mut m,
        &mut ctx,
        &call_op("run-1", 0, "chat", &vec![0u8; roomy]),
    )
    .unwrap();
    commit(&mut m);
    assert_eq!(pending_calls(&m).len(), 1);
}

#[test]
fn an_exact_call_replay_is_a_no_op_and_any_drift_is_rejected() {
    let mut m = module();
    exec(
        &mut m,
        &mut requester_ctx(5),
        &call_op("run-1", 0, "chat", b"x"),
    )
    .unwrap();
    commit(&mut m);
    let before = m.root();

    // the exact replay applies and stages nothing.
    exec(
        &mut m,
        &mut requester_ctx(6),
        &call_op("run-1", 0, "chat", b"x"),
    )
    .unwrap();
    assert!(m.staged.is_empty(), "an exact replay stages nothing");
    commit(&mut m);
    assert_eq!(m.root(), before);

    // each drift is a rejection that names what changed.
    let other_program = program(8, RUNS, 0, ProgramStanding::Active);
    let moved_generation = program(PROGRAM, RUNS, 1, ProgramStanding::Active);
    let chained = Cause::Chain {
        root: Root::Call(call_id("elsewhere", 0)),
        hop: Hop::Call(call_id("elsewhere", 0)),
    };
    let drifts: Vec<(DispatchMsg, TestCtx, &str)> = vec![
        (
            call_op("run-1", 0, "chat", b"y"),
            requester_ctx(6),
            "different payload",
        ),
        (
            call_op("run-1", 0, "pages", b"x"),
            requester_ctx(6),
            "different target",
        ),
        (
            DispatchMsg::Call {
                invocation: "run-1".into(),
                step: 0,
                account: 8,
                target: "chat".into(),
                payload: b"x".to_vec(),
            },
            with_identity(mk_ctx(6, Origin::Module(RUNS.into())), vec![other_program]),
            "different account",
        ),
        (
            call_op("run-1", 0, "chat", b"x"),
            with_identity(
                TestCtx::with_env(env(6, Origin::Module(RUNS.into()), chained)),
                vec![program(PROGRAM, RUNS, 0, ProgramStanding::Active)],
            ),
            "different cause",
        ),
        (
            call_op("run-1", 0, "chat", b"x"),
            with_identity(
                mk_ctx(6, Origin::Module(RUNS.into())),
                vec![moved_generation],
            ),
            "different generation",
        ),
    ];
    for (op, mut ctx, needle) in drifts {
        let err = exec(&mut m, &mut ctx, &op).unwrap_err();
        assert!(err.to_string().contains(needle), "wanted {needle} in {err}");
        assert!(m.staged.is_empty(), "a rejected replay stages nothing");
    }

    // the record's lifecycle is not a fact of the call: after completion and
    // delivery the exact replay is still a no-op.
    complete(&mut m, 7, 0, call_id("run-1", 0), applied(b"out")).unwrap();
    commit(&mut m);
    drain_applied(&mut m, 8);
    let after_delivery = m.root();
    exec(
        &mut m,
        &mut requester_ctx(9),
        &call_op("run-1", 0, "chat", b"x"),
    )
    .unwrap();
    assert!(m.staged.is_empty());
    commit(&mut m);
    assert_eq!(m.root(), after_delivery);
}

#[test]
fn the_same_id_from_another_hop_of_the_same_chain_is_a_different_call() {
    // the admitting cause is compared WHOLE: two hops of one chain share a
    // root, and a call re-queued from the second hop is a rejected replay,
    // not the first call again.
    let root = Root::Item(ItemRef {
        source: "dispatch".into(),
        item: 3,
    });
    let hop = |item: u64| Cause::Chain {
        root: root.clone(),
        hop: Hop::Delivery(ItemRef {
            source: "dispatch".into(),
            item,
        }),
    };
    let ctx_at = |cause: Cause| {
        with_identity(
            TestCtx::with_env(env(5, Origin::Module(RUNS.into()), cause)),
            vec![program(PROGRAM, RUNS, 0, ProgramStanding::Active)],
        )
    };
    let mut m = module();
    exec(
        &mut m,
        &mut ctx_at(hop(3)),
        &call_op("run-1", 0, "chat", b"x"),
    )
    .unwrap();
    commit(&mut m);
    exec(
        &mut m,
        &mut ctx_at(hop(3)),
        &call_op("run-1", 0, "chat", b"x"),
    )
    .expect("the same hop is the exact replay");
    let err = exec(
        &mut m,
        &mut ctx_at(hop(4)),
        &call_op("run-1", 0, "chat", b"x"),
    )
    .unwrap_err();
    assert!(err.to_string().contains("different cause"), "got {err}");
}

#[test]
fn complete_call_is_system_only_in_order_and_every_outcome_lands_in_the_mailbox() {
    let outcomes = [
        applied(b"out"),
        CallOutcome::Rejected {
            reason: "the target said no".into(),
        },
        CallOutcome::Refused(Refusal::Suspended),
        CallOutcome::Unrepresentable {
            attempted: Attempt::Rejected,
        },
    ];
    let mut m = module();
    for step in 0..outcomes.len() as u64 {
        exec(
            &mut m,
            &mut requester_ctx(5),
            &call_op("run-1", step, "chat", b"x"),
        )
        .unwrap();
    }
    commit(&mut m);

    // only the host finalizes.
    let mut runs = mk_ctx(6, Origin::Module(RUNS.into()));
    let err = exec(
        &mut m,
        &mut runs,
        &DispatchMsg::CompleteCall {
            enqueued: 0,
            id: call_id("run-1", 0),
            outcome: applied(b"out"),
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("System-origin only"), "got {err}");
    // in order: the head first.
    let err = complete(&mut m, 6, 1, call_id("run-1", 1), applied(b"out")).unwrap_err();
    assert!(err.to_string().contains("out of order"), "got {err}");
    // the head's id must be the one named.
    let err = complete(&mut m, 6, 0, call_id("run-1", 3), applied(b"out")).unwrap_err();
    assert!(err.to_string().contains("not runs/run-1#3"), "got {err}");
    assert!(m.staged.is_empty(), "a refused completion stages nothing");

    // each outcome kind finalizes its call and lands in the mailbox, in
    // queue order, addressed to the requester, under the completion hop of
    // the call's chain.
    for (step, outcome) in outcomes.iter().enumerate() {
        complete(
            &mut m,
            6,
            step as u64,
            call_id("run-1", step as u64),
            outcome.clone(),
        )
        .unwrap();
    }
    commit(&mut m);
    assert!(pending_calls(&m).is_empty(), "every call left the queue");
    assert_eq!(pending_deliveries(&m) as usize, outcomes.len());
    let items = pending_items(&m).unwrap();
    assert_eq!(items.len(), outcomes.len());
    for (i, item) in items.iter().enumerate() {
        let id = call_id("run-1", i as u64);
        assert_eq!(item.item, i as u64);
        assert_eq!(item.target, RUNS);
        assert_eq!(
            delivered(&item.payload),
            Delivery::CallCompleted(CallCompleted {
                id: id.clone(),
                account: PROGRAM,
                outcome: outcomes[i].clone(),
            })
        );
        assert_eq!(
            item.cause,
            Cause::Chain {
                root: Root::Call(id.clone()),
                hop: Hop::Completion(id.clone()),
            }
        );
        assert_eq!(
            call_view(&m, &id).unwrap().status,
            CallStatus::Completed {
                outcome: outcomes[i].summary()
            }
        );
    }
}

#[test]
fn a_re_completion_is_a_no_op_with_the_same_outcome_and_rejected_with_another() {
    let mut m = module();
    exec(
        &mut m,
        &mut requester_ctx(5),
        &call_op("run-1", 0, "chat", b"x"),
    )
    .unwrap();
    commit(&mut m);
    complete(&mut m, 6, 0, call_id("run-1", 0), applied(b"out")).unwrap();
    commit(&mut m);
    let before = m.root();

    // the host re-running its finalization (recovery replay): a no-op.
    complete(&mut m, 7, 0, call_id("run-1", 0), applied(b"out")).unwrap();
    assert!(m.staged.is_empty(), "a re-completion stages nothing");
    commit(&mut m);
    assert_eq!(m.root(), before);
    // a different outcome for a finalized call is never waved through.
    let err = complete(
        &mut m,
        7,
        0,
        call_id("run-1", 0),
        CallOutcome::Rejected {
            reason: "changed my mind".into(),
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("different outcome"), "got {err}");
    let err = complete(&mut m, 7, 0, call_id("run-1", 0), applied(b"other")).unwrap_err();
    assert!(err.to_string().contains("different outcome"), "got {err}");
    // beyond the queue is out of order, not a replay.
    let err = complete(&mut m, 7, 5, call_id("run-1", 5), applied(b"out")).unwrap_err();
    assert!(err.to_string().contains("out of order"), "got {err}");

    // after delivery only the summary survives, and the check still holds.
    drain_applied(&mut m, 8);
    complete(&mut m, 9, 0, call_id("run-1", 0), applied(b"out")).unwrap();
    assert!(m.staged.is_empty());
    let err = complete(&mut m, 9, 0, call_id("run-1", 0), applied(b"other")).unwrap_err();
    assert!(err.to_string().contains("different outcome"), "got {err}");
}

// ---- the mailbox: acknowledgment ----------------------------------------

#[test]
fn acknowledge_retires_only_the_head_and_the_receipt_keeps_each_delivery_outcome() {
    let mut m = module();
    for step in 0..3 {
        exec(
            &mut m,
            &mut requester_ctx(5),
            &call_op("run-1", step, "chat", b"x"),
        )
        .unwrap();
    }
    commit(&mut m);
    // an output bigger than the digest that replaces it, so the receipt's
    // retention is measurable.
    let output = vec![b'x'; 4096];
    for step in 0..3 {
        complete(&mut m, 6, step, call_id("run-1", step), applied(&output)).unwrap();
    }
    commit(&mut m);
    let completed_bytes = block_on(m.staged.get_committed(&call_key(0)))
        .unwrap()
        .unwrap()
        .len();

    // above the head is a host bug; a misrouted target is a host bug; neither
    // retires anything.
    let err = ack(&mut m, 7, 1, RUNS, DeliveryOutcome::Applied).unwrap_err();
    assert!(err.to_string().contains("out of order"), "got {err}");
    let err = ack(&mut m, 7, 0, "chat", DeliveryOutcome::Applied).unwrap_err();
    assert!(err.to_string().contains("addressed to"), "got {err}");
    assert!(m.staged.is_empty());
    assert_eq!(pending_deliveries(&m), 3);

    // every delivery outcome retires the head; the non-applied ones leave a
    // breadcrumb beside the receipt.
    let ctx = ack(&mut m, 7, 0, RUNS, DeliveryOutcome::Applied).unwrap();
    assert!(ctx.events().is_empty());
    let ctx = ack(
        &mut m,
        7,
        1,
        RUNS,
        DeliveryOutcome::Failed {
            reason: "receiver rejected".into(),
        },
    )
    .unwrap();
    assert_eq!(ctx.events().len(), 1);
    assert!(String::from_utf8_lossy(&ctx.events()[0].payload).contains("receiver rejected"));
    let ctx = ack(&mut m, 7, 2, RUNS, DeliveryOutcome::Unrepresentable).unwrap();
    assert_eq!(ctx.events().len(), 1);
    commit(&mut m);
    assert_eq!(pending_deliveries(&m), 0);
    assert!(pending_items(&m).unwrap().is_empty());

    // the receipts are queryable with their real outcomes, minus the bulk.
    let summary = applied(&output).summary();
    for (step, delivery) in [
        (0, DeliveryOutcome::Applied),
        (
            1,
            DeliveryOutcome::Failed {
                reason: "receiver rejected".into(),
            },
        ),
        (2, DeliveryOutcome::Unrepresentable),
    ] {
        assert_eq!(
            call_view(&m, &call_id("run-1", step)).unwrap().status,
            CallStatus::Delivered {
                outcome: summary.clone(),
                delivery,
            }
        );
    }
    let delivered_bytes = block_on(m.staged.get_committed(&call_key(0)))
        .unwrap()
        .unwrap()
        .len();
    assert!(
        delivered_bytes < completed_bytes,
        "the receipt ({delivered_bytes} bytes) dropped the outcome bytes of the completed record ({completed_bytes} bytes)"
    );

    // below the head is a recovery replay: a no-op, whatever it names.
    let before = m.root();
    ack(&mut m, 8, 0, RUNS, DeliveryOutcome::Applied).unwrap();
    ack(&mut m, 8, 2, "anyone", DeliveryOutcome::Unrepresentable).unwrap();
    assert!(
        m.staged.is_empty(),
        "a replayed acknowledgment stages nothing"
    );
    commit(&mut m);
    assert_eq!(m.root(), before);
}

#[test]
fn a_failed_delivery_retires_its_item_and_the_next_item_still_delivers() {
    // a receiver that rejects a delivery is acknowledged Failed; the item is
    // retired with that outcome on its receipt and the next item delivers as
    // if nothing happened — a rejecting receiver never stalls the plane.
    let mut m = module();
    let mut ctx = mk_ctx(0, owner());
    exec(
        &mut m,
        &mut ctx,
        &register(OutputContract::Text, Routing::Rendezvous),
    )
    .unwrap();
    let mut caller = mk_ctx(5, Origin::Module("caller".into()));
    exec(&mut m, &mut caller, &dispatch_op("d1", b"in")).unwrap();
    exec(&mut m, &mut caller, &dispatch_op("d2", b"in")).unwrap();
    commit(&mut m);
    let (k1, k2) = (dispatch_key("caller", "d1"), dispatch_key("caller", "d2"));
    callback_for(&mut m, 6, &k1, SagaOutcome::Done(b"one".to_vec())).unwrap();
    callback_for(&mut m, 6, &k2, SagaOutcome::Done(b"two".to_vec())).unwrap();
    commit(&mut m);

    let items = pending_items(&m).unwrap();
    assert_eq!(items.len(), 2);
    let failed = DeliveryOutcome::Failed {
        reason: "receiver rejected: bad shape".into(),
    };
    ack(&mut m, 7, 0, "caller", failed.clone()).unwrap();
    commit(&mut m);
    let rest = pending_items(&m).unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].item, 1);
    ack(&mut m, 8, 1, "caller", DeliveryOutcome::Applied).unwrap();
    commit(&mut m);
    assert_eq!(pending_deliveries(&m), 0);

    let one = get_dispatch(&m, &k1).unwrap();
    assert_eq!(one.status, DispatchStatus::Delivered { delivery: failed });
    assert_eq!(one.outcome, None, "the outcome bytes went to the receiver");
    let two = get_dispatch(&m, &k2).unwrap();
    assert_eq!(
        two.status,
        DispatchStatus::Delivered {
            delivery: DeliveryOutcome::Applied
        }
    );
    assert_eq!(two.outcome, None);
}

#[test]
fn a_failed_reason_the_receipt_cannot_hold_is_refused_never_truncated() {
    // a Failed reason rides the receipt verbatim; one the record cap cannot
    // hold is an error — the host's designed fallback is to retry with the
    // fixed-size Unrepresentable marker, so a reason is never truncated and
    // never silently dropped.
    let mut m = module();
    let key = registered_and_dispatched(&mut m, OutputContract::Text);
    callback_for(&mut m, 6, &key, SagaOutcome::Done(b"small".to_vec())).unwrap();
    commit(&mut m);
    let err = ack(
        &mut m,
        7,
        0,
        "caller",
        DeliveryOutcome::Failed {
            reason: "x".repeat(MAX_RECORD_BYTES),
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("delivery receipt"), "got {err}");
    assert!(
        m.staged.is_empty(),
        "a refused acknowledgment stages nothing"
    );
    ack(&mut m, 7, 0, "caller", DeliveryOutcome::Unrepresentable).unwrap();
    commit(&mut m);
    assert_eq!(
        get_dispatch(&m, &key).unwrap().status,
        DispatchStatus::Delivered {
            delivery: DeliveryOutcome::Unrepresentable
        }
    );
}

#[test]
fn mailbox_item_numbers_stay_ascending_across_a_drain_and_mixed_entries() {
    // one numbering across every receiver and both entry kinds, never
    // restarting after a drain: a stale acknowledgment for an old number can
    // never retire a new item.
    let mut m = module();
    let mut ctx = mk_ctx(0, owner());
    exec(
        &mut m,
        &mut ctx,
        &register(OutputContract::Text, Routing::Rendezvous),
    )
    .unwrap();
    let mut caller = mk_ctx(5, Origin::Module("caller".into()));
    exec(&mut m, &mut caller, &dispatch_op("d1", b"in")).unwrap();
    exec(
        &mut m,
        &mut requester_ctx(5),
        &call_op("run-1", 0, "chat", b"x"),
    )
    .unwrap();
    commit(&mut m);
    let k1 = dispatch_key("caller", "d1");
    callback_for(&mut m, 6, &k1, SagaOutcome::Done(b"one".to_vec())).unwrap();
    complete(&mut m, 6, 0, call_id("run-1", 0), applied(b"out")).unwrap();
    commit(&mut m);

    let items = pending_items(&m).unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!((items[0].item, items[0].target.as_str()), (0, "caller"));
    assert_eq!((items[1].item, items[1].target.as_str()), (1, RUNS));
    assert!(matches!(delivered(&items[0].payload), Delivery::Result(_)));
    assert!(matches!(
        delivered(&items[1].payload),
        Delivery::CallCompleted(_)
    ));
    drain_applied(&mut m, 7);
    assert_eq!(pending_deliveries(&m), 0);

    // the drained cursor persists: new entries continue the numbering.
    exec(&mut m, &mut caller, &dispatch_op("d2", b"in")).unwrap();
    exec(
        &mut m,
        &mut requester_ctx(8),
        &call_op("run-1", 1, "chat", b"x"),
    )
    .unwrap();
    commit(&mut m);
    complete(&mut m, 9, 1, call_id("run-1", 1), applied(b"out")).unwrap();
    let k2 = dispatch_key("caller", "d2");
    callback_for(&mut m, 9, &k2, SagaOutcome::Done(b"two".to_vec())).unwrap();
    commit(&mut m);
    let items = pending_items(&m).unwrap();
    let numbers: Vec<u64> = items.iter().map(|item| item.item).collect();
    assert_eq!(numbers, vec![2, 3]);
    assert!(matches!(
        delivered(&items[0].payload),
        Delivery::CallCompleted(_)
    ));
    assert!(matches!(delivered(&items[1].payload), Delivery::Result(_)));
    // the call queue numbering persisted the same way.
    assert_eq!(call_view(&m, &call_id("run-1", 1)).unwrap().enqueued, 1);

    // a stale acknowledgment of an old number is a no-op, never a retirement
    // of a new item.
    ack(&mut m, 10, 0, "caller", DeliveryOutcome::Applied).unwrap();
    ack(&mut m, 10, 1, RUNS, DeliveryOutcome::Applied).unwrap();
    assert!(m.staged.is_empty());
    assert_eq!(pending_deliveries(&m), 2);
}

#[test]
fn a_saga_result_flows_through_pending_items_to_a_delivered_receipt() {
    let mut m = module();
    let key = registered_and_dispatched(&mut m, OutputContract::Text);
    callback_for(&mut m, 6, &key, SagaOutcome::Done(b"result".to_vec())).unwrap();
    commit(&mut m);
    let view = get_dispatch(&m, &key).unwrap();
    assert_eq!(view.status, DispatchStatus::AwaitingDelivery);
    assert_eq!(view.outcome, Some(Ok(b"result".to_vec())));
    assert_eq!(pending_deliveries(&m), 1);

    let items = pending_items(&m).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].item, 0);
    assert_eq!(items[0].target, "caller");
    assert_eq!(
        delivered(&items[0].payload),
        Delivery::Result(ResultEvent {
            dispatch_id: "d1".into(),
            recipe_id: "summarize".into(),
            outcome: Ok(b"result".to_vec()),
        })
    );
    let item_ref = ItemRef {
        source: "dispatch".into(),
        item: 0,
    };
    assert_eq!(
        items[0].cause,
        Cause::Chain {
            root: Root::Item(item_ref.clone()),
            hop: Hop::Delivery(item_ref),
        }
    );

    ack(&mut m, 7, 0, "caller", DeliveryOutcome::Applied).unwrap();
    commit(&mut m);
    let view = get_dispatch(&m, &key).unwrap();
    assert_eq!(
        view.status,
        DispatchStatus::Delivered {
            delivery: DeliveryOutcome::Applied
        }
    );
    assert_eq!(view.outcome, None);
    assert_eq!(pending_deliveries(&m), 0);
}

#[test]
fn calls_and_work_reserve_completion_numbers_together() {
    let mut m = module();
    stage_mailbox(
        &mut m.staged,
        Mailbox {
            head: u64::MAX - 2,
            next: u64::MAX - 2,
        },
    );
    commit(&mut m);
    let key = registered_and_dispatched(&mut m, OutputContract::Text);
    let mut requester = requester_ctx(5);
    exec(
        &mut m,
        &mut requester,
        &call_op("reserved", 0, "chat", b"hello"),
    )
    .unwrap();
    commit(&mut m);
    let error = exec(
        &mut m,
        &mut requester,
        &call_op("overflow", 0, "chat", b"hello"),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("cannot reserve a completion slot")
    );
    let mut extra_work = mk_ctx(6, Origin::Module("caller".into()));
    assert!(exec(&mut m, &mut extra_work, &dispatch_op("overflow", b"input")).is_err());
    assert!(extra_work.msgs().is_empty());
    callback_for(&mut m, 7, &key, SagaOutcome::Done(b"work result".to_vec())).unwrap();
    let mut system = mk_ctx(8, Origin::System);
    exec(
        &mut m,
        &mut system,
        &DispatchMsg::CompleteCall {
            enqueued: 0,
            id: call_id("reserved", 0),
            outcome: CallOutcome::Applied {
                output: Vec::new(),
                assigned: Vec::new(),
            },
        },
    )
    .unwrap();
    commit(&mut m);
    assert_eq!(
        block_on(records::committed_mailbox(&m.staged))
            .unwrap()
            .next,
        u64::MAX
    );
    assert_eq!(
        block_on(records::staged_reservations(&m.staged)).unwrap(),
        0
    );
}
