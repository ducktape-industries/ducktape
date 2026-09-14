//! Pure Pages menu edits. The binding proposes these edits; Commit owns history.
use crate::editor::{self, Doc, EditorCursor, EditorDecision, EditorHistoryEffect, EditorPosition};
use std::ops::Range;

/// The existing Pages block palette, shared by painting and its edit reducer.
pub const TURNS: &[(&str, &str, &str)] = &[
    ("text", "Text", ""),
    ("h1", "Heading 1", "# "),
    ("h2", "Heading 2", "## "),
    ("h3", "Heading 3", "### "),
    ("todo", "To-do list", "- [ ] "),
    ("bullet", "Bulleted list", "- "),
    ("number", "Numbered list", "1. "),
    ("toggle", "Toggle list", "+ "),
    ("quote", "Quote", "> "),
    ("callout", "Callout", "!> "),
    ("code", "Code", "```"),
    ("divider", "Divider", "---"),
];

fn finish(before: &Doc, after: Doc) -> EditorDecision {
    if before == &after {
        EditorDecision::Noop
    } else {
        editor::restore(before, &after, EditorHistoryEffect::NewGroup)
    }
}

fn replace(document: &Doc, range: Range<usize>, replacement: &str) -> Doc {
    let mut next = document.clone();
    next.text.replace_range(range.clone(), replacement);
    next.cursor = EditorCursor {
        position: next.position_at(range.start + replacement.len()),
        selection: None,
    };
    next
}

/// Rewrite one line's block marker while retaining its content and indentation.
pub fn turn(document: &Doc, line: usize, tag: &str) -> EditorDecision {
    finish(document, turned(document, line, tag))
}

fn turned(document: &Doc, line: usize, tag: &str) -> Doc {
    let Some(text) = document.line(line) else {
        return document.clone();
    };
    let trimmed = text.trim_start_matches([' ', '\t']);
    if trimmed.starts_with("```") {
        return document.clone();
    }
    let indent = &text[..text.len() - trimmed.len()];
    let content = strip_marker(trimmed);
    let Some((_, _, marker)) = TURNS.iter().find(|(name, ..)| *name == tag) else {
        return document.clone();
    };
    let (replacement, caret) = match *marker {
        "```" => (
            if content.is_empty() {
                format!("{indent}```\n{indent}```")
            } else {
                format!("{indent}```\n{indent}{content}\n{indent}```")
            },
            Some(EditorCursor::at(line + 1, indent.len() + content.len())),
        ),
        "---" => (
            if content.is_empty() {
                format!("{indent}---")
            } else {
                format!("{indent}---\n{indent}{content}")
            },
            None,
        ),
        _ => (format!("{indent}{marker}{content}"), None),
    };
    let start = document.offset(EditorPosition::new(line, 0));
    let mut next = replace(document, start..start + text.len(), &replacement);
    if let Some(cursor) = caret {
        next.cursor = cursor;
    }
    next
}

pub fn turn_from_slash(
    document: &Doc,
    line: usize,
    start: usize,
    slashed: bool,
    tag: &str,
) -> EditorDecision {
    let Some(row) = document.line(line) else {
        return EditorDecision::Noop;
    };
    let end = document.cursor.position.column as usize;
    if document.cursor.position.line as usize != line
        || row.get(start..end).is_none()
        || (slashed && !row[start..end].starts_with('/'))
        || !TURNS.iter().any(|(name, ..)| *name == tag)
    {
        return EditorDecision::Noop;
    }
    let offset = document.offset(EditorPosition::new(line, 0));
    // An empty palette filter is an empty replacement, never Backspace: the
    // preceding block must survive a pick from the gutter's fresh blank line.
    let stripped = replace(document, offset + start..offset + end, "");
    finish(document, turned(&stripped, line, tag))
}

fn strip_marker(text: &str) -> &str {
    const MARKERS: &[&str] = &[
        "### ", "## ", "# ", "- [x] ", "- [X] ", "- [ ] ", "!> ", "> ", "- ", "+ ", "* ",
    ];
    if let Some(rest) = MARKERS.iter().find_map(|marker| text.strip_prefix(marker)) {
        return rest;
    }
    if text == "---" {
        return "";
    }
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if (1..=2).contains(&digits) {
        let rest = &text[digits..];
        if let Some(rest) = rest
            .strip_prefix('.')
            .or_else(|| rest.strip_prefix(')'))
            .and_then(|rest| rest.strip_prefix(' '))
        {
            return rest;
        }
    }
    text
}

