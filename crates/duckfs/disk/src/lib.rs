//! native disk persistence for duckfs.

mod commit;
mod disk;
mod scratch;

pub use commit::{commit_refs, gc_due, persist_objects};
pub use disk::{DiskRefs, DiskStore};
pub use scratch::SyncScratch;
