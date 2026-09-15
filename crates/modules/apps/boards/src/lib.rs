//! Shared workspace canvases. Geometry is integer world coordinates; camera,
//! selection and unfinished gestures stay on the editing device.
mod interface;
pub use interface::*;
#[cfg(feature = "native")]
mod module;
#[cfg(feature = "native")]
pub use module::Boards;
#[cfg(feature = "guest")]
mod guest;
