//! The ownership guard: once a run that wrote owned keys returns, and before anything it emitted
//! is delivered, the kernel makes one call to the ownership role with who acted and how each owned
//! key stood before and after the run; the role's refusal undoes the run.
use std::path::Path;

use abi::role::ownership::{Guard, Write};
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
    }
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
    }
}

fn probe(salt: u64, steps: Vec<Step>) -> Submission {
    signed(salt, "probe", script(steps))
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

fn calls(host: &Host<Ctx>) -> Option<u64> {
    stored(host, "ownership", CALLS).map(|bytes| abi::decode(&bytes).unwrap())
}

fn last(host: &Host<Ctx>) -> Guard {
    abi::decode(&stored(host, "ownership", LAST).expect("a guard ran")).unwrap()
}

fn write(key: &[u8], kind: Option<&str>, existed: bool, kept: bool) -> Write {
    Write {
        key: key.to_vec(),
        kind: kind.map(str::to_owned),
        existed,
        kept,
    }
}

#[test]
fn a_run_that_writes_owned_keys_asks_once_with_each_key_as_it_stood() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path(), Vec::new()).await;
        let steps = vec![
            set(b"o/a"),
            set(b"written"),
            set(b"s/shared"),
            set(b"o/a"),
            delete(b"o/never"),
            set(b"o/brief"),
            delete(b"o/brief"),
        ];
        applied(&apply(&mut host, 1, vec![probe(1, steps)]).await[0]);
        assert_eq!(calls(&host), Some(1));
        let item = Some("probe.item");
        assert_eq!(
            last(&host),
            Guard {
                program: "probe".into(),
                origin: Origin::External(SIGNER.to_vec()),
                sender: Some(Principal::Account(ACCOUNT)),
                writes: vec![
                    write(b"o/a", item, false, true),
                    write(b"written", None, false, true),
                ],
            }
        );

        let steps = vec![delete(b"o/a"), set(b"written")];
        applied(&apply(&mut host, 2, vec![probe(2, steps)]).await[0]);
        assert_eq!(calls(&host), Some(2));
        assert_eq!(
            last(&host).writes,
            vec![
                write(b"o/a", item, true, false),
                write(b"written", None, true, true),
            ]
        );
    });
}

#[test]
fn with_no_ownership_role_bound_nothing_is_guarded() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut unbound = genesis(script(Vec::new()));
        unbound
            .programs
            .retain(|founding| founding.program != "ownership");
        let founded = Host::found(context, "net", dir.path(), block_id(0), unbound).await;
        let (mut host, _) = founded.unwrap();
        assert_eq!(host.roles().get(role::OWNERSHIP), None);
        assert!(!host.programs().unwrap().contains_key("ownership"));

        let steps = vec![set(b"o/a"), set(b"written"), delete(b"s/none")];
        applied(&apply(&mut host, 1, vec![probe(1, steps)]).await[0]);
        assert!(stored(&host, "probe", b"o/a").is_some());
        assert!(stored(&host, "probe", b"written").is_some());
        let again = vec![delete(b"o/a"), set(b"written")];
        applied(&apply(&mut host, 2, vec![probe(2, again)]).await[0]);
        assert_eq!(stored(&host, "probe", b"o/a"), None);
    });
}

#[test]
fn the_kernels_runs_and_shared_writes_make_no_call() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path(), vec![set(b"o/init")]).await;
        assert!(stored(&host, "probe", b"o/init").is_some());
        assert_eq!(calls(&host), None, "a founding run is the kernel's");

        let shared = probe(1, vec![set(b"s/one"), set(b"s/two")]);
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
            key: b"written".to_vec(),
        };
        applied(
            &apply(
                &mut host,
                1,
                vec![signed(1, "ownership", abi::encode(&deny))],
            )
            .await[0],
        );
        assert!(stored(&host, "ownership", &denied("probe", b"written")).is_some());
        assert_eq!(calls(&host), None, "the ownership role's own writes");

        let steps = vec![
            set(b"o/c"),
            emit("probe", script(vec![set(b"s/sent")])),
            set(b"written"),
        ];
        let refused = apply(&mut host, 2, vec![probe(2, steps)]).await;
        assert_eq!(refusal(&refused[0]).reason, reason::UNAUTHORIZED);
        assert!(refused[0].nested.is_empty(), "{:?}", refused[0]);
        assert_eq!(stored(&host, "probe", b"o/c"), None);
        assert_eq!(stored(&host, "probe", b"s/sent"), None);
        assert_eq!(calls(&host), None);
    });
}

#[test]
fn a_guard_that_sends_a_message_is_refused() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path(), Vec::new()).await;
        let chatty = signed(1, "ownership", abi::encode(&Op::Chatty));
        applied(&apply(&mut host, 1, vec![chatty]).await[0]);

        let refused = apply(&mut host, 2, vec![probe(2, vec![set(b"o/a")])]).await;
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
        let relayed = probe(1, vec![emit("probe", script(vec![set(b"o/m")]))]);
        applied(&apply(&mut host, 1, vec![relayed]).await[0]);
        assert_eq!(calls(&host), Some(1), "the sending run wrote nothing owned");
        let guard = last(&host);
        assert_eq!(guard.origin, Origin::Program("probe".into()));
        assert!(
            matches!(guard.sender, Some(Principal::Account(number)) if number != ACCOUNT),
            "{guard:?}"
        );
        assert_eq!(
            guard.writes,
            vec![write(b"o/m", Some("probe.item"), false, true)]
        );
    });
}

