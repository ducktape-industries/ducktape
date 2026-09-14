//! The document text and the block records are the same page in two shapes.
//! This module is the translation, both ways, plus the plan that turns an
//! edited buffer back into the module's own ops.
//!
//! WHY THERE IS NO TREE DIFF HERE. `RemoveBlock` deletes the whole SUBTREE
//! (pages `block_ops::delete_subtree`), and the submit path gives no ordering
//! guarantee across separate requests. A general "reconcile any two trees"
//! engine would have to reparent, and getting that wrong destroys committed
//! records on an append-only chain. So nesting is NEVER inferred from the text:
//! an inserted line adopts the depth of the line above it, depth changes stay
//! explicit `MoveBlock` ops from Tab/Shift+Tab, and a removal that would take a
//! block still holding children is REFUSED rather than guessed at.
//!
//! The pairing is a prefix/suffix trim, not an LCS: equal lines at the head and
//! tail keep their block ids (so a comment anchored to a paragraph survives an
//! edit three lines above it), and only the disturbed middle is paired by
//! position. A wholesale reshuffle costs some ids; it never costs text.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One block of the open page, as this view folds it off the module's own
/// `Block` record: `prefix` is two spaces per depth, the only nesting signal
/// the rendered line carries.
#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct PageBlock {
    pub key: i64,
    pub id: String,
    pub parent: String,
    pub kind: String,
    pub text: String,
    pub checked: bool,
    pub prefix: String,
    pub child_count: i64,
}

/// A margin badge: the document line a commented block starts on, and how
/// many threads sit on it.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentMark {
    pub line: i64,
    pub count: i64,
}

/// Small read-only navigation emitted after the matching editor interaction:
/// a link to open, text to copy, or a comment to start on a line — with the
/// exact selection it anchors on, in the module's UTF-16 units over the
/// block's own text.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Navigation {
    pub link: String,
    pub comment_line: Option<u32>,
    pub copy: String,
    pub anchor: Option<(u32, u32)>,
    /// The agent account an "Ask AI" comment is addressed to; `0` is nobody.
    pub mention: u64,
}

/// A selection's byte columns on a document line as the module's comment
/// anchor: UTF-16 units over the block's text, the marker prefix dropped.
/// Nothing when the selection is empty or sits entirely in the marker.
pub fn text_anchor(line: &str, columns: Option<std::ops::Range<usize>>) -> Option<(u32, u32)> {
    let columns = columns?;
    let (_, rest) = split_indent(line);
    let content = parse_line(rest, 0).text;
    let prefix = line.len() - content.len();
    let start = columns.start.max(prefix);
    let end = columns.end.min(line.len());
    if start >= end || !line.is_char_boundary(start) || !line.is_char_boundary(end) {
        return None;
    }
    let units = |slice: &str| slice.encode_utf16().count() as u32;
    Some((units(&line[prefix..start]), units(&line[prefix..end])))
}

/// Two spaces per depth, matching the prefix the block fold writes.
/// One level of nesting in the document buffer, and in a block's `prefix`.
pub const INDENT: &str = "  ";
const FENCE: &str = "```";

/// One line of the document, resolved to the block vocabulary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub kind: String,
    pub text: String,
    pub checked: bool,
    pub depth: usize,
}

/// A line that is already a record, carrying the id the ops address it by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredLine {
    pub id: String,
    pub has_children: bool,
    pub line: Line,
}

/// One write the document owes the node. Applied strictly in order.
///
/// The three in-place variants are separate ON PURPOSE: each is its own signed
/// transaction, so a fat "update everything" op would bill three writes for
/// fixing one typo. The plan emits only the fields that actually moved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockOp {
    SetText {
        id: String,
        text: String,
    },
    SetKind {
        id: String,
        kind: String,
    },
    SetChecked {
        id: String,
        checked: bool,
    },
    /// A new line. `after` is the block it follows, empty for the page head.
    Insert {
        after: String,
        kind: String,
        text: String,
    },
    /// A line whose indentation moved. ONE step per plan: `block_move`
    /// resolves a direction against the live tree, so a two-step drag
    /// converges over consecutive save ticks rather than guessing a parent.
    Nest {
        id: String,
        direction: String,
    },
    /// A line that is gone. Only ever emitted for a childless block.
    Remove {
        id: String,
    },
}

