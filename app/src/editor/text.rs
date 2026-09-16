//! The plain multi-line editor: ONE native text field projecting one guest
//! document.
//!
//! An editor that asks for no block furniture is a text box, and a text box is
//! one field. Selecting across two lines, joining them with Backspace, walking
//! by word, dragging a selection through a paragraph and remembering the
//! caret's column are the editing engine's own work. A stack of one-line
//! fields can only imitate them one key at a time, and every key it has not
//! learned is an edit the writer cannot make.

use super::{EditorStore, Projection, key_state, offset, position};
use gpui_kit::base::input::{InputEditorStyle, Textarea, TextareaState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Edges, Entity, EntityInputHandler as _, EventEmitter,
    Focusable as _, Hsla, InteractiveElement as _, IntoElement, Keystroke, MouseButton,
    ParentElement as _, Render, Styled as _, Subscription, Window, div, px,
};
use std::ops::Range;
use std::sync::Arc;
use view_wire as wire;
use unicode_segmentation::UnicodeSegmentation;

/// The key context a guest editor sits in. The shell's keystroke interceptor
/// runs before this editor's and cannot be stopped by it, so it reads this off
/// the context stack to yield the chords a guest claims.
pub const GUEST_EDITOR_CONTEXT: &str = "GuestEditor";

/// The tallest a field grows before it scrolls inside itself. The document
/// byte budget is the real limit; this is only the point past which growing
/// the element stops being how anyone reads it.
const MAX_ROWS: usize = 4096;

pub struct TextEditor {
    key: String,
    store: EditorStore,
    input: Entity<TextareaState>,
    preview: Arc<str>,
    cursor: wire::EditorCursor,
    reset: Option<u64>,
    projection: Option<Projection>,
    painted: Option<wire::EditorOptions>,
    fills: bool,
    ime: Option<crate::module_view::input::ImeState>,
    _observation: Subscription,
    _keystrokes: Subscription,
}

impl EventEmitter<()> for TextEditor {}

