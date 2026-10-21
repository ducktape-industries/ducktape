//! The ownership guard: once a run that claimed a new key, or changed a key the ownership program
//! holds a record for, returns, and before anything it emitted is delivered, the kernel makes one
//! call to the ownership role with who acted and how each such key stood; its refusal undoes the run.
use std::path::Path;

use abi::role::ownership::{Guard, Write, record_key};
use abi::role::{self, halo, validators};
use abi::{HostOp, Message, Origin, Outcome, Principal, Receipt, Refusal, reason};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use fixture_awaiter::Plan;
use fixture_ownership::{CALLS, LAST, Op, denied};
use fixture_probe::Step;
use fixture_wasm::{
    ACCESS, AWAITER, AWAITS, BLOBS as BLOBS_PROGRAM, HALO, IDENTITY, OWNERSHIP, PROBE, PROGRAMS,
    VALSET,
};
use host::{Block, BlockId, Founding, Genesis, Host, Layer, Limits, Submission, Tip};

const SIGNER: &[u8] = b"signer";
const ACCOUNT: u64 = 1;
const TIME: u64 = 1_700_000_000;
const ITEM: &str = "probe.item";

type Ctx = deterministic::Context;

fn founding(program: &str, code: &[u8], params: Vec<u8>, roles: &[&str]) -> Founding {
    Founding {
        program: program.to_owned(),
        code: code.to_vec(),
        params,
        roles: roles.iter().map(|role| (*role).to_owned()).collect(),
    }
}

fn script(steps: Vec<Step>) -> Vec<u8> {
    abi::encode(&steps)
}

fn set(key: &[u8]) -> Step {
    Step::Op(HostOp::Set {
        key: key.to_vec(),
        value: key.to_vec(),
    })
}

fn claim(key: &[u8]) -> Step {
    Step::Op(HostOp::SetOwned {
        key: key.to_vec(),
        value: key.to_vec(),
        kind: ITEM.to_owned(),
    })
}

fn delete(key: &[u8]) -> Step {
    Step::Op(HostOp::Delete(key.to_vec()))
}

fn emit(target: &str, payload: Vec<u8>) -> Step {
    Step::Op(HostOp::Emit(Message {
        target: target.to_owned(),
        payload,
        reply: false,
        acting: None,
    }))
}

