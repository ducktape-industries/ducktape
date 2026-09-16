//! Native window effects are executed on GPUI's application thread. The app's
//! async actions await the result, so opening a window never reports success
//! before the platform has actually opened it.

use ducktape_view_guest::Task;
use futures::{
    StreamExt as _,
    channel::{mpsc, oneshot},
};
use gpui_kit::prelude::FluentBuilder;
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct WindowKey(u64);

impl WindowKey {
    pub(crate) fn unique() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowKind {
    Onboarding,
    Console,
    Huddle,
}

#[derive(Clone)]
pub(crate) struct KeyPress {
    pub(crate) key: String,
    pub(crate) modifiers: gpui_kit::Modifiers,
}

pub(crate) enum Command {
    Open {
        key: WindowKey,
        kind: WindowKind,
        reply: oneshot::Sender<WindowKey>,
    },
    Close(WindowKey),
    Raise(WindowKey),
    Clipboard(String),
    Focus(String),
    OpenLink(String),
    Quit,
}

pub(crate) struct PendingCommand {
    pub command: Command,
    pub completed: oneshot::Sender<()>,
}

fn sender() -> &'static Mutex<Option<mpsc::UnboundedSender<PendingCommand>>> {
    static SENDER: OnceLock<Mutex<Option<mpsc::UnboundedSender<PendingCommand>>>> = OnceLock::new();
    SENDER.get_or_init(Mutex::default)
}

pub(crate) fn commands() -> mpsc::UnboundedReceiver<PendingCommand> {
    let (send, receive) = mpsc::unbounded();
    let mut current = sender().lock().expect("native shell commands");
    assert!(current.is_none(), "one native shell per process");
    *current = Some(send);
    receive
}

async fn send(command: Command) {
    let (completed, received) = oneshot::channel();
    let pending = PendingCommand { command, completed };
    let sent = sender()
        .lock()
        .expect("native shell commands")
        .as_ref()
        .is_some_and(|sender| sender.unbounded_send(pending).is_ok());
    if !sent {
        tracing::error!(target: "ducktape::app", reason = "native_shell_closed", "native window command could not be delivered");
        return;
    }
    let _ = received.await;
}

pub(crate) fn open(kind: WindowKind) -> (WindowKey, Task<WindowKey>) {
    let key = WindowKey::unique();
    let task = Task::stream(
        futures::stream::once(async move {
            let (reply, receive) = oneshot::channel();
            send(Command::Open { key, kind, reply }).await;
            receive.await.ok()
        })
        .filter_map(std::future::ready),
    );
    (key, task)
}

fn effect<Message: 'static>(command: Command) -> Task<Message> {
    Task::future(async move {
        send(command).await;
    })
    .discard()
}

pub(crate) fn close<Message: 'static>(key: WindowKey) -> Task<Message> {
    effect(Command::Close(key))
}

pub(crate) fn raise<Message: 'static>(key: WindowKey) -> Task<Message> {
    effect(Command::Raise(key))
}

pub(crate) fn clipboard<Message: 'static>(text: String) -> Task<Message> {
    effect(Command::Clipboard(text))
}

pub(crate) fn focus<Message: 'static>(key: String) -> Task<Message> {
    effect(Command::Focus(key))
}

pub(crate) fn quit<Message: 'static>() -> Task<Message> {
    effect(Command::Quit)
}

/// A link a native widget wants opened — the same door a `duck://` URL handed
/// to the app from outside comes through. Nothing waits on it, so unlike the
/// commands above it needs no task to drive: a widget is not in the message
/// loop and has nothing to hand one to.
pub(crate) fn open_link(url: String) {
    let (completed, _dropped) = oneshot::channel();
    let pending = PendingCommand {
        command: Command::OpenLink(url),
        completed,
    };
    let sent = sender()
        .lock()
        .expect("native shell commands")
        .as_ref()
        .is_some_and(|sender| sender.unbounded_send(pending).is_ok());
    if !sent {
        tracing::error!(target: "ducktape::app", reason = "native_shell_closed", "a pressed link could not be delivered");
    }
}

use crate::{AppMessage as Message, Ducktape, ShellTab};
use gpui_kit::{
    AppContext as _, AsyncApp, Context, Entity, IntoElement, ParentElement as _, Render,
    Styled as _, Window,
};
use std::collections::{BTreeMap, HashMap};

struct Desktop {
    state: Ducktape,
    tray: crate::tray::Tray,
    windows: BTreeMap<WindowKey, gpui_kit::AnyWindowHandle>,
    views: BTreeMap<WindowKey, gpui_kit::WeakEntity<DesktopWindow>>,
    streams: HashMap<u64, gpui_kit::Task<()>>,
    pending_focus: Option<String>,
    pending_urls: Vec<String>,
}

impl Desktop {
    fn dispatch(&mut self, message: Message, cx: &mut Context<Self>) {
        // Native callbacks run on GPUI's thread, not a Tokio worker. Reducers
        // may construct effects which spawn immediately, before their first poll.
        let runtime = crate::module_view::runtime();
        let _runtime = runtime.enter();
        let appearance = self.state.appearance;
        let task = self.state.update(message);
        if appearance != self.state.appearance {
            self.sync_appearance(cx);
        }
        self.tray.sync(&self.state);
        self.start(task, cx).detach();
        self.subscriptions(cx);
        cx.notify();
        self.open_pending_urls(cx);
    }

    fn open_pending_urls(&mut self, cx: &mut Context<Self>) {
        let ready = self.state.connected
            && self.state.console_win.is_some()
            && !self.state.network_chain_id.is_empty();
        if !ready {
            return;
        }
        for url in std::mem::take(&mut self.pending_urls) {
            self.dispatch(Message::OpenMessageLink(url), cx);
        }
    }

    fn sync_appearance(&mut self, cx: &mut Context<Self>) {
        use gpui_kit::component::{Theme, ThemeMode};
        match self.state.appearance {
            crate::Appearance::Light => Theme::change(ThemeMode::Light, None, cx),
            crate::Appearance::Dark => Theme::change(ThemeMode::Dark, None, cx),
            crate::Appearance::System => Theme::sync_system_appearance(None, cx),
        }
        // System resolves through the kit: the palette every view paints
        // with must follow the mode the theme actually landed on.
        self.state.system_dark = Theme::global(cx).is_dark();
        configure_native_theme(cx);
    }

    fn start(&self, task: Task<Message>, cx: &mut Context<Self>) -> gpui_kit::Task<()> {
        let mut stream = task.into_stream();
        let runtime = crate::module_view::runtime();
        cx.spawn(async move |desktop, cx| {
            loop {
                let message = futures::future::poll_fn(|context| {
                    let _runtime = runtime.enter();
                    stream.poll_next_unpin(context)
                })
                .await;
                let Some(message) = message else {
                    break;
                };
                if desktop
                    .update(cx, |this, cx| this.dispatch(message, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
    }

    fn subscriptions(&mut self, cx: &mut Context<Self>) {
        // Boot also enters here without dispatch; stream constructors may spawn.
        let runtime = crate::module_view::runtime();
        let _runtime = runtime.enter();
        let recipes = self.state.subscriptions().into_recipes();
        self.streams
            .retain(|key, _| recipes.iter().any(|recipe| recipe.key == *key));
        for recipe in recipes {
            if self.streams.contains_key(&recipe.key) {
                continue;
            }
            let stream = (recipe.start)();
            let task = self.start(Task::stream(stream), cx);
            self.streams.insert(recipe.key, task);
        }
    }

    fn execute(&mut self, command: Command, cx: &mut Context<Self>) {
        match command {
            Command::Open { key, kind, reply } => self.open_window(key, kind, reply, cx),
            Command::Close(key) => self.close_window(key, cx),
            Command::Raise(key) => self.raise_window(key, cx),
            Command::Clipboard(text) => self.write_clipboard(text, cx),
            Command::Focus(key) => self.focus_control(key, cx),
            Command::OpenLink(url) => self.dispatch(Message::OpenMessageLink(url), cx),
            Command::Quit => self.quit(cx),
        }
    }

    fn open_window(
        &mut self,
        key: WindowKey,
        kind: WindowKind,
        reply: oneshot::Sender<WindowKey>,
        cx: &mut Context<Self>,
    ) {
        use gpui_kit::*;
        let size = match kind {
            crate::shell::WindowKind::Onboarding => size(px(480.0), px(680.0)),
            crate::shell::WindowKind::Console => size(px(1280.0), px(800.0)),
            // room for a stage at a readable size; it resizes from here
            crate::shell::WindowKind::Huddle => size(px(560.0), px(600.0)),
        };
        let model = cx.entity();
        let titlebar = match kind {
            crate::shell::WindowKind::Onboarding => None,
            crate::shell::WindowKind::Console => Some(TitlebarOptions {
                title: (!cfg!(target_os = "macos")).then(|| "Ducktape".into()),
                appears_transparent: cfg!(target_os = "macos"),
                traffic_light_position: Some(point(px(12.), px(12.))),
            }),
            crate::shell::WindowKind::Huddle => Some(TitlebarOptions {
                title: Some("Ducktape · Huddle".into()),
                ..Default::default()
            }),
        };
        let minimum = match kind {
            crate::shell::WindowKind::Onboarding => size,
            crate::shell::WindowKind::Console => gpui_kit::size(px(1040.), px(540.)),
            crate::shell::WindowKind::Huddle => gpui_kit::size(px(320.), px(340.)),
        };
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size, cx))),
            titlebar,
            window_min_size: Some(minimum),
            is_resizable: kind != crate::shell::WindowKind::Onboarding,
            app_id: Some("dev.ducktape.app".into()),
            kind: if kind == crate::shell::WindowKind::Huddle {
                gpui_kit::WindowKind::PopUp
            } else {
                gpui_kit::WindowKind::Normal
            },
            icon: image::RgbaImage::from_raw(
                128,
                128,
                include_bytes!("../assets/icon.rgba").to_vec(),
            )
            .map(std::sync::Arc::new),
            ..Default::default()
        };
        // Native window creation can synchronously render its root. Release
        // the model borrow before GPUI enters that renderer.
        cx.defer(move |cx| {
        let mut opened_view = None;
        let window_model = model.clone();
        match cx.open_window(options, |window, cx| {
            let view = cx.new(|cx| {
                cx.on_release(DesktopWindow::released).detach();
                let observer = cx.observe(&window_model, |_, _, cx| cx.notify());
                let activation = cx.observe_window_activation(
                    window,
                    move |this: &mut DesktopWindow, window, cx| {
                        let message = if window.is_window_active() {
                            Message::WindowFocused(key)
                        } else {
                            Message::WindowUnfocused(key)
                        };
                        let model = this.model.clone();
                        cx.defer(move |cx| model.update(cx, |model, cx| model.dispatch(message, cx)));
                    },
                );
                let focus = cx.focus_handle();
                focus.focus(window, cx);
                let keystrokes = DesktopWindow::intercept_global_keys(window, cx);
                DesktopWindow {
                    model: window_model,
                    kind,
                    module: None,
                    module_route: None,
                    route: None,
                    overlay_module: None,
                    overlay_route: None,
                    inputs: HashMap::new(),
                    input_step: None,
                    qr: None,
                    focus,
                    _activation: activation,
                    _observer: observer,
                    _keystrokes: keystrokes,
                }
            });
            opened_view = Some(view.downgrade());
            let closing = view.downgrade();
            window.on_window_should_close(cx, move |window, cx| {
                let _ = closing.update(cx, |this, cx| {
                    this.observe_module_window(view_wire::events::Window::CloseRequested, cx)
                });
                release_window_input(window, cx);
                true
            });
            cx.new(|cx| gpui_kit::component::Root::new(view, window, cx))
        }) {
            Ok(handle) => {
                model.update(cx, |model, _| {
                    model.windows.insert(key, handle.into());
                    if let Some(view) = opened_view {
                        model.views.insert(key, view);
                    }
                });
                let _ = reply.send(key);
            }
            Err(error) => {
                tracing::error!(target: "ducktape::app", reason = "native_window_open_failed", %error, "window could not be opened");
                model.update(cx, |model, cx| {
                    model.state.onboarding_error = format!("The window could not be opened: {error}");
                    cx.notify();
                });
            }
        }
        });
    }

    fn close_window(&mut self, key: WindowKey, cx: &mut Context<Self>) {
        let Some(handle) = self.windows.remove(&key) else {
            return;
        };
        if let Some(view) = self.views.remove(&key) {
            let _ = view.update(cx, |this, cx| {
                this.observe_module_window(view_wire::events::Window::CloseRequested, cx)
            });
        }
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, cx| {
                release_window_input(window, cx);
                window.remove_window();
            });
        });
        self.dispatch(Message::WindowWasClosed(key), cx);
    }

    fn raise_window(&mut self, key: WindowKey, cx: &mut Context<Self>) {
        let Some(handle) = self.windows.get(&key) else {
            return;
        };
        let _ = handle.update(cx, |_, window, _| window.activate_window());
    }

    fn write_clipboard(&self, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text));
    }

    fn focus_control(&mut self, key: String, cx: &mut Context<Self>) {
        self.pending_focus = Some(key);
        cx.notify();
    }

    fn quit(&mut self, cx: &mut Context<Self>) {
        let windows = self.windows.values().copied().collect::<Vec<_>>();
        cx.defer(move |cx| {
            for handle in windows {
                let _ = handle.update(cx, |_, window, cx| release_window_input(window, cx));
            }
            cx.quit();
        });
    }
}

