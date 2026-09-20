//! end-to-end proof of the host/dispatch skeleton on the deterministic runtime.
//!
//! two modules are registered: a real qmdb-backed `kv`, and a tiny `relay` whose
//! `execute` emits a follow-up [`Msg`] targeting `kv` (a cross-module
//! self-trigger). one message is submitted to `relay`; after `submit` returns we
//! prove the follow-up was actually dispatched into `kv` (its root moved), that
//! the root-hash moved and is recompute-stable, and that the drain terminated.
//! a second, independent deterministic run must yield the byte-identical
//! root-hash.

use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use host::{DispatchRecord, Host};
use kv::Kv;
use sdk::{Ctx, Error, Module, ModuleId, Msg, Origin, StateRoot};
use statesync::qmdb::QmdbStore;

const KV_ID: &str = "kv";
const RELAY_ID: &str = "relay";
const WRITE_KEY: &[u8] = b"from-relay";
const WRITE_VAL: &[u8] = b"landed";

/// a module that holds no state (root is constant) and, on dispatch, emits a
/// single follow-up write to the kv module — proving cross-module re-entry.
struct Relay;

#[async_trait::async_trait(?Send)]
impl Module for Relay {
    fn id(&self) -> ModuleId {
        RELAY_ID.to_string()
    }
    // constant root: the ONLY thing that can move the root-hash is kv, so an
    // root-hash delta is attributable to the follow-up landing in kv.
    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }
    async fn execute(&mut self, ctx: &mut dyn Ctx, _msg: &Msg) -> Result<(), Error> {
        // NOT a reentrant call into kv — an intent the host re-dispatches as a
        // follow-up op after this execute returns.
        // typed follow-up write via kv's interface crate (not an ad-hoc encoding).
        let payload = kv::encode(&kv::KvMsg::Set {
            key: WRITE_KEY.to_vec(),
            value: WRITE_VAL.to_vec(),
        });
        ctx.emit_msg(Msg {
            target: KV_ID.to_string(),
            payload,
        });
        Ok(())
    }
}

/// build a fresh host (kv + relay) on `context`, submit one message to relay, and
/// return the resulting root-hash. asserts the cross-module follow-up landed.
async fn run_block(context: deterministic::Context) -> StateRoot {
    let mut host = Host::new();
    let kv = Kv::new(
        KV_ID,
        Box::new(QmdbStore::init(context.child(KV_ID), KV_ID).await),
    );
    host.register(Box::new(kv));
    host.register(Box::new(Relay));

    let kv_root_before = host.module_root(KV_ID).unwrap();
    let app_before = host.root_hash();

    // submit to RELAY, not kv. any kv change therefore came via the follow-up.
    let outcome = host
        .submit(Msg {
            target: RELAY_ID.to_string(),
            payload: Vec::new(),
        })
        .await
        .expect("submit must terminate and succeed");

    let kv_root_after = host.module_root(KV_ID).unwrap();

    // (a) the follow-up was processed, not dropped: kv's real qmdb root moved
    //     even though we submitted to relay.
    assert_ne!(
        kv_root_before, kv_root_after,
        "kv root must move — the relay's follow-up write must reach kv"
    );
    assert_ne!(
        kv_root_after,
        StateRoot::ZERO,
        "kv root after write is real"
    );

    // (b) the root-hash moved, and recomputing it is identical (idempotent over
    //     the settled registry).
    assert_ne!(
        app_before, outcome.root_hash,
        "root-hash must move after the write"
    );
    assert_eq!(
        outcome.root_hash,
        host.root_hash(),
        "root-hash must be recompute-stable"
    );

    // (c) termination: submit returned Ok.
    outcome.root_hash
}

#[test]
fn relay_follow_up_reaches_kv_and_moves_root_hash() {
    deterministic::Runner::default().start(|context| async move {
        run_block(context).await;
    });
}

#[test]
fn root_hash_is_deterministic_across_runs() {
    let a =
        deterministic::Runner::default().start(|context| async move { run_block(context).await });
    let b =
        deterministic::Runner::default().start(|context| async move { run_block(context).await });
    assert_eq!(
        a, b,
        "same submit on a fresh fixed-seed host -> identical root-hash"
    );
}

#[test]
fn unknown_target_is_rejected_without_corrupting_the_registry() {
    deterministic::Runner::default().start(|context| async move {
        let mut host = Host::new();
        let kv = Kv::new(
            KV_ID,
            Box::new(QmdbStore::init(context.child(KV_ID), KV_ID).await),
        );
        host.register(Box::new(kv));

        let err = host
            .submit(Msg {
                target: "ghost".to_string(),
                payload: Vec::new(),
            })
            .await
            .expect_err("unknown target must error");
        assert_eq!(
            err,
            host::SubmitError::Rejected(Error::UnknownModule("ghost".to_string()))
        );

        // registry intact: kv is still routable.
        assert!(host.module_root(KV_ID).is_some());
    });
}

#[test]
fn dispatch_trace_records_every_dispatch_in_causal_order() {
    deterministic::Runner::default().start(|context| async move {
        let mut host = Host::new();
        let kv = Kv::new(
            KV_ID,
            Box::new(QmdbStore::init(context.child(KV_ID), KV_ID).await),
        );
        host.register(Box::new(kv));
        host.register(Box::new(Relay));

        let outcome = host
            .submit(Msg {
                target: RELAY_ID.to_string(),
                payload: Vec::new(),
            })
            .await
            .expect("submit succeeds");

        // the deterministic trace: the root op (relay) then the follow-up it
        // emitted (kv), in drain order — each tagged with its trigger origin,
        // the op bytes it applied, and its intent fan-out. `submit` uses the
        // default context, so the root op's origin is an empty External.
        assert_eq!(
            outcome.dispatches,
            vec![
                DispatchRecord {
                    module: RELAY_ID.to_string(),
                    origin: Origin::External(Vec::new()),
                    payload: Vec::new(),
                    emitted_msgs: 1,
                    emitted_events: 0,
                    assigned: Vec::new(),
                    cause: sdk::Cause::Direct,
                    output: None,
                },
                DispatchRecord {
                    module: KV_ID.to_string(),
                    origin: Origin::Module(RELAY_ID.to_string()),
                    payload: kv::encode(&kv::KvMsg::Set {
                        key: WRITE_KEY.to_vec(),
                        value: WRITE_VAL.to_vec(),
                    }),
                    emitted_msgs: 0,
                    emitted_events: 0,
                    assigned: Vec::new(),
                    cause: sdk::Cause::Direct,
                    output: None,
                },
            ],
        );
    });
}

#[test]
fn root_hash_is_schedule_independent() {
    // the real consensus property: the root-hash must NOT depend on task
    // scheduling. each seed drives a different deterministic schedule; every node
    // (modeled here as a seed) must agree on the same root-hash.
    let roots: Vec<StateRoot> = (0..16u64)
        .map(|s| {
            deterministic::Runner::seeded(s)
                .start(|context| async move { run_block(context).await })
        })
        .collect();
    assert!(
        roots.windows(2).all(|w| w[0] == w[1]),
        "root-hash must be identical across schedules: {roots:?}"
    );
}
