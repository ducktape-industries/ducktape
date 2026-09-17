//! The floating toolbar shown over a text selection, and the popovers it
//! opens: turn into, link and color.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::DropdownMenu as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Disableable as _, Selectable as _, Sizable as _, h_flex};
use gpui_kit::{
    Anchor, AnyElement, App, AppContext as _, Context, Entity, FocusHandle, IntoElement,
    ParentElement as _, Pixels, Point, SharedString, Styled as _, Window, deferred, div, px,
};

use super::actions;
use super::block::{BlockId, BlockRegistry};
use super::mark::{HighlightColor, MarkKind, TextColor};
use super::theme::ActiveEditorTheme;
use super::ui::{self, Lucide};

/// Stacking order of the editor's own overlays.
pub(crate) const OVERLAY_PRIORITY: usize = 100;

/// A toolbar item owned by an embedding application's interaction model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolbarItem {
    pub tag: SharedString,
    pub label: SharedString,
}
/// The host returns a selected tag without applying an editing policy.
pub struct ToolbarAction {
    pub tag: SharedString,
}

/// The link editor opened from the toolbar.
pub struct LinkEditor {
    pub block: BlockId,
    pub range: std::ops::Range<usize>,
    pub input: Entity<InputState>,
    pub _subscription: gpui_kit::Subscription,
}

impl super::view::NotionEditor {
    /// Replace the toolbar. None selects the standalone editor's toolbar.
    pub fn set_toolbar(&mut self, items: Option<Vec<ToolbarItem>>, cx: &mut Context<Self>) {
        if self.toolbar == items {
            return;
        }
        self.toolbar = items;
        cx.notify();
    }
    pub fn choose_toolbar_action(&mut self, tag: &str, cx: &mut Context<Self>) {
        let Some(item) = self
            .toolbar
            .as_ref()
            .and_then(|items| items.iter().find(|item| item.tag.as_ref() == tag))
        else {
            return;
        };
        cx.emit(ToolbarAction {
            tag: item.tag.clone(),
        });
    }

    /// Whether the selection toolbar should be on screen.
    pub fn selection_toolbar_visible(&self, cx: &App) -> bool {
        if self.toolbar.as_ref().is_some_and(Vec::is_empty) {
            return false;
        }
        if self.suggestion_is_open() || self.drop_target.is_some() {
            return false;
        }
        // A comment box or a link card owns the selection while it is open;
        // two surfaces over one range is one too many, and they hang at the
        // same point.
        if self.comment_draft_is_open() || self.link_editor_is_open() {
            return false;
        }
        // While the button is still down the selection is still being made;
        // the toolbar waits for it to be let go rather than flickering along
        // with the pointer.
        if self.press_in_progress() {
            return false;
        }
        if self.has_block_selection() {
            return true;
        }
        // A selection belongs to the block that holds the caret; with the
        // caret elsewhere — a table cell, another window — there is nothing
        // for the toolbar to act on.
        if self.focused_id().is_none() {
            return false;
        }
        let Some((id, range)) = self.selection(cx) else {
            return false;
        };
        if range.is_empty() || !self.block_is_on_screen(id) {
            return false;
        }
        self.index_of(id)
            .map(|ix| BlockRegistry::global(cx).get(&self.blocks[ix].ty).caps().marks)
            .unwrap_or(false)
    }

    /// The toolbar, anchored above the selection.
    pub(crate) fn render_selection_toolbar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.selection_toolbar_visible(cx) {
            return None;
        }
        let position = self.toolbar_anchor(cx)?;

