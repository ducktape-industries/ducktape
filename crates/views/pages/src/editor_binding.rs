//! Pages' structural keys and commit-confirmed history on the guest editor lane.
//! Rich document painting, gutter actions and anchored menus are independent
//! consumers; this adapter does not replace them with a plain editor.

use crate::editor::{self, Doc, History};
use ducktape_view_guest::wire::{self, EditorDecision, EditorHistoryEffect, EditorKeyClaim};
use ducktape_view_guest::{EditorBinding, EditorKeyRequest, EditorTransactionEvent};
use std::{cell::RefCell, rc::Rc};
use wire::keyboard::{Key, Modifiers, Named};

/// Guest-owned editor history retained in state snapshots.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HistoryState {
    pub snapshot: Vec<u8>,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct StoredHistory {
    reset: Option<u64>,
    history: History,
}

impl StoredHistory {
    fn at_reset(&mut self, reset: u64) {
        if self.reset != Some(reset) {
            self.history.reset();
            self.reset = Some(reset);
        }
    }

    fn save(&self) -> HistoryState {
        HistoryState {
            snapshot: wire::encode(self),
        }
    }
}

pub fn initial_history() -> HistoryState {
    HistoryState::default()
}

/// Small menu state is separate from the bounded undo snapshots so painting
/// never needs to decode the document history.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MenuState {
    pub snapshot: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EditorUpdate {
    pub notice: String,
    pub history: HistoryState,
    pub menu: MenuState,
    /// The accepted canonical document, without copying its text into an intent.
    pub reference: Vec<u8>,
    /// Read-only presentation actions (link and comment targets) are not edits.
    pub interaction: Vec<u8>,
}

pub fn initial_menu() -> MenuState {
    MenuState::default()
}

/// The menu part of Pages presentation; the Markdown pass adds its spans and
/// document affordances to this same declarative value.
pub fn menu_paint(
    state: ducktape_view_guest::EditorStateView<'_>,
    menu: MenuState,
) -> wire::editor_presentation::EditorPresentation {
    use wire::editor_presentation::{
        EditorMenu, EditorMenuAnchor, EditorMenuItem, EditorPresentation,
    };
    let menu: crate::editor_menu::Menu = if menu.snapshot.is_empty() {
        Default::default()
    } else {
        wire::decode(&menu.snapshot).expect("Pages menu snapshot")
    };
    let mut presentation = EditorPresentation::default();
    if !menu.is_open() {
        return presentation;
    }
    presentation.affordances.menu = menu
        .current(&document(state.text, state.cursor))
        .map(|view| EditorMenu {
            anchor: view.line.map_or(EditorMenuAnchor::Caret, |line| {
                EditorMenuAnchor::Line(line as u32)
            }),
            items: view
                .items
                .into_iter()
                .map(|(tag, label)| EditorMenuItem { tag, label })
                .collect(),
            selected: view.selected as u32,
        });
    presentation
}

struct BindingState {
    history: StoredHistory,
    menu: crate::editor_menu::Menu,
}
impl BindingState {
    fn update(
        &self,
        id: &wire::EditorTransactionId,
        state: ducktape_view_guest::EditorStateView<'_>,
        interaction: Vec<u8>,
    ) -> EditorUpdate {
        let reference = wire::editor_document::EditorDocumentRef {
            document: id.document.clone(),
            reset: state.reset,
            revision: state.revision,
            text_revision: state.text_revision,
            byte_len: state.text.len() as u32,
            cursor: state.cursor,
        };
        EditorUpdate {
            notice: crate::presentation::format_notice(state.text),
            history: self.history.save(),
            menu: MenuState {
                snapshot: wire::encode(&self.menu),
            },
            reference: wire::encode(&reference),
            interaction,
        }
    }

