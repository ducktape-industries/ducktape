//! Independent regressions for authority changes between queue admission and
//! execution, and the single-submit rejection contract with internal work.

use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use dispatch::{DispatchModule, DispatchMsg, DispatchQuery, DispatchReply, Refusal};
use futures::executor::block_on;
use host::{BlockContext, CallDisposition, Host, MemberOutcome, SubmitError};
use identity::{Identity, IdentityMsg, KeyScheme, ProgramStanding};
use sdk::{Ctx, Error, Module, ModuleId, Msg, Origin, StateRoot};
use sdk_testkit::MemStore;
use std::{cell::Cell, rc::Rc};

const PROGRAM: u64 = 3;

struct Executor;

#[async_trait::async_trait(?Send)]
impl Module for Executor {
    fn id(&self) -> ModuleId {
        "executor".into()
    }

    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let provisioning = matches!(&ctx.env().origin, Origin::Module(id) if id == "identity");
        if provisioning {
            identity::authenticate_event(&ctx.env().origin, "identity", &msg.payload)
                .map_err(|e| Error::module("codec", e))?;
            return Ok(());
        }
        let queue_requested = msg.payload == b"queue";
        if !queue_requested {
            return Err(Error::module(
                "unknown_executor_input",
                "unknown executor input",
            ));
        }
        ctx.emit_msg(Msg {
            target: "dispatch".into(),
            payload: dispatch::encode_msg(&DispatchMsg::Call {
                invocation: "authority-review".into(),
                step: 0,
                account: PROGRAM,
                target: "target".into(),
                payload: Vec::new(),
            }),
        });
        Ok(())
    }
}

#[derive(Default)]
struct Target {
    committed: u64,
    staged: Option<u64>,
}

#[async_trait::async_trait(?Send)]
impl Module for Target {
    fn id(&self) -> ModuleId {
        "target".into()
    }

    fn root(&self) -> StateRoot {
        let mut bytes = [0; sdk::ROOT_LEN];
        bytes[..8].copy_from_slice(&self.committed.to_le_bytes());
        StateRoot(bytes)
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, _msg: &Msg) -> Result<(), Error> {
        assert_eq!(ctx.env().origin, Origin::Program(PROGRAM));
        self.staged = Some(
            self.staged
                .unwrap_or(self.committed)
                .checked_add(1)
                .unwrap(),
        );
        Ok(())
    }

    async fn commit_block(&mut self) -> Result<(), Error> {
        if let Some(value) = self.staged.take() {
            self.committed = value;
        }
        Ok(())
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged = None;
        Ok(())
    }
}

fn account_origin(seed: u64) -> Origin {
    Origin::External(PrivateKey::from_seed(seed).public_key().as_ref().to_vec())
}

fn context(height: u64, origin: Origin) -> BlockContext {
    BlockContext {
        height,
        consensus_time: height,
        origin,
    }
}

fn identity_msg(input: IdentityMsg) -> Msg {
    Msg {
        target: "identity".into(),
        payload: identity::encode_msg(&input),
    }
}

fn with_pending_call() -> Host {
    let mut host = Host::new();
    host.register(Box::new(Identity::new(
        "identity",
        Box::new(MemStore::new()),
        "review-chain".into(),
    )));
    host.register(Box::new(DispatchModule::new(
        "dispatch",
        "saga",
        "identity",
        Box::new(MemStore::new()),
    )));
    host.register(Box::new(Executor));
    host.register(Box::new(Target::default()));
    // No ACL module: live program authority must still be enforced.
    for seed in [1, 2] {
        block_on(host.submit_at(
            context(seed, account_origin(seed)),
            identity_msg(IdentityMsg::Create {
                name: format!("controller-{seed}"),
                scheme: KeyScheme::Ed25519,
            }),
        ))
        .unwrap();
    }
    block_on(host.submit_at(
        context(3, Origin::Module("executor".into())),
        identity_msg(IdentityMsg::CreateProgram {
            name: "programmable user".into(),
            controller: 1,
            request: 1,
        }),
    ))
    .unwrap();
    block_on(host.submit_at(
        context(4, account_origin(1)),
        Msg {
            target: "executor".into(),
            payload: b"queue".to_vec(),
        },
    ))
    .unwrap();
    assert_eq!(host.module_root("target"), Some(StateRoot::ZERO));
    assert_eq!(pending_calls(&host), 1);
    host
}

fn pending_calls(host: &Host) -> usize {
    let bytes = block_on(host.query(
        "dispatch",
        &dispatch::encode_query(&DispatchQuery::PendingCalls),
    ))
    .unwrap();
    let DispatchReply::PendingCalls(calls) = dispatch::decode_reply(&bytes).unwrap() else {
        panic!("pending calls query returned another reply");
    };
    calls.len()
}

