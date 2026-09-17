//! The runs module's wire surface: the messages and queries the module answers,
//! the records it stores, the action catalog a program's operations decode to,
//! and the programs the module runs. Types and their codecs, no module — so a
//! view or the daemon links THIS and not the 20,000-line module behind it.
//!
//! It names `sdk` (origins, ids) and `agent-wire` (a program IS an agent
//! program), because its types are made of theirs. Both are SDK-shaped: no
//! `native` feature, no kernel host, no disk.

pub mod catalog;
mod conversation_interface;
mod ids;
mod interface;
mod model;
pub mod view;
mod workflow;

pub use catalog::*;
pub use conversation_interface::*;
pub use ids::*;
pub use interface::*;
pub use model::*;
pub use view::*;
pub use workflow::{conversation_program, model_program};

/// the reply-block kinds normalization keeps — the closed vocabulary the
/// strict-output instruction names. The module shapes a model's raw text into
/// these; the catalog describes them to a caller, so the names are wire.
pub const REPLY_KIND_PARAGRAPH: &str = "paragraph";
pub const REPLY_KIND_CODE: &str = "code";

/// mirror of `forge::MAX_TITLE_BYTES` (conformance-pinned): an OpenPr whose
/// title exceeds it would REJECT — and a rejected follow-up aborts the
/// delivery block, so the derivation must clamp below it (100 4-byte chars
/// would be 400 bytes).
pub const FORGE_TITLE_BYTE_CAP: usize = 256;

/// mirror of `forge::MAX_BODY_BYTES` (conformance-pinned) — same no-abort
/// reasoning as the title cap. unreachable under the 32 KiB reply-blocks cap;
/// kept as a deterministic guard.
pub const FORGE_BODY_BYTE_CAP: usize = 64 * 1024;

/// The longest title `pages` accepts, which the catalog publishes in the
/// `pages.post` schema so a program hears the limit before it writes.
///
/// A COPY, deliberately. `pages` depends on `files` with its `native` feature,
/// so naming `pages` here would put duckfs-disk and `std::fs` into every
/// consumer of this crate — the whole graph a wire crate exists to keep out,
/// dragged in by one `usize`. The copy is safe because it is pinned: see
/// `pages_title_limit_is_the_one_pages_enforces` in `tests/`, which dev-deps
/// `pages` and fails the day the two disagree.
pub const MAX_PAGE_TITLE_LEN: usize = 512;
