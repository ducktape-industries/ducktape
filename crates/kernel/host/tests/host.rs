use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use abi::Receipt;
use abi::{
    Blob, BlobHeader, BlobId, Cause, CryptoOp, CryptoReply, Entry, Env, HashKind, HostOp,
    HostReply, ItemRef, Message, Origin, Outcome, Principal, Refusal, Scan, Scheme, hook, reason,
    role::{self, halo, identity, programs, validators},
};
use blob::contract as blobs;
use commonware_codec::Encode as _;
use commonware_cryptography::bls12381::primitives::group::{Private, Scalar};
use commonware_cryptography::bls12381::primitives::ops;
use commonware_cryptography::bls12381::primitives::variant::MinPk;
use commonware_cryptography::{Signer as _, ed25519};
use commonware_runtime::{Runner as _, Storage as _, Supervisor as _, deterministic};
use fixture_probe::{Reply, Step};
use fixture_programs::Change;
use fixture_relay::Script;
use fixture_wasm::{
    ACCESS, AWAITS, BLOBS as BLOBS_PROGRAM, HALO, IDENTITY, PROBE, PROGRAMS, RELAY, VALSET,
};
use host::{
    ANCHOR_WINDOW, BLOBS, Block, BlockId, Error, Founding, FoundingView, Genesis, Host, Layer,
    Limits, NETWORK, RECENT, Roles, SPENT, Submission, Tip,
};
use keyscheme::testkit;
use sha2::Digest as _;
use state::{Commitment, SyncTarget, commitment_name};

const SIGNER: &[u8] = b"signer";
/// Fuel that runs one relay handler and a message of it, not a chain of eight.
const FRAME_FUEL: u64 = 150_000;
const EPOCH_LENGTH: u64 = 4;
const TIME: u64 = 1_700_000_000;

type Ctx = deterministic::Context;

fn member(key: &[u8], address: &str) -> validators::Member {
    validators::Member {
        key: key.to_vec(),
        address: Some(address.to_owned()),
        halo: address.to_owned(),
    }
}

fn founding(program: &str, code: &[u8], params: Vec<u8>) -> Founding {
    Founding {
        program: program.to_owned(),
        code: code.to_vec(),
        params,
        roles: Vec::new(),
    }
}

fn filling(role: &str, mut founding: Founding) -> Founding {
    founding.roles.push(role.to_owned());
    founding
}

/// The account [`SIGNER`] holds.
const ACCOUNT: u64 = 1;

fn roles() -> Roles {
    Roles::from_iter([
        (role::PROGRAMS, "programs"),
        (role::VALIDATORS, "valset"),
        (role::IDENTITY, "identity"),
        (role::BLOBS, "blobs"),
        (role::AWAITS, "awaits"),
        (role::HALO, "halo"),
        (role::ACCESS, "access"),
    ])
}

const PING: u64 = fixture_identity::PROGRAMS_FROM + 5;

/// The lanes the halo role is founded with: `first` pinned, `second` not.
fn lanes() -> Vec<u8> {
    let founded = |name: &str, cap, pinned| halo::Founded {
        name: name.into(),
        cap,
        pinned,
    };
    abi::encode(&halo::Genesis {
        lanes: vec![
            founded("first", halo::Class::Critical, true),
            founded("second", halo::Class::Client, false),
        ],
    })
}

