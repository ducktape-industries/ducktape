//! The editor view: the document, its blocks, and everything that happens to
//! them. State lives in one entity, the way GPUI wants it; each block owns a
//! child `EditorState` entity for its own text.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::component::input::{Editor, EditorState, InputEvent};
use gpui_kit::DragMoveEvent;
use gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Font, FontStyle, FontWeight, HighlightStyle, InteractiveElement as _, IntoElement, LineFragment,
    ParentElement as _, Pixels, Render, SharedString, StatefulInteractiveElement as _,
    StrikethroughStyle, Styled as _, UnderlineStyle, Window, canvas, div, px, relative,
};

use super::actions;
use super::block::{
    Block, BlockAttrs, BlockContext, BlockContent, BlockId, BlockLayout, BlockRegistry, BlockSpec,
    BlockType, types,
};
use gpui_kit::TestSupportExt as _;

use super::gutter::DraggedBlock;
use super::mark::{MarkKind, diff_edit};
use super::theme::ActiveEditorTheme;

/// Emitted when the document changes, so a host can persist it.
pub struct DocumentChanged;

/// The focused block's caret or selection changed without editing text.
pub struct SelectionChanged;

/// Emitted when a reader presses a link, carrying its href. The editor has no
/// opinion on what an href names — a page in the host's own workspace as
/// readily as a web address — so opening one is the host's call.
pub struct LinkPressed(pub SharedString);

pub struct NotionEditor {
    pub(crate) blocks: Vec<Block>,
    next_id: u64,
    focus_handle: FocusHandle,
    /// Block whose input currently holds focus.
    pub(crate) focused: Option<BlockId>,
    /// Review aid: show every block's gutter controls without hovering.
    pub(crate) always_show_gutter: bool,
    /// Where each block sits on screen, refreshed every frame, so the pointer
    /// can be turned into a block.
    pub(crate) block_bounds: HashMap<BlockId, Bounds<Pixels>>,
    /// The block a press started in, which anchors a drag selection.
    pub(crate) mouse_anchor: Option<BlockId>,
    /// Where each block's text was given room to start, refreshed per frame.
    pub(crate) text_slots: HashMap<BlockId, gpui_kit::Point<Pixels>>,
    /// Child text areas of blocks that hold a grid, keyed by block.
    pub(crate) grids: HashMap<BlockId, super::grid::CellGrid>,
    /// Which cell has the caret, when one does.
    pub(crate) focused_cell: Option<(BlockId, super::grid::CellPosition)>,
    /// Kept alive so a theme change repaints the marks.
    theme: Vec<gpui_kit::Subscription>,
    /// Comment threads, and which one is on screen.
    pub(crate) comments: Vec<super::comments::Thread>,
    pub(crate) next_thread_id: u64,
    pub(crate) open_thread: Option<super::comments::ThreadId>,
    pub(crate) comment_draft: Option<super::comments::CommentDraft>,
    /// Blocks selected as nodes, e.g. by a drag or Escape.
    pub(crate) selected: Vec<BlockId>,
    /// Width the text column last laid out at, for wrapping measurements.
    pub(crate) wrap_width: Pixels,
    /// The open suggestion menu, if any.
    pub(crate) suggestion: Option<super::slash::SuggestionMenu>,
    pub(crate) menu_source: super::slash::MenuSource,
    pub(crate) input_rule_mode: super::input_rules::InputRuleMode,
    /// People offered by the `@` menu.
    pub(crate) mentions: Vec<super::suggestion::Mention>,
    /// Where a dragged block would land.
    pub(crate) drop_target: Option<super::gutter::DropTarget>,
    /// The block whose gutter menu is open. Its controls are hover-shown, and
    /// reaching for the menu takes the pointer off them.
    pub(crate) gutter_menu: Option<BlockId>,
    /// The open link editor, if any.
    pub(crate) link_editor: Option<super::toolbar::LinkEditor>,
    /// Undo and redo for the document as a whole.
    pub(crate) history: super::history::History,
    pub(crate) annotation_mode: super::comments::AnnotationMode,
    pub(crate) toolbar: Option<Vec<super::toolbar::ToolbarItem>>,
}

