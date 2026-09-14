//! Renderer-independent execution of dynamically loaded WASM views.
//! Semantic slots and document transfer preserve the existing host contract.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

pub use ui_lang_wire as wire;
pub use wit_bindgen;

use futures::StreamExt;
use std::collections::HashSet;
use task::BoxStream;
pub mod rev;
mod subscription;
pub mod task;
pub use subscription::{Recipe, Subscription};
pub use task::Task;

mod editor;
mod editor_binding;
mod editor_documents;
pub use editor::Editor;
pub use editor_binding::{
    EditorBinding, EditorInteractionRequest, EditorKeyRequest, EditorStateView, EditorTransaction,
    EditorTransactionEvent,
};
pub use editor_documents::EditorDocumentUpdate;
pub mod events;
pub mod keyboard;
pub mod kit;
mod memo;
pub mod mouse;
pub use memo::{invalidate_component, memo_lazy};
pub mod host;
pub mod testing;
pub mod widget;
pub mod window;

pub use snapshot::SnapshotApp;
mod snapshot;

/// The application contract consumed by `export_app!`.
pub trait App: Sized + 'static {
    type Message: Clone + 'static;
    fn boot() -> (Self, Task<Self::Message>);
    fn view(&self) -> wire::Node;
    fn update(&mut self, message: Self::Message) -> Task<Self::Message>;
    fn subscription(&self) -> Subscription<Self::Message>;
}

/// The per-frame tables a view fills as it builds: a button's `on_press`
/// is the index its message took here, an input's `on_input` the index of
/// its `String -> Message` constructor, a checkbox's `on_toggle` that of a
/// `bool -> Message` one, a slider's `f32`, a pick list's `u32`, a
/// sensor's and a mouse area's `(f32, f32)` size or position, a mouse
/// area's `(f32, f32, bool)` scroll, and a resize handle's `(f64, f64)` drag. The host
/// echoes an index back with the value; the driver looks the handler up in
/// the table of the frame it echoed and runs it.
///
/// The tables are untyped so each guest can use this
/// crate without naming the app's message type; the driver downcasts, by
/// argument and message type both, so an index the host sends with the
/// wrong kind of value finds nothing.
pub mod slots;

/// One running task: its stream, and the flag its waker sets. A task is
/// polled only when the flag is up — set at spawn, by a host answer
/// through [`host::fulfill`], by its own yield, or by another task in the
/// same pass — so a task waiting on the host costs a tick nothing.
struct Running<M> {
    /// Tracker runners restart from restored state; ordinary tasks must settle.
    subscription: Option<u64>,
    woken: Arc<Woken>,
    stream: BoxStream<M>,
}

/// One running app: its state, its in-flight tasks, the streams its
/// `subscribe` block keeps alive, and the last tree it sent so an identical
/// one crosses as `unchanged` and a changed one as patches against it.
pub struct Driver<A: App> {
    slots: slots::Context,
    app: A,
    tasks: Vec<Running<A::Message>>,
    subscriptions: HashSet<u64>,
    observers: Vec<subscription::Observer<A::Message>>,
    last_root: Option<wire::Node>,
    /// The last tick ran out of budget with work still ready: the frame
    /// asks the host for the next tick at once instead of waiting for an
    /// event or an answer that may never come.
    busy: bool,
}

impl<A: App> Default for Driver<A> {
    fn default() -> Self {
        Self::new()
    }
}

/// How many `update` rounds one tick runs before it hands the rest to the
/// next: a handler that re-emits synchronously forever cannot pin the frame.
const MAX_ROUNDS: usize = 8;

/// How many times one poll pass revisits the tasks still woken.
const MAX_POLLS: usize = 64;

impl<A: App> Driver<A> {
    pub fn new() -> Self {
        Self::with_macos(cfg!(target_os = "macos"))
    }

    /// The host platform selects Command/word-jump semantics before app boot.
    pub fn with_macos(macos: bool) -> Self {
        Self::initialize(macos, || Ok(A::boot())).expect("ordinary boot is infallible")
    }

    fn initialize(
        macos: bool,
        boot: impl FnOnce() -> Result<(A, Task<A::Message>), String>,
    ) -> Result<Self, String> {
        let slots = slots::Context::with_macos(macos);
        let _context = slots.enter();
        let (app, boot) = boot()?;
        let mut driver = Self {
            slots,
            app,
            tasks: Vec::new(),
            subscriptions: HashSet::new(),
            observers: Vec::new(),
            last_root: None,
            busy: false,
        };
        spawn(&mut driver.tasks, boot);
        Ok(driver)
    }

