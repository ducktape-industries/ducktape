//! Guest-owned conversations, composers, and module operations.

pub mod host;
mod live;
mod hydration;
pub mod notice;

#[path = "ui/app.rs"]
mod app;
pub(crate) use app::*;
pub use app::{ChatView, CopySurface, Message};

ducktape_view_guest::export_app!(
    ChatView,
    "Chat",
    "Channels, direct messages, threads and the live huddle of this workspace.",
    ["chat"]
);