    fn observe(&mut self, event: EditorTransactionEvent<'_>) -> Option<EditorUpdate> {
        match event {
            event @ EditorTransactionEvent::Commit { .. } => self.committed(event),
            EditorTransactionEvent::Interaction {
                id, state, action, ..
            } => self.interacted(id, state, action),
            EditorTransactionEvent::Fault { .. } => None,
            EditorTransactionEvent::Cancelled { .. } => None,
        }
    }

    fn interacted(
        &mut self,
        id: &wire::EditorTransactionId,
        state: ducktape_view_guest::EditorStateView<'_>,
        action: &wire::editor_presentation::EditorInteraction,
    ) -> Option<EditorUpdate> {
        if self.history.reset != Some(state.reset) {
            self.menu.close();
        }
        self.history.at_reset(state.reset);
        let doc = document(state.text, state.cursor);
        // What the pick asks the host for is read off the menu it was picked
        // from, before that menu closes.
        let mut navigation = match action {
            wire::editor_presentation::EditorInteraction::MenuPick { tag } => {
                self.menu.intent(&doc, tag)
            }
            _ => crate::document_sync::Navigation::default(),
        };
        let (_, successor) = interaction(&doc, self.menu.clone(), action);
        self.menu = successor;
        // A margin press with the selection on that line comments on the
        // selected words (the host editor's "Comment" on a selection); with
        // no selection it opens the block's threads.
        if let wire::editor_presentation::EditorInteraction::Margin { line } = action {
            navigation.comment_line = Some(*line);
            navigation.anchor = crate::document_sync::text_anchor(
                doc.line(*line as usize).unwrap_or_default(),
                crate::editor_menu::selection_columns(&doc, *line as usize),
            );
        }
        Some(self.update(id, state, wire::encode(&navigation)))
    }

    fn committed(&mut self, event: EditorTransactionEvent<'_>) -> Option<EditorUpdate> {
        let EditorTransactionEvent::Commit {
            id,
            before,
            after,
            kind,
            history: effect,
            input_time_ms,
            origin,
        } = event
        else {
            unreachable!("observe routes only Commit here")
        };
        let replaced = self.history.reset != Some(before.reset);
        self.history.at_reset(before.reset);
        if replaced {
            self.menu.close();
        }
        let after_doc = document(after.text, after.cursor);
        if let Some(wire::EditorRequestInput::Interaction { action }) = origin {
            let (_, successor) = interaction(
                &document(before.text, before.cursor),
                self.menu.clone(),
                action,
            );
            self.menu = successor;
        } else if opens_format_menu(origin) {
            self.menu.format(&after_doc);
        } else if matches!(kind, wire::EditorEditKind::Cursor) {
            self.menu.moved(&after_doc);
        } else {
            let typed = matches!(kind, wire::EditorEditKind::Insert)
                && after.text_revision != before.text_revision;
            let trigger = after_doc
                .line(after.cursor.position.line as usize)
                .and_then(|line| line.get(..after.cursor.position.column as usize))
                .and_then(|prefix| prefix.chars().next_back())
                .filter(|last| typed && TRIGGERS.contains(last));
            self.menu.after_edit(&after_doc, trigger);
        }
        let changes_history = before.text_revision != after.text_revision
            || matches!(
                effect,
                EditorHistoryEffect::Undo | EditorHistoryEffect::Redo
            );
        if changes_history {
            self.history.history.commit(
                &document(before.text, before.cursor),
                &after_doc,
                history_effect(effect),
                input_time_ms,
            );
        }
        Some(self.update(id, after, Vec::new()))
    }
}

/// The characters that open a picker when typed: the slash palette, a
/// member mention, an emoji short code.
const TRIGGERS: &[char] = &['/', '@', ':'];