impl TextEditor {
    pub fn new(
        key: String,
        store: EditorStore,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, MAX_ROWS)
                .soft_wrap(true)
                .searchable(false)
                .context_menu(false)
        });
        let observation = cx.observe_in(&input, window, |this, _, window, cx| {
            this.observed(window, cx);
        });
        // Native key bindings consume Enter/Tab/navigation before element key
        // listeners. Guest claims must run at GPUI's pre-action seam.
        let editor = cx.entity().downgrade();
        let keystrokes = cx.intercept_keystrokes(move |event, window, cx| {
            let _ = editor.update(cx, |editor, cx| {
                editor.key_down(&event.keystroke, window, cx);
            });
        });
        let mut this = Self {
            key,
            store,
            input,
            preview: Arc::from(""),
            cursor: Default::default(),
            reset: None,
            projection: None,
            painted: None,
            fills: true,
            ime: None,
            _observation: observation,
            _keystrokes: keystrokes,
        };
        this.sync(window, cx);
        this
    }

    pub fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.input.read(cx).focus_handle(cx).is_focused(window)
    }

    /// Whether the field takes the box it was given or the room its own words
    /// need. Set from the node's height, because a field that always asked for
    /// all of its parent's height gave a shrinking box nothing to shrink to.
    pub fn set_fills(&mut self, fills: bool, cx: &mut Context<Self>) {
        if self.fills == fills {
            return;
        }
        self.fills = fills;
        cx.notify();
    }

    pub fn widget_command(
        &mut self,
        command: &wire::WidgetCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let at = |index: u32| {
            position(
                &self.preview,
                self.preview
                    .grapheme_indices(true)
                    .nth(index as usize)
                    .map_or(self.preview.len(), |(offset, _)| offset),
            )
        };
        let cursor = match command {
            wire::WidgetCommand::Focus { .. } => self.cursor,
            wire::WidgetCommand::CursorFront { .. } => wire::EditorCursor::default(),
            wire::WidgetCommand::CursorEnd { .. } => wire::EditorCursor {
                position: position(&self.preview, self.preview.len()),
                selection: None,
            },
            wire::WidgetCommand::Cursor {
                position: index, ..
            } => wire::EditorCursor {
                position: at(*index),
                selection: None,
            },
            wire::WidgetCommand::SelectAll { .. } => wire::EditorCursor {
                position: position(&self.preview, self.preview.len()),
                selection: Some(Default::default()),
            },
            wire::WidgetCommand::Select { start, end, .. } => wire::EditorCursor {
                position: at(*end),
                selection: (*start != *end).then(|| at(*start)),
            },
            _ => return false,
        };
        if cursor != self.cursor {
            self.move_cursor(cursor, cx);
            self.install(window, cx);
        }
        self.input.read(cx).focus_handle(cx).focus(window, cx);
        self.sync(window, cx);
        true
    }

    /// Carry the guest's accepted document into the field, and the field's
    /// options with it. While an edit is in flight the guest's copy is behind
    /// what was typed, so nothing is installed until the queue settles: a
    /// field that reverted to the last acknowledged text between keystrokes
    /// would eat every letter typed faster than a block.
    pub fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(projection) = self.store.projection(&self.key) else {
            return;
        };
        let reset = self.reset != Some(projection.reference.reset);
        let settled = !projection.pending;
        let canonical = projection.text.clone().unwrap_or_else(|| Arc::from(""));
        let install = reset
            || (settled
                && (canonical != self.preview || projection.reference.cursor != self.cursor));
        if install {
            self.preview = canonical;
            self.cursor = projection.reference.cursor;
            self.reset = Some(projection.reference.reset);
            self.install(window, cx);
        }
        let editable =
            projection.editable && projection.fault.is_none() && projection.text.is_some();
        let input = self.input.clone();
        input.update(cx, |input, cx| {
            if input.is_editable() != editable {
                input.set_readonly(!editable, cx);
            }
            input.set_placeholder(projection.placeholder.clone(), window, cx);
            input.set_soft_wrap(
                !matches!(projection.options.wrapping, Some(wire::Wrapping::None)),
                window,
                cx,
            );
            input.set_editor_paddings(Edges::all(px(0.)));
            let face = &projection.options.style.active;
            // A face that names no selection ink gets the theme's: the default
            // is transparent, and a selection nobody can see is a selection
            // nobody trusts.
            let selection = face
                .selection
                .map(color)
                .unwrap_or(gpui_kit::component::Theme::global(cx).selection);
            input.set_editor_style(InputEditorStyle {
                foreground: face.value.map(color).unwrap_or_default(),
                muted_foreground: face.placeholder.map(color).unwrap_or_default(),
                background: face.background.map(color).unwrap_or_default(),
                caret: face.value.map(color).unwrap_or_default(),
                selection,
                ..Default::default()
            });
        });
        self.painted = Some(projection.options.clone());
        self.projection = Some(projection);
    }

    /// Put the document this mount holds into the field, text and caret both.
    /// ONLY the guest's own answer reaches here: a field rewritten on every
    /// frame would be rewritten between a keystroke and the observation of it,
    /// and the letter just typed would be the one it took back.
    fn install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selection = selected_range(&self.preview, self.cursor);
        let text = self.preview.clone();
        self.input.clone().update(cx, |input, cx| {
            if input.value().as_ref() != text.as_ref() {
                input.set_value(text.to_string(), window, cx);
            }
            let standing = selection.start.min(selection.end)..selection.start.max(selection.end);
            let moved = input.selected_range() != standing || input.cursor() != selection.end;
            if moved {
                input.set_selected_range(selection, cx);
            }
        });
    }

    /// The field changed under the writer's hands: report the whole text and
    /// the caret that came with it. The editing engine has already decided
    /// what the keystroke meant, so there is one description of the edit here
    /// and it is the difference between two documents.
    fn observed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_focused(window, cx) {
            return;
        }
        let input = self.input.clone();
        let (text, marked, caret, selected) = input.update(cx, |input, cx| {
            (
                input.value().to_string(),
                input.marked_text_range(window, cx),
                input.cursor(),
                input.selected_range(),
            )
        });
        let events =
            crate::module_view::input::ime_events(&mut self.ime, &text, marked, caret, selected);
        if !events.is_empty() {
            self.store.observe_ime(events);
            cx.emit(());
        }
        // Preedit is observation only. The committed native edit follows the
        // ordinary guest transaction path exactly once after composition ends.
        if self.composing(window, cx) {
            return;
        }
        let state = input.read(cx);
        let text = state.value().to_string();
        let caret = state.cursor();
        let selected = state.selected_range();
        let next = cursor_at(&text, caret, selected);
        let same = text == self.preview.as_ref() && next == self.cursor;
        if same {
            return;
        }
        let kind = edit_kind(&self.preview, &text);
        self.store
            .native(&self.key, &self.preview, self.cursor, &text, next, kind);
        self.preview = Arc::from(text);
        self.cursor = next;
        cx.emit(());
        cx.notify();
    }

    fn composing(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.input.clone().update(cx, |input, cx| {
            input.marked_text_range(window, cx).is_some()
        })
    }

    /// The caret moved and nothing else: the guest still owns the cursor, so
    /// it hears about the move the same way it hears about a letter.
    fn move_cursor(&mut self, cursor: wire::EditorCursor, cx: &mut Context<Self>) {
        self.store.native(
            &self.key,
            &self.preview,
            self.cursor,
            &self.preview,
            cursor,
            wire::EditorEditKind::Cursor,
        );
        self.cursor = cursor;
        cx.emit(());
        cx.notify();
    }

    /// Only the chords the guest claimed are taken off the field. Everything
    /// else — every arrow, every Backspace, every selection — belongs to the
    /// editing engine, and taking one of those away is how an editor stops
    /// being one.
    fn key_down(&mut self, keystroke: &Keystroke, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_focused(window, cx) {
            return;
        }
        if self.composing(window, cx) {
            return;
        }
        let key = key_state(keystroke);
        let claimed = self
            .projection
            .as_ref()
            .and_then(|projection| projection.options.binding.as_ref())
            .is_some_and(|binding| {
                binding
                    .claims
                    .iter()
                    .any(|claim| claim.matches(&key, cfg!(target_os = "macos")))
            });
        if claimed {
            self.store.request(
                &self.key,
                wire::EditorRequestInput::Key { key, repeat: false },
            );
            cx.stop_propagation();
            cx.emit(());
            return;
        }
        self.tab(keystroke, window, cx);
    }

    /// Tab, which the writer means as an indent and the field would otherwise
    /// spend on leaving. The editing engine has an indent of its own and will
    /// not run it here — it is switched off for a field that grows with its
    /// text, which is every guest editor — so the keystroke walks on to the
    /// window's focus ring and the caret never sees it. Type the indent
    /// instead, exactly as if the two spaces had been pressed.
    fn tab(&mut self, keystroke: &Keystroke, window: &mut Window, cx: &mut Context<Self>) {
        let tab = keystroke.key == "tab";
        let plain = !keystroke.modifiers.control
            && !keystroke.modifiers.alt
            && !keystroke.modifiers.platform
            && !keystroke.modifiers.function;
        if !tab || !plain {
            return;
        }
        let outward = keystroke.modifiers.shift;
        let input = self.input.clone();
        let (text, selected, writable) = input.update(cx, |input, _| {
            (
                input.value().to_string(),
                input.selected_range(),
                input.is_editable(),
            )
        });
        if !writable {
            return;
        }
        // Shift+Tab against a line with no indent left to give has nothing to
        // do, and a key with nothing to do is the key that walks the focus
        // ring. Only an indent that actually moved is one this field keeps.
        let Some((next, moved)) = indent(&text, selected, outward) else {
            return;
        };
        input.update(cx, |input, cx| {
            input.set_value(next, window, cx);
            input.set_selected_range(moved, cx);
        });
        cx.stop_propagation();
    }

    /// A press in the box that the field itself did not take — the empty room
    /// under the last line — is still a press on the writing. It puts the
    /// caret at the end of the text, the way clicking under the words in any
    /// text box does, instead of landing nowhere.
    fn pressed(
        &mut self,
        event: &gpui_kit::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let on_the_words = self.input.read(cx).input_bounds().contains(&event.position);
        if on_the_words {
            return;
        }
        let end = wire::EditorCursor {
            position: position(&self.preview, self.preview.len()),
            selection: None,
        };
        if end != self.cursor {
            self.move_cursor(end, cx);
            self.install(window, cx);
        }
        self.input.read(cx).focus_handle(cx).focus(window, cx);
    }
}