#[test]
fn preceding_member_invalidates_queued_program_authority_without_acl() {
    let changes = [
        (
            account_origin(1),
            IdentityMsg::RevokeProgram { account: PROGRAM },
            Refusal::Revoked,
        ),
        (
            account_origin(1),
            IdentityMsg::TransferControl {
                account: PROGRAM,
                to: 2,
            },
            Refusal::StaleGeneration,
        ),
        (
            Origin::Module("executor".into()),
            IdentityMsg::SetProgramStanding {
                account: PROGRAM,
                standing: ProgramStanding::Active,
            },
            Refusal::StaleGeneration,
        ),
    ];
    for (origin, change, refusal) in changes {
        let mut host = with_pending_call();
        let outcome = block_on(host.submit_block(
            context(5, Origin::System),
            vec![(origin, identity_msg(change.clone()))],
        ))
        .unwrap();
        assert!(matches!(outcome.members[0], MemberOutcome::Applied { .. }));
        assert_eq!(outcome.calls.len(), 1);
        assert_eq!(
            outcome.calls[0].disposition,
            CallDisposition::Refused(refusal),
            "the earlier member {change:?} must invalidate this call"
        );
        assert_eq!(host.module_root("target"), Some(StateRoot::ZERO));
        assert_eq!(pending_calls(&host), 0);
    }
}

#[test]
fn rejecting_sole_submit_commits_nothing_but_batch_can_run_pending_work() {
    let mut host = with_pending_call();
    let before = host.root_hash();
    let rejected = Msg {
        target: "unknown-module".into(),
        payload: Vec::new(),
    };
    let result = block_on(host.submit_at(context(5, account_origin(1)), rejected.clone()));
    assert!(matches!(result, Err(SubmitError::Rejected(_))));
    assert_eq!(host.root_hash(), before);
    assert_eq!(pending_calls(&host), 1);

    let batch = block_on(host.submit_block(
        context(6, Origin::System),
        vec![(account_origin(1), rejected)],
    ))
    .unwrap();
    assert!(matches!(batch.members[0], MemberOutcome::Rejected { .. }));
    assert_eq!(batch.calls[0].disposition, CallDisposition::Applied);
    assert_ne!(host.module_root("target"), Some(StateRoot::ZERO));
    assert_eq!(pending_calls(&host), 0);
}

struct UnreadableIdentity(StateRoot);

#[async_trait::async_trait(?Send)]
impl Module for UnreadableIdentity {
    fn id(&self) -> ModuleId {
        "identity".into()
    }

    fn root(&self) -> StateRoot {
        self.0
    }

    async fn execute(&mut self, _: &mut dyn Ctx, _: &Msg) -> Result<(), Error> {
        panic!("the failing identity is only queried");
    }

    async fn query(&self, _: &[u8]) -> Result<Vec<u8>, Error> {
        Err(Error::module(
            "injected_fault",
            "injected identity read failure",
        ))
    }
}

struct StageProbe(Rc<Cell<bool>>);

#[async_trait::async_trait(?Send)]
impl Module for StageProbe {
    fn id(&self) -> ModuleId {
        "stage-probe".into()
    }

    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }

    async fn execute(&mut self, _: &mut dyn Ctx, _: &Msg) -> Result<(), Error> {
        self.0.set(true);
        Ok(())
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.0.set(false);
        Ok(())
    }

    async fn commit_block(&mut self) -> Result<(), Error> {
        panic!("an authority read failure must never commit the preceding member");
    }
}

#[test]
fn authority_read_failure_aborts_preceding_member_writes() {
    let mut host = with_pending_call();
    let prepared = block_on(host.prepare_work(5)).unwrap();
    let identity_root = host.module_root("identity").unwrap();
    host.register(Box::new(UnreadableIdentity(identity_root)));
    let staged = Rc::new(Cell::new(false));
    host.register(Box::new(StageProbe(staged.clone())));
    let before = host.root_hash();
    let result = block_on(host.submit_block_prepared(
        context(5, Origin::System),
        vec![host::BlockOp::bare(
            account_origin(1),
            Msg {
                target: "stage-probe".into(),
                payload: Vec::new(),
            },
        )],
        prepared,
        &mut host::NoWitness,
    ));
    assert!(matches!(result, Err(SubmitError::Fatal(_))));
    assert!(!staged.get(), "the earlier member's pending write leaked");
    assert_eq!(host.root_hash(), before);
    assert_eq!(pending_calls(&host), 1);
}
