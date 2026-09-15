//! state-sync round-trip: a joiner reconstructs a byte-identical qmdb root by
//! pulling a source store's operation range through commonware's qmdb sync,
//! then wraps a fresh `AgentModule` around the injected store — the same
//! discriminating property chat and pages prove, over this module's layout.
//!
//! the source provisions a program account (the correlated request written,
//! then consumed by identity's answer — a delete in the op log), replaces
//! its program (a key overwrite), and runs one invocation through a change
//! and a completion (the invocation record overwritten as it advances), so
//! the op log carries overwrites and deletes a naive "export live records
//! and re-apply sorted" could never reproduce — the qmdb root is
//! operation-log ordered. only a real sync that ships the ACTUAL proven op
//! range lands on the same root.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use agent::{
    AgentModule, AgentMsg, AgentQuery, AgentReply, CallOutcome, Continuation, Decode, Program,
    Siblings, Status, Step, Value, decode_reply, encode_msg, encode_query,
};
use attribution::{Actor, AttributionEvent, Change, ChangeEntry, ChangeKind, Reason, Source};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use dispatch::{CallCompleted, Delivery};
use identity::{
    AccountView, Control, IdentityEvent, IdentityQuery, IdentityReply, KeyScheme, KeyView,
    ProgramStanding,
};
use sdk::{
    AccountNumber, CallId, Cause, Env, Error, Hop, ItemRef, MerkleStore as _, Module, Msg, Origin,
    Root, StateRoot,
};
use sdk_testkit::TestCtx;
use statesync::qmdb::QmdbStore;

const ALICE: AccountNumber = 1;
const ALICE_KEY: [u8; 32] = [0xA1; 32];

fn siblings() -> Siblings {
    Siblings {
        identity: "identity".into(),
        attribution: "attribution".into(),
        dispatch: "dispatch".into(),
    }
}

/// identity's book and attribution's ledger as the round trip scripts them.
struct Scripted {
    accounts: Rc<RefCell<BTreeMap<AccountNumber, AccountView>>>,
    changes: Rc<RefCell<BTreeMap<u64, Change>>>,
}

impl Scripted {
    fn new() -> Self {
        let alice = AccountView {
            number: ALICE,
            name: "alice".into(),
            control: Control::Keys,
            keys: vec![KeyView {
                scheme: KeyScheme::Ed25519,
                pubkey: ALICE_KEY.to_vec(),
                label: None,
                added_at: 0,
            }],
            avatar: None,
            bio: None,
            updated_at: 0,
        };
        Self {
            accounts: Rc::new(RefCell::new(BTreeMap::from([(ALICE, alice)]))),
            changes: Rc::new(RefCell::new(BTreeMap::new())),
        }
    }

    fn found_program(&self, number: AccountNumber, generation: u64) {
        self.accounts.borrow_mut().insert(
            number,
            AccountView {
                number,
                name: "bot".into(),
                control: Control::Program {
                    controller: ALICE,
                    executor: "agent".into(),
                    generation,
                    standing: ProgramStanding::Active,
                },
                keys: Vec::new(),
                avatar: None,
                bio: None,
                updated_at: 0,
            },
        );
    }

    fn ctx(&self, height: u64, origin: Origin, cause: Cause) -> TestCtx {
        let accounts = Rc::clone(&self.accounts);
        let changes = Rc::clone(&self.changes);
        TestCtx::with_env(Env {
            height,
            consensus_time: height,
            origin,
            me: "agent".into(),
            cause,
        })
        .on_query("identity", move |req| {
            let reply = match identity::decode_query(req).map_err(Error::Module)? {
                IdentityQuery::Get { number } => {
                    IdentityReply::Account(accounts.borrow().get(&number).cloned())
                }
                IdentityQuery::OfKey { key } => IdentityReply::Account(
                    accounts
                        .borrow()
                        .values()
                        .find(|view| view.keys.iter().any(|held| held.pubkey == key))
                        .cloned(),
                ),
                other => return Err(Error::Module(format!("unscripted {other:?}"))),
            };
            Ok(identity::encode_reply(&reply))
        })
        .on_query("attribution", move |req| {
            let reply = match attribution::decode_query(req).map_err(Error::Module)? {
                attribution::AttributionQuery::Changes { after, limit } => {
                    attribution::AttributionReply::Changes(
                        changes
                            .borrow()
                            .range(after + 1..)
                            .take(limit as usize)
                            .map(|(seq, change)| ChangeEntry {
                                at: *seq,
                                change: change.clone(),
                            })
                            .collect(),
                    )
                }
                other => return Err(Error::Module(format!("unscripted {other:?}"))),
            };
            Ok(attribution::encode_reply(&reply))
        })
    }
}

