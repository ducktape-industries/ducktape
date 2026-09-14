//! The page editor's keys and its undo history, as a pure library the guest
//! can answer the host's editor transaction lane with.
//!
//! The host owns the live buffer and pre-empts the keys this module claims
//! (Enter, Tab, Shift+Tab, Backspace). For each it asks [`decide`] with the
//! whole document; the answer is one of the host's own shapes — let the
//! editor do its default, do nothing, or apply byte patches against THAT
//! document — and after the host applies it, [`History::commit`] records the
//! step. The guest keeps the only history: [`History::undo`] and
//! [`History::redo`] are decisions too.
//!
//! Every name here mirrors the `ducktape_view_guest` / `wire` editor API
//! (`EditorDecision`, `EditorPatch`, `EditorCursor`, `EditorPosition`,
//! `EditorHistoryEffect`) so wiring is a rename, not a translation. Nothing
//! here depends on a renderer, the wire, or the host: it compiles for wasm32 and the
//! native tests alike.
//!
//! The transforms preserve Markdown structure: Enter after a list
//! item carries the marker down, on an empty item ends the list, at the end of
//! an unmatched fence closes it; Backspace at the content edge drops the
//! marker rather than the line above; Tab / Shift+Tab move a line one
//! two-space step and keep the caret on its character; every structural key
//! recounts the ordered runs it touched. Columns are UTF-8 byte offsets
//! within their line, as the host's editor state carries them.

use std::ops::Range;

/// A caret or anchor: `column` is a UTF-8 byte offset within `line`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EditorPosition {
    pub line: u32,
    pub column: u32,
}

/// The active caret and, while a range is selected, its fixed anchor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EditorCursor {
    pub position: EditorPosition,
    pub selection: Option<EditorPosition>,
}

/// One replacement against the document the decision was made for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditorPatch {
    pub start_byte: u32,
    pub end_byte: u32,
    pub replacement: String,
}

/// What the step means to the undo stacks. `Native` leaves the grouping to
/// the guest's own policy at commit time (see [`History::commit`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorHistoryEffect {
    Native,
    NewGroup,
    ExtendPrevious,
    Undo,
    Redo,
}

/// The guest's answer to a claimed key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorDecision {
    /// The editor's own edit for the key.
    DefaultEditorAction,
    /// The key is consumed and the document stays as it is.
    Noop,
    /// Sorted, non-overlapping patches against the request's document, and
    /// where the caret lands once they are in.
    Apply {
        patches: Vec<EditorPatch>,
        cursor: EditorCursor,
        history: EditorHistoryEffect,
    },
}

/// The document as the host reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Doc {
    pub text: String,
    pub cursor: EditorCursor,
}

/// The keys this module claims.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Enter,
    Tab,
    ShiftTab,
    Backspace,
}

/// The most patches one decision may carry; past it the run collapses into
/// one span (see [`patches_between`]).
pub const MAX_PATCHES: usize = 256;

/// Keystrokes inside this window coalesce into one undo step — the reference
/// editor's own grouping cadence.
const COALESCE_MS: u64 = 750;
const MAX_STEPS: usize = 200;
/// Snapshots live inside the guest's own snapshot, which the runtime caps
/// at 8 MiB for everything; retained text takes at most 2 MiB, plus the
/// metadata for at most 200 steps.
const MAX_BYTES: usize = 2 * 1024 * 1024;

impl EditorPosition {
    pub fn new(line: usize, column: usize) -> Self {
        Self {
            line: line as u32,
            column: column as u32,
        }
    }
}

impl EditorCursor {
    pub fn at(line: usize, column: usize) -> Self {
        Self {
            position: EditorPosition::new(line, column),
            selection: None,
        }
    }
}

impl Doc {
    pub fn new(text: impl Into<String>, cursor: EditorCursor) -> Self {
        Self {
            text: text.into(),
            cursor,
        }
    }

    /// The lines as the editor counts them: a trailing newline is a final
    /// empty line.
    pub(crate) fn lines(&self) -> Vec<&str> {
        self.text.split('\n').collect()
    }

    pub(crate) fn line(&self, index: usize) -> Option<&str> {
        self.text.split('\n').nth(index)
    }

