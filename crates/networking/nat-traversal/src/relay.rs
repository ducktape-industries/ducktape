//! TCP relay fallback for UDP-dead joiners.
//!
//! The whole join path is UDP-only, so on a network that eats outbound UDP
//! (hostile café wifi) a joiner can never deliver its sealed first-contact
//! intro. This lane is the fix: the joiner connects to the coordinator over
//! TCP (deployed on 443, the port every network forwards), sends an
//! authenticated [`RelayIntro`] naming a target member and carrying an OPAQUE
//! sealed payload, and the relay forwards that payload to the member's current
//! reflexive as one UDP datagram — then pumps whatever UDP datagrams come back
//! (the member's sealed IntroAck) down the TCP stream as [`RelayFrame::Forwarded`]
//! frames. From the member's perspective this is indistinguishable from the
//! coordinated intro path; the member needs zero changes.
//!
//! Trust model: the relay is UNTRUSTED infrastructure, exactly like the
//! coordinator. It moves sealed bytes it cannot read and must not try to;
//! authentication of the JOIN itself stays end-to-end (invite token + seal +
//! in-consensus Redeem). The relay lane's own authenticator exists ONLY to
//! gate who may use the relay (anti-abuse), mirroring the UDP [`AuthPolicy`].
//!
//! Targets resolve from the SAME [`SharedAdverts`] book the UDP rendezvous
//! maintains — never a second registry. Telemetry is the [`RelayMetrics`]
//! counters plus `ducktape::reachability` events: a counter says how many
//! sessions were refused, the events say which peer and for what reason. Every
//! event on this lane is peer-driven, so refusals are latched and nothing here
//! logs per forwarded frame.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use arrayvec::ArrayVec;
use commonware_cryptography::ed25519;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{Semaphore, mpsc};
use tokio::time::{Instant, timeout};

pub use crate::advert::SharedAdverts;
use crate::advert::{Bucket, source_bucket};
use crate::auth::{
    AuthError, AuthPolicy, Authenticator, CoordCap, DEFAULT_FRESHNESS_WINDOW_SECS, now_secs,
    sign_authenticator, verify_request,
};
use crate::wire::{
    MAX_CAP_LEN, NodeKey, Reader, WireError, put, put_cap, put_key, put_u16, put_u64,
};
use crate::{Latch, short_key};

// ---------------------------------------------------------------------------
// Framing constants

// Relay frame tags start at 16: tags 1-11 are the UDP `Msg`/`AuthRequest`
// encodings and 8/9 + 12-15 are permanently reserved (wire.rs). Keeping the
// relay strictly above them means a relay frame body can NEVER alias a UDP
// message — load-bearing, because the PoP signing namespace (`COORD_REQ_NS`)
// is shared: a signature minted over a relay intro must not verify as a UDP
// request, nor vice versa.
pub(crate) const TAG_RELAY_INTRO: u8 = 16;
pub(crate) const TAG_RELAY_FORWARDED: u8 = 17;
pub(crate) const TAG_RELAY_ERROR: u8 = 18;

/// Max frame BODY length (the u16 BE length prefix counts body bytes only).
/// Comfortably covers the largest [`RelayIntro`] (~1.6 KiB with a cap and a
/// full payload); anything longer is a protocol violation and closes the
/// stream.
pub const MAX_FRAME_LEN: usize = 2048;

/// Relay payload cap: one UDP datagram's worth. The relay forwards the sealed
/// payload as a SINGLE datagram to the member, so a payload that would not fit
/// an unfragmented datagram on a conservative path MTU could never be
/// delivered anyway.
pub const MAX_RELAY_PAYLOAD: usize = 1400;

// ---------------------------------------------------------------------------
// Session limits (each is a hard cap; the relay is abuse-facing by design)

/// Global concurrent-session cap: bounds relay memory and socket use under a
/// connect flood — 256 in-flight joins is far beyond any real burst.
pub const MAX_RELAY_SESSIONS: usize = 256;
/// Per-IP concurrent-session cap: one joiner needs one session; a handful
/// covers a NAT'd venue without letting a single host soak the global budget.
pub const MAX_SESSIONS_PER_IP: usize = 4;
/// Session TTL: the join gate settles in <=30 s; 90 s covers retransmits
/// without letting an abandoned session pin its UDP socket forever.
pub const SESSION_TTL: Duration = Duration::from_secs(90);
/// Per-frame read timeout: a live joiner retransmits every ~2 s, so 15 s of
/// TCP silence means the peer is gone.
pub const FRAME_READ_TIMEOUT: Duration = Duration::from_secs(15);
/// Forward budget in EACH direction: a join needs a handful of datagrams; 64
/// stops an abusive pump long before it costs anything.
pub const MAX_SESSION_FORWARDS: u32 = 64;
/// Pacing floor between joiner->member forwards: the joiner retransmits every
/// 2 s, so 250 ms only ever bites abuse (a too-fast retransmit is dropped, not
/// fatal).
pub const MIN_FORWARD_GAP: Duration = Duration::from_millis(250);
/// Cap on retry Intro frames RECEIVED, forwarded or not. `MAX_SESSION_FORWARDS`
/// only bounds frames that clear the pacing floor; a frame the floor drops
/// still costs a channel recv and a frame decode, so a flood that never once
/// clears `MIN_FORWARD_GAP` would otherwise run the full `SESSION_TTL` for
/// free. Sized at 4x `MAX_SESSION_FORWARDS` — generous slack over what a
/// legitimate joiner's ~2 s cadence produces (~45 frames in 90 s), tight
/// enough to close a flood long before it matters.
pub const MAX_SESSION_FRAMES: u32 = MAX_SESSION_FORWARDS * 4;
/// Bound on one relay->joiner frame write. A joiner that stops READING its
/// TCP stream would otherwise park the session inside an unbounded
/// `write_all` once the kernel send buffer fills — a state the TTL branch
/// cannot preempt, because the select is not polling while the write is
/// awaited inline.
const FORWARD_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Cap on refusal writes in flight at once. A `session_limit` refusal frame
/// is a few bytes, so a generous handful is plenty for real traffic; it
/// exists only to bound the worst case — a peer that never reads leaves its
/// accepted socket (and the task writing to it) alive for up to 5 s, and the
/// accept loop that spawns these tasks does not itself slow down under a
/// connect flood. Past this many outstanding writes, a refused socket is
/// dropped with no frame at all: closer to what a flood deserves than an
/// unbounded task per connection.
const MAX_PENDING_REFUSAL_WRITES: usize = 32;

// ---------------------------------------------------------------------------
// Error reasons — stable snake_case tokens (greppable and countable), never
// prose, per the logging doctrine.

/// The target member has no live advert in the book.
pub const REASON_TARGET_UNREGISTERED: &[u8] = b"target_unregistered";
/// The intro's authenticator failed against the relay's policy.
pub const REASON_NOT_AUTHORIZED: &[u8] = b"not_authorized";
/// The peer sent something that is not a well-formed frame for its position.
pub const REASON_MALFORMED: &[u8] = b"malformed";
/// The global or per-IP session cap refused the connection.
pub const REASON_SESSION_LIMIT: &[u8] = b"session_limit";

// ---------------------------------------------------------------------------
// Frames