fn release_window_input(window: &mut gpui_kit::Window, cx: &mut gpui_kit::App) {
    // A platform window can outlive its GPUI window during asynchronous native
    // teardown. Complete the blur frame first so it no longer holds an input
    // handler (and therefore a strong entity reference) when teardown begins.
    window.blur(cx);
    window.draw(cx).clear(cx);
}

fn huddle_props(state: &Ducktape) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"panel": {
        "instance": state.huddle_instance, "channel": state.huddle_channel,
        "joined": state.huddle_joined, "loading": state.loading,
        "dark": state.is_dark(), "network": state.network_chain_id,
        "endpoint": state.connected_rpc, "account": state.account_number,
        "user_key": state.settings_user_key,
        "status": state.call_status, "joined_at": state.huddle_joined_at,
        "now": state.huddle_now, "muted": state.call_muted,
        "camera": state.call_camera, "sharing": state.call_sharing,
        // Only the host can enumerate displays and windows, so the view gets
        // the labels and answers with a row index — see `Action::Share`.
        "share_targets": state.share_picker.iter().map(|choice| &choice.label).collect::<Vec<_>>(),
        // GATED ON `call_sharing` HERE, which is why `sharing_label` needs
        // clearing on none of the paths that end a share.
        "sharing_label": if state.call_sharing { state.sharing_label.as_str() } else { "" },
        "speaking": state.call_speaking, "stage": state.huddle_stage,
        "tiles": state.huddle_tiles, "video_live": state.call_video_live,
        "peers": state.call_peers,
    }}))
    .expect("call panel facts")
}

fn huddle_route(event: crate::module_view::ModuleViewEvent) -> Message {
    match event.kind.as_str() {
        "mute" => Message::ToggleCallMute,
        "camera" => Message::ToggleCallCamera,
        "screen" => Message::ToggleCallScreen,
        "share" => match event.detail.trim().parse::<usize>() {
            Ok(index) => Message::PickShareTarget(index),
            Err(_) => Message::CloseSharePicker,
        },
        "share_cancel" => Message::CloseSharePicker,
        "channel" => Message::HuddleGoChannel,
        "leave" => Message::LeaveHuddleHere,
        _ => Message::ExternalUrlFailed("unrecognized call control".to_owned().into()),
    }
}

pub(crate) struct DesktopWindow {
    model: Entity<Desktop>,
    kind: WindowKind,
    module: Option<(&'static str, Entity<crate::module_view::NativeModuleView>)>,
    module_route: Option<fn(crate::module_view::ModuleViewEvent) -> Message>,
    route: Option<gpui_kit::Subscription>,
    /// The overlay's own seat. The bell draws the deployed `inbox` view, and
    /// the tab underneath must keep its seat while it does — one slot cannot
    /// hold both.
    overlay_module: Option<Entity<crate::module_view::NativeModuleView>>,
    overlay_route: Option<gpui_kit::Subscription>,
    inputs: HashMap<&'static str, NativeInput>,
    input_step: Option<crate::HubStep>,
    qr: Option<(String, Entity<crate::view_tree::ViewTree>)>,
    focus: gpui_kit::FocusHandle,
    _activation: gpui_kit::Subscription,
    _observer: gpui_kit::Subscription,
    _keystrokes: gpui_kit::Subscription,
}

struct NativeInput {
    state: Entity<gpui_kit::component::input::InputState>,
    subscription: gpui_kit::Subscription,
}

impl DesktopWindow {
    fn intercept_global_keys(
        window: &gpui_kit::Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Subscription {
        let window_id = window.window_handle().window_id();
        let view = cx.entity().downgrade();
        // Native input actions resolve before element key listeners. Only the
        // owning window's shell commands precede them; ordinary keys stay native.
        cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle().window_id() != window_id {
                return;
            }
            // This interceptor runs before the guest editor's, which cannot
            // stop it: the stack says whether the key lands in one.
            let in_guest_editor = event
                .context_stack
                .iter()
                .any(|context| context.contains(crate::editor::wire::GUEST_EDITOR_CONTEXT));
            let _ = view.update(cx, |view, cx| {
                view.global_key(
                    KeyPress {
                        key: event.keystroke.key.clone(),
                        modifiers: event.keystroke.modifiers,
                    },
                    in_guest_editor,
                    cx,
                );
            });
        })
    }

    fn global_key(&mut self, key: KeyPress, in_guest_editor: bool, cx: &mut Context<Self>) {
        let state = &self.model.read(cx).state;
        let chord = crate::backend::command_chord(key.key.clone(), key.modifiers);
        let palette =
            crate::backend::palette_key_action(key.key.clone(), key.modifiers, state.palette_open);
        // A guest editor claims Ctrl+K for a link: the palette does not open
        // over it. Closing an open palette is unaffected — its focus is not in
        // the editor.
        let editor_claims_the_chord = in_guest_editor && palette == "open";
        let palette = match editor_claims_the_chord {
            true => "none".to_owned(),
            false => palette,
        };
        let escape =
            crate::backend::escape_target(key.key.clone(), state.palette_open, state.bell_open);
        let global = palette != "none" || !escape.is_empty();
        let message = match chord {
            crate::CommandChord::Quit | crate::CommandChord::CloseWindow => {
                Message::CommandChordPressed(key)
            }
            crate::CommandChord::Ignored => {
                if !global {
                    return;
                }
                Message::GlobalKeyPressed(key)
            }
        };
        self.model
            .update(cx, |model, cx| model.dispatch(message, cx));
        cx.stop_propagation();
    }

