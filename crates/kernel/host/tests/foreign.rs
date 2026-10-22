//! A foreign user's message: only the program bound to the crosspond role sends one, its target
//! runs with the foreign principal as sender, and the access check, the ownership guard and a
//! resumed run all see that principal.
use std::path::Path;

use abi::role::access::{Need, Ownership, Party, Resource};
use abi::role::ownership::{Guard, record_key};
use abi::role::{self, halo, validators};
use abi::{
    Cause, Env, Foreign, HostOp, ItemRef, Message, Origin, Outcome, Principal, Receipt, Refusal,
    reason,
};
use commonware_runtime::{Runner as _, deterministic};
use fixture_awaiter::{ANSWERS, Plan};
use fixture_crosspond_probe::{Sent, done};
use fixture_oracle::Ask;
use fixture_probe::{NEEDS, Reply, Step, resource_key};
use fixture_wasm::{
    ACCESS, AWAITER, AWAITS, BLOBS as BLOBS_PROGRAM, CROSSPOND_PROBE, HALO, IDENTITY, ORACLE,
    OWNERSHIP, PROBE, PROGRAMS, VALSET,
};
use host::{Block, BlockId, Founding, Genesis, Host, Layer, Limits, Submission, Tip};

const SIGNER: &[u8] = b"signer";
const ACCOUNT: u64 = 1;
const TIME: u64 = 1_700_000_000;
const ITEM: &str = "probe.item";
const ALICE: Foreign = Foreign {
    network: [0xa; 32],
    account: 18,
};
const BOB: Foreign = Foreign {
    network: [0xa; 32],
    account: 19,
};

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

fn set(key: &[u8], value: Vec<u8>) -> Step {
    Step::Op(HostOp::Set {
        key: key.to_vec(),
        value,
    })
}

fn listed_for_alice() -> Vec<u8> {
    let resource = Resource {
        owner: Some(Party::Account(ACCOUNT)),
        default: Some(Ownership::Listed(vec![Party::Foreign(ALICE)])),
    };
    script(vec![
        set(NEEDS, abi::encode(&vec![Need::write("doc")])),
        set(&resource_key("doc"), abi::encode(&resource)),
    ])
}

fn genesis() -> Genesis {
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
            founding("crosspond", CROSSPOND_PROBE, Vec::new(), &[role::CROSSPOND]),
            founding("impostor", CROSSPOND_PROBE, Vec::new(), &[]),
            founding("probe", PROBE, script(Vec::new()), &[]),
            founding("listed", PROBE, listed_for_alice(), &[]),
            founding("awaiter", AWAITER, Vec::new(), &[]),
            founding("oracle", ORACLE, Vec::new(), &[]),
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

async fn found(context: Ctx, dir: &Path) -> Host<Ctx> {
    Host::found(context, "net", dir, block_id(0), genesis())
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

fn as_foreign(foreign: Foreign, target: &str, payload: Vec<u8>, reply: bool) -> Sent {
    Sent {
        foreign,
        message: Message {
            target: target.to_owned(),
            payload,
            reply,
            acting: None,
        },
    }
}

fn sends(salt: u64, from: &str, sent: Vec<Sent>) -> Submission {
    signed(salt, from, abi::encode(&sent))
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

fn applied(receipt: &Receipt) -> &[u8] {
    match &receipt.outcome {
        Outcome::Applied { output } => output,
        Outcome::Rejected(refusal) => panic!("{} was rejected: {refusal}", receipt.program),
    }
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

fn item(source: &str, item: u64) -> ItemRef {
    ItemRef {
        source: source.to_owned(),
        item,
    }
}

fn sent_items(receipt: &Receipt) -> Vec<ItemRef> {
    abi::decode(applied(receipt)).unwrap()
}

fn reply_to(host: &Host<Ctx>, receipt: &Receipt) -> Outcome {
    let [sent] = sent_items(receipt).try_into().unwrap();
    let kept = stored(host, &sent.source, &done(&sent)).expect("the reply came back");
    abi::decode(&kept).unwrap()
}

fn last_guard(host: &Host<Ctx>) -> Guard {
    abi::decode(&stored(host, "ownership", fixture_ownership::LAST).expect("a guard ran")).unwrap()
}

#[test]
fn the_program_bound_to_crosspond_sends_as_a_foreign_user_its_target_sees() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path()).await;
        let probe = as_foreign(ALICE, "probe", script(vec![Step::Env]), true);
        let receipts = apply(&mut host, 1, vec![sends(1, "crosspond", vec![probe])]).await;
        applied(&receipts[0]);
        let [probed, replied] = receipts[0].nested.as_slice() else {
            panic!("{:?}", receipts[0].nested);
        };
        let answered: Vec<Reply> = abi::decode(applied(probed)).unwrap();
        let [Reply::Env(env)] = answered.as_slice() else {
            panic!("{answered:?}");
        };
        let expected = Env {
            network: b"net".to_vec(),
            height: 1,
            time: TIME + 1,
            me: "probe".into(),
            origin: Origin::Program("crosspond".into()),
            sender: Some(Principal::Foreign(ALICE)),
            roles: env.roles.clone(),
            cause: Cause::Message(item("crosspond", 0)),
        };
        assert_eq!(env, &expected);
        applied(replied);
    });
}

