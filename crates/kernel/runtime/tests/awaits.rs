//! A program that awaits: a run that cannot finish parks, resumes on its live instance or, frozen
//! to an image of its changed pages, on a fresh runtime, until the program finishes.
use std::collections::BTreeMap;
use std::time::Instant;

use abi::{
    Cause, Env, HostOp, HostReply, Invocation, Origin, Principal, ProgramCall, Refusal, Roles,
    reason,
};
use fixture_awaiter::{ANSWERS, Finished, Plan};
use runtime::{Fault, Host, Limits, PAGE_BYTES, Ran, Runtime, Suspended};

const AWAITER: &[u8] = include_bytes!("../../fixtures/wasm/fixture_awaiter.wasm");
const LIMITS: Limits = Limits {
    fuel: Some(500_000_000),
    memory_bytes: Some(64 << 20),
};

#[derive(Default)]
struct Bench {
    tickets: BTreeMap<u64, Vec<u8>>,
    state: BTreeMap<Vec<u8>, Vec<u8>>,
    output: Option<Vec<u8>>,
}

#[async_trait::async_trait]
impl Host for Bench {
    async fn call(&mut self, op: HostOp, _fuel: &mut Option<u64>) -> HostReply {
        match op {
            HostOp::Await { payload, .. } => {
                let ticket = self.tickets.len() as u64 + 1;
                self.tickets.insert(ticket, payload);
                HostReply::Ticket(ticket)
            }
            HostOp::Set { key, value } => {
                self.state.insert(key, value);
                HostReply::Done
            }
            HostOp::Output(bytes) => {
                self.output = Some(bytes);
                HostReply::Done
            }
            other => HostReply::Refused(Refusal::new(
                reason::UNSUPPORTED,
                format!("the bench does not serve {other:?}"),
            )),
        }
    }
}

fn at(height: u64, call: ProgramCall) -> Invocation {
    Invocation {
        env: Env {
            network: b"net".to_vec(),
            height,
            time: height * 10,
            me: "awaiter".into(),
            origin: Origin::External(vec![7; 32]),
            sender: Some(Principal::Account(1)),
            roles: Roles {
                programs: "programs".into(),
                validators: "validators".into(),
                identity: "identity".into(),
                blobs: "blobs".into(),
                awaits: "awaits".into(),
                halo: "halo".into(),
            },
            cause: Cause::Direct,
        },
        call,
    }
}

fn plan(kept: u32, together: bool) -> ProgramCall {
    let plan = Plan {
        target: "oracle".into(),
        acting: None,
        asks: vec![b"first".to_vec(), b"second".to_vec()],
        together,
        kept,
        fail: false,
    };
    ProgramCall::Execute(abi::encode(&plan))
}

fn answer(ticket: u64, answer: &[u8]) -> ProgramCall {
    ProgramCall::Resume {
        ticket,
        answer: Ok(answer.to_vec()),
    }
}

fn parked(ran: Ran) -> Suspended {
    match ran {
        Ran::Parked(suspended) => suspended,
        Ran::Done(reply) => panic!("the program finished ({reply:?}) where it should park"),
    }
}

fn done(ran: Ran) {
    match ran {
        Ran::Done(reply) => assert_eq!(reply, Ok(())),
        Ran::Parked(suspended) => {
            panic!("the program parked ({suspended:?}) where it should finish")
        }
    }
}

fn finished(bench: &Bench) -> Finished {
    abi::decode(bench.output.as_deref().expect("a finished program outputs")).unwrap()
}

fn fresh() -> (Runtime, runtime::Code) {
    let runtime = Runtime::new(LIMITS);
    let code = runtime.load(AWAITER).unwrap();
    (runtime, code)
}

