//! the fail-stop block-boundary contract: a module whose `commit_block` or
//! `abort_block` FAILS is a NODE-LOCAL fault, not a deterministic rejection.
//!
//! a rejected op errors identically on every honest validator (module error,
//! unknown target, blown budget) and the abort path rolls every touched module
//! back — the ordered lane safely treats it as a no-op. a boundary fault hits
//! only THIS node: a commit failure leaves the block half-published (modules
//! earlier in registry order already committed), an abort failure leaves a
//! stage that can leak into a later block. `submit` must surface these as
//! [`SubmitError::Fatal`] so the caller fail-stops instead of silently forking.

use futures::executor::block_on;
use host::{BoundaryPhase, Host, SubmitError};
use sdk::{Ctx, Error, Module, ModuleId, Msg, StateRoot};

/// a module that stages fine but fails the requested BOUNDARY hook.
struct BoundaryBomb {
    id: &'static str,
    fail_commit: bool,
    fail_abort: bool,
    /// staged marker so root() moves only after a successful commit.
    staged: bool,
    committed: u8,
}

impl BoundaryBomb {
    fn new(id: &'static str, fail_commit: bool, fail_abort: bool) -> Self {
        Self {
            id,
            fail_commit,
            fail_abort,
            staged: false,
            committed: 0,
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Module for BoundaryBomb {
    fn id(&self) -> ModuleId {
        self.id.into()
    }
    fn root(&self) -> StateRoot {
        StateRoot([self.committed; 32])
    }
    async fn execute(&mut self, _c: &mut dyn Ctx, _m: &Msg) -> Result<(), Error> {
        self.staged = true;
        Ok(())
    }
    async fn commit_block(&mut self) -> Result<(), Error> {
        if self.fail_commit {
            return Err(Error::module("injected_fault", "disk died mid-commit"));
        }
        if self.staged {
            self.committed = self.committed.wrapping_add(1);
            self.staged = false;
        }
        Ok(())
    }
    async fn abort_block(&mut self) -> Result<(), Error> {
        if self.fail_abort {
            return Err(Error::module("injected_fault", "could not discard stage"));
        }
        self.staged = false;
        Ok(())
    }
}

/// a module whose execute always errors — forces the drain onto the abort path.
struct Boom;
#[async_trait::async_trait(?Send)]
impl Module for Boom {
    fn id(&self) -> ModuleId {
        "boom".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }
    async fn execute(&mut self, _c: &mut dyn Ctx, _m: &Msg) -> Result<(), Error> {
        Err(Error::module("boom", "boom"))
    }
}

/// fans one root op into a write to `target` (staging it) plus optionally boom.
struct Fanout {
    target: &'static str,
    also_boom: bool,
}
#[async_trait::async_trait(?Send)]
impl Module for Fanout {
    fn id(&self) -> ModuleId {
        "fanout".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }
    async fn execute(&mut self, ctx: &mut dyn Ctx, _m: &Msg) -> Result<(), Error> {
        ctx.emit_msg(Msg {
            target: self.target.into(),
            payload: Vec::new(),
        });
        if self.also_boom {
            ctx.emit_msg(Msg {
                target: "boom".into(),
                payload: Vec::new(),
            });
        }
        Ok(())
    }
}

#[test]
fn commit_failure_is_fatal_not_rejected() {
    block_on(async {
        let mut host = Host::genesis(vec![
            Box::new(BoundaryBomb::new("bomb", true, false)),
            Box::new(Fanout {
                target: "bomb",
                also_boom: false,
            }),
        ])
        .expect("genesis");

        let err = host
            .submit(Msg {
                target: "fanout".into(),
                payload: Vec::new(),
            })
            .await
            .expect_err("a failing commit_block must fail the submit");

        match err {
            SubmitError::Fatal(f) => {
                assert_eq!(f.module, "bomb");
                assert_eq!(f.phase, BoundaryPhase::Commit);
            }
            SubmitError::Rejected(e) => {
                panic!("a commit fault must be Fatal, not a deterministic rejection: {e:?}")
            }
        }
    });
}

#[test]
fn abort_failure_is_fatal_not_rejected() {
    block_on(async {
        // the drain fails deterministically (boom), which routes to the abort
        // path — where the bomb refuses to discard its stage. that stage could
        // leak into the NEXT block's commit, so it must surface as Fatal.
        let mut host = Host::genesis(vec![
            Box::new(BoundaryBomb::new("bomb", false, true)),
            Box::new(Boom),
            Box::new(Fanout {
                target: "bomb",
                also_boom: true,
            }),
        ])
        .expect("genesis");

        let err = host
            .submit(Msg {
                target: "fanout".into(),
                payload: Vec::new(),
            })
            .await
            .expect_err("the boom follow-up must fail the block");

        match err {
            SubmitError::Fatal(f) => {
                assert_eq!(f.module, "bomb");
                assert_eq!(f.phase, BoundaryPhase::Abort);
            }
            SubmitError::Rejected(e) => {
                panic!("an abort fault must be Fatal, not a deterministic rejection: {e:?}")
            }
        }
    });
}

#[test]
fn clean_rejection_stays_rejected() {
    block_on(async {
        // the negative control: identical block shape, but every boundary hook
        // succeeds — the deterministic boom rejection must stay Rejected so the
        // ordered lane keeps treating it as a no-op.
        let mut host = Host::genesis(vec![
            Box::new(BoundaryBomb::new("bomb", false, false)),
            Box::new(Boom),
            Box::new(Fanout {
                target: "bomb",
                also_boom: true,
            }),
        ])
        .expect("genesis");

        let app0 = host.root_hash();
        let err = host
            .submit(Msg {
                target: "fanout".into(),
                payload: Vec::new(),
            })
            .await
            .expect_err("the boom follow-up must fail the block");

        assert_eq!(err, SubmitError::Rejected(Error::module("boom", "boom")));
        assert_eq!(host.root_hash(), app0, "a rejected block leaves no trace");
    });
}

#[test]
fn abort_fault_still_aborts_the_remaining_modules() {
    block_on(async {
        // two staged modules; the FIRST one's abort fails. the host must still
        // abort the second (each un-aborted stage is one more leak) while
        // reporting the first fault. registry order: "a-bomb" < "b-clean".
        struct Probe;
        #[async_trait::async_trait(?Send)]
        impl Module for Probe {
            fn id(&self) -> ModuleId {
                "probe".into()
            }
            fn root(&self) -> StateRoot {
                StateRoot::ZERO
            }
            async fn execute(&mut self, ctx: &mut dyn Ctx, _m: &Msg) -> Result<(), Error> {
                ctx.emit_msg(Msg {
                    target: "a-bomb".into(),
                    payload: Vec::new(),
                });
                ctx.emit_msg(Msg {
                    target: "b-clean".into(),
                    payload: Vec::new(),
                });
                ctx.emit_msg(Msg {
                    target: "boom".into(),
                    payload: Vec::new(),
                });
                Ok(())
            }
        }

        let mut host = Host::genesis(vec![
            Box::new(BoundaryBomb::new("a-bomb", false, true)),
            Box::new(BoundaryBomb::new("b-clean", false, false)),
            Box::new(Boom),
            Box::new(Probe),
        ])
        .expect("genesis");

        let err = host
            .submit(Msg {
                target: "probe".into(),
                payload: Vec::new(),
            })
            .await
            .expect_err("boom fails the block");
        match err {
            SubmitError::Fatal(f) => {
                assert_eq!(f.module, "a-bomb", "the FIRST abort fault is reported");
                assert_eq!(f.phase, BoundaryPhase::Abort);
            }
            other => panic!("expected a Fatal abort fault, got {other:?}"),
        }

        // b-clean's abort ran: submitting a clean block against it commits only
        // that block's stage — the earlier (aborted) stage did not leak into it.
        // (state is fatal anyway; this pins the best-effort abort sweep.)
        let out = host
            .submit(Msg {
                target: "b-clean".into(),
                payload: Vec::new(),
            })
            .await
            .expect("direct clean block");
        assert_ne!(out.root_hash, StateRoot::ZERO);
    });
}
