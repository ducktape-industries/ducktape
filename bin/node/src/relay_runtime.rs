//! Runtime state machines for the submit-relay lane.
//!
//! `relay` owns the wire and pure validation. This module owns the mutable
//! off-consensus transfer state: resident fanout, validator fanout, bounded
//! incoming pack assembly, acknowledgements, and timeout cleanup. `main.rs`
//! remains the process orchestrator and only applies the returned submit
//! actions to its `OrderedNode`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Instant, SystemTime};

use commonware_cryptography::ed25519;
use commonware_p2p::{Recipients, Sender as P2pSender};
use commonware_runtime::IoBuf;
use futures::channel::oneshot;
use sdk::Msg;

use crate::constants::SUBMIT_HOLD;
use crate::relay;
use crate::rpc::RpcReply;

const MAX_INCOMING_BLOBS: usize = 4;

pub(crate) enum ResidentHold {
    Rpc(std::sync::mpsc::Sender<RpcReply>),
    Http(oneshot::Sender<Result<noded::BlockSummary, String>>),
}

impl ResidentHold {
    pub(crate) fn fail(self, detail: String) {
        match self {
            Self::Rpc(tx) => {
                let _ = tx.send(RpcReply::err(detail));
            }
            Self::Http(tx) => {
                let _ = tx.send(Err(detail));
            }
        }
    }
}

struct ResidentFanout {
    hold: ResidentHold,
    frame: Vec<u8>,
    digest: [u8; 32],
    awaiting: Vec<ed25519::PublicKey>,
    custodian: ed25519::PublicKey,
    deadline: Instant,
}

pub(crate) struct ResidentRelay {
    blobs: std::sync::Arc<dyn blobstore::Blobs>,
    seq_file: PathBuf,
    seq: u64,
    round: usize,
    pending: HashMap<node::FrameId, (ResidentHold, Instant)>,
    fanouts: HashMap<node::FrameId, ResidentFanout>,
}

impl ResidentRelay {
    pub(crate) fn new(seq_file: PathBuf, blobs: std::sync::Arc<dyn blobstore::Blobs>) -> Self {
        let seq = std::fs::read_to_string(&seq_file)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        Self {
            blobs,
            seq_file,
            seq,
            round: 0,
            pending: HashMap::new(),
            fanouts: HashMap::new(),
        }
    }

