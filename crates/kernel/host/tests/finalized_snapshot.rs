//! finalized snapshot capture: the host can expose the registry roots and
//! per-module state-sync handles for the exact root-hash a finalized block
//! produced. capture must not serve stale heights after the registry advances,
//! and it must not expose staged writes from an aborted block. one module that
//! cannot produce a handle is reported as degraded, never as a failed capture:
//! this call feeds BOTH recovery checkpointing and serving a joiner, so letting
//! a single module abort it stopped the whole node from doing either.

use commonware_runtime::{Runner as _, deterministic};
use host::{BlockContext, CapturePayloads, FinalizedBlock, Host, SnapshotError};
use sdk::{Ctx, Error, Module, ModuleId, Msg, Origin, StateRoot, StateSyncHandle};

const BYTES_ID: &str = "bytes";
const RESOLVER_ID: &str = "resolver";
const DEGRADED_ID: &str = "degraded";
const SECOND_DEGRADED_ID: &str = "degraded2";

struct BytesModule {
    committed: u8,
    staged: Option<u8>,
}

impl BytesModule {
    fn new(committed: u8) -> Self {
        Self {
            committed,
            staged: None,
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Module for BytesModule {
    fn id(&self) -> ModuleId {
        BYTES_ID.into()
    }

    fn root(&self) -> StateRoot {
        StateRoot([self.committed; sdk::ROOT_LEN])
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        Ok(StateSyncHandle::SnapshotBytes(vec![self.committed]))
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let Some((&next, rest)) = msg.payload.split_first() else {
            return Err(Error::module("injected_fault", "missing staged byte"));
        };
        self.staged = Some(next);
        if rest == *b"!" {
            ctx.emit_msg(Msg {
                target: "missing".into(),
                payload: Vec::new(),
            });
        }
        Ok(())
    }

    async fn commit_block(&mut self) -> Result<(), Error> {
        if let Some(next) = self.staged.take() {
            self.committed = next;
        }
        Ok(())
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged = None;
        Ok(())
    }
}

/// a module whose committed state cannot be turned into a sync surface — the
/// forge shape: a head it accepted names objects it does not hold, so
/// `snapshot()` errors forever while `root()` stays perfectly well defined.
struct DegradedBytesModule;

#[async_trait::async_trait(?Send)]
impl Module for DegradedBytesModule {
    fn id(&self) -> ModuleId {
        DEGRADED_ID.into()
    }

    fn root(&self) -> StateRoot {
        StateRoot([3u8; sdk::ROOT_LEN])
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        Err(Error::module(
            "injected_fault",
            "missing pack for committed head",
        ))
    }

    async fn execute(&mut self, _ctx: &mut dyn Ctx, _msg: &Msg) -> Result<(), Error> {
        Ok(())
    }
}

struct SecondDegradedModule;

#[async_trait::async_trait(?Send)]
impl Module for SecondDegradedModule {
    fn id(&self) -> ModuleId {
        SECOND_DEGRADED_ID.into()
    }

    fn root(&self) -> StateRoot {
        StateRoot([4u8; sdk::ROOT_LEN])
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        Err(Error::module("injected_fault", "also broken"))
    }

    async fn execute(&mut self, _ctx: &mut dyn Ctx, _msg: &Msg) -> Result<(), Error> {
        Ok(())
    }
}

struct ResolverBackedModule;

#[async_trait::async_trait(?Send)]
impl Module for ResolverBackedModule {
    fn id(&self) -> ModuleId {
        RESOLVER_ID.into()
    }

    fn root(&self) -> StateRoot {
        StateRoot([2u8; sdk::ROOT_LEN])
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        Ok(StateSyncHandle::ResolverBacked {
            backend: "qmdb".into(),
            detail: "requires a manifest-pinned sync target plus a sync Source".into(),
        })
    }

    async fn execute(&mut self, _ctx: &mut dyn Ctx, _msg: &Msg) -> Result<(), Error> {
        Ok(())
    }
}

fn block(height: u64, root_hash: StateRoot) -> FinalizedBlock {
    FinalizedBlock { height, root_hash }
}

fn ctx(height: u64) -> BlockContext {
    BlockContext {
        height,
        consensus_time: height * 10,
        origin: Origin::System,
    }
}

#[test]
fn snapshot_capture_uses_the_finalized_root_hash_boundary() {
    deterministic::Runner::default().start(|_| async move {
        let mut host = Host::genesis(vec![
            Box::new(BytesModule::new(1)),
            Box::new(ResolverBackedModule),
        ])
        .expect("genesis");
        let start_hash = host.root_hash();

        let err = host
            .submit_at(
                ctx(7),
                Msg {
                    target: BYTES_ID.into(),
                    payload: vec![9, b'!'],
                },
            )
            .await
            .expect_err("unknown follow-up aborts the block");
        assert_eq!(err, host::SubmitError::Rejected(Error::UnknownModule("missing".into())));

        let after_abort = host
            .capture_finalized_snapshot(block(6, start_hash))
            .expect("unchanged root-hash can still be served");
        let bytes = after_abort.module(BYTES_ID).expect("bytes module");
        assert_eq!(bytes.root, StateRoot([1u8; sdk::ROOT_LEN]));
        assert_eq!(
            bytes.state_sync,
            StateSyncHandle::SnapshotBytes(vec![1]),
            "aborted staged byte must not leak into the snapshot handle",
        );

        let committed = host
            .submit_at(
                ctx(8),
                Msg {
                    target: BYTES_ID.into(),
                    payload: vec![7],
                },
            )
            .await
            .expect("committed block");

        let snapshot = host
            .capture_finalized_snapshot(block(8, committed.root_hash))
            .expect("current finalized root-hash must capture");
        assert_eq!(snapshot.height, 8);
        assert_eq!(snapshot.root_hash, committed.root_hash);

        let bytes = snapshot.module(BYTES_ID).expect("bytes module");
        assert_eq!(bytes.root, StateRoot([7u8; sdk::ROOT_LEN]));
        assert_eq!(bytes.state_sync, StateSyncHandle::SnapshotBytes(vec![7]));

        let resolver = snapshot.module(RESOLVER_ID).expect("resolver module");
        assert_eq!(resolver.root, StateRoot([2u8; sdk::ROOT_LEN]));
        assert_eq!(
            resolver.state_sync,
            StateSyncHandle::ResolverBacked {
                backend: "qmdb".into(),
                detail: "requires a manifest-pinned sync target plus a sync Source".into(),
            },
        );
        assert!(
            !snapshot.has_all_snapshot_bytes(),
            "resolver-backed modules must prevent this from being advertised as a full byte snapshot",
        );

        let moved = host
            .submit_at(
                ctx(9),
                Msg {
                    target: BYTES_ID.into(),
                    payload: vec![8],
                },
            )
            .await
            .expect("second committed block");
        let stale = host
            .capture_finalized_snapshot(block(8, committed.root_hash))
            .expect_err("old boundary must not be served from new registry state");
        assert_eq!(
            stale,
            SnapshotError::RootHashMismatch {
                expected: committed.root_hash,
                actual: moved.root_hash,
            },
        );
    });
}

#[test]
fn one_module_that_cannot_snapshot_does_not_abort_the_capture() {
    deterministic::Runner::default().start(|_| async move {
        let host = Host::genesis(vec![
            Box::new(BytesModule::new(1)),
            Box::new(DegradedBytesModule),
            Box::new(ResolverBackedModule),
        ])
        .expect("genesis");

        let snapshot = host
            .capture_finalized_snapshot(block(4, host.root_hash()))
            .expect("a module's own bad state must not take the boundary down");

        // the healthy modules are captured in full — the amplifier was that
        // ONE `?` discarded every one of these.
        assert_eq!(
            snapshot.module(BYTES_ID).expect("bytes module").state_sync,
            StateSyncHandle::SnapshotBytes(vec![1]),
        );
        assert!(snapshot.module(RESOLVER_ID).is_some());
        assert!(
            snapshot.module(DEGRADED_ID).is_none(),
            "a degraded module has no sync surface, so it is not an entry",
        );

        // and the failure is named, with the root it still has.
        assert_eq!(snapshot.degraded.len(), 1);
        let degraded = &snapshot.degraded[0];
        assert_eq!(degraded.id, DEGRADED_ID);
        assert_eq!(degraded.root, StateRoot([3u8; sdk::ROOT_LEN]));
        assert_eq!(
            degraded.reason,
            Error::module("injected_fault", "missing pack for committed head"),
        );

        assert!(!snapshot.has_all_snapshot_bytes());
        assert!(
            !snapshot.is_self_contained(),
            "a degraded module cannot be rebuilt from the capture at all",
        );
    });
}

#[test]
fn every_degraded_module_is_reported_not_just_the_first() {
    deterministic::Runner::default().start(|_| async move {
        let host = Host::genesis(vec![
            Box::new(DegradedBytesModule),
            Box::new(SecondDegradedModule),
        ])
        .expect("genesis");

        let snapshot = host
            .capture_finalized_snapshot(block(1, host.root_hash()))
            .expect("capture");

        let mut ids: Vec<_> = snapshot.degraded.iter().map(|m| m.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec![DEGRADED_ID, SECOND_DEGRADED_ID],
            "collecting into Result stopped at the first failure in registry order",
        );
        assert!(snapshot.modules.is_empty());
    });
}

#[test]
fn the_capture_cost_breakdown_covers_every_registered_module() {
    deterministic::Runner::default().start(|_| async move {
        let host = Host::genesis(vec![
            Box::new(BytesModule::new(1)),
            Box::new(DegradedBytesModule),
            Box::new(ResolverBackedModule),
        ])
        .expect("genesis");

        // the caller's clock: one millisecond per reading, so each module's
        // cost is attributable and a per-module measurement is distinguishable
        // from one aggregate stamped onto everybody.
        let mut tick = std::time::Duration::ZERO;
        let (_snapshot, capture_cost) =
            host.capture_current_snapshot(4, CapturePayloads::All, || {
                tick += std::time::Duration::from_millis(1);
                tick
            });

        let mut billed: Vec<&str> = capture_cost.iter().map(|(id, _)| id.as_str()).collect();
        billed.sort_unstable();
        assert_eq!(
            billed,
            vec![BYTES_ID, DEGRADED_ID, RESOLVER_ID],
            "every registered module is billed, degraded included — an absent \
             module is exactly the one a slow capture would be blamed on",
        );
        for (id, spent) in &capture_cost {
            assert_eq!(
                *spent,
                std::time::Duration::from_millis(2),
                "{id} must be billed its OWN root + handle readings",
            );
        }
    });
}
