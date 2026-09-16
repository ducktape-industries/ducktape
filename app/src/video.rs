//! Native camera/screen capture, JPEG codec and renderer image resources.
//! Devices expose encoded images; deployed guests own transport and peer identity.
//! Captures and decoded images are bounded, and their session owns their lifetime.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use gpui_kit::RenderImage;
#[derive(serde::Serialize)]
pub(crate) struct CapturedImage {
    pub preview: &'static str,
    pub timestamp_ms: u32,
    pub jpeg: Vec<u8>,
}
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
    /// `store_image` checks-and-sets this BEFORE decoding and clears it
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

/// What a share captures. A SHARE IS A CHOICE, NOT "THE DESKTOP": a two-head
/// desktop glued into one strip and shrunk onto [`SCREEN_PIXEL_BUDGET`] is
/// illegible, and the thing someone means to present is usually one window —
/// where the same budget buys several times the legibility.
///
/// A HEAD AND A WINDOW ARE NAMED, NEVER A CACHED RECTANGLE. The index and the
/// window id are re-resolved on EVERY grab, so a head that changes resolution
/// and a window that moves or resizes mid-share both keep working.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShareTarget {
    /// Every head at once — the root window whole.
    #[default]
    Desktop,
    /// One head, by its place in the platform's monitor list.
    Monitor(usize),
    /// One top-level window, by the platform's window id.
    Window(u32),
}

/// A share target and the label the picker shows for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareChoice {
    pub target: ShareTarget,
    pub label: String,
}

/// The target the next [`Source::Screen`] opens, beside [`SOURCE`] and for the
/// same reason: the capture thread reads both, and the app writes both before
/// the capture the deployed call guest asks for ever starts. One process shares
/// one thing.
fn share_target() -> &'static Mutex<ShareTarget> {
    static TARGET: OnceLock<Mutex<ShareTarget>> = OnceLock::new();
    TARGET.get_or_init(|| Mutex::new(ShareTarget::default()))
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
pub(crate) fn use_source(next: Source) -> VideoSource {
    SOURCE.store(next.code(), Ordering::Relaxed);
    // The outgoing preview belongs to the source that is ending — a camera
    // still on screen under a "sharing" beacon is a lie for one frame.
    let mut store = store().lock().expect("video store");
    let ended = store.preview.take();
    store.retire(ended);
    drop(store);
    VideoSource {
        camera: next == Source::Camera,
        sharing: next == Source::Screen,
    }
}

/// Turn the camera on or off. On ends any screen share.
pub fn call_use_camera(on: bool) -> VideoSource {
    crate::call::set_video_source(if on { "camera" } else { "off" });
    VideoSource { camera: on, sharing: false }
}

