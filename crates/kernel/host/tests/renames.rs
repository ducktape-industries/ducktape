//! A rename at the roster fold: the program answers under its new id over the same state, its
//! account moves with it, identity's refusal holds both ids where they are and says why each
//! height, and a later admission under a live place gets a place of its own.
use std::collections::BTreeMap;

use abi::role::{self, halo, identity, validators};
use abi::{HostOp, HostReply, Origin, Outcome, Receipt, reason};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use fixture_probe::{Reply, Step};
use fixture_programs::Change;
use fixture_wasm::{
    ACCESS, AWAITS, BLOBS as BLOBS_PROGRAM, HALO, IDENTITY, PROBE, PROGRAMS, VALSET,
};
use host::{
    Applied, Block, BlockId, Founding, Genesis, Host, Layer, Limits, NETWORK, Places, Submission,
    Tip,
};

const SIGNER: &[u8] = b"signer";
const TIME: u64 = 1_700_000_000;
const RENAMED: &str = "owner/probe";

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

fn set(key: &[u8], value: &[u8]) -> Step {
    Step::Op(HostOp::Set {
        key: key.to_vec(),
        value: value.to_vec(),
    })
}

fn get(key: &[u8]) -> Step {
    Step::Op(HostOp::Get(key.to_vec()))
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
            founding("programs", PROGRAMS, Vec::new()),
            founding("valset", VALSET, Vec::new()),
            founding(
                "identity",
                IDENTITY,
                abi::encode(&vec![(SIGNER.to_vec(), 1u64)]),
            ),
            founding("blobs", BLOBS_PROGRAM, Vec::new()),
            founding("awaits", AWAITS, Vec::new()),
            founding("probe", PROBE, abi::encode(&vec![set(b"k", b"v")])),
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
        queries: validators::QueryPolicy::Signed,
    }
}

fn block_id(height: u64) -> BlockId {
    let mut id = [0u8; 32];
    id[..8].copy_from_slice(&height.to_be_bytes());
    id
}

fn block(height: u64, sent: Vec<(&str, Vec<u8>)>) -> Block {
    let submissions = sent
        .into_iter()
        .enumerate()
        .map(|(n, (target, payload))| Submission {
            signer: SIGNER.to_vec(),
            anchor: Tip {
                height: height - 1,
                id: block_id(height - 1),
            },
            salt: Some(height * 64 + n as u64),
            target: target.to_owned(),
            payload,
            client: None,
        })
        .collect();
    Block {
        height,
        id: block_id(height),
        time: TIME + height,
        submissions,
    }
}

fn rename() -> (&'static str, Vec<u8>) {
    let change = Change::Rename {
        from: "probe".into(),
        to: RENAMED.into(),
    };
    ("programs", abi::encode(&change))
}

fn closed(number: u64) -> (&'static str, Vec<u8>) {
    (
        "identity",
        abi::encode(&(b"#closed/probe".to_vec(), number)),
    )
}

async fn found(context: Ctx, dir: &std::path::Path) -> Host<Ctx> {
    Host::found(context, "net", dir, block_id(0), genesis())
        .await
        .unwrap()
        .0
}

async fn ask(host: &Host<Ctx>, program: &str, steps: Vec<Step>) -> Result<Vec<Reply>, String> {
    let answer = host
        .query(
            Layer::Confirmed,
            TIME,
            Origin::System,
            program,
            abi::encode(&steps),
        )
        .await
        .unwrap();
    answer
        .map(|bytes| abi::decode(&bytes).unwrap())
        .map_err(|refusal| refusal.reason)
}

async fn account(host: &Host<Ctx>, program: &str) -> Option<u64> {
    let asked = identity::Query::OfProgram(program.to_owned());
    let answer = host
        .query(
            Layer::Confirmed,
            TIME,
            Origin::System,
            "identity",
            abi::encode(&asked),
        )
        .await
        .unwrap()
        .unwrap();
    let identity::Reply::Account(number) = abi::decode(&answer).unwrap() else {
        panic!("identity answered OfProgram with another reply");
    };
    number
}

fn value(replies: &[Reply]) -> Option<Vec<u8>> {
    match replies {
        [Reply::Host(HostReply::Value(value))] => value.clone(),
        other => panic!("{other:?}"),
    }
}

fn renamings(applied: &Applied) -> Vec<&Receipt> {
    applied
        .admissions
        .iter()
        .filter(|receipt| receipt.program == "identity")
        .collect()
}

