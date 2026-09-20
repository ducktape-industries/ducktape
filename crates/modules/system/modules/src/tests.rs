//! the modules registry proof: the module-code path on one module, one root, one wire.

use super::*;
use sdk_testkit::TestCtx;

/// the sole host-routed read the registry makes: answers the valset `Validators`
/// query with `members`, every other query unsupported.
fn validators(members: Vec<Vec<u8>>) -> impl FnMut(&[u8]) -> Result<Vec<u8>, Error> {
    move |req| {
        let is_validators = matches!(
            valset::decode_query(req),
            Ok(valset::ValsetQuery::Validators)
        );
        if is_validators {
            return Ok(valset::encode_reply(&valset::ValsetReply::Validators(
                members.clone(),
            )));
        }
        Err(Error::QueryUnsupported)
    }
}

fn env_at(origin: Origin, height: u64) -> sdk::Env {
    sdk::Env {
        height,
        consensus_time: 0,
        origin,
        me: "modules".into(),
        cause: sdk::Cause::Direct,
    }
}

/// a ctx over `origin`/`height`, valset defaulting to a single member.
fn ctx(origin: Origin, height: u64) -> TestCtx {
    TestCtx::with_env(env_at(origin, height)).on_query("valset", validators(vec![member(1)]))
}

/// registry-domain verb kept as a call-site method so the `.with_members`
/// sites read unchanged: it re-keys the valset reply.
trait WithMembers {
    fn with_members(self, members: Vec<Vec<u8>>) -> Self;
}
impl WithMembers for TestCtx {
    fn with_members(self, members: Vec<Vec<u8>>) -> Self {
        self.on_query("valset", validators(members))
    }
}

fn member(seed: u8) -> Vec<u8> {
    vec![seed; 32]
}
fn hash(seed: u8) -> Vec<u8> {
    vec![seed; CODE_HASH_LEN]
}
fn fresh() -> Modules {
    Modules::new(
        "modules",
        Box::new(sdk_testkit::MemStore::new()),
        "valset",
        "governance",
    )
}

/// the root of a store that never committed anything — the store-backed twin
/// of the old ZERO sentinel.
fn empty_root() -> StateRoot {
    fresh().root()
}
fn msg(m: ModulesMsg) -> Msg {
    Msg {
        target: "modules".into(),
        payload: encode_msg(&m),
    }
}
fn run(lc: &mut Modules, ctx: &mut TestCtx, m: &Msg) -> Result<(), Error> {
    futures::executor::block_on(lc.execute(ctx, m))
}
fn commit(lc: &mut Modules) {
    futures::executor::block_on(lc.commit_block()).unwrap();
}

fn advance() -> Msg {
    msg(ModulesMsg::Advance)
}

// ---- module-code path helpers -----------------------------------------------

fn register_module(lc: &mut Modules, module_id: &str, code: u8) {
    register_kind(lc, module_id, Kind::Module, code);
}
fn register_kind(lc: &mut Modules, module_id: &str, kind: Kind, code: u8) {
    let mut sys = ctx(Origin::System, 0);
    run(
        lc,
        &mut sys,
        &msg(ModulesMsg::RegisterModule {
            module_id: module_id.into(),
            kind,
            code_hash: hash(code),
            lanes: Vec::new(),
        }),
    )
    .unwrap();
    commit(lc);
}
fn schedule_swap(module_id: &str, name: &str, ah: u64, code: u8) -> Msg {
    msg(ModulesMsg::ScheduleSwap {
        name: name.into(),
        module_id: module_id.into(),
        activation_height: ah,
        code_hash: hash(code),
    })
}
fn schedule_register(module_id: &str, name: &str, ah: u64, code: u8) -> Msg {
    schedule_register_kind(module_id, Kind::Module, name, ah, code)
}
fn schedule_register_kind(module_id: &str, kind: Kind, name: &str, ah: u64, code: u8) -> Msg {
    msg(ModulesMsg::ScheduleRegister {
        name: name.into(),
        module_id: module_id.into(),
        kind,
        activation_height: ah,
        code_hash: hash(code),
        lanes: Vec::new(),
    })
}
fn cancel_swap(module_id: &str, name: &str) -> Msg {
    msg(ModulesMsg::CancelSwap {
        name: name.into(),
        module_id: module_id.into(),
    })
}
fn swap_ready(module_id: &str, name: &str, code: u8) -> Msg {
    msg(ModulesMsg::SwapReady {
        name: name.into(),
        module_id: module_id.into(),
        code_hash: hash(code),
    })
}
/// drive the full single-member readiness latch for a pending swap.
fn make_swap_ready(lc: &mut Modules, module_id: &str, name: &str, code: u8) {
    let mut ext = ctx(Origin::External(member(1)), 0);
    run(lc, &mut ext, &swap_ready(module_id, name, code)).unwrap();
    commit(lc);
}
fn module_status(lc: &Modules) -> Vec<ModuleCode> {
    let bytes = futures::executor::block_on(lc.query_with(
        &ctx(Origin::System, 0),
        &encode_query(&ModulesQuery::ModuleStatus),
    ))
    .unwrap();
    match decode_reply(&bytes).unwrap() {
        ModulesReply::ModuleStatus { modules } => modules,
        other => panic!("expected ModuleStatus, got {other:?}"),
    }
}
fn armed_at(lc: &Modules, height: u64) -> Vec<ArmedSwap> {
    let bytes = futures::executor::block_on(lc.query_with(
        &ctx(Origin::System, 0),
        &encode_query(&ModulesQuery::ArmedAt { height }),
    ))
    .unwrap();
    match decode_reply(&bytes).unwrap() {
        ModulesReply::ArmedAt { swaps } => swaps,
        other => panic!("expected ArmedAt, got {other:?}"),
    }
}

// ============================================================================
// module-code path
// ============================================================================

