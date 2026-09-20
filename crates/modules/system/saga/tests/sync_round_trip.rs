//! state-sync round trip over the REAL store: a joiner reconstructs a
//! byte-identical qmdb root by pulling the source store's operation range
//! through commonware's qmdb sync, then wraps a fresh `SagaModule` around the
//! injected store — the sync lane that REPLACED this module's byte snapshot.
//!
//! the source drives ops through the real module so the op log is what a
//! validator produces, and it deliberately carries every shape a naive "export
//! live records and re-apply sorted" could not reproduce:
//!
//! * one committed saga in EVERY status (`Pending`, `Done`, `Failed`,
//!   `TimedOut`, `Cancelled`) across all three origin shapes,
//! * record OVERWRITES (a retry, a lease grant, a terminal transition),
//! * a CHUNKED spec — over [`saga::SPEC_CHUNK_BYTES`], so the saga spans
//!   several store keys the joiner has to reassemble,
//! * record DELETES: an explicit `Prune`, and the retention trim EVICTING a
//!   receipt when a block crosses [`saga::MAX_RETAINED_TERMINAL`],
//! * and the sentinel indexes that ride the same root and are the only reason
//!   the store-backed module can enumerate at all: `terminal`, and the
//!   hash-SHARDED `pending` — the capful of sagas below writes rows into many
//!   of its shard records and clears them again inside ONE block.
//!
//! a `SagaModule` consumes its injected store, so the handoff-as-resolver form
//! is only reachable on the raw store: REOPEN the committed partitions under
//! the same id (exactly the recovery path a restarting node takes — the
//! deterministic runtime shares storage across child contexts).

use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use saga::{
    MAX_RETAINED_TERMINAL, SPEC_CHUNK_BYTES, SagaModule, SagaMsg, SagaOrigin, SagaQuery, SagaReply,
    SagaStatus, SagaView, decode_reply, encode_msg, encode_query,
};
use sdk::{Env, Error, MerkleStore as _, Module, Msg, Origin, StateRoot, StateSyncHandle};
use sdk_testkit::TestCtx;
use statesync::qmdb::QmdbStore;
use valset_module::{ValsetReply, encode_reply as valset_encode_reply};

const SAGA: &str = "saga";

/// the canned validator set saga's lease-holder pool resolves against — served
/// for whichever assigned target (valset/capability) saga queries.
fn validators_query(_req: &[u8]) -> Result<Vec<u8>, Error> {
    Ok(valset_encode_reply(&ValsetReply::Validators(vec![
        vec![7u8; 32],
        vec![9u8; 32],
    ])))
}

/// the ctx a host hands saga: env at `height`, `me = "saga"`, the "agent"
/// module live via `module_root` (reply_to validation gates on it), and the
/// canned validator set.
fn ctx(height: u64, origin: Origin) -> TestCtx {
    TestCtx::with_env(Env {
        height,
        consensus_time: height,
        origin,
        me: SAGA.into(),
        cause: sdk::Cause::Direct,
    })
    .with_module_root("agent", StateRoot::ZERO)
    .on_query("valset", validators_query)
    .on_query("capability", validators_query)
}

/// drive one op through the REAL module path, WITHOUT committing — the caller
/// closes the block, so a multi-op block exercises the staged overlay too.
async fn exec(m: &mut SagaModule, height: u64, origin: Origin, op: &SagaMsg) {
    let msg = Msg {
        target: SAGA.into(),
        payload: encode_msg(op),
    };
    m.execute(&mut ctx(height, origin), &msg).await.unwrap();
}

async fn get(m: &SagaModule, id: &str) -> Option<SagaView> {
    let reply = m
        .query(&encode_query(&SagaQuery::Get { saga_id: id.into() }))
        .await
        .unwrap();
    match decode_reply(&reply).unwrap() {
        SagaReply::Saga(v) => v,
        other => panic!("expected Saga reply, got {other:?}"),
    }
}

/// the WHOLE announcement projection, walked page by page — the read is
/// bounded per call, so "every announcement" is the caller's loop.
async fn unassigned(m: &SagaModule) -> Vec<String> {
    let mut ids = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let reply = m
            .query(&encode_query(&SagaQuery::UnassignedPending {
                after: after.clone(),
            }))
            .await
            .unwrap();
        let page = match decode_reply(&reply).unwrap() {
            SagaReply::UnassignedPending(page) => page,
            other => panic!("expected UnassignedPending reply, got {other:?}"),
        };
        ids.extend(page.requests.into_iter().map(|r| r.saga_id));
        match page.next {
            Some(cursor) => after = Some(cursor),
            None => return ids,
        }
    }
}