    /// Byte offset of `position` in `text`, clamped to the line.
    pub(crate) fn offset(&self, position: EditorPosition) -> usize {
        let mut offset = 0;
        for (index, line) in self.lines().iter().enumerate() {
            if index == position.line as usize {
                return offset + (position.column as usize).min(line.len());
            }
            offset += line.len() + 1;
        }
        self.text.len()
    }

    pub(crate) fn position_at(&self, offset: usize) -> EditorPosition {
        let before = &self.text[..offset.min(self.text.len())];
        let line = before.matches('\n').count();
        let column = before.rfind('\n').map_or(offset, |nl| offset - nl - 1);
        EditorPosition::new(line, column)
    }

    /// `start..end` of the selection in byte offsets, when one stands.
    fn selected(&self) -> Option<Range<usize>> {
        let anchor = self.cursor.selection?;
        let (a, b) = (self.offset(anchor), self.offset(self.cursor.position));
        (a != b).then(|| a.min(b)..a.max(b))
    }
}

/// Decide the claimed `key` for `doc`. `history` and `input_time_ms` only
/// name the undo group an applied step joins.
pub fn decide(doc: &Doc, key: Key, history: &History, input_time_ms: u64) -> EditorDecision {
    // `Some(edit)`: the transform took the key. `None`: the editor's own edit
    // for the key, modelled here so a recount below it can ride along.
    let structural = match key {
        Key::Enter => close_fence(doc).or_else(|| continue_list(doc)),
        Key::Backspace => remove_list_marker(doc),
        Key::Tab => Some(shift_indent(doc, 1)),
        Key::ShiftTab => Some(shift_indent(doc, -1)),
    };
    let handled = structural.is_some();
    let Some(edit) = structural.or_else(|| default_edit(doc, key)) else {
        return EditorDecision::DefaultEditorAction;
    };
    let after = apply_edit(doc, &edit);
    let from = list_start(&after, after.cursor.position.line as usize);
    let (recounted, renumbers) = renumber_below(&after, from);
    if !handled && renumbers.is_empty() {
        return EditorDecision::DefaultEditorAction;
    }
    if recounted.text == doc.text && recounted.cursor == doc.cursor {
        return EditorDecision::Noop;
    }
    let patches = patches_between(doc, &edit, &renumbers);
    EditorDecision::Apply {
        patches,
        cursor: recounted.cursor,
        history: history.group_effect(input_time_ms),
    }
}

/// `doc` with an `Apply` decision's patches in and its caret placed — the
/// guest's mirror of what the host's editor will hold.
pub fn apply(doc: &Doc, patches: &[EditorPatch], cursor: EditorCursor) -> Doc {
    let mut text = String::with_capacity(doc.text.len());
    let mut at = 0;
    for patch in patches {
        let (start, end) = (patch.start_byte as usize, patch.end_byte as usize);
        assert!(at <= start && start <= end, "patches sorted and disjoint");
        text.push_str(&doc.text[at..start]);
        text.push_str(&patch.replacement);
        at = end;
    }
    text.push_str(&doc.text[at..]);
    Doc { text, cursor }
}

/// One replacement, in the coordinates of the document it is made on.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Edit {
    range: Range<usize>,
    replacement: String,
    cursor: EditorCursor,
}

fn apply_edit(doc: &Doc, edit: &Edit) -> Doc {
    let mut text = doc.text.clone();
    text.replace_range(edit.range.clone(), &edit.replacement);
    Doc {
        text,
        cursor: edit.cursor,
    }
}

/// The editor's own edit for `key`, or `None` where it would do nothing.
fn default_edit(doc: &Doc, key: Key) -> Option<Edit> {
    let caret = doc.offset(doc.cursor.position);
    match key {
        Key::Enter => {
            let range = doc.selected().unwrap_or(caret..caret);
            let landed = doc.position_at(range.start);
            Some(Edit {
                range,
                replacement: "\n".into(),
                cursor: EditorCursor::at(landed.line as usize + 1, 0),
            })
        }
        Key::Backspace => {
            let range = match doc.selected() {
                Some(range) => range,
                None if caret == 0 => return None,
                None => {
                    let previous = doc.text[..caret].chars().next_back()?;
                    caret - previous.len_utf8()..caret
                }
            };
            let landed = doc.position_at(range.start);
            Some(Edit {
                range,
                replacement: String::new(),
                cursor: EditorCursor {
                    position: landed,
                    selection: None,
                },
            })
        }
        Key::Tab | Key::ShiftTab => None,
    }
}

