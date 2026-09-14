//! Forge as a module-owned view: the repo overview, one repo's code, pull
//! requests and issues, and an item's merge box, reviews and discussion,
//! all read off the node through the kernel contract. Opening a repo, an
//! item, a directory or a file is this view's own state and its own read; a
//! review and a merge leave as `op.submit`, signed by the kernel with the
//! seated key. The app holds no forge state at all.

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
