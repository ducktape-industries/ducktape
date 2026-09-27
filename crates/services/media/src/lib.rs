//! The huddle media service: the real-time media planes a call rides,
//! entirely OFF consensus. Consensus (the chat module's channel membership,
//! the `CallJoin`/`CallLeave` roster) decides WHO may be in a call; every
//! byte here — an Opus frame, a VP8 fragment, a call-control datagram, a
//! `/v1/call/ws` frame — rides a data-plane flow or the webview socket and is
//! never proposed, ordered, or replicated.
//!
//! Three planes, one crate:
//! - [`voice`]: the Opus media engine over `Service::Voice` datagrams —
//!   encode, fan-out, per-speaker jitter buffering, mixed playout.
//! - [`video`]: VP8 frame fragmentation/reassembly over `Service::Video`,
//!   plus the call-control wire (keyframe kicks, presence beacons, rate
//!   hints) on the voice flow so control keeps working in an audio-only
//!   build.
//! - [`call_wire`]: the single definition of the `/v1/call/ws` binary
//!   framing the daemon and the app's webview leg both speak.

pub mod call_wire;
pub mod video;
pub mod voice;