fn genesis(init: Vec<u8>) -> Genesis {
    Genesis {
        network: b"net".to_vec(),
        validators: vec![validators::Member {
            key: b"v1".to_vec(),
            address: Some("v1:1".into()),
            halo: "v1:1".into(),
        }],
        programs: vec![
            founding("programs", PROGRAMS, Vec::new(), &[role::PROGRAMS]),
            founding("valset", VALSET, Vec::new(), &[role::VALIDATORS]),
            founding(
                "identity",
                IDENTITY,
                abi::encode(&vec![(SIGNER.to_vec(), ACCOUNT)]),
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
            founding("access", ACCESS, Vec::new(), &[role::ACCESS]),
            founding("ownership", OWNERSHIP, Vec::new(), &[role::OWNERSHIP]),
            founding("probe", PROBE, init, &[]),
            founding("awaiter", AWAITER, Vec::new(), &[]),
        ],
        views: Vec::new(),
        hooks: Vec::new(),
        limits: Limits::default(),
        epoch_length: 64,
        time: TIME,
        member_cap: 16,
        queries: abi::role::validators::QueryPolicy::Signed,
    }
}

fn network(guard: Option<&[u8]>) -> Genesis {
    let mut genesis = genesis(script(Vec::new()));
    genesis
        .programs
        .retain(|founding| founding.program != "ownership");
    if let Some(code) = guard {
        let guard = founding("ownership", code, Vec::new(), &[role::OWNERSHIP]);
        genesis.programs.push(guard);
    }
    genesis
}

async fn found(context: Ctx, dir: &Path, init: Vec<Step>) -> Host<Ctx> {
    Host::found(context, "net", dir, block_id(0), genesis(script(init)))
        .await
        .unwrap()
        .0
}

fn block_id(height: u64) -> BlockId {
    let mut id = [0u8; 32];
    id[..8].copy_from_slice(&height.to_be_bytes());
    id
}

fn signed(salt: u64, target: &str, payload: Vec<u8>) -> Submission {
    Submission {
        signer: SIGNER.to_vec(),
        anchor: Tip {
            height: 0,
            id: block_id(0),
        },
        salt: Some(salt),
        target: target.to_owned(),
        payload,
        client: None,
    }
}

fn probe(salt: u64, steps: Vec<Step>) -> Submission {
    signed(salt, "probe", script(steps))
}

fn ownership(salt: u64, op: &Op) -> Submission {
    signed(salt, "ownership", abi::encode(op))
}

async fn apply(host: &mut Host<Ctx>, height: u64, submissions: Vec<Submission>) -> Vec<Receipt> {
    let block = Block {
        height,
        id: block_id(height),
        time: TIME + height,
        submissions,
    };
    host.apply(block).await.unwrap().submissions
}

fn applied(receipt: &Receipt) {
    assert!(
        matches!(receipt.outcome, Outcome::Applied { .. }),
        "{receipt:?}"
    );
}

fn refusal(receipt: &Receipt) -> &Refusal {
    match &receipt.outcome {
        Outcome::Rejected(refusal) => refusal,
        Outcome::Applied { .. } => panic!("applied: {receipt:?}"),
    }
}

fn stored(host: &Host<Ctx>, program: &str, key: &[u8]) -> Option<Vec<u8>> {
    host.view(Layer::Confirmed).get(program, key).unwrap()
}

fn recorded(host: &Host<Ctx>, program: &str, key: &[u8]) -> bool {
    stored(host, "ownership", &record_key(program, key)).is_some()
}

fn calls(host: &Host<Ctx>) -> Option<u64> {
    stored(host, "ownership", CALLS).map(|bytes| abi::decode(&bytes).unwrap())
}

fn last(host: &Host<Ctx>) -> Guard {
    abi::decode(&stored(host, "ownership", LAST).expect("a guard ran")).unwrap()
}

fn write(key: &[u8], claimed: Option<&str>, existed: bool, kept: bool) -> Write {
    Write {
        key: key.to_vec(),
        claimed: claimed.map(str::to_owned),
        existed,
        kept,
    }
}

#[test]
fn a_claimed_create_asks_once_and_its_record_guards_every_later_write() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path(), Vec::new()).await;
        let steps = vec![
            claim(b"o/a"),
            set(b"s/shared"),
            claim(b"o/a"),
            delete(b"o/never"),
            claim(b"o/brief"),
            delete(b"o/brief"),
        ];
        applied(&apply(&mut host, 1, vec![probe(1, steps)]).await[0]);
        assert_eq!(calls(&host), Some(1));
        assert_eq!(
            last(&host),
            Guard {
                program: "probe".into(),
                origin: Origin::External(SIGNER.to_vec()),
                sender: Some(Principal::Account(ACCOUNT)),
                writes: vec![write(b"o/a", Some(ITEM), false, true)],
            }
        );
        assert!(recorded(&host, "probe", b"o/a"));
        assert!(!recorded(&host, "probe", b"o/brief"));

        let steps = vec![set(b"o/a"), set(b"s/shared")];
        applied(&apply(&mut host, 2, vec![probe(2, steps)]).await[0]);
        assert_eq!(calls(&host), Some(2));
        assert_eq!(last(&host).writes, vec![write(b"o/a", None, true, true)]);

        applied(&apply(&mut host, 3, vec![probe(3, vec![delete(b"o/a")])]).await[0]);
        assert_eq!(calls(&host), Some(3));
        assert_eq!(last(&host).writes, vec![write(b"o/a", None, true, false)]);
        assert!(!recorded(&host, "probe", b"o/a"));

        applied(&apply(&mut host, 4, vec![probe(4, vec![set(b"o/a")])]).await[0]);
        assert_eq!(calls(&host), Some(3), "a key with no record is shared");
    });
}

#[test]
fn a_claim_on_a_key_that_holds_a_value_is_an_ordinary_write() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path(), Vec::new()).await;
        applied(&apply(&mut host, 1, vec![probe(1, vec![set(b"s/k")])]).await[0]);
        applied(&apply(&mut host, 2, vec![probe(2, vec![claim(b"s/k")])]).await[0]);
        assert_eq!(calls(&host), None);
        assert!(!recorded(&host, "probe", b"s/k"));
    });
}

