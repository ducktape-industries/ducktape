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
use gpui_kit::component::{ActiveTheme, Disableable, Sizable as _};
use gpui_kit::{
    App, AppContext, ClipboardEntry, ClipboardItem, Context, Entity, EventEmitter, FontWeight,
    HighlightStyle,
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
    /// The standby zone: files attached to the next send, each on its way
    /// into DuckFS or already there under a `duck://` address. The local
    /// path never crosses the wire; the address does.
    attachments: Vec<Attachment>,
    /// Why the last attach was refused; empty once one succeeds.
    attach_note: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    /// The upload's own id — also the directory the file lands in.
    pub id: String,
    pub path: String,
    pub name: String,
    pub bytes: u64,
    pub state: AttachState,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachState {
    Uploading,
    Ready { uri: String },
    Failed { reason: String },
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

thread_local! {
    /// What a submitted send takes with it, keyed by the operation id the
    /// submit minted: the app collects it when it runs the send, so the
    /// intent wire stays the four fields it is.
    static SENT_ATTACHMENTS: RefCell<HashMap<String, Vec<Attachment>>> = RefCell::default();
}

/// The files a submit with this operation id sent — every one of them
/// already in DuckFS — handed over once, as (name, `duck://` address).
pub fn take_attachments(operation_id: &str) -> Vec<(String, String)> {
    SENT_ATTACHMENTS
        .with_borrow_mut(|sent| sent.remove(operation_id).unwrap_or_default())
        .into_iter()
        .filter_map(|attachment| match attachment.state {
            AttachState::Ready { uri } => Some((attachment.name, uri)),
            AttachState::Uploading | AttachState::Failed { .. } => None,
        })
        .collect()
}

/// Put local files into the standby zone of the composer under `scope`,
/// each with a fresh upload id; the app starts the uploads. A path that is
/// not a readable file is refused with the reason; a path already there
/// is not queued twice.
pub fn attach(scope: &str, paths: &[String]) -> Result<Vec<Attachment>, String> {
    let shared = slot(scope);
    let mut slot = lock(&shared);
    let mut queued = Vec::new();
    for path in paths {
        let attachment = attachment_of(path)?;
        let already = slot.document.attachments.iter().any(|a| a.path == *path);
        if already {
            continue;
        }
        slot.document.attachments.push(attachment.clone());
        slot.rev += 1;
        queued.push(attachment);
    }
    slot.document.attach_note.clear();
    Ok(queued)
}

/// The app's word on an upload: where the file landed, or why it did not.
pub fn attached(scope: &str, id: &str, outcome: Result<String, String>) {
    let shared = slot(scope);
    let mut slot = lock(&shared);
    let Some(attachment) = slot.document.attachments.iter_mut().find(|a| a.id == id) else {
        return;
    };
    attachment.state = match outcome {
        Ok(uri) => AttachState::Ready { uri },
        Err(reason) => AttachState::Failed { reason },
    };
    slot.rev += 1;
}

/// Put a failed upload back on its way, under a fresh id.
fn retry(document: &mut Document, index: usize) -> Option<Attachment> {
    let attachment = document.attachments.get_mut(index)?;
    attachment.id = crate::backend::fresh_operation_id("attach".into());
    attachment.state = AttachState::Uploading;
    Some(attachment.clone())
}

fn attachment_of(path: &str) -> Result<Attachment, String> {
    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("{path} has no file name"))?
        .to_owned();
    let meta = std::fs::metadata(path).map_err(|error| format!("cannot read {name}: {error}"))?;
    if !meta.is_file() {
        return Err(format!("{name} is not a file"));
    }
    Ok(Attachment {
        id: crate::backend::fresh_operation_id("attach".into()),
        path: path.to_owned(),
        name,
        bytes: meta.len(),
        state: AttachState::Uploading,
    })
}

/// What a paste attaches: a copied picture (its bytes and extension) or a
/// copied file (its path). Text on the clipboard is not an attachment.
enum Pasted {
    Picture { extension: &'static str, bytes: Vec<u8> },
    File(String),
}

fn pasted_attachments(item: &ClipboardItem) -> Vec<Pasted> {
    let mut pasted = Vec::new();
    for entry in item.entries() {
        match entry {
            ClipboardEntry::Image(image) => pasted.push(Pasted::Picture {
                extension: image.format().extension(),
                bytes: image.bytes().to_vec(),
            }),
            ClipboardEntry::ExternalPaths(paths) => pasted.extend(
                paths
                    .paths()
                    .iter()
                    .filter_map(|path| path.to_str().map(str::to_owned))
                    .map(Pasted::File),
            ),
            ClipboardEntry::String(_) => {}
        }
    }
    pasted
}

/// A pasted picture becomes a file the upload can read: it is written under
/// the cache directory, named by the moment it was pasted.
fn pasted_paths(pasted: Vec<Pasted>) -> Result<Vec<String>, String> {
    let dir = crate::backend::cache_dir()?.join("pasted");
    pasted
        .into_iter()
        .map(|item| match item {
            Pasted::File(path) => Ok(path),
            Pasted::Picture { extension, bytes } => park_pasted_picture(&dir, extension, &bytes),
        })
        .collect()
}

fn park_pasted_picture(
    dir: &std::path::Path,
    extension: &str,
    bytes: &[u8],
) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|error| format!("cannot keep the pasted picture: {error}"))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("pasted-{stamp}.{extension}"));
    std::fs::write(&path, bytes).map_err(|error| format!("cannot keep the pasted picture: {error}"))?;
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| "the cache path is not unicode".to_owned())
}

