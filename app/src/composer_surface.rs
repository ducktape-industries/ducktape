//! Native host composers for dynamically loaded WASM views. Drafts belong to
//! their scope, not a mounted view: switching rooms never loses an unsent body.
//! Only submit intents cross the wire. Native GPUI owns caret, IME, selection,
//! scrolling and text history; this module owns mention identities and sends.

use crate::backend::{ChatMember, MentionCandidates, names_at, names_generation, room_scope};
use gpui_kit::EntityInputHandler;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{
    Copy, Cut, Editor, EditorState, InputEvent, Paste, TextDecoration, TextDecorationCollection,
};
use gpui_kit::component::{ActiveTheme, Disableable};
use gpui_kit::{
    App, AppContext, ClipboardItem, Context, Entity, EventEmitter, FontWeight, HighlightStyle,
    InteractiveElement, IntoElement, KeyDownEvent, Keystroke, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Window, div, px,
};
use std::{
    cell::RefCell,
    collections::HashMap,
    ops::Range,
    sync::{Arc, Mutex, MutexGuard},
};
use ui_lang_wire::SurfaceValue as Value;

type Mentions = Vec<(Range<usize>, chat::Party)>;
#[derive(Default)]
struct Document {
    text: String,
    failed: String,
    menu: MenuState,
    handles: Vec<chat::client::MentionChoice>,
    mentions: Mentions,
}
#[derive(Default)]
struct MenuState {
    partial: String,
    selected: usize,
    dismissed: bool,
}
#[derive(Clone, Debug)]
struct MentionQuery {
    range: Range<usize>,
    partial: String,
}
#[derive(Clone)]
struct Menu {
    query: MentionQuery,
    matches: Vec<chat::client::MentionChoice>,
    selected: usize,
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Facts {
    names: u64,
    rosters: u64,
}
struct Slot {
    scope: String,
    document: Document,
    rev: u64,
    facts: Option<Facts>,
    focus_pending: bool,
}
type Shared = Arc<Mutex<Slot>>;
thread_local! {
    // ponytail: one retained draft per visited scope; add eviction only when a
    // real long session requires it, never discard a draft on view unmount.
    static SLOTS: RefCell<HashMap<String, Shared>> = RefCell::default();
}
fn slot(scope: &str) -> Shared {
    SLOTS.with_borrow_mut(|slots| {
        slots
            .entry(scope.to_owned())
            .or_insert_with(|| {
                Arc::new(Mutex::new(Slot {
                    scope: scope.to_owned(),
                    document: Document::default(),
                    rev: 0,
                    facts: None,
                    focus_pending: false,
                }))
            })
            .clone()
    })
}
fn restore_document(document: &mut Document, body: &str) {
    let (_, names) = names_at();
    let normalized = body.replace("\r\n", "\n").replace('\r', "\n");
    (document.text, document.mentions) = chat::client::draft_mentions(&normalized, &names);
    document.menu = MenuState::default();
}
pub fn seed(scope: &str, body: &str) {
    let shared = slot(scope);
    let mut slot = lock(&shared);
    restore_document(&mut slot.document, body);
    slot.document.failed.clear();
    slot.focus_pending = true;
    slot.rev += 1;
}

#[derive(Default)]
struct Rosters {
    by_room: HashMap<String, Vec<ChatMember>>,
    generation: u64,
}

thread_local! {
    static ROSTERS: RefCell<Rosters> = RefCell::default();
}

/// The app's hand-off: the roster of the room under `scope`, for the
/// composers over it. Called wherever the app learns a room's members.
pub fn roster(scope: &str, members: &[ChatMember]) {
    ROSTERS.with_borrow_mut(|rosters| {
        rosters.by_room.insert(scope.to_owned(), members.to_vec());
        rosters.generation += 1;
    });
}

/// The generations the fact stores are at right now.
fn current_facts() -> Facts {
    Facts {
        names: names_generation(),
        rosters: ROSTERS.with_borrow(|rosters| rosters.generation),
    }
}

/// Who an `@` in this composer may complete to — the send's own rule over
/// the name directory and the room's roster, under the scope itself (a
/// room), else under the room a thread scope names — and the generations
/// the stores were read at.
fn handles(scope: &str) -> (Facts, Vec<chat::client::MentionChoice>) {
    let (rosters_generation, members) = ROSTERS.with_borrow(|rosters| {
        let members = rosters
            .by_room
            .get(scope)
            .or_else(|| rosters.by_room.get(&room_scope(scope)))
            .cloned()
            .unwrap_or_default();
        (rosters.generation, members)
    });
    let (names_generation, directory) = names_at();
    let handles = MentionCandidates::new(&directory, &members).choices();
    let facts = Facts {
        names: names_generation,
        rosters: rosters_generation,
    };
    (facts, handles)
}

fn lock(slot: &Shared) -> MutexGuard<'_, Slot> {
    // A panic while the lock was held leaves the slot usable: the widget
    // only ever reads it here and applies whole interactions.
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The app's share-out: what a room's or thread's send let go of, handed
/// back to the box it came from. A committed body is not unsent, so it
/// never stashes — `remember_failed_draft` decides, as it did in the view.
/// The banner it raises is shape, so it shows at the next paint.
pub fn unsent(scope: &str, text: &str, committed: bool) {
    let slot = slot(scope);
    let mut slot = lock(&slot);
    let before = std::mem::take(&mut slot.document.failed);
    let failed = crate::backend::remember_failed_draft(
        before.clone(),
        "stash".into(),
        text.to_owned(),
        committed,
    );
    let stash_changed = failed != before;
    slot.document.failed = failed;
    if stash_changed {
        slot.rev += 1;
    }
}

/// The handles brought up to the fact stores when either moved on since
/// they were last computed; a change moves the document's revision.
fn take_facts(slot: &mut Slot) {
    let facts_current = slot.facts == Some(current_facts());
    if facts_current {
        return;
    }
    let (facts, handles) = handles(&slot.scope);
    slot.facts = Some(facts);
    if slot.document.handles == handles {
        return;
    }
    slot.document.handles = handles;
    slot.rev += 1;
}

pub fn intent(value: &Value) -> Option<crate::module_view::ModuleViewEvent> {
    let Value::Record { name, fields } = value else {
        return None;
    };
    if name != "composer" {
        return None;
    }
    let field = |wanted: &str| {
        fields.iter().find_map(|(key, value)| match value {
            Value::Str(text) if key == wanted => Some(text.as_str()),
            _ => None,
        })
    };
    let detail = serde_json::json!({
        "scope": field("scope")?,
        "kind": field("kind")?,
        "body": field("body")?,
        "id": field("id")?,
    });
    Some(crate::module_view::ModuleViewEvent {
        kind: "composer".into(),
        detail: detail.to_string(),
    })
}

/// Only the existing seven-argument composer contract is accepted.
struct Arguments {
    scope: String,
    kind: String,
    compact: bool,
    hint: String,
    blocked: bool,
    restore_blocked: bool,
    failed_note: String,
}
pub fn validate_args(args: &[Value]) -> Result<(), String> {
    Arguments::parse(args).map(|_| ())
}
impl Arguments {
    fn parse(args: &[Value]) -> Result<Self, String> {
        let [
            Value::Str(scope),
            Value::Str(kind),
            Value::Bool(compact),
            Value::Str(hint),
            Value::Bool(blocked),
            Value::Bool(restore_blocked),
            Value::Str(failed_note),
        ] = args
        else {
            return Err("invalid composer arguments".into());
        };
        Ok(Self {
            scope: scope.clone(),
            kind: kind.clone(),
            compact: *compact,
            hint: hint.clone(),
            blocked: *blocked,
            restore_blocked: *restore_blocked,
            failed_note: failed_note.clone(),
        })
    }
}

pub struct ComposerView {
    shared: Shared,
    args: Arguments,
    editor: Entity<EditorState>,
    decorations: TextDecorationCollection,
    _input: Subscription,
    _observation: Subscription,
    revision: u64,
    edit_anchor: Option<Range<usize>>,
    ime: Option<crate::module_view::input::ImeState>,
}
impl EventEmitter<Value> for ComposerView {}
impl EventEmitter<ui_lang_wire::Event> for ComposerView {}
impl ComposerView {
    pub fn new(
        args: &[Value],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Self, String> {
        let args = Arguments::parse(args)?;
        let shared = slot(&args.scope);
        let editor = cx.new(|cx| {
            let mut editor = EditorState::new(window, cx)
                .language("markdown")
                .line_number(false)
                .folding(false)
                .soft_wrap(true)
                .searchable(false);
            editor.set_placeholder(args.hint.clone(), window, cx);
            editor.set_value(lock(&shared).document.text.clone(), window, cx);
            editor
        });
        let decorations = editor.update(cx, |editor, cx| {
            editor.create_decorations_collection(Vec::new(), cx)
        });
        let input = cx.subscribe_in(&editor, window, |this, _, event, window, cx| match event {
            InputEvent::Change => this.changed(window, cx),
            InputEvent::PressEnter { .. } | InputEvent::Focus | InputEvent::Blur => cx.notify(),
        });
        let observation = cx.observe_in(&editor, window, |this, editor, window, cx| {
            let (text, marked, cursor, selection) = editor.update(cx, |editor, cx| {
                (
                    editor.value().to_string(),
                    editor.marked_text_range(window, cx),
                    editor.cursor(),
                    editor.selected_range(),
                )
            });
            for event in crate::module_view::input::ime_events(
                &mut this.ime,
                &text,
                marked,
                cursor,
                selection,
            ) {
                cx.emit(event);
            }
            cx.notify();
        });
        let mut this = Self {
            shared,
            args,
            editor,
            decorations,
            _input: input,
            _observation: observation,
            revision: u64::MAX,
            edit_anchor: None,
            ime: None,
        };
        this.sync(window, cx);
        Ok(this)
    }
    pub fn replace(
        &mut self,
        args: &[Value],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let args = Arguments::parse(args)?;
        if self.args.scope != args.scope {
            self.shared = slot(&args.scope);
            self.revision = u64::MAX;
        }
        if self.args.hint != args.hint {
            self.editor.update(cx, |editor, cx| {
                editor.set_placeholder(args.hint.clone(), window, cx)
            });
        }
        self.args = args;
        self.sync(window, cx);
        cx.notify();
        Ok(())
    }
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (revision, text, focus) = {
            let mut slot = lock(&self.shared);
            take_facts(&mut slot);
            (
                slot.rev,
                slot.document.text.clone(),
                std::mem::take(&mut slot.focus_pending),
            )
        };
        let changed = revision != self.revision;
        if changed {
            // Guest argument echoes and roster updates do not reset native
            // history, scroll or caret. Only an authoritative text change does.
            if self.editor.read(cx).value().as_ref() != text {
                if self.ime.take().is_some() {
                    cx.emit(ui_lang_wire::Event::Observation {
                        event: ui_lang_wire::events::Event::InputMethod(
                            ui_lang_wire::events::InputMethod::Closed,
                        ),
                        captured: false,
                    });
                }
                self.editor
                    .update(cx, |editor, cx| editor.set_value(text, window, cx));
            }
            self.revision = revision;
            self.decorate(cx);
        }
        if focus {
            self.editor
                .update(cx, |editor, cx| editor.focus(window, cx));
        }
    }
    fn decorate(&self, cx: &mut Context<Self>) {
        let style = HighlightStyle {
            color: Some(cx.theme().link),
            font_weight: Some(FontWeight::MEDIUM),
            ..Default::default()
        };
        let decorations = lock(&self.shared)
            .document
            .mentions
            .iter()
            .map(|(range, _)| TextDecoration::new(range.clone(), style))
            .collect();
        self.decorations.set(decorations, cx);
    }
    fn changed(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value().to_string();
        {
            let mut slot = lock(&self.shared);
            if slot.document.text == text {
                return;
            }
            let document = &mut slot.document;
            reconcile_mentions(
                &document.text,
                &text,
                &mut document.mentions,
                self.edit_anchor.take(),
            );
            document.text = text;
            slot.rev += 1;
            self.revision = slot.rev;
        }
        self.decorate(cx);
        cx.notify();
    }
    fn selection(&self, cx: &App) -> Range<usize> {
        self.editor.read(cx).selected_range()
    }
    fn menu(&self, cx: &App) -> Option<Menu> {
        if self.args.blocked {
            return None;
        }
        menu(&lock(&self.shared).document, self.selection(cx))
    }
    fn composing(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.editor.update(cx, |editor, cx| {
            editor.marked_text_range(window, cx).is_some()
        })
    }
    fn native_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.composing(window, cx) {
            cx.propagate();
            return;
        }
        let menu_open = self.menu(cx).is_some();
        let claims = menu_open && matches!(key, "up" | "down" | "tab" | "escape") || key == "enter";
        if claims {
            self.key_down(
                &KeyDownEvent {
                    keystroke: Keystroke {
                        key: key.into(),
                        key_char: None,
                        modifiers: Default::default(),
                    },
                    is_held: false,
                    prefer_character_input: false,
                },
                window,
                cx,
            );
            return;
        }
        if matches!(key, "backspace" | "delete") {
            let selected = expand_selection(
                self.selection(cx),
                &lock(&self.shared).document.mentions,
                Some(key),
            );
            self.editor.update(cx, |editor, cx| {
                editor.set_selected_range(selected.clone(), cx)
            });
            self.edit_anchor = Some(selected);
        }
        cx.propagate();
    }
    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.composing(window, cx) {
            return;
        }
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let command = if cfg!(target_os = "macos") {
            modifiers.platform
        } else {
            modifiers.control
        };
        if !command && let Some(menu) = self.menu(cx) {
            match key {
                "up" | "down" => {
                    let selected = match key {
                        "up" => (menu.selected + menu.matches.len() - 1) % menu.matches.len(),
                        _ => (menu.selected + 1) % menu.matches.len(),
                    };
                    lock(&self.shared).document.menu = MenuState {
                        partial: menu.query.partial,
                        selected,
                        dismissed: false,
                    };
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                "enter" | "tab" => {
                    self.pick(menu.matches[menu.selected].clone(), window, cx);
                    cx.stop_propagation();
                    return;
                }
                "escape" => {
                    lock(&self.shared).document.menu = MenuState {
                        partial: menu.query.partial,
                        selected: 0,
                        dismissed: true,
                    };
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                _ => {}
            }
        }
        if command {
            let mark = match (key, modifiers.shift) {
                ("b", false) => Some("bold"),
                ("i", false) => Some("italic"),
                ("c", true) => Some("code"),
                ("9", true) => Some("quote"),
                _ => None,
            };
            if let Some(mark) = mark {
                self.mark(mark, window, cx);
                cx.stop_propagation();
                return;
            }
        }
        let sends = key == "enter" && !modifiers.shift && !command && !modifiers.alt;
        if sends {
            if !event.is_held {
                self.submit(window, cx);
            }
            cx.stop_propagation();
            return;
        }
        let edits = matches!(key, "backspace" | "delete" | "enter")
            || (!command && event.keystroke.key_char.is_some());
        if edits {
            let selected = expand_selection(
                self.selection(cx),
                &lock(&self.shared).document.mentions,
                Some(key),
            );
            self.editor.update(cx, |editor, cx| {
                editor.set_selected_range(selected.clone(), cx)
            });
            self.edit_anchor = Some(selected);
        }
    }
    fn replace_selection(
        &mut self,
        range: Range<usize>,
        text: String,
        mentions: Mentions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        {
            let mut slot = lock(&self.shared);
            replace_ranges(&mut slot.document.mentions, range.clone(), text.len());
            slot.document.mentions.extend(
                mentions
                    .into_iter()
                    .map(|(span, party)| (range.start + span.start..range.start + span.end, party)),
            );
            slot.document.mentions.sort_by_key(|(span, _)| span.start);
            slot.document.text.replace_range(range.clone(), &text);
            slot.document.menu = MenuState::default();
            slot.rev += 1;
            self.revision = slot.rev;
        }
        self.edit_anchor = None;
        self.editor.update(cx, |editor, cx| {
            editor.set_selected_range(range, cx);
            editor.replace(text, window, cx);
            editor.focus(window, cx);
        });
        self.decorate(cx);
        cx.notify();
    }
    fn pick(
        &mut self,
        choice: chat::client::MentionChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.menu(cx) else {
            return;
        };
        if !menu.matches.contains(&choice) {
            return;
        }
        let text = format!("@{} ", choice.label);
        let mentions = vec![(0..text.len() - 1, choice.party)];
        self.replace_selection(menu.query.range, text, mentions, window, cx);
    }
    fn copy(&mut self, cut: bool, window: &mut Window, cx: &mut Context<Self>) {
        if cut && self.args.blocked {
            return;
        }
        let selection = self.selection(cx);
        if selection.is_empty() {
            return;
        }
        let (range, body) = {
            let slot = lock(&self.shared);
            let range = expand_selection(selection, &slot.document.mentions, None);
            let mentions = slot
                .document
                .mentions
                .iter()
                .filter(|(span, _)| span.start >= range.start && span.end <= range.end)
                .map(|(span, party)| {
                    (
                        span.start - range.start..span.end - range.start,
                        party.clone(),
                    )
                })
                .collect::<Vec<_>>();
            let body = mention_body(&slot.document.text[range.clone()], &mentions);
            (range, body)
        };
        cx.write_to_clipboard(ClipboardItem::new_string(body));
        if cut {
            self.replace_selection(range, String::new(), Vec::new(), window, cx);
        } else {
            self.editor
                .update(cx, |editor, cx| editor.set_selected_range(range, cx));
        }
    }
    fn paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.args.blocked {
            return;
        }
        let Some(body) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let (_, names) = names_at();
        let normalized = body.replace("\r\n", "\n").replace('\r', "\n");
        let (text, mentions) = chat::client::draft_mentions(&normalized, &names);
        let range = expand_selection(
            self.selection(cx),
            &lock(&self.shared).document.mentions,
            None,
        );
        self.replace_selection(range, text, mentions, window, cx);
    }
    fn mark(&mut self, kind: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.args.blocked {
            return;
        }
        let (open, close) = match kind {
            "bold" => ("**", "**"),
            "italic" => ("_", "_"),
            "code" => ("```\n", "\n```"),
            "quote" => ("> ", ""),
            _ => return,
        };
        let (range, text, mentions) = {
            let slot = lock(&self.shared);
            let range = expand_selection(self.selection(cx), &slot.document.mentions, None);
            let text = format!("{open}{}{close}", &slot.document.text[range.clone()]);
            let mentions = slot
                .document
                .mentions
                .iter()
                .filter(|(span, _)| span.start >= range.start && span.end <= range.end)
                .map(|(span, party)| {
                    (
                        open.len() + span.start - range.start..open.len() + span.end - range.start,
                        party.clone(),
                    )
                })
                .collect();
            (range, text, mentions)
        };
        let caret = range.start + open.len();
        let empty = range.is_empty();
        self.replace_selection(range, text, mentions, window, cx);
        if empty {
            self.editor
                .update(cx, |editor, cx| editor.set_selected_range(caret..caret, cx));
        }
    }
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let submitted = {
            let mut slot = lock(&self.shared);
            let value = submit_document(
                &mut slot.document,
                &self.args.scope,
                &self.args.kind,
                self.args.blocked,
            );
            if value.is_some() {
                slot.rev += 1;
            }
            value
        };
        if let Some(value) = submitted {
            self.sync(window, cx);
            cx.emit(value);
        }
    }
    fn restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        {
            let mut slot = lock(&self.shared);
            if !restore_failed(&mut slot.document, self.args.restore_blocked) {
                return;
            }
            slot.focus_pending = true;
            slot.rev += 1;
        }
        self.sync(window, cx);
        cx.notify();
    }
}
impl Render for ComposerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync(window, cx);
        let (empty, failed, lines) = {
            let slot = lock(&self.shared);
            (
                slot.document.text.trim().is_empty(),
                !slot.document.failed.is_empty(),
                slot.document.text.lines().count().max(1),
            )
        };
        let mut content = div()
            .id("composer")
            .flex()
            .flex_col()
            .w_full()
            .gap(px(8.))
            .capture_key_down(cx.listener(Self::key_down))
            .capture_action(cx.listener(
                |this, action: &gpui_kit::component::input::Enter, window, cx| {
                    if action.shift || action.secondary {
                        cx.propagate();
                    } else {
                        this.native_key("enter", window, cx);
                    }
                },
            ))
            .capture_action(cx.listener(
                |this, _: &gpui_kit::component::input::MoveUp, window, cx| {
                    this.native_key("up", window, cx)
                },
            ))
            .capture_action(cx.listener(
                |this, _: &gpui_kit::component::input::MoveDown, window, cx| {
                    this.native_key("down", window, cx)
                },
            ))
            .capture_action(cx.listener(
                |this, _: &gpui_kit::component::input::Indent, window, cx| {
                    this.native_key("tab", window, cx)
                },
            ))
            .capture_action(cx.listener(
                |this, _: &gpui_kit::component::input::Escape, window, cx| {
                    this.native_key("escape", window, cx)
                },
            ))
            .capture_action(cx.listener(
                |this, _: &gpui_kit::component::input::Backspace, window, cx| {
                    this.native_key("backspace", window, cx)
                },
            ))
            .capture_action(cx.listener(
                |this, _: &gpui_kit::component::input::Delete, window, cx| {
                    this.native_key("delete", window, cx)
                },
            ))
            .capture_action(cx.listener(|this, _: &Copy, window, cx| {
                this.copy(false, window, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &Cut, window, cx| {
                this.copy(true, window, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &Paste, window, cx| {
                this.paste(window, cx);
                cx.stop_propagation();
            }));
        if failed {
            content = content.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .p(px(10.))
                    .rounded(px(9.))
                    .bg(cx.theme().muted)
                    .text_color(cx.theme().danger)
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.5))
                            .child(self.args.failed_note.clone()),
                    )
                    .child(
                        Button::new("restore")
                            .label("Restore")
                            .disabled(self.args.restore_blocked || !empty)
                            .on_click(cx.listener(|this, _, window, cx| this.restore(window, cx))),
                    )
                    .child(
                        Button::new("dismiss")
                            .label("×")
                            .ghost()
                            .accessibility_label("Dismiss failed draft")
                            .on_click(cx.listener(|this, _, _, cx| {
                                let mut slot = lock(&this.shared);
                                slot.document.failed.clear();
                                slot.rev += 1;
                                cx.notify();
                            })),
                    ),
            );
        }
        let mut plate = div()
            .flex()
            .flex_col()
            .w_full()
            .p(px(if self.args.compact { 6. } else { 8. }))
            .border_1()
            .border_color(cx.theme().border)
            .rounded(px(design::radius::CARD as f32))
            .bg(cx.theme().background);
        if let Some(menu) = self.menu(cx) {
            let mut choices = div()
                .id("mention-choices")
                .flex()
                .flex_col()
                .w_full()
                .max_h(px(220.))
                .overflow_y_scroll();
            for (index, choice) in menu.matches.into_iter().enumerate() {
                let button = Button::new(("mention", index))
                    .label(format!("@{}", choice.label))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.pick(choice.clone(), window, cx)
                    }));
                choices = choices.child(if index == menu.selected {
                    button.secondary()
                } else {
                    button.ghost()
                });
            }
            plate = plate.child(choices);
        }
        plate = plate.child(
            // Prose, not code: the composer reads in the UI face at body size.
            Editor::new(&self.editor)
                .readonly(self.args.blocked)
                .bordered(false)
                .appearance(false)
                .font_family(design::fonts::FAMILY_UI)
                .text_size(px(design::type_scale::BODY as f32))
                .h(px((lines as f32 * 20. + 16.).clamp(40., 160.)))
                .aria_label(self.args.hint.clone()),
        );
        let mut toolbar = div().flex().items_center().gap(px(4.));
        for (kind, label, aria) in [
            ("bold", "B", "Bold"),
            ("italic", "I", "Italic"),
            ("code", "</>", "Code block"),
            ("quote", "❝", "Quote"),
        ] {
            toolbar = toolbar.child(
                Button::new(kind)
                    .label(label)
                    .ghost()
                    .accessibility_label(aria)
                    .disabled(self.args.blocked)
                    .on_click(cx.listener(move |this, _, window, cx| this.mark(kind, window, cx))),
            );
        }
        let send_label = if matches!(self.args.kind.as_str(), "edit" | "thread_edit") {
            "Save"
        } else {
            "Send"
        };
        toolbar = toolbar.child(div().flex_1()).child(
            Button::new("send")
                .label(send_label)
                .primary()
                .disabled(self.args.blocked || empty)
                .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
        );
        content.child(plate.child(toolbar))
    }
}

