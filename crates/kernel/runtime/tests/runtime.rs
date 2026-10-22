use std::collections::BTreeMap;

use abi::{
    Cause, CryptoOp, CryptoReply, Entry, Env, HostOp, HostReply, Invocation, ItemRef, Message,
    Origin, Principal, ProgramCall, Refusal, Roles, Scan, reason, role,
};
use fixture_probe::{Reply, Step};
use fixture_wasm::PROBE;
use runtime::{Fault, Host, Limits, Runtime};
use sha2::{Digest as _, Sha256};

#[derive(Default)]
struct Bench {
    seen: Vec<HostOp>,
    state: BTreeMap<Vec<u8>, Vec<u8>>,
    fuel: Vec<Option<u64>>,
}

fn env() -> Env {
    Env {
        network: b"net".to_vec(),
        height: 12,
        time: 34,
        me: "probe".into(),
        origin: Origin::External(vec![7; 32]),
        sender: Some(Principal::Account(1)),
        roles: Roles::from_iter([
            (role::PROGRAMS, "programs"),
            (role::VALIDATORS, "validators"),
            (role::IDENTITY, "identity"),
            (role::BLOBS, "blobs"),
            (role::AWAITS, "awaits"),
            (role::HALO, "halo"),
        ]),
        cause: Cause::Direct,
    }
}

#[async_trait::async_trait]
impl Host for Bench {
    async fn call(&mut self, op: HostOp, fuel: &mut Option<u64>) -> HostReply {
        self.fuel.push(*fuel);
        let reply = match &op {
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
            HostOp::Emit(_) => HostReply::Item(ItemRef {
                source: "probe".into(),
                item: 1,
            }),
            HostOp::Event(_) | HostOp::Output(_) | HostOp::Respond(_) => HostReply::Done,
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

fn invocation(call: ProgramCall) -> Invocation {
    Invocation { env: env(), call }
}

fn replies(bench: &Bench) -> Vec<Reply> {
    let Some(HostOp::Output(bytes)) = bench.seen.last() else {
        panic!("the probe ends an execute with its replies as output");
    };
    abi::decode(bytes).unwrap()
}

async fn execute(limits: Limits, steps: Vec<Step>) -> (Bench, Result<abi::ProgramReply, Fault>) {
    let runtime = Runtime::new(limits);
    let code = runtime.load(PROBE).unwrap();
    let mut bench = Bench::default();
    let verdict = runtime
        .run(
            &code,
            invocation(ProgramCall::Execute(script(steps))),
            &mut bench,
            &mut limits.fuel.clone(),
        )
        .await;
    (bench, verdict)
}

#[tokio::test]
async fn every_host_op_crosses_the_boundary_and_back() {
    let steps = vec![
        Step::Env,
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
            acting: None,
        })),
        Step::Op(HostOp::Event(b"happened".to_vec())),
        Step::Op(HostOp::Crypto(CryptoOp::Sha256(b"abc".to_vec()))),
        Step::Op(HostOp::BlobStat(abi::BlobId::Sha1([0; 20]))),
    ];
    let (bench, verdict) = execute(Limits::default(), steps).await;
    assert_eq!(verdict, Ok(Ok(())));
    assert_eq!(bench.seen.len(), 13);
    let hosted: Vec<HostReply> = vec![
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
        HostReply::Item(ItemRef {
            source: "probe".into(),
            item: 1,
        }),
        HostReply::Done,
        HostReply::Crypto(CryptoReply::Digest(Sha256::digest(b"abc").into())),
        HostReply::Refused(Refusal::new(
            reason::UNSUPPORTED,
            "the bench does not serve BlobStat(BlobId(Sha1:0000000000000000000000000000000000000000))",
        )),
    ];
    let mut expected = vec![Reply::Env(env())];
    expected.extend(hosted.into_iter().map(Reply::Host));
    assert_eq!(replies(&bench), expected);
}

#[tokio::test]
async fn a_query_responds_with_bytes_and_sets_no_output() {
    let runtime = Runtime::new(Limits::default());
    let code = runtime.load(PROBE).unwrap();
    let mut bench = Bench::default();
    bench.state.insert(b"a".to_vec(), b"1".to_vec());
    let steps = script(vec![Step::Op(HostOp::Get(b"a".to_vec()))]);
    let verdict = runtime
        .run(
            &code,
            invocation(ProgramCall::Query(steps)),
            &mut bench,
            &mut None,
        )
        .await;
    assert_eq!(verdict, Ok(Ok(())));
    assert_eq!(
        bench.seen,
        vec![
            HostOp::Get(b"a".to_vec()),
            HostOp::Respond(abi::encode(&vec![Reply::Host(HostReply::Value(Some(
                b"1".to_vec()
            )))])),
        ]
    );
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
    let verdict = runtime
        .run(
            &code,
            invocation(ProgramCall::Init(steps)),
            &mut bench,
            &mut None,
        )
        .await;
    assert_eq!(verdict, Ok(Ok(())));
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
        .run(
            &code,
            invocation(ProgramCall::Execute(vec![0xff; 3])),
            &mut bench,
            &mut None,
        )
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
    let (_, metered) = execute(limits, vec![Step::Env]).await;
    assert!(matches!(metered, Err(Fault::Trap(_))), "{metered:?}");
    let (_, unmetered) = execute(Limits::default(), vec![Step::Env]).await;
    assert_eq!(unmetered, Ok(Ok(())));
}