/// the network-capture gate: a module origin that is NOT governance is refused
/// on every authoring op. an admitted app module's follow-up carries
/// `Origin::Module("chat")`, and accepting it would let that one module
/// schedule a swap of governance itself — no ballot, total capture.
#[test]
fn a_non_governance_module_authors_nothing() {
    let mut lc = fresh();
    register_module(&mut lc, "governance", 1);
    let mut chat = ctx(Origin::Module("chat".into()), 0);
    let refused = [
        msg(ModulesMsg::RegisterModule {
            module_id: "kanban".into(),
            kind: Kind::Module,
            code_hash: hash(1),
            lanes: Vec::new(),
        }),
        schedule_swap("governance", "capture", 10, 9),
        schedule_register("kanban", "capture", 10, 9),
        cancel_swap("governance", "capture"),
    ];
    for m in &refused {
        assert!(
            matches!(run(&mut lc, &mut chat, m), Err(Error::Module { ref reason, .. }) if reason == "not_governance"),
            "a chat-module origin authored {m:?}"
        );
    }
    // the SAME ops from governance's own follow-up pass the gate.
    let mut gov = ctx(Origin::Module("governance".into()), 0);
    run(
        &mut lc,
        &mut gov,
        &schedule_swap("governance", "upgrade", 10, 9),
    )
    .unwrap();
    commit(&mut lc);
    assert_eq!(
        module_status(&lc)[0].pending.as_ref().unwrap().code_hash,
        hash(9)
    );
}

#[test]
fn register_and_schedule_origin_gate() {
    let mut lc = fresh();
    let mut ext = ctx(Origin::External(member(1)), 0);
    assert!(matches!(
        run(
            &mut lc,
            &mut ext,
            &msg(ModulesMsg::RegisterModule {
                module_id: "hello".into(),
                kind: Kind::Module,
                code_hash: hash(1),
                lanes: Vec::new()
            })
        ),
        Err(Error::Module { ref reason, .. }) if reason == "not_governance"
    ));
    let mut gov = ctx(Origin::Module("governance".into()), 0);
    run(
        &mut lc,
        &mut gov,
        &msg(ModulesMsg::RegisterModule {
            module_id: "hello".into(),
            kind: Kind::Module,
            code_hash: hash(1),
            lanes: Vec::new(),
        }),
    )
    .unwrap();
    commit(&mut lc);
    assert_eq!(module_status(&lc)[0].active_code_hash, hash(1));
    assert!(matches!(
        run(
            &mut lc,
            &mut ext,
            &schedule_swap("hello", "replacement", 10, 2)
        ),
        Err(Error::Module { ref reason, .. }) if reason == "not_governance"
    ));
    run(
        &mut lc,
        &mut gov,
        &schedule_swap("hello", "replacement", 10, 2),
    )
    .unwrap();
    commit(&mut lc);
    assert!(module_status(&lc)[0].pending.is_some());
}

#[test]
fn advance_is_system_only() {
    let mut lc = fresh();
    register_module(&mut lc, "hello", 1);
    let mut gov = ctx(Origin::Module("governance".into()), 10);
    assert!(matches!(
        run(&mut lc, &mut gov, &advance()),
        Err(Error::Module { ref reason, .. }) if reason == "not_system"
    ));
}

#[test]
fn register_rejects_reregistration_and_bad_hash() {
    let mut lc = fresh();
    register_module(&mut lc, "hello", 1);
    let mut sys = ctx(Origin::System, 0);
    assert!(
        run(
            &mut lc,
            &mut sys,
            &msg(ModulesMsg::RegisterModule {
                module_id: "hello".into(),
                kind: Kind::Module,
                code_hash: hash(9),
                lanes: Vec::new()
            })
        )
        .is_err()
    );
    assert!(
        run(
            &mut lc,
            &mut sys,
            &msg(ModulesMsg::RegisterModule {
                module_id: "other".into(),
                kind: Kind::Module,
                code_hash: vec![1, 2, 3],
                lanes: Vec::new()
            })
        )
        .is_err()
    );
}

#[test]
fn schedule_swap_validation() {
    let mut lc = fresh();
    register_module(&mut lc, "hello", 1);
    let mut sys = ctx(Origin::System, 0);
    // unregistered module.
    assert!(
        run(
            &mut lc,
            &mut sys,
            &schedule_swap("ghost", "replacement", 10, 2)
        )
        .is_err()
    );
    // min lead.
    assert!(
        run(
            &mut lc,
            &mut sys,
            &schedule_swap("hello", "replacement", 3, 2)
        )
        .is_err()
    );
    // no-op swap.
    assert!(
        run(
            &mut lc,
            &mut sys,
            &schedule_swap("hello", "replacement", 10, 1)
        )
        .is_err()
    );
    run(
        &mut lc,
        &mut sys,
        &schedule_swap("hello", "replacement", 10, 2),
    )
    .unwrap();
    commit(&mut lc);
    // second pending.
    assert!(run(&mut lc, &mut sys, &schedule_swap("hello", "other", 20, 3)).is_err());
}

#[test]
fn swap_advance_activates_at_height_and_frees_slot() {
    let mut lc = fresh();
    register_module(&mut lc, "hello", 1);
    let mut sys = ctx(Origin::System, 0);
    run(
        &mut lc,
        &mut sys,
        &schedule_swap("hello", "replacement", 10, 2),
    )
    .unwrap();
    commit(&mut lc);
    make_swap_ready(&mut lc, "hello", "replacement", 2);

    // below activation: no-op.
    let root_before = lc.root();
    let mut below = ctx(Origin::System, 9);
    run(&mut lc, &mut below, &advance()).unwrap();
    commit(&mut lc);
    assert_eq!(lc.root(), root_before);
    assert_eq!(module_status(&lc)[0].active_code_hash, hash(1));

    // at activation.
    let mut at = ctx(Origin::System, 10);
    run(&mut lc, &mut at, &advance()).unwrap();
    commit(&mut lc);
    assert_eq!(module_status(&lc)[0].active_code_hash, hash(2));
    assert!(module_status(&lc)[0].pending.is_none());
}