impl Render for TextEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync(window, cx);
        let options = self
            .projection
            .as_ref()
            .map(|projection| projection.options.clone())
            .unwrap_or_default();
        let size = options.size.unwrap_or(14.);
        let line_height = match options.line_height {
            Some(wire::LineHeight::Absolute(height)) => height,
            Some(wire::LineHeight::Relative(ratio)) => ratio * size,
            None => size * 1.4,
        };
        // The generic families name no face this app registered; every one but
        // the monospace is the app's own text face, which is the one with the
        // weights on it.
        let family = options.font.as_ref().map(|font| match &font.family {
            wire::FontFamily::Named(name) => name.clone(),
            wire::FontFamily::Monospace => design::fonts::FAMILY_MONO.to_owned(),
            _ => design::fonts::FAMILY_UI.to_owned(),
        });
        // The shell reads this context off a keystroke to yield the chords a
        // guest editor claims — Ctrl+K is a link here, not the search palette.
        div()
            .key_context(GUEST_EDITOR_CONTEXT)
            .relative()
            .w_full()
            // The box the guest gave, not the room the words take: a press in
            // the empty part of a card is a press on the card's writing. A
            // field asked to shrink has no empty part to press — its box IS
            // its words — and taking the parent's height there would be taking
            // the height the parent is waiting on this field to report.
            .when(self.fills, |element| element.h_full())
            .on_mouse_down(MouseButton::Left, cx.listener(Self::pressed))
            .p(px(options.padding.unwrap_or(8.)))
            .text_size(px(size))
            .line_height(px(line_height))
            .when_some(family, |element, family| {
                crate::shell::with_family(element, family)
            })
            .child(Textarea::new(&self.input))
    }
}