/// The joiner's opening (and retransmitted) frame: relay `payload` to the
/// member `target`. The auth trio is exactly [`Authenticator`], signed via
/// [`sign_authenticator`] over [`RelayIntro::core_bytes`] and verified via
/// [`verify_request`] with subject = `caller` — the same stateless gate the
/// UDP requests use, over byte-disjoint input.
#[derive(Clone, Debug, PartialEq)]
pub struct RelayIntro {
    /// The authenticating identity — the key whose signer produced the PoP.
    pub caller: NodeKey,
    /// The member whose current reflexive the relay resolves and forwards to.
    pub target: NodeKey,
    /// The sealed first-contact intro. OPAQUE to the relay: it moves these
    /// bytes and must never interpret them.
    pub payload: Vec<u8>,
    pub auth: Authenticator,
}

/// One TCP frame on the relay stream: u16 BE body-length prefix, then a
/// tagged body in the wire.rs encoding idiom.
// large_enum_variant: an Intro is ~430 bytes of inline signature material.
// Frames are transient (decoded, acted on, dropped — never collected), so
// boxing would buy nothing but a per-frame allocation.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum RelayFrame {
    /// joiner -> relay.
    Intro(RelayIntro),
    /// relay -> joiner: one member datagram, pumped back verbatim.
    Forwarded { payload: Vec<u8> },
    /// relay -> joiner: refusal, as one of the `REASON_*` tokens.
    Error { reason: Vec<u8> },
}

/// Decode/IO-boundary failures for one relay frame.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("frame length {0} exceeds MAX_FRAME_LEN")]
    Oversize(usize),
    #[error("relay payload exceeds MAX_RELAY_PAYLOAD")]
    PayloadTooLong,
    #[error(transparent)]
    Wire(#[from] WireError),
}

/// Largest encoded [`RelayIntro`] body: tag + caller + target + payload
/// (len-prefixed, capped) + timestamp + pop_sig + cap option. Fits well under
/// [`MAX_FRAME_LEN`], so every frame encodes on the stack.
const MAX_INTRO_LEN: usize = 1 + 32 + 32 + 2 + MAX_RELAY_PAYLOAD + 8 + 64 + MAX_CAP_LEN;
const _: () = assert!(MAX_INTRO_LEN <= MAX_FRAME_LEN);

/// tag ‖ caller ‖ target ‖ payload_len ‖ payload — the CORE bytes the PoP
/// signs and the relay verifies. Deliberately excludes the authenticator
/// itself (which carries the signature) and starts with a tag >= 16, so the
/// signed bytes can never parse as a UDP request.
fn write_core<const CAP: usize>(
    out: &mut ArrayVec<u8, CAP>,
    caller: &NodeKey,
    target: &NodeKey,
    payload: &[u8],
) {
    out.push(TAG_RELAY_INTRO);
    put_key(out, caller);
    put_key(out, target);
    put_u16(out, payload.len() as u16);
    put(out, payload);
}

impl RelayIntro {
    /// The bytes the PoP covers — see [`write_core`]. Panics (capacity
    /// `expect`, like every wire.rs encoder) if `payload` exceeds
    /// [`MAX_RELAY_PAYLOAD`]: that is a caller bug, not peer input.
    pub fn core_bytes(&self) -> Vec<u8> {
        let mut out = ArrayVec::<u8, MAX_INTRO_LEN>::new();
        write_core(&mut out, &self.caller, &self.target, &self.payload);
        out.into_iter().collect()
    }
}

/// Build a signed [`RelayIntro`]: sign the core bytes with the caller's
/// identity key, attaching `cap` under a private policy (or `None` for
/// public/PoP-only). `payload` must fit [`MAX_RELAY_PAYLOAD`].
pub fn sign_relay_intro(
    signer: &ed25519::PrivateKey,
    caller: NodeKey,
    target: NodeKey,
    payload: Vec<u8>,
    timestamp: u64,
    cap: Option<CoordCap>,
) -> RelayIntro {
    let mut core = ArrayVec::<u8, MAX_INTRO_LEN>::new();
    write_core(&mut core, &caller, &target, &payload);
    let auth = sign_authenticator(signer, &core, timestamp, cap);
    RelayIntro {
        caller,
        target,
        payload,
        auth,
    }
}

fn read_payload(r: &mut Reader) -> Result<Vec<u8>, FrameError> {
    let len = r.u16()? as usize;
    if len > MAX_RELAY_PAYLOAD {
        return Err(FrameError::PayloadTooLong);
    }
    Ok(r.take(len)?.to_vec())
}

impl RelayFrame {
    /// Encode one frame BODY into a stack-backed vector (the length prefix is
    /// the stream framer's job — [`write_frame`]).
    pub fn encode_inline(&self) -> ArrayVec<u8, MAX_FRAME_LEN> {
        let mut out = ArrayVec::new();
        match self {
            RelayFrame::Intro(intro) => {
                write_core(&mut out, &intro.caller, &intro.target, &intro.payload);
                put_u64(&mut out, intro.auth.timestamp);
                put(&mut out, intro.auth.pop_sig.as_ref());
                put_cap(&mut out, intro.auth.cap.as_ref());
            }
            RelayFrame::Forwarded { payload } => {
                out.push(TAG_RELAY_FORWARDED);
                put_u16(&mut out, payload.len() as u16);
                put(&mut out, payload);
            }
            RelayFrame::Error { reason } => {
                out.push(TAG_RELAY_ERROR);
                out.push(reason.len() as u8);
                put(&mut out, reason);
            }
        }
        out
    }

    pub fn encode(&self) -> Vec<u8> {
        self.encode_inline().into_iter().collect()
    }

    /// Decode one frame body. Whole-buffer semantics match `Msg::decode`:
    /// trailing garbage after a well-formed frame is rejected outright.
    pub fn decode(buf: &[u8]) -> Result<RelayFrame, FrameError> {
        if buf.len() > MAX_FRAME_LEN {
            return Err(FrameError::Oversize(buf.len()));
        }
        let mut r = Reader::new(buf);
        let tag = r.take(1)?[0];
        let frame = match tag {
            TAG_RELAY_INTRO => {
                let caller = r.key()?;
                let target = r.key()?;
                let payload = read_payload(&mut r)?;
                let timestamp = r.u64()?;
                let pop_sig = r.sig()?;
                let cap = r.cap()?;
                RelayFrame::Intro(RelayIntro {
                    caller,
                    target,
                    payload,
                    auth: Authenticator {
                        timestamp,
                        pop_sig,
                        cap,
                    },
                })
            }
            TAG_RELAY_FORWARDED => RelayFrame::Forwarded {
                payload: read_payload(&mut r)?,
            },
            TAG_RELAY_ERROR => {
                let len = r.take(1)?[0] as usize;
                RelayFrame::Error {
                    reason: r.take(len)?.to_vec(),
                }
            }
            other => return Err(WireError::BadTag(other).into()),
        };
        if r.remaining() != 0 {
            return Err(WireError::Trailing.into());
        }
        Ok(frame)
    }
}

// ---------------------------------------------------------------------------
// Stream framing (shared by client and server)

fn invalid_data(error: FrameError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error)
}

