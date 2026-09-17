//! Forge as a module-owned view: the repo overview, one repo's code, pull
//! requests and issues, and an item's merge box, reviews and discussion,
//! all read off the node through the kernel contract. Opening a repo, an
//! item, a directory or a file is this view's own state and its own read;
//! opening an issue, a review and a merge leave as `op.submit`, signed by
//! the kernel with the seated key. The app holds no forge state at all.

/// The tracker filter's key, named once: the view draws the field with it
/// and `/` asks the host to focus exactly that.
pub(crate) const TRACKER_FILTER_KEY: &str = "ForgeView/forge/tracker-filter";

/// The page column's key. Escape and `/` reach a view only while something
/// inside it holds the window's focus, so every navigation inside forge
/// asks for this one — a field the reader clicked into keeps the focus,
/// because nothing takes it back until the screen moves again.
pub(crate) const PAGE_KEY: &str = "ForgeView/page";

pub mod blocks;
pub mod host;

#[path = "ui/app.rs"]
mod app;
pub(crate) use app::*;
pub use app::{ForgeView, Message};

ducktape_view_guest::export_app!(
    ForgeView,
    "Forge",
    "This workspace's repositories: their code, pull requests and issues, with reviews and merges.",
    ["forge"]
);