/// The command-key letters the binding claims besides undo/redo, with the
/// shift and alt they need — Tiptap's own bindings. Bold, italic, underline,
/// code, link, the format menu; strike and highlight on shift; the block
/// turns on shift (quote, the lists) and on alt (text, headings, code); the
/// alignment on shift. Both cases of a letter are claimed — a shifted letter
/// arrives as its capital on some platforms.
const COMMAND_KEYS: &[(&str, bool, bool)] = &[
    ("b", false, false),
    ("i", false, false),
    ("u", false, false),
    ("e", false, false),
    ("k", false, false),
    ("/", false, false),
    ("x", true, false),
    ("X", true, false),
    ("h", true, false),
    ("H", true, false),
    ("b", true, false),
    ("B", true, false),
    ("7", true, false),
    ("8", true, false),
    ("9", true, false),
    ("l", true, false),
    ("L", true, false),
    ("e", true, false),
    ("E", true, false),
    ("r", true, false),
    ("R", true, false),
    ("0", false, true),
    ("1", false, true),
    ("2", false, true),
    ("3", false, true),
    ("c", false, true),
];

/// The block a `Cmd+Shift` / `Cmd+Alt` key turns the caret's line into.
const TURN_KEYS: &[(&str, bool, &str)] = &[
    ("b", true, "quote"),
    ("7", true, "number"),
    ("8", true, "bullet"),
    ("9", true, "todo"),
    ("0", false, "text"),
    ("1", false, "h1"),
    ("2", false, "h2"),
    ("3", false, "h3"),
    ("c", false, "code"),
];

/// `Cmd+/` — the key that opens the floating format menu. It edits nothing,
/// so the host commits it as an empty step and the menu opens on that.
fn opens_format_menu(origin: Option<&wire::EditorRequestInput>) -> bool {
    let Some(wire::EditorRequestInput::Key { key, .. }) = origin else {
        return false;
    };
    let command = key.modifiers.logo || key.modifiers.control;
    command && matches!(&key.key, Key::Character(c) if c == "/")
}

/// Claims the structural keys, the platform command's undo/redo keys and the
/// inline formatting shortcuts. All other input stays native and joins this
/// history only after commit.
pub fn keys(
    state: HistoryState,
    menu: MenuState,
    names: Vec<String>,
    agents: Vec<(String, u64)>,
) -> EditorBinding<EditorUpdate> {
    let history = if state.snapshot.is_empty() {
        StoredHistory::default()
    } else {
        wire::decode(&state.snapshot).expect("Pages history snapshot")
    };
    let menu = if menu.snapshot.is_empty() {
        crate::editor_menu::Menu::default()
    } else {
        wire::decode(&menu.snapshot).expect("Pages menu snapshot")
    };
    let menu = menu.with_names(&names).with_agents(&agents);
    let state = Rc::new(RefCell::new(BindingState { history, menu }));
    let deciding = state.clone();
    let interacting = state.clone();
    let bare = Modifiers::default();
    let mut claims = [Named::Enter, Named::Tab, Named::Backspace]
        .into_iter()
        .map(|key| EditorKeyClaim {
            key: Key::Named(key),
            modifiers: bare,
            command: false,
        })
        .collect::<Vec<_>>();
    claims.push(EditorKeyClaim {
        key: Key::Named(Named::Tab),
        modifiers: Modifiers {
            shift: true,
            ..bare
        },
        command: false,
    });
    for shift in [false, true] {
        claims.push(EditorKeyClaim {
            key: Key::Character("z".into()),
            modifiers: Modifiers { shift, ..bare },
            command: true,
        });
    }
    claims.extend(COMMAND_KEYS.iter().map(|(key, shift, alt)| EditorKeyClaim {
        key: Key::Character((*key).into()),
        modifiers: Modifiers {
            shift: *shift,
            alt: *alt,
            ..bare
        },
        command: true,
    }));
    EditorBinding::new(
        claims,
        move |request| {
            let state = deciding.borrow();
            if state.history.reset == Some(request.state.reset) {
                decide(request, &state.history.history)
            } else {
                decide(request, &History::default())
            }
        },
        move |event| state.borrow_mut().observe(event),
    )
    .on_interaction(move |request| {
        let state = interacting.borrow();
        let doc = document(request.state.text, request.state.cursor);
        let menu = if state.history.reset == Some(request.state.reset) {
            state.menu.clone()
        } else {
            crate::editor_menu::Menu::default()
        };
        let (decision, _) = interaction(&doc, menu, request.action);
        wire_decision(&doc, decision)
    })
}

