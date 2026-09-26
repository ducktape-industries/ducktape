use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use abi::{
    Blob, BlobHeader, BlobId, Cause, CryptoOp, CryptoReply, Entry, Env, HashKind, HostOp,
    HostReply, ItemRef, Message, Origin, Outcome, Principal, Refusal, Scan, Scheme, reason,
    role::{identity, registry, validators},
};
use commonware_codec::Encode as _;
use commonware_cryptography::bls12381::primitives::group::{Private, Scalar};
use commonware_cryptography::bls12381::primitives::ops;
use commonware_cryptography::bls12381::primitives::variant::MinPk;
use commonware_cryptography::{Signer as _, ed25519};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use fixture_module_registry::Change;
use fixture_probe::{Reply, Step};
use fixture_relay::Script;
use host::{
    BLOBS, Block, BlockId, Error, Founding, FoundingView, Genesis, Host, Layer, Limits, NETWORK,
    Receipt, Roles, SIGNERS, Submission, Tip,
};
use keyscheme::testkit;
use sha2::Digest as _;
use state::{Commitment, SyncTarget, commitment_name};

const MODULE_REGISTRY: &[u8] = include_bytes!("../../fixtures/wasm/fixture_module_registry.wasm");
const VALSET: &[u8] = include_bytes!("../../fixtures/wasm/fixture_valset.wasm");
const RELAY: &[u8] = include_bytes!("../../fixtures/wasm/fixture_relay.wasm");
const IDENTITY: &[u8] = include_bytes!("../../fixtures/wasm/fixture_identity.wasm");
const PROBE: &[u8] = include_bytes!("../../fixtures/wasm/fixture_probe.wasm");

const SIGNER: &[u8] = b"signer";
/// Fuel that runs one relay handler and a message of it, not a chain of eight.
const FRAME_FUEL: u64 = 100_000;
const EPOCH_LENGTH: u64 = 4;
const TIME: u64 = 1_700_000_000;

type Ctx = deterministic::Context;

fn member(key: &[u8], address: &str) -> validators::Member {
    validators::Member {
        key: key.to_vec(),
        address: address.to_owned(),
    }
}

fn founding(program: &str, code: &[u8], params: Vec<u8>) -> Founding {
    Founding {
        program: program.to_owned(),
        code: code.to_vec(),
        params,
    }
}

/// The account [`SIGNER`] holds.
const ACCOUNT: u64 = 1;

fn roles() -> Roles {
    Roles {
        registry: "module-registry".into(),
        validators: "valset".into(),
        identity: "identity".into(),
    }
}

/// The account identity gives `ping`, the fourth founding program
/// ([`standard`]): programs are numbered from `MODULES_FROM` in order.
const PING: u64 = fixture_identity::MODULES_FROM + 3;

/// The registry, the validators and identity (where [`SIGNER`] holds
/// [`ACCOUNT`]), then `programs`.
fn genesis(programs: Vec<Founding>) -> Genesis {
    let bound = vec![
        founding("module-registry", MODULE_REGISTRY, Vec::new()),
        founding("valset", VALSET, Vec::new()),
        founding(
            "identity",
            IDENTITY,
            abi::encode(&vec![(SIGNER.to_vec(), ACCOUNT)]),
        ),
    ];
    Genesis {
        network: b"net".to_vec(),
        roles: roles(),
        validators: vec![member(b"v1", "v1:1")],
        programs: bound.into_iter().chain(programs).collect(),
        views: Vec::new(),
        limits: Limits::default(),
        epoch_length: EPOCH_LENGTH,
        time: TIME,
    }
}

fn standard() -> Genesis {
    genesis(vec![
        founding("ping", RELAY, Vec::new()),
        founding("pong", RELAY, Vec::new()),
        founding("probe", PROBE, script(Vec::new())),
    ])
}

async fn found(context: Ctx, name: &str, dir: &Path, genesis: Genesis) -> Host<Ctx> {
    Host::found(context, name, dir, block_id(0), genesis)
        .await
        .unwrap()
        .0
}

fn script(steps: Vec<Step>) -> Vec<u8> {
    abi::encode(&steps)
}

fn op(op: HostOp) -> Step {
    Step::Op(op)
}

fn set(key: &[u8], value: &[u8]) -> Step {
    op(HostOp::Set {
        key: key.to_vec(),
        value: value.to_vec(),
    })
}

fn get(key: &[u8]) -> Step {
    op(HostOp::Get(key.to_vec()))
}

fn submit(seq: u64, target: &str, payload: Vec<u8>) -> Submission {
    Submission {
        signer: SIGNER.to_vec(),
        seq,
        target: target.to_owned(),
        payload,
    }
}

fn block_id(height: u64) -> BlockId {
    let mut id = [0u8; 32];
    id[..8].copy_from_slice(&height.to_be_bytes());
    id
}

fn block(height: u64, submissions: Vec<Submission>) -> Block {
    Block {
        height,
        id: block_id(height),
        time: TIME + height,
        submissions,
    }
}

/// A relay message: `target` runs `script` in the frame, replying or not.
fn message(target: &str, script: Script, reply: bool) -> Message {
    Message {
        target: target.to_owned(),
        payload: abi::encode(&script),
        reply,
    }
}

/// A relay payload that emits `messages`.
fn send(messages: Vec<Message>) -> Vec<u8> {
    abi::encode(&Script::Send(messages))
}

fn note(bytes: &[u8]) -> Script {
    Script::Note(bytes.to_vec())
}

fn fail(loud: bool) -> Script {
    Script::Fail {
        sentence: "asked to fail".into(),
        loud,
    }
}

/// The refusal a relay's `fail` is.
fn refused() -> Refusal {
    Refusal::new(reason::INVALID_INPUT, "asked to fail")
}

/// `receipt` with `nested` runs.
fn nested(mut receipt: Receipt, nested: Vec<Receipt>) -> Receipt {
    receipt.nested = nested;
    receipt
}

fn stored(host: &Host<Ctx>, program: &str, key: &[u8]) -> Option<Vec<u8>> {
    host.view(Layer::Confirmed).get(program, key).unwrap()
}

fn item(source: &str, item: u64) -> ItemRef {
    ItemRef {
        source: source.to_owned(),
        item,
    }
}

fn output(receipt: &Receipt) -> &[u8] {
    match &receipt.outcome {
        Outcome::Applied { output } => output,
        Outcome::Rejected(refusal) => panic!("{} was rejected: {refusal}", receipt.program),
    }
}

fn rejected(receipt: &Receipt) -> &Refusal {
    match &receipt.outcome {
        Outcome::Applied { .. } => panic!("{} was applied", receipt.program),
        Outcome::Rejected(refusal) => refusal,
    }
}

fn replies(receipt: &Receipt) -> Vec<Reply> {
    abi::decode(output(receipt)).unwrap()
}

fn answers(bytes: &[u8]) -> Vec<Reply> {
    abi::decode(bytes).unwrap()
}

/// The account identity says `program` runs as.
async fn account_of(host: &Host<Ctx>, program: &str) -> Option<u64> {
    let asked = identity::Query::OfModule(program.into());
    let reply = host
        .query(
            Layer::Confirmed,
            TIME,
            Origin::System,
            "identity",
            abi::encode(&asked),
        )
        .await
        .unwrap()
        .unwrap();
    match abi::decode(&reply).unwrap() {
        identity::Reply::Account(account) => account,
        other => panic!("{other:?}"),
    }
}