    fn released(&mut self, cx: &mut gpui_kit::App) {
        self.hide_module(cx);
        self.unseat_inbox(cx);
        self.observe_module_window(view_wire::events::Window::Closed, cx);
    }
    /// The bell's body IS the deployed `inbox` view. It takes the session
    /// facts every view gets and nothing else: what the rows say, which of
    /// them are unread, and where each one's door leads are its reads, not
    /// the app's. The chrome around it — the card, the heading, the dismiss
    /// — stays the shell's.
    fn seat_inbox(&mut self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        use gpui_kit::IntoElement as _;
        let spec = {
            let state = &self.model.read(cx).state;
            crate::module_view::inbox_view(
                state.is_dark(),
                state.connected,
                &state.network_chain_id,
                &state.account_number,
            )
        };
        if self.overlay_module.is_none() {
            let view = cx.new(|_| crate::module_view::NativeModuleView::new(spec.module));
            let model = self.model.clone();
            self.overlay_route = Some(cx.subscribe(&view, move |_, _, event, cx| {
                model.update(cx, |model, cx| {
                    model.dispatch(Message::RegisteredViewEvent(event.clone()), cx)
                });
            }));
            self.overlay_module = Some(view);
        }
        let view = self
            .overlay_module
            .as_ref()
            .expect("inbox view seated")
            .clone();
        view.update(cx, |view, cx| view.set_props(spec.props, cx));
        view.into_any_element()
    }
    /// A closed bell returns its seat: nothing is drawn, so nothing keeps
    /// re-reading the queue. The headless errand behind the rail's number is
    /// the only inbox read a closed bell pays for.
    fn unseat_inbox(&mut self, cx: &mut gpui_kit::App) {
        let Some(view) = self.overlay_module.take() else {
            return;
        };
        self.overlay_route = None;
        let intents = view.update(cx, |view, _| view.hide());
        let model = self.model.clone();
        cx.defer(move |cx| {
            for intent in intents {
                model.update(cx, |model, cx| {
                    model.dispatch(Message::RegisteredViewEvent(intent), cx)
                });
            }
        });
    }
    fn hide_module(&mut self, cx: &mut gpui_kit::App) {
        let (Some((_, module)), Some(route)) = (&self.module, self.module_route) else {
            return;
        };
        let intents = module.update(cx, |module, _| module.hide());
        let model = self.model.clone();
        cx.defer(move |cx| {
            for intent in intents {
                model.update(cx, |model, cx| model.dispatch(route(intent), cx));
            }
        });
    }
    fn observe_module_window(
        &mut self,
        event: view_wire::events::Window,
        cx: &mut gpui_kit::App,
    ) {
        let (Some((_, module)), Some(route)) = (&self.module, self.module_route) else {
            return;
        };
        let intents = module.update(cx, |module, cx| {
            module.observe_final_window_event(event, cx)
        });
        // Closing can run inside a model update. Keep the model and frozen
        // route alive until that borrow ends, independent of the presenter.
        let model = self.model.clone();
        cx.defer(move |cx| {
            for intent in intents {
                model.update(cx, |model, cx| model.dispatch(route(intent), cx));
            }
        });
    }
    #[cfg(test)]
    pub(crate) fn test_state<'a>(&self, cx: &'a gpui_kit::App) -> &'a Ducktape {
        &self.model.read(cx).state
    }

    /// The two seats: the tab's module id, and whether the overlay holds one.
    #[cfg(test)]
    pub(crate) fn test_seats(&self) -> (Option<&'static str>, bool) {
        (
            self.module.as_ref().map(|(module, _)| *module),
            self.overlay_module.is_some(),
        )
    }

    #[cfg(test)]
    pub(crate) fn test_dispatch(&mut self, message: Message, cx: &mut Context<Self>) {
        self.model
            .update(cx, |model, cx| model.dispatch(message, cx));
    }
    fn value(&self, key: &'static str, cx: &gpui_kit::App) -> String {
        self.inputs
            .get(key)
            .map(|input| input.state.read(cx).value().to_string())
            .unwrap_or_default()
    }

    fn input(
        &mut self,
        key: &'static str,
        placeholder: &'static str,
        masked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        use gpui_kit::component::input::{Input, InputEvent, InputState};
        if !self.inputs.contains_key(key) {
            let state = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .masked(masked)
            });
            let model = self.model.clone();
            let subscription = cx.subscribe(&state, move |_, input, event, cx| {
                let InputEvent::Change = event else {
                    return;
                };
                let secret_slot = matches!(key, "restore_words" | "join_invite");
                if secret_slot {
                    let text = input.read(cx).value().to_string();
                    model.update(cx, |model, cx| {
                        model.dispatch(Message::SecretTyped(key.into(), text), cx)
                    });
                }
                let palette_input = key == "palette-input";
                if palette_input {
                    let text = input.read(cx).value().to_string();
                    model.update(cx, |model, cx| {
                        model.dispatch(Message::PaletteChanged(text), cx)
                    });
                }
                cx.notify();
            });
            self.inputs.insert(
                key,
                NativeInput {
                    state,
                    subscription,
                },
            );
        }
        Input::new(&self.inputs[key].state)
            .aria_label(placeholder)
            .into_any_element()
    }

    fn action(
        &self,
        key: impl Into<gpui_kit::ElementId>,
        label: impl Into<gpui_kit::SharedString>,
        message: Message,
        disabled: bool,
    ) -> gpui_kit::component::button::Button {
        use gpui_kit::component::Disableable as _;
        let model = self.model.clone();
        gpui_kit::component::button::Button::new(key)
            .label(label)
            .disabled(disabled)
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                model.update(cx, |model, cx| model.dispatch(message.clone(), cx))
            })
    }

    fn submit(
        &self,
        key: &'static str,
        label: &'static str,
        disabled: bool,
        message: impl Fn(&Self, &gpui_kit::App) -> Message + 'static,
        cx: &mut Context<Self>,
    ) -> gpui_kit::component::button::Button {
        use gpui_kit::component::Disableable as _;
        gpui_kit::component::button::Button::new(key)
            .label(label)
            .disabled(disabled)
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                let message = message(this, cx);
                this.model
                    .update(cx, |model, cx| model.dispatch(message, cx));
            }))
    }

    fn onboarding(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        use crate::HubStep;
        use gpui_kit::component::button::ButtonVariants as _;
        use gpui_kit::*;
        let colors = gpui_kit::component::Theme::global(cx).color_tokens();
        let state = &self.model.read(cx).state;
        let step = state.hub_step;
        let busy = state.mutation_phase != crate::MutationPhase::Idle;
        let error = state.onboarding_error.clone();
        let step_changed = self.input_step != Some(step);
        if step_changed {
            for (_, input) in std::mem::take(&mut self.inputs) {
                drop(input.subscription);
                input
                    .state
                    .update(cx, |state, cx| state.set_value("", window, cx));
            }
            self.input_step = Some(step);
        }
        let hero = |title: &'static str, subtitle: &'static str| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(px(design::type_scale::TITLE as f32))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(13.5))
                        .text_color(colors.muted_foreground)
                        .child(subtitle),
                )
        };
        let panel = || {
            div()
                .border_1()
                .border_color(colors.border)
                .bg(colors.surface)
                .rounded(px(design::radius::CARD as f32))
                .overflow_hidden()
        };
        let hint = |text: String| {
            div()
                .text_size(px(12.5))
                .text_color(colors.muted_foreground)
                .child(text)
        };
        let mut body = div().flex().flex_col().gap_4().w_full();
        body = match step {
            HubStep::Loading => body.child(hint("Opening your workspace…".into())),
            HubStep::Wallets => {
                let state = &self.model.read(cx).state;
                let unlock_label = if busy {
                    state.wallet_opening_status
                } else {
                    "Unlock"
                };
                let selected = state.hub_wallet_selected.clone();
                let wallets = state.hub_wallets.clone();
                body = body.child(hero("Welcome back", "Choose a wallet to sign in with."));
                let mut list = panel().flex().flex_col().p_2().gap_1();
                for wallet in wallets {
                    let picked = wallet.name == selected;
                    list = list.child(
                        self.action(
                            format!("wallet/{}", wallet.name),
                            format!("{} · {}", wallet.name, wallet.state),
                            Message::PickWallet(wallet.name),
                            busy,
                        )
                        .ghost()
                        .w_full()
                        .when(picked, |button| button.secondary()),
                    );
                }
                if !selected.is_empty() {
                    list = list.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .p_2()
                            .child(self.input("unlock", "Wallet password", true, window, cx))
                            .child(
                                self.submit(
                                    "unlock-submit",
                                    unlock_label,
                                    busy,
                                    |this, cx| Message::UnlockSubmit(this.value("unlock", cx)),
                                    cx,
                                )
                                .loading(busy)
                                .primary()
                                .w_full()
                                .h_8(),
                            ),
                    );
                }
                body.child(list).child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(
                            self.action(
                                "wallet-create",
                                "Create a wallet",
                                Message::LoginSkip,
                                busy,
                            )
                            .outline(),
                        )
                        .child(
                            self.action(
                                "wallet-restore",
                                "Restore a wallet",
                                Message::GoRestore,
                                busy,
                            )
                            .ghost(),
                        )
                        .child(
                            self.action("wallet-networks", "Networks", Message::GoNetworks, busy)
                                .ghost(),
                        ),
                )
            }
            HubStep::Password => {
                body = body
                    .child(hero(
                        "Protect your wallet",
                        "A password encrypts the key on this device.",
                    ))
                    .child(self.input("password", "Password", true, window, cx))
                    .child(self.input("password-confirm", "Confirm password", true, window, cx));
                let problem = crate::backend::password_problem(
                    &self.value("password", cx),
                    &self.value("password-confirm", cx),
                );
                let invalid = busy || !problem.is_empty();
                body.when(!problem.is_empty(), |body| body.child(hint(problem)))
                    .child(
                        self.submit(
                            "password-submit",
                            "Create wallet",
                            invalid,
                            |this, cx| Message::PasswordSubmit(this.value("password", cx)),
                            cx,
                        )
                        .primary()
                        .w_full()
                        .h_8(),
                    )
                    .child(
                        self.action("password-back", "Back", Message::GoLogin, busy)
                            .ghost(),
                    )
            }
            HubStep::Phrase => {
                body = body.child(hero(
                    "Write down your recovery phrase",
                    "Keep it private. This phrase can restore your wallet.",
                ));
                let mut words = panel().flex().flex_col().p_3().gap_1();
                for row in crate::backend::phrase_rows() {
                    let word = |number: String, text: String| {
                        div()
                            .flex_1()
                            .flex()
                            .gap_2()
                            .child(
                                div()
                                    .w(px(24.))
                                    .text_size(px(12.))
                                    .map(mono_family)
                                    .text_color(colors.muted_foreground)
                                    .child(number),
                            )
                            .child(div().map(mono_family).child(text))
                    };
                    words = words.child(
                        div()
                            .flex()
                            .child(word(row.left_number.to_string(), row.left_word.to_string()))
                            .child(word(row.right_number.to_string(), row.right_word.to_string())),
                    );
                }
                body.child(words).child(
                    self.action(
                        "phrase-saved",
                        "I wrote it down",
                        Message::PhraseWrittenDown,
                        busy,
                    )
                    .primary()
                    .w_full()
                    .h_8(),
                )
            }
            HubStep::Confirm => body
                .child(hero(
                    "Confirm your phrase",
                    "Type the requested words to prove the phrase is written down.",
                ))
                .child(hint(crate::backend::recovery_prompt()))
                .child(self.input(
                    "phrase-answer",
                    "Requested words, separated by spaces",
                    true,
                    window,
                    cx,
                ))
                .child(
                    self.submit(
                        "phrase-confirm",
                        "Confirm recovery phrase",
                        busy,
                        |this, cx| Message::ConfirmPhraseSubmit(this.value("phrase-answer", cx)),
                        cx,
                    )
                    .primary()
                    .w_full()
                    .h_8(),
                )
                .child(
                    self.action(
                        "phrase-again",
                        "Show phrase again",
                        Message::ShowPhraseAgain,
                        busy,
                    )
                    .ghost(),
                ),
            HubStep::Restore => body
                .child(hero(
                    "Restore your wallet",
                    "The recovery phrase rebuilds the key on this device.",
                ))
                .child(self.input("restore-name", "Wallet name", false, window, cx))
                .child(self.input("restore_words", "Recovery phrase", true, window, cx))
                .child(self.input("restore-password", "New password", true, window, cx))
                .child(
                    self.submit(
                        "restore-submit",
                        "Restore",
                        busy,
                        |this, cx| {
                            Message::RestoreSubmit(
                                this.value("restore-name", cx),
                                this.value("restore-password", cx),
                            )
                        },
                        cx,
                    )
                    .primary()
                    .w_full()
                    .h_8(),
                )
                .child(
                    self.action("restore-back", "Back", Message::GoLogin, busy)
                        .ghost(),
                ),
            HubStep::Networks => {
                let state = &self.model.read(cx).state;
                let networks = state.hub_networks.clone();
                let selected = state.hub_selected.clone();
                body = body.child(hero("Your networks", "Choose where your team works."));
                let empty = networks.is_empty();
                let mut recent = panel().child(
                    div()
                        .px_4()
                        .py_3()
                        .border_b_1()
                        .border_color(colors.border)
                        .flex()
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(12.5))
                                .font_weight(FontWeight::MEDIUM)
                                .child("Saved networks"),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .map(mono_family)
                                .text_color(colors.muted_foreground)
                                .child(networks.len().to_string()),
                        ),
                );
                if empty {
                    recent = recent.child(
                        div()
                            .px_4()
                            .py_6()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(15.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child("No networks yet"),
                            )
                            .child(hint(
                                "Join a network or connect to a node below.".into(),
                            )),
                    );
                }
                let refused = crate::backend::selected_network_refuses(&networks, &selected);
                for network in networks {
                    let label = crate::backend::network_row_label(&network);
                    let picked = network.id == selected;
                    recent = recent.child(
                        div()
                            .flex()
                            .gap_1()
                            .px_2()
                            .py_1()
                            .child(
                                self.action(
                                    format!("network/{}", network.id),
                                    label,
                                    Message::PickNetwork(network.id.clone()),
                                    busy,
                                )
                                .ghost()
                                .flex_1()
                                .when(picked, |button| button.secondary()),
                            )
                            .child(
                                self.action(
                                    format!("forget/{}", network.id),
                                    "Forget",
                                    Message::ForgetNetworkSubmit(network.id),
                                    busy,
                                )
                                .ghost(),
                            ),
                    );
                }
                // a measured contract mismatch disables the open the way no
                // selection does: the row's own line says why.
                let no_selection = busy || selected.is_empty() || refused;
                if !empty {
                    recent = recent.child(
                        div()
                            .p_3()
                            .border_t_1()
                            .border_color(colors.border)
                            .child(
                                self.action(
                                    "network-open",
                                    "Open network",
                                    Message::OpenNetworkSubmit,
                                    no_selection,
                                )
                                .primary()
                                .w_full()
                                .h_8(),
                            ),
                    );
                }
                body.child(recent)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child("Connect directly"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(
                                        div().flex_1().child(self.input(
                                            "remote",
                                            "Remote node address",
                                            false,
                                            window,
                                            cx,
                                        )),
                                    )
                                    .child(
                                        self.submit(
                                            "remote-connect",
                                            "Connect",
                                            busy || self.value("remote", cx).trim().is_empty(),
                                            |this, cx| {
                                                Message::ConnectRemoteSubmit(this.value("remote", cx))
                                            },
                                            cx,
                                        )
                                        .outline(),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .border_t_1()
                            .border_color(colors.border)
                            .pt_4()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(hint("Already have an invitation?".into()))
                            .child(
                                self.action(
                                    "network-join",
                                    "Join with invitation",
                                    Message::GoJoin,
                                    busy,
                                )
                                .outline(),
                            ),
                    )
            }
            HubStep::Join => body
                .child(hero("Join your team", "One invitation. A shared place to work."))
                .child(
                    panel()
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(
                            div()
                                .text_size(px(12.5))
                                .font_weight(FontWeight::MEDIUM)
                                .child("Network invitation"),
                        )
                        .child(self.input("join_invite", "Invitation", true, window, cx))
                        .child(hint(
                            "Paste the invitation shared by a network member. It stays hidden on this screen."
                                .into(),
                        )),
                )
                .child(
                    self.action(
                        "join-submit",
                        "Join network",
                        Message::JoinNetworkSubmit,
                        busy || self.value("join_invite", cx).trim().is_empty(),
                    )
                    .primary()
                    .w_full()
                    .h_8(),
                )
                .child(
                    self.action("join-back", "Back to networks", Message::GoNetworks, busy)
                        .ghost()
                        .w_full(),
                )
                .child(
                    div()
                        .border_t_1()
                        .border_color(colors.border)
                        .pt_4()
                        .child(hint(
                            "Your wallet identifies you. The invitation connects you to the right workspace."
                                .into(),
                        )),
                ),
            HubStep::Provisioning => {
                body = body.child(hero(
                    "Setting up your network",
                    "This takes a moment on first launch.",
                ));
                let mut steps = panel().flex().flex_col().p_3().gap_2();
                for step in &self.model.read(cx).state.provision_steps {
                    steps = steps.child(
                        div()
                            .flex()
                            .justify_between()
                            .gap_3()
                            .child(div().child(step.label.clone()))
                            .child(hint(step.state.clone())),
                    );
                }
                body.child(steps)
            }
            HubStep::Live => body
                .child(hero(
                    "Your network is ready",
                    "Invite your team, then open the workspace.",
                ))
                .child(
                    self.action(
                        "copy-invite",
                        "Copy invitation",
                        Message::CopyOnboardingInvite,
                        busy,
                    )
                    .outline()
                    .w_full()
                    .h_8(),
                )
                .child(
                    self.action("enter-console", "Open Ducktape", Message::EnterConsole, busy)
                        .primary()
                        .w_full()
                        .h_8(),
                ),
            HubStep::Account => {
                let state = &self.model.read(cx).state;
                let detail = state.ceremony_detail.clone();
                let left = state.ceremony_left.clone();
                let payload = state.ceremony_qr.clone();
                body = body
                    .child(hero("Your account", "One account, every device you sign in from."))
                    .when(!detail.is_empty(), |body| body.child(hint(detail)))
                    .when(!left.is_empty(), |body| body.child(hint(left)));
                if !payload.is_empty() {
                    let changed = self
                        .qr
                        .as_ref()
                        .is_none_or(|(current, _)| current != &payload);
                    if changed {
                        let node = view_wire::Node::Qr {
                            key: "account-qr".into(),
                            code: view_wire::Qr {
                                payload: Some(payload.as_bytes().to_vec()),
                                size: Some(view_wire::QrSize::Total(220.0)),
                                ..Default::default()
                            },
                        };
                        self.qr =
                            Some((payload, cx.new(|_| crate::view_tree::ViewTree::new(node))));
                    }
                    body = body.child(self.qr.as_ref().expect("account QR").1.clone());
                }
                body.child(self.input("account-name", "Account name", false, window, cx))
                    .child(
                        self.submit(
                            "account-create",
                            "Create account",
                            busy,
                            |this, cx| {
                                Message::WelcomeCreateSubmit(this.value("account-name", cx))
                            },
                            cx,
                        )
                        .primary()
                        .w_full()
                        .h_8(),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .child(
                                self.action(
                                    "account-login",
                                    "Sign in",
                                    Message::WelcomeLoginSubmit,
                                    busy,
                                )
                                .outline(),
                            )
                            .child(
                                self.action(
                                    "account-desktop",
                                    "Use this device",
                                    Message::WelcomeDesktop,
                                    busy,
                                )
                                .outline(),
                            )
                            .child(
                                self.action(
                                    "account-skip",
                                    "Continue without account",
                                    Message::WelcomeSkip,
                                    busy,
                                )
                                .ghost(),
                            )
                            .child(
                                self.action("account-cancel", "Cancel", Message::WelcomeCancel, false)
                                    .ghost(),
                            ),
                    )
            }
        };
        let entering = self.model.read(cx).state.console_entry == crate::ConsoleEntry::Entering;
        if entering {
            body = div()
                .flex()
                .flex_col()
                .gap_4()
                .w_full()
                .child(hero(
                    "Opening workspace",
                    "You can cancel and choose another network.",
                ))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(gpui_kit::component::spinner::Spinner::new())
                        .child(hint(self.model.read(cx).state.connection_progress.clone())),
                )
                .child(
                    self.action("connection-cancel", "Cancel", Message::GoNetworks, false)
                        .outline(),
                );
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.background)
            .text_color(colors.foreground)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_5()
                    .h(px(64.))
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .size(px(26.))
                                    .rounded(px(design::radius::CONTROL as f32))
                                    .bg(colors.foreground)
                                    .text_color(colors.background)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .font_weight(FontWeight::BOLD)
                                    .child("D"),
                            )
                            .child(
                                div()
                                    .text_size(px(13.5))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Ducktape"),
                            )
                            .on_mouse_down(gpui_kit::MouseButton::Left, |_, window, _| {
                                window.start_window_move()
                            }),
                    )
                    .child(
                        self.action("launch-close", "×", Message::CloseLaunchWindow, false)
                            .ghost()
                            .accessibility_label("Close window")
                            .w_8()
                            .h_8(),
                    ),
            )
            .child(
                div()
                    .id("onboarding-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_5()
                    .child(body)
                    .when(!error.is_empty(), |element| {
                        element.child(
                            div()
                                .mt_4()
                                .p_3()
                                .border_1()
                                .rounded(px(design::radius::CONTROL as f32))
                                .border_color(colors.destructive)
                                .text_color(colors.destructive)
                                .text_size(px(12.5))
                                .child(error),
                        )
                    }),
            )
            .child(
                div()
                    .h(px(36.))
                    .px_5()
                    .flex_shrink_0()
                    .border_t_1()
                    .border_color(colors.border)
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(colors.muted_foreground)
                            .child("A workspace your team runs."),
                    ),
            )
            .into_any_element()
    }

    fn huddle(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        use gpui_kit::IntoElement as _;
        let props = huddle_props(&self.model.read(cx).state);
        if self.module.is_none() {
            let view = cx.new(|_| crate::module_view::NativeModuleView::new("call"));
            let model = self.model.clone();
            self.route = Some(cx.subscribe(&view, move |_, _, event, cx| {
                model.update(cx, |model, cx| {
                    model.dispatch(huddle_route(event.clone()), cx)
                });
            }));
            self.module = Some(("call", view));
            self.module_route = Some(huddle_route);
        }
        let view = self.module.as_ref().expect("call view seated").1.clone();
        view.update(cx, |view, cx| view.set_props(props, cx));
        view.into_any_element()
    }

    fn console(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        use gpui_kit::component::{Sizable as _, button::ButtonVariants as _};
        use gpui_kit::*;
        let colors = gpui_kit::component::Theme::global(cx).color_tokens();
        let (spec, route) = self.model.read(cx).state.native_view();
        let module_changed = self
            .module
            .as_ref()
            .is_none_or(|(module, _)| *module != spec.module);
        if module_changed {
            self.hide_module(cx);
            let view = cx.new(|_| crate::module_view::NativeModuleView::new(spec.module));
            let model = self.model.clone();
            self.route = Some(cx.subscribe(&view, move |_, _, event, cx| {
                model.update(cx, |model, cx| model.dispatch(route(event.clone()), cx));
            }));
            self.module = Some((spec.module, view));
            self.module_route = Some(route);
        }
        let view = self.module.as_ref().expect("module seated").1.clone();
        view.update(cx, |view, cx| view.set_props(spec.props, cx));
        let selected_tab = self.model.read(cx).state.shell_tab;
        // every tab by its module: a seat tasting a proposed view says so
        // on its label
        let label = |module: &'static str| {
            crate::module_view::tab_label(module, &crate::module_view::module_name(module))
        };
        // the dashboard leads the rail, above every section: it is the
        // network at a glance, not a workspace tool or a network tool
        let registered = crate::module_view::registered_views();
        let dashboard = registered
            .iter()
            .copied()
            .filter(|module| *module == HOME_VIEW);
        let mut navigation: Vec<(ShellTab, String)> = dashboard
            .map(|module| (ShellTab::Registered(module), label(module)))
            .collect();
        navigation.extend([
            (ShellTab::Chat, label("chat")),
            (ShellTab::Pages, label("pages")),
            (ShellTab::Forge, label("forge")),
            (ShellTab::Agents, label("agents")),
            (ShellTab::Files, label("files")),
        ]);
        // the other views the connected node's registry lists are workspace
        // tools: they follow the built-in workspace tabs, in the registry's
        // order, named by their manifests
        navigation.extend(
            registered
                .into_iter()
                .filter(|module| *module != HOME_VIEW && *module != "call")
                .map(|module| (ShellTab::Registered(module), label(module))),
        );
        navigation.extend([
            (ShellTab::Explorer, label("explorer")),
            (ShellTab::Node, label("node")),
            (ShellTab::Members, label("members")),
            (ShellTab::Governance, label("governance")),
        ]);
        navigation.push((ShellTab::Settings, label("settings")));
        let (sidebar, popover) = {
            let theme = gpui_kit::component::Theme::global(cx);
            (theme.sidebar, theme.popover)
        };
        let state = &self.model.read(cx).state;
        let palette = design::palette(state.is_dark());
        let accent = hsla_of(palette.accent);
        let ink_fg = hsla_of(palette.sidebar_foreground);
        let ink_muted = hsla_of(palette.sidebar_muted);
        let ink_raised = hsla_of(palette.sidebar_raised);
        let ink_border = hsla_of(palette.sidebar_border);
        let faint = hsla_of(palette.faint);
        let live = state.connected;
        let bell_unread = state.bell_unread;
        // The voice dock's facts, read here so the rail below owns no borrow.
        let voice = state.huddle_joined.then(|| VoiceDock {
            room: state.huddle_channel_name.clone(),
            elapsed: crate::backend::mmss(state.huddle_now - state.huddle_joined_at),
            others: state.huddle_roster.len().saturating_sub(1),
            muted: state.call_muted,
        });
        let success = hsla_of(palette.success);
        let danger = hsla_of(palette.danger);
        // Who is signed in, as the rail's foot shows it.
        let (who, whose_key) = crate::backend::rail_identity(
            state.account_exists,
            &state.account_name,
            &state.account_number,
            &state.signer_key,
        );
        // The sidebar is the ink rail: it carries the network — its name,
        // whether it is live, the way to another one — the search and the
        // bell, the navigation, and at its foot the signed-in account. There
        // is no header: the screen is the module's own.
        let network = self
            .action("switch-network", "", Message::SwitchNetwork, false)
            .ghost()
            .w_full()
            .h_auto()
            .px_2()
            .py_1p5()
            .text_color(ink_fg)
            .accessibility_label("Switch network")
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .min_w_0()
                    .child(
                        div()
                            .size(px(6.))
                            .flex_shrink_0()
                            .rounded_full()
                            .bg(if live { accent } else { faint }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .items_start()
                            .child(
                                div()
                                    .w_full()
                                    .truncate()
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(ink_fg)
                                    .child(state.network_name.clone()),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .truncate()
                                    .text_size(px(11.))
                                    .font_weight(FontWeight::NORMAL)
                                    .text_color(ink_muted)
                                    .child(state.status.clone()),
                            ),
                    )
                    .child(
                        gpui_kit::component::Icon::new(
                            gpui_kit::component::IconName::ChevronsUpDown,
                        )
                        .xsmall()
                        .text_color(ink_muted),
                    ),
            );
        let reserve_traffic_lights = cfg!(target_os = "macos") && !window.is_fullscreen();
        let mut modifiers = Modifiers::default();
        if cfg!(target_os = "macos") {
            modifiers.platform = true;
        } else {
            modifiers.control = true;
        }
        let shortcut = if cfg!(target_os = "macos") {
            "⌘K"
        } else {
            "Ctrl K"
        };
        let ink = RailInk {
            fg: ink_fg,
            muted: ink_muted,
            raised: ink_raised,
        };
        let search = rail_row(
            "rail-search",
            gpui_kit::component::Icon::new(gpui_kit::component::IconName::Search),
            "Search",
            ink,
            false,
            live,
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(11.))
                .text_color(ink_muted)
                .child(shortcut),
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.model.update(cx, |model, cx| {
                model.dispatch(
                    Message::GlobalKeyPressed(KeyPress {
                        key: "k".into(),
                        modifiers,
                    }),
                    cx,
                )
            });
        }));
        let bell_label = match bell_unread {
            0 => "Notifications".to_owned(),
            unread => format!("Notifications ({unread})"),
        };
        let bell = rail_row(
            "rail-bell",
            gpui_kit::component::Icon::new(gpui_kit::component::IconName::Bell),
            bell_label,
            ink,
            false,
            live,
        )
        .when(bell_unread > 0, |row| {
            row.child(div().flex_shrink_0().size(px(6.)).rounded_full().bg(accent))
        })
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.model
                .update(cx, |model, cx| model.dispatch(Message::ToggleBell, cx));
        }));
        let mut tabs = div()
            .id("workspace-rail")
            .flex()
            .flex_col()
            .w(px(200.))
            .h_full()
            .flex_shrink_0()
            .px_2()
            .py_2()
            .bg(sidebar)
            .border_r_1()
            .border_color(ink_border)
            // Transparent macOS chrome overlays the rail. Keep its buttons
            // above the network switcher and leave the strip draggable.
            .when(reserve_traffic_lights, |rail| {
                rail.child(
                    div()
                        .id("workspace-titlebar")
                        .h(px(32.))
                        .w_full()
                        .flex_shrink_0()
                        .on_mouse_down(MouseButton::Left, |_, window, _| {
                            window.start_window_move()
                        }),
                )
            })
            .child(network)
            .child(div().h_2().flex_shrink_0())
            .child(search)
            .child(bell);
        for (tab, label) in navigation {
            let section = match tab {
                ShellTab::Chat => Some("Workspace"),
                ShellTab::Explorer => Some("Network"),
                _ => None,
            };
            if tab == ShellTab::Settings {
                tabs = tabs.child(div().flex_1().min_h_4());
            }
            if let Some(section) = section {
                tabs = tabs.child(
                    div()
                        .px_2()
                        .pt_3()
                        .pb_1()
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(ink_muted)
                        .child(section),
                );
            }
            let selected = tab == selected_tab;
            // a registered tab's element id is its registry id, not its
            // manifest name: two views may share a name, never an id
            let id: gpui_kit::SharedString = match tab {
                ShellTab::Registered(module) => format!("view:{module}").into(),
                _ => label.clone().into(),
            };
            let row = rail_row(id, nav_icon(tab), label, ink, selected, true).on_click(
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.model.update(cx, |model, cx| {
                        model.dispatch(Message::SelectShellTab(tab), cx)
                    });
                }),
            );
            #[cfg(test)]
            let row = {
                use gpui_kit::test::TestSupportExt as _;
                row.test_support()
            };
            tabs = tabs.child(row);
        }
        // The foot of the rail: who is signed in. Pressing it opens the
        // account screen — a sign-in when there is no account yet. The update
        // decides which (`on_open_account`); the row itself does not know.
        let account = div()
            .id("rail-account")
            .flex()
            .items_center()
            .gap_2()
            .h(px(40.))
            .px_2()
            .mt_1()
            .rounded(px(design::radius::CONTROL as f32))
            .cursor_pointer()
            .hover(move |style| style.bg(ink_raised))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.model
                    .update(cx, |model, cx| model.dispatch(Message::OpenAccount, cx));
            }))
            .child(
                div()
                    .flex_shrink_0()
                    .size(px(24.))
                    .rounded_full()
                    .bg(ink_raised)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(11.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(ink_fg)
                    .child(crate::backend::initials_of(&who)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .truncate()
                            .text_size(px(13.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(ink_fg)
                            .child(who),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(ink_muted)
                            .child(whose_key),
                    ),
            );
        // The voice dock: while the reader is in a huddle the rail's foot
        // says so — the room, how long, who else — with mute, the huddle
        // window and leave at hand, the way a voice client keeps its call
        // under the navigation.
        if let Some(voice) = voice {
            tabs = tabs.child(self.voice_dock(voice, ink, success, danger));
        }
        tabs = tabs.child(account);
        let state = &self.model.read(cx).state;
        let error = state.error.clone();
        let toast = state.toast.clone();
        let update_strip = state.update_strip();
        let needs_account =
            state.connected && !state.account_exists && !state.account_banner_dismissed;
        let mut content = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .h_full()
            .overflow_hidden()
            .bg(colors.background);
        if needs_account {
            // A quiet one-line notice: the screen behind it stays the loudest thing.
            content = content.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .h(px(32.))
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(colors.border)
                    .bg(hsla_of(palette.surface))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.))
                            .text_color(colors.muted_foreground)
                            .child("Sign in to use your account on this network."),
                    )
                    .child(
                        self.action(
                            "account-open",
                            "Sign in",
                            Message::OpenAccountWelcome,
                            false,
                        )
                        .outline()
                        .h_6()
                        .text_size(px(12.)),
                    )
                    .child(
                        self.action(
                            "account-dismiss",
                            "Dismiss",
                            Message::DismissAccountBanner,
                            false,
                        )
                        .ghost()
                        .h_6()
                        .text_size(px(12.))
                        .text_color(colors.muted_foreground),
                    ),
            );
        }
        if let Some(strip) = update_strip {
            content = content.child(self.update_strip(strip, &colors, palette));
        }
        if !error.is_empty() {
            content = content.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_2()
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(colors.border)
                    .bg(hsla_of(palette.danger_soft))
                    .text_color(hsla_of(palette.danger))
                    .child(
                        gpui_kit::component::Icon::new(
                            gpui_kit::component::IconName::TriangleAlert,
                        )
                        .xsmall(),
                    )
                    .child(div().flex_1().text_size(px(12.5)).child(error))
                    .child(
                        self.action("error-dismiss", "Dismiss", Message::DismissError, false)
                            .ghost()
                            .h_7(),
                    ),
            );
        }
        content = content.child(
            div()
                .id("workspace-content")
                .relative()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .w_full()
                .overflow_hidden()
                .child(view)
                .when(!toast.is_empty(), |element| {
                    element.child(
                        div()
                            .absolute()
                            .bottom_4()
                            .right_4()
                            .max_w(px(420.))
                            .flex()
                            .items_center()
                            .gap_3()
                            .px_4()
                            .py_2p5()
                            .rounded(px(design::radius::CARD as f32))
                            .border_1()
                            .border_color(colors.border)
                            .bg(popover)
                            .shadow_md()
                            .child(div().flex_1().text_size(px(12.5)).child(toast))
                            .child(
                                self.action(
                                    "toast-dismiss",
                                    "Dismiss",
                                    Message::DismissToast,
                                    false,
                                )
                                .ghost()
                                .h_7(),
                            ),
                    )
                }),
        );
        let mut root = div()
            .relative()
            .flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .child(tabs)
            .child(content);
        if let Some(overlay) = self.overlay(window, cx) {
            root = root.child(overlay);
        }
        root.into_any_element()
    }

    /// The update strip across the top of the console: the same quiet
    /// one-line band as the account notice. A staged release offers the
    /// restart; a rollback says so until dismissed.
    fn update_strip(
        &self,
        strip: crate::backend::update::UpdateStrip,
        colors: &gpui_kit::component::ColorTokens,
        palette: &design::Palette,
    ) -> gpui_kit::AnyElement {
        use gpui_kit::*;
        let (words, tone, action) = match strip {
            crate::backend::update::UpdateStrip::Ready { display } => (
                format!("Ducktape {display} is ready"),
                hsla_of(palette.accent_soft),
                self.action(
                    "update-restart",
                    "Restart to update",
                    Message::UpdateAction(crate::UpdateAction::RestartToUpdate),
                    false,
                ),
            ),
            crate::backend::update::UpdateStrip::RolledBack { failed, reason } => (
                format!("Update {failed} was rolled back ({reason})"),
                hsla_of(palette.warning_soft),
                self.action(
                    "update-rollback-dismiss",
                    "Dismiss",
                    Message::UpdateAction(crate::UpdateAction::DismissRollbackNotice),
                    false,
                ),
            ),
        };
        div()
            .id("update-strip")
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .h(px(32.))
            .flex_shrink_0()
            .border_b_1()
            .border_color(colors.border)
            .bg(tone)
            .child(
                div()
                    .flex_1()
                    .text_size(px(12.))
                    .text_color(colors.foreground)
                    .child(words),
            )
            .child(action.outline().h_6().text_size(px(12.)))
            .into_any_element()
    }

    fn overlay(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui_kit::AnyElement> {
        use gpui_kit::*;
        let topmost = {
            let state = &self.model.read(cx).state;
            crate::backend::topmost_overlay(state.palette_open, state.bell_open)
        };
        // The seat is taken before the card is drawn — it needs the window,
        // and everything below borrows the model. A bell that is not the
        // topmost overlay gives its seat back.
        let inbox = match topmost.as_str() {
            "bell" => Some(self.seat_inbox(cx)),
            _ => {
                self.unseat_inbox(cx);
                None
            }
        };
        let state = &self.model.read(cx).state;
        use gpui_kit::component::button::ButtonVariants as _;
        use gpui_kit::component::ActiveTheme as _;
        let colors = cx.theme().color_tokens();
        let muted = colors.muted_foreground;
        // A modal is one card: a title row with its close, then its body.
        let heading = |this: &Self, title: &'static str, close: Message| {
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_4()
                .pt_3()
                .pb_2()
                .child(
                    div()
                        .flex_1()
                        .text_size(px(15.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(
                    this.action("modal-close", "", close, false)
                        .ghost()
                        .icon(gpui_kit::component::IconName::Close)
                        .accessibility_label("Close")
                        .h_7()
                        .w_7()
                        .px_0(),
                )
        };
        // A result or a notification is one full-width row: a name, then
        // what it says, in the muted tone.
        let row = |this: &Self, key: String, name: String, detail: String, message, disabled| {
            this.action(key, "", message, disabled)
                .accessibility_label(format!("{name} {detail}"))
                .ghost()
                .w_full()
                .h_auto()
                .px_2()
                .py_1p5()
                .justify_start()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap_0p5()
                        .min_w_0()
                        .w_full()
                        .child(
                            div()
                                .w_full()
                                .truncate()
                                .font_weight(FontWeight::MEDIUM)
                                .child(name),
                        )
                        .child(
                            div()
                                .w_full()
                                .truncate()
                                .text_size(px(12.5))
                                .font_weight(FontWeight::NORMAL)
                                .text_color(muted)
                                .child(detail),
                        ),
                )
        };
        let mut body = div().flex().flex_col().gap_1().px_3().pb_3();
        let (title, dismiss) = match topmost.as_str() {
            "palette" => {
                let chats = state.palette_chat_hits.clone();
                let pages = state.palette_page_hits.clone();
                let phase = state.palette_search_phase;
                let query = state.palette_draft.clone();
                let empty = chats.is_empty() && pages.is_empty();
                body = body.child(div().px_1().pb_1().child(self.input(
                    "palette-input",
                    "Search messages and pages",
                    false,
                    window,
                    cx,
                )));
                let note = match phase {
                    crate::SearchPhase::Searching => Some("Searching…"),
                    crate::SearchPhase::Done if empty => Some("No messages or pages matched."),
                    crate::SearchPhase::Idle if !query.trim().is_empty() => Some("Search failed."),
                    _ => None,
                };
                if let Some(note) = note {
                    body = body.child(
                        div()
                            .px_2()
                            .py_2()
                            .text_size(px(12.5))
                            .text_color(muted)
                            .child(note),
                    );
                }
                for hit in chats {
                    body = body.child(row(
                        self,
                        format!("search-chat/{}/{}", hit.channel_id, hit.seq),
                        hit.author,
                        hit.text,
                        Message::OpenChatSearchHit(hit.channel_id, hit.seq),
                        false,
                    ));
                }
                for hit in pages {
                    body = body.child(row(
                        self,
                        format!("search-page/{}/{}", hit.page_id, hit.block_id),
                        hit.page_title,
                        hit.text,
                        Message::OpenPageSearchHit(hit.page_id, hit.block_id),
                        false,
                    ));
                }
                ("Search this workspace", Message::ClosePalette)
            }
            // The body is the guest's frame and nothing else: no rows, no
            // empty state, no mark-read button and no retry drawn here. All
            // of those are inbox content, and the view words them. The
            // height is the chrome's cap on the guest, which draws no
            // popover of its own.
            "bell" => {
                body = body.child(
                    div()
                        .h(px(420.))
                        .w_full()
                        .child(inbox.expect("the bell overlay seats the inbox view")),
                );
                ("Notifications", Message::CloseBell)
            }
            _ => return None,
        };
        let model = self.model.clone();
        let panel = div()
            .id("shell-modal")
            .flex()
            .flex_col()
            .w(px(560.0))
            .max_h(relative(0.8))
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .rounded(px(design::radius::CARD as f32 + 2.))
            .border_1()
            .border_color(colors.border)
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(heading(self, title, dismiss.clone()))
            .child(
                div()
                    .id("shell-modal-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            );
        Some(
            div()
                .id("shell-scrim")
                .absolute()
                .inset_0()
                .flex()
                .items_start()
                .justify_center()
                .pt(px(96.))
                .bg(rgba(0x00000055))
                .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                    model.update(cx, |model, cx| model.dispatch(dismiss.clone(), cx))
                })
                .child(panel)
                .into_any_element(),
        )
    }
}

impl Render for DesktopWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::InteractiveElement as _;
        use gpui_kit::component::ActiveTheme as _;
        let content = match self.kind {
            WindowKind::Console => self.console(window, cx),
            WindowKind::Onboarding => self.onboarding(window, cx),
            WindowKind::Huddle => self.huddle(window, cx),
        };
        let pending_focus = self.model.read(cx).pending_focus.clone();
        if let Some(key) = pending_focus {
            let local_key = key.rsplit('/').next().unwrap_or(&key);
            if let Some(input) = self.inputs.get(local_key) {
                input.state.update(cx, |state, cx| state.focus(window, cx));
                self.model.update(cx, |model, _| model.pending_focus = None);
            }
        }
        // Every text style in every app window descends from this one, and
        // `font_family` — which is all the tree below ever overrides — leaves
        // the chain in place. Set it here and a run in any pane falls back by
        // lookup.
        let mut root = gpui_kit::div();
        root.text_style().font_fallbacks = Some(fallback_chain());
        root.id("desktop-root")
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &gpui_kit::KeyDownEvent, _, cx| {
                let key = KeyPress {
                    key: event.keystroke.key.clone(),
                    modifiers: event.keystroke.modifiers,
                };
                let copy = this.model.read(cx).state.shell_tab == ShellTab::Chat
                    && crate::backend::is_copy_chord(key.key.clone(), key.modifiers);
                if !copy {
                    return;
                }
                this.model.update(cx, |model, cx| {
                    model.dispatch(Message::CopyChordPressed(key), cx)
                });
            }))
            .on_modifiers_changed(cx.listener(
                |this, event: &gpui_kit::ModifiersChangedEvent, _, cx| {
                    this.model.update(cx, |model, cx| {
                        model.dispatch(Message::ModifierStateChanged(event.modifiers), cx)
                    });
                },
            ))
            .child(content)
    }
}