/// Enter at the end of an UNMATCHED ``` line closes the fence: the newline is
/// typed and a closing ``` appears below the caret, so typing continues INSIDE
/// the fence and the save tick never reads the rest of the page as code.
fn close_fence(doc: &Doc) -> Option<Edit> {
    if doc.cursor.selection.is_some() {
        return None;
    }
    let line = doc.cursor.position.line as usize;
    let text = doc.line(line)?;
    let trimmed = text.trim_start_matches([' ', '\t']);
    let at_line_end = doc.cursor.position.column as usize >= text.len();
    if !trimmed.starts_with("```") || !at_line_end || !has_unclosed_fence(&doc.text) {
        return None;
    }
    let indent = &text[..text.len() - trimmed.len()];
    let caret = doc.offset(doc.cursor.position);
    Some(Edit {
        range: caret..caret,
        replacement: format!("\n\n{indent}```"),
        cursor: EditorCursor::at(line + 1, indent.len()),
    })
}

/// True while the buffer holds an ODD number of fence lines. Line 0 is the
/// title and is never parsed, so it does not count.
fn has_unclosed_fence(text: &str) -> bool {
    let fences = text
        .split('\n')
        .skip(1)
        .filter(|line| line.trim_start_matches([' ', '\t']).starts_with("```"))
        .count();
    fences % 2 == 1
}

/// Enter on a list line carries the marker down; Enter on an EMPTY list item
/// ends the list instead of stacking another empty bullet.
fn continue_list(doc: &Doc) -> Option<Edit> {
    if doc.cursor.selection.is_some() {
        return None;
    }
    let line = doc.cursor.position.line as usize;
    let text = doc.line(line)?;
    let marker = list_marker(text)?;
    let column = doc.cursor.position.column as usize;
    // Splitting mid-marker is not a list continuation, it is ordinary typing.
    if column < marker.content {
        return None;
    }
    let start = doc.offset(EditorPosition::new(line, 0));
    if text[marker.content..].trim().is_empty() {
        return Some(Edit {
            range: start..start + text.len(),
            replacement: String::new(),
            cursor: EditorCursor::at(line, 0),
        });
    }
    let carried = format!("\n{}", marker.next_prefix(text));
    let caret = doc.offset(doc.cursor.position);
    Some(Edit {
        range: caret..caret,
        cursor: EditorCursor::at(line + 1, carried.len() - 1),
        replacement: carried,
    })
}

/// Backspace at the first character of a list item's CONTENT deletes the
/// marker, turning the item into a paragraph — the standard escape from a list
/// that does not also eat the line above.
fn remove_list_marker(doc: &Doc) -> Option<Edit> {
    if doc.cursor.selection.is_some() {
        return None;
    }
    let line = doc.cursor.position.line as usize;
    let text = doc.line(line)?;
    let (indent, content) = match list_marker(text) {
        Some(marker) => (marker.indent, marker.content),
        None => block_prefix(text)?,
    };
    // At the content edge, or anywhere inside the prefix the focused row
    // shows raw (Home lands before a heading's `# `): the shape goes, the
    // line above stays.
    let past_the_prefix = doc.cursor.position.column as usize > content;
    if past_the_prefix {
        return None;
    }
    let start = doc.offset(EditorPosition::new(line, 0));
    Some(Edit {
        range: start + indent..start + content,
        replacement: String::new(),
        cursor: EditorCursor::at(line, indent),
    })
}

/// A heading, quote or callout prefix as `(indent, content)`: the shape
/// Backspace at the content edge drops — the line turns into a paragraph —
/// before it would join the line above, the way a list marker does.
fn block_prefix(text: &str) -> Option<(usize, usize)> {
    let indent = text.len() - text.trim_start_matches([' ', '\t']).len();
    let content = crate::markdown::content_start(text);
    let prefixed = content > indent;
    prefixed.then_some((indent, content))
}