#[test]
fn swap_readiness_gate() {
    let mut lc = fresh();
    register_module(&mut lc, "hello", 1);
    let mut sys = ctx(Origin::System, 0);
    run(
        &mut lc,
        &mut sys,
        &schedule_swap("hello", "replacement", 10, 2),
    )
    .unwrap();
    commit(&mut lc);
    // no readiness: never arms.
    assert!(armed_at(&lc, u64::MAX).is_empty());

    let two = vec![member(1), member(2)];
    let mut m1 = ctx(Origin::External(member(1)), 0).with_members(two.clone());
    run(&mut lc, &mut m1, &swap_ready("hello", "replacement", 2)).unwrap();
    commit(&mut lc);
    assert!(
        module_status(&lc)[0]
            .pending
            .clone()
            .unwrap()
            .ready_at
            .is_none()
    );
    assert!(armed_at(&lc, 10).is_empty());

    // the covering signal, in block 6: the latch records THAT height.
    let mut m2 = ctx(Origin::External(member(2)), 6).with_members(two);
    run(&mut lc, &mut m2, &swap_ready("hello", "replacement", 2)).unwrap();
    commit(&mut lc);
    let latched = module_status(&lc)[0].pending.clone().unwrap();
    assert_eq!(latched.ready_at, Some(6));
    assert_eq!(armed_at(&lc, 10).len(), 1);
    // the arm predicate: latched STRICTLY before the height, floor reached.
    assert!(!latched.armed_at(6), "the latch block itself is not armed");
    assert!(!latched.armed_at(9), "below the activation floor");
    assert!(latched.armed_at(10));
    let early = ScheduledSwap {
        activation_height: 3,
        ..latched.clone()
    };
    assert!(
        !early.armed_at(6),
        "latched at 6: block 6 still runs old code"
    );
    assert!(early.armed_at(7));
    let unlatched = ScheduledSwap {
        ready_at: None,
        ..latched
    };
    assert!(!unlatched.armed_at(u64::MAX));
}

#[test]
fn swap_signal_gates_origin_and_identity() {
    let mut lc = fresh();
    register_module(&mut lc, "hello", 1);
    let mut sys = ctx(Origin::System, 0);
    run(
        &mut lc,
        &mut sys,
        &schedule_swap("hello", "replacement", 10, 2),
    )
    .unwrap();
    commit(&mut lc);
    assert!(run(&mut lc, &mut sys, &swap_ready("hello", "replacement", 2)).is_err());
    let mut stranger = ctx(Origin::External(member(9)), 0);
    assert!(
        run(
            &mut lc,
            &mut stranger,
            &swap_ready("hello", "replacement", 2)
        )
        .is_err()
    );
    let mut m1 = ctx(Origin::External(member(1)), 0);
    assert!(run(&mut lc, &mut m1, &swap_ready("hello", "vX", 2)).is_err());
}

/// A SIGNAL NAMES BYTES, NOT A NAME. A schedule may replace a stale pending
/// under the SAME name with a different hash; a signal written for the old
/// pending and finalized after the replacement must be refused, not credited
/// as readiness for code that validator never verified.
#[test]
fn swap_ready_for_a_replaced_pending_is_refused() {
    let mut lc = fresh();
    register_module(&mut lc, "hello", 1);
    let mut sys = ctx(Origin::System, 0);
    run(&mut lc, &mut sys, &schedule_swap("hello", "upgrade", 10, 2)).unwrap();
    commit(&mut lc);

    // the first pending goes stale and a NEW hash takes its name.
    let mut past = ctx(Origin::System, 30);
    run(
        &mut lc,
        &mut past,
        &schedule_swap("hello", "upgrade", 40, 3),
    )
    .unwrap();
    commit(&mut lc);

    // the in-flight signal for hash 2 lands now: same name, wrong bytes.
    let mut m1 = ctx(Origin::External(member(1)), 31);
    assert!(run(&mut lc, &mut m1, &swap_ready("hello", "upgrade", 2)).is_err());
    assert!(
        module_status(&lc)[0]
            .pending
            .as_ref()
            .unwrap()
            .readiness
            .is_empty(),
        "a refused signal credits nothing"
    );

    // the signal for the bytes actually pending is accepted and latches.
    run(&mut lc, &mut m1, &swap_ready("hello", "upgrade", 3)).unwrap();
    commit(&mut lc);
    let pending = module_status(&lc)[0].pending.clone().unwrap();
    assert_eq!(pending.readiness, vec![member(1)]);
    assert_eq!(pending.ready_at, Some(31));
}

#[test]
fn swap_cancel_guards_and_clears() {
    let mut lc = fresh();
    register_module(&mut lc, "hello", 1);
    let mut sys = ctx(Origin::System, 0);
    run(
        &mut lc,
        &mut sys,
        &schedule_swap("hello", "replacement", 10, 2),
    )
    .unwrap();
    commit(&mut lc);
    assert!(run(&mut lc, &mut sys, &cancel_swap("hello", "vX")).is_err());
    // ARMED (readiness latched) and past its activation height: the boundary's
    // business, not governance's.
    make_swap_ready(&mut lc, "hello", "replacement", 2);
    let mut late = ctx(Origin::System, 10);
    assert!(run(&mut lc, &mut late, &cancel_swap("hello", "replacement")).is_err());
    run(&mut lc, &mut sys, &cancel_swap("hello", "replacement")).unwrap();
    commit(&mut lc);
    assert!(module_status(&lc)[0].pending.is_none());
    assert_eq!(module_status(&lc)[0].active_code_hash, hash(1));
}