#[cfg(test)]
pub(crate) fn test_window(
    state: Ducktape,
    kind: WindowKind,
    window: &mut Window,
    cx: &mut gpui_kit::App,
) -> Entity<DesktopWindow> {
    let model = cx.new(|_| Desktop {
        state,
        tray: crate::tray::Tray::without_status_item(),
        windows: BTreeMap::new(),
        views: BTreeMap::new(),
        streams: HashMap::new(),
        pending_focus: None,
        pending_urls: Vec::new(),
    });
    cx.new(|cx| {
        cx.on_release(DesktopWindow::released).detach();
        let observer = cx.observe(&model, |_, _, cx| cx.notify());
        let activation = cx.observe_window_activation(window, |_, _, _| {});
        let keystrokes = DesktopWindow::intercept_global_keys(window, cx);
        DesktopWindow {
            model,
            kind,
            module: None,
            module_route: None,
            route: None,
            overlay_module: None,
            overlay_route: None,
            inputs: HashMap::new(),
            input_step: None,
            qr: None,
            focus: cx.focus_handle(),
            _activation: activation,
            _observer: observer,
            _keystrokes: keystrokes,
        }
    })
}

#[cfg(test)]
mod call_control_tests {
    use super::*;

    /// A CONTROL THE VIEW PRESSES AND THE HOST NEVER DECLARED IS SWALLOWED
    /// SILENTLY: an intent absent from `intents_of("call")` never becomes a
    /// `ModuleViewEvent` at all, so the button simply does nothing, with no
    /// error anywhere. `huddle_route`'s arms and that list are one seam kept in
    /// two files, so the arms are READ OUT OF THIS SOURCE rather than copied
    /// into a third list that could drift from both.
    #[test]
    fn every_call_control_the_host_routes_is_one_the_view_may_send() {
        let source = include_str!("shell.rs");
        let body = source
            .split_once("fn huddle_route(")
            .expect("huddle_route lives in this file")
            .1
            .split_once("\n}\n")
            .expect("and its body ends")
            .0;
        let routed: Vec<&str> = body
            .lines()
            .filter_map(|line| line.trim().strip_prefix('"'))
            .filter_map(|line| line.split_once('"'))
            .map(|(kind, _)| kind)
            .collect();
        assert!(
            routed.contains(&"share") && routed.contains(&"mute"),
            "the arms did not parse out of the source: {routed:?}"
        );
        let declared = crate::module_view::intents_of("call");
        for kind in &routed {
            assert!(
                declared.contains(kind),
                "the host routes `{kind}` and the view is not allowed to send it"
            );
        }
        for kind in declared {
            assert!(
                routed.contains(kind),
                "the view may send `{kind}` and the host routes it nowhere"
            );
        }
        // And the picker's row index survives the trip as an index, not as the
        // "unrecognized control" a bad parse would fall through to.
        let picked = huddle_route(crate::module_view::view_event(
            "share".to_owned(),
            "2".to_owned(),
        ));
        assert!(matches!(picked, Message::PickShareTarget(2)));
        // A row index that is not one closes the picker rather than sharing
        // something nobody asked for.
        let nonsense = huddle_route(crate::module_view::view_event(
            "share".to_owned(),
            "not a row".to_owned(),
        ));
        assert!(matches!(nonsense, Message::CloseSharePicker));
    }
}

