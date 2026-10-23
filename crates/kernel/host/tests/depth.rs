//! Runs a frame at the nesting cap within the worker stack budget: Tokio's 2 MiB fallback
//! in debug, and half of that stack in release.
use std::path::Path;

use abi::role::{self, halo, validators};
use abi::{Message, Outcome, Receipt};
use commonware_runtime::{Runner as _, deterministic};
use fixture_relay::Script;
use fixture_wasm::{
    ACCESS, AWAITS, BLOBS as BLOBS_PROGRAM, HALO, IDENTITY, PROGRAMS, RELAY, VALSET,
};
use host::{Block, BlockId, Founding, Genesis, Host, Limits, MAX_DEPTH, Submission, Tip};

const SIGNER: &[u8] = b"signer";
const TIME: u64 = 1_700_000_000;
const TOKIO_FALLBACK_THREAD_STACK: usize = 2 << 20;
const FRAME_THREAD_STACK: usize = if cfg!(debug_assertions) {
    TOKIO_FALLBACK_THREAD_STACK
} else {
    TOKIO_FALLBACK_THREAD_STACK / 2
};

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
        queries: validators::QueryPolicy::Signed,
    }
}

fn block_id(height: u64) -> BlockId {
    let mut id = [0u8; 32];
    id[..8].copy_from_slice(&height.to_be_bytes());
    id
}

fn replying_chain(depth: u32) -> Script {
    if depth == 0 {
        return Script::Note(b"end".to_vec());
    }
    let target = match depth % 2 {
        0 => "ping",
        _ => "pong",
    };
    Script::Send(vec![Message {
        target: target.into(),
        payload: abi::encode(&replying_chain(depth - 1)),
        reply: true,
        acting: None,
    }])
}

fn depth_of(receipt: &Receipt) -> u32 {
    receipt
        .nested
        .iter()
        .map(|nested| 1 + depth_of(nested))
        .max()
        .unwrap_or(0)
}

fn refusals(receipt: &Receipt) -> Vec<String> {
    let here = match &receipt.outcome {
        Outcome::Rejected(refusal) => vec![format!("{}: {refusal}", receipt.program)],
        Outcome::Applied { .. } => Vec::new(),
    };
    here.into_iter()
        .chain(receipt.nested.iter().flat_map(refusals))
        .collect()
}

async fn deepest_frame(context: deterministic::Context, dir: &Path) -> Receipt {
    let mut host = Host::found(context, "net", dir, block_id(0), genesis())
        .await
        .unwrap()
        .0;
    let submission = Submission {
        signer: SIGNER.to_vec(),
        anchor: Tip {
            height: 0,
            id: block_id(0),
        },
        salt: None,
        target: "ping".to_owned(),
        payload: abi::encode(&replying_chain(MAX_DEPTH)),
        client: None,
    };
    let block = Block {
        height: 1,
        id: block_id(1),
        time: TIME + 1,
        submissions: vec![submission],
    };
    let mut applied = host.apply(block).await.unwrap();
    applied.submissions.remove(0)
}

#[test]
fn a_frame_as_deep_as_the_cap_fits_the_worker_stack_budget() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_owned();
    let receipt = std::thread::Builder::new()
        .stack_size(FRAME_THREAD_STACK)
        .spawn(move || {
            deterministic::Runner::default()
                .start(|context| async move { deepest_frame(context, &path).await })
        })
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(refusals(&receipt), Vec::<String>::new());
    assert_eq!(depth_of(&receipt), MAX_DEPTH);
}