/// Start sharing `target`, or stop with `None`. Starting one turns the camera
/// off. THE TARGET IS STORED BEFORE THE SOURCE MOVES, so the capture thread —
/// which the call guest starts a round trip later — can never see
/// [`Source::Screen`] without the target it belongs to.
pub fn call_use_screen(target: Option<ShareTarget>) -> VideoSource {
    let Some(target) = target else {
        crate::call::set_video_source("off");
        return VideoSource { camera: false, sharing: false };
    };
    *share_target().lock().expect("share target") = target;
    crate::call::set_video_source("screen");
    VideoSource { camera: false, sharing: true }
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
pub(crate) fn store_image(peer: String, jpeg: Vec<u8>, alive: &std::sync::atomic::AtomicBool) {
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
    let tile = decode_frame(&jpeg);
    let mut store = store().lock().expect("video store");
    store.decoding.remove(&peer);
    if !alive.load(Ordering::Acquire) { return; }
    let Some(tile) = tile else { return; };
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

fn encode_bounded(bgra: &[u8], mut width: u32, mut height: u32, max_bytes: usize) -> Option<Vec<u8>> {
    let mut pixels = std::borrow::Cow::Borrowed(bgra);
    loop {
        if let Some(encoded) = encode_frame(&pixels, width, height)
            && encoded.len() <= max_bytes {
            return Some(encoded);
        }
        if width <= 1 || height <= 1 { return None; }
        let smaller = codec::halve(&pixels, width, height);
        pixels = std::borrow::Cow::Owned(smaller.pixels);
        width = smaller.width;
        height = smaller.height;
    }
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
    events: &tokio::sync::mpsc::Sender<String>,
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
    events: &tokio::sync::mpsc::Sender<String>,
    message: String,
) {
    let _ = events.try_send(message);
    SOURCE.store(Source::Off.code(), Ordering::Relaxed);
}

/// The displays a share may pick on macOS, in the order
/// [`ShareTarget::Monitor`] indexes them. Windows are not offered: capturing
/// one wants `CGWindowListCopyWindowInfo` to enumerate and
/// `CGWindowListCreateImage` to read, and neither can be exercised from the
/// Linux hosts this is developed on.
// ponytail: displays only on macOS. Add windows when a Mac is in the loop to
// verify them — the picker and the target enum already carry them.
#[cfg(target_os = "macos")]
pub fn call_share_targets() -> Result<Vec<ShareChoice>, String> {
    use core_graphics::display::CGDisplay;

    let displays =
        CGDisplay::active_displays().map_err(|error| format!("no display to share ({error})"))?;
    if displays.is_empty() {
        return Err("this Mac reports no active display".into());
    }
    let main = CGDisplay::main().id;
    let choices = displays
        .iter()
        .enumerate()
        .map(|(index, id)| {
            let bounds = CGDisplay::new(*id).bounds();
            let primary = if *id == main { " (main)" } else { "" };
            ShareChoice {
                target: ShareTarget::Monitor(index),
                label: format!(
                    "Display {}{primary} — {}×{}",
                    index + 1,
                    bounds.size.width as i64,
                    bounds.size.height as i64
                ),
            }
        })
        .collect();
    Ok(choices)
}

/// The screen source on macOS: one display, one `CGDisplayCreateImage` per
/// frame. `core-graphics` is already in this binary under gpui, so the
/// desktop costs no new dependency. The image comes back in device pixels
/// (a Retina desktop is 4× the points) as 32-bit BGRX rows that may carry
/// padding; the grab strips the padding and halves onto the screen budget.
///
/// SCREEN RECORDING PERMISSION IS THE SYSTEM'S: without it macOS hands back
/// the wallpaper with no windows on it and no error. The first share prompts
/// once (the app must be launched from a bundle for the prompt to name it).
// ponytail: no pointer drawn in, unlike the X11 source — `CGDisplayCreateImage`
// leaves the cursor out too, but reading it back wants AppKit's `NSCursor`
// image, which no Linux host here can exercise.
#[cfg(target_os = "macos")]
struct ScreenSource {
    display: core_graphics::display::CGDisplay,
}

#[cfg(target_os = "macos")]
impl ScreenSource {
    fn open(target: ShareTarget) -> Result<Self, String> {
        use core_graphics::display::CGDisplay;

        let display = match target {
            // Every head at once is one `CGDisplayCreateImage` per head glued
            // into one picture; nothing offers it, so nothing implements it.
            ShareTarget::Desktop => CGDisplay::main(),
            ShareTarget::Monitor(head) => {
                let displays = CGDisplay::active_displays()
                    .map_err(|error| format!("no display to share ({error})"))?;
                let id = displays
                    .get(head)
                    .ok_or_else(|| "that display is no longer attached".to_string())?;
                CGDisplay::new(*id)
            }
            ShareTarget::Window(_) => {
                return Err("sharing a single window is not available on macOS yet".into());
            }
        };
        // one probe grab: a display that cannot be imaged (a headless run, a
        // locked session) is refused at open, not on every frame
        display
            .image()
            .ok_or_else(|| "the main display cannot be captured".to_string())?;
        Ok(ScreenSource { display })
    }

    /// One grab, BGRA, already inside the wire budget.
    fn grab(&self) -> Result<(Vec<u8>, u32, u32), String> {
        let image = self
            .display
            .image()
            .ok_or_else(|| "the display stopped answering".to_string())?;
        let (width, height) = (image.width(), image.height());
        let packed_32 = image.bits_per_pixel() == 32;
        if !packed_32 {
            return Err(format!(
                "this display's {}-bit pixel layout is not one screen sharing can read",
                image.bits_per_pixel()
            ));
        }
        let data = image.data();
        let bytes = data.bytes();
        let stride = image.bytes_per_row();
        let row = width * 4;
        let mut pixels = Vec::with_capacity(row * height);
        for y in 0..height {
            let start = y * stride;
            pixels.extend_from_slice(&bytes[start..start + row]);
        }
        let codec::Picture {
            mut pixels,
            width,
            height,
        } = codec::shrink_to_budget(pixels, width as u32, height as u32, SCREEN_PIXEL_BUDGET);
        // Core Graphics hands back BGRX in memory (32-bit little-endian
        // ARGB), which IS the renderer's and the encoder's order; only the
        // alpha byte needs writing, over the small image.
        for pixel in pixels.chunks_exact_mut(4) {
            pixel[3] = 0xff;
        }
        Ok((pixels, width, height))
    }
}

/// One X11 display this app may capture from: the connection and its root.
///
/// X11 AND PURE RUST ON PURPOSE. `x11rb` is already in this binary (gpui draws
/// through it), so the desktop costs no new dependency, no C toolchain and no
/// build-time system library — which the portal/pipewire route would cost on
/// every machine that builds this app. The app itself runs natively on either
/// display server; a share is an X11-session feature, and a Wayland session is
/// refused here rather than grabbed.
///
/// BOTH the picker and the capture come through this, so a target can never be
/// offered on a display that would then refuse it.
#[cfg(not(target_os = "macos"))]
fn x11_display() -> Result<(x11rb::rust_connection::RustConnection, u32), String> {
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
    let screen = setup
        .roots
        .get(screen)
        .ok_or_else(|| "the X display named no screen".to_string())?;
    let (root, depth) = (screen.root, screen.root_depth);
    // The one pixel layout `grab` reads: 32 bits per pixel, little-endian,
    // which is every TrueColor desktop this app runs on. Anything else is
    // refused rather than shipped as swapped colour.
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
    Ok((connection, root))
}

/// The heads RandR reports, in the order the picker numbers them — the same
/// order [`ShareTarget::Monitor`] indexes, so the list is the contract between
/// the picker and the grab and nothing needs to carry a rectangle around.
///
/// A server too old for RandR 1.5, or one with no monitor objects, reports
/// none: the root IS its one screen, and [`ShareTarget::Desktop`] shares it.
#[cfg(not(target_os = "macos"))]
fn x11_monitors(
    connection: &x11rb::rust_connection::RustConnection,
    root: u32,
) -> Vec<x11rb::protocol::randr::MonitorInfo> {
    use x11rb::protocol::randr::ConnectionExt as _;

    // RandR is version-negotiated before use, and `get_monitors` is 1.5.
    let Ok(version) = connection.randr_query_version(1, 5) else {
        return Vec::new();
    };
    let Ok(version) = version.reply() else {
        return Vec::new();
    };
    let has_monitors = (version.major_version, version.minor_version) >= (1, 5);
    if !has_monitors {
        return Vec::new();
    }
    connection
        .randr_get_monitors(root, true)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| reply.monitors)
        .unwrap_or_default()
}

/// The top-level windows a share may pick, newest-mapped first as the window
/// manager lists them. Only `_NET_CLIENT_LIST` windows are offered: that is the
/// WM's own list of the things a person thinks of as windows, so no menu,
/// tooltip or override-redirect surface can end up in the picker.
#[cfg(not(target_os = "macos"))]
fn x11_windows(
    connection: &x11rb::rust_connection::RustConnection,
    root: u32,
) -> Vec<ShareChoice> {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};

    let atom = |name: &str| -> Option<u32> {
        connection
            .intern_atom(true, name.as_bytes())
            .ok()?
            .reply()
            .ok()
            .map(|reply| reply.atom)
            .filter(|atom| *atom != 0)
    };
    // No `_NET_CLIENT_LIST` is a window manager that does not publish one (or
    // none running at all): there is nothing to enumerate, and the heads above
    // are the whole picker.
    let Some(client_list) = atom("_NET_CLIENT_LIST") else {
        return Vec::new();
    };
    let listed = connection
        .get_property(false, root, client_list, AtomEnum::WINDOW, 0, MAX_LISTED_WINDOWS)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .and_then(|reply| reply.value32().map(|windows| windows.collect::<Vec<u32>>()))
        .unwrap_or_default();
    let net_wm_name = atom("_NET_WM_NAME");
    let utf8 = atom("UTF8_STRING");
    let mut choices = Vec::with_capacity(listed.len());
    for window in listed {
        // A window too small to read is a tray icon or a stray 1×1 helper; a
        // share of one is never what was meant.
        let big_enough = connection
            .get_geometry(window)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|geometry| {
                geometry.width >= MIN_SHAREABLE_EDGE && geometry.height >= MIN_SHAREABLE_EDGE
            });
        if !big_enough {
            continue;
        }
        let utf8_title = net_wm_name
            .zip(utf8)
            .and_then(|(name, utf8)| x11_text(connection, window, name, utf8));
        let title = utf8_title
            .or_else(|| x11_text(connection, window, AtomEnum::WM_NAME.into(), AtomEnum::STRING.into()))
            .unwrap_or_else(|| "Untitled window".to_string());
        choices.push(ShareChoice {
            target: ShareTarget::Window(window),
            label: row_label(&title),
        });
    }
    choices
}

