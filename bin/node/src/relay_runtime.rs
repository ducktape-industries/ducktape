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

pub(crate) const MAX_INCOMING_BLOBS: usize = 4;

/// how long a target may leave the window unmoved before the sender assumes
/// the chunks it holds outstanding were DROPPED and rewinds to that target's
/// last acknowledged mark. commonware drops an inbound message into a full
/// mailbox without telling either end, so silence is the only symptom a
/// sender ever gets.
const BLOB_WINDOW_STALL: std::time::Duration = std::time::Duration::from_secs(10);

/// how many CONSECUTIVE stalls one target earns before the transfer is called
/// off — the counter resets whenever bytes land. Twenty windows in a row lost
/// with nothing acknowledged in between is a link that is down, not slow, and
/// the push says so instead of holding its caller until the hold expires.
const BLOB_WINDOW_MAX_REWINDS: u32 = 20;

pub(crate) enum ResidentHold {
    Rpc(std::sync::mpsc::Sender<RpcReply>),
    Http(oneshot::Sender<Result<noded::BlockSummary, noded::Refused>>),
}

impl ResidentHold {
    pub(crate) fn fail(self, refused: noded::Refused) {
        match self {
            // the local rpc lane prints to a terminal: a person there wants the
            // sentence, and the token is already in the node's own log line.
            Self::Rpc(tx) => {
                let _ = tx.send(RpcReply::err(refused.message));
            }
            Self::Http(tx) => {
                let _ = tx.send(Err(refused));
            }
        }
    }
}

struct ResidentFanout {
    hold: ResidentHold,
    frame: Vec<u8>,
    transfer: BlobFanout,
    custodian: ed25519::PublicKey,
    deadline: Instant,
}

pub(crate) struct ResidentRelay {
    blobs: blobstore::BlobHandle,
    seq_file: PathBuf,
    seq: u64,
    round: usize,
    pending: HashMap<node::FrameId, (ResidentHold, Instant)>,
    fanouts: HashMap<node::FrameId, ResidentFanout>,
}

