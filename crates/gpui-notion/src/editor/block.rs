//! Blocks: the node types of the document.
//!
//! A block is a type name plus attributes plus text; everything a type *does*
//! — how it looks, what it renders beside its text, which markdown rule
//! creates it, what Enter and Backspace mean inside it — lives in a
//! [`BlockSpec`] held by the [`BlockRegistry`]. Adding a node type means
//! writing one spec and registering it; no core file needs to change.

use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::component::input::{EditorState, TextDecorationCollection};
use gpui_kit::{
    AnyElement, App, Entity, FontWeight, Global, IntoElement, Pixels, SharedString, Subscription,
    WeakEntity, Window, px,
};

use super::mark::{MarkKind, MarkList};
use super::theme::EditorTheme;
use super::view::NotionEditor;

/// Stable identity of a block, used by focus, drag & drop and selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u64);

/// Node type name, matching the Tiptap/ProseMirror schema names.
pub type BlockType = SharedString;

pub mod types {
    pub const PARAGRAPH: &str = "paragraph";
    pub const HEADING: &str = "heading";
    pub const BULLET_LIST: &str = "bulletList";
    pub const ORDERED_LIST: &str = "orderedList";
    pub const TASK_LIST: &str = "taskList";
    pub const BLOCKQUOTE: &str = "blockquote";
    pub const CODE_BLOCK: &str = "codeBlock";
    pub const HORIZONTAL_RULE: &str = "horizontalRule";
    pub const IMAGE: &str = "image";
    pub const CALLOUT: &str = "callout";
    pub const TOGGLE: &str = "details";
    pub const TABLE: &str = "table";
}

/// Attributes of a block. The named fields cover the built-in node types;
/// extensions put their own in `extra`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BlockAttrs {
    /// Heading level, 1-6.
    pub level: u8,
    /// Checked state of a to-do item.
    pub checked: bool,
    /// Collapsed state of a toggle.
    pub collapsed: bool,
    /// Ordered-list start number.
    pub start: Option<usize>,
    pub language: Option<SharedString>,
    pub src: Option<SharedString>,
    pub alt: Option<SharedString>,
    pub emoji: Option<SharedString>,
    pub extra: HashMap<SharedString, SharedString>,
}

impl BlockAttrs {
    pub fn level(level: u8) -> Self {
        Self {
            level: level.clamp(1, 6),
            ..Default::default()
        }
    }

    pub fn language(language: impl Into<SharedString>) -> Self {
        Self {
            language: Some(language.into()),
            ..Default::default()
        }
    }

    /// Read an attribute a block extension defined for itself.
    pub fn extra(&self, key: &str) -> Option<&SharedString> {
        self.extra.get(key)
    }

    /// Set an attribute a block extension defined for itself.
    pub fn set_extra(&mut self, key: impl Into<SharedString>, value: impl Into<SharedString>) {
        self.extra.insert(key.into(), value.into());
    }
}

/// What a node type permits, mirroring the ProseMirror schema flags the
/// Notion-like editor relies on.
#[derive(Clone, Copy, Debug)]
pub struct BlockCaps {
    /// Holds editable text (false for separators and images).
    pub textual: bool,
    /// Inline marks apply inside it.
    pub marks: bool,
    /// Markdown input rules run inside it.
    pub input_rules: bool,
    /// Belongs to a list: Tab nests, Enter continues the list.
    pub list: bool,
    /// Enter on an empty block of this type lifts it to a paragraph.
    pub lifts_when_empty: bool,
    /// Enter inserts a newline instead of splitting the block.
    pub multiline: bool,
    /// The editor keeps a grid of child text areas beside the block.
    pub grid: bool,
}

impl Default for BlockCaps {
    fn default() -> Self {
        Self {
            textual: true,
            marks: true,
            input_rules: true,
            list: false,
            lifts_when_empty: false,
            multiline: false,
            grid: false,
        }
    }
}