/// One text property as a `String`, or `None` when it is absent or empty —
/// which is how a window with no title falls through to the next property.
#[cfg(not(target_os = "macos"))]
fn x11_text(
    connection: &x11rb::rust_connection::RustConnection,
    window: u32,
    property: u32,
    kind: u32,
) -> Option<String> {
    use x11rb::protocol::xproto::ConnectionExt as _;

    let reply = connection
        .get_property(false, window, property, kind, 0, MAX_TITLE_WORDS)
        .ok()?
        .reply()
        .ok()?;
    let text = String::from_utf8_lossy(&reply.value).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// `_NET_CLIENT_LIST` is read in 32-bit words: a desktop with more open windows
/// than this has a picker nobody would scroll anyway.
#[cfg(not(target_os = "macos"))]
const MAX_LISTED_WINDOWS: u32 = 256;
/// A window title is read in 32-bit words — 1 KiB of name is already far more
/// than a row can show.
#[cfg(not(target_os = "macos"))]
const MAX_TITLE_WORDS: u32 = 256;
/// The smallest window edge worth offering, in pixels.
#[cfg(not(target_os = "macos"))]
const MIN_SHAREABLE_EDGE: u16 = 64;
/// The longest a picker row's label may be, in characters. Past this it stops
/// being a name anyone picks by, and it is the row that pays.
#[cfg(not(target_os = "macos"))]
const MAX_LABEL_CHARS: usize = 72;

/// A window title as a picker row's label.
///
/// A TITLE IS WHATEVER THE PROGRAM THAT OWNS THE WINDOW WROTE. `_NET_WM_NAME`
/// has no length rule and no character rule: a browser tab puts a page title
/// there, so it arrives with the page's newlines, tabs and control bytes in it
/// and at whatever length the page felt like. A newline in a button label
/// breaks the row it is drawn in, and a kilobyte of title breaks the picker, so
/// the collapse and the bound both happen here — at the one place an outside
/// string becomes something this app draws.
#[cfg(not(target_os = "macos"))]
fn row_label(title: &str) -> String {
    let printable = title
        .chars()
        .map(|character| if character.is_control() { ' ' } else { character });
    let collapsed = printable.collect::<String>();
    let mut label: String = collapsed.split_whitespace().collect::<Vec<_>>().join(" ");
    let over = label.chars().count() > MAX_LABEL_CHARS;
    if !over {
        return label;
    }
    // Cut on a character boundary, never a byte one: a title is UTF-8 and a
    // split through a multi-byte character would panic.
    let cut = label
        .char_indices()
        .nth(MAX_LABEL_CHARS)
        .map(|(at, _)| at)
        .unwrap_or(label.len());
    label.truncate(cut);
    label.push('…');
    label
}

/// The share targets this host can capture, in the order the picker shows them:
/// the whole desktop (only where there is more than one head to glue), then
/// each head, then each window.
#[cfg(not(target_os = "macos"))]
pub fn call_share_targets() -> Result<Vec<ShareChoice>, String> {
    let (connection, root) = x11_display()?;
    let heads = x11_monitors(&connection, root);
    let mut choices = Vec::new();
    let one_head = heads.len() < 2;
    if !one_head {
        choices.push(ShareChoice {
            target: ShareTarget::Desktop,
            label: format!("Entire desktop — all {} screens", heads.len()),
        });
    }
    for (index, head) in heads.iter().enumerate() {
        let name = x11_text_of_atom(&connection, head.name).unwrap_or_else(|| format!("Screen {}", index + 1));
        let primary = if head.primary { " (primary)" } else { "" };
        choices.push(ShareChoice {
            target: ShareTarget::Monitor(index),
            label: format!("{name}{primary} — {}×{}", head.width, head.height),
        });
    }
    if choices.is_empty() {
        choices.push(ShareChoice {
            target: ShareTarget::Desktop,
            label: "Entire screen".to_string(),
        });
    }
    choices.extend(x11_windows(&connection, root));
    Ok(choices)
}

/// A RandR monitor's name, which is an atom (`DP-1`, `eDP-1`, `XWAYLAND0`).
#[cfg(not(target_os = "macos"))]
fn x11_text_of_atom(
    connection: &x11rb::rust_connection::RustConnection,
    atom: u32,
) -> Option<String> {
    use x11rb::protocol::xproto::ConnectionExt as _;

    let reply = connection.get_atom_name(atom).ok()?.reply().ok()?;
    let name = String::from_utf8_lossy(&reply.name).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// The screen source: one X11 connection, and one grab of [`ShareTarget`] per
/// frame.
#[cfg(not(target_os = "macos"))]
struct ScreenSource {
    connection: x11rb::rust_connection::RustConnection,
    root: x11rb::protocol::xproto::Window,
    target: ShareTarget,
}

/// One frame's read: the drawable, the rectangle inside it, and where that
/// rectangle sits on the root — which is the frame of reference the pointer's
/// position comes in, and the only reason the origin is carried.
#[cfg(not(target_os = "macos"))]
struct Plan {
    drawable: x11rb::protocol::xproto::Drawable,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    root_x: i16,
    root_y: i16,
    /// A pixmap this plan named, which the grab frees after reading it.
    pixmap: Option<x11rb::protocol::xproto::Pixmap>,
}

#[cfg(not(target_os = "macos"))]
impl ScreenSource {
    fn open(target: ShareTarget) -> Result<Self, String> {
        let (connection, root) = x11_display()?;
        let source = ScreenSource {
            connection,
            root,
            target,
        };
        source.prepare()?;
        // One probe grab: a target that cannot be read — a window that closed
        // between the picker and the press, a head RandR no longer reports —
        // is refused at open, where the toggle can go back, not every frame.
        source.plan()?;
        Ok(source)
    }

    /// What a target needs before its first grab. A WINDOW MUST BE REDIRECTED
    /// TO BE NAMED: `NameWindowPixmap` answers `BadMatch` for a window nobody
    /// redirected, and reading the window drawable itself instead would hand
    /// back undefined bytes wherever another window overlaps it — the sharer's
    /// own huddle window, most of the time. `AUTOMATIC` keeps the server
    /// painting it to the screen as well, so redirecting changes nothing the
    /// sharer can see, and a compositing WM that already holds a `MANUAL`
    /// redirect on the same window is unaffected by ours.
    fn prepare(&self) -> Result<(), String> {
        use x11rb::protocol::composite::{ConnectionExt as _, Redirect};
        use x11rb::protocol::xfixes::ConnectionExt as _;

        // XFIXES IS NEGOTIATED PER CONNECTION before any of its requests, and
        // this source owns its own. A server without it is a share with no
        // pointer drawn into it, never a refused share — so the result is
        // dropped rather than raised.
        let _ = self
            .connection
            .xfixes_query_version(5, 0)
            .ok()
            .and_then(|cookie| cookie.reply().ok());
        let ShareTarget::Window(window) = self.target else {
            return Ok(());
        };
        self.connection
            .composite_query_version(0, 4)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|_| "this X server has no Composite extension, so a single window cannot be shared".to_string())?;
        self.connection
            .composite_redirect_window(window, Redirect::AUTOMATIC)
            .map_err(|error| error.to_string())?
            .check()
            .map_err(|error| format!("that window cannot be captured ({error})"))?;
        Ok(())
    }

    /// ONE DISPATCH over the target, each arm one delegation — a new kind of
    /// share must fail the build here until it is routed.
    fn plan(&self) -> Result<Plan, String> {
        match self.target {
            ShareTarget::Desktop => self.plan_desktop(),
            ShareTarget::Monitor(head) => self.plan_monitor(head),
            ShareTarget::Window(window) => self.plan_window(window),
        }
    }

    /// The root whole. Its geometry is re-read per frame: a resolution change
    /// mid-share would otherwise grab a rectangle the root no longer has.
    fn plan_desktop(&self) -> Result<Plan, String> {
        let geometry = self.geometry(self.root)?;
        Ok(Plan {
            drawable: self.root,
            x: 0,
            y: 0,
            width: geometry.width,
            height: geometry.height,
            root_x: 0,
            root_y: 0,
            pixmap: None,
        })
    }

    /// One head, as a rectangle of the root — never a drawable of its own, so
    /// nothing can overlap it and no compositing is involved.
    fn plan_monitor(&self, head: usize) -> Result<Plan, String> {
        let heads = x11_monitors(&self.connection, self.root);
        let head = heads
            .get(head)
            .ok_or_else(|| "that screen is no longer attached".to_string())?;
        Ok(Plan {
            drawable: self.root,
            x: head.x,
            y: head.y,
            width: head.width,
            height: head.height,
            root_x: head.x,
            root_y: head.y,
            pixmap: None,
        })
    }

    /// One window, through its redirected offscreen pixmap. THE PIXMAP IS
    /// NAMED PER FRAME: the server frees the old backing store when the window
    /// resizes, and a name held across that grabs a stale size forever.
    fn plan_window(&self, window: u32) -> Result<Plan, String> {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::composite::ConnectionExt as _;
        use x11rb::protocol::xproto::ConnectionExt as _;

        let geometry = self.geometry(window)?;
        let root_at = self
            .connection
            .translate_coordinates(window, self.root, 0, 0)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| error.to_string())?;
        let pixmap = self
            .connection
            .generate_id()
            .map_err(|error| error.to_string())?;
        self.connection
            .composite_name_window_pixmap(window, pixmap)
            .map_err(|error| error.to_string())?
            .check()
            .map_err(|error| format!("that window stopped answering ({error})"))?;
        Ok(Plan {
            drawable: pixmap,
            x: 0,
            y: 0,
            width: geometry.width,
            height: geometry.height,
            root_x: root_at.dst_x,
            root_y: root_at.dst_y,
            pixmap: Some(pixmap),
        })
    }

    fn geometry(
        &self,
        drawable: x11rb::protocol::xproto::Drawable,
    ) -> Result<x11rb::protocol::xproto::GetGeometryReply, String> {
        use x11rb::protocol::xproto::ConnectionExt as _;

        self.connection
            .get_geometry(drawable)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| format!("that share target is gone ({error})"))
    }

    /// One grab, BGRA, already inside the wire budget.
    fn grab(&self) -> Result<(Vec<u8>, u32, u32), String> {
        use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};

        let plan = self.plan()?;
        let image = self
            .connection
            .get_image(
                ImageFormat::Z_PIXMAP,
                plan.drawable,
                plan.x,
                plan.y,
                plan.width,
                plan.height,
                u32::MAX,
            )
            .map_err(|error| error.to_string())
            .and_then(|cookie| cookie.reply().map_err(|error| error.to_string()));
        // The pixmap goes back BEFORE the reply is unwrapped: a failed read
        // must not leak one per frame for as long as the share runs.
        if let Some(pixmap) = plan.pixmap {
            let _ = self.connection.free_pixmap(pixmap);
        }
        let mut pixels = image?.data;
        // The pointer goes in at full size, so it shrinks with the picture.
        self.draw_pointer(&mut pixels, &plan);
        let codec::Picture {
            mut pixels,
            width,
            height,
        } = codec::shrink_to_budget(
            pixels,
            u32::from(plan.width),
            u32::from(plan.height),
            SCREEN_PIXEL_BUDGET,
        );
        // X hands back BGRX, which IS the renderer's and the encoder's order;
        // only the alpha byte needs writing, over the small image.
        for pixel in pixels.chunks_exact_mut(4) {
            pixel[3] = 0xff;
        }
        Ok((pixels, width, height))
    }

    /// Draw the pointer into the grab, BECAUSE X DOES NOT PUT IT THERE.
    /// `GetImage` reads the framebuffer and the cursor is an overlay the server
    /// composites on the way out, so a share without this step is one where
    /// "click here" points at nothing — the single most common thing a shared
    /// screen is for.
    ///
    /// A pointer that cannot be read is not a reason to drop the frame, and it
    /// is not loggable either (this runs ten times a second), so every failure
    /// here is simply a frame without a cursor in it.
    fn draw_pointer(&self, pixels: &mut [u8], plan: &Plan) {
        use x11rb::protocol::xfixes::ConnectionExt as _;

        let Some(cursor) = self
            .connection
            .xfixes_get_cursor_image()
            .ok()
            .and_then(|cookie| cookie.reply().ok())
        else {
            return;
        };
        // The pointer's position is in ROOT coordinates and names its hotspot,
        // not its corner; the grab's own origin on the root turns one into the
        // other.
        blend_pointer(
            pixels,
            i32::from(plan.width),
            i32::from(plan.height),
            Pointer {
                argb: &cursor.cursor_image,
                width: i32::from(cursor.width).max(1),
                left: i32::from(cursor.x) - i32::from(cursor.xhot) - i32::from(plan.root_x),
                top: i32::from(cursor.y) - i32::from(cursor.yhot) - i32::from(plan.root_y),
            },
        );
    }
}

