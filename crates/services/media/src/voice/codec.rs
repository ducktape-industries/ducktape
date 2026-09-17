//! Opus wrappers (pure-Rust `opus-rs`) pinned to the engine's frame contract:
//! 48 kHz mono VOIP, 20 ms / 960-sample frames.
//!
//! `i16` PCM is the engine's surface; `opus-rs` is `f32` I/O, so the
//! conversion lives here at the boundary and nowhere else.
//!
//! `opus-rs` exposes neither FEC-decode nor PLC — its decoder errors on empty
//! input and has no flag to reconstruct a frame from a successor packet. So
//! this wrapper only encodes present frames and decodes received ones;
//! concealing a gap is the engine's job (silence today). Encoding LBRR would
//! be dead weight — nothing can decode it — so in-band FEC is left off.

use std::panic::AssertUnwindSafe;

use opus_rs::{Application, OpusDecoder, OpusEncoder};

use super::{FRAME_SAMPLES, SAMPLE_RATE};

/// Room for any 20 ms mono Opus frame at sane bitrates. Public because it is
/// also the bound every wire that CARRIES one of these frames checks against
/// — a frame larger than the encoder can produce is not one of ours.
pub const MAX_ENCODED: usize = 1275;
/// Full-scale for i16 ↔ normalized-f32 conversion.
const SCALE: f32 = 32_768.0;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    // opus-rs reports failures as &'static str.
    #[error("opus: {0}")]
    Opus(&'static str),
    /// A peer's packet refused at this boundary, never handed to (or unwound
    /// out of) the library. The payload is a stable snake_case reason token —
    /// what a lane's `bad_packet` log and counter carry.
    #[error("bad packet: {0}")]
    BadPacket(&'static str),
}

pub struct VoiceEncoder {
    inner: OpusEncoder,
    scratch: Vec<f32>,
}

impl VoiceEncoder {
    pub fn new(bitrate_bits_per_sec: i32) -> Result<Self, CodecError> {
        let mut inner =
            OpusEncoder::new(SAMPLE_RATE as i32, 1, Application::Voip).map_err(CodecError::Opus)?;
        inner.bitrate_bps = bitrate_bits_per_sec;
        inner.use_inband_fec = false;
        inner.packet_loss_perc = 0;
        Ok(VoiceEncoder {
            inner,
            scratch: vec![0.0; FRAME_SAMPLES],
        })
    }

    /// Encode exactly one 20 ms frame.
    pub fn encode(&mut self, pcm: &[i16; FRAME_SAMPLES]) -> Result<Vec<u8>, CodecError> {
        for (dst, &src) in self.scratch.iter_mut().zip(pcm.iter()) {
            *dst = f32::from(src) / SCALE;
        }
        let mut out = vec![0u8; MAX_ENCODED];
        let len = self
            .inner
            .encode(&self.scratch, FRAME_SAMPLES, &mut out)
            .map_err(CodecError::Opus)?;
        out.truncate(len);
        Ok(out)
    }
}

pub struct VoiceDecoder {
    inner: OpusDecoder,
    scratch: Vec<f32>,
}

impl VoiceDecoder {
    pub fn new() -> Result<Self, CodecError> {
        let inner = OpusDecoder::new(SAMPLE_RATE as i32, 1).map_err(CodecError::Opus)?;
        Ok(VoiceDecoder {
            inner,
            scratch: vec![0.0; FRAME_SAMPLES],
        })
    }

    /// Decode one received frame to `i16` PCM. A short decode (fewer samples
    /// than a full frame) leaves the tail silent rather than stale.
    ///
    /// Every byte here was chosen by a remote peer, so the packet is checked
    /// against the engine's one config before the library sees it and the
    /// call itself is unwind-guarded: a decoder panic must cost one packet,
    /// never the whole huddle session that holds the lanes lock.
    pub fn decode(&mut self, payload: &[u8]) -> Result<[i16; FRAME_SAMPLES], CodecError> {
        let Some(&toc) = payload.first() else {
            return Err(CodecError::BadPacket("empty_packet"));
        };
        if !is_twenty_ms_mono(toc) {
            return Err(CodecError::BadPacket("toc_off_contract"));
        }
        // A bare TOC codes no audio; every packet shape needs a byte after it.
        if payload.len() < 2 {
            return Err(CodecError::BadPacket("packet_too_short"));
        }
        let attempt = {
            let (inner, scratch) = (&mut self.inner, &mut self.scratch);
            std::panic::catch_unwind(AssertUnwindSafe(|| {
                inner.decode(payload, FRAME_SAMPLES, scratch)
            }))
        };
        let decoded = match attempt {
            Ok(Ok(decoded)) => decoded,
            Ok(Err(error)) => return Err(CodecError::Opus(error)),
            Err(_) => {
                // The library panicked on a shape the guard above did not
                // anticipate. Its decoder state is now arbitrary, so replace
                // it: the lane must not stay wedged for the rest of the call.
                self.inner = OpusDecoder::new(SAMPLE_RATE as i32, 1).map_err(CodecError::Opus)?;
                return Err(CodecError::BadPacket("decoder_panicked"));
            }
        };
        let mut pcm = [0i16; FRAME_SAMPLES];
        for (dst, &src) in pcm.iter_mut().zip(self.scratch.iter()).take(decoded) {
            *dst = (src * SCALE).clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
        }
        Ok(pcm)
    }
}

/// Does this packet's TOC byte name the one config the engine contracts on —
/// 20 ms, mono?
///
/// It is not a taste check. `opus-rs` sizes its internal SILK scratch for a
/// 20 ms frame (640 samples) but derives the frame length from the packet's
/// own TOC behind a `debug_assert!`, so a peer-chosen 60 ms wideband TOC
/// slices that buffer to 960 samples and panics in a release build. The top
/// five bits are the Opus config number; these nine are the 20 ms ones
/// (SILK 1/5/9, hybrid 13/15, CELT 19/23/27/31), and bit 2 is the stereo
/// flag the mono decoder rejects anyway.
const fn is_twenty_ms_mono(toc: u8) -> bool {
    const STEREO: u8 = 0b0000_0100;
    matches!(toc >> 3, 1 | 5 | 9 | 13 | 15 | 19 | 23 | 27 | 31) && toc & STEREO == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(tick: usize) -> [i16; FRAME_SAMPLES] {
        let mut pcm = [0i16; FRAME_SAMPLES];
        for (i, sample) in pcm.iter_mut().enumerate() {
            let t = (tick * FRAME_SAMPLES + i) as f32 / SAMPLE_RATE as f32;
            *sample = ((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
        }
        pcm
    }

    fn rms(pcm: &[i16]) -> f64 {
        let sum: f64 = pcm.iter().map(|&s| (s as f64) * (s as f64)).sum();
        (sum / pcm.len() as f64).sqrt()
    }

    #[test]
    fn round_trip_preserves_energy() {
        let mut enc = VoiceEncoder::new(32_000).unwrap();
        let mut dec = VoiceDecoder::new().unwrap();
        // A few frames so codec state settles past the initial transient.
        let mut last = [0i16; FRAME_SAMPLES];
        for tick in 0..10 {
            let payload = enc.encode(&tone(tick)).unwrap();
            assert!(payload.len() <= super::super::media::MAX_OPUS_PAYLOAD);
            last = dec.decode(&payload).unwrap();
        }
        let (input_rms, output_rms) = (rms(&tone(9)), rms(&last));
        assert!(
            (output_rms - input_rms).abs() / input_rms < 0.25,
            "energy drifted: in {input_rms}, out {output_rms}"
        );
    }

    /// The exploit packet: a SILK-only mono TOC naming a 60 ms wideband frame
    /// (960 internal samples) plus one byte. The library would slice its
    /// 640-sample scratch to 960 and panic in release, killing the session
    /// task while it holds the lanes lock. It must cost this one packet, and
    /// the lane must keep decoding afterwards.
    #[test]
    fn a_sixty_millisecond_toc_is_refused_and_the_lane_keeps_decoding() {
        let mut dec = VoiceDecoder::new().unwrap();
        assert!(matches!(
            dec.decode(&[0x58, 0x00]),
            Err(CodecError::BadPacket("toc_off_contract"))
        ));
        let mut enc = VoiceEncoder::new(32_000).unwrap();
        let payload = enc.encode(&tone(0)).unwrap();
        assert!(
            dec.decode(&payload).is_ok(),
            "a valid frame after the refused one must still decode"
        );
    }

    /// Nothing a peer can put in the first four bytes may reach a panic: the
    /// TOC guard, the library's own errors and the unwind guard together are
    /// the contract, and this walks every TOC byte at every short length.
    #[test]
    fn no_short_packet_panics() {
        let mut dec = VoiceDecoder::new().unwrap();
        for toc in 0..=u8::MAX {
            for len in 1..=4usize {
                let mut packet = vec![0u8; len];
                packet[0] = toc;
                // the value under test is that this returns at all.
                let _ = dec.decode(&packet);
            }
        }
    }
}