#[test]
fn with_no_ownership_role_bound_nothing_is_guarded() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let unbound = network(None);
        let founded = Host::found(context, "net", dir.path(), block_id(0), unbound).await;
        let (mut host, _) = founded.unwrap();
        assert_eq!(host.roles().get(role::OWNERSHIP), None);
        assert!(!host.programs().unwrap().contains_key("ownership"));

        let steps = vec![claim(b"o/a"), set(b"written"), delete(b"s/none")];
        applied(&apply(&mut host, 1, vec![probe(1, steps)]).await[0]);
        assert!(stored(&host, "probe", b"o/a").is_some());
        assert!(stored(&host, "probe", b"written").is_some());
        let again = vec![delete(b"o/a"), claim(b"written")];
        applied(&apply(&mut host, 2, vec![probe(2, again)]).await[0]);
        assert_eq!(stored(&host, "probe", b"o/a"), None);
    });
}

#[test]
fn the_kernels_runs_and_shared_writes_make_no_call() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path(), vec![claim(b"o/init")]).await;
        assert!(stored(&host, "probe", b"o/init").is_some());
        assert!(!recorded(&host, "probe", b"o/init"));
        assert_eq!(calls(&host), None, "a founding run is the kernel's");

        let shared = probe(1, vec![set(b"s/one"), set(b"s/two"), delete(b"o/init")]);
        applied(&apply(&mut host, 1, vec![shared]).await[0]);
        assert!(stored(&host, "probe", b"s/two").is_some());
        assert_eq!(calls(&host), None);
    });
}

#[test]
fn a_refusal_undoes_the_run_and_nothing_it_emitted_runs() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path(), Vec::new()).await;
        let deny = Op::Deny {
            program: "probe".into(),
            key: b"o/denied".to_vec(),
        };
        applied(&apply(&mut host, 1, vec![ownership(1, &deny)]).await[0]);
        assert!(stored(&host, "ownership", &denied("probe", b"o/denied")).is_some());
        assert_eq!(calls(&host), None, "the ownership role's own writes");

        let steps = vec![
            claim(b"o/c"),
            emit("probe", script(vec![claim(b"s/sent")])),
            claim(b"o/denied"),
        ];
        let refused = apply(&mut host, 2, vec![probe(2, steps)]).await;
        assert_eq!(refusal(&refused[0]).reason, reason::UNAUTHORIZED);
        assert!(refused[0].nested.is_empty(), "{:?}", refused[0]);
        assert_eq!(stored(&host, "probe", b"o/c"), None);
        assert_eq!(stored(&host, "probe", b"s/sent"), None);
        assert!(!recorded(&host, "probe", b"o/c"));
        assert_eq!(calls(&host), None);
    });
}

#[test]
fn a_guard_that_sends_a_message_is_refused() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path(), Vec::new()).await;
        applied(&apply(&mut host, 1, vec![ownership(1, &Op::Chatty)]).await[0]);

        let refused = apply(&mut host, 2, vec![probe(2, vec![claim(b"o/a")])]).await;
        assert_eq!(
            *refusal(&refused[0]),
            Refusal::new(
                reason::UNSUPPORTED,
                "ownership sends no message while it guards"
            )
        );
        assert_eq!(stored(&host, "probe", b"o/a"), None);
    });
}

#[test]
fn a_message_run_is_guarded_as_the_program_that_sent_it() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path(), Vec::new()).await;
        let relayed = probe(1, vec![emit("probe", script(vec![claim(b"o/m")]))]);
        applied(&apply(&mut host, 1, vec![relayed]).await[0]);
        assert_eq!(calls(&host), Some(1), "the sending run claimed nothing");
        let guard = last(&host);
        assert_eq!(guard.origin, Origin::Program("probe".into()));
        assert!(
            matches!(guard.sender, Some(Principal::Account(number)) if number != ACCOUNT),
            "{guard:?}"
        );
        assert_eq!(guard.writes, vec![write(b"o/m", Some(ITEM), false, true)]);
    });
}