#[tokio::test]
async fn memory_past_the_limit_is_a_trap() {
    let limits = Limits {
        fuel: None,
        memory_bytes: Some(4 << 20),
    };
    let (_, verdict) = execute(limits, vec![Step::Grow(96)]).await;
    assert_eq!(
        verdict,
        Err(Fault::Trap("the run trapped: unreachable".into()))
    );
    let (_, within) = execute(limits, vec![Step::Grow(16)]).await;
    assert_eq!(within, Ok(Ok(())));
}

#[tokio::test]
async fn bytes_that_are_not_a_program_do_not_load() {
    let runtime = Runtime::new(Limits::default());
    assert!(matches!(runtime.load(b"not wasm"), Err(Fault::Load(_))));
    let no_exports = wat::parse_str("(module)").unwrap();
    let code = runtime.load(&no_exports).unwrap();
    let mut bench = Bench::default();
    let verdict = runtime
        .run(
            &code,
            invocation(ProgramCall::Execute(vec![])),
            &mut bench,
            &mut None,
        )
        .await;
    assert!(matches!(verdict, Err(Fault::Load(_))), "{verdict:?}");
}

const RECURSES: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "alloc") (param i32) (result i32) i32.const 0)
  (func $deeper (param i32) (result i64) (call $deeper (local.get 0)))
  (func (export "call") (param i32 i32) (result i64) (call $deeper (local.get 0))))
"#;

#[tokio::test]
async fn a_run_on_a_reused_stack_overflows_as_one_on_a_fresh_stack_does() {
    let runtime = Runtime::new(Limits::default());
    let code = runtime.load(&wat::parse_str(RECURSES).unwrap()).unwrap();
    for _ in 0..2 {
        let verdict = runtime
            .run(
                &code,
                invocation(ProgramCall::Execute(Vec::new())),
                &mut Bench::default(),
                &mut None,
            )
            .await;
        let Err(Fault::Trap(sentence)) = verdict else {
            panic!("the recursing run did not trap");
        };
        assert_eq!(sentence, "the run trapped: call stack exhausted");
    }
}

#[test]
fn a_refused_load_says_the_kernels_words_never_wasmtimes() {
    let runtime = Runtime::new(Limits::default());
    let refused = Some(Fault::Load(
        "the code is not a wasm module this network runs".into(),
    ));
    let exceptions = wat::parse_str("(module (tag $e) (func (throw $e)))").unwrap();
    assert_eq!(runtime.load(&exceptions).err(), refused);
    assert_eq!(runtime.load(b"not wasm").err(), refused);
}

#[test]
fn a_code_answers_whether_it_exports_a_name() {
    let runtime = Runtime::new(Limits::default());
    let plain = runtime.load(&wat::parse_str(RECURSES).unwrap()).unwrap();
    assert!(plain.exports("call"));
    assert!(!plain.exports(abi::hook::BATCH_EXPORT));
    assert!(
        !runtime
            .load(PROBE)
            .unwrap()
            .exports(abi::hook::BATCH_EXPORT)
    );
    let batch = runtime.load(fixture_wasm::PROBE_BATCH).unwrap();
    assert!(batch.exports(abi::hook::BATCH_EXPORT));
}

const LEDGER_FUEL: u64 = 500_000_000;

