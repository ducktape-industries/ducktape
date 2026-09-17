//! Compiled program, attribution and dispatch guests preserve authority fences,
//! failure recovery and earlier successful effects across queued reaction steps.

use agent::{AgentMsg, Continuation, Decode, Program, Step, Value};
use attribution::{
    Actor, AttributionMsg, AttributionQuery, AttributionReply, ObjectRef, Reason, Relation,
};
use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use futures::executor::block_on;
use host::{BlockContext, Host};
use identity::{IdentityMsg, KeyScheme, ProgramStanding};
use sdk::{Ctx, Error, MerkleStore as _, Module, ModuleId, Msg, Origin, StateRoot};
use sdk_testkit::MemStore;
use wasm_host::WasmModule;

const PROGRAM: u64 = 3;

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

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        assert_eq!(ctx.env().origin, Origin::Program(PROGRAM));
        let count = self
            .staged
            .unwrap_or(self.committed)
            .checked_add(1)
            .unwrap();
        self.staged = Some(count);
        let reject_after_staging = msg.payload == b"\"fail\"";
        if reject_after_staging {
            return Err(Error::Module("scripted target failure".into()));
        }
        ctx.set_output(count.to_string().into_bytes());
        Ok(())
    }

    async fn query(&self, _req: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(self.committed.to_string().into_bytes())
    }

    async fn commit_block(&mut self) -> Result<(), Error> {
        if let Some(count) = self.staged.take() {
            self.committed = count;
        }
        Ok(())
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged = None;
        Ok(())
    }
}

fn origin(seed: u64) -> Origin {
    Origin::External(PrivateKey::from_seed(seed).public_key().as_ref().to_vec())
}

fn context(height: u64, origin: Origin) -> BlockContext {
    BlockContext {
        height,
        consensus_time: height,
        origin,
    }
}

fn identity_msg(msg: IdentityMsg) -> Msg {
    Msg {
        target: "identity".into(),
        payload: identity::encode_msg(&msg),
    }
}

fn report() -> Step {
    Step::Report {
        recipient: Value::Number(1),
        reason: Reason::Report,
        detail: Value::Text("script ran".into()),
    }
}

fn setup(program: Program) -> Host {
    let mut host = Host::new();
    let mut store = MemStore::new();
    block_on(store.commit_batch(vec![(
        sdk::store_key(sdk::genesis_config::CONFIG_KEY),
        Some(sdk::genesis_config::encode_config(&[(
            "chain_id",
            b"reaction-review",
        )])),
    )]))
    .unwrap();
    host.register(Box::new(
        WasmModule::with_store(
            "identity",
            include_bytes!("fixtures/identity.component.wasm"),
            Box::new(store),
        )
        .unwrap(),
    ));
    for (id, component) in [
        (
            "dispatch",
            include_bytes!("fixtures/dispatch.component.wasm").as_slice(),
        ),
        (
            "attribution",
            include_bytes!("fixtures/attribution.component.wasm").as_slice(),
        ),
        (
            "agent",
            include_bytes!("fixtures/agent.component.wasm").as_slice(),
        ),
    ] {
        host.register(Box::new(
            WasmModule::with_store(id, component, Box::new(MemStore::new())).unwrap(),
        ));
    }
    host.register(Box::new(Target::default()));
    for seed in [1, 2] {
        block_on(host.submit_at(
            context(seed, origin(seed)),
            identity_msg(IdentityMsg::Create {
                name: format!("controller-{seed}"),
                scheme: KeyScheme::Ed25519,
            }),
        ))
        .unwrap();
    }
    block_on(host.submit_at(
        context(3, origin(1)),
        Msg {
            target: "agent".into(),
            payload: agent::encode_msg(&AgentMsg::Provision {
                request_id: "review-program".into(),
                name: "review-program".into(),
                program,
            }),
        },
    ))
    .unwrap();
    host
}

fn publish(host: &mut Host, height: u64, object: &str) {
    block_on(host.submit_at(
        context(height, Origin::Module("review-source".into())),
        Msg {
            target: "attribution".into(),
            payload: attribution::encode_msg(&AttributionMsg::Attribute {
                object: ObjectRef {
                    kind: "message".into(),
                    object: object.into(),
                },
                revision: 1,
                actor: Actor::Account(1),
                relations: vec![Relation {
                    recipient: PROGRAM,
                    reason: Reason::Mention,
                    detail: Vec::new(),
                }],
                transfers: Vec::new(),
            }),
        },
    ))
    .unwrap();
}

fn pump(host: &mut Host, height: u64) {
    block_on(host.submit_block(context(height, Origin::System), Vec::new())).unwrap();
}

