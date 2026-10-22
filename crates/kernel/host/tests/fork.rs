//! A fork of a network's state: the same rows at the same tip under a network id of its own, every
//! commitment empty, the first parent named and its layout kept.
use std::collections::BTreeSet;
use std::path::Path;

use abi::role::{self, halo, validators};
use abi::{Entry, Message, Outcome, Scan};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use fixture_probe::Step;
use fixture_relay::Script;
use fixture_wasm::{
    ACCESS, AWAITS, BLOBS as BLOBS_PROGRAM, HALO, IDENTITY, PROBE, PROGRAMS, RELAY, VALSET,
};
use host::{Block, BlockId, Error, Founding, Genesis, Host, Layer, Limits, SPENT, Submission, Tip};

const SIGNER: &[u8] = b"signer";
const TIME: u64 = 1_700_000_000;
const FORKED: u64 = 3;

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

fn genesis(network: &[u8]) -> Genesis {
    Genesis {
        network: network.to_vec(),
        queries: validators::QueryPolicy::Signed,
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
            founding("probe", PROBE, abi::encode(&Vec::<Step>::new())),
            founding("ping", RELAY, Vec::new()),
            founding("pong", RELAY, Vec::new()),
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
    }
}

fn block_id(height: u64) -> BlockId {
    let mut id = [0u8; 32];
    id[..8].copy_from_slice(&height.to_be_bytes());
    id
}

fn frame(height: u64) -> Submission {
    let anchored = height - 1;
    Submission {
        signer: SIGNER.to_vec(),
        anchor: Tip {
            height: anchored,
            id: block_id(anchored),
        },
        salt: Some(height),
        target: "probe".to_owned(),
        payload: abi::encode(&Vec::<Step>::new()),
        client: None,
    }
}

fn block(height: u64) -> Block {
    Block {
        height,
        id: block_id(height),
        time: TIME + height,
        submissions: vec![frame(height)],
    }
}

async fn founded(context: Ctx, network: &str, dir: &Path) -> Host<Ctx> {
    let (mut host, _) = Host::found(
        context,
        network,
        dir,
        block_id(0),
        genesis(network.as_bytes()),
    )
    .await
    .unwrap();
    for height in 1..=FORKED {
        applied(&mut host, height).await;
    }
    host
}

async fn applied(host: &mut Host<Ctx>, height: u64) {
    let applied = host.apply(block(height)).await.unwrap();
    let outcomes: Vec<&Outcome> = applied.submissions.iter().map(|r| &r.outcome).collect();
    let all_applied = outcomes
        .iter()
        .all(|outcome| matches!(outcome, Outcome::Applied { .. }));
    assert!(all_applied, "{outcomes:?}");
}

fn chain(depth: u32) -> Script {
    if depth == 0 {
        return Script::Note(b"end".to_vec());
    }
    let target = match depth % 2 {
        0 => "ping",
        _ => "pong",
    };
    Script::Send(vec![Message {
        target: target.into(),
        payload: abi::encode(&chain(depth - 1)),
        reply: false,
        acting: None,
    }])
}

fn rows(host: &Host<Ctx>, program: &str) -> Vec<Entry> {
    host.view(Layer::Confirmed)
        .scan(program, &Scan::prefix(b""))
        .unwrap()
}

fn spent_shapes(host: &Host<Ctx>) -> BTreeSet<usize> {
    rows(host, SPENT)
        .iter()
        .map(|entry| entry.key.len())
        .collect()
}

#[test]
fn a_fork_keeps_the_rows_and_the_tip_under_its_own_id_with_every_commitment_empty() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let host = founded(context.child("found"), "net", dir.path()).await;
        let tip = host.tip().unwrap();
        let identity = rows(&host, "identity");
        assert!(!identity.is_empty());
        drop(host);

        let forked = Host::fork(context.child("fork"), dir.path(), "net-fork-3-a")
            .await
            .unwrap();
        assert_eq!(forked.network(), b"net-fork-3-a");
        assert_eq!(forked.parent(), Some(b"net".as_slice()));
        assert_eq!(forked.tip().unwrap(), tip);
        assert_eq!(rows(&forked, "identity"), identity);
        let held = forked
            .store()
            .commitment("identity")
            .unwrap()
            .entries()
            .await
            .unwrap();
        assert!(held.is_empty(), "{held:?}");
        drop(forked);

        let mut reopened = Host::open(context.child("reopened"), "net-fork-3-a", dir.path())
            .await
            .unwrap();
        assert_eq!(reopened.network(), b"net-fork-3-a");
        assert_eq!(reopened.parent(), Some(b"net".as_slice()));
        applied(&mut reopened, FORKED + 1).await;
        assert_eq!(reopened.height().unwrap(), FORKED + 1);
    });
}

#[test]
fn a_fork_of_the_network_the_layout_table_names_keeps_its_parents_layout() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let host = founded(context.child("found"), "duckpond", dir.path()).await;
        assert_eq!(spent_shapes(&host), BTreeSet::from([40]));
        drop(host);

        let mut forked = Host::fork(context.child("fork"), dir.path(), "duckpond-fork-3-a")
            .await
            .unwrap();
        applied(&mut forked, FORKED + 1).await;
        assert_eq!(spent_shapes(&forked), BTreeSet::from([40]));
    });
}

#[test]
fn a_fork_of_a_fork_names_the_first_parent_and_a_fork_under_its_own_id_is_refused() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        drop(founded(context.child("found"), "net", dir.path()).await);
        drop(
            Host::fork(context.child("first"), dir.path(), "net-fork-3-a")
                .await
                .unwrap(),
        );
        let second = Host::fork(context.child("second"), dir.path(), "net-fork-3-b")
            .await
            .unwrap();
        assert_eq!(second.network(), b"net-fork-3-b");
        assert_eq!(second.parent(), Some(b"net".as_slice()));
        drop(second);

        let again = Host::fork(context.child("again"), dir.path(), "net-fork-3-b").await;
        let refused = matches!(again, Err(Error::Unrenamed(ref id)) if id == "net-fork-3-b");
        assert!(refused);
    });
}

#[test]
fn a_fork_of_a_network_that_nests_eight_deep_nests_eight_deep() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        drop(founded(context.child("found"), "duckpond", dir.path()).await);
        let mut forked = Host::fork(context.child("fork"), dir.path(), "duckpond-fork-3-a")
            .await
            .unwrap();
        let height = FORKED + 1;
        let nine_deep = Submission {
            target: "ping".to_owned(),
            payload: abi::encode(&chain(9)),
            ..frame(height)
        };
        let nine_deep_block = Block {
            submissions: vec![nine_deep],
            ..block(height)
        };
        let applied = forked.apply(nine_deep_block).await.unwrap();
        let sentence = match &applied.submissions[0].outcome {
            Outcome::Rejected(refusal) => refusal.sentence.clone(),
            Outcome::Applied { .. } => "applied".to_owned(),
        };
        assert_eq!(sentence, "messages nest deeper than 8");
    });
}
