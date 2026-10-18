//! Scheduled runs: a program schedules a run for a later height through the scheduler role, the
//! host runs it at the end of that block as the program that scheduled it, and the scheduler
//! keeps its receipt.
use std::path::Path;

use abi::role::scheduler::{self, Entry, EntryId};
use abi::role::{self, halo, identity, validators};
use abi::{Cause, HostOp, HostReply, Message, Origin, Outcome, Principal, Receipt, reason};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use fixture_awaiter::{ANSWERS, Plan};
use fixture_probe::{Reply, Step};
use fixture_wasm::{
    ACCESS, AWAITER, AWAITS, BLOBS as BLOBS_PROGRAM, HALO, IDENTITY, PROBE, PROGRAMS, SCHEDULER,
    VALSET,
};
use host::{Applied, Block, BlockId, Founding, Genesis, Host, Layer, Limits, Submission, Tip};

const SIGNER: &[u8] = b"signer";
const TIME: u64 = 1_700_000_000;

type Ctx = deterministic::Context;

fn founding(program: &str, code: &[u8], params: Vec<u8>) -> Founding {
    let roles: &[&str] = match program {
        "programs" => &[role::PROGRAMS],
        "valset" => &[role::VALIDATORS],
        "identity" => &[role::IDENTITY],
        "blobs" => &[role::BLOBS],
        "awaits" => &[role::AWAITS],
        "halo" => &[role::HALO],
        "access" => &[role::ACCESS],
        _ => &[],
    };
    Founding {
        program: program.to_owned(),
        code: code.to_vec(),
        params,
        roles: roles.iter().map(|role| (*role).to_owned()).collect(),
    }
}

fn genesis(bound: bool) -> Genesis {
    let mut scheduler = founding("scheduler", SCHEDULER, Vec::new());
    if bound {
        scheduler.roles.push(role::SCHEDULER.to_owned());
    }
    Genesis {
        network: b"net".to_vec(),
        validators: vec![validators::Member {
            key: b"v1".to_vec(),
            address: Some("v1:1".into()),
            halo: "v1:1".into(),
        }],
        programs: vec![
            founding("programs", PROGRAMS, Vec::new()),
            founding("valset", VALSET, Vec::new()),
            founding(
                "identity",
                IDENTITY,
                abi::encode(&vec![(SIGNER.to_vec(), 1u64)]),
            ),
            founding("blobs", BLOBS_PROGRAM, Vec::new()),
            founding("awaits", AWAITS, Vec::new()),
            scheduler,
            founding("awaiter", AWAITER, Vec::new()),
            founding("other", AWAITER, Vec::new()),
            founding("probe", PROBE, abi::encode(&Vec::<Step>::new())),
            founding(
                "halo",
                HALO,
                abi::encode(&halo::Genesis { lanes: Vec::new() }),
            ),
            founding("access", ACCESS, Vec::new()),
        ],
        views: Vec::new(),
        hooks: Vec::new(),
        limits: Limits::default(),
        epoch_length: 1_000,
        time: TIME,
        member_cap: 16,
    }
}

async fn found(context: Ctx, dir: &Path, bound: bool) -> Host<Ctx> {
    Host::found(context, "net", dir, block_id(0), genesis(bound))
        .await
        .unwrap()
        .0
}

fn block_id(height: u64) -> BlockId {
    let mut id = [0u8; 32];
    id[..8].copy_from_slice(&height.to_be_bytes());
    id
}

fn submit(target: &str, payload: Vec<u8>) -> Submission {
    Submission {
        signer: SIGNER.to_vec(),
        anchor: Tip {
            height: 0,
            id: block_id(0),
        },
        salt: None,
        target: target.to_owned(),
        payload,
    }
}

async fn apply(host: &mut Host<Ctx>, height: u64, submissions: Vec<Submission>) -> Applied {
    let block = Block {
        height,
        id: block_id(height),
        time: TIME + height,
        submissions,
    };
    host.apply(block).await.unwrap()
}

async fn until(host: &mut Host<Ctx>, from: u64, to: u64) {
    for height in from..=to {
        apply(host, height, Vec::new()).await;
    }
}

fn schedule(height: u64, target: &str, steps: Vec<Step>) -> Vec<u8> {
    abi::encode(&scheduler::Op::Schedule {
        height,
        target: target.into(),
        payload: abi::encode(&steps),
    })
}