#[cfg(test)]
mod close_tests {
    use super::*;

    #[gpui_kit::test]
    fn appearance_changes_keep_native_fonts_radii_and_the_palette(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        cx.update(|cx| {
            use gpui_kit::component::{Theme, ThemeMode};
            gpui_kit::init(cx);
            for (mode, palette) in [
                (ThemeMode::Light, &design::LIGHT),
                (ThemeMode::Dark, &design::DARK),
            ] {
                Theme::change(mode, None, cx);
                configure_native_theme(cx);
                let theme = Theme::global(cx);
                assert_eq!(theme.font_family.as_ref(), design::fonts::FAMILY_UI);
                assert_eq!(theme.mono_font_family.as_ref(), design::fonts::FAMILY_MONO);
                assert_eq!(theme.radius, gpui_kit::px(design::radius::CONTROL as f32));
                assert_eq!(theme.radius_lg, gpui_kit::px(design::radius::CARD as f32));
                assert_eq!(theme.highlight_theme.appearance, mode);
                assert_eq!(
                    theme.highlight_theme.style.editor_background,
                    Some(theme.background)
                );
                let background: gpui_kit::Rgba = theme.background.into();
                let [r, g, b, _] = palette.background;
                let close = |a: f32, b: f32| (a - b).abs() < 1.5 / 255.;
                assert!(
                    close(background.r, r) && close(background.g, g) && close(background.b, b),
                    "{mode:?} background follows the palette: {background:?}"
                );
            }
        });
    }