fn interaction(
    doc: &Doc,
    mut menu: crate::editor_menu::Menu,
    action: &wire::editor_presentation::EditorInteraction,
) -> (editor::EditorDecision, crate::editor_menu::Menu) {
    use editor::EditorDecision::Noop;
    use wire::editor_presentation::{EditorGutterButton, EditorInteraction};
    match action {
        EditorInteraction::Gutter {
            line,
            button: EditorGutterButton::Plus,
        } => menu.plus(doc, *line as usize),
        EditorInteraction::Gutter {
            line,
            button: EditorGutterButton::Handle,
        } => {
            menu.block(doc, *line as usize);
            (Noop, menu)
        }
        EditorInteraction::GutterDrop { from, boundary } => {
            menu.close();
            (
                crate::editor_menu::drop_move(doc, *from as usize, *boundary as usize),
                menu,
            )
        }
        EditorInteraction::MenuPick { tag } => menu.pick(doc, tag),
        EditorInteraction::MenuSelect { index } => {
            menu.select(doc, *index as usize);
            (Noop, menu)
        }
        EditorInteraction::MenuDismiss => {
            menu.close();
            (Noop, menu)
        }
        EditorInteraction::LinePress { tag: 1, position } => {
            menu.close();
            (
                crate::editor_menu::toggle_todo(doc, position.line as usize),
                menu,
            )
        }
        // A pressed link opens its popover; navigating is one of its picks.
        EditorInteraction::LinePress { tag: 2, position } => {
            menu.link(doc, position.line as usize, position.column as usize);
            (Noop, menu)
        }
        EditorInteraction::LinePress { .. } | EditorInteraction::Margin { .. } => (Noop, menu),
    }
}

fn document(text: &str, cursor: wire::EditorCursor) -> Doc {
    Doc::new(
        text,
        editor::EditorCursor {
            position: position(cursor.position),
            selection: cursor.selection.map(position),
        },
    )
}
fn position(value: wire::EditorPosition) -> editor::EditorPosition {
    editor::EditorPosition {
        line: value.line,
        column: value.column,
    }
}
fn wire_position(value: editor::EditorPosition) -> wire::EditorPosition {
    wire::EditorPosition {
        line: value.line,
        column: value.column,
    }
}
fn history_effect(value: EditorHistoryEffect) -> editor::EditorHistoryEffect {
    match value {
        EditorHistoryEffect::Native => editor::EditorHistoryEffect::Native,
        EditorHistoryEffect::NewGroup => editor::EditorHistoryEffect::NewGroup,
        EditorHistoryEffect::ExtendPrevious => editor::EditorHistoryEffect::ExtendPrevious,
        EditorHistoryEffect::Undo => editor::EditorHistoryEffect::Undo,
        EditorHistoryEffect::Redo => editor::EditorHistoryEffect::Redo,
    }
}
fn wire_history(value: editor::EditorHistoryEffect) -> EditorHistoryEffect {
    match value {
        editor::EditorHistoryEffect::Native => EditorHistoryEffect::Native,
        editor::EditorHistoryEffect::NewGroup => EditorHistoryEffect::NewGroup,
        editor::EditorHistoryEffect::ExtendPrevious => EditorHistoryEffect::ExtendPrevious,
        editor::EditorHistoryEffect::Undo => EditorHistoryEffect::Undo,
        editor::EditorHistoryEffect::Redo => EditorHistoryEffect::Redo,
    }
}