impl BlockCaps {
    pub fn atom() -> Self {
        Self {
            textual: false,
            marks: false,
            input_rules: false,
            ..Default::default()
        }
    }

    /// A block whose content lives in child cells rather than in its own
    /// input — what a table is.
    pub fn grid() -> Self {
        Self {
            textual: false,
            marks: false,
            input_rules: false,
            grid: true,
            ..Default::default()
        }
    }

    pub fn list() -> Self {
        Self {
            list: true,
            lifts_when_empty: true,
            ..Default::default()
        }
    }
}

/// Typography and spacing of a block.
#[derive(Clone, Copy, Debug)]
pub struct BlockLayout {
    pub text_size: Pixels,
    pub font_weight: FontWeight,
    pub line_height: f32,
    pub margin_top: Pixels,
    pub margin_bottom: Pixels,
    /// Monospace text (code blocks).
    pub mono: bool,
    /// Width reserved left of the text for a marker.
    pub leading_width: Pixels,
    /// Padding inside the block's own surface, left and right.
    pub inner_padding: Pixels,
    /// Drop the top margin when the block above has the same type, the way
    /// CSS collapses margins between list items.
    pub collapse_with_siblings: bool,
    /// Opacity of the text, used by completed to-do items.
    pub text_opacity: f32,
    /// Strike the whole block's text, used by completed to-do items.
    pub strikethrough: bool,
}

impl BlockLayout {
    /// How an ordinary paragraph is set, which every other block starts from.
    pub fn new(theme: &EditorTheme) -> Self {
        Self {
            text_size: theme.text_size,
            font_weight: FontWeight::NORMAL,
            line_height: theme.line_height,
            margin_top: theme.block_gap,
            margin_bottom: px(0.),
            mono: false,
            leading_width: px(0.),
            inner_padding: px(0.),
            collapse_with_siblings: false,
            text_opacity: 1.0,
            strikethrough: false,
        }
    }

    pub fn line_height_px(&self) -> Pixels {
        self.text_size * self.line_height
    }
}

/// A markdown input rule that creates this node type.
pub struct BlockInputRule {
    /// Regex matched against the text from the block start to the caret.
    pub pattern: &'static str,
    /// Builds the node from the match; `None` declines.
    pub build: fn(&regex::Captures) -> Option<(BlockType, BlockAttrs)>,
}

/// An entry this node type contributes to the slash menu.
pub struct SlashItem {
    pub title: &'static str,
    pub subtext: &'static str,
    pub keywords: &'static [&'static str],
    pub group: &'static str,
    pub icon: &'static str,
    /// What the item does; the query text has already been removed.
    pub run: fn(&mut NotionEditor, &mut Window, &mut gpui_kit::Context<NotionEditor>),
}

/// Everything a spec needs to render one block.
pub struct BlockContext<'a> {
    pub id: BlockId,
    /// Index of the block in the document.
    pub ix: usize,
    pub attrs: &'a BlockAttrs,
    pub text: &'a str,
    pub indent: usize,
    pub focused: bool,
    pub selected: bool,
    /// Position within a run of list items at the same indent, 1-based.
    pub ordinal: usize,
    /// Child text areas, for specs whose caps ask for a grid.
    pub grid: Option<&'a super::grid::CellGrid>,
    /// Review aid: draw hover-only controls as if the pointer were here.
    pub reveal_controls: bool,
    /// Line height the block's input laid out with.
    pub line_height: Pixels,
    /// Width of the marker column this block's layout asks for.
    pub leading_width: Pixels,
    /// The tokens the document is drawn with.
    pub theme: std::rc::Rc<EditorTheme>,
    pub editor: WeakEntity<NotionEditor>,
}