fn asks(caller: &str, asks: Vec<Vec<u8>>) -> Submission {
    let plan = Plan {
        target: "scheduler".into(),
        acting: None,
        asks,
        together: false,
        kept: 0,
        fail: false,
    };
    submit(caller, abi::encode(&plan))
}

fn set(key: &[u8], value: &[u8]) -> Step {
    Step::Op(HostOp::Set {
        key: key.to_vec(),
        value: value.to_vec(),
    })
}

fn answers(host: &Host<Ctx>, program: &str) -> Vec<abi::role::awaits::Answer> {
    let bytes = stored(host, program, ANSWERS).expect("the program kept its answers");
    abi::decode(&bytes).unwrap()
}

fn scheduled(host: &Host<Ctx>, program: &str) -> Vec<EntryId> {
    answers(host, program)
        .into_iter()
        .map(|answer| abi::decode(&answer.expect("the scheduler took the entry")).unwrap())
        .collect()
}

fn stored(host: &Host<Ctx>, program: &str, key: &[u8]) -> Option<Vec<u8>> {
    host.view(Layer::Confirmed).get(program, key).unwrap()
}

async fn entry(host: &Host<Ctx>, id: EntryId) -> Option<Entry> {
    let asked = abi::encode(&scheduler::Query::Entry { id });
    let reply = host
        .query(Layer::Confirmed, TIME, Origin::System, "scheduler", asked)
        .await
        .unwrap()
        .unwrap();
    match abi::decode(&reply).unwrap() {
        scheduler::Reply::Entry(entry) => entry,
        other => panic!("{other:?}"),
    }
}

async fn receipt(host: &Host<Ctx>, id: EntryId) -> Receipt {
    entry(host, id)
        .await
        .expect("the entry is kept")
        .receipt
        .expect("the entry ran")
}

fn replies(receipt: &Receipt) -> Vec<Reply> {
    match &receipt.outcome {
        Outcome::Applied { output } => abi::decode(output).unwrap(),
        Outcome::Rejected(refusal) => panic!("{} was rejected: {refusal}", receipt.program),
    }
}

async fn account_of(host: &Host<Ctx>, program: &str) -> identity::AccountNumber {
    let asked = abi::encode(&identity::Query::OfProgram(program.into()));
    let reply = host
        .query(Layer::Confirmed, TIME, Origin::System, "identity", asked)
        .await
        .unwrap()
        .unwrap();
    match abi::decode(&reply).unwrap() {
        identity::Reply::Account(Some(account)) => account,
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_entry_runs_at_the_end_of_its_block_as_the_program_that_scheduled_it() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path(), true).await;
        let steps = vec![Step::Env, set(b"ran", b"yes")];
        apply(
            &mut host,
            1,
            vec![asks("awaiter", vec![schedule(3, "probe", steps)])],
        )
        .await;
        let [id] = scheduled(&host, "awaiter")[..] else {
            panic!("one entry was scheduled");
        };

        apply(&mut host, 2, Vec::new()).await;
        assert_eq!(stored(&host, "probe", b"ran"), None);
        let waiting = entry(&host, id).await.expect("the entry is kept");
        assert_eq!(
            (
                waiting.scheduler.as_str(),
                waiting.height,
                waiting.target.as_str()
            ),
            ("awaiter", 3, "probe")
        );
        assert_eq!(waiting.receipt, None);

        let applied = apply(&mut host, 3, Vec::new()).await;
        assert!(applied.submissions.is_empty());
        assert_eq!(stored(&host, "probe", b"ran"), Some(b"yes".to_vec()));
        let receipt = receipt(&host, id).await;
        assert_eq!(receipt.program, "probe");
        let [Reply::Env(env), Reply::Host(HostReply::Done)] = &replies(&receipt)[..] else {
            panic!("the probe saw its env and set its key");
        };
        let account = account_of(&host, "awaiter").await;
        assert_eq!(env.origin, Origin::Program("awaiter".into()));
        assert_eq!(env.sender, Some(Principal::Account(account)));
        assert_eq!(env.cause, Cause::Scheduled { entry: id });
        assert_eq!(env.height, 3);
    });
}