/// Write one length-prefixed frame as a SINGLE `write_all` (no partial
/// interleave between prefix and body).
pub async fn write_frame<W: AsyncWrite + Unpin>(
    stream: &mut W,
    frame: &RelayFrame,
) -> std::io::Result<()> {
    let body = frame.encode_inline();
    let mut framed = ArrayVec::<u8, { 2 + MAX_FRAME_LEN }>::new();
    put(&mut framed, &(body.len() as u16).to_be_bytes());
    put(&mut framed, &body);
    stream.write_all(&framed).await
}

/// Read the next length-prefixed frame. An oversize declared length or a
/// malformed body surfaces as `ErrorKind::InvalidData` (peer misbehavior);
/// everything else is transport failure/EOF.
pub async fn read_frame<R: AsyncRead + Unpin>(stream: &mut R) -> std::io::Result<RelayFrame> {
    let mut prefix = [0u8; 2];
    stream.read_exact(&mut prefix).await?;
    let len = u16::from_be_bytes(prefix) as usize;
    if len > MAX_FRAME_LEN {
        return Err(invalid_data(FrameError::Oversize(len)));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await?;
    RelayFrame::decode(&body).map_err(invalid_data)
}

/// A joiner's TCP connection to the relay. Transport ONLY — framing plus
/// bounded connect/read timeouts; pacing, retransmits, and the join protocol
/// itself belong to the joiner (stage B), not here.
pub struct RelayConn {
    stream: TcpStream,
}

impl RelayConn {
    /// Dial the relay with a bounded connect timeout.
    pub async fn connect(relay: SocketAddr, connect_timeout: Duration) -> std::io::Result<Self> {
        let stream = timeout(connect_timeout, TcpStream::connect(relay))
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "relay connect timed out")
            })??;
        // Frames are tiny and latency-sensitive (a 2 s retransmit cadence
        // must not be Nagle-delayed behind an unacked frame).
        let _ = stream.set_nodelay(true);
        Ok(Self { stream })
    }

    /// Encode and send one frame.
    pub async fn send(&mut self, frame: &RelayFrame) -> std::io::Result<()> {
        write_frame(&mut self.stream, frame).await
    }

    /// Read the next frame within `read_timeout`.
    pub async fn recv(&mut self, read_timeout: Duration) -> std::io::Result<RelayFrame> {
        timeout(read_timeout, read_frame(&mut self.stream))
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "relay read timed out")
            })?
    }
}

// ---------------------------------------------------------------------------
// Metrics

#[derive(Default)]
struct RelayMetricsInner {
    sessions_opened: AtomicU64,
    sessions_rejected: AtomicU64,
    forwards: AtomicU64,
    replies: AtomicU64,
    expired: AtomicU64,
    refusal_writes_dropped: AtomicU64,
}

/// Cheap live counters for the relay lane, mirroring `CoordinatorMetrics`:
/// the crate stays log-free, so a sampled snapshot is the relay's only
/// telemetry.
#[derive(Clone, Default)]
pub struct RelayMetrics(Arc<RelayMetricsInner>);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelayMetricsSnapshot {
    /// Sessions whose intro was accepted and first forward sent.
    pub sessions_opened: u64,
    /// Connections refused before opening (auth, resolution, caps, garbage).
    pub sessions_rejected: u64,
    /// joiner -> member datagrams sent.
    pub forwards: u64,
    /// member -> joiner datagrams pumped back.
    pub replies: u64,
    /// Sessions that hit the TTL.
    pub expired: u64,
    /// A `session_limit` refusal that was never told to the peer: the
    /// bounded refusal-write budget ([`MAX_PENDING_REFUSAL_WRITES`]) was
    /// exhausted, so the socket was dropped in silence instead of spawning
    /// another write task.
    pub refusal_writes_dropped: u64,
}

impl RelayMetrics {
    pub fn snapshot(&self) -> RelayMetricsSnapshot {
        let load = |value: &AtomicU64| value.load(Ordering::Relaxed);
        RelayMetricsSnapshot {
            sessions_opened: load(&self.0.sessions_opened),
            sessions_rejected: load(&self.0.sessions_rejected),
            forwards: load(&self.0.forwards),
            replies: load(&self.0.replies),
            expired: load(&self.0.expired),
            refusal_writes_dropped: load(&self.0.refusal_writes_dropped),
        }
    }

