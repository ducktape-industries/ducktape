//! The huddle's media leg: the `/v1/call/ws` client the node's call hub has
//! been serving since the webview era — mic capture in, mixed playout out,
//! call control as json text frames. Binary framing is `media_service::call_wire`, the
//! single definition site; the control json mirrors `noded`'s
//! `CallClientControl`/`CallServerControl` (tag = `type`, snake_case) — the
//! app does not link the daemon crate, so the three-variant shapes are
//! restated here and drift is a wire break the e2e lane would catch.
//!
//! LIFECYCLE IS THE SUBSCRIPTION'S. `call_session` is a `stream` extern the
//! app's one subscribe block runs `when (huddle_joined && connected)`: joining
//! starts the session, leaving (or disconnecting) drops the stream, and every
//! resource follows that drop — the pump task exits when the event channel
//! closes, the websocket closes with the task, and the audio thread drops the
//! cpal streams when its shutdown sender goes with the pump. No imperative
//! stop, nothing to leak.
//!
//! AUDIO THREADING: cpal streams are not `Send`, so they live on one
//! dedicated OS thread that builds input+output and parks on a shutdown
//! channel. Capture callbacks push mono i16 into a frame accumulator and hand
//! full 20 ms frames (`media_service::voice::FRAME_SAMPLES`) to the pump over an
//! unbounded channel; playout callbacks drain a shared ring the pump fills
//! from `mixed` frames. Late audio is dead audio: the ring caps at ~200 ms
//! and drops oldest, capture frames drop when the pump is behind.

use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use media_service::call_wire;
use media_service::call_wire::CapturedFrame;
use media_service::voice::FRAME_SAMPLES;
use futures::stream::BoxStream;
use futures::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// One call-session event: `kind` picks the arm
/// (`connecting` | `live` | `refused` | `closed` | `error` | `peer` | `self`),
/// `message` carries refusal/error prose, the rest is a peer beacon — or,
/// for `self`, this side's own voice gate in `speaking`.
#[derive(Clone, Debug, Hash, PartialEq, Default)]
pub struct CallEvent {
    pub kind: String,
    pub message: String,
    pub peer: String,
    pub muted: bool,
    pub camera_on: bool,
    pub sharing: bool,
    pub speaking: bool,
}

impl CallEvent {
    fn of(kind: &str) -> Self {
        Self {
            kind: kind.to_owned(),
            ..Self::default()
        }
    }

    fn failed(kind: &str, message: impl Into<String>) -> Self {
        Self {
            kind: kind.to_owned(),
            message: message.into(),
            ..Self::default()
        }
    }

    /// This side's own voice gate flipped.
    fn speaking(open: bool) -> Self {
        Self {
            kind: "self".to_owned(),
            speaking: open,
            ..Self::default()
        }
    }
}

/// Client → hub control, mirroring `noded::CallClientControl`.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientControl {
    Recipients {
        peers: Vec<String>,
    },
    Beacon {
        muted: bool,
        camera_on: bool,
        sharing: bool,
        speaking: bool,
    },
}

/// Hub → client control, mirroring `noded::CallServerControl`.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerControl {
    KeyframeRequest,
    PeerBeacon {
        peer: String,
        muted: bool,
        camera_on: bool,
        sharing: bool,
        speaking: bool,
    },
    RateHint {
        #[allow(dead_code)]
        max_kbps: u32,
    },
}

/// The live session's steerable ends, parked globally so the flat `sync`
/// externs (mute, recipients) can reach the running pump. One session at a
/// time — the subscribe gate guarantees it.
struct Handles {
    muted: Arc<AtomicBool>,
    /// Deafened: the speaker plays silence. Local only — Discord shows a
    /// deafened peer, this beacon does not (yet).
    deafened: Arc<AtomicBool>,
    /// The mic's voice gate as the pump last read it — what our beacon says.
    speaking: Arc<AtomicBool>,
    control: tokio::sync::mpsc::UnboundedSender<ClientControl>,
}

fn handles() -> &'static Mutex<Option<Handles>> {
    static HANDLES: OnceLock<Mutex<Option<Handles>>> = OnceLock::new();
    HANDLES.get_or_init(|| Mutex::new(None))
}

/// Toggle the mic. Applies to the running session (capture frames stop while
/// muted) and beacons the new state to peers; the return value is the state
/// the view should show.
pub fn call_set_muted(muted: bool) -> bool {
    if let Some(handles) = handles().lock().expect("call handles").as_ref() {
        handles.muted.store(muted, Ordering::Relaxed);
    }
    beacon_state();
    muted
}

/// Deafen: the speaker goes silent for the running session (the mixed frames
/// still arrive and drain, so nothing stale plays on undeafen). Deafening
/// also mutes — that is the caller's step, the way Discord ties the two.
pub fn call_set_deafened(deafened: bool) -> bool {
    if let Some(handles) = handles().lock().expect("call handles").as_ref() {
        handles.deafened.store(deafened, Ordering::Relaxed);
    }
    deafened
}

/// Beacon the CURRENT local state (mute + camera) to peers — the one place
/// the beacon is assembled, called by both toggles and the session open.
pub(crate) fn beacon_state() {
    let guard = handles().lock().expect("call handles");
    let Some(handles) = guard.as_ref() else {
        return;
    };
    let source = crate::video::source();
    let _ = handles.control.send(ClientControl::Beacon {
        muted: handles.muted.load(Ordering::Relaxed),
        camera_on: source == crate::video::Source::Camera,
        sharing: source == crate::video::Source::Screen,
        speaking: handles.speaking.load(Ordering::Relaxed),
    });
}

