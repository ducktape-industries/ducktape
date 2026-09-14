//! The huddle's video leg: capture → intra-only JPEG → the call socket's
//! video frames, and inbound peer frames → decode → the tile strip and the
//! stage.
//!
//! TWO SOURCES, ONE STREAM. A participant occupies one video flow and one
//! tile, and the beacon's `camera_on`/`sharing` pair says which of the two the
//! far end is looking at — so [`Source`] is one discriminant and starting
//! either source ends the other. The camera is a device (`nokhwa`); the screen
//! is a root-window grab over X11 (`x11rb`, already in this binary under
//! winit — see [`ScreenSource`] for why not the portal).
//!
//! CODEC v1 IS BASELINE JPEG, EVERY FRAME A KEYFRAME. The wire (ws
//! `media_service::call_wire` and the mesh fragmentation in `media_service::video`) treats the
//! encoded bytes as opaque and both ends of the webview leg are THIS app, so
//! the client picks the codec. Pure-Rust JPEG keeps the build free of C
//! toolchains on every platform; intra-only means a lost frame costs nothing
//! (the next one is a sync point), so inbound `KeyframeRequest`s are
//! meaningless and never sent. The codec itself is
//! `media_service::video::codec` — NOT this crate, because its two APIs are
//! generic and would instantiate here at the app's dev `opt-level = 0`, where
//! a VGA frame cost ~90 ms of encode, decode and channel swap and the loop
//! fell behind the camera (the "video latency" under `make dev`). The seam to
//! a delta codec (VP8/AV1) is that module and `store_peer_frame`; nothing
//! else knows JPEG exists. MAX_FRAME_BYTES on the mesh is ~129 KB; 640×480 at
//! q60 runs 30–60 KB.
// ponytail: fixed 640x480-ish @ q60, wire ≤60 fps, no rate-ladder response —
// ducktape is a private-network workspace app, so the generous ceiling is
// deliberate (q60 VGA at 60 fps ≈ 2-4 MB/s per sender); wire the RateHint →
// (fps, quality) ladder when a real WAN leg complains.
//
//! THREADING mirrors the audio leg: one OS thread owns the open source (the
//! nokhwa camera is not `Send`), follows the toggle, holds a source only while
//! it is the one asked for, and dies with the session's shutdown sender.
//! Decoded peer frames land in a global store the `call_video_tiles` and
//! `call_video_stage` extern components read; both are SELF-REDRAWING widgets
//! that repaint their own window at the capture cadence — no app message, no
//! view rebuild, no other window woken.
//!
//! EVERY FRAME IS A NEW RENDERER IMAGE, AND THE RENDERER FREES NOTHING ON ITS
//! OWN. `img(Arc<RenderImage>)` uploads a sprite-atlas tile per image id and
//! keeps it until `Window::drop_image`; a call that never called it grew the
//! atlas by a VGA tile per frame, ~36 MB/s, for as long as it ran. So the
//! store RETIRES every handle it replaces, and the surfaces drop the retired
//! ones on their next paint — the one place a `Window` is in hand.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use gpui_kit::StyledImage;
use gpui_kit::{
    Context, IntoElement, ObjectFit, ParentElement, Render, RenderImage, Styled, Window, div, img,
    px,
};
use media_service::call_wire::{CapturedFrame, PeerFrame};
use media_service::video::codec;

/// Toggle/shutdown poll while no source is open. WITH A CAMERA OPEN THE LOOP
/// KEEPS NO CLOCK AT ALL: `Camera::frame()` blocks until the device has the
/// next frame, so the camera itself is the pace. A screen grab has nothing to
/// block on and takes [`SCREEN_INTERVAL`] instead — see [`capture_thread`].
const IDLE_POLL: std::time::Duration = std::time::Duration::from_millis(40);
/// The wire's send floor: at most one encoded frame per this interval
/// (~60 fps). A camera slower than this just sends every frame; a faster one
/// is thinned to it. Display and preview are NOT gated by this.
const WIRE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);
/// A screen share's frame rate — the pace of a pull, not of a device: nothing
/// blocks on a grab, so this interval IS the rate. Shared screens are read,
/// not watched: ~10/s tracks a scroll and a typed line without spending a
/// camera's bandwidth on a mostly-still picture.
const SCREEN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
/// Tile width in the strip; height follows the frame's aspect.
const TILE_WIDTH: f32 = 128.0;
/// Capture ceiling, in pixels: the documented ~VGA budget whose q60 JPEG
/// stays well under the mesh's MAX_FRAME_BYTES. A camera that only offers
/// bigger modes is box-halved down to it before the encode.
const CAPTURE_PIXEL_BUDGET: u32 = 640 * 480;
/// Decoded-tile ceiling: peers cannot allocate more than this pixel budget.
const TILE_PIXEL_BUDGET: u32 = 512 * 1024 - 1;
/// A shared screen's capture ceiling — the TILE budget, not the camera's,
/// because legibility is the whole point of a screen and the receiver cannot
/// hold more than this anyway (it would halve it again on arrival). A 1080p
/// desktop lands at 960×540: a shared editor is readable, a 4K one is not.
// ponytail: one halving of whatever the desktop is. The way past it is a
// codec that carries a still screen cheaply (delta frames), not a bigger JPEG.
const SCREEN_PIXEL_BUDGET: u32 = TILE_PIXEL_BUDGET;

