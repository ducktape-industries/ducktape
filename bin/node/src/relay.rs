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
//!
//! THE PACK HAS NO SIZE LIMIT, and the chunk transfer is what makes that safe.
//! commonware sizes a channel's inbound mailbox to one burst per peer and
//! DROPS what overruns it rather than blocking the sender, so a pack blasted
//! chunk after chunk arrives with holes and no way to learn of them. Instead a
//! sender keeps at most [`RELAY_BLOB_WINDOW_CHUNKS`] outstanding per target
//! and moves that window on the receiver's [`RelayMsg::BlobAck`]; the receiver
//! appends strictly in order into a [`blobstore::StagedBlob`] — on DISK, never
//! a pack held whole in memory — and answers a chunk that would leave a hole
//! with [`RelayMsg::BlobResend`], the offset to resume from. A drop that
//! swallows a whole window instead shows up as silence, which the sender's
//! own stall timer rewinds. Either way the bytes are re-sent, and the transfer
//! completes only when the file on disk re-hashes to the offered digest.

use serde::{Deserialize, Serialize};

/// 768 KiB raw -> 1.5 MiB hex plus a small JSON envelope, safely below the
/// process-wide 2 MiB commonware message cap.
pub const RELAY_BLOB_CHUNK_BYTES: usize = 768 * 1024;

/// how many chunks one transfer keeps in flight to one target before it waits
/// for that target's [`RelayMsg::BlobAck`].
///
/// THE PACK SIZE IS NOT BOUNDED — this window is what makes that safe.
/// commonware sizes a channel's inbound mailbox to one burst per peer and
/// DROPS an inbound message when it is full (it never blocks a sender), so an
/// unpaced blast of a pack's chunks silently loses whatever overruns the
/// burst. A sender that keeps at most this many chunks outstanding never
/// reaches that boundary: with [`MAX_INCOMING_BLOBS`](crate::relay_runtime)
/// transfers to the same peer at once, the offers plus every in-flight chunk
/// stay well inside one burst, leaving the rest of it for submits and replies.
pub const RELAY_BLOB_WINDOW_CHUNKS: usize = 16;

/// how many appended chunks a receiver takes before it reports its contiguous
/// high-water mark. Small enough that the sender's window never drains
/// waiting for credit, large enough that a transfer is not one ack per chunk.
pub const RELAY_BLOB_ACK_EVERY: usize = 4;

// one window's worth of chunks, plus its offer, plus every other transfer this
// node may have open to the same peer, fits one sender's burst of the inbound
// mailbox — the DROP boundary the window exists to stay under.
const RELAY_MESSAGES_PER_WINDOW: usize = RELAY_BLOB_WINDOW_CHUNKS + 1;
const _: () = assert!(
    RELAY_MESSAGES_PER_WINDOW * crate::relay_runtime::MAX_INCOMING_BLOBS
        < crate::constants::MESH_QUOTA_BURST
);