#[test]
fn a_height_not_later_than_now_is_refused() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path(), true).await;
        let now = schedule(1, "probe", vec![set(b"ran", b"now")]);
        let past = schedule(0, "probe", vec![set(b"ran", b"past")]);
        apply(&mut host, 1, vec![asks("awaiter", vec![now, past])]).await;
        let refused: Vec<_> = answers(&host, "awaiter")
            .into_iter()
            .map(|answer| answer.unwrap_err().reason)
            .collect();
        assert_eq!(refused, [reason::INVALID_INPUT, reason::INVALID_INPUT]);
        let direct = submit("scheduler", schedule(5, "probe", Vec::new()));
        let applied = apply(&mut host, 2, vec![direct]).await;
        let Outcome::Rejected(refusal) = &applied.submissions[0].outcome else {
            panic!("a key schedules nothing");
        };
        assert_eq!(refusal.reason, reason::UNAUTHORIZED);
        until(&mut host, 3, 5).await;
        assert_eq!(entry(&host, 0).await, None);
        assert_eq!(stored(&host, "probe", b"ran"), None);
    });
}

#[test]
fn only_the_scheduling_program_cancels_and_a_cancelled_entry_never_runs() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path(), true).await;
        let steps = vec![set(b"ran", b"yes")];
        apply(
            &mut host,
            1,
            vec![asks("awaiter", vec![schedule(4, "probe", steps)])],
        )
        .await;
        let [id] = scheduled(&host, "awaiter")[..] else {
            panic!("one entry was scheduled");
        };
        let cancel = abi::encode(&scheduler::Op::Cancel { entry: id });

        apply(&mut host, 2, vec![asks("other", vec![cancel.clone()])]).await;
        let [Err(refusal)] = &answers(&host, "other")[..] else {
            panic!("another program cannot cancel the entry");
        };
        assert_eq!(refusal.reason, reason::UNAUTHORIZED);
        assert!(entry(&host, id).await.is_some());

        let ours = Submission {
            salt: Some(1),
            ..asks("awaiter", vec![cancel])
        };
        apply(&mut host, 3, vec![ours]).await;
        assert_eq!(answers(&host, "awaiter"), vec![Ok(Vec::new())]);
        assert_eq!(entry(&host, id).await, None);

        until(&mut host, 4, 5).await;
        assert_eq!(stored(&host, "probe", b"ran"), None);
    });
}

#[test]
fn a_cancel_after_the_run_is_refused() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path(), true).await;
        apply(
            &mut host,
            1,
            vec![asks("awaiter", vec![schedule(2, "probe", Vec::new())])],
        )
        .await;
        let [id] = scheduled(&host, "awaiter")[..] else {
            panic!("one entry was scheduled");
        };
        apply(&mut host, 2, Vec::new()).await;
        let cancel = abi::encode(&scheduler::Op::Cancel { entry: id });
        let late = Submission {
            salt: Some(1),
            ..asks("awaiter", vec![cancel])
        };
        apply(&mut host, 3, vec![late]).await;
        let [Err(refusal)] = &answers(&host, "awaiter")[..] else {
            panic!("a run that ran is not cancelled");
        };
        assert_eq!(refusal.reason, reason::WRONG_STATE);
        assert!(replies(&receipt(&host, id).await).is_empty());
    });
}

#[test]
fn entries_due_at_one_height_run_in_the_order_they_were_made() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path(), true).await;
        let ordered = |index: u8| {
            let steps = vec![
                Step::Op(HostOp::Get(b"last".to_vec())),
                set(b"last", &[index]),
            ];
            schedule(3, "probe", steps)
        };
        let made = vec![ordered(0), ordered(1), ordered(2)];
        apply(&mut host, 1, vec![asks("awaiter", made)]).await;
        let ids = scheduled(&host, "awaiter");
        until(&mut host, 2, 3).await;
        let mut seen = Vec::new();
        for id in ids {
            let receipt = receipt(&host, id).await;
            let [
                Reply::Host(HostReply::Value(before)),
                Reply::Host(HostReply::Done),
            ] = &replies(&receipt)[..]
            else {
                panic!("each run read the last index and set its own");
            };
            seen.push(before.clone());
        }
        assert_eq!(seen, [None, Some(vec![0]), Some(vec![1])]);
        assert_eq!(stored(&host, "probe", b"last"), Some(vec![2]));
    });
}