fn program(text: &str) -> Program {
    Program {
        steps: vec![
            Step::Call {
                module: "chat".into(),
                msg: Value::Map(BTreeMap::from([
                    ("text".into(), Value::Text(text.into())),
                    (
                        "reply_to".into(),
                        Value::Ref(vec!["change".into(), "source".into(), "object".into()]),
                    ),
                ])),
                bind: "posted".into(),
                decode: Decode::Json,
                on_failure: Continuation::Unhandled,
            },
            Step::Finish,
        ],
    }
}

fn mention(seq: u64, recipient: AccountNumber) -> Change {
    Change {
        seq,
        source: Source {
            module: "chat".into(),
            kind: "message".into(),
            object: format!("m{seq}"),
        },
        revision: 1,
        recipient,
        reason: Reason::Mention,
        kind: ChangeKind::Added,
        detail: Vec::new(),
        actor: Actor::Account(ALICE),
        cause: Cause::Direct,
        height: 3,
    }
}

// drive one op through the REAL module path: execute + commit_block (one op
// per block-height), so the committed op log is what a validator produces.
async fn apply_commit(m: &mut AgentModule, ctx: &mut TestCtx, payload: Vec<u8>) {
    let msg = Msg {
        target: "agent".into(),
        payload,
    };
    m.execute(ctx, &msg).await.unwrap();
    m.commit_block().await.unwrap();
}

async fn query_reply(m: &AgentModule, q: &AgentQuery) -> AgentReply {
    decode_reply(&m.query(&encode_query(q)).await.unwrap()).unwrap()
}