    #[test]
    fn shell_commands_precede_focused_input_actions_in_only_their_window() {
        use gpui_kit::test::TestWindowExt as _;
        let _turn = crate::module_view::tests::blocking_connection_turn();
        let mut native = crate::frame_probe::headless_context();
        let cx = &mut native;
        let mut views = Vec::new();
        let mut windows = Vec::new();
        for _ in 0..2 {
            let handle = cx
                .open_window(
                    gpui_kit::size(gpui_kit::px(600.), gpui_kit::px(700.)),
                    |window, cx| {
                        let mut state = Ducktape::initial_state();
                        state.hub_step = crate::HubStep::Networks;
                        state.connected = true;
                        let view = test_window(state, WindowKind::Onboarding, window, cx);
                        views.push(view.clone());
                        cx.new(|cx| gpui_kit::component::Root::new(view, window, cx))
                    },
                )
                .unwrap();
            windows.push(gpui_kit::AnyWindowHandle::from(handle));
        }
        let view = &views[0];
        windows[0]
            .update(cx, |_, window, cx| {
                window.render_frame(cx);
                let input = view.read(cx).inputs["remote"].state.clone();
                input.update(cx, |input, cx| input.focus(window, cx));
                window.render_frame(cx);
                window.input("keep these words", cx);
                let command = if cfg!(target_os = "macos") {
                    "cmd"
                } else {
                    "ctrl"
                };
                window.press(&format!("{command}-k"), cx);
                assert!(view.read(cx).model.read(cx).state.palette_open);
                view.read(cx)
                    .model
                    .clone()
                    .update(cx, |model, _| model.state.bell_open = true);
                window.press("escape", cx);
                assert!(!view.read(cx).model.read(cx).state.palette_open);
                assert!(
                    view.read(cx).model.read(cx).state.bell_open,
                    "only the top shell overlay closes"
                );
                window.press("escape", cx);
                assert!(!view.read(cx).model.read(cx).state.bell_open);
                window.press(&format!("{command}-w"), cx);
                assert_eq!(
                    input.read(cx).value(),
                    "keep these words",
                    "Close is not native delete-word"
                );
                window.press(&format!("{command}-a"), cx);
                assert_eq!(
                    input.read(cx).selected_range(),
                    0.."keep these words".len(),
                    "ordinary native shortcuts remain available"
                );
                let before = view
                    .read(cx)
                    .model
                    .read(cx)
                    .state
                    .account_qr_auth_generation;
                let other_before = views[1]
                    .read(cx)
                    .model
                    .read(cx)
                    .state
                    .account_qr_auth_generation;
                window.press(&format!("{command}-q"), cx);
                assert_eq!(
                    view.read(cx)
                        .model
                        .read(cx)
                        .state
                        .account_qr_auth_generation,
                    before + 1
                );
                assert_eq!(
                    views[1]
                        .read(cx)
                        .model
                        .read(cx)
                        .state
                        .account_qr_auth_generation,
                    other_before,
                    "global interceptor must not route another native window's command"
                );
            })
            .unwrap();
    }