/// a pending that PASSED its activation height without ever latching readiness
/// never arms — the shape a designation for a component no validator can probe
/// takes. governance may replace it and may cancel it; an armed or not-yet-due
/// pending still refuses both.
#[test]
fn a_stale_pending_swap_is_replaceable_and_cancellable() {
    // replace: reschedule to a new hash past the dead activation height.
    let mut lc = fresh();
    register_module(&mut lc, "hello", 1);
    let mut sys = ctx(Origin::System, 0);
    run(&mut lc, &mut sys, &schedule_swap("hello", "dead", 10, 2)).unwrap();
    commit(&mut lc);
    // not yet due: still refuses a second schedule and a cancel is allowed.
    let mut early = ctx(Origin::System, 9);
    assert!(run(&mut lc, &mut early, &schedule_swap("hello", "next", 40, 3)).is_err());
    let mut past = ctx(Origin::System, 30);
    run(&mut lc, &mut past, &schedule_swap("hello", "next", 40, 3)).unwrap();
    commit(&mut lc);
    let pending = module_status(&lc)[0].pending.clone().unwrap();
    assert_eq!(pending.name, "next");
    assert_eq!(pending.code_hash, hash(3));
    assert!(armed_at(&lc, 30).is_empty());

    // cancel: the same stale pending, withdrawn instead of replaced.
    let mut lc = fresh();
    register_module(&mut lc, "hello", 1);
    run(&mut lc, &mut sys, &schedule_swap("hello", "dead", 10, 2)).unwrap();
    commit(&mut lc);
    let mut past = ctx(Origin::System, 30);
    run(&mut lc, &mut past, &cancel_swap("hello", "dead")).unwrap();
    commit(&mut lc);
    assert!(module_status(&lc)[0].pending.is_none());

    // ARMED past its height: neither replaceable nor cancellable.
    let mut lc = fresh();
    register_module(&mut lc, "hello", 1);
    run(&mut lc, &mut sys, &schedule_swap("hello", "armed", 10, 2)).unwrap();
    commit(&mut lc);
    make_swap_ready(&mut lc, "hello", "armed", 2);
    let mut past = ctx(Origin::System, 30);
    assert!(run(&mut lc, &mut past, &schedule_swap("hello", "next", 40, 3)).is_err());
    assert!(run(&mut lc, &mut past, &cancel_swap("hello", "armed")).is_err());
}

/// #1676: a SwapReady arriving AFTER the pending already went stale (past its
/// activation height, never latched) must not resurrect it. Before the fix,
/// `handle_swap_ready` latched `ready_at` on any covering signal regardless of
/// how late it was, which armed the swap at the next height with no fresh
/// decision behind it and then froze it — `stale_at` becomes permanently
/// false once latched, so neither CancelSwap nor a replacing ScheduleSwap
/// could touch it again.
#[test]
fn a_late_swap_ready_after_activation_height_never_arms_the_stale_pending() {
    let mut lc = fresh();
    register_module(&mut lc, "hello", 1);
    let mut sys = ctx(Origin::System, 0);
    run(&mut lc, &mut sys, &schedule_swap("hello", "dead", 10, 2)).unwrap();
    commit(&mut lc);

    // the pending is stale well past its activation height, never latched.
    assert!(
        module_status(&lc)[0]
            .pending
            .as_ref()
            .unwrap()
            .ready_at
            .is_none()
    );
    assert!(module_status(&lc)[0].pending.as_ref().unwrap().stale_at(30));

    // a signal lands at height 100 — long after activation_height 10.
    let mut late = ctx(Origin::External(member(1)), 100);
    run(&mut lc, &mut late, &swap_ready("hello", "dead", 2)).unwrap();
    commit(&mut lc);

    // it must NOT arm: no latch, no activation at the next boundary.
    let pending = module_status(&lc)[0].pending.clone().unwrap();
    assert!(
        pending.ready_at.is_none(),
        "a late signal must not latch a stale pending"
    );
    assert!(armed_at(&lc, 101).is_empty());

    // still evictable: CancelSwap succeeds on the stale pending.
    let mut past = ctx(Origin::System, 100);
    run(&mut lc, &mut past, &cancel_swap("hello", "dead")).unwrap();
    commit(&mut lc);
    assert!(module_status(&lc)[0].pending.is_none());

    // ...and a fresh ScheduleSwap can (re)designate the module.
    let mut fresh_sched = ctx(Origin::System, 100);
    run(
        &mut lc,
        &mut fresh_sched,
        &schedule_swap("hello", "retry", 200, 3),
    )
    .unwrap();
    commit(&mut lc);
    assert_eq!(
        module_status(&lc)[0].pending.as_ref().unwrap().code_hash,
        hash(3)
    );
}

/// the netstack shape: an ADMISSION (never activated, so no readiness probe can
/// ever latch it) is re-designated by a later schedule once it is past due.
#[test]
fn a_stale_admission_is_re_designated() {
    let mut lc = fresh();
    let mut sys = ctx(Origin::System, 0);
    run(
        &mut lc,
        &mut sys,
        &schedule_register("netstack", "v1", 10, 2),
    )
    .unwrap();
    commit(&mut lc);
    let mut past = ctx(Origin::System, 30);
    run(&mut lc, &mut past, &schedule_swap("netstack", "v2", 40, 3)).unwrap();
    commit(&mut lc);
    let entry = &module_status(&lc)[0];
    assert!(entry.active_code_hash.is_empty());
    assert_eq!(entry.pending.as_ref().unwrap().code_hash, hash(3));
}

// ---- admission (ScheduleRegister) -------------------------------------------

#[test]
fn admission_realizes_and_refuses_bad_inputs() {
    let mut lc = fresh();
    let mut gov = ctx(Origin::Module("governance".into()), 0);
    run(&mut lc, &mut gov, &schedule_register("kanban", "v1", 10, 5)).unwrap();
    commit(&mut lc);
    assert!(module_status(&lc)[0].active_code_hash.is_empty());
    assert!(armed_at(&lc, 10).is_empty(), "not armed until ready");
    make_swap_ready(&mut lc, "kanban", "v1", 5);
    assert_eq!(armed_at(&lc, 10)[0].code_hash, hash(5));
    let mut at = ctx(Origin::System, 10);
    run(&mut lc, &mut at, &advance()).unwrap();
    commit(&mut lc);
    assert_eq!(module_status(&lc)[0].active_code_hash, hash(5));
    assert!(module_status(&lc)[0].pending.is_none());

    // refuses an existing id, short lead, external origin, live host id.
    let mut lc = fresh();
    register_module(&mut lc, "hello", 1);
    let mut sys = ctx(Origin::System, 0);
    assert!(run(&mut lc, &mut sys, &schedule_register("hello", "v1", 10, 5)).is_err());
    assert!(
        run(
            &mut lc,
            &mut sys,
            &schedule_register("kanban", "v1", MIN_SWAP_LEAD, 5)
        )
        .is_err()
    );
    let mut ext = ctx(Origin::External(member(1)), 0);
    assert!(run(&mut lc, &mut ext, &schedule_register("kanban", "v1", 10, 5)).is_err());
    let mut live = ctx(Origin::System, 0).with_module_root("valset", StateRoot::ZERO);
    assert!(
        run(
            &mut lc,
            &mut live,
            &schedule_register("valset", "v1", 10, 5)
        )
        .is_err()
    );
}