/// The pointer as XFIXES hands it over, placed: premultiplied ARGB words, the
/// row length that shapes them, and where its top-left corner falls INSIDE the
/// grabbed rectangle — which is the only coordinate space [`blend_pointer`]
/// knows about.
#[cfg(not(target_os = "macos"))]
struct Pointer<'a> {
    argb: &'a [u32],
    width: i32,
    left: i32,
    top: i32,
}

/// Composite the pointer onto a BGRA grab, clipped to it: the pointer sits
/// where it sits, and a rectangle that only catches a corner of it gets that
/// corner and nothing outside the buffer.
#[cfg(not(target_os = "macos"))]
fn blend_pointer(pixels: &mut [u8], width: i32, height: i32, pointer: Pointer<'_>) {
    for (index, argb) in pointer.argb.iter().enumerate() {
        let index = index as i32;
        let x = pointer.left + index % pointer.width;
        let y = pointer.top + index / pointer.width;
        let inside = x >= 0 && y >= 0 && x < width && y < height;
        let alpha = (argb >> 24) & 0xff;
        if !inside || alpha == 0 {
            continue;
        }
        let at = ((y * width + x) * 4) as usize;
        let Some(pixel) = pixels.get_mut(at..at + 4) else {
            continue;
        };
        // XFIXES hands back PREMULTIPLIED ARGB, so source-over onto an opaque
        // destination is an add — no divide by alpha anywhere.
        let keep = 255 - alpha;
        let over = |source: u32, under: u8| (source + u32::from(under) * keep / 255).min(255) as u8;
        pixel[0] = over(argb & 0xff, pixel[0]);
        pixel[1] = over((argb >> 8) & 0xff, pixel[1]);
        pixel[2] = over((argb >> 16) & 0xff, pixel[2]);
    }
}