/// The behavior of a node type. Implement it and register it to add a block.
pub trait BlockSpec: 'static + Send + Sync {
    /// Schema name, matching Tiptap's JSON `type`.
    fn type_name(&self) -> &'static str;

    /// Label shown in "Turn into" and the slash menu.
    fn label(&self, attrs: &BlockAttrs) -> SharedString;

    fn caps(&self) -> BlockCaps {
        BlockCaps::default()
    }

    /// How this block is set, in the tokens in force. A spec reads its sizes
    /// and spacing from `theme` rather than stating pixels, so a document
    /// zooms and re-themes as one piece.
    fn layout(&self, attrs: &BlockAttrs, theme: &EditorTheme) -> BlockLayout {
        let _ = attrs;
        BlockLayout::new(theme)
    }

    fn placeholder(&self, attrs: &BlockAttrs) -> SharedString {
        let _ = attrs;
        SharedString::default()
    }

    /// Show the placeholder even when the block is not focused.
    fn placeholder_always(&self) -> bool {
        false
    }

    /// The marker drawn left of the text: bullet, number, checkbox.
    fn render_leading(
        &self,
        ctx: &BlockContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let (_, _, _) = (ctx, window, cx);
        None
    }

    /// Decorate the block's own surface: quote bar, code background, callout.
    fn wrap(
        &self,
        ctx: &BlockContext,
        content: AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let (_, _, _) = (ctx, window, cx);
        content
    }

    /// Body of a non-textual node, drawn instead of a text input.
    fn render_body(
        &self,
        ctx: &BlockContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let (_, _, _) = (ctx, window, cx);
        None
    }

    /// The node type a new block takes when Enter splits this one.
    ///
    /// `at_end` says whether the caret sat at the end of the text, which is
    /// what decides a heading's answer: Enter at the end of a heading starts a
    /// paragraph, Enter inside it makes a second heading.
    fn split_into(&self, attrs: &BlockAttrs, at_end: bool) -> (BlockType, BlockAttrs) {
        let (_, _) = (attrs, at_end);
        (types::PARAGRAPH.into(), BlockAttrs::default())
    }

    /// Markdown rules that turn a paragraph into this node.
    fn input_rules(&self) -> Vec<BlockInputRule> {
        Vec::new()
    }

    /// Slash-menu entries for this node.
    fn slash_items(&self) -> Vec<SlashItem> {
        Vec::new()
    }

    /// Marks carried into the block when it is created by "turn into".
    fn keeps_marks(&self) -> bool {
        self.caps().marks
    }
}

/// The registered node types.
pub struct BlockRegistry {
    specs: Vec<Arc<dyn BlockSpec>>,
    by_name: HashMap<&'static str, usize>,
    fallback: Arc<dyn BlockSpec>,
}

impl Global for BlockRegistry {}

impl BlockRegistry {
    pub fn new(fallback: Arc<dyn BlockSpec>) -> Self {
        let mut this = Self {
            specs: Vec::new(),
            by_name: HashMap::new(),
            fallback: fallback.clone(),
        };
        this.push(fallback);
        this
    }