#[test]
fn a_refused_run_undoes_only_its_own_writes_and_its_receipt_records_the_refusal() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path(), true).await;
        let refusing = schedule(
            2,
            "probe",
            vec![set(b"refused", b"yes"), Step::Fail("no".into())],
        );
        let applying = schedule(2, "probe", vec![set(b"applied", b"yes")]);
        apply(
            &mut host,
            1,
            vec![asks("awaiter", vec![refusing, applying])],
        )
        .await;
        let [refused, applied] = scheduled(&host, "awaiter")[..] else {
            panic!("two entries were scheduled");
        };
        apply(&mut host, 2, Vec::new()).await;
        let Outcome::Rejected(refusal) = receipt(&host, refused).await.outcome else {
            panic!("the refusing run was rejected");
        };
        assert_eq!(refusal.reason, "probe");
        assert_eq!(stored(&host, "probe", b"refused"), None);
        replies(&receipt(&host, applied).await);
        assert_eq!(stored(&host, "probe", b"applied"), Some(b"yes".to_vec()));
    });
}

#[test]
fn a_scheduled_run_can_schedule_a_later_entry() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path(), true).await;
        let later = Message {
            target: "scheduler".into(),
            payload: schedule(5, "probe", vec![set(b"later", b"yes")]),
            reply: false,
            acting: None,
        };
        let first = schedule(3, "probe", vec![Step::Op(HostOp::Emit(later))]);
        apply(&mut host, 1, vec![asks("awaiter", vec![first])]).await;
        let [first] = scheduled(&host, "awaiter")[..] else {
            panic!("one entry was scheduled");
        };
        until(&mut host, 2, 3).await;
        replies(&receipt(&host, first).await);
        let second = entry(&host, first + 1)
            .await
            .expect("the run scheduled one");
        assert_eq!(
            (second.scheduler.as_str(), second.height, second.receipt),
            ("probe", 5, None)
        );
        apply(&mut host, 4, Vec::new()).await;
        assert_eq!(stored(&host, "probe", b"later"), None);
        apply(&mut host, 5, Vec::new()).await;
        assert_eq!(stored(&host, "probe", b"later"), Some(b"yes".to_vec()));
        replies(&receipt(&host, first + 1).await);
    });
}

#[test]
fn a_scheduled_run_cannot_schedule_its_own_height() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path(), true).await;
        let again = Message {
            target: "scheduler".into(),
            payload: schedule(3, "probe", Vec::new()),
            reply: false,
            acting: None,
        };
        let first = schedule(3, "probe", vec![Step::Op(HostOp::Emit(again))]);
        apply(&mut host, 1, vec![asks("awaiter", vec![first])]).await;
        let [first] = scheduled(&host, "awaiter")[..] else {
            panic!("one entry was scheduled");
        };
        until(&mut host, 2, 3).await;
        let Outcome::Rejected(refusal) = receipt(&host, first).await.outcome else {
            panic!("the run's schedule for its own height was refused");
        };
        assert_eq!(refusal.reason, reason::INVALID_INPUT);
        assert_eq!(entry(&host, first + 1).await, None);
    });
}

#[test]
fn with_no_scheduler_row_bound_nothing_runs() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path(), false).await;
        let steps = vec![set(b"ran", b"yes")];
        apply(
            &mut host,
            1,
            vec![asks("awaiter", vec![schedule(2, "probe", steps)])],
        )
        .await;
        let [id] = scheduled(&host, "awaiter")[..] else {
            panic!("the scheduler program took the entry");
        };
        until(&mut host, 2, 3).await;
        assert_eq!(stored(&host, "probe", b"ran"), None);
        assert_eq!(entry(&host, id).await.expect("kept").receipt, None);
    });
}

#[test]
fn an_entry_an_earlier_run_of_its_block_cancels_does_not_run() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path(), true).await;
        let emit = |payload: Vec<u8>| {
            Step::Op(HostOp::Emit(Message {
                target: "scheduler".into(),
                payload,
                reply: false,
                acting: None,
            }))
        };
        let cancel_next = emit(abi::encode(&scheduler::Op::Cancel { entry: 2 }));
        let both = vec![
            emit(schedule(4, "probe", vec![cancel_next])),
            emit(schedule(4, "probe", vec![set(b"cancelled", b"ran")])),
        ];
        apply(
            &mut host,
            1,
            vec![asks("awaiter", vec![schedule(2, "probe", both)])],
        )
        .await;
        until(&mut host, 2, 3).await;
        let cancelled = entry(&host, 2).await.expect("the second entry waits");
        assert_eq!(
            (cancelled.scheduler.as_str(), cancelled.height),
            ("probe", 4)
        );
        apply(&mut host, 4, Vec::new()).await;
        replies(&receipt(&host, 1).await);
        assert_eq!(entry(&host, 2).await, None);
        assert_eq!(stored(&host, "probe", b"cancelled"), None);
    });
}
