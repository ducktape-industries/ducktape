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
        match origin {
            Some(wire::EditorRequestInput::Interaction { action }) => {
                let (_, successor) = interaction(
                    &document(before.text, before.cursor),
                    self.menu.clone(),
                    action,
                );
                self.menu = successor;
            }
            Some(wire::EditorRequestInput::RichEdit { edit }) => {
                let native_edit = edit.action.is_empty() && edit.interaction.is_none();
                if native_edit {
                    let changed_text = before.text != after.text;
                    if changed_text {
                        self.menu
                            .after_edit(&after_doc, rich_trigger(before.text, &after_doc));
                    } else {
                        self.menu.moved(&after_doc);
                    }
                }
            }
            Some(wire::EditorRequestInput::Key { .. }) | None => {
                if opens_format_menu(origin) {
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
            }
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
        let navigation = match origin {
            Some(wire::EditorRequestInput::RichEdit { edit })
                if !edit.action.is_empty() || edit.interaction.is_some() =>
            {
                let Ok((text, cursor)) = crate::rich_document::document(edit) else {
                    return None;
                };
                let doc = rebase_rich(before, edit, document(&text, cursor))?;
                match &edit.interaction {
                    Some(wire::editor_presentation::EditorInteraction::Margin { .. }) => {
                        crate::document_sync::Navigation {
                            comment_line: Some(doc.cursor.position.line),
                            anchor: crate::document_sync::text_anchor(
                                doc.line(doc.cursor.position.line as usize)
                                    .unwrap_or_default(),
                                crate::editor_menu::selection_columns(
                                    &doc,
                                    doc.cursor.position.line as usize,
                                ),
                            ),
                            ..Default::default()
                        }
                    }
                    Some(action) => {
                        let navigation = match action {
                            wire::editor_presentation::EditorInteraction::MenuPick { tag } => {
                                self.menu.intent(&doc, tag)
                            }
                            _ => crate::document_sync::Navigation::default(),
                        };
                        self.menu = interaction(&doc, self.menu.clone(), action).1;
                        navigation
                    }
                    None => {
                        if !self.menu.is_open() {
                            self.menu.format(&doc);
                        }
                        let navigation = self.menu.intent(&doc, &edit.action);
                        self.menu = self.menu.pick(&doc, &edit.action).1;
                        navigation
                    }
                }
            }
            _ => crate::document_sync::Navigation::default(),
        };
        Some(self.update(id, after, wire::encode(&navigation)))
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
    let rich = state.clone();
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
    .on_rich_edit(move |request| {
        let state = rich.borrow();
        let menu = if state.history.reset == Some(request.state.reset) {
            state.menu.clone()
        } else {
            Default::default()
        };
        rich_decision(request.state, request.edit, menu)
    })
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
        EditorInteraction::Action { tag } => match tag.as_str() {
            "/" => menu.open_trigger(doc, '/'),
            "@" => menu.open_trigger(doc, '@'),
            ":" => menu.open_trigger(doc, ':'),
            _ => {
                menu.format(doc);
                menu.pick(doc, tag)
            }
        },
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

/// A single inserted trigger opens suggestions. Bulk edits, deletion, and
/// caret movement cannot turn existing source into a newly typed trigger.
fn rich_trigger(before: &str, after: &Doc) -> Option<char> {
    let patches = wire::editor_document::editor_changed_span(before, &after.text).ok()?;
    let [patch] = patches.as_slice() else {
        return None;
    };
    let inserted_at_caret = patch.start_byte == patch.end_byte
        && after.offset(after.cursor.position)
            == patch.start_byte as usize + patch.replacement.len();
    if !inserted_at_caret {
        return None;
    }
    let mut characters = patch.replacement.chars();
    let trigger = characters.next()?;
    let single_trigger = characters.next().is_none() && TRIGGERS.contains(&trigger);
    single_trigger.then_some(trigger)
}

fn rich_input_rule(
    before: &str,
    edit: &wire::editor_rich::RichEdit,
    doc: &Doc,
) -> editor::EditorDecision {
    let unchanged = before == doc.text;
    if unchanged {
        return editor::EditorDecision::Noop;
    }
    let code = edit
        .document
        .blocks
        .get(edit.document.cursor.position.line as usize)
        .is_some_and(|block| block.kind == "codeBlock");
    if code {
        return editor::EditorDecision::Noop;
    }
    crate::editor_menu::typography(doc)
}

/// A rich snapshot is still one canonical transaction, so draft, undo and
/// replacement lifetime follow the same commit acknowledgment as native text.
fn rich_decision(
    state: ducktape_view_guest::EditorStateView<'_>,
    edit: &wire::editor_rich::RichEdit,
    mut menu: crate::editor_menu::Menu,
) -> EditorDecision {
    let Ok((text, cursor)) = crate::rich_document::document(edit) else {
        return EditorDecision::Noop;
    };
    let incoming = document(&text, cursor);
    let Some(doc) = rebase_rich(state, edit, incoming) else {
        return EditorDecision::Noop;
    };
    let decision = match (&edit.interaction, edit.action.as_str()) {
        (Some(action), _) => interaction(&doc, menu, action).0,
        (None, "") => rich_input_rule(state.text, edit, &doc),
        (None, tag) => {
            if !menu.is_open() {
                menu.format(&doc);
            }
            menu.pick(&doc, tag).0
        }
    };
    let after = match decision {
        editor::EditorDecision::Apply {
            patches, cursor, ..
        } => editor::apply(&doc, &patches, cursor),
        editor::EditorDecision::Noop | editor::EditorDecision::DefaultEditorAction => doc,
    };
    let Ok(patches) = wire::editor_document::editor_changed_span(state.text, &after.text) else {
        return EditorDecision::Noop;
    };
    EditorDecision::Apply {
        patches,
        cursor: wire::EditorCursor {
            position: wire_position(after.cursor.position),
            selection: after.cursor.selection.map(wire_position),
        },
        history: EditorHistoryEffect::Native,
    }
}

/// A text insertion uses the source caret, which distinguishes the inside
/// and outside of a closing delimiter even when both paint at the same pixel.
fn rich_insertion(
    state: ducktape_view_guest::EditorStateView<'_>,
    edit: &wire::editor_rich::RichEdit,
    base: &wire::editor_rich::RichDocument,
) -> Option<Doc> {
    let command = edit.interaction.is_some() || !edit.action.is_empty();
    let selection = base.cursor.selection.is_some() || edit.document.cursor.selection.is_some();
    if command || selection {
        return None;
    }
    let index = base.cursor.position.line as usize;
    let at = base.cursor.position.column as usize;
    let before = base.blocks.get(index)?;
    let after = edit.document.blocks.get(index)?;
    let inserted = after
        .text
        .strip_prefix(before.text.get(..at)?)?
        .strip_suffix(before.text.get(at..)?)?;
    if inserted.is_empty() {
        return None;
    }
    let mut expected = base.clone();
    expected.blocks[index].text = after.text.clone();
    expected.blocks[index].marks = after.marks.clone();
    expected.cursor.position.column += u32::try_from(inserted.len()).ok()?;
    let only_insertion = expected == edit.document;
    if !only_insertion {
        return None;
    }
    let mut source = document(state.text, state.cursor);
    let at = source.offset(source.cursor.position);
    source.text.insert_str(at, inserted);
    source.cursor.position = source.position_at(at + inserted.len());
    Some(source)
}

fn rebase_rich(
    state: ducktape_view_guest::EditorStateView<'_>,
    edit: &wire::editor_rich::RichEdit,
    incoming: Doc,
) -> Option<Doc> {
    let Some(base) = &edit.before else {
        return Some(incoming);
    };
    let current_projection = crate::rich_document::presentation(state.text, state.cursor).document;
    let current_base = *base == current_projection;
    if current_base {
        return Some(rich_insertion(state, edit, base).unwrap_or(incoming));
    }
    let (text, cursor) = crate::rich_document::canonical(base).ok()?;
    if text == state.text {
        return Some(incoming);
    }
    let base = document(&text, cursor);
    let current = document(state.text, state.cursor);
    let before_caret = base.offset(base.cursor.position) as isize;
    let current_caret = current.offset(current.cursor.position) as isize;
    let patches = wire::editor_document::editor_changed_span(&base.text, &incoming.text).ok()?;
    let source = patches.into_iter().next().unwrap_or(wire::EditorPatch {
        start_byte: before_caret as u32,
        end_byte: before_caret as u32,
        replacement: String::new(),
    });
    let translated = |at: u32| u32::try_from(current_caret + at as isize - before_caret).ok();
    let patch = wire::EditorPatch {
        start_byte: translated(source.start_byte)?,
        end_byte: translated(source.end_byte)?,
        replacement: source.replacement,
    };
    let text = wire::patched_editor_text(
        state.text,
        std::slice::from_ref(&patch),
        wire::EditorCursor::default(),
    )
    .ok()?;
    let after = document(&text, Default::default());
    let mapped = |position| {
        let relative = incoming.offset(position) as isize - source.start_byte as isize;
        let at = usize::try_from(patch.start_byte as isize + relative).ok()?;
        if !after.text.is_char_boundary(at) {
            return None;
        }
        Some(after.position_at(at))
    };
    Some(Doc::new(
        &text,
        editor::EditorCursor {
            position: mapped(incoming.cursor.position)?,
            selection: match incoming.cursor.selection {
                Some(at) => Some(mapped(at)?),
                None => None,
            },
        },
    ))
}

#[cfg(test)]
mod rich_tests {
    use super::*;

    #[test]
    fn rich_typing_opens_guest_suggestions_without_treating_paste_or_delete_as_typing() {
        for (before_text, after_text, opens) in [
            ("T\nhello ", "T\nhello @", true),
            ("T\n", "T\n/", true),
            ("T\nhello ", "T\nhello :", true),
            ("T\n", "T\npasted @", false),
            ("T\n@x", "T\n@", false),
            ("T\n@", "T\n@", false),
            ("T\naddress", "T\naddress@", false),
        ] {
            let view = |text| ducktape_view_guest::EditorStateView {
                text,
                cursor: wire::EditorCursor {
                    position: wire::EditorPosition {
                        line: 1,
                        column: (text.len() - 2) as u32,
                    },
                    selection: None,
                },
                reset: 1,
                revision: 1,
                text_revision: 1,
            };
            let before = view(before_text);
            let after = ducktape_view_guest::EditorStateView {
                revision: 2,
                text_revision: 2,
                ..view(after_text)
            };
            let id = wire::EditorTransactionId {
                instance: 1,
                document: "doc".into(),
                reset: 1,
                sequence: 1,
                attempt: 0,
                text_revision: 1,
                revision: 1,
            };
            let origin = wire::EditorRequestInput::RichEdit {
                edit: Box::new(wire::editor_rich::RichEdit {
                    document: crate::rich_document::presentation(after_text, after.cursor).document,
                    ..Default::default()
                }),
            };
            let mut binding = BindingState {
                history: Default::default(),
                menu: crate::editor_menu::Menu::default().with_names(&["Ada".into()]),
            };
            binding
                .observe(EditorTransactionEvent::Commit {
                    id: &id,
                    before,
                    after,
                    origin: Some(&origin),
                    kind: wire::EditorEditKind::GuestPatch,
                    history: EditorHistoryEffect::Native,
                    input_time_ms: 0,
                })
                .expect("rich edit observed");
            assert_eq!(
                binding.menu.is_open(),
                opens,
                "{before_text:?} -> {after_text:?}"
            );
        }
    }

    #[test]
    fn rich_menu_interactions_apply_guest_choices_and_open_triggers() {
        use wire::editor_presentation::EditorInteraction;
        for (text, action, expected, open_after) in [
            (
                "T\n@",
                EditorInteraction::MenuPick { tag: "Ada".into() },
                "T\n@Ada ",
                false,
            ),
            (
                "T\nhello",
                EditorInteraction::Action { tag: "@".into() },
                "T\nhello @",
                true,
            ),
        ] {
            let cursor = wire::EditorCursor {
                position: wire::EditorPosition {
                    line: 1,
                    column: (text.len() - 2) as u32,
                },
                selection: None,
            };
            let doc = document(text, cursor);
            let mut menu = crate::editor_menu::Menu::default().with_names(&["Ada".into()]);
            menu.after_edit(&doc, Some('@'));
            let state = ducktape_view_guest::EditorStateView {
                text,
                cursor,
                reset: 1,
                revision: 1,
                text_revision: 1,
            };
            let rich = crate::rich_document::presentation(text, cursor).document;
            let edit = wire::editor_rich::RichEdit {
                before: Some(rich.clone()),
                document: rich,
                interaction: Some(action),
                ..Default::default()
            };
            let mut binding = BindingState {
                history: Default::default(),
                menu: menu.clone(),
            };
            binding.history.at_reset(1);
            let EditorDecision::Apply {
                patches,
                cursor,
                history,
            } = rich_decision(state, &edit, menu)
            else {
                panic!("rich interaction decision");
            };
            let actual = wire::patched_editor_text(text, &patches, cursor).unwrap();
            assert_eq!(actual, expected);
            let id = wire::EditorTransactionId {
                instance: 1,
                document: "doc".into(),
                reset: 1,
                sequence: 1,
                attempt: 0,
                revision: 1,
                text_revision: 1,
            };
            let origin = wire::EditorRequestInput::RichEdit {
                edit: Box::new(edit),
            };
            binding
                .observe(EditorTransactionEvent::Commit {
                    id: &id,
                    before: state,
                    after: ducktape_view_guest::EditorStateView {
                        text: &actual,
                        cursor,
                        revision: 2,
                        text_revision: 2,
                        ..state
                    },
                    origin: Some(&origin),
                    kind: wire::EditorEditKind::GuestPatch,
                    history,
                    input_time_ms: 0,
                })
                .unwrap();
            assert_eq!(
                binding.menu.is_open(),
                open_after,
                "commit installs the menu successor"
            );
        }
    }

    #[test]
    fn rich_typography_is_guest_owned_and_leaves_code_literal() {
        for (text, typed, expected) in [
            ("T\n", "(c)", "T\n©"),
            ("T\n", "한...", "T\n한…"),
            ("T\n```\n\n```", "(c)", "T\n```\n(c)\n```"),
            ("T\n```\n```", "(c)", "T\n```\n(c)\n```"),
        ] {
            let before = crate::rich_document::presentation(text, Default::default()).document;
            let mut incoming = before.clone();
            incoming.blocks[1].text = typed.into();
            incoming.cursor.position = wire::EditorPosition {
                line: 1,
                column: typed.len() as u32,
            };
            let edit = wire::editor_rich::RichEdit {
                before: Some(before),
                document: incoming,
                ..Default::default()
            };
            let state = ducktape_view_guest::EditorStateView {
                text,
                cursor: Default::default(),
                reset: 1,
                revision: 1,
                text_revision: 1,
            };
            let EditorDecision::Apply {
                patches, cursor, ..
            } = rich_decision(state, &edit, Default::default())
            else {
                panic!("rich input decision");
            };
            assert_eq!(
                wire::patched_editor_text(text, &patches, cursor).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn rich_caret_commit_closes_the_guest_mention_menu_without_editing_text() {
        let text = "T\n@";
        let cursor = wire::EditorCursor {
            position: wire::EditorPosition { line: 1, column: 1 },
            selection: None,
        };
        let state = ducktape_view_guest::EditorStateView {
            text,
            cursor,
            reset: 1,
            revision: 1,
            text_revision: 1,
        };
        let mut menu = crate::editor_menu::Menu::default().with_names(&["Ada".into()]);
        menu.after_edit(&document(text, cursor), Some('@'));
        assert!(menu.is_open());
        let before = crate::rich_document::presentation(text, cursor).document;
        let mut moved = before.clone();
        moved.cursor.position.column = 0;
        let edit = wire::editor_rich::RichEdit {
            before: Some(before),
            document: moved,
            ..Default::default()
        };
        let EditorDecision::Apply {
            patches,
            cursor,
            history,
        } = rich_decision(state, &edit, menu.clone())
        else {
            panic!("rich caret decision");
        };
        assert!(patches.is_empty());
        assert_eq!(cursor.position.column, 0);
        let mut binding = BindingState {
            history: Default::default(),
            menu,
        };
        binding.history.at_reset(1);
        let id = wire::EditorTransactionId {
            instance: 1,
            document: "doc".into(),
            reset: 1,
            sequence: 1,
            attempt: 0,
            revision: 1,
            text_revision: 1,
        };
        let origin = wire::EditorRequestInput::RichEdit {
            edit: Box::new(edit),
        };
        binding
            .observe(EditorTransactionEvent::Commit {
                id: &id,
                before: state,
                after: ducktape_view_guest::EditorStateView {
                    cursor,
                    revision: 2,
                    ..state
                },
                origin: Some(&origin),
                kind: wire::EditorEditKind::GuestPatch,
                history,
                input_time_ms: 0,
            })
            .unwrap();
        assert!(!binding.menu.is_open());
    }

    #[test]
    fn annotation_queued_behind_formatting_anchors_the_committed_selection() {
        let cursor = wire::EditorCursor {
            position: wire::EditorPosition { line: 1, column: 5 },
            selection: Some(wire::EditorPosition { line: 1, column: 0 }),
        };
        let snapshot = crate::rich_document::presentation("T\nhello", cursor).document;
        let before = ducktape_view_guest::EditorStateView {
            text: "T\n**hello**",
            cursor: wire::EditorCursor {
                position: wire::EditorPosition { line: 1, column: 7 },
                selection: Some(wire::EditorPosition { line: 1, column: 2 }),
            },
            reset: 1,
            text_revision: 1,
            revision: 1,
        };
        let edit = wire::editor_rich::RichEdit {
            before: Some(snapshot.clone()),
            document: snapshot,
            interaction: Some(wire::editor_presentation::EditorInteraction::Margin { line: 1 }),
            ..Default::default()
        };
        let EditorDecision::Apply {
            patches,
            cursor,
            history,
        } = rich_decision(before, &edit, Default::default())
        else {
            panic!("annotation decision");
        };
        assert!(patches.is_empty());
        let after = ducktape_view_guest::EditorStateView {
            cursor,
            revision: 2,
            ..before
        };
        let id = wire::EditorTransactionId {
            instance: 1,
            document: "doc".into(),
            reset: 1,
            sequence: 2,
            attempt: 0,
            text_revision: 1,
            revision: 1,
        };
        let origin = wire::EditorRequestInput::RichEdit {
            edit: Box::new(edit),
        };
        let mut binding = BindingState {
            history: Default::default(),
            menu: Default::default(),
        };
        let update = binding
            .observe(EditorTransactionEvent::Commit {
                id: &id,
                origin: Some(&origin),
                before,
                after,
                kind: wire::EditorEditKind::GuestPatch,
                history,
                input_time_ms: 0,
            })
            .unwrap();
        let navigation: crate::document_sync::Navigation =
            wire::decode(&update.interaction).unwrap();
        assert_eq!(navigation.comment_line, Some(1));
        assert_eq!(navigation.anchor, Some((2, 7)));
    }

    #[test]
    fn rich_insertion_uses_the_source_cursor_for_mark_affinity() {
        for (column, expected) in [
            (8, "T\n**bold** plain"),
            (6, "T\n**bold plain**"),
            (4, "T\n**bo plainld**"),
        ] {
            let text = "T\n**bold**";
            let cursor = wire::EditorCursor {
                position: wire::EditorPosition { line: 1, column },
                selection: None,
            };
            let before = crate::rich_document::presentation(text, cursor).document;
            let mut incoming = before.clone();
            let at = incoming.cursor.position.column as usize;
            incoming.blocks[1].text.insert_str(at, " plain");
            incoming.blocks[1].marks[0].end += 6;
            incoming.cursor.position.column += 6;
            let edit = wire::editor_rich::RichEdit {
                before: Some(before),
                document: incoming,
                ..Default::default()
            };
            let state = ducktape_view_guest::EditorStateView {
                text,
                cursor,
                reset: 1,
                revision: 1,
                text_revision: 1,
            };
            let EditorDecision::Apply {
                patches, cursor, ..
            } = rich_decision(state, &edit, Default::default())
            else {
                panic!("rich insertion decision");
            };
            assert_eq!(
                wire::patched_editor_text(text, &patches, cursor).unwrap(),
                expected
            );
            assert_eq!(cursor.position.column, column + 6);
        }
    }

    #[test]
    fn typing_queued_behind_guest_formatting_preserves_the_accepted_mark() {
        let before_cursor = wire::EditorCursor {
            position: wire::EditorPosition { line: 1, column: 5 },
            selection: Some(wire::EditorPosition { line: 1, column: 0 }),
        };
        let before = crate::rich_document::presentation("T\nhello", before_cursor).document;
        let incoming_cursor = wire::EditorCursor {
            position: wire::EditorPosition { line: 1, column: 1 },
            selection: None,
        };
        let incoming = crate::rich_document::presentation("T\nx", incoming_cursor).document;
        let state = ducktape_view_guest::EditorStateView {
            text: "T\n**hello**",
            cursor: wire::EditorCursor {
                position: wire::EditorPosition { line: 1, column: 7 },
                selection: Some(wire::EditorPosition { line: 1, column: 2 }),
            },
            reset: 1,
            text_revision: 1,
            revision: 1,
        };
        let edit = wire::editor_rich::RichEdit {
            before: Some(before),
            document: incoming,
            ..Default::default()
        };
        let EditorDecision::Apply {
            patches, cursor, ..
        } = rich_decision(state, &edit, Default::default())
        else {
            panic!("typing decision");
        };
        let after = wire::patched_editor_text(state.text, &patches, cursor).unwrap();
        assert_eq!(after, "T\n**x**");
        assert_eq!(cursor.position, wire::EditorPosition { line: 1, column: 3 });
    }
}