impl NotionEditor {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            blocks: Vec::new(),
            next_id: 1,
            focus_handle: cx.focus_handle(),
            focused: None,
            always_show_gutter: false,
            block_bounds: HashMap::new(),
            text_slots: HashMap::new(),
            mouse_anchor: None,
            grids: HashMap::new(),
            focused_cell: None,
            comments: Vec::new(),
            next_thread_id: 1,
            open_thread: None,
            comment_draft: None,
            selected: Vec::new(),
            wrap_width: cx.editor_theme().page_width - cx.editor_theme().page_padding * 2.,
            suggestion: None,
            menu_source: Default::default(),
            input_rule_mode: Default::default(),
            mentions: super::suggestion::default_mentions(),
            drop_target: None,
            gutter_menu: None,
            link_editor: None,
            history: super::history::History::default(),
            annotation_mode: Default::default(),
            toolbar: None,
            theme: Vec::new(),
        };
        // Mark colours are baked into the decoration layer when they are
        // applied, so a theme change has to repaint them.
        this.theme = vec![cx.observe_global::<gpui_kit::component::Theme>(|this, cx| {
            this.reapply_decorations(cx)
        })];
        this.insert_block(0, BlockContent::paragraph(""), window, cx);
        this
    }

    /// Build an editor holding `content`.
    pub fn with_content(
        content: Vec<BlockContent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self::new(window, cx);
        if !content.is_empty() {
            this.blocks.clear();
            for (ix, block) in content.into_iter().enumerate() {
                this.insert_block(ix, block, window, cx);
            }
        }
        this
    }

    // ---------------------------------------------------------------- lookup

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    pub fn index_of(&self, id: BlockId) -> Option<usize> {
        self.blocks.iter().position(|b| b.id == id)
    }

    pub fn block(&self, id: BlockId) -> Option<&Block> {
        self.blocks.iter().find(|b| b.id == id)
    }

    pub fn block_mut(&mut self, id: BlockId) -> Option<&mut Block> {
        self.blocks.iter_mut().find(|b| b.id == id)
    }

    pub fn spec_at(&self, ix: usize, cx: &App) -> Arc<dyn BlockSpec> {
        BlockRegistry::global(cx).get(&self.blocks[ix].ty)
    }

    /// The block that commands act on: the focused one, else the first.
    pub fn active_id(&self) -> Option<BlockId> {
        self.focused
            .or_else(|| self.selected.first().copied())
            .or_else(|| self.blocks.first().map(|b| b.id))
    }

    /// Adopt whichever block's input holds keyboard focus.
    ///
    /// A block input takes focus from a click without the editor hearing
    /// about it, so the window is the authority on which block is active and
    /// the field below is a cache of it, refreshed each frame.
    /// Reads which block input the window is focused on, and answers whether
    /// one of them holds the caret at all.
    pub(crate) fn refresh_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        use gpui_kit::Focusable as _;
        let focused = self
            .blocks
            .iter()
            .find(|block| block.state.focus_handle(cx).is_focused(window))
            .map(|block| block.id);
        if focused == self.focused {
            return focused.is_some();
        }
        if focused.is_none() {
            // Focus went somewhere that is not a block. A popover that edits
            // the block it came from keeps it; anything else — a table cell,
            // another window — means no block holds the caret, and the
            // toolbar, the placeholder and every command have to know.
            if self.link_editor_is_open() || self.comment_draft_is_open() {
                return false;
            }
            self.focused = None;
            self.sync_placeholders(window, cx);
            cx.notify();
            return false;
        }
        self.focused = focused;
        self.selected.clear();
        self.sync_placeholders(window, cx);
        true
    }

    /// Hint the block being written in, and any block that always hints.
    ///
    /// Tiptap's template shows "Write, type '/' for commands…" only in the
    /// focused empty paragraph, while headings always name themselves.
    pub(crate) fn sync_placeholders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let registry = BlockRegistry::global(cx);
        let wanted: Vec<(usize, SharedString)> = self
            .blocks
            .iter()
            .enumerate()
            .map(|(ix, block)| {
                let spec = registry.get(&block.ty);
                let show = spec.placeholder_always() || self.focused == Some(block.id);
                let text = if show {
                    placeholder_of(&spec, &block.attrs)
                } else {
                    SharedString::default()
                };
                (ix, text)
            })
            .collect();

        for (ix, text) in wanted {
            let state = self.blocks[ix].state.clone();
            state.update(cx, |state, cx| state.set_placeholder(text, window, cx));
        }
    }

    /// Review aid: keep the gutter controls on screen for every block, for
    /// screenshots taken without a pointer. Off unless an application asks.
    pub fn show_gutter_always(&mut self) {
        self.always_show_gutter = true;
    }

    /// Focus a block and select a byte range inside it.
    pub fn select_text_in_block(
        &mut self,
        ix: usize,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(block) = self.blocks.get(ix) else {
            return;
        };
        let (id, state) = (block.id, block.state.clone());
        self.focus_block(id, Caret::At(range.start), window, cx);
        state.update(cx, |state, cx| state.set_selected_range(range, cx));
        cx.notify();
    }

    /// Put the caret in the last block of the document.
    pub fn focus_last_block(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.blocks.last().map(|block| block.id) else {
            return;
        };
        self.focus_block(id, Caret::End, window, cx);
    }

    /// Whether the block at `ix` is an empty paragraph, i.e. nothing would be
    /// gained by appending another one after it.
    pub(crate) fn block_is_empty_paragraph(&self, ix: usize) -> bool {
        self.blocks
            .get(ix)
            .is_some_and(|block| block.ty == types::PARAGRAPH && block.text.is_empty())
    }

    /// How far a block's input has scrolled inside itself. A block sized to
    /// its own text should never scroll; this is what proves it.
    pub fn block_scroll_offset(&self, id: BlockId, cx: &App) -> Option<gpui_kit::Point<Pixels>> {
        Some(self.block(id)?.state.read(cx).scroll_offset())
    }

    /// Forget a drop target, however the drag ended.
    pub(crate) fn clear_drop_target(&mut self, cx: &mut Context<Self>) {
        if self.drop_target.take().is_some() {
            cx.notify();
        }
    }

    /// Repaint every block's inline marks, after something the colours are
    /// read from has changed.
    pub(crate) fn reapply_decorations(&mut self, cx: &mut Context<Self>) {
        for id in self.blocks.iter().map(|block| block.id).collect::<Vec<_>>() {
            self.apply_decorations(id, cx);
        }
        cx.notify();
    }

    /// Re-count the wrapped rows of every block, after the width they wrap
    /// at has changed.
    pub(crate) fn remeasure_all(&mut self, cx: &mut Context<Self>) {
        for id in self.blocks.iter().map(|block| block.id).collect::<Vec<_>>() {
            self.remeasure(id, cx);
        }
    }

    /// The line height a block's input laid out with, which is the one the
    /// markers and gutter controls have to line up against.
    fn line_height_at(&self, ix: usize, cx: &App) -> Pixels {
        let layout = self.layout_at(ix, cx);
        self.blocks[ix]
            .state
            .read(cx)
            .line_height()
            .unwrap_or_else(|| layout.line_height_px())
    }

    /// Where a block's first glyph actually sits on screen.
    ///
    /// This is not the origin of the input's text area: the input insets the
    /// glyphs from it, so anything that has to line up with the text — the
    /// selection toolbar, a comment popover — asks the input where the text
    /// is rather than working it out from the block's box.
    pub fn block_text_origin(&self, id: BlockId, cx: &App) -> Option<gpui_kit::Point<Pixels>> {
        let state = self.block(id)?.state.read(cx);
        state
            .range_to_bounds(&(0..0))
            .map(|bounds| bounds.origin)
            .or_else(|| state.text_bounds().map(|bounds| bounds.origin))
    }

    /// Review aid: the input's own box and where it puts the first glyph.
    pub fn input_geometry(&self, id: BlockId, cx: &App) -> Option<(f32, f32, f32, f32, f32, f32)> {
        let state = self.block(id)?.state.read(cx);
        let area = state.text_bounds()?;
        let glyph = state.range_to_bounds(&(0..0))?.origin;
        Some((
            f32::from(area.origin.x),
            f32::from(area.origin.y),
            f32::from(area.size.width),
            f32::from(area.size.height),
            f32::from(glyph.x),
            f32::from(glyph.y),
        ))
    }

    /// Whether a block's text area is at least as tall as the text in it.
    ///
    /// This is the invariant that keeps a document still: a text area shorter
    /// than its text scrolls inside the block as the caret moves between
    /// rows, and the text appears to jump.
    pub fn text_area_fits_text(&self, id: BlockId, cx: &App) -> bool {
        let Some(ix) = self.index_of(id) else {
            return true;
        };
        let layout = self.layout_at(ix, cx);
        let needed = self.block_text_height(ix, &layout, cx);
        let area = self.blocks[ix]
            .state
            .read(cx)
            .text_bounds()
            .map(|bounds| bounds.size.height);
        super::fit::InputFit::fits(needed, area)
    }

    /// Caret offset inside a block, in bytes.
    pub fn caret_offset(&self, id: BlockId, cx: &App) -> Option<usize> {
        Some(self.block(id)?.state.read(cx).cursor())
    }

    /// Id of the block at `ix`, for tests and hosts.
    pub fn block_id_at(&self, ix: usize) -> Option<BlockId> {
        self.blocks.get(ix).map(|block| block.id)
    }

    /// Focus handle of the block at `ix`, for tests and hosts.
    pub fn block_focus_handle(&self, ix: usize, cx: &App) -> Option<FocusHandle> {
        use gpui_kit::Focusable as _;
        Some(self.blocks.get(ix)?.state.focus_handle(cx))
    }

    /// The block whose input holds focus, if any.
    pub fn focused_id(&self) -> Option<BlockId> {
        self.focused
    }

    pub fn active_index(&self) -> Option<usize> {
        self.active_id().and_then(|id| self.index_of(id))
    }

    /// Caret or selection in the active block, in byte offsets.
    pub fn selection(&self, cx: &App) -> Option<(BlockId, Range<usize>)> {
        let id = self.active_id()?;
        let block = self.block(id)?;
        Some((id, block.state.read(cx).selected_range()))
    }

    pub fn content(&self) -> Vec<BlockContent> {
        self.blocks.iter().map(|b| b.content()).collect()
    }

    /// The hint a block is showing, for tests that check which one it takes.
    pub fn placeholder_at(&self, ix: usize, cx: &App) -> SharedString {
        self.blocks[ix]
            .state
            .read(cx)
            .presentation()
            .placeholder()
            .clone()
    }

    // ------------------------------------------------------- block lifecycle

    fn new_id(&mut self) -> BlockId {
        let id = BlockId(self.next_id);
        self.next_id += 1;
        id
    }

    fn make_state(
        &self,
        ty: &str,
        attrs: &BlockAttrs,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<EditorState> {
        let spec = BlockRegistry::global(cx).get(ty);
        let caps = spec.caps();
        let placeholder = placeholder_of(&spec, attrs);
        let language = attrs
            .language
            .clone()
            .unwrap_or(SharedString::new_static("plaintext"));
        let text = text.to_string();

        cx.new(|cx| {
            super::fit::document_text_state(window, cx)
                .auto_close(caps.multiline)
                .smart_indent(caps.multiline)
                .language(language)
                .placeholder(placeholder)
                .default_value(text)
        })
    }

    fn subscribe_to_block(
        &self,
        id: BlockId,
        state: &Entity<EditorState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui_kit::Subscription> {
        let mut selection = (state.read(cx).cursor(), state.read(cx).selected_range());
        let observation = cx.observe_in(state, window, move |this, state, window, cx| {
            let input = state.read(cx);
            let next = (input.cursor(), input.selected_range());
            let unchanged = next == selection;
            if unchanged {
                return;
            }
            selection = next;
            let focused = state.focus_handle(cx).is_focused(window);
            if !focused {
                return;
            }
            let Some(block) = this.block(id) else {
                return;
            };
            let text_settled = input.value().as_str() == block.text.as_str();
            if !text_settled {
                return;
            }
            let composing = state.update(cx, |state, cx| {
                use gpui_kit::EntityInputHandler as _;
                state.marked_text_range(window, cx).is_some()
            });
            if composing {
                return;
            }
            this.focused = Some(id);
            cx.emit(SelectionChanged);
        });
        vec![
            observation,
            cx.subscribe_in(
                state,
                window,
                move |this, _state, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => this.on_block_changed(id, window, cx),
                    InputEvent::Focus => {
                        this.focused = Some(id);
                        this.selected.clear();
                        cx.emit(SelectionChanged);
                        cx.notify();
                    }
                    _ => {}
                },
            ),
        ]
    }

    /// Insert a block at `ix` and return its id.
    pub fn insert_block(
        &mut self,
        ix: usize,
        content: BlockContent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> BlockId {
        let id = self.new_id();
        let state = self.make_state(&content.ty, &content.attrs, &content.text, window, cx);
        let subscriptions = self.subscribe_to_block(id, &state, window, cx);

        let mut block = Block {
            id,
            ty: content.ty,
            attrs: content.attrs,
            text: content.text,
            marks: content.marks,
            stored_marks: None,
            state,
            decorations: None,
            indent: content.indent,
            rows: 1,
            fit: super::fit::InputFit::new(cx.editor_theme()),
            subscriptions,
        };
        block.marks.clamp(block.text.len());

        let ix = ix.min(self.blocks.len());
        self.blocks.insert(ix, block);
        if BlockRegistry::global(cx).get(&self.blocks[ix].ty).caps().grid {
            self.restore_grid(id, window, cx);
        }
        self.remeasure(id, cx);
        self.apply_decorations(id, cx);
        cx.emit(DocumentChanged);
        cx.notify();
        id
    }

    pub fn remove_block(&mut self, id: BlockId, cx: &mut Context<Self>) {
        let Some(ix) = self.index_of(id) else { return };
        self.blocks.remove(ix);
        if self.focused == Some(id) {
            self.focused = None;
        }
        // A grid belongs to its block: leaving it behind keeps its cells
        // alive, and a focused cell of a deleted table still answers Tab.
        self.grids.remove(&id);
        if self.focused_cell.is_some_and(|(block, _)| block == id) {
            self.focused_cell = None;
        }
        self.selected.retain(|s| *s != id);
        cx.emit(DocumentChanged);
        cx.notify();
    }

    /// Replace a block's node type, keeping its text and marks.
    ///
    /// The new type may have a different size and line height, so what the
    /// old one learned about its input's inset is worth nothing.
    pub fn set_block_type(
        &mut self,
        id: BlockId,
        ty: BlockType,
        attrs: BlockAttrs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.index_of(id) else { return };
        if self.blocks[ix].ty == ty && self.blocks[ix].attrs == attrs {
            return;
        }

        let spec = BlockRegistry::global(cx).get(&ty);
        let caps = spec.caps();
        let was_multiline = self.spec_at(ix, cx).caps().multiline;

        self.blocks[ix].ty = ty.clone();
        self.blocks[ix].attrs = attrs;
        // Text size and line height come from the type, so the inset this
        // block learned belongs to the type it no longer is.
        self.blocks[ix].fit = super::fit::InputFit::new(cx.editor_theme());
        // A type that holds no grid holds no cells either.
        if !caps.grid {
            self.grids.remove(&id);
            if self.focused_cell.is_some_and(|(block, _)| block == id) {
                self.focused_cell = None;
            }
        }
        if !caps.marks {
            self.blocks[ix].marks.clear();
        }
        if !caps.list {
            self.blocks[ix].indent = 0;
        }

        // A code block edits differently from prose, so it needs a fresh state.
        if was_multiline != caps.multiline {
            let text = self.blocks[ix].text.clone();
            let attrs = self.blocks[ix].attrs.clone();
            let state = self.make_state(&ty, &attrs, &text, window, cx);
            let subscriptions = self.subscribe_to_block(id, &state, window, cx);
            self.blocks[ix].state = state;
            self.blocks[ix].subscriptions = subscriptions;
            self.blocks[ix].decorations = None;
        } else {
            let placeholder = spec.placeholder(&self.blocks[ix].attrs);
            self.blocks[ix]
                .state
                .update(cx, |state, cx| state.set_placeholder(placeholder, window, cx));
        }

        self.remeasure(id, cx);
        self.apply_decorations(id, cx);
        cx.emit(DocumentChanged);
        cx.notify();
    }

    // ------------------------------------------------------------- edit sync

    /// Keep the block's mirror, marks and decorations in step with its input.
    fn on_block_changed(&mut self, id: BlockId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.index_of(id) else { return };
        let new_text = self.blocks[ix].state.read(cx).value().to_string();
        if new_text == self.blocks[ix].text {
            return;
        }
        self.record(super::history::Step::Typing, cx);
        let old_text = std::mem::replace(&mut self.blocks[ix].text, new_text.clone());

        let edit = diff_edit(&old_text, &new_text);
        // A lone newline is Shift-Enter's soft break; anything longer that
        // carries newlines arrived as a paste.
        let pasted = edit.as_ref().is_some_and(|edit| {
            edit.new_len > 1
                && new_text[edit.range.start..edit.range.start + edit.new_len].contains('\n')
        });

        if let Some(edit) = edit {
            // What the new text is formatted as: the marks armed at the caret
            // if any, otherwise the marks that were live where it was typed.
            let at = edit.range.start..edit.range.start;
            let inherited = self.blocks[ix]
                .marks
                .active(&at)
                .into_iter()
                .filter(MarkKind::is_inclusive)
                .collect();
            let applied: Vec<MarkKind> = self.blocks[ix]
                .stored_marks
                .take()
                .unwrap_or(inherited);

            self.blocks[ix].marks.remap(&edit);
            if edit.new_len > 0 {
                let range = edit.range.start..edit.range.start + edit.new_len;
                for kind in applied {
                    self.blocks[ix].marks.add(kind, range.clone());
                }
            }
            self.blocks[ix].marks.clamp(new_text.len());
        }

        self.remeasure(id, cx);
        self.apply_decorations(id, cx);
        if pasted && self.split_pasted_lines(id, window, cx) {
            return;
        }
        self.run_input_rules(id, window, cx);
        self.sync_suggestion_menu(window, cx);
        cx.emit(DocumentChanged);
        cx.notify();
    }

    /// Text arriving with newlines — a paste — becomes one block per line,
    /// each line still subject to the markdown rules.
    fn split_pasted_lines(
        &mut self,
        id: BlockId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(ix) = self.index_of(id) else {
            return false;
        };
        if self.spec_at(ix, cx).caps().multiline || !self.blocks[ix].text.contains('\n') {
            return false;
        }

        let text = self.blocks[ix].text.clone();
        let mut lines = text.split('\n');
        let head = lines.next().unwrap_or_default().to_string();
        let rest: Vec<String> = lines.map(|line| line.to_string()).collect();

        let caret = head.len();
        self.set_block_text(ix, head, Some(caret), window, cx);
        self.apply_markdown_prefix(id, window, cx);

        let mut at = ix;
        let mut last = id;
        for line in rest {
            at += 1;
            let content = BlockContent {
                ty: self.blocks[ix].ty.clone(),
                attrs: self.blocks[ix].attrs.clone(),
                text: line,
                marks: Default::default(),
                indent: self.blocks[ix].indent,
            };
            last = self.insert_block(at, content, window, cx);
            self.apply_markdown_prefix(last, window, cx);
        }
        self.focus_block(last, Caret::End, window, cx);
        cx.emit(DocumentChanged);
        cx.notify();
        true
    }

    /// Push the block's marks into the input's decoration layer.
    pub(crate) fn apply_decorations(&mut self, id: BlockId, cx: &mut Context<Self>) {
        let Some(ix) = self.index_of(id) else { return };
        let layout = self.layout_at(ix, cx);
        let mut runs = self.blocks[ix].marks.runs();
        if layout.strikethrough {
            runs = vec![(0..self.blocks[ix].text.len(), vec![MarkKind::Strike])];
        }

        let decorations: Vec<_> = runs
            .into_iter()
            .map(|(range, kinds)| {
                gpui_kit::component::input::TextDecoration::new(range, highlight_style(&kinds, cx))
            })
            .collect();

        let collection = match self.blocks[ix].decorations.clone() {
            Some(collection) => collection,
            None => {
                let state = self.blocks[ix].state.clone();
                let collection = state.update(cx, |state, cx| {
                    state.create_decorations_collection(Vec::new(), cx)
                });
                self.blocks[ix].decorations = Some(collection.clone());
                collection
            }
        };
        collection.set(decorations, cx);
    }

    // ----------------------------------------------------------------- focus

    pub fn focus_block(
        &mut self,
        id: BlockId,
        caret: Caret,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.index_of(id) else { return };
        if !self.spec_at(ix, cx).caps().textual {
            self.focused = None;
            self.selected = vec![id];
            self.focus_handle.focus(window, cx);
            cx.notify();
            return;
        }
        let len = self.blocks[ix].text.len();
        let offset = match caret {
            Caret::Start => 0,
            Caret::End => len,
            Caret::At(offset) => offset.min(len),
        };
        self.focused = Some(id);
        self.selected.clear();
        self.blocks[ix].state.update(cx, |state, cx| {
            state.focus(window, cx);
            state.set_selected_range(offset..offset, cx);
        });
        cx.notify();
    }

    /// Move focus to the next or previous textual block.
    pub(crate) fn focus_sibling(
        &mut self,
        from: usize,
        delta: isize,
        caret: Caret,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut ix = from as isize + delta;
        while ix >= 0 && (ix as usize) < self.blocks.len() {
            let ix_usize = ix as usize;
            if self.spec_at(ix_usize, cx).caps().textual && self.is_visible(ix_usize) {
                let id = self.blocks[ix_usize].id;
                self.focus_block(id, caret, window, cx);
                return true;
            }
            ix += delta;
        }
        false
    }

    /// Whether a block is shown, i.e. no collapsed toggle encloses it.
    pub(crate) fn is_visible(&self, ix: usize) -> bool {
        let indent = self.blocks[ix].indent;
        !self.blocks[..ix]
            .iter()
            .rev()
            .any(|block| block.indent < indent && block.attrs.collapsed)
    }

    pub fn toggle_collapsed(&mut self, id: BlockId, cx: &mut Context<Self>) {
        let Some(block) = self.block_mut(id) else {
            return;
        };
        block.attrs.collapsed = !block.attrs.collapsed;
        cx.notify();
    }

    // ------------------------------------------------------------ measurement

    pub(crate) fn layout_at(&self, ix: usize, cx: &App) -> BlockLayout {
        let block = &self.blocks[ix];
        BlockRegistry::global(cx)
            .get(&block.ty)
            .layout(&block.attrs, cx.editor_theme())
    }

    fn block_font(&self, layout: &BlockLayout, cx: &App) -> Font {
        let family = if layout.mono {
            cx.theme().mono_font_family.clone()
        } else {
            cx.theme().font_family.clone()
        };
        let mut font = gpui_kit::font(family);
        font.weight = layout.font_weight;
        font
    }

    /// Rows the text wraps into at the current column width.
    /// Re-measure a block's wrapped height. Called when its text, type or
    /// nesting changes — the only things that can change it.
    pub(crate) fn remeasure(&mut self, id: BlockId, cx: &App) {
        let Some(ix) = self.index_of(id) else { return };
        let layout = self.layout_at(ix, cx);
        let rows = self.measure_rows(ix, &layout, cx);
        self.blocks[ix].rows = rows;
    }

    fn measure_rows(&self, ix: usize, layout: &BlockLayout, cx: &App) -> usize {
        let block = &self.blocks[ix];
        // The width text wraps at is narrower than the text area: the input
        // keeps a gutter on the left — the distance measured into the fit's
        // lead — and the same margin again on the right. The arithmetic in
        // the fallback is only the guess for a block that has not laid out.
        let lead = block.fit.lead().x;
        let wrap_width = block
            .state
            .read(cx)
            .text_bounds()
            .map(|bounds| bounds.size.width - lead * 2.)
            .filter(|width| *width > px(1.))
            .unwrap_or_else(|| {
                self.wrap_width
                    - cx.editor_theme().indent_width * block.indent as f32
                    - layout.leading_width
                    - layout.inner_padding * 2.
            });
        if wrap_width <= px(1.) {
            return 1;
        }

        let font = self.block_font(layout, cx);
        let mut wrapper = cx.text_system().line_wrapper(font, layout.text_size);
        block
            .text
            .split('\n')
            .map(|line| {
                if line.is_empty() {
                    return 1;
                }
                1 + wrapper
                    .wrap_line(&[LineFragment::text(line)], wrap_width)
                    .count()
            })
            .sum::<usize>()
            .max(1)
    }

    /// Height the text inside a block needs, before the input's own inset.
    fn block_text_height(&self, ix: usize, layout: &BlockLayout, cx: &App) -> Pixels {
        let state = self.blocks[ix].state.read(cx);
        let line_height = state.line_height().unwrap_or(layout.line_height_px());
        let estimate = line_height * self.blocks[ix].rows.max(1) as f32;

        let measured = state
            .range_to_bounds(&(0..self.blocks[ix].text.len()))
            .map(|bounds| bounds.size.height)
            .unwrap_or(estimate);
        estimate.max(measured)
    }

    /// Learn how much of the height an input keeps for itself.
    ///
    /// A block is sized to its own text, so its input must never have to
    /// scroll: a text area even a pixel short of its content makes the text
    /// jump by that pixel whenever the caret moves to another row — the
    /// document rattles as it is clicked around. The gap between the height
    /// handed to the input and the height its text area ends up with is
    /// measured rather than assumed, and only ever widened, because the text
    /// area is snapped to whole device pixels and chasing that snapping in
    /// both directions never settles.
    pub(crate) fn sync_input_insets(&mut self, window: &Window, cx: &mut Context<Self>) {
        let theme = cx.editor_theme().clone();
        // What one physical pixel is worth here: the resolution at which the
        // measurements below are worth acting on at all.
        let device_pixel = px(1.) / window.scale_factor();
        for ix in 0..self.blocks.len() {
            let layout = self.layout_at(ix, cx);
            let needed = self.block_text_height(ix, &layout, cx);
            let area = self.blocks[ix]
                .state
                .read(cx)
                .text_bounds()
                .map(|bounds| bounds.size.height);
            if let Some(area) = area {
                self.blocks[ix].fit.observe(needed, area, &theme);
            }
            let glyph = self.blocks[ix]
                .state
                .read(cx)
                .range_to_bounds(&(0..0))
                .map(|bounds| bounds.origin);
            let slot = self.text_slots.get(&self.blocks[ix].id).copied();
            if let (Some(slot), Some(glyph)) = (slot, glyph) {
                self.blocks[ix]
                    .fit
                    .observe_lead(slot, glyph, &theme, device_pixel);
            }

            let state = self.blocks[ix].state.clone();
            super::fit::reset_scroll_when_text_fits(&state, needed, cx);
        }
        self.sync_cell_insets(cx);
    }

    /// Height the block's input needs for its text.
    ///
    /// The input lays out inside the height it is given, so the height has to
    /// be known before layout: it is estimated by wrapping the text the way
    /// the text system will, reconciled with what the last layout produced,
    /// and widened by the inset the input keeps for itself.
    pub(crate) fn block_height(&self, ix: usize, layout: &BlockLayout, cx: &App) -> Pixels {
        let state = self.blocks[ix].state.read(cx);
        let _ = state;
        self.blocks[ix]
            .fit
            .height(self.block_text_height(ix, layout, cx))
    }

    // ------------------------------------------------------------- rendering

    fn block_context(&self, ix: usize, cx: &Context<Self>) -> BlockContext<'_> {
        let block = &self.blocks[ix];
        BlockContext {
            id: block.id,
            ix,
            grid: self.grids.get(&block.id),
            reveal_controls: self.always_show_gutter,
            line_height: self.line_height_at(ix, cx),
            leading_width: self.layout_at(ix, cx).leading_width,
            theme: super::theme::EditorTheme::shared(cx),
            attrs: &block.attrs,
            text: &block.text,
            indent: block.indent,
            focused: self.focused == Some(block.id),
            selected: self.selected.contains(&block.id),
            ordinal: self.list_ordinal(ix),
            editor: cx.entity().downgrade(),
        }
    }

    fn render_text(&self, ix: usize, layout: &BlockLayout, cx: &mut Context<Self>) -> AnyElement {
        let block = &self.blocks[ix];
        let height = self.block_height(ix, layout, cx);
        let family = if layout.mono {
            cx.theme().mono_font_family.clone()
        } else {
            cx.theme().font_family.clone()
        };
        let mut color = cx.theme().foreground;
        color.a *= layout.text_opacity;

        // An input insets its own text, so the block pulls the box back by
        // however much that turned out to be, and its first glyph lands on
        // the corner the layout gave it — beside the marker, inside the quote
        // bar, level with the block above.
        let lead = block.fit.lead();
        h_flex()
            .w_full()
            .items_start()
            .child(self.slot_probe(block.id, cx))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .ml(-lead.x)
                    .mt(-lead.y)
                    .mb(-(block.fit.inset() - lead.y))
                    .child(
                        Editor::new(&block.state)
                            .appearance(false)
                            .bordered(false)
                            .h(height)
                            .font_family(family)
                            .text_size(layout.text_size)
                            .font_weight(layout.font_weight)
                            .line_height(relative(layout.line_height))
                            .text_color(color),
                    ),
            )
            .into_any_element()
    }

    /// A zero-sized element at the corner the layout gave the block, so the
    /// distance from there to the first glyph can be measured rather than
    /// assumed.
    fn slot_probe(&self, id: BlockId, cx: &mut Context<Self>) -> AnyElement {
        let editor = cx.entity().downgrade();
        canvas(
            move |bounds, _window, cx| {
                let _ = editor.update(cx, |this, _| {
                    this.text_slots.insert(id, bounds.origin);
                });
            },
            |_, _, _, _| {},
        )
        .w(px(0.))
        .h(px(0.))
        .flex_none()
        .into_any_element()
    }

    fn render_block(&self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let spec = self.spec_at(ix, cx);
        let layout = spec.layout(&self.blocks[ix].attrs, cx.editor_theme());
        let id = self.blocks[ix].id;
        let indent = self.blocks[ix].indent;

        let body = {
            let ctx = self.block_context(ix, cx);
            spec.render_body(&ctx, window, cx)
        };
        let content = match body {
            Some(body) => body,
            None => self.render_text(ix, &layout, cx),
        };

        let leading = {
            let ctx = self.block_context(ix, cx);
            spec.render_leading(&ctx, window, cx)
        };

        let inner = h_flex()
            .w_full()
            .items_start()
            .when_some(leading, |this, leading| this.child(leading))
            .child(div().flex_1().min_w(px(0.)).child(content))
            .into_any_element();

        let wrapped = {
            let ctx = self.block_context(ix, cx);
            spec.wrap(&ctx, inner, window, cx)
        };

        // The text wrapper cancels the input's inset on both sides, so a
        // block occupies exactly its text and these margins are the ones the
        // layout asks for, with nothing to trim back.
        let margin_top = if ix == 0 {
            px(0.)
        } else if layout.collapse_with_siblings && self.blocks[ix - 1].ty == self.blocks[ix].ty {
            cx.editor_theme().rems(0.125)
        } else {
            layout.margin_top
        };

        let selected = self.selected.contains(&id);
        let probe = self.bounds_probe(id, cx);
        let gutter = self.render_gutter(ix, window, cx);
        let indicator = self.drop_indicator(ix, cx);

        div()
            .id(("block", id.0 as usize))
            .test_support()
            .child(probe)
            .group(group_name(id))
            .relative()
            .w_full()
            .mt(margin_top)
            .mb(layout.margin_bottom)
            // The row reaches into the left margin so hovering it covers the
            // gutter controls, which live in that reach.
            .ml(-cx.editor_theme().gutter_controls_width)
            .pl(cx.editor_theme().gutter_controls_width
                + cx.editor_theme().indent_width * indent as f32)
            .child(gutter)
            .child(
                // The tint belongs to the block, not to the gutter the row
                // reaches into, so it goes on the content rather than the row.
                div()
                    .w_full()
                    .when(selected, |this| {
                        this.rounded(cx.editor_theme().radius_sm)
                            .bg(cx.theme().selection.opacity(0.4))
                    })
                    .child(wrapped),
            )
            .children(indicator)
            .on_drag_move(cx.listener(move |this, event: &DragMoveEvent<DraggedBlock>, _, cx| {
                this.on_drag_over(ix, event, cx)
            }))
            .on_drop(cx.listener(move |this, dragged: &DraggedBlock, _, cx| {
                this.on_drop_block(dragged, cx)
            }))
            .into_any_element()
    }

    /// Whether a block is being rendered at all — a block inside a collapsed
    /// toggle is not, and nothing may be anchored to it.
    pub(crate) fn block_is_on_screen(&self, id: BlockId) -> bool {
        self.index_of(id).is_some_and(|ix| self.is_visible(ix))
    }

    /// Where a block sits on screen as of the last frame.
    ///
    /// The origin is the block's *content* corner — past the gutter the row
    /// reaches into and past any indent — while the size is the row's, which
    /// includes that reach. Consumers here use the origin and the height.
    pub fn block_bounds(&self, id: BlockId) -> Option<Bounds<Pixels>> {
        self.block_bounds.get(&id).copied()
    }

    /// An invisible element that reports how wide the content column came out,
    /// which is the width text wraps at before a block has laid out once.
    fn column_probe(&self, cx: &mut Context<Self>) -> AnyElement {
        let editor = cx.entity().downgrade();
        canvas(
            move |bounds, _window, cx| {
                let _ = editor.update(cx, |this, cx| {
                    if (this.wrap_width - bounds.size.width).abs() > px(0.5) {
                        this.wrap_width = bounds.size.width;
                        // Text wraps differently at a new width, so every
                        // block's row count is worth nothing now, and the
                        // frame that is being laid out was built from it.
                        this.remeasure_all(cx);
                        cx.notify();
                    }
                });
            },
            |_, _, _, _| {},
        )
        .w_full()
        .h(px(0.))
        .into_any_element()
    }

    /// An invisible element that reports where the block landed, which is how
    /// a pointer position becomes a block.
    fn bounds_probe(&self, id: BlockId, cx: &mut Context<Self>) -> AnyElement {
        let editor = cx.entity().downgrade();
        canvas(
            move |bounds, _window, cx| {
                let _ = editor.update(cx, |this, cx| {
                    let changed = this.block_bounds.insert(id, bounds) != Some(bounds);
                    if changed {
                        cx.notify();
                    }
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full()
        .into_any_element()
    }

    /// Ordinal of an ordered-list item within its run at the same indent.
    fn list_ordinal(&self, ix: usize) -> usize {
        let block = &self.blocks[ix];
        let mut n = block.attrs.start.unwrap_or(1);
        for prev in self.blocks[..ix].iter().rev() {
            if prev.indent < block.indent {
                break;
            }
            if prev.indent > block.indent {
                continue;
            }
            if prev.ty == block.ty {
                n += 1;
                if let Some(start) = prev.attrs.start {
                    return n + start - 1;
                }
            } else {
                break;
            }
        }
        n
    }
}

/// Where a caret lands when focus moves to a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Caret {
    Start,
    End,
    At(usize),
}

pub(crate) fn group_name(id: BlockId) -> SharedString {
    SharedString::from(format!("block-{}", id.0))
}

/// The hint an empty block shows. A block may name its own: an application
/// that reuses a built-in type for something of its own — a page title drawn
/// on a heading — has no other way to say so, because the spec belongs to
/// every block of that type and sees nothing but the attributes.
fn placeholder_of(spec: &Arc<dyn BlockSpec>, attrs: &BlockAttrs) -> SharedString {
    attrs
        .extra("placeholder")
        .cloned()
        .unwrap_or_else(|| spec.placeholder(attrs))
}

/// Turn a run's marks into the style the decoration layer paints.
pub(crate) fn highlight_style(kinds: &[MarkKind], cx: &App) -> HighlightStyle {
    let mut style = HighlightStyle::default();
    for kind in kinds {
        match kind {
            MarkKind::Bold => style.font_weight = Some(FontWeight::BOLD),
            MarkKind::Italic => style.font_style = Some(FontStyle::Italic),
            MarkKind::Underline => {
                style.underline = Some(UnderlineStyle {
                    thickness: px(1.),
                    color: None,
                    wavy: false,
                })
            }
            MarkKind::Strike => {
                style.strikethrough = Some(StrikethroughStyle {
                    thickness: px(1.),
                    color: None,
                })
            }
            MarkKind::Code => {
                style.background_color = Some(cx.editor_theme().code_background);
                style.color = Some(cx.editor_theme().code_foreground);
            }
            MarkKind::Link(_) => {
                style.color = Some(cx.theme().link);
                style.underline = Some(UnderlineStyle {
                    thickness: px(1.),
                    color: None,
                    wavy: false,
                });
            }
            MarkKind::Highlight(color) => {
                style.background_color = Some(cx.editor_theme().highlight_fill(*color))
            }
            MarkKind::TextColor(color) => style.color = cx.editor_theme().text_color(*color),
            MarkKind::Mention(_) => {
                style.color = Some(cx.theme().primary);
                style.background_color = Some(cx.theme().accent);
            }
            MarkKind::Comment(_) => {
                style.background_color = Some(cx.editor_theme().comment_fill);
                style.underline = Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(cx.editor_theme().comment_accent),
                    wavy: false,
                });
            }
            MarkKind::Superscript | MarkKind::Subscript => {}
        }
    }
    style
}

