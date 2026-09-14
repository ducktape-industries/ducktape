//! The pages document on gpui-notion.
//!
//! The guest keeps the canonical markdown (title on line 0, then the block
//! dialect `document_sync` renders: two spaces per depth, `# `/`- `/`1. `/
//! `- [ ] `/`> `/`!> `/`+ `/`---`/fences, single-level inline fences). This
//! mount is the bridge: the canonical text becomes `NotionEditor` blocks, and
//! every `DocumentChanged` serializes the blocks back and submits the whole
//! text as one native edit, the way the line editor submits a keystroke.
//!
//! What the dialect cannot spell (a bold+italic run, an image, a table) is
//! flattened on the way out; the guest never learns a shape it cannot store.
use super::{EditorStore, position};
use gpui_kit::{
    App, AppContext as _, Context, Entity, EventEmitter, Focusable as _, IntoElement,
    ParentElement as _, Render, Styled as _, Subscription, Window, div, px,
};
use gpui_notion::NotionEditor;
use gpui_notion::editor::block::{BlockAttrs, BlockContent, types};
use gpui_notion::editor::mark::{Mark, MarkKind, MarkList};
use gpui_notion::editor::view::{Caret, DocumentChanged};
use std::sync::Arc;
use ui_lang_wire as wire;

/// Two spaces per depth: `document_sync::INDENT`.
const INDENT: &str = "  ";
const FENCE: &str = "```";

/// The editor key suffix the host mounts on gpui-notion instead of the line
/// editor: the pages document, nothing else.
pub const NOTION_DOCUMENT_KEY: &str = "/pages/document";

/// Register gpui-notion after `gpui_kit::init`. The guest already sizes and
/// pads the document column, so the editor's own page column is flush: no
/// side padding, no width cap, a short tail under the last block.
pub fn init(cx: &mut App) {
    gpui_notion::editor::init(cx);
    gpui_notion::editor::EditorTheme::customize(cx, |theme, _| {
        theme.page_width = px(f32::MAX);
        theme.page_padding = px(0.);
        theme.page_bottom = theme.rem * 4.;
    });
}

pub struct NotionWireEditor {
    key: String,
    store: EditorStore,
    editor: Entity<NotionEditor>,
    /// The text the editor currently reflects — what the next native edit is
    /// diffed against, and what an echoed projection is compared with.
    installed: Arc<str>,
    cursor: wire::EditorCursor,
    reset: Option<u64>,
    fault: Option<String>,
    _changes: Subscription,
}

impl EventEmitter<()> for NotionWireEditor {}

