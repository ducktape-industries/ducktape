//! Reads a program asks the kernel for: the call parks its frame, the member the epoch assigns
//! answers with the reply's blob entry, and the frame resumes once the blobs role holds that blob.
#[path = "answering/preparation.rs"]
mod preparation;
#[path = "answering/preparation_pin.rs"]
mod preparation_pin;

use std::path::Path;

use abi::role::reads::{self, Entry, Op, Read, Row, State};
use abi::role::{self, halo, validators};
use abi::{BlobId, Outcome, Receipt, Refusal, Scan, reason};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use fixture_awaiter::{Finished, Plan};
use fixture_wasm::{AWAITER, AWAITS, BLOBS as BLOBS_PROGRAM, HALO, IDENTITY, PROGRAMS, VALSET};
use host::{
    Applied, BLOBS, Block, BlockId, Founding, Genesis, Host, Layer, Limits, PAGE_FUEL, PARKED,
    Submission, Tip,
};

const TIME: u64 = 1_700_000_000;
const SIGNER: &[u8] = b"signer";
const V1: &[u8] = b"v1";
const V2: &[u8] = b"v2";

type Ctx = deterministic::Context;

fn founding(program: &str, code: &[u8], params: Vec<u8>, roles: &[&str]) -> Founding {
    Founding {
        program: program.to_owned(),
        code: code.to_vec(),
        params,
        roles: roles.iter().map(|role| (*role).to_owned()).collect(),
    }
}

fn member(key: &[u8]) -> validators::Member {
    let name = String::from_utf8_lossy(key).into_owned();
    validators::Member {
        key: key.to_vec(),
        address: Some(format!("{name}:1")),
        halo: format!("{name}:1"),
    }
}

fn genesis(members: &[&[u8]]) -> Genesis {
    genesis_with(members, Limits::default(), 0)
}

fn caller(index: usize) -> String {
    format!("awaiter-{index}")
}

fn genesis_with(members: &[&[u8]], limits: Limits, callers: usize) -> Genesis {
    let mut genesis = Genesis {
        network: b"net".to_vec(),
        validators: members.iter().map(|key| member(key)).collect(),
        programs: vec![
            founding("programs", PROGRAMS, Vec::new(), &[role::PROGRAMS]),
            founding("valset", VALSET, Vec::new(), &[role::VALIDATORS]),
            founding(
                "identity",
                IDENTITY,
                abi::encode(&vec![(SIGNER.to_vec(), 1u64)]),
                &[role::IDENTITY],
            ),
            founding("blobs", BLOBS_PROGRAM, Vec::new(), &[role::BLOBS]),
            founding("awaits", AWAITS, Vec::new(), &[role::AWAITS]),
            founding(
                "halo",
                HALO,
                abi::encode(&halo::Genesis { lanes: Vec::new() }),
                &[role::HALO],
            ),
            founding("awaiter", AWAITER, Vec::new(), &[]),
        ],
        views: Vec::new(),
        hooks: Vec::new(),
        limits,
        epoch_length: 1_000,
        time: TIME,
        member_cap: 16,
        queries: validators::QueryPolicy::Signed,
    };
    for index in 0..callers {
        let code = founding(&caller(index), AWAITER, Vec::new(), &[]);
        genesis.programs.push(code);
    }
    genesis
}

async fn found(context: Ctx, dir: &Path, members: &[&[u8]]) -> Host<Ctx> {
    Host::found(context, "net", dir, block_id(0), genesis(members))
        .await
        .unwrap()
        .0
}

fn block_id(height: u64) -> BlockId {
    let mut id = [0u8; 32];
    id[..8].copy_from_slice(&height.to_be_bytes());
    id
}

