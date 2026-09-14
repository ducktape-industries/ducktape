//! Pure inline formatting edits: the floating menu's and the shortcuts' marks
//! over the selection, links, and the "reset formatting" strip. Every edit
//! returns a decision the binding proposes; Commit owns history.
use crate::editor::{self, Doc, EditorCursor, EditorDecision, EditorHistoryEffect};
use crate::inline::{self, Inline};
use crate::markdown::{Align, align_marker, content_start};
use std::ops::Range;

impl Align {
    /// The menu tag and shortcut name each side answers to.
    pub fn from_tag(tag: &str) -> Option<Self> {
        Some(match tag {
            "left" => Align::Start,
            "center" => Align::Center,
            "right" => Align::End,
            _ => return None,
        })
    }
}

/// Tiptap's text-align over every block line the target touches: the side's
/// marker right after each line's prefix, replaced in place; `Start` strips
/// it. The title, a divider and a code fence take none.
pub fn align(document: &Doc, align: Align) -> EditorDecision {
    let range = target(document);
    let first = document.position_at(range.start).line as usize;
    let last = document.position_at(range.end).line as usize;
    align_lines(document, first..last + 1, align)
}

/// The same edit over one block, from the block menu.
pub fn align_lines(document: &Doc, lines: Range<usize>, align: Align) -> EditorDecision {
    let mut next = document.clone();
    for line in lines.start.max(1)..lines.end {
        realign(&mut next, line, align);
    }
    finish(document, next)
}

/// Rewrite one line's alignment marker; a caret on that line stays over the
/// same content byte, and one inside the old marker lands after the new one.
fn realign(next: &mut Doc, line: usize, align: Align) {
    let Some(text) = next.line(line) else {
        return;
    };
    let takes_none = text.trim() == "---" || fenced(next, line);
    if takes_none {
        return;
    }
    let content = content_start(text);
    let (current, current_len) = align_marker(&text[content..]);
    if current == align {
        return;
    }
    let marker = align.marker();
    let base = next.offset(editor::EditorPosition::new(line, 0));
    next.text
        .replace_range(base + content..base + content + current_len, marker);
    let old_end = (content + current_len) as u32;
    let new_end = (content + marker.len()) as u32;
    let positions =
        std::iter::once(&mut next.cursor.position).chain(next.cursor.selection.iter_mut());
    for position in positions.filter(|position| position.line as usize == line) {
        let past_marker = position.column >= old_end;
        let inside_marker = position.column > content as u32;
        position.column = match (past_marker, inside_marker) {
            (true, _) => position.column - old_end + new_end,
            (false, true) => new_end,
            (false, false) => position.column,
        };
    }
}

/// True on a fence line or between two of them.
fn fenced(document: &Doc, line: usize) -> bool {
    let lines = document.lines();
    let is_fence = |text: &&str| text.trim_start_matches([' ', '\t']).starts_with("```");
    let opened_above = lines[..line].iter().copied().filter(is_fence).count() % 2 == 1;
    opened_above || lines.get(line).is_some_and(is_fence)
}

/// A fence the reader toggles: the marker it is spelled with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wrap {
    Bold,
    Italic,
    Strike,
    Underline,
    Code,
    Highlight,
}

impl Wrap {
    pub fn marker(self) -> &'static str {
        match self {
            Wrap::Bold => "**",
            Wrap::Italic => "*",
            Wrap::Strike => "~~",
            Wrap::Underline => "++",
            Wrap::Code => "`",
            Wrap::Highlight => "==",
        }
    }

    /// The menu tag and shortcut name each wrap answers to.
    pub fn from_tag(tag: &str) -> Option<Self> {
        Some(match tag {
            "bold" => Wrap::Bold,
            "italic" => Wrap::Italic,
            "strike" => Wrap::Strike,
            "underline" => Wrap::Underline,
            "code" => Wrap::Code,
            "highlight" => Wrap::Highlight,
            _ => return None,
        })
    }
}

fn finish(before: &Doc, after: Doc) -> EditorDecision {
    if before == &after {
        return EditorDecision::Noop;
    }
    editor::restore(before, &after, EditorHistoryEffect::NewGroup)
}