#[test]
fn a_foreign_message_from_any_other_program_or_acting_as_an_account_is_refused() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path()).await;
        let impostor = as_foreign(ALICE, "probe", script(vec![Step::Env]), true);
        let mut acting = as_foreign(ALICE, "probe", script(vec![Step::Env]), true);
        acting.message.acting = Some(ACCOUNT);
        let receipts = apply(
            &mut host,
            1,
            vec![
                sends(1, "impostor", vec![impostor]),
                sends(2, "crosspond", vec![acting]),
            ],
        )
        .await;
        let Outcome::Rejected(unbound) = reply_to(&host, &receipts[0]) else {
            panic!("an unbound program sent as a foreign user");
        };
        assert_eq!(unbound.reason, reason::UNAUTHORIZED, "{unbound:?}");
        let Outcome::Rejected(both) = reply_to(&host, &receipts[1]) else {
            panic!("a foreign message also acted as an account");
        };
        assert_eq!(both.reason, reason::INVALID_INPUT, "{both:?}");
        let ran_the_probe = receipts
            .iter()
            .flat_map(|receipt| receipt.nested.iter())
            .any(|nested| {
                matches!(nested.outcome, Outcome::Applied { .. }) && nested.program == "probe"
            });
        assert!(!ran_the_probe, "{receipts:?}");
    });
}

#[test]
fn the_access_check_admits_the_foreign_user_a_list_names_and_no_other() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path()).await;
        let write = |foreign, value: &[u8]| {
            let steps = vec![set(b"doc", value.to_vec())];
            as_foreign(foreign, "listed", script(steps), true)
        };
        let receipts = apply(
            &mut host,
            1,
            vec![
                sends(1, "crosspond", vec![write(ALICE, b"alice")]),
                sends(2, "crosspond", vec![write(BOB, b"bob")]),
            ],
        )
        .await;
        assert!(matches!(
            reply_to(&host, &receipts[0]),
            Outcome::Applied { .. }
        ));
        assert_eq!(stored(&host, "listed", b"doc"), Some(b"alice".to_vec()));
        let Outcome::Rejected(refused) = reply_to(&host, &receipts[1]) else {
            panic!("a foreign user the list does not name wrote");
        };
        assert_eq!(refused.reason, reason::UNAUTHORIZED, "{refused:?}");
    });
}

#[test]
fn the_ownership_guard_is_told_the_foreign_sender_and_makes_it_the_owner() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path()).await;
        let claim = Step::Op(HostOp::SetOwned {
            key: b"o/a".to_vec(),
            value: b"o/a".to_vec(),
            kind: ITEM.to_owned(),
        });
        let sent = as_foreign(ALICE, "probe", script(vec![claim]), false);
        applied(&apply(&mut host, 1, vec![sends(1, "crosspond", vec![sent])]).await[0]);
        let guard = last_guard(&host);
        assert_eq!(guard.program, "probe");
        assert_eq!(guard.origin, Origin::Program("crosspond".into()));
        assert_eq!(guard.sender, Some(Principal::Foreign(ALICE)));
        assert_eq!(
            stored(&host, "ownership", &record_key("probe", b"o/a")),
            Some(abi::encode(&Some(Principal::Foreign(ALICE))))
        );
    });
}

#[test]
fn a_parked_foreign_run_resumes_as_the_same_foreign_user() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path()).await;
        let record = fixture_ownership::Op::Record {
            program: "awaiter".into(),
            key: ANSWERS.to_vec(),
        };
        applied(
            &apply(
                &mut host,
                1,
                vec![signed(1, "ownership", abi::encode(&record))],
            )
            .await[0],
        );
        let plan = Plan {
            target: "oracle".into(),
            acting: None,
            asks: vec![abi::encode(&Ask::Later)],
            together: false,
            kept: 0,
            fail: false,
        };
        let parks = as_foreign(ALICE, "awaiter", abi::encode(&plan), false);
        applied(&apply(&mut host, 2, vec![sends(2, "crosspond", vec![parks])]).await[0]);
        assert_eq!(stored(&host, "awaiter", ANSWERS), None);
        let answer = fixture_oracle::Op::Answer {
            ticket: 0,
            answer: Ok(b"later".to_vec()),
        };
        applied(
            &apply(
                &mut host,
                3,
                vec![signed(3, "oracle", abi::encode(&answer))],
            )
            .await[0],
        );
        assert!(stored(&host, "awaiter", ANSWERS).is_some());
        let guard = last_guard(&host);
        assert_eq!(guard.program, "awaiter");
        assert_eq!(guard.sender, Some(Principal::Foreign(ALICE)));
    });
}

#[test]
fn a_refused_foreign_message_that_wants_a_reply_leaves_the_sending_run_applied() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path()).await;
        let fails = as_foreign(ALICE, "probe", script(vec![Step::Fail("no".into())]), true);
        let receipts = apply(&mut host, 1, vec![sends(1, "crosspond", vec![fails])]).await;
        let Outcome::Rejected(refused) = reply_to(&host, &receipts[0]) else {
            panic!("the failing message applied");
        };
        assert_eq!(refused, Refusal::new("probe", "no"));
        let [sent] = sent_items(&receipts[0]).try_into().unwrap();
        let kept = stored(&host, "crosspond", &fixture_crosspond_probe::sent(&sent));
        assert!(kept.is_some());
        let unreplied = as_foreign(ALICE, "probe", script(vec![Step::Fail("no".into())]), false);
        let receipts = apply(&mut host, 2, vec![sends(2, "crosspond", vec![unreplied])]).await;
        assert_eq!(refusal(&receipts[0]), &Refusal::new("probe", "no"));
    });
}