    pub(crate) fn submit<S>(
        &mut self,
        signer: &ed25519::PrivateKey,
        targets: &[ed25519::PublicKey],
        relay_tx: &mut S,
        target: String,
        payload: Vec<u8>,
        hold: ResidentHold,
    ) -> Result<node::FrameId, (ResidentHold, String)>
    where
        S: P2pSender<PublicKey = ed25519::PublicKey>,
    {
        self.submit_with_blob(signer, targets, relay_tx, target, payload, None, hold)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn submit_with_blob<S>(
        &mut self,
        signer: &ed25519::PrivateKey,
        targets: &[ed25519::PublicKey],
        relay_tx: &mut S,
        target: String,
        payload: Vec<u8>,
        required_blob: Option<[u8; 32]>,
        hold: ResidentHold,
    ) -> Result<node::FrameId, (ResidentHold, String)>
    where
        S: P2pSender<PublicKey = ed25519::PublicKey>,
    {
        let (frame_id, frame, custodian) =
            match self.signed_frame(signer, targets, target, payload, required_blob) {
                Ok(prepared) => prepared,
                Err(detail) => return Err((hold, detail)),
            };
        self.relay_frame(frame_id, frame, custodian, targets, relay_tx, hold)
    }

    /// Relay an ALREADY-SIGNED frame. The frame is not this node's: an agent's
    /// per-run session key signed it and the resident is only the courier, so
    /// nothing here re-signs or re-originates it — `RelayMsg::Submit` carries
    /// frames, not msgs, and the validator verifies the signature before it
    /// pins. The resident's own `seq` counter is untouched too: the frame
    /// carries the SIGNER's seq, not the courier's.
    pub(crate) fn submit_frame<S>(
        &mut self,
        frame: Vec<u8>,
        targets: &[ed25519::PublicKey],
        relay_tx: &mut S,
        hold: ResidentHold,
    ) -> Result<node::FrameId, (ResidentHold, String)>
    where
        S: P2pSender<PublicKey = ed25519::PublicKey>,
    {
        let custodian = match self.custodian(targets) {
            Ok(custodian) => custodian,
            Err(detail) => return Err((hold, detail)),
        };
        let frame_id = node::frame_id(&frame);
        self.relay_frame(frame_id, frame, custodian, targets, relay_tx, hold)
    }

    /// The shared relay tail both submit paths take: fan a required blob out to
    /// every target first when the frame needs one, otherwise hand the frame
    /// straight to the custodian and hold the caller's reply against its id.
    fn relay_frame<S>(
        &mut self,
        frame_id: node::FrameId,
        frame: Vec<u8>,
        custodian: ed25519::PublicKey,
        targets: &[ed25519::PublicKey],
        relay_tx: &mut S,
        hold: ResidentHold,
    ) -> Result<node::FrameId, (ResidentHold, String)>
    where
        S: P2pSender<PublicKey = ed25519::PublicKey>,
    {
        let deadline = Instant::now() + SUBMIT_HOLD;
        if let Some(digest) = relay::required_blob_digest(&frame) {
            let Some(bytes) = self.blobs.get_chunk(&digest) else {
                return Err((
                    hold,
                    "required blob referenced by the submit is not in this node's blob store"
                        .into(),
                ));
            };
            if let Err(detail) = send_blob(relay_tx, targets, &frame, digest, &bytes) {
                return Err((hold, detail));
            }
            // the pack has to cross the wire before anyone can ack it — the
            // fanout hold earns the transfer allowance on top of the base,
            // sized by the serial fan-out width.
            let deadline =
                deadline + relay::blob_transfer_allowance(bytes.len() as u64, targets.len());
            self.fanouts.insert(
                frame_id,
                ResidentFanout {
                    hold,
                    frame,
                    digest,
                    awaiting: targets.to_vec(),
                    custodian,
                    deadline,
                },
            );
            return Ok(frame_id);
        }

        if !send(relay_tx, &custodian, relay::RelayMsg::Submit { frame }) {
            return Err((hold, "validator unreachable - retry shortly".into()));
        }
        self.pending.insert(frame_id, (hold, deadline));
        Ok(frame_id)
    }

    /// Handle validator acknowledgements and final outcomes. An unclaimed
    /// final reply belongs to nobody now that the resident announce pump is
    /// gone; it is still returned, and still routed for its release side
    /// effect.
    pub(crate) fn on_message<S>(
        &mut self,
        peer: ed25519::PublicKey,
        msg: relay::RelayMsg,
        relay_tx: &mut S,
    ) -> Option<(node::FrameId, relay::RelayOutcome)>
    where
        S: P2pSender<PublicKey = ed25519::PublicKey>,
    {
        match msg {
            relay::RelayMsg::BlobResult {
                frame_id,
                digest,
                error,
            } => {
                let fanout = self.fanouts.get_mut(&frame_id)?;
                if fanout.digest != digest {
                    return None;
                }
                let pos = fanout
                    .awaiting
                    .iter()
                    .position(|candidate| candidate == &peer)?;
                if let Some(detail) = error {
                    let fanout = self.fanouts.remove(&frame_id).expect("fanout exists");
                    fanout.hold.fail(detail);
                    return None;
                }
                fanout.awaiting.swap_remove(pos);
                if !fanout.awaiting.is_empty() {
                    return None;
                }
                let fanout = self
                    .fanouts
                    .remove(&frame_id)
                    .expect("completed fanout exists");
                if !send(
                    relay_tx,
                    &fanout.custodian,
                    relay::RelayMsg::Submit {
                        frame: fanout.frame,
                    },
                ) {
                    fanout
                        .hold
                        .fail("validator unreachable after required blob fanout".into());
                } else {
                    self.pending
                        .insert(frame_id, (fanout.hold, fanout.deadline));
                }
                None
            }
            relay::RelayMsg::Reply { frame_id, outcome } => {
                let Some((hold, _)) = self.pending.remove(&frame_id) else {
                    return Some((frame_id, outcome));
                };
                resolve_resident_hold(hold, outcome);
                None
            }
            _ => None,
        }
    }

    pub(crate) fn expire(&mut self, now: Instant) {
        let expired_fanouts: Vec<_> = self
            .fanouts
            .iter()
            .filter(|(_, fanout)| fanout.deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired_fanouts {
            if let Some(fanout) = self.fanouts.remove(&id) {
                // once per failed push, and the QA daemon.log for a failed
                // push was otherwise EMPTY — this line is the server-side
                // evidence.
                tracing::warn!(
                    target: "ducktape::submit",
                    digest = %relay::encode_hex(&fanout.digest),
                    awaiting = fanout.awaiting.len(),
                    reason = "blob_fanout_expired",
                    "required blob fanout expired before every validator acked; the push fails and can be retried"
                );
                fanout
                    .hold
                    .fail("timed out distributing the required blob to validators".into());
            }
        }

        let expired_pending: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, (_, deadline))| *deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired_pending {
            if let Some((hold, _)) = self.pending.remove(&id) {
                hold.fail(
                    "timed out awaiting the relay answer - re-query on the next block".into(),
                );
            }
        }
    }

    fn signed_frame(
        &mut self,
        signer: &ed25519::PrivateKey,
        targets: &[ed25519::PublicKey],
        target: String,
        payload: Vec<u8>,
        required_blob: Option<[u8; 32]>,
    ) -> Result<(node::FrameId, Vec<u8>, ed25519::PublicKey), String> {
        let custodian = self.custodian(targets)?;
        self.seq += 1;
        std::fs::write(&self.seq_file, self.seq.to_string())
            .map_err(|e| format!("cannot persist the submit seq: {e}"))?;
        let frame =
            node::encode_frame_with_blob(signer, self.seq, &Msg { target, payload }, required_blob);
        let frame_id = node::frame_id(&frame);
        Ok((frame_id, frame, custodian))
    }

    /// The validator that takes custody of the next relayed frame: round-robin
    /// over the announce targets, so a resident's submit stream never leans on
    /// one validator. No target means no relay is possible at all.
    fn custodian(&mut self, targets: &[ed25519::PublicKey]) -> Result<ed25519::PublicKey, String> {
        if targets.is_empty() {
            return Err("no validator known yet - the manifest poll has not landed".into());
        }
        let custodian = targets[self.round % targets.len()].clone();
        self.round += 1;
        Ok(custodian)
    }
}

fn resolve_resident_hold(hold: ResidentHold, outcome: relay::RelayOutcome) {
    match (hold, outcome) {
        (ResidentHold::Rpc(tx), relay::RelayOutcome::Applied { .. }) => {
            let _ = tx.send(RpcReply::ok());
        }
        (ResidentHold::Rpc(tx), relay::RelayOutcome::Rejected { detail })
        | (ResidentHold::Rpc(tx), relay::RelayOutcome::Refused { detail }) => {
            let _ = tx.send(RpcReply::err(detail));
        }
        (ResidentHold::Http(tx), relay::RelayOutcome::Applied { height, root_hash }) => {
            let _ = tx.send(Ok(noded::BlockSummary { height, root_hash }));
        }
        (ResidentHold::Http(tx), relay::RelayOutcome::Rejected { detail })
        | (ResidentHold::Http(tx), relay::RelayOutcome::Refused { detail }) => {
            let _ = tx.send(Err(detail));
        }
    }
}

type HttpReply = oneshot::Sender<Result<noded::BlockSummary, String>>;

struct LocalFanout {
    reply: HttpReply,
    frame: Vec<u8>,
    digest: [u8; 32],
    awaiting: Vec<ed25519::PublicKey>,
    deadline: SystemTime,
}

struct IncomingBlob {
    peer: ed25519::PublicKey,
    digest: [u8; 32],
    assembly: relay::BlobAssembly,
    deadline: SystemTime,
}

pub(crate) enum ValidatorAction {
    SubmitResident {
        frame_id: node::FrameId,
        frame: Vec<u8>,
        peer: ed25519::PublicKey,
    },
    SubmitLocal {
        frame_id: node::FrameId,
        frame: Vec<u8>,
        reply: HttpReply,
        deadline: SystemTime,
    },
}

pub(crate) struct ValidatorRelay {
    blobs: std::sync::Arc<dyn blobstore::Blobs>,
    local_fanouts: HashMap<node::FrameId, LocalFanout>,
    incoming: HashMap<node::FrameId, IncomingBlob>,
}

impl ValidatorRelay {
    pub(crate) fn new(blobs: std::sync::Arc<dyn blobstore::Blobs>) -> Self {
        Self {
            blobs,
            local_fanouts: HashMap::new(),
            incoming: HashMap::new(),
        }
    }