async fn ask(host: &Host<Ctx>, layer: Layer, program: &str, steps: Vec<Step>) -> Vec<Reply> {
    let answer = host
        .query(
            layer,
            TIME,
            Origin::External(SIGNER.to_vec()),
            program,
            script(steps),
        )
        .await
        .unwrap()
        .unwrap();
    answers(&answer)
}

fn receipt(program: &str, outcome: Outcome, events: Vec<&[u8]>) -> Receipt {
    Receipt {
        program: program.to_owned(),
        outcome,
        events: events.into_iter().map(<[u8]>::to_vec).collect(),
        nested: Vec::new(),
    }
}

fn ok(output: &[u8]) -> Outcome {
    Outcome::Applied {
        output: output.to_vec(),
    }
}

fn change(program: &str, code: BlobId, params: Vec<u8>) -> Vec<u8> {
    abi::encode(&Change::Set(registry::Entry {
        program: program.to_owned(),
        code,
        params,
    }))
}

#[test]
fn founding_admits_every_program_and_the_host_reopens() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut founded = standard();
        // a view has no program behind it: it is listed, never admitted
        founded.views.push(FoundingView {
            name: "explorer".into(),
            view: b"view".to_vec(),
        });
        let (host, applied) = Host::found(
            context.child("found"),
            "net",
            dir.path(),
            block_id(0),
            founded,
        )
        .await
        .unwrap();
        assert_eq!(applied.height, 0);
        let admitted: Vec<&str> = applied
            .admissions
            .iter()
            .map(|r| r.program.as_str())
            .collect();
        let founded = [
            "module-registry",
            "valset",
            "identity",
            "ping",
            "pong",
            "probe",
        ];
        // the roles' admissions and accounts, then each other program's
        // account before its admission
        assert_eq!(admitted[..3], founded[..3]);
        assert_eq!(admitted[3..6], ["identity"; 3]);
        for (pair, program) in admitted[6..].chunks(2).zip(&founded[3..]) {
            assert_eq!(pair, ["identity", *program]);
        }
        for (at, program) in founded.into_iter().enumerate() {
            let number = fixture_identity::MODULES_FROM + at as u64;
            assert_eq!(account_of(&host, program).await, Some(number));
        }
        for receipt in &applied.admissions {
            assert!(
                matches!(receipt.outcome, Outcome::Applied { .. }),
                "{receipt:?}"
            );
        }
        let programs = host.programs().unwrap();
        let ids: Vec<&str> = programs.keys().map(String::as_str).collect();
        assert_eq!(
            ids,
            [
                "identity",
                "module-registry",
                "ping",
                "pong",
                "probe",
                "valset"
            ]
        );
        assert_eq!(programs["ping"], programs["pong"]);
        assert_ne!(programs["ping"], programs["probe"]);
        assert_eq!(
            host.epoch_members(0).unwrap(),
            Some(vec![member(b"v1", "v1:1")])
        );
        assert_eq!(host.epoch_members(1).unwrap(), None);
        assert_eq!(host.epoch_length().unwrap(), EPOCH_LENGTH);
        assert_eq!(
            host.tip().unwrap(),
            Tip {
                height: 0,
                id: block_id(0)
            }
        );
        assert_eq!(host.height().unwrap(), 0);
        assert_eq!(host.root().unwrap(), applied.root);
        assert!(host.missing_blobs().unwrap().is_empty());
        assert_eq!(
            host.view(Layer::Confirmed).get(NETWORK, b"limits").unwrap(),
            Some(abi::encode(&Limits::default()))
        );
        let host_tip = host.tip().unwrap();
        drop(host);

        let reopened = Host::open(context.child("reopen"), "net", dir.path())
            .await
            .unwrap();
        assert_eq!(reopened.root().unwrap(), applied.root);
        assert_eq!(reopened.programs().unwrap(), programs);
        assert_eq!(
            reopened.epoch_members(0).unwrap(),
            Some(vec![member(b"v1", "v1:1")])
        );
        assert_eq!(reopened.tip().unwrap(), host_tip);
        let env = ask(&reopened, Layer::Confirmed, "probe", vec![Step::Env]).await;
        assert_eq!(
            env,
            vec![Reply::Env(Env {
                network: b"net".to_vec(),
                height: 0,
                time: TIME,
                me: "probe".into(),
                origin: Origin::External(SIGNER.to_vec()),
                sender: None,
                roles: roles(),
                cause: Cause::Direct,
            })]
        );
    });
}

#[test]
fn founding_refuses_an_unbound_role() {
    deterministic::Runner::default().start(|context| async move {
        for (name, identity) in [("empty", ""), ("unfounded", "nobody")] {
            let dir = tempfile::tempdir().unwrap();
            let mut unbound = standard();
            unbound.roles.identity = identity.into();
            let Err(Error::Unbound { role, program }) =
                Host::found(context.child(name), name, dir.path(), block_id(0), unbound).await
            else {
                panic!("a genesis with identity bound to {identity:?} was founded");
            };
            assert_eq!((role, program.as_str()), ("identity", identity));
        }
    });
}

#[test]
fn founding_refuses_a_program_that_does_not_admit() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let refusing = genesis(vec![founding(
            "probe",
            PROBE,
            script(vec![Step::Fail("no".into())]),
        )]);
        let Err(Error::Genesis { program, refusal }) = Host::found(
            context.child("refusing"),
            "a",
            dir.path(),
            block_id(0),
            refusing,
        )
        .await
        else {
            panic!("a refusing founder was admitted");
        };
        assert_eq!(program, "probe");
        assert_eq!(refusal, Refusal::new("probe", "no"));

        let dir = tempfile::tempdir().unwrap();
        let twice = genesis(vec![
            founding("ping", RELAY, Vec::new()),
            founding("ping", RELAY, Vec::new()),
        ]);
        let Err(Error::Genesis { program, refusal }) =
            Host::found(context.child("twice"), "b", dir.path(), block_id(0), twice).await
        else {
            panic!("a duplicate founder was admitted");
        };
        assert_eq!(program, "ping");
        assert_eq!(refusal.reason, reason::INVALID_INPUT);

        let dir = tempfile::tempdir().unwrap();
        let mut shadowed = genesis(vec![founding("ping", RELAY, Vec::new())]);
        shadowed.views.push(FoundingView {
            name: "ping".into(),
            view: b"view".to_vec(),
        });
        let Err(Error::Genesis { program, refusal }) = Host::found(
            context.child("shadowed"),
            "b2",
            dir.path(),
            block_id(0),
            shadowed,
        )
        .await
        else {
            panic!("a view took a program's name");
        };
        assert_eq!(program, "ping");
        assert_eq!(refusal.reason, reason::INVALID_INPUT);

        let dir = tempfile::tempdir().unwrap();
        let reserved = genesis(vec![founding("$ping", RELAY, Vec::new())]);
        let Err(Error::Genesis { program, .. }) = Host::found(
            context.child("reserved"),
            "c",
            dir.path(),
            block_id(0),
            reserved,
        )
        .await
        else {
            panic!("a reserved founder was admitted");
        };
        assert_eq!(program, "$ping");

        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Host::open(context.child("empty"), "d", dir.path()).await,
            Err(Error::Unfounded)
        ));
    });
}