/// The programs role, the validators and identity (where [`SIGNER`] holds
/// [`ACCOUNT`]), then `programs`.
fn genesis(programs: Vec<Founding>) -> Genesis {
    let bound = vec![
        filling(role::PROGRAMS, founding("programs", PROGRAMS, Vec::new())),
        filling(role::VALIDATORS, founding("valset", VALSET, Vec::new())),
        filling(
            role::IDENTITY,
            founding(
                "identity",
                IDENTITY,
                abi::encode(&vec![(SIGNER.to_vec(), ACCOUNT)]),
            ),
        ),
        filling(role::BLOBS, founding("blobs", BLOBS_PROGRAM, Vec::new())),
    ];
    Genesis {
        network: b"net".to_vec(),
        validators: vec![member(b"v1", "v1:1")],
        programs: bound
            .into_iter()
            .chain(programs)
            .chain([
                filling(role::AWAITS, founding("awaits", AWAITS, Vec::new())),
                filling(role::HALO, founding("halo", HALO, lanes())),
                filling(role::ACCESS, founding("access", ACCESS, Vec::new())),
            ])
            .collect(),
        views: Vec::new(),
        hooks: Vec::new(),
        limits: Limits::default(),
        epoch_length: EPOCH_LENGTH,
        time: TIME,
        member_cap: 16,
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

/// Restarts `host` over its directory and asserts the restarted node's root
/// is the running node's: a commitment the running node still holds that a
/// reopened one would not open is a root the two disagree on.
async fn restart(context: Ctx, dir: &Path, host: Host<Ctx>) -> Host<Ctx> {
    let root = host.root().unwrap();
    drop(host);
    let reopened = Host::open(context, "net", dir).await.unwrap();
    assert_eq!(reopened.root().unwrap(), root);
    reopened
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

fn submit(target: &str, payload: Vec<u8>) -> Submission {
    Submission {
        signer: SIGNER.to_vec(),
        anchor: anchor(0),
        salt: None,
        target: target.to_owned(),
        payload,
    }
}

fn enroll(key: &[u8], address: &str) -> Submission {
    let enroll = validators::Op::Enroll {
        address: Some(address.to_owned()),
        halo: address.to_owned(),
        invite: None,
    };
    Submission {
        signer: key.to_vec(),
        ..submit("valset", abi::encode(&enroll))
    }
}

fn promote(key: &[u8]) -> Submission {
    let motion = validators::Motion::Promote { key: key.to_vec() };
    submit("valset", abi::encode(&validators::Op::Vote(motion)))
}

fn anchor(height: u64) -> Tip {
    Tip {
        height,
        id: block_id(height),
    }
}

fn anchored(height: u64, submission: Submission) -> Submission {
    Submission {
        anchor: anchor(height),
        ..submission
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
        acting: None,
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
    let asked = identity::Query::OfProgram(program.into());
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
    abi::encode(&Change::Set(programs::Entry {
        program: program.to_owned(),
        code,
        params,
    }))
}

fn fill(role: &str, program: Option<&str>) -> Submission {
    let change = Change::Fill {
        role: role.to_owned(),
        program: program.map(str::to_owned),
    };
    submit("programs", abi::encode(&change))
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
            "programs", "valset", "identity", "blobs", "access", "ping", "pong", "probe",
        ];
        // the roles' admissions and accounts, then each other program's
        // account before its admission
        assert_eq!(admitted[..5], founded[..5]);
        assert_eq!(admitted[5..10], ["identity"; 5]);
        for (pair, program) in admitted[10..].chunks(2).zip(&founded[5..]) {
            assert_eq!(pair, ["identity", *program]);
        }
        for (at, program) in founded.into_iter().enumerate() {
            let number = fixture_identity::PROGRAMS_FROM + at as u64;
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
                "access", "awaits", "blobs", "halo", "identity", "ping", "pong", "probe",
                "programs", "valset"
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
fn founding_refuses_a_role_two_programs_fill() {
    deterministic::Runner::default().start(|context| async move {
        for twice in [role::IDENTITY, role::ACCESS, role::EGRESS] {
            let dir = tempfile::tempdir().unwrap();
            let mut doubled = standard();
            doubled
                .programs
                .push(filling(twice, founding("twin", PROBE, script(Vec::new()))));
            doubled
                .programs
                .push(filling(role::EGRESS, founding("other", RELAY, Vec::new())));
            let Err(Error::Roles(refusal)) = Host::found(
                context.child(twice),
                twice,
                dir.path(),
                block_id(0),
                doubled,
            )
            .await
            else {
                panic!("a genesis with {twice} filled twice was founded");
            };
            assert_eq!(refusal.reason, reason::INVALID_INPUT);
            assert!(refusal.sentence.contains(twice), "{refusal:?}");
            assert!(!dir.path().join("state").exists(), "{twice} wrote state");
            assert!(!dir.path().join("blobs").exists(), "{twice} wrote blobs");
        }
    });
}

#[test]
fn founding_refuses_a_roster_that_leaves_a_kernel_role_unfilled() {
    deterministic::Runner::default().start(|context| async move {
        for lacking in role::REQUIRED {
            let dir = tempfile::tempdir().unwrap();
            let mut short = standard();
            for founding in &mut short.programs {
                founding.roles.retain(|role| role != lacking);
            }
            let Err(Error::Roles(refusal)) = Host::found(
                context.child(lacking),
                lacking,
                dir.path(),
                block_id(0),
                short,
            )
            .await
            else {
                panic!("a genesis with no {lacking} role was founded");
            };
            assert_eq!(refusal.reason, reason::INVALID_INPUT);
            assert!(refusal.sentence.contains(lacking), "{refusal:?}");
            assert!(!dir.path().join("state").exists(), "{lacking} wrote state");
        }
    });
}

#[test]
fn founding_keeps_a_role_row_the_kernel_never_calls() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut extra = standard();
        for founding in &mut extra.programs {
            if founding.program == "probe" {
                founding.roles.push("later".into());
            }
        }
        let (host, _) = Host::found(context, "later", dir.path(), block_id(0), extra)
            .await
            .unwrap();
        let expected: Roles = roles()
            .iter()
            .map(|(role, program)| (role, program.clone()))
            .chain([("later", "probe".to_owned())])
            .collect();
        assert_eq!(host.roles(), &expected);
        assert_eq!(host.role("later").unwrap(), "probe");
    });
}

/// The halo role's lanes, as the kernel reads them.
async fn lanes_of(host: &Host<Ctx>) -> Vec<halo::Lane> {
    let reply = host
        .query(
            Layer::Confirmed,
            TIME,
            Origin::System,
            "halo",
            abi::encode(&halo::Query::Lanes),
        )
        .await
        .unwrap()
        .unwrap();
    match abi::decode(&reply).unwrap() {
        halo::Reply::Lanes(lanes) => lanes,
        other => panic!("{other:?}"),
    }
}

fn lane(name: &str, id: halo::LaneId, cap: halo::Class, since: u64) -> halo::Lane {
    halo::Lane {
        name: name.into(),
        id,
        cap,
        enabled: true,
        pinned: false,
        since,
    }
}

fn pinned(lane: halo::Lane) -> halo::Lane {
    halo::Lane {
        pinned: true,
        ..lane
    }
}

#[test]
fn the_halo_role_opens_a_lane_on_the_validators_vote_alone() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        let founded = vec![
            pinned(lane("first", 0, halo::Class::Critical, 0)),
            lane("second", 1, halo::Class::Client, 0),
        ];
        assert_eq!(lanes_of(&host).await, founded);

        let open = abi::encode(&halo::Op::Open {
            name: "third".into(),
            cap: halo::Class::Member,
        });
        let motion = validators::Motion::Role {
            role: role::HALO.into(),
            op: open.clone(),
        };
        let voted = submit("valset", abi::encode(&validators::Op::Vote(motion)));
        let applied = host
            .apply(block(1, vec![submit("halo", open), voted]))
            .await
            .unwrap();
        assert_eq!(
            rejected(&applied.submissions[0]).reason,
            reason::UNAUTHORIZED,
            "a signed frame changes no lane"
        );
        output(&applied.submissions[1]);
        let opened = lane("third", 2, halo::Class::Member, 1);
        assert_eq!(lanes_of(&host).await, [founded, vec![opened]].concat());
    });
}

/// The validators' vote on `motion`, through the fixture valset: its receipt.
async fn vote(host: &mut Host<Ctx>, height: u64, motion: validators::Motion) -> Receipt {
    let voted = submit("valset", abi::encode(&validators::Op::Vote(motion)));
    let mut applied = host.apply(block(height, vec![voted])).await.unwrap();
    applied.submissions.remove(0)
}

#[test]
fn a_role_motion_runs_its_op_on_the_program_bound_to_the_role() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), "net", dir.path(), standard()).await;
        let relay = host.programs().unwrap()["ping"];
        let motion = validators::Motion::Role {
            role: role::PROGRAMS.into(),
            op: change("echo", relay, Vec::new()),
        };
        output(&vote(&mut host, 1, motion).await);
        host.apply(block(2, Vec::new())).await.unwrap();
        assert_eq!(host.programs().unwrap()["echo"], relay);

        let motion = validators::Motion::Role {
            role: "ledger".into(),
            op: Vec::new(),
        };
        let refused = vote(&mut host, 3, motion).await;
        assert_eq!(rejected(&refused).reason, reason::UNSUPPORTED);
        assert!(rejected(&refused).sentence.contains("ledger"));
    });
}