/// One indent. Two spaces, the editing engine's own tab size: what an indent
/// has to do in prose is line the next line up under this one, and a hard tab
/// lines it up against a stop no painter here draws.
const INDENT: &str = "  ";

/// The document after Tab, and where the selection lands in it. `None` when
/// the key had nothing to do.
///
/// A caret types an indent where it stands. A SELECTION moves whole lines
/// instead — that is what makes Tab worth having in a list, and replacing the
/// selected words with two spaces is a deletion nobody asked for.
fn indent(text: &str, selected: Range<usize>, outward: bool) -> Option<(String, Range<usize>)> {
    let lo = selected.start.min(selected.end);
    let hi = selected.start.max(selected.end);
    let typing = lo == hi && !outward;
    if typing {
        let mut next = text.to_owned();
        next.insert_str(lo, INDENT);
        let at = lo + INDENT.len();
        return Some((next, at..at));
    }
    // The first line is the one the selection starts ON, wherever in it that
    // is; the last is the last one it starts BEFORE, so a selection carried to
    // the head of a line leaves that line alone, as it does everywhere else.
    let head = text[..lo].rfind('\n').map_or(0, |at| at + 1);
    let mut next = text[..head].to_owned();
    let mut edits: Vec<(usize, usize, usize)> = Vec::new();
    let mut at = head;
    for line in text[head..].split_inclusive('\n') {
        let touched = at == head || at < hi;
        if !touched {
            next.push_str(line);
            at += line.len();
            continue;
        }
        match outward {
            true => {
                let shed = outdent(line);
                edits.push((at, 0, shed));
                next.push_str(&line[shed..]);
            }
            false => {
                edits.push((at, INDENT.len(), 0));
                next.push_str(INDENT);
                next.push_str(line);
            }
        }
        at += line.len();
    }
    let nothing_to_shed = edits
        .iter()
        .all(|(_, added, removed)| *added + *removed == 0);
    if nothing_to_shed {
        return None;
    }
    let shifted = |offset: usize| {
        let mut moved = offset;
        for &(start, added, removed) in &edits {
            if start > offset {
                break;
            }
            moved += added;
            moved -= removed.min(offset - start);
        }
        moved
    };
    Some((next, shifted(selected.start)..shifted(selected.end)))
}