    #[gpui_kit::test]
    fn closing_a_focused_native_input_releases_its_handler(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::Focusable as _;
        use gpui_kit::test::TestWindowExt as _;
        cx.update(gpui_kit::init);
        let mut presenter = None;
        let handle = cx.open_window(
            gpui_kit::size(gpui_kit::px(600.), gpui_kit::px(700.)),
            |window, cx| {
                let mut state = Ducktape::initial_state();
                state.hub_step = crate::HubStep::Networks;
                let view = test_window(state, WindowKind::Onboarding, window, cx);
                presenter = Some(view.downgrade());
                gpui_kit::component::Root::new(view, window, cx)
            },
        );
        let handle: gpui_kit::AnyWindowHandle = handle.into();
        handle
            .update(cx, |_, window, cx| {
                window.render_frame(cx);
            })
            .unwrap();
        let presenter = presenter.unwrap();
        let input = presenter
            .update(cx, |view, _| view.inputs["remote"].state.downgrade())
            .unwrap();
        handle
            .update(cx, |_, window, cx| {
                input
                    .upgrade()
                    .unwrap()
                    .update(cx, |input, cx| input.focus(window, cx));
                window.render_frame(cx);
                window.input("typing-before-close", cx);
                assert!(
                    input
                        .upgrade()
                        .unwrap()
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window)
                );
                release_window_input(window, cx);
                assert!(window.focused(cx).is_none());
                window.remove_window();
            })
            .unwrap();
        cx.run_until_parked();
        assert!(presenter.upgrade().is_none());
        assert!(
            input.upgrade().is_none(),
            "native input handler cannot retain the closed window's input"
        );
    }

    #[test]
    fn native_error_dismiss_reaches_its_domain_handler() {
        use gpui_kit::test::TestWindowExt as _;
        use gpui_kit::{px, size};
        assert!(tokio::runtime::Handle::try_current().is_err());
        let _turn = crate::module_view::tests::blocking_connection_turn();
        let mut cx = crate::frame_probe::headless_context();
        let mut state = Ducktape::initial_state();
        state.connected = true;
        state.connected_rpc = "http://127.0.0.1:0".into();
        state.shell_tab = ShellTab::Files;
        state.error = "Could not complete the request".into();
        let mut view = None;
        let handle = cx
            .open_window(size(px(1120.), px(720.)), |window, cx| {
                let presenter = test_window(state, WindowKind::Console, window, cx);
                view = Some(presenter.clone());
                cx.new(|cx| gpui_kit::component::Root::new(presenter, window, cx))
            })
            .unwrap();
        let view = view.unwrap();
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
        cx.update_window(handle.into(), |_, window, cx| {
            window.click("error-dismiss", cx)
        })
        .unwrap();
        view.read_with(&cx, |view, cx| {
            assert!(view.test_state(cx).error.is_empty())
        });
    }

    #[test]
    fn huddle_forwards_tile_changes_without_selecting_a_stage() {
        let (mut state, _) = Ducktape::boot();
        state.call_video_live = true;
        for key in ["first-image", "second-image"] {
            state.huddle_tiles = vec![key.into()];
            let props: serde_json::Value = serde_json::from_slice(&huddle_props(&state)).unwrap();
            assert_eq!(props["panel"]["tiles"], serde_json::json!([key]));
            assert_eq!(props["panel"]["stage"], "");
        }
    }

    fn frozen_route(event: crate::module_view::ModuleViewEvent) -> Message {
        Message::ExternalUrlFailed(crate::backend::AppError {
            message: event.detail,
            committed: false,
        })
    }

    #[gpui_kit::test]
    async fn final_observations_tick_and_route_after_the_presenter_is_released(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use crate::module_view::tests::{
            close_observer_fixture, close_observer_reading, queue_close_intent,
        };
        let _turn = crate::module_view::tests::blocking_connection_turn();
        cx.update(gpui_kit::init);
        let mut presenter = None;
        let handle = cx.open_window(
            gpui_kit::size(gpui_kit::px(320.), gpui_kit::px(460.)),
            |window, cx| {
                let (state, _) = Ducktape::boot();
                // Onboarding has no deployed module of its own to replace this fixture.
                let view = test_window(state, WindowKind::Onboarding, window, cx);
                presenter = Some(view.clone());
                gpui_kit::component::Root::new(view, window, cx)
            },
        );
        let presenter = presenter.unwrap();
        let model = presenter.update(cx, |view, cx| {
            view.module = Some(("governance", cx.new(|_| close_observer_fixture())));
            view.module_route = Some(frozen_route);
            view.model.clone()
        });
        let baseline = close_observer_reading().0;
        queue_close_intent("requested");
        // The command executor owns this model borrow while requesting close.
        // Routing inline here would re-enter it and panic.
        model.update(cx, |_, cx| {
            presenter.update(cx, |view, cx| {
                view.observe_module_window(view_wire::events::Window::CloseRequested, cx);
            });
        });
        assert_eq!(close_observer_reading(), (baseline + 1, 0));
        cx.condition(&model, |model, _| model.state.error == "requested")
            .await;

        // A real guest frame replaces interest; explicitly rearm this host-only
        // fixture before exercising the actual native presenter's release hook.
        queue_close_intent("closed");
        model.update(cx, |model, _| model.state.shell_tab = ShellTab::Files);
        let weak = presenter.downgrade();
        drop(presenter);
        handle
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.condition(&model, |model, _| model.state.error == "closed")
            .await;
        assert!(
            weak.upgrade().is_none(),
            "the route does not retain the presenter"
        );
        assert_eq!(close_observer_reading(), (baseline + 2, 0));
    }
}

/// The product palette as the kit's light and dark themes. Registered once;
/// every later mode change re-applies the matching one, so the kit's own
/// controls paint with the colors the views paint with.
fn configure_native_theme(cx: &mut gpui_kit::App) {
    use gpui_kit::component::{Theme, ThemeRegistry};
    let registry = ThemeRegistry::global_mut(cx);
    let registered = registry.themes().contains_key(design::LIGHT_THEME);
    if !registered {
        registry
            .load_themes_from_str(&design::kit_theme_json())
            .expect("the product theme parses");
    }
    let light = with_syntax_colors(
        &registry.themes()[design::LIGHT_THEME],
        registry.default_light_theme(),
        &design::LIGHT,
    );
    let dark = with_syntax_colors(
        &registry.themes()[design::DARK_THEME],
        registry.default_dark_theme(),
        &design::DARK,
    );
    let theme = Theme::global_mut(cx);
    theme.light_theme = light;
    theme.dark_theme = dark;
    let mode = theme.mode;
    Theme::change(mode, None, cx);
}

/// A palette color as the kit paints it.
/// The product theme carrying the kit's default syntax colors for its mode.
/// The product JSON names no highlight block, and `Theme::apply_config`
/// keeps whatever highlight theme it last saw when a config has none — a
/// light syntax palette and a light editor on a dark window. The editor
/// paints on the window background, so a code reader sits flush in its pane.
fn with_syntax_colors(
    product: &std::rc::Rc<gpui_kit::component::ThemeConfig>,
    defaults: &std::rc::Rc<gpui_kit::component::ThemeConfig>,
    palette: &design::Palette,
) -> std::rc::Rc<gpui_kit::component::ThemeConfig> {
    let mut theme = (**product).clone();
    let mut style = defaults.highlight.clone().unwrap_or_default();
    style.editor_background = Some(hsla_of(palette.background));
    theme.highlight = Some(style);
    std::rc::Rc::new(theme)
}

fn hsla_of(color: design::Color) -> gpui_kit::Hsla {
    let [r, g, b, a] = color;
    gpui_kit::Rgba { r, g, b, a }.into()
}

/// Lucide glyphs the kit's default bundle does not carry.
/// The registry id of the dashboard view: the one registered view that
/// leads the rail instead of following the built-in tabs.
const HOME_VIEW: &str = "home";

const MESSAGE_SQUARE: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/></svg>"#;
const GIT_BRANCH: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="6" x2="6" y1="3" y2="15"/><circle cx="18" cy="6" r="3"/><circle cx="6" cy="18" r="3"/><path d="M18 9a9 9 0 0 1-9 9"/></svg>"#;
const USERS: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/></svg>"#;
const VOTE: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m9 12 2 2 4-4"/><path d="M5 7c0-1.1.9-2 2-2h10a2 2 0 0 1 2 2v12H5V7Z"/><path d="M22 19H2"/></svg>"#;

/// The glyph beside a tab's name in the sidebar.
/// The ink rail's row colours.
#[derive(Clone, Copy)]
struct RailInk {
    fg: gpui_kit::Hsla,
    muted: gpui_kit::Hsla,
    raised: gpui_kit::Hsla,
}

/// A rail row: an icon and a left-aligned label on a 28px line, the ink
/// wash on hover and when chosen. Its own element, not a kit button: the
/// kit centres a button's content and a rail reads left-aligned. The
/// caller adds the click.
/// What the rail's voice dock says while the reader is in a huddle.
struct VoiceDock {
    room: String,
    elapsed: String,
    others: usize,
    muted: bool,
}

impl DesktopWindow {
    fn voice_dock(
        &self,
        voice: VoiceDock,
        ink: RailInk,
        success: gpui_kit::Hsla,
        danger: gpui_kit::Hsla,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        use gpui_kit::component::Sizable as _;
        use gpui_kit::component::button::ButtonVariants as _;
        use gpui_kit::*;
        let RailInk { fg, muted, raised } = ink;
        let with = match voice.others {
            0 => "alone".to_owned(),
            1 => "with 1 other".to_owned(),
            n => format!("with {n} others"),
        };
        let mute = if voice.muted { "Unmute" } else { "Mute" };
        div()
            .id("rail-voice")
            .flex()
            .flex_col()
            .gap(px(6.))
            .p(px(8.))
            .mt_1()
            .rounded(px(design::radius::CONTROL as f32))
            .bg(raised)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(div().size(px(8.)).rounded_full().bg(success))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(success)
                            .child("Voice connected"),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(muted)
                            .child(voice.elapsed),
                    ),
            )
            .child(
                div()
                    .truncate()
                    .text_size(px(12.))
                    .text_color(fg)
                    .child(format!("#{} · {with}", voice.room)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(
                        self.action("rail-voice-mute", mute, Message::ToggleCallMute, false)
                            .xsmall()
                            .outline(),
                    )
                    .child(
                        self.action("rail-voice-open", "Open", Message::ShowHuddle, false)
                            .xsmall()
                            .ghost()
                            .text_color(fg),
                    )
                    .child(div().flex_1())
                    .child(
                        self.action("rail-voice-leave", "Leave", Message::LeaveHuddleHere, false)
                            .xsmall()
                            .ghost()
                            .text_color(danger),
                    ),
            )
    }
}

fn rail_row(
    id: impl Into<gpui_kit::ElementId>,
    icon: gpui_kit::component::Icon,
    label: impl Into<gpui_kit::SharedString>,
    ink: RailInk,
    selected: bool,
    enabled: bool,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    use gpui_kit::component::Sizable as _;
    use gpui_kit::*;
    let RailInk { fg, muted, raised } = ink;
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .h(px(28.))
        .px_2()
        .mb_0p5()
        .rounded(px(design::radius::CONTROL as f32))
        .cursor_pointer()
        .text_size(px(13.))
        .font_weight(if selected {
            FontWeight::MEDIUM
        } else {
            FontWeight::NORMAL
        })
        .text_color(if selected { fg } else { muted })
        .when(selected, |row| row.bg(raised))
        .when(!enabled, |row| row.opacity(0.5))
        .hover(move |style| style.bg(raised).text_color(fg))
        .child(icon.small())
        .child(div().flex_1().min_w_0().truncate().child(label.into()))
}

fn nav_icon(tab: ShellTab) -> gpui_kit::component::Icon {
    use gpui_kit::component::{Icon, IconName};
    match tab {
        // the view's own `icons/tab.svg`, once it is seated; a plain mark
        // until then and for a view that ships none
        ShellTab::Registered(module) => match crate::module_view::registered_view_icon(module) {
            Some(bytes) => Icon::empty().data(&bytes),
            None => Icon::new(IconName::LayoutDashboard),
        },
        ShellTab::Chat => Icon::empty().data(MESSAGE_SQUARE),
        ShellTab::Pages => Icon::new(IconName::BookOpen),
        ShellTab::Forge => Icon::empty().data(GIT_BRANCH),
        ShellTab::Agents => Icon::new(IconName::Bot),
        ShellTab::Files => Icon::new(IconName::Folder),
        ShellTab::Explorer => Icon::new(IconName::Globe),
        ShellTab::Node => Icon::new(IconName::HardDrive),
        ShellTab::Members => Icon::empty().data(USERS),
        ShellTab::Governance => Icon::empty().data(VOTE),
        ShellTab::Settings => Icon::new(IconName::Settings),
    }
}