/// What the buffer wants written, or why it cannot be.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocumentPlan {
    pub ops: Vec<BlockOp>,
    /// Non-empty means: write NOTHING and resync. The document asked for
    /// something the module cannot do without losing records.
    pub refusal: String,
}

/// Subpages are navigation, not prose — they have no markdown spelling, and a
/// text diff has no business deciding they were deleted. They are rendered
/// beside the document and skipped by every function here.
pub fn is_prose(block: &PageBlock) -> bool {
    block.kind != "Page"
}

/// THE TITLE IS LINE 0. It is a page property on the wire, not a block, but in
/// the buffer it is simply the document's first line — which is what makes
/// Enter at the end of the title and Backspace at the start of the body work
/// without either being special-cased: they are ordinary text edits, and the
/// save path reads line 0 back out.
pub fn page_document_text(title: &str, blocks: &[PageBlock]) -> String {
    let body = page_markdown(blocks);
    match body.is_empty() {
        // A PAGE WITH NO BLOCKS STILL NEEDS A BODY LINE TO TYPE INTO. Without
        // the newline the buffer is exactly one line — the title — so a fresh
        // page has nowhere else for the caret to be: clicking into the empty
        // space below the heading leaves it at the end of line 0, and the first
        // sentence the reader types is appended to the TITLE, silently renaming
        // their page in the sidebar, the tab and the header. Measured on a
        // newly created page, not theorised.
        //
        // This costs nothing downstream: `document_body` splits at the first
        // newline and `parse_document("")` returns no lines, so a page opened
        // and left alone still serializes zero blocks. The blank line lives in
        // the buffer, never in the document.
        true => format!("{title}\n"),
        false => format!("{title}\n{body}"),
    }
}

/// The title the buffer is carrying.
pub fn document_title(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_string()
}

/// Everything under the title, resolved into block lines.
pub fn document_body(text: &str) -> Vec<Line> {
    let body = text.split_once('\n').map_or("", |(_, body)| body);
    parse_document(body)
}

/// The document text for a page's blocks, title excluded.
pub fn page_markdown(blocks: &[PageBlock]) -> String {
    let lines: Vec<String> = rendered_blocks(blocks)
        .into_iter()
        .map(|(_, rendered)| rendered)
        .collect();
    lines.join("\n")
}

/// Every prose block as `(id, markdown)`, in document order.
///
/// An ordered marker is POSITIONAL — the stored block carries no number (see
/// [`ordered_content`]) — so the run's count is recomputed here rather than
/// read back. Rendering every Number as `1. ` made a saved list reopen as
/// "1. 1. 1.".
fn rendered_blocks(blocks: &[PageBlock]) -> Vec<(String, String)> {
    let mut counts: Vec<usize> = Vec::new();
    blocks
        .iter()
        .filter(|block| is_prose(block))
        .map(|block| {
            let line = Line {
                kind: block.kind.clone(),
                text: block.text.clone(),
                checked: block.checked,
                depth: block.prefix.len() / INDENT.len(),
            };
            let ordinal = ordinal(&mut counts, &line);
            (block.id.clone(), render_line(&line, ordinal))
        })
        .collect()
}

/// The number an ordered item wears: its 1-based place in the run at its OWN
/// depth. Any other kind ends the run at that depth, and going shallower drops
/// every deeper count — a nested list restarts at 1 under each parent.
fn ordinal(counts: &mut Vec<usize>, line: &Line) -> usize {
    counts.truncate(line.depth + 1);
    counts.resize(line.depth + 1, 0);
    let count = &mut counts[line.depth];
    *count = match line.kind == "Number" {
        true => *count + 1,
        false => 0,
    };
    *count
}