/// How much of a line's leading whitespace one Shift+Tab takes back: a hard
/// tab whole, or up to an indent's worth of spaces.
fn outdent(line: &str) -> usize {
    if line.starts_with('\t') {
        return 1;
    }
    line.bytes()
        .take(INDENT.len())
        .take_while(|byte| *byte == b' ')
        .count()
}

/// The field's selection for a guest cursor, ANCHOR first: the range runs
/// backwards when the caret is the earlier end, which is how the engine is
/// told which end a shift-arrow extends from.
fn selected_range(text: &str, cursor: wire::EditorCursor) -> Range<usize> {
    let caret = offset(text, cursor.position);
    let anchor = cursor.selection.map_or(caret, |at| offset(text, at));
    anchor..caret
}

/// The guest cursor for a field's caret and selection, read back the same way.
fn cursor_at(text: &str, caret: usize, selected: Range<usize>) -> wire::EditorCursor {
    let anchor = if caret == selected.start {
        selected.end
    } else {
        selected.start
    };
    wire::EditorCursor {
        position: position(text, caret),
        selection: (selected.start != selected.end).then(|| position(text, anchor)),
    }
}

fn edit_kind(before: &str, after: &str) -> wire::EditorEditKind {
    let unchanged = before == after;
    if unchanged {
        return wire::EditorEditKind::Cursor;
    }
    let shorter = after.len() < before.len();
    match shorter {
        true => wire::EditorEditKind::Backspace,
        false => wire::EditorEditKind::Insert,
    }
}

fn color(ink: wire::Rgba) -> Hsla {
    gpui_kit::Rgba {
        r: ink.0[0],
        g: ink.0[1],
        b: ink.0[2],
        a: ink.0[3],
    }
    .into()
}

/// A store holding one ready document with the given claims. The mount reads
/// its projection exactly the way it reads a live guest's.
#[cfg(test)]
fn store_with(
    name: &str,
    text: &str,
    claims: Vec<wire::EditorKeyClaim>,
    placeholder: &str,
) -> EditorStore {
    let store = EditorStore::new(91);
    let reference = wire::editor_document::EditorDocumentRef {
        document: name.into(),
        reset: 1,
        revision: 0,
        text_revision: 0,
        byte_len: text.len() as u32,
        cursor: wire::EditorCursor {
            position: position(text, text.len()),
            selection: None,
        },
    };
    let mut locked = store.lock();
    locked.fields.insert(
        "document".into(),
        super::Field {
            reference: reference.clone(),
            handler: 1,
            editable: true,
            placeholder: placeholder.to_owned(),
            options: wire::EditorOptions {
                binding: Some(Box::new(wire::EditorBinding {
                    authored: true,
                    on_request: 2,
                    on_event: 3,
                    claims,
                })),
                ..Default::default()
            },
        },
    );
    locked.documents.insert(
        reference.document.clone(),
        super::Document {
            reference,
            text: Some(Arc::from(text)),
            queue: Default::default(),
            queued_bytes: 0,
            phase: super::Phase::Ready,
        },
    );
    drop(locked);
    store
}