#[test]
fn a_renamed_program_answers_under_its_new_id_over_the_same_state_and_account() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context.child("found"), dir.path()).await;
        let number = account(&host, "probe").await;
        assert!(number.is_some(), "probe has an account before the rename");
        let root = host.store().root("probe").unwrap();
        let code = host.programs().unwrap()["probe"];

        host.apply(block(1, vec![rename()])).await.unwrap();
        let applied = host.apply(block(2, Vec::new())).await.unwrap();

        let told = renamings(&applied);
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(matches!(told[0].outcome, Outcome::Applied { .. }));
        let programs = host.programs().unwrap();
        assert_eq!(programs.get(RENAMED), Some(&code));
        assert!(!programs.contains_key("probe"));
        assert_eq!(
            value(&ask(&host, RENAMED, vec![get(b"k")]).await.unwrap()),
            Some(b"v".to_vec())
        );
        assert_eq!(
            ask(&host, "probe", vec![get(b"k")]).await,
            Err(reason::UNKNOWN_PROGRAM.to_owned())
        );
        assert_eq!(account(&host, RENAMED).await, number);
        assert_eq!(account(&host, "probe").await, None);
        assert_eq!(host.store().root("probe").unwrap(), root);
        assert_eq!(host.store().root(RENAMED).unwrap(), None);
        let place = host
            .view(Layer::Confirmed)
            .get(NETWORK, format!("place/{RENAMED}").as_bytes())
            .unwrap();
        assert_eq!(place, Some(b"probe".to_vec()));

        let write = (RENAMED, abi::encode(&vec![set(b"k2", b"w")]));
        host.apply(block(3, vec![write])).await.unwrap();
        let landed = host.view(Layer::Confirmed).get("probe", b"k2").unwrap();
        assert_eq!(landed, Some(b"w".to_vec()), "a write lands in the place");
        assert_ne!(host.store().root("probe").unwrap(), root);

        let root = host.root().unwrap();
        drop(host);
        let reopened = Host::open(context.child("reopen"), "net", dir.path())
            .await
            .unwrap();
        assert_eq!(reopened.root().unwrap(), root);
        assert_eq!(
            value(&ask(&reopened, RENAMED, vec![get(b"k2")]).await.unwrap()),
            Some(b"w".to_vec())
        );
    });
}

#[test]
fn identity_refusing_a_rename_holds_both_ids_and_says_why_at_each_height() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path()).await;
        host.apply(block(1, vec![closed(1), rename()]))
            .await
            .unwrap();
        for height in 2..4 {
            let applied = host.apply(block(height, Vec::new())).await.unwrap();
            let told = renamings(&applied);
            assert_eq!(told.len(), 1, "height {height}: {told:?}");
            let Outcome::Rejected(refusal) = &told[0].outcome else {
                panic!("height {height}: {:?}", told[0]);
            };
            assert_eq!(refusal.reason, "closed");
            let programs = host.programs().unwrap();
            assert!(programs.contains_key("probe"), "height {height}");
            assert!(!programs.contains_key(RENAMED), "height {height}");
            assert_eq!(
                value(&ask(&host, "probe", vec![get(b"k")]).await.unwrap()),
                Some(b"v".to_vec())
            );
        }
        host.apply(block(4, vec![closed(0)])).await.unwrap();
        host.apply(block(5, Vec::new())).await.unwrap();
        let programs = host.programs().unwrap();
        assert!(programs.contains_key(RENAMED));
        assert!(!programs.contains_key("probe"));
    });
}

#[test]
fn an_admission_under_a_live_place_gets_a_place_of_its_own() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path()).await;
        host.apply(block(1, vec![rename()])).await.unwrap();
        host.apply(block(2, Vec::new())).await.unwrap();
        let code = host.programs().unwrap()[RENAMED];
        let again = Change::Set(abi::role::programs::Entry {
            program: "probe".into(),
            code,
            params: abi::encode(&vec![get(b"k"), set(b"k", b"fresh")]),
        });
        host.apply(block(3, vec![("programs", abi::encode(&again))]))
            .await
            .unwrap();
        host.apply(block(4, Vec::new())).await.unwrap();

        let place = host
            .view(Layer::Confirmed)
            .get(NETWORK, b"place/probe")
            .unwrap();
        assert_eq!(place, Some(b"probe@4".to_vec()));
        assert_eq!(
            value(&ask(&host, "probe", vec![get(b"k")]).await.unwrap()),
            Some(b"fresh".to_vec())
        );
        assert_eq!(
            value(&ask(&host, RENAMED, vec![get(b"k")]).await.unwrap()),
            Some(b"v".to_vec())
        );
    });
}

#[test]
fn an_old_id_another_program_now_holds_as_its_place_answers_nothing() {
    let places = Places::new(BTreeMap::from([(RENAMED.to_owned(), "probe".to_owned())]));
    assert_eq!(places.of(RENAMED), Some("probe"));
    assert_eq!(places.of("probe"), None);
    assert_eq!(places.of("chat"), Some("chat"));
}