#[test]
fn the_roster_moves_a_role_at_its_height_and_refuses_a_table_it_cannot_run() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), "net", dir.path(), standard()).await;
        let relay = host.programs().unwrap()["ping"];
        let bound = |host: &Host<Ctx>, role: &str| host.roles().get(role).cloned();
        let set = |program: &str| submit("programs", change(program, relay, Vec::new()));

        host.apply(block(
            1,
            vec![set("echo"), fill(role::EGRESS, Some("echo"))],
        ))
        .await
        .unwrap();
        assert_eq!(
            bound(&host, role::EGRESS),
            None,
            "a role moves at its height"
        );
        host.apply(block(2, Vec::new())).await.unwrap();
        assert_eq!(bound(&host, role::EGRESS).as_deref(), Some("echo"));

        host.apply(block(
            3,
            vec![
                fill(role::EGRESS, Some("pong")),
                fill(role::SCHEDULER, Some("pong")),
            ],
        ))
        .await
        .unwrap();
        host.apply(block(4, Vec::new())).await.unwrap();
        assert_eq!(bound(&host, role::EGRESS).as_deref(), Some("pong"));
        assert_eq!(bound(&host, role::SCHEDULER).as_deref(), Some("pong"));
        let moved = host.roles().clone();

        host.apply(block(5, vec![set("fresh"), fill(role::HALO, None)]))
            .await
            .unwrap();
        host.apply(block(6, Vec::new())).await.unwrap();
        assert_eq!(host.roles(), &moved, "a table without halo moves nothing");
        assert!(
            !host.programs().unwrap().contains_key("fresh"),
            "a refused roster lands no entry"
        );
        host.apply(block(7, vec![fill(role::HALO, Some("halo"))]))
            .await
            .unwrap();
        host.apply(block(8, Vec::new())).await.unwrap();
        assert!(host.programs().unwrap().contains_key("fresh"));

        host.apply(block(9, vec![fill("ledger", Some("nobody"))]))
            .await
            .unwrap();
        host.apply(block(10, vec![set("later")])).await.unwrap();
        host.apply(block(11, Vec::new())).await.unwrap();
        assert_eq!(
            host.roles(),
            &moved,
            "a role naming no roster program moves nothing"
        );
        assert!(!host.programs().unwrap().contains_key("later"));
        host.apply(block(12, vec![fill("ledger", None)]))
            .await
            .unwrap();
        host.apply(block(13, Vec::new())).await.unwrap();
        assert!(host.programs().unwrap().contains_key("later"));
        assert_eq!(host.roles(), &moved);

        drop(host);
        let reopened = Host::open(context.child("reopen"), "net", dir.path())
            .await
            .unwrap();
        assert_eq!(reopened.roles(), &moved);
    });
}

#[test]
fn dropping_a_program_drops_every_role_it_filled_in_the_same_block() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), "net", dir.path(), standard()).await;
        let relay = host.programs().unwrap()["ping"];
        host.apply(block(
            1,
            vec![
                submit("programs", change("echo", relay, Vec::new())),
                fill(role::SCHEDULER, Some("echo")),
                fill(role::EGRESS, Some("echo")),
                fill("ledger", Some("pong")),
            ],
        ))
        .await
        .unwrap();
        host.apply(block(2, Vec::new())).await.unwrap();
        assert_eq!(
            host.roles().get(role::SCHEDULER).map(String::as_str),
            Some("echo")
        );

        host.apply(block(
            3,
            vec![submit(
                "programs",
                abi::encode(&Change::Remove("echo".into())),
            )],
        ))
        .await
        .unwrap();
        let applied = host.apply(block(4, Vec::new())).await.unwrap();
        assert!(!host.programs().unwrap().contains_key("echo"));
        let table_written = applied
            .writes
            .programs
            .get(NETWORK)
            .is_some_and(|written| written.contains_key(b"roles".as_slice()));
        assert!(
            table_written,
            "the block that drops echo rewrites the roles table"
        );
        assert_eq!(host.roles().get(role::SCHEDULER), None);
        assert_eq!(host.roles().get(role::EGRESS), None);
        assert_eq!(host.roles().get("ledger").map(String::as_str), Some("pong"));
        let names: Vec<&str> = host.roles().iter().map(|(role, _)| role).collect();
        assert_eq!(
            names,
            [
                "access",
                "awaits",
                "blobs",
                "halo",
                "identity",
                "ledger",
                "programs",
                "validators"
            ]
        );
    });
}

/// The validators' vote on `op` at `height`, through the fixture valset: its receipt.
async fn halo_vote(host: &mut Host<Ctx>, height: u64, op: &halo::Op) -> Receipt {
    let motion = validators::Motion::Role {
        role: role::HALO.into(),
        op: abi::encode(op),
    };
    vote(host, height, motion).await
}

#[test]
fn a_pinned_lane_is_never_paused_or_closed_and_an_unpinned_one_is() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        let first = pinned(lane("first", 0, halo::Class::Critical, 0));
        let second = lane("second", 1, halo::Class::Client, 0);
        let set = |name: &str, cap, enabled| halo::Op::Set {
            name: name.into(),
            cap,
            enabled,
        };
        let close = |name: &str| halo::Op::Close { name: name.into() };

        let paused = halo_vote(&mut host, 1, &set("first", halo::Class::Critical, false)).await;
        assert_eq!(rejected(&paused).reason, reason::INVALID_INPUT);
        let closed = halo_vote(&mut host, 2, &close("first")).await;
        assert_eq!(rejected(&closed).reason, reason::INVALID_INPUT);
        assert_eq!(lanes_of(&host).await, [first.clone(), second.clone()]);

        let capped = halo_vote(&mut host, 3, &set("first", halo::Class::Member, true)).await;
        output(&capped);
        let first = halo::Lane {
            cap: halo::Class::Member,
            ..first
        };
        assert_eq!(lanes_of(&host).await, [first.clone(), second.clone()]);

        output(&halo_vote(&mut host, 4, &set("second", halo::Class::Client, false)).await);
        let second = halo::Lane {
            enabled: false,
            ..second
        };
        assert_eq!(lanes_of(&host).await, [first.clone(), second]);
        output(&halo_vote(&mut host, 5, &close("second")).await);
        assert_eq!(lanes_of(&host).await, [first]);
    });
}

/// Whether the runtime's storage holds any partition of `program`'s
/// commitment in the store `name`: the partitions its merkle journal,
/// merkle metadata and variable op log lay out.
async fn committed(context: &Ctx, name: &str, program: &str) -> bool {
    let commitment = commitment_name(name, program);
    for partition in [
        "merkle-journal-blobs",
        "merkle-journal-metadata",
        "merkle-meta",
        "log_data",
        "log_offsets-blobs",
        "log_offsets-metadata",
    ] {
        let scanned = context
            .scan(&format!("commitment-{commitment}-{partition}"))
            .await;
        if scanned.is_ok() {
            return true;
        }
    }
    false
}

/// The bound programs, then a probe whose init refuses.
fn refusing() -> Genesis {
    genesis(vec![founding(
        "probe",
        PROBE,
        script(vec![Step::Fail("no".into())]),
    )])
}

/// Founds [`standard`] as `name` in `dir`, then deletes its `state/` and
/// `blobs/`: its commitments stay in the runtime's storage holding height
/// 0, as a failed join, a moved `state/` or an interrupted cleanup leaves
/// them.
async fn plant(context: &Ctx, name: &str, dir: &Path) {
    drop(found(context.child("planted"), name, dir, standard()).await);
    std::fs::remove_dir_all(dir.join("state")).unwrap();
    std::fs::remove_dir_all(dir.join("blobs")).unwrap();
    assert!(committed(context, name, "valset").await);
}