#[test]
fn blocks_apply_in_sequence_only() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        assert!(matches!(
            host.apply(block(2, Vec::new())).await,
            Err(Error::Height {
                expected: 1,
                got: 2
            })
        ));
        assert!(matches!(
            host.apply(block(0, Vec::new())).await,
            Err(Error::Height {
                expected: 1,
                got: 0
            })
        ));
        assert_eq!(host.apply(block(1, Vec::new())).await.unwrap().height, 1);
        assert_eq!(host.height().unwrap(), 1);
    });
}

#[test]
fn a_message_runs_in_the_frame_that_emits_it_and_replies_there() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        let first = item("ping", 0);

        let applied = host
            .apply(block(
                1,
                vec![submit(
                    0,
                    "ping",
                    send(vec![message("pong", note(b"hello"), true)]),
                )],
            ))
            .await
            .unwrap();
        assert_eq!(
            applied.submissions,
            vec![nested(
                receipt("ping", ok(&abi::encode(&vec![first.clone()])), vec![]),
                vec![
                    receipt("pong", ok(b"olleh"), vec![b"hello"]),
                    receipt("ping", ok(b""), vec![]),
                ]
            )]
        );
        assert_eq!(
            stored(&host, "ping", &fixture_relay::sent(&first)),
            Some(abi::encode(&note(b"hello")))
        );
        assert_eq!(
            stored(&host, "pong", &fixture_relay::got(&first)),
            Some(b"hello".to_vec())
        );
        assert_eq!(
            stored(&host, "ping", &fixture_relay::done(&first)),
            Some(abi::encode(&ok(b"olleh")))
        );

        // without a reply wanted, the target's run is all that nests
        let again = item("ping", 0);
        let applied = host
            .apply(block(
                2,
                vec![submit(
                    1,
                    "ping",
                    send(vec![message("pong", note(b"again"), false)]),
                )],
            ))
            .await
            .unwrap();
        assert_eq!(
            applied.submissions,
            vec![nested(
                receipt("ping", ok(&abi::encode(&vec![again.clone()])), vec![]),
                vec![receipt("pong", ok(b"niaga"), vec![b"again"])]
            )]
        );
    });
}

#[test]
fn messages_run_in_emit_order_depth_first() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        // ping: [pong: [ping: a], pong: b]
        let inner = Script::Send(vec![message("ping", note(b"a"), false)]);
        let applied = host
            .apply(block(
                1,
                vec![submit(
                    0,
                    "ping",
                    send(vec![
                        message("pong", inner, false),
                        message("pong", note(b"b"), false),
                    ]),
                )],
            ))
            .await
            .unwrap();
        let items = |items: &[ItemRef]| ok(&abi::encode(&items.to_vec()));
        assert_eq!(
            applied.submissions,
            vec![nested(
                receipt("ping", items(&[item("ping", 0), item("ping", 1)]), vec![]),
                vec![
                    nested(
                        receipt("pong", items(&[item("pong", 2)]), vec![]),
                        vec![receipt("ping", ok(b"a"), vec![b"a"])]
                    ),
                    receipt("pong", ok(b"b"), vec![b"b"]),
                ]
            )]
        );
    });
}

#[test]
fn a_message_sees_who_emitted_it() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        let probe_script = script(vec![Step::Env]);
        let applied = host
            .apply(block(
                1,
                vec![submit(
                    0,
                    "ping",
                    send(vec![Message {
                        target: "probe".into(),
                        payload: probe_script,
                        reply: true,
                    }]),
                )],
            ))
            .await
            .unwrap();
        let env = Env {
            network: b"net".to_vec(),
            height: 1,
            time: TIME + 1,
            me: "probe".into(),
            origin: Origin::Program("ping".into()),
            // ping's own account, which identity gave it at genesis
            sender: Some(Principal::Account(PING)),
            roles: roles(),
            cause: Cause::Message(item("ping", 0)),
        };
        let [probed, replied] = applied.submissions[0].nested.as_slice() else {
            panic!("{:?}", applied.submissions[0].nested);
        };
        assert_eq!(replies(probed), vec![Reply::Env(env.clone())]);
        assert_eq!(replied, &receipt("ping", ok(b""), vec![]));
        assert_eq!(
            stored(&host, "ping", &fixture_relay::done(&item("ping", 0))),
            Some(abi::encode(&ok(&abi::encode(&vec![Reply::Env(env)]))))
        );
    });
}

#[test]
fn a_message_emitted_from_init_carries_the_program_account() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut founding_file = standard();
        let emit = op(HostOp::Emit(Message {
            target: "probe".into(),
            payload: script(vec![Step::Env]),
            reply: false,
        }));
        founding_file
            .programs
            .push(founding("emitter", PROBE, script(vec![emit])));
        let (host, applied) = Host::found(context, "net", dir.path(), block_id(0), founding_file)
            .await
            .unwrap();
        let account = account_of(&host, "emitter").await.unwrap();
        let init = applied
            .admissions
            .iter()
            .find(|receipt| receipt.program == "emitter" && !receipt.nested.is_empty())
            .unwrap();
        let [probed] = init.nested.as_slice() else {
            panic!("{:?}", init.nested);
        };
        let env = Env {
            network: b"net".to_vec(),
            height: 0,
            time: TIME,
            me: "probe".into(),
            origin: Origin::Program("emitter".into()),
            // registered before its init ran
            sender: Some(Principal::Account(account)),
            roles: roles(),
            cause: Cause::Message(item("emitter", 0)),
        };
        assert_eq!(replies(probed), vec![Reply::Env(env)]);
    });
}

#[test]
fn a_rejected_message_without_a_reply_undoes_the_frame() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        let applied = host
            .apply(block(
                1,
                vec![
                    submit(
                        0,
                        "ping",
                        send(vec![
                            message("pong", note(b"first"), false),
                            message("pong", fail(false), false),
                            message("pong", note(b"never"), false),
                        ]),
                    ),
                    submit(0, "ping", send(vec![message("nobody", note(b"x"), false)])),
                ],
            ))
            .await
            .unwrap();
        assert_eq!(
            applied.submissions,
            vec![
                nested(
                    receipt("ping", Outcome::Rejected(refused()), vec![]),
                    vec![
                        receipt("pong", ok(b"tsrif"), vec![b"first"]),
                        receipt("pong", Outcome::Rejected(refused()), vec![]),
                    ]
                ),
                nested(
                    receipt(
                        "ping",
                        Outcome::Rejected(Refusal::new(reason::UNKNOWN_PROGRAM, "nobody")),
                        vec![]
                    ),
                    vec![receipt(
                        "nobody",
                        Outcome::Rejected(Refusal::new(reason::UNKNOWN_PROGRAM, "nobody")),
                        vec![]
                    )]
                ),
            ]
        );
        // the sender's and the applied target's writes are undone with it
        assert_eq!(
            stored(&host, "ping", &fixture_relay::sent(&item("ping", 0))),
            None
        );
        assert_eq!(
            stored(&host, "pong", &fixture_relay::got(&item("ping", 0))),
            None
        );
        assert_eq!(
            stored(&host, "pong", &fixture_relay::got(&item("ping", 2))),
            None
        );
    });
}

