//! Chat as a module-owned view: the channel sidebar, the message stream,
//! the thread rail and the channel-details drawer, from the facts the
//! desktop app pushes. Every act — a room, a reaction, an edit, a search —
//! leaves as an intent the app signs; the two composers are the app's own
//! surfaces, left as slots.

pub mod host;

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