fn decide(request: EditorKeyRequest<'_>, history: &History) -> EditorDecision {
    let doc = document(request.state.text, request.state.cursor);
    let shift = request.key.modifiers.shift;
    let alt = request.key.modifiers.alt;
    let decision = match &request.key.key {
        Key::Character(key) if key.eq_ignore_ascii_case("z") => if shift {
            history.redo(&doc)
        } else {
            history.undo(&doc)
        }
        .unwrap_or(editor::EditorDecision::Noop),
        Key::Character(key) if alt => turn_shortcut(&doc, key, shift),
        Key::Character(key) => shortcut(&doc, key, shift),
        Key::Named(key) => {
            let key = match key {
                Named::Enter => editor::Key::Enter,
                Named::Tab if request.key.modifiers.shift => editor::Key::ShiftTab,
                Named::Tab => editor::Key::Tab,
                Named::Backspace => editor::Key::Backspace,
                _ => return EditorDecision::DefaultEditorAction,
            };
            editor::decide(&doc, key, history, request.input_time_ms)
        }
        _ => return EditorDecision::DefaultEditorAction,
    };
    wire_decision(&doc, decision)
}

/// The command-key formatting shortcuts. `Cmd+/` edits nothing: its empty
/// commit is what opens the format menu.
fn shortcut(doc: &Doc, key: &str, shift: bool) -> editor::EditorDecision {
    use crate::format::{Wrap, align, link, toggle};
    use crate::markdown::Align;
    match (key.to_ascii_lowercase().as_str(), shift) {
        ("b", false) => toggle(doc, Wrap::Bold),
        ("i", false) => toggle(doc, Wrap::Italic),
        ("u", false) => toggle(doc, Wrap::Underline),
        ("e", false) => toggle(doc, Wrap::Code),
        ("x", true) => toggle(doc, Wrap::Strike),
        ("h", true) => toggle(doc, Wrap::Highlight),
        ("l", true) => align(doc, Align::Start),
        ("e", true) => align(doc, Align::Center),
        ("r", true) => align(doc, Align::End),
        ("k", false) => link(doc),
        ("/", false) => editor::EditorDecision::Noop,
        _ => turn_shortcut(doc, key, shift),
    }
}

/// The block-turn shortcuts, over the caret's line. The title turns into
/// nothing.
fn turn_shortcut(doc: &Doc, key: &str, shift: bool) -> editor::EditorDecision {
    let line = doc.cursor.position.line as usize;
    let lowered = key.to_ascii_lowercase();
    let turn = TURN_KEYS
        .iter()
        .find(|(turn_key, turn_shift, _)| *turn_key == lowered && *turn_shift == shift);
    match (line, turn) {
        (0, _) | (_, None) => editor::EditorDecision::DefaultEditorAction,
        (line, Some((_, _, tag))) => crate::editor_menu::turn(doc, line, tag),
    }
}

fn wire_decision(doc: &Doc, decision: editor::EditorDecision) -> EditorDecision {
    match decision {
        editor::EditorDecision::DefaultEditorAction => EditorDecision::DefaultEditorAction,
        editor::EditorDecision::Noop => EditorDecision::Noop,
        editor::EditorDecision::Apply {
            patches,
            cursor,
            history,
        } => {
            let after = editor::apply(doc, &patches, cursor);
            // Native replacement endpoints include entire graphemes and paired
            // newlines. A scalar-only diff of emoji history is not sufficient.
            let patches = wire::editor_document::editor_changed_span(&doc.text, &after.text)
                .unwrap_or_else(|_| {
                    patches
                        .into_iter()
                        .map(|p| wire::EditorPatch {
                            start_byte: p.start_byte,
                            end_byte: p.end_byte,
                            replacement: p.replacement,
                        })
                        .collect()
                }); // The host reports an oversized edit as a fault.
            EditorDecision::Apply {
                patches,
                cursor: wire::EditorCursor {
                    position: wire_position(cursor.position),
                    selection: cursor.selection.map(wire_position),
                },
                history: wire_history(history),
            }
        }
    }
}
