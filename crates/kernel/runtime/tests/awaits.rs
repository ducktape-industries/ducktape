//! A program that awaits: each run that cannot finish parks as an image of its memory, and a
//! fresh runtime resumes that image with a ticket's answer until the program finishes.
use std::collections::BTreeMap;
use std::time::Instant;

use abi::{
    Cause, Env, HostOp, HostReply, Invocation, Origin, Principal, ProgramCall, Refusal, Roles,
    reason,
};
use fixture_awaiter::{Finished, Plan};
use runtime::{Fault, Host, Image, Limits, PAGE_BYTES, Ran, Runtime};

const AWAITER: &[u8] = include_bytes!("../../fixtures/wasm/fixture_awaiter.wasm");
const LIMITS: Limits = Limits {
    fuel: Some(500_000_000),
    memory_bytes: Some(64 << 20),
};

#[derive(Default)]
struct Bench {
    tickets: BTreeMap<u64, (String, Vec<u8>)>,
    state: BTreeMap<Vec<u8>, Vec<u8>>,
    output: Option<Vec<u8>>,
}

#[async_trait::async_trait]
impl Host for Bench {
    async fn call(&mut self, op: HostOp, _fuel: &mut Option<u64>) -> HostReply {
        match op {
            HostOp::Await { target, payload } => {
                let ticket = self.tickets.len() as u64 + 1;
                self.tickets.insert(ticket, (target, payload));
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
            },
            cause: Cause::Direct,
        },
        call,
    }
}

fn plan(kept: u32, together: bool) -> ProgramCall {
    ProgramCall::Execute(abi::encode(&Plan { kept, together }))
}

fn answer(ticket: u64, answer: &[u8]) -> ProgramCall {
    ProgramCall::Resume {
        ticket,
        answer: Ok(answer.to_vec()),
    }
}

fn asked(what: &[u8]) -> (String, Vec<u8>) {
    ("oracle".into(), what.to_vec())
}

fn parked(ran: Ran) -> Image {
    match ran {
        Ran::Parked(image) => image,
        Ran::Done(reply) => panic!("the program finished ({reply:?}) where it should park"),
    }
}

fn finished(bench: &Bench) -> Finished {
    abi::decode(bench.output.as_deref().expect("a finished program outputs")).unwrap()
}

async fn fresh() -> (Runtime, runtime::Code) {
    let runtime = Runtime::new(LIMITS);
    let code = runtime.load(AWAITER).unwrap();
    (runtime, code)
}

#[tokio::test]
async fn a_frame_parks_at_each_await_and_a_fresh_runtime_resumes_it() {
    let mut bench = Bench::default();
    let (runtime, code) = fresh().await;
    let started = runtime
        .start(
            &code,
            at(12, plan(1000, false)),
            &mut bench,
            &mut LIMITS.fuel.clone(),
        )
        .await
        .unwrap();
    let first = parked(started);
    assert_eq!(bench.tickets, BTreeMap::from([(1, asked(b"first"))]));
    assert!(bench.state.is_empty());

    let (runtime, code) = fresh().await;
    let resumed = runtime
        .resume(
            &code,
            &first,
            at(13, answer(1, b"A")),
            &mut bench,
            &mut LIMITS.fuel.clone(),
        )
        .await
        .unwrap();
    let second = parked(resumed);
    assert_eq!(bench.state.get(b"first".as_slice()), Some(&b"A".to_vec()));
    assert_eq!(bench.tickets.get(&2), Some(&asked(b"second")));

    let (runtime, code) = fresh().await;
    let done = runtime
        .resume(
            &code,
            &second,
            at(14, answer(2, b"B")),
            &mut bench,
            &mut LIMITS.fuel.clone(),
        )
        .await
        .unwrap();
    assert_eq!(done, Ran::Done(Ok(())));
    let sum = (0..1000u32).fold(0u8, |sum, i| sum.wrapping_add(i as u8));
    assert_eq!(
        finished(&bench),
        Finished {
            first: b"A".to_vec(),
            second: b"B".to_vec(),
            height: 14,
            kept: 1000,
            sum,
        }
    );
}

#[tokio::test]
async fn two_awaits_together_park_once_and_finish_in_either_order() {
    let mut bench = Bench::default();
    let (runtime, code) = fresh().await;
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
            .resume(&code, &both, at(2, answer(2, b"second")), &mut bench, fuel)
            .await
            .unwrap(),
    );
    let done = runtime
        .resume(&code, &one, at(3, answer(1, b"first")), &mut bench, fuel)
        .await
        .unwrap();
    assert_eq!(done, Ran::Done(Ok(())));
    let finished = finished(&bench);
    assert_eq!(
        (finished.first, finished.second),
        (b"first".to_vec(), b"second".to_vec())
    );
}

#[tokio::test]
async fn a_run_that_may_not_park_faults_when_its_program_parks() {
    let mut bench = Bench::default();
    let (runtime, code) = fresh().await;
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

fn pages(image: &Image) -> impl Iterator<Item = &[u8]> {
    image.memory.chunks(PAGE_BYTES)
}

fn changed(image: &Image, initial: &Image, chunk: usize) -> usize {
    let blank = vec![0u8; chunk];
    image
        .memory
        .chunks(chunk)
        .zip(
            initial
                .memory
                .chunks(chunk)
                .chain(std::iter::repeat(blank.as_slice())),
        )
        .filter(|(now, before)| now != before)
        .count()
        * chunk
}

#[tokio::test]
#[ignore = "a measurement: run with --ignored --nocapture"]
async fn measure_what_a_parked_frame_costs() {
    let (runtime, code) = fresh().await;
    let initial = runtime.initial(&code).await.unwrap();
    println!(
        "initial memory: {} pages, {} bytes",
        initial.memory.len() / PAGE_BYTES,
        initial.memory.len()
    );
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
        let parked_in = started.elapsed();
        let spent_start = LIMITS.fuel.unwrap() - fuel.unwrap();
        let fuel = &mut LIMITS.fuel.clone();
        let resumed_at = Instant::now();
        let second = parked(
            runtime
                .resume(&code, &first, at(2, answer(1, b"A")), &mut bench, fuel)
                .await
                .unwrap(),
        );
        let resumed_in = resumed_at.elapsed();
        let spent_resume = LIMITS.fuel.unwrap() - fuel.unwrap();
        let nonzero = pages(&second)
            .filter(|page| page.iter().any(|byte| *byte != 0))
            .count();
        println!(
            "kept {kept:>8} B: image {:>4} pages ({:>9} B), {:>3} nonzero; changed from the initial image: {:>8} B in 64 KiB pages, {:>8} B in 4 KiB chunks; start {:>8.2?} ({spent_start} fuel), resume {:>8.2?} ({spent_resume} fuel)",
            second.memory.len() / PAGE_BYTES,
            second.memory.len(),
            nonzero,
            changed(&second, &initial, PAGE_BYTES),
            changed(&second, &initial, 4 << 10),
            parked_in,
            resumed_in,
        );
    }
}