impl ResidentRelay {
    pub(crate) fn new(seq_file: PathBuf, blobs: blobstore::BlobHandle) -> Self {
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
        let now = Instant::now();
        let deadline = now + SUBMIT_HOLD;
        if let Some(digest) = relay::required_blob_digest(&frame) {
            let transfer =
                match BlobFanout::open(&self.blobs, relay_tx, targets, &frame, digest, now) {
                    Ok(transfer) => transfer,
                    Err(detail) => return Err((hold, detail)),
                };
            // the pack has to cross the wire before anyone can ack it — the
            // fanout hold earns the transfer allowance on top of the base,
            // sized by the fan-out width.
            let deadline = deadline + relay::blob_transfer_allowance(transfer.total, targets.len());
            self.fanouts.insert(
                frame_id,
                ResidentFanout {
                    hold,
                    frame,
                    transfer,
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
            relay::RelayMsg::BlobAck {
                frame_id,
                digest,
                received_through,
            } => {
                let fanout = self.fanouts.get_mut(&frame_id)?;
                if let Err(detail) = fanout.transfer.on_ack(
                    &self.blobs,
                    relay_tx,
                    &peer,
                    &digest,
                    received_through,
                    Instant::now(),
                ) {
                    let fanout = self.fanouts.remove(&frame_id).expect("fanout exists");
                    fanout.hold.fail(noded::Refused::new("blob_fanout", detail));
                }
                None
            }
            relay::RelayMsg::BlobResend {
                frame_id,
                digest,
                from,
            } => {
                let fanout = self.fanouts.get_mut(&frame_id)?;
                if let Err(detail) = fanout.transfer.on_resend(
                    &self.blobs,
                    relay_tx,
                    &peer,
                    &digest,
                    from,
                    Instant::now(),
                ) {
                    let fanout = self.fanouts.remove(&frame_id).expect("fanout exists");
                    fanout.hold.fail(noded::Refused::new("blob_fanout", detail));
                }
                None
            }
            relay::RelayMsg::BlobResult {
                frame_id,
                digest,
                error,
            } => {
                let fanout = self.fanouts.get_mut(&frame_id)?;
                if fanout.transfer.digest != digest || !fanout.transfer.holds(&peer) {
                    return None;
                }
                if let Some(detail) = error {
                    let fanout = self.fanouts.remove(&frame_id).expect("fanout exists");
                    fanout.hold.fail(noded::Refused::new("blob_fanout", detail));
                    return None;
                }
                if !fanout.transfer.on_complete(&peer) {
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
                    fanout.hold.fail(noded::Refused::new(
                        "validator_unreachable",
                        "validator unreachable after required blob fanout",
                    ));
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

    /// drive every open transfer and reap what ran out of time. Called on the
    /// loop's tick, which is what makes a dropped window heal: nothing else
    /// wakes a transfer whose chunks all vanished.
    pub(crate) fn expire<S>(&mut self, now: Instant, relay_tx: &mut S)
    where
        S: P2pSender<PublicKey = ed25519::PublicKey>,
    {
        let stalled: Vec<_> = self
            .fanouts
            .iter_mut()
            .filter_map(|(id, fanout)| {
                let repaired = fanout.transfer.repair_stalled(&self.blobs, relay_tx, now);
                repaired.err().map(|detail| (*id, detail))
            })
            .collect();
        for (id, detail) in stalled {
            if let Some(fanout) = self.fanouts.remove(&id) {
                fanout.hold.fail(noded::Refused::new("blob_fanout", detail));
            }
        }

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
                    digest = %relay::encode_hex(&fanout.transfer.digest),
                    awaiting = fanout.transfer.awaiting(),
                    reason = "blob_fanout_expired",
                    "required blob fanout expired before every validator acked; the push fails and can be retried"
                );
                fanout.hold.fail(noded::Refused::new(
                    "blob_fanout_timeout",
                    "timed out distributing the required blob to validators",
                ));
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
                hold.fail(noded::Refused::new(
                    "relay_timeout",
                    "timed out awaiting the relay answer - re-query on the next block",
                ));
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
        // two outcomes, two tokens: the custodian's consensus REJECTED the op
        // (the module's own answer, relayed) or REFUSED to take it at all (the
        // courier lane's), and a caller that must tell them apart no longer has
        // to read the sentence to do it.
        (ResidentHold::Http(tx), relay::RelayOutcome::Rejected { detail }) => {
            // the custodian relayed the refusal FRAMED: split it so this
            // caller sees the refusing module's own token, not a stamp that
            // only says a module was involved.
            let _ = tx.send(Err(noded::Refused::framed(&detail)));
        }
        (ResidentHold::Http(tx), relay::RelayOutcome::Refused { detail }) => {
            let _ = tx.send(Err(noded::Refused::new("relay_refused", detail)));
        }
    }
}

type HttpReply = oneshot::Sender<Result<noded::BlockSummary, noded::Refused>>;

struct LocalFanout {
    reply: HttpReply,
    frame: Vec<u8>,
    transfer: BlobFanout,
    deadline: SystemTime,
}

struct IncomingBlob {
    peer: ed25519::PublicKey,
    digest: [u8; 32],
    assembly: relay::BlobAssembly,
    deadline: SystemTime,
    /// how many holes this receiver asked its sender to re-send from. It is
    /// the ONLY server-side number that separates "the mesh dropped chunks
    /// and the window repaired them" from "nothing was ever dropped", and a
    /// completed transfer carries it into the log.
    repairs: u32,
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
    blobs: blobstore::BlobHandle,
    local_fanouts: HashMap<node::FrameId, LocalFanout>,
    incoming: HashMap<node::FrameId, IncomingBlob>,
}

impl ValidatorRelay {
    pub(crate) fn new(blobs: blobstore::BlobHandle) -> Self {
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
        if !self.blobs.has_chunk(&digest) {
            return Err((
                reply,
                "required blob referenced by the submit is not in this validator's blob store"
                    .into(),
            ));
        }
        if peers.is_empty() {
            return Ok(Some(ValidatorAction::SubmitLocal {
                frame_id,
                frame,
                reply,
                deadline,
            }));
        }
        let transfer = match BlobFanout::open(
            &self.blobs,
            relay_tx,
            &peers,
            &frame,
            digest,
            Instant::now(),
        ) {
            Ok(transfer) => transfer,
            Err(detail) => return Err((reply, detail)),
        };
        // the pack has to cross the wire before every peer can ack it — the
        // fanout hold earns the transfer allowance on top of the base, sized
        // by the fan-out width.
        let deadline = deadline + relay::blob_transfer_allowance(transfer.total, peers.len());
        self.local_fanouts.insert(
            frame_id,
            LocalFanout {
                reply,
                frame,
                transfer,
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
                match relay::BlobAssembly::new(&self.blobs, digest, total) {
                    Ok(assembly) => {
                        // the chunks are still crossing the wire — the
                        // assembly hold earns the transfer allowance on top
                        // of the base, sized by the offered total and the
                        // sender's fan-out (this node may be its last target,
                        // so the whole membership is the width).
                        let deadline = now
                            + SUBMIT_HOLD
                            + relay::blob_transfer_allowance(total, members.len());
                        // a partial from an earlier attempt resumes where it
                        // stopped, so tell the sender where that is before it
                        // re-sends a prefix this node already holds.
                        let resume_from = assembly.received_through();
                        self.incoming.insert(
                            frame_id,
                            IncomingBlob {
                                peer: peer.clone(),
                                digest,
                                assembly,
                                deadline,
                                repairs: 0,
                            },
                        );
                        if resume_from > 0 {
                            send_blob_resend(relay_tx, &peer, frame_id, digest, resume_from);
                        }
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
                let (progress, owes_ack) = {
                    let incoming = self.incoming.get_mut(&frame_id)?;
                    if incoming.peer != peer || incoming.digest != digest {
                        return None;
                    }
                    let progress = incoming.assembly.push(offset, &chunk_hex);
                    let asked_for_a_repair =
                        matches!(progress, Ok(relay::ChunkProgress::Gap { .. }));
                    if asked_for_a_repair {
                        incoming.repairs += 1;
                    }
                    let owes_ack = incoming.assembly.owes_ack();
                    if owes_ack {
                        incoming.assembly.ack_sent();
                    }
                    (progress, owes_ack)
                };
                match progress {
                    // the window's credit: the sender may push its far edge
                    // out by whatever this mark freed.
                    Ok(relay::ChunkProgress::Appended { received_through }) => {
                        if owes_ack {
                            send_blob_ack(relay_tx, &peer, frame_id, digest, received_through);
                        }
                    }
                    // a chunk that would have left a hole. Nothing was
                    // written; the sender is told where to resume, which is
                    // how a dropped chunk becomes a retransmission instead of
                    // a pack that never completes.
                    Ok(relay::ChunkProgress::Gap { received_through }) => {
                        send_blob_resend(relay_tx, &peer, frame_id, digest, received_through);
                    }
                    // the same hole, already reported: the window behind the
                    // dropped chunk is still draining past this door.
                    Ok(relay::ChunkProgress::Waiting) => {}
                    // finished, verified against its digest, and published.
                    Ok(relay::ChunkProgress::Complete) => {
                        let (bytes, repairs) = match self.incoming.remove(&frame_id) {
                            Some(done) => (done.assembly.received_through(), done.repairs),
                            None => (0, 0),
                        };
                        debug_assert!(self.blobs.has_chunk(&digest));
                        // once per completed transfer, on the side that
                        // RECEIVED it: the only evidence that a pack reached
                        // this node over the relay rather than by any later
                        // catch-up, and `repairs` is how many holes the
                        // window had to recover on the way.
                        tracing::debug!(
                            target: "ducktape::submit",
                            digest = %relay::encode_hex(&digest),
                            bytes,
                            repairs,
                            reason = "blob_transfer_complete",
                            "required blob received whole over the relay"
                        );
                        send_blob_result(relay_tx, &peer, frame_id, digest, None);
                    }
                    Err(detail) => {
                        if let Some(incoming) = self.incoming.remove(&frame_id) {
                            incoming.assembly.abort();
                        }
                        send_blob_result(relay_tx, &peer, frame_id, digest, Some(detail));
                    }
                }
                None
            }
            relay::RelayMsg::BlobAck {
                frame_id,
                digest,
                received_through,
            } => {
                let fanout = self.local_fanouts.get_mut(&frame_id)?;
                if let Err(detail) = fanout.transfer.on_ack(
                    &self.blobs,
                    relay_tx,
                    &peer,
                    &digest,
                    received_through,
                    Instant::now(),
                ) {
                    let fanout = self
                        .local_fanouts
                        .remove(&frame_id)
                        .expect("local fanout exists");
                    let _ = fanout
                        .reply
                        .send(Err(noded::Refused::new("blob_fanout", detail)));
                }
                None
            }
            relay::RelayMsg::BlobResend {
                frame_id,
                digest,
                from,
            } => {
                let fanout = self.local_fanouts.get_mut(&frame_id)?;
                if let Err(detail) = fanout.transfer.on_resend(
                    &self.blobs,
                    relay_tx,
                    &peer,
                    &digest,
                    from,
                    Instant::now(),
                ) {
                    let fanout = self
                        .local_fanouts
                        .remove(&frame_id)
                        .expect("local fanout exists");
                    let _ = fanout
                        .reply
                        .send(Err(noded::Refused::new("blob_fanout", detail)));
                }
                None
            }
            relay::RelayMsg::BlobResult {
                frame_id,
                digest,
                error,
            } => {
                let fanout = self.local_fanouts.get_mut(&frame_id)?;
                if fanout.transfer.digest != digest || !fanout.transfer.holds(&peer) {
                    return None;
                }
                if let Some(detail) = error {
                    let fanout = self
                        .local_fanouts
                        .remove(&frame_id)
                        .expect("local fanout exists");
                    let _ = fanout
                        .reply
                        .send(Err(noded::Refused::new("blob_fanout", detail)));
                    return None;
                }
                if !fanout.transfer.on_complete(&peer) {
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
        let monotonic = Instant::now();
        let stalled: Vec<_> = self
            .local_fanouts
            .iter_mut()
            .filter_map(|(id, fanout)| {
                let repaired = fanout
                    .transfer
                    .repair_stalled(&self.blobs, relay_tx, monotonic);
                repaired.err().map(|detail| (*id, detail))
            })
            .collect();
        for (id, detail) in stalled {
            if let Some(fanout) = self.local_fanouts.remove(&id) {
                let _ = fanout
                    .reply
                    .send(Err(noded::Refused::new("blob_fanout", detail)));
            }
        }

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
                    digest = %relay::encode_hex(&fanout.transfer.digest),
                    awaiting = fanout.transfer.awaiting(),
                    reason = "blob_fanout_expired",
                    "required blob fanout expired before every peer acked; the push fails and can be retried"
                );
                let _ = fanout.reply.send(Err(noded::Refused::new(
                    "blob_fanout_timeout",
                    "timed out distributing the required blob to validators",
                )));
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
                // the sender went quiet: its window is repaired from the
                // receiver's mark, so a transfer that reaches this deadline
                // has a peer that stopped sending, not one that lost chunks.
                // the staged bytes STAY — a retried push resumes from them
                // until the staging sweep reclaims them.
                tracing::warn!(
                    target: "ducktape::submit",
                    digest = %relay::encode_hex(&incoming.digest),
                    received_through = incoming.assembly.received_through(),
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

/// one target's place in a transfer: what it has acknowledged, how far the
/// window has been pushed past that, and how long the two have disagreed.
struct TargetCursor {
    peer: ed25519::PublicKey,
    /// every byte below this is on the target's disk — its last ack.
    acked: u64,
    /// how far the window has been pushed out. Rewound to `acked` on a stall:
    /// the bytes between the two are exactly what a drop swallowed.
    sent: u64,
    /// when `acked` last moved.
    progress_at: Instant,
    /// the mark this cursor has already rewound to and re-sent from. A repair
    /// that names it again is the receiver answering the SAME hole (its
    /// window is still draining), not a new one — re-sending for each of
    /// those would multiply one dropped chunk into a window per straggler.
    rewound_to: Option<u64>,
    rewinds: u32,
}

/// A pack crossing the wire to every target that must hold it before the frame
/// enters consensus. THE PACK SIZE IS NOT BOUNDED: bytes are read out of the
/// blob store one chunk at a time ([`blobstore::Blobs::read_range`]) and at
/// most one window per target is ever outstanding, so what this costs in
/// memory is the window, not the pack — see [`relay::RELAY_BLOB_WINDOW_CHUNKS`].
///
/// The receiver's ack is both the credit that refills the window and the
/// repair mark a rewind resumes from, so a chunk the mesh dropped is re-sent
/// rather than silently missing from a pack that then never completes.
struct BlobFanout {
    frame_id: node::FrameId,
    digest: [u8; 32],
    total: u64,
    cursors: Vec<TargetCursor>,
}

impl BlobFanout {
    /// offer the pack to every target and open each one's window.
    fn open<S>(
        blobs: &blobstore::BlobHandle,
        relay_tx: &mut S,
        targets: &[ed25519::PublicKey],
        frame: &[u8],
        digest: [u8; 32],
        now: Instant,
    ) -> Result<Self, String>
    where
        S: P2pSender<PublicKey = ed25519::PublicKey>,
    {
        let Some(total) = blobs.chunk_len(&digest).filter(|len| *len > 0) else {
            return Err(
                "required blob referenced by the submit is not in this node's blob store".into(),
            );
        };
        let mut fanout = Self {
            frame_id: node::frame_id(frame),
            digest,
            total,
            cursors: targets
                .iter()
                .map(|peer| TargetCursor {
                    peer: peer.clone(),
                    acked: 0,
                    sent: 0,
                    progress_at: now,
                    rewound_to: None,
                    rewinds: 0,
                })
                .collect(),
        };
        for cursor in &fanout.cursors {
            let offered = send(
                relay_tx,
                &cursor.peer,
                relay::RelayMsg::BlobOffer {
                    frame: frame.to_vec(),
                    digest,
                    total,
                },
            );
            if !offered {
                return Err(format!(
                    "validator {} unreachable during required blob offer",
                    cursor.peer
                ));
            }
        }
        for index in 0..fanout.cursors.len() {
            fanout.pump(blobs, relay_tx, index)?;
        }
        Ok(fanout)
    }

    /// fill one target's window from its `sent` mark.
    fn pump<S>(
        &mut self,
        blobs: &blobstore::BlobHandle,
        relay_tx: &mut S,
        index: usize,
    ) -> Result<(), String>
    where
        S: P2pSender<PublicKey = ed25519::PublicKey>,
    {
        const WINDOW_BYTES: u64 =
            (relay::RELAY_BLOB_WINDOW_CHUNKS * relay::RELAY_BLOB_CHUNK_BYTES) as u64;
        let (frame_id, digest, total) = (self.frame_id, self.digest, self.total);
        let cursor = &mut self.cursors[index];
        while cursor.sent < total && cursor.sent - cursor.acked < WINDOW_BYTES {
            let len = relay::RELAY_BLOB_CHUNK_BYTES.min((total - cursor.sent) as usize);
            let Some(chunk) = blobs.read_range(&digest, cursor.sent, len) else {
                return Err("required blob left this node's blob store mid-transfer".into());
            };
            let delivered = send(
                relay_tx,
                &cursor.peer,
                relay::RelayMsg::BlobChunk {
                    frame_id,
                    digest,
                    offset: cursor.sent,
                    chunk_hex: relay::encode_hex(&chunk),
                },
            );
            if !delivered {
                return Err(format!(
                    "validator {} unreachable during required blob transfer",
                    cursor.peer
                ));
            }
            cursor.sent += chunk.len() as u64;
        }
        Ok(())
    }

    /// a target reported its high-water mark: credit the window and refill it.
    fn on_ack<S>(
        &mut self,
        blobs: &blobstore::BlobHandle,
        relay_tx: &mut S,
        peer: &ed25519::PublicKey,
        digest: &[u8; 32],
        received_through: u64,
        now: Instant,
    ) -> Result<(), String>
    where
        S: P2pSender<PublicKey = ed25519::PublicKey>,
    {
        if digest != &self.digest {
            return Ok(());
        }
        let Some(index) = self.cursors.iter().position(|c| &c.peer == peer) else {
            return Ok(());
        };
        let cursor = &mut self.cursors[index];
        let mark = received_through.min(self.total);
        if mark > cursor.acked {
            cursor.acked = mark;
            cursor.progress_at = now;
            // bytes are landing again, so the hole a rewind answered is
            // behind us: the next repair is a new one, and the rewind budget
            // below counts CONSECUTIVE stalls — a link that keeps delivering
            // between them is slow, not down.
            cursor.rewound_to = None;
            cursor.rewinds = 0;
        }
        self.pump(blobs, relay_tx, index)
    }

    /// a target refused a chunk that would have left a hole: everything from
    /// its mark on has to cross again. A mark behind what the target already
    /// acknowledged is a repair that lost its race with the bytes it asked
    /// for — ignore it rather than re-send an acknowledged prefix.
    fn on_resend<S>(
        &mut self,
        blobs: &blobstore::BlobHandle,
        relay_tx: &mut S,
        peer: &ed25519::PublicKey,
        digest: &[u8; 32],
        from: u64,
        now: Instant,
    ) -> Result<(), String>
    where
        S: P2pSender<PublicKey = ed25519::PublicKey>,
    {
        if digest != &self.digest {
            return Ok(());
        }
        let Some(index) = self.cursors.iter().position(|c| &c.peer == peer) else {
            return Ok(());
        };
        let cursor = &mut self.cursors[index];
        let stale = from < cursor.acked;
        let same_hole = cursor.rewound_to == Some(from);
        if stale || same_hole {
            return Ok(());
        }
        cursor.acked = from;
        cursor.sent = from;
        cursor.rewound_to = Some(from);
        cursor.progress_at = now;
        self.pump(blobs, relay_tx, index)
    }

    /// this target holds the whole pack; drop its cursor. `true` once every
    /// target has.
    fn on_complete(&mut self, peer: &ed25519::PublicKey) -> bool {
        if let Some(index) = self.cursors.iter().position(|c| &c.peer == peer) {
            let done = self.cursors.swap_remove(index);
            // once per target, on the side that SENT it. `rewinds` is how
            // many times this target's window sat unacknowledged long enough
            // to be re-sent from its mark — zero on a link that dropped
            // nothing.
            tracing::debug!(
                target: "ducktape::submit",
                digest = %relay::encode_hex(&self.digest),
                bytes = self.total,
                rewinds = done.rewinds,
                awaiting = self.cursors.len(),
                reason = "blob_target_complete",
                "a validator holds the required blob"
            );
        }
        self.cursors.is_empty()
    }

    fn awaiting(&self) -> usize {
        self.cursors.len()
    }

    fn holds(&self, peer: &ed25519::PublicKey) -> bool {
        self.cursors.iter().any(|c| &c.peer == peer)
    }

    /// rewind and re-send every target whose window has sat unmoved past
    /// [`BLOB_WINDOW_STALL`] — the only evidence a sender ever gets that its
    /// chunks were dropped rather than delayed.
    fn repair_stalled<S>(
        &mut self,
        blobs: &blobstore::BlobHandle,
        relay_tx: &mut S,
        now: Instant,
    ) -> Result<(), String>
    where
        S: P2pSender<PublicKey = ed25519::PublicKey>,
    {
        for index in 0..self.cursors.len() {
            let cursor = &mut self.cursors[index];
            let window_is_open = cursor.sent > cursor.acked;
            let stalled =
                window_is_open && now.duration_since(cursor.progress_at) >= BLOB_WINDOW_STALL;
            if !stalled {
                continue;
            }
            cursor.rewinds += 1;
            if cursor.rewinds > BLOB_WINDOW_MAX_REWINDS {
                return Err(format!(
                    "validator {} stopped acknowledging the required blob at {} of {} bytes",
                    cursor.peer, cursor.acked, self.total
                ));
            }
            // attempt 1 and every eighth after it: a stalled link retries for
            // minutes, and the COUNTER is the diagnosis.
            let worth_logging = cursor.rewinds == 1 || cursor.rewinds.is_multiple_of(8);
            if worth_logging {
                tracing::warn!(
                    target: "ducktape::submit",
                    digest = %relay::encode_hex(&self.digest),
                    acked = cursor.acked,
                    total = self.total,
                    attempts = cursor.rewinds,
                    reason = "blob_window_stalled",
                    "no acknowledgement for a whole blob window; resending from the last mark"
                );
            }
            cursor.sent = cursor.acked;
            cursor.progress_at = now;
            self.pump(blobs, relay_tx, index)?;
        }
        Ok(())
    }
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

/// the window's credit: how far this receiver's disk has got.
fn send_blob_ack<S>(
    relay_tx: &mut S,
    peer: &ed25519::PublicKey,
    frame_id: node::FrameId,
    digest: [u8; 32],
    received_through: u64,
) where
    S: P2pSender<PublicKey = ed25519::PublicKey>,
{
    let _ = send(
        relay_tx,
        peer,
        relay::RelayMsg::BlobAck {
            frame_id,
            digest,
            received_through,
        },
    );
}

/// the repair: where the sender has to resume, because everything after this
/// mark is missing from the receiver's disk.
fn send_blob_resend<S>(
    relay_tx: &mut S,
    peer: &ed25519::PublicKey,
    frame_id: node::FrameId,
    digest: [u8; 32],
    from: u64,
) where
    S: P2pSender<PublicKey = ed25519::PublicKey>,
{
    let _ = send(
        relay_tx,
        peer,
        relay::RelayMsg::BlobResend {
            frame_id,
            digest,
            from,
        },
    );
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
        let blobs = blobstore::BlobHandle::default();
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
        let blobs = blobstore::BlobHandle::default();
        let pack = b"PACK-test";
        let digest: [u8; 32] = Sha256::digest(pack).into();
        let mut assembly = relay::BlobAssembly::new(&blobs, digest, pack.len() as u64).unwrap();
        assert_eq!(
            assembly.push(0, &relay::encode_hex(pack)).unwrap(),
            relay::ChunkProgress::Complete
        );
        assert_eq!(blobs.get_chunk(&digest).as_deref(), Some(pack.as_slice()));
    }

    /// THE PROPERTY THE WHOLE TRANSPORT EXISTS FOR: a chunk the mesh drops
    /// cannot leave a pack quietly short. The mesh here is a function that
    /// swallows one chunk; the receiver refuses the hole, the sender rewinds
    /// to the mark it names, and the pack completes with every byte.
    ///
    /// The second half is the one that matters: with the repair suppressed,
    /// the transfer must NOT complete. A transport that could finish over a
    /// hole would publish a pack that is not the pack that was pushed.
    #[test]
    fn a_dropped_chunk_is_retransmitted_and_never_silently_lost() {
        use commonware_cryptography::Signer as _;
        let pack: Vec<u8> = (0..relay::RELAY_BLOB_CHUNK_BYTES * 3 + 11)
            .map(|byte| byte as u8)
            .collect();
        let author = ed25519::PrivateKey::from_seed(7);
        let target = ed25519::PrivateKey::from_seed(8).public_key();

        // one run of the transfer. `drop_nth` swallows that chunk on its first
        // crossing; `repair` says whether the receiver is allowed to answer a
        // hole with the mark to resume from.
        let run = |drop_nth: usize, repair: bool| {
            let sender_store = blobstore::BlobHandle::default();
            let digest = sender_store.put_chunk(pack.clone());
            let receiver_store = blobstore::BlobHandle::default();
            let frame = node::encode_frame_with_blob(
                &author,
                1,
                &Msg {
                    target: "forge".into(),
                    payload: b"push".to_vec(),
                },
                Some(digest),
            );
            let mut wire = RecordingSender::default();
            let mut fanout = BlobFanout::open(
                &sender_store,
                &mut wire,
                std::slice::from_ref(&target),
                &frame,
                digest,
                Instant::now(),
            )
            .expect("the pack is in the sender's store");

            let mut assembly =
                relay::BlobAssembly::new(&receiver_store, digest, pack.len() as u64).unwrap();
            let mut crossings = 0usize;
            let mut delivered = 0usize;
            // drain what the sender put on the wire, feed the receiver, and
            // carry its answers back — exactly the two message kinds the live
            // lane routes.
            loop {
                let outbound: Vec<relay::RelayMsg> = std::mem::take(&mut *wire.0.lock().unwrap());
                if outbound.is_empty() {
                    break;
                }
                for msg in outbound {
                    let relay::RelayMsg::BlobChunk {
                        offset, chunk_hex, ..
                    } = msg
                    else {
                        continue;
                    };
                    crossings += 1;
                    let swallowed = crossings == drop_nth;
                    if swallowed {
                        continue;
                    }
                    delivered += 1;
                    match assembly
                        .push(offset, &chunk_hex)
                        .expect("no protocol error")
                    {
                        relay::ChunkProgress::Appended { received_through } => fanout
                            .on_ack(
                                &sender_store,
                                &mut wire,
                                &target,
                                &digest,
                                received_through,
                                Instant::now(),
                            )
                            .expect("the pack is still there"),
                        relay::ChunkProgress::Gap { received_through } => {
                            if repair {
                                fanout
                                    .on_resend(
                                        &sender_store,
                                        &mut wire,
                                        &target,
                                        &digest,
                                        received_through,
                                        Instant::now(),
                                    )
                                    .expect("the pack is still there");
                            }
                        }
                        relay::ChunkProgress::Waiting | relay::ChunkProgress::Complete => {}
                    }
                }
            }
            (receiver_store.get_chunk(&digest), delivered)
        };

        let (repaired, delivered) = run(2, true);
        assert_eq!(
            repaired.as_deref(),
            Some(pack.as_slice()),
            "a dropped chunk must be re-sent and the pack must arrive whole"
        );
        assert!(
            delivered > pack.len().div_ceil(relay::RELAY_BLOB_CHUNK_BYTES),
            "the repair re-crosses at least the chunks the drop swallowed"
        );

        let (unrepaired, _) = run(2, false);
        assert!(
            unrepaired.is_none(),
            "a pack with a hole in it must never be published"
        );
    }

    #[test]
    fn seq_file_is_read_without_mutating_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay-submit-seq");
        std::fs::write(&path, "41").unwrap();
        let relay = ResidentRelay::new(path.clone(), blobstore::BlobHandle::default());
        assert_eq!(relay.seq, 41);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "41");
    }
}