/// The faces the app registers: ONE FILE PER FACE, each the vendor's own
/// released static (`crates/views/support/design/assets/fonts/SOURCES`), never
/// a variable font. The text system keeps the requested weight only long
/// enough to match a face — `gpui-pre-wgpu`'s `cosmic_text_system.rs` then
/// shapes with the matched face's own `usWeightClass` and rasterizes from a
/// font built at `Weight::NORMAL` with no variation settings — so a family
/// holding one variable face draws every weight at 400. A static face has no
/// axes and is immune to both.
///
/// SLANT IS A FACE, NOT AN EFFECT. `find_best_match` scores style with a flat
/// penalty and then picks the best-scoring face anyway, and neither gpui nor
/// cosmic-text shears a glyph, so an italic request in a family with no
/// italic face is drawn upright. `element.italic()` and a `*run*` in a chat
/// message need the Italic and Bold Italic files to have anything to select.
///
/// The set is the four RIBBI faces, so a `MEDIUM` request lands on the
/// Regular face and a `SEMIBOLD` on the Bold one. That same substitution read
/// the other way — the matched face's weight is also the weight every
/// FALLBACK lookup runs at — is why [`FALLBACK_FAMILIES`] exists: a
/// registered face at a weight no system font declares leaves the shaper with
/// nothing to match and it walks the font database instead.
///
/// The two Hangul faces are bundled rather than named and hoped for: a chain
/// entry no box can resolve is dropped, and neither Latin family draws 한글.
/// Both are upright only — no open Hangul font has an italic — so an italic
/// run slants its Latin and leaves its Korean standing.
pub(crate) const BUNDLED_FACES: [&[u8]; 12] = [
    include_bytes!("../../crates/views/support/design/assets/fonts/Inter-Regular.ttf"),
    include_bytes!("../../crates/views/support/design/assets/fonts/Inter-Bold.ttf"),
    include_bytes!("../../crates/views/support/design/assets/fonts/Inter-Italic.ttf"),
    include_bytes!("../../crates/views/support/design/assets/fonts/Inter-BoldItalic.ttf"),
    include_bytes!("../../crates/views/support/design/assets/fonts/JetBrainsMono-Regular.ttf"),
    include_bytes!("../../crates/views/support/design/assets/fonts/JetBrainsMono-Bold.ttf"),
    include_bytes!("../../crates/views/support/design/assets/fonts/JetBrainsMono-Italic.ttf"),
    include_bytes!("../../crates/views/support/design/assets/fonts/JetBrainsMono-BoldItalic.ttf"),
    include_bytes!("../../crates/views/support/design/assets/fonts/Pretendard-Regular.otf"),
    include_bytes!("../../crates/views/support/design/assets/fonts/Pretendard-Bold.otf"),
    include_bytes!("../../crates/views/support/design/assets/fonts/D2Coding-Regular.ttf"),
    include_bytes!("../../crates/views/support/design/assets/fonts/D2Coding-Bold.ttf"),
];

/// The emoji face, kept apart from [`BUNDLED_FACES`] because the two
/// platforms differ on it (see the registration below).
pub(crate) const EMOJI_FACE: &[u8] =
    include_bytes!("../../crates/views/support/design/assets/fonts/NotoColorEmoji.ttf");

/// The families a run falls back to when the primary face has no glyph, after
/// the bundled Hangul face each chain leads with. WITHOUT this list a Korean
/// run reaches cosmic-text tagged only `Inter`: no glyph is found, and
/// `FontFallbackIter` walks the font database, SHAPING the word with one face
/// after another until one covers it. WITH it, `gpui-pre-wgpu`'s
/// `compute_run_spans` cuts the run at the first uncovered codepoint and tags
/// that span with the family that covers it, so the fallback is a lookup —
/// and the requested weight stops steering the search, which is what made a
/// face at a weight no system font declares expensive for every non-Latin run
/// at that weight.
///
/// One list for both platforms: `load_family` drops a family it cannot
/// resolve, so the macOS names vanish on Linux and the Linux names on macOS.
/// The list stays short because every non-ASCII grapheme the primary face
/// misses is tested against it in order.
///
/// NO EMOJI FAMILY BELONGS HERE. Resolving a name runs `load_family`, which
/// REMOVES from the font database any face whose charmap has no 'm' — which
/// is every color emoji face. Naming one deletes it, and emoji then have no
/// face at all: one uncached line of 🎉 went from 200us to 11ms and drew
/// from whatever the walk landed on. The platform's own fallback reaches the
/// emoji face without being named.
const FALLBACK_FAMILIES: [&str; 9] = [
    "Noto Sans CJK KR",
    "Noto Sans CJK JP",
    "Noto Sans CJK SC",
    "Apple SD Gothic Neo",
    "Hiragino Sans",
    "PingFang SC",
    "Noto Sans",
    "DejaVu Sans",
    "Apple Symbols",
];

/// The chain for body text: Pretendard, then [`FALLBACK_FAMILIES`]. Built
/// once — the root element asks for it on every frame and `FontFallbacks` is
/// an `Arc`.
pub(crate) fn fallback_chain() -> gpui_kit::FontFallbacks {
    static CHAIN: std::sync::LazyLock<gpui_kit::FontFallbacks> =
        std::sync::LazyLock::new(|| chain_led_by(design::fonts::FAMILY_UI_HANGUL));
    CHAIN.clone()
}

/// The chain for the code face: D2Coding, then [`FALLBACK_FAMILIES`]. A
/// SEPARATE chain because Pretendard is proportional — reached from a
/// terminal, a diff or a hash it would shape 한글 off the column grid the
/// mono face exists for. [`with_family`] is what puts the right one on an
/// element; `font_family` alone leaves the body chain the root installed.
pub(crate) fn mono_fallback_chain() -> gpui_kit::FontFallbacks {
    static CHAIN: std::sync::LazyLock<gpui_kit::FontFallbacks> =
        std::sync::LazyLock::new(|| chain_led_by(design::fonts::FAMILY_MONO_HANGUL));
    CHAIN.clone()
}

fn chain_led_by(hangul: &str) -> gpui_kit::FontFallbacks {
    gpui_kit::FontFallbacks::from_fonts(
        std::iter::once(hangul.to_string())
            .chain(FALLBACK_FAMILIES.iter().map(|name| name.to_string()))
            .collect(),
    )
}

/// Sets a family AND the fallback chain that belongs to it. Every family the
/// app names goes through here: a run that switches to the code face has to
/// switch its Hangul face with it, and the two are set in one place so they
/// cannot drift apart.
pub(crate) fn with_family<E: gpui_kit::Styled>(
    mut element: E,
    family: impl Into<gpui_kit::SharedString>,
) -> E {
    let family = family.into();
    let is_code_face = family == design::fonts::FAMILY_MONO;
    let style = element.text_style();
    style.font_fallbacks = Some(if is_code_face {
        mono_fallback_chain()
    } else {
        fallback_chain()
    });
    style.font_family = Some(family);
    element
}

/// [`with_family`] with the code face, in the shape a fluent chain takes it:
/// `.map(shell::mono_family)`.
pub(crate) fn mono_family<E: gpui_kit::Styled>(element: E) -> E {
    with_family(element, design::fonts::FAMILY_MONO)
}

pub(crate) fn run() {
    // The kit's component icons (search, bell, folder, …) are SVGs the app
    // loads by path; without a source they draw as nothing.
    // The full Lucide catalog: gpui-notion names its toolbar, gutter and menu
    // icons out of it, well past the default subset.
    let application = gpui_kit::application().with_assets(gpui_kit::assets::AllAssets);
    let (url_sender, mut urls) = mpsc::unbounded::<Vec<String>>();
    // Install before launching: macOS may deliver its initial URL before the
    // desktop actor exists. The channel keeps it until the actor can receive.
    application.on_open_urls(move |urls| {
        let _ = url_sender.unbounded_send(urls);
    });
    application.run(move |cx| {
        gpui_kit::init(cx);
        crate::editor::wire::init_notion(cx);
        // Ask the host about banners at launch, so the macOS prompt is a
        // launch event and its answer is in the log before the first mention.
        crate::backend::boot_desktop_notifications();
        let fonts: Vec<std::borrow::Cow<'static, [u8]>> =
            BUNDLED_FACES.iter().copied().map(std::borrow::Cow::Borrowed).collect();
        // CoreGraphics cannot load Noto's CBDT color font. Including it in
        // the batch rejects every other family too; macOS supplies emoji.
        #[cfg(not(target_os = "macos"))]
        let fonts = {
            let mut fonts = fonts;
            fonts.push(std::borrow::Cow::Borrowed(EMOJI_FACE));
            fonts
        };
        if let Err(error) = cx.text_system().add_fonts(fonts) {
            tracing::error!(target: "ducktape::app", reason = "font_registration_failed", %error, "bundled desktop fonts could not be registered");
        }
        configure_native_theme(cx);
        let mut commands = commands();
        let (state, initial) = Ducktape::boot();
        let (mut tray, mut tray_events) = crate::tray::init(cx);
        tray.sync(&state);
        let desktop = cx.new(|_| Desktop {
            state,
            tray,
            windows: BTreeMap::new(),
            views: BTreeMap::new(),
            streams: HashMap::new(),
            pending_focus: None,
            pending_urls: Vec::new(),
        });
        desktop.update(cx, |desktop, cx| desktop.sync_appearance(cx));
        let url_desktop = desktop.downgrade();
        cx.spawn(async move |cx: &mut AsyncApp| {
            while let Some(urls) = urls.next().await {
                let result = url_desktop.update(cx, |desktop, cx| {
                    desktop.pending_urls.extend(urls.into_iter().filter(|url| url.starts_with("duck://")));
                    desktop.open_pending_urls(cx);
                });
                if result.is_err() {
                    break;
                }
            }
        }).detach();
        let tray_desktop = desktop.downgrade();
        cx.spawn(async move |cx: &mut AsyncApp| {
            while let Some(row) = tray_events.next().await {
                let Some(message) = crate::tray::message(row) else {
                    continue;
                };
                if tray_desktop
                    .update(cx, |desktop, cx| desktop.dispatch(message, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let weak = desktop.downgrade();
        cx.on_window_closed(move |cx, id| {
            let weak = weak.clone();
            cx.defer(move |cx| {
            let _ = weak.update(cx, |desktop, cx| {
                let key = desktop
                    .windows
                    .iter()
                    .find_map(|(key, handle)| (handle.window_id() == id).then_some(*key));
                let Some(key) = key else {
                    return;
                };
                desktop.windows.remove(&key);
                desktop.views.remove(&key);
                desktop.dispatch(Message::WindowWasClosed(key), cx);
            });
            });
        })
        .detach();
        desktop.update(cx, |desktop, cx| {
            desktop.start(initial, cx).detach();
            desktop.subscriptions(cx);
        });
        let command_desktop = desktop.downgrade();
        cx.spawn(async move |cx: &mut AsyncApp| {
            while let Some(pending) = commands.next().await {
                let _ = command_desktop.update(cx, |desktop, cx| desktop.execute(pending.command, cx));
                let _ = pending.completed.send(());
            }
        }).detach();
        // Keep the windowless desktop/tray alive until quit, without putting
        // its strong handle in a detached future whose cancellation may lag.
        let mut desktop = Some(desktop);
        cx.on_app_quit(move |_| {
            drop(desktop.take());
            async {}
        }).detach();
    });
}

pub(crate) fn seconds() -> impl futures::Stream<Item = ()> {
    futures::stream::unfold((), |()| async {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        Some(((), ()))
    })
}

pub(crate) fn toast_ticks() -> impl futures::Stream<Item = ()> {
    futures::stream::unfold((), |()| async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        Some(((), ()))
    })
}
