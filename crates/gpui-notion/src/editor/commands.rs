//! The command surface, named after Tiptap's.
//!
//! Tiptap spells these `editor.chain().focus().toggleBold().run()`; here they
//! are methods on the editor entity, taking the GPUI `Window`/`Context` pair
//! instead of a chain, and acting on the focused block.

use std::ops::Range;

use gpui_kit::{App, Context, SharedString, Window};

use super::block::{BlockAttrs, BlockContent, BlockId, BlockRegistry, BlockType, types};
use super::mark::{Edit, HighlightColor, MarkKind, TextColor};
use super::history::Step;
use super::view::{Caret, DocumentChanged, LinkPressed, NotionEditor};

impl NotionEditor {
    // ----------------------------------------------------------------- marks

    /// Tiptap `toggleMark`. With a selection it formats it; with a bare caret
    /// it arms the mark for the next typed characters.
    pub fn toggle_mark(&mut self, kind: MarkKind, _window: &mut Window, cx: &mut Context<Self>) {
        self.record(Step::Structural, cx);
        if self.has_block_selection() {
            self.toggle_mark_over_selected_blocks(kind, cx);
            return;
        }
        let Some((id, range)) = self.selection(cx) else {
            return;
        };
        let Some(ix) = self.index_of(id) else { return };
        if !self.spec_at(ix, cx).caps().marks {
            return;
        }

        if range.is_empty() {
            let active = self.active_marks(cx);
            let mut stored = self.blocks[ix]
                .stored_marks
                .clone()
                .unwrap_or_else(|| active.clone());
            match stored.iter().position(|k| k.id() == kind.id()) {
                Some(pos) => {
                    stored.remove(pos);
                }
                None => stored.push(kind),
            }
            self.blocks[ix].stored_marks = Some(stored);
            cx.notify();
            return;
        }

        self.blocks[ix].marks.toggle(kind, range);
        self.apply_decorations(id, cx);
        cx.emit(DocumentChanged);
        cx.notify();
    }

    /// With whole blocks selected a mark covers all of their text, and comes
    /// off only when every one of them already carries it.
    fn toggle_mark_over_selected_blocks(&mut self, kind: MarkKind, cx: &mut Context<Self>) {
        let ids: Vec<BlockId> = self
            .selected_blocks()
            .into_iter()
            .filter(|id| {
                self.index_of(*id)
                    .is_some_and(|ix| self.spec_at(ix, cx).caps().marks)
            })
            .collect();
        if ids.is_empty() {
            return;
        }
        let everywhere = ids.iter().all(|id| {
            self.block(*id)
                .is_some_and(|block| block.marks.has(&kind, &(0..block.text.len())))
        });

        for id in ids {
            let Some(ix) = self.index_of(id) else { continue };
            let range = 0..self.blocks[ix].text.len();
            if range.is_empty() {
                continue;
            }
            if everywhere {
                self.blocks[ix].marks.remove(&kind, &range);
            } else {
                self.blocks[ix].marks.add(kind.clone(), range);
            }
            self.apply_decorations(id, cx);
        }
        cx.emit(DocumentChanged);
        cx.notify();
    }

    pub fn set_mark(&mut self, kind: MarkKind, _window: &mut Window, cx: &mut Context<Self>) {
        self.record(Step::Structural, cx);
        let Some((id, range)) = self.selection(cx) else {
            return;
        };
        let Some(ix) = self.index_of(id) else { return };
        if range.is_empty() || !self.spec_at(ix, cx).caps().marks {
            return;
        }
        self.blocks[ix].marks.add(kind, range);
        self.apply_decorations(id, cx);
        cx.emit(DocumentChanged);
        cx.notify();
    }