impl NotionWireEditor {
    pub fn new(
        key: String,
        store: EditorStore,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| NotionEditor::new(window, cx));
        let changes = cx.subscribe_in(
            &editor,
            window,
            |this, _, _: &DocumentChanged, window, cx| this.changed(window, cx),
        );
        let mut this = Self {
            key,
            store,
            editor,
            installed: Arc::from(""),
            cursor: Default::default(),
            reset: None,
            fault: None,
            _changes: changes,
        };
        this.sync(window, cx);
        this
    }

    /// Install the projection when it settled on text this editor did not
    /// produce (another writer, a guest normalization, a page switch).
    pub fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(projection) = self.store.projection(&self.key) else {
            return;
        };
        self.note_fault(projection.fault.as_deref());
        // No text yet (a page just opened, its transfer in flight): nothing to
        // install — a blank rebuild here would blink the page and drop focus.
        let Some(canonical) = projection.text.clone() else {
            return;
        };
        let reset = self.reset != Some(projection.reference.reset);
        let settled = !projection.pending;
        let moved = projection.reference.cursor != self.cursor;
        let install = reset || (settled && (canonical != self.installed || moved));
        if !install {
            return;
        }
        self.installed = canonical;
        self.cursor = projection.reference.cursor;
        self.reset = Some(projection.reference.reset);
        let content = blocks_of(&self.installed);
        let unchanged = self.editor.read(cx).content() == content;
        if unchanged {
            return;
        }
        self.editor.update(cx, |editor, cx| {
            while let Some(id) = editor.block_id_at(0) {
                editor.remove_block(id, cx);
            }
            for (ix, block) in content.into_iter().enumerate() {
                editor.insert_block(ix, block, window, cx);
            }
        });
        cx.notify();
    }

    /// A store fault stops every editor on the document; say so once.
    fn note_fault(&mut self, fault: Option<&str>) {
        if fault == self.fault.as_deref() {
            return;
        }
        if let Some(fault) = fault {
            tracing::warn!(target: "ducktape::pages_editor", fault, "the notion editor store faulted");
        }
        self.fault = fault.map(str::to_owned);
    }

    fn changed(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let text = markdown_of(&self.editor.read(cx).content());
        if text.as_str() == &*self.installed {
            return;
        }
        let next = wire::EditorCursor {
            position: position(&text, changed_end(&self.installed, &text)),
            selection: None,
        };
        self.store.native(
            &self.key,
            &self.installed,
            self.cursor,
            &text,
            next,
            wire::EditorEditKind::Insert,
        );
        self.installed = Arc::from(text);
        self.cursor = next;
        cx.emit(());
        cx.notify();
    }

    pub fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.editor
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx)
    }

    pub fn widget_command(
        &mut self,
        command: &wire::WidgetCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !matches!(command, wire::WidgetCommand::Focus { .. }) {
            return false;
        }
        if self.is_focused(window, cx) {
            return true;
        }
        // The caret goes to the block the document cursor names, never to a
        // trailing paragraph the editor would have to insert: focusing a page
        // must not write to it.
        let line = self.cursor.position.line as usize;
        let column = self.cursor.position.column as usize;
        self.editor.update(cx, |editor, cx| {
            let last = editor.block_count().saturating_sub(1);
            let Some(id) = editor.block_id_at(line.min(last)) else {
                return;
            };
            editor.focus_block(id, Caret::At(column), window, cx);
        });
        true
    }
}

impl Render for NotionWireEditor {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.editor.clone())
    }
}

/// The byte in `after` just past the edit that turned `before` into it.
fn changed_end(before: &str, after: &str) -> usize {
    let prefix = before
        .bytes()
        .zip(after.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = before[prefix..]
        .bytes()
        .rev()
        .zip(after[prefix..].bytes().rev())
        .take_while(|(a, b)| a == b)
        .count();
    after.len() - suffix
}

// ----------------------------------------------------------------- markdown → blocks

/// The canonical text as blocks: line 0 is the title, every later line one
/// block of the guest dialect. Depth is clamped to the line above's + 1, the
/// only shape the guest tree can hold.
pub fn blocks_of(text: &str) -> Vec<BlockContent> {
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
pub fn markdown_of(blocks: &[BlockContent]) -> String {
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
    let marker: String = match block.ty.as_ref() {
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
        Some(MarkKind::Link(url)) if url.as_ref() == body => body.to_string(),
        Some(MarkKind::Link(url)) => format!("[{body}]({url})"),
        Some(MarkKind::Bold) => format!("**{body}**"),
        Some(MarkKind::Italic) => format!("*{body}*"),
        Some(MarkKind::Strike) => format!("~~{body}~~"),
        Some(MarkKind::Underline) => format!("++{body}++"),
        Some(MarkKind::Highlight(_)) => format!("=={body}=="),
        _ => body.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOCUMENT: &str = "Welcome\n# Heading\nPlain **bold** and *it* and `code`\n- one\n  - [x] nested\n1. first\n2. second\n> quote\n!> callout\n+ toggle\n---\n```rust\nfn main() {}\n```\nSee [docs](https://x.y) or https://a.b and @ada\n";

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
            .map(|block| (block.ty.as_ref(), block.indent))
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
    fn changed_end_lands_after_the_edit() {
        assert_eq!(changed_end("T\nhello", "T\nhello world"), 13);
        assert_eq!(changed_end("T\nhello world", "T\nhello"), 7);
        assert_eq!(changed_end("abc", "abc"), 3);
    }
}