/// The stored shape of a page's prose, in document order.
pub fn stored_lines(blocks: &[PageBlock]) -> Vec<StoredLine> {
    blocks
        .iter()
        .filter(|block| is_prose(block))
        .map(|block| StoredLine {
            id: block.id.clone(),
            has_children: block.child_count > 0,
            line: Line {
                kind: block.kind.clone(),
                text: block.text.clone(),
                // `SetKind` off Todo leaves the stored flag behind; reading it
                // for other kinds would pair phantom ticks the plan can never
                // reconcile (`SetChecked` is Todo-only on the node).
                checked: block.kind == "Todo" && block.checked,
                depth: block.prefix.len() / INDENT.len(),
            },
        })
        .collect()
}

/// One block as its markdown line (or lines — a Code block is a fence).
/// `ordinal` is the number a Number block wears; every other kind ignores it.
pub fn render_line(line: &Line, ordinal: usize) -> String {
    let indent = INDENT.repeat(line.depth);
    let marker = match line.kind.as_str() {
        "Heading 1" => "# ",
        "Heading 2" => "## ",
        "Heading 3" => "### ",
        "Bullet" => "- ",
        "Number" => return format!("{indent}{ordinal}. {}", line.text),
        "Todo" => match line.checked {
            true => "- [x] ",
            false => "- [ ] ",
        },
        // `+` is a legal CommonMark bullet, reserved here for Toggle so the
        // kind survives a round trip instead of degrading into Bullet.
        "Toggle" => "+ ",
        "Quote" => "> ",
        "Callout" => "!> ",
        "Divider" => return format!("{indent}---"),
        "Code" => {
            // `split`, not `lines`: a code body's trailing newline is content,
            // and `lines()` eats it — the next save would then write the
            // stripped text back as a permanent edit.
            let body: Vec<String> = line
                .text
                .split('\n')
                .map(|body| format!("{indent}{body}"))
                .collect();
            let body = body.join("\n");
            return match body.is_empty() {
                true => format!("{indent}{FENCE}\n{indent}{FENCE}"),
                false => format!("{indent}{FENCE}\n{body}\n{indent}{FENCE}"),
            };
        }
        _ => "",
    };
    format!("{indent}{marker}{}", line.text)
}

/// Each prose block's [start, len] in DOCUMENT LINES (line 0 is the title).
/// Derived from the rendered shape itself, so a Code block counts its fences
/// and body exactly as the buffer shows them; subpages take no lines.
pub fn line_spans(blocks: &[PageBlock]) -> Vec<(String, usize, usize)> {
    let mut spans = Vec::new();
    let mut next = 1;
    for (id, rendered) in rendered_blocks(blocks) {
        let lines = rendered.split('\n').count();
        spans.push((id, next, lines));
        next += lines;
    }
    spans
}

/// The block the document line sits in — "" for the title line and for lines
/// past the last saved block (fresh typing the node has not seen yet).
pub fn block_at_line(blocks: &[PageBlock], line: usize) -> String {
    line_spans(blocks)
        .into_iter()
        .find(|(_, start, len)| line >= *start && line < start + len)
        .map(|(id, _, _)| id)
        .unwrap_or_default()
}

/// True while the buffer holds an ODD number of fence lines — an open ``` with
/// no close yet. Parsing such a buffer folds everything under the open fence
/// into one Code block, and the plan would then REMOVE the blocks that
/// "vanished"; the save tick waits instead. Line 0 is the title and is never
/// parsed, so it does not count.
pub fn has_unclosed_fence(text: &str) -> bool {
    let fences = text
        .lines()
        .skip(1)
        .filter(|line| line.trim_start_matches([' ', '\t']).starts_with(FENCE))
        .count();
    fences % 2 == 1
}

pub use crate::indent::split_indent;