#[test]
fn a_rejected_reply_undoes_the_target_and_the_sender_absorbs_or_propagates() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        // pong writes, then fails without a reply: its frame is undone and
        // ping, which wanted the reply, absorbs the rejection and goes on
        let pong = Script::Send(vec![
            message("ping", note(b"inner"), false),
            message("ping", fail(false), false),
        ]);
        let applied = host
            .apply(block(
                1,
                vec![submit(
                    0,
                    "ping",
                    send(vec![
                        message("pong", pong, true),
                        message("pong", note(b"after"), false),
                    ]),
                )],
            ))
            .await
            .unwrap();
        let receipt_ = &applied.submissions[0];
        assert_eq!(
            receipt_.outcome,
            ok(&abi::encode(&vec![item("ping", 0), item("ping", 1)]))
        );
        let programs: Vec<(&str, bool)> = receipt_
            .nested
            .iter()
            .map(|r| {
                (
                    r.program.as_str(),
                    matches!(r.outcome, Outcome::Applied { .. }),
                )
            })
            .collect();
        assert_eq!(programs, [("pong", false), ("ping", true), ("pong", true)]);
        assert_eq!(receipt_.nested[0].outcome, Outcome::Rejected(refused()));
        assert_eq!(
            stored(&host, "pong", &fixture_relay::sent(&item("pong", 2))),
            None
        );
        assert_eq!(
            stored(&host, "ping", &fixture_relay::got(&item("pong", 2))),
            None
        );
        assert_eq!(
            stored(&host, "ping", &fixture_relay::done(&item("ping", 0))),
            Some(abi::encode(&Outcome::Rejected(refused())))
        );
        assert_eq!(
            stored(&host, "pong", &fixture_relay::got(&item("ping", 1))),
            Some(b"after".to_vec())
        );

        // a reply run that refuses fails the whole frame
        let applied = host
            .apply(block(
                2,
                vec![submit(
                    1,
                    "ping",
                    send(vec![
                        message("pong", note(b"before"), false),
                        message("pong", fail(true), true),
                    ]),
                )],
            ))
            .await
            .unwrap();
        assert_eq!(
            applied.submissions,
            vec![nested(
                receipt("ping", Outcome::Rejected(refused()), vec![]),
                vec![
                    receipt("pong", ok(b"erofeb"), vec![b"before"]),
                    receipt("pong", Outcome::Rejected(refused()), vec![]),
                    receipt("ping", Outcome::Rejected(refused()), vec![]),
                ]
            )]
        );
        assert_eq!(
            stored(&host, "pong", &fixture_relay::got(&item("ping", 0))),
            None
        );
        assert_eq!(
            stored(&host, "ping", &fixture_relay::done(&item("ping", 1))),
            None
        );
    });
}

/// A chain of `depth` messages, ping to pong and back, ending in a note.
fn chain(depth: u32) -> Script {
    if depth == 0 {
        return note(b"end");
    }
    let target = if depth.is_multiple_of(2) {
        "ping"
    } else {
        "pong"
    };
    Script::Send(vec![message(target, chain(depth - 1), false)])
}

#[test]
fn messages_nest_at_most_eight_deep() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        let applied = host
            .apply(block(
                1,
                vec![
                    submit(0, "ping", abi::encode(&chain(8))),
                    submit(1, "ping", abi::encode(&chain(9))),
                ],
            ))
            .await
            .unwrap();
        let mut deepest = &applied.submissions[0];
        let mut depth = 0;
        while let Some(inner) = deepest.nested.first() {
            deepest = inner;
            depth += 1;
        }
        assert_eq!(depth, 8);
        assert_eq!(deepest, &receipt("pong", ok(b"dne"), vec![b"end"]));
        let refusal = rejected(&applied.submissions[1]);
        assert_eq!(refusal.reason, reason::CAPACITY);
        assert_eq!(refusal.sentence, "messages nest deeper than 8");
    });
}

#[test]
fn a_frame_runs_on_one_fuel_budget() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut metered = standard();
        // enough for a run or two of relay, not for a chain of nine
        metered.limits = Limits {
            fuel: Some(FRAME_FUEL),
            memory_bytes: None,
        };
        let mut host = found(context, "net", dir.path(), metered).await;
        let applied = host
            .apply(block(
                1,
                vec![
                    submit(0, "ping", abi::encode(&chain(1))),
                    submit(1, "ping", abi::encode(&chain(8))),
                ],
            ))
            .await
            .unwrap();
        assert!(
            matches!(applied.submissions[0].outcome, Outcome::Applied { .. }),
            "{:?}",
            applied.submissions[0]
        );
        assert_eq!(rejected(&applied.submissions[1]).reason, reason::TRAP);
    });
}

/// A probe script that asks `probe`'s own query to spin, `times` times.
fn spin_queries(times: usize) -> Vec<Step> {
    let spin = || HostOp::Query {
        program: "probe".into(),
        request: script(vec![Step::Spin]),
    };
    (0..times).map(|_| op(spin())).collect()
}

/// The refusal of a run whose frame spent its fuel.
fn spent() -> Refusal {
    Refusal::new(reason::TRAP, runtime::OUT_OF_FUEL)
}

fn metered() -> Genesis {
    let mut metered = standard();
    metered.limits = Limits {
        fuel: Some(FRAME_FUEL),
        memory_bytes: None,
    };
    metered
}

#[test]
fn a_query_inside_a_frame_runs_on_the_frame_budget() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), metered()).await;
        // each spin would burn a whole budget of its own: the first spends
        // the frame's, and the handler traps on what is left, nothing
        let applied = host
            .apply(block(1, vec![submit(0, "probe", script(spin_queries(3)))]))
            .await
            .unwrap();
        assert_eq!(rejected(&applied.submissions[0]), &spent());
        // a query from outside any frame still runs on its own budget
        let asked = ask(&host, Layer::Confirmed, "probe", vec![Step::Env]).await;
        assert_eq!(asked.len(), 1);
    });
}

#[test]
fn a_spent_frame_starts_no_further_run() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), metered()).await;
        let spend = Message {
            target: "probe".into(),
            payload: script(spin_queries(1)),
            reply: true,
        };
        let payload = send(vec![spend, message("pong", note(b"late"), false)]);
        let applied = host
            .apply(block(1, vec![submit(0, "ping", payload)]))
            .await
            .unwrap();
        let frame = &applied.submissions[0];
        assert_eq!(rejected(frame), &spent());
        // the probe spends the frame; the reply it wanted, and pong after
        // it, never start
        assert_eq!(
            frame.nested,
            vec![
                rejected_receipt("probe", spent()),
                rejected_receipt("ping", spent()),
            ]
        );
        assert_eq!(
            stored(&host, "pong", &fixture_relay::got(&item("ping", 1))),
            None
        );
    });
}

fn rejected_receipt(program: &str, refusal: Refusal) -> Receipt {
    receipt(program, Outcome::Rejected(refusal), Vec::new())
}