/// The mic's voice gate: open while captured frames carry sound, and closing
/// [`VOICE_GATE_HANGOVER`] after the last one so a breath between words does
/// not flicker the badge. `push`/`expire` answer the flips only — the caller
/// beacons on a flip, not on every frame.
#[derive(Default)]
pub(crate) struct VoiceGate {
    open: bool,
    last_sound: Option<std::time::Instant>,
}

/// The RMS of a 20 ms mic frame (i16 samples) that counts as a voice —
/// about -38 dBFS, above room tone and below a whisper into the mic.
const VOICE_GATE_FLOOR: f64 = 400.;
pub(crate) const VOICE_GATE_HANGOVER: std::time::Duration = std::time::Duration::from_millis(400);

impl VoiceGate {
    pub(crate) fn push(&mut self, frame: &[i16], now: std::time::Instant) -> Option<bool> {
        if frame_rms(frame) < VOICE_GATE_FLOOR {
            return self.expire(now);
        }
        self.last_sound = Some(now);
        self.flip_to(true)
    }

    pub(crate) fn expire(&mut self, now: std::time::Instant) -> Option<bool> {
        let quiet = self
            .last_sound
            .is_none_or(|at| now.duration_since(at) >= VOICE_GATE_HANGOVER);
        match quiet {
            true => self.flip_to(false),
            false => None,
        }
    }

    fn flip_to(&mut self, open: bool) -> Option<bool> {
        if self.open == open {
            return None;
        }
        self.open = open;
        Some(open)
    }
}

fn frame_rms(frame: &[i16]) -> f64 {
    if frame.is_empty() {
        return 0.;
    }
    let energy: f64 = frame
        .iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum();
    (energy / frame.len() as f64).sqrt()
}

/// How often the live session re-reads its huddle's roster. The hub beacons
/// at 1 Hz, so this is the same order as the presence traffic it unblocks: a
/// peer who joins is admitted within a second of their join committing.
const ROSTER_POLL: std::time::Duration = std::time::Duration::from_secs(1);
/// How many roster reads in a row have to fail before the session says so on
/// its status line. One is nothing — a node mid-restart, a view a block
/// behind. Three is the fan-out standing still, which is a huddle that hears
/// nobody, and the app keeps no log for anyone to find that in.
const ROSTER_REFUSALS_VOICED: u32 = 3;

/// Steer the fan-out set from the huddle's ON-CHAIN roster, for as long as the
/// session lives. See [`crate::backend::huddle_fanout_nodes`] for why this is a
/// poll and not a push: admission is roster-gated on receive, so the peer whose
/// arrival should have re-steered the set is exactly the peer we cannot hear
/// until it is steered.
///
/// Ends with the session — the pump's control receiver drops and the send
/// fails. A failed read (node restarting, view not yet synced) keeps the last
/// set and tries again on the next tick.
///
/// THE POLL IS ALSO HOW A HUDDLE ENDS FOR SOMEONE. A peer who leaves simply
/// stops beaconing, and nothing that stops arriving can announce itself: their
/// badge, their last decoded frame and their claim on the stage would all
/// stand for the rest of the call. The set that drops them is the one place
/// that knows, so it says so — one `gone` event each, which the folds treat as
/// the peer's last word.
async fn steer_recipients(
    rpc: String,
    channel_id: String,
    control: tokio::sync::mpsc::UnboundedSender<ClientControl>,
    mut events: futures::channel::mpsc::UnboundedSender<CallEvent>,
) {
    let mut steered: Vec<String> = Vec::new();
    let mut ever_read = false;
    let mut refusals: u32 = 0;
    loop {
        match crate::backend::huddle_fanout_nodes(&rpc, &channel_id).await {
            Ok(peers) => {
                // A read that comes back after the session complained about it
                // takes the complaint down with it: an empty message is what
                // the status fold reads as a plain "live".
                let complained = refusals >= ROSTER_REFUSALS_VOICED;
                if complained && events.send(CallEvent::of("live")).await.is_err() {
                    return;
                }
                refusals = 0;
                let moved = !ever_read || peers != steered;
                if moved {
                    ever_read = true;
                    if control
                        .send(ClientControl::Recipients {
                            peers: peers.clone(),
                        })
                        .is_err()
                    {
                        return;
                    }
                    for departed in steered.iter().filter(|node| !peers.contains(node)) {
                        crate::video::forget_peer(departed);
                        let mut gone = CallEvent::of("gone");
                        gone.peer = departed.clone();
                        if events.send(gone).await.is_err() {
                            return;
                        }
                    }
                    steered = peers;
                }
            }
            // A ROSTER THIS SESSION CANNOT READ IS A SILENT HUDDLE, and the app
            // keeps no log for anyone to find it in. One reading is nothing (a
            // node mid-restart, a view a block behind); several in a row is the
            // fan-out standing still, which is the exact failure this whole
            // poll exists to end — so it says so on the status line, once.
            Err(reason) => {
                refusals += 1;
                if refusals == ROSTER_REFUSALS_VOICED
                    && events
                        .send(CallEvent::failed("live", format!("roster: {reason}")))
                        .await
                        .is_err()
                {
                    return;
                }
            }
        }
        if control.is_closed() {
            return;
        }
        tokio::time::sleep(ROSTER_POLL).await;
    }
}

/// The session stream: connect, pump, and yield state the handlers fold. The
/// stream owns everything — see the module doc's lifecycle note.
pub fn call_session(rpc: String, channel_id: String) -> BoxStream<'static, CallEvent> {
    let (events_tx, events_rx) = futures::channel::mpsc::unbounded();
    tokio::spawn(run_session(rpc, channel_id, events_tx));
    Box::pin(events_rx)
}