#[test]
fn a_refused_founding_removes_the_dirs_it_made() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let refused = Host::found(
            context.child("refused"),
            "net",
            dir.path(),
            block_id(0),
            refusing(),
        )
        .await
        .err();
        assert!(
            matches!(refused, Some(Error::Genesis { .. })),
            "{refused:?}"
        );
        assert!(!dir.path().join("state").exists());
        assert!(!dir.path().join("blobs").exists());
        // valset's init ran and opened its commitment before probe's
        // refused, and the store opened the reserved ones: they stay in the
        // runtime's storage, and the corrected founding opens each empty
        assert!(committed(&context, "net", "valset").await);
        assert!(committed(&context, "net", NETWORK).await);

        let clean = tempfile::tempdir().unwrap();
        let clean = found(context.child("clean"), "clean", clean.path(), standard())
            .await
            .root()
            .unwrap();
        let host = found(context.child("corrected"), "net", dir.path(), standard()).await;
        assert_eq!(host.height().unwrap(), 0);
        assert_eq!(host.root().unwrap(), clean);
        restart(context.child("restart"), dir.path(), host).await;

        // blobs that were there stay; the state the founding made goes
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("blobs")).unwrap();
        std::fs::write(dir.path().join("blobs").join("kept"), b"kept").unwrap();
        let refused = Host::found(
            context.child("blobs_kept"),
            "blobs_kept",
            dir.path(),
            block_id(0),
            refusing(),
        )
        .await
        .err();
        assert!(
            matches!(refused, Some(Error::Genesis { .. })),
            "{refused:?}"
        );
        assert!(!dir.path().join("state").exists());
        assert_eq!(
            std::fs::read(dir.path().join("blobs").join("kept")).unwrap(),
            b"kept"
        );

        // a state that was there is left as the refused founding leaves it
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("state")).unwrap();
        let refused = Host::found(
            context.child("state_kept"),
            "state_kept",
            dir.path(),
            block_id(0),
            refusing(),
        )
        .await
        .err();
        assert!(
            matches!(refused, Some(Error::Genesis { .. })),
            "{refused:?}"
        );
        assert!(dir.path().join("state").exists());
        assert!(!dir.path().join("blobs").exists());
    });
}

/// Opening a directory whose store records no height is refused before any
/// commitment under the name opens: a `state/` restored from a backup finds
/// them as it left them.
#[test]
fn an_unfounded_store_is_refused_before_its_commitments_open() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("founded"), "net", dir.path(), standard()).await;
        // a block past genesis, so the last block's writes cannot rebuild
        // the reserved commitments on their own
        host.apply(block(1, Vec::new())).await.unwrap();
        let root = host.root().unwrap();
        drop(host);
        let backup = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::rename(&state, backup.path().join("state")).unwrap();

        let refused = Host::open(context.child("unfounded"), "net", dir.path())
            .await
            .err();
        assert!(matches!(refused, Some(Error::Unfounded)), "{refused:?}");

        let _ = std::fs::remove_dir_all(&state);
        std::fs::rename(backup.path().join("state"), &state).unwrap();
        let restored = Host::open(context.child("restored"), "net", dir.path())
            .await
            .unwrap();
        assert_eq!(restored.root().unwrap(), root);
    });
}

#[test]
fn a_corrected_founding_over_a_refused_one_founds_the_clean_root() {
    deterministic::Runner::default().start(|context| async move {
        let clean = tempfile::tempdir().unwrap();
        let clean = found(context.child("clean"), "clean", clean.path(), standard())
            .await
            .root()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        plant(&context, "net", dir.path()).await;
        let refused = Host::found(
            context.child("refused"),
            "net",
            dir.path(),
            block_id(0),
            refusing(),
        )
        .await
        .err();
        assert!(
            matches!(refused, Some(Error::Genesis { .. })),
            "{refused:?}"
        );
        let host = found(context.child("corrected"), "net", dir.path(), standard()).await;
        assert_eq!(host.root().unwrap(), clean);
    });
}

#[test]
fn a_fresh_founding_destroys_the_commitments_left_under_its_name() {
    deterministic::Runner::default().start(|context| async move {
        let clean = tempfile::tempdir().unwrap();
        let clean = found(context.child("clean"), "clean", clean.path(), standard())
            .await
            .root()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        plant(&context, "net", dir.path()).await;
        let host = found(context.child("over"), "net", dir.path(), standard()).await;
        assert_eq!(host.root().unwrap(), clean);
    });
}

