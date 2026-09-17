//! Inline marks — the rich-text layer inside a single block.
//!
//! A block owns plain text (the `EditorState`'s rope) plus a [`MarkList`]: a
//! set of byte ranges carrying formatting. This mirrors ProseMirror/Tiptap
//! marks, which is what the Notion-like editor is built from, and is rendered
//! by turning the list into non-overlapping `TextDecoration` runs.

use std::mem::Discriminant;
use std::ops::Range;

use gpui_kit::SharedString;

/// A formatting mark, mirroring the Tiptap mark set of the Notion-like editor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkKind {
    Bold,
    Italic,
    Underline,
    Strike,
    Code,
    Link(SharedString),
    /// Background highlight, `None` = the default yellow.
    Highlight(Option<HighlightColor>),
    TextColor(TextColor),
    Superscript,
    Subscript,
    /// A person referenced with `@`; the payload is their id.
    Mention(SharedString),
    /// Text a comment thread hangs off; the payload names the thread.
    Comment(super::comments::ThreadId),
}

impl MarkKind {
    /// Identity ignoring the payload, so `Link("a")` and `Link("b")` are the
    /// same *kind* of mark for toggling and coverage tests.
    pub fn id(&self) -> Discriminant<Self> {
        std::mem::discriminant(self)
    }