/// saga's id space is namespaced per trigger origin, so every fixture id is
/// built from the origin that triggers it — and no other origin could have.
fn alice_id(id: &str) -> String {
    saga::namespaced_id(&Origin::External(b"alice".to_vec()), id)
}
fn agent_id(id: &str) -> String {
    saga::namespaced_id(&Origin::Module("agent".into()), id)
}
fn system_id(id: &str) -> String {
    saga::namespaced_id(&Origin::System, id)
}

fn trigger(id: &str, reply_to: Option<&str>, max_attempts: u32, deadline: Option<u64>) -> SagaMsg {
    SagaMsg::Trigger {
        pinned_assignee: None,
        saga_id: id.into(),
        spec: format!("spec:{id}").into_bytes(),
        reply_to: reply_to.map(String::from),
        reply_payload: format!("corr:{id}").into_bytes(),
        deadline,
        max_attempts,
        lease_views: Some(4),
        capability: None,
        demands: Default::default(),
    }
}

fn oracle(id: &str, attempt: u32, outcome: Result<Vec<u8>, String>) -> SagaMsg {
    SagaMsg::OracleResult {
        saga_id: id.into(),
        attempt,
        outcome,
        usage: None,
    }
}

/// a spec that does not fit one store record — the joiner has to carry every
/// chunk key for the `Get` projection to answer identically.
fn chunked_spec() -> Vec<u8> {
    (0..SPEC_CHUNK_BYTES + 1).map(|i| i as u8).collect()
}

