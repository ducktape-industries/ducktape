//! The left gutter: the `+` insert button and the drag handle, plus the
//! drag-to-reorder machinery they drive.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu};
use gpui_kit::component::{ActiveTheme, Sizable as _};

use gpui_kit::{
    AnyElement, App, AppContext as _, Context, DragMoveEvent, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Styled as _, Window, div, px,
};


use super::block::BlockId;
use super::actions;
use super::block::BlockRegistry;
use super::theme::ActiveEditorTheme;
use super::ui::Lucide;
use super::view::{Caret, NotionEditor, group_name};

/// The payload carried while dragging a block.
#[derive(Clone, Debug)]
pub struct DraggedBlock {
    pub id: BlockId,
}

/// What the pointer carries during a drag.
pub struct DragPreview {
    pub text: String,
}

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.editor_theme();
        div()
            .px(theme.rems(0.625))
            .py(theme.rems(0.25))
            .rounded(theme.radius)
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .text_size(theme.ui_text_size)
            .text_color(cx.theme().foreground)
            .shadow_md()
            .child(if self.text.is_empty() {
                "Empty block".to_string()
            } else {
                self.text.chars().take(48).collect::<String>()
            })
    }
}

/// Where a dragged block would land.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DropTarget {
    pub index: usize,
    pub below: bool,
}

impl NotionEditor {
    /// The `+` and drag handle, shown while the pointer is over the block.
    pub(crate) fn render_gutter(
        &self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let block = &self.blocks[ix];
        let id = block.id;
        let text = block.text.clone();
        let layout = self.layout_at(ix, cx);
        let label = BlockRegistry::global(cx)
            .get(&block.ty)
            .label(&block.attrs);
        let focus = self.focus_handle_for_editor();
        let is_table = BlockRegistry::global(cx).get(&block.ty).caps().grid;
        // Centre the controls on the block's first line, using the line
        // height the input laid out with rather than the asked-for one.
        let line_height = block
            .state
            .read(cx)
            .line_height()
            .unwrap_or_else(|| layout.line_height_px());
        // The controls are as tall as a marker slot and centred on the first
        // line of text, whatever size that line turned out to be.
        let theme = cx.editor_theme().clone();
        let controls_height = theme.marker_width;
        let top = (line_height - controls_height).max(px(0.)) / 2.;

        let mut controls = div()
            .absolute()
            // The controls track the block's indentation, so they stay the
            // same distance from its text however deeply it is nested.
            .left(theme.rems(0.25) + theme.indent_width * block.indent() as f32)
            .top(top)
            .h(controls_height)
            .flex()
            .items_center()
            .gap(theme.rems(0.125));
        // The controls belong to the block whose menu is open until it closes.
        // Hover alone would take them away the moment the pointer reached the
        // menu, leaving an open menu with nothing on screen that opened it.
        let holds_the_menu = self.gutter_menu == Some(id);
        if !self.always_show_gutter && !holds_the_menu {
            controls = controls
                .invisible()
                .group_hover(group_name(id), |this| this.visible());
        }

        controls
            .child(
                Button::new(("insert", id.0 as usize))
                    .ghost()
                    .xsmall()
                    .icon(Lucide("plus"))
                    .tooltip("Insert block")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.insert_block_below(id, window, cx)
                    })),
            )
            .child(
                draggable(
                    Button::new(("drag", id.0 as usize))
                        .ghost()
                        .xsmall()
                        .icon(Lucide("grip-vertical"))
                        .tooltip("Click for options, hold for drag")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.select_block_as_node(id, window, cx)
                        })),
                    id,
                    text,
                )
                .dropdown_menu(move |menu, window, cx| {
                    block_menu(id, label.clone(), focus.clone(), is_table, menu, window, cx)
                })
                .on_open_change(cx.listener(move |this, open: &bool, _, cx| {
                    this.gutter_menu = open.then_some(id);
                    cx.notify();
                })),
            )
            .into_any_element()
    }

    /// Insert an empty paragraph under `id` and put the caret in it, the way
    /// the template's `+` button does.
    pub fn insert_block_below(&mut self, id: BlockId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.index_of(id) else { return };
        let new_id = self.insert_block(ix + 1, super::block::BlockContent::paragraph(""), window, cx);
        self.focus_block(new_id, Caret::Start, window, cx);
        self.open_slash_menu(window, cx);
    }

    /// Select a block as a node, as clicking the drag handle does.
    pub fn select_block_as_node(
        &mut self,
        id: BlockId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = vec![id];
        self.take_focus_from_blocks(window, cx);
        cx.notify();
    }

    /// Track where a dragged block would land.
    pub(crate) fn on_drag_over(
        &mut self,
        ix: usize,
        event: &DragMoveEvent<DraggedBlock>,
        cx: &mut Context<Self>,
    ) {
        // The listener fires on every block that has one, so the pointer
        // position decides which row is actually being dragged over.
        let bounds = event.bounds;
        let position = event.event.position;
        if !bounds.contains(&position) {
            return;
        }
        let below = position.y > bounds.center().y;
        let target = DropTarget { index: ix, below };
        if self.drop_target != Some(target) {
            self.drop_target = Some(target);
            cx.notify();
        }
    }

    /// Complete a drag, moving the block to the tracked position.
    pub(crate) fn on_drop_block(&mut self, dragged: &DraggedBlock, cx: &mut Context<Self>) {
        let Some(target) = self.drop_target.take() else {
            return;
        };
        let to = if target.below {
            target.index + 1
        } else {
            target.index
        };

        // Dragging one of several selected blocks moves the whole selection,
        // which is what having selected them was for.
        let moving: Vec<BlockId> = if self.selected_blocks().contains(&dragged.id) {
            self.selected_blocks()
        } else {
            vec![dragged.id]
        };
        self.reorder_blocks(&moving, to, cx);
        cx.notify();
    }

    /// The drop target, for tests that check a drag cleaned up after itself.
    pub fn drop_target_for_test(&self) -> Option<DropTarget> {
        self.drop_target
    }

    /// The block holding an open gutter menu, for tests that check the
    /// controls outlive the pointer.
    pub fn gutter_menu_for_test(&self) -> Option<BlockId> {
        self.gutter_menu
    }

    pub(crate) fn drop_indicator(&self, ix: usize, cx: &App) -> Option<impl IntoElement> {
        let target = self.drop_target?;
        if target.index != ix {
            return None;
        }
        // The row reaches into the gutter and is padded back out of it, and
        // an absolutely positioned child is placed against the border edge —
        // so the line starts where the block's own content starts.
        let indent = self.blocks.get(ix).map(|block| block.indent()).unwrap_or(0);
        let left = cx.editor_theme().gutter_controls_width
            + cx.editor_theme().indent_width * indent as f32;
        Some(
            div()
                .absolute()
                .left(left)
                .right(px(0.))
                .when_else(
                    target.below,
                    |this| this.bottom(px(-1.)),
                    |this| this.top(px(-1.)),
                )
                .h(cx.editor_theme().rems(0.125))
                .rounded(cx.editor_theme().radius_sm)
                .bg(cx.theme().primary),
        )
    }
}

