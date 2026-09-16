//! Shared presentation helpers: icons, popover surfaces, toolbar buttons.

use gpui_kit::component::{ActiveTheme, ThemeStyled as _};
use gpui_kit::{App, Div, InteractiveElement as _, IntoElement, SharedString, Styled as _, div};

use super::theme::ActiveEditorTheme;

/// An icon from the bundled Lucide set, named the way the Tiptap template
/// names them (`heading-1`, `list-ordered`, …).
#[derive(Clone, Copy)]
pub struct Lucide(pub &'static str);

impl gpui_kit::assets::IconNamed for Lucide {
    fn path(self) -> SharedString {
        format!("icons/{}.svg", self.0).into()
    }
}

pub fn icon(name: &'static str, size: gpui_kit::Pixels, color: gpui_kit::Hsla) -> impl IntoElement {
    gpui_kit::svg()
        .path(Lucide(name).path_string())
        .size(size)
        .flex_none()
        .text_color(color)
}

impl Lucide {
    fn path_string(self) -> SharedString {
        format!("icons/{}.svg", self.0).into()
    }
}

/// The popover surface shared by the slash menu, toolbar and link editor.
///
/// This is the framework's own popup treatment — the one Select, Combobox and
/// the menus use — so the editor's surfaces cannot drift away from the ones
/// beside them.
pub fn popover_surface(cx: &App) -> Div {
    div().popover_style(cx).p(cx.editor_theme().rems(0.25))
}

/// A row in a menu: fixed height, hover fill, rounded.
pub fn menu_row(selected: bool, cx: &App) -> Div {
    let theme = cx.editor_theme();
    let row = div()
        .h(theme.rems(2.))
        .px(theme.rems(0.5))
        .flex()
        .items_center()
        .gap(theme.rems(0.5))
        .rounded(theme.radius)
        .cursor_pointer()
        .text_size(theme.ui_text_size);
    if selected {
        row.bg(cx.theme().accent).text_color(cx.theme().accent_foreground)
    } else {
        row.hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
    }
}

/// A toolbar button: 2rem square, ghost by default, filled when active.
pub fn toolbar_button(id: impl Into<gpui_kit::ElementId>, active: bool, cx: &App) -> gpui_kit::Stateful<Div> {
    let theme = cx.editor_theme();
    let button = div()
        .id(id)
        .size(theme.rems(2.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme.radius)
        .cursor_pointer();
    if active {
        button
            .bg(cx.theme().accent)
            .text_color(cx.theme().accent_foreground)
    } else {
        button.hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
    }
}