    /// Delivers the host's events and returns the frame they produced.
    ///
    /// Events name entries in the tables the LAST view filled, so they are
    /// dispatched before the tables are reset for this view. An index the
    /// last frame did not hand out (the host raced a rebuild) is dropped.
    ///
    /// The frame always carries the tree, `unchanged` or patched or not, so
    /// a test can read it; the component export drops a tree the host can
    /// rebuild before it crosses.
    pub fn tick(&mut self, events: Vec<wire::Event>) -> wire::Frame {
        let _context = self.slots.enter();
        for message in slots::take_deferred::<A::Message>() {
            spawn(&mut self.tasks, self.app.update(message));
            self.settle();
        }
        self.settle();
        for event in events {
            let message = match event {
                observation @ (wire::Event::Observation { .. }
                | wire::Event::Mouse { .. }
                | wire::Event::Keyboard { .. }) => {
                    let valid = match &observation {
                        wire::Event::Observation { event, .. } => event.validate().is_ok(),
                        wire::Event::Mouse { event, .. } => event.sanitize().is_some(),
                        wire::Event::Keyboard { .. } => true,
                        _ => unreachable!(),
                    };
                    if valid {
                        let messages: Vec<_> = self
                            .observers
                            .iter()
                            .filter_map(|observe| observe(&observation))
                            .collect();
                        for message in messages {
                            spawn(&mut self.tasks, self.app.update(message));
                            self.settle();
                        }
                    }
                    None
                }
                wire::Event::Message(index) => slots::take_message::<A::Message>(index),
                wire::Event::Surface { handler, value } => {
                    slots::run_handler::<wire::SurfaceValue, A::Message>(handler, value)
                }
                wire::Event::Input { handler, text } => {
                    slots::run_handler::<String, A::Message>(handler, text)
                }
                wire::Event::EditorDocument { handler, message } => {
                    use wire::editor_document::EditorDocumentMessage;
                    if matches!(
                        message,
                        EditorDocumentMessage::Acknowledged { .. }
                            | EditorDocumentMessage::Failed { .. }
                    ) {
                        slots::finish_editor_transfer(message.id());
                        continue;
                    }
                    slots::run_handler::<wire::editor_document::EditorDocumentMessage, A::Message>(
                        handler, message,
                    )
                }
                wire::Event::EditorRequest { handler, request } => {
                    slots::run_handler::<wire::EditorRequest, A::Message>(handler, request)
                }
                wire::Event::EditorTransaction { handler, event } => {
                    if let wire::EditorTransactionEvent::Fault { id, .. }
                    | wire::EditorTransactionEvent::Cancelled { id, .. } = &event
                    {
                        slots::finish_editor_transfer(&wire::editor_document::EditorTransferId {
                            instance: id.instance,
                            document: id.document.clone(),
                            reset: id.reset,
                            serial: id.sequence,
                            attempt: id.attempt,
                        });
                    }
                    if let wire::EditorTransactionEvent::Cancelled { id, .. } = &event {
                        if !slots::editor_matches_pending(id) {
                            continue;
                        }
                        slots::editor_acknowledge(&event);
                    }
                    slots::run_handler::<wire::EditorTransactionEvent, A::Message>(handler, event)
                }
                wire::Event::Toggle { handler, on } => {
                    slots::run_handler::<bool, A::Message>(handler, on)
                }
                wire::Event::Slide { handler, value } => {
                    slots::run_handler::<f32, A::Message>(handler, value)
                }
                wire::Event::Select { handler, index } => {
                    slots::run_handler::<u32, A::Message>(handler, index)
                }
                wire::Event::Size {
                    handler,
                    width,
                    height,
                } => slots::run_handler::<(f32, f32), A::Message>(handler, (width, height)),
                wire::Event::Drag { handler, dx, dy } => {
                    slots::run_handler::<(f64, f64), A::Message>(handler, (dx, dy))
                }
                wire::Event::Pointer { handler, x, y } => {
                    slots::run_handler::<(f32, f32), A::Message>(handler, (x, y))
                }
                wire::Event::Scroll {
                    handler,
                    dx,
                    dy,
                    pixels,
                } => slots::run_handler::<(f32, f32, bool), A::Message>(handler, (dx, dy, pixels)),
                wire::Event::ScrollOffset {
                    handler,
                    x,
                    y,
                    relative_x,
                    relative_y,
                } => slots::run_handler::<(f32, f32, f32, f32), A::Message>(
                    handler,
                    (x, y, relative_x, relative_y),
                ),
                wire::Event::Response { id, result, done } => {
                    host::fulfill(id, result, done);
                    None
                }
                // The host dropped the tree the patches build on.
                wire::Event::Resync => {
                    self.last_root = None;
                    None
                }
            };
            if let Some(message) = message {
                spawn(&mut self.tasks, self.app.update(message));
                self.settle();
            }
        }
        // A response woke a task without a message of its own to run: poll
        // once more so what it produced reaches `update` before the view.
        self.settle();
        slots::reset();
        if slots::editor_transferring() {
            memo::invalidate();
        }
        let mut root = self.app.view();
        memo::finish_render();
        // Synchronous mount pruning can cancel work after the last settle.
        // Reconcile subscriptions and request another tick to drain woken
        // tasks; updating here would publish a tree from before that update.
        self.subscribe();
        self.busy |= slots::has_deferred()
            || self
                .tasks
                .iter()
                .any(|task| task.woken.0.load(Ordering::Relaxed));
        let unchanged = self.last_root.as_ref() == Some(&root);
        let mut patches = Vec::new();
        if !unchanged {
            // Patches against the last tree, unless there is none — a first
            // frame, or one after the host asked to resync — or the patches
            // would cross bigger than the tree itself.
            if let Some(last) = &mut self.last_root {
                patches = wire::diff(last, &mut root);
                if patches.len() > wire::MAX_PATCHES
                    || wire::encoded_size(&patches) >= wire::encoded_size(&root)
                {
                    patches.clear();
                }
            }
            // Remembered without the picture bytes this frame carried: the
            // next view names those pictures by hash alone, and that is
            // the same tree — and the tree the host keeps, which drops the
            // bytes the same way once it has the pictures.
            let mut kept = root.clone();
            kept.for_each_mut(&mut |node| match node {
                wire::Node::Svg { bytes, .. } => *bytes = None,
                wire::Node::Image { data, .. } | wire::Node::ImageViewer { data, .. } => {
                    *data = None
                }
                _ => {}
            });
            self.last_root = Some(kept);
        }
        let editor_decisions = slots::take_editor_responses();
        self.busy |= slots::editor_responses_ready();
        wire::Frame {
            upstream_sanitization: Default::default(),
            editor_decisions,
            editor_documents: slots::take_editor_documents(),
            mouse_interest: slots::mouse_interest(),
            event_interest: slots::event_interest(),
            root: Some(root),
            patches,
            requests: host::drain_outbox(),
            cancels: host::drain_cancels(),
            unchanged,
            busy: self.busy,
        }
    }