#[test]
fn cancelled_admission_removes_entry() {
    let mut lc = fresh();
    let mut sys = ctx(Origin::System, 0);
    run(&mut lc, &mut sys, &schedule_register("kanban", "v1", 10, 5)).unwrap();
    commit(&mut lc);
    run(&mut lc, &mut sys, &cancel_swap("kanban", "v1")).unwrap();
    commit(&mut lc);
    assert!(module_status(&lc).is_empty());
}

// ---- kind ------------------------------------------------------------------

/// what an entry IS travels with it: a seed, a register and an admission each
/// record their kind, and status reports it.
#[test]
fn seed_register_and_admission_carry_kind_and_status_reports_it() {
    let mut lc = fresh();
    futures::executor::block_on(async {
        lc.seed("chat", Kind::Module, hash(1), Vec::new())
            .await
            .unwrap();
        lc.seed("home", Kind::View, hash(2), Vec::new())
            .await
            .unwrap();
        lc.finish_seed().await.unwrap();
    });
    register_kind(&mut lc, "kanban", Kind::Module, 3);
    register_kind(&mut lc, "board", Kind::View, 4);
    let mut gov = ctx(Origin::Module("governance".into()), 0);
    run(
        &mut lc,
        &mut gov,
        &schedule_register_kind("later", Kind::View, "v1", 10, 5),
    )
    .unwrap();
    commit(&mut lc);
    let kinds: Vec<(String, Kind)> = module_status(&lc)
        .into_iter()
        .map(|entry| (entry.module_id, entry.kind))
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("board".into(), Kind::View),
            ("chat".into(), Kind::Module),
            ("home".into(), Kind::View),
            ("kanban".into(), Kind::Module),
            ("later".into(), Kind::View),
        ]
    );
}

/// the kind is in the root: two registries that differ only in one entry's
/// kind commit different state.
#[test]
fn kind_is_committed_state() {
    let mut as_module = fresh();
    register_kind(&mut as_module, "home", Kind::Module, 1);
    let mut as_view = fresh();
    register_kind(&mut as_view, "home", Kind::View, 1);
    assert_ne!(as_module.root(), as_view.root());
}

/// a swap keeps the entry's kind: schedule, readiness, activation and the
/// history behave for a `View` exactly as they do for a `Module`, and the
/// kind never changes across the flip.
#[test]
fn a_swap_preserves_kind_and_a_view_swaps_like_a_module() {
    let mut lc = fresh();
    register_kind(&mut lc, "home", Kind::View, 1);
    register_kind(&mut lc, "chat", Kind::Module, 1);
    let mut gov = ctx(Origin::Module("governance".into()), 0);
    for id in ["home", "chat"] {
        run(&mut lc, &mut gov, &schedule_swap(id, "next", 10, 2)).unwrap();
    }
    commit(&mut lc);
    for entry in module_status(&lc) {
        assert_eq!(entry.pending.as_ref().unwrap().code_hash, hash(2));
        assert_eq!(entry.active_code_hash, hash(1));
    }
    assert!(armed_at(&lc, 10).is_empty(), "not armed until ready");
    make_swap_ready(&mut lc, "home", "next", 2);
    make_swap_ready(&mut lc, "chat", "next", 2);
    let armed: Vec<String> = armed_at(&lc, 10)
        .into_iter()
        .map(|swap| swap.module_id)
        .collect();
    assert_eq!(armed, ["chat", "home"]);
    let mut at = ctx(Origin::System, 10);
    run(&mut lc, &mut at, &advance()).unwrap();
    commit(&mut lc);
    let status = module_status(&lc);
    let home = status
        .iter()
        .find(|entry| entry.module_id == "home")
        .unwrap();
    let chat = status
        .iter()
        .find(|entry| entry.module_id == "chat")
        .unwrap();
    assert_eq!(home.kind, Kind::View, "the swap kept the view's kind");
    assert_eq!(chat.kind, Kind::Module);
    for entry in [home, chat] {
        assert_eq!(entry.active_code_hash, hash(2));
        assert!(entry.pending.is_none());
        assert_eq!(entry.history.len(), 2);
    }
    // the same gates: a no-op swap and a second in-flight pending refuse
    // for a view exactly as for a module.
    assert!(run(&mut lc, &mut gov, &schedule_swap("home", "same", 20, 2)).is_err());
    run(&mut lc, &mut gov, &schedule_swap("home", "third", 20, 3)).unwrap();
    commit(&mut lc);
    assert!(run(&mut lc, &mut gov, &schedule_swap("home", "fourth", 30, 4)).is_err());
    let mut sys = ctx(Origin::System, 0);
    run(&mut lc, &mut sys, &cancel_swap("home", "third")).unwrap();
    commit(&mut lc);
    let home = module_status(&lc)
        .into_iter()
        .find(|entry| entry.module_id == "home")
        .unwrap();
    assert!(home.pending.is_none());
    assert_eq!(home.kind, Kind::View);
}

/// an admission of a view realizes at its boundary like a module's, and a
/// cancelled one leaves no entry behind.
#[test]
fn a_view_admission_realizes_and_cancels_like_a_module() {
    let mut lc = fresh();
    let mut gov = ctx(Origin::Module("governance".into()), 0);
    run(
        &mut lc,
        &mut gov,
        &schedule_register_kind("home", Kind::View, "v1", 10, 5),
    )
    .unwrap();
    commit(&mut lc);
    let home = &module_status(&lc)[0];
    assert_eq!(home.kind, Kind::View);
    assert!(home.active_code_hash.is_empty());
    make_swap_ready(&mut lc, "home", "v1", 5);
    let mut at = ctx(Origin::System, 10);
    run(&mut lc, &mut at, &advance()).unwrap();
    commit(&mut lc);
    let home = &module_status(&lc)[0];
    assert_eq!(home.kind, Kind::View);
    assert_eq!(home.active_code_hash, hash(5));

    let mut lc = fresh();
    run(
        &mut lc,
        &mut gov,
        &schedule_register_kind("home", Kind::View, "v1", 10, 5),
    )
    .unwrap();
    commit(&mut lc);
    run(&mut lc, &mut gov, &cancel_swap("home", "v1")).unwrap();
    commit(&mut lc);
    assert!(module_status(&lc).is_empty());
}