    fn increment(value: &AtomicU64) {
        value.fetch_add(1, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Session accounting

#[derive(Default)]
struct SessionTable {
    total: usize,
    // Keyed by Bucket, not IpAddr: an IPv6 /64 is one host (RFC 6177), same
    // as the advert book's per-source cap (#1505) — see `source_bucket`.
    per_ip: std::collections::HashMap<Bucket, usize>,
}

/// RAII admission slot: holds this session's place in the global and per-IP
/// counts, released on Drop so EVERY exit path (refusal, TTL, io error, even
/// a panic unwinding the task) frees it.
struct SessionSlot {
    table: Arc<Mutex<SessionTable>>,
    bucket: Bucket,
}

fn lock_table(table: &Mutex<SessionTable>) -> std::sync::MutexGuard<'_, SessionTable> {
    // Same poisoning stance as SharedAdverts: a counter table's worst partial
    // state is an off-by-one; keep admitting joins.
    table.lock().unwrap_or_else(PoisonError::into_inner)
}

impl SessionSlot {
    fn try_acquire(table: &Arc<Mutex<SessionTable>>, ip: std::net::IpAddr) -> Option<Self> {
        let bucket = source_bucket(ip);
        let mut t = lock_table(table);
        let ip_count = t.per_ip.get(&bucket).copied().unwrap_or(0);
        if t.total >= MAX_RELAY_SESSIONS || ip_count >= MAX_SESSIONS_PER_IP {
            return None;
        }
        t.total += 1;
        *t.per_ip.entry(bucket).or_insert(0) += 1;
        Some(Self {
            table: table.clone(),
            bucket,
        })
    }
}

impl Drop for SessionSlot {
    fn drop(&mut self) {
        let mut t = lock_table(&self.table);
        t.total = t.total.saturating_sub(1);
        if let Some(count) = t.per_ip.get_mut(&self.bucket) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                t.per_ip.remove(&self.bucket);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Server

/// Reply-source gate: a datagram pumped back to the joiner must come from the
/// member's advertised IP. The PORT may legitimately differ — a symmetric NAT
/// allocates a fresh mapping per destination, and the relay's per-session
/// socket is a destination the member never spoke to before — but the IP is
/// stable across those mappings, so a datagram from any other IP (a third
/// party spraying the relay's ephemeral port) is dropped, never forwarded.
fn reply_source_matches(advert: SocketAddr, src: SocketAddr) -> bool {
    advert.ip() == src.ip()
}

fn verify_intro(policy: &AuthPolicy, intro: &RelayIntro) -> Result<(), AuthError> {
    verify_request(
        policy,
        now_secs(),
        DEFAULT_FRESHNESS_WINDOW_SECS,
        intro.caller,
        &intro.core_bytes(),
        &intro.auth,
    )
}

/// Why the TCP reader stopped: `Malformed` is peer misbehavior worth naming
/// back; everything else (timeout, io error, EOF) just closes.
enum ReadEnd {
    Malformed,
    Disconnected,
}

/// Read frames off the TCP stream into a channel so the session loop can
/// `select!` over them cancel-safely (`read_exact` mid-frame is NOT
/// cancel-safe; an mpsc recv is). Enforces the per-frame read timeout.
async fn pump_frames(mut read: OwnedReadHalf, frames: mpsc::Sender<Result<RelayFrame, ReadEnd>>) {
    loop {
        let event = match timeout(FRAME_READ_TIMEOUT, read_frame(&mut read)).await {
            Ok(Ok(frame)) => Ok(frame),
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::InvalidData => {
                Err(ReadEnd::Malformed)
            }
            // io error / EOF / 15 s of TCP silence: the peer is gone.
            _ => Err(ReadEnd::Disconnected),
        };
        let stop = event.is_err();
        if frames.send(event).await.is_err() || stop {
            return;
        }
    }
}

/// How one session ended: `Refused` sends the token as a [`RelayFrame::Error`]
/// before closing; `Closed` just closes.
enum SessionEnd {
    Refused(&'static [u8]),
    Closed,
}

struct Session {
    frames: mpsc::Receiver<Result<RelayFrame, ReadEnd>>,
    write: OwnedWriteHalf,
    policy: Arc<AuthPolicy>,
    adverts: SharedAdverts,
    metrics: RelayMetrics,
}

impl Session {
    /// The session state machine. One joiner, one member: the first frame
    /// pins {caller, target}; every later intro is a retransmit of that pair.
    async fn drive(&mut self) -> SessionEnd {
        // --- First frame: MUST be an authorized intro for a live target.
        // Every pre-open exit counts a reject; `sessions_opened` only counts
        // sessions that actually reached the forwarding state.
        let frame = match self.frames.recv().await {
            Some(Ok(frame)) => frame,
            Some(Err(ReadEnd::Malformed)) => {
                RelayMetrics::increment(&self.metrics.0.sessions_rejected);
                return SessionEnd::Refused(REASON_MALFORMED);
            }
            // The peer connected and left (or went silent) without a frame.
            Some(Err(ReadEnd::Disconnected)) | None => {
                RelayMetrics::increment(&self.metrics.0.sessions_rejected);
                return SessionEnd::Closed;
            }
        };
        let RelayFrame::Intro(intro) = frame else {
            RelayMetrics::increment(&self.metrics.0.sessions_rejected);
            return SessionEnd::Refused(REASON_MALFORMED);
        };
        if verify_intro(&self.policy, &intro).is_err() {
            RelayMetrics::increment(&self.metrics.0.sessions_rejected);
            return SessionEnd::Refused(REASON_NOT_AUTHORIZED);
        }
        let Some(mut target_addr) = self.adverts.current(intro.target, now_secs()) else {
            RelayMetrics::increment(&self.metrics.0.sessions_rejected);
            return SessionEnd::Refused(REASON_TARGET_UNREGISTERED);
        };

        // --- Open: bind the PER-SESSION UDP socket and forward the payload.
        // A dedicated socket per session is what lets the reply pump attribute
        // inbound datagrams to exactly one joiner. It must share the target's
        // address family — a v4-bound socket cannot send to a v6 advert.
        let bind_addr = if target_addr.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        };
        let Ok(udp) = UdpSocket::bind(bind_addr).await else {
            return SessionEnd::Closed;
        };
        if udp.send_to(&intro.payload, target_addr).await.is_err() {
            return SessionEnd::Closed;
        }
        RelayMetrics::increment(&self.metrics.0.sessions_opened);
        RelayMetrics::increment(&self.metrics.0.forwards);
        tracing::debug!(
            target: "ducktape::reachability",
            event = "relay_session_opened",
            caller = short_key(intro.caller),
            member = short_key(intro.target),
            member_reflexive = %target_addr,
            bytes = intro.payload.len(),
            "relaying a sealed intro to a member"
        );
        let caller = intro.caller;
        let target = intro.target;
        let mut forwards: u32 = 1;
        let mut replies: u32 = 0;
        let mut frames: u32 = 0;
        let mut frames_dropped: u32 = 0;
        let mut last_forward = Instant::now();
        let deadline = Instant::now() + SESSION_TTL;
        let mut buf = [0u8; MAX_FRAME_LEN];

        loop {
            tokio::select! {
                event = self.frames.recv() => {
                    match event {
                        None | Some(Err(ReadEnd::Disconnected)) => return SessionEnd::Closed,
                        Some(Err(ReadEnd::Malformed)) => return SessionEnd::Refused(REASON_MALFORMED),
                        Some(Ok(RelayFrame::Intro(retry))) => {
                            // A session is ONE joiner -> ONE member; a retransmit
                            // naming any other pair is a protocol violation.
                            if retry.caller != caller || retry.target != target {
                                return SessionEnd::Refused(REASON_MALFORMED);
                            }
                            // Frame budget FIRST: bounds sheer frame churn on
                            // this connection, independent of whether frames
                            // clear pacing or the forward budget below.
                            frames += 1;
                            if frames > MAX_SESSION_FRAMES {
                                note_frame_budget_exceeded(caller, target, frames_dropped);
                                return SessionEnd::Closed;
                            }
                            // Pacing floor next, still ahead of the ed25519
                            // verify: the joiner's 2 s cadence never trips
                            // this, a flood does, and is dropped without
                            // malice or a signature check.
                            if last_forward.elapsed() < MIN_FORWARD_GAP {
                                frames_dropped += 1;
                                continue;
                            }
                            if forwards >= MAX_SESSION_FORWARDS {
                                return SessionEnd::Closed;
                            }
                            // The verify is the expensive gate, so it runs
                            // last, only for a frame that already cleared
                            // pacing and the forward budget.
                            if verify_intro(&self.policy, &retry).is_err() {
                                return SessionEnd::Refused(REASON_NOT_AUTHORIZED);
                            }
                            // Re-resolve: the member may have rebound (new
                            // reflexive) since the last forward.
                            let Some(addr) = self.adverts.current(target, now_secs()) else {
                                return SessionEnd::Refused(REASON_TARGET_UNREGISTERED);
                            };
                            target_addr = addr;
                            if udp.send_to(&retry.payload, target_addr).await.is_err() {
                                return SessionEnd::Closed;
                            }
                            forwards += 1;
                            last_forward = Instant::now();
                            RelayMetrics::increment(&self.metrics.0.forwards);
                        }
                        // The joiner may only ever send intros.
                        Some(Ok(_)) => return SessionEnd::Refused(REASON_MALFORMED),
                    }
                }
                received = udp.recv_from(&mut buf) => {
                    let Ok((n, src)) = received else { continue };
                    if !reply_source_matches(target_addr, src) {
                        continue;
                    }
                    // A datagram too big to ride one Forwarded frame could
                    // never be a legal intro-ack; drop rather than truncate.
                    if n > MAX_RELAY_PAYLOAD {
                        continue;
                    }
                    if replies >= MAX_SESSION_FORWARDS {
                        return SessionEnd::Closed;
                    }
                    let frame = RelayFrame::Forwarded { payload: buf[..n].to_vec() };
                    match timeout(FORWARD_WRITE_TIMEOUT, write_frame(&mut self.write, &frame)).await
                    {
                        Ok(Ok(())) => {}
                        // write error, or a joiner that stopped reading: gone.
                        Ok(Err(_)) | Err(_) => return SessionEnd::Closed,
                    }
                    replies += 1;
                    RelayMetrics::increment(&self.metrics.0.replies);
                }
                _ = tokio::time::sleep_until(deadline) => {
                    RelayMetrics::increment(&self.metrics.0.expired);
                    tracing::debug!(
                        target: "ducktape::reachability",
                        event = "relay_session_expired",
                        reason = "session_ttl",
                        caller = short_key(caller),
                        member = short_key(target),
                        forwards,
                        replies,
                        "relay session hit its TTL"
                    );
                    return SessionEnd::Closed;
                }
            }
        }
    }
}

/// A `REASON_*` token as a latch key. The tokens are ASCII by construction;
/// the fallback exists only so a future non-UTF8 token cannot silence the line.
fn reason_key(reason: &'static [u8]) -> &'static str {
    std::str::from_utf8(reason).unwrap_or("non_utf8")
}

/// One refused relay session, named. `reason` is the same stable token the
/// peer gets back in its [`RelayFrame::Error`]; latched PER REASON, because
/// refusing strangers is this lane's steady state under abuse and a connection
/// flood tripping `session_limit` must not swallow the first
/// `target_unregistered` — the one an operator is actually hunting.
fn note_refused(peer: SocketAddr, reason: &'static [u8]) {
    static REFUSALS: Latch = Latch::new();
    let reason = reason_key(reason);
    if let Some(occurrences) = REFUSALS.hit(reason) {
        tracing::warn!(
            target: "ducktape::reachability",
            event = "relay_session_refused",
            reason,
            peer = %peer,
            occurrences,
            "relay session refused"
        );
    }
}

/// One session closed for exceeding `MAX_SESSION_FRAMES`, latched: a flood
/// that never once clears `MIN_FORWARD_GAP` still drives this arm once per
/// frame, and a per-frame log line would be the very bomb this cap exists to
/// prevent.
fn note_frame_budget_exceeded(caller: NodeKey, target: NodeKey, frames_dropped: u32) {
    static EXCEEDED: Latch = Latch::new();
    if let Some(occurrences) = EXCEEDED.hit("frame_budget_exceeded") {
        tracing::warn!(
            target: "ducktape::reachability",
            event = "relay_session_frame_budget_exceeded",
            reason = "frame_budget_exceeded",
            caller = short_key(caller),
            member = short_key(target),
            frames_dropped,
            occurrences,
            "relay session closed: frame budget exceeded"
        );
    }
}

/// A `session_limit` refusal write that never happened: the bounded refusal
/// budget was exhausted, so the socket was dropped rather than spawning yet
/// another write task. Latched like every other stranger-driven refusal.
fn note_refusal_write_dropped(peer: SocketAddr) {
    static DROPPED: Latch = Latch::new();
    if let Some(occurrences) = DROPPED.hit("refusal_write_dropped") {
        tracing::debug!(
            target: "ducktape::reachability",
            event = "relay_refusal_write_dropped",
            reason = "refusal_budget_exhausted",
            peer = %peer,
            occurrences,
            "dropped a session_limit refusal write: budget exhausted"
        );
    }
}

/// Tell a refused peer it was the cap, not a dead relay — but only if a
/// refusal-write permit is free. Without one the accepted socket is simply
/// dropped (closing it): a session_limit refusal frame is a courtesy, not a
/// promise, and an unbounded task per refusal is exactly the hole the cap
/// exists to close (issue: refused connections holding a socket for up to 5 s
/// with no bound on how many exist at once).
fn spawn_refusal_write(
    permits: &Arc<Semaphore>,
    stream: TcpStream,
    peer: SocketAddr,
    metrics: &RelayMetrics,
) {
    let Ok(permit) = Arc::clone(permits).try_acquire_owned() else {
        RelayMetrics::increment(&metrics.0.refusal_writes_dropped);
        note_refusal_write_dropped(peer);
        return;
    };
    tokio::spawn(async move {
        let _permit = permit; // held for the write; released on task exit
        let mut stream = stream;
        let refusal = RelayFrame::Error {
            reason: REASON_SESSION_LIMIT.to_vec(),
        };
        let _ = timeout(Duration::from_secs(5), write_frame(&mut stream, &refusal)).await;
    });
}

async fn run_session(
    stream: TcpStream,
    peer: SocketAddr,
    slot: SessionSlot,
    policy: Arc<AuthPolicy>,
    adverts: SharedAdverts,
    metrics: RelayMetrics,
) {
    let _slot = slot; // held for the whole session; Drop releases the caps
    let _ = stream.set_nodelay(true);
    let (read_half, write_half) = stream.into_split();
    let (frames_tx, frames) = mpsc::channel(4);
    let reader = tokio::spawn(pump_frames(read_half, frames_tx));
    let mut session = Session {
        frames,
        write: write_half,
        policy,
        adverts,
        metrics,
    };
    if let SessionEnd::Refused(reason) = session.drive().await {
        note_refused(peer, reason);
        // Best-effort refusal token; a stalled peer must not pin the task.
        let error = RelayFrame::Error {
            reason: reason.to_vec(),
        };
        let _ = timeout(
            Duration::from_secs(5),
            write_frame(&mut session.write, &error),
        )
        .await;
    }
    reader.abort();
}

/// Serve the TCP relay lane. `policy` gates WHO may use the relay (the same
/// [`AuthPolicy`] the UDP loops enforce); `adverts` is the coordinator's own
/// book ([`crate::Coordinator::adverts`]) so targets resolve to wherever the
/// UDP rendezvous currently places them. Never returns; never panics on peer
/// input.
pub async fn run_relay_listener(
    listener: TcpListener,
    policy: Arc<AuthPolicy>,
    adverts: SharedAdverts,
    metrics: RelayMetrics,
) {
    let sessions: Arc<Mutex<SessionTable>> = Arc::default();
    let refusal_permits = Arc::new(Semaphore::new(MAX_PENDING_REFUSAL_WRITES));
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            // A persistent accept error (EMFILE, and every other exhaustion
            // that does not clear by itself) otherwise spins this loop
            // forever emitting nothing at all. Latched because the failure
            // repeats once per poll: unlatched it would be its own outage.
            // The errno is the whole diagnosis, so it rides the line.
            Err(err) => {
                static ACCEPT_FAILED: Latch = Latch::new();
                if let Some(occurrences) = ACCEPT_FAILED.hit("accept_failed") {
                    tracing::warn!(
                        target: "ducktape::reachability",
                        event = "relay_accept_failed",
                        reason = "accept_failed",
                        error = %err,
                        occurrences,
                        "relay accept failed"
                    );
                }
                continue;
            }
        };
        let Some(slot) = SessionSlot::try_acquire(&sessions, peer.ip()) else {
            RelayMetrics::increment(&metrics.0.sessions_rejected);
            note_refused(peer, REASON_SESSION_LIMIT);
            spawn_refusal_write(&refusal_permits, stream, peer, &metrics);
            continue;
        };
        tokio::spawn(run_session(
            stream,
            peer,
            slot,
            policy.clone(),
            adverts.clone(),
            metrics.clone(),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::mint_coord_cap;
    use crate::client::{NatClient, SocketEvent, run_coordinator_with};
    use crate::{AuthRequest, Coordinator, Msg};
    use commonware_cryptography::{Signer as _, ed25519};
    use tokio::time::{sleep, timeout};

    fn keypair(seed: u64) -> (ed25519::PrivateKey, NodeKey) {
        let signer = ed25519::PrivateKey::from_seed(seed);
        let mut key = [0u8; 32];
        key.copy_from_slice(signer.public_key().as_ref());
        (signer, NodeKey(key))
    }

    // -----------------------------------------------------------------------
    // Frame encoding

    #[test]
    fn every_frame_shape_roundtrips() {
        let (signer, caller) = keypair(1);
        let issuer = ed25519::PrivateKey::from_seed(2);
        let target = NodeKey([9u8; 32]);
        let mut cases = Vec::new();
        // Intro without a capability, with a root cap, and with the deepest
        // chain — the largest intro the frame bound must hold.
        let root = mint_coord_cap(&issuer, caller, 9_999_999);
        let deepest = (1..crate::auth::MAX_CAP_CHAIN).fold(root.clone(), |parent, _| {
            crate::auth::delegate_coord_cap(&parent, &issuer, caller, 9_999_999).unwrap()
        });
        for cap in [None, Some(root), Some(deepest)] {
            cases.push(RelayFrame::Intro(sign_relay_intro(
                &signer,
                caller,
                target,
                b"\xffsealed-intro".to_vec(),
                1234,
                cap,
            )));
        }
        cases.push(RelayFrame::Forwarded { payload: vec![] });
        cases.push(RelayFrame::Forwarded {
            payload: vec![0xab; MAX_RELAY_PAYLOAD],
        });
        for reason in [
            REASON_TARGET_UNREGISTERED,
            REASON_NOT_AUTHORIZED,
            REASON_MALFORMED,
            REASON_SESSION_LIMIT,
        ] {
            cases.push(RelayFrame::Error {
                reason: reason.to_vec(),
            });
        }
        for frame in cases {
            let bytes = frame.encode();
            assert_eq!(&frame.encode_inline()[..], &bytes[..]);
            assert_eq!(RelayFrame::decode(&bytes).expect("decode"), frame);
        }
    }

    /// A session-limit flood is this lane's loudest refusal; the line an
    /// operator came for is the first `target_unregistered`. One latch per lane
    /// would swallow it 99 times out of 100, so the latch counts per reason.
    #[test]
    fn a_session_limit_flood_does_not_swallow_the_first_unregistered_refusal() {
        let latch = Latch::new();
        assert_eq!(latch.hit(reason_key(REASON_SESSION_LIMIT)), Some(1));
        for _ in 0..1_000 {
            let _ = latch.hit(reason_key(REASON_SESSION_LIMIT));
        }
        assert_eq!(latch.hit(reason_key(REASON_TARGET_UNREGISTERED)), Some(1));
        // ... and each reason still latches down to first-and-every-Nth on its
        // own count, rather than every occurrence.
        assert_eq!(latch.hit(reason_key(REASON_TARGET_UNREGISTERED)), None);
    }

    #[test]
    fn decode_rejects_trailing_garbage_oversize_and_fat_payloads() {
        // Trailing bytes after a well-formed frame: rejected outright, same
        // rule as Msg::decode.
        let mut bytes = RelayFrame::Error {
            reason: REASON_MALFORMED.to_vec(),
        }
        .encode();
        bytes.push(0xff);
        assert_eq!(
            RelayFrame::decode(&bytes),
            Err(FrameError::Wire(WireError::Trailing))
        );

        // A body longer than the frame cap never parses.
        let oversize = vec![0u8; MAX_FRAME_LEN + 1];
        assert_eq!(
            RelayFrame::decode(&oversize),
            Err(FrameError::Oversize(MAX_FRAME_LEN + 1))
        );

        // A Forwarded frame declaring a payload fatter than one datagram.
        let mut fat = vec![TAG_RELAY_FORWARDED];
        fat.extend_from_slice(&((MAX_RELAY_PAYLOAD + 1) as u16).to_be_bytes());
        fat.extend_from_slice(&vec![0u8; MAX_RELAY_PAYLOAD + 1]);
        assert_eq!(RelayFrame::decode(&fat), Err(FrameError::PayloadTooLong));

        // Same cap on an Intro's payload field.
        let mut fat_intro = vec![TAG_RELAY_INTRO];
        fat_intro.extend_from_slice(&[0x11; 32]);
        fat_intro.extend_from_slice(&[0x22; 32]);
        fat_intro.extend_from_slice(&((MAX_RELAY_PAYLOAD + 1) as u16).to_be_bytes());
        fat_intro.extend_from_slice(&[0u8; 100]); // truncated body: cap fires first
        assert_eq!(
            RelayFrame::decode(&fat_intro),
            Err(FrameError::PayloadTooLong)
        );
    }

    #[test]
    fn relay_intro_bytes_never_alias_udp_messages() {
        // The PoP namespace is shared between UDP requests and relay intros,
        // so the byte-level disjointness (tags >= 16) is load-bearing: neither
        // the SIGNED core bytes nor the full frame body may parse as a UDP
        // Msg or AuthRequest.
        let (signer, caller) = keypair(3);
        let intro = sign_relay_intro(
            &signer,
            caller,
            NodeKey([7u8; 32]),
            b"\xffsealed-intro".to_vec(),
            1,
            None,
        );
        let core = intro.core_bytes();
        assert_eq!(Msg::decode(&core), Err(WireError::BadTag(TAG_RELAY_INTRO)));
        assert!(AuthRequest::decode(&core).is_err());
        let body = RelayFrame::Intro(intro).encode();
        assert_eq!(Msg::decode(&body), Err(WireError::BadTag(TAG_RELAY_INTRO)));
        assert!(AuthRequest::decode(&body).is_err());
    }

    #[tokio::test]
    async fn stream_framing_roundtrips_and_rejects_oversize_declared_length() {
        let (mut a, mut b) = tokio::io::duplex(4 * MAX_FRAME_LEN);
        let frame = RelayFrame::Forwarded {
            payload: vec![7u8; 100],
        };
        write_frame(&mut a, &frame).await.expect("write");
        assert_eq!(read_frame(&mut b).await.expect("read"), frame);

        // A declared length beyond the cap is refused from the PREFIX alone,
        // before any body bytes are read (or even sent).
        a.write_all(&((MAX_FRAME_LEN + 1) as u16).to_be_bytes())
            .await
            .expect("write prefix");
        let error = read_frame(&mut b).await.expect_err("oversize");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn reply_source_gate_is_ip_equality_port_agnostic() {
        let advert: SocketAddr = "203.0.113.7:4000".parse().unwrap();
        assert!(
            reply_source_matches(advert, "203.0.113.7:9999".parse().unwrap()),
            "port may differ: a symmetric NAT remaps per destination"
        );
        assert!(reply_source_matches(advert, advert));
        assert!(
            !reply_source_matches(advert, "203.0.113.8:4000".parse().unwrap()),
            "a third party's IP never rides back to the joiner"
        );
        assert!(!reply_source_matches(
            advert,
            "[2001:db8::1]:4000".parse().unwrap()
        ));
    }

    // -----------------------------------------------------------------------
    // Live rig: one Coordinator serving UDP rendezvous AND sharing its book
    // with a relay listener — the deployed topology in miniature.

    struct Rig {
        coord_addr: SocketAddr,
        relay_addr: SocketAddr,
        adverts: SharedAdverts,
        metrics: RelayMetrics,
    }

    async fn rig(policy: AuthPolicy) -> Rig {
        let coord_sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let coord_addr = coord_sock.local_addr().unwrap();
        let coord = Coordinator::with_policy(policy.clone());
        let adverts = coord.adverts();
        tokio::spawn(run_coordinator_with(
            crate::NatSocket::Owned(coord_sock),
            coord,
        ));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = listener.local_addr().unwrap();
        let metrics = RelayMetrics::default();
        tokio::spawn(run_relay_listener(
            listener,
            Arc::new(policy),
            adverts.clone(),
            metrics.clone(),
        ));
        Rig {
            coord_addr,
            relay_addr,
            adverts,
            metrics,
        }
    }

    /// Register an authenticated member over the REAL UDP path and wait until
    /// its advert is resolvable — the register datagram lands asynchronously.
    async fn register_member(rig: &Rig, seed: u64) -> (NatClient, NodeKey) {
        let (signer, key) = keypair(seed);
        let member = NatClient::bind(key, vec![rig.coord_addr], signer, None)
            .await
            .unwrap();
        member.register().await.unwrap();
        let adverts = rig.adverts.clone();
        timeout(Duration::from_secs(2), async {
            while adverts.current(key, now_secs()).is_none() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("member registration must land in the shared book");
        (member, key)
    }

    /// The member's view of a relayed intro: an opaque datagram on its NAT
    /// socket (the sealed bytes deliberately do not decode as a `Msg`).
    async fn expect_datagram(member: &NatClient) -> (SocketAddr, Vec<u8>) {
        timeout(Duration::from_secs(2), async {
            loop {
                if let SocketEvent::Datagram { src, bytes } =
                    member.recv_socket_event().await.unwrap()
                {
                    return (src, bytes);
                }
            }
        })
        .await
        .expect("member receives the relayed datagram")
    }

    async fn wait_for(
        metrics: &RelayMetrics,
        what: &str,
        check: impl Fn(RelayMetricsSnapshot) -> bool,
    ) {
        timeout(Duration::from_secs(2), async {
            while !check(metrics.snapshot()) {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}: {:?}", metrics.snapshot()));
    }

    // Opaque stand-ins for the sealed intro / intro-ack. First byte 0xff so
    // they can never decode as a Msg (the member classifies them as Datagram).
    const SEALED_INTRO: &[u8] = b"\xffopaque-sealed-intro-bytes";
    const SEALED_ACK: &[u8] = b"\xffopaque-sealed-intro-ack";

    #[tokio::test]
    async fn relayed_intro_reaches_member_and_reply_pumps_back() {
        let rig = rig(AuthPolicy::Public).await;
        let (member, member_key) = register_member(&rig, 40).await;

        // The fake member: sees the sealed bytes VERBATIM (the relay moved
        // them, never interpreted them) and answers to the observed source —
        // exactly what the coordinated intro path looks like from its side.
        let member_task = tokio::spawn(async move {
            let (src, bytes) = expect_datagram(&member).await;
            assert_eq!(bytes, SEALED_INTRO, "the relay must not touch the payload");
            member.send_datagram_to(SEALED_ACK, src).await.unwrap();
        });

        let (joiner_signer, joiner_key) = keypair(41);
        let mut conn = RelayConn::connect(rig.relay_addr, Duration::from_secs(2))
            .await
            .expect("connect");
        let intro = sign_relay_intro(
            &joiner_signer,
            joiner_key,
            member_key,
            SEALED_INTRO.to_vec(),
            now_secs(),
            None,
        );
        conn.send(&RelayFrame::Intro(intro))
            .await
            .expect("send intro");

        let frame = conn
            .recv(Duration::from_secs(5))
            .await
            .expect("reply frame");
        assert_eq!(
            frame,
            RelayFrame::Forwarded {
                payload: SEALED_ACK.to_vec()
            },
            "the member's sealed ack rides back down the TCP stream"
        );
        timeout(Duration::from_secs(2), member_task)
            .await
            .expect("member task finishes")
            .expect("member assertions hold");

        wait_for(&rig.metrics, "happy-path counters", |m| {
            m.sessions_opened == 1 && m.forwards == 1 && m.replies == 1
        })
        .await;
        assert_eq!(rig.metrics.snapshot().sessions_rejected, 0);
    }

    #[tokio::test]
    async fn retransmitted_intro_forwards_again_after_the_pacing_floor() {
        let rig = rig(AuthPolicy::Public).await;
        let (member, member_key) = register_member(&rig, 42).await;

        let (joiner_signer, joiner_key) = keypair(43);
        let mut conn = RelayConn::connect(rig.relay_addr, Duration::from_secs(2))
            .await
            .expect("connect");
        let intro = sign_relay_intro(
            &joiner_signer,
            joiner_key,
            member_key,
            SEALED_INTRO.to_vec(),
            now_secs(),
            None,
        );
        conn.send(&RelayFrame::Intro(intro.clone())).await.unwrap();
        let (_, first) = expect_datagram(&member).await;
        assert_eq!(first, SEALED_INTRO);

        // Wait out the pacing floor, then retransmit the identical intro (the
        // stage-B joiner's 2 s cadence, compressed): the member sees a SECOND
        // datagram — the session re-verified, re-resolved, and forwarded again.
        sleep(MIN_FORWARD_GAP + Duration::from_millis(50)).await;
        conn.send(&RelayFrame::Intro(intro)).await.unwrap();
        let (_, second) = expect_datagram(&member).await;
        assert_eq!(second, SEALED_INTRO);
        wait_for(&rig.metrics, "two forwards", |m| m.forwards == 2).await;
    }

    /// The exploit from the issue: one valid, signed intro replayed back to
    /// back so every retry lands inside `MIN_FORWARD_GAP` and never once
    /// clears pacing to trip `MAX_SESSION_FORWARDS`. Before the fix that ran
    /// `verify_intro` for every one of these for the full `SESSION_TTL`; now
    /// pacing runs (and drops) BEFORE verify, and the frame budget closes the
    /// session long before the TTL regardless.
    #[tokio::test]
    async fn frame_flood_that_never_clears_pacing_closes_on_the_frame_budget() {
        let rig = rig(AuthPolicy::Public).await;
        let (_member, member_key) = register_member(&rig, 50).await;

        let (joiner_signer, joiner_key) = keypair(51);
        let mut conn = RelayConn::connect(rig.relay_addr, Duration::from_secs(2))
            .await
            .unwrap();
        let intro = sign_relay_intro(
            &joiner_signer,
            joiner_key,
            member_key,
            SEALED_INTRO.to_vec(),
            now_secs(),
            None,
        );
        // Opens the session and forwards once.
        conn.send(&RelayFrame::Intro(intro.clone())).await.unwrap();

        // Flood the identical, validly-signed retry as fast as the socket
        // takes it. Every one lands well inside MIN_FORWARD_GAP, so none of
        // them can ever reach the (already-passed) MAX_SESSION_FORWARDS
        // check — only the frame budget can end this.
        for _ in 0..=MAX_SESSION_FRAMES {
            conn.send(&RelayFrame::Intro(intro.clone())).await.unwrap();
        }

        // `Closed` (not `Refused`) sends no frame back: the read side just
        // observes the relay end of the stream close.
        assert!(
            conn.recv(Duration::from_secs(2)).await.is_err(),
            "the session must close once the frame budget is exceeded"
        );

        // Only the single pacing-clearing FIRST frame was ever forwarded;
        // every flooded retry was paced out before it could re-verify or
        // re-forward.
        assert_eq!(rig.metrics.snapshot().forwards, 1);
    }

    #[tokio::test]
    async fn forged_pop_is_refused_as_not_authorized() {
        let rig = rig(AuthPolicy::Public).await;
        let (_member, member_key) = register_member(&rig, 44).await;

        // The intro claims one caller but is signed by a DIFFERENT key.
        let (_joiner_signer, joiner_key) = keypair(45);
        let (forger, _) = keypair(46);
        let intro = sign_relay_intro(
            &forger,
            joiner_key,
            member_key,
            SEALED_INTRO.to_vec(),
            now_secs(),
            None,
        );
        let mut conn = RelayConn::connect(rig.relay_addr, Duration::from_secs(2))
            .await
            .unwrap();
        conn.send(&RelayFrame::Intro(intro)).await.unwrap();
        assert_eq!(
            conn.recv(Duration::from_secs(2)).await.expect("refusal"),
            RelayFrame::Error {
                reason: REASON_NOT_AUTHORIZED.to_vec()
            }
        );
        // The refusal closes the stream.
        assert!(conn.recv(Duration::from_secs(2)).await.is_err());
        wait_for(&rig.metrics, "one reject", |m| m.sessions_rejected == 1).await;
        assert_eq!(rig.metrics.snapshot().sessions_opened, 0);
    }

    #[tokio::test]
    async fn unregistered_target_is_refused() {
        let rig = rig(AuthPolicy::Public).await;
        let (joiner_signer, joiner_key) = keypair(47);
        let intro = sign_relay_intro(
            &joiner_signer,
            joiner_key,
            NodeKey([0xEE; 32]), // nobody ever registered this key
            SEALED_INTRO.to_vec(),
            now_secs(),
            None,
        );
        let mut conn = RelayConn::connect(rig.relay_addr, Duration::from_secs(2))
            .await
            .unwrap();
        conn.send(&RelayFrame::Intro(intro)).await.unwrap();
        assert_eq!(
            conn.recv(Duration::from_secs(2)).await.expect("refusal"),
            RelayFrame::Error {
                reason: REASON_TARGET_UNREGISTERED.to_vec()
            }
        );
        wait_for(&rig.metrics, "one reject", |m| m.sessions_rejected == 1).await;
    }

    #[tokio::test]
    async fn second_intro_naming_a_different_target_closes_the_session() {
        let rig = rig(AuthPolicy::Public).await;
        let (member, member_key) = register_member(&rig, 48).await;

        let (joiner_signer, joiner_key) = keypair(49);
        let mut conn = RelayConn::connect(rig.relay_addr, Duration::from_secs(2))
            .await
            .unwrap();
        let intro = sign_relay_intro(
            &joiner_signer,
            joiner_key,
            member_key,
            SEALED_INTRO.to_vec(),
            now_secs(),
            None,
        );
        conn.send(&RelayFrame::Intro(intro)).await.unwrap();
        let (_, first) = expect_datagram(&member).await;
        assert_eq!(first, SEALED_INTRO);

        // A session is one joiner -> ONE member: naming a different target on
        // the same stream is a protocol violation, not a second join.
        let hijack = sign_relay_intro(
            &joiner_signer,
            joiner_key,
            NodeKey([0xDD; 32]),
            SEALED_INTRO.to_vec(),
            now_secs(),
            None,
        );
        conn.send(&RelayFrame::Intro(hijack)).await.unwrap();
        assert_eq!(
            conn.recv(Duration::from_secs(2)).await.expect("refusal"),
            RelayFrame::Error {
                reason: REASON_MALFORMED.to_vec()
            }
        );
        assert!(
            conn.recv(Duration::from_secs(2)).await.is_err(),
            "the mismatch closed the session"
        );
    }

    #[tokio::test]
    async fn per_ip_session_cap_refuses_the_next_connection() {
        let rig = rig(AuthPolicy::Public).await;
        // Fill the per-IP budget (loopback: one IP) with idle connections.
        let mut held = Vec::new();
        for _ in 0..MAX_SESSIONS_PER_IP {
            held.push(
                RelayConn::connect(rig.relay_addr, Duration::from_secs(2))
                    .await
                    .unwrap(),
            );
        }
        // A connect() returning does not mean the accept loop has admitted it
        // yet, so probe until the cap is actually populated: an over-cap probe
        // is told session_limit; an under-cap probe times out silently (it
        // became a session) and is dropped to free its slot.
        let mut refused = false;
        for _ in 0..50 {
            let mut probe = RelayConn::connect(rig.relay_addr, Duration::from_secs(2))
                .await
                .unwrap();
            if let Ok(frame) = probe.recv(Duration::from_millis(200)).await {
                assert_eq!(
                    frame,
                    RelayFrame::Error {
                        reason: REASON_SESSION_LIMIT.to_vec()
                    }
                );
                refused = true;
                break;
            }
            drop(probe);
            sleep(Duration::from_millis(50)).await;
        }
        assert!(refused, "the fifth same-IP connection is refused");
        assert!(rig.metrics.snapshot().sessions_rejected >= 1);
        drop(held);
    }

    #[test]
    fn per_ip_cap_buckets_ipv6_by_64_not_by_exact_address() {
        // Five DISTINCT addresses inside one /64: a single host with a routed
        // prefix (RFC 6177) can pick any of 2^64 of these for free. Bucketing
        // by the /64 (mirrors advert.rs's `source_bucket`, #1505) must count
        // them as one source, so the fifth hits MAX_SESSIONS_PER_IP (4).
        let table: Arc<Mutex<SessionTable>> = Arc::default();
        let addr_in_prefix = |host: u16| {
            std::net::IpAddr::V6(std::net::Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, host))
        };
        let mut held = Vec::new();
        for host in 0..MAX_SESSIONS_PER_IP as u16 {
            let slot = SessionSlot::try_acquire(&table, addr_in_prefix(host));
            assert!(slot.is_some(), "session {host} within the cap must admit");
            held.push(slot.unwrap());
        }
        assert!(
            SessionSlot::try_acquire(&table, addr_in_prefix(MAX_SESSIONS_PER_IP as u16)).is_none(),
            "a fifth address in the same /64 must be refused as the same source"
        );
        drop(held);
    }

    #[tokio::test]
    async fn exhausted_refusal_budget_drops_the_socket_with_no_task_and_no_frame() {
        // A permit-less semaphore models the budget already fully spent by
        // other refusals in flight.
        let permits = Arc::new(Semaphore::new(0));
        let metrics = RelayMetrics::default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connector = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (stream, peer) = listener.accept().await.unwrap();
        let mut client = connector.await.unwrap();

        spawn_refusal_write(&permits, stream, peer, &metrics);

        // If a task had been spawned it would hold the write open for up to
        // the 5 s FORWARD-style timeout; with no permit, no task exists at
        // all, so the accepted socket is simply dropped and the peer sees
        // EOF right away, not a session_limit frame.
        let mut buf = [0u8; 8];
        let n = timeout(Duration::from_millis(500), client.read(&mut buf))
            .await
            .expect("closes immediately; must not wait anywhere near the write timeout")
            .unwrap();
        assert_eq!(
            n, 0,
            "no frame is ever written when the budget is exhausted"
        );
        assert_eq!(metrics.snapshot().refusal_writes_dropped, 1);
    }
}