#[test]
fn founding_refuses_a_program_that_does_not_admit() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let Err(Error::Genesis { program, refusal }) = Host::found(
            context.child("refusing"),
            "a",
            dir.path(),
            block_id(0),
            refusing(),
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
        assert!(!dir.path().join("state").exists());
        assert!(!dir.path().join("blobs").exists());

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
        assert!(!dir.path().join("state").exists());
        assert!(!dir.path().join("blobs").exists());

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
        assert!(!dir.path().join("state").exists());
        assert!(!dir.path().join("blobs").exists());

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
        let again = item("ping", 1);
        let applied = host
            .apply(block(
                2,
                vec![submit(
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
                    "ping",
                    send(vec![Message {
                        target: "probe".into(),
                        payload: probe_script,
                        reply: true,
                        acting: None,
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
fn a_message_acts_as_an_account_only_when_identity_says_its_program_runs_it() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        const AGENT: u64 = 77;
        let mut genesis = standard();
        let held = vec![(SIGNER.to_vec(), ACCOUNT), (b"#runs/ping".to_vec(), AGENT)];
        genesis.programs[2].params = abi::encode(&held);
        let mut host = found(context, "net", dir.path(), genesis).await;
        let as_agent = |account| Message {
            target: "probe".into(),
            payload: script(vec![Step::Env]),
            reply: true,
            acting: Some(account),
        };
        let applied = host
            .apply(block(
                1,
                vec![
                    submit("ping", send(vec![as_agent(AGENT)])),
                    submit("ping", send(vec![as_agent(AGENT + 1)])),
                ],
            ))
            .await
            .unwrap();
        let env = Env {
            network: b"net".to_vec(),
            height: 1,
            time: TIME + 1,
            me: "probe".into(),
            origin: Origin::Program("ping".into()),
            sender: Some(Principal::Account(AGENT)),
            roles: roles(),
            cause: Cause::Message(item("ping", 0)),
        };
        let [probed, replied] = applied.submissions[0].nested.as_slice() else {
            panic!("{:?}", applied.submissions[0].nested);
        };
        assert_eq!(replies(probed), vec![Reply::Env(env)]);
        assert_eq!(replied, &receipt("ping", ok(b""), vec![]));
        let not_run = Refusal::new(reason::UNAUTHORIZED, "ping");
        let [refused, absorbed] = applied.submissions[1].nested.as_slice() else {
            panic!("{:?}", applied.submissions[1].nested);
        };
        assert_eq!(
            refused,
            &receipt("probe", Outcome::Rejected(not_run), vec![])
        );
        assert_eq!(absorbed, &receipt("ping", ok(b""), vec![]));
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
            acting: None,
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
                        "ping",
                        send(vec![
                            message("pong", note(b"first"), false),
                            message("pong", fail(false), false),
                            message("pong", note(b"never"), false),
                        ]),
                    ),
                    submit("ping", send(vec![message("nobody", note(b"x"), false)])),
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
                    submit("ping", abi::encode(&chain(8))),
                    submit("ping", abi::encode(&chain(9))),
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
        let mut host = found(context, "net", dir.path(), metered()).await;
        let applied = host
            .apply(block(
                1,
                vec![
                    submit("ping", abi::encode(&chain(1))),
                    submit("ping", abi::encode(&chain(8))),
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
            .apply(block(1, vec![submit("probe", script(spin_queries(3)))]))
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
            acting: None,
        };
        let payload = send(vec![spend, message("pong", note(b"late"), false)]);
        let applied = host
            .apply(block(1, vec![submit("ping", payload)]))
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
        let stranger = |target: &str, payload| Submission {
            signer: b"stranger".to_vec(),
            ..submit(target, payload)
        };
        let sender = |receipt: &Receipt| match replies(receipt).as_slice() {
            [.., Reply::Env(env)] => env.sender.clone(),
            other => panic!("{other:?}"),
        };
        let env = || script(vec![Step::Env]);
        let env_again = || script(vec![get(b"again"), Step::Env]);
        let applied = host
            .apply(block(
                1,
                vec![
                    submit("probe", env()),
                    // a key that holds no account runs, as no one
                    stranger("probe", env()),
                    // identity seats it; the next frame in the block sees it
                    stranger("identity", abi::encode(&(b"stranger".to_vec(), 2u64))),
                    stranger("probe", env_again()),
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
        let id = blob::id_of(HashKind::Sha256, &blob::frame("page", b"body").unwrap()).unwrap();

        let applied = host
            .apply(block(
                1,
                vec![submit(
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
        assert_eq!(touched, [RECENT, SPENT]);
        assert_eq!(
            host.view(Layer::Confirmed).get("probe", b"k").unwrap(),
            None
        );
        assert_eq!(host.blob(&id, TIME).await.unwrap(), None);

        let applied = host
            .apply(block(
                2,
                vec![submit("probe", script(vec![set(b"k", b"v"), put()]))],
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
        assert_eq!(
            host.blob(&id, TIME).await.unwrap(),
            Some(b"page 4\0body".to_vec())
        );
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

        let staged = blob::id_of(HashKind::Sha1, &blob::frame("note", b"x").unwrap()).unwrap();
        let applied = host
            .apply(block(
                3,
                vec![
                    submit(
                        "probe",
                        script(vec![op(HostOp::BlobPut {
                            hash: HashKind::Sha1,
                            kind: "note".into(),
                            body: b"x".to_vec(),
                        })]),
                    ),
                    submit("probe", script(vec![op(HostOp::BlobStat(staged))])),
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
        assert_eq!(
            host.blob(&staged, TIME).await.unwrap(),
            Some(b"note 1\0x".to_vec())
        );
    });
}

#[test]
fn a_program_reads_a_put_blob_only_once_a_quorum_holds_its_bundle() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        let framed = blob::frame("page", b"held").unwrap();
        let entry = blobs::Entry::of(&framed).unwrap();
        let id = entry.id;
        host.blobs().write(&id, &framed).unwrap();
        let put = |blobs: Vec<blobs::Entry>| abi::encode(&blobs::Op::Put { blobs });
        let receipt = |signer: &[u8], bundle: u64| Submission {
            signer: signer.to_vec(),
            ..submit("blobs", abi::encode(&blobs::Op::Receipt { bundle }))
        };
        let read = |id: BlobId, salt: u64| Submission {
            salt: Some(salt),
            ..submit("probe", script(vec![op(HostOp::BlobGet(id))]))
        };

        let applied = host
            .apply(block(
                1,
                vec![submit("blobs", put(vec![entry])), read(id, 1)],
            ))
            .await
            .unwrap();
        assert_eq!(output(&applied.submissions[0]), abi::encode(&0u64));
        assert_eq!(
            replies(&applied.submissions[1]),
            vec![Reply::Host(HostReply::Blob(None))]
        );
        assert!(!host.readable(&id, TIME).await.unwrap());

        let applied = host
            .apply(block(
                2,
                vec![receipt(SIGNER, 0), receipt(b"v1", 0), read(id, 2)],
            ))
            .await
            .unwrap();
        assert_eq!(
            rejected(&applied.submissions[0]).reason,
            reason::UNAUTHORIZED
        );
        assert!(matches!(
            applied.submissions[1].outcome,
            Outcome::Applied { .. }
        ));
        assert_eq!(
            replies(&applied.submissions[2]),
            vec![Reply::Host(HostReply::Blob(Some(Blob {
                kind: "page".into(),
                body: b"held".to_vec(),
            })))]
        );
        assert!(host.readable(&id, TIME).await.unwrap());

        let lacking = blob::frame("page", b"held, never gathered").unwrap();
        let lacking = blobs::Entry::of(&lacking).unwrap();
        let lacked = lacking.id;
        host.apply(block(3, vec![submit("blobs", put(vec![lacking]))]))
            .await
            .unwrap();
        host.apply(block(4, vec![receipt(b"v1", 1)])).await.unwrap();
        let unavailable = host.apply(block(5, vec![read(lacked, 5)])).await;
        assert!(
            matches!(unavailable, Err(Error::BlobUnavailable(missing)) if missing == lacked),
            "{unavailable:?}"
        );
    });
}

#[test]
fn an_epoch_is_recorded_as_the_block_ending_the_one_before_commits() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        let founding = vec![member(b"v1", "v1:1")];
        let members = vec![
            member(b"v1", "v1:1"),
            member(b"v2", "v2:2"),
            member(b"r3", "r3:3"),
        ];
        let keys = vec![b"v1".to_vec(), b"v2".to_vec()];

        host.apply(block(1, Vec::new())).await.unwrap();
        let seating = vec![enroll(b"v2", "v2:2"), enroll(b"r3", "r3:3"), promote(b"v2")];
        let applied = host.apply(block(2, seating)).await.unwrap();
        let outcomes: Vec<Outcome> = applied
            .submissions
            .into_iter()
            .map(|receipt| receipt.outcome)
            .collect();
        let enacted = ok(&abi::encode(&validators::Voted::Enacted));
        assert_eq!(outcomes, vec![ok(b""), ok(b""), enacted]);
        assert_eq!(host.epoch_members(0).unwrap(), Some(founding.clone()));
        assert_eq!(
            host.epoch_validators(0).unwrap(),
            Some(vec![b"v1".to_vec()])
        );
        assert_eq!(host.epoch_members(1).unwrap(), None);
        assert_eq!(host.epoch_validators(1).unwrap(), None);

        host.apply(block(3, Vec::new())).await.unwrap();
        assert_eq!(host.epoch_members(0).unwrap(), Some(founding));
        assert_eq!(host.epoch_members(1).unwrap(), Some(members.clone()));
        assert_eq!(host.epoch_validators(1).unwrap(), Some(keys.clone()));
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
        assert_eq!(host.epoch_members(2).unwrap(), Some(members));
        assert_eq!(host.epoch_validators(2).unwrap(), Some(keys));
    });
}

#[test]
fn the_roster_admits_swaps_and_drops_programs() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), "net", dir.path(), standard()).await;
        let programs = host.programs().unwrap();
        let relay = programs["ping"];
        let probe = programs["probe"];

        host.apply(block(
            1,
            vec![submit("programs", change("echo", relay, Vec::new()))],
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
        let eleventh = fixture_identity::PROGRAMS_FROM + 10;
        assert_eq!(account_of(&host, "echo").await, Some(eleventh));
        assert_eq!(host.programs().unwrap()["echo"], relay);
        let applied = host
            .apply(block(
                3,
                vec![submit(
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
                "programs",
                change("echo", probe, script(Vec::new())),
            )],
        ))
        .await
        .unwrap();
        let applied = host.apply(block(5, Vec::new())).await.unwrap();
        assert_eq!(applied.admissions, vec![receipt("echo", ok(b""), vec![])]);
        let swapped = applied
            .writes
            .programs
            .get(host::PROGRAMS)
            .is_some_and(|written| written.contains_key(b"echo".as_slice()));
        assert!(swapped, "a swap writes `$programs` at the program's id");
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
                "programs",
                abi::encode(&Change::Remove("echo".into())),
            )],
        ))
        .await
        .unwrap();
        let applied = host.apply(block(7, Vec::new())).await.unwrap();
        assert!(applied.admissions.is_empty());
        assert!(!host.programs().unwrap().contains_key("echo"));
        assert!(host.store().commitment("echo").is_none());
        let mut host = restart(context.child("restart"), dir.path(), host).await;
        let applied = host
            .apply(block(8, vec![submit("echo", script(Vec::new()))]))
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
                    "programs",
                    change("ghost", BlobId::Sha256([9; 32]), Vec::new()),
                ),
                submit("programs", change("$evil", relay, Vec::new())),
                submit(
                    "programs",
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
                "access", "awaits", "blobs", "halo", "identity", "ping", "pong", "probe",
                "programs", "valset"
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
        let mut host = found(context.child("found"), "net", dir.path(), standard()).await;
        let relay = host.programs().unwrap()["ping"];
        let closed =
            |number: u64| submit("identity", abi::encode(&(b"#closed/late".to_vec(), number)));
        host.apply(block(
            1,
            vec![
                closed(1),
                submit("programs", change("late", relay, Vec::new())),
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
            assert!(host.store().commitment("late").is_none());
            assert_eq!(account_of(&host, "late").await, None);
        }
        let mut host = restart(context.child("restart"), dir.path(), host).await;
        let applied = host
            .apply(block(
                4,
                vec![submit(
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

        host.apply(block(5, vec![closed(0)])).await.unwrap();
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
            vec![submit("probe", script(vec![set(b"a", b"1")]))],
        ))
        .await
        .unwrap();
        let receipts = host
            .preconfirm(
                TIME,
                vec![submit(
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
            vec![submit("probe", script(vec![set(b"k", b"v")]))],
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
        assert_eq!(program, "programs");
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
                    submit("probe", script(vec![Step::Env])),
                    submit("probe", script(vec![Step::Spin])),
                    submit("probe", script(vec![Step::Grow(2048)])),
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
            framed.insert(id, upstream.blob(&id, TIME).await.unwrap().unwrap());
        }
        assert_eq!(framed.len(), 10);
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

        let page = blob::id_of(HashKind::Sha256, &blob::frame("page", b"body").unwrap()).unwrap();
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
fn a_block_runs_a_frame_once_and_a_rejected_run_spends_it() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        let first = || submit("probe", script(vec![set(b"k", b"first")]));
        let fails = || submit("probe", script(vec![Step::Fail("no".into())]));
        let applied = host
            .apply(block(
                1,
                vec![
                    first(),
                    first(),
                    fails(),
                    fails(),
                    submit("probe", script(vec![set(b"k", b"second")])),
                ],
            ))
            .await
            .unwrap();
        assert_eq!(
            replies(&applied.submissions[0]),
            vec![Reply::Host(HostReply::Done)]
        );
        assert_eq!(rejected(&applied.submissions[1]).reason, reason::DUPLICATE);
        assert_eq!(rejected(&applied.submissions[2]).reason, "probe");
        assert_eq!(rejected(&applied.submissions[3]).reason, reason::DUPLICATE);
        assert_eq!(
            replies(&applied.submissions[4]),
            vec![Reply::Host(HostReply::Done)]
        );
        assert_eq!(stored(&host, "probe", b"k"), Some(b"second".to_vec()));

        let applied = host
            .apply(block(
                2,
                vec![
                    first(),
                    anchored(1, first()),
                    Submission {
                        salt: Some(1),
                        ..first()
                    },
                ],
            ))
            .await
            .unwrap();
        assert_eq!(rejected(&applied.submissions[0]).reason, reason::DUPLICATE);
        assert_eq!(
            replies(&applied.submissions[1]),
            vec![Reply::Host(HostReply::Done)]
        );
        assert_eq!(
            replies(&applied.submissions[2]),
            vec![Reply::Host(HostReply::Done)]
        );
        assert_eq!(stored(&host, "probe", b"k"), Some(b"first".to_vec()));
        assert_eq!(
            host.fresh(&first()).unwrap().unwrap_err().reason,
            reason::DUPLICATE
        );

        let fresh = || anchored(2, fails());
        let receipts = host
            .preconfirm(
                TIME,
                vec![
                    fresh(),
                    fresh(),
                    anchored(2, submit("probe", script(Vec::new()))),
                ],
            )
            .await
            .unwrap();
        assert_eq!(rejected(&receipts[0]).reason, "probe");
        assert_eq!(rejected(&receipts[1]).reason, reason::DUPLICATE);
        assert!(matches!(receipts[2].outcome, Outcome::Applied { .. }));
        assert_eq!(host.fresh(&fresh()).unwrap(), Ok(()));
    });
}

#[test]
fn a_frame_runs_only_while_its_anchor_is_one_of_the_recent_blocks() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        let write = |key: &[u8]| submit("probe", script(vec![set(key, b"v")]));
        let forged = Submission {
            anchor: Tip {
                height: 0,
                id: block_id(9),
            },
            ..write(b"forged")
        };
        let applied = host
            .apply(block(
                1,
                vec![anchored(1, write(b"ahead")), forged, write(b"last")],
            ))
            .await
            .unwrap();
        assert_eq!(rejected(&applied.submissions[0]).reason, reason::ANCHOR);
        assert_eq!(rejected(&applied.submissions[1]).reason, reason::ANCHOR);
        assert_eq!(
            replies(&applied.submissions[2]),
            vec![Reply::Host(HostReply::Done)]
        );

        for height in 2..ANCHOR_WINDOW {
            host.apply(block(height, Vec::new())).await.unwrap();
        }
        assert_eq!(
            host.fresh(&write(b"last")).unwrap().unwrap_err().reason,
            reason::DUPLICATE
        );
        let applied = host
            .apply(block(ANCHOR_WINDOW, vec![write(b"oldest")]))
            .await
            .unwrap();
        assert_eq!(
            replies(&applied.submissions[0]),
            vec![Reply::Host(HostReply::Done)]
        );
        let view = host.view(Layer::Confirmed);
        assert_eq!(view.get(RECENT, &0u64.to_be_bytes()).unwrap(), None);
        assert_eq!(
            view.scan(SPENT, &Scan::prefix(0u64.to_be_bytes())).unwrap(),
            Vec::new()
        );
        assert_eq!(
            view.scan(RECENT, &Scan::prefix(b"")).unwrap().len() as u64,
            ANCHOR_WINDOW
        );

        let applied = host
            .apply(block(
                ANCHOR_WINDOW + 1,
                vec![write(b"late"), anchored(1, write(b"late"))],
            ))
            .await
            .unwrap();
        assert_eq!(rejected(&applied.submissions[0]).reason, reason::ANCHOR);
        assert_eq!(
            replies(&applied.submissions[1]),
            vec![Reply::Host(HostReply::Done)]
        );
        assert_eq!(stored(&host, "probe", b"late"), Some(b"v".to_vec()));
        assert_eq!(stored(&host, "probe", b"ahead"), None);
        assert_eq!(stored(&host, "probe", b"forged"), None);
    });
}

/// A dropped program's keys leave storage with its commitment: admitted
/// again under the same id it reads nothing, as a node that synced the
/// empty commitment would.
#[test]
fn a_program_admitted_again_under_its_id_starts_empty() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), "net", dir.path(), standard()).await;
        let probe = host.programs().unwrap()["probe"];
        let set = op(HostOp::Set {
            key: b"k".to_vec(),
            value: b"v".to_vec(),
        });
        host.apply(block(
            1,
            vec![submit("programs", change("echo", probe, script(vec![set])))],
        ))
        .await
        .unwrap();
        host.apply(block(2, Vec::new())).await.unwrap();
        assert_eq!(
            host.view(Layer::Confirmed).get("echo", b"k").unwrap(),
            Some(b"v".to_vec())
        );
        host.apply(block(
            3,
            vec![submit(
                "programs",
                abi::encode(&Change::Remove("echo".into())),
            )],
        ))
        .await
        .unwrap();
        host.apply(block(4, Vec::new())).await.unwrap();
        assert!(host.store().commitment("echo").is_none());
        assert_eq!(host.view(Layer::Confirmed).get("echo", b"k").unwrap(), None);
        let mut host = restart(context.child("restart"), dir.path(), host).await;
        host.apply(block(
            5,
            vec![submit(
                "programs",
                change("echo", probe, script(Vec::new())),
            )],
        ))
        .await
        .unwrap();
        host.apply(block(6, Vec::new())).await.unwrap();
        assert_eq!(host.view(Layer::Confirmed).get("echo", b"k").unwrap(), None);
        let entries = host
            .store()
            .commitment("echo")
            .unwrap()
            .entries()
            .await
            .unwrap();
        assert!(entries.is_empty());
    });
}

#[test]
fn a_member_cap_outside_1_to_128_is_refused() {
    deterministic::Runner::default().start(|context| async move {
        for cap in [0, validators::MAX_MEMBERS + 1] {
            let dir = tempfile::tempdir().unwrap();
            let mut founding = standard();
            founding.member_cap = cap;
            let refused = Host::found(
                context.child("found"),
                "net",
                dir.path(),
                block_id(0),
                founding,
            )
            .await
            .err();
            assert!(
                matches!(refused, Some(Error::MemberCap { cap: got }) if got == cap),
                "cap {cap}: {refused:?}"
            );
            assert_eq!(
                refused.unwrap().to_string(),
                format!("a network's member cap is 1 to 128; this founding asks for {cap}")
            );
            assert!(!dir.path().join("state").exists(), "cap {cap} wrote state");
        }
    });
}

/// The fixture valset ignores the cap, so the kernel's backstop at epoch 0
/// is what refuses (valset's own `init` refuses first).
#[test]
fn more_founding_validators_than_the_cap_is_refused() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut founding = standard();
        founding.validators = vec![member(b"v1", "v1:1"), member(b"v2", "v2:2")];
        founding.member_cap = 1;
        let refused = Host::found(context, "net", dir.path(), block_id(0), founding)
            .await
            .err();
        assert!(
            matches!(&refused, Some(Error::Corrupt(why)) if why.contains("past the network's cap of 1")),
            "{refused:?}"
        );
        assert!(!dir.path().join("state").exists());
        assert!(!dir.path().join("blobs").exists());
    });
}

