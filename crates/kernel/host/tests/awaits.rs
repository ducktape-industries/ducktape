//! Frames that await: a call answered at once resumes the frame in the same submission, a deferred
//! call parks it until the called program resolves the ticket or a code change ends it.
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use abi::Receipt;
use abi::role::awaits::{self, Answer, Ticket, TicketRow};
use abi::role::{self, programs, validators};
use abi::{BlobId, ItemRef, Message, Origin, Outcome, Principal, Refusal, Scan, reason};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use fixture_awaiter::{ANSWERS, Finished, Plan};
use fixture_oracle::{Ask, Op, owed_key};
use fixture_programs::Change;
use fixture_relay::Script;
use host::{
    Applied, BLOBS, Block, BlockId, Founding, Genesis, Host, Layer, Limits, PARKED, Roles,
    Submission, Tip,
};
use state::{Commitment, SyncTarget, commitment_name};

const PROGRAMS: &[u8] = include_bytes!("../../fixtures/wasm/fixture_programs.wasm");
const VALSET: &[u8] = include_bytes!("../../fixtures/wasm/fixture_valset.wasm");
const IDENTITY: &[u8] = include_bytes!("../../fixtures/wasm/fixture_identity.wasm");
const BLOBS_PROGRAM: &[u8] = include_bytes!("../../fixtures/wasm/fixture_blobs.wasm");
const AWAITS: &[u8] = include_bytes!("../../fixtures/wasm/fixture_awaits.wasm");
const AWAITER: &[u8] = include_bytes!("../../fixtures/wasm/fixture_awaiter.wasm");
const ORACLE: &[u8] = include_bytes!("../../fixtures/wasm/fixture_oracle.wasm");
const RELAY: &[u8] = include_bytes!("../../fixtures/wasm/fixture_relay.wasm");

const SIGNER: &[u8] = b"signer";
const AGENT: u64 = 77;
const TIME: u64 = 1_700_000_000;

type Ctx = deterministic::Context;

fn founding(program: &str, code: &[u8], params: Vec<u8>) -> Founding {
    Founding {
        program: program.to_owned(),
        code: code.to_vec(),
        params,
    }
}

fn genesis() -> Genesis {
    Genesis {
        network: b"net".to_vec(),
        roles: Roles::from_iter([
            (role::PROGRAMS, "programs"),
            (role::VALIDATORS, "valset"),
            (role::IDENTITY, "identity"),
            (role::BLOBS, "blobs"),
            (role::AWAITS, "awaits"),
        ]),
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
                abi::encode(&vec![
                    (SIGNER.to_vec(), 1u64),
                    (b"#runs/awaiter".to_vec(), AGENT),
                ]),
            ),
            founding("blobs", BLOBS_PROGRAM, Vec::new()),
            founding("awaits", AWAITS, Vec::new()),
            founding("awaiter", AWAITER, Vec::new()),
            founding("outer", AWAITER, Vec::new()),
            founding("oracle", ORACLE, Vec::new()),
            founding("echo", RELAY, Vec::new()),
            founding("ping", RELAY, Vec::new()),
        ],
        views: Vec::new(),
        hooks: Vec::new(),
        limits: Limits::default(),
        epoch_length: 1_000,
        time: TIME,
        member_cap: 16,
    }
}

async fn found(context: Ctx, dir: &Path) -> Host<Ctx> {
    found_as(context, "net", dir).await
}

async fn found_as(context: Ctx, name: &str, dir: &Path) -> Host<Ctx> {
    Host::found(context, name, dir, block_id(0), genesis())
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

fn plan(target: &str, asks: Vec<Vec<u8>>, together: bool, fail: bool) -> Vec<u8> {
    abi::encode(&Plan {
        target: target.into(),
        acting: None,
        asks,
        together,
        kept: 0,
        fail,
    })
}

fn note(bytes: &[u8]) -> Vec<u8> {
    abi::encode(&Script::Note(bytes.to_vec()))
}

fn later() -> Vec<u8> {
    abi::encode(&Ask::Later)
}

fn answer(ticket: Ticket, bytes: &[u8]) -> Submission {
    let answer = Ok(bytes.to_vec());
    submit("oracle", abi::encode(&Op::Answer { ticket, answer }))
}

fn output(receipt: &Receipt) -> &[u8] {
    match &receipt.outcome {
        Outcome::Applied { output } => output,
        Outcome::Rejected(refusal) => panic!("{} was rejected: {refusal}", receipt.program),
    }
}

fn refusal(receipt: &Receipt) -> &Refusal {
    match &receipt.outcome {
        Outcome::Applied { .. } => panic!("{} was applied", receipt.program),
        Outcome::Rejected(refusal) => refusal,
    }
}

fn find<'r>(receipt: &'r Receipt, program: &str) -> Vec<&'r Receipt> {
    let mut found = Vec::new();
    let here = receipt.program == program;
    if here {
        found.push(receipt);
    }
    for nested in &receipt.nested {
        found.extend(find(nested, program));
    }
    found
}

