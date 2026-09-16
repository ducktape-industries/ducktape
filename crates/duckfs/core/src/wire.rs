//! the duckfs wire surface — types only. writes go via [`FilesMsg`] (json)
//! or the binary putblob frame; reads via [`FilesQuery`] -> [`FilesReply`];
//! the off-block object fetch speaks [`FilesSyncReq`] -> [`FilesSyncResp`].

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};

use crate::objects::ObjectId;

/// a sha256-derived object id rendered as 64-char lowercase hex on the wire.
pub type DigestHex = String;

// ---- network constants (consensus; execute-time rejection on breach) ----
pub const CHUNK_SIZE: u64 = 1024 * 1024;
pub const MAX_NAME_BYTES: usize = 255;
pub const MAX_PATH_BYTES: usize = 4096;
pub const MAX_DEPTH: usize = 128;
pub const MAX_DIR_ENTRIES: usize = 65_536;
pub const MAX_INLINE_COMMIT_BYTES: usize = 256 * 1024;
/// the per-commit change ceiling — a CONSENSUS-WIRE bound, not a client setting.
/// a commit is ONE op in ONE block: its inline bytes ride that block's payload,
/// every chunked file rides one [`CHUNK_SIZE`] (~1 MiB) staging block each, in
/// sequence, with no priority lane, and the commit walks a tree spine per touched
/// path under [`MAX_OBJECT_READS_PER_OP`]. so raising this does not make a big
/// tree land faster — it makes one commit monopolize the chain, and every
/// validator pays. do not raise it: the client's answer to a large tree is
/// `.duckfsignore` (never walk build output) and `ducktape fs commit --path`
/// (one subtree per commit, each still atomic).
pub const MAX_CHANGES_PER_COMMIT: usize = 4096;
/// per-op ceiling on DISTINCT committed-store object reads — a files CONSENSUS
/// cap enforced in the pure core, so the native `Files` module and the wasm
/// files tenant reject the IDENTICAL oversized commit. a commit walks the
/// pre-existing tree spine of every touched path (base + effective-head trees
/// and their snapshots — `object-get`) and probes the odb once per newly-staged
/// object (`stage_object`'s `has` — `object-stat`); without a cap a commit
/// touching files under enough distinct pre-existing directories (or staging
/// enough new objects) reads unboundedly.
///
/// the number is what a GUEST DISPATCH can actually SPEND, measured, not a
/// round number picked for its shape. every unresolved committed read replays
/// the pure guest once with the answer memoized, out of ONE
/// `wasm_host::DEFAULT_FUEL` budget spent across the rounds, so an op that
/// spends the whole cap re-treads a prefix growing by one read per round: its
/// cost is quadratic in the reads and linear in the bytes each round re-walks.
/// a cap the guest cannot reach inside that fuel is not a cap at all — wasm
/// would trap on fuel where native still accepted, which is a native↔wasm
/// interchange break dressed as a resource limit. the binding shape is the most
/// expensive commit the other budgets admit: [`MAX_INLINE_COMMIT_BYTES`] of
/// inline bodies spread over cap/2 distinct files (each stages a chunk and a
/// fileobj — two reads). that commit COMPLETES on the guest at this cap and
/// traps above it, which `wasm_files_parity` drives on the real component from
/// both runtimes.
///
/// it must also stay within the wasm kernel's per-dispatch object-plane budget
/// (`wasm_host::MAX_OBJECT_READS`) — the outer bound the kernel itself enforces
/// — and the core counts EXACTLY what the kernel counts: the distinct
/// `object-get` and distinct `object-stat` ids that MISS the same-block object
/// overlay (the block-local object index here). it charges a read BEFORE issuing
/// it, so the guest — which runs this same core — trips this cap and rejects
/// STRICTLY BEFORE it can reach the kernel trap, and native applies the same cap
/// over `DiskStore`. a commit accepted by one runtime is therefore accepted by
/// both (the `files` crate compile-asserts the ordering). the `files` crate
/// re-exports this const.
pub const MAX_OBJECT_READS_PER_OP: usize = 256;
pub const MAX_MESSAGE_BYTES: usize = 4096;
pub const MAX_META_ENTRIES: usize = 16;
pub const MAX_META_KEY_BYTES: usize = 64;
pub const MAX_META_VALUE_BYTES: usize = 256;
pub const MAX_CHUNKS_PER_FILE: usize = 4_194_304;
pub const MAX_SYMLINK_TARGET_BYTES: usize = 4096;
pub const STAGING_QUOTA_BYTES: u64 = 1024 * 1024 * 1024;
pub const STAGING_TTL_BLOCKS: u64 = 4096;
pub const MAX_PINS: usize = 1024;
/// one owner's share of [`MAX_PINS`] — the pin-table analogue of
/// [`MAX_STAGING_ENTRIES_PER_OWNER`]. without it, one owner can fill the whole
/// global table (a permanent gc root each), refusing every other member's pin
/// and — since staging quota is reclaimed at commit — parking unbounded bytes
/// out of gc forever via {stage → commit → pin head} loops. 1024 / 32 = 32,
/// mirroring the staging table's ratio (65_536 / 4096 = 16) at a size that
/// still leaves room for many owners.
pub const MAX_PINS_PER_OWNER: usize = MAX_PINS / 32;
/// the global staging-table entry ceiling — [`MAX_PINS`] × 64 = 65_536. it is a
/// consensus constant shared by BOTH the canonical decode and the execute path:
/// [`decode_refs`](crate::state::decode_refs) rejects any refs image whose
/// staging section exceeds it, and `putblob` rejects a stage that would grow the
/// table to it. keeping ONE definition across the two sides is load-bearing —
/// because putblob refuses to stage past this bound, EVERY `Refs` an execute path
/// can produce stays within the decode ceiling, so the agreed image always
/// re-decodes on reboot and installs on a joiner. the per-owner byte quota does
/// NOT bound the entry count (distinct tiny chunks cost almost no quota), so
/// without this cap one owner could grow the table past the decode limit, commit
/// it as agreed consensus state, then brick the whole cluster the next time each
/// node loads its refs file (`decode_refs` would reject the honest, agreed
/// image everywhere at once).
pub const MAX_STAGING_ENTRIES: usize = MAX_PINS * 64;
/// one owner's share of the [`MAX_STAGING_ENTRIES`] staging table — the count
/// analogue of [`STAGING_QUOTA_BYTES`], bounding how many outstanding entries a
/// single owner may hold so no owner can monopolize the shared table. 4096 ×
/// [`CHUNK_SIZE`] (1 MiB) = 4 GiB, four times the 1 GiB byte quota, so an honest
/// large upload always trips the byte quota first and is never limited by this
/// cap; it bites only a hostile flood of tiny (sub-quota) chunks.
pub const MAX_STAGING_ENTRIES_PER_OWNER: usize = 4096;
pub const MAX_PIN_NAME_BYTES: usize = 128;
pub const MAX_WATCHES: usize = 256;
pub const MAX_WATCH_MODULE_ID_BYTES: usize = 128;
pub const HISTORY_WINDOW: usize = 1024;
pub const GC_PERIOD_BLOCKS: u64 = 1024;
pub const MAX_PAGE: u64 = 256;
/// the `GetObjects` request id cap — a sync fetch beyond this rejects ("too many
/// ids"). one batch is a page of ids, so it mirrors [`MAX_PAGE`]; the fetch loop
/// pages through larger missing sets a batch at a time.
pub const MAX_SYNC_IDS: usize = MAX_PAGE as usize;
/// ceiling on one encoded `GetObjects` reply (1.5 MiB). a batch may name up to
/// [`MAX_SYNC_IDS`] objects, and each can be a full [`CHUNK_SIZE`] chunk that
/// base64 inflates ~4/3x — unbudgeted, one reply could reach ~350 MiB, far past
/// the p2p message cap the reply must ride under (which the sender ASSERTS on).
/// `serve_sync` stops filling a reply at this budget and marks the remainder
/// absent; the possession driver's missing-walk re-requests them next round, so
/// a truncated page is progress, never an error. sized so the LARGEST single
/// object (one full chunk, ~1.4 MiB as base64 + envelope) always fits — the
/// first present object is served unconditionally, keeping every round's
/// `landed >= 1` invariant intact. bin/node compile-asserts this stays under
/// its `MAX_MESSAGE_SIZE` with rpc-envelope headroom.
pub const MAX_SYNC_REPLY_BYTES: usize = 3 << 19;
/// ceiling on the encoded refs image (1 MiB). the `GetRefs` sync reply ships it
/// WHOLE, base64-wrapped, under the same [`MAX_SYNC_REPLY_BYTES`] budget
/// `GetObjects` spends — and there is no cursor to page it. the count caps alone
/// do not hold it: at their ceilings the image is ~8 MB (65,536 staging rows at
/// ~124 B each), several times the p2p cap the sender ASSERTS on. so every refs
/// growth path (`putblob`, `pin`, `watch`) refuses an entry that would push the
/// image past this, `decode_refs` refuses a larger image outright, and
/// `serve_sync` refuses to ship one — the same execute/decode pairing as
/// [`MAX_STAGING_ENTRIES`], for the same reason (an agreed image must always
/// re-decode on reboot and install on a joiner).
pub const MAX_REFS_IMAGE_BYTES: usize = 1 << 20;
// the base64 image plus its json envelope rides under the reply budget.
const _: () = assert!(MAX_REFS_IMAGE_BYTES.div_ceil(3) * 4 + 64 <= MAX_SYNC_REPLY_BYTES);
pub const MAX_READ_BYTES: u64 = 1024 * 1024;
pub const MAX_GREP_SCAN_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_GREP_LINE_BYTES: usize = 256;
/// hard ceiling on hits emitted by ONE grep call (MAX_PAGE * 16 — the same
/// bounded-reply convention as the diff cap). the scan budget bounds bytes
/// SCANNED, not hits EMITTED, so without this a single in-budget file of
/// pathologically many matching lines could amplify into an unbounded reply;
/// with it a reply is bounded at roughly 4096 hits x ~0.5 KiB each (path +
/// capped line text + locator) — a couple of MiB worst case.
pub const MAX_GREP_HITS_PER_CALL: usize = MAX_PAGE as usize * 16;
/// first byte of the binary putblob op frame. json msgs start with b'{',
/// so one leading byte disambiguates the whole op space.
pub const PUTBLOB_FRAME_TAG: u8 = 0x00;