#[test]
fn synced_store_reconstructs_source_root_and_records() {
    deterministic::Runner::default().start(|context| async move {
        let scripted = Scripted::new();
        let alice = Origin::External(ALICE_KEY.to_vec());
        let mut src = AgentModule::new(
            "agent",
            Box::new(QmdbStore::init(context.child("src"), "src").await),
            siblings(),
        );

        // SOURCE: provision (request written, then consumed), replace (the
        // binding overwritten at generation 1), one invocation started by a
        // change and finished by its completion (the record overwritten).
        let mut ctx = scripted.ctx(1, alice.clone(), Cause::Direct);
        apply_commit(
            &mut src,
            &mut ctx,
            encode_msg(&AgentMsg::Provision {
                request_id: "bot".into(),
                name: "bot".into(),
                program: program("first"),
            }),
        )
        .await;
        let account: AccountNumber = 2;
        scripted.found_program(account, 0);
        let mut ctx = scripted.ctx(1, Origin::Module("identity".into()), Cause::Direct);
        apply_commit(
            &mut src,
            &mut ctx,
            identity::encode_event(&IdentityEvent::ProgramCreated {
                request: 1,
                account,
                controller: ALICE,
            }),
        )
        .await;
        let mut ctx = scripted.ctx(2, alice.clone(), Cause::Direct);
        apply_commit(
            &mut src,
            &mut ctx,
            encode_msg(&AgentMsg::Initialize {
                account,
                request_id: "install".into(),
            }),
        )
        .await;
        let mut ctx = scripted.ctx(2, alice.clone(), Cause::Direct);
        apply_commit(
            &mut src,
            &mut ctx,
            encode_msg(&AgentMsg::Replace {
                account,
                program: program("second"),
            }),
        )
        .await;
        scripted.found_program(account, 1);

        let change = mention(9, account);
        scripted.changes.borrow_mut().insert(9, change.clone());
        let item = ItemRef {
            source: "attribution".into(),
            item: 4,
        };
        let mut ctx = scripted.ctx(
            3,
            Origin::Module("attribution".into()),
            Cause::Chain {
                root: Root::Item(item.clone()),
                hop: Hop::Delivery(item),
            },
        );
        apply_commit(
            &mut src,
            &mut ctx,
            attribution::encode_event(&AttributionEvent::Changed(change)),
        )
        .await;
        let id = CallId {
            requester: "agent".into(),
            invocation: format!("{account}/9"),
            step: 0,
        };
        let mut ctx = scripted.ctx(
            4,
            Origin::Module("dispatch".into()),
            Cause::Chain {
                root: Root::Item(ItemRef {
                    source: "attribution".into(),
                    item: 4,
                }),
                hop: Hop::Completion(id.clone()),
            },
        );
        apply_commit(
            &mut src,
            &mut ctx,
            dispatch::encode_delivery(&Delivery::CallCompleted(CallCompleted {
                id,
                account,
                outcome: CallOutcome::Applied {
                    output: br#"{"message_id":"m10"}"#.to_vec(),
                    assigned: Vec::new(),
                },
            })),
        )
        .await;

        let src_root: StateRoot = src.root();
        assert_ne!(src_root, StateRoot::ZERO, "source must have a real root");
        let src_binding = query_reply(&src, &AgentQuery::Binding { account }).await;
        let provision_query = AgentQuery::Provision {
            controller: ALICE,
            request_id: "bot".into(),
        };
        let initialization_query = AgentQuery::Initialization {
            account,
            request_id: "install".into(),
        };
        let src_provision = query_reply(&src, &provision_query).await;
        let src_initialization = query_reply(&src, &initialization_query).await;
        let src_invocations = query_reply(
            &src,
            &AgentQuery::Invocations {
                account,
                after: 0,
                limit: 8,
            },
        )
        .await;

        // the module consumed its store, so REOPEN the committed partitions
        // as a bare store for the handoff (drop first — one owner at a time).
        drop(src);
        let src_store = QmdbStore::init(context.child("src_serve"), "src").await;
        assert_eq!(
            src_store.root(),
            src_root,
            "reopened store must recover the committed root"
        );

        // describe the target (root + op range), THEN hand the source off as
        // the sync resolver (consumes it — order matters).
        let target = src_store.sync_boundary_target().await;
        let resolver = src_store.into_resolver();

        // JOINER: reconstruct on a FRESH context + namespace by pulling from
        // the resolver, then wrap the module around the injected store — the
        // exact shape a joining host uses. no ops are applied in application
        // order on this side.
        let store = QmdbStore::sync_from(context.child("dst"), "dst", target, resolver)
            .await
            .expect("sync_from");
        let mut synced = AgentModule::new("agent", Box::new(store), siblings());

        // THE PROPERTY: identical qmdb root — the root-hash linkage a joiner
        // needs at the boundary height.
        assert_eq!(
            synced.root(),
            src_root,
            "synced store root must equal the source root"
        );

        // every committed mutation survived: the replaced binding at its
        // generation, the finished invocation with its bound outcome.
        assert_eq!(query_reply(&synced, &provision_query).await, src_provision);
        assert_eq!(
            query_reply(&synced, &initialization_query).await,
            src_initialization
        );
        let mut ctx = scripted.ctx(5, alice.clone(), Cause::Direct);
        apply_commit(
            &mut synced,
            &mut ctx,
            encode_msg(&AgentMsg::Provision {
                request_id: "bot".into(),
                name: "bot".into(),
                program: program("first"),
            }),
        )
        .await;
        assert!(ctx.msgs().is_empty());
        let mut ctx = scripted.ctx(5, alice, Cause::Direct);
        apply_commit(
            &mut synced,
            &mut ctx,
            encode_msg(&AgentMsg::Initialize {
                account,
                request_id: "install".into(),
            }),
        )
        .await;
        assert!(ctx.msgs().is_empty());
        assert_eq!(
            synced.root(),
            src_root,
            "retries after sync must not allocate or initialize again"
        );
        let binding = query_reply(&synced, &AgentQuery::Binding { account }).await;
        assert_eq!(binding, src_binding);
        let AgentReply::Binding(Some(binding)) = binding else {
            panic!("bound");
        };
        assert_eq!(binding.program, program("second"));
        assert_eq!(binding.revision, 1);
        let invocations = query_reply(
            &synced,
            &AgentQuery::Invocations {
                account,
                after: 0,
                limit: 8,
            },
        )
        .await;
        assert_eq!(invocations, src_invocations);
        let AgentReply::Invocations(listing) = invocations else {
            panic!("listing");
        };
        assert_eq!(listing.len(), 1);
        assert_eq!(listing[0].invocation.seq, 9);
        assert_eq!(
            listing[0].invocation.status,
            Status::Finished { at_step: 1 }
        );
        assert_eq!(
            listing[0].invocation.bindings["posted"],
            serde_json::json!({"applied": {"output": {"message_id": "m10"}, "assigned": null}})
        );
    });
}