    /// Brings the subscription in line with the state, polls every woken
    /// task, and runs what they produced through `update` — whose own tasks
    /// join the pool and whose state changes move the subscription — until
    /// a round produces nothing or the round budget is spent. Spent with
    /// work still ready is what `busy` means.
    fn settle(&mut self) {
        for _ in 0..MAX_ROUNDS {
            self.subscribe();
            let (messages, cut_short) = poll_tasks(&mut self.tasks);
            self.busy = cut_short;
            if messages.is_empty() {
                return;
            }
            for message in messages {
                spawn(&mut self.tasks, self.app.update(message));
            }
        }
        self.busy = true;
    }

    fn subscribe(&mut self) {
        slots::set_mouse_interest(false);
        slots::clear_event_interest();
        let subscription = self.app.subscription();
        let next: HashSet<_> = subscription
            .recipes
            .iter()
            .map(|recipe| recipe.key)
            .collect();
        self.tasks
            .retain(|task| task.subscription.is_none_or(|key| next.contains(&key)));
        self.subscriptions.retain(|key| next.contains(key));
        self.observers = subscription.observers;
        for recipe in subscription.recipes {
            if self.subscriptions.insert(recipe.key) {
                self.tasks.push(Running {
                    subscription: Some(recipe.key),
                    woken: Arc::new(Woken(AtomicBool::new(true))),
                    stream: (recipe.start)(),
                });
            }
        }
    }
}

