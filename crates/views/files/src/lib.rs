//! Files as a module-owned view on the kernel contract: a Finder-style
//! browser over duckfs — a sidebar of places, a toolbar with the trail and
//! the path bar, the directory as a sortable list or as Miller columns, an
//! inspector over the chosen entry, and a status bar.
//!
//! The kernel pushes session facts only (`files.props`: connected, dark, the
//! chain a draft belongs to, and the reader's account). The view lists the
//! directory, the homes and the snapshot history, reads the preview, asks
//! which snapshot last touched a path and diffs a snapshot for itself through
//! generic `rpc.query`, re-reads on every files block (`rpc.live`), and every write
//! leaves as `op.submit` carrying the duckfs commit the kernel signs with the
//! seated key. The pictures, the highlighted reader and the Markdown document
//! are the app's own surfaces, painted into the slots the view leaves for them;
//! a link the reader activates and a file dropped on the window are the app's
//! doors, and those two alone still cross as intents.

pub mod host;

#[path = "ui/app.rs"]
mod app;
pub use app::{FilesView, Message, browse};

ducktape_view_guest::export_app!(
    FilesView,
    "Files",
    "The duckfs browser: places, the trail, the directory as rows or columns, and the chosen entry.",
    ["files"]
);