#[test]
fn a_frame_that_awaits_is_guarded_after_it_resumes() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path(), Vec::new()).await;
        let record = Op::Record {
            program: "awaiter".into(),
            key: fixture_awaiter::ANSWERS.to_vec(),
        };
        applied(&apply(&mut host, 1, vec![ownership(1, &record)]).await[0]);
        assert!(recorded(&host, "awaiter", fixture_awaiter::ANSWERS));
        let plan = Plan {
            target: "probe".into(),
            acting: None,
            asks: vec![script(Vec::new())],
            together: false,
            kept: 0,
            fail: false,
        };
        applied(&apply(&mut host, 2, vec![signed(2, "awaiter", abi::encode(&plan))]).await[0]);
        assert_eq!(calls(&host), Some(1));
        let guard = last(&host);
        assert_eq!(guard.program, "awaiter");
        assert_eq!(
            guard.writes,
            vec![write(fixture_awaiter::ANSWERS, None, false, true)]
        );

        let deny = Op::Deny {
            program: "awaiter".into(),
            key: fixture_awaiter::ANSWERS.to_vec(),
        };
        applied(&apply(&mut host, 3, vec![ownership(3, &deny)]).await[0]);
        let refused = apply(&mut host, 4, vec![signed(4, "awaiter", abi::encode(&plan))]).await;
        assert_eq!(refusal(&refused[0]).reason, reason::UNAUTHORIZED);
    });
}

#[test]
#[ignore = "a measurement: prints the guard's cost per frame; OWNERSHIP_WASM names a real guard"]
fn the_guards_cost_per_frame() {
    deterministic::Runner::default().start(|context| async move {
        let real = std::env::var_os("OWNERSHIP_WASM").map(|path| std::fs::read(path).unwrap());
        let mut guards = vec![
            ("no role bound", None),
            ("the fixture", Some(OWNERSHIP.to_vec())),
        ];
        guards.extend(real.map(|code| ("OWNERSHIP_WASM", Some(code))));
        let mut attempt = 0;
        for (guard, code) in &guards {
            eprintln!("guard: {guard}");
            let mut base = None;
            for shape in shapes() {
                let fuel = least_fuel(&context, &mut attempt, code.as_deref(), &shape).await;
                let over = fuel - *base.get_or_insert(fuel);
                attempt += 1;
                let per_frame = wall(&context, attempt, code.as_deref(), &shape).await;
                let name = shape.name;
                eprintln!("{name:>36}: {fuel:>9} fuel (+{over:>7}), {per_frame:?} a frame");
            }
        }
    });
}

const MOST_FUEL: u64 = 1 << 30;
const FRAMES: u64 = 1_000;

struct Shape {
    name: &'static str,
    setup: fn(u64) -> Vec<Step>,
    frame: fn(u64) -> Vec<Step>,
}

fn key(table: &str, salt: u64, i: usize) -> Vec<u8> {
    format!("{table}{salt}/{i}").into_bytes()
}

fn shared(salt: u64, n: usize) -> Vec<Step> {
    (0..n).map(|i| set(&key("s/", salt, i))).collect()
}

fn claims(salt: u64, n: usize) -> Vec<Step> {
    (0..n).map(|i| claim(&key("o/", salt, i))).collect()
}

fn earlier_claim(salt: u64) -> Vec<Step> {
    vec![claim(&key("r/", salt, 0))]
}

fn shapes() -> Vec<Shape> {
    let none: fn(u64) -> Vec<Step> = |_| Vec::new();
    vec![
        Shape {
            name: "no write",
            setup: none,
            frame: |_| Vec::new(),
        },
        Shape {
            name: "1 shared write",
            setup: none,
            frame: |salt| shared(salt, 1),
        },
        Shape {
            name: "10 shared writes",
            setup: none,
            frame: |salt| shared(salt, 10),
        },
        Shape {
            name: "100 shared writes",
            setup: none,
            frame: |salt| shared(salt, 100),
        },
        Shape {
            name: "1 claim, the owner's first",
            setup: none,
            frame: |salt| claims(salt, 1),
        },
        Shape {
            name: "1 claim, the owner's second",
            setup: earlier_claim,
            frame: |salt| claims(salt, 1),
        },
        Shape {
            name: "2 claims",
            setup: earlier_claim,
            frame: |salt| claims(salt, 2),
        },
        Shape {
            name: "1 write to its owner's recorded key",
            setup: earlier_claim,
            frame: |salt| vec![set(&key("r/", salt, 0))],
        },
        Shape {
            name: "1 claim beside 1 recorded write",
            setup: earlier_claim,
            frame: |salt| {
                let recorded = set(&key("r/", salt, 0));
                claims(salt, 1).into_iter().chain([recorded]).collect()
            },
        },
    ]
}

