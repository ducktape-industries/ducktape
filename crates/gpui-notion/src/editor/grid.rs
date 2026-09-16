//! Grids of child text areas, which is what a table block is made of.
//!
//! A block owns one input. A table needs one per cell, so the editor keeps a
//! [`CellGrid`] beside any block whose spec asks for one, and hands it to the
//! spec at render time. Cells hold plain text: marks live on blocks, and a
//! cell is not a block.

use gpui_kit::component::input::{EditorState, InputEvent};
use gpui_kit::{App, AppContext as _, Context, Entity, Focusable as _, Pixels, Subscription, Window, px};

use super::fit::InputFit;
use super::theme::ActiveEditorTheme;

use super::block::{BlockContent, BlockId};
use super::view::NotionEditor;

/// One cell: its own input plus a mirror of what is in it.
pub struct Cell {
    state: Entity<EditorState>,
    text: String,
    /// Height the text in this cell needs, as the input lays it out.
    needed: Pixels,
    /// How much of its height this cell's input keeps for itself.
    fit: InputFit,
    _subscription: Subscription,
}

impl Cell {
    pub fn state(&self) -> &Entity<EditorState> {
        &self.state
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Height the text in the cell needs, at least one row.
    pub fn text_height(&self) -> Pixels {
        self.needed
    }

    /// Height to give this cell's input so `text` pixels of text fit in it.
    pub fn height(&self, text: Pixels) -> Pixels {
        self.fit.height(text)
    }
}

/// Where a cell sits in its grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CellPosition {
    pub row: usize,
    pub column: usize,
}

impl CellPosition {
    pub fn new(row: usize, column: usize) -> Self {
        Self { row, column }
    }
}

/// A rectangular grid of cells. The first row is the header row.
pub struct CellGrid {
    rows: Vec<Vec<Cell>>,
}

impl CellGrid {
    pub fn rows(&self) -> usize {
        self.rows.len()
    }

    pub fn columns(&self) -> usize {
        self.rows.first().map(Vec::len).unwrap_or(0)
    }

    pub fn cell(&self, at: CellPosition) -> Option<&Cell> {
        self.rows.get(at.row)?.get(at.column)
    }

    pub fn row(&self, row: usize) -> Option<&[Cell]> {
        self.rows.get(row).map(Vec::as_slice)
    }

    /// Height the tallest text in a row needs; every cell in the row is laid
    /// out to it, so the row reads as one line of the table.
    pub fn row_text_height(&self, row: usize) -> Pixels {
        self.rows
            .get(row)
            .map(|cells| {
                cells
                    .iter()
                    .map(Cell::text_height)
                    .fold(px(0.), |tallest, height| tallest.max(height))
            })
            .unwrap_or(px(0.))
    }

    /// Height of the row's box, which the controls beside it match.
    pub fn row_height(&self, row: usize) -> Pixels {
        let text = self.row_text_height(row);
        self.rows
            .get(row)
            .map(|cells| {
                cells
                    .iter()
                    .map(|cell| cell.height(text))
                    .fold(px(0.), |tallest, height| tallest.max(height))
            })
            .unwrap_or(text)
    }

    /// Every position, reading order, which is what Tab walks.
    pub fn positions(&self) -> impl Iterator<Item = CellPosition> + '_ {
        self.rows.iter().enumerate().flat_map(|(row, cells)| {
            (0..cells.len()).map(move |column| CellPosition::new(row, column))
        })
    }

    /// The grid as a markdown table, header row included.
    pub fn to_markdown(&self) -> String {
        let mut lines = Vec::new();
        for (ix, row) in self.rows.iter().enumerate() {
            let cells: Vec<&str> = row.iter().map(|cell| cell.text()).collect();
            lines.push(format!("| {} |", cells.join(" | ")));
            if ix == 0 {
                let rule: Vec<&str> = row.iter().map(|_| "---").collect();
                lines.push(format!("| {} |", rule.join(" | ")));
            }
        }
        lines.join("\n")
    }
}