/// How this device proves itself to the hub. The upgrade itself is what the
/// hub admits — the call socket hands out live mic and camera bytes — and it
/// takes ONE of two proofs, decided by whether this device hosts the node.
enum Admission {
    /// This device hosts the node: its 0600 workspace secret, on the query
    /// string — the same `service-link.token` the `/v1/ws` subscribe presents.
    Workspace(String),
    /// This device is pointed at a node it does not host: the seated user
    /// key's data-plane signature over the upgrade (`GET`, the exact
    /// path+query, an empty body), as headers. The node admits it only for an
    /// account the channel's committed huddle roster names at that node —
    /// which the join that seated the key put there.
    Signed([(&'static str, String); 3]),
}

/// The path+query the upgrade goes to — and, for a signed admission, the
/// exact bytes the signature binds.
fn ws_path(channel_id: &str) -> String {
    format!("/v1/call/ws?channel={channel_id}")
}

fn ws_url(rpc: &str, path_and_query: &str) -> String {
    let base = rpc.trim_end_matches('/');
    let base = base
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    format!("{base}{path_and_query}")
}

/// The upgrade request, carrying the proof where the hub reads it.
fn ws_request(
    rpc: &str,
    channel_id: &str,
    admission: Admission,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
    let path = ws_path(channel_id);
    match admission {
        Workspace(token) => ws_url(rpc, &format!("{path}&token={token}"))
            .into_client_request()
            .map_err(|error| format!("call socket: {error}")),
        Signed(headers) => {
            let mut request = ws_url(rpc, &path)
                .into_client_request()
                .map_err(|error| format!("call socket: {error}"))?;
            for (name, value) in headers {
                let value = HeaderValue::from_str(&value)
                    .map_err(|error| format!("call socket: {error}"))?;
                request
                    .headers_mut()
                    .insert(HeaderName::from_static(name), value);
            }
            Ok(request)
        }
    }
}
use Admission::{Signed, Workspace};

/// This node's workspace secret, found the way the agent stream finds it:
/// the registered workspace whose endpoint is `rpc`. `None` when this device
/// holds no workspace for the node — a remote node.
fn workspace_secret(rpc: &str) -> Option<String> {
    let (_, workspace) = crate::backend::workspace_at(rpc)?;
    crate::backend::read_link_token(&workspace).ok()
}

/// Decide the proof: the workspace secret where this device hosts the node,
/// else the seated key's signature bound to that node's identity. A remote
/// node with the seat locked is the one way in that is closed — the join
/// that would have seated the key is the join that starts this session.
async fn admission(rpc: &str, channel_id: &str) -> Result<Admission, String> {
    if let Some(token) = workspace_secret(rpc) {
        return Ok(Workspace(token));
    }
    let status = crate::backend::rpc_client(rpc)?
        .status()
        .await
        .map_err(|error| error.to_string())?;
    let node_key = crate::backend::hex_decode(&status.public_key)?;
    let headers = crate::backend::seated_request_headers("GET", &ws_path(channel_id), &node_key, b"")
        .await
        .ok_or_else(|| {
            "this device does not host the node, and its user key is locked; unlock it to \
             huddle through a remote node"
                .to_string()
        })?;
    Ok(Signed(headers))
}

async fn run_session(
    rpc: String,
    channel_id: String,
    mut events: futures::channel::mpsc::UnboundedSender<CallEvent>,
) {
    let _ = events.send(CallEvent::of("connecting")).await;
    let request = match admission(&rpc, &channel_id).await {
        Ok(admission) => ws_request(&rpc, &channel_id, admission),
        Err(reason) => Err(format!("call socket: {reason}")),
    };
    let request = match request {
        Ok(request) => request,
        Err(reason) => {
            let _ = events.send(CallEvent::failed("error", reason)).await;
            return;
        }
    };
    let connected = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio_tungstenite::connect_async(request),
    )
    .await;
    let (socket, _) = match connected {
        Ok(Ok(pair)) => pair,
        Ok(Err(error)) => {
            let _ = events
                .send(CallEvent::failed("error", format!("call socket: {error}")))
                .await;
            return;
        }
        Err(_) => {
            let _ = events
                .send(CallEvent::failed(
                    "error",
                    "call socket: connection timed out",
                ))
                .await;
            return;
        }
    };
    let (mut ws_out, mut ws_in) = socket.split();

    let muted = Arc::new(AtomicBool::new(false));
    let (control_tx, mut control_rx) = tokio::sync::mpsc::unbounded_channel::<ClientControl>();
    let (mic_tx, mut mic_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<i16>>();
    let playout = Arc::new(Mutex::new(PlayoutRing::default()));

    let deafened = Arc::new(AtomicBool::new(false));
    let audio = AudioThread::start(muted.clone(), deafened.clone(), mic_tx, playout.clone());
    let audio_note = audio.note.clone();

    // The video leg (crate::video): camera or shared screen, one at a time.
    // Its thread mirrors the audio thread's ownership rules and dies with the
    // same teardown chain.
    let (video_tx, mut video_rx) = tokio::sync::mpsc::unbounded_channel::<CapturedFrame>();
    let _video_keepalive = video_tx.clone();
    let (capture_shutdown_tx, capture_shutdown_rx) = std::sync::mpsc::channel::<()>();
    let capture_events = events.clone();
    let capture_thread = std::thread::Builder::new()
        .name("huddle-capture".into())
        .spawn(move || crate::video::capture_thread(video_tx, capture_shutdown_rx, capture_events))
        .ok();

    let speaking = Arc::new(AtomicBool::new(false));
    let mut voice_gate = VoiceGate::default();
    // The gate's hangover has to run out even when the mic goes quiet — or
    // mutes, which stops the frames that would have closed it.
    let mut gate_tick = tokio::time::interval(VOICE_GATE_HANGOVER / 2);
    *handles().lock().expect("call handles") = Some(Handles {
        muted: muted.clone(),
        deafened,
        speaking: speaking.clone(),
        control: control_tx.clone(),
    });

    // The fan-out set is the huddle's, and the huddle's roster is on-chain —
    // this poll is the ONLY thing that puts a later joiner into it.
    tokio::spawn(steer_recipients(
        rpc.clone(),
        channel_id.clone(),
        control_tx.clone(),
        events.clone(),
    ));

    // The hub beacons our state at 1 Hz on our behalf; one push seeds it.
    beacon_state();
    let mut live = CallEvent::of("live");
    live.message = audio_note.lock().expect("audio note").clone();
    let _ = events.send(live).await;

    loop {
        tokio::select! {
            inbound = ws_in.next() => match inbound {
                Some(Ok(WsMessage::Binary(bytes))) => {
                    if let Some(frame) = call_wire::decode_audio(&bytes) {
                        playout.lock().expect("playout ring").push_frame(&frame);
                    } else if let Some(frame) = call_wire::decode_peer(&bytes) {
                        // JPEG decode is ~1–3 ms — off the pump, and dropped
                        // frames are free (the next one is a keyframe too).
                        tokio::task::spawn_blocking(move || crate::video::store_peer_frame(frame));
                    }
                }
                Some(Ok(WsMessage::Text(text))) => {
                    match serde_json::from_str::<ServerControl>(&text) {
                        Ok(ServerControl::PeerBeacon { peer, muted, camera_on, sharing, speaking }) => {
                            // A SOURCE THAT WENT OFF TAKES ITS LAST FRAME WITH
                            // IT. Nothing arrives to replace a frame after the
                            // camera stops, so the tile would hold the moment
                            // it was turned off for the rest of the call — the
                            // beacon is the only thing that says otherwise.
                            if !camera_on && !sharing {
                                crate::video::forget_peer(&peer);
                            }
                            let event = CallEvent {
                                kind: "peer".into(),
                                message: String::new(),
                                peer,
                                muted,
                                camera_on,
                                sharing,
                                speaking,
                            };
                            if events.send(event).await.is_err() {
                                break;
                            }
                        }
                        Ok(ServerControl::KeyframeRequest | ServerControl::RateHint { .. }) => {}
                        // Any non-control text frame is the hub's refusal
                        // prose, sent once before it closes the socket.
                        Err(_) => {
                            let _ = events.send(CallEvent::failed("refused", text.to_string())).await;
                            break;
                        }
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    let _ = events
                        .send(CallEvent::failed("error", format!("call socket: {error}")))
                        .await;
                    break;
                }
                None => {
                    let _ = events.send(CallEvent::of("closed")).await;
                    break;
                }
            },
            frame = mic_rx.recv() => match frame {
                Some(frame) => {
                    if let Some(open) = voice_gate.push(&frame, std::time::Instant::now()) {
                        speaking.store(open, Ordering::Relaxed);
                        beacon_state();
                        let _ = events.send(CallEvent::speaking(open)).await;
                    }
                    let encoded = call_wire::encode_audio(&frame);
                    if ws_out.send(WsMessage::Binary(encoded)).await.is_err() {
                        let _ = events.send(CallEvent::of("closed")).await;
                        break;
                    }
                }
                None => break,
            },
            frame = video_rx.recv() => match frame {
                Some(frame) => {
                    let encoded = call_wire::encode_captured(&frame);
                    if ws_out.send(WsMessage::Binary(encoded)).await.is_err() {
                        let _ = events.send(CallEvent::of("closed")).await;
                        break;
                    }
                }
                None => break,
            },
            _ = gate_tick.tick() => {
                if let Some(open) = voice_gate.expire(std::time::Instant::now()) {
                    speaking.store(open, Ordering::Relaxed);
                    beacon_state();
                    let _ = events.send(CallEvent::speaking(open)).await;
                }
            },
            control = control_rx.recv() => match control {
                Some(control) => {
                    let Ok(text) = serde_json::to_string(&control) else { continue };
                    if ws_out.send(WsMessage::Text(text)).await.is_err() {
                        let _ = events.send(CallEvent::of("closed")).await;
                        break;
                    }
                }
                None => break,
            },
        }
        // The subscription dropped the stream — the session is over.
        if events.is_closed() {
            break;
        }
    }

    *handles().lock().expect("call handles") = None;
    crate::video::reset();
    drop(capture_shutdown_tx);
    if let Some(thread) = capture_thread {
        let _ = thread.join();
    }
    drop(audio);
}

// ============================================================================
// audio — one OS thread owns the cpal streams (they are not Send)
// ============================================================================

/// Mixed frames that arrived carrying SOUND, for this process's life.
///
/// The one seam that can answer "is anybody else audible?" from outside the
/// pump: the playout ring holds samples for ~200 ms and then forgets them, and
/// the speaker is the only other place they go. The live huddle lane
/// (`tests::huddle_live`) waits on this the way it waits on a decoded frame
/// for the picture — a silent huddle is the failure it exists to catch.
static VOICE_HEARD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Anything above this in a mixed frame is somebody talking. The far end's
/// SILK decoder converges to near-zero on silence, so the floor only has to
/// clear the decoder's own noise.
const AUDIBLE: i16 = 1000;

/// How many mixed frames with sound in them this process has received.
#[cfg(test)]
pub(crate) fn voice_frames_heard() -> u64 {
    VOICE_HEARD.load(Ordering::Relaxed)
}

/// The playout ring: mixed 20 ms frames in, device-rate samples out. Caps at
/// ~200 ms and drops oldest — late audio is dead audio.
#[derive(Default)]
struct PlayoutRing {
    samples: VecDeque<i16>,
}

const PLAYOUT_CAP: usize = FRAME_SAMPLES * 10;

impl PlayoutRing {
    fn push_frame(&mut self, frame: &[i16]) {
        if frame.iter().any(|sample| sample.abs() > AUDIBLE) {
            VOICE_HEARD.fetch_add(1, Ordering::Relaxed);
        }
        self.samples.extend(frame);
        while self.samples.len() > PLAYOUT_CAP {
            self.samples.pop_front();
        }
    }

    fn drain_into(&mut self, out: &mut [i16]) {
        for slot in out.iter_mut() {
            *slot = self.samples.pop_front().unwrap_or(0);
        }
    }

    /// Drain a deafened session too — the ring must not hold 200 ms of stale
    /// voice for the undeafen — but hand the speaker silence.
    fn drain_into_unless(&mut self, out: &mut [i16], deafened: bool) {
        self.drain_into(out);
        if deafened {
            out.fill(0);
        }
    }
}

/// Accumulates mono 48 kHz i16 samples into exact voice frames.
#[derive(Default)]
pub struct FrameAccumulator {
    buffer: Vec<i16>,
}

impl FrameAccumulator {
    pub fn push(&mut self, samples: impl IntoIterator<Item = i16>) -> Vec<Vec<i16>> {
        self.buffer.extend(samples);
        let mut frames = Vec::new();
        while self.buffer.len() >= FRAME_SAMPLES {
            frames.push(self.buffer.drain(..FRAME_SAMPLES).collect());
        }
        frames
    }
}

/// Fold an interleaved buffer to mono i16: channels average per sample tick.
pub fn interleaved_to_mono(samples: &[i16], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        return samples.to_vec();
    }
    samples
        .chunks_exact(channels)
        .map(|tick| {
            let sum: i32 = tick.iter().map(|sample| i32::from(*sample)).sum();
            (sum / tick.len() as i32) as i16
        })
        .collect()
}

/// f32 sample to i16, clamped.
pub fn f32_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32767.0) as i16
}

/// A linear resampler from `from_rate` to 48 kHz mono.
// ponytail: linear interpolation, fine for voice; swap for a windowed-sinc
// resampler if capture quality ever matters more than simplicity.
pub struct Resampler {
    step: f64,
    phase: f64,
    last: i16,
}

impl Resampler {
    pub fn new(from_rate: u32) -> Self {
        Self {
            step: f64::from(from_rate) / 48_000.0,
            phase: 0.0,
            last: 0,
        }
    }