#[test]
fn a_frame_that_awaits_is_guarded_after_it_resumes() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path(), Vec::new()).await;
        let plan = Plan {
            target: "probe".into(),
            acting: None,
            asks: vec![script(Vec::new())],
            together: false,
            kept: 0,
            fail: false,
        };
        applied(&apply(&mut host, 1, vec![signed(1, "awaiter", abi::encode(&plan))]).await[0]);
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
        applied(
            &apply(
                &mut host,
                2,
                vec![signed(2, "ownership", abi::encode(&deny))],
            )
            .await[0],
        );
        let refused = apply(&mut host, 3, vec![signed(3, "awaiter", abi::encode(&plan))]).await;
        assert_eq!(refusal(&refused[0]).reason, reason::UNAUTHORIZED);
    });
}

/// What the guard costs a frame, and what a frame costs without it. Fuel is
/// exact: the least network budget a frame still applies on, found by
/// bisection. Every shape runs the same ballast of reads first, so the frame,
/// not the founding, is what needs the budget (asserted), and each shape's
/// fuel over `no write` is its writes' own cost. Wall time is indicative: the
/// mean over a block of frames on a warm host. Run in release with
/// `--ignored --nocapture`.
#[test]
#[ignore = "a measurement: prints the guard's cost per frame"]
fn the_guards_cost_per_frame() {
    deterministic::Runner::default().start(|context| async move {
        let keys = |table: &str, n: usize| -> Vec<Step> {
            (0..n)
                .map(|i| set(format!("{table}{i}").as_bytes()))
                .collect()
        };
        let shapes = [
            ("no write", Vec::new()),
            ("1 shared write", keys("s/", 1)),
            ("1 owned write", keys("o/", 1)),
            ("10 shared writes", keys("s/", 10)),
            ("10 owned writes", keys("o/", 10)),
        ];
        let mut attempt = 0;
        let founding = least_fuel(&context, &mut attempt, None).await;
        eprintln!("{:>18}: {founding:>9} fuel", "the founding");
        let mut base = None;
        for (shape, steps) in shapes {
            let payload = script(ballast().into_iter().chain(steps).collect());
            let fuel = least_fuel(&context, &mut attempt, Some(&payload)).await;
            assert!(fuel > founding, "{shape}: the founding sets the budget");
            let over = fuel - *base.get_or_insert(fuel);
            attempt += 1;
            let per_frame = wall(&context, attempt, &payload).await;
            eprintln!("{shape:>18}: {fuel:>9} fuel (+{over:>7}), {per_frame:?} a frame");
        }
    });
}

const MOST_FUEL: u64 = 1 << 30;
const FRAMES: u64 = 1_000;

// Reads the probe makes before every shape's writes: enough that the frame
// needs more fuel than the founding's runs do.
fn ballast() -> Vec<Step> {
    (0..256)
        .map(|_| Step::Op(HostOp::Get(b"ballast".to_vec())))
        .collect()
}

async fn least_fuel(context: &Ctx, attempt: &mut u64, payload: Option<&[u8]>) -> u64 {
    let (mut spent, mut kept) = (0, MOST_FUEL);
    *attempt += 1;
    assert!(fits(context, *attempt, kept, payload).await);
    while kept - spent > 1 {
        let mid = spent + (kept - spent) / 2;
        *attempt += 1;
        match fits(context, *attempt, mid, payload).await {
            true => kept = mid,
            false => spent = mid,
        }
    }
    kept
}

async fn fits(context: &Ctx, attempt: u64, fuel: u64, payload: Option<&[u8]>) -> bool {
    let dir = tempfile::tempdir().unwrap();
    let mut metered = genesis(script(Vec::new()));
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
        return false;
    };
    let Some(payload) = payload else {
        return true;
    };
    let receipts = apply(&mut host, 1, vec![signed(1, "probe", payload.to_vec())]).await;
    matches!(receipts[0].outcome, Outcome::Applied { .. })
}

async fn wall(context: &Ctx, attempt: u64, payload: &[u8]) -> std::time::Duration {
    let dir = tempfile::tempdir().unwrap();
    let name = format!("wall{attempt}");
    let unmetered = genesis(script(Vec::new()));
    let (mut host, _) = Host::found(
        context.child("wall"),
        &name,
        dir.path(),
        block_id(0),
        unmetered,
    )
    .await
    .unwrap();
    let warm = (0..FRAMES)
        .map(|salt| signed(salt, "probe", payload.to_vec()))
        .collect();
    apply(&mut host, 1, warm).await;
    let frames = (FRAMES..2 * FRAMES)
        .map(|salt| signed(salt, "probe", payload.to_vec()))
        .collect();
    let started = std::time::Instant::now();
    let receipts = apply(&mut host, 2, frames).await;
    let took = started.elapsed();
    for receipt in &receipts {
        applied(receipt);
    }
    took / FRAMES as u32
}