/// The extra hold a blob transfer earns on top of `SUBMIT_HOLD`,
/// budgeted at a 1 MiB/s floor over the bytes that actually cross the wire:
/// chunks ride hex-encoded (2x), and every target's copy crosses the same
/// uplink, so the budget counts the fan-out width whether the windows are
/// filled one after another or side by side. The base hold alone assumed the
/// pack lands within an app-submit budget — structurally impossible for a
/// repository-sized pack crossing a WAN validator link.
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
    /// The receiver's contiguous high-water mark for one transfer: every byte
    /// below `received_through` is on its disk. This is the transfer's CREDIT
    /// and its REPAIR in one message — it refills the sender's window, and
    /// after a dropped chunk it is the offset the sender rewinds to, because
    /// the receiver appends strictly in order and answers an out-of-order
    /// chunk with this mark instead of taking it.
    BlobAck {
        frame_id: [u8; 32],
        digest: [u8; 32],
        received_through: u64,
    },
    /// The receiver REFUSING a chunk that would leave a hole, naming the
    /// offset the sender must resume from. A receiver appends strictly in
    /// order, so this is what a dropped chunk turns into — a repair, instead
    /// of a pack that quietly completes short. (When the drop swallows the
    /// rest of the window too, the sender's stall timer rewinds it instead:
    /// silence is the other symptom of the same loss.)
    BlobResend {
        frame_id: [u8; 32],
        digest: [u8; 32],
        from: u64,
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

/// What an offered chunk did to a transfer.
#[derive(Debug, PartialEq, Eq)]
pub enum ChunkProgress {
    /// appended in order; `received_through` is the new high-water mark.
    Appended { received_through: u64 },
    /// REFUSED — it would have left a hole. `received_through` is where the
    /// sender has to resume, and nothing was written.
    Gap { received_through: u64 },
    /// refused for the same hole the sender has already been told about:
    /// the rest of a window arriving behind one dropped chunk. Nothing to
    /// write, nothing to say.
    Waiting,
    /// the last byte landed, the file re-hashed to its digest, and the bytes
    /// are published in the store under it.
    Complete,
}

/// Ordered, disk-backed assembly for one accepted blob offer. The bytes stream
/// into the store's staging slot — a pack is never held whole in memory — and
/// [`blobstore::StagedBlob::finish`] re-reads the FILE to verify its length and
/// hash, so nothing is addressable until it verifies.
///
/// Appending is STRICTLY sequential: the staging slot's high-water offset is
/// the only place a chunk may land. That is what makes a dropped chunk
/// impossible to lose silently — the chunk after a gap does not fit, and the
/// receiver answers with the mark the sender must rewind to instead of taking
/// bytes that would leave a hole.
pub struct BlobAssembly {
    /// the live slot, taken at completion: `finish` consumes it because what
    /// it verifies and publishes is the FILE, not this writer's history.
    staged: Option<blobstore::StagedBlob>,
    total: u64,
    chunks_since_ack: usize,
    /// the mark this transfer has ALREADY asked its sender to resume from.
    /// One dropped chunk is followed by the whole window arriving out of
    /// order; without this, each of those would ask for the same repair again
    /// and the sender would re-send the window once per straggler.
    asked_resend_at: Option<u64>,
}

impl BlobAssembly {
    /// open (or RESUME) the staging slot for this offer. A partial file left
    /// by an earlier attempt resumes at its own length, so a retried push
    /// re-sends only what never landed.
    pub fn new(
        blobs: &blobstore::BlobHandle,
        digest: [u8; 32],
        total: u64,
    ) -> Result<Self, String> {
        if total == 0 {
            return Err("relay blob must carry at least one byte".into());
        }
        let staged = blobs
            .stage(digest, total)
            .map_err(|e| format!("cannot stage the required blob: {e}"))?;
        Ok(Self {
            staged: Some(staged),
            total,
            chunks_since_ack: 0,
            asked_resend_at: None,
        })
    }

    /// the contiguous high-water mark: every byte below it is on disk.
    pub fn received_through(&self) -> u64 {
        match &self.staged {
            Some(staged) => staged.offset(),
            None => self.total,
        }
    }

    /// whether the receiver owes its sender a mark — either the ack cadence
    /// came round or the chunk did not fit and the sender must rewind.
    pub fn owes_ack(&self) -> bool {
        self.chunks_since_ack >= RELAY_BLOB_ACK_EVERY
    }

    pub fn ack_sent(&mut self) {
        self.chunks_since_ack = 0;
    }

    /// Append one exact-next chunk. A chunk at any other offset is a
    /// REPAIR SIGNAL, not an error: it leaves the slot untouched and asks for
    /// the mark to be re-sent, which is how the sender learns where the drop
    /// began.
    pub fn push(&mut self, offset: u64, chunk_hex: &str) -> Result<ChunkProgress, String> {
        let Some(staged) = self.staged.as_mut() else {
            return Ok(ChunkProgress::Complete);
        };
        let received_through = staged.offset();
        if offset != received_through {
            // one repair per gap: the rest of the window is still arriving
            // behind this chunk and every frame of it lands here too.
            let already_asked = self.asked_resend_at == Some(received_through);
            if already_asked {
                return Ok(ChunkProgress::Waiting);
            }
            self.asked_resend_at = Some(received_through);
            return Ok(ChunkProgress::Gap { received_through });
        }
        if chunk_hex.len() > RELAY_BLOB_CHUNK_BYTES * 2 {
            return Err("blob chunk exceeds the relay chunk ceiling".into());
        }
        let chunk = decode_hex(chunk_hex)?;
        if chunk.is_empty() {
            return Err("blob chunk must not be empty".into());
        }
        staged
            .append(&chunk)
            .map_err(|e| format!("staging the required blob: {e}"))?;
        self.chunks_since_ack += 1;
        // the hole is filled; the next one earns its own repair.
        self.asked_resend_at = None;
        let received_through = staged.offset();
        if received_through < self.total {
            return Ok(ChunkProgress::Appended { received_through });
        }
        self.staged
            .take()
            .expect("the slot was live one statement ago")
            .finish()
            .map_err(|e| format!("completing the required blob: {e}"))?;
        Ok(ChunkProgress::Complete)
    }

    /// drop the staged bytes — a refused or superseded transfer keeps nothing.
    pub fn abort(self) {
        if let Some(staged) = self.staged {
            staged.abort();
        }
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
    use sha2::{Digest as _, Sha256};

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
            blob_transfer_allowance(127 * 1024 * 1024, 1),
            Duration::from_secs(254),
            "a pack's allowance is the bytes it actually puts on the wire, hex included"
        );
    }

    /// There is no pack this assembly refuses for its SIZE: a push carries
    /// whatever history it carries, and the transfer is windowed and staged to
    /// disk rather than sized to a mailbox. Only an empty offer is nonsense.
    #[test]
    fn the_relay_assembly_takes_a_pack_of_any_size() {
        let blobs = blobstore::BlobHandle::default();
        assert!(BlobAssembly::new(&blobs, [1; 32], 4 * 1024 * 1024 * 1024).is_ok());
        assert!(BlobAssembly::new(&blobs, [2; 32], 127 * 1024 * 1024).is_ok());
        assert!(BlobAssembly::new(&blobs, [3; 32], 0).is_err());
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

    /// ordered, digest-checked, and — the property the whole window protocol
    /// rests on — a chunk that would leave a HOLE is refused with the mark to
    /// resume from, never quietly appended somewhere else.
    #[test]
    fn blob_assembly_is_ordered_repairable_and_digest_checked() {
        let blobs = blobstore::BlobHandle::default();
        let bytes = b"the complete git pack";
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        let mut assembly = BlobAssembly::new(&blobs, digest, bytes.len() as u64).unwrap();
        assert!(matches!(
            assembly.push(0, &encode_hex(&bytes[..7])).unwrap(),
            ChunkProgress::Appended {
                received_through: 7
            }
        ));
        // the chunk that would skip bytes 7..9 names where to resume instead.
        assert!(matches!(
            assembly.push(9, &encode_hex(&bytes[9..])).unwrap(),
            ChunkProgress::Gap {
                received_through: 7
            }
        ));
        assert!(matches!(
            assembly.push(7, &encode_hex(&bytes[7..])).unwrap(),
            ChunkProgress::Complete
        ));
        assert_eq!(blobs.get_chunk(&digest).as_deref(), Some(bytes.as_slice()));

        // bytes that do not hash to the offered digest are never published.
        let lie = [0xAB; 32];
        let mut wrong_digest = BlobAssembly::new(&blobs, lie, bytes.len() as u64).unwrap();
        assert!(
            wrong_digest
                .push(0, &encode_hex(bytes))
                .unwrap_err()
                .contains("hash")
        );
        assert!(blobs.get_chunk(&lie).is_none());
    }

    #[test]
    fn largest_blob_chunk_stays_below_the_mesh_message_cap() {
        let msg = RelayMsg::BlobChunk {
            frame_id: [0xFF; 32],
            digest: [0xFF; 32],
            offset: u64::MAX,
            chunk_hex: encode_hex(&vec![0xFF; RELAY_BLOB_CHUNK_BYTES]),
        };
        assert!(
            encode_msg(&msg).len() < 1 << 21,
            "encoded relay chunk must fit the process-wide 2 MiB p2p cap"
        );
    }
}