pub fn toggle_todo(document: &Doc, line: usize) -> EditorDecision {
    let Some(text) = document.line(line) else {
        return EditorDecision::Noop;
    };
    let trimmed = text.trim_start_matches([' ', '\t']);
    if !["- [ ] ", "- [x] ", "- [X] "]
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
    {
        return EditorDecision::Noop;
    }
    let tick = text.len() - trimmed.len() + 3;
    let replacement = if text[tick..].starts_with(['x', 'X']) {
        " "
    } else {
        "x"
    };
    let start = document.offset(EditorPosition::new(line, tick));
    finish(document, replace(document, start..start + 1, replacement))
}

fn spans(document: &Doc) -> Vec<Range<usize>> {
    spans_text(&document.text)
}
fn spans_text(text: &str) -> Vec<Range<usize>> {
    let lines: Vec<_> = text.split('\n').collect();
    let mut result = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let end = if lines[index]
            .trim_start_matches([' ', '\t'])
            .starts_with("```")
        {
            (index + 1..lines.len())
                .find(|&line| {
                    lines[line]
                        .trim_start_matches([' ', '\t'])
                        .starts_with("```")
                })
                .map_or(lines.len(), |close| close + 1)
        } else {
            index + 1
        };
        result.push(index..end);
        index = end;
    }
    result
}
fn span(document: &Doc, line: usize) -> Option<Range<usize>> {
    spans(document)
        .into_iter()
        .find(|span| span.contains(&line))
}
fn rebuilt(lines: &[&str], caret: usize) -> Doc {
    Doc::new(
        lines.join("\n"),
        EditorCursor::at(caret.min(lines.len().saturating_sub(1)), 0),
    )
}

pub fn insert_below(document: &Doc, line: usize) -> EditorDecision {
    let Some(span) = span(document, line) else {
        return EditorDecision::Noop;
    };
    let last = span.end - 1;
    let column = document.line(last).map_or(0, str::len);
    let offset = document.offset(EditorPosition::new(last, column));
    finish(document, replace(document, offset..offset, "\n"))
}

pub fn delete(document: &Doc, line: usize) -> EditorDecision {
    let Some(span) = span(document, line).filter(|span| !span.contains(&0)) else {
        return EditorDecision::Noop;
    };
    let mut lines = document.lines();
    lines.drain(span.clone());
    finish(document, rebuilt(&lines, span.start))
}

pub fn duplicate(document: &Doc, line: usize) -> EditorDecision {
    let Some(span) = span(document, line).filter(|span| !span.contains(&0)) else {
        return EditorDecision::Noop;
    };
    let mut lines = document.lines();
    let copied = lines[span.clone()].to_vec();
    lines.splice(span.end..span.end, copied);
    finish(document, rebuilt(&lines, span.end))
}

pub fn move_block(document: &Doc, line: usize, direction: i32) -> EditorDecision {
    if ![-1, 1].contains(&direction) {
        return EditorDecision::Noop;
    }
    let spans = spans(document);
    let Some(index) = spans.iter().position(|span| span.contains(&line)) else {
        return EditorDecision::Noop;
    };
    let Some(neighbor) = index
        .checked_add_signed(direction as isize)
        .filter(|&i| i < spans.len())
    else {
        return EditorDecision::Noop;
    };
    if spans[index].contains(&0) || spans[neighbor].contains(&0) {
        return EditorDecision::Noop;
    }
    let (first, second) = if direction < 0 {
        (&spans[neighbor], &spans[index])
    } else {
        (&spans[index], &spans[neighbor])
    };
    let lines = document.lines();
    let mut next = lines[..first.start].to_vec();
    let landing = if direction < 0 {
        first.start
    } else {
        first.start + second.len()
    };
    next.extend_from_slice(&lines[second.clone()]);
    next.extend_from_slice(&lines[first.clone()]);
    next.extend_from_slice(&lines[second.end..]);
    finish(document, rebuilt(&next, landing))
}

pub fn drop_boundaries(document: &Doc) -> Vec<usize> {
    drop_boundaries_text(&document.text)
}
pub fn drop_boundaries_text(text: &str) -> Vec<usize> {
    let mut boundaries: Vec<_> = spans_text(text)
        .into_iter()
        .map(|span| span.start)
        .filter(|&line| line > 0)
        .collect();
    boundaries.push(text.split('\n').count());
    boundaries
}