    /// Name used in the Tiptap-compatible JSON document format.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Bold => "bold",
            Self::Italic => "italic",
            Self::Underline => "underline",
            Self::Strike => "strike",
            Self::Code => "code",
            Self::Link(_) => "link",
            Self::Highlight(_) => "highlight",
            Self::TextColor(_) => "textStyle",
            Self::Superscript => "superscript",
            Self::Subscript => "subscript",
            Self::Mention(_) => "mention",
            Self::Comment(_) => "comment",
        }
    }

    /// Marks that should not survive a newline / be carried into a new block.
    pub fn is_inclusive(&self) -> bool {
        !matches!(
            self,
            Self::Link(_) | Self::Code | Self::Mention(_) | Self::Comment(_)
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
pub enum HighlightColor {
    Yellow,
    Green,
    Blue,
    Purple,
    Pink,
    Red,
    Gray,
}

impl HighlightColor {
    pub const ALL: [Self; 7] = [
        Self::Yellow,
        Self::Green,
        Self::Blue,
        Self::Purple,
        Self::Pink,
        Self::Red,
        Self::Gray,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Self::Yellow => "Yellow",
            Self::Green => "Green",
            Self::Blue => "Blue",
            Self::Purple => "Purple",
            Self::Pink => "Pink",
            Self::Red => "Red",
            Self::Gray => "Gray",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
pub enum TextColor {
    Default,
    Gray,
    Brown,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
    Pink,
    Red,
}

impl TextColor {
    pub const ALL: [Self; 10] = [
        Self::Default,
        Self::Gray,
        Self::Brown,
        Self::Orange,
        Self::Yellow,
        Self::Green,
        Self::Blue,
        Self::Purple,
        Self::Pink,
        Self::Red,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Gray => "Gray",
            Self::Brown => "Brown",
            Self::Orange => "Orange",
            Self::Yellow => "Yellow",
            Self::Green => "Green",
            Self::Blue => "Blue",
            Self::Purple => "Purple",
            Self::Pink => "Pink",
            Self::Red => "Red",
        }
    }
}

/// A mark applied to a UTF-8 byte range of a block's text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    pub kind: MarkKind,
    pub range: Range<usize>,
}

impl Mark {
    pub fn new(kind: MarkKind, range: Range<usize>) -> Self {
        Self { kind, range }
    }
}

/// An applied text edit, used to move marks with the text they annotate.
#[derive(Clone, Debug)]
pub struct Edit {
    /// Replaced byte range in the pre-edit text.
    pub range: Range<usize>,
    /// Byte length of the inserted text.
    pub new_len: usize,
}

impl Edit {
    pub fn new(range: Range<usize>, new_len: usize) -> Self {
        Self { range, new_len }
    }

    /// Map a pre-edit offset onto the post-edit text.
    ///
    /// Inserted text never joins a mark by itself: a mark's start moves right
    /// past it and its end stays put. Which marks the new text actually takes
    /// is decided separately, from the marks active at the caret, so an input
    /// rule can say "plain from here" without fighting the remapping.
    fn map(&self, offset: usize, bias_right: bool) -> usize {
        if offset < self.range.start {
            return offset;
        }
        if offset > self.range.end {
            return offset - (self.range.end - self.range.start) + self.new_len;
        }
        if bias_right {
            self.range.start + self.new_len
        } else {
            self.range.start
        }
    }
}

/// The marks of one block, kept sorted and normalized (no overlapping or
/// touching ranges of the same mark kind).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MarkList {
    marks: Vec<Mark>,
}

impl MarkList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_marks(marks: Vec<Mark>) -> Self {
        let mut this = Self { marks };
        this.normalize();
        this
    }

    pub fn is_empty(&self) -> bool {
        self.marks.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Mark> {
        self.marks.iter()
    }

    pub fn clear(&mut self) {
        self.marks.clear();
    }

    /// Every mark that covers the whole of `range` (empty range = the marks
    /// that would apply to text typed at that caret position).
    pub fn active(&self, range: &Range<usize>) -> Vec<MarkKind> {
        let mut out = Vec::new();
        for mark in &self.marks {
            let covers = if range.is_empty() {
                // A caret sits "inside" a mark when it is strictly within it or
                // at its trailing edge, which is how typing continues a mark.
                mark.range.start < range.start && range.start <= mark.range.end
            } else {
                mark.range.start <= range.start && mark.range.end >= range.end
            };
            if covers && !out.contains(&mark.kind) {
                out.push(mark.kind.clone());
            }
        }
        out
    }

    /// Whether `kind` (payload ignored) covers all of `range`.
    pub fn has(&self, kind: &MarkKind, range: &Range<usize>) -> bool {
        if range.is_empty() {
            return self.active(range).iter().any(|k| k.id() == kind.id());
        }
        let mut covered = range.start;
        loop {
            let Some(mark) = self
                .marks
                .iter()
                .filter(|m| m.kind.id() == kind.id())
                .find(|m| m.range.start <= covered && m.range.end > covered)
            else {
                return false;
            };
            covered = mark.range.end;
            if covered >= range.end {
                return true;
            }
        }
    }

    /// Value of the payload-carrying mark at `offset`, e.g. a link href.
    pub fn mark_at(&self, kind: &MarkKind, offset: usize) -> Option<&Mark> {
        self.marks
            .iter()
            .find(|m| m.kind.id() == kind.id() && m.range.start <= offset && m.range.end >= offset)
    }

    pub fn add(&mut self, kind: MarkKind, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        // A new payload replaces any old one on the same span.
        self.remove(&kind, &range);
        self.marks.push(Mark::new(kind, range));
        self.normalize();
    }

    pub fn remove(&mut self, kind: &MarkKind, range: &Range<usize>) {
        if range.is_empty() {
            return;
        }
        let mut out = Vec::with_capacity(self.marks.len());
        for mark in self.marks.drain(..) {
            if mark.kind.id() != kind.id()
                || mark.range.end <= range.start
                || mark.range.start >= range.end
            {
                out.push(mark);
                continue;
            }
            if mark.range.start < range.start {
                out.push(Mark::new(mark.kind.clone(), mark.range.start..range.start));
            }
            if mark.range.end > range.end {
                out.push(Mark::new(mark.kind.clone(), range.end..mark.range.end));
            }
        }
        self.marks = out;
        self.normalize();
    }

    /// Tiptap's `toggleMark`: clear when the range is fully covered, else set.
    pub fn toggle(&mut self, kind: MarkKind, range: Range<usize>) -> bool {
        if self.has(&kind, &range) {
            self.remove(&kind, &range);
            false
        } else {
            self.add(kind, range);
            true
        }
    }

    /// Move marks across a text edit.
    pub fn remap(&mut self, edit: &Edit) {
        for mark in &mut self.marks {
            let start = edit.map(mark.range.start, true);
            let end = edit.map(mark.range.end, false);
            mark.range = start..end.max(start);
        }
        self.marks.retain(|m| !m.range.is_empty());
        self.normalize();
    }

    /// Marks of the text after `at`, rebased to 0 — the tail half of a split.
    pub fn split_off(&mut self, at: usize) -> MarkList {
        let mut tail = Vec::new();
        let mut head = Vec::new();
        for mark in self.marks.drain(..) {
            if mark.range.start >= at {
                tail.push(Mark::new(
                    mark.kind,
                    mark.range.start - at..mark.range.end - at,
                ));
            } else if mark.range.end <= at {
                head.push(mark);
            } else {
                head.push(Mark::new(mark.kind.clone(), mark.range.start..at));
                tail.push(Mark::new(mark.kind, 0..mark.range.end - at));
            }
        }
        self.marks = head;
        self.normalize();
        MarkList::from_marks(tail)
    }

    /// Append `other`'s marks, shifted by `offset` — the merge of two blocks.
    pub fn extend_from(&mut self, other: &MarkList, offset: usize) {
        for mark in &other.marks {
            self.marks.push(Mark::new(
                mark.kind.clone(),
                mark.range.start + offset..mark.range.end + offset,
            ));
        }
        self.normalize();
    }

    /// Marks covering `range`, rebased to 0 — used when copying a slice out.
    pub fn slice(&self, range: &Range<usize>) -> MarkList {
        let marks = self
            .marks
            .iter()
            .filter_map(|mark| {
                let start = mark.range.start.max(range.start);
                let end = mark.range.end.min(range.end);
                (start < end)
                    .then(|| Mark::new(mark.kind.clone(), start - range.start..end - range.start))
            })
            .collect();
        MarkList::from_marks(marks)
    }

    /// Clamp every mark into `len`, dropping the empties.
    pub fn clamp(&mut self, len: usize) {
        for mark in &mut self.marks {
            mark.range.start = mark.range.start.min(len);
            mark.range.end = mark.range.end.min(len);
        }
        self.marks.retain(|m| !m.range.is_empty());
        self.normalize();
    }

    /// Ordered, non-overlapping runs with the full mark set of each run.
    pub fn runs(&self) -> Vec<(Range<usize>, Vec<MarkKind>)> {
        if self.marks.is_empty() {
            return Vec::new();
        }
        let mut edges: Vec<usize> = Vec::with_capacity(self.marks.len() * 2);
        for mark in &self.marks {
            edges.push(mark.range.start);
            edges.push(mark.range.end);
        }
        edges.sort_unstable();
        edges.dedup();

        let mut runs = Vec::new();
        for pair in edges.windows(2) {
            let (start, end) = (pair[0], pair[1]);
            let kinds: Vec<MarkKind> = self
                .marks
                .iter()
                .filter(|m| m.range.start <= start && m.range.end >= end)
                .map(|m| m.kind.clone())
                .collect();
            if !kinds.is_empty() {
                runs.push((start..end, kinds));
            }
        }
        runs
    }

    fn normalize(&mut self) {
        self.marks.retain(|m| !m.range.is_empty());
        self.marks.sort_by(|a, b| {
            a.range
                .start
                .cmp(&b.range.start)
                .then(a.range.end.cmp(&b.range.end))
        });

        let mut merged: Vec<Mark> = Vec::with_capacity(self.marks.len());
        for mark in self.marks.drain(..) {
            let joinable = merged
                .iter_mut()
                .find(|m| m.kind == mark.kind && m.range.end >= mark.range.start);
            match joinable {
                Some(prev) => prev.range.end = prev.range.end.max(mark.range.end),
                None => merged.push(mark),
            }
        }
        self.marks = merged;
    }
}

/// Diff two revisions of a block's text into a single replace edit.
///
/// The input states only report "something changed", so the edit is recovered
/// from the common prefix/suffix, which is exact for the single-caret typing,
/// paste and delete operations an input can perform.
pub fn diff_edit(old: &str, new: &str) -> Option<Edit> {
    if old == new {
        return None;
    }
    let max_prefix = old.len().min(new.len());
    let mut prefix = 0;
    while prefix < max_prefix && old.as_bytes()[prefix] == new.as_bytes()[prefix] {
        prefix += 1;
    }
    while prefix > 0 && (!old.is_char_boundary(prefix) || !new.is_char_boundary(prefix)) {
        prefix -= 1;
    }

    let mut suffix = 0;
    let max_suffix = max_prefix - prefix;
    while suffix < max_suffix
        && old.as_bytes()[old.len() - 1 - suffix] == new.as_bytes()[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    while suffix > 0
        && (!old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix))
    {
        suffix -= 1;
    }

    Some(Edit::new(
        prefix..old.len() - suffix,
        new.len() - suffix - prefix,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bold(range: Range<usize>) -> Mark {
        Mark::new(MarkKind::Bold, range)
    }

    #[test]
    fn toggle_sets_then_clears() {
        let mut marks = MarkList::new();
        assert!(marks.toggle(MarkKind::Bold, 0..5));
        assert!(marks.has(&MarkKind::Bold, &(0..5)));
        assert!(!marks.toggle(MarkKind::Bold, 0..5));
        assert!(marks.is_empty());
    }

    #[test]
    fn partial_coverage_sets_the_whole_range() {
        let mut marks = MarkList::from_marks(vec![bold(0..3)]);
        assert!(!marks.has(&MarkKind::Bold, &(0..6)));
        marks.toggle(MarkKind::Bold, 0..6);
        assert_eq!(marks.iter().count(), 1);
        assert!(marks.has(&MarkKind::Bold, &(0..6)));
    }

    #[test]
    fn removing_the_middle_splits_a_mark() {
        let mut marks = MarkList::from_marks(vec![bold(0..10)]);
        marks.remove(&MarkKind::Bold, &(3..6));
        let ranges: Vec<_> = marks.iter().map(|m| m.range.clone()).collect();
        assert_eq!(ranges, vec![0..3, 6..10]);
    }

    #[test]
    fn adjacent_marks_of_one_kind_merge() {
        let marks = MarkList::from_marks(vec![bold(0..3), bold(3..6)]);
        assert_eq!(marks.iter().count(), 1);
        assert!(marks.has(&MarkKind::Bold, &(0..6)));
    }

    #[test]
    fn insertion_before_shifts_marks() {
        let mut marks = MarkList::from_marks(vec![bold(5..10)]);
        marks.remap(&Edit::new(0..0, 3));
        assert_eq!(marks.iter().next().unwrap().range, 8..13);
    }

    #[test]
    fn remapping_alone_does_not_extend_a_mark() {
        let mut marks = MarkList::from_marks(vec![bold(0..5)]);
        marks.remap(&Edit::new(5..5, 1));
        assert_eq!(marks.iter().next().unwrap().range, 0..5);
    }

    #[test]
    fn typing_before_a_mark_does_not_extend_it() {
        let mut marks = MarkList::from_marks(vec![bold(5..10)]);
        marks.remap(&Edit::new(5..5, 2));
        assert_eq!(marks.iter().next().unwrap().range, 7..12);
    }

    #[test]
    fn deleting_across_a_mark_trims_it() {
        let mut marks = MarkList::from_marks(vec![bold(4..10)]);
        marks.remap(&Edit::new(2..6, 0));
        assert_eq!(marks.iter().next().unwrap().range, 2..6);
    }

    #[test]
    fn splitting_divides_marks() {
        let mut marks = MarkList::from_marks(vec![bold(2..8)]);
        let tail = marks.split_off(5);
        assert_eq!(marks.iter().next().unwrap().range, 2..5);
        assert_eq!(tail.iter().next().unwrap().range, 0..3);
    }

    #[test]
    fn merging_shifts_the_appended_marks() {
        let mut head = MarkList::from_marks(vec![bold(0..2)]);
        let tail = MarkList::from_marks(vec![Mark::new(MarkKind::Italic, 0..3)]);
        head.extend_from(&tail, 5);
        let marks: Vec<_> = head.iter().cloned().collect();
        assert_eq!(marks[1], Mark::new(MarkKind::Italic, 5..8));
    }

    #[test]
    fn runs_merge_overlapping_kinds() {
        let marks = MarkList::from_marks(vec![bold(0..6), Mark::new(MarkKind::Italic, 3..9)]);
        let runs = marks.runs();
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].0, 0..3);
        assert_eq!(runs[1].1.len(), 2);
        assert_eq!(runs[2].0, 6..9);
    }

    #[test]
    fn active_marks_continue_at_a_caret_inside_them() {
        let marks = MarkList::from_marks(vec![bold(0..5)]);
        assert_eq!(marks.active(&(5..5)), vec![MarkKind::Bold]);
        assert!(marks.active(&(0..0)).is_empty());
    }

    #[test]
    fn diff_finds_an_insertion() {
        let edit = diff_edit("hello world", "hello brave world").unwrap();
        assert_eq!(edit.range, 6..6);
        assert_eq!(edit.new_len, 6);
    }

    #[test]
    fn diff_finds_a_deletion() {
        let edit = diff_edit("hello world", "hello").unwrap();
        assert_eq!(edit.range, 5..11);
        assert_eq!(edit.new_len, 0);
    }

    #[test]
    fn diff_respects_char_boundaries() {
        let edit = diff_edit("héllo", "héllo!").unwrap();
        assert_eq!(edit.new_len, 1);
        assert_eq!(edit.range, 6..6);
    }
}
