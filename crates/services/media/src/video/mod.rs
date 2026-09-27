//! The video call media wire: fragmentation of encoded (VP8) frames onto
//! data-plane datagrams, per-sender reassembly, and the call-control
//! datagram codec. The full design: a node WebRTC gateway (str0m,
//! localhost-only SDP) on one
//! side, `Service::Video` on the data plane on the other, and the roster
//! (`CallJoin`/`CallLeave`) as the only piece that ever touches consensus.
//!
//! **Consensus never carries media.** This module is deliberately pure and
//! synchronous: no async, no qmdb, no root-hash, nothing that touches chat's
//! consensus state. Every byte here is either an encoded video frame
//! fragment or a call-control message (keyframe requests, presence beacons,
//! rate hints) riding a data-plane flow — none of it is proposed, ordered,
//! or replicated by consensus. A submodule of chat like [`crate::voice`],
//! with the same one-directional coupling: chat's channel membership will
//! (eventually) drive admission for these flows; this module knows nothing
//! of channels.
//!
//! Layers (this module root + [`frame`], [`assembly`], [`control`]):
//!
//! ```text
//! encoded VP8 frame → fragment_frame → datagrams on Service::Video
//! datagrams (per peer) → Reassembler::insert → CompleteFrame | drop
//! call state (mute, camera, rate) → CallControl → datagrams on Service::Voice
//! ```
//!
//! `frame` fragments/reassembles-header-only; `assembly` owns per-sender
//! reassembly state (the hub keys one [`assembly::Reassembler`] per peer);
//! `control` is the separate, tiny ctl-flow wire so control keeps working in
//! an audio-only build.

pub mod assembly;
pub mod codec;
pub mod control;
pub mod frame;

pub use assembly::{Assembly, CompleteFrame, Reassembler};
pub use control::{CallControl, ControlError, RATE_LADDER_KBPS, step_down, step_up};
pub use frame::{
    FLAG_KEYFRAME, MAX_FRAGMENT_PAYLOAD, MAX_FRAGS, MAX_FRAME_BYTES, VIDEO_HEADER_LEN, VideoError,
    VideoHeader, decode_fragment, encode_fragment, fragment_frame,
};
