//! Selection that spans blocks, and what can be done to it.
//!
//! Inside a block the input owns the selection. Once a selection reaches past
//! a block's edge the editor takes over and selects whole blocks, which is
//! what Notion does and what `Mod+A` escalates to.

use gpui_kit::base::actions::{SelectDown, SelectUp};
use gpui_kit::component::input::{Copy, Cut, SelectAll};
use gpui_kit::{
    ClipboardItem, Context, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, Window,
};

use super::block::{BlockContent, BlockId, BlockRegistry, types};
use super::history::Step;
use super::view::{Caret, NotionEditor};

impl NotionEditor {
    /// Blocks currently selected as nodes, in document order.
    pub fn selected_blocks(&self) -> Vec<BlockId> {
        let mut ids: Vec<BlockId> = self
            .blocks
            .iter()
            .filter(|block| self.selected.contains(&block.id))
            .map(|block| block.id)
            .collect();
        ids.dedup();
        ids
    }

    pub fn has_block_selection(&self) -> bool {
        !self.selected.is_empty()
    }

    /// Select every block between `from` and `to`, inclusive.
    pub fn select_block_range(
        &mut self,
        from: usize,
        to: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (low, high) = if from <= to { (from, to) } else { (to, from) };
        self.selected = self.blocks[low..=high.min(self.blocks.len() - 1)]
            .iter()
            .map(|block| block.id)
            .collect();
        self.take_focus_from_blocks(window, cx);
        cx.notify();
    }

