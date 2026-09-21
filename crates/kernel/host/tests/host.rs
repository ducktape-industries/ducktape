use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use abi::{
    Blob, BlobHeader, BlobId, Cause, CryptoOp, CryptoReply, Entry, Env, HashKind, HostOp,
    HostReply, ItemRef, Message, Origin, Outcome, Refusal, Scan, Scheme, module_registry, reason,
    valset,
};
use commonware_codec::Encode as _;
use commonware_cryptography::bls12381::primitives::group::{Private, Scalar};
use commonware_cryptography::bls12381::primitives::ops;
use commonware_cryptography::bls12381::primitives::variant::MinPk;
use commonware_cryptography::{Signer as _, ed25519};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use fixture_module_registry::Change;
use fixture_probe::{Reply, Step};
use host::{
    BLOBS, Block, BlockId, Delivered, Error, Founding, Genesis, Host, Layer, Limits, NETWORK,
    QUEUE, Receipt, SIGNERS, Submission, Tip,
};
use keyscheme::testkit;
use sha2::Digest as _;
use state::{Commitment, SyncTarget, commitment_name};

const MODULE_REGISTRY: &[u8] = include_bytes!("../../fixtures/wasm/fixture_module_registry.wasm");
const VALSET: &[u8] = include_bytes!("../../fixtures/wasm/fixture_valset.wasm");
const RELAY: &[u8] = include_bytes!("../../fixtures/wasm/fixture_relay.wasm");
const PROBE: &[u8] = include_bytes!("../../fixtures/wasm/fixture_probe.wasm");

const SIGNER: &[u8] = b"signer";
const EPOCH_LENGTH: u64 = 4;
const TIME: u64 = 1_700_000_000;

type Ctx = deterministic::Context;

fn member(key: &[u8], address: &str) -> valset::Member {
    valset::Member {
        key: key.to_vec(),
        address: address.to_owned(),
    }
}

fn seated(validators: &[valset::Member]) -> valset::Seating {
    valset::Seating {
        validators: validators.iter().map(|member| member.key.clone()).collect(),
        members: validators.to_vec(),
    }
}

fn founding(program: &str, code: &[u8], params: Vec<u8>) -> Founding {
    Founding {
        program: program.to_owned(),
        code: code.to_vec(),
        params,
    }
}

