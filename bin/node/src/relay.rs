//! the submit-relay channel wire format — how a relaying node delivers a
//! signed frame, and how a validator answers with the frame's consensus fate.
//!
//! transport: ordinary submits ship the frame bytes on `CHANNEL_SUBMIT_RELAY`
//! to one current validator, exactly as `node::encode_frame` produced them.
//! a frame that references a node-local blob first fans those bytes out to
//! EVERY current validator in bounded, content-addressed chunks; only after all
//! validators acknowledge the bytes does one validator take consensus custody.
//! the frame's OWN signature is the AUTHORSHIP: it binds
//! (origin, seq, target, payload, required_blob) to the origin key, so forgery is impossible,
//! and a byte-identical replay collapses in the consensus lane's exactly-once
//! digest gate. authorship is not admission, though — the RELAYING peer must
//! itself hold committed node standing (member or resident), exactly as
//! `verify_blob_offer` requires of a blob offer. consensus custody is a bounded
//! resource, and a door open to every mesh peer is a door open to anyone who
//! reaches the mesh. the check is on the COURIER, never on the frame's origin:
//! a resident relays frames signed by keys with no standing at all (an agent's
//! per-run session key), and every one of them still enters consensus on the
//! same contract as a validator's local HTTP submit lane. who may do WHAT is
//! per-module policy, decided deterministically inside the state machine (the
//! acl module's dispatch gate plus each module's own origin checks), never at
//! the transport door. the validator takes consensus custody via
//! `submit_frame` and replies when the frame drains — Applied with the sealed
//! block's coordinates, Rejected for a deterministic no-op, Refused for door
//! failures and expired holds.
//!
//! json on the wire: matches the module-interface idiom. blob chunks use hex rather than a
//! JSON byte array so the encoded message stays below commonware's 2 MiB cap.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// Relayed blobs share the generic CAS transfer ceiling. One offer plus all
/// chunks must fit the sender's inbound-mailbox quota burst; this transport
/// does not retransmit dropped chunks.
pub const MAX_RELAY_BLOB_BYTES: usize = blobstore::MAX_TRANSFER_BYTES;

/// 768 KiB raw -> 1.5 MiB hex plus a small JSON envelope, safely below the
/// process-wide 2 MiB commonware message cap.
pub const RELAY_BLOB_CHUNK_BYTES: usize = 768 * 1024;

// every chunk of a max-size pack plus its one offer fits one sender's burst
// of the inbound mailbox — a DROP boundary, not a backpressure one.
const RELAY_MESSAGES_PER_PACK: usize = MAX_RELAY_BLOB_BYTES.div_ceil(RELAY_BLOB_CHUNK_BYTES) + 1;
const _: () = assert!(RELAY_MESSAGES_PER_PACK <= crate::constants::MESH_QUOTA_BURST);
// this repository's own full-history pack (83 MiB) fits.
const _: () = assert!(MAX_RELAY_BLOB_BYTES >= 83 * 1024 * 1024);