        if let Some(items) = &self.toolbar {
            let toolbar = ui::popover_surface(cx)
                .flex()
                .flex_row()
                .flex_wrap()
                .max_w(window.viewport_size().width - cx.editor_theme().rems(2.))
                .items_center()
                .gap(cx.editor_theme().rems(0.125))
                .p(cx.editor_theme().rems(0.25))
                .children(items.iter().enumerate().map(|(index, item)| {
                    let tag = item.tag.clone();
                    Button::new(("guest-toolbar", index))
                        .ghost()
                        .small()
                        .label(item.label.clone())
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.choose_toolbar_action(&tag, cx)),
                        )
                }));
            return Some(
                deferred(
                    gpui_kit::base::Positioner::corner(Anchor::BottomLeft, position)
                        .margin(cx.editor_theme().rems(0.5))
                        .occlude()
                        .child(toolbar),
                )
                .with_priority(OVERLAY_PRIORITY)
                .into_any_element(),
            );
        }
        let focus = self.focus_handle_for_editor();
        let code_active = self.is_mark_active(&MarkKind::Code, cx);

        let toolbar = ui::popover_surface(cx)
            .flex()
            .flex_row()
            .items_center()
            .gap(cx.editor_theme().rems(0.125))
            .p(cx.editor_theme().rems(0.25))
            .child(self.render_turn_into(&focus, cx))
            .child(separator(cx))
            .child(self.mark_button("bold", "Bold", MarkKind::Bold, cx))
            .child(self.mark_button("italic", "Italic", MarkKind::Italic, cx))
            .child(self.mark_button("underline", "Underline", MarkKind::Underline, cx))
            .child(self.mark_button(
                "strikethrough",
                "Strikethrough",
                MarkKind::Strike,
                cx,
            ))
            .child(self.mark_button("code", "Code", MarkKind::Code, cx))
            .child(separator(cx))
            .child(
                Button::new("comment")
                    .ghost()
                    .small()
                    .icon(ui::Lucide("message-square-plus"))
                    .tooltip("Comment")
                    .on_click(cx.listener(|this, _, window, cx| this.add_comment(window, cx))),
            )
            .child(self.render_link_button(cx))
            .child(self.render_color_menu(&focus, cx))
            .when_not(code_active, |this| {
                this.child(separator(cx))
                    .child(self.render_more_menu(&focus, cx))
            });

        Some(
            deferred(
                gpui_kit::base::Positioner::corner(Anchor::BottomLeft, position)
                    .margin(cx.editor_theme().rems(0.5))
                    .occlude()
                    .child(toolbar),
            )
            .with_priority(OVERLAY_PRIORITY)
            .into_any_element(),
        )
    }

    /// Where the toolbar hangs, for tests that check it lines up with what it
    /// acts on.
    pub fn toolbar_anchor_for_test(&self, cx: &App) -> Option<Point<Pixels>> {
        self.toolbar_anchor(cx)
    }

    /// Where the toolbar hangs: over the selected text, or over the first
    /// block when whole blocks are selected.
    fn toolbar_anchor(&self, cx: &App) -> Option<Point<Pixels>> {
        if self.has_block_selection() {
            // Ask the block where its text is; the block's own box is inset
            // from the glyphs by whatever the input keeps for itself.
            let id = self.selected_blocks().first().copied()?;
            let origin = self
                .block_text_origin(id, cx)
                .or_else(|| self.block_bounds(id).map(|bounds| bounds.origin))?;
            return Some(origin + Point::new(px(0.), -cx.editor_theme().rems(0.5)));
        }
        let (id, range) = self.selection(cx)?;
        let ix = self.index_of(id)?;
        let bounds = self.blocks[ix].state.read(cx).range_to_bounds(&range)?;
        Some(bounds.origin + Point::new(px(0.), -cx.editor_theme().rems(0.5)))
    }

    fn mark_button(
        &self,
        icon: &'static str,
        label: &'static str,
        kind: MarkKind,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.is_mark_active(&kind, cx);
        Button::new(SharedString::from(format!("mark-{icon}")))
            .ghost()
            .small()
            .icon(Lucide(icon))
            .tooltip(label)
            .selected(active)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.toggle_mark(kind.clone(), window, cx);
            }))
    }

    /// Tiptap's "Turn into" dropdown, listing the convertible node types.
    fn render_turn_into(&self, focus: &FocusHandle, cx: &mut Context<Self>) -> impl IntoElement {
        let label = self.active_block_label(cx);
        let focus = focus.clone();
        Button::new("turn-into")
            .ghost()
            .small()
            .label(label)
            .icon(Lucide("chevron-down"))
            .dropdown_menu(move |menu, _window, _cx| {
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
            })
    }

    /// Label of the active block's node type, for the turn-into trigger.
    pub fn active_block_label(&self, cx: &App) -> SharedString {
        let Some(ix) = self.active_index() else {
            return "Text".into();
        };
        let block = &self.blocks[ix];
        BlockRegistry::global(cx)
            .get(&block.ty)
            .label(&block.attrs)
    }

    fn render_more_menu(&self, focus: &FocusHandle, _cx: &mut Context<Self>) -> impl IntoElement {
        let focus = focus.clone();
        Button::new("more-marks")
            .ghost()
            .small()
            .icon(Lucide("ellipsis"))
            .tooltip("More formatting")
            .dropdown_menu(move |menu, _window, _cx| {
                menu.action_context(focus.clone())
                    .menu("Superscript", Box::new(actions::ToggleSuperscript))
                    .menu("Subscript", Box::new(actions::ToggleSubscript))
                    .separator()
                    .menu("Reset formatting", Box::new(actions::ClearMarks))
            })
    }

    fn render_link_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.link_at_caret(cx).is_some();
        Button::new("link")
            .ghost()
            .small()
            .icon(Lucide("link"))
            .tooltip("Link")
            .selected(active)
            .on_click(cx.listener(|this, _, window, cx| this.open_link_editor(window, cx)))
    }

    /// Text and highlight swatches, as the template's color popover.
    fn render_color_menu(&self, focus: &FocusHandle, _cx: &mut Context<Self>) -> impl IntoElement {
        let focus = focus.clone();
        Button::new("color")
            .ghost()
            .small()
            .icon(Lucide("palette"))
            .tooltip("Color")
            .dropdown_menu(move |menu, _window, _cx| {
                let mut menu = menu.action_context(focus.clone()).label("Text color");
                for color in TextColor::ALL {
                    menu = menu.menu_element(Box::new(actions::ApplyColor::Text(color)), move |_, cx| {
                        swatch_row(
                            color.label(),
                            cx.editor_theme()
                                .text_color(color)
                                .unwrap_or(cx.theme().foreground),
                            cx,
                        )
                    });
                }
                menu = menu.separator().label("Highlight color");
                for color in HighlightColor::ALL {
                    menu = menu
                        .menu_element(Box::new(actions::ApplyColor::Highlight(color)), move |_, cx| {
                            swatch_row(
                                color.label(),
                                cx.editor_theme().highlight_fill(Some(color)),
                                cx,
                            )
                        });
                }
                menu
            })
    }

    // ------------------------------------------------------------ link editor

    /// Open the link editor over the selection, seeded with any existing href.
    pub fn open_link_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, selection)) = self.selection(cx) else {
            return;
        };
        let existing = self.link_at_caret(cx);
        let range = existing
            .as_ref()
            .map(|(range, _)| range.clone())
            .unwrap_or(selection);
        if range.is_empty() {
            return;
        }
        let href = existing.map(|(_, href)| href).unwrap_or_default();

        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Paste a link...")
                .default_value(href)
        });
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.apply_link_editor(window, cx);
                }
            },
        );
        input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });

        self.link_editor = Some(LinkEditor {
            block: id,
            range,
            input,
            _subscription: subscription,
        });
        cx.notify();
    }

    pub fn close_link_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.link_editor.take() else {
            return;
        };
        // Escape returns focus to the text it was opened from.
        self.focus_block(editor.block, super::view::Caret::At(editor.range.end), window, cx);
        cx.notify();
    }

    pub fn apply_link_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.link_editor.take() else {
            return;
        };
        let href = editor.input.read(cx).value().to_string();
        let Some(ix) = self.index_of(editor.block) else {
            return;
        };
        // Setting a link is an edit like any other mark: undo must reach it,
        // and a host must be told, or the link lives only until the document
        // is next installed from the text the host kept.
        self.record(super::history::Step::Structural, cx);
        if href.trim().is_empty() {
            self.blocks[ix]
                .marks
                .remove(&MarkKind::Link(SharedString::default()), &editor.range);
        } else {
            self.blocks[ix]
                .marks
                .add(MarkKind::Link(normalize_href(&href)), editor.range.clone());
        }
        self.apply_decorations(editor.block, cx);
        self.focus_block(
            editor.block,
            super::view::Caret::At(editor.range.end),
            window,
            cx,
        );
        cx.emit(super::view::DocumentChanged);
        cx.notify();
    }

    pub fn link_editor_is_open(&self) -> bool {
        self.link_editor.is_some()
    }

    pub(crate) fn render_link_editor(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let editor = self.link_editor.as_ref()?;
        let ix = self.index_of(editor.block)?;
        let bounds = self.blocks[ix]
            .state
            .read(cx)
            .range_to_bounds(&editor.range)?;
        let theme = cx.editor_theme().clone();
        let position = bounds.origin + Point::new(px(0.), -theme.rems(0.5));
        let has_link = !editor.input.read(cx).value().is_empty();

        let card = ui::popover_surface(cx)
            .flex()
            .flex_row()
            .items_center()
            .gap(theme.rems(0.25))
            .p(theme.rems(0.25))
            .child(
                Input::new(&editor.input)
                    .id("link-input")
                    .w(theme.rems(16.25)),
            )
            .child(
                Button::new("apply-link")
                    .ghost()
                    .small()
                    .icon(Lucide("corner-down-left"))
                    .tooltip("Apply link")
                    .disabled(!has_link)
                    .on_click(cx.listener(|this, _, window, cx| this.apply_link_editor(window, cx))),
            )
            .child(separator(cx))
            .child(
                Button::new("remove-link")
                    .ghost()
                    .small()
                    .icon(Lucide("unlink"))
                    .tooltip("Remove link")
                    .on_click(cx.listener(|this, _, window, cx| {
                        if let Some(editor) = this.link_editor.as_ref() {
                            let (block, range) = (editor.block, editor.range.clone());
                            if let Some(ix) = this.index_of(block) {
                                this.blocks[ix]
                                    .marks
                                    .remove(&MarkKind::Link(SharedString::default()), &range);
                                this.apply_decorations(block, cx);
                            }
                        }
                        this.close_link_editor(window, cx);
                    })),
            );

        Some(
            deferred(
                gpui_kit::base::Positioner::corner(Anchor::BottomLeft, position)
                    .margin(theme.rems(0.5))
                    .occlude()
                    .child(card),
            )
            .with_priority(OVERLAY_PRIORITY + 1)
            .into_any_element(),
        )
    }
}