fn mention_query(text: &str, selected: Range<usize>) -> Option<MentionQuery> {
    if !selected.is_empty() {
        return None;
    }
    let before = text.get(..selected.start)?;
    let after = text.get(selected.end..)?;
    let handle_char = |c: char| c.is_alphanumeric() || matches!(c, '-' | '_' | '.');
    if after.chars().next().is_some_and(handle_char) {
        return None;
    }
    let line = before.rsplit('\n').next()?;
    let at = line.rfind('@')?;
    let partial = &line[at + 1..];
    let valid = partial.chars().all(|c| handle_char(c) || c == ' ')
        && line[..at]
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace);
    if !valid {
        return None;
    }
    Some(MentionQuery {
        range: before.len() - line.len() + at..selected.end,
        partial: partial.into(),
    })
}
fn menu(document: &Document, selected: Range<usize>) -> Option<Menu> {
    let query = mention_query(&document.text, selected)?;
    let overlaps = document
        .mentions
        .iter()
        .any(|(span, _)| query.range.start < span.end && query.range.end > span.start);
    if overlaps {
        return None;
    }
    let same_word = document.menu.partial == query.partial;
    if same_word && document.menu.dismissed {
        return None;
    }
    let needle = query.partial.to_ascii_lowercase();
    let matches = document
        .handles
        .iter()
        .filter(|choice| choice.label.to_ascii_lowercase().starts_with(&needle))
        .cloned()
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return None;
    }
    let selected = if same_word {
        document.menu.selected.min(matches.len() - 1)
    } else {
        0
    };
    Some(Menu {
        query,
        matches,
        selected,
    })
}
fn mention_body(text: &str, mentions: &[(Range<usize>, chat::Party)]) -> String {
    let mut body = String::new();
    let mut start = 0;
    for (range, party) in mentions {
        body.push_str(&text[start..range.start]);
        body.push_str(&chat::client::mention_token(party));
        start = range.end;
    }
    body.push_str(&text[start..]);
    body
}
fn expand_selection(
    mut selected: Range<usize>,
    mentions: &[(Range<usize>, chat::Party)],
    key: Option<&str>,
) -> Range<usize> {
    for (range, _) in mentions {
        let overlaps = selected.start < range.end && selected.end > range.start;
        let inside =
            selected.is_empty() && selected.start > range.start && selected.start < range.end;
        let boundary_delete = selected.is_empty()
            && ((key == Some("backspace") && selected.start == range.end)
                || (key == Some("delete") && selected.start == range.start));
        if overlaps || inside || boundary_delete {
            selected.start = selected.start.min(range.start);
            selected.end = selected.end.max(range.end);
        }
    }
    selected
}
fn replace_ranges(mentions: &mut Mentions, replaced: Range<usize>, inserted: usize) {
    mentions.retain_mut(|(range, _)| {
        if range.end <= replaced.start {
            return true;
        }
        if range.start >= replaced.end {
            range.start = replaced.start + inserted + (range.start - replaced.end);
            range.end = replaced.start + inserted + (range.end - replaced.end);
            return true;
        }
        false
    });
}
fn reconcile_mentions(
    before: &str,
    after: &str,
    mentions: &mut Mentions,
    anchor: Option<Range<usize>>,
) {
    if let Some(range) = anchor {
        let retained = before.len() - range.len();
        if after.len() >= retained {
            let inserted = after.len() - retained;
            let matches = before.get(..range.start) == after.get(..range.start)
                && before.get(range.end..) == after.get(range.start + inserted..);
            if matches {
                replace_ranges(mentions, range, inserted);
                return;
            }
        }
    }
    let same_lines = before.split_inclusive('\n').count() == after.split_inclusive('\n').count();
    if same_lines {
        let mut offset = 0;
        for (old, new) in before
            .split_inclusive('\n')
            .zip(after.split_inclusive('\n'))
        {
            let (range, inserted) = text_change(old, new);
            replace_ranges(mentions, offset + range.start..offset + range.end, inserted);
            offset += new.len();
        }
        return;
    }
    let (range, inserted) = text_change(before, after);
    replace_ranges(mentions, range, inserted);
}
fn text_change(before: &str, after: &str) -> (Range<usize>, usize) {
    let prefix = before
        .chars()
        .zip(after.chars())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum::<usize>();
    let suffix = before[prefix..]
        .chars()
        .rev()
        .zip(after[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum::<usize>();
    (prefix..before.len() - suffix, after.len() - prefix - suffix)
}

fn submit_document(
    document: &mut Document,
    scope: &str,
    kind: &str,
    blocked: bool,
) -> Option<Value> {
    if blocked {
        return None;
    }
    let body = mention_body(&document.text, &document.mentions)
        .trim()
        .to_owned();
    if body.is_empty() {
        return None;
    }
    if !matches!(kind, "edit" | "thread_edit") {
        document.text.clear();
        document.mentions.clear();
        document.menu = MenuState::default();
    }
    let prefix = if kind == "reply" { "reply" } else { "message" };
    Some(Value::Record {
        name: "composer".into(),
        fields: vec![
            ("scope".into(), Value::Str(scope.into())),
            ("kind".into(), Value::Str(kind.into())),
            ("body".into(), Value::Str(body)),
            (
                "id".into(),
                Value::Str(crate::backend::fresh_operation_id(prefix.into())),
            ),
        ],
    })
}
fn restore_failed(document: &mut Document, blocked: bool) -> bool {
    let unavailable = blocked || document.failed.is_empty() || !document.text.trim().is_empty();
    if unavailable {
        return false;
    }
    let body = std::mem::take(&mut document.failed);
    restore_document(document, &body);
    true
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    pub fn append(scope: &str, text: &str) {
        let shared = slot(scope);
        let mut slot = lock(&shared);
        slot.document.text.push_str(text);
        slot.rev += 1;
    }
    pub fn replace(scope: &str, text: &str) {
        let shared = slot(scope);
        let mut slot = lock(&shared);
        slot.document.text = text.to_owned();
        slot.document.mentions.clear();
        slot.document.menu = MenuState::default();
        slot.rev += 1;
    }
    pub fn submit(scope: &str, kind: &str, blocked: bool) -> Option<Value> {
        let shared = slot(scope);
        let mut slot = lock(&shared);
        let value = submit_document(&mut slot.document, scope, kind, blocked);
        slot.rev += 1;
        value
    }
    pub fn restore(scope: &str, blocked: bool) {
        let shared = slot(scope);
        let mut slot = lock(&shared);
        restore_failed(&mut slot.document, blocked);
        slot.rev += 1;
    }
    pub fn text(scope: &str) -> String {
        lock(&slot(scope)).document.text.clone()
    }
    pub fn failed(scope: &str) -> String {
        lock(&slot(scope)).document.failed.clone()
    }
    pub fn roster_of(scope: &str) -> Vec<String> {
        ROSTERS.with_borrow(|rosters| {
            rosters
                .by_room
                .get(scope)
                .map(|members| members.iter().map(|member| member.label.clone()).collect())
                .unwrap_or_default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mention_identity_survives_multibyte_edits_and_partial_copy() {
        let party = chat::Party::Account(5);
        let mut mentions = vec![(3..9, party.clone())];
        reconcile_mentions("한@Alice hi", "한글@Alice hi", &mut mentions, Some(3..3));
        assert_eq!(mentions, vec![(6..12, party.clone())]);
        assert_eq!(expand_selection(8..9, &mentions, None), 6..12);
        assert_eq!(
            mention_body("한글@Alice hi", &mentions),
            format!("한글{} hi", chat::client::mention_token(&party))
        );
        assert_eq!(
            expand_selection(12..12, &mentions, Some("backspace")),
            6..12
        );
    }
    #[test]
    fn replacing_one_of_repeated_labels_keeps_the_other_identity() {
        let mut mentions = vec![
            (0..6, chat::Party::Account(5)),
            (7..13, chat::Party::Account(6)),
        ];
        reconcile_mentions("@Alice @Alice", "@Alice ", &mut mentions, Some(7..13));
        assert_eq!(mentions, vec![(0..6, chat::Party::Account(5))]);
    }
    #[test]
    fn indenting_selected_lines_keeps_each_mention_identity() {
        let mut mentions = vec![
            (0..6, chat::Party::Account(5)),
            (7..13, chat::Party::Account(6)),
        ];
        reconcile_mentions("@Alice\n@Alice", "  @Alice\n  @Alice", &mut mentions, None);
        assert_eq!(
            mentions,
            vec![
                (2..8, chat::Party::Account(5)),
                (11..17, chat::Party::Account(6))
            ]
        );
    }
    #[test]
    fn room_drafts_and_failed_bodies_remain_scope_local() {
        seed("native-first", "draft");
        seed("native-second", "other");
        unsent("native-first", "failed", false);
        assert_eq!(lock(&slot("native-first")).document.text, "draft");
        assert_eq!(lock(&slot("native-first")).document.failed, "failed");
        assert_eq!(lock(&slot("native-second")).document.text, "other");
        unsent("native-second", "committed", true);
        assert!(lock(&slot("native-second")).document.failed.is_empty());
    }
    #[test]
    fn mention_query_requires_a_complete_word_and_never_promotes_plain_text() {
        assert!(mention_query("mail@alice", 10..10).is_none());
        assert!(mention_query("@alice", 3..3).is_none());
        assert_eq!(mention_query("hi\n@al", 6..6).unwrap().range, 3..6);
        assert_eq!(mention_body("@alice", &[]), "@alice");
    }
}