impl EventEmitter<DocumentChanged> for NotionEditor {}
impl EventEmitter<SelectionChanged> for NotionEditor {}
impl EventEmitter<LinkPressed> for NotionEditor {}
impl EventEmitter<super::slash::MenuAction> for NotionEditor {}
impl EventEmitter<super::toolbar::ToolbarAction> for NotionEditor {}
impl EventEmitter<super::comments::AnnotationRequested> for NotionEditor {}

impl Focusable for NotionEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for NotionEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_input_insets(window, cx);
        let in_a_block = self.refresh_focus(window, cx);
        self.refresh_focused_cell(in_a_block, window, cx);
        let visible: Vec<usize> = (0..self.blocks.len())
            .filter(|ix| self.is_visible(*ix))
            .collect();
        // Keep the last measured positions until layout reports this frame's
        // bounds. Hidden and removed blocks cannot anchor an overlay.
        let visible_ids: HashSet<_> = visible.iter().map(|ix| self.blocks[*ix].id).collect();
        self.block_bounds.retain(|id, _| visible_ids.contains(id));
        let blocks: Vec<AnyElement> = visible
            .into_iter()
            .map(|ix| self.render_block(ix, window, cx))
            .collect();

        let root = div()
            .id("editor")
            .test_support()
            .key_context(actions::CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground);

        self.with_key_handlers(root, cx)
            // A click anywhere in the page dismisses an open suggestion menu,
            // the way clicking away from a popover closes it.
            .on_mouse_down(
                gpui_kit::MouseButton::Left,
                cx.listener(|this, event: &gpui_kit::MouseDownEvent, window, cx| {
                    this.close_suggestion_menu(cx);
                    this.open_thread_for_press(event, window, cx);
                    this.open_link_for_press(event, window, cx);
                }),
            )
            .capture_any_mouse_down(cx.listener(Self::on_page_mouse_down))
            .on_mouse_move(cx.listener(Self::on_page_mouse_move))
            .capture_any_mouse_up(cx.listener(Self::on_page_mouse_up))
            // A drag let go anywhere — over the margin, the trailing space,
            // another window — ends it. Bubble phase, so a row that was
            // dropped on has already taken the drop.
            .on_mouse_up(
                gpui_kit::MouseButton::Left,
                cx.listener(|this, _, _window, cx| this.clear_drop_target(cx)),
            )
            .child(
                v_flex()
                    .id("page")
                    .size_full()
                    .overflow_y_scroll()
                    .items_center()
                    .child(
                        v_flex()
                            // `minmax(auto, 708px)`: the column gives way on a
                            // narrow window instead of running off it.
                            .w_full()
                            .max_w(cx.editor_theme().page_width)
                            .px(cx.editor_theme().page_padding)
                            .pt(cx.editor_theme().page_padding)
                            .child(self.column_probe(cx))
                            .children(blocks)
                            .child(
                                // Clicking the space under the document puts
                                // the caret in a trailing paragraph.
                                div()
                                    .id("trailing-space")
                                    .test_support()
                                    .w_full()
                                    .h(cx.editor_theme().page_bottom)
                                    .cursor_text()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.focus_trailing_block(window, cx)
                                    })),
                            ),
                    ),
            )
            .children(self.render_suggestion_menu(window, cx))
            .children(self.render_selection_toolbar(window, cx))
            .children(self.render_link_editor(window, cx))
            .children(self.render_comment_popover(window, cx))
    }
}