fn separator(cx: &App) -> impl IntoElement {
    let theme = cx.editor_theme();
    div()
        // A hairline is a device boundary, not a spacing value: it stays one
        // pixel however the document is scaled.
        .w(px(1.))
        .h(theme.rems(1.25))
        .mx(theme.rems(0.125))
        .bg(cx.theme().border)
}

/// A palette entry: the color, then its name.
fn swatch_row(label: &'static str, color: gpui_kit::Hsla, cx: &App) -> gpui_kit::Div {
    let theme = cx.editor_theme();
    h_flex()
        .gap(theme.rems(0.5))
        .child(
            div()
                .size(theme.rems(0.875))
                .rounded(theme.radius_sm)
                .bg(color),
        )
        .child(label)
}

/// Give a bare host name a scheme, the way the template's link input does.
pub fn normalize_href(href: &str) -> SharedString {
    let href = href.trim();
    if href.contains("://") || href.starts_with("mailto:") || href.starts_with('#') {
        return href.to_string().into();
    }
    format!("https://{href}").into()
}

/// `when_not`, the inverse of `FluentBuilder::when`, kept local.
trait WhenNot: Sized {
    fn when_not(self, condition: bool, then: impl FnOnce(Self) -> Self) -> Self {
        if condition { self } else { then(self) }
    }
}

impl<T: Sized> WhenNot for T {}