fn signed(signer: &[u8], target: &str, payload: Vec<u8>) -> Submission {
    Submission {
        signer: signer.to_vec(),
        anchor: Tip {
            height: 0,
            id: block_id(0),
        },
        salt: None,
        target: target.to_owned(),
        payload,
        client: None,
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

fn read(query: &[u8]) -> Submission {
    read_by("awaiter", query)
}

fn read_by(program: &str, query: &[u8]) -> Submission {
    let asked = Op::Read {
        target: "oracle".into(),
        query: query.to_vec(),
    };
    let plan = Plan {
        target: reads::PROGRAM.into(),
        acting: None,
        asks: vec![abi::encode(&asked)],
        together: false,
        kept: 0,
        fail: false,
    };
    signed(SIGNER, program, abi::encode(&plan))
}

const ASKED_AT: u64 = 1;

fn answer(signer: &[u8], id: u64, outcome: Result<Entry, Refusal>) -> Submission {
    answer_at(signer, id, ASKED_AT, outcome)
}

fn answer_at(signer: &[u8], id: u64, at: u64, outcome: Result<Entry, Refusal>) -> Submission {
    let op = Op::Answer { id, at, outcome };
    signed(signer, reads::PROGRAM, abi::encode(&op))
}

fn ready(signer: &[u8], id: u64) -> Submission {
    signed(signer, reads::PROGRAM, abi::encode(&Op::Ready { id }))
}

fn receipt(signer: &[u8], bundle: u64) -> Submission {
    let op = blob::contract::Op::Receipt { bundle };
    signed(signer, "blobs", abi::encode(&op))
}

fn entry(seed: u8, len: u64) -> Entry {
    Entry {
        id: BlobId::Sha256([seed; 32]),
        len,
        kind: reads::KIND.into(),
    }
}

fn refusal(receipt: &Receipt) -> &Refusal {
    match &receipt.outcome {
        Outcome::Applied { .. } => panic!("{} was applied: {receipt:?}", receipt.program),
        Outcome::Rejected(refusal) => refusal,
    }
}

fn applied(receipt: &Receipt) {
    if let Outcome::Rejected(refusal) = &receipt.outcome {
        panic!("{} was rejected: {refusal}", receipt.program);
    }
}

fn find<'r>(receipt: &'r Receipt, program: &str) -> Vec<&'r Receipt> {
    let mut found = Vec::new();
    if receipt.program == program {
        found.push(receipt);
    }
    for nested in &receipt.nested {
        found.extend(find(nested, program));
    }
    found
}

fn finished(receipt: &Receipt) -> Finished {
    let runs = find(receipt, "awaiter");
    let last = runs.last().expect("the awaiter ran");
    match &last.outcome {
        Outcome::Applied { output } => abi::decode(output).unwrap(),
        Outcome::Rejected(refusal) => panic!("the awaiter was rejected: {refusal}"),
    }
}

fn frames(host: &Host<Ctx>) -> usize {
    host.view(Layer::Confirmed)
        .scan(PARKED, &Scan::prefix(b"frame/"))
        .unwrap()
        .len()
}

fn open_reads(host: &Host<Ctx>) -> Vec<(u64, Row)> {
    let prefix = b"read/";
    host.view(Layer::Confirmed)
        .scan(PARKED, &Scan::prefix(prefix))
        .unwrap()
        .into_iter()
        .map(|row| {
            let id = u64::from_be_bytes(row.key[prefix.len()..].try_into().unwrap());
            (id, abi::decode(&row.value).unwrap())
        })
        .collect()
}

fn one_read(host: &Host<Ctx>) -> (u64, Row) {
    let open = open_reads(host);
    let [read]: [(u64, Row); 1] = open.try_into().expect("one read is open");
    read
}

fn rostered(host: &Host<Ctx>, id: &BlobId) -> bool {
    host.view(Layer::Confirmed)
        .get(BLOBS, &abi::encode(id))
        .unwrap()
        .is_some()
}

type Shape = Vec<(String, usize, Option<usize>)>;

fn shape(applied: &Applied) -> Shape {
    applied
        .writes
        .programs
        .iter()
        .flat_map(|(program, keys)| {
            keys.iter()
                .map(move |(key, value)| (program.clone(), key.len(), value.as_ref().map(Vec::len)))
        })
        .collect()
}

