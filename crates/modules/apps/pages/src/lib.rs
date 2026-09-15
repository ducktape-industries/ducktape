//! qmdb-backed pages module — a notion-like block tree, one block per key.
//!
//! a page is a TREE of [`Block`]s: a `Page` block starts the document (its text
//! is the title), every block carries an ordered `children` list, and
//! every block id is GLOBALLY UNIQUE within the module. unlike the document
//! module's whole-doc-per-key layout, the store key here is `sha256(block_id)`
//! and the value is ONE serialized block — so the merkle root commits to every
//! block individually, and a single block is readable (and one day provable)
//! by id alone with no page context. that is the addressability contract: any
//! module can resolve a bare block id via `Ctx::query(pages, GetBlock {
//! block_id })`.
//!
//! pure logic over a host-injected [`sdk::MerkleStore`]: the HOST constructs
//! the concrete store (qmdb today — `statesync::qmdb::QmdbStore`) and hands it
//! to [`Pages::new`], so this crate never names a storage crate. the module's
//! authenticated [`StateRoot`] IS the store's merkle root, so it flows
//! directly into the global root-hash via `host::global_root`.
//!
//! ## keys are hashed to a fixed width
//!
//! the logical key is the `block_id` string at the interface seam, but the
//! store key is `sha256(block_id)` — a fixed 32-byte digest, mirroring the
//! kv/document modules. this is load-bearing: the store's state-sync
//! resolvers are bounded on fixed-width keys.
//!
//! ## enumeration via a reserved index entry
//!
//! one extra store entry is reserved: the sentinel logical key
//! [`PAGE_INDEX_KEY`] whose value is the serialized SORTED map from every page
//! block id to its containing page. its leading NUL makes it uncollidable with
//! a client-minted block id, and every op that names it is rejected before any
//! storage touch. top-level pages enter through [`PageMsg::CreatePage`]; nested
//! pages are ordinary `Page` blocks, so insert/move/remove update this index in
//! the same staged transaction as the block tree.
//!
//! ## host-lent staging (the kv/document pattern, plus deletes)
//!
//! writes made during a block are STAGED in an in-memory `pending` overlay and
//! flushed to the store in ONE batch by `commit_block`; `abort_block` drops
//! the overlay. the pages twist: `RemoveBlock` deletes a whole subtree, so the
//! overlay value is an `Option<Vec<u8>>` — `Some` stages a write, `None`
//! stages a DELETE (`commit_batch`'s `None`), and reads through the overlay
//! see a staged delete as absence.
//!
//! ## state-sync
//!
//! sync belongs to the injected store, not this module: a joiner (dynamic-
//! valset catch-up, a fresh full node, crash recovery) rebuilds the CONCRETE
//! store from a peer (`QmdbStore::sync_from`) and wraps a fresh `Pages` around
//! it. this module only forwards the trait's serve surface —
//! [`Module::serve_sync`] and [`Module::resolver_sync_target`] delegate
//! straight to the store.

// the wire surface: this module's shared types, flattened at the crate root.
mod interface;
pub use interface::*;
// the derived-tier materialized view: the PURE decision core (fold + view
// over index_guest::StateRead), compiled everywhere and unit-tested
// natively. the engine shell that runs it inside the module's index
// database is `index_guest` below.
pub mod index;

// the CLIENT view model: applied-op classification for feed followers —
// module-owned beside the index fold, pure, ui.wasm-portable.
pub mod client;

// the wasm index-mapper shell: wires the pure core into the fluent31 engine.
// compiled only by `guest-builder --index`'s synthesized wasm32 workspace
// (feature `index-guest`), never by the native build.
#[cfg(feature = "index-guest")]
mod index_guest;

use std::collections::BTreeMap;

use sdk::{
    Ctx, Error, MerkleStore, Module, ModuleId, Msg, ResolverSyncTarget, StagedStore, StateRoot,
    StateSyncHandle,
};

mod block_ops;
mod comment_ops;
mod error;
mod module_impl;
mod ops;
mod page_ops;
mod record_ops;
mod store;
mod text_ranges;

use error::{PageError, to_page_err};

/// write-time cap on ONE serialized block record (and on the enumeration
/// index value — both stage through the same guard). the concrete store's
/// codec bounds a stored value at 1 MiB AT DECODE TIME only (see
/// `statesync::qmdb::store_config`), so an oversized value that staged fine
/// would panic every later read on every validator: a poison pill. 768 KiB
/// leaves the same 256 KiB framing margin the document module keeps. a block
/// record carries its text plus its ordered child-id list, so this also
/// bounds a single parent to tens of thousands of children.
pub const MAX_BLOCK_LEN: usize = 768 * 1024;