fn finished_in(receipt: &Receipt, program: &str) -> Finished {
    let runs = find(receipt, program);
    let last = runs.last().expect("the program ran");
    abi::decode(output(last)).unwrap()
}

fn stored(host: &Host<Ctx>, program: &str, key: &[u8]) -> Option<Vec<u8>> {
    host.view(Layer::Confirmed).get(program, key).unwrap()
}

fn answers(host: &Host<Ctx>, program: &str) -> Vec<Answer> {
    let bytes = stored(host, program, ANSWERS).expect("the program kept its answers");
    abi::decode(&bytes).unwrap()
}

fn frames(host: &Host<Ctx>) -> usize {
    host.view(Layer::Confirmed)
        .scan(PARKED, &Scan::prefix(b"frame/"))
        .unwrap()
        .len()
}

async fn ticket(host: &Host<Ctx>, ticket: Ticket) -> Option<TicketRow> {
    let asked = abi::encode(&awaits::Query::Ticket { ticket });
    let reply = host
        .query(Layer::Confirmed, TIME, Origin::System, "awaits", asked)
        .await
        .unwrap()
        .unwrap();
    match abi::decode(&reply).unwrap() {
        awaits::Reply::Ticket(row) => row,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_call_answered_at_once_resumes_the_frame_in_the_same_submission() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path()).await;
        let asks = vec![note(b"first"), note(b"second")];
        let applied = apply(
            &mut host,
            1,
            vec![submit("awaiter", plan("echo", asks, false, false))],
        )
        .await;
        let finished = finished_in(&applied.submissions[0], "awaiter");
        assert_eq!(
            finished.answers,
            vec![Ok(b"tsrif".to_vec()), Ok(b"dnoces".to_vec())]
        );
        assert_eq!(finished.height, 1);
        assert_eq!(frames(&host), 0);
        assert!(find(&applied.submissions[0], "awaits").is_empty());
    });
}

#[test]
fn a_call_acts_as_an_account_only_when_identity_says_its_program_runs_it() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path()).await;
        let asking = |acting| {
            let plan = Plan {
                target: "oracle".into(),
                acting,
                asks: vec![abi::encode(&Ask::Sender)],
                together: false,
                kept: 0,
                fail: false,
            };
            submit("awaiter", abi::encode(&plan))
        };
        let submissions = vec![asking(None), asking(Some(AGENT)), asking(Some(AGENT + 1))];
        let applied = apply(&mut host, 1, submissions).await;
        let answered = |index: usize| finished_in(&applied.submissions[index], "awaiter").answers;
        let [Ok(itself)]: [Answer; 1] = answered(0).try_into().unwrap() else {
            panic!("the call as itself is answered");
        };
        let itself: Option<Principal> = abi::decode(&itself).unwrap();
        assert!(
            matches!(itself, Some(Principal::Account(number)) if number != AGENT),
            "{itself:?}"
        );
        assert_eq!(
            answered(1),
            vec![Ok(abi::encode(&Some(Principal::Account(AGENT))))]
        );
        assert_eq!(
            answered(2),
            vec![Err(Refusal::new(reason::UNAUTHORIZED, "awaiter"))]
        );
    });
}

#[test]
fn a_deferred_call_parks_the_frame_until_the_called_program_answers() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path()).await;
        let asks = vec![later(), later()];
        let applied = apply(
            &mut host,
            1,
            vec![submit("awaiter", plan("oracle", asks, false, false))],
        )
        .await;
        output(&applied.submissions[0]);
        assert_eq!(frames(&host), 1);
        assert!(stored(&host, "oracle", &owed_key(0)).is_some());
        let row = ticket(&host, 0).await.expect("the call holds a ticket");
        assert_eq!(
            (row.caller.as_str(), row.target.as_str(), row.opened),
            ("awaiter", "oracle", 1)
        );

        apply(&mut host, 2, vec![answer(0, b"A")]).await;
        assert_eq!(answers(&host, "awaiter"), vec![Ok(b"A".to_vec())]);
        assert_eq!(ticket(&host, 0).await, None);
        assert!(ticket(&host, 1).await.is_some());
        assert_eq!(frames(&host), 1);

        let applied = apply(&mut host, 3, vec![answer(1, b"B")]).await;
        let finished = finished_in(&applied.submissions[0], "awaiter");
        assert_eq!(finished.answers, vec![Ok(b"A".to_vec()), Ok(b"B".to_vec())]);
        assert_eq!(finished.height, 3);
        assert_eq!(frames(&host), 0);
        assert_eq!(ticket(&host, 1).await, None);
        let parked = host
            .view(Layer::Confirmed)
            .scan(PARKED, &Scan::prefix(b"page/"))
            .unwrap();
        assert!(parked.is_empty());
    });
}

