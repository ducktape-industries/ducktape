//! A Notion-like block editor for GPUI, built on `gpui-kit`.
//!
//! The document is a flat list of [`block::Block`]s, each backed by its own
//! `EditorState`; inline formatting lives in [`mark::MarkList`] and is rendered
//! through the input's decoration layer. Node types are registered
//! [`block::BlockSpec`]s, so an application can add its own. The command
//! surface in [`commands`] mirrors Tiptap's, so `toggle_bold`, `set_heading`
//! and `toggle_bullet_list` mean what they mean there.

pub mod actions;
pub mod block;
pub mod blocks;
pub mod commands;
pub mod comments;
pub mod fit;
pub mod grid;
pub mod gutter;
pub mod history;
pub mod input_rules;
pub mod keymap;
pub mod mark;
pub mod selection;
pub mod slash;
pub mod suggestion;
pub mod theme;
pub mod toolbar;
pub mod ui;
pub mod view;

pub use block::{
    Block, BlockAttrs, BlockContent, BlockId, BlockRegistry, BlockSpec, BlockType, types,
};
pub use comments::{Comment, Thread, ThreadId};
pub use fit::InputFit;
pub use grid::{Cell, CellGrid, CellPosition, table_content};
pub use mark::{HighlightColor, Mark, MarkKind, MarkList, TextColor};
pub use theme::{ActiveEditorTheme, EditorTheme, HeadingStyle, HighlightPalette, TextPalette};
pub use view::{DocumentChanged, LinkPressed, NotionEditor};

/// Initialise the editor: tokens, node types, actions and key bindings.
///
/// The tokens are derived from the `gpui-kit` theme in force and derived
/// again whenever it changes, so an application only has to set its theme.
/// To change what the editor itself looks like, see [`EditorTheme::customize`].
pub fn init(cx: &mut gpui_kit::App) {
    EditorTheme::init(cx);
    cx.observe_global::<gpui_kit::component::Theme>(EditorTheme::refresh)
        .detach();
    blocks::init(cx);
    actions::init(cx);
    input_rules::init(cx);
}