#[test]
fn the_cap_is_recorded_at_founding_and_read_on_open() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut founding = standard();
        founding.member_cap = 7;
        let host = found(context.child("found"), "net", dir.path(), founding).await;
        assert_eq!(host.member_cap().unwrap(), 7);
        assert_eq!(
            host.view(Layer::Confirmed)
                .get(NETWORK, b"member_cap")
                .unwrap(),
            Some(vec![7, 0, 0, 0])
        );
        let host = restart(context.child("restart"), dir.path(), host).await;
        assert_eq!(host.member_cap().unwrap(), 7);
    });
}

#[test]
fn an_epoch_seating_more_members_than_the_cap_is_not_applied() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut founding = standard();
        founding.member_cap = 2;
        let mut host = found(context, "net", dir.path(), founding).await;
        let past_the_cap = vec![enroll(b"v2", "v2:2"), enroll(b"r3", "r3:3"), promote(b"v2")];
        host.apply(block(1, past_the_cap)).await.unwrap();
        host.apply(block(2, Vec::new())).await.unwrap();
        let tip = host.tip().unwrap();
        let closing = host.apply(block(3, Vec::new())).await;
        assert!(
            matches!(&closing, Err(Error::Corrupt(why)) if why.contains("seats 3 members for epoch 1")),
            "{closing:?}"
        );
        assert_eq!(host.tip().unwrap(), tip);
    });
}

