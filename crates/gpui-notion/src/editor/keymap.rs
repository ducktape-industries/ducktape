//! Key handling.
//!
//! The block inputs bind their own actions in the `Input` key context, so the
//! editor pre-empts the ones that mean something at block level — Enter,
//! Backspace at the start, arrows at an edge, Tab — by capturing the action on
//! the way down and stopping it there.

use gpui_kit::component::input::{
    Backspace, Delete, Enter, Escape, IndentInline, MoveDown, MoveLeft, MoveRight, MoveUp,
    OutdentInline, Redo, Undo,
};
use gpui_kit::{App, Context, InteractiveElement, Window};

use super::actions;
use super::block::types;
use super::mark::{HighlightColor, MarkKind};
use super::view::{Caret, NotionEditor};

impl NotionEditor {
    /// Caret row within the block's wrapped text, and the row count.
    pub(crate) fn caret_row(&self, ix: usize, cx: &App) -> Option<(usize, usize)> {
        let state = self.blocks[ix].state.read(cx);
        let (caret, line_height) = state.cursor_layout()?;
        let text_bounds = state.text_bounds()?;
        if line_height <= gpui_kit::px(0.) {
            return None;
        }
        let offset = (caret.origin.y - text_bounds.origin.y) / line_height;
        let row = offset.round().max(0.) as usize;
        // Count the rows of *text*, not of the box: a block's text area is
        // deliberately a little taller than its text, and a caret on the last
        // row of a box that has room to spare is still on the last row.
        let text_height = state
            .range_to_bounds(&(0..self.blocks[ix].text().len()))
            .map(|bounds| bounds.size.height)
            .unwrap_or(text_bounds.size.height);
        let rows = (text_height / line_height).round().max(1.) as usize;
        Some((row, rows))
    }

    pub(crate) fn caret_on_first_row(&self, ix: usize, cx: &App) -> bool {
        match self.caret_row(ix, cx) {
            Some((row, _)) => row == 0,
            // Before the first layout, fall back to the buffer line.
            None => self.blocks[ix].state.read(cx).cursor_position().line == 0,
        }
    }

    pub(crate) fn caret_on_last_row(&self, ix: usize, cx: &App) -> bool {
        match self.caret_row(ix, cx) {
            Some((row, rows)) => row + 1 >= rows,
            None => true,
        }
    }

    fn caret(&self, ix: usize, cx: &App) -> (usize, usize, bool) {
        let state = self.blocks[ix].state.read(cx);
        let range = state.selected_range();
        (range.start, range.end, range.is_empty())
    }