pub fn drop_move(document: &Doc, line: usize, boundary: usize) -> EditorDecision {
    let Some(span) = span(document, line).filter(|span| !span.contains(&0)) else {
        return EditorDecision::Noop;
    };
    if (span.start..=span.end).contains(&boundary) || !drop_boundaries(document).contains(&boundary)
    {
        return EditorDecision::Noop;
    }
    let mut lines = document.lines();
    let block = lines[span.clone()].to_vec();
    lines.drain(span.clone());
    let landing = if boundary > span.end {
        boundary - span.len()
    } else {
        boundary
    };
    lines.splice(landing..landing, block);
    finish(document, rebuilt(&lines, landing))
}

/// Reset formatting: the block becomes plain text and its inline marks go.
pub fn clear_block(document: &Doc, line: usize) -> EditorDecision {
    let plain = turned(document, line, "text");
    finish(document, crate::format::cleared(&plain, line))
}

/// The slash palette past the block turns: what `/mention` and `/emoji`
/// open instead of a block.
const SLASH_EXTRAS: &[(&str, &str, &str)] = &[("mention", "Mention", "@"), ("emoji", "Emoji", ":")];

/// The floating format menu over a selection — Tiptap's floating toolbar
/// as a list the keyboard can walk.
const FORMAT_ITEMS: &[(&str, &str)] = &[
    ("bold", "Bold"),
    ("italic", "Italic"),
    ("strike", "Strikethrough"),
    ("underline", "Underline"),
    ("code", "Code"),
    ("highlight", "Highlight"),
    ("color", "Text color"),
    ("link", "Link"),
    ("comment", "Comment"),
    ("ai", "Ask AI…"),
    ("align", "Align…"),
    ("turn", "Turn into…"),
    ("clear", "Clear formatting"),
];

/// The alignment submenu: the three sides the wire can lay a line on.
const ALIGN_ITEMS: &[(&str, &str)] = &[
    ("left", "Align left"),
    ("center", "Align center"),
    ("right", "Align right"),
];

/// The colour submenu: the Notion palette Tiptap's template ships, as the
/// `0xRRGGBB` each colour span is written with. Default strips the span.
const COLORS: &[(&str, &str, Option<u32>)] = &[
    ("default", "Default", None),
    ("gray", "Gray", Some(0x787774)),
    ("brown", "Brown", Some(0x9f6b53)),
    ("orange", "Orange", Some(0xd9730d)),
    ("yellow", "Yellow", Some(0xcb912f)),
    ("green", "Green", Some(0x448361)),
    ("blue", "Blue", Some(0x337ea9)),
    ("purple", "Purple", Some(0x9065b0)),
    ("pink", "Pink", Some(0xc14c8a)),
    ("red", "Red", Some(0xd44c47)),
];

/// The link popover: what a press on a link offers instead of navigating.
const LINK_ITEMS: &[(&str, &str)] = &[
    ("open", "Open link"),
    ("copy", "Copy link"),
    ("unlink", "Remove link"),
];

const BLOCK_ITEMS: &[(&str, &str)] = &[
    ("turn", "Turn into…"),
    ("align", "Align…"),
    ("comment", "Comment"),
    ("ai", "Ask AI…"),
    ("copy", "Copy to clipboard"),
    ("clear", "Reset formatting"),
    ("duplicate", "Duplicate"),
    ("move-up", "Move up"),
    ("move-down", "Move down"),
    ("delete", "Delete"),
];

/// GitHub's short codes for the emoji the `:` picker completes.
// ponytail: a curated table; the full GitHub set is the upgrade if a search
// ever comes up empty.
pub const EMOJI: &[(&str, &str)] = &[
    ("+1", "👍"),
    ("-1", "👎"),
    ("100", "💯"),
    ("bug", "🐛"),
    ("bulb", "💡"),
    ("check", "✅"),
    ("clap", "👏"),
    ("construction", "🚧"),
    ("cry", "😢"),
    ("duck", "🦆"),
    ("eyes", "👀"),
    ("fire", "🔥"),
    ("grin", "😁"),
    ("heart", "❤️"),
    ("hourglass", "⏳"),
    ("joy", "😂"),
    ("laughing", "😆"),
    ("lock", "🔒"),
    ("memo", "📝"),
    ("muscle", "💪"),
    ("neutral_face", "😐"),
    ("no_entry", "⛔"),
    ("ok_hand", "👌"),
    ("party", "🥳"),
    ("pray", "🙏"),
    ("question", "❓"),
    ("rocket", "🚀"),
    ("smile", "😄"),
    ("sob", "😭"),
    ("sparkles", "✨"),
    ("star", "⭐"),
    ("sunglasses", "😎"),
    ("tada", "🎉"),
    ("thinking", "🤔"),
    ("thumbsup", "👍"),
    ("wave", "👋"),
    ("warning", "⚠️"),
    ("white_check_mark", "✅"),
    ("x", "❌"),
    ("zap", "⚡"),
];