fn assigned(host: &Host<Ctx>, id: u64) -> Vec<u8> {
    let keys: Vec<Vec<u8>> = host
        .epoch_members(0)
        .unwrap()
        .unwrap()
        .into_iter()
        .map(|member| member.key)
        .collect();
    reads::assigned(id, &keys).unwrap().to_vec()
}

#[test]
fn a_read_parks_its_caller_and_its_query_never_runs_in_the_block() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path(), &[V1]).await;
        let block = apply(&mut host, 1, vec![read(b"grep /shared")]).await;
        applied(&block.submissions[0]);
        assert_eq!(frames(&host), 1, "the caller parks on its read");
        let (_, row) = one_read(&host);
        let expected = Row {
            requester: "awaiter".into(),
            target: "oracle".into(),
            query: b"grep /shared".to_vec(),
            made: 1,
            state: State::Asked,
        };
        assert_eq!(row, expected);
        assert!(
            find(&block.submissions[0], "oracle").is_empty(),
            "the read's target never runs in the caller's block"
        );
    });
}

async fn answered_and_resumed(context: Ctx, dir: &Path, asked: Entry) -> (Applied, Applied) {
    let mut host = found(context, dir, &[V1]).await;
    apply(&mut host, 1, vec![read(b"grep /shared")]).await;
    let (id, _) = one_read(&host);
    let answering = apply(&mut host, 2, vec![answer(V1, id, Ok(asked.clone()))]).await;
    applied(&answering.submissions[0]);
    let (_, row) = one_read(&host);
    let read = Read {
        entry: asked.clone(),
        at: ASKED_AT,
    };
    assert_eq!(
        row.state,
        State::Putting {
            bundle: 0,
            read: read.clone()
        }
    );
    assert_eq!(frames(&host), 1, "the caller waits until the blob is held");
    let early = apply(&mut host, 3, vec![ready(b"anyone", id), receipt(V1, 0)]).await;
    assert_eq!(refusal(&early.submissions[0]).reason, reason::WRONG_STATE);
    applied(&early.submissions[1]);
    assert!(!rostered(&host, &asked.id));
    let resuming = apply(&mut host, 4, vec![ready(b"anyone else", id)]).await;
    applied(&resuming.submissions[0]);
    let done = finished(&resuming.submissions[0]);
    assert_eq!(done.answers, vec![Ok(abi::encode(&read))]);
    assert_eq!(frames(&host), 0);
    assert!(open_reads(&host).is_empty());
    assert!(rostered(&host, &asked.id), "the caller holds the blob");
    assert!(
        !host.blobs().has(&asked.id),
        "no frame needed the blob's bytes"
    );
    (answering, resuming)
}

#[test]
fn the_answer_and_its_resume_write_the_same_for_ten_kilobytes_and_ten_megabytes() {
    deterministic::Runner::default().start(|context| async move {
        let small_dir = tempfile::tempdir().unwrap();
        let large_dir = tempfile::tempdir().unwrap();
        let (small_answer, small_resume) =
            answered_and_resumed(context.child("small"), small_dir.path(), entry(1, 10 << 10))
                .await;
        let (large_answer, large_resume) =
            answered_and_resumed(context.child("large"), large_dir.path(), entry(2, 10 << 20))
                .await;
        assert_eq!(shape(&small_answer), shape(&large_answer));
        assert_eq!(shape(&small_resume), shape(&large_resume));
    });
}

#[test]
fn with_no_member_seated_a_read_is_refused_at_once() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path(), &[]).await;
        let block = apply(&mut host, 1, vec![read(b"grep /shared")]).await;
        let done = finished(&block.submissions[0]);
        assert_eq!(done.answers, vec![Err(reads::unserved())]);
        assert_eq!(frames(&host), 0);
        assert!(open_reads(&host).is_empty());
    });
}