/// A redirect this source asked for is released with it — the server drops a
/// dead client's redirect anyway, but the share ending is not the app exiting.
#[cfg(not(target_os = "macos"))]
impl Drop for ScreenSource {
    fn drop(&mut self) {
        use x11rb::protocol::composite::ConnectionExt as _;

        let ShareTarget::Window(window) = self.target else {
            return;
        };
        let _ = self.connection.composite_unredirect_window(
            window,
            x11rb::protocol::composite::Redirect::AUTOMATIC,
        );
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
    events: &tokio::sync::mpsc::Sender<String>,
) -> Open {
    match source {
        Source::Off => Open::None,
        Source::Camera => open_camera(events).map_or(Open::None, Open::Camera),
        Source::Screen => match ScreenSource::open(*share_target().lock().expect("share target")) {
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
    frames: tokio::sync::mpsc::Sender<CapturedImage>,
    shutdown: std::sync::mpsc::Receiver<()>,
    events: tokio::sync::mpsc::Sender<String>,
    max_bytes: usize,
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
            .then(|| encode_bounded(&bgra, width, height, max_bytes))
            .flatten();
        store_preview(bgra, width, height);
        let Some(encoded) = encoded else {
            continue;
        };
        last_sent = Some(std::time::Instant::now());
        let captured = CapturedImage {
            preview: SELF_STAGE,
            timestamp_ms: started.elapsed().as_millis() as u32,
            jpeg: encoded,
        };
        match frames.try_send(captured) {
            Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// EVERY TARGET THE PICKER OFFERS MUST ACTUALLY GRAB — a row that refuses
    /// on press is worse than no row — and a window share must come back with
    /// THAT WINDOW's pixels in it, which is the whole question the composite
    /// redirect answers.
    ///
    /// The test brings its own window and publishes it the way a window manager
    /// does, because one `_NET_CLIENT_LIST` property is the entire contribution
    /// a WM makes to the picker's list. So a bare `DISPLAY=:95` against an Xvfb
    /// is enough to run it:
    /// `DISPLAY=:95 cargo test -p ducktape-app --bin ducktape-app
    /// every_offered_share_target -- --ignored --nocapture`
    #[cfg(not(target_os = "macos"))]
    #[ignore = "needs a real X display"]
    #[test]
    fn every_offered_share_target_grabs_and_a_window_share_is_that_window() {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::{
            AtomEnum, ConnectionExt as _, CreateGCAux, CreateWindowAux, PropMode, Rectangle,
            WindowClass,
        };
        use x11rb::wrapper::ConnectionExt as _;

        const EDGE: u16 = 320;
        /// Where the pointer's hotspot is put inside the window — far from the
        /// centre, so the fill and the pointer are checked in their own corners.
        const POINTER_AT: u16 = 40;
        let (connection, root) = x11_display().expect("an X display to share");
        let screen = connection
            .setup()
            .roots
            .iter()
            .find(|screen| screen.root == root)
            .expect("the screen this root belongs to")
            .clone();
        let window = connection.generate_id().expect("a window id");
        connection
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                root,
                0,
                0,
                EDGE,
                EDGE,
                0,
                WindowClass::INPUT_OUTPUT,
                screen.root_visual,
                &CreateWindowAux::new().background_pixel(screen.black_pixel),
            )
            .expect("create a window")
            .check()
            .expect("the window is created");
        connection
            .map_window(window)
            .expect("map it")
            .check()
            .expect("it is mapped");
        let client_list = connection
            .intern_atom(false, b"_NET_CLIENT_LIST")
            .expect("intern")
            .reply()
            .expect("the atom")
            .atom;
        connection
            .change_property32(PropMode::REPLACE, root, client_list, AtomEnum::WINDOW, &[
                window,
            ])
            .expect("publish the client list")
            .check()
            .expect("it is published");
        // The title a hostile (or merely careless) program would set: newlines,
        // a control byte and far more of it than a row can hold. The picker
        // must offer a bounded single line, not this.
        let net_wm_name = connection
            .intern_atom(false, b"_NET_WM_NAME")
            .expect("intern")
            .reply()
            .expect("the atom")
            .atom;
        let utf8 = connection
            .intern_atom(false, b"UTF8_STRING")
            .expect("intern")
            .reply()
            .expect("the atom")
            .atom;
        let shouting = format!("a\nvery\u{1}\tlong {}", "가".repeat(400));
        connection
            .change_property8(
                PropMode::REPLACE,
                window,
                net_wm_name,
                utf8,
                shouting.as_bytes(),
            )
            .expect("set a hostile title")
            .check()
            .expect("it is set");

        let offered = call_share_targets().expect("this display offers a share target");
        assert!(!offered.is_empty(), "a display with no target is a bug");
        let ours = offered
            .iter()
            .find(|choice| choice.target == ShareTarget::Window(window))
            .expect("the published window must be offered");
        println!("offered window: {}", ours.label);
        assert!(
            ours.label.chars().count() <= MAX_LABEL_CHARS + 1,
            "a title a program shouted must arrive bounded: {} chars",
            ours.label.chars().count()
        );
        assert!(
            !ours.label.chars().any(|character| character.is_control()),
            "and on one line: {:?}",
            ours.label
        );
        assert!(ours.label.starts_with("a very long 가"), "{:?}", ours.label);

        for choice in &offered {
            let source = ScreenSource::open(choice.target)
                .unwrap_or_else(|reason| panic!("{} must open: {reason}", choice.label));
            // The window is painted AFTER the source opened, because opening it
            // is what redirects the window: a fill from before the redirect is
            // not in the pixmap the grab names.
            let is_ours = choice.target == ShareTarget::Window(window);
            if is_ours {
                let gc = connection.generate_id().expect("a gc id");
                connection
                    .create_gc(gc, window, &CreateGCAux::new().foreground(0x00_ff_00))
                    .expect("create a gc")
                    .check()
                    .expect("the gc is created");
                connection
                    .poly_fill_rectangle(window, gc, &[Rectangle {
                        x: 0,
                        y: 0,
                        width: EDGE,
                        height: EDGE,
                    }])
                    .expect("fill the window")
                    .check()
                    .expect("it is filled");
                // The pointer is put INSIDE the window, so the grab below has
                // to draw it in — see `draw_pointer`. Its hotspot lands in the
                // top-left quadrant, away from the centre the fill is checked
                // at.
                connection
                    .warp_pointer(
                        x11rb::NONE,
                        window,
                        0,
                        0,
                        0,
                        0,
                        (POINTER_AT) as i16,
                        (POINTER_AT) as i16,
                    )
                    .expect("warp the pointer")
                    .check()
                    .expect("the pointer moved");
                // A round trip on this connection is the wait: the server has
                // processed the fill and the warp before it can answer. No
                // sleep, no retry.
                connection
                    .get_input_focus()
                    .expect("sync")
                    .reply()
                    .expect("the server is caught up");
            }
            let (pixels, width, height) = source
                .grab()
                .unwrap_or_else(|reason| panic!("{} must grab: {reason}", choice.label));
            assert_eq!(
                pixels.len(),
                (width * height * 4) as usize,
                "{} handed back a picture that is not its own size",
                choice.label
            );
            assert!(
                width * height <= SCREEN_PIXEL_BUDGET,
                "{} came back over the share budget at {width}×{height}",
                choice.label
            );
            println!("{} grabbed at {width}×{height}", choice.label);
            if !is_ours {
                continue;
            }
            assert_eq!(
                (width, height),
                (u32::from(EDGE), u32::from(EDGE)),
                "a window share must be the window's own size, not the desktop's"
            );
            // The middle of the window, where nothing else can be: BGRA, so the
            // green we filled with reads back in the middle channel.
            let middle = ((height / 2 * width + width / 2) * 4) as usize;
            assert_eq!(
                &pixels[middle..middle + 3],
                &[0x00, 0xff, 0x00],
                "a window share must carry that window's pixels"
            );
            // And the pointer must be IN the picture: X leaves it out of
            // `GetImage`, so the only thing that can have broken the green fill
            // around its hotspot is `draw_pointer` having run.
            let near_pointer = |x: u32, y: u32| {
                let at = ((y * width + x) * 4) as usize;
                pixels[at..at + 3] != [0x00, 0xff, 0x00]
            };
            let drawn = (0..u32::from(POINTER_AT) * 2)
                .flat_map(|y| (0..u32::from(POINTER_AT) * 2).map(move |x| (x, y)))
                .any(|(x, y)| near_pointer(x, y));
            assert!(
                drawn,
                "the pointer is inside this window and nothing drew it into the share"
            );
        }
    }

    /// A WINDOW TITLE IS UNTRUSTED INPUT — `_NET_WM_NAME` is whatever the
    /// owning program wrote, so a browser hands over its page title complete
    /// with the page's newlines and at the page's length. The row it becomes is
    /// drawn in a button, where a newline breaks the row and a kilobyte breaks
    /// the picker.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_row_label_collapses_and_bounds_whatever_a_window_calls_itself() {
        assert_eq!(row_label("src/video.rs — Neovim"), "src/video.rs — Neovim");
        // whitespace of every kind collapses, including the control bytes that
        // are not whitespace at all
        assert_eq!(
            row_label(" a\ttitle\nover \u{1} lines  "),
            "a title over lines"
        );
        // the bound counts CHARACTERS, and says it was cut
        let long = "가".repeat(MAX_LABEL_CHARS * 3);
        let bounded = row_label(&long);
        assert_eq!(bounded.chars().count(), MAX_LABEL_CHARS + 1);
        assert!(bounded.ends_with('…'));
        // exactly at the bound is not cut
        let exact = "x".repeat(MAX_LABEL_CHARS);
        assert_eq!(row_label(&exact), exact);
        // and a title of nothing but whitespace does not become a blank row
        // that looks pressable but reads as empty — the caller's fallback is
        // what a window with no title gets, so this one is simply empty
        assert_eq!(row_label("   \n\t "), "");
    }

    /// The pointer is drawn in BY HAND because X leaves it out of `GetImage`,
    /// so this arithmetic is the only thing standing between a share and a
    /// "click here" that points at nothing: premultiplied source-over, and
    /// clipped to the grab so a pointer half off the shared rectangle writes
    /// only the half that is on it.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_pointer_is_blended_in_and_clipped_to_the_share() {
        // a 2×2 black grab, and a 2×2 pointer — words are A, R, G, B from the
        // top byte down: opaque blue, transparent, half-alpha white
        // (premultiplied, so 0x80 in every channel), opaque red
        let opaque_blue = 0xff_00_00_ffu32;
        let clear = 0x00_00_00_00u32;
        let half_white = 0x80_80_80_80u32;
        let opaque_red = 0xff_ff_00_00u32;
        let cursor = [opaque_blue, clear, half_white, opaque_red];
        let mut pixels = vec![0u8; 2 * 2 * 4];
        blend_pointer(
            &mut pixels,
            2,
            2,
            Pointer {
                argb: &cursor,
                width: 2,
                left: 0,
                top: 0,
            },
        );
        // BGRA out: blue is (255, 0, 0), the clear word left its pixel alone,
        // half-alpha over black is the premultiplied value itself, red is
        // (0, 0, 255).
        assert_eq!(&pixels[0..3], &[0xff, 0x00, 0x00]);
        assert_eq!(&pixels[4..7], &[0x00, 0x00, 0x00]);
        assert_eq!(&pixels[8..11], &[0x80, 0x80, 0x80]);
        assert_eq!(&pixels[12..15], &[0x00, 0x00, 0xff]);

        // Half-alpha over an opaque WHITE ground keeps the rest of the ground:
        // 0x80 + 0xff * (255 - 0x80) / 255 = 0xff.
        let mut white = vec![0xffu8; 4];
        blend_pointer(
            &mut white,
            1,
            1,
            Pointer {
                argb: &[half_white],
                width: 1,
                left: 0,
                top: 0,
            },
        );
        assert_eq!(&white[0..3], &[0xff, 0xff, 0xff]);

        // A pointer whose hotspot sits one pixel off the top-left of the
        // shared rectangle: only its bottom-right quarter lands, and nothing
        // is written outside the buffer.
        let mut corner = vec![0u8; 2 * 2 * 4];
        blend_pointer(
            &mut corner,
            2,
            2,
            Pointer {
                argb: &cursor,
                width: 2,
                left: -1,
                top: -1,
            },
        );
        assert_eq!(&corner[0..3], &[0x00, 0x00, 0xff], "the red corner lands");
        assert_eq!(&corner[4..], &[0u8; 12], "and nothing else does");
    }

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
        // A guest may select an image before its first decoded frame arrives.
        reset();
        assert!(stage_frame(SELF_STAGE).is_none());
        store_preview(vec![10, 20, 30, 0xff], 1, 1);
        assert!(stage_frame(SELF_STAGE).is_some());
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
        assert!(stage_frame(SELF_STAGE).is_some());

        let remote = render_bgra(vec![0xff; 4], 1, 1).unwrap();
        let remote_id = remote.id;
        store().lock().unwrap().peers.insert(
            "remote-image".into(),
            TileFrame {
                width: 1,
                height: 1,
                handle: remote,
            },
        );
        assert_eq!(stage_frame("remote-image").unwrap().2.id, remote_id);
        assert!(stage_frame("missing").is_none());
        crate::view_tree::assert_released_image_is_not_cached("remote-image");

        // ONE SOURCE: starting either one ends the other, and either one off
        // is off — there is no state where both are live.
        let camera = use_source(Source::Camera);
        assert_eq!(source(), Source::Camera);
        assert!(camera.camera && !camera.sharing);
        let screen = use_source(Source::Screen);
        assert_eq!(source(), Source::Screen);
        assert!(screen.sharing && !screen.camera);
        // ...and the outgoing source's last frame goes with it, so the tile
        // strip cannot paint a camera under a "sharing" beacon.
        assert!(preview_id().is_none());
        let off = use_source(Source::Off);
        assert_eq!(source(), Source::Off);
        assert!(!off.camera && !off.sharing);

        // A peer's frame arriving while a decode for that SAME peer is
        // already running is DROPPED, never queued. `store_image`
        // checks-and-sets the in-flight marker before it ever touches the
        // bytes, so pre-arming that marker here stands in for a real decode
        // still running on another blocking-pool thread.
        reset();
        let peer_hex = "opaque-image-7".to_owned();
        store()
            .lock()
            .expect("video store")
            .decoding
            .insert(peer_hex.clone());
        store_image(peer_hex.clone(), Vec::new(), &std::sync::atomic::AtomicBool::new(true));
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
