//! Queued call failures are consensus data. Native and compiled guest execution
//! must finalize identical outcomes and roots, including declaration errors.

use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use dispatch::{
    CallOutcomeSummary, CallStatus, DispatchModule, DispatchMsg, DispatchQuery, DispatchReply,
};
use futures::executor::block_on;
use host::{BlockContext, Host};
use identity::{Identity, IdentityMsg, KeyScheme};
use sdk::{CallId, Ctx, Error, Module, ModuleId, Msg, Origin, StateRoot};
use sdk_testkit::MemStore;
use sha2::Digest as _;
use std::collections::BTreeMap;
use wasm_host::WasmModule;

const HELLO: &[u8] = include_bytes!("fixtures/hello.component.wasm");
const PROGRAM: u64 = 2;

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
        match &ctx.env().origin {
            Origin::Module(id) => {
                assert!(matches!(id.as_str(), "identity" | "dispatch"));
                Ok(())
            }
            Origin::External(_) => {
                ctx.emit_msg(Msg {
                    target: "dispatch".into(),
                    payload: dispatch::encode_msg(&DispatchMsg::Call {
                        invocation: "parity".into(),
                        step: 0,
                        account: PROGRAM,
                        target: "hello".into(),
                        payload: msg.payload.clone(),
                    }),
                });
                Ok(())
            }
            Origin::Program(_) | Origin::System => Err(Error::Module("unexpected origin".into())),
        }
    }
}

#[derive(Default)]
struct Native {
    committed: BTreeMap<Vec<u8>, Vec<u8>>,
    staged: Option<BTreeMap<Vec<u8>, Vec<u8>>>,
}

#[async_trait::async_trait(?Send)]
impl Module for Native {
    fn id(&self) -> ModuleId {
        "hello".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot(sha2::Sha256::digest(sdk::hash::encode_pairs(&self.committed)).into())
    }
    fn code_hash(&self) -> Option<Vec<u8>> {
        // Compare execution under the same deployed code commitment.
        Some(
            module_artifact::Artifact::module(HELLO.to_vec())
                .hash()
                .to_vec(),
        )
    }
    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        assert_eq!(ctx.env().origin, Origin::Program(PROGRAM));
        match msg.payload.as_slice() {
            b"module-error" => return Err(Error::Module("explicit refusal".into())),
            b"self-query" => return ctx.query("hello", b"").await.map(|_| ()),
            b"missing-query" => return ctx.query("missing", b"").await.map(|_| ()),
            _ => {}
        }
        self.staged
            .get_or_insert_with(|| self.committed.clone())
            .insert(b"count".to_vec(), 1u64.to_le_bytes().to_vec());
        match msg.payload.as_slice() {
            b"output-cap" => ctx.set_output(vec![0; sdk::MAX_OUTPUT_BYTES + 1]),
            b"output-cap-then-small" => {
                ctx.set_output(vec![0; sdk::MAX_OUTPUT_BYTES + 1]);
                ctx.set_output(b"small".to_vec());
            }
            b"output-cap-then-error" => {
                ctx.set_output(vec![0; sdk::MAX_OUTPUT_BYTES + 1]);
                return Err(Error::Module("explicit refusal".into()));
            }
            b"assigned-cap" => ctx.set_assigned(vec![0; sdk::MAX_ASSIGNED_BYTES + 1]),
            b"declarations-valid" => {
                ctx.set_output(b"result".to_vec());
                ctx.set_assigned(b"stamp".to_vec());
            }
            _ => panic!("unknown parity op"),
        }
        Ok(())
    }
    async fn commit_block(&mut self) -> Result<(), Error> {
        if let Some(staged) = self.staged.take() {
            self.committed = staged;
        }
        Ok(())
    }
    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged = None;
        Ok(())
    }
}

fn context(height: u64, origin: Origin) -> BlockContext {
    BlockContext {
        height,
        consensus_time: height,
        origin,
    }
}