#[test]
fn a_submission_acts_as_the_account_its_signer_holds() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("net"), "net", dir.path(), standard()).await;
        let stranger = |seq, target: &str, payload| Submission {
            signer: b"stranger".to_vec(),
            ..submit(seq, target, payload)
        };
        let sender = |receipt: &Receipt| match replies(receipt).as_slice() {
            [Reply::Env(env)] => env.sender.clone(),
            other => panic!("{other:?}"),
        };
        let env = || script(vec![Step::Env]);
        let applied = host
            .apply(block(
                1,
                vec![
                    submit(0, "probe", env()),
                    // a key that holds no account runs, as no one
                    stranger(0, "probe", env()),
                    // identity seats it; the next frame in the block sees it
                    stranger(1, "identity", abi::encode(&(b"stranger".to_vec(), 2u64))),
                    stranger(2, "probe", env()),
                ],
            ))
            .await
            .unwrap();
        let senders: Vec<_> = [0, 1, 3].map(|at| sender(&applied.submissions[at])).into();
        assert_eq!(
            senders,
            [
                Some(Principal::Account(ACCOUNT)),
                None,
                Some(Principal::Account(2))
            ]
        );

        // an identity that does not give programs their accounts founds
        // nothing
        let dir = tempfile::tempdir().unwrap();
        let mut broken = standard();
        broken.programs[2].code = PROBE.to_vec();
        let founded = Host::found(
            context.child("broken"),
            "broken",
            dir.path(),
            block_id(0),
            broken,
        )
        .await;
        assert!(
            matches!(founded, Err(Error::Genesis { .. })),
            "{:?}",
            founded.err()
        );
    });
}

#[test]
fn a_rejected_unit_leaves_no_writes_and_no_blobs() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        let put = || {
            op(HostOp::BlobPut {
                hash: HashKind::Sha256,
                kind: "page".into(),
                body: b"body".to_vec(),
            })
        };
        let id = blobs::id_of(HashKind::Sha256, &blobs::frame("page", b"body").unwrap());

        let applied = host
            .apply(block(
                1,
                vec![submit(
                    0,
                    "probe",
                    script(vec![set(b"k", b"v"), put(), Step::Fail("nope".into())]),
                )],
            ))
            .await
            .unwrap();
        assert_eq!(
            rejected(&applied.submissions[0]),
            &Refusal::new("probe", "nope")
        );
        let touched: Vec<&str> = applied.writes.programs.keys().map(String::as_str).collect();
        assert_eq!(touched, [NETWORK]);
        assert_eq!(
            host.view(Layer::Confirmed).get("probe", b"k").unwrap(),
            None
        );
        assert_eq!(host.blob(&id).unwrap(), None);

        let applied = host
            .apply(block(
                2,
                vec![submit(0, "probe", script(vec![set(b"k", b"v"), put()]))],
            ))
            .await
            .unwrap();
        assert_eq!(
            replies(&applied.submissions[0]),
            vec![
                Reply::Host(HostReply::Done),
                Reply::Host(HostReply::BlobId(id))
            ]
        );
        assert!(applied.writes.programs.contains_key(BLOBS));
        assert_eq!(host.blob(&id).unwrap(), Some(b"page 4\0body".to_vec()));
        assert!(host.missing_blobs().unwrap().is_empty());

        let reads = ask(
            &host,
            Layer::Confirmed,
            "probe",
            vec![
                op(HostOp::BlobGet(id)),
                op(HostOp::BlobStat(id)),
                op(HostOp::BlobRead {
                    id,
                    offset: 1,
                    len: 2,
                }),
                op(HostOp::BlobStat(BlobId::Sha1([0; 20]))),
                put(),
            ],
        )
        .await;
        assert_eq!(
            reads,
            vec![
                Reply::Host(HostReply::Blob(Some(Blob {
                    kind: "page".into(),
                    body: b"body".to_vec()
                }))),
                Reply::Host(HostReply::BlobHeader(Some(BlobHeader {
                    kind: "page".into(),
                    len: 4
                }))),
                Reply::Host(HostReply::Value(Some(b"od".to_vec()))),
                Reply::Host(HostReply::BlobHeader(None)),
                Reply::Host(HostReply::Refused(Refusal::new(
                    reason::UNSUPPORTED,
                    "a query does not write"
                ))),
            ]
        );

        let staged = blobs::id_of(HashKind::Sha1, &blobs::frame("note", b"x").unwrap());
        let applied = host
            .apply(block(
                3,
                vec![
                    submit(
                        1,
                        "probe",
                        script(vec![op(HostOp::BlobPut {
                            hash: HashKind::Sha1,
                            kind: "note".into(),
                            body: b"x".to_vec(),
                        })]),
                    ),
                    submit(2, "probe", script(vec![op(HostOp::BlobStat(staged))])),
                ],
            ))
            .await
            .unwrap();
        assert_eq!(
            replies(&applied.submissions[1]),
            vec![Reply::Host(HostReply::BlobHeader(Some(BlobHeader {
                kind: "note".into(),
                len: 1
            })))]
        );
        assert_eq!(host.blob(&staged).unwrap(), Some(b"note 1\0x".to_vec()));
    });
}

#[test]
fn an_epoch_is_recorded_as_the_block_ending_the_one_before_commits() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        let founding = vec![member(b"v1", "v1:1")];
        let seated = vec![member(b"v1", "v1:1"), member(b"v2", "v2:2")];

        host.apply(block(1, Vec::new())).await.unwrap();
        let applied = host
            .apply(block(2, vec![submit(0, "valset", abi::encode(&seated))]))
            .await
            .unwrap();
        assert_eq!(applied.submissions[0].outcome, ok(b""));
        assert_eq!(host.epoch_members(0).unwrap(), Some(founding.clone()));
        assert_eq!(host.epoch_members(1).unwrap(), None);

        host.apply(block(3, Vec::new())).await.unwrap();
        assert_eq!(host.epoch_members(0).unwrap(), Some(founding));
        assert_eq!(host.epoch_members(1).unwrap(), Some(seated.clone()));
        assert_eq!(host.epoch_members(2).unwrap(), None);
        assert_eq!(
            host.tip().unwrap(),
            Tip {
                height: 3,
                id: block_id(3)
            }
        );

        for height in 4..=6 {
            host.apply(block(height, Vec::new())).await.unwrap();
            assert_eq!(host.epoch_members(2).unwrap(), None);
        }
        host.apply(block(7, Vec::new())).await.unwrap();
        assert_eq!(host.epoch_members(2).unwrap(), Some(seated));
    });
}