fn genesis(programs: Vec<Founding>) -> Genesis {
    Genesis {
        network: b"net".to_vec(),
        module_registry: MODULE_REGISTRY.to_vec(),
        valset: VALSET.to_vec(),
        validators: vec![member(b"v1", "v1:1")],
        programs,
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

fn message(target: &str, payload: &[u8], reply: bool) -> Vec<u8> {
    abi::encode(&Message {
        target: target.to_owned(),
        payload: payload.to_vec(),
        reply,
    })
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
    }
}

fn ok(output: &[u8]) -> Outcome {
    Outcome::Applied {
        output: output.to_vec(),
    }
}

fn change(program: &str, code: BlobId, params: Vec<u8>) -> Vec<u8> {
    abi::encode(&Change::Set(module_registry::Entry {
        program: program.to_owned(),
        code,
        params,
    }))
}

#[test]
fn founding_admits_every_program_and_the_host_reopens() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (host, applied) = Host::found(
            context.child("found"),
            "net",
            dir.path(),
            block_id(0),
            standard(),
        )
        .await
        .unwrap();
        assert_eq!(applied.height, 0);
        let admitted: Vec<&str> = applied
            .admissions
            .iter()
            .map(|r| r.program.as_str())
            .collect();
        assert_eq!(
            admitted,
            ["module-registry", "valset", "ping", "pong", "probe"]
        );
        assert!(applied.deliveries.is_empty());
        for receipt in &applied.admissions {
            assert!(
                matches!(receipt.outcome, Outcome::Applied { .. }),
                "{receipt:?}"
            );
        }
        let programs = host.programs().unwrap();
        let ids: Vec<&str> = programs.keys().map(String::as_str).collect();
        assert_eq!(ids, ["module-registry", "ping", "pong", "probe", "valset"]);
        assert_eq!(programs["ping"], programs["pong"]);
        assert_ne!(programs["ping"], programs["probe"]);
        assert_eq!(
            host.epoch_seating(0).unwrap(),
            Some(seated(&[member(b"v1", "v1:1")]))
        );
        assert_eq!(host.epoch_seating(1).unwrap(), None);
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
            reopened.epoch_seating(0).unwrap(),
            Some(seated(&[member(b"v1", "v1:1")]))
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
                cause: Cause::Direct,
            })]
        );
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
fn a_call_is_delivered_next_block_and_completes_the_block_after() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        let first = item("ping", 0);

        let applied = host
            .apply(block(
                1,
                vec![submit(0, "ping", message("pong", b"hello", true))],
            ))
            .await
            .unwrap();
        assert_eq!(
            applied.submissions,
            vec![receipt("ping", ok(&abi::encode(&first)), vec![])]
        );
        assert!(applied.deliveries.is_empty());
        assert_eq!(
            host.view(Layer::Confirmed)
                .get("ping", &fixture_relay::sent(&first))
                .unwrap(),
            Some(b"hello".to_vec())
        );

        let applied = host.apply(block(2, Vec::new())).await.unwrap();
        assert_eq!(
            applied.deliveries,
            vec![Delivered {
                item: 0,
                receipt: receipt("pong", ok(b"olleh"), vec![b"hello"]),
            }]
        );
        assert_eq!(
            host.view(Layer::Confirmed)
                .get("pong", &fixture_relay::got(&first))
                .unwrap(),
            Some(b"hello".to_vec())
        );

        let applied = host.apply(block(3, Vec::new())).await.unwrap();
        assert_eq!(
            applied.deliveries,
            vec![Delivered {
                item: 1,
                receipt: receipt("ping", ok(b""), vec![]),
            }]
        );
        assert_eq!(
            host.view(Layer::Confirmed)
                .get("ping", &fixture_relay::done(&first))
                .unwrap(),
            Some(abi::encode(&ok(b"olleh")))
        );

        let applied = host.apply(block(4, Vec::new())).await.unwrap();
        assert!(applied.deliveries.is_empty());
        assert!(
            host.view(Layer::Confirmed)
                .scan(QUEUE, &Scan::prefix(b"i/"))
                .unwrap()
                .is_empty()
        );

        let applied = host
            .apply(block(
                5,
                vec![submit(1, "ping", message("pong", b"again", false))],
            ))
            .await
            .unwrap();
        assert_eq!(
            output(&applied.submissions[0]),
            abi::encode(&item("ping", 2))
        );
        let applied = host.apply(block(6, Vec::new())).await.unwrap();
        assert_eq!(
            applied.deliveries,
            vec![Delivered {
                item: 2,
                receipt: receipt("pong", ok(b"niaga"), vec![b"again"]),
            }]
        );
        let applied = host.apply(block(7, Vec::new())).await.unwrap();
        assert!(applied.deliveries.is_empty());
    });
}

#[test]
fn a_delivery_sees_who_emitted_it() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        let probe_script = script(vec![Step::Env]);
        host.apply(block(
            1,
            vec![submit(0, "ping", message("probe", &probe_script, true))],
        ))
        .await
        .unwrap();
        let applied = host.apply(block(2, Vec::new())).await.unwrap();
        let env = Env {
            network: b"net".to_vec(),
            height: 2,
            time: TIME + 2,
            me: "probe".into(),
            origin: Origin::Program("ping".into()),
            cause: Cause::Delivery(item("ping", 0)),
        };
        assert_eq!(
            replies(&applied.deliveries[0].receipt),
            vec![Reply::Env(env.clone())]
        );
        let applied = host.apply(block(3, Vec::new())).await.unwrap();
        assert_eq!(applied.deliveries[0].receipt.program, "ping");
        assert_eq!(
            host.view(Layer::Confirmed)
                .get("ping", &fixture_relay::done(&item("ping", 0)))
                .unwrap(),
            Some(abi::encode(&ok(&abi::encode(&vec![Reply::Env(env)]))))
        );
    });
}