/// Tab / Shift+Tab move the caret's line by one nesting step. Two spaces is
/// the depth unit the block projection speaks.
///
/// An indent the TREE cannot hold is refused as a no-op edit: line 0 is the
/// title, the first body line has no sibling to move under, and any line may
/// only go one step past the line above it.
fn shift_indent(doc: &Doc, steps: i32) -> Edit {
    let line = doc.cursor.position.line as usize;
    let column = doc.cursor.position.column as usize;
    let text = doc.line(line).unwrap_or_default();
    let start = doc.offset(EditorPosition::new(line, 0));
    let noop = Edit {
        range: start..start,
        replacement: String::new(),
        cursor: doc.cursor,
    };
    let indent = text.len() - text.trim_start_matches([' ', '\t']).len();
    if steps > 0 {
        if line <= 1 {
            return noop;
        }
        let ceiling = doc
            .line(line - 1)
            .map_or(0, |above| split_indent(above).0 + 1);
        if split_indent(text).0 + 1 > ceiling {
            return noop;
        }
        return Edit {
            range: start..start,
            replacement: "  ".into(),
            cursor: EditorCursor::at(line, column + 2),
        };
    }
    let removable = indent.min(2);
    if removable == 0 {
        return noop;
    }
    Edit {
        range: start..start + removable,
        replacement: String::new(),
        cursor: EditorCursor::at(line, column.saturating_sub(removable)),
    }
}

/// The first line of the list the edit landed in. Numbering is positional, so
/// a run can only be counted from its own start.
fn list_start(doc: &Doc, line: usize) -> usize {
    let lines = doc.lines();
    let mut start = line.min(lines.len().saturating_sub(1));
    while start > 0 && list_marker(lines[start - 1]).is_some() {
        start -= 1;
    }
    start
}

/// Re-count every ordered run from `from` down, one counter per depth: the
/// recounted document, and each rewritten number as an edit against `doc`
/// (one per changed line, in order).
///
/// A shallower line drops every deeper counter, so a nested list restarts at 1
/// under each parent; a line that is not an ordered item ends the run at its
/// own depth, and the next item there starts a NEW run at 1.
// ponytail: O(lines) per structural key. A page is tens of lines and only a
// line whose number actually moved is rewritten.
fn renumber_below(doc: &Doc, from: usize) -> (Doc, Vec<Edit>) {
    let mut caret = doc.cursor;
    let mut counts: Vec<u64> = Vec::new();
    let mut edits = Vec::new();
    let mut offset = 0;
    for (index, text) in doc.lines().iter().enumerate() {
        let line_start = offset;
        offset += text.len() + 1;
        if index < from {
            continue;
        }
        let (depth, rest) = split_indent(text);
        counts.truncate(depth + 1);
        counts.resize(depth + 1, 0);
        let Some(digits) = ordered_digits(rest) else {
            counts[depth] = 0;
            continue;
        };
        counts[depth] += 1;
        let number = counts[depth].to_string();
        let column = text.len() - rest.len();
        if number == rest[..digits] {
            continue;
        }
        // The caret sits on this line and past the marker: widening or
        // narrowing the number carries the caret with it.
        let past_marker = index == caret.position.line as usize
            && caret.position.column as usize >= column + digits;
        if past_marker {
            caret.position.column = caret
                .position
                .column
                .saturating_add_signed(number.len() as i32 - digits as i32);
        }
        edits.push(Edit {
            range: line_start + column..line_start + column + digits,
            replacement: number,
            cursor: caret,
        });
    }
    let mut text = doc.text.clone();
    for edit in edits.iter().rev() {
        text.replace_range(edit.range.clone(), &edit.replacement);
    }
    (
        Doc {
            text,
            cursor: caret,
        },
        edits,
    )
}