#[test]
fn only_the_member_the_epoch_assigns_answers_a_read() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path(), &[V1, V2]).await;
        apply(&mut host, 1, vec![read(b"grep /shared")]).await;
        let (id, _) = one_read(&host);
        let mine = assigned(&host, id);
        let other = [V1, V2]
            .into_iter()
            .find(|key| *key != mine.as_slice())
            .unwrap();
        let answers = vec![
            answer(other, id, Ok(entry(1, 1))),
            answer(b"stranger", id, Ok(entry(1, 1))),
            answer(&mine, id, Ok(entry(1, 1))),
            answer(&mine, id, Ok(entry(2, 1))),
        ];
        let block = apply(&mut host, 2, answers).await;
        let refused = |index: usize| refusal(&block.submissions[index]).reason.clone();
        assert_eq!(refused(0), reason::UNAUTHORIZED);
        assert_eq!(refused(1), reason::UNAUTHORIZED);
        applied(&block.submissions[2]);
        assert_eq!(refused(3), reason::WRONG_STATE);
        let late = apply(&mut host, 3, vec![answer(&mine, id + 1, Ok(entry(1, 1)))]).await;
        assert_eq!(refusal(&late.submissions[0]).reason, reason::NOT_FOUND);
    });
}

#[test]
fn an_answer_read_before_its_ask_or_after_its_own_block_is_refused() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path(), &[V1]).await;
        apply(&mut host, 1, vec![read(b"grep /shared")]).await;
        let (id, _) = one_read(&host);
        let answers = vec![
            answer_at(V1, id, ASKED_AT - 1, Ok(entry(1, 1))),
            answer_at(V1, id, 3, Ok(entry(1, 1))),
            answer_at(V1, id, 2, Ok(entry(1, 1))),
        ];
        let block = apply(&mut host, 2, answers).await;
        assert_eq!(refusal(&block.submissions[0]).reason, reason::INVALID_INPUT);
        assert_eq!(refusal(&block.submissions[1]).reason, reason::INVALID_INPUT);
        applied(&block.submissions[2]);
    });
}

#[test]
fn a_read_no_member_answers_within_the_wait_is_refused_to_its_caller() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path(), &[V1]).await;
        apply(&mut host, ASKED_AT, vec![read(b"grep /shared")]).await;
        let (id, _) = one_read(&host);
        let due = ASKED_AT + reads::WAIT;
        for height in ASKED_AT + 1..due {
            apply(&mut host, height, Vec::new()).await;
        }
        assert_eq!(frames(&host), 1, "the read waits until its due height");
        assert_eq!(open_reads(&host).len(), 1);
        let block = apply(&mut host, due, Vec::new()).await;
        assert!(block.submissions.is_empty());
        assert_eq!(frames(&host), 0, "the caller resumed");
        assert!(open_reads(&host).is_empty());
        let kept = host
            .view(Layer::Confirmed)
            .get("awaiter", fixture_awaiter::ANSWERS)
            .unwrap()
            .expect("the awaiter kept its answer");
        let kept: Vec<Result<Vec<u8>, Refusal>> = abi::decode(&kept).unwrap();
        assert_eq!(kept, vec![Err(reads::expired(id))]);
        let late = apply(&mut host, due + 1, vec![answer(V1, id, Ok(entry(1, 1)))]).await;
        assert_eq!(refusal(&late.submissions[0]).reason, reason::NOT_FOUND);
    });
}

type Kept = Option<Vec<Result<Vec<u8>, Refusal>>>;

fn expired_together(fuel: u64, callers: usize) -> Vec<(u64, Kept)> {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let limits = Limits {
            fuel: Some(fuel),
            ..Limits::default()
        };
        let genesis = genesis_with(&[V1], limits, callers);
        let founded = Host::found(
            context.child("found"),
            "net",
            dir.path(),
            block_id(0),
            genesis,
        )
        .await;
        let Ok((mut host, _)) = founded else {
            return Vec::new();
        };
        let reads = (0..callers)
            .map(|index| read_by(&caller(index), b"grep /shared"))
            .collect();
        apply(&mut host, ASKED_AT, reads).await;
        let open = open_reads(&host);
        if open.len() < callers {
            return Vec::new();
        }
        let due = ASKED_AT + reads::WAIT;
        for height in ASKED_AT + 1..=due {
            apply(&mut host, height, Vec::new()).await;
        }
        open.into_iter()
            .map(|(id, row)| {
                let kept = host
                    .view(Layer::Confirmed)
                    .get(&row.requester, fixture_awaiter::ANSWERS)
                    .unwrap()
                    .map(|bytes| abi::decode(&bytes).unwrap());
                (id, kept)
            })
            .collect()
    })
}