/// A decoded frame owns one renderer image. Cloning it between paints keeps
/// its upload cached until the next captured frame replaces it.
struct TileFrame {
    width: u32,
    height: u32,
    handle: Arc<RenderImage>,
}

struct VideoStore {
    /// peer node-key hex → latest decoded frame.
    peers: HashMap<String, TileFrame>,
    /// the local camera's preview, when on.
    preview: Option<TileFrame>,
    /// peer node-key hex → a decode for that peer is running right now.
    /// `store_peer_frame` checks-and-sets this BEFORE decoding and clears it
    /// after, so a peer with a decode already in flight gets its next frame
    /// DROPPED rather than queued — a hostile 25 fps sender must not stack
    /// concurrent decodes on the blocking pool.
    decoding: HashSet<String>,
    /// handles a newer frame (or a departure) replaced, whose atlas tiles the
    /// next paint drops — see the module doc.
    retired: Vec<Arc<RenderImage>>,
}

impl VideoStore {
    fn retire(&mut self, frame: Option<TileFrame>) {
        self.retired.extend(frame.map(|frame| frame.handle));
    }
}

fn store() -> &'static Mutex<VideoStore> {
    static STORE: OnceLock<Mutex<VideoStore>> = OnceLock::new();
    STORE.get_or_init(|| {
        Mutex::new(VideoStore {
            peers: HashMap::new(),
            preview: None,
            decoding: HashSet::new(),
            retired: Vec::new(),
        })
    })
}

/// The handles replaced since the last paint, for `Window::drop_image`.
pub(crate) fn take_retired() -> Vec<Arc<RenderImage>> {
    std::mem::take(&mut store().lock().expect("video store").retired)
}

/// A BGRA picture as one renderer image. The renderer reads BGRA out of an
/// `RgbaImage` container, so the bytes go in as they are.
fn render_bgra(pixels: Vec<u8>, width: u32, height: u32) -> Option<Arc<RenderImage>> {
    let pixels = image::RgbaImage::from_raw(width, height, pixels)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(pixels)])))
}

/// What the video leg is sending, if anything.
///
/// ONE DISCRIMINANT, because the camera and the screen are two SOURCES for one
/// STREAM, never two streams: a participant occupies one video flow and one
/// tile, and the beacon's `camera_on`/`sharing` pair says which of the two the
/// far end is looking at. Starting a share therefore stops the camera, and
/// turning the camera on stops the share — there is no state where both are
/// true, so no state where the two could disagree about what the peer sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Off,
    Camera,
    Screen,
}

impl Source {
    fn code(self) -> u8 {
        match self {
            Source::Off => 0,
            Source::Camera => 1,
            Source::Screen => 2,
        }
    }

    fn of(code: u8) -> Self {
        match code {
            1 => Source::Camera,
            2 => Source::Screen,
            _ => Source::Off,
        }
    }
}

static SOURCE: AtomicU8 = AtomicU8::new(0);

pub(crate) fn source() -> Source {
    Source::of(SOURCE.load(Ordering::Relaxed))
}

/// Both readings of the video source, because a toggle moves BOTH: the view
/// draws a camera button and a share button, and starting either one ends the
/// other.
#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct VideoSource {
    pub camera: bool,
    pub sharing: bool,
}

/// Point the video leg at `next`. The capture thread notices on its next pass
/// (it holds no device it is not currently asked for); the beacon rides the
/// call module's control channel.
fn use_source(next: Source) -> VideoSource {
    SOURCE.store(next.code(), Ordering::Relaxed);
    // The outgoing preview belongs to the source that is ending — a camera
    // still on screen under a "sharing" beacon is a lie for one frame.
    let mut store = store().lock().expect("video store");
    let ended = store.preview.take();
    store.retire(ended);
    drop(store);
    crate::call::beacon_state();
    VideoSource {
        camera: next == Source::Camera,
        sharing: next == Source::Screen,
    }
}

/// Turn the camera on or off. On ends any screen share.
pub fn call_use_camera(on: bool) -> VideoSource {
    use_source(if on { Source::Camera } else { Source::Off })
}

/// Start or stop sharing the screen. Starting one turns the camera off.
pub fn call_use_screen(on: bool) -> VideoSource {
    use_source(if on { Source::Screen } else { Source::Off })
}

/// Clear everything at session end — the next session must not open on the
/// last call's faces.
pub(crate) fn reset() {
    let mut store = store().lock().expect("video store");
    let peers: Vec<TileFrame> = store.peers.drain().map(|(_, frame)| frame).collect();
    for frame in peers {
        store.retire(Some(frame));
    }
    let preview = store.preview.take();
    store.retire(preview);
    store.decoding.clear();
    SOURCE.store(Source::Off.code(), Ordering::Relaxed);
}

