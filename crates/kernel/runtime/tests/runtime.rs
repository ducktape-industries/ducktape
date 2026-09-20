use std::collections::BTreeMap;

use abi::{
    Cause, CryptoOp, CryptoReply, Entry, Env, GuestCall, HostOp, HostReply, Message, Origin,
    Refusal, Scan, reason,
};
use borsh::{BorshDeserialize, BorshSerialize};
use runtime::{Fault, Host, Limits, Runtime};
use sha2::{Digest as _, Sha256};

const PROBE: &[u8] = include_bytes!("fixtures/probe.wasm");

#[derive(BorshSerialize, BorshDeserialize)]
enum Step {
    Op(HostOp),
    Spin,
    Grow(u32),
    Fail(String),
}

#[derive(Default)]
struct Bench {
    seen: Vec<HostOp>,
    state: BTreeMap<Vec<u8>, Vec<u8>>,
}

fn env() -> Env {
    Env {
        height: 12,
        time: 34,
        me: "probe".into(),
        origin: Origin::External(vec![7; 32]),
        cause: Cause::Direct,
    }
}

#[async_trait::async_trait(?Send)]
impl Host for Bench {
    async fn call(&mut self, op: HostOp) -> HostReply {
        let reply = match &op {
            HostOp::Env => HostReply::Env(env()),
            HostOp::Get(key) | HostOp::CommittedGet(key) => {
                HostReply::Value(self.state.get(key).cloned())
            }
            HostOp::Set { key, value } => {
                self.state.insert(key.clone(), value.clone());
                HostReply::Done
            }
            HostOp::Delete(key) => {
                self.state.remove(key);
                HostReply::Done
            }
            HostOp::Scan(scan) | HostOp::CommittedScan(scan) => HostReply::Entries(
                self.state
                    .iter()
                    .filter(|(key, _)| scan.admits(key))
                    .map(|(key, value)| Entry {
                        key: key.clone(),
                        value: value.clone(),
                    })
                    .collect(),
            ),
            HostOp::Query { program, request } => HostReply::Query(answer(program, request)),
            HostOp::Crypto(CryptoOp::Sha256(bytes)) => {
                HostReply::Crypto(CryptoReply::Digest(Sha256::digest(bytes).into()))
            }
            HostOp::Emit(_) | HostOp::Event(_) | HostOp::Output(_) => HostReply::Done,
            other => HostReply::Refused(Refusal::new(
                reason::UNSUPPORTED,
                format!("the bench does not serve {other:?}"),
            )),
        };
        self.seen.push(op);
        reply
    }
}

fn answer(program: &str, request: &[u8]) -> Result<Vec<u8>, Refusal> {
    let program_is_the_oracle = program == "oracle";
    if program_is_the_oracle {
        return Ok(request.iter().rev().copied().collect());
    }
    Err(Refusal::new(reason::UNKNOWN_PROGRAM, program.to_owned()))
}

fn script(steps: Vec<Step>) -> Vec<u8> {
    abi::encode(&steps)
}

fn replies(bench: &Bench) -> Vec<HostReply> {
    let Some(HostOp::Output(bytes)) = bench.seen.last() else {
        panic!("the probe ends an execute with its replies as output");
    };
    abi::decode(bytes).unwrap()
}

async fn execute(limits: Limits, steps: Vec<Step>) -> (Bench, Result<abi::GuestReply, Fault>) {
    let runtime = Runtime::new(limits);
    let code = runtime.load(PROBE).unwrap();
    let mut bench = Bench::default();
    let verdict = runtime
        .run(&code, GuestCall::Execute(script(steps)), &mut bench)
        .await;
    (bench, verdict)
}

#[tokio::test]
async fn every_host_op_crosses_the_boundary_and_back() {
    let steps = vec![
        Step::Op(HostOp::Env),
        Step::Op(HostOp::Set {
            key: b"k/1".to_vec(),
            value: b"one".to_vec(),
        }),
        Step::Op(HostOp::Set {
            key: b"k/2".to_vec(),
            value: b"two".to_vec(),
        }),
        Step::Op(HostOp::Get(b"k/1".to_vec())),
        Step::Op(HostOp::Delete(b"k/1".to_vec())),
        Step::Op(HostOp::Get(b"k/1".to_vec())),
        Step::Op(HostOp::Scan(Scan::prefix(b"k/"))),
        Step::Op(HostOp::Query {
            program: "oracle".into(),
            request: vec![1, 2, 3],
        }),
        Step::Op(HostOp::Query {
            program: "nobody".into(),
            request: vec![],
        }),
        Step::Op(HostOp::Emit(Message {
            target: "oracle".into(),
            payload: vec![9],
            reply: true,
        })),
        Step::Op(HostOp::Event(b"happened".to_vec())),
        Step::Op(HostOp::Crypto(CryptoOp::Sha256(b"abc".to_vec()))),
        Step::Op(HostOp::BlobStat(abi::BlobId::Sha1([0; 20]))),
    ];
    let (bench, verdict) = execute(Limits::default(), steps).await;
    assert_eq!(verdict, Ok(Ok(Vec::new())));
    assert_eq!(bench.seen.len(), 14);
    assert_eq!(
        replies(&bench),
        vec![
            HostReply::Env(env()),
            HostReply::Done,
            HostReply::Done,
            HostReply::Value(Some(b"one".to_vec())),
            HostReply::Done,
            HostReply::Value(None),
            HostReply::Entries(vec![Entry {
                key: b"k/2".to_vec(),
                value: b"two".to_vec(),
            }]),
            HostReply::Query(Ok(vec![3, 2, 1])),
            HostReply::Query(Err(Refusal::new(reason::UNKNOWN_PROGRAM, "nobody"))),
            HostReply::Done,
            HostReply::Done,
            HostReply::Crypto(CryptoReply::Digest(Sha256::digest(b"abc").into())),
            HostReply::Refused(Refusal::new(
                reason::UNSUPPORTED,
                "the bench does not serve BlobStat(BlobId(Sha1:0000000000000000000000000000000000000000))",
            )),
        ]
    );
}