    pub fn global(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    /// Register a node type, replacing one of the same name.
    pub fn register(cx: &mut App, spec: impl BlockSpec) {
        cx.global_mut::<Self>().push(Arc::new(spec));
    }

    fn push(&mut self, spec: Arc<dyn BlockSpec>) {
        let name = spec.type_name();
        match self.by_name.get(name) {
            Some(&ix) => self.specs[ix] = spec,
            None => {
                self.by_name.insert(name, self.specs.len());
                self.specs.push(spec);
            }
        }
    }

    /// Spec for a node type; unknown types fall back to the paragraph, so a
    /// document from a newer schema still renders its text.
    pub fn get(&self, ty: &str) -> Arc<dyn BlockSpec> {
        self.by_name
            .get(ty)
            .map(|&ix| self.specs[ix].clone())
            .unwrap_or_else(|| self.fallback.clone())
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn BlockSpec>> {
        self.specs.iter()
    }

    /// Every node input rule, longest pattern first so `###` beats `#`.
    pub fn input_rules(&self) -> Vec<(BlockType, BlockInputRule)> {
        let mut rules: Vec<_> = self
            .specs
            .iter()
            .flat_map(|spec| {
                spec.input_rules()
                    .into_iter()
                    .map(|rule| (SharedString::from(spec.type_name()), rule))
            })
            .collect();
        rules.sort_by_key(|(_, rule)| std::cmp::Reverse(rule.pattern.len()));
        rules
    }

    pub fn slash_items(&self) -> Vec<SlashItem> {
        self.specs
            .iter()
            .flat_map(|spec| spec.slash_items())
            .collect()
    }
}

/// One block of the document: a node, its text and marks, and the input state
/// that renders and edits it.
pub struct Block {
    pub id: BlockId,
    pub ty: BlockType,
    pub attrs: BlockAttrs,
    /// Mirror of the input's text, kept in step to diff edits for marks.
    pub(crate) text: String,
    pub(crate) marks: MarkList,
    /// Marks applied to the next typed character at a collapsed caret.
    pub(crate) stored_marks: Option<Vec<MarkKind>>,
    pub(crate) state: Entity<EditorState>,
    pub(crate) decorations: Option<TextDecorationCollection>,
    /// Nesting depth inside lists; 0 at the top level.
    pub(crate) indent: usize,
    /// Rows the text wraps into at the current column width. Measuring text
    /// is not free, so it is done when the text changes, not every frame.
    pub(crate) rows: usize,
    /// How much of its height this block's input keeps for itself.
    pub(crate) fit: super::fit::InputFit,
    pub(crate) subscriptions: Vec<Subscription>,
}

impl Block {
    /// Text of the block, as the document holds it.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Inline formatting of the block.
    pub fn marks(&self) -> &MarkList {
        &self.marks
    }

    /// Nesting depth inside lists.
    pub fn indent(&self) -> usize {
        self.indent
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn spec(&self, cx: &App) -> Arc<dyn BlockSpec> {
        BlockRegistry::global(cx).get(&self.ty)
    }

    pub fn content(&self) -> BlockContent {
        BlockContent {
            ty: self.ty.clone(),
            attrs: self.attrs.clone(),
            text: self.text.clone(),
            marks: self.marks.clone(),
            indent: self.indent,
        }
    }
}

/// A block's content without its input state — the unit of clipboard, undo
/// and serialization.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockContent {
    pub ty: BlockType,
    pub attrs: BlockAttrs,
    pub text: String,
    pub marks: MarkList,
    pub indent: usize,
}

impl Default for BlockContent {
    fn default() -> Self {
        Self {
            ty: types::PARAGRAPH.into(),
            attrs: BlockAttrs::default(),
            text: String::new(),
            marks: MarkList::new(),
            indent: 0,
        }
    }
}

impl BlockContent {
    pub fn paragraph(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Default::default()
        }
    }

    pub fn new(ty: impl Into<BlockType>, text: impl Into<String>) -> Self {
        Self {
            ty: ty.into(),
            text: text.into(),
            ..Default::default()
        }
    }

    pub fn with_attrs(mut self, attrs: BlockAttrs) -> Self {
        self.attrs = attrs;
        self
    }

    pub fn with_marks(mut self, marks: MarkList) -> Self {
        self.marks = marks;
        self
    }

    pub fn with_indent(mut self, indent: usize) -> Self {
        self.indent = indent;
        self
    }
}

/// Convenience for specs: an element sized to one line of the block's text.
/// The column a bullet, number or checkbox sits in, one line tall, so the
/// marker centres on the first line of the block's text. The line height is
/// the one the input laid out with, not the one the layout asked for: they
/// differ by the rounding the text system applies.
pub fn leading_slot(ctx: &BlockContext, child: impl IntoElement) -> AnyElement {
    use gpui_kit::{ParentElement as _, Styled as _, div};
    div()
        .w(ctx.leading_width)
        .h(ctx.line_height)
        .flex()
        .items_center()
        .flex_none()
        .child(child)
        .into_any_element()
}
