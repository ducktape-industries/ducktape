//! Shared workspace canvases. Geometry is integer world coordinates; camera,
//! selection and unfinished gestures stay on the editing device.
// The format itself is a types-only crate, so a view can link it without the
// module behind it (`boards-wire`, the `chat-message` pattern). Re-exported
// here because this crate IS the board to everything that runs one.
pub use boards_wire::*;
#[cfg(feature = "native")]
mod module;
#[cfg(feature = "native")]
pub use module::Boards;
#[cfg(feature = "guest")]
mod guest;