/// Cell text is kept on the block as one attribute so that undo, redo and
/// any host that stores `BlockContent` round-trip a table without knowing
/// anything about grids. Rows and cells are split by control characters no
/// keyboard produces.
const ROW_SEPARATOR: char = '\u{1e}';
const CELL_SEPARATOR: char = '\u{1f}';
const CELLS_ATTRIBUTE: &str = "cells";

/// What a table starts as when nothing says otherwise.
const DEFAULT_ROWS: usize = 3;
const DEFAULT_COLUMNS: usize = 3;

/// What a cell carries across a rebuild: its input, its text, the height its
/// text needs and what its input keeps for itself.
type CellState = (Entity<EditorState>, String, Pixels, InputFit);

impl NotionEditor {
    /// The grid beside a block, if it has one.
    pub fn grid(&self, block: BlockId) -> Option<&CellGrid> {
        self.grids.get(&block)
    }

    /// Give a block a fresh grid, replacing any it already had.
    pub(crate) fn create_grid(
        &mut self,
        block: BlockId,
        rows: usize,
        columns: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = vec![vec![String::new(); columns]; rows];
        self.build_grid(block, text, window, cx);
    }

    /// Build the grid a block's `cells` attribute describes, which is how a
    /// table survives undo, redo and reloading a document.
    pub(crate) fn restore_grid(
        &mut self,
        block: BlockId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let stored = self
            .block(block)
            .and_then(|block| block.attrs.extra(CELLS_ATTRIBUTE).cloned());
        let text = match stored {
            Some(stored) => decode_cells(&stored),
            None => vec![vec![String::new(); DEFAULT_COLUMNS]; DEFAULT_ROWS],
        };
        self.build_grid(block, text, window, cx);
    }

    fn build_grid(
        &mut self,
        block: BlockId,
        text: Vec<Vec<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut grid = CellGrid { rows: Vec::new() };
        for (row, cells) in text.iter().enumerate() {
            let mut built = Vec::new();
            for (column, text) in cells.iter().enumerate() {
                let mut cell = self.make_cell(block, CellPosition::new(row, column), window, cx);
                if !text.is_empty() {
                    let state = cell.state.clone();
                    state.update(cx, |state, cx| state.set_value(text.clone(), window, cx));
                    cell.text = text.clone();
                }
                built.push(cell);
            }
            grid.rows.push(built);
        }
        self.grids.insert(block, grid);
        self.store_cells(block, cx);
        cx.notify();
    }

    /// Copy the grid's text back onto the block, where the document keeps it.
    fn store_cells(&mut self, block: BlockId, cx: &mut Context<Self>) {
        let Some(grid) = self.grids.get(&block) else {
            return;
        };
        let encoded = encode_cells(grid);
        if let Some(block) = self.block_mut(block) {
            block.attrs.set_extra(CELLS_ATTRIBUTE, encoded);
        }
        cx.notify();
    }