#[test]
fn the_roster_admits_swaps_and_drops_programs() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        let programs = host.programs().unwrap();
        let relay = programs["ping"];
        let probe = programs["probe"];

        host.apply(block(
            1,
            vec![submit(
                0,
                "module-registry",
                change("echo", relay, Vec::new()),
            )],
        ))
        .await
        .unwrap();
        let applied = host.apply(block(2, Vec::new())).await.unwrap();
        assert_eq!(
            applied.admissions,
            vec![
                receipt("identity", ok(b""), vec![]),
                receipt("echo", ok(b""), vec![]),
            ]
        );
        let seventh = fixture_identity::MODULES_FROM + 6;
        assert_eq!(account_of(&host, "echo").await, Some(seventh));
        assert_eq!(host.programs().unwrap()["echo"], relay);
        let applied = host
            .apply(block(
                3,
                vec![submit(
                    1,
                    "echo",
                    send(vec![message("ping", note(b"hi"), false)]),
                )],
            ))
            .await
            .unwrap();
        assert_eq!(
            output(&applied.submissions[0]),
            abi::encode(&vec![item("echo", 0)])
        );

        host.apply(block(
            4,
            vec![submit(
                2,
                "module-registry",
                change("echo", probe, script(Vec::new())),
            )],
        ))
        .await
        .unwrap();
        let applied = host.apply(block(5, Vec::new())).await.unwrap();
        assert_eq!(applied.admissions, vec![receipt("echo", ok(b""), vec![])]);
        assert_eq!(host.programs().unwrap()["echo"], probe);
        let env = ask(&host, Layer::Confirmed, "echo", vec![Step::Env]).await;
        assert_eq!(
            env,
            vec![Reply::Env(Env {
                network: b"net".to_vec(),
                height: 5,
                time: TIME,
                me: "echo".into(),
                origin: Origin::External(SIGNER.to_vec()),
                sender: None,
                roles: roles(),
                cause: Cause::Direct,
            })]
        );

        host.apply(block(
            6,
            vec![submit(
                3,
                "module-registry",
                abi::encode(&Change::Remove("echo".into())),
            )],
        ))
        .await
        .unwrap();
        let applied = host.apply(block(7, Vec::new())).await.unwrap();
        assert!(applied.admissions.is_empty());
        assert!(!host.programs().unwrap().contains_key("echo"));
        let applied = host
            .apply(block(8, vec![submit(4, "echo", script(Vec::new()))]))
            .await
            .unwrap();
        assert_eq!(
            rejected(&applied.submissions[0]).reason,
            reason::UNKNOWN_PROGRAM
        );
        assert_eq!(
            host.query(Layer::Confirmed, TIME, Origin::System, "echo", Vec::new())
                .await
                .unwrap(),
            Err(Refusal::new(reason::UNKNOWN_PROGRAM, "echo"))
        );

        host.apply(block(
            9,
            vec![
                submit(
                    4,
                    "module-registry",
                    change("ghost", BlobId::Sha256([9; 32]), Vec::new()),
                ),
                submit(5, "module-registry", change("$evil", relay, Vec::new())),
                submit(
                    6,
                    "module-registry",
                    change("bad", probe, script(vec![Step::Fail("no".into())])),
                ),
            ],
        ))
        .await
        .unwrap();
        let applied = host.apply(block(10, Vec::new())).await.unwrap();
        // bad's account is given, then undone with its refused init
        assert_eq!(
            applied.admissions,
            vec![
                receipt("identity", ok(b""), vec![]),
                receipt(
                    "bad",
                    Outcome::Rejected(Refusal::new("probe", "no")),
                    vec![]
                ),
            ]
        );
        assert_eq!(account_of(&host, "bad").await, None);
        let programs = host.programs().unwrap();
        let ids: Vec<&str> = programs.keys().map(String::as_str).collect();
        assert_eq!(
            ids,
            [
                "identity",
                "module-registry",
                "ping",
                "pong",
                "probe",
                "valset"
            ]
        );
        // retried: registered and refused again
        let applied = host.apply(block(11, Vec::new())).await.unwrap();
        assert_eq!(applied.admissions.len(), 2);
    });
}

/// A later install whose account identity refuses does not run: its init
/// never runs, the receipt says so, and the next height admits it once
/// identity gives it its account.
#[test]
fn a_program_identity_gives_no_account_is_not_admitted() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        let relay = host.programs().unwrap()["ping"];
        let closed = |seq, number: u64| {
            submit(
                seq,
                "identity",
                abi::encode(&(b"#closed/late".to_vec(), number)),
            )
        };
        host.apply(block(
            1,
            vec![
                closed(0, 1),
                submit(1, "module-registry", change("late", relay, Vec::new())),
            ],
        ))
        .await
        .unwrap();
        for height in [2, 3] {
            let applied = host.apply(block(height, Vec::new())).await.unwrap();
            assert_eq!(
                applied.admissions,
                vec![receipt(
                    "identity",
                    Outcome::Rejected(Refusal::new("closed", "late")),
                    vec![]
                )]
            );
            assert!(!host.programs().unwrap().contains_key("late"));
            assert_eq!(account_of(&host, "late").await, None);
        }
        let applied = host
            .apply(block(
                4,
                vec![submit(
                    2,
                    "late",
                    send(vec![message("ping", note(b"hi"), false)]),
                )],
            ))
            .await
            .unwrap();
        assert_eq!(
            rejected(&applied.submissions[0]).reason,
            reason::UNKNOWN_PROGRAM
        );

        // the refused submission kept the signer's sequence
        host.apply(block(5, vec![closed(2, 0)])).await.unwrap();
        let applied = host.apply(block(6, Vec::new())).await.unwrap();
        assert_eq!(
            applied.admissions,
            vec![
                receipt("identity", ok(b""), vec![]),
                receipt("late", ok(b""), vec![]),
            ]
        );
        assert_eq!(host.programs().unwrap()["late"], relay);
        assert!(account_of(&host, "late").await.is_some());
    });
}

#[test]
fn queries_read_layers_and_the_preconfirmed_layer_dies_at_commit() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        host.apply(block(
            1,
            vec![submit(0, "probe", script(vec![set(b"a", b"1")]))],
        ))
        .await
        .unwrap();
        let receipts = host
            .preconfirm(
                TIME,
                vec![submit(
                    1,
                    "probe",
                    script(vec![set(b"a", b"2"), set(b"b", b"3")]),
                )],
            )
            .await
            .unwrap();
        assert_eq!(
            receipts,
            vec![receipt(
                "probe",
                ok(&abi::encode(&vec![Reply::Host(HostReply::Done); 2])),
                vec![]
            )]
        );

        let steps = || {
            vec![
                get(b"a"),
                get(b"b"),
                op(HostOp::CommittedGet(b"a".to_vec())),
                op(HostOp::Scan(Scan::prefix(b""))),
                Step::Env,
            ]
        };
        let entry = |key: &[u8], value: &[u8]| Entry {
            key: key.to_vec(),
            value: value.to_vec(),
        };
        assert_eq!(
            ask(&host, Layer::Confirmed, "probe", steps()).await,
            vec![
                Reply::Host(HostReply::Value(Some(b"1".to_vec()))),
                Reply::Host(HostReply::Value(None)),
                Reply::Host(HostReply::Value(Some(b"1".to_vec()))),
                Reply::Host(HostReply::Entries(vec![entry(b"a", b"1")])),
                Reply::Env(Env {
                    network: b"net".to_vec(),
                    height: 1,
                    time: TIME,
                    me: "probe".into(),
                    origin: Origin::External(SIGNER.to_vec()),
                    sender: None,
                    roles: roles(),
                    cause: Cause::Direct,
                }),
            ]
        );
        assert_eq!(
            ask(&host, Layer::Preconfirmed, "probe", steps()).await,
            vec![
                Reply::Host(HostReply::Value(Some(b"2".to_vec()))),
                Reply::Host(HostReply::Value(Some(b"3".to_vec()))),
                Reply::Host(HostReply::Value(Some(b"1".to_vec()))),
                Reply::Host(HostReply::Entries(vec![
                    entry(b"a", b"2"),
                    entry(b"b", b"3")
                ])),
                Reply::Env(Env {
                    network: b"net".to_vec(),
                    height: 2,
                    time: TIME,
                    me: "probe".into(),
                    origin: Origin::External(SIGNER.to_vec()),
                    sender: None,
                    roles: roles(),
                    cause: Cause::Direct,
                }),
            ]
        );
        assert_eq!(
            host.view(Layer::Preconfirmed).get("probe", b"b").unwrap(),
            Some(b"3".to_vec())
        );

        host.apply(block(2, Vec::new())).await.unwrap();
        assert_eq!(
            ask(
                &host,
                Layer::Preconfirmed,
                "probe",
                vec![get(b"a"), get(b"b")]
            )
            .await,
            vec![
                Reply::Host(HostReply::Value(Some(b"1".to_vec()))),
                Reply::Host(HostReply::Value(None))
            ]
        );
    });
}