/// A peer's encoded frame off the call socket: decode and store. Runs on a
/// blocking task — JPEG decode of a 480p frame is ~1–3 ms.
///
/// DROP, NEVER QUEUE, A PEER'S SECOND FRAME WHILE ITS FIRST IS STILL
/// DECODING. `spawn_blocking` has no back-pressure of its own — a peer
/// sending faster than this box can decode (a hostile 25 fps sender, or a
/// legitimate one over a slow host) would otherwise stack concurrent decodes
/// without bound. The next frame is a keyframe too, so a dropped one costs
/// nothing.
pub(crate) fn store_peer_frame(frame: PeerFrame) {
    let peer = hex_of(&frame.peer);
    {
        let mut store = store().lock().expect("video store");
        if !store.decoding.insert(peer.clone()) {
            tracing::debug!(
                target: "ducktape::call",
                reason = "tile_decode_in_flight",
                "peer frame dropped, a decode for this peer is already running"
            );
            return;
        }
    }
    let tile = decode_frame(&frame.data);
    let mut store = store().lock().expect("video store");
    store.decoding.remove(&peer);
    let Some(tile) = tile else {
        return;
    };
    let replaced = store.peers.insert(peer, tile);
    store.retire(replaced);
}

/// Drop a peer's last frame: they left the huddle, or their beacon says the
/// source behind it is off. Frames only ever arrive, so nothing else would
/// ever take one down.
pub(crate) fn forget_peer(node: &str) {
    let mut store = store().lock().expect("video store");
    let gone = store.peers.remove(node);
    store.retire(gone);
}