    pub fn push(&mut self, input: &[i16]) -> Vec<i16> {
        if (self.step - 1.0).abs() < f64::EPSILON {
            if let Some(last) = input.last() {
                self.last = *last;
            }
            return input.to_vec();
        }
        let mut output = Vec::with_capacity((input.len() as f64 / self.step) as usize + 2);
        for sample in input {
            // Emit every 48 kHz tick that lands before this input sample.
            while self.phase < 1.0 {
                let mixed =
                    f64::from(self.last) * (1.0 - self.phase) + f64::from(*sample) * self.phase;
                output.push(mixed as i16);
                self.phase += self.step;
            }
            self.phase -= 1.0;
            self.last = *sample;
        }
        output
    }
}

/// The audio thread's owner: dropping it signals shutdown and joins.
struct AudioThread {
    shutdown: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// What the audio layer wants the session surface to say: empty when both
    /// devices opened, otherwise a short "mic unavailable"-class note.
    note: Arc<Mutex<String>>,
}

impl AudioThread {
    fn start(
        muted: Arc<AtomicBool>,
        deafened: Arc<AtomicBool>,
        mic: tokio::sync::mpsc::UnboundedSender<Vec<i16>>,
        playout: Arc<Mutex<PlayoutRing>>,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
        let note = Arc::new(Mutex::new(String::new()));
        let thread_note = note.clone();
        let thread = std::thread::Builder::new()
            .name("huddle-audio".into())
            .spawn(move || audio_thread(muted, deafened, mic, playout, shutdown_rx, thread_note))
            .ok();
        Self {
            shutdown: Some(shutdown_tx),
            thread,
            note,
        }
    }
}

impl Drop for AudioThread {
    fn drop(&mut self) {
        self.shutdown.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn audio_thread(
    muted: Arc<AtomicBool>,
    deafened: Arc<AtomicBool>,
    mic: tokio::sync::mpsc::UnboundedSender<Vec<i16>>,
    playout: Arc<Mutex<PlayoutRing>>,
    shutdown: std::sync::mpsc::Receiver<()>,
    note: Arc<Mutex<String>>,
) {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let mut notes = Vec::new();

    // The pump's mic arm must stay pending — never closed — in listen-only
    // sessions: this keepalive holds the channel open even when no input
    // device exists, so a missing microphone degrades instead of ending the
    // session.
    let mic_keepalive = mic.clone();

    let input_stream = host.default_input_device().and_then(|device| {
        let mic = mic.clone();
        let config = device.default_input_config().ok()?;
        let channels = config.channels() as usize;
        let rate = config.sample_rate().0;
        let mut accumulator = FrameAccumulator::default();
        let mut resampler = Resampler::new(rate);
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => device.build_input_stream(
                &config.into(),
                move |data: &[f32], _| {
                    if muted.load(Ordering::Relaxed) {
                        return;
                    }
                    let ints: Vec<i16> = data.iter().copied().map(f32_to_i16).collect();
                    let mono = interleaved_to_mono(&ints, channels);
                    for frame in accumulator.push(resampler.push(&mono)) {
                        let _ = mic.send(frame);
                    }
                },
                |_| {},
                None,
            ),
            cpal::SampleFormat::I16 => device.build_input_stream(
                &config.into(),
                move |data: &[i16], _| {
                    if muted.load(Ordering::Relaxed) {
                        return;
                    }
                    let mono = interleaved_to_mono(data, channels);
                    for frame in accumulator.push(resampler.push(&mono)) {
                        let _ = mic.send(frame);
                    }
                },
                |_| {},
                None,
            ),
            _ => return None,
        };
        let stream = stream.ok()?;
        stream.play().ok()?;
        Some(stream)
    });
    if input_stream.is_none() {
        notes.push("no microphone");
    }

    let output_stream = host.default_output_device().and_then(|device| {
        let config = device.default_output_config().ok()?;
        let channels = config.channels() as usize;
        let rate = config.sample_rate().0;
        // The hub mixes at 48 kHz; a device at another rate gets the nearest
        // sample (playout quality follows the ponytail note on Resampler).
        let step = 48_000.0 / f64::from(rate);
        let ring = playout;
        let mut phase = 0.0_f64;
        let mut current = 0i16;
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => device.build_output_stream(
                &config.into(),
                move |data: &mut [f32], _| {
                    let ticks = data.len() / channels.max(1);
                    let mut mono = vec![0i16; ((ticks as f64) * step).ceil() as usize];
                    ring.lock()
                        .expect("playout ring")
                        .drain_into_unless(&mut mono, deafened.load(Ordering::Relaxed));
                    let mut source = mono.into_iter();
                    for tick in data.chunks_exact_mut(channels.max(1)) {
                        phase += step;
                        while phase >= 1.0 {
                            current = source.next().unwrap_or(0);
                            phase -= 1.0;
                        }
                        let value = f32::from(current) / 32768.0;
                        for slot in tick {
                            *slot = value;
                        }
                    }
                },
                |_| {},
                None,
            ),
            cpal::SampleFormat::I16 => device.build_output_stream(
                &config.into(),
                move |data: &mut [i16], _| {
                    let ticks = data.len() / channels.max(1);
                    let mut mono = vec![0i16; ((ticks as f64) * step).ceil() as usize];
                    ring.lock()
                        .expect("playout ring")
                        .drain_into_unless(&mut mono, deafened.load(Ordering::Relaxed));
                    let mut source = mono.into_iter();
                    for tick in data.chunks_exact_mut(channels.max(1)) {
                        phase += step;
                        while phase >= 1.0 {
                            current = source.next().unwrap_or(0);
                            phase -= 1.0;
                        }
                        for slot in tick {
                            *slot = current;
                        }
                    }
                },
                |_| {},
                None,
            ),
            _ => return None,
        };
        let stream = stream.ok()?;
        stream.play().ok()?;
        Some(stream)
    });
    if output_stream.is_none() {
        notes.push("no speaker");
    }

