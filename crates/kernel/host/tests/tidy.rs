//! A block's apply never waits on the blob store's tidy: a round held mid-copy leaves every block
//! free to apply, and the round finishes its move once let go.
use abi::HashKind;
use abi::role::{self, halo, validators};
use commonware_runtime::{Metrics as _, Runner as _, Supervisor as _, deterministic};
use fixture_wasm::{ACCESS, AWAITS, BLOBS as BLOBS_PROGRAM, HALO, IDENTITY, PROGRAMS, VALSET};
use host::{BLOBS_DIR, Block, BlockId, Founding, Genesis, Host, Limits};

const SIGNER: &[u8] = b"signer";
const TIME: u64 = 1_700_000_000;

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
        queries: abi::role::validators::QueryPolicy::Signed,
    }
}

fn block_id(height: u64) -> BlockId {
    let mut id = [0u8; 32];
    id[..8].copy_from_slice(&height.to_be_bytes());
    id
}

fn block(height: u64) -> Block {
    Block {
        height,
        id: block_id(height),
        time: TIME + height,
        submissions: Vec::new(),
    }
}

#[test]
fn the_node_counts_what_its_tidy_rounds_did() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = Host::found(
            context.child("host"),
            "net",
            dir.path(),
            block_id(0),
            genesis(),
        )
        .await
        .unwrap()
        .0;
        let framed = blob::frame("part", b"a file an older build kept").unwrap();
        let id = blob::id_of(HashKind::Sha256, &framed).unwrap();
        std::fs::write(dir.path().join(BLOBS_DIR).join(blob::name_of(&id)), &framed).unwrap();

        let first = host.blobs().gate_copies();
        host.apply(block(1)).await.unwrap();
        first.reached();
        let next = host.blobs().gate_copies();
        host.apply(block(2)).await.unwrap();
        drop(first);
        next.reached();
        let encoded = context.encode();
        let counted = |line: &str| encoded.lines().any(|seen| seen.ends_with(line));
        assert!(
            counted("blob_tidy_moved_files_total 1"),
            "the round that moved the old file is counted: {encoded}"
        );
        assert!(counted("blob_tidy_round_seconds_count 1"), "{encoded}");
        assert!(counted("blob_tidy_retired_segments_total 0"), "{encoded}");
        let copied = encoded
            .lines()
            .find_map(|line| line.split_once("blob_tidy_copied_bytes_total "))
            .and_then(|(_, count)| count.parse::<u64>().ok());
        assert!(copied.is_some_and(|bytes| bytes > 0), "{encoded}");
        drop(next);
    });
}

#[test]
fn a_block_applies_while_a_tidy_round_is_held_mid_copy() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = Host::found(context, "net", dir.path(), block_id(0), genesis())
            .await
            .unwrap()
            .0;
        let blobs = dir.path().join(BLOBS_DIR);
        let body = b"a file an older build kept".to_vec();
        let framed = blob::frame("part", &body).unwrap();
        let id = blob::id_of(HashKind::Sha256, &framed).unwrap();
        let legacy = blobs.join(blob::name_of(&id));
        std::fs::write(&legacy, &framed).unwrap();

        let gate = host.blobs().gate_copies();
        host.apply(block(1)).await.unwrap();
        gate.reached();
        host.apply(block(2)).await.unwrap();
        assert_eq!(host.blobs().get(&id).unwrap().unwrap().body, body);
        drop(gate);
        drop(host);

        assert!(
            !legacy.exists(),
            "the round held mid-copy moves the old file in once let go"
        );
        let reopened = blob::Blobs::open(&blobs).unwrap();
        assert_eq!(reopened.get(&id).unwrap().unwrap().body, body);
    });
}