    fn make_cell(
        &mut self,
        block: BlockId,
        at: CellPosition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Cell {
        let state = cx.new(|cx| super::fit::document_text_state(window, cx));
        let subscription = cx.subscribe_in(&state, window, move |this: &mut Self, state, event, _window, cx| {
            if matches!(event, InputEvent::Change) {
                let text = state.read(cx).value().to_string();
                this.set_cell_mirror(block, at, text, cx);
            }
        });
        Cell {
            state,
            text: String::new(),
            needed: px(0.),
            fit: InputFit::new(cx.editor_theme()),
            _subscription: subscription,
        }
    }

    /// Measure what every cell's input did with the height it was given, the
    /// way blocks are measured, so a cell never scrolls inside itself.
    pub(crate) fn sync_cell_insets(&mut self, cx: &mut Context<Self>) {
        let theme = cx.editor_theme().clone();
        let fallback_line_height = theme.table_text_size * theme.table_line_height;
        for grid in self.grids.values_mut() {
            for row in &mut grid.rows {
                for cell in row.iter_mut() {
                    cell.needed =
                        super::fit::text_height(&cell.state, &cell.text, fallback_line_height, cx);
                }
            }

            // A cell is laid out to the tallest text in its row, so that — not
            // its own text — is the height its input has to make room for.
            let targets: Vec<Pixels> = (0..grid.rows.len())
                .map(|row| grid.row_text_height(row))
                .collect();
            for (row, cells) in grid.rows.iter_mut().enumerate() {
                for cell in cells.iter_mut() {
                    let area = cell
                        .state
                        .read(cx)
                        .text_bounds()
                        .map(|bounds| bounds.size.height);
                    if let Some(area) = area {
                        cell.fit.observe(targets[row], area, &theme);
                    }
                }
            }
        }

        // Every cell of a row is laid out to the tallest text in it, so that
        // is the height each of them has to hold.
        let cells: Vec<(Entity<EditorState>, Pixels)> = self
            .grids
            .values()
            .flat_map(|grid| {
                grid.positions().filter_map(|at| {
                    let cell = grid.cell(at)?;
                    Some((cell.state.clone(), grid.row_text_height(at.row)))
                })
            })
            .collect();
        for (state, needed) in cells {
            super::fit::reset_scroll_when_text_fits(&state, needed, cx);
        }
    }

    /// Whether every cell of a table holds all of its text.
    pub fn cells_fit_their_text(&self, block: BlockId, cx: &App) -> bool {
        let Some(grid) = self.grids.get(&block) else {
            return true;
        };
        grid.positions().all(|at| {
            let Some(cell) = grid.cell(at) else {
                return true;
            };
            let area = cell
                .state
                .read(cx)
                .text_bounds()
                .map(|bounds| bounds.size.height);
            InputFit::fits(grid.row_text_height(at.row), area)
        })
    }

    /// What a cell's input did with its height: text needed, text area it
    /// got, its line height, and how far it scrolled.
    pub fn cell_metrics(
        &self,
        block: BlockId,
        at: CellPosition,
        cx: &App,
    ) -> Option<(f32, f32, f32, f32)> {
        let grid = self.grids.get(&block)?;
        let cell = grid.cell(at)?;
        let state = cell.state.read(cx);
        Some((
            f32::from(grid.row_text_height(at.row)),
            f32::from(
                state
                    .text_bounds()
                    .map(|bounds| bounds.size.height)
                    .unwrap_or_default(),
            ),
            f32::from(state.line_height().unwrap_or_default()),
            f32::from(state.scroll_offset().y),
        ))
    }

    /// How far a cell's input has scrolled inside itself; zero is the point.
    pub fn cell_scroll_offset(&self, block: BlockId, at: CellPosition, cx: &App) -> Option<f32> {
        let cell = self.grids.get(&block)?.cell(at)?;
        Some(f32::from(cell.state.read(cx).scroll_offset().y))
    }

    fn set_cell_mirror(&mut self, block: BlockId, at: CellPosition, text: String, cx: &mut Context<Self>) {
        let Some(grid) = self.grids.get_mut(&block) else {
            return;
        };
        let Some(cell) = grid.rows.get_mut(at.row).and_then(|row| row.get_mut(at.column)) else {
            return;
        };
        cell.text = text;
        self.store_cells(block, cx);
    }

    /// Put text in a cell, keeping the mirror the editor measures from and
    /// the document's copy of the table in step — what a host loading a
    /// document calls.
    pub fn set_cell_text(
        &mut self,
        block: BlockId,
        at: CellPosition,
        text: impl Into<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = text.into();
        let Some(state) = self
            .grids
            .get(&block)
            .and_then(|grid| grid.cell(at))
            .map(|cell| cell.state.clone())
        else {
            return;
        };
        state.update(cx, |state, cx| state.set_value(text.clone(), window, cx));
        self.set_cell_mirror(block, at, text, cx);
    }

    /// Put the caret in a cell.
    pub fn focus_cell(
        &mut self,
        block: BlockId,
        at: CellPosition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self
            .grids
            .get(&block)
            .and_then(|grid| grid.cell(at))
            .map(|cell| cell.state.clone())
        else {
            return;
        };
        self.clear_block_selection(cx);
        // Through the state, not the handle: focusing the state also starts
        // the blink cursor, which is what paints the caret.
        state.update(cx, |state, cx| state.focus(window, cx));
        self.focused_cell = Some((block, at));
        cx.notify();
    }

    /// Which cell has the caret, refreshed from the window every frame.
    pub fn focused_cell(&self) -> Option<(BlockId, CellPosition)> {
        self.focused_cell
    }

    /// Track which cell the window is focused on. `in_a_block` says a block's
    /// own input holds the caret, which is what takes it out of the table.
    pub(crate) fn refresh_focused_cell(
        &mut self,
        in_a_block: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let found = self.grids.iter().find_map(|(block, grid)| {
            grid.positions().find_map(|at| {
                let cell = grid.cell(at)?;
                cell.state
                    .focus_handle(cx)
                    .is_focused(window)
                    .then_some((*block, at))
            })
        });
        match found {
            Some(found) => self.focused_cell = Some(found),
            // Nothing in a grid holds the caret. A popover that was opened
            // from a cell keeps it; anything else means the caret left.
            None if self.link_editor_is_open() || self.comment_draft_is_open() => {}
            None if in_a_block || self.has_block_selection() => self.focused_cell = None,
            None => self.focused_cell = None,
        }
    }

    /// Add a row under `row`, or at the end when it is the last one.
    pub fn insert_row(
        &mut self,
        block: BlockId,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let columns = self.grids.get(&block).map(CellGrid::columns).unwrap_or(0);
        if columns == 0 {
            return;
        }
        let at = (row + 1).min(self.grids.get(&block).map(CellGrid::rows).unwrap_or(0));
        let cells: Vec<Cell> = (0..columns)
            .map(|column| self.make_cell(block, CellPosition::new(at, column), window, cx))
            .collect();
        if let Some(grid) = self.grids.get_mut(&block) {
            grid.rows.insert(at, cells);
        }
        self.rebind_cells(block, window, cx);
        self.store_cells(block, cx);
        cx.notify();
    }

    /// Add a column to the right of `column`.
    pub fn insert_column(
        &mut self,
        block: BlockId,
        column: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows = self.grids.get(&block).map(CellGrid::rows).unwrap_or(0);
        let columns = self.grids.get(&block).map(CellGrid::columns).unwrap_or(0);
        if rows == 0 {
            return;
        }
        let at = (column + 1).min(columns);
        for row in 0..rows {
            let cell = self.make_cell(block, CellPosition::new(row, at), window, cx);
            if let Some(grid) = self.grids.get_mut(&block) {
                grid.rows[row].insert(at, cell);
            }
        }
        self.rebind_cells(block, window, cx);
        self.store_cells(block, cx);
        cx.notify();
    }

    /// Take a row out; the header row stays unless it is the only one left.
    pub fn remove_row(
        &mut self,
        block: BlockId,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(grid) = self.grids.get_mut(&block) else {
            return;
        };
        if grid.rows.len() <= 1 || row >= grid.rows.len() {
            return;
        }
        grid.rows.remove(row);
        self.rebind_cells(block, window, cx);
        self.store_cells(block, cx);
        cx.notify();
    }

    pub fn remove_column(
        &mut self,
        block: BlockId,
        column: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(grid) = self.grids.get_mut(&block) else {
            return;
        };
        if grid.columns() <= 1 {
            return;
        }
        for row in &mut grid.rows {
            if column < row.len() {
                row.remove(column);
            }
        }
        self.rebind_cells(block, window, cx);
        self.store_cells(block, cx);
        cx.notify();
    }

    /// Cell subscriptions carry their own position, so any insert or removal
    /// re-subscribes every cell to keep those positions true.
    fn rebind_cells(&mut self, block: BlockId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(grid) = self.grids.get(&block) else {
            return;
        };
        let states: Vec<Vec<CellState>> = grid
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| (cell.state.clone(), cell.text.clone(), cell.needed, cell.fit))
                    .collect()
            })
            .collect();

        let mut rows = Vec::new();
        for (row, cells) in states.into_iter().enumerate() {
            let mut rebound = Vec::new();
            for (column, (state, text, needed, fit)) in cells.into_iter().enumerate() {
                let at = CellPosition::new(row, column);
                let subscription = cx.subscribe_in(
                    &state,
                    window,
                    move |this: &mut Self, state, event, _window, cx| {
                        if matches!(event, InputEvent::Change) {
                            let text = state.read(cx).value().to_string();
                            this.set_cell_mirror(block, at, text, cx);
                        }
                    },
                );
                // The input entity survives the rebuild, so what it taught us
                // about its inset survives with it.
                rebound.push(Cell {
                    state,
                    text,
                    needed,
                    fit,
                    _subscription: subscription,
                });
            }
            rows.push(rebound);
        }
        self.grids.insert(block, CellGrid { rows });
    }

    /// Move the caret one cell forward or back, adding a row when Tab runs off
    /// the end of the table, the way Tiptap's `goToNextCell` does.
    pub fn move_cell(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some((block, at)) = self.focused_cell else {
            return false;
        };
        let Some(grid) = self.grids.get(&block) else {
            return false;
        };
        let (rows, columns) = (grid.rows(), grid.columns());
        if columns == 0 {
            return false;
        }

        let flat = at.row * columns + at.column;
        let next = flat as isize + delta;
        if next < 0 {
            return false;
        }
        let next = next as usize;
        if next >= rows * columns {
            let last = rows.saturating_sub(1);
            self.insert_row(block, last, window, cx);
            self.focus_cell(block, CellPosition::new(last + 1, 0), window, cx);
            return true;
        }
        self.focus_cell(
            block,
            CellPosition::new(next / columns, next % columns),
            window,
            cx,
        );
        true
    }

    /// Row and column edits aimed at whichever cell has the caret, which is
    /// what the block menu's Table entries call.
    pub fn insert_row_at_caret(
        &mut self,
        below: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((block, at)) = self.focused_cell else {
            return;
        };
        let row = if below { at.row } else { at.row.saturating_sub(1) };
        // Inserting "above" the header row still lands under it: the header
        // is part of the table's shape, not a row to push down.
        self.insert_row(block, row, window, cx);
    }

    pub fn insert_column_at_caret(
        &mut self,
        right: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((block, at)) = self.focused_cell else {
            return;
        };
        let column = if right {
            at.column
        } else {
            at.column.saturating_sub(1)
        };
        self.insert_column(block, column, window, cx);
    }

    pub fn delete_row_at_caret(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((block, at)) = self.focused_cell else {
            return;
        };
        self.remove_row(block, at.row, window, cx);
    }

    pub fn delete_column_at_caret(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((block, at)) = self.focused_cell else {
            return;
        };
        self.remove_column(block, at.column, window, cx);
    }

    /// Markdown for a table block, used by the clipboard.
    pub(crate) fn grid_markdown(&self, block: BlockId) -> Option<String> {
        self.grids.get(&block).map(CellGrid::to_markdown)
    }
}