/// The most names or emoji one list shows: the host's own row cap.
const MENU_ROWS: usize = 64;

/// Menu state belongs to the guest snapshot. Proposed edits return a successor;
/// callers adopt it only after Commit (or the read-only interaction callback).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Menu {
    open: Option<Open>,
    /// The members a mention completes to, handed in by the binding.
    names: Vec<String>,
    /// The active agents "Ask AI" addresses — display name and program
    /// account — handed in by the binding.
    agents: Vec<(String, u64)>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Open {
    kind: Kind,
    selected: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum Kind {
    Slash {
        line: usize,
        strip: usize,
        slashed: bool,
    },
    Block {
        line: usize,
    },
    Turn {
        line: usize,
    },
    /// The floating toolbar: inline marks over the selection on `line`.
    Format {
        line: usize,
    },
    /// The toolbar's colour palette over the selection on `line`.
    Color {
        line: usize,
    },
    /// The alignment submenu: over the one block at `line` from the block
    /// menu, else over the selection from the toolbar.
    Align {
        line: usize,
        block: bool,
    },
    /// The agent picker behind "Ask AI": a comment on the block at `line`
    /// from the block menu, else on the selection from the toolbar, addressed
    /// to the agent picked.
    Ai {
        line: usize,
        block: bool,
    },
    /// The popover over the link at `column` of `line`.
    Link {
        line: usize,
        column: usize,
    },
    /// `@` typed at `strip`, completing a member name.
    Mention {
        line: usize,
        strip: usize,
    },
    /// `:` typed at `strip`, completing a short code.
    Emoji {
        line: usize,
        strip: usize,
    },
}

pub struct MenuView {
    /// None anchors at the caret; Some anchors at the specified document line.
    pub line: Option<usize>,
    pub items: Vec<(String, String)>,
    pub selected: usize,
}

/// What a pick asks the host for besides an edit: a link to open, text to
/// copy, a comment to start and the exact text it anchors on.
pub type Intent = crate::document_sync::Navigation;

fn owned(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(tag, label)| ((*tag).to_owned(), (*label).to_owned()))
        .collect()
}

impl Menu {
    fn opened(kind: Kind) -> Self {
        Self {
            open: Some(Open { kind, selected: 0 }),
            names: Vec::new(),
            agents: Vec::new(),
        }
    }

    fn reopen(&self, kind: Kind) -> Self {
        Self {
            names: self.names.clone(),
            agents: self.agents.clone(),
            ..Self::opened(kind)
        }
    }

    fn closed(&self) -> Self {
        Self {
            open: None,
            names: self.names.clone(),
            agents: self.agents.clone(),
        }
    }

    /// The members a mention may complete to. Bounded: the list rides in
    /// every menu snapshot.
    pub fn with_names(mut self, names: &[String]) -> Self {
        self.names = names.iter().take(256).cloned().collect();
        self
    }

    /// The agents "Ask AI" may address, bounded like the names.
    pub fn with_agents(mut self, agents: &[(String, u64)]) -> Self {
        self.agents = agents.iter().take(MENU_ROWS).cloned().collect();
        self
    }

    pub fn close(&mut self) {
        self.open = None;
    }