fn spawn<M: 'static>(tasks: &mut Vec<Running<M>>, task: Task<M>) {
    if let Some(stream) = task.0 {
        tasks.push(Running {
            subscription: None,
            woken: Arc::new(Woken(AtomicBool::new(true))),
            stream,
        });
    }
}
struct Woken(AtomicBool);
impl Wake for Woken {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}
fn poll_tasks<M: 'static>(tasks: &mut Vec<Running<M>>) -> (Vec<M>, bool) {
    let mut messages = Vec::new();
    for _ in 0..MAX_POLLS {
        let mut polled = false;
        tasks.retain_mut(|task| {
            if !task.woken.0.swap(false, Ordering::SeqCst) {
                return true;
            }
            polled = true;
            let waker = Waker::from(task.woken.clone());
            let mut context = Context::from_waker(&waker);
            match task.stream.as_mut().poll_next(&mut context) {
                Poll::Ready(Some(message)) => {
                    messages.push(message);
                    task.woken.0.store(true, Ordering::SeqCst);
                    true
                }
                Poll::Ready(None) => false,
                Poll::Pending => true,
            }
        });
        if !polled {
            return (messages, false);
        }
    }
    (
        messages,
        tasks.iter().any(|task| task.woken.0.load(Ordering::SeqCst)),
    )
}

/// A periodic guest subscription: a module has no clock, so the period is the
/// host's `clock.ticks` — which the app's manifest must declare `clock`
/// for. The route carries no instant, because a guest cannot make one.
/// A refusal is logged and ends the stream; the recipe hashes by period,
/// so a `subscribe` that keeps the same `every` keeps the same ticker.
pub fn every(period: Duration) -> Subscription<()> {
    Subscription::run_with(period, |period| ticks(*period))
}

/// Run `f` immediately, then once per host tick.
pub fn repeat<F, T>(f: fn() -> F, period: Duration) -> Subscription<T>
where
    F: Future<Output = T> + 'static,
    T: 'static,
{
    Subscription::run_with((f, period), |(f, period)| {
        let f = *f;
        Box::pin(
            futures::stream::once(std::future::ready(()))
                .chain(ticks(*period))
                .then(move |()| f()),
        )
    })
}

fn ticks(period: Duration) -> BoxStream<()> {
    let millis = i64::try_from(period.as_millis()).unwrap_or(i64::MAX);
    Box::pin(
        host::subscribe("clock.ticks", &millis.to_le_bytes()).filter_map(|answer| {
            std::future::ready(match answer {
                Ok(_) => Some(()),
                Err(message) => {
                    host::log(format!("`every` needs the host's clock: {message}"));
                    None
                }
            })
        }),
    )
}

/// The most a panic message may carry across the `panicked` import. A host
/// shows one line of it, and every byte over that is one the host lifts out
/// of guest memory before it can refuse anything — so the message is cut
/// here, where the guest still owns it, on a char boundary.
pub const MAX_PANIC_BYTES: usize = 1024;

/// The line the panic hook hands the host: the payload and where it came
/// from, cut to [`MAX_PANIC_BYTES`].
pub fn panic_line(message: &str, at: &str) -> String {
    let mut line = format!("{message} at {at}");
    if line.len() > MAX_PANIC_BYTES {
        let cut = (0..=MAX_PANIC_BYTES)
            .rev()
            .find(|at| line.is_char_boundary(*at))
            .unwrap_or(0);
        line.truncate(cut);
    }
    line
}

/// Appends the preferred window size and current wire epoch at compile time.
pub const fn manifest_bytes<const N: usize>(text: &str, preferred_size: &str) -> [u8; N] {
    let bytes = text.as_bytes();
    let size = preferred_size.as_bytes();
    assert!(N == bytes.len() + size.len() + 1 + wire::WIRE_EPOCH.ilog10() as usize + 1);
    let mut out = [0u8; N];
    let mut i = 0;
    while i < bytes.len() + size.len() {
        out[i] = if i < bytes.len() {
            bytes[i]
        } else {
            size[i - bytes.len()]
        };
        i += 1;
    }
    out[i] = b'\n';
    let mut epoch = wire::WIRE_EPOCH;
    let mut end = N;
    while end > i + 1 {
        end -= 1;
        out[end] = b'0' + (epoch % 10) as u8;
        epoch /= 10;
    }
    out
}