fn hooked(genesis: Genesis, hooks: &[&str]) -> Genesis {
    Genesis {
        hooks: hooks.iter().map(|hook| (*hook).to_owned()).collect(),
        ..genesis
    }
}

fn hook_runs(genesis: Genesis, steps: Vec<Step>) -> Genesis {
    let mut genesis = genesis;
    let probe = genesis
        .programs
        .iter_mut()
        .find(|founding| founding.program == "probe")
        .expect("the probe is founded");
    probe.params = script(vec![set(fixture_probe::HOOK, &script(steps))]);
    genesis
}

fn told(host: &Host<Ctx>, height: u64, position: u32) -> Option<hook::Record> {
    stored(host, "probe", &fixture_probe::seen(height, position))
        .map(|bytes| abi::decode(&bytes).unwrap())
}

fn hook_change(change: Change) -> Submission {
    submit("programs", abi::encode(&change))
}

fn told_of(target: &str, signer: &[u8], account: Option<u64>, applied: bool) -> hook::Record {
    hook::Record {
        height: 1,
        position: 0,
        signer: signer.to_vec(),
        account,
        target: target.to_owned(),
        applied,
    }
}

#[test]
fn a_hook_is_told_of_every_carried_frame_after_it_runs_and_changes_none() {
    deterministic::Runner::default().start(|context| async move {
        let frames = || {
            let first = submit("ping", abi::encode(&note(b"one")));
            vec![
                first.clone(),
                submit("ping", abi::encode(&fail(false))),
                first,
                Submission {
                    signer: b"nobody".to_vec(),
                    ..submit("ping", abi::encode(&note(b"two")))
                },
            ]
        };
        let plain_dir = tempfile::tempdir().unwrap();
        let mut plain = found(
            context.child("plain"),
            "plain",
            plain_dir.path(),
            standard(),
        )
        .await;
        let unhooked = plain.apply(block(1, frames())).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let mut host = found(
            context.child("hooked"),
            "net",
            dir.path(),
            hooked(standard(), &["probe"]),
        )
        .await;
        host.preconfirm(TIME, frames()).await.unwrap();
        let preconfirmed = host
            .view(Layer::Preconfirmed)
            .get("probe", &fixture_probe::seen(1, 0))
            .unwrap();
        assert_eq!(preconfirmed, None, "a preconfirmed frame is told nothing");
        let applied = host.apply(block(1, frames())).await.unwrap();
        assert_eq!(applied.submissions, unhooked.submissions);
        assert_eq!(
            stored(&host, "ping", b"got/ping/0"),
            stored(&plain, "ping", b"got/ping/0")
        );
        assert_eq!(
            told(&host, 1, 0),
            Some(told_of("ping", SIGNER, Some(ACCOUNT), true))
        );
        assert_eq!(
            told(&host, 1, 1),
            Some(hook::Record {
                position: 1,
                ..told_of("ping", SIGNER, Some(ACCOUNT), false)
            })
        );
        assert_eq!(told(&host, 1, 2), None, "a frame carried already");
        assert_eq!(
            told(&host, 1, 3),
            Some(hook::Record {
                position: 3,
                ..told_of("ping", b"nobody", None, true)
            })
        );
        let host = restart(context.child("restart"), dir.path(), host).await;
        assert!(told(&host, 1, 0).is_some());
    });
}