// ---- the activation history (code-at-height) --------------------------------

fn activation(height: u64, code: u8) -> Activation {
    Activation {
        height,
        code_hash: hash(code),
    }
}

/// a hand-built entry over `history` (`(height, code)` pairs, ascending).
fn entry(history: &[(u64, u8)], pending: Option<ScheduledSwap>) -> ModuleCode {
    ModuleCode {
        module_id: "hello".into(),
        kind: Kind::Module,
        active_code_hash: history.last().map(|(_, c)| hash(*c)).unwrap_or_default(),
        pending,
        history: history.iter().map(|(h, c)| activation(*h, *c)).collect(),
    }
}

#[test]
fn code_at_reads_the_armed_pending_then_the_history() {
    let e = entry(&[(10, 1), (50, 2)], None);
    assert_eq!(code_at(&e, 20), Some(hash(1).as_slice()));
    assert_eq!(code_at(&e, 50), Some(hash(2).as_slice()));
    assert_eq!(code_at(&e, 70), Some(hash(2).as_slice()));
    // before the first activation: the first code is the natural seat.
    assert_eq!(code_at(&e, 5), Some(hash(1).as_slice()));

    // a pending swap armed at `height` wins — the live pre-flip read.
    let armed = ScheduledSwap {
        name: "replacement".into(),
        activation_height: 50,
        code_hash: hash(2),
        readiness: vec![member(1)],
        ready_at: Some(20),
    };
    let e = entry(&[(10, 1)], Some(armed.clone()));
    assert_eq!(code_at(&e, 50), Some(hash(2).as_slice()));
    assert_eq!(code_at(&e, 49), Some(hash(1).as_slice()));
    // an unready pending never arms, however high the height.
    let unready = ScheduledSwap {
        ready_at: None,
        ..armed.clone()
    };
    let e = entry(&[(10, 1)], Some(unready));
    assert_eq!(code_at(&e, 99), Some(hash(1).as_slice()));
    // LATE readiness: latched at 45, past the activation floor of 40. blocks
    // 40..=45 ran on the old code live (the drain saw the latch only from
    // 46), so a replay whose tip is 45 must say so.
    let late = ScheduledSwap {
        activation_height: 40,
        ready_at: Some(45),
        ..armed
    };
    let e = entry(&[(10, 1)], Some(late));
    assert_eq!(code_at(&e, 42), Some(hash(1).as_slice()));
    assert_eq!(code_at(&e, 45), Some(hash(1).as_slice()));
    assert_eq!(code_at(&e, 46), Some(hash(2).as_slice()));

    // registered, never activated: no code at all.
    assert_eq!(code_at(&entry(&[], None), 99), None);
}

#[test]
fn every_activation_is_appended_in_block_order() {
    let mut lc = fresh();
    let mut at7 = ctx(Origin::System, 7);
    run(
        &mut lc,
        &mut at7,
        &msg(ModulesMsg::RegisterModule {
            module_id: "hello".into(),
            kind: Kind::Module,
            code_hash: hash(1),
            lanes: Vec::new(),
        }),
    )
    .unwrap();
    commit(&mut lc);
    assert_eq!(module_status(&lc)[0].history, [activation(7, 1)]);

    run(
        &mut lc,
        &mut at7,
        &schedule_swap("hello", "replacement", 30, 2),
    )
    .unwrap();
    commit(&mut lc);
    make_swap_ready(&mut lc, "hello", "replacement", 2);
    assert_eq!(
        module_status(&lc)[0].history,
        [activation(7, 1)],
        "scheduling records nothing: only a flip is an activation"
    );
    let mut at30 = ctx(Origin::System, 30);
    run(&mut lc, &mut at30, &advance()).unwrap();
    commit(&mut lc);
    assert_eq!(
        module_status(&lc)[0].history,
        [activation(7, 1), activation(30, 2)]
    );

    // an admission has no history until its boundary flips it.
    let mut lc = fresh();
    let mut sys = ctx(Origin::System, 0);
    run(&mut lc, &mut sys, &schedule_register("kanban", "v1", 10, 5)).unwrap();
    commit(&mut lc);
    assert!(module_status(&lc)[0].history.is_empty());
    make_swap_ready(&mut lc, "kanban", "v1", 5);
    let mut at10 = ctx(Origin::System, 10);
    run(&mut lc, &mut at10, &advance()).unwrap();
    commit(&mut lc);
    assert_eq!(module_status(&lc)[0].history, [activation(10, 5)]);

    // a genesis seed is the activation at block zero.
    let mut lc = fresh();
    futures::executor::block_on(async {
        lc.seed("hello", Kind::Module, hash(1), Vec::new())
            .await
            .unwrap();
        lc.finish_seed().await.unwrap();
    });
    assert_eq!(module_status(&lc)[0].history, [activation(0, 1)]);
}

// ============================================================================
// root + snapshot
// ============================================================================

#[test]
fn root_empty_fresh_then_state_moves_it() {
    let mut lc = fresh();
    assert_eq!(lc.root(), empty_root());
    register_module(&mut lc, "hello", 1);
    let after_register = lc.root();
    assert_ne!(after_register, empty_root());
    let mut sys = ctx(Origin::System, 0);
    run(
        &mut lc,
        &mut sys,
        &schedule_swap("hello", "replacement", 10, 2),
    )
    .unwrap();
    commit(&mut lc);
    assert_ne!(lc.root(), after_register);
}