/// The event the app runs an upload on: the composer's scope, the upload
/// id, and the local path.
fn attach_event(scope: &str, attachment: &Attachment) -> Value {
    Value::Record {
        name: "composer_attach".into(),
        fields: vec![
            ("scope".into(), Value::Str(scope.into())),
            ("id".into(), Value::Str(attachment.id.clone())),
            ("path".into(), Value::Str(attachment.path.clone())),
        ],
    }
}

/// "12 B", "3.4 KB", "1.2 MB": a size a chip can carry.
pub fn attachment_size(bytes: u64) -> String {
    const KB: f64 = 1024.;
    let bytes_f = bytes as f64;
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    if bytes_f < KB * KB {
        return format!("{:.1} KB", bytes_f / KB);
    }
    format!("{:.1} MB", bytes_f / (KB * KB))
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
    let field = |wanted: &str| {
        fields.iter().find_map(|(key, value)| match value {
            Value::Str(text) if key == wanted => Some(text.as_str()),
            _ => None,
        })
    };
    let detail = match name.as_str() {
        "composer" => serde_json::json!({
            "scope": field("scope")?,
            "kind": field("kind")?,
            "body": field("body")?,
            "id": field("id")?,
        }),
        "composer_attach" => serde_json::json!({
            "scope": field("scope")?,
            "id": field("id")?,
            "path": field("path")?,
        }),
        _ => return None,
    };
    Some(crate::module_view::ModuleViewEvent {
        kind: name.clone(),
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
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        // "Content" attaches like a file: a copied screenshot or a file list
        // on the clipboard goes into the standby zone, not the text.
        let pasted = pasted_attachments(&item);
        if self.attaches() && !pasted.is_empty() {
            match pasted_paths(pasted) {
                Ok(paths) => self.attach_paths(paths, cx),
                Err(note) => {
                    lock(&self.shared).document.attach_note = note;
                    cx.notify();
                }
            }
            return;
        }
        let Some(body) = item.text() else {
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
    /// A send may carry files; an edit rewrites a body and never does.
    fn attaches(&self) -> bool {
        !matches!(self.args.kind.as_str(), "edit" | "thread_edit")
    }
    fn attach_paths(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        match attach(&self.args.scope, &paths) {
            Ok(queued) => {
                for attachment in queued {
                    cx.emit(attach_event(&self.args.scope, &attachment));
                }
            }
            Err(note) => lock(&self.shared).document.attach_note = note,
        }
        cx.notify();
    }
    fn detach(&mut self, index: usize, cx: &mut Context<Self>) {
        let mut slot = lock(&self.shared);
        if index < slot.document.attachments.len() {
            slot.document.attachments.remove(index);
            slot.document.attach_note.clear();
            slot.rev += 1;
        }
        drop(slot);
        cx.notify();
    }
    fn retry_attach(&mut self, index: usize, cx: &mut Context<Self>) {
        let retried = {
            let mut slot = lock(&self.shared);
            let retried = retry(&mut slot.document, index);
            slot.rev += 1;
            retried
        };
        if let Some(attachment) = retried {
            cx.emit(attach_event(&self.args.scope, &attachment));
        }
        cx.notify();
    }
    fn prompt_attach(&mut self, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn(async move |this, cx| {
            let paths = match chosen.await {
                Ok(Ok(Some(paths))) => paths,
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => {
                    let _ = this.update(cx, |this, cx| {
                        lock(&this.shared).document.attach_note =
                            format!("The file picker could not open: {error}");
                        cx.notify();
                    });
                    return;
                }
            };
            let paths: Vec<String> = paths
                .iter()
                .filter_map(|path| path.to_str().map(str::to_owned))
                .collect();
            let _ = this.update(cx, |this, cx| this.attach_paths(paths, cx));
        })
        .detach();
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
        let (empty, failed, lines, attachments, attach_note) = {
            let slot = lock(&self.shared);
            (
                slot.document.text.trim().is_empty(),
                !slot.document.failed.is_empty(),
                slot.document.text.lines().count().max(1),
                slot.document.attachments.clone(),
                slot.document.attach_note.clone(),
            )
        };
        let uploading = attachments
            .iter()
            .any(|attachment| attachment.state == AttachState::Uploading);
        let sends_files = self.attaches()
            && attachments
                .iter()
                .any(|attachment| matches!(attachment.state, AttachState::Ready { .. }));
        let empty = empty && !sends_files;
        let mut content = div()
            .id("composer")
            .flex()
            .flex_col()
            .w_full()
            .gap(px(8.))
            .on_drop(cx.listener(|this, paths: &gpui_kit::ExternalPaths, _, cx| {
                cx.stop_propagation();
                if !this.attaches() {
                    return;
                }
                let paths: Vec<String> = paths
                    .paths()
                    .iter()
                    .filter_map(|path| path.to_str().map(str::to_owned))
                    .collect();
                this.attach_paths(paths, cx);
            }))
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
        let drop_ring = cx.theme().primary;
        let mut plate = div()
            .flex()
            .flex_col()
            .w_full()
            .p(px(if self.args.compact { 6. } else { 8. }))
            .border_1()
            .border_color(cx.theme().border)
            // the standby zone lights up while a file is dragged over it
            .drag_over::<gpui_kit::ExternalPaths>(move |style, _, _, _| {
                style.border_color(drop_ring)
            })
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
        if !attachments.is_empty() {
            let mut tray = div()
                .id("attachments")
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(6.))
                .pb(px(6.));
            for (index, attachment) in attachments.into_iter().enumerate() {
                // what the chip says under the name: on its way, where it
                // landed, or why it did not
                let (note, note_color, failed) = match &attachment.state {
                    AttachState::Uploading => (
                        format!("Uploading · {}", attachment_size(attachment.bytes)),
                        cx.theme().muted_foreground,
                        false,
                    ),
                    AttachState::Ready { uri } => (uri.clone(), cx.theme().muted_foreground, false),
                    AttachState::Failed { reason } => (reason.clone(), cx.theme().danger, true),
                };
                let mut chip = div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .pl(px(10.))
                    .pr(px(4.))
                    .py(px(4.))
                    .rounded(px(design::radius::CARD as f32))
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().muted)
                    .text_size(px(12.5))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .max_w(px(320.))
                            .min_w_0()
                            .child(
                                div()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(attachment.name.clone()),
                            )
                            .child(
                                div()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .text_size(px(11.))
                                    .text_color(note_color)
                                    .child(note),
                            ),
                    );
                if failed {
                    chip = chip.child(
                        Button::new(("retry-attach", index))
                            .label("Retry")
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.retry_attach(index, cx)
                            })),
                    );
                }
                chip = chip.child(
                    Button::new(("detach", index))
                        .label("×")
                        .ghost()
                        .xsmall()
                        .accessibility_label(format!("Remove {}", attachment.name))
                        .on_click(cx.listener(move |this, _, _, cx| this.detach(index, cx))),
                );
                tray = tray.child(chip);
            }
            plate = plate.child(tray);
        }
        if !attach_note.is_empty() {
            plate = plate.child(
                div()
                    .pb(px(6.))
                    .text_size(px(12.))
                    .text_color(cx.theme().danger)
                    .child(attach_note),
            );
        }
        let mut toolbar = div().flex().items_center().gap(px(4.));
        if self.attaches() {
            toolbar = toolbar.child(
                Button::new("attach")
                    .label("+")
                    .ghost()
                    .accessibility_label("Attach a file")
                    .disabled(self.args.blocked)
                    .on_click(cx.listener(|this, _, _, cx| this.prompt_attach(cx))),
            );
        }
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
                // a send waits for its files to land
                .disabled(self.args.blocked || empty || uploading)
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
    // an edit rewrites a body; a send may be files alone, and waits for
    // every file still on its way
    let is_edit = matches!(kind, "edit" | "thread_edit");
    let landed = document
        .attachments
        .iter()
        .filter(|attachment| matches!(attachment.state, AttachState::Ready { .. }))
        .count();
    let uploading = document
        .attachments
        .iter()
        .any(|attachment| attachment.state == AttachState::Uploading);
    let sends_files = !is_edit && landed > 0;
    if (body.is_empty() && !sends_files) || (!is_edit && uploading) {
        return None;
    }
    let prefix = if kind == "reply" { "reply" } else { "message" };
    let id = crate::backend::fresh_operation_id(prefix.into());
    if !matches!(kind, "edit" | "thread_edit") {
        document.text.clear();
        document.mentions.clear();
        document.menu = MenuState::default();
        let attachments = std::mem::take(&mut document.attachments);
        if !attachments.is_empty() {
            SENT_ATTACHMENTS.with_borrow_mut(|sent| sent.insert(id.clone(), attachments));
        }
    }
    Some(Value::Record {
        name: "composer".into(),
        fields: vec![
            ("scope".into(), Value::Str(scope.into())),
            ("kind".into(), Value::Str(kind.into())),
            ("body".into(), Value::Str(body)),
            ("id".into(), Value::Str(id)),
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

    /// A pasted picture lands as a readable file named by its format; text on
    /// the clipboard attaches nothing.
    #[test]
    fn a_pasted_picture_becomes_a_file_and_text_attaches_nothing() {
        let dir = std::env::temp_dir().join(format!("composer-paste-{}", std::process::id()));
        let path = park_pasted_picture(&dir, "png", b"not really a png").expect("parked");
        assert!(path.ends_with(".png"));
        assert_eq!(std::fs::read(&path).expect("readable"), b"not really a png");
        std::fs::remove_dir_all(&dir).expect("cleanup");
        assert!(pasted_attachments(&ClipboardItem::new_string("hello".into())).is_empty());
    }

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
    fn a_send_may_be_files_alone_and_hands_them_over_by_operation_id() {
        let file = std::env::temp_dir().join("composer-attach-test.txt");
        std::fs::write(&file, b"hello").unwrap();
        let path = file.to_string_lossy().into_owned();
        let queued = attach("native-files", &[path.clone(), path.clone()]).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].bytes, 5);
        assert_eq!(lock(&slot("native-files")).document.attachments.len(), 1);
        assert!(attach("native-files", &[std::env::temp_dir().to_string_lossy().into_owned()]).is_err());
        // a send waits for the upload; an edit never takes files
        assert!(testing::submit("native-files", "message", false).is_none());
        attached("native-files", &queued[0].id, Ok("duck://files/shared/attachments/a/x".into()));
        assert!(testing::submit("native-files", "edit", false).is_none());
        // with nothing typed the send still goes, files alone
        let sent = testing::submit("native-files", "message", false).expect("files alone send");
        let Value::Record { fields, .. } = sent else {
            panic!("not a record")
        };
        let Value::Str(id) = &fields[3].1 else {
            panic!("no id")
        };
        let taken = take_attachments(id);
        assert_eq!(
            taken,
            vec![(
                "composer-attach-test.txt".to_owned(),
                "duck://files/shared/attachments/a/x".to_owned()
            )]
        );
        assert!(take_attachments(id).is_empty());
        // a failed upload is retried under a fresh id, and never sent
        let queued = attach("native-files", std::slice::from_ref(&path)).unwrap();
        attached("native-files", &queued[0].id, Err("no room".into()));
        let retried = retry(&mut lock(&slot("native-files")).document, 0).unwrap();
        assert_ne!(retried.id, queued[0].id);
        assert_eq!(retried.state, AttachState::Uploading);
        lock(&slot("native-files")).document.attachments.clear();
        assert!(lock(&slot("native-files")).document.attachments.is_empty());
        assert_eq!(attachment_size(5), "5 B");
        assert_eq!(attachment_size(1536), "1.5 KB");
        assert_eq!(attachment_size(3 * 1024 * 1024), "3.0 MB");
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