fn ballasted(shape: &Shape, salt: u64) -> Vec<u8> {
    let reads = (0..256).map(|_| Step::Op(HostOp::Get(b"ballast".to_vec())));
    script(reads.chain((shape.frame)(salt)).collect())
}

#[derive(Debug, PartialEq, Eq)]
enum Fit {
    Founding,
    Setup,
    Frame,
    Applied,
}

async fn least_fuel(context: &Ctx, attempt: &mut u64, guard: Option<&[u8]>, shape: &Shape) -> u64 {
    let (mut spent, mut kept) = (0, MOST_FUEL);
    *attempt += 1;
    assert_eq!(
        fits(context, *attempt, guard, kept, shape).await,
        Fit::Applied
    );
    while kept - spent > 1 {
        let mid = spent + (kept - spent) / 2;
        *attempt += 1;
        match fits(context, *attempt, guard, mid, shape).await {
            Fit::Applied => kept = mid,
            Fit::Founding | Fit::Setup | Fit::Frame => spent = mid,
        }
    }
    *attempt += 1;
    let short = fits(context, *attempt, guard, spent, shape).await;
    assert_eq!(
        short,
        Fit::Frame,
        "{}: the frame sets the budget",
        shape.name
    );
    kept
}

fn ran(receipts: &[Receipt]) -> bool {
    let refused = |receipt: &Receipt| matches!(receipt.outcome, Outcome::Rejected(_));
    !receipts.iter().any(refused)
}

fn setups(shape: &Shape, salts: std::ops::Range<u64>) -> Vec<Submission> {
    salts
        .map(|salt| (salt, (shape.setup)(salt)))
        .filter(|(_, steps)| !steps.is_empty())
        .map(|(salt, steps)| probe(salt, steps))
        .collect()
}

async fn fits(context: &Ctx, attempt: u64, guard: Option<&[u8]>, fuel: u64, shape: &Shape) -> Fit {
    let dir = tempfile::tempdir().unwrap();
    let mut metered = network(guard);
    metered.limits = Limits {
        fuel: Some(fuel),
        memory_bytes: None,
    };
    let name = format!("cost{attempt}");
    let founded = Host::found(
        context.child("cost"),
        &name,
        dir.path(),
        block_id(0),
        metered,
    )
    .await;
    let Ok((mut host, _)) = founded else {
        return Fit::Founding;
    };
    let set_up = apply(&mut host, 1, setups(shape, 0..1)).await;
    if !ran(&set_up) {
        return Fit::Setup;
    }
    let frame = signed(1, "probe", ballasted(shape, 0));
    match ran(&apply(&mut host, 2, vec![frame]).await) {
        true => Fit::Applied,
        false => Fit::Frame,
    }
}

async fn wall(
    context: &Ctx,
    attempt: u64,
    guard: Option<&[u8]>,
    shape: &Shape,
) -> std::time::Duration {
    let dir = tempfile::tempdir().unwrap();
    let name = format!("wall{attempt}");
    let (mut host, _) = Host::found(
        context.child("wall"),
        &name,
        dir.path(),
        block_id(0),
        network(guard),
    )
    .await
    .unwrap();
    assert!(ran(&apply(&mut host, 1, setups(shape, 0..2 * FRAMES)).await));
    let frames = |salts: std::ops::Range<u64>| -> Vec<Submission> {
        salts
            .map(|salt| signed(2 * FRAMES + salt, "probe", ballasted(shape, salt)))
            .collect()
    };
    assert!(ran(&apply(&mut host, 2, frames(0..FRAMES)).await));
    let measured = frames(FRAMES..2 * FRAMES);
    let started = std::time::Instant::now();
    let receipts = apply(&mut host, 3, measured).await;
    let took = started.elapsed();
    assert!(ran(&receipts));
    took / FRAMES as u32
}