#[tokio::test]
async fn a_query_answers_with_bytes_and_leaves_no_output() {
    let runtime = Runtime::new(Limits::default());
    let code = runtime.load(PROBE).unwrap();
    let mut bench = Bench::default();
    bench.state.insert(b"a".to_vec(), b"1".to_vec());
    let steps = script(vec![Step::Op(HostOp::Get(b"a".to_vec()))]);
    let verdict = runtime
        .run(&code, GuestCall::Query(steps), &mut bench)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        abi::decode::<Vec<HostReply>>(&verdict).unwrap(),
        vec![HostReply::Value(Some(b"1".to_vec()))]
    );
    assert_eq!(bench.seen, vec![HostOp::Get(b"a".to_vec())]);
}

#[tokio::test]
async fn init_runs_the_program_once_with_its_parameters() {
    let runtime = Runtime::new(Limits::default());
    let code = runtime.load(PROBE).unwrap();
    let mut bench = Bench::default();
    let steps = script(vec![Step::Op(HostOp::Set {
        key: b"born".to_vec(),
        value: b"yes".to_vec(),
    })]);
    let verdict = runtime.run(&code, GuestCall::Init(steps), &mut bench).await;
    assert_eq!(verdict, Ok(Ok(Vec::new())));
    assert_eq!(bench.state.get(b"born".as_slice()), Some(&b"yes".to_vec()));
}

#[tokio::test]
async fn a_refusal_is_the_programs_own_verdict() {
    let (bench, verdict) = execute(
        Limits::default(),
        vec![
            Step::Op(HostOp::Set {
                key: b"before".to_vec(),
                value: vec![],
            }),
            Step::Fail("no thanks".into()),
        ],
    )
    .await;
    assert_eq!(verdict, Ok(Err(Refusal::new("probe", "no thanks"))));
    assert_eq!(bench.seen.len(), 1);
}

#[tokio::test]
async fn an_undecodable_payload_is_refused_not_faulted() {
    let runtime = Runtime::new(Limits::default());
    let code = runtime.load(PROBE).unwrap();
    let mut bench = Bench::default();
    let verdict = runtime
        .run(&code, GuestCall::Execute(vec![0xff; 3]), &mut bench)
        .await
        .unwrap();
    assert_eq!(verdict.unwrap_err().reason, reason::PROTOCOL);
    assert!(bench.seen.is_empty());
}

#[tokio::test]
async fn fuel_runs_out_as_a_trap() {
    let limits = Limits {
        fuel: Some(1_000_000),
        memory_bytes: None,
    };
    let (_, verdict) = execute(limits, vec![Step::Spin]).await;
    assert!(matches!(verdict, Err(Fault::Trap(_))), "{verdict:?}");
}

#[tokio::test]
async fn fuel_is_only_metered_when_a_limit_is_set() {
    let limits = Limits {
        fuel: Some(10),
        memory_bytes: None,
    };
    let (_, metered) = execute(limits, vec![Step::Op(HostOp::Env)]).await;
    assert!(matches!(metered, Err(Fault::Trap(_))), "{metered:?}");
    let (_, unmetered) = execute(Limits::default(), vec![Step::Op(HostOp::Env)]).await;
    assert_eq!(unmetered, Ok(Ok(Vec::new())));
}

#[tokio::test]
async fn memory_past_the_limit_is_a_trap() {
    let limits = Limits {
        fuel: None,
        memory_bytes: Some(4 << 20),
    };
    let (_, verdict) = execute(limits, vec![Step::Grow(96)]).await;
    assert!(matches!(verdict, Err(Fault::Trap(_))), "{verdict:?}");
    let (_, within) = execute(limits, vec![Step::Grow(16)]).await;
    assert_eq!(within, Ok(Ok(Vec::new())));
}

#[tokio::test]
async fn bytes_that_are_not_a_program_do_not_load() {
    let runtime = Runtime::new(Limits::default());
    assert!(matches!(runtime.load(b"not wasm"), Err(Fault::Load(_))));
    let no_exports = wat::parse_str("(module)").unwrap();
    let code = runtime.load(&no_exports).unwrap();
    let mut bench = Bench::default();
    let verdict = runtime
        .run(&code, GuestCall::Execute(vec![]), &mut bench)
        .await;
    assert!(matches!(verdict, Err(Fault::Load(_))), "{verdict:?}");
}

#[tokio::test]
async fn the_same_bytes_hash_the_same() {
    let runtime = Runtime::new(Limits::default());
    let one = runtime.load(PROBE).unwrap();
    let two = runtime.load(PROBE).unwrap();
    assert_eq!(one.hash(), two.hash());
    assert_eq!(one.hash(), <[u8; 32]>::from(Sha256::digest(PROBE)));
}