#[test]
fn a_refused_or_unroutable_delivery_completes_with_its_refusal() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        let applied = host
            .apply(block(
                1,
                vec![
                    submit(0, "ping", message("pong", fixture_relay::FAIL, true)),
                    submit(1, "ping", message("nobody", b"x", true)),
                    submit(2, "nobody", b"x".to_vec()),
                ],
            ))
            .await
            .unwrap();
        assert_eq!(
            rejected(&applied.submissions[2]),
            &Refusal::new(reason::UNKNOWN_PROGRAM, "nobody")
        );

        let applied = host.apply(block(2, Vec::new())).await.unwrap();
        let refused = Refusal::new(reason::INVALID_INPUT, "asked to fail");
        assert_eq!(
            applied.deliveries,
            vec![
                Delivered {
                    item: 0,
                    receipt: receipt("pong", Outcome::Rejected(refused.clone()), vec![]),
                },
                Delivered {
                    item: 1,
                    receipt: receipt(
                        "nobody",
                        Outcome::Rejected(Refusal::new(reason::UNKNOWN_PROGRAM, "nobody")),
                        vec![],
                    ),
                },
            ]
        );
        assert_eq!(
            host.view(Layer::Confirmed)
                .get("pong", &fixture_relay::got(&item("ping", 0)))
                .unwrap(),
            None
        );

        let applied = host.apply(block(3, Vec::new())).await.unwrap();
        assert_eq!(applied.deliveries.len(), 2);
        let view = host.view(Layer::Confirmed);
        assert_eq!(
            view.get("ping", &fixture_relay::done(&item("ping", 0)))
                .unwrap(),
            Some(abi::encode(&Outcome::Rejected(refused)))
        );
        assert_eq!(
            view.get("ping", &fixture_relay::done(&item("ping", 1)))
                .unwrap(),
            Some(abi::encode(&Outcome::Rejected(Refusal::new(
                reason::UNKNOWN_PROGRAM,
                "nobody"
            ))))
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
        let reseated = vec![member(b"v1", "v1:1"), member(b"v2", "v2:2")];

        host.apply(block(1, Vec::new())).await.unwrap();
        let applied = host
            .apply(block(2, vec![submit(0, "valset", abi::encode(&reseated))]))
            .await
            .unwrap();
        assert_eq!(applied.submissions[0].outcome, ok(b""));
        assert_eq!(host.epoch_seating(0).unwrap(), Some(seated(&founding)));
        assert_eq!(host.epoch_seating(1).unwrap(), None);

        host.apply(block(3, Vec::new())).await.unwrap();
        assert_eq!(host.epoch_seating(0).unwrap(), Some(seated(&founding)));
        assert_eq!(host.epoch_seating(1).unwrap(), Some(seated(&reseated)));
        assert_eq!(host.epoch_seating(2).unwrap(), None);
        assert_eq!(
            host.tip().unwrap(),
            Tip {
                height: 3,
                id: block_id(3)
            }
        );

        for height in 4..=6 {
            host.apply(block(height, Vec::new())).await.unwrap();
            assert_eq!(host.epoch_seating(2).unwrap(), None);
        }
        host.apply(block(7, Vec::new())).await.unwrap();
        assert_eq!(host.epoch_seating(2).unwrap(), Some(seated(&reseated)));
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
        assert_eq!(applied.admissions, vec![receipt("echo", ok(b""), vec![])]);
        assert_eq!(host.programs().unwrap()["echo"], relay);
        let applied = host
            .apply(block(
                3,
                vec![submit(1, "echo", message("ping", b"hi", false))],
            ))
            .await
            .unwrap();
        assert_eq!(
            output(&applied.submissions[0]),
            abi::encode(&item("echo", 0))
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
        assert_eq!(
            applied.admissions,
            vec![receipt(
                "bad",
                Outcome::Rejected(Refusal::new("probe", "no")),
                vec![]
            )]
        );
        let programs = host.programs().unwrap();
        let ids: Vec<&str> = programs.keys().map(String::as_str).collect();
        assert_eq!(ids, ["module-registry", "ping", "pong", "probe", "valset"]);
        let applied = host.apply(block(11, Vec::new())).await.unwrap();
        assert_eq!(applied.admissions.len(), 1);
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
        assert_eq!(framed.len(), 5);
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