fn hex_of(key: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in key {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// A peer's JPEG as a tile: refused before allocation over
/// SCREEN_PIXEL_BUDGET (the larger receive-side budget — today the tile's,
/// but a screen-share frame is the one this path must not under-bound), and
/// bounded HERE onto TILE_PIXEL_BUDGET rather than trusted to its sender —
/// above the renderer's upload cliff a fresh handle is skipped for a frame,
/// which reads as the tile blinking.
fn decode_frame(data: &[u8]) -> Option<TileFrame> {
    let Some(picture) = codec::decode_bgra(data, SCREEN_PIXEL_BUDGET, TILE_PIXEL_BUDGET) else {
        // debug, not warn: a hostile sender can repeat this every frame at
        // 25 fps, and a per-frame warn would evict the whole ring in minutes.
        tracing::debug!(
            target: "ducktape::call",
            reason = "tile_refused",
            "peer tile refused: over budget or not a picture"
        );
        return None;
    };
    let codec::Picture {
        pixels,
        width,
        height,
    } = picture;
    Some(TileFrame {
        width,
        height,
        handle: render_bgra(pixels, width, height)?,
    })
}

/// Encode one captured BGRA frame to the wire's opaque bytes. The capture
/// thread is its only product caller; the codec does the work.
pub(crate) fn encode_frame(bgra: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    codec::encode_bgra(bgra, width, height)
}

/// How the self-view is ARRIVING: frames stored, and the worst and total gap
/// between consecutive ones in microseconds.
///
/// Stutter is a distribution, not a rate — a preview averaging 30 fps with one
/// 200 ms hole in it is the complaint, and an average alone cannot see the
/// hole. These are the three numbers that can: count says how many frames the
/// capture source actually delivered, `worst_gap_us` is the hole, and
/// `total_gap_us / (count - 1)` is what the mean should have been.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PreviewPace {
    pub frames: u64,
    pub worst_gap_us: u64,
    pub total_gap_us: u64,
}

static PREVIEW_FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static PREVIEW_WORST_GAP_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static PREVIEW_TOTAL_GAP_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static PREVIEW_LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// The self-view's delivery, for this process's life.
#[cfg(test)]
pub(crate) fn preview_pace() -> PreviewPace {
    PreviewPace {
        frames: PREVIEW_FRAMES.load(Ordering::Relaxed),
        worst_gap_us: PREVIEW_WORST_GAP_US.load(Ordering::Relaxed),
        total_gap_us: PREVIEW_TOTAL_GAP_US.load(Ordering::Relaxed),
    }
}

/// Fold one arrival into the pace above. The gap is measured HERE, where the
/// frame lands in the store — the last point that belongs to capture and the
/// first the surface can read, so a slow encode or a stalled device shows up
/// and a busy renderer does not.
fn note_preview_arrival() {
    let now = std::time::Instant::now();
    PREVIEW_FRAMES.fetch_add(1, Ordering::Relaxed);
    let mut last = PREVIEW_LAST.lock().expect("preview pace");
    if let Some(previous) = last.replace(now) {
        let gap = now.duration_since(previous).as_micros() as u64;
        PREVIEW_TOTAL_GAP_US.fetch_add(gap, Ordering::Relaxed);
        PREVIEW_WORST_GAP_US.fetch_max(gap, Ordering::Relaxed);
    }
}

/// The local preview: the camera's own pixels (BGRA), taking ownership of
/// the frame the capture pass decoded.
pub(crate) fn store_preview(bgra: Vec<u8>, width: u32, height: u32) {
    let Some(handle) = render_bgra(bgra, width, height) else {
        return;
    };
    note_preview_arrival();
    let mut store = store().lock().expect("video store");
    let replaced = store.preview.replace(TileFrame {
        width,
        height,
        handle,
    });
    store.retire(replaced);
}

/// Open the camera at 640×480 and its highest frame rate, or say why.
/// Larger frames increase decoding and upload costs and can exceed the mesh's
/// `MAX_FRAME_BYTES` JPEG budget. A camera that has no VGA mode, or refuses it, gets
/// the largest mode INSIDE the capture budget instead ([`open_within_budget`]):
/// every frame is shrunk onto that budget anyway, so a bigger mode only buys
/// a bigger decode — a 1080p JPEG per frame at the camera's top rate is a
/// whole core, and it bought nothing the wire could carry.
///
/// A refusal turns the toggle back off and surfaces as "live · camera: …"
/// through the status fold; the caller has nothing to decide.
fn open_camera(
    events: &futures::channel::mpsc::UnboundedSender<crate::call::CallEvent>,
) -> Option<nokhwa::Camera> {
    use nokhwa::pixel_format::RgbAFormat;
    use nokhwa::utils::{CameraIndex, RequestedFormat, RequestedFormatType, Resolution};

    let vga = RequestedFormat::new::<RgbAFormat>(RequestedFormatType::HighestResolution(
        Resolution::new(640, 480),
    ));
    match nokhwa::Camera::new(CameraIndex::Index(0), vga)
        .or_else(|_| open_within_budget())
        .and_then(|mut device| device.open_stream().map(|()| device))
    {
        Ok(device) => Some(device),
        Err(error) => {
            refuse_source(events, format!("camera: {error}"));
            None
        }
    }
}

/// The camera in the largest mode that fits [`CAPTURE_PIXEL_BUDGET`], at that
/// mode's highest frame rate — the same rule the VGA request states, applied
/// to whatever modes the device actually offers. The device is probed at any
/// mode nokhwa will open and re-set BEFORE the stream starts, so no frame is
/// ever decoded at the probe's size.
fn open_within_budget() -> Result<nokhwa::Camera, nokhwa::NokhwaError> {
    use nokhwa::pixel_format::RgbAFormat;
    use nokhwa::utils::{CameraIndex, RequestedFormat, RequestedFormatType};

    let probe = RequestedFormat::new::<RgbAFormat>(RequestedFormatType::AbsoluteHighestFrameRate);
    let mut device = nokhwa::Camera::new(CameraIndex::Index(0), probe)?;
    let formats = device.compatible_camera_formats()?;
    if let Some(format) = budget_format(&formats, CAPTURE_PIXEL_BUDGET) {
        device.set_camera_requset(RequestedFormat::new::<RgbAFormat>(
            RequestedFormatType::Exact(format),
        ))?;
    }
    Ok(device)
}

/// Among the modes the capture can decode: the largest inside `budget` pixels
/// at its highest frame rate; when none fits, the smallest mode there is, at
/// its highest rate — the capture shrink brings that one onto the budget.
fn budget_format(
    formats: &[nokhwa::utils::CameraFormat],
    budget: u32,
) -> Option<nokhwa::utils::CameraFormat> {
    use nokhwa::pixel_format::{FormatDecoder as _, RgbAFormat};
    use nokhwa::utils::CameraFormat;

    let pixels = |format: &CameraFormat| format.width() * format.height();
    let decodable: Vec<CameraFormat> = formats
        .iter()
        .copied()
        .filter(|format| RgbAFormat::FORMATS.contains(&format.format()))
        .collect();
    let largest_within_budget = decodable
        .iter()
        .copied()
        .filter(|format| pixels(format) <= budget)
        .max_by_key(|format| (pixels(format), format.frame_rate()));
    let smallest_of_all = || {
        decodable
            .iter()
            .copied()
            .min_by_key(|format| (pixels(format), std::cmp::Reverse(format.frame_rate())))
    };
    largest_within_budget.or_else(smallest_of_all)
}

/// A source that will not open: say why on the session's status line, and put
/// the toggle back where the user can see it is off. The capture thread has
/// nothing left to decide.
fn refuse_source(
    events: &futures::channel::mpsc::UnboundedSender<crate::call::CallEvent>,
    message: String,
) {
    let _ = events.unbounded_send(crate::call::CallEvent {
        kind: "live".into(),
        message,
        ..crate::call::CallEvent::default()
    });
    SOURCE.store(Source::Off.code(), Ordering::Relaxed);
}

/// The screen source: one X11 connection, and a full-desktop grab per frame.
///
/// X11 AND PURE RUST ON PURPOSE. `x11rb` is already in this binary (winit
/// draws through it), so the desktop costs no new dependency, no C toolchain
/// and no build-time system library — which the portal/pipewire route would
/// cost on every machine that builds this app. The app itself runs natively
/// on either display server; a share is an X11-session feature, and a
/// Wayland session is refused below rather than grabbed.
// ponytail: the WHOLE root window, so a multi-head desktop shares every head
// at once — a per-monitor or per-window picker is the obvious next step and
// wants a picker UI, not a different capture.
struct ScreenSource {
    connection: x11rb::rust_connection::RustConnection,
    root: x11rb::protocol::xproto::Window,
}

impl ScreenSource {
    fn open() -> Result<Self, String> {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::ImageOrder;

        // A WAYLAND SESSION'S X SERVER IS XWAYLAND, and its root window holds
        // X clients only — a grab there is a black rectangle with this app's
        // own windows in it, never the desktop. Refusing says that; sharing it
        // would be a lie the sharer cannot see (they see their own screen).
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            return Err("screen sharing needs an X11 session, and this one is Wayland".into());
        }
        let (connection, screen) =
            x11rb::connect(None).map_err(|error| format!("no X display ({error})"))?;
        let setup = connection.setup();
        let root = setup
            .roots
            .get(screen)
            .ok_or_else(|| "the X display named no screen".to_string())?
            .root;
        // The one pixel layout `grab` reads: 32 bits per pixel, little-endian,
        // which is every TrueColor desktop this app runs on. Anything else is
        // refused rather than shipped as swapped colour.
        let depth = setup
            .roots
            .get(screen)
            .map(|screen| screen.root_depth)
            .unwrap_or_default();
        let bits = setup
            .pixmap_formats
            .iter()
            .find(|format| format.depth == depth)
            .map(|format| format.bits_per_pixel);
        let packed_bgrx = bits == Some(32) && setup.image_byte_order == ImageOrder::LSB_FIRST;
        if !packed_bgrx {
            return Err(format!(
                "this display's {depth}-bit pixel layout is not one screen sharing can read"
            ));
        }
        Ok(ScreenSource { connection, root })
    }

    /// One grab, RGBA, already inside the wire budget.
    fn grab(&self) -> Result<(Vec<u8>, u32, u32), String> {
        use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};

        // The geometry is re-read per frame: a resolution change mid-share
        // would otherwise grab a rectangle the root no longer has.
        let geometry = self
            .connection
            .get_geometry(self.root)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| error.to_string())?;
        let image = self
            .connection
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                0,
                0,
                geometry.width,
                geometry.height,
                u32::MAX,
            )
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| error.to_string())?;
        let codec::Picture {
            mut pixels,
            width,
            height,
        } = codec::shrink_to_budget(
            image.data,
            u32::from(geometry.width),
            u32::from(geometry.height),
            SCREEN_PIXEL_BUDGET,
        );
        // X hands back BGRX, which IS the renderer's and the encoder's order;
        // only the alpha byte needs writing, over the small image.
        for pixel in pixels.chunks_exact_mut(4) {
            pixel[3] = 0xff;
        }
        Ok((pixels, width, height))
    }
}

