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
                    inputs: HashMap::new(),
                    input_step: None,
                    qr: None,
                    video: None,
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
                    this.observe_module_window(ui_lang_wire::events::Window::CloseRequested, cx)
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
                this.observe_module_window(ui_lang_wire::events::Window::CloseRequested, cx)
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

pub(crate) struct DesktopWindow {
    video: Option<(
        String,
        Entity<crate::video::VideoView>,
        Entity<crate::video::VideoView>,
    )>,
    model: Entity<Desktop>,
    kind: WindowKind,
    module: Option<(&'static str, Entity<crate::module_view::NativeModuleView>)>,
    module_route: Option<fn(crate::module_view::ModuleViewEvent) -> Message>,
    route: Option<gpui_kit::Subscription>,
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
        let escape = crate::backend::escape_target(
            key.key.clone(),
            state.palette_open,
            state.bell_open,
            state.channel_create_open,
        );
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
        self.observe_module_window(ui_lang_wire::events::Window::Closed, cx);
    }
    fn observe_module_window(
        &mut self,
        event: ui_lang_wire::events::Window,
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
                match key {
                    "palette-input" => {
                        let text = input.read(cx).value().to_string();
                        model.update(cx, |model, cx| {
                            model.dispatch(Message::PaletteChanged(text), cx)
                        });
                    }
                    "channel-draft" => {
                        let text = input.read(cx).value().to_string();
                        model.update(cx, |model, cx| {
                            model.dispatch(Message::ChannelDraftChanged(text), cx)
                        });
                    }
                    _ => {}
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
                        .text_size(px(24.))
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
                                    "Unlock",
                                    busy,
                                    |this, cx| Message::UnlockSubmit(this.value("unlock", cx)),
                                    cx,
                                )
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
                                    .font_family(design::fonts::FAMILY_MONO)
                                    .text_color(colors.muted_foreground)
                                    .child(number),
                            )
                            .child(
                                div()
                                    .font_family(design::fonts::FAMILY_MONO)
                                    .child(text),
                            )
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
                                .font_family(design::fonts::FAMILY_MONO)
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
                        let node = ui_lang_wire::Node::Qr {
                            key: "account-qr".into(),
                            code: ui_lang_wire::Qr {
                                payload: Some(payload.as_bytes().to_vec()),
                                size: Some(ui_lang_wire::QrSize::Total(220.0)),
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
        use gpui_kit::component::button::ButtonVariants as _;
        use gpui_kit::*;
        let colors = gpui_kit::component::Theme::global(cx).color_tokens();
        let state = &self.model.read(cx).state;
        let live_dot = hsla_of(design::palette(state.is_dark()).accent);
        // The ring a plate wears while its person talks.
        let speaking_ring = hsla_of(design::palette(state.is_dark()).success);
        let mute = if state.call_muted { "Unmute" } else { "Mute" };
        let camera = if state.call_camera {
            "Stop camera"
        } else {
            "Camera"
        };
        let screen = if state.call_sharing {
            "Stop sharing"
        } else {
            "Share screen"
        };
        let stage = state.huddle_stage.clone();
        let video_live = state.call_video_live;
        let invitees =
            crate::backend::huddle_invitees(&state.huddle_invitees, &state.huddle_roster);
        let (muted, camera_on, sharing) = (state.call_muted, state.call_camera, state.call_sharing);
        let rows = state.huddle_rows.clone();
        let row_count = rows.len();
        let title = state.huddle_channel_name.clone();
        let status = state.call_status.clone();
        let elapsed = if state.huddle_joined_at > 0 {
            crate::backend::mmss(state.huddle_now - state.huddle_joined_at)
        } else {
            String::new()
        };
        match &mut self.video {
            Some((previous, tiles, picture)) => {
                if previous != &stage {
                    tiles.update(cx, |view, cx| view.replace_tiles(stage.clone(), cx));
                    picture.update(cx, |view, cx| view.replace_stage(stage.clone(), cx));
                    *previous = stage.clone();
                }
            }
            None => {
                self.video = Some((
                    stage.clone(),
                    cx.new(|_| crate::video::call_video_tiles(&stage)),
                    cx.new(|_| crate::video::call_video_stage(&stage)),
                ))
            }
        }
        let (_, tiles, picture) = self.video.as_ref().expect("retained video surfaces");
        let mut body = div()
            .id("huddle-stage")
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .p_3()
            .flex()
            .flex_col()
            .gap_3();
        // The video takes the room the roster leaves: the stage whole in the
        // largest box left, the tiles a strip under it or, with no stage, the
        // grid in that box instead. The surfaces fill whatever box they get.
        if !stage.is_empty() {
            body = body.child(div().flex_1().min_h_0().w_full().child(picture.clone()));
        }
        if video_live {
            let tiles_box = if stage.is_empty() {
                div().flex_1().min_h_0()
            } else {
                div().flex_shrink_0()
            };
            body = body.child(tiles_box.w_full().child(tiles.clone()));
        }
        // The people, one a row, the way a voice channel lists them: the
        // plate, the name, and "you" / "muted" beside it. The list stays
        // virtual: only the rows on screen are built.
        body = body.child(
            div()
                .px_1()
                .text_size(px(11.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(colors.muted_foreground)
                .child(format!("In the huddle · {row_count}")),
        );
        body = body.child(
            uniform_list("huddle-roster", row_count, move |range, _, _| {
                rows[range]
                    .iter()
                    .map(|row| {
                        let caption = match (row.person.is_you, row.muted) {
                            (true, true) => "you · muted",
                            (true, false) => "you",
                            (false, true) => "muted",
                            (false, false) => "",
                        };
                        let mut plate = div()
                            .flex_shrink_0()
                            .size(px(26.))
                            .rounded_full()
                            .bg(colors.secondary)
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(row.person.initials.clone());
                        if row.speaking {
                            plate = plate.border_2().border_color(speaking_ring);
                        }
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .h(px(36.))
                            .px_2()
                            .rounded(px(design::radius::CONTROL as f32))
                            .child(plate)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(13.))
                                    .child(row.person.label.clone()),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(11.5))
                                    .text_color(colors.muted_foreground)
                                    .child(caption),
                            )
                    })
                    .collect()
            })
            // with video on screen the list yields to it and scrolls instead
            .when(video_live, |list| list.flex_shrink_0().max_h(px(150.)))
            .when(!video_live, |list| list.flex_1().min_h_0()),
        );
        // The room's members not seated yet, one chip each: a press posts a
        // mention into the room that says come join, and the chip is gone —
        // an invite is sent once.
        if !invitees.is_empty() {
            use gpui_kit::component::Sizable as _;
            let mut chips = div().flex().flex_wrap().gap_1();
            for member in invitees {
                chips = chips.child(
                    self.action(
                        format!("huddle-invite-{}", member.key),
                        member.label.clone(),
                        Message::InviteToHuddle(member.key.clone()),
                        false,
                    )
                    .outline()
                    .xsmall(),
                );
            }
            body = body.child(
                div()
                    .px_3()
                    .py_2()
                    .flex_shrink_0()
                    .border_t_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .pb_1()
                            .text_size(px(11.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(colors.muted_foreground)
                            .child("Invite"),
                    )
                    .child(chips),
            );
        }
        // The control bar reads left to right as media, then the room, then
        // the exit: a toggle that is ON is filled so its state is visible
        // without reading the label (a muted mic is the red one).
        let mute_button = self.action("huddle-mute", mute, Message::ToggleCallMute, false);
        let mute_button = match muted {
            true => mute_button.danger(),
            false => mute_button.outline(),
        };
        let camera_button = self.action("huddle-camera", camera, Message::ToggleCallCamera, false);
        let camera_button = match camera_on {
            true => camera_button.primary(),
            false => camera_button.outline(),
        };
        let screen_button = self.action("huddle-screen", screen, Message::ToggleCallScreen, false);
        let screen_button = match sharing {
            true => screen_button.primary(),
            false => screen_button.outline(),
        };
        let controls = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .p_3()
            .flex_shrink_0()
            .border_t_1()
            .border_color(colors.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(mute_button)
                    .child(camera_button)
                    .child(screen_button),
            )
            .child(div().flex_1())
            .child(
                self.action(
                    "huddle-channel",
                    "Go to channel",
                    Message::HuddleGoChannel,
                    false,
                )
                .ghost(),
            )
            .child(
                self.action(
                    "huddle-leave",
                    "Leave huddle",
                    Message::LeaveHuddleHere,
                    false,
                )
                .danger(),
            );
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.background)
            .text_color(colors.foreground)
            .child(
                div()
                    .px_3()
                    .py_2()
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(colors.border)
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().size(px(8.)).rounded_full().bg(live_dot))
                    .child(div().font_weight(FontWeight::MEDIUM).child(title))
                    .child(
                        div()
                            .font_family(design::fonts::FAMILY_MONO)
                            .text_size(px(12.))
                            .text_color(colors.muted_foreground)
                            .child(elapsed),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(colors.muted_foreground)
                            .child(status),
                    ),
            )
            .child(body)
            .child(controls)
            .into_any_element()
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
            (ShellTab::Explorer, label("explorer")),
            (ShellTab::Node, label("node")),
            (ShellTab::Members, label("members")),
            (ShellTab::Governance, label("governance")),
        ]);
        // the other views the connected node's registry lists, after the
        // built-in tabs and in the registry's order; named by their manifests
        navigation.extend(
            registered
                .into_iter()
                .filter(|module| *module != HOME_VIEW)
                .map(|module| (ShellTab::Registered(module), label(module))),
        );
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
            others: state.huddle_rows.len().saturating_sub(1),
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
        let state = &self.model.read(cx).state;
        let topmost = crate::backend::topmost_overlay(
            state.palette_open,
            state.bell_open,
            state.channel_create_open,
        );
        use gpui_kit::component::button::ButtonVariants as _;
        use gpui_kit::component::{ActiveTheme as _, Disableable as _};
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
            "bell" => {
                let generation = state.connect_generation;
                let account = state.account_number.clone();
                let items = crate::backend::bell_visible_items(
                    &state.bell_items,
                    &account,
                    &state.settings_user_key,
                );
                let presentations = state.bell_presentations.clone();
                let mut actions = div().flex().items_center().gap_2().px_1().pb_1().child(
                    self.action(
                        "bell-mark-read",
                        "Mark all read",
                        Message::MarkBellReadSubmit,
                        state.bell_marking,
                    )
                    .outline()
                    .h_7(),
                );
                if !state.bell_error.is_empty() {
                    actions = actions
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(12.5))
                                .text_color(colors.destructive)
                                .child(state.bell_error.clone()),
                        )
                        .child(
                            self.action("bell-retry", "Retry", Message::ReloadBell, false)
                                .ghost()
                                .h_7(),
                        );
                }
                body = body.child(actions);
                if items.is_empty() {
                    body = body.child(
                        div()
                            .px_2()
                            .py_2()
                            .text_size(px(12.5))
                            .text_color(muted)
                            .child("Nothing new. Mentions and direct messages land here."),
                    );
                }
                for item in items {
                    let presentation = crate::backend::bell_presentation(&item, &presentations);
                    let unavailable = !crate::backend::bell_openable(&item, &presentations);
                    body = body.child(row(
                        self,
                        format!("notification/{}", presentation.seq),
                        presentation.title.clone(),
                        presentation.detail.clone(),
                        Message::BellOpenItem(generation, account.clone(), presentation),
                        unavailable,
                    ));
                }
                ("Notifications", Message::CloseBell)
            }
            "channel_create" => {
                let busy = state.mutation_phase != crate::MutationPhase::Idle;
                let members_only = state.channel_create_members_only;
                let voice = state.channel_create_voice;
                body = body
                    .gap_3()
                    .px_4()
                    .pb_4()
                    .child(self.input("channel-draft", "Channel name", false, window, cx))
                    .child(
                        gpui_kit::component::checkbox::Checkbox::new("channel-voice")
                            .label("Voice room")
                            .checked(voice)
                            .disabled(busy)
                            .on_click({
                                let model = self.model.clone();
                                move |_, _, cx| {
                                    model.update(cx, |model, cx| {
                                        model.dispatch(Message::ToggleChannelCreateVoice, cx)
                                    })
                                }
                            }),
                    )
                    .child(
                        gpui_kit::component::checkbox::Checkbox::new("channel-private")
                            .label("Members only")
                            .checked(members_only)
                            .disabled(busy || voice)
                            .on_click({
                                let model = self.model.clone();
                                move |_, _, cx| {
                                    model.update(cx, |model, cx| {
                                        model.dispatch(
                                            Message::ToggleChannelCreateMembersOnly,
                                            cx,
                                        )
                                    })
                                }
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(muted)
                            .child(match voice {
                                true => "A voice room is a huddle with a name: pick it in the list to join.",
                                false => "A members-only channel is read and written by its roster alone.",
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                self.action(
                                    "channel-cancel",
                                    "Cancel",
                                    Message::ToggleChannelCreate,
                                    busy,
                                )
                                .ghost(),
                            )
                            .child(
                                self.action(
                                    "channel-submit",
                                    "Create channel",
                                    Message::CreateChannelSubmit,
                                    busy,
                                )
                                .primary(),
                            ),
                    );
                ("Create a channel", Message::ToggleChannelCreate)
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
        gpui_kit::div()
            .id("desktop-root")
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .track_focus(&self.focus)
            .on_drop(cx.listener(|this, paths: &gpui_kit::ExternalPaths, _, cx| {
                if this.kind != WindowKind::Console {
                    return;
                }
                // The existing Files reducer owns the write gate and permits
                // one upload at a time. Native paths never reach the WASM view.
                for path in paths.paths() {
                    let Some(path) = path.to_str() else {
                        this.model.update(cx, |model, cx| {
                            model.dispatch(
                                Message::FsDropFailed(crate::backend::AppError {
                                    message: "This file path cannot be represented as UTF-8."
                                        .into(),
                                    committed: false,
                                }),
                                cx,
                            )
                        });
                        continue;
                    };
                    this.model.update(cx, |model, cx| {
                        model.dispatch(Message::FsFileDropped(path.to_owned()), cx)
                    });
                }
            }))
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
            inputs: HashMap::new(),
            input_step: None,
            qr: None,
            video: None,
            focus: cx.focus_handle(),
            _activation: activation,
            _observer: observer,
            _keystrokes: keystrokes,
        }
    })
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
    fn native_drop_error_dismiss_and_bell_retry_reach_domain_handlers() {
        use gpui_kit::test::TestWindowExt as _;
        use gpui_kit::{ExternalPaths, FileDropEvent, InputEvent, point, px, size};
        assert!(tokio::runtime::Handle::try_current().is_err());
        let _turn = crate::module_view::tests::blocking_connection_turn();
        let mut cx = crate::frame_probe::headless_context();
        let mut state = Ducktape::initial_state();
        state.connected = true;
        state.connected_rpc = "http://127.0.0.1:0".into();
        state.shell_tab = ShellTab::Files;
        state.settings_user_key = "invalid signing key".into();
        state.fs_drop_dir = "/shared".into();
        let expected = crate::backend::files_write_gate(
            state.fs_drop_dir.clone(),
            state.settings_user_key.clone(),
        );
        assert!(!expected.is_empty());
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
        let position = point(px(400.), px(350.));
        cx.update_window(handle.into(), |_, window, cx| {
            window.dispatch_event(
                FileDropEvent::Entered {
                    position,
                    paths: ExternalPaths(
                        [std::path::PathBuf::from("/local/report.txt")]
                            .into_iter()
                            .collect(),
                    ),
                }
                .to_platform_input(),
                cx,
            )
        })
        .unwrap();
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
        cx.update_window(handle.into(), |_, window, cx| {
            window.dispatch_event(FileDropEvent::Submit { position }.to_platform_input(), cx)
        })
        .unwrap();
        view.read_with(&cx, |view, cx| {
            assert_eq!(view.test_state(cx).error, expected);
            assert!(
                !view.test_state(cx).fs_dropping,
                "write gate precedes local file I/O"
            );
        });
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
        cx.update_window(handle.into(), |_, window, cx| {
            window.click("error-dismiss", cx)
        })
        .unwrap();
        view.read_with(&cx, |view, cx| {
            assert!(view.test_state(cx).error.is_empty())
        });
        view.update(&mut cx, |view, cx| {
            view.model.update(cx, |model, cx| {
                model.state.bell_open = true;
                model.state.bell_error = "Could not load notifications".into();
                cx.notify();
            });
        });
        let generation = view.read_with(&cx, |view, cx| view.test_state(cx).bell_load_generation);
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
        cx.update_window(handle.into(), |_, window, cx| {
            window.click("bell-retry", cx)
        })
        .unwrap();
        view.read_with(&cx, |view, cx| {
            assert_eq!(
                view.test_state(cx).bell_load_generation,
                generation.wrapping_add(1)
            );
        });
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
                let view = test_window(state, WindowKind::Huddle, window, cx);
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
                view.observe_module_window(ui_lang_wire::events::Window::CloseRequested, cx);
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

pub(crate) fn run() {
    // The kit's component icons (search, bell, folder, …) are SVGs the app
    // loads by path; without a source they draw as nothing.
    let application = gpui_kit::application().with_assets(gpui_kit::assets::Assets);
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
        let fonts: Vec<std::borrow::Cow<'static, [u8]>> = vec![
            std::borrow::Cow::Borrowed(include_bytes!("../../crates/views/support/design/assets/fonts/Geist[wght].ttf")),
            std::borrow::Cow::Borrowed(include_bytes!("../../crates/views/support/design/assets/fonts/GeistMono[wght].ttf")),
            std::borrow::Cow::Borrowed(include_bytes!("../../crates/views/support/design/assets/fonts/NotoColorEmoji.ttf")),
        ];
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