#[test]
fn calls_awaited_together_park_once_and_resume_in_any_order() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path()).await;
        let now = abi::encode(&Ask::Now(b"now".to_vec()));
        let refuse = abi::encode(&Ask::Refuse("no".into()));
        let asks = vec![later(), now, refuse, later()];
        apply(
            &mut host,
            1,
            vec![submit("awaiter", plan("oracle", asks, true, false))],
        )
        .await;
        assert_eq!(frames(&host), 1);
        assert!(ticket(&host, 0).await.is_some() && ticket(&host, 3).await.is_some());
        assert_eq!(ticket(&host, 1).await, None);
        apply(&mut host, 2, vec![answer(3, b"second")]).await;
        assert_eq!(frames(&host), 1);
        let applied = apply(&mut host, 3, vec![answer(0, b"first")]).await;
        let finished = finished_in(&applied.submissions[0], "awaiter");
        let answers = vec![
            Ok(b"first".to_vec()),
            Ok(b"now".to_vec()),
            Err(Refusal::new(reason::INVALID_INPUT, "no")),
            Ok(b"second".to_vec()),
        ];
        assert_eq!(finished.answers, answers);
        assert_eq!(frames(&host), 0);
    });
}

#[test]
fn only_the_called_program_resolves_its_ticket_and_only_the_kernel_parks() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path()).await;
        apply(
            &mut host,
            1,
            vec![submit(
                "awaiter",
                plan("oracle", vec![later()], false, false),
            )],
        )
        .await;
        let forged = awaits::Op::Resolve {
            ticket: 0,
            answer: Ok(b"forged".to_vec()),
        };
        let park = awaits::Op::Park {
            frame: 7,
            program: "awaiter".into(),
            calls: Vec::new(),
        };
        let applied = apply(
            &mut host,
            2,
            vec![
                submit("awaits", abi::encode(&forged)),
                submit("awaits", abi::encode(&park)),
            ],
        )
        .await;
        assert_eq!(
            refusal(&applied.submissions[0]).reason,
            reason::UNAUTHORIZED
        );
        assert_eq!(
            refusal(&applied.submissions[1]).reason,
            reason::UNAUTHORIZED
        );
        assert!(ticket(&host, 0).await.is_some());
        assert_eq!(frames(&host), 1);
    });
}

#[test]
fn a_parked_frame_replies_to_the_message_that_started_it_once_it_finishes() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path()).await;
        let message = Message {
            target: "awaiter".into(),
            payload: plan("oracle", vec![later()], false, false),
            reply: true,
            acting: None,
        };
        let send = abi::encode(&Script::Send(vec![message]));
        let applied = apply(&mut host, 1, vec![submit("ping", send)]).await;
        let sent: Vec<ItemRef> = abi::decode(output(&applied.submissions[0])).unwrap();
        let item = sent[0].clone();
        assert_eq!(applied.submissions[0].nested.len(), 1);
        assert_eq!(stored(&host, "ping", &fixture_relay::done(&item)), None);

        let applied = apply(&mut host, 2, vec![answer(0, b"A")]).await;
        let finished = finished_in(&applied.submissions[0], "awaiter");
        let done = stored(&host, "ping", &fixture_relay::done(&item)).expect("ping got its reply");
        let outcome = Outcome::Applied {
            output: abi::encode(&finished),
        };
        assert_eq!(done, abi::encode(&outcome));
    });
}

#[test]
fn a_called_frame_that_parks_answers_its_caller_once_it_finishes() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path()).await;
        let inner = plan("oracle", vec![later()], false, false);
        apply(
            &mut host,
            1,
            vec![submit("outer", plan("awaiter", vec![inner], false, false))],
        )
        .await;
        assert_eq!(frames(&host), 2);
        let applied = apply(&mut host, 2, vec![answer(1, b"A")]).await;
        let inner = finished_in(&applied.submissions[0], "awaiter");
        let outer = finished_in(&applied.submissions[0], "outer");
        assert_eq!(inner.answers, vec![Ok(b"A".to_vec())]);
        assert_eq!(outer.answers, vec![Ok(abi::encode(&inner))]);
        assert_eq!(frames(&host), 0);
    });
}

