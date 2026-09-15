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
    AnyElement, App, AppContext as _, Bounds, Context, Entity, EventEmitter, Focusable as _,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, canvas, div, px,
};
use gpui_notion::NotionEditor;
use gpui_notion::editor::block::{BlockAttrs, BlockContent, types};
use gpui_notion::editor::comments::ThreadId;
use gpui_notion::editor::mark::{Mark, MarkKind, MarkList};
use gpui_notion::editor::theme::ActiveEditorTheme as _;
use gpui_notion::editor::view::{Caret, DocumentChanged};
use std::collections::HashSet;
use std::sync::Arc;
use ui_lang_wire as wire;
use wire::editor_presentation::{EditorInteraction, EditorMargin};

/// Two spaces per depth: `document_sync::INDENT`.
const INDENT: &str = "  ";
const FENCE: &str = "```";

/// The editor key suffix the host mounts on gpui-notion instead of the line
/// editor: the pages document, nothing else.
pub const NOTION_DOCUMENT_KEY: &str = "/pages/document";

/// Register gpui-notion after `gpui_kit::init`. The guest already sizes and
/// pads the document column, so the editor's own page column is flush with
/// it: no width cap, a short tail under the last block, and exactly the
/// gutter's width of side padding — the mount pulls the editor out by that
/// much on both sides (see `Render`), so the text lands on the guest column
/// and the hover "+ ⠿" controls hang in the guest's left padding.
pub fn init(cx: &mut App) {
    gpui_notion::editor::init(cx);
    gpui_notion::editor::EditorTheme::customize(cx, |theme, _| {
        theme.page_width = px(f32::MAX);
        theme.page_padding = theme.gutter_controls_width;
        theme.page_bottom = theme.rem * 4.;
    });
}

/// The badge's height: one marker slot.
const BADGE_HEIGHT: f32 = 22.;

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
    /// Where the mount painted last frame, so block bounds (window space)
    /// can be turned into overlay offsets.
    bounds: Option<Bounds<Pixels>>,
    /// The guest's comment badges: one per commented line, with its count.
    margins: Vec<EditorMargin>,
    /// gpui-notion threads already handed to the guest. The editor's own
    /// thread model is a stepping stone: a thread it opens is taken straight
    /// to the guest's card, which owns comments on the module.
    threads: HashSet<ThreadId>,
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
            bounds: None,
            margins: Vec::new(),
            threads: HashSet::new(),
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
        let margins = projection
            .options
            .presentation
            .as_ref()
            .map(|paint| paint.affordances.margins.clone())
            .unwrap_or_default();
        if margins != self.margins {
            self.margins = margins;
            cx.notify();
        }
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
        if self.hand_over_new_thread(cx) {
            return;
        }
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

    /// The toolbar's "Comment" opened a gpui-notion thread on the selection.
    /// The guest's card owns comments, so the thread is dropped here and the
    /// guest gets the same ask: the selection as the document cursor, then a
    /// margin press on its line, which opens the card anchored on the words.
    fn hand_over_new_thread(&mut self, cx: &mut Context<Self>) -> bool {
        let editor = self.editor.read(cx);
        let Some(thread) = editor
            .comment_threads()
            .iter()
            .find(|thread| !self.threads.contains(&thread.id()))
        else {
            return false;
        };
        let id = thread.id();
        let block = thread.block();
        self.threads.insert(id);
        let ix = editor.index_of(block);
        let range = editor.block(block).and_then(|block| {
            block
                .marks()
                .iter()
                .find(|mark| mark.kind == MarkKind::Comment(id))
                .map(|mark| mark.range.clone())
        });
        let content = editor.content();
        self.editor
            .update(cx, |editor, cx| editor.remove_comment_thread(id, cx));
        let (Some(ix), Some(range)) = (ix, range) else {
            return true;
        };
        let line = block_starts(&self.installed).get(ix).copied().unwrap_or(0);
        let Some(block) = content.get(ix) else {
            return true;
        };
        let prefix = line_of(block, 1).len() - inline_of(block).len();
        let at = |plain: usize| (prefix + fenced_column(block, plain)) as u32;
        let cursor = wire::EditorCursor {
            position: wire::EditorPosition {
                line: line as u32,
                column: at(range.end),
            },
            selection: Some(wire::EditorPosition {
                line: line as u32,
                column: at(range.start),
            }),
        };
        self.store.native(
            &self.key,
            &self.installed,
            self.cursor,
            &self.installed,
            cursor,
            wire::EditorEditKind::Cursor,
        );
        self.cursor = cursor;
        self.interaction(EditorInteraction::Margin { line: line as u32 }, cx);
        true
    }

    fn interaction(&mut self, action: EditorInteraction, cx: &mut Context<Self>) {
        self.store
            .request(&self.key, wire::EditorRequestInput::Interaction { action });
        cx.emit(());
        cx.notify();
    }

    /// One badge per commented block, on the block's last line at the text
    /// column's right edge; pressing it opens the guest's card for the block.
    fn badges(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(origin) = self.bounds.map(|bounds| bounds.origin) else {
            return Vec::new();
        };
        let starts = block_starts(&self.installed);
        let editor = self.editor.read(cx);
        let theme = cx.editor_theme().clone();
        self.margins
            .iter()
            .filter_map(|margin| {
                let line = margin.line as usize;
                let ix = starts.iter().rposition(|start| *start <= line)?;
                let bounds = editor.block_bounds(editor.block_id_at(ix)?)?;
                let top = bounds.bottom() - px(BADGE_HEIGHT) - origin.y;
                let line = margin.line;
                Some(
                    div()
                        .id(("comments", line as usize))
                        .absolute()
                        .right(px(0.))
                        .top(top)
                        .h(px(BADGE_HEIGHT))
                        .px(theme.rems(0.375))
                        .flex()
                        .items_center()
                        .gap(theme.rems(0.25))
                        .rounded(theme.radius_sm)
                        .bg(theme.comment_fill)
                        .text_color(theme.comment_accent)
                        .text_size(theme.ui_small_text_size)
                        .cursor_pointer()
                        .child(gpui_notion::editor::ui::icon(
                            "message-square",
                            theme.ui_small_text_size,
                            theme.comment_accent,
                        ))
                        .child(margin.count.to_string())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.interaction(EditorInteraction::Margin { line }, cx)
                        }))
                        .into_any_element(),
                )
            })
            .collect()
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The editor is pulled out of the mount by its gutter width on both
        // sides and pads itself back in by the same amount (`init`): its text
        // column is the mount's box, and the gutter controls hang to the left
        // of it, in the guest's own padding.
        let gutter = cx.editor_theme().gutter_controls_width;
        let weak = cx.entity().downgrade();
        let probe = canvas(
            move |bounds, _, cx| {
                let _ = weak.update(cx, |this, cx| {
                    if this.bounds != Some(bounds) {
                        this.bounds = Some(bounds);
                        cx.notify();
                    }
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0();
        let badges = self.badges(cx);
        div()
            .size_full()
            .relative()
            .flex()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .ml(-gutter)
                    .mr(-gutter)
                    .child(self.editor.clone()),
            )
            .child(probe)
            .children(badges)
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

    #[test]
    fn changed_end_lands_after_the_edit() {
        assert_eq!(changed_end("T\nhello", "T\nhello world"), 13);
        assert_eq!(changed_end("T\nhello world", "T\nhello"), 7);
        assert_eq!(changed_end("abc", "abc"), 3);
    }
}