/// The block a document carries for a table of `rows`.
///
/// Cell text travels on the block like any other attribute, so a table needs
/// nothing but [`BlockContent`] to be loaded, copied, undone or stored.
pub fn table_content(rows: &[&[&str]]) -> BlockContent {
    let cells = rows
        .iter()
        .map(|row| row.join(&CELL_SEPARATOR.to_string()))
        .collect::<Vec<_>>()
        .join(&ROW_SEPARATOR.to_string());

    let mut attrs = super::block::BlockAttrs::default();
    attrs.set_extra(CELLS_ATTRIBUTE, cells);
    BlockContent::new(super::block::types::TABLE, String::new()).with_attrs(attrs)
}

fn encode_cells(grid: &CellGrid) -> String {
    grid.rows
        .iter()
        .map(|row| {
            row.iter()
                .map(Cell::text)
                .collect::<Vec<_>>()
                .join(&CELL_SEPARATOR.to_string())
        })
        .collect::<Vec<_>>()
        .join(&ROW_SEPARATOR.to_string())
}

fn decode_cells(stored: &str) -> Vec<Vec<String>> {
    let rows: Vec<Vec<String>> = stored
        .split(ROW_SEPARATOR)
        .map(|row| {
            row.split(CELL_SEPARATOR)
                .map(ToString::to_string)
                .collect()
        })
        .collect();
    if rows.is_empty() || rows.iter().all(Vec::is_empty) {
        return vec![vec![String::new(); DEFAULT_COLUMNS]; DEFAULT_ROWS];
    }
    rows
}