/// The document text, resolved back into lines. A fenced run folds into ONE
/// Code line carrying the body verbatim, which is why this cannot be a `map`.
///
/// `split('\n')`, never `lines()`: the final empty line of a document IS a
/// block (the empty paragraph a page can end on), and `lines()` eats it — the
/// plan would then remove that block just for having opened the page.
///
/// Depth is CLAMPED to the line above's depth + 1 (and the first line to 0):
/// that is the only shape the tree can hold, and an unclamped depth becomes a
/// `MoveBlock` the module rejects forever.
pub fn parse_document(text: &str) -> Vec<Line> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<Line> = Vec::new();
    let mut source = text.split('\n').peekable();
    while let Some(raw) = source.next() {
        let (steps, rest) = split_indent(raw);
        let ceiling = lines.last().map_or(0, |line| line.depth + 1);
        let depth = steps.min(ceiling);
        if !rest.starts_with(FENCE) {
            lines.push(parse_line(rest, depth));
            continue;
        }
        // A code body is VERBATIM past the fence's own indent. Trimming all
        // leading whitespace here would silently reformat every indented line
        // of every code block on the first save.
        let own_indent = INDENT.repeat(depth);
        let mut body = Vec::new();
        for inside in source.by_ref() {
            if inside.trim_start_matches([' ', '\t']).starts_with(FENCE) {
                break;
            }
            let stripped = inside.strip_prefix(&own_indent).unwrap_or(inside);
            body.push(stripped.to_string());
        }
        lines.push(Line {
            kind: "Code".into(),
            text: body.join("\n"),
            checked: false,
            depth,
        });
    }
    lines
}

fn parse_line(rest: &str, depth: usize) -> Line {
    let plain = |kind: &str, text: &str, checked: bool| Line {
        kind: kind.into(),
        text: text.into(),
        checked,
        depth,
    };
    let trimmed = rest;
    if trimmed.trim_end() == "---" {
        return plain("Divider", "", false);
    }
    // Ordered longest-first: `### ` must not be read as `# ` plus prose.
    let markers = [
        ("### ", "Heading 3"),
        ("## ", "Heading 2"),
        ("# ", "Heading 1"),
        ("- [x] ", "Todo"),
        ("- [X] ", "Todo"),
        ("- [ ] ", "Todo"),
        ("!> ", "Callout"),
        ("> ", "Quote"),
        ("+ ", "Toggle"),
        ("- ", "Bullet"),
        ("* ", "Bullet"),
    ];
    for (marker, kind) in markers {
        let Some(rest) = trimmed.strip_prefix(marker) else {
            continue;
        };
        let checked = marker.eq_ignore_ascii_case("- [x] ");
        return plain(kind, rest, checked);
    }
    if let Some(content) = ordered_content(trimmed) {
        return plain("Number", content, false);
    }
    plain("Text", rest, false)
}

/// The digit count of an ordered marker (`12. ` / `12) `), or `None` when the
/// line does not wear one. Capped at three digits so a YEAR stays prose:
/// "1997. A great year" as a list item would come back renumbered, destroying
/// the year. Three, not two, because the render side now writes the real
/// position — a 100-item list must survive its own round trip.
// ponytail: 999 items is the ceiling; carrying the start number through the
// module is the upgrade if longer lists ever matter.
pub(crate) fn ordered_digits(trimmed: &str) -> Option<usize> {
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    let rest = trimmed.get(digits..)?;
    let rest = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')'))?;
    rest.starts_with(' ').then_some(digits)
}

/// `12. text` / `12) text` — the content past an ordered marker. The stored
/// number is positional, so the digits themselves are not kept.
fn ordered_content(trimmed: &str) -> Option<&str> {
    let digits = ordered_digits(trimmed)?;
    trimmed.get(digits + 2..)
}