    /// Attach every editor-level key handler to the root element.
    pub(crate) fn with_key_handlers<E: InteractiveElement>(
        &self,
        el: E,
        cx: &mut Context<Self>,
    ) -> E {
        el
            // ------------------------------------------------- structural keys
            .capture_action(cx.listener(Self::on_enter))
            .capture_action(cx.listener(Self::on_backspace))
            .capture_action(cx.listener(Self::on_delete))
            .capture_action(cx.listener(Self::on_move_up))
            .capture_action(cx.listener(Self::on_move_down))
            .capture_action(cx.listener(Self::on_move_left))
            .capture_action(cx.listener(Self::on_move_right))
            .capture_action(cx.listener(Self::on_tab))
            .capture_action(cx.listener(Self::on_shift_tab))
            .capture_action(cx.listener(Self::on_escape))
            .capture_action(cx.listener(Self::on_select_up))
            .capture_action(cx.listener(Self::on_select_down))
            .capture_action(cx.listener(Self::on_select_all))
            .on_key_down(cx.listener(Self::on_typing_over_blocks))
            .capture_action(cx.listener(Self::on_copy))
            .capture_action(cx.listener(Self::on_cut))
            .capture_action(cx.listener(|this, _: &Undo, window, cx| {
                this.undo(window, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &Redo, window, cx| {
                this.redo(window, cx);
                cx.stop_propagation();
            }))
            // -------------------------------------------------------- commands
            .on_action(cx.listener(|this, _: &actions::ToggleBold, window, cx| {
                this.toggle_bold(window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::ToggleItalic, window, cx| {
                this.toggle_italic(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &actions::ToggleUnderline, window, cx| {
                    this.toggle_underline(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &actions::ToggleStrike, window, cx| {
                this.toggle_strike(window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::ToggleCode, window, cx| {
                this.toggle_code(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &actions::ToggleHighlight, window, cx| {
                    this.toggle_highlight(Some(HighlightColor::Yellow), window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &actions::ToggleSuperscript, window, cx| {
                    this.toggle_superscript(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &actions::ToggleSubscript, window, cx| {
                    this.toggle_subscript(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &actions::ClearMarks, window, cx| {
                this.unset_all_marks(window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::SetParagraph, window, cx| {
                this.set_paragraph(window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::SetHeading1, window, cx| {
                this.toggle_heading(1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::SetHeading2, window, cx| {
                this.toggle_heading(2, window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::SetHeading3, window, cx| {
                this.toggle_heading(3, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &actions::ToggleBulletList, window, cx| {
                    this.toggle_bullet_list(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &actions::ToggleOrderedList, window, cx| {
                    this.toggle_ordered_list(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &actions::ToggleTaskList, window, cx| {
                this.toggle_task_list(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &actions::ToggleBlockquote, window, cx| {
                    this.toggle_blockquote(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &actions::ToggleCodeBlock, window, cx| {
                this.toggle_code_block(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &actions::SetHorizontalRule, window, cx| {
                    this.set_horizontal_rule(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &actions::DuplicateBlock, window, cx| {
                this.duplicate_block(window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::DeleteBlock, window, cx| {
                this.delete_active_block(window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::MoveBlockUp, _window, cx| {
                this.move_block(-1, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::MoveBlockDown, _window, cx| {
                this.move_block(1, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::OpenSlashMenu, window, cx| {
                this.open_slash_menu(window, cx)
            }))
            .on_action(
                cx.listener(|this, action: &actions::SetCodeLanguage, window, cx| {
                    this.set_code_language(action.0, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &actions::CopyBlock, _window, cx| {
                this.copy_active_block(cx)
            }))
            .on_action(cx.listener(|this, _: &actions::InsertRowAbove, window, cx| {
                this.insert_row_at_caret(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::InsertRowBelow, window, cx| {
                this.insert_row_at_caret(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::InsertColumnLeft, window, cx| {
                this.insert_column_at_caret(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::InsertColumnRight, window, cx| {
                this.insert_column_at_caret(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::DeleteRow, window, cx| {
                this.delete_row_at_caret(window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::DeleteColumn, window, cx| {
                this.delete_column_at_caret(window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::AddComment, window, cx| {
                this.add_comment(window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::OpenEmojiMenu, window, cx| {
                this.open_suggestion(super::suggestion::Trigger::Emoji, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &actions::OpenMentionMenu, window, cx| {
                    this.open_suggestion(super::suggestion::Trigger::Mention, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &actions::InsertImage, window, cx| {
                this.set_image("", "", window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::SetLink, window, cx| {
                this.open_link_editor(window, cx)
            }))
            .on_action(
                cx.listener(|this, action: &actions::ApplyColor, window, cx| match action {
                    actions::ApplyColor::Text(color) => this.set_color(*color, window, cx),
                    actions::ApplyColor::Highlight(color) => {
                        this.toggle_highlight(Some(*color), window, cx)
                    }
                }),
            )
    }

    // ------------------------------------------------------------- handlers

    fn on_enter(&mut self, action: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        // A table cell is its own text area; the document keeps out of it.
        if self.focused_cell().is_some() {
            return;
        }
        if self.comment_draft_is_open() {
            // The comment box owns Enter; it posts what was typed.
            return;
        }
        if self.link_editor_is_open() {
            self.apply_link_editor(window, cx);
            cx.stop_propagation();
            return;
        }
        if self.suggestion_is_open() {
            self.confirm_suggestion(window, cx);
            cx.stop_propagation();
            return;
        }
        let Some(ix) = self.active_index() else { return };

        // Shift-Enter is a soft break: let the input insert the newline.
        if action.shift {
            return;
        }

        if self.spec_at(ix, cx).caps().multiline {
            // Three Enters in a row leave a code block, as Tiptap's
            // `exitOnTripleEnter` does.
            let (start, _, collapsed) = self.caret(ix, cx);
            let text = &self.blocks[ix].text;
            if collapsed && start == text.len() && text.ends_with("\n\n") {
                let trimmed = text[..text.len() - 2].to_string();
                self.set_block_text(ix, trimmed, None, window, cx);
                let next = self.ensure_paragraph_after(ix, window, cx);
                self.focus_block(next, Caret::Start, window, cx);
                cx.stop_propagation();
            }
            return;
        }

        self.split_block(window, cx);
        cx.stop_propagation();
    }

    fn on_backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        // A table cell is its own text area; the document keeps out of it.
        if self.focused_cell().is_some() {
            return;
        }
        // While a popover owns the keyboard, the document keeps its hands off.
        if self.link_editor_is_open() || self.comment_draft_is_open() {
            return;
        }
        if self.has_block_selection() {
            self.delete_selected_blocks(window, cx);
            cx.stop_propagation();
            return;
        }
        let Some(ix) = self.active_index() else { return };
        let (start, _, collapsed) = self.caret(ix, cx);
        if !collapsed || start != 0 {
            return;
        }
        self.join_backward(window, cx);
        cx.stop_propagation();
    }

    fn on_delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        // A table cell is its own text area; the document keeps out of it.
        if self.focused_cell().is_some() {
            return;
        }
        // While a popover owns the keyboard, the document keeps its hands off.
        if self.link_editor_is_open() || self.comment_draft_is_open() {
            return;
        }
        if self.has_block_selection() {
            self.delete_selected_blocks(window, cx);
            cx.stop_propagation();
            return;
        }
        let Some(ix) = self.active_index() else { return };
        let (start, _, collapsed) = self.caret(ix, cx);
        if !collapsed || start != self.blocks[ix].text.len() {
            return;
        }
        self.join_forward(window, cx);
        cx.stop_propagation();
    }

    fn on_move_up(&mut self, _: &MoveUp, window: &mut Window, cx: &mut Context<Self>) {
        // A table cell is its own text area; the document keeps out of it.
        if self.focused_cell().is_some() {
            return;
        }
        // While a popover owns the keyboard, the document keeps its hands off.
        if self.link_editor_is_open() || self.comment_draft_is_open() {
            return;
        }
        if self.suggestion_is_open() {
            self.move_suggestion_selection(-1, cx);
            cx.stop_propagation();
            return;
        }
        let Some(ix) = self.active_index() else { return };
        if !self.caret_on_first_row(ix, cx) {
            return;
        }
        if self.focus_sibling(ix, -1, Caret::End, window, cx) {
            cx.stop_propagation();
        }
    }

    fn on_move_down(&mut self, _: &MoveDown, window: &mut Window, cx: &mut Context<Self>) {
        // A table cell is its own text area; the document keeps out of it.
        if self.focused_cell().is_some() {
            return;
        }
        // While a popover owns the keyboard, the document keeps its hands off.
        if self.link_editor_is_open() || self.comment_draft_is_open() {
            return;
        }
        if self.suggestion_is_open() {
            self.move_suggestion_selection(1, cx);
            cx.stop_propagation();
            return;
        }
        let Some(ix) = self.active_index() else { return };
        if !self.caret_on_last_row(ix, cx) {
            return;
        }
        if self.focus_sibling(ix, 1, Caret::Start, window, cx) {
            cx.stop_propagation();
            return;
        }
        // Down from the last block leaves a code block or any other node with
        // somewhere to go, as Tiptap's `exitOnArrowDown` does.
        if ix + 1 == self.block_count() && !self.block_is_empty_paragraph(ix) {
            let id = self.ensure_paragraph_after(ix, window, cx);
            self.focus_block(id, Caret::Start, window, cx);
            cx.stop_propagation();
        }
    }

    fn on_move_left(&mut self, _: &MoveLeft, window: &mut Window, cx: &mut Context<Self>) {
        // A table cell is its own text area; the document keeps out of it.
        if self.focused_cell().is_some() {
            return;
        }
        // While a popover owns the keyboard, the document keeps its hands off.
        if self.link_editor_is_open() || self.comment_draft_is_open() {
            return;
        }
        let Some(ix) = self.active_index() else { return };
        let (start, _, collapsed) = self.caret(ix, cx);
        if !collapsed || start != 0 {
            return;
        }
        if self.focus_sibling(ix, -1, Caret::End, window, cx) {
            cx.stop_propagation();
        }
    }

    fn on_move_right(&mut self, _: &MoveRight, window: &mut Window, cx: &mut Context<Self>) {
        // A table cell is its own text area; the document keeps out of it.
        if self.focused_cell().is_some() {
            return;
        }
        // While a popover owns the keyboard, the document keeps its hands off.
        if self.link_editor_is_open() || self.comment_draft_is_open() {
            return;
        }
        let Some(ix) = self.active_index() else { return };
        let (start, _, collapsed) = self.caret(ix, cx);
        if !collapsed || start != self.blocks[ix].text.len() {
            return;
        }
        if self.focus_sibling(ix, 1, Caret::Start, window, cx) {
            cx.stop_propagation();
        }
    }

    fn on_tab(&mut self, _: &IndentInline, window: &mut Window, cx: &mut Context<Self>) {
        // While a popover owns the keyboard, the document keeps its hands off.
        if self.link_editor_is_open() || self.comment_draft_is_open() {
            return;
        }
        if self.suggestion_is_open() {
            self.move_suggestion_selection(1, cx);
            cx.stop_propagation();
            return;
        }
        if self.focused_cell().is_some() {
            if self.move_cell(1, window, cx) {
                cx.stop_propagation();
            }
            return;
        }
        let Some(ix) = self.active_index() else { return };
        if self.spec_at(ix, cx).caps().multiline {
            return;
        }
        if self.sink_list_item(window, cx) {
            cx.stop_propagation();
        }
    }

    fn on_shift_tab(&mut self, _: &OutdentInline, window: &mut Window, cx: &mut Context<Self>) {
        // While a popover owns the keyboard, the document keeps its hands off.
        if self.link_editor_is_open() || self.comment_draft_is_open() {
            return;
        }
        if self.suggestion_is_open() {
            self.move_suggestion_selection(-1, cx);
            cx.stop_propagation();
            return;
        }
        if self.focused_cell().is_some() {
            if self.move_cell(-1, window, cx) {
                cx.stop_propagation();
            }
            return;
        }
        let Some(ix) = self.active_index() else { return };
        if self.spec_at(ix, cx).caps().multiline {
            return;
        }
        if self.lift_list_item(window, cx) {
            cx.stop_propagation();
        }
    }

    /// Typing with whole blocks selected replaces them, the way it does in a
    /// word processor and in Notion.
    fn on_typing_over_blocks(
        &mut self,
        event: &gpui_kit::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.has_block_selection() || self.comment_draft_is_open() {
            return;
        }
        let modifiers = event.keystroke.modifiers;
        if modifiers.control || modifiers.platform || modifiers.alt || modifiers.function {
            return;
        }
        let Some(text) = event.keystroke.key_char.clone() else {
            return;
        };
        if text.is_empty() || text.chars().any(char::is_control) {
            return;
        }

        let at = self.selected_indexes().first().copied().unwrap_or(0);
        self.delete_selected_blocks(window, cx);
        let at = at.min(self.blocks.len());
        // Deleting everything already leaves an empty paragraph behind; the
        // typed character belongs in that one rather than in a second.
        let id = match self.block_is_empty_paragraph(at) {
            true => self.block_id_at(at).expect("checked above"),
            false => self.insert_block(at, super::block::BlockContent::paragraph(""), window, cx),
        };
        self.focus_block(id, Caret::Start, window, cx);
        if let Some(ix) = self.index_of(id) {
            self.edit_block_text(ix, 0..0, &text, Some(text.len()), window, cx);
        }
        cx.stop_propagation();
    }

    fn on_escape(&mut self, _: &Escape, window: &mut Window, cx: &mut Context<Self>) {
        if self.comment_draft_is_open() {
            self.close_comment_popover(window, cx);
            cx.stop_propagation();
            return;
        }
        if self.link_editor_is_open() {
            self.close_link_editor(window, cx);
            cx.stop_propagation();
            return;
        }
        if self.suggestion_is_open() {
            self.close_suggestion_menu(cx);
            cx.stop_propagation();
            return;
        }
        if !self.selected.is_empty() {
            self.selected.clear();
            cx.notify();
            return;
        }
        // Escape out of a table cell selects the table itself.
        if let Some((block, _)) = self.focused_cell() {
            self.select_block_as_node(block, window, cx);
            cx.stop_propagation();
            return;
        }
        // Escape with a caret selects the block as a node, as Notion does.
        let Some(id) = self.focused else { return };
        self.selected = vec![id];
        self.take_focus_from_blocks(window, cx);
        cx.notify();
    }

    /// Mark shortcuts the toolbar mirrors; used by tests.
    pub fn mark_for_action(action: &dyn gpui_kit::Action) -> Option<MarkKind> {
        if action.as_any().is::<actions::ToggleBold>() {
            return Some(MarkKind::Bold);
        }
        if action.as_any().is::<actions::ToggleItalic>() {
            return Some(MarkKind::Italic);
        }
        if action.as_any().is::<actions::ToggleUnderline>() {
            return Some(MarkKind::Underline);
        }
        if action.as_any().is::<actions::ToggleStrike>() {
            return Some(MarkKind::Strike);
        }
        if action.as_any().is::<actions::ToggleCode>() {
            return Some(MarkKind::Code);
        }
        None
    }

    /// The type name of the active block, for the "Turn into" label.
    pub fn active_type(&self) -> &str {
        self.active_index()
            .map(|ix| self.blocks[ix].ty.as_ref())
            .unwrap_or(types::PARAGRAPH)
    }
}