#[test]
fn genesis_seed_publishes_once_and_reseeding_is_a_no_op() {
    let mut lc = fresh();
    futures::executor::block_on(async {
        lc.seed("hello", Kind::Module, hash(1), Vec::new())
            .await
            .unwrap();
        lc.seed("directory", Kind::Module, hash(2), Vec::new())
            .await
            .unwrap();
        lc.finish_seed().await.unwrap();
    });
    let seeded = lc.root();
    assert_ne!(seeded, empty_root(), "the seed set is committed state");
    assert_eq!(module_status(&lc).len(), 2);

    // a reopened workspace re-entering the genesis path re-seeds — the
    // idempotence gate must leave the store byte-untouched.
    futures::executor::block_on(async {
        lc.seed("hello", Kind::Module, hash(9), Vec::new())
            .await
            .unwrap();
        lc.finish_seed().await.unwrap();
    });
    assert_eq!(
        lc.root(),
        seeded,
        "re-seeding an initialized store is a no-op"
    );
    let status = module_status(&lc);
    assert_eq!(
        status
            .iter()
            .find(|m| m.module_id == "hello")
            .unwrap()
            .active_code_hash,
        hash(1),
        "the original seed survived the re-entry"
    );
}

// ============================================================================
// data-plane lanes
//
// A lane id is not a local preference: it decides two overlay ports on EVERY
// node, so the registry has to be the thing that refuses a bad one. These
// assert the refusals, not the happy path — the happy path is one insert.
// ============================================================================

fn lanes_of(lc: &Modules) -> Vec<LaneRecord> {
    let bytes = futures::executor::block_on(
        lc.query_with(&ctx(Origin::System, 0), &encode_query(&ModulesQuery::Lanes)),
    )
    .unwrap();
    match decode_reply(&bytes).unwrap() {
        ModulesReply::Lanes { lanes } => lanes,
        other => panic!("expected Lanes, got {other:?}"),
    }
}

/// a lane whose name is derived from its id — these tests are about the ID
/// rules, so the name only has to be well-formed and distinct.
fn lane(id: u8) -> LaneDecl {
    named_lane(id, &format!("lane{id}"))
}

fn named_lane(id: u8, name: &str) -> LaneDecl {
    LaneDecl {
        id,
        name: name.to_string(),
        stream: Some(LaneStream {
            pacing: LanePacing::Shared,
            accept_backlog: 8,
        }),
    }
}

fn register_with_lanes(
    lc: &mut Modules,
    module_id: &str,
    lanes: Vec<LaneDecl>,
) -> Result<(), Error> {
    let mut sys = ctx(Origin::System, 0);
    run(
        lc,
        &mut sys,
        &msg(ModulesMsg::RegisterModule {
            module_id: module_id.into(),
            kind: Kind::Module,
            code_hash: hash(1),
            lanes,
        }),
    )
}

#[test]
fn a_declared_lane_is_readable_by_its_owner_and_name() {
    let mut lc = fresh();
    register_with_lanes(
        &mut lc,
        "chat",
        vec![named_lane(2, "voice"), named_lane(3, "video")],
    )
    .unwrap();
    commit(&mut lc);
    let lanes = lanes_of(&lc);
    assert_eq!(lanes.len(), 2);
    // (module_id, name) is the binding key — the id is only the port it
    // resolves to. A host that bound by position would swap voice and video
    // the day chat declares a third lane.
    let voice = lanes
        .iter()
        .find(|l| l.module_id == "chat" && l.name == "voice")
        .expect("chat declares a lane named voice");
    assert_eq!(voice.id, 2);
    let video = lanes
        .iter()
        .find(|l| l.module_id == "chat" && l.name == "video")
        .expect("chat declares a lane named video");
    assert_eq!(video.id, 3);
    assert_eq!(video.stream.as_ref().unwrap().accept_backlog, 8);
}

#[test]
fn a_datagram_only_lane_declares_no_stream_half() {
    // the media lanes are this shape: sockets, no stream plane, so nothing to
    // pace and no backlog. A record that forced a budget on them would be
    // inventing a number the node does not use.
    let mut lc = fresh();
    register_with_lanes(
        &mut lc,
        "chat",
        vec![LaneDecl {
            id: 2,
            name: "voice".into(),
            stream: None,
        }],
    )
    .unwrap();
    commit(&mut lc);
    assert!(lanes_of(&lc)[0].stream.is_none());
}

#[test]
fn the_lane_table_stays_sorted_by_id_across_modules() {
    let mut lc = fresh();
    register_with_lanes(&mut lc, "agent", vec![lane(9)]).unwrap();
    register_with_lanes(&mut lc, "chat", vec![lane(2)]).unwrap();
    commit(&mut lc);
    // sorted by ID, not by insertion or by owner: the host binds in id order
    // and a node that read them in a different order would still agree, but
    // the record has ONE encoding only if the order is fixed.
    let ids: Vec<u8> = lanes_of(&lc).iter().map(|l| l.id).collect();
    assert_eq!(ids, [2, 9]);
}

#[test]
fn a_second_module_cannot_take_a_declared_lane_id() {
    let mut lc = fresh();
    register_with_lanes(&mut lc, "chat", vec![lane(2)]).unwrap();
    commit(&mut lc);
    let err = register_with_lanes(&mut lc, "agent", vec![lane(2)]).unwrap_err();
    let Error::Module {
        reason,
        sentence: text,
    } = err
    else {
        panic!("expected a module error")
    };
    assert_eq!(reason, "lane_id_taken");
    assert!(
        text.contains("already declared by module chat"),
        "the refusal must name the OWNER, or an operator cannot tell who took it: {text}"
    );
}

#[test]
fn one_module_cannot_declare_two_lanes_of_one_name() {
    // the host binds by (module_id, name), so a duplicate name is a lookup
    // with two answers. A module declares its whole set in ONE op, so this is
    // where the collision has to be caught — the check reads the table as it
    // grows, not once before the loop.
    let mut lc = fresh();
    let err = register_with_lanes(
        &mut lc,
        "chat",
        vec![named_lane(2, "voice"), named_lane(3, "voice")],
    )
    .unwrap_err();
    let Error::Module {
        reason,
        sentence: text,
    } = err
    else {
        panic!("expected a module error")
    };
    assert_eq!(reason, "lane_name_taken");
    assert!(text.contains("already declares a lane named"), "{text}");
    commit(&mut lc);
    assert!(lanes_of(&lc).is_empty(), "the refused op left nothing");

    // a DIFFERENT module may use the same name: the key is the pair, and two
    // modules each having a "telemetry" lane is normal.
    register_with_lanes(&mut lc, "chat", vec![named_lane(2, "voice")]).unwrap();
    register_with_lanes(&mut lc, "agent", vec![named_lane(4, "voice")]).unwrap();
    commit(&mut lc);
    assert_eq!(lanes_of(&lc).len(), 2);
}