#[test]
fn sibling_queries_route_by_id_and_a_cycle_is_refused() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let twins = genesis(vec![
            founding("ping", RELAY, Vec::new()),
            founding("probe", PROBE, script(Vec::new())),
            founding("twin", PROBE, script(Vec::new())),
        ]);
        let mut host = found(context, "net", dir.path(), twins).await;
        host.apply(block(
            1,
            vec![submit(0, "probe", script(vec![set(b"k", b"v")]))],
        ))
        .await
        .unwrap();

        let query = |program: &str, steps: Vec<Step>| {
            op(HostOp::Query {
                program: program.to_owned(),
                request: script(steps),
            })
        };
        assert_eq!(
            ask(
                &host,
                Layer::Confirmed,
                "twin",
                vec![
                    query("probe", vec![get(b"k"), Step::Env]),
                    query("nobody", Vec::new()),
                    op(HostOp::Root("ping".into())),
                    op(HostOp::Root("nobody".into())),
                ]
            )
            .await,
            vec![
                Reply::Host(HostReply::Query(Ok(abi::encode(&vec![
                    Reply::Host(HostReply::Value(Some(b"v".to_vec()))),
                    Reply::Env(Env {
                        network: b"net".to_vec(),
                        height: 1,
                        time: TIME,
                        me: "probe".into(),
                        origin: Origin::Program("twin".into()),
                        sender: None,
                        roles: roles(),
                        cause: Cause::Direct,
                    }),
                ])))),
                Reply::Host(HostReply::Query(Err(Refusal::new(
                    reason::UNKNOWN_PROGRAM,
                    "nobody"
                )))),
                Reply::Host(HostReply::Root(host.store().root("ping").unwrap())),
                Reply::Host(HostReply::Root(None)),
            ]
        );

        let applied = host
            .apply(block(
                2,
                vec![submit(
                    1,
                    "probe",
                    script(vec![
                        set(b"k2", b"v2"),
                        query(
                            "twin",
                            vec![query("probe", vec![get(b"k2"), query("twin", Vec::new())])],
                        ),
                    ]),
                )],
            ))
            .await
            .unwrap();
        let cycle = Refusal::new(
            reason::PROTOCOL,
            "twin is already answering a query on this stack",
        );
        let innermost = vec![
            Reply::Host(HostReply::Value(Some(b"v2".to_vec()))),
            Reply::Host(HostReply::Query(Err(cycle))),
        ];
        let twin = vec![Reply::Host(HostReply::Query(Ok(abi::encode(&innermost))))];
        assert_eq!(
            replies(&applied.submissions[0]),
            vec![
                Reply::Host(HostReply::Done),
                Reply::Host(HostReply::Query(Ok(abi::encode(&twin))))
            ]
        );
    });
}

#[test]
fn crypto_verifies_every_scheme() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let host = found(context, "net", dir.path(), standard()).await;
        let namespace = b"ducktape:test".to_vec();
        let message = b"the message".to_vec();

        let ed = ed25519::PrivateKey::from_seed(7);
        let ed_key = ed.public_key().as_ref().to_vec();
        let ed_sig = testkit::ed25519_proof(&ed, &namespace, &message);
        let eth = testkit::eth_key(7);
        let k_key = testkit::eth_pubkey(&eth);
        let k_sig = testkit::eth_proof(&eth, &namespace, &message);
        let passkey = testkit::passkey(7);
        let p_key = testkit::passkey_pubkey(&passkey);
        let p_sig = testkit::passkey_proof(&passkey, "ducktape.test", &namespace, &message, true);
        let (b_key, b_sig) = {
            let private = Private::new(Scalar::from_u64(7));
            let public = ops::compute_public::<MinPk>(&private);
            let signature = ops::sign_message::<MinPk>(&private, &namespace, &message);
            (public.encode().to_vec(), signature.encode().to_vec())
        };
        let verify = |scheme: Scheme, key: &[u8], message: &[u8], signature: &[u8]| {
            op(HostOp::Crypto(CryptoOp::Verify {
                scheme,
                key: key.to_vec(),
                namespace: namespace.clone(),
                message: message.to_vec(),
                signature: signature.to_vec(),
            }))
        };
        let steps = vec![
            op(HostOp::Crypto(CryptoOp::Sha256(b"abc".to_vec()))),
            verify(Scheme::Ed25519, &ed_key, &message, &ed_sig),
            verify(Scheme::Secp256k1, &k_key, &message, &k_sig),
            verify(Scheme::Secp256r1, &p_key, &message, &p_sig),
            verify(Scheme::Bls12381, &b_key, &message, &b_sig),
            verify(Scheme::Ed25519, &ed_key, b"other", &ed_sig),
            verify(Scheme::Secp256k1, &k_key, b"other", &k_sig),
            verify(Scheme::Secp256r1, &p_key, b"other", &p_sig),
            verify(Scheme::Bls12381, &b_key, b"other", &b_sig),
            verify(Scheme::Ed25519, b"short", &message, &ed_sig),
            verify(Scheme::Bls12381, &ed_key, &message, &b_sig),
        ];
        let verdicts = ask(&host, Layer::Confirmed, "probe", steps).await;
        let expected: Vec<Reply> = std::iter::once(HostReply::Crypto(CryptoReply::Digest(
            sha2::Sha256::digest(b"abc").into(),
        )))
        .chain(
            [
                true, true, true, true, false, false, false, false, false, false,
            ]
            .map(|valid| HostReply::Crypto(CryptoReply::Verified(valid))),
        )
        .map(Reply::Host)
        .collect();
        assert_eq!(verdicts, expected);
    });
}