#[tokio::test]
async fn a_frozen_frame_resumes_on_a_fresh_runtime_at_each_await() {
    let mut bench = Bench::default();
    let (runtime, code) = fresh();
    let fuel = &mut LIMITS.fuel.clone();
    let first = parked(
        runtime
            .start(&code, at(12, plan(1000, false)), &mut bench, fuel)
            .await
            .unwrap(),
    );
    let image = runtime.freeze(&first).await.unwrap();
    assert_eq!(bench.tickets, BTreeMap::from([(1, b"first".to_vec())]));
    assert!(bench.state.is_empty());

    let (runtime, code) = fresh();
    let thawed = runtime.thaw(&code, &image, fuel).await.unwrap();
    let second = parked(
        runtime
            .resume(thawed, at(13, answer(1, b"A")), &mut bench, fuel)
            .await
            .unwrap(),
    );
    let kept: Vec<Result<Vec<u8>, Refusal>> = vec![Ok(b"A".to_vec())];
    assert_eq!(bench.state.get(ANSWERS), Some(&abi::encode(&kept)));
    assert_eq!(bench.tickets.get(&2), Some(&b"second".to_vec()));

    let image = runtime.freeze(&second).await.unwrap();
    let (runtime, code) = fresh();
    let thawed = runtime.thaw(&code, &image, fuel).await.unwrap();
    done(
        runtime
            .resume(thawed, at(14, answer(2, b"B")), &mut bench, fuel)
            .await
            .unwrap(),
    );
    let sum = (0..1000u32).fold(0u8, |sum, i| sum.wrapping_add(i as u8));
    assert_eq!(
        finished(&bench),
        Finished {
            answers: vec![Ok(b"A".to_vec()), Ok(b"B".to_vec())],
            height: 14,
            kept: 1000,
            sum,
        }
    );
}

#[tokio::test]
async fn a_live_frame_resumes_without_an_image() {
    let mut bench = Bench::default();
    let (runtime, code) = fresh();
    let fuel = &mut LIMITS.fuel.clone();
    let first = parked(
        runtime
            .start(&code, at(1, plan(0, false)), &mut bench, fuel)
            .await
            .unwrap(),
    );
    let second = parked(
        runtime
            .resume(first, at(1, answer(1, b"A")), &mut bench, fuel)
            .await
            .unwrap(),
    );
    done(
        runtime
            .resume(second, at(1, answer(2, b"B")), &mut bench, fuel)
            .await
            .unwrap(),
    );
    assert_eq!(
        finished(&bench).answers,
        vec![Ok(b"A".to_vec()), Ok(b"B".to_vec())]
    );
}

#[tokio::test]
async fn two_awaits_together_park_once_and_finish_in_either_order() {
    let mut bench = Bench::default();
    let (runtime, code) = fresh();
    let fuel = &mut LIMITS.fuel.clone();
    let both = parked(
        runtime
            .start(&code, at(1, plan(0, true)), &mut bench, fuel)
            .await
            .unwrap(),
    );
    assert_eq!(bench.tickets.len(), 2);
    let one = parked(
        runtime
            .resume(both, at(2, answer(2, b"second")), &mut bench, fuel)
            .await
            .unwrap(),
    );
    done(
        runtime
            .resume(one, at(3, answer(1, b"first")), &mut bench, fuel)
            .await
            .unwrap(),
    );
    assert_eq!(
        finished(&bench).answers,
        vec![Ok(b"first".to_vec()), Ok(b"second".to_vec())]
    );
}

#[tokio::test]
async fn a_run_that_may_not_park_faults_when_its_program_parks() {
    let mut bench = Bench::default();
    let (runtime, code) = fresh();
    let ran = runtime
        .run(
            &code,
            at(1, plan(0, false)),
            &mut bench,
            &mut LIMITS.fuel.clone(),
        )
        .await;
    assert!(matches!(ran, Err(Fault::Protocol(_))), "{ran:?}");
}

#[tokio::test]
#[ignore = "a measurement: run with --ignored --nocapture"]
async fn measure_what_a_parked_frame_costs() {
    let (runtime, code) = fresh();
    for kept in [0u32, 64 << 10, 1 << 20, 8 << 20] {
        let mut bench = Bench::default();
        let fuel = &mut LIMITS.fuel.clone();
        let started = Instant::now();
        let first = parked(
            runtime
                .start(&code, at(1, plan(kept, false)), &mut bench, fuel)
                .await
                .unwrap(),
        );
        let image = runtime.freeze(&first).await.unwrap();
        let parked_in = started.elapsed();
        let fuel = &mut LIMITS.fuel.clone();
        let resumed_at = Instant::now();
        let thawed = runtime.thaw(&code, &image, fuel).await.unwrap();
        let second = parked(
            runtime
                .resume(thawed, at(2, answer(1, b"A")), &mut bench, fuel)
                .await
                .unwrap(),
        );
        let resumed_in = resumed_at.elapsed();
        let spent_resume = LIMITS.fuel.unwrap() - fuel.unwrap();
        let image = runtime.freeze(&second).await.unwrap();
        println!(
            "kept {kept:>8} B: memory {:>9} B, image {:>5} pages of {PAGE_BYTES} B ({:>9} B); park {parked_in:>8.2?}, resume {resumed_in:>8.2?} ({spent_resume} fuel)",
            image.length,
            image.pages.len(),
            image.kept_bytes(),
        );
    }
}