const PROPOSALS: &[(&str, &str)] = &[
    ("mvp", r#"(func (result i32) i32.const 1)"#),
    (
        "mutable-global",
        r#"(global (export "g") (mut i32) (i32.const 0))"#,
    ),
    (
        "sign-extension",
        r#"(func (result i32) i32.const 1 i32.extend8_s)"#,
    ),
    (
        "saturating-float-to-int",
        r#"(func (result i32) f32.const 1 i32.trunc_sat_f32_s)"#,
    ),
    (
        "multi-value",
        r#"(func (result i32 i32) i32.const 1 i32.const 2)"#,
    ),
    (
        "bulk-memory",
        r#"(memory 1) (func (memory.fill (i32.const 0) (i32.const 0) (i32.const 0)))"#,
    ),
    (
        "reference-types",
        r#"(table 1 externref) (func (result externref) ref.null extern)"#,
    ),
    (
        "extended-const",
        r#"(global i32 (i32.add (i32.const 1) (i32.const 2)))"#,
    ),
    ("simd", r#"(func (result v128) v128.const i32x4 0 0 0 0)"#),
    (
        "relaxed-simd",
        r#"(func (param v128 v128 v128) (result v128) local.get 0 local.get 1 local.get 2 f32x4.relaxed_madd)"#,
    ),
    ("threads", r#"(memory 1 1 shared)"#),
    ("tail-call", r#"(func $f return_call $f)"#),
    ("multi-memory", r#"(memory 1) (memory 1)"#),
    ("memory64", r#"(memory i64 1)"#),
    (
        "function-references",
        r#"(type $t (func)) (func (param (ref $t)))"#,
    ),
    ("gc", r#"(type (struct (field i32)))"#),
    ("exceptions", r#"(tag $e) (func (throw $e))"#),
    (
        "wide-arithmetic",
        r#"(func (param i64 i64 i64 i64) (result i64 i64) local.get 0 local.get 1 local.get 2 local.get 3 i64.add128)"#,
    ),
    ("custom-page-sizes", r#"(memory 1 (pagesize 1))"#),
    ("stack-switching", r#"(type $f (func)) (type $c (cont $f))"#),
    (
        "shared-everything-threads",
        r#"(global (shared i32) (i32.const 0))"#,
    ),
];

const DESCENDS: &str = r#"
(module
  (memory (export "memory") 64)
  (func (export "alloc") (param i32) (result i32) i32.const 0)
  (func $down (param i32) (result i64)
    (if (result i64) (i32.eqz (local.get 0))
      (then (i64.const 0))
      (else (i64.add (i64.const 1) (call $down (i32.sub (local.get 0) (i32.const 1)))))))
  (func (export "call") (param i32 i32) (result i64)
    (drop (call $down (local.get 1)))
    (i64.const 0)))
"#;

fn spent(limit: u64, left: Option<u64>) -> u64 {
    limit - left.unwrap_or(limit)
}

async fn ledger_run(
    runtime: &Runtime,
    code: &runtime::Code,
    call: ProgramCall,
    limit: u64,
) -> (u64, String) {
    let mut bench = Bench::default();
    let mut left = Some(limit);
    let verdict = runtime
        .run(code, invocation(call), &mut bench, &mut left)
        .await;
    let burned = spent(limit, left);
    let crossings: Vec<u64> = bench.fuel.iter().map(|at| spent(limit, *at)).collect();
    (
        burned,
        format!("spent {burned} crossings {crossings:?} verdict {verdict:?}"),
    )
}

fn ledger_state() -> Vec<Step> {
    vec![
        Step::Op(HostOp::Set {
            key: b"k/1".to_vec(),
            value: b"one".to_vec(),
        }),
        Step::Op(HostOp::Set {
            key: b"k/2".to_vec(),
            value: vec![9; 2048],
        }),
        Step::Op(HostOp::Get(b"k/2".to_vec())),
        Step::Op(HostOp::Delete(b"k/1".to_vec())),
        Step::Op(HostOp::Get(b"k/1".to_vec())),
    ]
}

fn ledger_query() -> Step {
    Step::Op(HostOp::Query {
        program: "oracle".into(),
        request: b"a request".to_vec(),
    })
}

fn ledger_crypto() -> Step {
    Step::Op(HostOp::Crypto(CryptoOp::Sha256(vec![7; 4096])))
}

fn ledger_scripts() -> Vec<(&'static str, Vec<Step>)> {
    let mut mixed = vec![Step::Env];
    mixed.extend(ledger_state());
    mixed.extend([ledger_query(), ledger_crypto(), Step::Grow(8)]);
    mixed.push(Step::Fail("the ledger's refusal".into()));
    vec![
        ("env", vec![Step::Env]),
        ("state", ledger_state()),
        ("query", vec![ledger_query()]),
        ("crypto", vec![ledger_crypto()]),
        ("grow", vec![Step::Grow(16)]),
        ("fail", vec![Step::Fail("no".into())]),
        ("mixed", mixed),
    ]
}

async fn deepest_descent(runtime: &Runtime, code: &runtime::Code) -> usize {
    let overflows = |verdict: &Result<abi::ProgramReply, Fault>| matches!(verdict, Err(Fault::Trap(sentence)) if sentence.contains("call stack exhausted"));
    let (mut low, mut high) = (0usize, 4_000_000usize);
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        let verdict = runtime
            .run(
                code,
                invocation(ProgramCall::Execute(vec![0; middle])),
                &mut Bench::default(),
                &mut Some(LEDGER_FUEL),
            )
            .await;
        match overflows(&verdict) {
            true => high = middle,
            false => low = middle,
        }
    }
    low
}

async fn ledger() -> String {
    let limits = Limits {
        fuel: Some(LEDGER_FUEL),
        memory_bytes: None,
    };
    let runtime = Runtime::new(limits);
    let mut lines = Vec::new();
    let probe = runtime.load(PROBE).unwrap();
    for (name, steps) in ledger_scripts() {
        let bytes = script(steps);
        let call = || ProgramCall::Execute(bytes.clone());
        let (burned, line) = ledger_run(&runtime, &probe, call(), LEDGER_FUEL).await;
        lines.push(format!("probe {name}: {line}"));
        let (mut short, mut enough) = (0u64, burned);
        while enough - short > 1 {
            let middle = short + (enough - short) / 2;
            let (_, line) = ledger_run(&runtime, &probe, call(), middle).await;
            match line.ends_with("Err(Trap(\"the run spent its fuel\"))") {
                true => short = middle,
                false => enough = middle,
            }
        }
        for limit in [enough, short] {
            let (_, line) = ledger_run(&runtime, &probe, call(), limit).await;
            lines.push(format!("probe {name} on {limit}: {line}"));
        }
    }
    let spin = ProgramCall::Execute(script(vec![Step::Spin]));
    let (_, line) = ledger_run(&runtime, &probe, spin, 1_000_000).await;
    lines.push(format!("probe spin on 1000000: {line}"));
    for (name, wasm) in fixture_wasm::ALL {
        let code = runtime.load(wasm).unwrap();
        let calls = [
            ("init", ProgramCall::Init(Vec::new())),
            ("execute", ProgramCall::Execute(Vec::new())),
            ("query", ProgramCall::Query(Vec::new())),
        ];
        for (kind, call) in calls {
            let (_, line) = ledger_run(&runtime, &code, call, LEDGER_FUEL).await;
            lines.push(format!("{name} {kind}: {line}"));
        }
    }
    let mut modules = Vec::new();
    let mut unwritable = Vec::new();
    for (name, fields) in PROPOSALS {
        match wat::parse_str(format!("(module {fields})")) {
            Ok(bytes) => modules.push((*name, bytes)),
            Err(_) => unwritable.push(*name),
        }
    }
    assert!(unwritable.is_empty(), "wat writes none of {unwritable:?}");
    let mut every_proposal = wasmtime::Config::new();
    every_proposal
        .wasm_reference_types(true)
        .wasm_bulk_memory(true)
        .wasm_multi_value(true)
        .wasm_extended_const(true)
        .wasm_simd(true)
        .wasm_relaxed_simd(true)
        .wasm_threads(true)
        .wasm_tail_call(true)
        .wasm_multi_memory(true)
        .wasm_memory64(true)
        .wasm_function_references(true)
        .wasm_gc(true)
        .wasm_exceptions(true)
        .wasm_wide_arithmetic(true)
        .wasm_custom_page_sizes(true)
        .wasm_stack_switching(true)
        .wasm_shared_everything_threads(true);
    let every_proposal =
        wasmtime::Engine::new(&every_proposal).expect("an engine with every proposal on");
    let invalid: Vec<&str> = modules
        .iter()
        .filter(|(_, bytes)| wasmtime::Module::new(&every_proposal, bytes).is_err())
        .map(|(name, _)| *name)
        .collect();
    assert!(
        invalid.is_empty(),
        "not valid even with every proposal on: {invalid:?}"
    );
    for (name, bytes) in modules {
        let admitted = match runtime.load(&bytes) {
            Ok(_) => "loads",
            Err(_) => "refused",
        };
        lines.push(format!("proposal {name}: {admitted}"));
    }
    let descends = runtime.load(&wat::parse_str(DESCENDS).unwrap()).unwrap();
    let deepest = deepest_descent(&runtime, &descends).await;
    lines.push(format!(
        "descent: the longest input that does not exhaust the stack is {deepest} bytes"
    ));
    lines.join("\n") + "\n"
}

#[tokio::test]
async fn fuel_crossings_verdicts_and_stack_depth_match_the_golden_ledger() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ledger.golden");
    let ledger = ledger().await;
    if std::env::var_os("LEDGER_GOLDEN_WRITE").is_some() {
        std::fs::write(path, &ledger).unwrap();
    }
    let golden = std::fs::read_to_string(path).unwrap();
    assert_eq!(ledger, golden);
}