    /// Prepare a validator-local app submit. Ordinary ops and single-validator
    /// blob-dependent submits return immediately; multi-validator blob-dependent submits remain
    /// pending until every peer acknowledges the pack.
    pub(crate) fn prepare_local<S>(
        &mut self,
        now: SystemTime,
        frame: Vec<u8>,
        reply: HttpReply,
        peers: Vec<ed25519::PublicKey>,
        relay_tx: &mut S,
    ) -> Result<Option<ValidatorAction>, (HttpReply, String)>
    where
        S: P2pSender<PublicKey = ed25519::PublicKey>,
    {
        let frame_id = node::frame_id(&frame);
        let deadline = now + SUBMIT_HOLD;
        let Some(digest) = relay::required_blob_digest(&frame) else {
            return Ok(Some(ValidatorAction::SubmitLocal {
                frame_id,
                frame,
                reply,
                deadline,
            }));
        };
        let Some(pack) = self.blobs.get_chunk(&digest) else {
            return Err((
                reply,
                "required blob referenced by the submit is not in this validator's blob store"
                    .into(),
            ));
        };
        if peers.is_empty() {
            return Ok(Some(ValidatorAction::SubmitLocal {
                frame_id,
                frame,
                reply,
                deadline,
            }));
        }
        if let Err(detail) = send_blob(relay_tx, &peers, &frame, digest, &pack) {
            return Err((reply, detail));
        }
        // the pack has to cross the wire before every peer can ack it — the
        // fanout hold earns the transfer allowance on top of the base, sized
        // by the serial fan-out width.
        let deadline = deadline + relay::blob_transfer_allowance(pack.len() as u64, peers.len());
        self.local_fanouts.insert(
            frame_id,
            LocalFanout {
                reply,
                frame,
                digest,
                awaiting: peers,
                deadline,
            },
        );
        Ok(None)
    }