    pub fn unset_mark(&mut self, kind: &MarkKind, _window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, range)) = self.selection(cx) else {
            return;
        };
        let Some(ix) = self.index_of(id) else { return };
        let range = if range.is_empty() {
            let caret = range.start;
            match self.blocks[ix].marks.mark_at(kind, caret) {
                Some(mark) => mark.range.clone(),
                None => return,
            }
        } else {
            range
        };
        self.blocks[ix].marks.remove(kind, &range);
        self.apply_decorations(id, cx);
        cx.emit(DocumentChanged);
        cx.notify();
    }

    /// Marks that apply at the caret — what the toolbar shows as active.
    pub fn active_marks(&self, cx: &App) -> Vec<MarkKind> {
        let Some(id) = self.active_id() else {
            return Vec::new();
        };
        let Some(block) = self.block(id) else {
            return Vec::new();
        };
        if let Some(stored) = &block.stored_marks {
            return stored.clone();
        }
        block.marks.active(&block.state.read(cx).selected_range())
    }

    pub fn is_mark_active(&self, kind: &MarkKind, cx: &App) -> bool {
        let Some(id) = self.active_id() else {
            return false;
        };
        let Some(block) = self.block(id) else {
            return false;
        };
        if let Some(stored) = &block.stored_marks {
            return stored.iter().any(|k| k.id() == kind.id());
        }
        block.marks.has(kind, &block.state.read(cx).selected_range())
    }

    pub fn toggle_bold(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_mark(MarkKind::Bold, window, cx);
    }

    pub fn toggle_italic(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_mark(MarkKind::Italic, window, cx);
    }

    pub fn toggle_underline(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_mark(MarkKind::Underline, window, cx);
    }

    pub fn toggle_strike(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_mark(MarkKind::Strike, window, cx);
    }

    pub fn toggle_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_mark(MarkKind::Code, window, cx);
    }

    pub fn toggle_superscript(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.unset_mark(&MarkKind::Subscript, window, cx);
        self.toggle_mark(MarkKind::Superscript, window, cx);
    }

    pub fn toggle_subscript(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.unset_mark(&MarkKind::Superscript, window, cx);
        self.toggle_mark(MarkKind::Subscript, window, cx);
    }

    pub fn toggle_highlight(
        &mut self,
        color: Option<HighlightColor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_mark(MarkKind::Highlight(color), window, cx);
    }

    pub fn set_color(&mut self, color: TextColor, window: &mut Window, cx: &mut Context<Self>) {
        match color {
            TextColor::Default => self.unset_mark(&MarkKind::TextColor(color), window, cx),
            _ => self.set_mark(MarkKind::TextColor(color), window, cx),
        }
    }

    pub fn set_link(
        &mut self,
        href: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_mark(MarkKind::Link(href.into()), window, cx);
    }

    pub fn unset_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.unset_mark(&MarkKind::Link(SharedString::default()), window, cx);
    }

    /// Href at the caret, for the link popover.
    pub fn link_at_caret(&self, cx: &App) -> Option<(Range<usize>, SharedString)> {
        let (id, range) = self.selection(cx)?;
        let block = self.block(id)?;
        let mark = block
            .marks
            .mark_at(&MarkKind::Link(SharedString::default()), range.start)?;
        match &mark.kind {
            MarkKind::Link(href) => Some((mark.range.clone(), href.clone())),
            _ => None,
        }
    }

    /// A press inside a link opens it, the way a reader expects a link to
    /// behave; the caret still lands where it was pressed, so the run stays
    /// editable. Deferred because the input moves its own caret while
    /// handling this press — where it ended up is only true a frame later.
    pub(crate) fn open_link_for_press(
        &mut self,
        event: &gpui_kit::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(block) = self.block_at_point(event.position) else {
            return;
        };
        let editor = cx.entity();
        window.defer(cx, move |_window, cx| {
            editor.update(cx, |this, cx| {
                let Some(href) = this.link_at_offset(block, cx) else {
                    return;
                };
                cx.emit(LinkPressed(href));
            });
        });
    }

    /// The link covering a block's caret, which is where a press just put it.
    fn link_at_offset(&self, block: BlockId, cx: &App) -> Option<SharedString> {
        let ix = self.index_of(block)?;
        let caret = self.blocks[ix].state.read(cx).cursor();
        self.blocks[ix]
            .marks
            .active(&(caret..caret))
            .into_iter()
            .find_map(|kind| match kind {
                MarkKind::Link(href) => Some(href),
                _ => None,
            })
    }

    /// Tiptap `unsetAllMarks` — the template's "Reset formatting".
    pub fn unset_all_marks(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.record(Step::Structural, cx);
        let Some((id, range)) = self.selection(cx) else {
            return;
        };
        let Some(ix) = self.index_of(id) else { return };
        let range = if range.is_empty() {
            0..self.blocks[ix].text.len()
        } else {
            range
        };
        for kind in [
            MarkKind::Bold,
            MarkKind::Italic,
            MarkKind::Underline,
            MarkKind::Strike,
            MarkKind::Code,
            MarkKind::Link(SharedString::default()),
            MarkKind::Highlight(None),
            MarkKind::TextColor(TextColor::Default),
            MarkKind::Superscript,
            MarkKind::Subscript,
        ] {
            self.blocks[ix].marks.remove(&kind, &range);
        }
        self.blocks[ix].stored_marks = None;
        self.apply_decorations(id, cx);
        cx.emit(DocumentChanged);
        cx.notify();
    }

    // ------------------------------------------------------------ node types

    /// Tiptap `setNode`: change the active block's type.
    pub fn turn_into(
        &mut self,
        ty: impl Into<BlockType>,
        attrs: BlockAttrs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.record(Step::Structural, cx);
        let ty = ty.into();
        if self.has_block_selection() {
            for id in self.selected_blocks() {
                self.set_block_type(id, ty.clone(), attrs.clone(), window, cx);
            }
            cx.notify();
            return;
        }
        let Some(id) = self.active_id() else { return };
        self.set_block_type(id, ty, attrs, window, cx);
        self.focus_block(id, Caret::End, window, cx);
    }

    /// Set the type, or return to a paragraph when it is already set.
    pub fn toggle_node(
        &mut self,
        ty: impl Into<BlockType>,
        attrs: BlockAttrs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.record(Step::Structural, cx);
        let ty = ty.into();
        if self.has_block_selection() {
            let all_are = self.selected_blocks().iter().all(|id| {
                self.block(*id)
                    .is_some_and(|block| block.ty == ty && block.attrs.level == attrs.level)
            });
            let (ty, attrs) = if all_are {
                (types::PARAGRAPH.into(), BlockAttrs::default())
            } else {
                (ty, attrs)
            };
            for id in self.selected_blocks() {
                self.set_block_type(id, ty.clone(), attrs.clone(), window, cx);
            }
            cx.notify();
            return;
        }
        let Some(id) = self.active_id() else { return };
        let is_active = self
            .block(id)
            .map(|b| b.ty == ty && b.attrs.level == attrs.level)
            .unwrap_or(false);
        if is_active {
            self.set_block_type(id, types::PARAGRAPH.into(), BlockAttrs::default(), window, cx);
        } else {
            self.set_block_type(id, ty, attrs, window, cx);
        }
        self.focus_block(id, Caret::End, window, cx);
    }

    pub fn set_paragraph(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.turn_into(types::PARAGRAPH, BlockAttrs::default(), window, cx);
    }

    pub fn toggle_heading(&mut self, level: u8, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_node(types::HEADING, BlockAttrs::level(level), window, cx);
    }

    pub fn toggle_bullet_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_node(types::BULLET_LIST, BlockAttrs::default(), window, cx);
    }

    pub fn toggle_ordered_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_node(types::ORDERED_LIST, BlockAttrs::default(), window, cx);
    }

    pub fn toggle_task_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_node(types::TASK_LIST, BlockAttrs::default(), window, cx);
    }

    pub fn toggle_blockquote(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_node(types::BLOCKQUOTE, BlockAttrs::default(), window, cx);
    }

    pub fn toggle_code_block(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_node(types::CODE_BLOCK, BlockAttrs::default(), window, cx);
    }

    pub fn toggle_callout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_node(types::CALLOUT, BlockAttrs::default(), window, cx);
    }

    /// Put a table under the caret and start typing in its first cell.
    pub fn insert_table(
        &mut self,
        rows: usize,
        columns: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.record(Step::Structural, cx);
        let at = match self.active_index() {
            // An empty paragraph makes room for the table rather than
            // hanging around above it.
            Some(ix) if self.block_is_empty_paragraph(ix) => {
                let id = self.blocks[ix].id;
                self.remove_block(id, cx);
                ix
            }
            Some(ix) => ix + 1,
            None => self.blocks.len(),
        };

        let id = self.insert_block(
            at,
            BlockContent::new(types::TABLE, String::new()),
            window,
            cx,
        );
        self.create_grid(id, rows.max(1), columns.max(1), window, cx);
        self.focus_cell(id, super::grid::CellPosition::new(0, 0), window, cx);
        cx.emit(DocumentChanged);
        cx.notify();
    }

    pub fn toggle_check(&mut self, id: BlockId, cx: &mut Context<Self>) {
        self.record(Step::Structural, cx);
        let Some(block) = self.block_mut(id) else {
            return;
        };
        block.attrs.checked = !block.attrs.checked;
        self.apply_decorations(id, cx);
        cx.emit(DocumentChanged);
        cx.notify();
    }

    /// Tiptap `setHorizontalRule`: replaces an empty block, else inserts after.
    pub fn set_horizontal_rule(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.record(Step::Structural, cx);
        let Some(ix) = self.active_index() else { return };
        if self.blocks[ix].text.is_empty() {
            let id = self.blocks[ix].id;
            self.set_block_type(
                id,
                types::HORIZONTAL_RULE.into(),
                BlockAttrs::default(),
                window,
                cx,
            );
        } else {
            self.insert_block(
                ix + 1,
                BlockContent::new(types::HORIZONTAL_RULE, ""),
                window,
                cx,
            );
        }
        let next = self.ensure_paragraph_after(self.active_rule_index(ix), window, cx);
        self.focus_block(next, Caret::Start, window, cx);
    }

    fn active_rule_index(&self, ix: usize) -> usize {
        self.blocks
            .iter()
            .position(|b| b.ty == types::HORIZONTAL_RULE)
            .map(|found| found.max(ix))
            .unwrap_or(ix)
    }

    pub fn set_image(
        &mut self,
        src: impl Into<SharedString>,
        alt: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.active_index() else { return };
        let src = src.into();
        let attrs = BlockAttrs {
            src: (!src.is_empty()).then_some(src),
            alt: Some(alt.into()),
            ..Default::default()
        };
        if self.blocks[ix].text.is_empty() {
            let id = self.blocks[ix].id;
            self.set_block_type(id, types::IMAGE.into(), attrs, window, cx);
        } else {
            self.insert_block(
                ix + 1,
                BlockContent::new(types::IMAGE, "").with_attrs(attrs),
                window,
                cx,
            );
        }
        let next = self.ensure_paragraph_after(ix, window, cx);
        self.focus_block(next, Caret::Start, window, cx);
    }

    /// Guarantee a text block after `ix`, so a non-text node is never last.
    pub(crate) fn ensure_paragraph_after(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> BlockId {
        let after = ix + 1;
        let textual = self
            .blocks
            .get(after)
            .map(|b| BlockRegistry::global(cx).get(&b.ty).caps().textual)
            .unwrap_or(false);
        match (textual, self.blocks.get(after)) {
            (true, Some(block)) => block.id,
            _ => self.insert_block(after, BlockContent::paragraph(""), window, cx),
        }
    }

    // ------------------------------------------------------------ block edit

    /// Tiptap `splitBlock`: Enter. The text after the caret starts a new block
    /// of the type this one splits into.
    pub fn split_block(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.record(Step::Structural, cx);
        let Some(ix) = self.active_index() else { return };
        let range = self.blocks[ix].state.read(cx).selected_range();
        let (start, end) = (range.start, range.end);

        let spec = self.spec_at(ix, cx);
        let caps = spec.caps();
        let text = self.blocks[ix].text.clone();

        // Enter on an empty list item, quote or toggle lifts it instead.
        if text.is_empty() && caps.lifts_when_empty {
            if self.blocks[ix].indent > 0 {
                self.lift_list_item(window, cx);
            } else {
                let id = self.blocks[ix].id;
                self.set_block_type(id, types::PARAGRAPH.into(), BlockAttrs::default(), window, cx);
                self.focus_block(id, Caret::Start, window, cx);
            }
            return;
        }

        let head = text[..start].to_string();
        let tail = text[end.min(text.len())..].to_string();

        let mut tail_marks = self.blocks[ix].marks.clone();
        if start != end {
            tail_marks.remap(&Edit::new(start..end, 0));
        }
        let tail_marks = tail_marks.split_off(start);

        let at_end = end >= text.len();
        let (ty, mut attrs) = spec.split_into(&self.blocks[ix].attrs, at_end);
        attrs.checked = false;
        let content = BlockContent {
            ty,
            attrs,
            text: tail,
            marks: tail_marks,
            indent: self.blocks[ix].indent,
        };

        self.set_block_text(ix, head, Some(start), window, cx);
        let new_id = self.insert_block(ix + 1, content, window, cx);
        self.focus_block(new_id, Caret::Start, window, cx);
    }

    /// Tiptap `joinBackward`: Backspace at the start of a block.
    pub fn join_backward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.record(Step::Structural, cx);
        let Some(ix) = self.active_index() else { return };

        // A formatted block first returns to a plain paragraph.
        if self.blocks[ix].indent > 0 {
            self.lift_list_item(window, cx);
            return;
        }
        if self.blocks[ix].ty != types::PARAGRAPH {
            let id = self.blocks[ix].id;
            self.set_block_type(id, types::PARAGRAPH.into(), BlockAttrs::default(), window, cx);
            self.focus_block(id, Caret::Start, window, cx);
            return;
        }
        let Some(prev_ix) = ix.checked_sub(1) else {
            return;
        };

        // Backspace into a non-text node deletes that node.
        if !self.spec_at(prev_ix, cx).caps().textual {
            let prev_id = self.blocks[prev_ix].id;
            self.remove_block(prev_id, cx);
            return;
        }

        let offset = self.blocks[prev_ix].text.len();
        let tail_text = self.blocks[ix].text.clone();
        let tail_marks = self.blocks[ix].marks.clone();
        let id = self.blocks[ix].id;

        let merged = format!("{}{}", self.blocks[prev_ix].text, tail_text);
        self.blocks[prev_ix].marks.extend_from(&tail_marks, offset);
        self.set_block_text(prev_ix, merged, Some(offset), window, cx);
        self.remove_block(id, cx);

        let prev_id = self.blocks[prev_ix].id;
        self.focus_block(prev_id, Caret::At(offset), window, cx);
    }

    /// Delete at the end of a block pulls the next one up.
    pub fn join_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.record(Step::Structural, cx);
        let Some(ix) = self.active_index() else { return };
        if ix + 1 >= self.blocks.len() {
            return;
        }
        if !self.spec_at(ix + 1, cx).caps().textual {
            let next_id = self.blocks[ix + 1].id;
            self.remove_block(next_id, cx);
            return;
        }

        let offset = self.blocks[ix].text.len();
        let next_id = self.blocks[ix + 1].id;
        let next_text = self.blocks[ix + 1].text.clone();
        let next_marks = self.blocks[ix + 1].marks.clone();

        let merged = format!("{}{}", self.blocks[ix].text, next_text);
        self.blocks[ix].marks.extend_from(&next_marks, offset);
        self.set_block_text(ix, merged, Some(offset), window, cx);
        self.remove_block(next_id, cx);

        let id = self.blocks[ix].id;
        self.focus_block(id, Caret::At(offset), window, cx);
    }

    /// Tiptap `sinkListItem`: Tab.
    pub fn sink_list_item(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.record(Step::Structural, cx);
        if self.has_block_selection() {
            let mut moved = false;
            for ix in self.selected_indexes() {
                moved |= self.sink_block(ix, cx);
            }
            return moved;
        }
        let Some(ix) = self.active_index() else {
            return false;
        };
        self.sink_block(ix, cx)
    }

    /// Indexes of the selected blocks, top to bottom.
    pub(crate) fn selected_indexes(&self) -> Vec<usize> {
        self.selected_blocks()
            .into_iter()
            .filter_map(|id| self.index_of(id))
            .collect()
    }

    fn sink_block(&mut self, ix: usize, cx: &mut Context<Self>) -> bool {
        if !self.spec_at(ix, cx).caps().list {
            return false;
        }
        // An item may only nest one level deeper than the item above it.
        let max = if ix == 0 {
            0
        } else if BlockRegistry::global(cx)
            .get(&self.blocks[ix - 1].ty)
            .caps()
            .list
        {
            self.blocks[ix - 1].indent + 1
        } else {
            0
        };
        if self.blocks[ix].indent >= max {
            return false;
        }
        self.blocks[ix].indent += 1;
        let id = self.blocks[ix].id;
        self.remeasure(id, cx);
        cx.emit(DocumentChanged);
        cx.notify();
        true
    }

    /// Tiptap `liftListItem`: Shift-Tab.
    pub fn lift_list_item(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.record(Step::Structural, cx);
        if self.has_block_selection() {
            let mut moved = false;
            for ix in self.selected_indexes() {
                moved |= self.lift_block(ix, window, cx);
            }
            return moved;
        }
        let Some(ix) = self.active_index() else {
            return false;
        };
        self.lift_block(ix, window, cx)
    }

    fn lift_block(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.blocks[ix].indent > 0 {
            self.blocks[ix].indent -= 1;
            let id = self.blocks[ix].id;
            self.remeasure(id, cx);
            cx.emit(DocumentChanged);
            cx.notify();
            return true;
        }
        if self.spec_at(ix, cx).caps().list {
            let id = self.blocks[ix].id;
            self.set_block_type(id, types::PARAGRAPH.into(), BlockAttrs::default(), window, cx);
            self.focus_block(id, Caret::End, window, cx);
            return true;
        }
        false
    }

    pub fn duplicate_block(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.record(Step::Structural, cx);
        // A selection duplicates whole, below itself, and stays selected.
        if self.has_block_selection() {
            let indexes = self.selected_indexes();
            let Some(&last) = indexes.last() else { return };
            let copies: Vec<BlockContent> = indexes
                .iter()
                .map(|ix| self.blocks[*ix].content())
                .collect();
            let mut made = Vec::new();
            for (offset, content) in copies.into_iter().enumerate() {
                made.push(self.insert_block(last + 1 + offset, content, window, cx));
            }
            self.selected = made;
            cx.notify();
            return;
        }
        let Some(ix) = self.active_index() else { return };
        let content = self.blocks[ix].content();
        let id = self.insert_block(ix + 1, content, window, cx);
        self.focus_block(id, Caret::End, window, cx);
    }

    pub fn delete_active_block(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_block_selection() {
            self.delete_selected_blocks(window, cx);
            return;
        }
        self.record(Step::Structural, cx);
        let Some(ix) = self.active_index() else { return };
        if self.blocks.len() == 1 {
            let id = self.blocks[ix].id;
            self.set_block_text(ix, String::new(), Some(0), window, cx);
            self.blocks[ix].marks.clear();
            self.set_block_type(id, types::PARAGRAPH.into(), BlockAttrs::default(), window, cx);
            return;
        }
        let id = self.blocks[ix].id;
        self.remove_block(id, cx);
        let target = ix.saturating_sub(1).min(self.blocks.len() - 1);
        let target_id = self.blocks[target].id;
        self.focus_block(target_id, Caret::End, window, cx);
    }

    /// Move a block past its neighbour, taking focus with it.
    pub fn move_block(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.record(Step::Structural, cx);
        let Some(ix) = self.active_index() else { return };
        let target = ix as isize + delta;
        if target < 0 || target as usize >= self.blocks.len() {
            return;
        }
        self.blocks.swap(ix, target as usize);
        cx.emit(DocumentChanged);
        cx.notify();
    }

    /// Move the block at `from` so it lands before index `to`, as a drag does.
    pub fn reorder_block(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        self.record(Step::Structural, cx);
        if from >= self.blocks.len() || to > self.blocks.len() || from == to {
            return;
        }
        let block = self.blocks.remove(from);
        let to = if to > from { to - 1 } else { to };
        self.blocks.insert(to.min(self.blocks.len()), block);
        cx.emit(DocumentChanged);
        cx.notify();
    }

    /// Move a run of blocks so that they land before what is currently at
    /// `to`, keeping the order they had.
    pub fn reorder_blocks(&mut self, ids: &[BlockId], to: usize, cx: &mut Context<Self>) {
        if ids.len() <= 1 {
            if let Some(from) = ids.first().and_then(|id| self.index_of(*id)) {
                self.reorder_block(from, to, cx);
            }
            return;
        }
        self.record(Step::Structural, cx);

        // The landing place is named by the block that is there now, so that
        // taking the moved blocks out cannot shift it.
        let anchor = self.blocks.get(to).map(|block| block.id);
        let mut moving = Vec::new();
        for id in ids {
            let Some(ix) = self.index_of(*id) else { continue };
            moving.push(self.blocks.remove(ix));
        }
        let at = match anchor.and_then(|id| self.index_of(id)) {
            Some(ix) => ix,
            None => self.blocks.len(),
        };
        for (offset, block) in moving.into_iter().enumerate() {
            self.blocks.insert((at + offset).min(self.blocks.len()), block);
        }
        cx.emit(DocumentChanged);
        cx.notify();
    }

    // -------------------------------------------------------- text plumbing

    /// Replace a block's whole text, keeping the mirror, marks and input in
    /// step, and optionally placing the caret.
    pub(crate) fn set_block_text(
        &mut self,
        ix: usize,
        text: String,
        caret: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ix >= self.blocks.len() {
            return;
        }
        let id = self.blocks[ix].id;
        self.blocks[ix].text = text.clone();
        self.blocks[ix].marks.clamp(text.len());

        let state = self.blocks[ix].state.clone();
        state.update(cx, |state, cx| {
            state.replace_all(text.clone(), window, cx);
            if let Some(caret) = caret {
                let caret = caret.min(text.len());
                state.set_selected_range(caret..caret, cx);
            }
        });
        self.remeasure(id, cx);
        self.apply_decorations(id, cx);
        cx.emit(DocumentChanged);
        cx.notify();
    }

    /// Apply a ranged edit to a block, moving its marks with the text.
    pub(crate) fn edit_block_text(
        &mut self,
        ix: usize,
        range: Range<usize>,
        replacement: &str,
        caret: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ix >= self.blocks.len() {
            return;
        }
        let mut text = self.blocks[ix].text.clone();
        if range.end > text.len()
            || !text.is_char_boundary(range.start)
            || !text.is_char_boundary(range.end)
        {
            return;
        }
        text.replace_range(range.clone(), replacement);
        self.blocks[ix]
            .marks
            .remap(&Edit::new(range, replacement.len()));
        self.set_block_text(ix, text, caret, window, cx);
    }

    /// Ask the platform for an image file and put it in the block.
    pub fn pick_image(&mut self, id: BlockId, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose image".into()),
        });

        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this, cx| this.set_image_source(id, path, cx));
        })
        .detach();
    }

    /// Point an image block at a file on disk.
    pub fn set_image_source(
        &mut self,
        id: BlockId,
        path: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.index_of(id) else { return };
        self.record(Step::Structural, cx);
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        self.blocks[ix].attrs.src = Some(path.to_string_lossy().to_string().into());
        self.blocks[ix].attrs.alt = Some(name.into());
        cx.emit(DocumentChanged);
        cx.notify();
    }

    /// Set the language a code block is highlighted with.
    pub fn set_code_language(
        &mut self,
        language: impl Into<SharedString>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.active_index() else { return };
        if self.blocks[ix].ty != types::CODE_BLOCK {
            return;
        }
        self.record(Step::Structural, cx);
        let language = language.into();
        self.blocks[ix].attrs.language = Some(language.clone());
        let state = self.blocks[ix].state.clone();
        state.update(cx, |state, cx| state.set_highlighter(language, cx));
        cx.emit(DocumentChanged);
        cx.notify();
    }

    /// Put the active block's text on the clipboard.
    pub fn copy_active_block(&mut self, cx: &mut Context<Self>) {
        let Some(ix) = self.active_index() else { return };
        let text = self.blocks[ix].text.clone();
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text));
    }

    /// Insert plain text at the caret, as a paste of unformatted text does.
    pub fn insert_content(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.active_index() else { return };
        let range = self.blocks[ix].state.read(cx).selected_range();
        let caret = range.start + text.len();
        self.edit_block_text(ix, range, text, Some(caret), window, cx);
    }
}