    *note.lock().expect("audio note") = notes.join(" · ");

    // Park until the session drops the sender; the streams die with the frame.
    let _ = shutdown.recv();
    drop(mic_keepalive);
    drop(input_stream);
    drop(output_stream);
}

// ============================================================================
// state folds — the flat handlers' arms live here
// ============================================================================

/// The status line after `event`: connecting → live (with the audio note
/// folded in) → refused/error prose → closed.
pub fn call_status_after(current: String, event: CallEvent) -> String {
    match event.kind.as_str() {
        "connecting" => "connecting".into(),
        "live" if event.message.is_empty() => "live".into(),
        "live" => format!("live · {}", event.message),
        "refused" | "error" => event.message,
        "closed" => "closed".into(),
        _ => current,
    }
}

/// One peer beacon folded into the presence list, keyed by node key. A
/// session's end (closed/refused/error) clears it — stale badges on the next
/// session's tiles would be someone else's state.
pub fn apply_call_peer(peers: Vec<CallEvent>, event: CallEvent) -> Vec<CallEvent> {
    match event.kind.as_str() {
        "peer" => {
            let mut peers: Vec<CallEvent> = peers
                .into_iter()
                .filter(|peer| peer.peer != event.peer)
                .collect();
            peers.push(event);
            peers
        }
        // Someone left the huddle. A beacon is the only thing that can update
        // a peer's row and theirs have stopped, so without this their badges,
        // their frame and their claim on the stage outlive them.
        "gone" => peers
            .into_iter()
            .filter(|peer| peer.peer != event.peer)
            .collect(),
        "closed" | "refused" | "error" => Vec::new(),
        _ => peers,
    }
}

/// Any live video in the call — this device's own source, or any peer
/// beaconing a camera or a share — gates the tile strip and its repaint tick.
pub fn call_video_live_after(peers: Vec<CallEvent>, camera: bool, sharing: bool) -> bool {
    camera || sharing || peers.iter().any(|peer| peer.camera_on || peer.sharing)
}

/// WHO HOLDS THE STAGE — the one participant whose video is a screen, so the
/// panel can show it whole instead of cropping a desktop into a 4:3 thumbnail.
/// Empty means nobody is sharing and there is no stage.
///
/// A PEER'S SHARE OUTRANKS OUR OWN. Both can be true — nothing stops two
/// people sharing at once — and of the two pictures, the one you have not
/// already got on your screen is the one worth the space. Ours still appears
/// (as [`crate::video::SELF_STAGE`]) when it is the only one, because a
/// sharer with no view of what they published is sharing blind.
pub fn huddle_stage_peer(peers: Vec<CallEvent>, local_sharing: bool) -> String {
    let remote = peers.into_iter().find(|peer| peer.sharing);
    match remote {
        Some(peer) => peer.peer,
        None if local_sharing => crate::video::SELF_STAGE.to_string(),
        None => String::new(),
    }
}

/// One huddle tile with its mute and voice decisions already attached.
#[derive(Clone, Debug, Hash, PartialEq)]
pub struct HuddleTileRow {
    pub person: crate::backend::HuddleParticipant,
    pub muted: bool,
    pub speaking: bool,
}

/// Tile rows prepared whenever the roster, call beacons, or local mute or
/// voice gate moves.
pub fn huddle_tile_rows(
    roster: Vec<crate::backend::HuddleParticipant>,
    peers: Vec<CallEvent>,
    local_muted: bool,
    local_speaking: bool,
) -> Vec<HuddleTileRow> {
    let muted_peers: BTreeSet<String> = peers
        .iter()
        .filter(|peer| peer.muted)
        .map(|peer| peer.peer.clone())
        .collect();
    let speaking_peers = speaking_peers(&peers);
    roster
        .into_iter()
        .map(|person| HuddleTileRow {
            muted: if person.is_you {
                local_muted
            } else {
                muted_peers.contains(&person.node)
            },
            speaking: if person.is_you {
                local_speaking
            } else {
                speaking_peers.contains(&person.node)
            },
            person,
        })
        .collect()
}

/// This side's voice gate after a session event: a `self` event carries the
/// flip, a session start or end clears it, and every other event keeps it.
pub fn call_speaking_after(current: bool, event: &CallEvent) -> bool {
    match event.kind.as_str() {
        "self" => event.speaking,
        "connecting" | "closed" | "refused" | "error" => false,
        _ => current,
    }
}

/// The node keys of every peer whose beacon says they are talking — the
/// shape the chat view lights its seats from.
pub fn speaking_peers(peers: &[CallEvent]) -> Vec<String> {
    peers
        .iter()
        .filter(|peer| peer.speaking && !peer.muted)
        .map(|peer| peer.peer.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_accumulate_to_exact_voice_frames() {
        let mut accumulator = FrameAccumulator::default();
        assert!(accumulator.push(vec![1i16; FRAME_SAMPLES - 1]).is_empty());
        let frames = accumulator.push(vec![2i16; FRAME_SAMPLES + 1]);
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|frame| frame.len() == FRAME_SAMPLES));
        // The tail stays buffered for the next callback.
        let frames = accumulator.push(vec![3i16; FRAME_SAMPLES]);
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn interleaved_folds_to_mono_by_average() {
        assert_eq!(interleaved_to_mono(&[10, 20, 30, 50], 2), vec![15, 40]);
        assert_eq!(interleaved_to_mono(&[7, 8, 9], 1), vec![7, 8, 9]);
    }

    #[test]
    fn resampler_identity_and_ratio() {
        let mut same = Resampler::new(48_000);
        assert_eq!(same.push(&[1, 2, 3]), vec![1, 2, 3]);

        let mut up = Resampler::new(24_000);
        let out = up.push(&[100; 240]);
        // 24k → 48k roughly doubles the sample count.
        assert!((470..=490).contains(&out.len()), "got {}", out.len());

        let mut down = Resampler::new(96_000);
        let out = down.push(&[100; 960]);
        assert!((470..=490).contains(&out.len()), "got {}", out.len());
    }

    /// The gate opens on the first loud frame, ignores a breath inside the
    /// hangover, and closes once the hangover runs out — flips only.
    #[test]
    fn the_voice_gate_opens_on_sound_and_closes_after_the_hangover() {
        let start = std::time::Instant::now();
        let loud = [3000i16; FRAME_SAMPLES];
        let quiet = [20i16; FRAME_SAMPLES];
        let mut gate = VoiceGate::default();
        assert_eq!(gate.push(&quiet, start), None);
        assert_eq!(gate.push(&loud, start), Some(true));
        assert_eq!(gate.push(&loud, start), None);
        assert_eq!(gate.push(&quiet, start + VOICE_GATE_HANGOVER / 2), None);
        assert_eq!(gate.expire(start + VOICE_GATE_HANGOVER / 2), None);
        assert_eq!(gate.expire(start + VOICE_GATE_HANGOVER), Some(false));
        assert_eq!(gate.expire(start + VOICE_GATE_HANGOVER * 2), None);
        let talking = |peer: &str, speaking: bool, muted: bool| CallEvent {
            kind: "peer".into(),
            peer: peer.into(),
            speaking,
            muted,
            ..CallEvent::default()
        };
        assert_eq!(
            speaking_peers(&[
                talking("aa", true, false),
                talking("bb", true, true),
                talking("cc", false, false)
            ]),
            vec!["aa".to_owned()]
        );
    }

    #[test]
    fn playout_ring_caps_and_zero_fills() {
        let mut ring = PlayoutRing::default();
        ring.push_frame(&[5i16; FRAME_SAMPLES * 12]);
        assert_eq!(ring.samples.len(), PLAYOUT_CAP);
        let mut out = [1i16; 4];
        let mut empty = PlayoutRing::default();
        empty.drain_into(&mut out);
        assert_eq!(out, [0i16; 4]);
        // Deafened: the samples leave the ring and the speaker gets silence.
        let mut deaf = PlayoutRing::default();
        deaf.push_frame(&[7i16; 8]);
        let mut out = [1i16; 4];
        deaf.drain_into_unless(&mut out, true);
        assert_eq!(out, [0i16; 4]);
        assert_eq!(deaf.samples.len(), 4);
        deaf.drain_into_unless(&mut out, false);
        assert_eq!(out, [7i16; 4]);
    }

    #[test]
    fn status_and_peer_folds() {
        assert_eq!(
            call_status_after("".into(), CallEvent::of("connecting")),
            "connecting"
        );
        assert_eq!(call_status_after("x".into(), CallEvent::of("live")), "live");
        let mut live = CallEvent::of("live");
        live.message = "no microphone".into();
        assert_eq!(call_status_after("x".into(), live), "live · no microphone");
        assert_eq!(
            call_status_after("live".into(), CallEvent::failed("refused", "nope")),
            "nope"
        );

        let beacon = |peer: &str, muted: bool| CallEvent {
            kind: "peer".into(),
            peer: peer.into(),
            muted,
            ..CallEvent::default()
        };
        let peers = apply_call_peer(Vec::new(), beacon("aa", true));
        let peers = apply_call_peer(peers, beacon("bb", true));
        let peers = apply_call_peer(peers, beacon("aa", false));
        assert_eq!(peers.len(), 2);
        // A peer who left stops beaconing, so their last beacon would stand
        // for the rest of the call; the roster poll's `gone` is what ends it.
        let mut left = CallEvent::of("gone");
        left.peer = "bb".into();
        let peers = apply_call_peer(peers, left);
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].peer, "aa");
        let peers = apply_call_peer(peers, beacon("bb", true));
        let participant = |node: &str, is_you: bool| crate::backend::HuddleParticipant {
            key: node.into(),
            label: node.into(),
            initials: node.into(),
            is_agent: false,
            is_you,
            joined_at: 0,
            node: node.into(),
        };
        let rows = huddle_tile_rows(
            vec![
                participant("aa", false),
                participant("bb", false),
                participant("cc", true),
            ],
            peers,
            true,
            false,
        );
        assert!(!rows[0].muted);
        assert!(rows[1].muted);
        assert!(rows[2].muted, "the local tile reads the local mute");
    }

    #[test]
    fn the_stage_prefers_the_share_you_cannot_already_see() {
        let sharing = |peer: &str, sharing: bool| CallEvent {
            kind: "peer".into(),
            peer: peer.into(),
            sharing,
            ..CallEvent::default()
        };
        // Nobody sharing: no stage, whatever the cameras are doing.
        assert!(huddle_stage_peer(vec![sharing("aa", false)], false).is_empty());
        // A peer's share takes it.
        assert_eq!(
            huddle_stage_peer(vec![sharing("aa", false), sharing("bb", true)], false),
            "bb"
        );
        // Ours alone is staged too — a sharer with no view of what they
        // published is sharing blind.
        assert_eq!(
            huddle_stage_peer(vec![sharing("aa", false)], true),
            crate::video::SELF_STAGE
        );
        // Both at once: the picture we do NOT already have on screen wins.
        assert_eq!(huddle_stage_peer(vec![sharing("bb", true)], true), "bb");
        // And any live source at all lights the strip.
        assert!(call_video_live_after(Vec::new(), false, true));
        let live = |peers: Vec<CallEvent>| call_video_live_after(peers, false, false);
        assert!(live(vec![sharing("bb", true)]));
        assert!(!live(vec![sharing("bb", false)]));
    }

    #[test]
    fn control_json_is_the_daemon_wire_verbatim() {
        // These literals ARE the `/v1/call/ws` text-frame contract
        // (`noded::CallClientControl` / `CallServerControl`); this pin is
        // what catches a serde-attribute drift on either side.
        assert_eq!(
            serde_json::to_string(&ClientControl::Recipients {
                peers: vec!["aa".into()]
            })
            .unwrap(),
            r#"{"type":"recipients","peers":["aa"]}"#
        );
        assert_eq!(
            serde_json::to_string(&ClientControl::Beacon {
                muted: true,
                camera_on: false,
                sharing: false,
                speaking: false
            })
            .unwrap(),
            r#"{"type":"beacon","muted":true,"camera_on":false,"sharing":false,"speaking":false}"#
        );
        let beacon: ServerControl = serde_json::from_str(
            r#"{"type":"peer_beacon","peer":"bb","muted":false,"camera_on":true,"sharing":false,"speaking":true}"#,
        )
        .unwrap();
        assert!(matches!(
            beacon,
            ServerControl::PeerBeacon {
                camera_on: true,
                ..
            }
        ));
        assert!(matches!(
            serde_json::from_str::<ServerControl>(r#"{"type":"rate_hint","max_kbps":900}"#)
                .unwrap(),
            ServerControl::RateHint { .. }
        ));
        // The refusal path depends on prose NOT parsing as control.
        assert!(serde_json::from_str::<ServerControl>("this node runs no call hub").is_err());
    }

    #[test]
    fn a_workspace_admission_swaps_scheme_only_and_carries_the_secret_on_the_query() {
        let request = ws_request("http://127.0.0.1:8844/", "eng", Workspace("s3cret".into()))
            .unwrap();
        assert_eq!(
            request.uri().to_string(),
            "ws://127.0.0.1:8844/v1/call/ws?channel=eng&token=s3cret"
        );
        assert!(request.headers().get("x-ducktape-sig").is_none());
        let request =
            ws_request("https://node.example", "general", Workspace("s3cret".into())).unwrap();
        assert_eq!(
            request.uri().to_string(),
            "wss://node.example/v1/call/ws?channel=general&token=s3cret"
        );
    }

    /// A remote node's admission: the trio rides as headers over a query with
    /// NO token, and the signed path is exactly the path the request goes to
    /// — the node verifies the signature against what it received.
    #[test]
    fn a_signed_admission_carries_the_trio_as_headers_over_the_exact_path() {
        use commonware_cryptography::Signer as _;
        let signer = commonware_cryptography::ed25519::PrivateKey::from_seed(3);
        let node_key = [9u8; 32];
        let headers =
            ::node::signed_req::request_headers(&signer, "GET", &ws_path("eng"), &node_key, b"");
        let request = ws_request("http://127.0.0.1:8844", "eng", Signed(headers.clone())).unwrap();
        assert_eq!(
            request.uri().to_string(),
            "ws://127.0.0.1:8844/v1/call/ws?channel=eng"
        );
        assert_eq!(request.uri().path_and_query().unwrap().as_str(), ws_path("eng"));
        for (name, value) in headers {
            assert_eq!(request.headers().get(name).unwrap(), value.as_str());
        }
        // the websocket handshake headers the client fills in are still there.
        assert!(request.headers().get("sec-websocket-key").is_some());
    }
}