/// The patches that carry `doc` to the recounted document: the key's `edit`
/// and the `renumbers` made on top of it, all in `doc`'s coordinates, sorted
/// and disjoint. A renumber inside the key's replacement edits that
/// replacement; one past it shifts back by the replacement's growth. Past
/// [`MAX_PATCHES`] the run collapses into one span.
fn patches_between(doc: &Doc, edit: &Edit, renumbers: &[Edit]) -> Vec<EditorPatch> {
    let mut replacement = edit.replacement.clone();
    let after_start = edit.range.start + edit.replacement.len();
    let delta = edit.replacement.len() as isize - edit.range.len() as isize;
    let mut before = Vec::new();
    let mut after = Vec::new();
    for renumber in renumbers {
        if renumber.range.end <= edit.range.start {
            before.push(patch(renumber.range.clone(), &renumber.replacement));
        } else if renumber.range.start >= after_start {
            let start = (renumber.range.start as isize - delta) as usize;
            let end = (renumber.range.end as isize - delta) as usize;
            after.push(patch(start..end, &renumber.replacement));
        } else {
            let start = renumber.range.start - edit.range.start;
            let end = renumber.range.end - edit.range.start;
            replacement.replace_range(start..end, &renumber.replacement);
        }
    }
    let mut patches = before;
    if !edit.range.is_empty() || !replacement.is_empty() {
        patches.push(patch(edit.range.clone(), &replacement));
    }
    patches.extend(after);
    if patches.len() > MAX_PATCHES {
        let start = patches[0].start_byte as usize;
        let end = patches[patches.len() - 1].end_byte as usize;
        let recounted = apply(doc, &patches, doc.cursor).text;
        let new_end =
            (end as isize + (recounted.len() as isize - doc.text.len() as isize)) as usize;
        patches = vec![patch(start..end, &recounted[start..new_end])];
    }
    patches
}

fn patch(range: Range<usize>, replacement: &str) -> EditorPatch {
    EditorPatch {
        start_byte: range.start as u32,
        end_byte: range.end as u32,
        replacement: replacement.to_owned(),
    }
}

/// A list line's shape: where its indent ends, where its content starts, and
/// the marker the NEXT item should wear.
struct ListMarker {
    indent: usize,
    content: usize,
    next: String,
}

impl ListMarker {
    fn next_prefix(&self, text: &str) -> String {
        format!("{}{}", &text[..self.indent], self.next)
    }
}

fn list_marker(text: &str) -> Option<ListMarker> {
    let trimmed = text.trim_start_matches([' ', '\t']);
    let indent = text.len() - trimmed.len();
    let bytes = trimmed.as_bytes();
    let (mut cursor, next) = match *bytes.first()? {
        bullet @ (b'-' | b'+' | b'*') => (1, format!("{} ", char::from(bullet))),
        byte if byte.is_ascii_digit() => {
            let digits = bytes
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            let delimiter = *bytes.get(digits)?;
            if !matches!(delimiter, b'.' | b')') {
                return None;
            }
            let number: u64 = trimmed[..digits].parse().ok()?;
            (
                digits + 1,
                format!("{}{} ", number.saturating_add(1), char::from(delimiter)),
            )
        }
        _ => return None,
    };
    if bytes.get(cursor) != Some(&b' ') {
        return None;
    }
    cursor += 1;
    // A task marker carries down UNTICKED — the next thing you write is not
    // already done.
    let ticked = matches!(
        bytes.get(cursor..cursor + 4),
        Some(b"[ ] " | b"[x] " | b"[X] ")
    );
    if ticked {
        cursor += 4;
        return Some(ListMarker {
            indent,
            content: indent + cursor,
            next: format!("{next}[ ] "),
        });
    }
    Some(ListMarker {
        indent,
        content: indent + cursor,
        next,
    })
}

/// A line's leading whitespace as nesting steps: two spaces or one tab per
/// step, and a leftover odd space belongs to the TEXT.
fn split_indent(raw: &str) -> (usize, &str) {
    let mut steps = 0;
    let mut pending = 0;
    let mut consumed = 0;
    for byte in raw.bytes() {
        match byte {
            b' ' => {
                pending += 1;
                if pending == 2 {
                    steps += 1;
                    pending = 0;
                }
            }
            b'\t' => {
                steps += 1;
                pending = 0;
            }
            _ => break,
        }
        consumed += 1;
    }
    (steps, &raw[consumed - pending..])
}

/// `12. text` / `12) text` — how many digits the ordered marker wears. Capped
/// at three so a YEAR stays prose.
fn ordered_digits(trimmed: &str) -> Option<usize> {
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    let rest = trimmed.get(digits..)?;
    let rest = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')'))?;
    rest.starts_with(' ').then_some(digits)
}

/// Bounded undo for the page document — snapshots, not deltas: a page is
/// kilobytes, the budget caps the worst case, and a snapshot restore can never
/// desynchronize the way a mis-rebased delta can. The guest's is the ONLY
/// history: native edits and applied decisions both land here through
/// [`History::commit`].
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct History {
    undo: Vec<Doc>,
    redo: Vec<Doc>,
    bytes: usize,
    group_open_until: Option<u64>,
}

