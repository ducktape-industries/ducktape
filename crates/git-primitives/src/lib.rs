//! the bounded local-git read surface, as plain Rust data.
//!
//! these are the six `ducktape:module/host` git types, hand-written here with
//! no dependencies so that BOTH sides of a git read can name them: the kernel
//! host declares them across [`wasm_host::OdbBacking`] and converts them to its
//! own WIT-generated twins inside the import implementations, while a module's
//! read policy names them directly and so stays repo-separable from the kernel
//! (#2303). the field-for-field correspondence with `wit/module.wit` is the
//! contract; `crates/kernel/wasm-host/src/git_wit.rs` is the one place the two
//! shapes meet, so a WIT edit that does not reach here fails to compile there.
//!
//! [`wasm_host::OdbBacking`]: ../wasm_host/trait.OdbBacking.html

/// a commit's structural fields — what a bounded read can answer without
/// materializing the message or the author.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitCommit {
    pub tree: Vec<u8>,
    pub parents: Vec<Vec<u8>>,
}

/// one entry of a tree: the raw mode nibble the host packs into a byte, the
/// path component as stored, and the target oid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitTreeEntry {
    pub kind: u8,
    pub name: Vec<u8>,
    pub oid: Vec<u8>,
}

/// the materialized body of an object, absent when the read's byte ceiling
/// refused to produce one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitObjectData {
    Commit(GitCommit),
    Tree(Vec<GitTreeEntry>),
    Blob(Vec<u8>),
    Tag(Vec<u8>),
}

/// an object read: always its kind and true size, its body only within budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitObject {
    pub kind: u8,
    pub size: u64,
    pub data: Option<GitObjectData>,
}

/// a diff between two commits, with the counts that stay true even when
/// `patch` was clipped at the ceiling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitDiff {
    pub patch: String,
    pub truncated: bool,
    pub files_changed: u64,
    pub additions: u64,
    pub deletions: u64,
}

/// why a diff could not be answered. `Unsupported` is the substrate saying it
/// has no git plane at all, which is a different thing from a read that hit a
/// ceiling ([`GitDiffError::Limit`]) or a repository that would not open
/// ([`GitDiffError::Unavailable`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitDiffError {
    Unavailable(String),
    Limit(String),
    Unsupported,
}