/// The byte span an inline edit works on: the selection, else the word under
/// the caret, else the empty span at the caret.
pub fn target(document: &Doc) -> Range<usize> {
    let caret = document.offset(document.cursor.position);
    if let Some(anchor) = document.cursor.selection {
        let anchor = document.offset(anchor);
        return past_prefix(document, caret.min(anchor)..caret.max(anchor));
    }
    let text = &document.text;
    let is_word = |c: char| c.is_alphanumeric() || c == '\'';
    let start = text[..caret]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map_or(caret, |(index, _)| index);
    let end = text[caret..]
        .find(|c| !is_word(c))
        .map_or(text.len(), |offset| caret + offset);
    past_prefix(document, start..end)
}

/// A selection that swallowed the block prefix — Shift+Home on a quote, a
/// todo, a heading — marks the words after it. The prefix is the block's
/// shape, never part of what an inline mark wraps: `[> quote](url)` is a
/// paragraph that lost its quote.
fn past_prefix(document: &Doc, range: Range<usize>) -> Range<usize> {
    let line = document.position_at(range.start).line as usize;
    let Some(text) = document.line(line) else {
        return range;
    };
    let base = document.offset(editor::EditorPosition::new(line, 0));
    let content = base + content_start(text);
    range.start.max(content).min(range.end)..range.end
}

/// The title is a page property, not prose: line 0 takes no inline marks.
fn on_title(document: &Doc, range: &Range<usize>) -> bool {
    document.position_at(range.start).line == 0
}

fn selected(document: &Doc, range: Range<usize>) -> Doc {
    let mut next = document.clone();
    next.cursor = EditorCursor {
        position: next.position_at(range.end),
        selection: Some(next.position_at(range.start)),
    };
    next
}

/// Wrap the target in `wrap`'s marker — or unwrap it when it already wears
/// the marker, inside or just outside the selection. An empty target gets an
/// empty pair with the caret between, so what is typed next wears the mark.
pub fn toggle(document: &Doc, wrap: Wrap) -> EditorDecision {
    let range = target(document);
    if on_title(document, &range) || crosses_lines(document, &range) {
        return EditorDecision::Noop;
    }
    let marker = wrap.marker();
    let text = &document.text;
    let body = &text[range.clone()];
    let wrapped_inside =
        body.len() > 2 * marker.len() && body.starts_with(marker) && body.ends_with(marker);
    if wrapped_inside {
        let inner = range.start + marker.len()..range.end - marker.len();
        let mut next = document.clone();
        next.text.replace_range(range.clone(), &text[inner.clone()]);
        let marked = range.start..range.start + inner.len();
        return finish(document, settled(document, next, marked, &range));
    }
    // The fence around the selection must be exactly this marker: the `*`
    // beside `**bold**` belongs to the bold fence, not to an italic one.
    let glyph = marker.chars().next().unwrap_or_default();
    let before = text[..range.start]
        .strip_suffix(marker)
        .is_some_and(|rest| !rest.ends_with(glyph));
    let after = text[range.end..]
        .strip_prefix(marker)
        .is_some_and(|rest| !rest.starts_with(glyph));
    if before && after {
        // Ctrl+B at the end of what was just typed in bold leaves the mark
        // and keeps the words in it: the caret steps past the closing fence
        // so what is typed next is plain. A selection, a caret mid-run, or
        // an empty pair still unwraps.
        let caret = document.offset(document.cursor.position);
        let leaving_the_run = !selecting(document) && caret == range.end && !body.is_empty();
        if leaving_the_run {
            let mut next = document.clone();
            next.cursor = EditorCursor {
                position: next.position_at(range.end + marker.len()),
                selection: None,
            };
            return finish(document, next);
        }
        let mut next = document.clone();
        next.text
            .replace_range(range.end..range.end + marker.len(), "");
        next.text
            .replace_range(range.start - marker.len()..range.start, "");
        let start = range.start - marker.len();
        let marked = start..start + body.len();
        return finish(document, settled(document, next, marked, &range));
    }
    let mut next = document.clone();
    next.text.insert_str(range.end, marker);
    next.text.insert_str(range.start, marker);
    let start = range.start + marker.len();
    let marked = start..start + body.len();
    finish(document, settled(document, next, marked, &range))
}

/// A standing selection, as opposed to a bare caret.
fn selecting(document: &Doc) -> bool {
    document
        .cursor
        .selection
        .is_some_and(|anchor| anchor != document.cursor.position)
}

/// Where the cursor lands after a toggle over `was`, now `marked`: a selection
/// stays selected for the next mark, Tiptap's way; a bare caret keeps its
/// place in the word it was in, so what is typed next goes ON the word it
/// just marked, never over it.
fn settled(document: &Doc, next: Doc, marked: Range<usize>, was: &Range<usize>) -> Doc {
    if selecting(document) {
        return selected(&next, marked);
    }
    let caret = document.offset(document.cursor.position);
    let into = caret.saturating_sub(was.start).min(marked.len());
    let mut next = next;
    next.cursor = EditorCursor {
        position: next.position_at(marked.start + into),
        selection: None,
    };
    next
}