#[macro_export]
macro_rules! export_app {
    ($app:ident, $name:expr, $description:expr, [$($capability:literal),* $(,)?]) => {
        impl $crate::App for $app {
            type Message = Message;

            fn boot() -> (Self, $crate::Task<Self::Message>) {
                <$app>::boot()
            }

            fn view(&self) -> $crate::wire::Node {
                <$app>::view(self)
            }

            fn update(&mut self, message: Self::Message) -> $crate::Task<Self::Message> {
                <$app>::update(self, message)
            }

            fn subscription(&self) -> $crate::Subscription<Self::Message> {
                <$app>::subscription(self)
            }
        }

        impl $crate::SnapshotApp for $app {
            fn snapshot(&self) -> ::std::result::Result<::std::vec::Vec<u8>, ::std::string::String> { <$app>::snapshot(self) }
            fn restore(bytes: &[u8]) -> ::std::result::Result<Self, ::std::string::String> { <$app>::restore(bytes) }
        }

        const MANIFEST: &str = concat!("ice.manifest.v2\n", $name, "\n", $description, "\n" $(, $capability, ",")*, "\n");

        #[unsafe(link_section = "ice.manifest")]
        #[used]
        static MANIFEST_SECTION: [u8; MANIFEST.len() + <$app>::PREFERRED_WINDOW_SIZE.len() + 2 + $crate::wire::WIRE_EPOCH.ilog10() as usize] =
            $crate::manifest_bytes(MANIFEST, <$app>::PREFERRED_WINDOW_SIZE);

        thread_local! {
            static DRIVER: ::std::cell::RefCell<Option<$crate::Driver<$app>>> =
                const { ::std::cell::RefCell::new(None) };
        }


        pub fn boot_native() {
            DRIVER.with(|driver| *driver.borrow_mut() = Some($crate::Driver::new()));
        }

        pub fn snapshot_native() -> ::std::result::Result<::std::vec::Vec<u8>, ::std::string::String> {
            DRIVER.with(|driver| driver.borrow().as_ref().ok_or_else(|| ::std::string::String::from("initialize first"))?.snapshot())
        }

        pub fn restore_native(bytes: &[u8], macos: bool) -> ::std::result::Result<(), ::std::string::String> {
            let candidate = $crate::Driver::from_snapshot(bytes, macos)?;
            DRIVER.with(|driver| *driver.borrow_mut() = Some(candidate));
            ::std::result::Result::Ok(())
        }

        pub fn tick_native(events: Vec<$crate::wire::Event>) -> $crate::wire::Frame {
            DRIVER.with(|driver| driver.borrow_mut().as_mut().expect("boot first").tick(events))
        }

        #[cfg(target_arch = "wasm32")]
        mod wasm_exports {
            macro_rules! bindings {
                ($wit:literal) => {
                    $crate::wit_bindgen::generate!({
                        inline: $wit,
                        runtime_path: "::ducktape_view_guest::wit_bindgen::rt",
                    });
                };
            }
            $crate::wire::with_view_wit!(bindings);

            struct Component;

            fn install_panic_hook() {
                    // A trapped instance can never be entered again, so the
                    // message leaves through the host's import before the
                    // abort that follows the hook.
                    ::std::panic::set_hook(::std::boxed::Box::new(|info| {
                        let payload = info.payload();
                        let message = payload
                            .downcast_ref::<&str>()
                            .copied()
                            .or_else(|| payload.downcast_ref::<::std::string::String>().map(|text| text.as_str()))
                            .unwrap_or("panicked");
                        let at = info
                            .location()
                            .map(|location| ::std::format!("{}:{}", location.file(), location.line()))
                            .unwrap_or_else(|| "unknown".into());
                        panicked(&$crate::panic_line(message, &at));
                    }));
            }

            impl Guest for Component {
                fn init(macos: bool) {
                    install_panic_hook();
                    super::DRIVER.with(|driver| *driver.borrow_mut() = Some($crate::Driver::with_macos(macos)));
                }

                fn snapshot() -> Result<Vec<u8>, String> { super::snapshot_native() }
                fn restore(state: Vec<u8>, macos: bool) -> Result<(), String> {
                    install_panic_hook();
                    super::restore_native(&state, macos)
                }

                fn tick(events: Vec<u8>) -> Vec<u8> {
                    let events: Vec<$crate::wire::Event> =
                        $crate::wire::decode(&events).expect("invalid host event frame");
                    let mut frame = super::tick_native(events);
                    // The host keeps the tree it has, or patches it; the
                    // whole tree crosses only when neither will do.
                    if frame.unchanged || !frame.patches.is_empty() {
                        frame.root = None;
                    }
                    $crate::wire::encode(&frame)
                }
            }

            export!(Component);
        }
    };
}

mod combo;
pub use combo::Combo;

#[cfg(test)]
mod tests;