/// The writes that carry `stored` to `wanted`.
///
/// Head and tail runs that are already equal are skipped, so their ids — and
/// anything anchored to them — never move. The disturbed middle is paired by
/// position: the overlap updates in place, a stored surplus is removed, a
/// wanted surplus is inserted after the last block that survives ahead of it.
pub fn document_plan(stored: &[StoredLine], wanted: &[Line]) -> DocumentPlan {
    let common_head = stored
        .iter()
        .zip(wanted)
        .take_while(|(have, want)| have.line == **want)
        .count();
    let tail_room = stored.len().min(wanted.len()) - common_head;
    let common_tail = stored
        .iter()
        .rev()
        .zip(wanted.iter().rev())
        .take(tail_room)
        .take_while(|(have, want)| have.line == **want)
        .count();

    let stored_middle = &stored[common_head..stored.len() - common_tail];
    let wanted_middle = &wanted[common_head..wanted.len() - common_tail];

    // A removal may take a parent ONLY when its whole subtree goes with it —
    // `RemoveBlock` is defined to take the subtree, so deleting the lines of a
    // nested list together is ONE remove on its root. A parent whose subtree
    // extends past the removed run would take survivors with it; that is
    // refused, and the caller resyncs and says so.
    let doomed_end = common_head + stored_middle.len();
    let survivors = stored_middle.len().min(wanted_middle.len());
    for (offset, doomed) in stored_middle.iter().enumerate().skip(survivors) {
        if !doomed.has_children {
            continue;
        }
        let index = common_head + offset;
        let subtree = stored[index + 1..]
            .iter()
            .take_while(|below| below.line.depth > doomed.line.depth)
            .count();
        let subtree_leaks = index + 1 + subtree > doomed_end;
        if subtree_leaks {
            return DocumentPlan {
                ops: Vec::new(),
                refusal: format!(
                    "\"{}\" still has sub-items — delete those first",
                    summarize(&doomed.line.text)
                ),
            };
        }
    }

    let mut ops = Vec::new();
    for (offset, (have, want)) in stored_middle.iter().zip(wanted_middle).enumerate() {
        // Depth becomes a `MoveBlock` direction, never a guessed parent — and
        // an indent is only asked for when the stored tree can PERFORM it (a
        // previous sibling to move under). An unperformable step is deferred:
        // the next tick re-plans against fresher state, and a plan that comes
        // back empty settles the baseline instead of retrying forever.
        if have.line.depth != want.depth {
            let indent = want.depth > have.line.depth;
            let previous_peer = stored[..common_head + offset]
                .iter()
                .rev()
                .map(|earlier| earlier.line.depth)
                .find(|depth| *depth <= have.line.depth);
            let performable = !indent || previous_peer == Some(have.line.depth);
            if performable {
                ops.push(BlockOp::Nest {
                    id: have.id.clone(),
                    direction: match indent {
                        true => "indent".into(),
                        false => "outdent".into(),
                    },
                });
            }
        }
        if have.line.text != want.text {
            ops.push(BlockOp::SetText {
                id: have.id.clone(),
                text: want.text.clone(),
            });
        }
        if have.line.kind != want.kind {
            ops.push(BlockOp::SetKind {
                id: have.id.clone(),
                kind: want.kind.clone(),
            });
        }
        // A tick is a Todo fact — on any other wanted kind there is nothing to
        // reconcile, and the module rejects the op (`NotTodo`).
        if want.kind == "Todo" && have.line.checked != want.checked {
            ops.push(BlockOp::SetChecked {
                id: have.id.clone(),
                checked: want.checked,
            });
        }
    }
    // REVERSE document order: a parent precedes its subtree in preorder, so
    // walking backwards removes leaves first and every parent is childless by
    // the time its own `Remove` lands — no op ever takes a survivor.
    for surplus in stored_middle.iter().skip(survivors).rev() {
        ops.push(BlockOp::Remove {
            id: surplus.id.clone(),
        });
    }

    // An insert anchors on the last block that is still there ahead of it. The
    // stored middle's own survivors come first, then the head run.
    let survivors = stored_middle.len().min(wanted_middle.len());
    let mut anchor = stored_middle
        .get(survivors.wrapping_sub(1))
        .map(|stored| stored.id.clone())
        .or_else(|| {
            stored
                .get(common_head.wrapping_sub(1))
                .map(|stored| stored.id.clone())
        })
        .unwrap_or_default();
    for fresh in wanted_middle.iter().skip(stored_middle.len()) {
        ops.push(BlockOp::Insert {
            after: anchor.clone(),
            kind: fresh.kind.clone(),
            text: fresh.text.clone(),
        });
        // Each insert anchors on the one before it, and the caller applies the
        // list strictly in order, so the chain resolves as it is written.
        anchor = String::new();
    }

    DocumentPlan {
        ops,
        refusal: String::new(),
    }
}