fn fits(kept: &[(u64, Kept)]) -> bool {
    !kept.is_empty()
        && kept
            .iter()
            .all(|(id, kept)| *kept == Some(vec![Err(reads::expired(*id))]))
}

#[test]
fn reads_that_expire_in_one_block_each_resume_on_a_frame_budget_of_their_own() {
    let mut fuel = PAGE_FUEL;
    while !fits(&expired_together(fuel, 1)) {
        fuel = fuel
            .checked_mul(2)
            .expect("some budget fits one read and its resume");
    }
    let callers = 8;
    let kept = expired_together(fuel, callers);
    assert_eq!(
        kept.len(),
        callers,
        "every caller parked on its read at {fuel}"
    );
    for (id, kept) in &kept {
        assert_eq!(
            kept.as_ref(),
            Some(&vec![Err(reads::expired(*id))]),
            "read {id} resumed its caller on a budget of {fuel}"
        );
    }
}

#[test]
fn a_query_the_member_refuses_answers_the_caller_with_that_refusal() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path(), &[V1]).await;
        apply(&mut host, 1, vec![read(b"grep /shared")]).await;
        let (id, _) = one_read(&host);
        let refused = Refusal::new(reason::TRAP, "out of fuel");
        let block = apply(&mut host, 2, vec![answer(V1, id, Err(refused.clone()))]).await;
        let done = finished(&block.submissions[0]);
        assert_eq!(done.answers, vec![Err(refused)]);
        assert_eq!(frames(&host), 0);
        assert!(open_reads(&host).is_empty());
    });
}

#[test]
fn a_program_cannot_answer_or_ready_a_read_and_a_frame_cannot_ask_one() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path(), &[V1]).await;
        let asked = Op::Read {
            target: "oracle".into(),
            query: Vec::new(),
        };
        let block = apply(
            &mut host,
            1,
            vec![signed(V1, reads::PROGRAM, abi::encode(&asked))],
        )
        .await;
        assert_eq!(refusal(&block.submissions[0]).reason, reason::UNSUPPORTED);
        let answering = Plan {
            target: reads::PROGRAM.into(),
            acting: None,
            asks: vec![abi::encode(&Op::Ready { id: 0 })],
            together: false,
            kept: 0,
            fail: false,
        };
        let block = apply(
            &mut host,
            2,
            vec![signed(SIGNER, "awaiter", abi::encode(&answering))],
        )
        .await;
        let done = finished(&block.submissions[0]);
        let [Err(refused)]: [Result<Vec<u8>, Refusal>; 1] = done.answers.try_into().unwrap() else {
            panic!("a program's Ready is refused");
        };
        assert_eq!(refused.reason, reason::UNSUPPORTED);
    });
}

#[test]
fn before_the_activation_height_reads_is_no_target_at_all() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path(), &[V1]).await;
        host.set_reads_from(u64::MAX);
        let block = apply(&mut host, 1, vec![read(b"grep /shared")]).await;
        let done = finished(&block.submissions[0]);
        let [Err(refused)]: [Result<Vec<u8>, Refusal>; 1] = done.answers.try_into().unwrap() else {
            panic!("the call is refused");
        };
        assert_eq!(refused.reason, reason::UNKNOWN_PROGRAM);
        assert!(open_reads(&host).is_empty());
        let block = apply(&mut host, 2, vec![answer(V1, 0, Ok(entry(1, 1)))]).await;
        assert_eq!(
            refusal(&block.submissions[0]).reason,
            reason::UNKNOWN_PROGRAM
        );
    });
}