/// What the capture thread currently HOLDS, which follows [`Source`] one pass
/// behind it — a device is opened when the toggle asks for it and dropped the
/// moment it is not what is asked for.
// Half a kilobyte of X11 connection in the largest variant, held once, on one
// thread, for as long as a share lasts — boxing it would trade a pointer chase
// per frame for nothing anyone can measure.
#[allow(clippy::large_enum_variant)]
enum Open {
    None,
    Camera(nokhwa::Camera),
    Screen(ScreenSource),
}

impl Open {
    fn is(&self, source: Source) -> bool {
        match self {
            Open::None => source == Source::Off,
            Open::Camera(_) => source == Source::Camera,
            Open::Screen(_) => source == Source::Screen,
        }
    }
}

fn open_source(
    source: Source,
    events: &futures::channel::mpsc::UnboundedSender<crate::call::CallEvent>,
) -> Open {
    match source {
        Source::Off => Open::None,
        Source::Camera => open_camera(events).map_or(Open::None, Open::Camera),
        Source::Screen => match ScreenSource::open() {
            Ok(screen) => Open::Screen(screen),
            Err(reason) => {
                refuse_source(events, format!("share: {reason}"));
                Open::None
            }
        },
    }
}

/// One frame from whatever is open, BGRA and inside its source's budget. An
/// error is the source having stopped answering; the loop drops it and the
/// reopen says why if it cannot come back.
fn grab(open: &mut Open) -> Result<(Vec<u8>, u32, u32), String> {
    use nokhwa::pixel_format::RgbAFormat;

    match open {
        Open::None => Err("nothing is open".into()),
        Open::Camera(device) => {
            // Blocks until the device has a frame: this IS the loop's clock.
            let frame = device.frame().map_err(|error| error.to_string())?;
            let decoded = frame
                .decode_image::<RgbAFormat>()
                .map_err(|error| error.to_string())?;
            let (width, height) = (decoded.width(), decoded.height());
            let codec::Picture {
                mut pixels,
                width,
                height,
            } = codec::shrink_to_budget(decoded.into_raw(), width, height, CAPTURE_PIXEL_BUDGET);
            // the one swap left in the pipeline: the camera decodes RGBA, the
            // renderer and the encoder both read BGRA
            codec::rgba_to_bgra_in_place(&mut pixels);
            Ok((pixels, width, height))
        }
        Open::Screen(screen) => screen.grab(),
    }
}