#[test]
fn a_rejected_resume_keeps_what_earlier_submissions_wrote_and_ends_the_frame() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path()).await;
        apply(
            &mut host,
            1,
            vec![submit(
                "awaiter",
                plan("oracle", vec![later(), later()], false, true),
            )],
        )
        .await;
        apply(&mut host, 2, vec![answer(0, b"A")]).await;
        let applied = apply(&mut host, 3, vec![answer(1, b"B")]).await;
        output(&applied.submissions[0]);
        let resumed = find(&applied.submissions[0], "awaiter");
        assert_eq!(refusal(resumed[0]).reason, reason::INVALID_INPUT);
        assert_eq!(answers(&host, "awaiter"), vec![Ok(b"A".to_vec())]);
        assert_eq!(stored(&host, "oracle", &owed_key(1)), None);
        assert_eq!(frames(&host), 0);
    });
}

#[test]
fn a_program_that_changes_code_cancels_its_parked_frames() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path()).await;
        let message = Message {
            target: "awaiter".into(),
            payload: plan("oracle", vec![later()], false, false),
            reply: true,
            acting: None,
        };
        let send = abi::encode(&Script::Send(vec![message]));
        let applied = apply(&mut host, 1, vec![submit("ping", send)]).await;
        let sent: Vec<ItemRef> = abi::decode(output(&applied.submissions[0])).unwrap();
        let relay: BlobId = host.programs().unwrap()["echo"];
        let swap = abi::encode(&Change::Set(programs::Entry {
            program: "awaiter".into(),
            code: relay,
            params: Vec::new(),
        }));
        apply(&mut host, 2, vec![submit("programs", swap)]).await;
        apply(&mut host, 3, Vec::new()).await;
        assert_eq!(frames(&host), 0);
        assert_eq!(ticket(&host, 0).await, None);
        let done =
            stored(&host, "ping", &fixture_relay::done(&sent[0])).expect("ping got its refusal");
        let cancelled = Outcome::Rejected(awaits::cancelled("awaiter"));
        assert_eq!(done, abi::encode(&cancelled));
        let late = apply(&mut host, 4, vec![answer(0, b"late")]).await;
        output(&late.submissions[0]);
    });
}

#[test]
fn a_joiner_that_syncs_while_a_frame_waits_resumes_it_as_the_network_does() {
    deterministic::Runner::default().start(|context| async move {
        let blocks = |height: u64| match height {
            1 => vec![submit(
                "awaiter",
                plan("oracle", vec![later()], false, false),
            )],
            _ => vec![answer(0, b"A")],
        };
        let twin_dir = tempfile::tempdir().unwrap();
        let mut twin = found_as(context.child("twin"), "twin", twin_dir.path()).await;
        let upstream_dir = tempfile::tempdir().unwrap();
        let mut upstream = found_as(context.child("upstream"), "up", upstream_dir.path()).await;
        apply(&mut twin, 1, blocks(1)).await;
        apply(&mut upstream, 1, blocks(1)).await;
        assert_eq!(frames(&upstream), 1);

        let mut framed = BTreeMap::new();
        for entry in upstream
            .view(Layer::Confirmed)
            .scan(BLOBS, &Scan::prefix(b""))
            .unwrap()
        {
            let id: BlobId = abi::decode(&entry.key).unwrap();
            framed.insert(id, upstream.blob(&id, TIME).await.unwrap().unwrap());
        }
        let targets: BTreeMap<String, SyncTarget> = upstream
            .store()
            .programs()
            .map(|name| {
                let commitment = upstream.store().commitment(name).unwrap();
                (name.clone(), commitment.target().unwrap().unwrap())
            })
            .collect();
        let (_, commitments) = upstream.into_store().into_parts();
        let mut synced = BTreeMap::new();
        for (name, commitment) in commitments {
            let source = Arc::new(commitment.into_db().unwrap());
            let sync = context
                .child("sync")
                .with_attribute("program", name.as_str());
            let target = targets[&name].clone();
            let commitment =
                Commitment::sync_from(sync, &commitment_name("join", &name), target, source)
                    .await
                    .unwrap();
            synced.insert(name, commitment);
        }
        let joiner_dir = tempfile::tempdir().unwrap();
        let mut joiner = Host::adopt(
            context.child("joiner"),
            "join",
            joiner_dir.path(),
            1,
            synced,
        )
        .await
        .unwrap();
        for (id, bytes) in &framed {
            joiner.install(*id, bytes).unwrap();
        }
        assert_eq!(frames(&joiner), 1);

        let applied = apply(&mut joiner, 2, blocks(2)).await;
        apply(&mut twin, 2, blocks(2)).await;
        let finished = finished_in(&applied.submissions[0], "awaiter");
        assert_eq!(finished.answers, vec![Ok(b"A".to_vec())]);
        assert_eq!(frames(&joiner), 0);
        assert_eq!(joiner.root().unwrap(), twin.root().unwrap());
    });
}