impl History {
    /// A page install replaced the buffer — the stacks belong to the old page.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// The group a step at `input_time_ms` joins: inside the coalescing
    /// window it extends the open group, else it opens a new one.
    pub fn group_effect(&self, input_time_ms: u64) -> EditorHistoryEffect {
        match self.group_open_until {
            Some(until) if input_time_ms < until => EditorHistoryEffect::ExtendPrevious,
            _ => EditorHistoryEffect::NewGroup,
        }
    }

    /// The host applied a step from `before` to `after`: record it. A
    /// `Native` step is grouped by this policy; an explicit `NewGroup` /
    /// `ExtendPrevious` is taken as stated; `Undo` / `Redo` move the stacks
    /// only after the host confirms the requested snapshot. Cancelled decisions
    /// leave them untouched. A text-preserving native commit or `Noop` ack
    /// is no step: it neither opens a group nor
    /// clears the redo lane.
    pub fn commit(
        &mut self,
        before: &Doc,
        after: &Doc,
        history: EditorHistoryEffect,
        input_time_ms: u64,
    ) {
        if matches!(
            history,
            EditorHistoryEffect::Undo | EditorHistoryEffect::Redo
        ) {
            let (source, destination) = if history == EditorHistoryEffect::Undo {
                (&mut self.undo, &mut self.redo)
            } else {
                (&mut self.redo, &mut self.undo)
            };
            if source.last() == Some(after) {
                self.bytes -= source.pop().expect("confirmed history snapshot").text.len();
                self.bytes += before.text.len();
                destination.push(before.clone());
                self.group_open_until = None;
                self.trim();
            }
            return;
        }
        if before.text == after.text {
            return;
        }
        let effect = match history {
            EditorHistoryEffect::Native => self.group_effect(input_time_ms),
            EditorHistoryEffect::Undo | EditorHistoryEffect::Redo => return,
            stated => stated,
        };
        self.group_open_until = Some(input_time_ms.saturating_add(COALESCE_MS));
        if effect == EditorHistoryEffect::ExtendPrevious {
            return;
        }
        self.bytes += before.text.len();
        self.undo.push(before.clone());
        self.bytes -= self.redo.drain(..).map(|doc| doc.text.len()).sum::<usize>();
        self.trim();
    }

    fn trim(&mut self) {
        while self.undo.len() + self.redo.len() > MAX_STEPS || self.bytes > MAX_BYTES {
            let oldest = if self.undo.is_empty() {
                self.redo.remove(0)
            } else {
                self.undo.remove(0)
            };
            self.bytes -= oldest.text.len();
        }
    }

    /// Propose the newest undo snapshot. The host's accepted commit moves it
    /// to the redo stack; merely asking must not consume a cancelled step.
    pub fn undo(&self, current: &Doc) -> Option<EditorDecision> {
        Some(restore(
            current,
            self.undo.last()?,
            EditorHistoryEffect::Undo,
        ))
    }

    /// Propose the newest redo snapshot, with the same commit-only rule.
    pub fn redo(&self, current: &Doc) -> Option<EditorDecision> {
        Some(restore(
            current,
            self.redo.last()?,
            EditorHistoryEffect::Redo,
        ))
    }
}

/// The one patch that turns `current`'s text into `snapshot`'s, over the
/// span that differs.
pub(crate) fn restore(
    current: &Doc,
    snapshot: &Doc,
    history: EditorHistoryEffect,
) -> EditorDecision {
    let (old, new) = (&current.text, &snapshot.text);
    let mut prefix = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !(old.is_char_boundary(prefix) && new.is_char_boundary(prefix)) {
        prefix -= 1;
    }
    let mut suffix = old[prefix..]
        .bytes()
        .rev()
        .zip(new[prefix..].bytes().rev())
        .take_while(|(a, b)| a == b)
        .count();
    while !(old.is_char_boundary(old.len() - suffix) && new.is_char_boundary(new.len() - suffix)) {
        suffix -= 1;
    }
    let patches = if old == new {
        Vec::new()
    } else {
        vec![patch(
            prefix..old.len() - suffix,
            &new[prefix..new.len() - suffix],
        )]
    };
    EditorDecision::Apply {
        patches,
        cursor: snapshot.cursor,
        history,
    }
}