impl NotionEditor {
    /// Put the caret in the last block, appending a paragraph when the last
    /// block cannot hold one, the way clicking under a Notion page does.
    pub fn focus_trailing_block(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let last = self.blocks.len().saturating_sub(1);
        let textual = self
            .blocks
            .get(last)
            .map(|b| BlockRegistry::global(cx).get(&b.ty).caps().textual)
            .unwrap_or(false);
        let empty = self.blocks.get(last).map(|b| b.is_empty()).unwrap_or(true);

        if textual && empty {
            let id = self.blocks[last].id;
            self.focus_block(id, Caret::End, window, cx);
            return;
        }
        let id = self.insert_block(self.blocks.len(), BlockContent::paragraph(""), window, cx);
        self.focus_block(id, Caret::End, window, cx);
    }

    /// The editor's own focus handle, for node selection.
    pub(crate) fn focus_handle_for_editor(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// Ordinal helper used by list specs and tests.
    pub fn ordinal_at(&self, ix: usize) -> usize {
        self.list_ordinal(ix)
    }

    /// Type name of a block, for tests and hosts.
    pub fn block_type(&self, ix: usize) -> &str {
        self.blocks
            .get(ix)
            .map(|b| b.ty.as_ref())
            .unwrap_or(types::PARAGRAPH)
    }
}
