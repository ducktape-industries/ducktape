//! Pages owns its Markdown dialect and conversion to the reusable rich block wire.
use ducktape_view_guest::wire;
use std::ops::Range;
use wire::editor_rich::{RichBlock, RichDocument, RichEdit, RichMark, RichPresentation};
const INDENT: &str = "  ";
const FENCE: &str = "```";
mod types {
    pub const HEADING: &str = "heading";
    pub const PARAGRAPH: &str = "paragraph";
    pub const BULLET_LIST: &str = "bulletList";
    pub const ORDERED_LIST: &str = "orderedList";
    pub const TASK_LIST: &str = "taskList";
    pub const BLOCKQUOTE: &str = "blockquote";
    pub const CODE_BLOCK: &str = "codeBlock";
    pub const HORIZONTAL_RULE: &str = "horizontalRule";
    pub const CALLOUT: &str = "callout";
    pub const TOGGLE: &str = "details";
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum MarkKind {
    Bold,
    Italic,
    Strike,
    Underline,
    Highlight(Option<String>),
    Code,
    Link(String),
    Mention(String),
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Mark {
    kind: MarkKind,
    range: Range<usize>,
}
impl Mark {
    fn new(kind: MarkKind, range: Range<usize>) -> Self {
        Self { kind, range }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct MarkList(Vec<Mark>);
impl MarkList {
    fn from_marks(marks: Vec<Mark>) -> Self {
        Self(marks)
    }
    fn iter(&self) -> std::slice::Iter<'_, Mark> {
        self.0.iter()
    }
    fn runs(&self) -> Vec<(Range<usize>, Vec<MarkKind>)> {
        let mut edges: Vec<_> = self
            .0
            .iter()
            .flat_map(|m| [m.range.start, m.range.end])
            .collect();
        edges.sort_unstable();
        edges.dedup();
        edges
            .windows(2)
            .filter_map(|pair| {
                let range = pair[0]..pair[1];
                let kinds: Vec<_> = self
                    .0
                    .iter()
                    .filter(|m| m.range.start <= range.start && m.range.end >= range.end)
                    .map(|m| m.kind.clone())
                    .collect();
                (!kinds.is_empty()).then_some((range, kinds))
            })
            .collect()
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct BlockAttrs {
    level: u8,
    checked: bool,
    language: Option<String>,
}
impl BlockAttrs {
    fn level(level: u8) -> Self {
        Self {
            level,
            ..Default::default()
        }
    }
    fn language(language: String) -> Self {
        Self {
            language: Some(language),
            ..Default::default()
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct BlockContent {
    ty: String,
    text: String,
    attrs: BlockAttrs,
    marks: MarkList,
    indent: usize,
}
impl BlockContent {
    fn new(ty: &str, text: impl Into<String>) -> Self {
        Self {
            ty: ty.into(),
            text: text.into(),
            ..Default::default()
        }
    }
    #[cfg(test)]
    fn paragraph(text: &str) -> Self {
        Self::new(types::PARAGRAPH, text)
    }
    fn with_attrs(mut self, attrs: BlockAttrs) -> Self {
        self.attrs = attrs;
        self
    }
    fn with_marks(mut self, marks: MarkList) -> Self {
        self.marks = marks;
        self
    }
    fn with_indent(mut self, indent: usize) -> Self {
        self.indent = indent;
        self
    }
}
/// The document line each block starts on: the title is block 0 on line 0,
/// and a code block spans its two fences and its body.
fn block_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    let Some((_, body)) = text.split_once('\n') else {
        return starts;
    };
    let mut line = 1;
    let mut source = body.split('\n');
    while let Some(raw) = source.next() {
        starts.push(line);
        line += 1;
        let (_, rest) = split_indent(raw);
        if !rest.starts_with(FENCE) {
            continue;
        }
        for inside in source.by_ref() {
            line += 1;
            if inside.trim_start_matches([' ', '\t']).starts_with(FENCE) {
                break;
            }
        }
    }
    starts
}

/// A byte offset in a block's plain text as the byte column of the same
/// character in the block's fenced line, marker excluded.
fn fenced_column(block: &BlockContent, plain: usize) -> usize {
    let text = block.text.as_str();
    let mut out = 0;
    let mut at = 0;
    for (range, kinds) in block.marks.runs() {
        let range = range.start.max(at)..range.end.min(text.len());
        if range.start >= range.end {
            continue;
        }
        // A selection starting on the run's first character starts INSIDE
        // its fence, so the anchor covers the words and not the markers.
        if plain < range.start {
            return out + (plain - at);
        }
        out += range.start - at;
        let body = &text[range.clone()];
        let fenced = fence_of(body, &kinds);
        let open = fenced.find(body).unwrap_or(0);
        if plain <= range.end {
            return out + open + (plain - range.start);
        }
        out += fenced.len();
        at = range.end;
    }
    out + plain.saturating_sub(at)
}

// ----------------------------------------------------------------- markdown → blocks

/// The canonical text as blocks: line 0 is the title, every later line one
/// block of the guest dialect. Depth is clamped to the line above's + 1, the
/// only shape the guest tree can hold.
fn blocks_of(text: &str) -> Vec<BlockContent> {
    let Some((title, body)) = text.split_once('\n') else {
        return vec![title_block(text)];
    };
    let mut blocks = vec![title_block(title)];
    let mut source = body.split('\n');
    while let Some(raw) = source.next() {
        let (steps, rest) = split_indent(raw);
        let ceiling = match blocks.len() {
            1 => 0,
            _ => blocks.last().map_or(0, |block| block.indent + 1),
        };
        let indent = steps.min(ceiling);
        if !rest.starts_with(FENCE) {
            blocks.push(block_of(rest, indent));
            continue;
        }
        let own_indent = INDENT.repeat(indent);
        let mut lines = Vec::new();
        for inside in source.by_ref() {
            if inside.trim_start_matches([' ', '\t']).starts_with(FENCE) {
                break;
            }
            lines.push(inside.strip_prefix(&own_indent).unwrap_or(inside));
        }
        let language = rest[FENCE.len()..].trim();
        let attrs = match language.is_empty() {
            true => BlockAttrs::default(),
            false => BlockAttrs::language(language.to_string()),
        };
        blocks.push(
            BlockContent::new(types::CODE_BLOCK, lines.join("\n"))
                .with_attrs(attrs)
                .with_indent(indent),
        );
    }
    blocks
}

fn title_block(title: &str) -> BlockContent {
    BlockContent::new(types::HEADING, title).with_attrs(BlockAttrs::level(1))
}

fn split_indent(raw: &str) -> (usize, &str) {
    let mut steps = 0;
    let mut rest = raw;
    while let Some(next) = rest.strip_prefix(INDENT) {
        steps += 1;
        rest = next;
    }
    (steps, rest)
}

fn block_of(rest: &str, indent: usize) -> BlockContent {
    if rest.trim_end() == "---" {
        return BlockContent::new(types::HORIZONTAL_RULE, "").with_indent(indent);
    }
    // Longest first: `### ` must not be read as `# ` plus prose.
    let markers: [(&str, &str, BlockAttrs); 11] = [
        ("### ", types::HEADING, BlockAttrs::level(3)),
        ("## ", types::HEADING, BlockAttrs::level(2)),
        ("# ", types::HEADING, BlockAttrs::level(1)),
        ("- [x] ", types::TASK_LIST, checked()),
        ("- [X] ", types::TASK_LIST, checked()),
        ("- [ ] ", types::TASK_LIST, BlockAttrs::default()),
        ("!> ", types::CALLOUT, BlockAttrs::default()),
        ("> ", types::BLOCKQUOTE, BlockAttrs::default()),
        ("+ ", types::TOGGLE, BlockAttrs::default()),
        ("- ", types::BULLET_LIST, BlockAttrs::default()),
        ("* ", types::BULLET_LIST, BlockAttrs::default()),
    ];
    for (marker, ty, attrs) in markers {
        let Some(content) = rest.strip_prefix(marker) else {
            continue;
        };
        return inline_block(ty, attrs, content, indent);
    }
    if let Some(content) = ordered_content(rest) {
        return inline_block(types::ORDERED_LIST, BlockAttrs::default(), content, indent);
    }
    inline_block(types::PARAGRAPH, BlockAttrs::default(), rest, indent)
}

fn checked() -> BlockAttrs {
    BlockAttrs {
        checked: true,
        ..Default::default()
    }
}

/// `12. text` → `text`; the number is positional and never stored.
fn ordered_content(rest: &str) -> Option<&str> {
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    rest[digits..].strip_prefix(". ")
}

fn inline_block(ty: &str, attrs: BlockAttrs, content: &str, indent: usize) -> BlockContent {
    let (text, marks) = inline(content);
    BlockContent::new(ty, text)
        .with_attrs(attrs)
        .with_marks(marks)
        .with_indent(indent)
}

/// The inline fences the guest grammar knows, longest first so `**` is never
/// read as two `*`. Single level: a body is never scanned again.
const FENCES: &[(&str, MarkKind)] = &[
    ("**", MarkKind::Bold),
    ("__", MarkKind::Bold),
    ("~~", MarkKind::Strike),
    ("++", MarkKind::Underline),
    ("==", MarkKind::Highlight(None)),
    ("`", MarkKind::Code),
    ("*", MarkKind::Italic),
    ("_", MarkKind::Italic),
];

/// The text with its fences removed, and the marks over the stripped text.
fn inline(content: &str) -> (String, MarkList) {
    let mut text = String::with_capacity(content.len());
    let mut marks = Vec::new();
    let mut at = 0;
    while at < content.len() {
        let rest = &content[at..];
        if let Some((label, url, len)) = named_link(rest) {
            marks.push(Mark::new(
                MarkKind::Link(url.into()),
                text.len()..text.len() + label.len(),
            ));
            text.push_str(label);
            at += len;
            continue;
        }
        if let Some(len) = url_len(rest) {
            let url = &rest[..len];
            marks.push(Mark::new(
                MarkKind::Link(url.into()),
                text.len()..text.len() + len,
            ));
            text.push_str(url);
            at += len;
            continue;
        }
        if let Some(len) = mention_len(content, at) {
            let handle = &rest[1..len];
            marks.push(Mark::new(
                MarkKind::Mention(handle.into()),
                text.len()..text.len() + len,
            ));
            text.push_str(&rest[..len]);
            at += len;
            continue;
        }
        let fence = FENCES.iter().find_map(|(marker, kind)| {
            fenced(rest, marker).map(|body| (*marker, body, kind.clone()))
        });
        let Some((marker, body, kind)) = fence else {
            let c = rest.chars().next().expect("inside the text");
            text.push(c);
            at += c.len_utf8();
            continue;
        };
        marks.push(Mark::new(kind, text.len()..text.len() + body.len()));
        text.push_str(body);
        at += marker.len() * 2 + body.len();
    }
    (text, MarkList::from_marks(marks))
}

/// If `rest` opens with `marker` and a later `marker` closes a non-empty body,
/// that body.
fn fenced<'a>(rest: &'a str, marker: &str) -> Option<&'a str> {
    let body = rest.strip_prefix(marker)?;
    let close = body.find(marker)?;
    (close > 0).then(|| &body[..close])
}

/// `[label](url)` at the start of `rest`: the label, the url, the source length.
fn named_link(rest: &str) -> Option<(&str, &str, usize)> {
    let inner = rest.strip_prefix('[')?;
    let label_end = inner.find("](")?;
    let label = &inner[..label_end];
    let url_start = label_end + 2;
    let url_len = inner[url_start..].find(')')?;
    let url = &inner[url_start..url_start + url_len];
    let plain = !label.is_empty() && !label.contains('[') && !url.is_empty() && !url.contains(' ');
    plain.then_some((label, url, 1 + url_start + url_len + 1))
}

/// A bare `http(s)://` link runs to the next whitespace.
fn url_len(rest: &str) -> Option<usize> {
    let starts_link = rest.starts_with("http://") || rest.starts_with("https://");
    if !starts_link {
        return None;
    }
    Some(rest.find(char::is_whitespace).unwrap_or(rest.len()))
}

fn handle_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '-' | '_' | '.')
}

/// An `@` at a word start followed by a handle; an `@` inside a word (an
/// email address) is prose.
fn mention_len(content: &str, at: usize) -> Option<usize> {
    let rest = content[at..].strip_prefix('@')?;
    let mid_word = content[..at]
        .chars()
        .next_back()
        .is_some_and(char::is_alphanumeric);
    if mid_word {
        return None;
    }
    let handle = rest.find(|c| !handle_char(c)).unwrap_or(rest.len());
    (handle > 0).then_some(1 + handle)
}

// ----------------------------------------------------------------- blocks → markdown

/// The blocks as the canonical text. Block 0 is the title, whatever type the
/// editor gave it; an ordered item's number is its place in the run.
fn markdown_of(blocks: &[BlockContent]) -> String {
    let mut lines = Vec::with_capacity(blocks.len());
    let title = blocks.first().map_or("", |block| block.text.as_str());
    lines.push(title.to_string());
    let mut ordinals: Vec<usize> = Vec::new();
    for (ix, block) in blocks.iter().enumerate().skip(1) {
        let ordinal = ordinal_of(blocks, ix, &mut ordinals);
        lines.push(line_of(block, ordinal));
    }
    lines.join("\n")
}

/// The number an ordered item wears: one past the previous ordered item at
/// the same depth, unless another kind at that depth broke the run.
fn ordinal_of(blocks: &[BlockContent], ix: usize, ordinals: &mut Vec<usize>) -> usize {
    let block = &blocks[ix];
    ordinals.resize(block.indent + 1, 0);
    if block.ty != types::ORDERED_LIST {
        ordinals[block.indent] = 0;
        return 0;
    }
    ordinals[block.indent] += 1;
    ordinals[block.indent]
}

fn line_of(block: &BlockContent, ordinal: usize) -> String {
    let indent = INDENT.repeat(block.indent);
    let text = inline_of(block);
    let marker: String = match block.ty.as_str() {
        types::HEADING => "#".repeat(block.attrs.level.clamp(1, 3) as usize) + " ",
        types::BULLET_LIST => "- ".into(),
        types::ORDERED_LIST => format!("{ordinal}. "),
        types::TASK_LIST => match block.attrs.checked {
            true => "- [x] ".into(),
            false => "- [ ] ".into(),
        },
        types::TOGGLE => "+ ".into(),
        types::BLOCKQUOTE => "> ".into(),
        types::CALLOUT => "!> ".into(),
        types::HORIZONTAL_RULE => return format!("{indent}---"),
        types::CODE_BLOCK => {
            let language = block.attrs.language.as_deref().unwrap_or("");
            let body: Vec<String> = block
                .text
                .split('\n')
                .map(|body| format!("{indent}{body}"))
                .collect();
            let body = body.join("\n");
            return match body.is_empty() {
                true => format!("{indent}{FENCE}{language}\n{indent}{FENCE}"),
                false => format!("{indent}{FENCE}{language}\n{body}\n{indent}{FENCE}"),
            };
        }
        // ponytail: images and tables have no line in the guest dialect;
        // their text rides as a paragraph until the dialect grows a shape.
        _ => String::new(),
    };
    format!("{indent}{marker}{text}")
}

/// The block text with one fence per marked run. The dialect nests nothing,
/// so a run wearing several marks keeps the one that reads strongest.
fn inline_of(block: &BlockContent) -> String {
    let text = block.text.as_str();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (range, kinds) in block.marks.runs() {
        let range = range.start.max(at)..range.end.min(text.len());
        if range.start >= range.end {
            continue;
        }
        out.push_str(&text[at..range.start]);
        let body = &text[range.clone()];
        out.push_str(&fence_of(body, &kinds));
        at = range.end;
    }
    out.push_str(&text[at..]);
    out
}

/// The fence order when a run wears several marks: the one the reader would
/// miss most wins.
const FENCE_RANK: [fn(&MarkKind) -> bool; 7] = [
    |kind| matches!(kind, MarkKind::Code),
    |kind| matches!(kind, MarkKind::Link(_)),
    |kind| matches!(kind, MarkKind::Bold),
    |kind| matches!(kind, MarkKind::Italic),
    |kind| matches!(kind, MarkKind::Strike),
    |kind| matches!(kind, MarkKind::Underline),
    |kind| matches!(kind, MarkKind::Highlight(_)),
];

fn fence_of(body: &str, kinds: &[MarkKind]) -> String {
    let strongest = FENCE_RANK
        .iter()
        .find_map(|ranked| kinds.iter().find(|kind| ranked(kind)));
    match strongest {
        Some(MarkKind::Code) => format!("`{body}`"),
        Some(MarkKind::Link(url)) if url.as_str() == body => body.to_string(),
        Some(MarkKind::Link(url)) => format!("[{body}]({url})"),
        Some(MarkKind::Bold) => format!("**{body}**"),
        Some(MarkKind::Italic) => format!("*{body}*"),
        Some(MarkKind::Strike) => format!("~~{body}~~"),
        Some(MarkKind::Underline) => format!("++{body}++"),
        Some(MarkKind::Highlight(_)) => format!("=={body}=="),
        _ => body.to_string(),
    }
}

fn wire_mark(mark: &Mark) -> RichMark {
    let (kind, value) = match &mark.kind {
        MarkKind::Bold => ("bold", ""),
        MarkKind::Italic => ("italic", ""),
        MarkKind::Underline => ("underline", ""),
        MarkKind::Strike => ("strike", ""),
        MarkKind::Code => ("code", ""),
        MarkKind::Highlight(_) => ("highlight", ""),
        MarkKind::Link(value) => ("link", value.as_str()),
        MarkKind::Mention(value) => ("mention", value.as_str()),
    };
    RichMark {
        start: mark.range.start as u32,
        end: mark.range.end as u32,
        kind: kind.into(),
        value: value.into(),
    }
}
fn native_mark(mark: &RichMark) -> Option<Mark> {
    let kind = match mark.kind.as_str() {
        "bold" => MarkKind::Bold,
        "italic" => MarkKind::Italic,
        "underline" => MarkKind::Underline,
        "strike" => MarkKind::Strike,
        "code" => MarkKind::Code,
        "highlight" => MarkKind::Highlight(None),
        "link" => MarkKind::Link(mark.value.clone()),
        "mention" => MarkKind::Mention(mark.value.clone()),
        _ => return None,
    };
    Some(Mark::new(kind, mark.start as usize..mark.end as usize))
}
fn wire_block(block: &BlockContent) -> RichBlock {
    RichBlock {
        kind: block.ty.clone(),
        text: block.text.clone(),
        indent: block.indent as u32,
        level: block.attrs.level,
        checked: block.attrs.checked,
        language: block.attrs.language.clone().unwrap_or_default(),
        marks: block.marks.iter().map(wire_mark).collect(),
        attributes: Vec::new(),
    }
}
fn native_block(block: &RichBlock) -> BlockContent {
    BlockContent {
        ty: block.kind.clone(),
        text: block.text.clone(),
        indent: block.indent as usize,
        attrs: BlockAttrs {
            level: block.level,
            checked: block.checked,
            language: (!block.language.is_empty()).then(|| block.language.clone()),
        },
        marks: MarkList(block.marks.iter().filter_map(native_mark).collect()),
    }
}

fn canonical_position(blocks: &[BlockContent], at: wire::EditorPosition) -> wire::EditorPosition {
    let text = markdown_of(blocks);
    let index = (at.line as usize).min(blocks.len().saturating_sub(1));
    let Some(block) = blocks.get(index) else {
        return Default::default();
    };
    let line = block_starts(&text)[index];
    let mut ordinals = Vec::new();
    let ordinal = (1..=index)
        .map(|ix| ordinal_of(blocks, ix, &mut ordinals))
        .last()
        .unwrap_or(0);
    block_position(block, index, line, ordinal, at.column as usize)
}
fn block_position(
    block: &BlockContent,
    index: usize,
    line: usize,
    ordinal: usize,
    plain: usize,
) -> wire::EditorPosition {
    let plain = plain.min(block.text.len());
    if index == 0 {
        return wire::EditorPosition {
            line: 0,
            column: plain as u32,
        };
    }
    if block.ty == types::CODE_BLOCK {
        let prefix = &block.text[..plain];
        let inner_line = prefix.bytes().filter(|byte| *byte == b'\n').count();
        let column =
            prefix.rsplit('\n').next().unwrap_or_default().len() + INDENT.len() * block.indent;
        return wire::EditorPosition {
            line: (line + 1 + inner_line) as u32,
            column: column as u32,
        };
    }
    let prefix = line_of(block, ordinal).len() - inline_of(block).len();
    wire::EditorPosition {
        line: line as u32,
        column: (prefix + fenced_column(block, plain)) as u32,
    }
}

fn rich_position(blocks: &[BlockContent], at: wire::EditorPosition) -> wire::EditorPosition {
    let text = markdown_of(blocks);
    let starts = block_starts(&text);
    let index = starts
        .iter()
        .rposition(|line| *line <= at.line as usize)
        .unwrap_or(0);
    let block = &blocks[index];
    let column = match (index, block.ty.as_str()) {
        (0, _) => at.column as usize,
        (_, types::CODE_BLOCK) => {
            let inner_line = (at.line as usize).saturating_sub(starts[index] + 1);
            let start: usize = block
                .text
                .split('\n')
                .take(inner_line)
                .map(|line| line.len() + 1)
                .sum();
            start + (at.column as usize).saturating_sub(INDENT.len() * block.indent)
        }
        _ => {
            let mut ordinals = Vec::new();
            let ordinal = (1..=index)
                .map(|ix| ordinal_of(blocks, ix, &mut ordinals))
                .last()
                .unwrap_or(0);
            let prefix = line_of(block, ordinal).len() - inline_of(block).len();
            unfenced_column(block, (at.column as usize).saturating_sub(prefix))
        }
    };
    let mut column = column.min(block.text.len());
    while !block.text.is_char_boundary(column) {
        column -= 1;
    }
    wire::EditorPosition {
        line: index as u32,
        column: column as u32,
    }
}

fn unfenced_column(block: &BlockContent, source: usize) -> usize {
    let mut plain = 0;
    let mut written = 0;
    for (range, kinds) in block.marks.runs() {
        let gap = range.start - plain;
        if source <= written + gap {
            return plain + source.saturating_sub(written);
        }
        written += gap;
        let body = &block.text[range.clone()];
        let fenced = fence_of(body, &kinds);
        let open = fenced.find(body).unwrap_or(0);
        if source <= written + fenced.len() {
            return range.start + source.saturating_sub(written + open).min(body.len());
        }
        written += fenced.len();
        plain = range.end;
    }
    plain + source.saturating_sub(written)
}

pub fn presentation(text: &str, cursor: wire::EditorCursor) -> RichPresentation {
    let blocks = blocks_of(text);
    RichPresentation {
        document: RichDocument {
            blocks: blocks.iter().map(wire_block).collect(),
            cursor: wire::EditorCursor {
                position: rich_position(&blocks, cursor.position),
                selection: cursor.selection.map(|at| rich_position(&blocks, at)),
            },
        },
        toolbar: crate::editor_menu::format_items()
            .iter()
            .map(|(tag, label)| wire::editor_presentation::EditorMenuItem {
                tag: (*tag).into(),
                label: (*label).into(),
            })
            .collect(),
    }
}

/// The current snapshot becomes canonical text before a guest toolbar command
/// is decided. No native renderer interprets Markdown or comment anchors.
pub fn document(edit: &RichEdit) -> Result<(String, wire::EditorCursor), &'static str> {
    canonical(&edit.document)
}
pub fn canonical(snapshot: &RichDocument) -> Result<(String, wire::EditorCursor), &'static str> {
    snapshot.validate()?;
    let blocks: Vec<_> = snapshot.blocks.iter().map(native_block).collect();
    let cursor = wire::EditorCursor {
        position: canonical_position(&blocks, snapshot.cursor.position),
        selection: snapshot
            .cursor
            .selection
            .map(|at| canonical_position(&blocks, at)),
    };
    Ok((markdown_of(&blocks), cursor))
}

pub fn block_index(text: &str, line: u32) -> u32 {
    block_starts(text)
        .iter()
        .rposition(|start| *start <= line as usize)
        .unwrap_or(0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOCUMENT: &str = "Welcome\n# Heading\nPlain **bold** and *it* and `code`\n- one\n  - [x] nested\n1. first\n2. second\n> quote\n!> callout\n+ toggle\n---\n```rust\nfn main() {}\n```\nSee [docs](https://x.y) or https://a.b and @ada\n";

    #[test]
    fn rich_wire_round_trip_preserves_code_and_marked_selection_coordinates() {
        let text = "Title\nSee **한글** here\n```rust\nlet 한 = 1;\n한\n```\nTail";
        for cursor in [
            wire::EditorCursor {
                position: wire::EditorPosition {
                    line: 1,
                    column: 12,
                },
                selection: Some(wire::EditorPosition { line: 1, column: 6 }),
            },
            wire::EditorCursor {
                position: wire::EditorPosition { line: 4, column: 3 },
                selection: Some(wire::EditorPosition { line: 3, column: 4 }),
            },
        ] {
            let rich = presentation(text, cursor);
            let edit = RichEdit {
                document: rich.document,
                ..Default::default()
            };
            assert_eq!(document(&edit).unwrap(), (text.to_owned(), cursor));
        }
    }

    #[test]
    fn the_canonical_text_round_trips_through_blocks() {
        let blocks = blocks_of(DOCUMENT);
        assert_eq!(markdown_of(&blocks), DOCUMENT);
    }

    #[test]
    fn lines_resolve_to_the_notion_vocabulary() {
        let blocks = blocks_of(DOCUMENT);
        let kinds: Vec<(&str, usize)> = blocks
            .iter()
            .map(|block| (block.ty.as_str(), block.indent))
            .collect();
        assert_eq!(
            kinds,
            [
                (types::HEADING, 0),
                (types::HEADING, 0),
                (types::PARAGRAPH, 0),
                (types::BULLET_LIST, 0),
                (types::TASK_LIST, 1),
                (types::ORDERED_LIST, 0),
                (types::ORDERED_LIST, 0),
                (types::BLOCKQUOTE, 0),
                (types::CALLOUT, 0),
                (types::TOGGLE, 0),
                (types::HORIZONTAL_RULE, 0),
                (types::CODE_BLOCK, 0),
                (types::PARAGRAPH, 0),
                (types::PARAGRAPH, 0),
            ]
        );
        assert!(blocks[4].attrs.checked);
        assert_eq!(blocks[11].text, "fn main() {}");
        assert_eq!(blocks[11].attrs.language.as_deref(), Some("rust"));
    }

    #[test]
    fn fences_become_marks_over_the_stripped_text() {
        let blocks = blocks_of("T\nPlain **bold** and *it* and `code`");
        let block = &blocks[1];
        assert_eq!(block.text, "Plain bold and it and code");
        let marks: Vec<(MarkKind, std::ops::Range<usize>)> = block
            .marks
            .iter()
            .map(|mark| (mark.kind.clone(), mark.range.clone()))
            .collect();
        assert_eq!(
            marks,
            [
                (MarkKind::Bold, 6..10),
                (MarkKind::Italic, 15..17),
                (MarkKind::Code, 22..26),
            ]
        );
    }

    #[test]
    fn links_and_mentions_keep_their_targets() {
        let blocks = blocks_of("T\nSee [docs](https://x.y) or https://a.b and @ada");
        let marks: Vec<(MarkKind, &str)> = blocks[1]
            .marks
            .iter()
            .map(|mark| (mark.kind.clone(), &blocks[1].text[mark.range.clone()]))
            .collect();
        assert_eq!(
            marks,
            [
                (MarkKind::Link("https://x.y".into()), "docs"),
                (MarkKind::Link("https://a.b".into()), "https://a.b"),
                (MarkKind::Mention("ada".into()), "@ada"),
            ]
        );
    }

    #[test]
    fn a_run_with_several_marks_keeps_the_strongest() {
        let block = BlockContent::paragraph("both").with_marks(MarkList::from_marks(vec![
            Mark::new(MarkKind::Italic, 0..4),
            Mark::new(MarkKind::Bold, 0..4),
        ]));
        assert_eq!(
            markdown_of(&[BlockContent::paragraph("T"), block]),
            "T\n**both**"
        );
    }

    #[test]
    fn depth_is_clamped_to_the_line_above() {
        let blocks = blocks_of("T\n    - too deep\n- one\n    - two deep");
        let depths: Vec<usize> = blocks.iter().map(|block| block.indent).collect();
        assert_eq!(depths, [0, 0, 0, 1]);
    }

    #[test]
    fn a_fresh_page_is_a_title_and_one_empty_line() {
        let blocks = blocks_of("Untitled\n");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1].ty, types::PARAGRAPH);
        assert_eq!(markdown_of(&blocks), "Untitled\n");
        assert_eq!(markdown_of(&blocks_of("")), "");
    }

    #[test]
    fn blocks_start_on_their_document_lines() {
        assert_eq!(block_starts("T\na\n```\nx\ny\n```\nb"), [0, 1, 2, 6]);
        assert_eq!(block_starts("T"), [0]);
        assert_eq!(block_starts("T\n"), [0, 1]);
    }

    #[test]
    fn plain_offsets_map_onto_the_fenced_line() {
        let block = blocks_of("T\nSee **bold** and [docs](https://x.y) now").remove(1);
        assert_eq!(block.text, "See bold and docs now");
        // "See " is plain; "bold" opens after `**`; "docs" after `[`.
        assert_eq!(fenced_column(&block, 0), 0);
        assert_eq!(fenced_column(&block, 4), 6);
        assert_eq!(fenced_column(&block, 8), 10);
        assert_eq!(fenced_column(&block, 13), 18);
        assert_eq!(fenced_column(&block, 17), 22);
        assert_eq!(fenced_column(&block, 21), 40);
    }
}
