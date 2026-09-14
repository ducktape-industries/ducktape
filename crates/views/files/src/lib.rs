//! Files as a module-owned view on the kernel contract: the duckfs browser —
//! one directory at a time, its preview, its history and the write bar.
//!
//! The kernel pushes session facts only (`files.props`: connected, dark, and
//! the chain a draft belongs to). The view lists the directory, reads the
//! preview, walks the snapshot history and diffs a snapshot for itself through
//! `files.get`, re-reads on every files block (`rpc.live`), and every write
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
    "The duckfs browser: one directory at a time, its preview, its history and its writes.",
    ["files"]
);