#[test]
fn fuel_is_a_network_parameter() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut metered = standard();
        metered.limits = Limits {
            fuel: Some(10),
            memory_bytes: None,
        };
        let Err(Error::Genesis { program, refusal }) = Host::found(
            context.child("starved"),
            "starved",
            dir.path(),
            block_id(0),
            metered,
        )
        .await
        else {
            panic!("founding ran a program on ten fuel");
        };
        assert_eq!(program, "module-registry");
        assert_eq!(refusal.reason, reason::TRAP);

        let dir = tempfile::tempdir().unwrap();
        let limits = Limits {
            fuel: Some(50_000_000),
            memory_bytes: Some(64 << 20),
        };
        let mut roomy = standard();
        roomy.limits = limits;
        let mut host = found(context.child("roomy"), "roomy", dir.path(), roomy).await;
        assert_eq!(
            host.view(Layer::Confirmed).get(NETWORK, b"limits").unwrap(),
            Some(abi::encode(&limits))
        );
        let applied = host
            .apply(block(
                1,
                vec![
                    submit(0, "probe", script(vec![Step::Env])),
                    submit(1, "probe", script(vec![Step::Spin])),
                    submit(1, "probe", script(vec![Step::Grow(2048)])),
                ],
            ))
            .await
            .unwrap();
        assert_eq!(replies(&applied.submissions[0]).len(), 1);
        assert_eq!(rejected(&applied.submissions[1]).reason, reason::TRAP);
        assert_eq!(rejected(&applied.submissions[2]).reason, reason::TRAP);
    });
}

#[test]
fn a_joiner_adopts_synced_commitments_and_installs_the_blobs_it_lacks() {
    deterministic::Runner::default().start(|context| async move {
        let upstream_dir = tempfile::tempdir().unwrap();
        let mut upstream = found(
            context.child("upstream"),
            "up",
            upstream_dir.path(),
            standard(),
        )
        .await;
        upstream
            .apply(block(
                1,
                vec![submit(
                    0,
                    "probe",
                    script(vec![
                        set(b"k", b"v"),
                        op(HostOp::BlobPut {
                            hash: HashKind::Sha256,
                            kind: "page".into(),
                            body: b"body".to_vec(),
                        }),
                    ]),
                )],
            ))
            .await
            .unwrap();
        let height = upstream.height().unwrap();
        let root = upstream.root().unwrap();
        let programs = upstream.programs().unwrap();
        let mut framed = BTreeMap::new();
        for entry in upstream
            .view(Layer::Confirmed)
            .scan(BLOBS, &Scan::prefix(b""))
            .unwrap()
        {
            let id: BlobId = abi::decode(&entry.key).unwrap();
            framed.insert(id, upstream.blob(&id).unwrap().unwrap());
        }
        assert_eq!(framed.len(), 6);
        let targets: BTreeMap<String, SyncTarget> = upstream
            .store()
            .programs()
            .map(|name| {
                let target = upstream
                    .store()
                    .commitment(name)
                    .unwrap()
                    .target()
                    .unwrap()
                    .unwrap();
                (name.clone(), target)
            })
            .collect();
        let (_, commitments) = upstream.into_store().into_parts();

        let mut synced = BTreeMap::new();
        for (name, commitment) in commitments {
            let target = targets[&name].clone();
            let source = Arc::new(commitment.into_db().unwrap());
            let commitment = Commitment::sync_from(
                context
                    .child("sync")
                    .with_attribute("program", name.as_str()),
                &commitment_name("join", &name),
                target,
                source,
            )
            .await
            .unwrap();
            synced.insert(name, commitment);
        }

        let joiner_dir = tempfile::tempdir().unwrap();
        let mut joiner = Host::adopt(
            context.child("joiner"),
            "join",
            joiner_dir.path(),
            height,
            synced,
        )
        .await
        .unwrap();
        assert_eq!(joiner.height().unwrap(), height);
        assert_eq!(joiner.root().unwrap(), root);
        assert_eq!(joiner.programs().unwrap(), programs);
        let mut missing = joiner.missing_blobs().unwrap();
        missing.sort();
        let mut wanted: Vec<BlobId> = framed.keys().copied().collect();
        wanted.sort();
        assert_eq!(missing, wanted);
        assert!(matches!(
            joiner.apply(block(2, Vec::new())).await,
            Err(Error::BlobUnavailable(_))
        ));
        assert!(matches!(
            joiner
                .query(Layer::Confirmed, TIME, Origin::System, "probe", Vec::new())
                .await,
            Err(Error::BlobUnavailable(_))
        ));

        let page = blobs::id_of(HashKind::Sha256, &blobs::frame("page", b"body").unwrap());
        assert!(matches!(
            joiner.install(page, b"page 4\0nope"),
            Err(Error::Corrupt(_))
        ));
        for (id, bytes) in &framed {
            joiner.install(*id, bytes).unwrap();
        }
        assert!(joiner.missing_blobs().unwrap().is_empty());
        let applied = joiner
            .apply(block(
                2,
                vec![submit(
                    1,
                    "probe",
                    script(vec![get(b"k"), op(HostOp::BlobStat(page))]),
                )],
            ))
            .await
            .unwrap();
        assert_eq!(
            replies(&applied.submissions[0]),
            vec![
                Reply::Host(HostReply::Value(Some(b"v".to_vec()))),
                Reply::Host(HostReply::BlobHeader(Some(BlobHeader {
                    kind: "page".into(),
                    len: 4
                }))),
            ]
        );
    });
}

#[test]
fn a_signer_submits_in_sequence_and_a_refusal_keeps_the_sequence() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        let applied = host
            .apply(block(
                1,
                vec![
                    submit(1, "probe", script(vec![set(b"k", b"early")])),
                    submit(0, "probe", script(vec![set(b"k", b"first")])),
                    submit(0, "probe", script(vec![set(b"k", b"replay")])),
                    submit(1, "probe", script(vec![Step::Fail("no".into())])),
                    submit(1, "probe", script(vec![set(b"k", b"second")])),
                ],
            ))
            .await
            .unwrap();
        assert_eq!(rejected(&applied.submissions[0]).reason, reason::SEQUENCE);
        assert_eq!(
            replies(&applied.submissions[1]),
            vec![Reply::Host(HostReply::Done)]
        );
        assert_eq!(rejected(&applied.submissions[2]).reason, reason::SEQUENCE);
        assert_eq!(rejected(&applied.submissions[3]).reason, "probe");
        assert_eq!(
            replies(&applied.submissions[4]),
            vec![Reply::Host(HostReply::Done)]
        );
        let view = host.view(Layer::Confirmed);
        assert_eq!(view.get("probe", b"k").unwrap(), Some(b"second".to_vec()));
        assert_eq!(view.get(SIGNERS, SIGNER).unwrap(), Some(abi::encode(&2u64)));

        let receipts = host
            .preconfirm(
                TIME,
                vec![
                    submit(2, "probe", script(Vec::new())),
                    submit(2, "probe", script(Vec::new())),
                ],
            )
            .await
            .unwrap();
        assert!(matches!(receipts[0].outcome, Outcome::Applied { .. }));
        assert_eq!(rejected(&receipts[1]).reason, reason::SEQUENCE);
        assert_eq!(
            host.view(Layer::Preconfirmed).get(SIGNERS, SIGNER).unwrap(),
            Some(abi::encode(&3u64))
        );
        assert_eq!(
            host.view(Layer::Confirmed).get(SIGNERS, SIGNER).unwrap(),
            Some(abi::encode(&2u64))
        );
    });
}