#[test]
fn synced_store_reconstructs_source_root_and_every_read() {
    deterministic::Runner::default().start(|context| async move {
        let mut src = SagaModule::with_assignment(
            SAGA,
            Box::new(QmdbStore::init(context.child("src"), "src").await),
            "valset",
            "capability",
            saga::LeasePolicy::Open,
        );
        let alice = Origin::External(b"alice".to_vec());
        let oracle_origin = Origin::External(b"oracle".to_vec());

        // block 1 — one saga per status-to-be, across all three origin shapes,
        // plus the chunked-spec saga and a capability-tagged one.
        exec(
            &mut src,
            1,
            alice.clone(),
            &trigger(&alice_id("s-cancelled"), None, 1, None),
        )
        .await;
        exec(
            &mut src,
            1,
            Origin::Module("agent".into()),
            &trigger(&agent_id("s-done"), Some("agent"), 1, None),
        )
        .await;
        exec(
            &mut src,
            1,
            alice.clone(),
            &trigger(&alice_id("s-failed"), Some("agent"), 2, Some(50)),
        )
        .await;
        exec(
            &mut src,
            1,
            Origin::System,
            &trigger(&system_id("s-pending"), None, 3, Some(90)),
        )
        .await;
        exec(
            &mut src,
            1,
            alice.clone(),
            &trigger(&alice_id("s-timedout"), None, 1, Some(2)),
        )
        .await;
        exec(
            &mut src,
            1,
            alice.clone(),
            &SagaMsg::Trigger {
                pinned_assignee: None,
                saga_id: alice_id("s-chunked"),
                spec: chunked_spec(),
                reply_to: None,
                reply_payload: Vec::new(),
                deadline: None,
                max_attempts: 1,
                // no capability registry answers this tag, so the attempt
                // assigns nobody — an ANNOUNCEMENT the joiner must still see.
                lease_views: None,
                capability: Some("alpha".into()),
                demands: Default::default(),
            },
        )
        .await;
        src.commit_block().await.unwrap();

        // block 2 — terminal transitions and a retry (record OVERWRITES).
        exec(
            &mut src,
            2,
            oracle_origin.clone(),
            &oracle(&agent_id("s-done"), 0, Ok(b"agreed-answer".to_vec())),
        )
        .await;
        exec(
            &mut src,
            2,
            oracle_origin.clone(),
            &oracle(&alice_id("s-failed"), 0, Err("first worker crashed".into())),
        )
        .await;
        exec(
            &mut src,
            2,
            alice.clone(),
            &SagaMsg::Cancel {
                saga_id: alice_id("s-cancelled"),
            },
        )
        .await;
        src.commit_block().await.unwrap();

        // block 3 — the second attempt fails too (terminal Failed with a
        // stored error), then a crank times the past-deadline saga out.
        exec(
            &mut src,
            3,
            oracle_origin.clone(),
            &oracle(
                &alice_id("s-failed"),
                1,
                Err("second worker crashed".into()),
            ),
        )
        .await;
        src.commit_block().await.unwrap();
        exec(
            &mut src,
            5,
            Origin::External(b"cranker".to_vec()),
            &SagaMsg::Crank {},
        )
        .await;
        src.commit_block().await.unwrap();

        // block 4 — an explicit Prune: a record DELETE in the op log.
        exec(
            &mut src,
            6,
            alice.clone(),
            &SagaMsg::Prune {
                saga_ids: vec![alice_id("s-cancelled")],
            },
        )
        .await;
        src.commit_block().await.unwrap();

        // block 5 — a capful of settled sagas in ONE block, which crosses the
        // retention cap and EVICTS the oldest receipt (the second delete
        // shape, and the one the joiner must not resurrect).
        // three receipts are already retained (s-done, s-failed, s-timedout —
        // s-cancelled was pruned), so a capful minus two overshoots the cap by
        // exactly one and the OLDEST receipt is the one that goes.
        let evicted = agent_id("s-done");
        for i in 0..MAX_RETAINED_TERMINAL - 2 {
            let id = system_id(&format!("bulk{i:04}"));
            exec(&mut src, 7, Origin::System, &trigger(&id, None, 1, None)).await;
            exec(
                &mut src,
                7,
                oracle_origin.clone(),
                &oracle(&id, 0, Ok(b"r".to_vec())),
            )
            .await;
        }
        src.commit_block().await.unwrap();
        assert_eq!(
            get(&src, &evicted).await,
            None,
            "the block crossed the retention cap and dropped the oldest receipt"
        );

        // the module is resolver-backed: there is NO byte snapshot to ship.
        match src.state_sync_handle().expect("handle") {
            StateSyncHandle::ResolverBacked { backend, .. } => assert_eq!(backend, "qmdb"),
            other => panic!("expected ResolverBacked, got {other:?}"),
        }
        assert!(
            src.snapshot_bytes().is_none(),
            "a store-backed module ships no byte snapshot"
        );
        let src_root = src.root();
        assert_ne!(src_root, StateRoot::ZERO, "source must have a real root");

        // the whole read matrix, captured off the source.
        let ids = [
            system_id("s-pending"),
            alice_id("s-failed"),
            alice_id("s-timedout"),
            alice_id("s-chunked"),
            system_id("bulk0000"),
        ];
        let mut views = Vec::new();
        for id in &ids {
            views.push(get(&src, id).await);
        }
        let src_unassigned = unassigned(&src).await;

        // the source really covers the status and field space.
        let pending = views[0].clone().expect("s-pending survives");
        assert_eq!(pending.status, SagaStatus::Pending);
        assert_eq!(pending.origin, SagaOrigin::System);
        assert!(
            pending.assignee.is_some(),
            "the valset assigned a lease holder"
        );
        assert!(pending.lease_expires_at.is_some());
        let failed = views[1].clone().expect("s-failed survives");
        assert_eq!(failed.status, SagaStatus::Failed);
        assert_eq!(failed.attempt, 1, "the failed saga consumed both attempts");
        assert_eq!(failed.error, Some("second worker crashed".to_string()));
        assert_eq!(
            views[2].as_ref().map(|v| v.status),
            Some(SagaStatus::TimedOut)
        );
        assert_eq!(
            views[3].as_ref().map(|v| v.spec.len()),
            Some(SPEC_CHUNK_BYTES + 1),
            "the chunked spec reads back whole"
        );

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
        let synced = SagaModule::with_assignment(
            SAGA,
            Box::new(store),
            "valset",
            "capability",
            saga::LeasePolicy::Open,
        );

        // THE PROPERTY: identical qmdb root — the root-hash linkage a joiner
        // needs at the boundary height.
        assert_eq!(
            synced.root(),
            src_root,
            "synced store root must equal the source root"
        );

        // and every read answers exactly like the source, including the
        // reassembled chunked spec and the enumeration that only the sentinel
        // indexes can answer.
        for (id, expected) in ids.iter().zip(&views) {
            assert_eq!(&get(&synced, id).await, expected, "read parity for {id}");
        }
        assert_eq!(unassigned(&synced).await, src_unassigned);
        assert!(
            src_unassigned.contains(&alice_id("s-chunked")),
            "the announcement rode the pending index: {src_unassigned:?}"
        );
        assert_eq!(
            get(&synced, &alice_id("s-cancelled")).await,
            None,
            "the pruned saga is genuinely gone"
        );
        assert_eq!(
            get(&synced, &evicted).await,
            None,
            "the evicted receipt is genuinely gone"
        );
    });
}