    /// A caret move: the format menu floats over a standing selection —
    /// Tiptap's bubble toolbar — and nothing floats over a bare caret.
    pub fn moved(&mut self, document: &Doc) {
        let selecting = document
            .cursor
            .selection
            .is_some_and(|anchor| anchor != document.cursor.position);
        match selecting {
            true => self.format(document),
            false => self.close(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    pub fn current(&self, document: &Doc) -> Option<MenuView> {
        let open = self.open.as_ref()?;
        let (line, items) = match open.kind {
            Kind::Slash {
                line,
                strip,
                slashed,
            } => (
                None,
                slash_items(slash_filter(document, line, strip, slashed)?),
            ),
            Kind::Block { line } => (Some(line), block_items(document, line)?),
            Kind::Turn { line } => (Some(line), turn_items("")),
            Kind::Format { .. } => (None, owned(FORMAT_ITEMS)),
            Kind::Color { .. } => (
                None,
                COLORS
                    .iter()
                    .map(|(tag, label, _)| ((*tag).to_owned(), (*label).to_owned()))
                    .collect(),
            ),
            Kind::Align { line, block } => (block.then_some(line), owned(ALIGN_ITEMS)),
            Kind::Ai { line, block } => (block.then_some(line), agent_items(&self.agents)),
            Kind::Link { line, .. } => (Some(line), owned(LINK_ITEMS)),
            Kind::Mention { line, strip } => (
                None,
                mention_items(&self.names, trigger_filter(document, line, strip)?),
            ),
            Kind::Emoji { line, strip } => {
                (None, emoji_items(trigger_filter(document, line, strip)?))
            }
        };
        if items.is_empty() {
            return None;
        }
        let selected = open.selected.min(items.len() - 1);
        Some(MenuView {
            line,
            items,
            selected,
        })
    }

    pub fn block(&mut self, document: &Doc, line: usize) {
        if document.line(line).is_some() && line > 0 {
            *self = self.reopen(Kind::Block { line });
        }
    }

    /// The floating toolbar, at the caret. The title takes no marks.
    pub fn format(&mut self, document: &Doc) {
        let line = document.cursor.position.line as usize;
        if line > 0 {
            *self = self.reopen(Kind::Format { line });
        }
    }

    /// The popover over a pressed link — nothing opens where no link is.
    pub fn link(&mut self, document: &Doc, line: usize, column: usize) {
        let on_link = document
            .line(line)
            .and_then(|text| crate::inline::document_link_at(text, column))
            .is_some();
        if on_link {
            *self = self.reopen(Kind::Link { line, column });
        }
    }

    pub fn select(&mut self, document: &Doc, selected: usize) {
        if let Some(view) = self.current(document)
            && let Some(open) = self.open.as_mut()
        {
            open.selected = selected.min(view.items.len() - 1);
        }
    }

    /// Called only for an accepted text edit, with the trigger character the
    /// edit ended on (`/`, `@`, `:`) if any. Caret/navigation and source
    /// replacement call close instead; a paste containing a trigger does not
    /// open.
    pub fn after_edit(&mut self, document: &Doc, trigger: Option<char>) {
        let kind = self.open.as_ref().map(|open| open.kind);
        match kind {
            None => self.after_closed_edit(document, trigger),
            Some(Kind::Slash { .. } | Kind::Mention { .. } | Kind::Emoji { .. }) => {
                self.retain_filter(document)
            }
            Some(
                Kind::Block { .. }
                | Kind::Turn { .. }
                | Kind::Format { .. }
                | Kind::Color { .. }
                | Kind::Align { .. }
                | Kind::Ai { .. }
                | Kind::Link { .. },
            ) => self.close(),
        }
    }

    fn after_closed_edit(&mut self, document: &Doc, trigger: Option<char>) {
        let Some(trigger) = trigger else {
            return;
        };
        let Some(strip) = document.cursor.position.column.checked_sub(1) else {
            return;
        };
        let line = document.cursor.position.line as usize;
        let strip = strip as usize;
        let kind = match trigger {
            '/' => Kind::Slash {
                line,
                strip,
                slashed: true,
            },
            '@' => Kind::Mention { line, strip },
            ':' => Kind::Emoji { line, strip },
            _ => return,
        };
        let opens = match kind {
            Kind::Slash { .. } => slash_filter(document, line, strip, true).is_some(),
            // A mention or an emoji starts a word: `a@b` and `12:30` are prose.
            _ => line > 0 && at_word_start(document, line, strip),
        };
        if opens {
            *self = self.reopen(kind);
        }
    }

    fn retain_filter(&mut self, document: &Doc) {
        let Some(open) = self.open.as_ref() else {
            return;
        };
        // A picker whose filter is still legal stays open with no rows: the
        // emoji list shows nothing until a letter narrows it.
        let filter_stands = match open.kind {
            Kind::Mention { line, strip } | Kind::Emoji { line, strip } => {
                trigger_filter(document, line, strip).is_some()
            }
            _ => self.current(document).is_some(),
        };
        match filter_stands {
            true => self.select(document, open.selected),
            false => self.close(),
        }
    }

    pub fn plus(&self, document: &Doc, line: usize) -> (EditorDecision, Self) {
        let decision = insert_below(document, line);
        let next = match &decision {
            EditorDecision::Apply { cursor, .. } => self.reopen(Kind::Slash {
                line: cursor.position.line as usize,
                strip: cursor.position.column as usize,
                slashed: false,
            }),
            _ => self.clone(),
        };
        (decision, next)
    }

    pub fn pick(&self, document: &Doc, tag: &str) -> (EditorDecision, Self) {
        let offered = self
            .current(document)
            .is_some_and(|view| view.items.iter().any(|item| item.0 == tag));
        if !offered {
            return (EditorDecision::Noop, self.clone());
        }
        let Some(open) = self.open.as_ref() else {
            return (EditorDecision::Noop, self.clone());
        };
        match open.kind {
            Kind::Slash {
                line,
                strip,
                slashed,
            } => self.pick_slash(document, line, strip, slashed, tag),
            Kind::Turn { line } => (turn(document, line, tag), self.closed()),
            Kind::Block { line } => self.pick_block(document, line, tag),
            Kind::Format { line } => self.pick_format(document, line, tag),
            Kind::Color { .. } => {
                let rgb = COLORS
                    .iter()
                    .find(|(name, _, _)| *name == tag)
                    .and_then(|(_, _, rgb)| *rgb);
                (crate::format::color(document, rgb), self.closed())
            }
            Kind::Align { line, block } => {
                let Some(align) = crate::markdown::Align::from_tag(tag) else {
                    return (EditorDecision::Noop, self.clone());
                };
                let decision = match block {
                    true => crate::format::align_lines(document, line..line + 1, align),
                    false => crate::format::align(document, align),
                };
                (decision, self.closed())
            }
            // The pick edits nothing: its intent opens the composer.
            Kind::Ai { .. } => (EditorDecision::Noop, self.closed()),
            Kind::Link { line, column } => {
                let decision = match tag {
                    "unlink" => crate::format::unlink(document, line, column),
                    _ => EditorDecision::Noop,
                };
                (decision, self.closed())
            }
            Kind::Mention { line, strip } => (
                complete(document, line, strip, &format!("@{tag} ")),
                self.closed(),
            ),
            Kind::Emoji { line, strip } => {
                let glyph = EMOJI
                    .iter()
                    .find(|(code, _)| *code == tag)
                    .map_or("", |(_, glyph)| glyph);
                (complete(document, line, strip, glyph), self.closed())
            }
        }
    }

    fn pick_slash(
        &self,
        document: &Doc,
        line: usize,
        strip: usize,
        slashed: bool,
        tag: &str,
    ) -> (EditorDecision, Self) {
        let Some((_, _, trigger)) = SLASH_EXTRAS.iter().find(|(name, ..)| *name == tag) else {
            return (
                turn_from_slash(document, line, strip, slashed, tag),
                self.closed(),
            );
        };
        // The palette text becomes the trigger, and the picker it opens
        // filters on what is typed after it.
        let decision = replace_slash(document, line, strip, slashed, trigger);
        let kind = match *trigger {
            "@" => Kind::Mention { line, strip },
            _ => Kind::Emoji { line, strip },
        };
        (decision, self.reopen(kind))
    }

    fn pick_block(&self, document: &Doc, line: usize, tag: &str) -> (EditorDecision, Self) {
        if tag == "turn" {
            return (EditorDecision::Noop, self.reopen(Kind::Turn { line }));
        }
        if tag == "align" {
            return (
                EditorDecision::Noop,
                self.reopen(Kind::Align { line, block: true }),
            );
        }
        if tag == "ai" {
            return (
                EditorDecision::Noop,
                self.reopen(Kind::Ai { line, block: true }),
            );
        }
        let decision = match tag {
            "comment" | "copy" => EditorDecision::Noop,
            "clear" => clear_block(document, line),
            "duplicate" => duplicate(document, line),
            "move-up" => move_block(document, line, -1),
            "move-down" => move_block(document, line, 1),
            "delete" => delete(document, line),
            _ => return (EditorDecision::Noop, self.clone()),
        };
        (decision, self.closed())
    }

    fn pick_format(&self, document: &Doc, line: usize, tag: &str) -> (EditorDecision, Self) {
        if tag == "turn" {
            return (EditorDecision::Noop, self.reopen(Kind::Turn { line }));
        }
        if tag == "color" {
            return (EditorDecision::Noop, self.reopen(Kind::Color { line }));
        }
        if tag == "align" {
            return (
                EditorDecision::Noop,
                self.reopen(Kind::Align { line, block: false }),
            );
        }
        if tag == "ai" {
            return (
                EditorDecision::Noop,
                self.reopen(Kind::Ai { line, block: false }),
            );
        }
        let decision = match (tag, crate::format::Wrap::from_tag(tag)) {
            (_, Some(wrap)) => crate::format::toggle(document, wrap),
            ("link", None) => crate::format::link(document),
            ("clear", None) => crate::format::clear(document, line),
            ("comment", None) => EditorDecision::Noop,
            _ => return (EditorDecision::Noop, self.clone()),
        };
        (decision, self.closed())
    }

    /// What a pick asks the host for besides the edit. Empty for a pick the
    /// menu is not offering.
    pub fn intent(&self, document: &Doc, tag: &str) -> Intent {
        let mut intent = Intent::default();
        let offered = self
            .current(document)
            .is_some_and(|view| view.items.iter().any(|item| item.0 == tag));
        let Some(open) = self.open.as_ref().filter(|_| offered) else {
            return intent;
        };
        match (open.kind, tag) {
            (Kind::Block { line }, "comment") => intent.comment_line = Some(line as u32),
            (Kind::Block { line }, "copy") => intent.copy = block_text(document, line),
            (Kind::Format { line }, "comment") => {
                intent.comment_line = Some(line as u32);
                intent.anchor = crate::document_sync::text_anchor(
                    document.line(line).unwrap_or_default(),
                    selection_columns(document, line),
                );
            }
            (Kind::Link { line, column }, "open") => intent.link = link_at(document, line, column),
            (Kind::Link { line, column }, "copy") => intent.copy = link_at(document, line, column),
            (Kind::Ai { .. }, NO_AGENTS) => {}
            // An agent pick is a comment addressed to that agent: on the
            // selection from the toolbar, on the whole block otherwise.
            (Kind::Ai { line, block }, account) => {
                intent.comment_line = Some(line as u32);
                intent.mention = account.parse().unwrap_or_default();
                if !block {
                    intent.anchor = crate::document_sync::text_anchor(
                        document.line(line).unwrap_or_default(),
                        selection_columns(document, line),
                    );
                }
            }
            _ => {}
        }
        intent
    }
}

/// The agent picker's rows: the account as the tag, the name as the label.
/// With nobody to ask the picker still opens, on one row that says so — a
/// pick that did nothing at all reads as a broken menu.
fn agent_items(agents: &[(String, u64)]) -> Vec<(String, String)> {
    if agents.is_empty() {
        return vec![(NO_AGENTS.to_owned(), "No active agents".to_owned())];
    }
    agents
        .iter()
        .map(|(name, account)| (account.to_string(), name.clone()))
        .collect()
}

/// The tag of the empty roster's one row: picking it asks nobody.
const NO_AGENTS: &str = "none";

/// The selection's byte columns on `line`, when the whole selection sits on it.
fn selection_columns(document: &Doc, line: usize) -> Option<std::ops::Range<usize>> {
    let range = crate::format::target(document);
    let start = document.position_at(range.start);
    let end = document.position_at(range.end);
    let on_line =
        start.line as usize == line && end.line as usize == line && range.start < range.end;
    on_line.then_some(start.column as usize..end.column as usize)
}

fn link_at(document: &Doc, line: usize, column: usize) -> String {
    document
        .line(line)
        .and_then(|text| crate::inline::document_link_at(text, column))
        .unwrap_or_default()
}

/// The block's source text, a fence whole.
fn block_text(document: &Doc, line: usize) -> String {
    let Some(span) = span(document, line) else {
        return String::new();
    };
    document.lines()[span].join("\n")
}

fn at_word_start(document: &Doc, line: usize, strip: usize) -> bool {
    document
        .line(line)
        .and_then(|text| text.get(..strip))
        .is_some_and(|before| {
            !before
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric)
        })
}

/// The text typed after a `@` or `:` trigger up to the caret, while it is
/// still one handle-shaped word on the trigger's own line.
fn trigger_filter(document: &Doc, line: usize, strip: usize) -> Option<&str> {
    let cursor = document.cursor.position;
    if cursor.line as usize != line {
        return None;
    }
    let text = document.line(line)?;
    let filter = text.get(strip + 1..cursor.column as usize)?;
    filter
        .chars()
        .all(|c| crate::inline::handle_char(c) || c == '+')
        .then_some(filter)
}

fn mention_items(names: &[String], filter: &str) -> Vec<(String, String)> {
    let filter = filter.to_lowercase();
    names
        .iter()
        .filter(|name| name.to_lowercase().contains(&filter))
        .take(MENU_ROWS)
        .map(|name| (name.clone(), format!("@{name}")))
        .collect()
}

fn emoji_items(filter: &str) -> Vec<(String, String)> {
    if filter.is_empty() {
        return Vec::new();
    }
    let filter = filter.to_lowercase();
    EMOJI
        .iter()
        .filter(|(code, _)| code.contains(&filter))
        .take(MENU_ROWS)
        .map(|(code, glyph)| ((*code).to_owned(), format!("{glyph}  :{code}:")))
        .collect()
}

/// Replace the trigger and its filter with what the picker chose.
fn complete(document: &Doc, line: usize, strip: usize, with: &str) -> EditorDecision {
    let cursor = document.cursor.position;
    let end = cursor.column as usize;
    let on_filter = cursor.line as usize == line
        && document
            .line(line)
            .and_then(|text| text.get(strip..end))
            .is_some();
    if !on_filter || with.is_empty() {
        return EditorDecision::Noop;
    }
    let offset = document.offset(EditorPosition::new(line, 0));
    finish(
        document,
        replace(document, offset + strip..offset + end, with),
    )
}

/// The slash text (`/` and its filter) replaced with `with`.
fn replace_slash(
    document: &Doc,
    line: usize,
    start: usize,
    slashed: bool,
    with: &str,
) -> EditorDecision {
    let Some(row) = document.line(line) else {
        return EditorDecision::Noop;
    };
    let end = document.cursor.position.column as usize;
    if document.cursor.position.line as usize != line
        || row.get(start..end).is_none()
        || (slashed && !row[start..end].starts_with('/'))
    {
        return EditorDecision::Noop;
    }
    let offset = document.offset(EditorPosition::new(line, 0));
    finish(
        document,
        replace(document, offset + start..offset + end, with),
    )
}

fn slash_filter(document: &Doc, line: usize, strip: usize, slashed: bool) -> Option<&str> {
    let cursor = document.cursor.position;
    if cursor.line as usize != line {
        return None;
    }
    let text = document.line(line)?;
    if slashed && !text.get(strip..)?.starts_with('/') {
        return None;
    }
    text.get(strip.checked_add(usize::from(slashed))?..cursor.column as usize)
}

fn matches_filter(tag: &str, label: &str, filter: &str) -> bool {
    label.to_ascii_lowercase().contains(filter) || tag.contains(filter)
}

fn turn_items(filter: &str) -> Vec<(String, String)> {
    let filter = filter.to_ascii_lowercase();
    TURNS
        .iter()
        .filter(|(tag, label, _)| matches_filter(tag, label, &filter))
        .map(|(tag, label, _)| ((*tag).to_owned(), (*label).to_owned()))
        .collect()
}

/// The slash palette: every block turn, then the two pickers.
fn slash_items(filter: &str) -> Vec<(String, String)> {
    let lowered = filter.to_ascii_lowercase();
    let mut items = turn_items(filter);
    items.extend(
        SLASH_EXTRAS
            .iter()
            .filter(|(tag, label, _)| matches_filter(tag, label, &lowered))
            .map(|(tag, label, _)| ((*tag).to_owned(), (*label).to_owned())),
    );
    items
}

fn block_items(document: &Doc, line: usize) -> Option<Vec<(String, String)>> {
    let on_fence = document
        .line(line)?
        .trim_start_matches([' ', '\t'])
        .starts_with("```");
    Some(
        BLOCK_ITEMS
            .iter()
            .copied()
            .filter(|(tag, _)| !(on_fence && ["turn", "clear"].contains(tag)))
            .map(|(tag, label)| (tag.to_owned(), label.to_owned()))
            .collect(),
    )
}
