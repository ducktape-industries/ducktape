//! The engine controller: one encoder for the local speaker, one lane
//! (jitter buffer + stateful decoder) per remote speaker, client-side mix.
//!
//! Wiring: construction spawns a pump that drains the `DatagramFlow` into
//! per-speaker lanes (lanes appear on first frame from a peer — admission
//! already vetted the sender at the plane). The audio clock drives the
//! other side: `send_frame` per captured 20 ms frame, `playout` per output
//! tick. Speakers are keyed by transport-authenticated `PeerId`; the mix is
//! a saturating sum of whatever each lane's jitter buffer decided.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use data_plane::{DataPlaneTransport, DatagramFlow, PeerId, SendError};
use tokio::sync::watch;

use super::FRAME_SAMPLES;
use super::codec::{CodecError, VoiceDecoder, VoiceEncoder};
use super::jitter::{JitterStats, MinimalJitter, PlayoutStep};
use super::media::{self, MediaError, MediaHeader};

#[derive(Clone, Copy, Debug)]
pub struct VoiceConfig {
    pub bitrate_bits_per_sec: i32,
    /// Initial jitter cushion, frames (x20 ms).
    pub prefill_frames: usize,
    /// Cushion ceiling the buffer may grow to under underruns.
    pub max_depth_frames: usize,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        VoiceConfig {
            bitrate_bits_per_sec: 32_000,
            prefill_frames: 2,
            max_depth_frames: 6,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error(transparent)]
    Media(#[from] MediaError),
    #[error(transparent)]
    Send(#[from] SendError),
}

#[derive(Clone, Copy, Debug)]
pub struct SpeakerStats {
    pub peer: PeerId,
    pub jitter: JitterStats,
    pub decode_errors: u64,
    /// Packets this peer sent that the codec boundary refused outright
    /// (off-contract TOC, too short, or an unwind out of the library).
    pub bad_packets: u64,
}

/// A refused packet logs on the first and every hundredth occurrence per
/// lane: a peer can send one per datagram, and a warn per datagram is a log
/// bomb that evicts the ring the operator is reading.
const BAD_PACKET_EVERY: u64 = 100;

/// An epoch change is a per-session lifecycle fact, but a peer chooses the
/// field: latch it the same way, first and every hundredth.
const EPOCH_CHANGE_EVERY: u64 = 100;

struct Lane {
    jitter: MinimalJitter,
    decoder: VoiceDecoder,
    decode_errors: u64,
    bad_packets: u64,
    /// which of the sender's engines this lane is following.
    epoch: u32,
    epoch_changes: u64,
}

impl Lane {
    fn new(epoch: u32, decoder: VoiceDecoder, config: VoiceConfig) -> Self {
        Lane {
            jitter: MinimalJitter::new(config.prefill_frames, config.max_depth_frames),
            decoder,
            decode_errors: 0,
            bad_packets: 0,
            epoch,
            epoch_changes: 0,
        }
    }

    /// The peer restarted their media without leaving the roster. Their seq
    /// went back to 0, so a jitter buffer anchored at the old high seq would
    /// count the whole new stream late: start the lane over.
    fn follow_new_epoch(
        &mut self,
        epoch: u32,
        decoder: VoiceDecoder,
        config: VoiceConfig,
        peer: &PeerId,
    ) {
        self.jitter = MinimalJitter::new(config.prefill_frames, config.max_depth_frames);
        self.decoder = decoder;
        self.epoch = epoch;
        self.epoch_changes += 1;
        let occurrences = self.epoch_changes;
        if occurrences == 1 || occurrences.is_multiple_of(EPOCH_CHANGE_EVERY) {
            tracing::info!(
                target: "ducktape::voice",
                reason = "media_epoch_changed",
                peer = %peer_label(peer),
                occurrences,
                "peer's media restarted — speaker lane reopened"
            );
        }
    }

    fn note_bad_packet(&mut self, peer: &PeerId, reason: &'static str) {
        self.bad_packets += 1;
        let occurrences = self.bad_packets;
        if occurrences == 1 || occurrences.is_multiple_of(BAD_PACKET_EVERY) {
            tracing::warn!(
                target: "ducktape::voice",
                reason,
                peer = %peer_label(peer),
                occurrences,
                "refused a peer's opus packet"
            );
        }
    }
}

type Lanes = Arc<Mutex<HashMap<PeerId, Lane>>>;

/// One voice channel's engine over one data-plane datagram flow.
pub struct VoiceEngine<T: DataPlaneTransport> {
    flow: Arc<DatagramFlow<T>>,
    encoder: VoiceEncoder,
    /// this engine instance, stamped on every media and video frame it sends
    /// so a receiver can tell a restart from stale traffic.
    epoch: u32,
    seq: u16,
    timestamp: u32,
    lanes: Lanes,
    malformed: Arc<AtomicU64>,
    /// the pump task; aborted on drop so the pump's flow handle is released
    /// and the (service, flow) slot can be registered again by a later engine.
    pump: tokio::task::JoinHandle<()>,
}

impl<T: DataPlaneTransport> Drop for VoiceEngine<T> {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

impl<T: DataPlaneTransport> VoiceEngine<T> {
    /// `recipients` is the same roster watch the host session gates sends
    /// with. The pump re-checks it on every datagram so a peer dropped from
    /// the roster stops feeding this engine's lanes immediately, not only
    /// once the host's periodic sweep calls `forget_peer`.
    pub fn new(
        flow: DatagramFlow<T>,
        config: VoiceConfig,
        recipients: watch::Receiver<Vec<[u8; 32]>>,
    ) -> Result<Self, CodecError> {
        let encoder = VoiceEncoder::new(config.bitrate_bits_per_sec)?;
        let flow = Arc::new(flow);
        let lanes: Lanes = Arc::new(Mutex::new(HashMap::new()));
        let malformed = Arc::new(AtomicU64::new(0));
        let pump = tokio::spawn(pump(
            flow.clone(),
            lanes.clone(),
            config,
            malformed.clone(),
            recipients,
        ));
        Ok(VoiceEngine {
            flow,
            encoder,
            epoch: rand::random(),
            seq: 0,
            timestamp: 0,
            lanes,
            malformed,
            pump,
        })
    }

    /// Encode one captured 20 ms frame and fan it out to the channel's
    /// other members. All recipients are attempted; the first failure is
    /// reported after the fan-out completes.
    pub async fn send_frame(
        &mut self,
        pcm: &[i16; FRAME_SAMPLES],
        recipients: &[PeerId],
    ) -> Result<(), EngineError> {
        let payload = self.encoder.encode(pcm)?;
        let frame = media::encode_frame(
            MediaHeader {
                epoch: self.epoch,
                seq: self.seq,
                timestamp: self.timestamp,
            },
            &payload,
        )?;
        self.seq = self.seq.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(FRAME_SAMPLES as u32);
        let mut first_error = None;
        for recipient in recipients {
            if let Err(error) = self.flow.send_to(*recipient, &frame).await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), |error| Err(error.into()))
    }

    /// One 20 ms output tick: step every speaker's jitter buffer, decode
    /// present frames, and mix. A gap contributes silence (the codec offers
    /// no concealment). Call at the frame cadence; returns silence while no
    /// speaker has playable audio.
    pub fn playout(&self) -> [i16; FRAME_SAMPLES] {
        let mut mix = [0i32; FRAME_SAMPLES];
        let mut lanes = self.lanes.lock().expect("lanes lock");
        for (peer, lane) in lanes.iter_mut() {
            let decoded = match lane.jitter.tick() {
                // Buffering and Gap both render as silence: nothing to add.
                PlayoutStep::Buffering | PlayoutStep::Gap => None,
                PlayoutStep::Frame(payload) => Some(lane.decoder.decode(&payload)),
            };
            let pcm = match decoded {
                None => continue,
                Some(Ok(pcm)) => pcm,
                // A peer's undecodable payload must not silence the rest of
                // the mix: count it and keep going. A refused packet is the
                // hostile-or-broken case and says so by reason.
                Some(Err(CodecError::BadPacket(reason))) => {
                    lane.note_bad_packet(peer, reason);
                    continue;
                }
                Some(Err(CodecError::Opus(_))) => {
                    lane.decode_errors += 1;
                    continue;
                }
            };
            for (mixed, sample) in mix.iter_mut().zip(pcm) {
                *mixed += i32::from(sample);
            }
        }
        let mut out = [0i16; FRAME_SAMPLES];
        for (out_sample, mixed) in out.iter_mut().zip(mix) {
            *out_sample = mixed.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        }
        out
    }

    /// Drop a speaker's lane. Call it when the peer leaves the roster: their
    /// buffered audio and decoder state are dead weight from that moment, and
    /// a lane per departed peer accumulates for the life of the call. A peer
    /// who comes back is re-anchored by their media epoch, not by this.
    /// Returns whether a lane was there to drop.
    pub fn forget_peer(&self, peer: PeerId) -> bool {
        self.lanes
            .lock()
            .expect("lanes lock")
            .remove(&peer)
            .is_some()
    }

    pub fn speaker_stats(&self) -> Vec<SpeakerStats> {
        self.lanes
            .lock()
            .expect("lanes lock")
            .iter()
            .map(|(peer, lane)| SpeakerStats {
                peer: *peer,
                jitter: lane.jitter.stats(),
                decode_errors: lane.decode_errors,
                bad_packets: lane.bad_packets,
            })
            .collect()
    }

    /// This engine instance. The video plane stamps the same value, so both
    /// of a peer's streams restart together.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Datagrams that failed media decoding (bad version/truncated).
    pub fn malformed_frames(&self) -> u64 {
        self.malformed.load(Ordering::Relaxed)
    }
}

/// the first 8 hex chars of a peer's key: enough to match a roster entry in
/// a log, never the key.
fn peer_label(peer: &PeerId) -> String {
    peer.0[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// Drain the flow into per-speaker lanes. Runs for the engine's lifetime;
/// admission and flow demux already happened at the plane.
async fn pump<T: DataPlaneTransport>(
    flow: Arc<DatagramFlow<T>>,
    lanes: Lanes,
    config: VoiceConfig,
    malformed: Arc<AtomicU64>,
    recipients: watch::Receiver<Vec<[u8; 32]>>,
) {
    loop {
        let (peer, bytes) = flow.recv().await;
        // the plane's admission is a standing ACL, not this call's live
        // roster: a peer already kicked from the huddle can still have
        // frames sitting in the flow's queue when they pop here. Re-check
        // the roster on every datagram so those never open or feed a lane.
        if !recipients.borrow().contains(&peer.0) {
            continue;
        }
        let Ok((header, payload)) = media::decode_frame(&bytes) else {
            malformed.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let mut lanes = lanes.lock().expect("lanes lock");
        let lane = match lanes.entry(peer) {
            Entry::Occupied(existing) => existing.into_mut(),
            Entry::Vacant(vacant) => {
                let Ok(decoder) = VoiceDecoder::new() else {
                    malformed.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                // the peer's first admitted frame: the plane authenticated
                // it, the roster admitted it, and its speaker lane exists
                // from here — once per peer per engine, never per frame.
                tracing::info!(
                    target: "ducktape::voice",
                    event = "voice_peer_handshake_complete",
                    peer = %peer_label(&peer),
                    "first media frame from peer — speaker lane opened"
                );
                vacant.insert(Lane::new(header.epoch, decoder, config))
            }
        };
        if lane.epoch != header.epoch {
            let Ok(decoder) = VoiceDecoder::new() else {
                malformed.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            lane.follow_new_epoch(header.epoch, decoder, config, &peer);
        }
        lane.jitter.insert(header.seq, payload.to_vec());
    }
}