    pub(crate) fn on_message<S>(
        &mut self,
        now: SystemTime,
        peer: ed25519::PublicKey,
        msg: relay::RelayMsg,
        members: &[Vec<u8>],
        residents: &[Vec<u8>],
        relay_tx: &mut S,
    ) -> Option<ValidatorAction>
    where
        S: P2pSender<PublicKey = ed25519::PublicKey>,
    {
        match msg {
            relay::RelayMsg::BlobOffer {
                frame,
                digest,
                total,
            } => {
                let frame_id = node::frame_id(&frame);
                if let Err(detail) =
                    relay::verify_blob_offer(peer.as_ref(), &frame, &digest, members, residents)
                {
                    send_blob_result(relay_tx, &peer, frame_id, digest, Some(detail));
                    return None;
                }
                if self.blobs.has_chunk(&digest) {
                    send_blob_result(relay_tx, &peer, frame_id, digest, None);
                    return None;
                }
                if self.incoming.len() >= MAX_INCOMING_BLOBS
                    && !self.incoming.contains_key(&frame_id)
                {
                    send_blob_result(
                        relay_tx,
                        &peer,
                        frame_id,
                        digest,
                        Some("too many concurrent required blob transfers".into()),
                    );
                    return None;
                }
                match relay::BlobAssembly::new(digest, total) {
                    Ok(assembly) => {
                        // the chunks are still crossing the wire — the
                        // assembly hold earns the transfer allowance on top
                        // of the base, sized by the offered total and the
                        // sender's serial fan-out (this node may be its last
                        // target, so the whole membership is the width).
                        let deadline = now
                            + SUBMIT_HOLD
                            + relay::blob_transfer_allowance(total, members.len());
                        self.incoming.insert(
                            frame_id,
                            IncomingBlob {
                                peer,
                                digest,
                                assembly,
                                deadline,
                            },
                        );
                    }
                    Err(detail) => {
                        send_blob_result(relay_tx, &peer, frame_id, digest, Some(detail));
                    }
                }
                None
            }
            relay::RelayMsg::BlobChunk {
                frame_id,
                digest,
                offset,
                chunk_hex,
            } => {
                let progress = {
                    let incoming = self.incoming.get_mut(&frame_id)?;
                    if incoming.peer != peer || incoming.digest != digest {
                        return None;
                    }
                    incoming.assembly.push(offset, &chunk_hex)
                };
                match progress {
                    Ok(None) => {}
                    Ok(Some(pack)) => {
                        self.incoming.remove(&frame_id);
                        let stored = self.blobs.put_chunk(pack);
                        debug_assert_eq!(stored, digest);
                        send_blob_result(relay_tx, &peer, frame_id, digest, None);
                    }
                    Err(detail) => {
                        self.incoming.remove(&frame_id);
                        send_blob_result(relay_tx, &peer, frame_id, digest, Some(detail));
                    }
                }
                None
            }
            relay::RelayMsg::BlobResult {
                frame_id,
                digest,
                error,
            } => {
                let fanout = self.local_fanouts.get_mut(&frame_id)?;
                if fanout.digest != digest {
                    return None;
                }
                let pos = fanout
                    .awaiting
                    .iter()
                    .position(|candidate| candidate == &peer)?;
                if let Some(detail) = error {
                    let fanout = self
                        .local_fanouts
                        .remove(&frame_id)
                        .expect("local fanout exists");
                    let _ = fanout.reply.send(Err(detail));
                    return None;
                }
                fanout.awaiting.swap_remove(pos);
                if !fanout.awaiting.is_empty() {
                    return None;
                }
                let fanout = self
                    .local_fanouts
                    .remove(&frame_id)
                    .expect("completed local fanout exists");
                Some(ValidatorAction::SubmitLocal {
                    frame_id,
                    frame: fanout.frame,
                    reply: fanout.reply,
                    deadline: fanout.deadline,
                })
            }
            relay::RelayMsg::Submit { frame } => {
                let frame_id =
                    match relay::verify_relay_submit(&frame, peer.as_ref(), members, residents) {
                        Ok(id) => id,
                        Err(detail) => {
                            send_reply(
                                relay_tx,
                                &peer,
                                node::frame_id(&frame),
                                relay::RelayOutcome::Refused { detail },
                            );
                            return None;
                        }
                    };
                if let Some(digest) = relay::required_blob_digest(&frame)
                    && !self.blobs.has_chunk(&digest)
                {
                    send_reply(
                        relay_tx,
                        &peer,
                        frame_id,
                        relay::RelayOutcome::Refused {
                            detail: "required blob was not prepared on this validator".into(),
                        },
                    );
                    return None;
                }
                Some(ValidatorAction::SubmitResident {
                    frame_id,
                    frame,
                    peer,
                })
            }
            relay::RelayMsg::Reply { .. } => None,
            // dispatched in `on_relay` BEFORE the protocol machine (it carries
            // no frame and touches no relay state) — never reaches here.
            relay::RelayMsg::Nudge => None,
        }
    }