/// Tiptap's colour mark over the target, in the form it serializes to. A
/// target already inside a colour span is re-tinted in place; `None` strips
/// the span and keeps its text.
pub fn color(document: &Doc, rgb: Option<u32>) -> EditorDecision {
    let range = target(document);
    if on_title(document, &range) || crosses_lines(document, &range) {
        return EditorDecision::Noop;
    }
    let position = document.position_at(range.start);
    let line_start = range.start - position.column as usize;
    let line = document.line(position.line as usize).unwrap_or_default();
    let existing = inline::color_span_at(line, position.column as usize);
    let (strip, body) = match existing {
        Some((source, body)) => (
            line_start + source.start..line_start + source.end,
            line_start + body.start..line_start + body.end,
        ),
        None => (range.clone(), range),
    };
    let inner = document.text[body].to_owned();
    let replacement = match rgb {
        Some(rgb) => inline::color_source(rgb, &inner),
        None => inner.clone(),
    };
    let start = strip.start + replacement.len() - inner.len() - rgb.map_or(0, |_| "</span>".len());
    let mut next = document.clone();
    next.text.replace_range(strip, &replacement);
    finish(document, selected(&next, start..start + inner.len()))
}

fn crosses_lines(document: &Doc, range: &Range<usize>) -> bool {
    document.text[range.clone()].contains('\n')
}

/// `[target](url)` with `url` selected, so the destination is typed straight
/// over it. Skipped on the title and over a link that is already named.
pub fn link(document: &Doc) -> EditorDecision {
    let range = target(document);
    if on_title(document, &range) || crosses_lines(document, &range) {
        return EditorDecision::Noop;
    }
    let position = document.position_at(range.start);
    let already_named = document
        .line(position.line as usize)
        .is_some_and(|line| inline::inside_named_link(line, position.column as usize));
    if already_named {
        return EditorDecision::Noop;
    }
    let label = &document.text[range.clone()];
    let mut next = document.clone();
    next.text
        .replace_range(range.clone(), &format!("[{label}](url)"));
    let url = range.start + label.len() + 3;
    finish(document, selected(&next, url..url + 3))
}

/// A named link at `column` of `line` becomes its label alone. A bare URL is
/// its own text and has nothing to remove.
pub fn unlink(document: &Doc, line: usize, column: usize) -> EditorDecision {
    let Some(text) = document.line(line) else {
        return EditorDecision::Noop;
    };
    let Some((source, label)) = inline::named_link_at(text, column) else {
        return EditorDecision::Noop;
    };
    let base = document.offset(editor::EditorPosition::new(line, 0));
    let label_text = text[label.clone()].to_owned();
    let mut next = document.clone();
    next.text
        .replace_range(base + source.start..base + source.end, &label_text);
    next.cursor = EditorCursor::at(line, source.start + label_text.len());
    finish(document, next)
}

/// Strip every inline marker on `line` — bold, italic, strike, code,
/// highlight and named-link syntax — keeping the words and the block prefix.
pub fn clear(document: &Doc, line: usize) -> EditorDecision {
    finish(document, cleared(document, line))
}

pub(crate) fn cleared(document: &Doc, line: usize) -> Doc {
    let Some(text) = document.line(line) else {
        return document.clone();
    };
    let mut kept = String::with_capacity(text.len());
    let mut at = 0;
    for (range, kind) in inline::document_marks(text) {
        if kind != Inline::Marker {
            continue;
        }
        kept.push_str(&text[at..range.start]);
        at = range.end;
    }
    kept.push_str(&text[at..]);
    if kept == text {
        return document.clone();
    }
    let base = document.offset(editor::EditorPosition::new(line, 0));
    let mut next = document.clone();
    next.text.replace_range(base..base + text.len(), &kept);
    let caret_line = document.cursor.position.line as usize;
    next.cursor = match caret_line == line {
        true => EditorCursor::at(
            line,
            (document.cursor.position.column as usize).min(kept.len()),
        ),
        false => EditorCursor {
            position: document.cursor.position,
            selection: None,
        },
    };
    // A caret clamped into the middle of a character lands on its boundary.
    while !next
        .text
        .is_char_boundary(next.offset(next.cursor.position))
    {
        next.cursor.position.column -= 1;
    }
    next
}