/// The capture thread body: follow the source toggle, hold a device only while
/// it is the one asked for, thin the encode to the wire ceiling, hand frames to
/// the session pump. Ends when `shutdown` drops (the session's own teardown
/// chain).
///
/// THE OPEN SOURCE IS THE CLOCK, AND IT IS THE ONLY ONE. `Camera::frame()`
/// blocks until the device has the next frame, so a loop that reads it
/// back-to-back runs at exactly the negotiated rate, self-correcting, forever.
/// The version this replaced ALSO slept a frame interval before that blocking
/// read: a whole period of waiting, and then a wait for the frame after it.
/// The driver's buffers filled while we slept, every pass then took the oldest
/// one, and the self-view arrived a frame late and in bursts — the stutter, in
/// a preview that never touches the network or the codec. A screen grab is the
/// other shape: nothing to wait on, so the wait IS the frame rate — and for
/// that reason it is a DEADLINE the grab's own cost comes out of, not a nap
/// laid end to end with it.
pub(crate) fn capture_thread(
    frames: tokio::sync::mpsc::UnboundedSender<CapturedFrame>,
    shutdown: std::sync::mpsc::Receiver<()>,
    events: futures::channel::mpsc::UnboundedSender<crate::call::CallEvent>,
) {
    let mut open = Open::None;
    let started = std::time::Instant::now();
    // The wire thinning clock — see WIRE_INTERVAL. The only other clock.
    let mut last_sent: Option<std::time::Instant> = None;
    // The screen's next grab is a DEADLINE, not a nap — see the pace below.
    let mut next_grab = std::time::Instant::now();
    loop {
        // The shutdown sender dropping is the session ending, and what this
        // waits is the open source's own pace.
        //
        // A SCREEN'S INTERVAL IS A PERIOD, NOT A NAP. Grabbing a 1280×800 root
        // window, shrinking it and encoding it costs ~45 ms; sleeping the full
        // interval on top of that made a 10 fps share arrive at 6.8 (measured:
        // 146 ms mean between frames for a 100 ms interval). Waiting only what
        // is LEFT of the period puts the cost inside the frame instead of
        // after it, and a grab slower than the period simply runs flat out —
        // never a catch-up burst, because the deadline cannot fall behind now.
        let pace = match &open {
            Open::None => IDLE_POLL,
            Open::Camera(_) => std::time::Duration::ZERO,
            Open::Screen(_) => next_grab.saturating_duration_since(std::time::Instant::now()),
        };
        match shutdown.recv_timeout(pace) {
            Ok(()) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        next_grab = (next_grab + SCREEN_INTERVAL).max(std::time::Instant::now());
        let wanted = source();
        if wanted == Source::Off {
            // The device is released the moment the toggle goes off, and the
            // idle pace above becomes the toggle poll.
            open = Open::None;
            continue;
        }
        if !open.is(wanted) {
            // Whatever was open is released BEFORE the next source opens — a
            // device is never held for a source nobody asked for, and a camera
            // still streaming for the old one refuses the new open as busy.
            drop(std::mem::replace(&mut open, Open::None));
            open = open_source(wanted, &events);
            continue;
        }
        let Ok((bgra, width, height)) = grab(&mut open) else {
            // A source that stopped answering must not spin this loop: drop
            // it, and the reopen above says why if it cannot come back.
            open = Open::None;
            continue;
        };
        // The wire's copy only BORROWS the frame, so it is taken first and the
        // preview then takes ownership: the self-view mirrors every captured
        // frame regardless of the wire — it has no bandwidth to respect, and a
        // frame the encoder refuses (over the mesh cap) must not freeze it.
        let wire_due = last_sent.is_none_or(|at| at.elapsed() >= WIRE_INTERVAL);
        let encoded = wire_due
            .then(|| encode_frame(&bgra, width, height))
            .flatten();
        store_preview(bgra, width, height);
        let Some(encoded) = encoded else {
            continue;
        };
        last_sent = Some(std::time::Instant::now());
        let captured = CapturedFrame {
            keyframe: true,
            ts_ms: started.elapsed().as_millis() as u32,
            data: encoded,
        };
        if frames.send(captured).is_err() {
            break;
        }
    }
}

/// Native video surfaces read the latest decoded frame at the window's vsync.
/// Empty stores park redraws; the call roster update mounts/re-arms the surface.
pub struct VideoView {
    source: VideoDisplay,
}
enum VideoDisplay {
    Tiles(String),
    Stage(String),
}

pub fn call_video_tiles(staged: &str) -> VideoView {
    VideoView {
        source: VideoDisplay::Tiles(staged.to_owned()),
    }
}
pub fn call_video_stage(peer: &str) -> VideoView {
    VideoView {
        source: VideoDisplay::Stage(peer.to_owned()),
    }
}
impl VideoView {
    pub fn replace_tiles(&mut self, staged: String, cx: &mut Context<Self>) {
        self.source = VideoDisplay::Tiles(staged);
        cx.notify();
    }
    pub fn replace_stage(&mut self, peer: String, cx: &mut Context<Self>) {
        self.source = VideoDisplay::Stage(peer);
        cx.notify();
    }
}
impl Render for VideoView {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // the frames a newer one replaced since the last paint give their
        // atlas tiles back here, the one place a Window is in hand
        for handle in take_retired() {
            let _ = window.drop_image(handle);
        }
        match &self.source {
            VideoDisplay::Stage(peer) => {
                let Some((_, _, handle)) = stage_frame(peer) else {
                    return div().into_any_element();
                };
                window.request_animation_frame();
                img(handle)
                    .w_full()
                    .h_auto()
                    .object_fit(ObjectFit::Contain)
                    .rounded(px(8.))
                    .into_any_element()
            }
            VideoDisplay::Tiles(staged) => {
                let tiles = tiles_snapshot(staged);
                if !tiles.is_empty() {
                    window.request_animation_frame();
                }
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .gap(px(TILE_GAP))
                    .children(tiles.into_iter().map(|(_, _, handle)| {
                        img(handle)
                            .w(px(TILE_WIDTH))
                            .h(px(TILE_HEIGHT))
                            .object_fit(ObjectFit::Cover)
                            .rounded(px(6.))
                    }))
                    .into_any_element()
            }
        }
    }
}