/// write-time cap on a page TITLE — the text of a `Page` block. a page title
/// is one sidebar line, and the page list serves up to `MAX_PAGE_LIMIT` (256)
/// rows carrying it in a single reply; capping it here keeps a full list far
/// below [`MAX_PAGE_QUERY_BYTES`] instead of letting one client-chosen title
/// refuse the whole sidebar read. rejected deterministically at write time,
/// like every other bound the store guard enforces.
pub const MAX_PAGE_TITLE_LEN: usize = 512;

/// Maximum number of block edges below one page root. Nested `Page` blocks
/// are leaves in the containing document and start their own depth budget.
/// This keeps every valid preorder cursor page comfortably below the wasm
/// host's store-read ceiling while leaving far more nesting than the UI uses.
pub const MAX_PAGE_DEPTH: usize = 64;

/// client-minted id length cap (consensus constant) for a page id
/// (`CreatePage`) or any block id (`InsertBlock`) — the same wedge class the
/// comment ids guard against (see `id_is_index_safe`, interface.rs), but for
/// the page-enumeration index instead of a comment thread/target index. a
/// page id ALSO becomes an index entry, so without this cap a handful of
/// oversized ids reach [`MAX_BLOCK_LEN`] and abort `CreatePage`/`InsertBlock`
/// for every account, forever (nothing else can shrink the index).
pub const MAX_PAGE_ID_BYTES: usize = 128;
/// same cap, applied to every `InsertBlock` id (page-creating or not): a
/// non-page block id never enters the index, but it is still a client-minted
/// key stored forever, so it gets the same bound as a page id.
pub const MAX_BLOCK_ID_BYTES: usize = MAX_PAGE_ID_BYTES;

/// hard cap on the number of pages the enumeration index may ever hold.
/// `index_add` re-serializes the WHOLE index on every insert (store.rs), so
/// this bounds its worst case: each entry serializes as `"id":"parent",`
/// with both id and parent at most [`MAX_PAGE_ID_BYTES`] (128) bytes, i.e. at
/// most 1+128+1 + 1 + 1+128+1 + 1 = 262 bytes; `MAX_PAGES` (2048) × 262 B ≈
/// 524 KiB, comfortably under [`MAX_BLOCK_LEN`] (768 KiB). global rather than
/// per-author: it is the smaller diff, and it is what actually bounds the
/// shared index's size regardless of how many distinct accounts contribute.
pub const MAX_PAGES: usize = 2048;

/// the reserved logical key under which the page-enumeration INDEX rides in
/// the same store. its value is a serialized sorted map from every page block
/// id to its containing page. the leading NUL makes it UNCOLLIDABLE with a real
/// block id (clients mint uuids), and every op that names it is rejected
/// ([`PageError::ReservedId`]) before it can reach storage.
const PAGE_INDEX_KEY: &str = "\u{0}page-index";

/// Local ceiling for tree walks and the record work they schedule. The wasm
/// host permits 4096 reads per dispatch; the headroom covers records touched
/// before and after the walk while making native and wasm reject at one point.
const MAX_TRAVERSAL_WORK: usize = 3_500;

/// Leaves room for the moved block, both page-depth walks, and parent writes
/// below the wasm host's 4096 store-read ceiling.
const MAX_MOVE_SUBTREE_READS: usize = 3_000;

/// a block-tree pages module over a host-injected authenticated store.
pub struct Pages {
    id: ModuleId,
    /// the host-injected authenticated store plus this block-height's staging
    /// overlay: blocks touched this block are staged (a write, or a `None`
    /// DELETE for subtree removal), read ahead of committed state
    /// (read-your-writes), and flushed to the store in one batch at
    /// `commit_block`; NOT in `root()` until then. store key is
    /// `sha256(block_id)`, owned by [`StagedStore`].
    staged: StagedStore,
    /// Source-owned block and comment attribution, wired in production.
    attribution: Option<ModuleId>,
    identity: Option<ModuleId>,
    /// Receipt-owned immutable artifact retention, emitted as this module.
    files: Option<ModuleId>,
}

#[cfg(test)]
mod tests;

// the wasm-guest port: the dispatch shell that adapts this module to the
// ducktape:module world. compiled only by the guest-builder's synthesized
// wasm32 cdylib workspace (feature `guest`), never by the native build.
#[cfg(feature = "guest")]
mod guest;