    pub(crate) fn expire<S>(&mut self, now: SystemTime, relay_tx: &mut S)
    where
        S: P2pSender<PublicKey = ed25519::PublicKey>,
    {
        let expired_local: Vec<_> = self
            .local_fanouts
            .iter()
            .filter(|(_, fanout)| fanout.deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired_local {
            if let Some(fanout) = self.local_fanouts.remove(&id) {
                tracing::warn!(
                    target: "ducktape::submit",
                    digest = %relay::encode_hex(&fanout.digest),
                    awaiting = fanout.awaiting.len(),
                    reason = "blob_fanout_expired",
                    "required blob fanout expired before every peer acked; the push fails and can be retried"
                );
                let _ = fanout.reply.send(Err(
                    "timed out distributing the required blob to validators".into(),
                ));
            }
        }

        let expired_incoming: Vec<_> = self
            .incoming
            .iter()
            .filter(|(_, incoming)| incoming.deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired_incoming {
            if let Some(incoming) = self.incoming.remove(&id) {
                // chunk loss, not slowness, is the usual cause here: chunks
                // ride the p2p channel with no retransmit, so a tunnel that
                // drops mid-transfer can never complete this assembly.
                tracing::warn!(
                    target: "ducktape::submit",
                    digest = %relay::encode_hex(&incoming.digest),
                    reason = "blob_receive_expired",
                    "required blob receive expired mid-transfer; refusing so the pusher sees the timeout"
                );
                send_blob_result(
                    relay_tx,
                    &incoming.peer,
                    id,
                    incoming.digest,
                    Some("timed out receiving the required blob".into()),
                );
            }
        }
    }
}

fn send_blob<S>(
    relay_tx: &mut S,
    targets: &[ed25519::PublicKey],
    frame: &[u8],
    digest: [u8; 32],
    bytes: &[u8],
) -> Result<(), String>
where
    S: P2pSender<PublicKey = ed25519::PublicKey>,
{
    if bytes.is_empty() || bytes.len() > relay::MAX_RELAY_BLOB_BYTES {
        return Err(format!(
            "required blob must be 1..={} bytes for node relay, got {}",
            relay::MAX_RELAY_BLOB_BYTES,
            bytes.len()
        ));
    }
    let frame_id = node::frame_id(frame);
    for target in targets {
        if !send(
            relay_tx,
            target,
            relay::RelayMsg::BlobOffer {
                frame: frame.to_vec(),
                digest,
                total: bytes.len() as u64,
            },
        ) {
            return Err(format!(
                "validator {target} unreachable during required blob offer"
            ));
        }
        for (index, chunk) in bytes.chunks(relay::RELAY_BLOB_CHUNK_BYTES).enumerate() {
            if !send(
                relay_tx,
                target,
                relay::RelayMsg::BlobChunk {
                    frame_id,
                    digest,
                    offset: (index * relay::RELAY_BLOB_CHUNK_BYTES) as u64,
                    chunk_hex: relay::encode_hex(chunk),
                },
            ) {
                return Err(format!(
                    "validator {target} unreachable during required blob transfer"
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn send_reply<S>(
    relay_tx: &mut S,
    peer: &ed25519::PublicKey,
    frame_id: node::FrameId,
    outcome: relay::RelayOutcome,
) where
    S: P2pSender<PublicKey = ed25519::PublicKey>,
{
    let _ = send(relay_tx, peer, relay::RelayMsg::Reply { frame_id, outcome });
}

fn send_blob_result<S>(
    relay_tx: &mut S,
    peer: &ed25519::PublicKey,
    frame_id: node::FrameId,
    digest: [u8; 32],
    error: Option<String>,
) where
    S: P2pSender<PublicKey = ed25519::PublicKey>,
{
    let _ = send(
        relay_tx,
        peer,
        relay::RelayMsg::BlobResult {
            frame_id,
            digest,
            error,
        },
    );
}

/// fire a leader nudge at `peer` — best-effort, no reply expected: a lost or
/// mis-aimed nudge costs at most one idle beat of latency, never correctness.
pub(crate) fn send_nudge<S>(relay_tx: &mut S, peer: &ed25519::PublicKey)
where
    S: P2pSender<PublicKey = ed25519::PublicKey>,
{
    let _ = send(relay_tx, peer, relay::RelayMsg::Nudge);
}

fn send<S>(relay_tx: &mut S, peer: &ed25519::PublicKey, msg: relay::RelayMsg) -> bool
where
    S: P2pSender<PublicKey = ed25519::PublicKey>,
{
    !relay_tx
        .send(
            Recipients::One(peer.clone()),
            IoBuf::from(relay::encode_msg(&msg)),
            false,
        )
        .is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest as _, Sha256};

    #[derive(Clone, Default)]
    struct RecordingSender(std::sync::Arc<std::sync::Mutex<Vec<relay::RelayMsg>>>);

    struct CheckedRecording {
        sender: RecordingSender,
        peers: Vec<ed25519::PublicKey>,
    }

    impl commonware_p2p::LimitedSender for RecordingSender {
        type PublicKey = ed25519::PublicKey;
        type Checked<'a> = CheckedRecording;

        fn check(
            &mut self,
            recipients: Recipients<Self::PublicKey>,
        ) -> Result<Self::Checked<'_>, SystemTime> {
            let peers = match recipients {
                Recipients::One(peer) => vec![peer],
                Recipients::Some(peers) => peers,
                Recipients::All => panic!("test must name its peers"),
            };
            Ok(CheckedRecording {
                sender: self.clone(),
                peers,
            })
        }
    }

    impl commonware_p2p::CheckedSender for CheckedRecording {
        type PublicKey = ed25519::PublicKey;
        fn recipients(&self) -> Vec<Self::PublicKey> {
            self.peers.clone()
        }
        fn send(
            self,
            message: impl Into<commonware_runtime::IoBufs> + Send,
            _: bool,
        ) -> commonware_actor::Unreliable<commonware_actor::Feedback> {
            let bytes = message.into().coalesce();
            self.sender
                .0
                .lock()
                .unwrap()
                .push(relay::decode_msg(bytes.as_ref()).unwrap());
            commonware_actor::Unreliable::new(commonware_actor::Feedback::Ok)
        }
    }

    #[test]
    fn arbitrary_module_blob_precedes_admission_and_every_peer_ack() {
        use commonware_cryptography::Signer as _;
        let author = ed25519::PrivateKey::from_seed(101);
        let peers = [102, 103].map(|seed| ed25519::PrivateKey::from_seed(seed).public_key());
        let outsider = ed25519::PrivateKey::from_seed(104).public_key();
        let bytes = b"opaque application storage".to_vec();
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        let frame = node::encode_frame_with_blob(
            &author,
            1,
            &Msg {
                target: "new-storage-module".into(),
                payload: b"arbitrary op".to_vec(),
            },
            Some(digest),
        );
        let frame_id = node::frame_id(&frame);
        let blobs = std::sync::Arc::new(blobstore::BlobHandle::default());
        let mut validator = ValidatorRelay::new(blobs.clone());
        let mut sender = RecordingSender::default();
        let now = SystemTime::UNIX_EPOCH;
        let (reply, _) = oneshot::channel();
        let missing =
            validator.prepare_local(now, frame.clone(), reply, peers.to_vec(), &mut sender);
        assert!(
            matches!(missing, Err((_, detail)) if detail.contains("not in this validator's blob store"))
        );
        assert!(sender.0.lock().unwrap().is_empty());

        // Remote custody also refuses a valid frame before its bytes arrive.
        let standing = vec![peers[0].as_ref().to_vec()];
        assert!(
            validator
                .on_message(
                    now,
                    peers[0].clone(),
                    relay::RelayMsg::Submit {
                        frame: frame.clone()
                    },
                    &standing,
                    &[],
                    &mut sender
                )
                .is_none()
        );
        assert!(matches!(
            sender.0.lock().unwrap().last(),
            Some(relay::RelayMsg::Reply {
                outcome: relay::RelayOutcome::Refused { .. },
                ..
            })
        ));
        assert_eq!(blobs.put_chunk(bytes), digest);
        let (reply, _) = oneshot::channel();
        assert!(matches!(
            validator.prepare_local(now, frame.clone(), reply, peers.to_vec(), &mut sender),
            Ok(None)
        ));
        assert_eq!(validator.local_fanouts.len(), 1);
        let acknowledgement = |digest| relay::RelayMsg::BlobResult {
            frame_id,
            digest,
            error: None,
        };
        for (peer, acknowledged_digest) in [
            (outsider, digest),
            (peers[0].clone(), [0; 32]),
            (peers[0].clone(), digest),
            (peers[0].clone(), digest),
        ] {
            assert!(
                validator
                    .on_message(
                        now,
                        peer,
                        acknowledgement(acknowledged_digest),
                        &[],
                        &[],
                        &mut sender
                    )
                    .is_none()
            );
        }
        let action = validator.on_message(
            now,
            peers[1].clone(),
            acknowledgement(digest),
            &[],
            &[],
            &mut sender,
        );
        assert!(
            matches!(action, Some(ValidatorAction::SubmitLocal { frame: admitted, .. }) if admitted == frame)
        );
        assert!(validator.local_fanouts.is_empty());
    }

    #[test]
    fn blob_digest_is_content_addressed() {
        let pack = b"PACK-test";
        let digest: [u8; 32] = Sha256::digest(pack).into();
        let mut assembly = relay::BlobAssembly::new(digest, pack.len() as u64).unwrap();
        assert_eq!(
            assembly
                .push(0, &relay::encode_hex(pack))
                .unwrap()
                .as_deref(),
            Some(pack.as_slice())
        );
    }

    #[test]
    fn seq_file_is_read_without_mutating_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay-submit-seq");
        std::fs::write(&path, "41").unwrap();
        let relay = ResidentRelay::new(
            path.clone(),
            std::sync::Arc::new(blobstore::BlobHandle::default()),
        );
        assert_eq!(relay.seq, 41);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "41");
    }
}