fn owner() -> Origin {
    Origin::External(PrivateKey::from_seed(1).public_key().as_ref().to_vec())
}

fn prepared(target: Box<dyn Module>, op: &[u8]) -> Host {
    let mut host = Host::new();
    host.register(Box::new(Identity::new(
        "identity",
        Box::new(MemStore::new()),
        "parity".into(),
    )));
    host.register(Box::new(DispatchModule::new(
        "dispatch",
        "saga",
        "identity",
        Box::new(MemStore::new()),
    )));
    host.register(Box::new(Executor));
    host.register(target);
    block_on(host.submit_at(
        context(1, owner()),
        Msg {
            target: "identity".into(),
            payload: identity::encode_msg(&IdentityMsg::Create {
                name: "owner".into(),
                scheme: KeyScheme::Ed25519,
            }),
        },
    ))
    .unwrap();
    block_on(host.submit_at(
        context(2, Origin::Module("executor".into())),
        Msg {
            target: "identity".into(),
            payload: identity::encode_msg(&IdentityMsg::CreateProgram {
                name: "program".into(),
                controller: 1,
                request: 1,
            }),
        },
    ))
    .unwrap();
    block_on(host.submit_at(
        context(3, owner()),
        Msg {
            target: "executor".into(),
            payload: op.to_vec(),
        },
    ))
    .unwrap();
    host
}

fn call(host: &Host) -> dispatch::CallView {
    let bytes = block_on(host.query(
        "dispatch",
        &dispatch::encode_query(&DispatchQuery::Call {
            id: CallId {
                requester: "executor".into(),
                invocation: "parity".into(),
                step: 0,
            },
        }),
    ))
    .unwrap();
    let DispatchReply::Call(Some(call)) = dispatch::decode_reply(&bytes).unwrap() else {
        panic!("missing call")
    };
    call
}

#[test]
fn compiled_guest_and_native_finalize_identical_call_results_and_roots() {
    let cases: &[(&[u8], Option<&str>)] = &[
        (
            b"output-cap",
            Some("Module(op output exceeds cap (262145 > 262144))"),
        ),
        (
            b"output-cap-then-small",
            Some("Module(op output exceeds cap (262145 > 262144))"),
        ),
        (b"output-cap-then-error", Some("Module(explicit refusal)")),
        (
            b"assigned-cap",
            Some("Module(op assigned stamp exceeds cap (65537 > 65536))"),
        ),
        (b"module-error", Some("Module(explicit refusal)")),
        (b"self-query", Some("SelfQuery")),
        (b"missing-query", Some("UnknownModule(missing)")),
        (b"declarations-valid", None),
    ];
    for (op, expected_reason) in cases {
        let mut native = prepared(Box::new(Native::default()), op);
        let mut wasm = prepared(
            Box::new(WasmModule::from_bytes("hello", HELLO).unwrap()),
            op,
        );
        assert_eq!(native.root_hash(), wasm.root_hash(), "before {op:?}");
        for height in [4, 5] {
            block_on(native.submit_block(context(height, Origin::System), Vec::new())).unwrap();
            block_on(wasm.submit_block(context(height, Origin::System), Vec::new())).unwrap();
            assert_eq!(call(&native), call(&wasm), "outcome {op:?} at {height}");
            assert_eq!(
                native.root_hash(),
                wasm.root_hash(),
                "root {op:?} at {height}"
            );
        }
        let CallStatus::Delivered { outcome, delivery } = call(&native).status else {
            panic!("completion not delivered")
        };
        assert_eq!(delivery, sdk::DeliveryOutcome::Applied);
        match expected_reason {
            Some(reason) => assert_eq!(
                outcome,
                CallOutcomeSummary::Rejected {
                    reason: (*reason).into()
                }
            ),
            None => assert_eq!(
                outcome,
                dispatch::CallOutcome::Applied {
                    output: b"result".to_vec(),
                    assigned: b"stamp".to_vec()
                }
                .summary()
            ),
        }
    }
}