/// `FluentBuilder::when` with both branches, kept local to this module.
trait WhenElse: Sized {
    fn when_else(self, condition: bool, then: impl FnOnce(Self) -> Self, other: impl FnOnce(Self) -> Self) -> Self {
        if condition { then(self) } else { other(self) }
    }
}

impl<T: Sized> WhenElse for T {}

/// The block options menu, opened from the drag handle.
fn block_menu(
    id: BlockId,
    label: gpui_kit::SharedString,
    focus: gpui_kit::FocusHandle,
    is_table: bool,
    menu: PopupMenu,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let _ = id;
    let turn_into_focus = focus.clone();
    let color_focus = focus.clone();
    let table_focus = focus.clone();
    let mut menu = menu.action_context(focus).label(label);
    if is_table {
        menu = menu.submenu("Table", window, cx, {
            let focus = table_focus.clone();
            move |menu, _, _| {
                menu.action_context(focus.clone())
                    .menu("Insert row above", Box::new(actions::InsertRowAbove))
                    .menu("Insert row below", Box::new(actions::InsertRowBelow))
                    .menu("Insert column left", Box::new(actions::InsertColumnLeft))
                    .menu("Insert column right", Box::new(actions::InsertColumnRight))
                    .separator()
                    .menu("Delete row", Box::new(actions::DeleteRow))
                    .menu("Delete column", Box::new(actions::DeleteColumn))
            }
        });
    }
    menu
        .menu("Comment", Box::new(actions::AddComment))
        .submenu("Color", window, cx, {
            let focus = color_focus.clone();
            move |menu, _, _| color_menu(focus.clone(), menu)
        })
        .submenu("Turn into", window, cx, {
            let focus = turn_into_focus.clone();
            move |menu, _, _| {
                menu.action_context(focus.clone())
                    .menu("Text", Box::new(actions::SetParagraph))
                    .menu("Heading 1", Box::new(actions::SetHeading1))
                    .menu("Heading 2", Box::new(actions::SetHeading2))
                    .menu("Heading 3", Box::new(actions::SetHeading3))
                    .menu("Bulleted list", Box::new(actions::ToggleBulletList))
                    .menu("Numbered list", Box::new(actions::ToggleOrderedList))
                    .menu("To-do list", Box::new(actions::ToggleTaskList))
                    .menu("Blockquote", Box::new(actions::ToggleBlockquote))
                    .menu("Code block", Box::new(actions::ToggleCodeBlock))
            }
        })
        .menu("Reset formatting", Box::new(actions::ClearMarks))
        .separator()
        .menu("Duplicate", Box::new(actions::DuplicateBlock))
        .menu("Copy to clipboard", Box::new(actions::CopyBlock))
        .separator()
        .menu("Move up", Box::new(actions::MoveBlockUp))
        .menu("Move down", Box::new(actions::MoveBlockDown))
        .separator()
        .menu("Delete", Box::new(actions::DeleteBlock))
}

/// Give the drag handle its drag, through the imperative `Interactivity` API:
/// the styled `Button` is interactive but not stateful-interactive, so the
/// builder-style `on_drag` is not available on it.
fn draggable(mut button: Button, id: BlockId, text: String) -> Button {
    button
        .interactivity()
        .on_drag(DraggedBlock { id }, move |_, _, _, cx| {
            cx.new(|_| DragPreview { text: text.clone() })
        });
    button
}

/// The text and highlight palettes, shared with the selection toolbar.
fn color_menu(focus: gpui_kit::FocusHandle, menu: PopupMenu) -> PopupMenu {
    use super::mark::{HighlightColor, TextColor};

    let mut menu = menu.action_context(focus).label("Text color");
    for color in TextColor::ALL {
        menu = menu.menu(color.label(), Box::new(actions::ApplyColor::Text(color)));
    }
    menu = menu.separator().label("Highlight color");
    for color in HighlightColor::ALL {
        menu = menu.menu(color.label(), Box::new(actions::ApplyColor::Highlight(color)));
    }
    menu
}