/// Settle every queued edit against the document, the way a guest that accepts
/// what the field did would.
#[cfg(test)]
fn settle(store: &EditorStore, name: &str) {
    let mut locked = store.lock();
    while !locked.documents[name].queue.is_empty() {
        let accepted = locked.documents[name].reference.clone();
        locked.fields.get_mut("document").unwrap().reference = accepted;
        locked.acknowledge();
        locked.pump();
        assert!(locked.fault.is_none(), "{:?}", locked.fault);
    }
}

/// The field carries the guest's placeholder, follows it when the guest
/// changes it, and takes what is typed into it.
#[cfg(test)]
#[gpui_kit::test]
fn an_empty_field_wears_the_guests_placeholder_and_takes_what_is_typed(
    cx: &mut gpui_kit::TestAppContext,
) {
    use gpui_kit::test::TestWindowExt as _;
    cx.update(gpui_kit::init);
    let store = store_with("empty", "", Vec::new(), "Start writing");
    let window = cx.open_window(gpui_kit::size(px(400.), px(200.)), |window, cx| {
        TextEditor::new("document".into(), store.clone(), window, cx)
    });
    let editor = window.root(cx).unwrap();
    let mut native = gpui_kit::VisualTestContext::from_window(window.into(), cx);
    native.update(|window, cx| {
        window.render_frame(cx);
        let input = editor.read(cx).input.clone();
        assert!(input.read(cx).value().is_empty());
        assert_eq!(
            input.read(cx).presentation().placeholder().as_ref(),
            "Start writing"
        );
        input.read(cx).focus_handle(cx).focus(window, cx);
        window.render_frame(cx);
    });
    store.lock().fields.get_mut("document").unwrap().placeholder = "새 문서".into();
    native.update(|window, cx| {
        editor.update(cx, |editor, cx| editor.sync(window, cx));
        window.render_frame(cx);
        assert_eq!(
            editor
                .read(cx)
                .input
                .read(cx)
                .presentation()
                .placeholder()
                .as_ref(),
            "새 문서"
        );
        window.input("Written text", cx);
    });
    native.run_until_parked();
    native.update(|window, cx| {
        window.render_frame(cx);
        let editor = editor.read(cx);
        assert_eq!(editor.input.read(cx).value().as_ref(), "Written text");
        assert_eq!(editor.preview.as_ref(), "Written text");
        assert!(store.lock().fault.is_none());
        window.blur(cx);
    });
}

/// Shift and an arrow reach past the line they started on. This is the whole
/// reason the document is one field: a selection that stops at the newline is
/// a selection that cannot take a paragraph.
#[cfg(test)]
#[gpui_kit::test]
fn shift_and_an_arrow_select_across_the_lines_of_one_document(cx: &mut gpui_kit::TestAppContext) {
    use gpui_kit::test::TestWindowExt as _;
    cx.update(gpui_kit::init);
    let store = store_with("lines", "one\ntwo\nthree", Vec::new(), "");
    let window = cx.open_window(gpui_kit::size(px(400.), px(200.)), |window, cx| {
        TextEditor::new("document".into(), store.clone(), window, cx)
    });
    let editor = window.root(cx).unwrap();
    let mut native = gpui_kit::VisualTestContext::from_window(window.into(), cx);
    native.update(|window, cx| {
        window.render_frame(cx);
        editor.update(cx, |editor, cx| {
            editor.input.read(cx).focus_handle(cx).focus(window, cx)
        });
        window.render_frame(cx);
        window.dispatch_keystroke(Keystroke::parse("shift-up").unwrap(), cx);
        window.dispatch_keystroke(Keystroke::parse("shift-up").unwrap(), cx);
    });
    native.run_until_parked();
    native.update(|window, cx| window.render_frame(cx));
    editor.read_with(&native, |editor, cx| {
        let selected = editor.input.read(cx).selected_range();
        assert!(
            editor.preview[selected.start..selected.end].contains('\n'),
            "a selection that took two shift-ups spans the newlines it crossed"
        );
        let anchor = editor
            .cursor
            .selection
            .expect("the selection reaches the guest");
        assert_ne!(
            anchor.line, editor.cursor.position.line,
            "the guest is told the selection crosses lines"
        );
    });
}

