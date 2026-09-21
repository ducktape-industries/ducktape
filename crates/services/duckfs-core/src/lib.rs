//! deterministic duckfs core.
//!
//! This crate owns the reusable, sdk-free state machine and wire surface:
//! canonical codecs, object ids, path rules, refs/root calculation, `Fs<S>`,
//! queries, GC, store traits, and in-memory stores.

mod wire;
pub use wire::*;

mod actor;
mod codec;
pub use actor::{Actor, Authority};

pub mod fs;
pub mod gc;
pub mod objects;
pub mod paths;
pub mod queries;
mod retention;
pub mod state;
pub mod store;
mod tree;

#[doc(hidden)]
pub mod testkit;

pub use fs::{Fs, Notification, StagedObjects};
pub use objects::{Kind, ObjectId};
pub use state::{
    PinEntry, Refs, Staged, decode_block_objects, decode_refs, encode_block_objects, encode_refs,
    root_bytes,
};
pub use store::{MemRefs, MemStore, ObjectStore, RefsStore};
