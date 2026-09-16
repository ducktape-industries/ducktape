//! The voice media engine: Opus over the data plane's datagram class.
//!
//! A submodule of the chat module but deliberately self-contained: it touches
//! no qmdb, no root-hash, and none of chat's consensus state. The coupling is
//! one-directional and future — chat's channel membership will drive this
//! engine's `AdmissionPolicy` and the channel→flow derivation; the media
//! pipeline here knows nothing of channels.
//!
//! Wire surface (this module root + [`media`]): the 10-byte media header
//! (epoch, seq, timestamp) carried inside a data-plane datagram on
//! `Service::Voice`, payload = one 20 ms Opus frame. Speakers are identified
//! by the transport-authenticated `PeerId` — the plane's WireGuard identity
//! binding — so there is no SSRC and no media-level identity to spoof.
//!
//! ```text
//! pcm 20ms frame → Opus encode (VOIP) → media header → fan-out
//! per speaker:  datagrams → jitter buffer → Opus decode | gap→silence
//! playout tick: sum decoded speakers → one mixed pcm frame
//! ```
//!
//! Codec is pure-Rust `opus-rs`, which has no FEC-decode and no PLC, so a lost
//! frame is rendered as silence, not concealed — the jitter buffer still
//! reports the gap so a concealment-capable codec could fill it later.
//!
//! No audio hardware (capture/playback/AEC arrive with the hardware edge).
//! Everything here is provable on the data-plane sim transport under a paused
//! clock.

pub mod codec;
pub mod engine;
pub mod jitter;
pub mod media;

pub use codec::{CodecError, VoiceDecoder, VoiceEncoder};
pub use engine::{SpeakerStats, VoiceConfig, VoiceEngine};
pub use jitter::{JitterStats, MinimalJitter, PlayoutStep};
pub use media::{MediaError, MediaHeader};

/// Voice runs at Opus's native rate, mono.
pub const SAMPLE_RATE: u32 = 48_000;
/// Samples per frame: 48 kHz × 20 ms — Opus's sweet spot and the packet
/// cadence.
pub const FRAME_SAMPLES: usize = 960;