fn summarize(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "this block".into();
    }
    match trimmed.char_indices().nth(28) {
        Some((cut, _)) => format!("{}…", &trimmed[..cut]),
        None => trimmed.into(),
    }
}

// ---------- comment anchors ----------

/// Where a comment thread anchors, in the reader's own words: the opening of
/// the block it marks, quoted, or the page itself. No line number — the
/// editor draws none, so "line 3" named nothing the reader could find.
pub fn comment_anchor_label(blocks: &[PageBlock], target: &str, page_id: &str) -> String {
    anchor_label(&comment_anchor_labels(blocks), target, page_id)
}

/// The composer's own caption: where a NEW comment will anchor.
/// THE COMPOSER AT THE CARD'S FOOT ALWAYS OPENS A NEW THREAD on the scope the
/// card is showing — never a reply, which has its own box inside its thread.
/// `scope` is a block id, or empty (or the page's own id) for the page.
pub fn comment_compose_hint(blocks: &[PageBlock], scope: &str, page_id: &str) -> String {
    if is_page_scope(scope, page_id) {
        return "Comment on this page".into();
    }
    format!(
        "New comment on {}",
        anchor_label(&comment_anchor_labels(blocks), scope, page_id)
    )
}

/// The card's own title: the block it is scoped to, quoted, or the page with
/// the number of open threads on it.
pub fn comment_scope_label(
    blocks: &[PageBlock],
    scope: &str,
    page_id: &str,
    open_threads: i64,
) -> String {
    if !is_page_scope(scope, page_id) {
        return anchor_label(&comment_anchor_labels(blocks), scope, page_id);
    }
    match open_threads {
        1 => "This page · 1 thread".into(),
        count => format!("This page · {count} threads"),
    }
}

fn is_page_scope(scope: &str, page_id: &str) -> bool {
    scope.is_empty() || scope == page_id
}

fn comment_anchor_labels(blocks: &[PageBlock]) -> BTreeMap<String, String> {
    let text_by_id: BTreeMap<&str, &str> = blocks
        .iter()
        .map(|block| (block.id.as_str(), block.text.as_str()))
        .collect();
    line_spans(blocks)
        .into_iter()
        .map(|(id, _, _)| {
            let text = text_by_id.get(id.as_str()).copied().unwrap_or_default();
            (id, format!("“{}”", anchor_snippet(text)))
        })
        .collect()
}

fn anchor_label(labels: &BTreeMap<String, String>, target: &str, page_id: &str) -> String {
    if target.is_empty() || target == page_id {
        return "this page".into();
    }
    labels
        .get(target)
        .cloned()
        .unwrap_or_else(|| "a removed block".into())
}

fn anchor_snippet(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(36) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None if text.is_empty() => "an empty line".into(),
        None => text.to_owned(),
    }
}

/// ONE BADGE PER COMMENTED BLOCK, carrying its thread count — the repetition
/// in `hits` IS the count, so a line holding one stray note and a line
/// holding a whole argument do not draw the same dots.
pub fn comment_marks(blocks: &[PageBlock], hits: &[String]) -> Vec<CommentMark> {
    let mut marks = Vec::new();
    for (id, start, _len) in line_spans(blocks) {
        let count = hits.iter().filter(|hit| *hit == &id).count();
        if count > 0 {
            marks.push(CommentMark {
                line: start as i64,
                count: count as i64,
            });
        }
    }
    marks
}

/// The document lines wearing a commented block's wash, for the highlighter.
pub fn commented_lines(blocks: &[PageBlock], targets: &[String]) -> Vec<i64> {
    let mut lines = Vec::new();
    for (id, start, len) in line_spans(blocks) {
        if !targets.contains(&id) {
            continue;
        }
        for line in start..start + len {
            lines.push(line as i64);
        }
    }
    lines
}

/// The page's child pages, in document order. Subpages are navigation, not
/// prose: they have no markdown spelling, so the document editor never holds
/// them and the screen lists them underneath it instead.
pub fn subpages(blocks: &[PageBlock]) -> Vec<&PageBlock> {
    blocks.iter().filter(|block| !is_prose(block)).collect()
}