/// The extra hold a blob transfer earns on top of `SUBMIT_HOLD`,
/// budgeted at a 1 MiB/s floor over the bytes that actually cross the wire:
/// chunks ride hex-encoded (2x), and the fan-out is SERIAL per target, so the
/// last target only starts receiving after every earlier one is done. The
/// base hold alone assumed the pack lands within an app-submit budget —
/// structurally impossible for a multi-MB pack crossing a WAN validator
/// link — and a single-target 1 MiB/s window still timed out every multi-
/// validator push of a large pack.
pub fn blob_transfer_allowance(total: u64, targets: usize) -> std::time::Duration {
    const FLOOR_BYTES_PER_SEC: u64 = 1024 * 1024;
    const HEX_INFLATION: u64 = 2;
    let wire_bytes = total
        .saturating_mul(HEX_INFLATION)
        .saturating_mul(targets.max(1) as u64);
    std::time::Duration::from_secs(wire_bytes.div_ceil(FLOOR_BYTES_PER_SEC))
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RelayOutcome {
    /// drained Applied at `height`; `root_hash` is the PER-BLOCK boundary
    /// hash the frame settled at (what a local app-surface hold reports).
    Applied { height: u64, root_hash: String },
    /// finalized but deterministically rejected by its module.
    Rejected { detail: String },
    /// refused at the door (bad frame / non-external origin) or the
    /// validator's hold expired before finalization — the op may still
    /// land later; clients re-query on block events.
    Refused { detail: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RelayMsg {
    /// a resident-signed frame, bytes exactly as `encode_frame` produced.
    Submit { frame: Vec<u8> },
    /// Authorize and begin a pre-consensus pack transfer. `frame` is signed and
    /// must reference `digest`; receivers allocate nothing until it verifies.
    BlobOffer {
        frame: Vec<u8>,
        digest: [u8; 32],
        total: u64,
    },
    /// One ordered chunk for an accepted offer. Hex avoids JSON's ~3.5x byte
    /// array expansion; content addressing verifies the completed transfer.
    BlobChunk {
        frame_id: [u8; 32],
        digest: [u8; 32],
        offset: u64,
        chunk_hex: String,
    },
    /// A validator's acknowledgement (or clean refusal) of one blob offer.
    BlobResult {
        frame_id: [u8; 32],
        digest: [u8; 32],
        error: Option<String>,
    },
    /// a validator-to-validator LEADER NUDGE: the sender holds real parked
    /// proposals and (by its local estimate) the receiver leads the CURRENT
    /// view — close it now by beating the idle nop early, so leadership
    /// rotation runs at network speed instead of the 1s idle beat. carries
    /// nothing and grants nothing: the receiver acts only when quiet, only
    /// ever by beating one deterministic nop, and only for a sender that is a
    /// current validator — a stray, stale, or mis-aimed nudge is harmless.
    Nudge,
    /// the validator's answer, keyed by the frame's content address.
    Reply {
        frame_id: [u8; 32],
        outcome: RelayOutcome,
    },
}

pub fn encode_msg(m: &RelayMsg) -> Vec<u8> {
    serde_json::to_vec(m).expect("serializable")
}

pub fn decode_msg(b: &[u8]) -> Result<RelayMsg, String> {
    serde_json::from_slice(b).map_err(|e| e.to_string())
}

/// The optional storage prerequisite bound by the frame's verified signature.
/// Module target and payload are opaque to the relay.
pub fn required_blob_digest(frame: &[u8]) -> Option<[u8; 32]> {
    node::decode_frame_with_blob(frame).ok()?.2
}

pub use duckfs_core::{to_hex as encode_hex, unhex as decode_hex};

/// Ordered, bounded assembly for one accepted blob offer. The digest check is
/// completed before bytes enter the shared blob store.
pub struct BlobAssembly {
    digest: [u8; 32],
    total: usize,
    bytes: Vec<u8>,
}

impl BlobAssembly {
    pub fn new(digest: [u8; 32], total: u64) -> Result<Self, String> {
        let total = usize::try_from(total).map_err(|_| "relay blob length does not fit usize")?;
        if total == 0 || total > MAX_RELAY_BLOB_BYTES {
            return Err(format!(
                "relay blob must be 1..={MAX_RELAY_BLOB_BYTES} bytes, got {total}"
            ));
        }
        Ok(Self {
            digest,
            total,
            bytes: Vec::new(),
        })
    }

    /// Append one exact-next chunk. `Ok(None)` needs more data;
    /// `Ok(Some(bytes))` is a complete, digest-verified pack.
    pub fn push(&mut self, offset: u64, chunk_hex: &str) -> Result<Option<Vec<u8>>, String> {
        let offset = usize::try_from(offset).map_err(|_| "blob chunk offset does not fit usize")?;
        if offset != self.bytes.len() {
            return Err(format!(
                "blob chunk offset {offset} does not match next offset {}",
                self.bytes.len()
            ));
        }
        if chunk_hex.len() > RELAY_BLOB_CHUNK_BYTES * 2 {
            return Err("blob chunk exceeds the relay chunk ceiling".into());
        }
        let chunk = decode_hex(chunk_hex)?;
        if chunk.is_empty() {
            return Err("blob chunk must not be empty".into());
        }
        if self.bytes.len().saturating_add(chunk.len()) > self.total {
            return Err("blob chunks exceed the offered total".into());
        }
        self.bytes.extend_from_slice(&chunk);
        if self.bytes.len() != self.total {
            return Ok(None);
        }
        let actual: [u8; 32] = Sha256::digest(&self.bytes).into();
        if actual != self.digest {
            return Err("completed relay blob does not match its digest".into());
        }
        Ok(Some(std::mem::take(&mut self.bytes)))
    }
}

/// the validator's door check, pure so it is testable without a mesh: the
/// frame must decode AND verify (the kernel checks the signature binds
/// origin/seq/target/payload), its origin must be `Origin::External`, and the
/// RELAYING peer must hold committed node standing — a member or a resident.
///
/// the standing check is on the COURIER, not on the frame's author: this lane
/// exists precisely so a key with no standing (an agent's per-run session key,
/// a wallet, a passkey) can submit through a node that has some, and its ops
/// enter consensus on the same contract as a validator's local HTTP submit
/// lane. what the check buys is the bound: consensus custody is finite
/// (`node::MAX_CUSTODY_FRAMES`) and a door open to every mesh peer lets anyone
/// who reaches the mesh fill it. authorization for the OP stays per-module
/// policy resolved deterministically at dispatch (the acl module's gate plus
/// each module's own origin checks), never a transport-door decision.
pub fn verify_relay_submit(
    frame: &[u8],
    peer: &[u8],
    members: &[Vec<u8>],
    residents: &[Vec<u8>],
) -> Result<node::FrameId, String> {
    let (origin, _msg, _required_blob) =
        node::decode_frame_with_blob(frame).map_err(|e| format!("bad frame: {e}"))?;
    let sdk::Origin::External(_) = origin else {
        return Err("relayed frames carry an external origin".into());
    };
    if !holds_node_standing(peer, members, residents) {
        return Err("relaying peer holds no committed node standing".into());
    }
    Ok(node::frame_id(frame))
}

/// is this raw key a committed member or resident? the ONE standing predicate
/// both relay doors read, so a courier and a blob offeror are judged by the
/// same set.
fn holds_node_standing(key: &[u8], members: &[Vec<u8>], residents: &[Vec<u8>]) -> bool {
    members
        .iter()
        .chain(residents)
        .any(|standing| standing.as_slice() == key)
}

/// Blob offers may originate from a standing resident or a current validator
/// (the latter is the direct-to-validator HTTP push path fanning out to its
/// peers). Standing belongs to the authenticated courier; the original user
/// frame keeps its own signature and binds the offered digest before allocation.
pub fn verify_blob_offer(
    courier: &[u8],
    frame: &[u8],
    digest: &[u8; 32],
    members: &[Vec<u8>],
    residents: &[Vec<u8>],
) -> Result<node::FrameId, String> {
    let (origin, _msg, required_blob) =
        node::decode_frame_with_blob(frame).map_err(|e| format!("bad frame: {e}"))?;
    let sdk::Origin::External(_) = origin else {
        return Err("blob offers carry an external origin".into());
    };
    if !holds_node_standing(courier, members, residents) {
        return Err("blob offer courier holds no committed node standing".into());
    }
    if required_blob.as_ref() != Some(digest) {
        return Err("blob offer digest is not referenced by its signed frame".into());
    }
    Ok(node::frame_id(frame))
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_cryptography::Signer as _;

    fn sk(seed: u64) -> commonware_cryptography::ed25519::PrivateKey {
        commonware_cryptography::ed25519::PrivateKey::from_seed(seed)
    }

    fn msg() -> sdk::Msg {
        sdk::Msg {
            target: "kv".into(),
            payload: b"{}".to_vec(),
        }
    }

    #[test]
    fn a_pack_transfer_earns_hold_proportional_to_its_size_and_fan_out() {
        use std::time::Duration;
        assert_eq!(blob_transfer_allowance(1, 1), Duration::from_secs(1));
        assert_eq!(
            blob_transfer_allowance(4 * 1024 * 1024, 1),
            Duration::from_secs(8),
            "a 4 MiB pack crosses one link as 8 MiB of hex: 8s on top of the base hold"
        );
        assert_eq!(
            blob_transfer_allowance(4 * 1024 * 1024, 3),
            Duration::from_secs(24),
            "three serial targets each take the whole transfer in turn"
        );
        assert!(
            blob_transfer_allowance(83 * 1024 * 1024, 2)
                > blob_transfer_allowance(64 * 1024 * 1024, 2),
            "the budget grows with the pack"
        );
        assert_eq!(
            blob_transfer_allowance(MAX_RELAY_BLOB_BYTES as u64, 1),
            Duration::from_secs(191),
            "the relay cap (95.25 MiB, 190.5 MiB of hex) bounds a single-target allowance"
        );
    }

    /// A pack the door accepts is a pack the assembly accepts, right up to
    /// the shared limit — and not one byte past it.
    #[test]
    fn the_relay_assembly_accepts_every_pack_the_door_does() {
        let digest = [1; 32];
        assert!(BlobAssembly::new(digest, 83 * 1024 * 1024).is_ok());
        assert!(BlobAssembly::new(digest, blobstore::MAX_TRANSFER_BYTES as u64).is_ok());
        assert!(BlobAssembly::new(digest, blobstore::MAX_TRANSFER_BYTES as u64 + 1).is_err());
    }

    #[test]
    fn wire_round_trips() {
        for m in [
            RelayMsg::Submit {
                frame: vec![1, 2, 3],
            },
            RelayMsg::BlobOffer {
                frame: vec![4, 5],
                digest: [6; 32],
                total: 7,
            },
            RelayMsg::BlobChunk {
                frame_id: [8; 32],
                digest: [9; 32],
                offset: 10,
                chunk_hex: "abcd".into(),
            },
            RelayMsg::BlobResult {
                frame_id: [11; 32],
                digest: [12; 32],
                error: None,
            },
            RelayMsg::Reply {
                frame_id: [7; 32],
                outcome: RelayOutcome::Applied {
                    height: 42,
                    root_hash: "aa".into(),
                },
            },
            RelayMsg::Reply {
                frame_id: [0; 32],
                outcome: RelayOutcome::Refused { detail: "x".into() },
            },
        ] {
            assert_eq!(decode_msg(&encode_msg(&m)).expect("round trip"), m);
        }
    }

    /// the door judges the COURIER, not the author: a frame signed by a key
    /// with no standing whatsoever enters consensus, as long as the peer
    /// relaying it holds committed node standing. that is what keeps this lane
    /// on one contract with the validator's local HTTP submit lane while still
    /// bounding who can spend a validator's finite consensus custody.
    #[test]
    fn door_accepts_a_standingless_author_relayed_by_a_standing_peer() {
        let courier = sk(1).public_key().as_ref().to_vec();
        let author = sk(7);
        let frame = node::encode_frame(&author, 3, &msg());

        let id = verify_relay_submit(&frame, &courier, std::slice::from_ref(&courier), &[])
            .expect("a member courier is accepted");
        assert_eq!(id, node::frame_id(&frame));
        assert!(
            verify_relay_submit(&frame, &courier, &[], std::slice::from_ref(&courier)).is_ok(),
            "a resident courier is accepted too"
        );
    }

    #[test]
    fn door_refuses_a_relaying_peer_with_no_committed_standing() {
        let stranger = sk(2).public_key().as_ref().to_vec();
        let member = sk(1).public_key().as_ref().to_vec();
        // the frame is the member's OWN, validly signed: only the peer that
        // carried it lacks standing, and that alone must refuse it.
        let frame = node::encode_frame(&sk(1), 0, &msg());

        let err = verify_relay_submit(&frame, &stranger, std::slice::from_ref(&member), &[])
            .expect_err("a peer with no standing may not spend consensus custody");
        assert!(err.contains("no committed node standing"), "{err}");
    }

    #[test]
    fn door_refuses_a_signature_tampered_frame_that_still_parses() {
        let author = sk(7);
        let courier = author.public_key().as_ref().to_vec();
        let members = std::slice::from_ref(&courier);
        let mut tampered = node::encode_frame(&author, 0, &msg());

        // flip a bit INSIDE the trailing 64-byte ed25519 signature: the binary
        // envelope (the length-prefixed origin/seq/target/payload preimage) is
        // untouched, so the frame still PARSES — only the signature binding
        // breaks. this exercises the signature gate, not the envelope parser.
        let sig_start = tampered.len() - 64;
        tampered[sig_start] ^= 0x01;

        // it fails at proof verification, NOT as a parse error: a genuine
        // junk envelope errors with different wording.
        let junk = verify_relay_submit(b"not a frame", &courier, members, &[]).unwrap_err();
        let err = verify_relay_submit(&tampered, &courier, members, &[]).unwrap_err();
        assert_ne!(err, junk, "tamper must fail at the proof, not the parser");
        assert!(err.contains("frame proof does not bind"), "{err}");
    }

    #[test]
    fn blob_offer_requires_standing_and_a_matching_signed_digest() {
        let author = sk(8);
        let me = author.public_key().as_ref().to_vec();
        let digest = [0xCD; 32];
        let msg = sdk::Msg {
            target: "independently-installed-module".into(),
            payload: b"opaque module operation".to_vec(),
        };
        let frame = node::encode_frame_with_blob(&author, 1, &msg, Some(digest));
        let courier = sk(9).public_key().as_ref().to_vec();
        let admitted = verify_blob_offer(
            &courier,
            &frame,
            &digest,
            std::slice::from_ref(&courier),
            &[],
        )
        .unwrap();
        assert_eq!(admitted, node::frame_id(&frame));
        assert!(
            verify_blob_offer(
                &courier,
                &frame,
                &[0; 32],
                std::slice::from_ref(&courier),
                &[]
            )
            .is_err()
        );
        assert_eq!(
            node::decode_frame(&frame).unwrap().0,
            sdk::Origin::External(me.clone())
        );
        assert!(
            verify_blob_offer(&courier, &frame, &digest, std::slice::from_ref(&me), &[]).is_err()
        );
        let mut tampered = frame.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(
            verify_blob_offer(
                &courier,
                &tampered,
                &digest,
                std::slice::from_ref(&courier),
                &[]
            )
            .is_err()
        );
        assert!(verify_blob_offer(&me, &frame, &digest, std::slice::from_ref(&me), &[]).is_ok());
        assert!(verify_blob_offer(&me, &frame, &digest, &[], std::slice::from_ref(&me)).is_ok());
        assert!(verify_blob_offer(&me, &frame, &digest, &[], &[]).is_err());
        let undeclared = node::encode_frame(&author, 2, &msg);
        assert!(
            verify_blob_offer(&me, &undeclared, &digest, std::slice::from_ref(&me), &[]).is_err()
        );
        assert!(verify_blob_offer(&me, &frame, &[0; 32], std::slice::from_ref(&me), &[]).is_err());
    }

    #[test]
    fn prerequisite_comes_only_from_signed_metadata_for_any_target() {
        let author = sk(9);
        let digest = [0xAB; 32];
        for target in ["custom-storage", "forge", "files"] {
            let message = sdk::Msg {
                target: target.into(),
                payload: b"opaque payload that is not any native module schema".to_vec(),
            };
            let frame = node::encode_frame_with_blob(&author, 1, &message, Some(digest));
            assert_eq!(required_blob_digest(&frame), Some(digest));
            assert_eq!(
                required_blob_digest(&node::encode_frame(&author, 2, &message)),
                None
            );
            let mut tampered = frame;
            *tampered.last_mut().unwrap() ^= 1;
            assert_eq!(required_blob_digest(&tampered), None);
        }
    }

    #[test]
    fn blob_assembly_is_ordered_bounded_and_digest_checked() {
        let bytes = b"the complete git pack";
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        let mut assembly = BlobAssembly::new(digest, bytes.len() as u64).unwrap();
        assert!(
            assembly
                .push(0, &encode_hex(&bytes[..7]))
                .unwrap()
                .is_none()
        );
        let complete = assembly
            .push(7, &encode_hex(&bytes[7..]))
            .unwrap()
            .expect("complete");
        assert_eq!(complete, bytes);

        let mut wrong_offset = BlobAssembly::new(digest, bytes.len() as u64).unwrap();
        assert!(wrong_offset.push(1, "00").unwrap_err().contains("offset"));
        let mut wrong_digest = BlobAssembly::new([0; 32], bytes.len() as u64).unwrap();
        assert!(
            wrong_digest
                .push(0, &encode_hex(bytes))
                .unwrap_err()
                .contains("digest")
        );
        assert!(BlobAssembly::new(digest, (MAX_RELAY_BLOB_BYTES + 1) as u64).is_err());
    }

    #[test]
    fn largest_blob_chunk_stays_below_the_mesh_message_cap() {
        let msg = RelayMsg::BlobChunk {
            frame_id: [0xFF; 32],
            digest: [0xFF; 32],
            offset: MAX_RELAY_BLOB_BYTES as u64,
            chunk_hex: encode_hex(&vec![0xFF; RELAY_BLOB_CHUNK_BYTES]),
        };
        assert!(
            encode_msg(&msg).len() < 1 << 21,
            "encoded relay chunk must fit the process-wide 2 MiB p2p cap"
        );
    }
}