#[test]
fn a_hook_that_fails_keeps_none_of_its_writes_and_the_block_goes_on() {
    let emit = HostOp::Emit(Message {
        target: "pong".into(),
        payload: abi::encode(&note(b"from a hook")),
        reply: false,
        acting: None,
    });
    let runs: [(Vec<Step>, Option<&[u8]>); 4] = [
        (vec![set(b"k", b"v")], Some(b"v")),
        (vec![set(b"k", b"v"), Step::Fail("no".into())], None),
        (vec![set(b"k", b"v"), Step::Spin], None),
        (vec![set(b"k", b"v"), op(emit)], None),
    ];
    for (steps, kept) in runs {
        deterministic::Runner::default().start(|context| async move {
            let dir = tempfile::tempdir().unwrap();
            let genesis = hook_runs(hooked(metered(), &["probe"]), steps);
            let mut host = found(context, "net", dir.path(), genesis).await;
            let applied = host
                .apply(block(
                    1,
                    vec![
                        submit("ping", abi::encode(&note(b"one"))),
                        submit("ping", abi::encode(&note(b"two"))),
                    ],
                ))
                .await
                .unwrap();
            assert_eq!(output(&applied.submissions[0]), b"eno");
            assert_eq!(output(&applied.submissions[1]), b"owt");
            assert_eq!(stored(&host, "probe", b"k").as_deref(), kept);
            assert_eq!(told(&host, 1, 1).is_some(), kept.is_some());
            assert_eq!(
                stored(&host, "pong", &fixture_relay::got(&item("probe", 0))),
                None
            );
        });
    }
}

#[test]
fn the_programs_role_hooks_and_unhooks_a_program_from_the_next_block() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, "net", dir.path(), standard()).await;
        let ping = |bytes: &[u8]| submit("ping", abi::encode(&note(bytes)));
        host.apply(block(
            1,
            vec![
                hook_change(Change::Hook("probe".into())),
                hook_change(Change::Hook("ghost".into())),
                ping(b"one"),
            ],
        ))
        .await
        .unwrap();
        assert_eq!(
            told(&host, 1, 2),
            None,
            "hooks are asked before a block's frames"
        );
        host.apply(block(2, vec![ping(b"two")])).await.unwrap();
        assert_eq!(
            told(&host, 2, 0).map(|record| (record.height, record.position)),
            Some((2, 0))
        );
        host.apply(block(3, vec![hook_change(Change::Unhook("probe".into()))]))
            .await
            .unwrap();
        host.apply(block(4, vec![ping(b"four")])).await.unwrap();
        assert_eq!(told(&host, 4, 0), None);
    });
}

#[test]
fn the_kernel_hooks_only_what_the_programs_role_answers() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut silent = hooked(standard(), &["probe"]);
        silent.programs[0].code = RELAY.to_vec();
        let mut host = found(context, "net", dir.path(), silent).await;
        host.apply(block(1, vec![submit("ping", abi::encode(&note(b"one")))]))
            .await
            .unwrap();
        assert_eq!(told(&host, 1, 0), None);
    });
}

#[test]
fn founding_refuses_a_hook_it_does_not_found() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let refused = Host::found(
            context,
            "net",
            dir.path(),
            block_id(0),
            hooked(standard(), &["ghost"]),
        )
        .await
        .err();
        assert!(
            matches!(&refused, Some(Error::Unhooked(program)) if program == "ghost"),
            "{refused:?}"
        );
    });
}