    pub fn select_all_blocks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = self.blocks.iter().map(|block| block.id).collect();
        self.take_focus_from_blocks(window, cx);
        cx.notify();
    }

    /// Move keyboard focus to the editor itself, so a block selection is not
    /// undone by the focused input reclaiming it on the next frame.
    pub(crate) fn take_focus_from_blocks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focused = None;
        let handle = self.focus_handle_for_editor();
        handle.focus(window, cx);
    }

    pub fn clear_block_selection(&mut self, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            return;
        }
        self.selected.clear();
        cx.notify();
    }

    /// Extend the block selection by one block in `delta`'s direction.
    fn extend_block_selection(
        &mut self,
        delta: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let anchor = self
            .selected
            .first()
            .and_then(|id| self.index_of(*id))
            .or_else(|| self.active_index());
        let Some(anchor) = anchor else { return false };

        let last = self
            .selected
            .last()
            .and_then(|id| self.index_of(*id))
            .unwrap_or(anchor);
        let next = last as isize + delta;
        if next < 0 || next as usize >= self.blocks.len() {
            return false;
        }
        self.select_block_range(anchor, next as usize, window, cx);
        true
    }

    /// Delete every selected block, leaving a paragraph if none remain.
    pub fn delete_selected_blocks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            return;
        }
        self.record(Step::Structural, cx);
        let first = self
            .selected_blocks()
            .first()
            .and_then(|id| self.index_of(*id))
            .unwrap_or(0);

        for id in self.selected_blocks() {
            self.remove_block(id, cx);
        }
        self.selected.clear();

        if self.blocks.is_empty() {
            let id = self.insert_block(0, BlockContent::paragraph(""), window, cx);
            self.focus_block(id, Caret::Start, window, cx);
            return;
        }
        let target = first.min(self.blocks.len() - 1);
        let id = self.blocks[target].id;
        self.focus_block(id, Caret::Start, window, cx);
    }

    /// The selected blocks as markdown, which is what the clipboard gets.
    pub fn selected_markdown(&self, cx: &gpui_kit::App) -> String {
        self.markdown_of(&self.selected_blocks(), cx)
    }

    /// Named blocks as markdown, in document order.
    pub fn markdown_of(&self, ids: &[BlockId], cx: &gpui_kit::App) -> String {
        let registry = BlockRegistry::global(cx);
        self.blocks
            .iter()
            .filter(|block| ids.contains(&block.id))
            .map(|block| {
                let indent = "    ".repeat(block.indent);
                let prefix = match block.ty.as_ref() {
                    types::HEADING => "#".repeat(block.attrs.level.max(1) as usize) + " ",
                    types::BULLET_LIST => "- ".to_string(),
                    types::ORDERED_LIST => "1. ".to_string(),
                    types::TASK_LIST => {
                        if block.attrs.checked {
                            "- [x] ".to_string()
                        } else {
                            "- [ ] ".to_string()
                        }
                    }
                    types::BLOCKQUOTE => "> ".to_string(),
                    types::HORIZONTAL_RULE => return "---".to_string(),
                    types::TABLE => {
                        return self.grid_markdown(block.id).unwrap_or_default();
                    }
                    types::CODE_BLOCK => {
                        let language = block.attrs.language.clone().unwrap_or_default();
                        return format!("```{language}\n{}\n```", block.text);
                    }
                    _ => String::new(),
                };
                let _ = registry.get(&block.ty);
                format!("{indent}{prefix}{}", block.text)
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn copy_selected_blocks(&mut self, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            return;
        }
        let markdown = self.selected_markdown(cx);
        cx.write_to_clipboard(ClipboardItem::new_string(markdown));
    }

    /// Whether a press is still down inside the document.
    pub fn press_in_progress(&self) -> bool {
        self.mouse_anchor.is_some()
    }

    /// Whether a press landed on a block's content rather than in the gutter
    /// it reaches into. A block reports the bounds of its content, so its
    /// left edge is where the text starts.
    fn press_is_on_content(&self, id: BlockId, position: Point<Pixels>) -> bool {
        self.block_bounds
            .get(&id)
            .is_some_and(|bounds| position.x >= bounds.left())
    }

    /// The block under a pointer position: the one it is inside, else the
    /// nearest one above or below, so a drag into the margins still lands.
    pub(crate) fn block_at_point(&self, position: Point<Pixels>) -> Option<BlockId> {
        let mut nearest: Option<(Pixels, BlockId)> = None;
        for block in &self.blocks {
            let Some(bounds) = self.block_bounds.get(&block.id) else {
                continue;
            };
            if position.y >= bounds.top() && position.y < bounds.bottom() {
                return Some(block.id);
            }
            let distance = if position.y < bounds.top() {
                bounds.top() - position.y
            } else {
                position.y - bounds.bottom()
            };
            if nearest.is_none_or(|(best, _)| distance < best) {
                nearest = Some((distance, block.id));
            }
        }
        nearest.map(|(_, id)| id)
    }

    // ------------------------------------------------------------- handlers

    /// A press anchors a drag selection, and with Shift extends the current
    /// one to the pressed block the way Notion does.
    pub(crate) fn on_page_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        let Some(id) = self.block_at_point(event.position) else {
            self.mouse_anchor = None;
            return;
        };
        // A press in the gutter reach belongs to the drag handle and the `+`,
        // which run their own drag; only a press on the block's own content
        // starts a selection.
        if !self.press_is_on_content(id, event.position) {
            self.mouse_anchor = None;
            return;
        }

        if event.modifiers.shift {
            let from = self
                .selected_blocks()
                .first()
                .copied()
                .or(self.focused)
                .and_then(|anchor| self.index_of(anchor));
            if let (Some(from), Some(to)) = (from, self.index_of(id))
                && from != to
            {
                self.select_block_range(from, to, window, cx);
                cx.stop_propagation();
                return;
            }
        }

        self.mouse_anchor = Some(id);
        self.clear_block_selection(cx);
    }

    /// Dragging out of the block the press started in turns the drag into a
    /// block selection, which is what Notion switches to at the same point.
    pub(crate) fn on_page_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.pressed_button != Some(MouseButton::Left) || cx.has_active_drag() {
            return;
        }
        let Some(anchor) = self.mouse_anchor else {
            return;
        };
        let Some(over) = self.block_at_point(event.position) else {
            return;
        };
        let (Some(from), Some(to)) = (self.index_of(anchor), self.index_of(over)) else {
            return;
        };

        if from == to {
            // Back inside the block it started in: the input's own text
            // selection takes over again.
            if self.has_block_selection() {
                self.clear_block_selection(cx);
                self.focus_block(anchor, Caret::At(0), window, cx);
            }
            return;
        }

        let wanted: Vec<BlockId> = self.blocks[from.min(to)..=from.max(to)]
            .iter()
            .map(|block| block.id)
            .collect();
        if self.selected != wanted {
            self.select_block_range(from, to, window, cx);
        }
        // The block the drag started in is shown whole, like the rest.
        let len = self.blocks[from].text.len();
        let state = self.blocks[from].state.clone();
        state.update(cx, |state, cx| state.set_selected_range(0..len, cx));
    }

    pub(crate) fn on_page_mouse_up(
        &mut self,
        _: &MouseUpEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.mouse_anchor = None;
    }

    pub(crate) fn on_select_up(&mut self, _: &SelectUp, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_block_selection() {
            if self.extend_block_selection(-1, window, cx) {
                cx.stop_propagation();
            }
            return;
        }
        let Some(ix) = self.active_index() else { return };
        if !self.caret_on_first_row(ix, cx) || ix == 0 {
            return;
        }
        self.select_block_range(ix, ix - 1, window, cx);
        cx.stop_propagation();
    }

    pub(crate) fn on_select_down(
        &mut self,
        _: &SelectDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_block_selection() {
            if self.extend_block_selection(1, window, cx) {
                cx.stop_propagation();
            }
            return;
        }
        let Some(ix) = self.active_index() else { return };
        if !self.caret_on_last_row(ix, cx) || ix + 1 >= self.blocks.len() {
            return;
        }
        self.select_block_range(ix, ix + 1, window, cx);
        cx.stop_propagation();
    }

    /// `Mod+A` selects the block's text first, the document second.
    pub(crate) fn on_select_all(
        &mut self,
        _: &SelectAll,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.active_index() else { return };
        let selection = self.blocks[ix].state.read(cx).selected_range();
        let whole_block = !self.blocks[ix].text.is_empty()
            && selection.start == 0
            && selection.end == self.blocks[ix].text.len();

        if whole_block || self.has_block_selection() || self.blocks[ix].text.is_empty() {
            self.select_all_blocks(window, cx);
            cx.stop_propagation();
        }
    }

    pub(crate) fn on_copy(&mut self, _: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.has_block_selection() {
            return;
        }
        self.copy_selected_blocks(cx);
        cx.stop_propagation();
    }

    pub(crate) fn on_cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.has_block_selection() {
            return;
        }
        self.copy_selected_blocks(cx);
        self.delete_selected_blocks(window, cx);
        cx.stop_propagation();
    }
}
