use abi::valset::MAX_MEMBERS;
use commonware_codec::EncodeSize as _;
use commonware_codec::types::lazy::Lazy;
use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_consensus::simplex::types::{Finalization, Proposal};
use commonware_consensus::types::{Epoch, Round, View};
use commonware_cryptography::certificate::Signers;
use commonware_cryptography::ed25519::certificate::Certificate;
use commonware_cryptography::{Signer as _, ed25519};
use commonware_utils::Participant;
use host::Tip;
use node::{BLOCK_BYTES, Block, Digest, MESSAGE_BYTES};

const RESOLVER_FRAMING: usize = 8 + 1 + 5;

const WIDEST: Tip = Tip {
    height: u64::MAX - 1,
    id: [0xff; 32],
};

fn largest_carried(bytes: &[u8]) -> usize {
    (0..=bytes.len())
        .rev()
        .find(|&length| Block::carries(&bytes[..length]))
        .unwrap()
}

fn finalization_signed_by_every_member() -> Finalization<Scheme, Digest> {
    let signature = ed25519::PrivateKey::from_seed(1).sign(b"namespace", b"message");
    let members = MAX_MEMBERS as u32;
    let certificate = Certificate {
        signers: Signers::new(members, (0..members).map(Participant::new)).unwrap(),
        signatures: vec![Lazy::from(signature); MAX_MEMBERS],
    };
    let round = Round::new(Epoch::new(u64::MAX), View::new(u64::MAX));
    Finalization {
        proposal: Proposal::new(round, View::new(u64::MAX), Digest::from([0xff; 32])),
        certificate,
    }
}

#[test]
fn the_largest_frame_a_block_carries_fills_it_and_one_byte_more_does_not() {
    let bytes = vec![0u8; BLOCK_BYTES];
    let largest = largest_carried(&bytes);
    assert!(!Block::carries(&bytes[..largest + 1]));
    let frame = bytes[..largest].to_vec();
    let block = Block::packed(WIDEST, u64::MAX, [&frame]);
    assert_eq!(block.frames.len(), 1);
    assert!(block.encode_size() <= BLOCK_BYTES);
}

#[test]
fn a_block_packs_the_waiting_frames_in_order_until_the_next_would_overflow() {
    let half = vec![1u8; BLOCK_BYTES / 2];
    let small = vec![2u8; 16];
    let waiting = [&half, &half, &small];
    let block = Block::packed(WIDEST, u64::MAX, waiting);
    assert_eq!(block.frames, vec![half.clone()]);
    let block = Block::packed(WIDEST, u64::MAX, [&half, &small]);
    assert_eq!(block.frames, vec![half, small]);
    assert!(block.encode_size() <= BLOCK_BYTES);
}

#[test]
fn the_widest_backfill_reply_fits_one_mesh_message() {
    let bytes = vec![0u8; BLOCK_BYTES];
    let frame = bytes[..largest_carried(&bytes)].to_vec();
    let block = Block::packed(WIDEST, u64::MAX, [&frame]);
    let reply = (finalization_signed_by_every_member(), block);
    assert!(RESOLVER_FRAMING + reply.encode_size() <= MESSAGE_BYTES as usize);
}