#[test]
fn a_malformed_lane_name_is_refused() {
    // the name is stored in the root-hashed table and read by every node, so
    // it is bounded and lowercase-ASCII — never free-form text a module could
    // grow the table with.
    let mut lc = fresh();
    let too_long = "a".repeat(MAX_LANE_NAME_BYTES + 1);
    for bad in ["", "Voice", "voice-lane", "voice lane", "보이스", &too_long] {
        let err = register_with_lanes(&mut lc, "chat", vec![named_lane(2, bad)]).unwrap_err();
        let Error::Module {
            reason,
            sentence: text,
        } = err
        else {
            panic!("expected a module error")
        };
        assert_eq!(reason, "bad_lane_name");
        assert!(text.contains("malformed"), "name {bad:?}: {text}");
    }
    // the longest legal name still lands
    let longest = "a".repeat(MAX_LANE_NAME_BYTES);
    register_with_lanes(&mut lc, "chat", vec![named_lane(2, &longest)]).unwrap();
    commit(&mut lc);
    assert_eq!(lanes_of(&lc)[0].name, longest);
}

#[test]
fn a_kernel_lane_id_is_never_declarable() {
    let mut lc = fresh();
    for id in RESERVED_LANE_IDS {
        let err = register_with_lanes(&mut lc, "squatter", vec![lane(*id)]).unwrap_err();
        let Error::Module {
            reason,
            sentence: text,
        } = err
        else {
            panic!("expected a module error")
        };
        assert_eq!(reason, "reserved_lane_id");
        assert!(text.contains("kernel lane"), "id {id}: {text}");
    }
}

#[test]
fn a_lane_id_above_the_cap_is_refused_because_the_port_ranges_would_overlap() {
    // THE REASON IS ARITHMETIC. stream = 45800 + id, datagram = 45900 + id.
    // At id 100 the stream port is 45900, which IS lane 0's datagram port, so
    // two lanes would share a socket instead of failing a bind. Six
    // compile-time variants could never reach this; a declared id can.
    assert_eq!(45800 + MAX_LANE_ID as u16 + 1, 45900);
    let mut lc = fresh();
    for bad in [0u8, MAX_LANE_ID + 1, 255] {
        let err = register_with_lanes(&mut lc, "squatter", vec![lane(bad)]).unwrap_err();
        let Error::Module {
            reason,
            sentence: text,
        } = err
        else {
            panic!("expected a module error")
        };
        assert_eq!(reason, "bad_lane_id");
        assert!(text.contains("out of range"), "id {bad}: {text}");
    }
    // the last good id still lands
    register_with_lanes(&mut lc, "edge", vec![lane(MAX_LANE_ID)]).unwrap();
    commit(&mut lc);
    assert_eq!(lanes_of(&lc)[0].id, MAX_LANE_ID);
}

#[test]
fn a_refused_lane_registers_no_module() {
    // all-or-nothing: the lane check runs BEFORE the roster write, so a module
    // whose declaration is refused must not exist at all. A half-registered
    // module would be a codeless entry the registry promises never to hold.
    let mut lc = fresh();
    register_with_lanes(&mut lc, "chat", vec![lane(2)]).unwrap();
    commit(&mut lc);
    register_with_lanes(&mut lc, "agent", vec![lane(4), lane(2)]).unwrap_err();
    commit(&mut lc);
    let ids: Vec<String> = module_status(&lc)
        .into_iter()
        .map(|m| m.module_id)
        .collect();
    assert_eq!(ids, ["chat"], "the refused module must not be rostered");
    let lanes: Vec<u8> = lanes_of(&lc).iter().map(|l| l.id).collect();
    assert_eq!(lanes, [2], "and lane 4 from the same op must not survive");
}

#[test]
fn cancelling_an_admission_frees_its_lane_ids() {
    let mut lc = fresh();
    let mut sys = ctx(Origin::System, 0);
    run(
        &mut lc,
        &mut sys,
        &msg(ModulesMsg::ScheduleRegister {
            name: "admit-chat".into(),
            module_id: "chat".into(),
            kind: Kind::Module,
            activation_height: 10,
            code_hash: hash(1),
            lanes: vec![lane(2)],
        }),
    )
    .unwrap();
    commit(&mut lc);
    assert_eq!(lanes_of(&lc).len(), 1);

    run(&mut lc, &mut sys, &cancel_swap("chat", "admit-chat")).unwrap();
    commit(&mut lc);
    assert!(
        lanes_of(&lc).is_empty(),
        "a withdrawn admission must not burn its lane id forever"
    );
    // and the id is declarable again by someone else
    register_with_lanes(&mut lc, "agent", vec![lane(2)]).unwrap();
    commit(&mut lc);
    assert_eq!(lanes_of(&lc)[0].module_id, "agent");
}

#[test]
fn a_genesis_seed_declares_its_lanes() {
    let mut lc = fresh();
    futures::executor::block_on(async {
        lc.seed(
            "chat",
            Kind::Module,
            hash(1),
            vec![LaneDecl {
                id: 2,
                name: "voice".into(),
                stream: Some(LaneStream {
                    pacing: LanePacing::Local {
                        bulk_bytes_per_sec: 24 * 1024 * 1024,
                        bulk_burst_bytes: 512 * 1024,
                    },
                    accept_backlog: 16,
                }),
            }],
        )
        .await
        .unwrap();
        lc.finish_seed().await.unwrap();
    });
    let lanes = lanes_of(&lc);
    assert_eq!(lanes.len(), 1);
    assert_eq!(lanes[0].module_id, "chat");
    assert_eq!(
        lanes[0].stream.as_ref().unwrap().pacing,
        LanePacing::Local {
            bulk_bytes_per_sec: 24 * 1024 * 1024,
            bulk_burst_bytes: 512 * 1024,
        }
    );
}