/// Backspace at the head of a line takes the newline before it and joins the
/// two lines — the ordinary way any text box works.
#[cfg(test)]
#[gpui_kit::test]
fn backspace_at_the_head_of_a_line_joins_it_to_the_one_above(cx: &mut gpui_kit::TestAppContext) {
    use gpui_kit::test::TestWindowExt as _;
    cx.update(gpui_kit::init);
    let store = store_with("join", "one\ntwo", Vec::new(), "");
    let window = cx.open_window(gpui_kit::size(px(400.), px(200.)), |window, cx| {
        TextEditor::new("document".into(), store.clone(), window, cx)
    });
    let editor = window.root(cx).unwrap();
    let mut native = gpui_kit::VisualTestContext::from_window(window.into(), cx);
    native.update(|window, cx| {
        window.render_frame(cx);
        editor.update(cx, |editor, cx| {
            editor.input.read(cx).focus_handle(cx).focus(window, cx)
        });
        window.render_frame(cx);
        // The caret starts at the end of "two"; Home puts it at the head.
        window.dispatch_keystroke(Keystroke::parse("home").unwrap(), cx);
        window.dispatch_keystroke(Keystroke::parse("backspace").unwrap(), cx);
    });
    native.run_until_parked();
    settle(&store, "join");
    native.update(|window, cx| window.render_frame(cx));
    editor.read_with(&native, |editor, _| {
        assert_eq!(editor.preview.as_ref(), "onetwo");
    });
    assert_eq!(
        store.lock().documents["join"].text.as_deref(),
        Some("onetwo"),
        "the joined document reaches the guest"
    );
}

/// A claimed chord is the guest's and never the field's; everything else is
/// the field's and never the guest's.
#[cfg(test)]
#[gpui_kit::test]
fn the_guest_hears_the_chords_it_claimed_and_no_others(cx: &mut gpui_kit::TestAppContext) {
    use gpui_kit::test::TestWindowExt as _;
    cx.update(gpui_kit::init);
    let store = store_with(
        "claims",
        "text",
        vec![wire::EditorKeyClaim {
            key: wire::keyboard::Key::Character("z".into()),
            modifiers: Default::default(),
            command: true,
        }],
        "",
    );
    let window = cx.open_window(gpui_kit::size(px(400.), px(200.)), |window, cx| {
        TextEditor::new("document".into(), store.clone(), window, cx)
    });
    let editor = window.root(cx).unwrap();
    let mut native = gpui_kit::VisualTestContext::from_window(window.into(), cx);
    native.update(|window, cx| {
        window.render_frame(cx);
        editor.update(cx, |editor, cx| {
            editor.input.read(cx).focus_handle(cx).focus(window, cx)
        });
        window.render_frame(cx);
    });
    store.drain();
    let undo = if cfg!(target_os = "macos") {
        "cmd-z"
    } else {
        "ctrl-z"
    };
    native.update(|window, cx| window.dispatch_keystroke(Keystroke::parse(undo).unwrap(), cx));
    let claimed = store.drain();
    assert!(
        claimed.iter().any(
            |event| matches!(event, wire::Event::EditorRequest { request, .. }
            if matches!(&request.input, wire::EditorRequestInput::Key { key, .. }
                if key.key == wire::keyboard::Key::Character("z".into())))
        ),
        "undo must reach guest history, not the field's own undo stack: {claimed:?}"
    );
    native.update(|window, cx| window.dispatch_keystroke(Keystroke::parse("left").unwrap(), cx));
    let unclaimed = store.drain();
    assert!(
        !unclaimed
            .iter()
            .any(|event| matches!(event, wire::Event::EditorRequest { .. })),
        "an arrow is the field's to answer: {unclaimed:?}"
    );
}