// ---- write wire ----

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FilesMsg {
    /// atomic multi-path commit. `base_snapshot: None` means the empty tree
    /// (first commit). per-path CAS: every changed path must be identical
    /// between base and the live head or the whole commit rejects.
    Commit {
        base_snapshot: Option<DigestHex>,
        message: String,
        changes: Vec<Change>,
    },
    /// Create a detached snapshot of one path and its ancestor spine. The
    /// candidate enters the history window without moving the public head.
    ProjectSnapshot {
        snapshot: DigestHex,
        path: String,
    },
    /// Atomically replace a module-owned retention reference. The authenticated
    /// module origin supplies the namespace; ordinary pins cannot remove it.
    CompareExchangeRetention {
        key: String,
        expected: Option<RetentionReference>,
        replacement: Option<RetentionReference>,
    },
    Pin {
        snapshot: DigestHex,
        name: String,
    },
    /// any authority removes any pin by name; the pin's owner is attribution.
    Unpin {
        name: String,
    },
    Watch {
        prefix: String,
        module_id: String,
    },
    /// gated to the module that registered the watch.
    Unwatch {
        prefix: String,
        module_id: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Put {
        path: String,
        exec: bool,
        meta: BTreeMap<String, String>,
        content: Content,
    },
    Mkdir {
        path: String,
    },
    /// removes the entry at `path` (file, symlink, or whole subtree).
    Rm {
        path: String,
    },
    Mv {
        from: String,
        to: String,
    },
    Symlink {
        path: String,
        target: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Content {
    /// small files ride inside the commit op; the module chunks + hashes.
    Inline { b64: String },
    /// large files reference chunks staged via putblob (or already present).
    Chunks { size: u64, chunks: Vec<DigestHex> },
}

// ---- read wire ----

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FilesQuery {
    Stat {
        path: String,
        snapshot: Option<DigestHex>,
    },
    Ls {
        path: String,
        snapshot: Option<DigestHex>,
        after: Option<String>,
        limit: u64,
    },
    Read {
        path: String,
        snapshot: Option<DigestHex>,
        offset: u64,
        len: u64,
    },
    Find {
        prefix: String,
        snapshot: Option<DigestHex>,
        after: Option<String>,
        limit: u64,
    },
    Grep {
        pattern: String,
        prefix: String,
        snapshot: Option<DigestHex>,
        cursor: Option<String>,
        limit: u64,
    },
    History {
        limit: u64,
    },
    Diff {
        from: DigestHex,
        to: DigestHex,
        prefix: String,
    },
    Refs {},
    /// Committed reference only. Writers derive their expected CAS value from
    /// their own canonical state, not this preceding-boundary query.
    Retention {
        module_id: String,
        key: String,
    },
    /// the client staging probe: which of these chunk ids the cluster already
    /// holds (staged in refs OR durable in the odb). advisory — the reply can go
    /// stale between a gc sweep and the commit, which re-validates; a stale answer
    /// costs one redundant stage or one clean rejection, never corruption. capped
    /// at [`MAX_SYNC_IDS`] ids per request (the batch-page convention).
    HasChunks {
        ids: Vec<DigestHex>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryKindWire {
    File,
    Dir,
    Symlink,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct EntryInfo {
    pub path: String,
    pub kind: EntryKindWire,
    pub size: u64,
    pub exec: bool,
    pub object: DigestHex,
    pub meta: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInfo {
    pub id: DigestHex,
    pub parent: Option<DigestHex>,
    pub root_tree: DigestHex,
    pub author: crate::Actor,
    pub height: u64,
    pub consensus_time: u64,
    pub message: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    Added,
    Removed,
    Modified,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DiffEntry {
    pub path: String,
    pub kind: DiffKind,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct GrepHit {
    pub path: String,
    pub line: u64,
    pub text: String,
    /// `<path>@<snapshot>#L<line>` — where this hit was, pinned (see
    /// [`evidence_locator`]).
    pub locator: String,
}

/// A module-owned immutable snapshot reference. Replacements compare the whole
/// previous value and advance its nonzero revision; release names that value too.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RetentionReference {
    pub snapshot: DigestHex,
    pub revision: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RefsInfo {
    pub head: Option<DigestHex>,
    pub pins: BTreeMap<String, DigestHex>,
    pub window_len: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FilesReply {
    Stat(Option<EntryInfo>),
    Ls {
        entries: Vec<EntryInfo>,
        next: Option<String>,
    },
    Read {
        b64: String,
        eof: bool,
    },
    Find {
        entries: Vec<EntryInfo>,
        next: Option<String>,
    },
    Grep {
        hits: Vec<GrepHit>,
        next: Option<String>,
    },
    History(Vec<SnapshotInfo>),
    Diff(Vec<DiffEntry>),
    Refs(RefsInfo),
    Retention(Option<RetentionReference>),
    /// per-id presence, in request order — `present[i]` answers `ids[i]`.
    HasChunks {
        present: Vec<bool>,
    },
}

/// The actual result of an accepted write. Identical metadata-only JSON bytes
/// are published as both output and assigned stamp, so generic call/action
/// summaries retain minted snapshot IDs without carrying file bodies.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FilesWriteOutput {
    pub actor: crate::Actor,
    pub source_revision: u64,
    pub outcome: WriteOutcome,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WriteOutcome {
    PutBlob {
        chunk: DigestHex,
    },
    Commit {
        snapshot: DigestHex,
    },
    ProjectSnapshot {
        snapshot: DigestHex,
    },
    CompareExchangeRetention {
        key: String,
        reference: Option<RetentionReference>,
    },
    Pin {
        snapshot: DigestHex,
        name: String,
    },
    Unpin {
        name: String,
    },
    Watch {
        prefix: String,
        module_id: String,
    },
    Unwatch {
        prefix: String,
        module_id: String,
    },
}

pub fn encode_write_output(output: &FilesWriteOutput) -> Vec<u8> {
    serde_json::to_vec(output).expect("files write output serializes")
}

pub fn decode_write_output(bytes: &[u8]) -> Result<FilesWriteOutput, String> {
    serde_json::from_slice(bytes).map_err(|error| format!("files write output: {error}"))
}

// ---- off-block object fetch (state sync / self-heal lane) ----

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FilesSyncReq {
    /// batched fetch; response order matches request order.
    GetObjects { ids: Vec<DigestHex> },
    /// the boundary refs image — the `root()` preimage a joiner installs BEFORE
    /// it can walk `missing_objects`. rides the same resolver (`serve_sync`) lane
    /// so a duckfs-odb joiner never needs the snapshot/chunk lane.
    GetRefs,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SyncObject {
    pub id: DigestHex,
    pub present: bool,
    /// object kind tag byte (`objects::Kind as u8`) — receivers re-derive the
    /// id as sha256(tag ‖ body) and reject mismatches.
    pub kind: u8,
    pub b64: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FilesSyncResp {
    Objects(Vec<SyncObject>),
    /// the refs image, standard-base64 (raw bytes as a json number array would
    /// inflate ~4x). the joiner root-verifies it against the manifest boundary
    /// root inside [`Files::install`](crate::Files::install) before adopting.
    Refs {
        b64: String,
    },
}

// ---- codecs (same shape as every module in this repo) ----

pub fn encode_msg(m: &FilesMsg) -> Vec<u8> {
    serde_json::to_vec(m).expect("serializable")
}
pub fn decode_msg(b: &[u8]) -> Result<FilesMsg, String> {
    serde_json::from_slice(b).map_err(|e| e.to_string())
}
pub fn encode_query(q: &FilesQuery) -> Vec<u8> {
    serde_json::to_vec(q).expect("serializable")
}
pub fn decode_query(b: &[u8]) -> Result<FilesQuery, String> {
    serde_json::from_slice(b).map_err(|e| e.to_string())
}
pub fn encode_reply(r: &FilesReply) -> Vec<u8> {
    serde_json::to_vec(r).expect("serializable")
}
pub fn decode_reply(b: &[u8]) -> Result<FilesReply, String> {
    serde_json::from_slice(b).map_err(|e| e.to_string())
}
pub fn encode_sync_req(r: &FilesSyncReq) -> Vec<u8> {
    serde_json::to_vec(r).expect("serializable")
}
pub fn decode_sync_req(b: &[u8]) -> Result<FilesSyncReq, String> {
    serde_json::from_slice(b).map_err(|e| e.to_string())
}
pub fn encode_sync_resp(r: &FilesSyncResp) -> Vec<u8> {
    serde_json::to_vec(r).expect("serializable")
}
pub fn decode_sync_resp(b: &[u8]) -> Result<FilesSyncResp, String> {
    serde_json::from_slice(b).map_err(|e| e.to_string())
}

// ---- resolver-lane helpers (the duckfs-odb state-sync driver's wire seam) ----
// pure encode/decode over the `serve_sync` lane, shared by the node join glue
// and the statesync driver test so neither re-derives the base64/hex framing.

/// encode the resolver-lane request for the boundary refs image.
pub fn encode_get_refs() -> Vec<u8> {
    encode_sync_req(&FilesSyncReq::GetRefs)
}

/// decode a served `Refs` reply into the raw refs image bytes (the `install`
/// input). rejects any other reply shape — a caller that asked for refs must
/// not silently accept an object batch.
pub fn decode_refs_reply(resp: &[u8]) -> Result<Vec<u8>, String> {
    match decode_sync_resp(resp)? {
        FilesSyncResp::Refs { b64 } => STANDARD
            .decode(b64.as_bytes())
            .map_err(|_| "files: refs reply is not valid base64".to_string()),
        FilesSyncResp::Objects(_) => Err("files: expected a refs reply, got objects".into()),
    }
}

/// encode a resolver-lane `GetObjects` request from raw ids.
pub fn encode_get_objects(ids: &[ObjectId]) -> Vec<u8> {
    encode_sync_req(&FilesSyncReq::GetObjects {
        ids: ids.iter().map(|id| to_hex(id)).collect(),
    })
}

/// decode a served `Objects` reply into the PRESENT `(id, kind, body)` batch
/// [`Files::ingest_objects`](crate::Files::ingest_objects) accepts; absent
/// entries are dropped (the caller re-queues them by re-walking
/// `missing_objects`). every id is re-hashed again at ingest, so a hex/b64 that
/// decodes here is still verified before it lands.
pub fn decode_objects_reply(resp: &[u8]) -> Result<Vec<(ObjectId, u8, Vec<u8>)>, String> {
    match decode_sync_resp(resp)? {
        FilesSyncResp::Objects(objs) => objs
            .iter()
            .filter(|o| o.present)
            .map(|o| {
                let id =
                    from_hex_32(&o.id).ok_or_else(|| "files: reply id is not hex".to_string())?;
                let body = STANDARD
                    .decode(o.b64.as_bytes())
                    .map_err(|_| "files: reply body is not base64".to_string())?;
                Ok((id, o.kind, body))
            })
            .collect(),
        FilesSyncResp::Refs { .. } => Err("files: expected an objects reply, got refs".into()),
    }
}

/// build a putblob op: `[PUTBLOB_FRAME_TAG] ++ raw chunk bytes`.
pub fn encode_putblob(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + bytes.len());
    out.push(PUTBLOB_FRAME_TAG);
    out.extend_from_slice(bytes);
    out
}

/// grep-hit evidence locator: `<path>@<snapshot>#L<line>` — the file, the
/// snapshot it was scanned at, and the 1-based line.
///
/// NOT a `duck://` uri, deliberately. the one duck:// grammar admits a file
/// only as `duck://files/shared/attachments/<dir>/<name>`, plain, because
/// classification is the only guard between a crafted ref and a client read
/// at any path it names; a grep hit is an arbitrary path at a pinned
/// snapshot, so minting one as a duck:// address produced a link the protocol
/// refuses to open. evidence points AT a line, it does not address it.
pub fn evidence_locator(path: &str, snapshot: &str, line: u64) -> String {
    format!("{path}@{snapshot}#L{line}")
}

/// decode exactly 64 lowercase-hex chars into 32 bytes (uppercase rejected).
pub fn from_hex_32(s: &str) -> Option<[u8; 32]> {
    let bytes = s.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = hex_val(bytes[2 * i])?;
        let lo = hex_val(bytes[2 * i + 1])?;
        *slot = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

pub fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// strict variable-length hex decode (either case). ascii hex digits only:
/// `from_str_radix` alone would tolerate '+'/'-' signs, and byte-offset
/// slicing panics mid-codepoint on multibyte utf-8 — this parses PASTED
/// input (invite blobs, rpc hex), so it must Err, never panic.
pub fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("hex string contains non-hex characters".into());
    }
    if !s.len().is_multiple_of(2) {
        return Err("hex string has odd length".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::unhex;

    #[test]
    fn unhex_rejects_non_ascii_without_panicking() {
        // fixed-offset slicing panics mid-codepoint unless ascii is enforced —
        // this parses PASTED invite blobs and rpc hex, so Err, never panic.
        assert!(unhex("a\u{2026}").is_err());
        assert!(unhex("caf\u{e9}").is_err());
        assert!(unhex("zz").is_err());
        assert_eq!(unhex("00ff").unwrap(), vec![0x00, 0xff]);
    }

    #[test]
    fn unhex_rejects_sign_characters() {
        // from_str_radix would tolerate a leading '+' per pair.
        assert!(unhex("+1ab").is_err());
        assert!(unhex("-1ab").is_err());
    }
}
