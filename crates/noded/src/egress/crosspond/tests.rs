use abi::role::crosspond::{LONGEST_OP_PROOF, PROGRAM_ID, StateProof, max_packet};
use commonware_cryptography::Signer as _;

use super::*;

const NAME: &str = "a-network-whose-name-runs-on-and-on";

fn packet(op: Vec<u8>) -> Packet {
    Packet {
        source: [1; 32],
        destination: [2; 32],
        seq: u64::MAX,
        from: u64::MAX,
        name: "chief".into(),
        via: None,
        port: "chat".into(),
        op,
        sent: u64::MAX,
        deadline: None,
    }
}

fn packet_of(bytes: usize) -> Packet {
    let bare = abi::encode(&packet(Vec::new())).len();
    packet(vec![7; bytes - bare])
}

fn deliver_frame(name: &str, packet: Packet) -> Vec<u8> {
    let key = PrivateKey::from_seed(1);
    let deliver = Op::DeliverProven {
        from: [1; 32],
        epoch: u64::MAX,
        low: None,
        packets: vec![Proven {
            packet,
            proof: StateProof {
                location: u64::MAX,
                proof: vec![9; LONGEST_OP_PROOF],
            },
        }],
    };
    let anchor = Tip {
        height: u64::MAX,
        id: [3; 32],
    };
    let target = "p".repeat(PROGRAM_ID);
    let payload = abi::encode(&deliver);
    frame(&key, name, anchor, None, &target, payload).encode()
}

#[test]
fn a_deliver_with_no_low_of_one_packet_at_the_bound_with_the_longest_proof_fits_and_no_more() {
    let name = NAME.repeat(1 << 10);
    let bound = max_packet(&name);
    let packet = packet_of(bound);
    assert_eq!(abi::encode(&packet).len(), bound);
    assert!(Block::carries(&deliver_frame(&name, packet)));
    assert!(!Block::carries(&deliver_frame(&name, packet_of(bound + 1))));
    let short = max_packet("b");
    assert_eq!(short - bound, name.len() - 1);
    assert!(Block::carries(&deliver_frame("b", packet_of(short))));
    assert!(!Block::carries(&deliver_frame("b", packet_of(short + 1))));
}

#[test]
fn a_frame_measured_by_length_fits_exactly_when_its_bytes_do() {
    let frame = deliver_frame("b", packet_of(max_packet("b")));
    assert!(Block::carries_length(frame.len()));
    assert!(!Block::carries_length(frame.len() + 1));
    assert!(!Block::carries_length(usize::MAX));
}

struct Counted {
    tip: u64,
    expiry: Option<u64>,
    blocks: u64,
}

impl Blocks for Counted {
    fn tip(&mut self) -> u64 {
        self.tip
    }

    async fn expiry(&mut self) -> Option<u64> {
        self.expiry
    }

    async fn changed(&mut self) -> bool {
        let more = self.blocks > 0;
        if more {
            self.blocks -= 1;
            self.tip += 1;
        }
        more
    }
}

#[tokio::test]
async fn a_relay_waiting_for_an_epoch_to_close_wakes_at_its_height_and_not_once_blocks_stop() {
    let mut blocks = Counted {
        tip: 10,
        expiry: None,
        blocks: 8,
    };
    assert!(reach(&mut blocks, 13).await);
    assert_eq!(blocks.tip, 13);
    assert!(reach(&mut blocks, 13).await);
    assert_eq!(blocks.tip, 13);
    let mut stopped = Counted {
        tip: 10,
        expiry: None,
        blocks: 2,
    };
    assert!(!reach(&mut stopped, 13).await);
}