/// A field the guest will not let anyone write in reports nothing, and keeps
/// the text it was given.
#[cfg(test)]
#[gpui_kit::test]
fn a_readonly_field_reports_no_edit(cx: &mut gpui_kit::TestAppContext) {
    use gpui_kit::test::TestWindowExt as _;
    cx.update(gpui_kit::init);
    let store = store_with("readonly", "Read only 한글", Vec::new(), "");
    store.lock().fields.get_mut("document").unwrap().editable = false;
    let window = cx.open_window(gpui_kit::size(px(400.), px(200.)), |window, cx| {
        TextEditor::new("document".into(), store.clone(), window, cx)
    });
    let editor = window.root(cx).unwrap();
    let mut native = gpui_kit::VisualTestContext::from_window(window.into(), cx);
    native.update(|window, cx| {
        window.render_frame(cx);
        editor.update(cx, |editor, cx| {
            editor.input.read(cx).focus_handle(cx).focus(window, cx)
        });
        window.render_frame(cx);
    });
    store.drain();
    native.update(|window, cx| {
        window.input("no", cx);
        window.dispatch_keystroke(Keystroke::parse("backspace").unwrap(), cx);
    });
    native.run_until_parked();
    editor.read_with(&native, |editor, cx| {
        assert_eq!(editor.preview.as_ref(), "Read only 한글");
        assert!(!editor.input.read(cx).is_editable());
    });
    assert_eq!(
        store.lock().documents["readonly"].text.as_deref(),
        Some("Read only 한글")
    );
}

/// A drag-selection is one caret move per pointer sample, against a queue that
/// drains one item per guest frame and faults the whole view when it fills.
/// Two caret moves in a row compose, so the queue keeps the one in flight and
/// one destination however far the pointer travels.
#[cfg(test)]
#[test]
fn a_drag_through_a_paragraph_does_not_fill_the_queue() {
    let text = "one two three four five six seven eight nine ten";
    let store = store_with("drag", text, Vec::new(), "");
    let reaching = |byte: usize| wire::EditorCursor {
        position: position(text, byte),
        selection: Some(position(text, 0)),
    };
    let mut held = wire::EditorCursor {
        position: position(text, 0),
        selection: None,
    };
    for byte in 1..text.len() {
        let next = reaching(byte);
        store.native(
            "document",
            text,
            held,
            text,
            next,
            wire::EditorEditKind::Cursor,
        );
        held = next;
    }
    let locked = store.lock();
    assert!(locked.fault.is_none(), "{:?}", locked.fault);
    let queue = &locked.documents["drag"].queue;
    assert!(
        queue.len() <= 2,
        "a drag of {} samples left {} in the queue",
        text.len() - 1,
        queue.len()
    );
}

/// Tab is an indent: typed where the caret stands, and carried across whole
/// lines when a selection covers them. Shift+Tab takes one back, and takes
/// nothing when there is nothing left to take — which is what leaves the key
/// to the focus ring.
#[cfg(test)]
#[test]
fn tab_indents_a_caret_a_block_and_gives_it_back() {
    let typed = indent("ab", 1..1, false).expect("an indent at the caret");
    assert_eq!(typed, ("a  b".to_owned(), 3..3));

    // Two lines selected from the middle of the first to the middle of the
    // second: both move, and both ends of the selection move with them.
    let block = indent("one\ntwo\nthree", 1..5, false).expect("a block indent");
    assert_eq!(block, ("  one\n  two\nthree".to_owned(), 3..9));

    // A selection carried to the head of the next line leaves that line where
    // it is — the writer stopped before it.
    let up_to = indent("one\ntwo", 0..4, false).expect("a block indent");
    assert_eq!(up_to.0, "  one\ntwo");

    let back = indent("  one\n  two", 3..9, true).expect("an outdent");
    assert_eq!(back, ("one\ntwo".to_owned(), 1..5));

    // A caret inside the indentation being taken back lands at the line's
    // head rather than running off it.
    let inside = indent("  one", 1..1, true).expect("an outdent");
    assert_eq!(inside, ("one".to_owned(), 0..0));

    assert_eq!(indent("one\ntwo", 0..7, true), None);
    assert_eq!(indent("\tone", 0..0, true), Some(("one".to_owned(), 0..0)));
}
