//! Pages owns its whole screen: the page list, the document editor, Markdown
//! presentation, selection and undo history, the comment rail, and every read
//! and write behind them. It speaks the kernel contract — `rpc.view` and
//! `rpc.live` for the workspace, `op.submit` for the pages module's own ops —
//! so the desktop app carries no pages code at all.

#[path = "editor_indent.rs"]
pub mod indent;
#[path = "editor_inline.rs"]
pub mod inline;

pub mod document_sync;
pub mod editor;
pub mod editor_binding;
pub mod rich_document;
#[path = "editor_format.rs"]
pub mod format;
pub mod editor_menu;
pub mod editor_view;
pub mod host;
#[path = "editor_markdown.rs"]
pub mod markdown;
#[path = "editor_presentation.rs"]
pub mod presentation;

include!("ui/view.rs");

ducktape_view_guest::export_app!(
    PagesView,
    "Pages",
    "The workspace's pages: the sidebar, the document header, the tab strip and the comments rail.",
    ["pages"]
);