fn reports(host: &Host) -> usize {
    let bytes = block_on(host.query(
        "attribution",
        &attribution::encode_query(&AttributionQuery::Changes {
            after: 0,
            limit: 100,
        }),
    ))
    .unwrap();
    let AttributionReply::Changes(changes) = attribution::decode_reply(&bytes).unwrap() else {
        panic!("unexpected attribution query reply");
    };
    changes
        .iter()
        .filter(|entry| entry.change.source.module == "agent")
        .count()
}

#[test]
fn revoked_program_does_not_report_on_a_fresh_attribution() {
    let mut host = setup(Program {
        steps: vec![report(), Step::Finish],
    });
    // A live invocation proves the fixture's delivery and report path works.
    publish(&mut host, 4, "before");
    pump(&mut host, 5);
    assert_eq!(reports(&host), 1);
    block_on(host.submit_at(
        context(6, origin(1)),
        identity_msg(IdentityMsg::RevokeProgram { account: PROGRAM }),
    ))
    .unwrap();
    publish(&mut host, 7, "after");
    pump(&mut host, 8);
    assert_eq!(reports(&host), 1, "revoked program emitted a new report");
}

#[test]
fn pending_invocation_cannot_resume_under_changed_authority() {
    let cases = [
        (origin(1), IdentityMsg::RevokeProgram { account: PROGRAM }),
        (
            origin(1),
            IdentityMsg::TransferControl {
                account: PROGRAM,
                to: 2,
            },
        ),
        (
            Origin::Module("agent".into()),
            IdentityMsg::SetProgramStanding {
                account: PROGRAM,
                standing: ProgramStanding::Suspended,
            },
        ),
    ];
    for (by, change) in cases {
        let transferred = matches!(change, IdentityMsg::TransferControl { .. });
        let mut host = setup(Program {
            steps: vec![
                Step::Call {
                    module: "target".into(),
                    msg: Value::Null,
                    bind: "call".into(),
                    decode: Decode::Json,
                    on_failure: Continuation::Step(1),
                },
                report(),
                Step::Finish,
            ],
        });
        publish(&mut host, 4, "old");
        pump(&mut host, 5);
        // Members run before queued calls. This invalidates the call and
        // produces a refused completion without changing the agent binding.
        block_on(host.submit_block(
            context(6, Origin::System),
            vec![(by, identity_msg(change.clone()))],
        ))
        .unwrap();
        pump(&mut host, 7);
        assert_eq!(reports(&host), 0, "old invocation resumed after {change:?}");
        if transferred {
            publish(&mut host, 8, "new");
            pump(&mut host, 9);
            pump(&mut host, 10);
            pump(&mut host, 11);
            assert_eq!(reports(&host), 1, "fresh work did not use new authority");
        }
    }
}

#[test]
fn later_call_failure_preserves_prior_success_and_reports_its_cause() {
    let call = |msg: &str, bind: &str, on_failure| Step::Call {
        module: "target".into(),
        msg: Value::Text(msg.into()),
        bind: bind.into(),
        decode: Decode::Json,
        on_failure,
    };
    let mut host = setup(Program {
        steps: vec![
            call("ok", "first", Continuation::Unhandled),
            call("fail", "second", Continuation::Step(2)),
            Step::Report {
                recipient: Value::Number(2),
                reason: Reason::Report,
                detail: Value::Ref(vec!["second".into(), "rejected".into(), "reason".into()]),
            },
            Step::Finish,
        ],
    });
    publish(&mut host, 4, "trigger");
    // The queue contract defers each newly queued call/completion to the
    // next block: start, call, completion, second call, second completion.
    for height in 5..=9 {
        pump(&mut host, height);
    }
    assert_eq!(block_on(host.query("target", b"")).unwrap(), b"1");
    let bytes = block_on(host.query(
        "attribution",
        &attribution::encode_query(&AttributionQuery::Changes {
            after: 0,
            limit: 100,
        }),
    ))
    .unwrap();
    let AttributionReply::Changes(changes) = attribution::decode_reply(&bytes).unwrap() else {
        panic!("unexpected attribution reply");
    };
    assert_eq!(changes.len(), 2);
    let trigger = &changes[0].change;
    let report = &changes[1].change;
    assert_eq!(trigger.source.module, "review-source");
    assert_eq!(report.source.module, "agent");
    assert_eq!(report.actor, Actor::Account(PROGRAM));
    assert_eq!(
        report.recipient, 2,
        "the script chooses the report recipient"
    );
    assert!(
        String::from_utf8(report.detail.clone())
            .unwrap()
            .contains("scripted target failure")
    );
    let sdk::Cause::Chain { root, hop } = &report.cause else {
        panic!("failure report lost its causal chain");
    };
    assert_eq!(
        root,
        &sdk::Root::Change {
            source: "attribution".into(),
            seq: trigger.seq
        }
    );
    let sdk::Hop::Completion(id) = hop else {
        panic!("report did not retain the failed call's completion hop");
    };
    assert_eq!(id.requester, "agent");
    assert_eq!(id.step, 1);
}
