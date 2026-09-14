//! Real native shaping and scene-layout probes. Timings are reported, not gated.
use super::{AppMessage, Ducktape, backend};
use gpui_kit::{AppContext, HeadlessAppContext, px, size};
use std::alloc::{GlobalAlloc, Layout, System};
use std::borrow::Cow;
use std::cell::Cell;
use std::sync::Arc;
const ROWS: i64 = 256;
const HUDDLE_ROWS: usize = 32;
pub(crate) const FRAMES: usize = 12;
thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

pub(crate) fn allocations() -> u64 {
    ALLOCATIONS.with(Cell::get)
}

// ---------------------------------------------------------------------------
// Phases
// ---------------------------------------------------------------------------

pub(crate) struct Phase {
    label: &'static str,
    allocations: Vec<u64>,
    elapsed_us: Vec<u128>,
}

impl Phase {
    pub(crate) fn new(label: &'static str) -> Self {
        Self {
            label,
            allocations: Vec::with_capacity(FRAMES),
            elapsed_us: Vec::with_capacity(FRAMES),
        }
    }

    pub(crate) fn sample<T>(&mut self, work: impl FnOnce() -> T) -> T {
        let started = std::time::Instant::now();
        let before = allocations();
        let value = work();
        let spent = allocations() - before;
        let elapsed = started.elapsed().as_micros();
        self.allocations.push(spent);
        self.elapsed_us.push(elapsed);
        value
    }

    pub(crate) fn median_allocations(&self) -> u64 {
        let mut sorted = self.allocations.clone();
        sorted.sort_unstable();
        sorted[sorted.len() / 2]
    }

    fn median_us(&self) -> u128 {
        let mut sorted = self.elapsed_us.clone();
        sorted.sort_unstable();
        sorted[sorted.len() / 2]
    }

    pub(crate) fn report(&self) {
        tracing::info!(
            "{:<28} allocs(p50)={:>7}  {:>6}us",
            self.label,
            self.median_allocations(),
            self.median_us()
        );
    }
}

// ---------------------------------------------------------------------------
// The synthetic workspace
// ---------------------------------------------------------------------------

fn probe_channel_with_head(index: i64, head_seq: i64) -> backend::ChatChannel {
    backend::ChatChannel {
        id: format!("channel-{index}"),
        name: format!("channel-{index}"),
        archived: false,
        members_only: false,
        huddle_count: 0,
        voice: false,
        huddle: Vec::new(),
        head_seq,
    }
}

fn probe_channel(index: i64) -> backend::ChatChannel {
    probe_channel_with_head(index, ROWS)
}

fn probe_huddle_participant(index: usize) -> backend::HuddleParticipant {
    backend::HuddleParticipant {
        key: format!("user-{index}"),
        label: format!("Huddle member {index}"),
        initials: "HM".into(),
        is_agent: false,
        is_you: index == 0,
        joined_at: index as i64,
        node: format!("node-{index}"),
    }
}

fn console_in_huddle() -> (Ducktape, crate::shell::WindowKey) {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.connected_rpc = "http://node".into();
    app.settings_user_key = "user-0".into();
    let huddle = crate::shell::WindowKey::unique();
    let _ = app.update(AppMessage::ChatUpdated(backend::ChatData {
        generation: app.chat_generation,
        channels: vec![probe_channel(0)],
        active_channel: "channel-0".into(),
        active_channel_name: "channel-0".into(),
        active_channel_archived: false,
        active_channel_members_only: false,
        huddle_roster: (0..HUDDLE_ROWS).map(probe_huddle_participant).collect(),
        channel_members: Vec::new(),
    }));
    let _ = app.update(AppMessage::HuddleOpened(huddle));
    for index in 0..HUDDLE_ROWS {
        let _ = app.update(AppMessage::CallEvent(super::call::CallEvent {
            kind: "peer".into(),
            peer: format!("node-{index}"),
            muted: index % 2 == 0,
            ..super::call::CallEvent::default()
        }));
    }
    assert_eq!(app.huddle_roster.len(), HUDDLE_ROWS);
    assert_eq!(app.call_peers.len(), HUDDLE_ROWS);
    assert_eq!(app.huddle_win, Some(huddle));
    (app, huddle)
}

pub(crate) fn headless_context() -> HeadlessAppContext {
    let platform = gpui_kit::platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(()),
        gpui_kit::platform::current_headless_renderer,
    );
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::editor::wire::init_notion(cx);
        cx.text_system()
            .add_fonts(vec![
                Cow::Borrowed(include_bytes!(
                    "../../crates/views/support/design/assets/fonts/Geist[wght].ttf"
                )),
                Cow::Borrowed(include_bytes!(
                    "../../crates/views/support/design/assets/fonts/GeistMono[wght].ttf"
                )),
                Cow::Borrowed(include_bytes!(
                    "../../crates/views/support/design/assets/fonts/NotoColorEmoji.ttf"
                )),
            ])
            .expect("bundled fonts load into the production text system");
    });
    cx
}
#[test]
fn live_chat_batches_take_one_shipping_app_message() {
    let body = crate::tests::handler_body("LiveUpdated");
    assert_eq!(body.matches("matchnext.kind").count(), 1);
    for variant in ["Retry", "Tip", "Ready", "Chat", "Bell", "Plane", "Resync"] {
        assert_eq!(body.matches(&format!("LiveKind::{variant}=>")).count(), 1);
    }
    assert_eq!(body.matches("fold_live_chat(").count(), 1);
    assert!(!body.contains("LiveChatUpdated"));
}
#[test]
fn large_screens_stay_under_their_allocation_ceilings() {
    let _turn = crate::module_view::tests::blocking_connection_turn();
    let (app, _) = console_in_huddle();
    let mut cx = headless_context();
    let window = cx
        .open_window(size(px(320.), px(460.)), |window, cx| {
            let view = crate::shell::test_window(app, crate::shell::WindowKind::Huddle, window, cx);
            cx.new(|cx| gpui_kit::component::Root::new(view, window, cx))
        })
        .expect("native huddle window");
    for _ in 0..4 {
        cx.update_window(window.into(), |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .unwrap();
        cx.run_until_parked();
    }
    let mut phase = Phase::new("native huddle build+layout");
    for _ in 0..FRAMES {
        phase.sample(|| {
            cx.update_window(window.into(), |_, window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            })
            .unwrap()
        });
    }
    phase.report();
    // 32 roster tiles, each a card with an avatar, a name and a caption.
    assert!(
        phase.median_allocations() < 7_000,
        "native huddle allocations exceeded the retained 7000-allocation budget"
    );
}