/// The stage's stand-in for "the screen this device is sharing" — a sentinel
/// no node key can collide with (they are 64 hex characters).
pub const SELF_STAGE: &str = "you";

/// The staged frame's size and handle: a peer's by node key, or the local
/// preview under [`SELF_STAGE`]. `pub(crate)` for the live huddle lane, which
/// asks the store the same question the stage does: is this peer's picture
/// here yet?
pub(crate) fn stage_frame(peer: &str) -> Option<(u32, u32, Arc<RenderImage>)> {
    let store = store().lock().expect("video store");
    let frame = match peer {
        SELF_STAGE => store.preview.as_ref(),
        key => store.peers.get(key),
    }?;
    Some((frame.width, frame.height, frame.handle.clone()))
}

/// Displayed tile plate: fixed 4:3, the frame Cover-cropped onto it, wrapped
/// into rows on the strip's width.
const TILE_HEIGHT: f32 = 96.0;
const TILE_GAP: f32 = 8.0;
/// Peers in stable key order, the local preview last — the same order the
/// row-based strip always drew, minus whatever the stage is showing whole.
/// `Handle` is `Bytes`-backed (Arc) and its `Id` survives the clone, so each
/// entry is a refcount bump that keeps pointing at the renderer's cached
/// upload.
fn tiles_snapshot(staged: &str) -> Vec<(u32, u32, Arc<RenderImage>)> {
    let store = store().lock().expect("video store");
    let mut ordered: Vec<(&String, &TileFrame)> = store
        .peers
        .iter()
        .filter(|(node, _)| node.as_str() != staged)
        .collect();
    ordered.sort_by(|a, b| a.0.cmp(b.0));
    let mut tiles: Vec<_> = ordered
        .into_iter()
        .map(|(_, frame)| (frame.width, frame.height, frame.handle.clone()))
        .collect();
    if staged != SELF_STAGE
        && let Some(preview) = &store.preview
    {
        tiles.push((preview.width, preview.height, preview.handle.clone()));
    }
    tiles
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The codec's own tests own the round trip and the crafted-SOF refusal;
    /// this end is the budgets: a within-budget frame decodes AT its size
    /// (flat grey, so the encode stays under the mesh cap and never halves)
    /// and one declaring more than SCREEN_PIXEL_BUDGET is refused.
    #[test]
    fn a_peer_frame_decodes_inside_the_budget_and_is_refused_over_it() {
        let (width, height) = (700u32, 700u32);
        let bgra = vec![0x80u8; (width * height * 4) as usize];
        let encoded = encode_frame(&bgra, width, height).expect("encode");
        let tile = decode_frame(&encoded).expect("a within-budget frame must still decode");
        assert_eq!((tile.width, tile.height), (700, 700));
        assert_eq!(
            tile.handle.as_bytes(0).expect("frame pixels").len(),
            700 * 700 * 4
        );
        let (width, height) = (1280u32, 720u32);
        let bgra = vec![0x80u8; (width * height * 4) as usize];
        let encoded = encode_frame(&bgra, width, height).expect("encode");
        assert!(decode_frame(&encoded).is_none(), "720p is over the budget");
    }

    /// THE FALLBACK NEVER DECODES BIGGER THAN THE BUDGET. A camera that refuses
    /// VGA used to be opened at its fastest mode at its highest resolution —
    /// every frame a 1080p JPEG decode that the shrink then threw most of away.
    #[test]
    fn the_camera_fallback_is_the_largest_mode_inside_the_budget() {
        use nokhwa::utils::{CameraFormat, FrameFormat, Resolution};
        let mode = |w: u32, h: u32, fps: u32, format: FrameFormat| {
            CameraFormat::new(Resolution::new(w, h), format, fps)
        };
        let webcam = [
            mode(1920, 1080, 60, FrameFormat::MJPEG),
            mode(1280, 720, 60, FrameFormat::MJPEG),
            mode(640, 360, 30, FrameFormat::MJPEG),
            mode(640, 360, 60, FrameFormat::MJPEG),
            mode(320, 240, 60, FrameFormat::YUYV),
            // a format the capture cannot decode is never a candidate
            mode(640, 480, 60, FrameFormat::GRAY),
        ];
        assert_eq!(
            budget_format(&webcam, CAPTURE_PIXEL_BUDGET),
            Some(mode(640, 360, 60, FrameFormat::MJPEG)),
            "the largest mode inside the budget, at its highest rate"
        );
        // Nothing inside the budget: the smallest mode there is, at its
        // highest rate — the shrink brings it onto the budget.
        let big = [
            mode(1920, 1080, 30, FrameFormat::MJPEG),
            mode(1280, 720, 30, FrameFormat::MJPEG),
            mode(1280, 720, 60, FrameFormat::MJPEG),
        ];
        assert_eq!(
            budget_format(&big, CAPTURE_PIXEL_BUDGET),
            Some(mode(1280, 720, 60, FrameFormat::MJPEG))
        );
        assert_eq!(budget_format(&[], CAPTURE_PIXEL_BUDGET), None);
    }

    /// One global store AND one global source, so this stays ONE test, in
    /// sequence — and it carries the blink's property: a stored frame owns ONE
    /// renderer handle, so every view rebuild between two captures reads the
    /// same id and the renderer keeps its upload. Only a new frame is a new id.
    #[test]
    fn the_store_folds_frames_and_the_source_is_one_choice() {
        let preview_id = || {
            store()
                .lock()
                .unwrap()
                .preview
                .as_ref()
                .map(|frame| frame.handle.id)
        };
        // "" is "nothing is staged" — the strip's ordinary reading.
        reset();
        assert!(tiles_snapshot("").is_empty());
        store_preview(vec![10, 20, 30, 0xff], 1, 1);
        assert_eq!(tiles_snapshot("").len(), 1);
        let first = preview_id().expect("preview");
        assert_eq!(first, preview_id().expect("preview"));
        store_preview(vec![40, 50, 60, 0xff], 1, 1);
        assert_ne!(first, preview_id().expect("preview"));
        // ...and the replaced frame is RETIRED for the next paint to drop, so
        // its atlas tile does not outlive it (the leak that grew a call's
        // atlas by a tile per frame).
        let retired = take_retired();
        assert_eq!(
            retired.iter().map(|handle| handle.id).collect::<Vec<_>>(),
            vec![first]
        );
        assert!(take_retired().is_empty());

        // The staged frame preserves the original dimensions for native
        // image aspect layout, rather than the tile's fixed crop.
        store_preview(vec![0xff; 8], 2, 1);
        assert!(stage_frame(SELF_STAGE).is_some());
        let (width, height, _) = stage_frame(SELF_STAGE).unwrap();
        assert_eq!((width, height), (2, 1));
        assert!(stage_frame("a-peer-nobody-sent").is_none());
        // ...and what the stage shows whole, the strip leaves out, so a
        // desktop is not also a cropped plate beside itself.
        assert!(tiles_snapshot(SELF_STAGE).is_empty());
        assert_eq!(tiles_snapshot("").len(), 1);

        // ONE SOURCE: starting either one ends the other, and either one off
        // is off — there is no state where both are live.
        let camera = call_use_camera(true);
        assert_eq!(source(), Source::Camera);
        assert!(camera.camera && !camera.sharing);
        let screen = call_use_screen(true);
        assert_eq!(source(), Source::Screen);
        assert!(screen.sharing && !screen.camera);
        // ...and the outgoing source's last frame goes with it, so the tile
        // strip cannot paint a camera under a "sharing" beacon.
        assert!(preview_id().is_none());
        let off = call_use_screen(false);
        assert_eq!(source(), Source::Off);
        assert!(!off.camera && !off.sharing);

        // A peer's frame arriving while a decode for that SAME peer is
        // already running is DROPPED, never queued. `store_peer_frame`
        // checks-and-sets the in-flight marker before it ever touches the
        // bytes, so pre-arming that marker here stands in for a real decode
        // still running on another blocking-pool thread.
        reset();
        let peer_key = [7u8; 32];
        let peer_hex = hex_of(&peer_key);
        store()
            .lock()
            .expect("video store")
            .decoding
            .insert(peer_hex.clone());
        store_peer_frame(PeerFrame {
            peer: peer_key,
            keyframe: true,
            ts_ms: 0,
            data: Vec::new(),
        });
        assert!(
            !store()
                .lock()
                .expect("video store")
                .peers
                .contains_key(&peer_hex),
            "a frame arriving mid-decode must be dropped, not stored"
        );

        reset();
        assert!(preview_id().is_none());
        assert_eq!(source(), Source::Off);
    }
}
