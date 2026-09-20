//! network state sync: how a joiner rebuilds every module from a RUNNING node.
//!
//! ## role (unified-node design, phases 2-4)
//!
//! state sync is the JOIN-TIME BOOTSTRAP, not the steady state. a standing
//! node follows the head by FOLDING finalized frames (the replica pipeline:
//! verified certificates through `consensus::FollowerOrderer` into the same
//! `OrderedNode` a validator drains); these lanes serve exactly three
//! moments — standing a fresh joiner up at a boundary, the `Frames` suffix
//! that gives a bootstrap (or a fold-driver gap) journal continuity, and a
//! promotion's boundary artifacts. `MAX_CAPTURES` is sized for that
//! join-shaped load: concurrent joiners, not a resident fleet re-polling.
//!
//! ## protocol
//!
//! four request shapes ride one request/response transport (any transport —
//! a p2p channel, a socket, an in-process loopback — via [`SyncClient`]):
//!
//! 1. **Manifest** — the server captures a consistent view of its registry at
//!    its latest finalized boundary (height, root-hash, and per-module root +
//!    sync payload) and caches it; the response lists `(module, root, kind)`.
//!    everything in one capture comes from ONE boundary, so the payloads
//!    compose to exactly the manifest's root-hash.
//! 2. **Chunk** — fetch a captured module's snapshot payload in bounded chunks
//!    (snapshot bytes can exceed a transport's frame cap; chunking is the
//!    protocol's job, not the transport's).
//! 3. **Module** — route module-defined bytes to a live module's
//!    [`serve_sync`](sdk::Module::serve_sync): the qmdb op-range lane. served
//!    with HISTORICAL proofs, so an in-flight joiner target stays servable
//!    while the source keeps finalizing new blocks.
//! 4. **Frames** — fetch a bounded recovery-journal suffix: finalized,
//!    non-discarded frame bytes plus their seal roots/root-hash, so a promoted
//!    joiner can persist the same replay suffix a restart would have.
//! 5. **IndexOps** — the derived tier's OP-ROW BACKFILL (indexable spec §7):
//!    the serving node's stored index op rows below a joiner's boundary,
//!    cursor-paged in ascending key order. a joiner writes them into its own
//!    freshly-stamped index so its views hold pre-join history instead of
//!    beginning empty at the boundary. see the trust model — this is the one
//!    lane that is not verifiable.
//!
//! ## trust model
//!
//! the server is UNTRUSTED. every installable payload is verified by the
//! joiner against a root it obtained from the manifest — and the manifest's
//! root-hash is what the joiner ultimately recomposes and checks, so a lying
//! manifest fails the final compose. qmdb batches are merkle-verified by the
//! sync engine; snapshot installs re-derive the root before adopting bytes.
//! (the manifest root-hash itself is cross-checked against consensus when the
//! joiner later participates — a fabricated world still cannot vote.)
//!
//! the ONE exception is the index op-row lane: the derived tier has no root
//! by design (it is never part of the root-hash), so its rows cannot be
//! verified — a joiner trusts its OWN SYNC SOURCE for VIEW bytes only, the
//! same node it just accepted canonical state from. consensus state is
//! untouched either way, a lying row can never fork the node, and the honest
//! remedy for a bad backfill is the same as for any damaged index: wipe the
//! directory and let the boundary stamp answer honestly. what the lane still
//! enforces at the boundary is STRUCTURE ([`fetch_index_ops`]): key shape,
//! strictly ascending order, the boundary ceiling, and a decodable row
//! envelope — a source cannot make a joiner write garbage it would then fold.
//!
//! ## wire format
//!
//! compact hand-rolled binary (u64-le length prefixes, strict bounds checks,
//! no trailing bytes) — NOT serde_json: snapshot payloads are bulk bytes and
//! json inflates raw bytes ~3.7x, which would silently shrink the usable
//! chunk size under a transport frame cap.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use commonware_codec::DecodeExt as _;
use host::{FinalizedBlock, Host};
use sdk::{ModuleId, ROOT_LEN, StateRoot, StateSyncHandle};

pub mod dataplane;
pub mod monitor;
pub mod p2p;
pub mod qmdb;
pub mod wire;

use wire::WireError;

/// a pinned qmdb sync target for a resolver-backed module at a boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolverTarget {
    pub root: qmdb::SyncDigest,
    pub start: u64,
    pub op_count: u64,
}

impl ResolverTarget {
    pub fn to_sync_target(&self) -> Result<qmdb::SyncTarget, String> {
        let range = commonware_utils::range::NonEmptyRange::new(
            commonware_storage::merkle::Location::new(self.start)
                ..commonware_storage::merkle::Location::new(self.op_count),
        )
        .map_err(|_| {
            format!(
                "pinned resolver target has empty range {}..{}",
                self.start, self.op_count
            )
        })?;
        Ok(qmdb::SyncTarget {
            root: self.root,
            range,
        })
    }
}

/// max snapshot bytes per [`SyncResponse::Chunk`]. sized so a chunk plus
/// framing stays far under the mesh's 1 MiB message cap.
pub const CHUNK_LEN: usize = 256 * 1024;

/// max recovery frames examined per [`SyncResponse::Frames`] batch. This is a
/// work bound, not a byte guarantee: the node serve path separately budgets the
/// exact encoded response against its configured mesh message cap.
pub const FRAME_BATCH_LEN: usize = 64;
/// max index op rows examined per [`SyncResponse::IndexOps`] page. same
/// contract as [`FRAME_BATCH_LEN`]: a work bound, with the serve path
/// budgeting the exact encoded response against the mesh message cap.
pub const INDEX_OPS_BATCH_LEN: usize = 512;

/// hard cap on [`Manifest::applied_frames`] — the replay-window depth a
/// boundary carries. MUST equal the node's `REPLAY_WINDOW_HEIGHTS`: the
/// window a joiner inherits has to be exactly as deep as the one its peers
/// enforce, or the two disagree on a re-proposed batch and fork. this crate
/// does not link `node`, so `bin/node` pins the equality with a compile-time
/// assert. 4096 pairs is ~160 KiB on the wire, well inside the mesh cap.
pub const MAX_APPLIED_FRAMES: usize = 4096;

/// fixed bytes prepended to every authenticated statesync request and reply:
/// requester(32) + proof(64) + request id(8).
pub const RPC_HEADER_LEN: usize = 32 + 64 + 8;

/// how many boundary captures a server retains. more than one lets a second
/// joiner start syncing without invalidating the first joiner's in-flight
/// capture when the boundary advances between their manifest fetches.
pub const MAX_CAPTURES: usize = 4;
const MAX_LEASED_BOUNDARIES: usize = MAX_CAPTURES;

/// a deterministic state-sync boundary: both the app height and the module-root
/// composition served at that height.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BoundaryId {
    pub height: u64,
    pub root_hash: StateRoot,
}

impl Ord for BoundaryId {
    fn cmp(&self, other: &Self) -> Ordering {
        self.height
            .cmp(&other.height)
            .then_with(|| self.root_hash.0.cmp(&other.root_hash.0))
    }
}

impl PartialOrd for BoundaryId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// ============================================================================
// errors
// ============================================================================

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("wire error: {0}")]
    Wire(#[from] WireError),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("server error: {0}")]
    Server(String),
    #[error("unexpected response kind: {0}")]
    UnexpectedResponse(&'static str),
    #[error("module {module}: {reason}")]
    Module { module: ModuleId, reason: String },
    #[error("module {module}: the served stream is malformed ({reason})")]
    Malformed {
        module: ModuleId,
        /// stable snake_case token — never prose — so a caller (and the log
        /// line it lands in) can tell which check refused without parsing
        /// English.
        reason: &'static str,
    },
    #[error("module {module}: pinned qmdb range pruned ({reason}); refetch manifest")]
    Pruned { module: ModuleId, reason: String },
    #[error("recovery frame range pruned after {requested_after}; retained from {retained_from}")]
    RangePruned {
        requested_after: u64,
        retained_from: u64,
    },
}

// ============================================================================
// frames
// ============================================================================

/// how a captured module's state travels to a joiner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadKind {
    /// no durable state; recreate at genesis.
    Stateless,
    /// self-contained snapshot bytes, fetched via [`SyncRequest::Chunk`].
    Snapshot,
    /// module-specific resolver lane via [`SyncRequest::Module`] (qmdb).
    Resolver,
    /// the module declared no sync surface — a joiner cannot rebuild it.
    Unsupported,
}

impl PayloadKind {
    fn to_u8(self) -> u8 {
        match self {
            Self::Stateless => 0,
            Self::Snapshot => 1,
            Self::Resolver => 2,
            Self::Unsupported => 3,
        }
    }

    fn from_u8(v: u8) -> Result<Self, WireError> {
        Ok(match v {
            0 => Self::Stateless,
            1 => Self::Snapshot,
            2 => Self::Resolver,
            3 => Self::Unsupported,
            other => return Err(WireError::BadTag("PayloadKind", other)),
        })
    }
}

/// recovery-equivalent disposition for a finalized, non-discarded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameDisposition {
    Applied,
    Rejected,
}

impl FrameDisposition {
    fn to_u8(self) -> u8 {
        match self {
            Self::Applied => 0,
            Self::Rejected => 1,
        }
    }

    fn from_u8(v: u8) -> Result<Self, WireError> {
        Ok(match v {
            0 => Self::Applied,
            1 => Self::Rejected,
            other => return Err(WireError::BadTag("FrameDisposition", other)),
        })
    }
}

/// one finalized, non-discarded recovery frame and the seal that consensus
/// served at that height.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizedFrame {
    pub height: u64,
    pub frame: Vec<u8>,
    pub disposition: FrameDisposition,
    pub roots: Vec<(ModuleId, StateRoot)>,
    pub root_hash: StateRoot,
}

/// one module's row in a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    pub module_id: ModuleId,
    pub root: StateRoot,
    pub code_hash: Option<Vec<u8>>,
    pub kind: PayloadKind,
    pub resolver_target: Option<ResolverTarget>,
}

/// the joiner's picture of one captured boundary.
///
/// besides the module payloads, the manifest carries the boundary's CONSENSUS
/// COORDINATES — everything a syncing joiner needs to become a validator at
/// this exact boundary. like the root-hash, these are unauthenticated serving
/// hints under the same trust model: a lying epoch or base makes the joiner's
/// heights (and thus its root-hash) diverge, which fails loudly; a fabricated
/// world still cannot vote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub height: u64,
    pub root_hash: StateRoot,
    /// the consensus epoch whose engine was live at `height`.
    pub epoch: u64,
    /// that epoch's app-height base (`app_height = view_base + engine view`).
    pub view_base: u64,
    /// the epoch's engine participant set (raw public-key bytes) — NOT
    /// necessarily the valset projection at `height`, which may already
    /// stage a change awaiting its cutover.
    pub participants: Vec<Vec<u8>>,
    /// the epoch's RESIDENT set (transport standing, no quorum seat).
    pub residents: Vec<Vec<u8>>,
    /// the scheme-encoded finalization certificate for exactly `height`,
    /// when the serving node holds one (`None` right after a cutover, when
    /// the epoch has not finalized past its base — the joiner then spawns on
    /// the epoch's genesis floor instead).
    pub floor_cert: Option<Vec<u8>>,
    pub entries: Vec<ManifestEntry>,
    /// the serving node's REPLAY WINDOW at `height`: the `(height, batch id)`
    /// pairs it has journaled, oldest first, capped at
    /// [`MAX_APPLIED_FRAMES`]. a joiner seats with this as its own window, so
    /// it refuses the same re-proposed batches its peers refuse instead of
    /// applying one at a fresh height and forking the root. every entry is at
    /// or below `height`.
    ///
    /// an unauthenticated serving hint like the coordinates above: a lying
    /// window makes the joiner refuse a batch its peers apply, which diverges
    /// its root — the same failure a lying `view_base` produces, and a
    /// fabricated world still cannot vote.
    pub applied_frames: Vec<(u64, [u8; 32])>,
    /// the view a valset change already committed but not yet cut over at
    /// `height` (`None` when no change is armed). the served epoch's
    /// `participants` may already lag the committed valset by design (see
    /// above) — this is the serving orchestrator's OWN armed ceiling, so a
    /// seat resumed from it (`ValsetOrchestrator::resume(.., pending)`) stops
    /// at the same view every peer stops at instead of observing the
    /// already-changed valset itself and arming a LATER ceiling that forks
    /// the root.
    pub pending_cutover_view: Option<u64>,
}

impl Manifest {
    pub fn entry(&self, id: &str) -> Option<&ManifestEntry> {
        self.entries.iter().find(|e| e.module_id == id)
    }

    pub fn boundary_id(&self) -> BoundaryId {
        BoundaryId {
            height: self.height,
            root_hash: self.root_hash,
        }
    }

    /// the first module this boundary cannot transfer at all — the serving
    /// node could not prepare a sync handle for it (or the module declares no
    /// sync surface), so the capture holds no payload and pins no resolver
    /// target. a joiner cannot rebuild such a module from this source at ANY
    /// height: the fact belongs to the SOURCE, so the caller rotates to
    /// another one rather than refetching this manifest.
    pub fn undeliverable_module(&self) -> Option<&ModuleId> {
        self.entries
            .iter()
            .find(|e| e.kind == PayloadKind::Unsupported)
            .map(|e| &e.module_id)
    }
}

/// a state-sync request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncRequest {
    /// capture (or reuse) the latest finalized boundary and describe it.
    Manifest,
    /// fetch a chunk of a captured module's snapshot payload.
    Chunk {
        boundary: BoundaryId,
        module_id: ModuleId,
        offset: u64,
    },
    /// route module-defined bytes to the live module's `serve_sync`.
    Module {
        boundary: BoundaryId,
        module_id: ModuleId,
        body: Vec<u8>,
    },
    /// fetch recovery-equivalent finalized frame records in `(after, up_to]`.
    Frames {
        after_height: u64,
        up_to_height: u64,
    },
    /// one page of a module's stored index OP ROWS at or below `boundary`,
    /// in ascending key order, strictly after the `(height, seq)` cursor —
    /// the UNVERIFIED derived-tier backfill (indexable spec §7). the derived
    /// tier has no root by design, so nothing here composes into the
    /// root-hash check: a joiner trusts its own sync source for view bytes.
    /// `boundary` is a plain height CEILING, not a captured boundary: no
    /// lease is involved (the rows are node-local derived state, like the
    /// Frames lane's journal), so a long walk cannot lose a capture midway.
    IndexOps {
        boundary: u64,
        module: ModuleId,
        after: Option<(u64, u32)>,
    },
    /// read the tip's consensus coordinates (membership, epoch, height) —
    /// the DETECTION lane; see [`TipCoords`].
    TipCoords,
    /// fetch one node-local content-addressed blob by its sha256 digest —
    /// the fetch-on-miss lane for host-staged bytes that consensus pins by
    /// hash but never carries (an agent's registered prompt above all). the
    /// HOST layer answers this from its blob store, not [`SyncServer`]:
    /// blobs are node-local staging, no capture or boundary is involved.
    /// content addressing makes the answer self-verifying — the requester
    /// re-hashes the bytes and drops a mismatch, so no trust attaches to
    /// which peer answered.
    Blob { digest: [u8; 32] },
    /// the blob's total length — the discovery half of the RANGED fetch lane
    /// for host-staged artifacts too large for one mesh frame (wasm module
    /// components, quack capsules). same host-layer serving and honest-miss
    /// semantics as [`SyncRequest::Blob`].
    BlobInfo { digest: [u8; 32] },
    /// one bounded window of a blob, `[offset, offset+len)` clamped to the
    /// blob's tail. ranges carry no per-window proof — the requester stages
    /// the assembled whole and re-hashes it against the digest, dropping a
    /// mismatch (fail-closed), so no trust attaches to which peer answered.
    BlobRange {
        digest: [u8; 32],
        offset: u64,
        len: u64,
    },
    /// ask a peer to PRODUCE the git objects a forge branch head needs, and
    /// answer with the digest of the pack it staged — the catch-up lane for a
    /// node whose committed head runs ahead of its objects.
    ///
    /// the blob lanes above fetch bytes a peer already HOLDS, keyed by the
    /// digest consensus pinned. that is exact but brittle: pack bytes are not
    /// reproducible, so only a node holding that very pack can answer, and a
    /// head whose pack no node kept can never be explained. this lane asks for
    /// the OBJECTS instead — any peer that materialized `head` can build them
    /// — and the answer needs no trust: the requester installs the pack (which
    /// re-hashes every object) and then demands the full closure of `head`,
    /// an oid consensus already committed. a lying peer fails that check.
    ///
    /// `bases` are the requester's own on-disk heads, so the answer is bounded
    /// to what actually moved. the pack itself travels over
    /// [`SyncRequest::BlobRange`] like any other large artifact.
    ForgeObjects {
        repo: String,
        head: [u8; FORGE_OID_LEN],
        bases: Vec<[u8; FORGE_OID_LEN]>,
    },
}

/// a raw git sha1 oid — forge's object format, and the only oid width this
/// lane carries. named here so the codec's bounds read as intent.
pub const FORGE_OID_LEN: usize = 20;

/// the most on-disk heads a [`SyncRequest::ForgeObjects`] may name. the bases
/// only narrow the answer, so a requester never needs many; the cap is what
/// stops a forged count from driving a large allocation.
pub const MAX_FORGE_BASES: usize = 64;

impl SyncRequest {
    /// the request's kind as a wire-stable snake_case label — the
    /// [`monitor::ServeMonitor`]'s per-kind attribution (and thus a metric
    /// label value downstream), so renaming a variant must not rename these.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Chunk { .. } => "chunk",
            Self::Module { .. } => "module",
            Self::Frames { .. } => "frames",
            Self::IndexOps { .. } => "index_ops",
            Self::TipCoords => "tip_coords",
            Self::Blob { .. } => "blob",
            Self::BlobInfo { .. } => "blob_info",
            Self::BlobRange { .. } => "blob_range",
            Self::ForgeObjects { .. } => "forge_objects",
        }
    }
}

/// the tip's consensus coordinates without a captured boundary — the
/// DETECTION lane. a parked or folding resident polls this to track
/// membership, standing, and epoch cutovers; answering costs the server no
/// capture, no lease, and no floor-cert alignment, so a fleet's routine
/// polling never contends with (or churns) the join-shaped capture cache
/// the Manifest lane is sized for. same trust model as the manifest's
/// coordinates: unauthenticated serving hints — action taken on them
/// (ascension, promotion) re-fetches a full [`Manifest`] and verifies its
/// floor certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TipCoords {
    pub height: u64,
    pub root_hash: StateRoot,
    pub epoch: u64,
    pub view_base: u64,
    pub participants: Vec<Vec<u8>>,
    pub residents: Vec<Vec<u8>>,
    /// whether the server holds the finalization certificate for exactly
    /// `height` — a liveness hint, never the certificate itself.
    pub has_floor: bool,
    /// the tip's membership GENERATION — the committed valset counter, the
    /// index namespace the mesh tracks peer sets under (distinct from
    /// `epoch`, which stays the engine/channel coordinate).
    pub generation: u64,
    /// the tip's retained mesh-generation window, ascending — the same
    /// snapshots the server derives from committed valset state, so a parked
    /// joiner tracks the identical window every member tracks.
    pub mesh_window: Vec<MeshWindowEntry>,
    /// the SERVING node's build stamp — the same string it reports as
    /// `/v1/status .version`'s stamp half. `None` when that node could not
    /// identify its own build (a source tarball, a checkout without `.git`),
    /// which is a silence, never a claim.
    ///
    /// a DIAGNOSTIC, and it rides this lane precisely because the lane is
    /// unauthenticated by construction: the poller renders it on its peers
    /// view and may warn that the two builds differ, and NOTHING admits,
    /// refuses, gates or votes on it. two ducktape builds whose consensus
    /// logic has drifted still finalize together and their roots still
    /// silently diverge — this only makes the drift nameable.
    pub build: Option<String>,
}

/// one mesh-generation snapshot as carried on the sync wire. statesync-local
/// on purpose: this crate must not depend on the valset module crate — node
/// code converts at the fill site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshWindowEntry {
    pub generation: u64,
    pub validators: Vec<Vec<u8>>,
    pub residents: Vec<Vec<u8>>,
}

/// a state-sync response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncResponse {
    Manifest(Manifest),
    Chunk {
        total: u64,
        bytes: Vec<u8>,
    },
    Module(Vec<u8>),
    Frames {
        frames: Vec<FinalizedFrame>,
    },
    RangePruned {
        requested_after: u64,
        retained_from: u64,
    },
    /// one page of a module's index op rows: `(op key, row bytes)` verbatim,
    /// in ascending key order. `next_after` is `Some` exactly when the page
    /// was cut short — its value is the last row's `(height, seq)`, the next
    /// request's cursor. `source_floor` is the SERVER's own backfill floor
    /// for the module (rows below it never existed here) and `applied_height`
    /// its watermark, so the joiner can compose an honest floor and refuse a
    /// source that cannot cover the range it asked for.
    IndexOps {
        rows: Vec<(String, Vec<u8>)>,
        next_after: Option<(u64, u32)>,
        source_floor: Option<u64>,
        applied_height: u64,
    },
    /// the tip's consensus coordinates — the [`SyncRequest::TipCoords`] answer.
    TipCoords(TipCoords),
    Error(String),
    /// the [`SyncRequest::Blob`] answer: the digest's bytes when this node
    /// holds them, `None` when it does not — an honest miss, never an error
    /// (the requester's fan-out treats a miss and an old peer's `Error`
    /// identically: try the next peer).
    Blob {
        bytes: Option<Vec<u8>>,
    },
    /// the [`SyncRequest::BlobInfo`] answer: the blob's total length when
    /// held, `None` on an honest miss.
    BlobInfo {
        len: Option<u64>,
    },
    /// the [`SyncRequest::BlobRange`] answer: the window's bytes when held
    /// (shorter than asked at the blob's tail, empty past it), `None` on an
    /// honest miss.
    BlobRange {
        bytes: Option<Vec<u8>>,
    },
    /// the [`SyncRequest::ForgeObjects`] answer: the digest of the pack this
    /// node staged for the asked head, `None` when it cannot build one (it
    /// does not hold that head's objects either). the requester pulls the
    /// bytes over the ranged blob lane and verifies them against the head.
    ForgeObjects {
        digest: Option<[u8; 32]>,
    },
}

impl SyncResponse {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Manifest(_) => "Manifest",
            Self::Chunk { .. } => "Chunk",
            Self::Module(_) => "Module",
            Self::Frames { .. } => "Frames",
            Self::RangePruned { .. } => "RangePruned",
            Self::IndexOps { .. } => "IndexOps",
            Self::TipCoords(_) => "TipCoords",
            Self::Blob { .. } => "Blob",
            Self::BlobInfo { .. } => "BlobInfo",
            Self::BlobRange { .. } => "BlobRange",
            Self::ForgeObjects { .. } => "ForgeObjects",
            Self::Error(_) => "Error",
        }
    }
}

// ---- frame codec -----------------------------------------------------------

/// an optional `(height, seq)` op-row cursor: presence byte, then 8+4 LE.
fn put_op_cursor(out: &mut Vec<u8>, cursor: &Option<(u64, u32)>) {
    match cursor {
        Some((height, seq)) => {
            out.push(1);
            out.extend_from_slice(&height.to_le_bytes());
            out.extend_from_slice(&seq.to_le_bytes());
        }
        None => out.push(0),
    }
}

fn take_op_cursor(buf: &mut &[u8]) -> Result<Option<(u64, u32)>, WireError> {
    Ok(match wire::take_u8(buf)? {
        0 => None,
        1 => Some((wire::take_u64(buf)?, wire::take_u32(buf)?)),
        t => return Err(WireError::BadTag("op cursor", t)),
    })
}

pub fn encode_request(req: &SyncRequest) -> Vec<u8> {
    let mut out = Vec::new();
    match req {
        SyncRequest::Manifest => out.push(0u8),
        SyncRequest::Chunk {
            boundary,
            module_id,
            offset,
        } => {
            out.push(1u8);
            out.extend_from_slice(&boundary.height.to_le_bytes());
            out.extend_from_slice(boundary.root_hash.as_bytes());
            wire::put_str(&mut out, module_id);
            out.extend_from_slice(&offset.to_le_bytes());
        }
        SyncRequest::Module {
            boundary,
            module_id,
            body,
        } => {
            out.push(2u8);
            out.extend_from_slice(&boundary.height.to_le_bytes());
            out.extend_from_slice(boundary.root_hash.as_bytes());
            wire::put_str(&mut out, module_id);
            wire::put_bytes(&mut out, body);
        }
        SyncRequest::Frames {
            after_height,
            up_to_height,
        } => {
            out.push(3u8);
            out.extend_from_slice(&after_height.to_le_bytes());
            out.extend_from_slice(&up_to_height.to_le_bytes());
        }
        SyncRequest::IndexOps {
            boundary,
            module,
            after,
        } => {
            out.push(4u8);
            out.extend_from_slice(&boundary.to_le_bytes());
            wire::put_str(&mut out, module);
            put_op_cursor(&mut out, after);
        }
        SyncRequest::TipCoords => out.push(5u8),
        SyncRequest::Blob { digest } => {
            out.push(6u8);
            out.extend_from_slice(digest);
        }
        SyncRequest::BlobInfo { digest } => {
            out.push(7u8);
            out.extend_from_slice(digest);
        }
        SyncRequest::BlobRange {
            digest,
            offset,
            len,
        } => {
            out.push(8u8);
            out.extend_from_slice(digest);
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
        }
        SyncRequest::ForgeObjects { repo, head, bases } => {
            out.push(9u8);
            wire::put_str(&mut out, repo);
            out.extend_from_slice(head);
            out.extend_from_slice(&(bases.len() as u32).to_le_bytes());
            for base in bases {
                out.extend_from_slice(base);
            }
        }
    }
    out
}

pub fn decode_request(bytes: &[u8]) -> Result<SyncRequest, WireError> {
    let mut buf = bytes;
    let tag = wire::take_u8(&mut buf)?;
    let req = match tag {
        0 => SyncRequest::Manifest,
        1 => SyncRequest::Chunk {
            boundary: BoundaryId {
                height: wire::take_u64(&mut buf)?,
                root_hash: StateRoot(wire::take_array::<ROOT_LEN>(&mut buf)?),
            },
            module_id: wire::take_str(&mut buf)?,
            offset: wire::take_u64(&mut buf)?,
        },
        2 => SyncRequest::Module {
            boundary: BoundaryId {
                height: wire::take_u64(&mut buf)?,
                root_hash: StateRoot(wire::take_array::<ROOT_LEN>(&mut buf)?),
            },
            module_id: wire::take_str(&mut buf)?,
            body: wire::take_bytes(&mut buf)?.to_vec(),
        },
        3 => SyncRequest::Frames {
            after_height: wire::take_u64(&mut buf)?,
            up_to_height: wire::take_u64(&mut buf)?,
        },
        4 => SyncRequest::IndexOps {
            boundary: wire::take_u64(&mut buf)?,
            module: wire::take_str(&mut buf)?,
            after: take_op_cursor(&mut buf)?,
        },
        5 => SyncRequest::TipCoords,
        6 => SyncRequest::Blob {
            digest: wire::take_array::<32>(&mut buf)?,
        },
        7 => SyncRequest::BlobInfo {
            digest: wire::take_array::<32>(&mut buf)?,
        },
        8 => SyncRequest::BlobRange {
            digest: wire::take_array::<32>(&mut buf)?,
            offset: wire::take_u64(&mut buf)?,
            len: wire::take_u64(&mut buf)?,
        },
        9 => {
            let repo = wire::take_str(&mut buf)?;
            let head = wire::take_array::<FORGE_OID_LEN>(&mut buf)?;
            let count = wire::take_u32(&mut buf)? as usize;
            if count > MAX_FORGE_BASES {
                return Err(WireError::Codec(format!(
                    "forge base count {count} exceeds the {MAX_FORGE_BASES} cap"
                )));
            }
            let mut bases = Vec::with_capacity(count);
            for _ in 0..count {
                bases.push(wire::take_array::<FORGE_OID_LEN>(&mut buf)?);
            }
            SyncRequest::ForgeObjects { repo, head, bases }
        }
        other => return Err(WireError::BadTag("SyncRequest", other)),
    };
    wire::expect_empty(buf)?;
    Ok(req)
}

pub fn encode_response(resp: &SyncResponse) -> Vec<u8> {
    let mut out = Vec::new();
    match resp {
        SyncResponse::Manifest(m) => {
            out.push(0u8);
            out.extend_from_slice(&m.height.to_le_bytes());
            out.extend_from_slice(m.root_hash.as_bytes());
            out.extend_from_slice(&m.epoch.to_le_bytes());
            out.extend_from_slice(&m.view_base.to_le_bytes());
            out.extend_from_slice(&(m.participants.len() as u64).to_le_bytes());
            for p in &m.participants {
                wire::put_bytes(&mut out, p);
            }
            match &m.floor_cert {
                Some(cert) => {
                    out.push(1);
                    wire::put_bytes(&mut out, cert);
                }
                None => out.push(0),
            }
            out.extend_from_slice(&(m.entries.len() as u64).to_le_bytes());
            for e in &m.entries {
                wire::put_str(&mut out, &e.module_id);
                out.extend_from_slice(e.root.as_bytes());
                match &e.code_hash {
                    Some(hash) => {
                        out.push(1);
                        wire::put_bytes(&mut out, hash);
                    }
                    None => out.push(0),
                }
                out.push(e.kind.to_u8());
                match &e.resolver_target {
                    Some(target) => {
                        out.push(1);
                        out.extend_from_slice(target.root.as_ref());
                        out.extend_from_slice(&target.start.to_le_bytes());
                        out.extend_from_slice(&target.op_count.to_le_bytes());
                    }
                    None => out.push(0),
                }
            }
            out.extend_from_slice(&(m.residents.len() as u64).to_le_bytes());
            for o in &m.residents {
                wire::put_bytes(&mut out, o);
            }
            out.extend_from_slice(&(m.applied_frames.len() as u64).to_le_bytes());
            for (height, id) in &m.applied_frames {
                out.extend_from_slice(&height.to_le_bytes());
                out.extend_from_slice(id);
            }
            match m.pending_cutover_view {
                Some(view) => {
                    out.push(1);
                    out.extend_from_slice(&view.to_le_bytes());
                }
                None => out.push(0),
            }
        }
        SyncResponse::Chunk { total, bytes } => {
            out.push(1u8);
            out.extend_from_slice(&total.to_le_bytes());
            wire::put_bytes(&mut out, bytes);
        }
        SyncResponse::Module(bytes) => {
            out.push(2u8);
            wire::put_bytes(&mut out, bytes);
        }
        SyncResponse::Frames { frames } => {
            out.push(3u8);
            out.extend_from_slice(&(frames.len() as u64).to_le_bytes());
            for frame in frames {
                out.extend_from_slice(&frame.height.to_le_bytes());
                wire::put_bytes(&mut out, &frame.frame);
                out.push(frame.disposition.to_u8());
                out.extend_from_slice(&(frame.roots.len() as u64).to_le_bytes());
                for (module_id, root) in &frame.roots {
                    wire::put_str(&mut out, module_id);
                    out.extend_from_slice(root.as_bytes());
                }
                out.extend_from_slice(frame.root_hash.as_bytes());
            }
        }
        SyncResponse::RangePruned {
            requested_after,
            retained_from,
        } => {
            out.push(4u8);
            out.extend_from_slice(&requested_after.to_le_bytes());
            out.extend_from_slice(&retained_from.to_le_bytes());
        }
        SyncResponse::Error(msg) => {
            out.push(5u8);
            wire::put_str(&mut out, msg);
        }
        SyncResponse::IndexOps {
            rows,
            next_after,
            source_floor,
            applied_height,
        } => {
            out.push(6u8);
            out.extend_from_slice(&(rows.len() as u64).to_le_bytes());
            for (key, value) in rows {
                wire::put_str(&mut out, key);
                wire::put_bytes(&mut out, value);
            }
            put_op_cursor(&mut out, next_after);
            match source_floor {
                Some(floor) => {
                    out.push(1);
                    out.extend_from_slice(&floor.to_le_bytes());
                }
                None => out.push(0),
            }
            out.extend_from_slice(&applied_height.to_le_bytes());
        }
        SyncResponse::Blob { bytes } => {
            out.push(8u8);
            match bytes {
                Some(b) => {
                    out.push(1);
                    wire::put_bytes(&mut out, b);
                }
                None => out.push(0),
            }
        }
        SyncResponse::BlobInfo { len } => {
            out.push(9u8);
            match len {
                Some(l) => {
                    out.push(1);
                    out.extend_from_slice(&l.to_le_bytes());
                }
                None => out.push(0),
            }
        }
        SyncResponse::BlobRange { bytes } => {
            out.push(10u8);
            match bytes {
                Some(b) => {
                    out.push(1);
                    wire::put_bytes(&mut out, b);
                }
                None => out.push(0),
            }
        }
        SyncResponse::ForgeObjects { digest } => {
            out.push(11u8);
            match digest {
                Some(d) => {
                    out.push(1);
                    out.extend_from_slice(d);
                }
                None => out.push(0),
            }
        }
        SyncResponse::TipCoords(c) => {
            out.push(7u8);
            out.extend_from_slice(&c.height.to_le_bytes());
            out.extend_from_slice(c.root_hash.as_bytes());
            out.extend_from_slice(&c.epoch.to_le_bytes());
            out.extend_from_slice(&c.view_base.to_le_bytes());
            out.extend_from_slice(&(c.participants.len() as u64).to_le_bytes());
            for p in &c.participants {
                wire::put_bytes(&mut out, p);
            }
            out.extend_from_slice(&(c.residents.len() as u64).to_le_bytes());
            for r in &c.residents {
                wire::put_bytes(&mut out, r);
            }
            out.push(u8::from(c.has_floor));
            out.extend_from_slice(&c.generation.to_le_bytes());
            out.extend_from_slice(&(c.mesh_window.len() as u64).to_le_bytes());
            for entry in &c.mesh_window {
                out.extend_from_slice(&entry.generation.to_le_bytes());
                out.extend_from_slice(&(entry.validators.len() as u64).to_le_bytes());
                for v in &entry.validators {
                    wire::put_bytes(&mut out, v);
                }
                out.extend_from_slice(&(entry.residents.len() as u64).to_le_bytes());
                for r in &entry.residents {
                    wire::put_bytes(&mut out, r);
                }
            }
            match &c.build {
                Some(build) => {
                    out.push(1);
                    wire::put_str(&mut out, build);
                }
                None => out.push(0),
            }
        }
    }
    out
}

/// exact encoded body length of a Frames response.
///
/// Add RPC_HEADER_LEN for the complete mesh message. Saturating
/// arithmetic makes an impossible aggregate overflow fail closed at any
/// caller comparing the result with a transport budget.
pub fn encoded_frames_response_len(frames: &[FinalizedFrame]) -> usize {
    const TAG_LEN: usize = 1;
    const U64_LEN: usize = 8;
    const DISPOSITION_LEN: usize = 1;

    frames.iter().fold(TAG_LEN + U64_LEN, |len, frame| {
        let roots_len = frame.roots.iter().fold(0usize, |len, (module_id, _)| {
            len.saturating_add(U64_LEN)
                .saturating_add(module_id.len())
                .saturating_add(ROOT_LEN)
        });
        len.saturating_add(U64_LEN)
            .saturating_add(U64_LEN)
            .saturating_add(frame.frame.len())
            .saturating_add(DISPOSITION_LEN)
            .saturating_add(U64_LEN)
            .saturating_add(roots_len)
            .saturating_add(ROOT_LEN)
    })
}

/// exact encoded body length of an [`SyncResponse::IndexOps`] page carrying
/// `rows`, with both optional tails PRESENT — the conservative shape, so a
/// serve path that binary-searches this against a transport budget can never
/// pick a prefix the real encode then overflows.
///
/// Add RPC_HEADER_LEN for the complete mesh message. Saturating
/// arithmetic makes an impossible aggregate overflow fail closed.
pub fn encoded_index_ops_response_len(rows: &[(String, Vec<u8>)]) -> usize {
    const TAG_LEN: usize = 1;
    const U64_LEN: usize = 8;
    // presence byte + (height, seq), presence byte + floor, applied_height.
    const TAIL_LEN: usize = 1 + 8 + 4 + 1 + 8 + 8;

    rows.iter()
        .fold(TAG_LEN + U64_LEN + TAIL_LEN, |len, (key, value)| {
            len.saturating_add(U64_LEN)
                .saturating_add(key.len())
                .saturating_add(U64_LEN)
                .saturating_add(value.len())
        })
}

pub fn decode_response(bytes: &[u8]) -> Result<SyncResponse, WireError> {
    let mut buf = bytes;
    let tag = wire::take_u8(&mut buf)?;
    let resp = match tag {
        0 => {
            let height = wire::take_u64(&mut buf)?;
            let root_hash = StateRoot(wire::take_array::<ROOT_LEN>(&mut buf)?);
            let epoch = wire::take_u64(&mut buf)?;
            let view_base = wire::take_u64(&mut buf)?;
            let p = wire::take_u64(&mut buf)?;
            // each participant costs at least its 8-byte length prefix, so a
            // forged count can never drive allocation past the buffer.
            if p > (buf.len() / 8) as u64 {
                return Err(WireError::Codec(format!(
                    "participant count {p} exceeds the {} remaining bytes",
                    buf.len()
                )));
            }
            let mut participants = Vec::with_capacity(p as usize);
            for _ in 0..p {
                participants.push(wire::take_bytes(&mut buf)?.to_vec());
            }
            let floor_cert = match wire::take_u8(&mut buf)? {
                0 => None,
                1 => Some(wire::take_bytes(&mut buf)?.to_vec()),
                t => return Err(WireError::BadTag("floor_cert", t)),
            };
            let n = wire::take_u64(&mut buf)?;
            // each entry costs at least its id length prefix + root + kind, so
            // a forged count can never drive allocation past the buffer.
            if n > (buf.len() / (8 + ROOT_LEN + 1)) as u64 {
                return Err(WireError::Codec(format!(
                    "manifest count {n} exceeds the {} remaining bytes",
                    buf.len()
                )));
            }
            let mut entries = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let module_id = wire::take_str(&mut buf)?;
                let root = StateRoot(wire::take_array::<ROOT_LEN>(&mut buf)?);
                let code_hash = match wire::take_u8(&mut buf)? {
                    0 => None,
                    1 => {
                        let hash = wire::take_bytes(&mut buf)?;
                        let valid_hash = hash.len() == ROOT_LEN;
                        if !valid_hash {
                            return Err(WireError::Codec(
                                "module code hash must be 32 bytes".into(),
                            ));
                        }
                        Some(hash.to_vec())
                    }
                    t => return Err(WireError::BadTag("module code hash", t)),
                };
                let kind = PayloadKind::from_u8(wire::take_u8(&mut buf)?)?;
                let resolver_target = match wire::take_u8(&mut buf)? {
                    0 => None,
                    1 => {
                        let raw = wire::take_array::<ROOT_LEN>(&mut buf)?;
                        let digest = qmdb::SyncDigest::decode(raw.as_ref())
                            .map_err(|e| WireError::Codec(format!("resolver target root: {e}")))?;
                        Some(ResolverTarget {
                            root: digest,
                            start: wire::take_u64(&mut buf)?,
                            op_count: wire::take_u64(&mut buf)?,
                        })
                    }
                    t => return Err(WireError::BadTag("resolver_target", t)),
                };
                entries.push(ManifestEntry {
                    module_id,
                    root,
                    code_hash,
                    kind,
                    resolver_target,
                });
            }
            let o = wire::take_u64(&mut buf)?;
            // each resident costs at least its 8-byte length prefix, so a
            // forged count can never drive allocation past the buffer.
            if o > (buf.len() / 8) as u64 {
                return Err(WireError::Codec(format!(
                    "resident count {o} exceeds the {} remaining bytes",
                    buf.len()
                )));
            }
            let mut residents = Vec::with_capacity(o as usize);
            for _ in 0..o {
                residents.push(wire::take_bytes(&mut buf)?.to_vec());
            }
            let w = wire::take_u64(&mut buf)?;
            // the protocol depth is the cap, and a forged count can never
            // drive allocation past it (each pair is a fixed 40 bytes).
            if w > MAX_APPLIED_FRAMES as u64 {
                return Err(WireError::Codec(format!(
                    "replay window count {w} exceeds the {MAX_APPLIED_FRAMES}-entry cap"
                )));
            }
            let mut applied_frames = Vec::with_capacity(w as usize);
            for _ in 0..w {
                let height = wire::take_u64(&mut buf)?;
                applied_frames.push((height, wire::take_array::<32>(&mut buf)?));
            }
            let pending_cutover_view = match wire::take_u8(&mut buf)? {
                0 => None,
                1 => Some(wire::take_u64(&mut buf)?),
                t => return Err(WireError::BadTag("pending_cutover_view", t)),
            };
            SyncResponse::Manifest(Manifest {
                height,
                root_hash,
                epoch,
                view_base,
                participants,
                residents,
                floor_cert,
                entries,
                applied_frames,
                pending_cutover_view,
            })
        }
        1 => SyncResponse::Chunk {
            total: wire::take_u64(&mut buf)?,
            bytes: wire::take_bytes(&mut buf)?.to_vec(),
        },
        2 => SyncResponse::Module(wire::take_bytes(&mut buf)?.to_vec()),
        3 => {
            let n = wire::take_u64(&mut buf)?;
            if n > FRAME_BATCH_LEN as u64 {
                return Err(WireError::Codec(format!(
                    "frame batch count {n} exceeds cap {FRAME_BATCH_LEN}"
                )));
            }
            let mut frames = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let height = wire::take_u64(&mut buf)?;
                let frame = wire::take_bytes(&mut buf)?.to_vec();
                let disposition = FrameDisposition::from_u8(wire::take_u8(&mut buf)?)?;
                let roots_len = wire::take_u64(&mut buf)?;
                if roots_len > (buf.len() / (8 + ROOT_LEN)) as u64 {
                    return Err(WireError::Codec(format!(
                        "root count {roots_len} exceeds the {} remaining bytes",
                        buf.len()
                    )));
                }
                let mut roots = Vec::with_capacity(roots_len as usize);
                for _ in 0..roots_len {
                    roots.push((
                        wire::take_str(&mut buf)?,
                        StateRoot(wire::take_array::<ROOT_LEN>(&mut buf)?),
                    ));
                }
                let root_hash = StateRoot(wire::take_array::<ROOT_LEN>(&mut buf)?);
                frames.push(FinalizedFrame {
                    height,
                    frame,
                    disposition,
                    roots,
                    root_hash,
                });
            }
            SyncResponse::Frames { frames }
        }
        4 => SyncResponse::RangePruned {
            requested_after: wire::take_u64(&mut buf)?,
            retained_from: wire::take_u64(&mut buf)?,
        },
        5 => SyncResponse::Error(wire::take_str(&mut buf)?),
        6 => {
            let n = wire::take_u64(&mut buf)?;
            if n > INDEX_OPS_BATCH_LEN as u64 {
                return Err(WireError::Codec(format!(
                    "index op page count {n} exceeds cap {INDEX_OPS_BATCH_LEN}"
                )));
            }
            // each row costs at least its key + value length prefixes, so a
            // forged count can never drive allocation past the buffer.
            if n > (buf.len() / 16) as u64 {
                return Err(WireError::Codec(format!(
                    "index op row count {n} exceeds the {} remaining bytes",
                    buf.len()
                )));
            }
            let mut rows = Vec::with_capacity(n as usize);
            for _ in 0..n {
                rows.push((
                    wire::take_str(&mut buf)?,
                    wire::take_bytes(&mut buf)?.to_vec(),
                ));
            }
            let next_after = take_op_cursor(&mut buf)?;
            let source_floor = match wire::take_u8(&mut buf)? {
                0 => None,
                1 => Some(wire::take_u64(&mut buf)?),
                t => return Err(WireError::BadTag("source floor presence", t)),
            };
            SyncResponse::IndexOps {
                rows,
                next_after,
                source_floor,
                applied_height: wire::take_u64(&mut buf)?,
            }
        }
        7 => {
            let height = wire::take_u64(&mut buf)?;
            let root_hash = StateRoot(wire::take_array::<ROOT_LEN>(&mut buf)?);
            let epoch = wire::take_u64(&mut buf)?;
            let view_base = wire::take_u64(&mut buf)?;
            let p = wire::take_u64(&mut buf)?;
            // each key costs at least its 8-byte length prefix, so a forged
            // count can never drive allocation past the buffer.
            if p > (buf.len() / 8) as u64 {
                return Err(WireError::Codec(format!(
                    "participant count {p} exceeds the {} remaining bytes",
                    buf.len()
                )));
            }
            let mut participants = Vec::with_capacity(p as usize);
            for _ in 0..p {
                participants.push(wire::take_bytes(&mut buf)?.to_vec());
            }
            let r = wire::take_u64(&mut buf)?;
            if r > (buf.len() / 8) as u64 {
                return Err(WireError::Codec(format!(
                    "resident count {r} exceeds the {} remaining bytes",
                    buf.len()
                )));
            }
            let mut residents = Vec::with_capacity(r as usize);
            for _ in 0..r {
                residents.push(wire::take_bytes(&mut buf)?.to_vec());
            }
            let has_floor = match wire::take_u8(&mut buf)? {
                0 => false,
                1 => true,
                t => return Err(WireError::BadTag("has_floor", t)),
            };
            let generation = wire::take_u64(&mut buf)?;
            let entries = wire::take_u64(&mut buf)?;
            if entries > (buf.len() / 8) as u64 {
                return Err(WireError::Codec(format!(
                    "mesh window count {entries} exceeds the {} remaining bytes",
                    buf.len()
                )));
            }
            let mut mesh_window = Vec::with_capacity(entries as usize);
            for _ in 0..entries {
                let entry_generation = wire::take_u64(&mut buf)?;
                let v = wire::take_u64(&mut buf)?;
                if v > (buf.len() / 8) as u64 {
                    return Err(WireError::Codec(format!(
                        "window validator count {v} exceeds the {} remaining bytes",
                        buf.len()
                    )));
                }
                let mut validators = Vec::with_capacity(v as usize);
                for _ in 0..v {
                    validators.push(wire::take_bytes(&mut buf)?.to_vec());
                }
                let r = wire::take_u64(&mut buf)?;
                if r > (buf.len() / 8) as u64 {
                    return Err(WireError::Codec(format!(
                        "window resident count {r} exceeds the {} remaining bytes",
                        buf.len()
                    )));
                }
                let mut residents = Vec::with_capacity(r as usize);
                for _ in 0..r {
                    residents.push(wire::take_bytes(&mut buf)?.to_vec());
                }
                mesh_window.push(MeshWindowEntry {
                    generation: entry_generation,
                    validators,
                    residents,
                });
            }
            let build = match wire::take_u8(&mut buf)? {
                0 => None,
                1 => Some(wire::take_str(&mut buf)?),
                t => return Err(WireError::BadTag("tip build presence", t)),
            };
            SyncResponse::TipCoords(TipCoords {
                height,
                root_hash,
                epoch,
                view_base,
                participants,
                residents,
                has_floor,
                generation,
                mesh_window,
                build,
            })
        }
        8 => SyncResponse::Blob {
            bytes: match wire::take_u8(&mut buf)? {
                0 => None,
                1 => Some(wire::take_bytes(&mut buf)?.to_vec()),
                t => return Err(WireError::BadTag("blob presence", t)),
            },
        },
        9 => SyncResponse::BlobInfo {
            len: match wire::take_u8(&mut buf)? {
                0 => None,
                1 => Some(wire::take_u64(&mut buf)?),
                t => return Err(WireError::BadTag("blob info presence", t)),
            },
        },
        10 => SyncResponse::BlobRange {
            bytes: match wire::take_u8(&mut buf)? {
                0 => None,
                1 => Some(wire::take_bytes(&mut buf)?.to_vec()),
                t => return Err(WireError::BadTag("blob range presence", t)),
            },
        },
        11 => SyncResponse::ForgeObjects {
            digest: match wire::take_u8(&mut buf)? {
                0 => None,
                1 => Some(wire::take_array::<32>(&mut buf)?),
                t => return Err(WireError::BadTag("forge objects presence", t)),
            },
        },
        other => return Err(WireError::BadTag("SyncResponse", other)),
    };
    wire::expect_empty(buf)?;
    Ok(resp)
}

/// the ed25519 signing namespace for the statesync standing proof.
/// a client signs this namespace over the network's genesis namespace bytes
/// ONCE at construction; every request carries the result as its proof.
pub const SYNC_AUTH_NAMESPACE: &[u8] = b"ducktape-statesync-auth-v1";

/// the AUTHENTICATED rpc envelope (fail-closed):
/// `requester(32) ‖ proof(64) ‖
/// id(8 LE) ‖ body`. the codec only FRAMES bytes; the caller produces the
/// proof ([`sign_sync_proof`]) and the server verifies it ([`verify_sync_proof`])
/// against committed standing. `id` is requester-local (correlates replies).
/// server replies reuse the same frame with the auth fields zero-filled — the
/// client gates replies by transport peer and root-verifies payloads, so a
/// reply's requester/proof are never inspected.
pub fn encode_rpc(requester: &[u8; 32], proof: &[u8; 64], id: u64, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(RPC_HEADER_LEN + body.len());
    out.extend_from_slice(requester);
    out.extend_from_slice(proof);
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// decode the authenticated envelope into borrowed `(requester, proof, id, body)`.
/// errors `Truncated` on any buffer shorter than the 32+64+8 fixed header.
// the 4-tuple mirrors the fixed wire layout; a named type would just indirect it.
#[allow(clippy::type_complexity)]
pub fn decode_rpc(bytes: &[u8]) -> Result<(&[u8; 32], &[u8; 64], u64, &[u8]), WireError> {
    let (requester, rest) = bytes
        .split_first_chunk::<32>()
        .ok_or(WireError::Truncated)?;
    let (proof, rest) = rest.split_first_chunk::<64>().ok_or(WireError::Truncated)?;
    let (id_bytes, body) = rest.split_first_chunk::<8>().ok_or(WireError::Truncated)?;
    Ok((requester, proof, u64::from_le_bytes(*id_bytes), body))
}

/// sign the standing proof: the caller's real key signs [`SYNC_AUTH_NAMESPACE`]
/// over the genesis `namespace` bytes. returns `(requester_pubkey, proof)` to
/// attach to every request. sound as a STATIC per-session proof because the
/// mesh transport is authenticated+encrypted (the proof is not wire-capturable)
/// and a pre-admission joiner can only sign for its own non-standing key.
pub fn sign_sync_proof(
    signer: &commonware_cryptography::ed25519::PrivateKey,
    namespace: &[u8],
) -> ([u8; 32], [u8; 64]) {
    use commonware_codec::Encode as _;
    use commonware_cryptography::Signer as _;
    let requester: [u8; 32] = signer
        .public_key()
        .as_ref()
        .try_into()
        .expect("ed25519 public key is 32 bytes");
    let sig = signer.sign(SYNC_AUTH_NAMESPACE, namespace);
    let proof: [u8; 64] = sig
        .encode()
        .as_ref()
        .try_into()
        .expect("ed25519 signature is 64 bytes");
    (requester, proof)
}

/// verify a standing proof: `requester` must have signed [`SYNC_AUTH_NAMESPACE`]
/// over `namespace`. a malformed key/signature verifies as `false` (fail-closed).
/// standing (requester ∈ members ∪ residents) is a SEPARATE check by the server.
pub fn verify_sync_proof(requester: &[u8; 32], proof: &[u8; 64], namespace: &[u8]) -> bool {
    use commonware_cryptography::{Verifier as _, ed25519};
    let Ok(pk) = ed25519::PublicKey::decode(requester.as_slice()) else {
        return false;
    };
    let Ok(sig) = ed25519::Signature::decode(proof.as_slice()) else {
        return false;
    };
    pk.verify(SYNC_AUTH_NAMESPACE, namespace, &sig)
}

// ============================================================================
// SERVER — the capture cache a running node answers from.
// ============================================================================

/// one captured module payload.
#[derive(Debug, Clone)]
enum CapturedPayload {
    Stateless,
    Snapshot(Vec<u8>),
    /// the module serves its own qmdb resolver lane live; the capture records the
    /// pinned op-range target (and the boundary root, in the entry).
    Resolver(ResolverTarget),
    /// an object-store resolver (duckfs-odb): the module serves its refs image
    /// and content-addressed objects live over `serve_sync`, with NO qmdb
    /// op-range target to pin — the boundary root in the entry is all the joiner
    /// needs to root-verify the refs it fetches over the same Module lane.
    ObjectResolver,
    Unsupported,
}

impl CapturedPayload {
    fn kind(&self) -> PayloadKind {
        match self {
            Self::Stateless => PayloadKind::Stateless,
            Self::Snapshot(_) => PayloadKind::Snapshot,
            // both resolver flavors advertise the same wire kind; a joiner
            // distinguishes them by whether the entry pins a qmdb target
            // (`resolver_target: Some` for qmdb, `None` for an object resolver).
            Self::Resolver(_) | Self::ObjectResolver => PayloadKind::Resolver,
            Self::Unsupported => PayloadKind::Unsupported,
        }
    }
}

#[derive(Debug, Clone)]
struct CapturedModule {
    root: StateRoot,
    code_hash: Option<Vec<u8>>,
    payload: CapturedPayload,
}

/// consensus coordinates of a served boundary — captured WITH the module
/// payloads so a later manifest request for the same height serves one
/// consistent picture. the caller (the node's pump, which owns both the host
/// and the consensus wiring) supplies them per request; the floor-cert
/// contract is the caller's: pass it only when it certifies exactly the
/// current finalized height.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BoundaryCoords {
    pub epoch: u64,
    pub view_base: u64,
    pub participants: Vec<Vec<u8>>,
    pub residents: Vec<Vec<u8>>,
    pub floor_cert: Option<Vec<u8>>,
    /// the committed membership generation and its retained window — the
    /// mesh's index namespace; see [`TipCoords::generation`].
    pub generation: u64,
    pub mesh_window: Vec<MeshWindowEntry>,
    /// the serving node's replay window at this boundary, filtered to entries
    /// at or below its height — see [`Manifest::applied_frames`].
    pub applied_frames: Vec<(u64, [u8; 32])>,
    /// the caller's own orchestrator's armed cutover, if any — rides straight
    /// into [`Manifest::pending_cutover_view`].
    pub pending_cutover_view: Option<u64>,
}

/// a consistent boundary capture: every payload from ONE finalized boundary.
#[derive(Debug, Clone)]
struct Capture {
    root_hash: StateRoot,
    coords: BoundaryCoords,
    modules: BTreeMap<ModuleId, CapturedModule>,
}

/// a capture produced by the state owner, ready to install into a
/// [`SyncServer`] — the request/reply payload that lets serving run on a
/// different task than the host. opaque: only [`capture_boundary`] builds one
/// and only [`SyncServer::install_capture`] consumes it.
#[derive(Debug, Clone)]
pub struct CaptureData {
    root_hash: StateRoot,
    coords: BoundaryCoords,
    modules: BTreeMap<ModuleId, CapturedModule>,
}

/// capture the host's state at `finalized` — the STATE-OWNER half of serving:
/// the one call that must run on the task that owns the [`Host`]. the returned
/// [`CaptureData`] crosses to the serve task and installs via
/// [`SyncServer::install_capture`].
pub async fn capture_boundary(
    host: &Host,
    finalized: FinalizedBlock,
    coords: &BoundaryCoords,
) -> Result<(BoundaryId, CaptureData), String> {
    let snapshot = host
        .capture_finalized_snapshot(finalized)
        .map_err(|e| format!("capture failed: {e}"))?;
    let id = BoundaryId {
        height: finalized.height,
        root_hash: snapshot.root_hash,
    };
    let mut modules = BTreeMap::new();
    for m in snapshot.modules {
        let payload = match m.state_sync {
            StateSyncHandle::Stateless => CapturedPayload::Stateless,
            StateSyncHandle::SnapshotBytes(bytes) => CapturedPayload::Snapshot(bytes),
            // an object-store resolver has no qmdb op-range target to pin: its
            // refs image + content-addressed objects are served live over
            // `serve_sync`, root-verified by the joiner against the boundary
            // root already recorded in the entry. keep the qmdb arm below
            // UNCHANGED.
            StateSyncHandle::ResolverBacked { backend, .. } if backend == "duckfs-odb" => {
                CapturedPayload::ObjectResolver
            }
            StateSyncHandle::ResolverBacked { .. } => {
                let target = host
                    .resolver_sync_target(&m.id)
                    .await
                    .map_err(|e| format!("module {} sync target: {e}", m.id))?;
                if target.root != m.root {
                    return Err(format!(
                        "module {} resolver target root does not match boundary root",
                        m.id
                    ));
                }
                CapturedPayload::Resolver(ResolverTarget {
                    root: commonware_cryptography::sha256::Digest(target.root.0),
                    start: target.start,
                    op_count: target.op_count,
                })
            }
            StateSyncHandle::Unsupported { .. } => CapturedPayload::Unsupported,
        };
        modules.insert(
            m.id,
            CapturedModule {
                root: m.root,
                code_hash: m.code_hash,
                payload,
            },
        );
    }
    // a module that could not prepare a handle at all serves as Unsupported —
    // the SAME thing a joiner is told about a module that declares no sync
    // surface, because from the joiner's side they are the same fact. it is
    // reported PER MODULE and the rest of the boundary still transfers: one
    // module's bad state must not make this node unable to admit anyone.
    for m in snapshot.degraded {
        modules.insert(
            m.id,
            CapturedModule {
                root: m.root,
                code_hash: m.code_hash,
                payload: CapturedPayload::Unsupported,
            },
        );
    }
    Ok((
        id,
        CaptureData {
            root_hash: snapshot.root_hash,
            coords: coords.clone(),
            modules,
        },
    ))
}

/// what a [`SyncServer::serve`] step needs from its driver: either the answer
/// itself, or one of the STATE TOUCHES only the host-owning task can make —
/// the request/reply seam that keeps serving off the consensus loop.
#[derive(Debug)]
pub enum ServeStep {
    /// the request resolved from served state alone.
    Reply(SyncResponse),
    /// Manifest: obtain the current finalized boundary from the state owner
    /// ([`capture_boundary`] there unless this server already holds the id —
    /// see [`SyncServer::known_boundaries`]), install/refresh it, then finish
    /// with [`SyncServer::manifest_for`].
    NeedBoundary,
    /// Module lane, checks passed: route `body` to the live host's
    /// `serve_sync` and wrap the bytes in [`SyncResponse::Module`].
    NeedModuleServe { module_id: ModuleId, body: Vec<u8> },
    /// Frames lane: read the recovery journal on the state owner.
    NeedFrames {
        after_height: u64,
        up_to_height: u64,
    },
    /// IndexOps lane: read one page of the module's stored index op rows on
    /// the state owner (which owns the derived index; this crate deliberately
    /// does not).
    NeedIndexOps {
        boundary: u64,
        module: ModuleId,
        after: Option<(u64, u32)>,
    },
    /// TipCoords: read the tip's consensus coordinates from the state owner —
    /// no capture, no lease, no floor-cert alignment gate.
    NeedCoords,
}

/// the server side of the protocol: capture consistent boundary views on
/// demand, cache a few, and answer manifest/chunk requests from them; route
/// module-lane requests to the live host. hold one per node; drive it from the
/// same task that owns the host (answers between drains are automatically
/// consistent — no locks, no torn reads).
#[derive(Default)]
pub struct SyncServer {
    captures: BTreeMap<BoundaryId, Capture>,
    leased: BTreeMap<BoundaryId, u64>,
    lease_clock: u64,
}

impl SyncServer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lease(&mut self, id: BoundaryId) {
        self.touch_lease(id);
        self.release_leased_overflow();
        self.evict_unleased_overflow();
    }

    /// every installed capture's id — the `known` list a boundary request
    /// carries (bounded by [`MAX_CAPTURES`]).
    pub fn known_boundaries(&self) -> Vec<BoundaryId> {
        self.captures.keys().copied().collect()
    }

    /// install a state-owner-produced capture (evicting past the cache cap),
    /// or — when `id` is already held — refresh its consensus coordinates:
    /// an epoch cutover at a stalled boundary (a 1->2 admission is the
    /// canonical case) changes the coordinates without changing
    /// (height, root_hash), and a capture taken just before the cutover would
    /// otherwise serve its stale epoch/participants forever.
    pub fn install_capture(&mut self, id: BoundaryId, data: CaptureData) {
        match self.captures.get_mut(&id) {
            Some(capture) => {
                if capture.coords != data.coords {
                    capture.coords = data.coords;
                }
            }
            None => {
                self.captures.insert(
                    id,
                    Capture {
                        root_hash: data.root_hash,
                        coords: data.coords,
                        modules: data.modules,
                    },
                );
                // spare the newborn: it is not leased until the manifest_for
                // that every install exists to answer, so when every older
                // capture holds a lease (leases never release, they only age
                // out by overflow) the newborn would be the sole eviction
                // candidate — evicting itself one step before its own
                // manifest lookup ("no capture at boundary N").
                self.evict_unleased_overflow_sparing(Some(id));
            }
        }
    }

    /// refresh a held capture's consensus coordinates without new payload —
    /// the known-boundary half of [`SyncServer::install_capture`].
    pub fn refresh_coords(&mut self, id: BoundaryId, coords: BoundaryCoords) {
        if let Some(capture) = self.captures.get_mut(&id)
            && capture.coords != coords
        {
            capture.coords = coords;
        }
    }

    /// lease `id` and build its manifest — the serve-side finish of a
    /// Manifest request, after the boundary round-trip installed/refreshed
    /// the capture.
    pub fn manifest_for(&mut self, id: BoundaryId) -> Result<SyncResponse, String> {
        self.lease(id);
        let capture = self
            .captures
            .get(&id)
            .ok_or_else(|| format!("no capture at boundary {} (refetch manifest)", id.height))?;
        Ok(SyncResponse::Manifest(Manifest {
            height: id.height,
            root_hash: capture.root_hash,
            epoch: capture.coords.epoch,
            view_base: capture.coords.view_base,
            participants: capture.coords.participants.clone(),
            residents: capture.coords.residents.clone(),
            floor_cert: capture.coords.floor_cert.clone(),
            applied_frames: capture.coords.applied_frames.clone(),
            pending_cutover_view: capture.coords.pending_cutover_view,
            entries: capture
                .modules
                .iter()
                .map(|(id, m)| ManifestEntry {
                    module_id: id.clone(),
                    root: m.root,
                    code_hash: m.code_hash.clone(),
                    kind: m.payload.kind(),
                    resolver_target: match &m.payload {
                        CapturedPayload::Resolver(target) => Some(target.clone()),
                        _ => None,
                    },
                })
                .collect(),
        }))
    }

    /// one PURE serve step: answer what served state can, and name the state
    /// touch the driver must make otherwise (see [`ServeStep`]). this is the
    /// whole protocol minus the host — a serve task drives it off the
    /// consensus loop, round-tripping the named touches to the state owner.
    pub fn serve(&mut self, req: SyncRequest) -> ServeStep {
        let step = self.try_serve(req);
        match step {
            Ok(step) => step,
            Err(msg) => ServeStep::Reply(SyncResponse::Error(msg)),
        }
    }

    fn try_serve(&mut self, req: SyncRequest) -> Result<ServeStep, String> {
        Ok(match req {
            SyncRequest::Manifest => ServeStep::NeedBoundary,
            SyncRequest::TipCoords => ServeStep::NeedCoords,
            // the HOST layer answers blob fetches from its node-local store
            // BEFORE requests reach this server; one arriving here means the
            // host did not intercept — answer honestly instead of wedging.
            // (the lane was briefly retired with the prompt plane; the wasm
            // code-distribution plane is its new, live consumer.)
            SyncRequest::Blob { .. }
            | SyncRequest::BlobInfo { .. }
            | SyncRequest::BlobRange { .. } => {
                return Err("blob requests are answered by the host layer".into());
            }
            // likewise the forge object lane: the host builds the pack from
            // its own git substrate, which this server cannot see.
            SyncRequest::ForgeObjects { .. } => {
                return Err("forge object requests are answered by the host layer".into());
            }
            SyncRequest::Frames {
                after_height,
                up_to_height,
            } => ServeStep::NeedFrames {
                after_height,
                up_to_height,
            },
            SyncRequest::Chunk {
                boundary,
                module_id,
                offset,
            } => {
                let capture = self.leased_capture(boundary)?;
                let module = capture.modules.get(&module_id).ok_or_else(|| {
                    format!("no module {module_id} in capture {}", boundary.height)
                })?;
                let CapturedPayload::Snapshot(bytes) = &module.payload else {
                    return Err(format!("module {module_id} has no snapshot payload"));
                };
                let total = bytes.len() as u64;
                if offset > total {
                    return Err(format!(
                        "offset {offset} past the {total}-byte snapshot of {module_id}"
                    ));
                }
                let start = offset as usize;
                let end = (start + CHUNK_LEN).min(bytes.len());
                ServeStep::Reply(SyncResponse::Chunk {
                    total,
                    bytes: bytes[start..end].to_vec(),
                })
            }
            SyncRequest::Module {
                boundary,
                module_id,
                body,
            } => {
                let capture = self.leased_capture(boundary)?;
                let module = capture.modules.get(&module_id).ok_or_else(|| {
                    format!("no module {module_id} in capture {}", boundary.height)
                })?;
                // both resolver flavors serve their bytes live through the
                // module's `serve_sync` (qmdb op ranges, or duckfs refs/objects).
                if !matches!(
                    module.payload,
                    CapturedPayload::Resolver(_) | CapturedPayload::ObjectResolver
                ) {
                    return Err(format!("module {module_id} has no resolver payload"));
                }
                ServeStep::NeedModuleServe { module_id, body }
            }
            SyncRequest::IndexOps {
                boundary,
                module,
                after,
            } => ServeStep::NeedIndexOps {
                boundary,
                module,
                after,
            },
        })
    }

    #[doc(hidden)]
    pub fn insert_capture_for_test(&mut self, id: BoundaryId) {
        self.captures.insert(
            id,
            Capture {
                root_hash: id.root_hash,
                coords: BoundaryCoords::default(),
                modules: BTreeMap::new(),
            },
        );
    }

    /// like [`SyncServer::insert_capture_for_test`] but through the REAL
    /// install path, eviction included — for tests exercising install-time
    /// cache behavior.
    #[doc(hidden)]
    pub fn install_capture_for_test(&mut self, id: BoundaryId) {
        self.install_capture(
            id,
            CaptureData {
                root_hash: id.root_hash,
                coords: BoundaryCoords::default(),
                modules: BTreeMap::new(),
            },
        );
    }

    #[doc(hidden)]
    pub fn has_capture(&self, id: BoundaryId) -> bool {
        self.captures.contains_key(&id)
    }

    #[doc(hidden)]
    pub fn leased_count_for_test(&self) -> usize {
        self.leased.len()
    }

    #[doc(hidden)]
    pub fn is_leased_for_test(&self, id: BoundaryId) -> bool {
        self.leased.contains_key(&id)
    }

    /// handle one decoded request. `finalized` is the node's latest applied
    /// boundary (None before the first block) and `coords` its consensus
    /// coordinates — both required for Manifest.
    pub async fn handle(
        &mut self,
        host: &Host,
        finalized: Option<FinalizedBlock>,
        coords: &BoundaryCoords,
        req: SyncRequest,
    ) -> SyncResponse {
        match self.try_handle(host, finalized, coords, req).await {
            Ok(resp) => resp,
            Err(msg) => SyncResponse::Error(msg),
        }
    }

    /// handle one still-encoded request frame; the transport loop calls this.
    pub async fn handle_frame(
        &mut self,
        host: &Host,
        finalized: Option<FinalizedBlock>,
        coords: &BoundaryCoords,
        frame: &[u8],
    ) -> Vec<u8> {
        let resp = match decode_request(frame) {
            Ok(req) => self.handle(host, finalized, coords, req).await,
            Err(e) => SyncResponse::Error(format!("bad request frame: {e}")),
        };
        encode_response(&resp)
    }

    /// the composed one-owner path: drive [`SyncServer::serve`] and make every
    /// state touch inline against `host`. callers that split ownership (the
    /// node's off-loop serve task) drive `serve()` themselves and round-trip
    /// the touches to the state owner instead.
    async fn try_handle(
        &mut self,
        host: &Host,
        finalized: Option<FinalizedBlock>,
        coords: &BoundaryCoords,
        req: SyncRequest,
    ) -> Result<SyncResponse, String> {
        match self.serve(req) {
            ServeStep::Reply(resp) => Ok(resp),
            ServeStep::NeedBoundary => {
                let finalized = finalized.ok_or("no finalized boundary to serve yet")?;
                let (id, data) = capture_boundary(host, finalized, coords).await?;
                self.install_capture(id, data);
                self.manifest_for(id)
            }
            ServeStep::NeedModuleServe { module_id, body } => host
                .serve_sync(&module_id, &body)
                .await
                .map(SyncResponse::Module)
                .map_err(|e| format!("module {module_id} serve_sync: {e}")),
            ServeStep::NeedFrames { .. } => {
                Err("frame range requests require the recovery journal".into())
            }
            ServeStep::NeedIndexOps { .. } => {
                // this owner holds no index store — an EMPTY page at floor 0,
                // not an error: a state owner without a derived tier has
                // nothing to say about it, and refusing would read as a fault.
                // `applied_height: 0` is below any boundary a joiner asks for,
                // so the fetcher refuses to lower its floor on this answer.
                Ok(SyncResponse::IndexOps {
                    rows: Vec::new(),
                    next_after: None,
                    source_floor: None,
                    applied_height: 0,
                })
            }
            ServeStep::NeedCoords => {
                let finalized = finalized.ok_or("no finalized boundary to serve yet")?;
                Ok(SyncResponse::TipCoords(TipCoords {
                    height: finalized.height,
                    root_hash: finalized.root_hash,
                    epoch: coords.epoch,
                    view_base: coords.view_base,
                    participants: coords.participants.clone(),
                    residents: coords.residents.clone(),
                    has_floor: coords.floor_cert.is_some(),
                    generation: coords.generation,
                    mesh_window: coords.mesh_window.clone(),
                    // this crate is not a node and holds no build stamp: the
                    // node's own state owner answers the detection lane (and
                    // fills the stamp) before a request reaches here.
                    build: None,
                }))
            }
        }
    }

    fn leased_capture(&mut self, boundary: BoundaryId) -> Result<&Capture, String> {
        if !self.leased.contains_key(&boundary) {
            return Err(format!(
                "boundary {} {} is not leased (refetch manifest)",
                boundary.height,
                hex_root(&boundary.root_hash)
            ));
        }
        self.touch_lease(boundary);
        self.captures.get(&boundary).ok_or_else(|| {
            format!(
                "no capture at boundary {} {} (refetch manifest)",
                boundary.height,
                hex_root(&boundary.root_hash)
            )
        })
    }

    fn touch_lease(&mut self, id: BoundaryId) {
        self.lease_clock = self.lease_clock.wrapping_add(1);
        self.leased.insert(id, self.lease_clock);
    }

    fn release_leased_overflow(&mut self) {
        while self.leased.len() > MAX_LEASED_BOUNDARIES {
            let oldest = self
                .leased
                .iter()
                .min_by_key(|(_, tick)| **tick)
                .map(|(id, _)| *id)
                .expect("leased len above cap implies at least one lease");
            self.leased.remove(&oldest);
        }
    }

    fn evict_unleased_overflow(&mut self) {
        self.evict_unleased_overflow_sparing(None);
    }

    /// evict past the cache cap, never touching a leased capture — nor
    /// `spared`, the id an in-flight install is about to manifest. sparing an
    /// unleased newborn can leave the cache one over cap for the single step
    /// until its `manifest_for` lease lands and the next eviction rebalances.
    fn evict_unleased_overflow_sparing(&mut self, spared: Option<BoundaryId>) {
        while self.captures.len() > MAX_CAPTURES {
            let Some(oldest) = self
                .captures
                .keys()
                .copied()
                .find(|id| !self.leased.contains_key(id) && Some(*id) != spared)
            else {
                break;
            };
            self.captures
                .remove(&oldest)
                .expect("removed capture existed");
        }
    }
}

// ============================================================================
// CLIENT — transport seam + fetch helpers.
// ============================================================================

/// the joiner's transport seam: move one request to the serving peer and bring
/// its response back. `Clone + Send + Sync` because the qmdb sync engine holds
/// the resolver (and therefore the client) across concurrent fetches.
pub trait SyncClient: Clone + Send + Sync + 'static {
    fn request(
        &self,
        req: SyncRequest,
    ) -> impl std::future::Future<Output = Result<SyncResponse, SyncError>> + Send;
}

/// fetch the serving peer's manifest.
pub async fn fetch_manifest<C: SyncClient>(client: &C) -> Result<Manifest, SyncError> {
    match client.request(SyncRequest::Manifest).await? {
        SyncResponse::Manifest(m) => Ok(m),
        SyncResponse::Error(e) => Err(SyncError::Server(e)),
        other => Err(SyncError::UnexpectedResponse(other.kind_name())),
    }
}

/// fetch a captured module's full snapshot payload, chunk by chunk. each
/// chunk's length is the SERVING peer's choice, so every check here runs
/// BEFORE the bytes it would refuse are appended: an oversized chunk refuses
/// before it is appended, and a peer that keeps sending past its own
/// declared `total` refuses too (it is lying, not generous).
pub async fn fetch_snapshot<C: SyncClient>(
    client: &C,
    boundary: BoundaryId,
    module_id: &str,
) -> Result<Vec<u8>, SyncError> {
    let malformed = |reason: &'static str| SyncError::Malformed {
        module: module_id.to_string(),
        reason,
    };
    let mut out: Vec<u8> = Vec::new();
    loop {
        let req = SyncRequest::Chunk {
            boundary,
            module_id: module_id.to_string(),
            offset: out.len() as u64,
        };
        match client.request(req).await? {
            SyncResponse::Chunk { total, bytes } => {
                if bytes.len() > CHUNK_LEN {
                    return Err(malformed("chunk_exceeds_chunk_len"));
                }
                if bytes.is_empty() && out.len() < total as usize {
                    return Err(SyncError::Module {
                        module: module_id.to_string(),
                        reason: "server returned an empty chunk mid-payload".into(),
                    });
                }
                let would_be = out.len() as u64 + bytes.len() as u64;
                if would_be > total {
                    // a peer that keeps sending past its own declared total
                    // is lying about the total, the stream, or both.
                    return Err(malformed("accumulated_exceeds_total"));
                }
                out.extend_from_slice(&bytes);
                if out.len() as u64 >= total {
                    out.truncate(total as usize);
                    return Ok(out);
                }
            }
            SyncResponse::Error(e) => return Err(SyncError::Server(e)),
            other => return Err(SyncError::UnexpectedResponse(other.kind_name())),
        }
    }
}

/// walk one module's index op rows at or below `boundary` in ASCENDING key
/// order, handing each page to `write` as it arrives — never accumulating,
/// because an op history is not a resident `Vec<u8>`. returns the source's own
/// backfill floor (the max seen across pages: a source may re-stamp mid-walk,
/// and the higher floor is the honest one to inherit), `None` when the source
/// claims complete coverage from genesis.
///
/// `after` RESUMES the walk: rows must ascend strictly past it, so a caller
/// that already holds a contiguous feed names its own end and pulls only the
/// delta above it. `None` walks from the source's own beginning.
///
/// # trust
///
/// these rows are NOT consensus-verified — the derived tier has no root by
/// design. accepting them is exactly the trust the joiner already extended to
/// this node when it accepted canonical state from it: your own sync source.
/// what IS enforced here, once, at the trust boundary:
///
/// * every key is BYTE-EXACTLY `op/{height:016x}/{seq:08x}` — not merely
///   parseable as one. [`index_guest::parse_op_key`] reads hex with
///   `from_str_radix`, which accepts any width and a leading `+`, so `op/2/0`
///   parses to `(2, 0)` while sorting AFTER `op/0000000000000009/00000000`. a
///   non-canonical key would pass every ascent check here and then break key
///   order in the store — which the next refold replays as history running
///   backwards. the fixed width IS the ordering, so it is checked as bytes;
/// * `(height, seq)` ascends STRICTLY across the whole walk (the caller's
///   commit order is key order — the invariant the fold depends on);
/// * every height is at or below `boundary`;
/// * every row borsh-decodes as an [`index_guest::OpRow`] whose own
///   `(height, seq)` matches its key;
/// * the source's watermark covers `boundary` — a source that folded less
///   than it is being asked for would leave a HOLE above the joiner's floor;
/// * the source's own floor stays at or below `boundary` — a source that
///   re-stamps ABOVE it mid-walk holds nothing the caller asked for, and
///   inheriting such a floor would leave the caller claiming more missing than
///   its own watermark says it has.
///
/// any violation aborts the walk with [`SyncError::Module`]; the caller keeps
/// its stamped floor, which stays honest.
pub async fn fetch_index_ops<C, W>(
    client: &C,
    module: &str,
    boundary: u64,
    after: Option<(u64, u32)>,
    mut write: W,
) -> Result<Option<u64>, SyncError>
where
    C: SyncClient,
    W: FnMut(&[(String, Vec<u8>)]) -> Result<(), String>,
{
    let refuse = |reason: String| SyncError::Module {
        module: module.to_string(),
        reason,
    };
    let mut cursor = after;
    let mut floor: Option<u64> = None;
    loop {
        let resp = client
            .request(SyncRequest::IndexOps {
                boundary,
                module: module.to_string(),
                after: cursor,
            })
            .await?;
        let SyncResponse::IndexOps {
            rows,
            next_after,
            source_floor,
            applied_height,
        } = resp
        else {
            return match resp {
                SyncResponse::Error(e) => Err(SyncError::Server(e)),
                other => Err(SyncError::UnexpectedResponse(other.kind_name())),
            };
        };
        if applied_height < boundary {
            return Err(refuse(format!(
                "source index watermark {applied_height} is below the requested \
                 boundary {boundary}; backfilling would leave a hole"
            )));
        }
        // a floor ABOVE the ceiling is not a truncation to inherit, it is a
        // source that no longer holds the range at all (it re-stamped past the
        // caller's boundary mid-walk). composing it would hand the caller a
        // floor above its own watermark — more missing than it has.
        if let Some(risen) = source_floor
            && risen > boundary
        {
            return Err(refuse(format!(
                "source index floor {risen} rose above the requested boundary \
                 {boundary}; it no longer holds the range"
            )));
        }
        floor = floor.max(source_floor);
        let mut last = cursor;
        for (key, value) in &rows {
            let pos = index_guest::parse_op_key(key.as_bytes())
                .ok_or_else(|| refuse(format!("row key {key:?} is not an op-row key")))?;
            // PARSING IS NOT ENOUGH: only the canonical fixed-width rendering
            // sorts where its position says it does, and key order is what the
            // whole lane rests on. round-tripping the parse is the check.
            if *key != index_guest::op_key(pos.0, pos.1) {
                return Err(refuse(format!(
                    "row key {key:?} is not the canonical rendering of {pos:?}"
                )));
            }
            if Some(pos) <= last {
                return Err(refuse(format!(
                    "row key {key:?} does not ascend past {last:?}"
                )));
            }
            if pos.0 > boundary {
                return Err(refuse(format!(
                    "row height {} is above the requested boundary {boundary}",
                    pos.0
                )));
            }
            let row = borsh::from_slice::<index_guest::OpRow>(value)
                .map_err(|e| refuse(format!("row {key:?} is not a borsh op envelope: {e}")))?;
            if (row.height, row.seq) != pos {
                return Err(refuse(format!(
                    "row {key:?} carries position ({}, {}), disagreeing with its key",
                    row.height, row.seq
                )));
            }
            last = Some(pos);
        }
        write(&rows).map_err(refuse)?;
        let Some(next) = next_after else {
            return Ok(floor);
        };
        // A CURSOR MUST MEAN PROGRESS. `next_after` has to be the last row
        // actually served, and an empty page carrying one would re-ask the
        // same cursor forever — the walk's only unbounded shape. Both are
        // closed here: rows ascend strictly past `cursor` above, so a
        // non-empty page's last position is strictly greater than it.
        if rows.is_empty() || last != Some(next) {
            return Err(refuse(format!(
                "page cursor {next:?} is not the last of {} rows served ({last:?})",
                rows.len()
            )));
        }
        cursor = Some(next);
    }
}

/// fetch a finite, ordered recovery-frame suffix in bounded batches. every
/// frame height is checked against the range before it is kept, so a served
/// batch cannot smuggle a frame from outside it.
pub async fn fetch_frames<C: SyncClient>(
    client: &C,
    after_height: u64,
    up_to_height: u64,
) -> Result<Vec<FinalizedFrame>, SyncError> {
    if after_height > up_to_height {
        return Err(SyncError::Server(format!(
            "invalid frame range ({after_height}, {up_to_height}]"
        )));
    }
    let mut out = Vec::new();
    let mut after = after_height;
    while after < up_to_height {
        let resp = client
            .request(SyncRequest::Frames {
                after_height: after,
                up_to_height,
            })
            .await?;
        match resp {
            SyncResponse::Frames { frames } => {
                if frames.is_empty() {
                    return Err(SyncError::Server(format!(
                        "server returned empty frame batch for non-empty range \
                         ({after}, {up_to_height}]"
                    )));
                }
                let mut last = after;
                for frame in frames {
                    if frame.height <= last || frame.height > up_to_height {
                        return Err(SyncError::Server(format!(
                            "server returned out-of-range frame height {} for \
                             ({after}, {up_to_height}]",
                            frame.height
                        )));
                    }
                    last = frame.height;
                    out.push(frame);
                }
                after = last;
            }
            SyncResponse::RangePruned {
                requested_after,
                retained_from,
            } => {
                return Err(SyncError::RangePruned {
                    requested_after,
                    retained_from,
                });
            }
            SyncResponse::Error(e) => return Err(SyncError::Server(e)),
            other => return Err(SyncError::UnexpectedResponse(other.kind_name())),
        }
    }
    Ok(out)
}

// ============================================================================
// OBJECT-RESOLVER DRIVER — the duckfs-odb ("object possession") sync path.
// ============================================================================
//
// unlike the qmdb resolver (a merkle op-range engine), an object-store module
// syncs by CONTENT-ADDRESSED FETCH: install the boundary refs image, then walk
// the reachable-but-absent object set (a BFS whose children are only revealed as
// their parents arrive), fetching each layer over the module's `serve_sync` lane
// until every reachable object is present. this crate is platform surface below
// the module crates, so it cannot name the module's types — the loop is generic
// over two seams the module (or the node glue) supplies:
//
//   * [`ModuleLane`]  — move one `serve_sync` request/response for the module.
//   * [`ObjectFetch`] — the module's install / missing / ingest / possession ops
//                       and its own request/response wire (encode/decode).
//
// the driver owns ONLY the control flow + the full-possession gate, so a joiner
// reports READY only once it holds every object — never on the refs alone.

/// move one `serve_sync` request to the serving peer for `module_id` and bring
/// its response bytes back. deliberately NOT [`SyncClient`]: this lane is driven
/// sequentially and need not be `Send` (an object-store module's `serve_sync`
/// future is `?Send`), so a test can back it with a source module in-process.
pub trait ModuleLane {
    fn fetch(
        &self,
        module_id: &str,
        body: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, SyncError>>;
}

/// a lane backed by a real [`SyncClient`] pinned at a manifest boundary: every
/// fetch is a boundary-scoped [`SyncRequest::Module`] round trip, exactly the
/// lane the server routes to `host.serve_sync`.
#[derive(Clone)]
pub struct ClientModuleLane<C> {
    client: C,
    boundary: BoundaryId,
}

impl<C> ClientModuleLane<C> {
    pub fn new(client: C, boundary: BoundaryId) -> Self {
        Self { client, boundary }
    }
}

impl<C: SyncClient> ModuleLane for ClientModuleLane<C> {
    async fn fetch(&self, module_id: &str, body: Vec<u8>) -> Result<Vec<u8>, SyncError> {
        match self
            .client
            .request(SyncRequest::Module {
                boundary: self.boundary,
                module_id: module_id.to_string(),
                body,
            })
            .await?
        {
            SyncResponse::Module(bytes) => Ok(bytes),
            SyncResponse::Error(e) => Err(qmdb::module_lane_error(module_id, e)),
            other => Err(SyncError::UnexpectedResponse(other.kind_name())),
        }
    }
}

/// the joiner-side seam an object-store ("duckfs-odb") module presents to the
/// generic possession driver: the module owns its `serve_sync` WIRE (encode the
/// request bodies, decode the replies) and its verify-then-store ops; the driver
/// owns the loop. all ops are sync — an object-store module's install / walk /
/// ingest touch only local durable state.
pub trait ObjectFetch {
    /// the `serve_sync` body requesting the boundary refs image.
    fn refs_request(&self) -> Vec<u8>;

    /// verify-then-install a served refs reply against `root`, persisting the
    /// durable envelope at the SYNC-TARGET `height` (a fresh joiner must not
    /// persist height 0 — a restart would replay from a pruned genesis).
    fn install_refs(&mut self, reply: &[u8], root: StateRoot, height: u64) -> Result<(), String>;

    /// the `serve_sync` body requesting up to `limit` reachable-but-absent
    /// objects, or `None` when possession is already complete (nothing missing).
    fn missing_request(&self, limit: usize) -> Result<Option<Vec<u8>>, String>;

    /// verify-then-store a served object reply (each id re-hashed inside);
    /// returns how many objects LANDED. a batch that lands zero while objects are
    /// still missing means the source pruned below the boundary — the driver
    /// stops rather than livelock.
    fn ingest(&mut self, reply: &[u8]) -> Result<usize, String>;

    /// whether every object the committed refs reach is now present.
    fn possession_complete(&self) -> Result<bool, String>;
}

/// drive an object-store module to FULL object possession at a manifest
/// boundary: install the boundary refs (root-verified) at `height`, then loop
/// { missing -> fetch -> ingest } until nothing is missing, and gate READY on
/// possession being genuinely complete. `batch` bounds the objects requested per
/// round (must not exceed the module's `serve_sync` id cap).
pub async fn sync_object_possession<L, M>(
    lane: &L,
    module_id: &str,
    root: StateRoot,
    height: u64,
    module: &mut M,
    batch: usize,
) -> Result<(), SyncError>
where
    L: ModuleLane,
    M: ObjectFetch,
{
    let module_err = |reason: String| SyncError::Module {
        module: module_id.to_string(),
        reason,
    };

    // 1. install the boundary refs image (root-verified) at the sync-target
    //    height. a mismatch (the source advanced past the captured boundary)
    //    fails here — the caller refetches the manifest and retries.
    let refs_reply = lane.fetch(module_id, module.refs_request()).await?;
    module
        .install_refs(&refs_reply, root, height)
        .map_err(module_err)?;

    // 2. the possession walk. each round reveals at least the newly-arrived
    //    layer's children, so the store strictly grows; the loop terminates when
    //    the reachable set is fully present (`missing_request` -> None).
    while let Some(body) = module.missing_request(batch).map_err(module_err)? {
        let reply = lane.fetch(module_id, body).await?;
        let landed = module.ingest(&reply).map_err(module_err)?;
        if landed == 0 {
            return Err(module_err(
                "source served no requested object (pruned below the boundary); \
                 refetch the manifest"
                    .into(),
            ));
        }
    }

    // 3. the full-possession gate — READY only when every reachable object is
    //    held, never on the refs image alone.
    if !module.possession_complete().map_err(module_err)? {
        return Err(module_err(
            "object fetch loop terminated before full possession".into(),
        ));
    }
    Ok(())
}

fn hex_root(root: &StateRoot) -> String {
    sdk::hash::hex_lower(root.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_frames_round_trip() {
        for req in [
            SyncRequest::Manifest,
            SyncRequest::Chunk {
                boundary: BoundaryId {
                    height: 42,
                    root_hash: StateRoot([4u8; ROOT_LEN]),
                },
                module_id: "forge".into(),
                offset: 1 << 20,
            },
            SyncRequest::Module {
                boundary: BoundaryId {
                    height: 42,
                    root_hash: StateRoot([4u8; ROOT_LEN]),
                },
                module_id: "kv".into(),
                body: vec![1, 2, 3],
            },
            SyncRequest::Frames {
                after_height: 42,
                up_to_height: 48,
            },
            SyncRequest::IndexOps {
                boundary: 42,
                module: "chat".into(),
                after: Some((7, 3)),
            },
            SyncRequest::IndexOps {
                boundary: 42,
                module: "chat".into(),
                after: None,
            },
            SyncRequest::Blob { digest: [7u8; 32] },
            SyncRequest::ForgeObjects {
                repo: "ducktape".into(),
                head: [9u8; FORGE_OID_LEN],
                bases: vec![[1u8; FORGE_OID_LEN], [2u8; FORGE_OID_LEN]],
            },
            SyncRequest::ForgeObjects {
                repo: String::new(),
                head: [9u8; FORGE_OID_LEN],
                bases: Vec::new(),
            },
        ] {
            let bytes = encode_request(&req);
            assert_eq!(decode_request(&bytes).unwrap(), req);
        }
    }

    /// the base list is attacker-chosen, so its count must be capped BEFORE
    /// it sizes an allocation — and a truncated tail must reject rather than
    /// decode a short list.
    #[test]
    fn forge_object_request_rejects_an_oversized_base_count() {
        let mut framed = encode_request(&SyncRequest::ForgeObjects {
            repo: "demo".into(),
            head: [9u8; FORGE_OID_LEN],
            bases: Vec::new(),
        });
        let count_at = framed.len() - 4;
        framed[count_at..].copy_from_slice(&(MAX_FORGE_BASES as u32 + 1).to_le_bytes());
        assert!(decode_request(&framed).is_err(), "count past the cap");

        let mut truncated = encode_request(&SyncRequest::ForgeObjects {
            repo: "demo".into(),
            head: [9u8; FORGE_OID_LEN],
            bases: vec![[1u8; FORGE_OID_LEN]],
        });
        truncated.truncate(truncated.len() - 1);
        assert!(decode_request(&truncated).is_err(), "short base tail");
    }

    #[test]
    fn an_unsupported_entry_names_the_undeliverable_module() {
        let entry = |id: &str, kind| ManifestEntry {
            module_id: id.into(),
            root: StateRoot([1u8; ROOT_LEN]),
            code_hash: None,
            kind,
            resolver_target: None,
        };
        let manifest = |entries| Manifest {
            height: 4,
            root_hash: StateRoot([2u8; ROOT_LEN]),
            epoch: 1,
            view_base: 0,
            participants: vec![],
            residents: vec![],
            floor_cert: None,
            entries,
            applied_frames: vec![],
            pending_cutover_view: None,
        };
        // a source whose `kv` could not prepare a handle serves it as
        // Unsupported: the joiner reads that off the manifest instead of
        // discovering it as a missing pinned target mid-compose.
        assert_eq!(
            manifest(vec![
                entry("valset", PayloadKind::Snapshot),
                entry("kv", PayloadKind::Unsupported),
            ])
            .undeliverable_module()
            .map(|m| m.as_str()),
            Some("kv")
        );
        // every other kind is transferable — including Stateless, which
        // carries no payload on purpose.
        assert_eq!(
            manifest(vec![
                entry("valset", PayloadKind::Snapshot),
                entry("kv", PayloadKind::Resolver),
                entry("modules", PayloadKind::Stateless),
            ])
            .undeliverable_module(),
            None
        );
    }

    #[test]
    fn forge_object_response_round_trips_hit_and_miss() {
        for resp in [
            SyncResponse::ForgeObjects {
                digest: Some([3u8; 32]),
            },
            SyncResponse::ForgeObjects { digest: None },
        ] {
            let bytes = encode_response(&resp);
            assert_eq!(decode_response(&bytes).unwrap(), resp);
        }
    }

    #[test]
    fn blob_response_round_trips_hit_and_miss() {
        for resp in [
            SyncResponse::Blob {
                bytes: Some(b"You are quack.".to_vec()),
            },
            SyncResponse::Blob { bytes: None },
        ] {
            let bytes = encode_response(&resp);
            assert_eq!(decode_response(&bytes).unwrap(), resp);
        }
        // a truncated presence flag rejects instead of decoding garbage.
        let mut framed = encode_response(&SyncResponse::Blob { bytes: None });
        framed.truncate(1);
        assert!(decode_response(&framed).is_err());
    }

    #[test]
    fn response_frames_round_trip() {
        for resp in [
            SyncResponse::Manifest(Manifest {
                height: 7,
                root_hash: StateRoot([9u8; ROOT_LEN]),
                epoch: 2,
                view_base: 5,
                participants: vec![vec![3u8; 32], vec![4u8; 32]],
                // non-empty: exercises the additive resident wire tail.
                residents: vec![vec![5u8; 32]],
                floor_cert: Some(vec![0xCC; 96]),
                entries: vec![
                    ManifestEntry {
                        code_hash: None,
                        module_id: "kv".into(),
                        root: StateRoot([1u8; ROOT_LEN]),
                        kind: PayloadKind::Resolver,
                        resolver_target: Some(ResolverTarget {
                            root: commonware_cryptography::sha256::Digest([1u8; ROOT_LEN]),
                            start: 1,
                            op_count: 2,
                        }),
                    },
                    ManifestEntry {
                        code_hash: None,
                        module_id: "valset".into(),
                        root: StateRoot([2u8; ROOT_LEN]),
                        kind: PayloadKind::Snapshot,
                        resolver_target: None,
                    },
                ],
                // non-empty: exercises the replay-window wire tail.
                applied_frames: vec![(6, [0xE1; 32]), (7, [0xE2; 32])],
                // exercises the Some(view) pending-cutover wire tail.
                pending_cutover_view: Some(9),
            }),
            // a fresh-epoch boundary: no finalization past the base yet, so
            // no floor certificate — the joiner spawns on the genesis floor,
            // and no valset change is armed.
            SyncResponse::Manifest(Manifest {
                height: 12,
                root_hash: StateRoot([8u8; ROOT_LEN]),
                epoch: 1,
                view_base: 12,
                participants: vec![vec![3u8; 32]],
                residents: vec![],
                floor_cert: None,
                entries: vec![],
                applied_frames: vec![],
                pending_cutover_view: None,
            }),
            SyncResponse::Chunk {
                total: 10,
                bytes: vec![0xAB; 10],
            },
            SyncResponse::Module(vec![4, 5]),
            SyncResponse::Frames {
                frames: vec![FinalizedFrame {
                    height: 8,
                    frame: vec![0xAB, 0xCD],
                    disposition: FrameDisposition::Applied,
                    roots: vec![("kv".into(), StateRoot([3u8; ROOT_LEN]))],
                    root_hash: StateRoot([4u8; ROOT_LEN]),
                }],
            },
            SyncResponse::RangePruned {
                requested_after: 10,
                retained_from: 12,
            },
            SyncResponse::Error("nope".into()),
            SyncResponse::IndexOps {
                rows: vec![
                    ("op/0000000000000001/00000000".into(), vec![1, 2, 3]),
                    ("op/0000000000000002/0000000a".into(), Vec::new()),
                ],
                next_after: Some((2, 10)),
                source_floor: Some(1),
                applied_height: 9,
            },
            SyncResponse::IndexOps {
                rows: vec![],
                next_after: None,
                source_floor: None,
                applied_height: 0,
            },
        ] {
            let bytes = encode_response(&resp);
            assert_eq!(decode_response(&bytes).unwrap(), resp);
        }
    }

    #[test]
    fn frames_response_length_matches_codec() {
        let frames = vec![
            FinalizedFrame {
                height: 8,
                frame: vec![0xAB; 31],
                disposition: FrameDisposition::Applied,
                roots: vec![
                    ("kv".into(), StateRoot([3u8; ROOT_LEN])),
                    ("a-longer-module-id".into(), StateRoot([4u8; ROOT_LEN])),
                ],
                root_hash: StateRoot([5u8; ROOT_LEN]),
            },
            FinalizedFrame {
                height: 9,
                frame: Vec::new(),
                disposition: FrameDisposition::Rejected,
                roots: Vec::new(),
                root_hash: StateRoot([6u8; ROOT_LEN]),
            },
        ];
        let encoded = encode_response(&SyncResponse::Frames {
            frames: frames.clone(),
        });
        assert_eq!(encoded_frames_response_len(&frames), encoded.len());
    }

    #[test]
    fn index_ops_response_length_bounds_the_codec() {
        // the length helper assumes both optional tails present, so it is an
        // upper bound for every shape — which is what a serve-side binary
        // search against a transport cap needs to stay sound.
        let rows = vec![
            ("op/0000000000000001/00000000".to_string(), vec![0xAB; 31]),
            ("op/0000000000000009/00000007".to_string(), Vec::new()),
        ];
        let widest = encode_response(&SyncResponse::IndexOps {
            rows: rows.clone(),
            next_after: Some((9, 7)),
            source_floor: Some(1),
            applied_height: 12,
        });
        assert_eq!(encoded_index_ops_response_len(&rows), widest.len());
        let narrowest = encode_response(&SyncResponse::IndexOps {
            rows: rows.clone(),
            next_after: None,
            source_floor: None,
            applied_height: 12,
        });
        assert!(encoded_index_ops_response_len(&rows) >= narrowest.len());
    }

    #[test]
    fn forged_index_op_page_counts_reject_before_allocation() {
        // tag 6 then a row count far past the buffer: refused, never sized.
        let mut bytes = vec![6u8];
        bytes.extend_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode_response(&bytes).is_err());
        // and a count inside the cap but past the remaining bytes.
        let mut bytes = vec![6u8];
        bytes.extend_from_slice(&(INDEX_OPS_BATCH_LEN as u64).to_le_bytes());
        assert!(decode_response(&bytes).is_err());
    }

    #[test]
    fn truncated_and_trailing_frames_reject() {
        let bytes = encode_request(&SyncRequest::Chunk {
            boundary: BoundaryId {
                height: 1,
                root_hash: StateRoot([1u8; ROOT_LEN]),
            },
            module_id: "m".into(),
            offset: 0,
        });
        assert!(
            decode_request(&bytes[..bytes.len() - 1]).is_err(),
            "truncation rejects"
        );
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(decode_request(&trailing).is_err(), "trailing bytes reject");
    }

    #[test]
    fn rpc_envelope_round_trips() {
        let requester = [7u8; 32];
        let proof = [9u8; 64];
        let framed = encode_rpc(&requester, &proof, 99, b"body");
        let (r, p, id, body) = decode_rpc(&framed).unwrap();
        assert_eq!(r, &requester);
        assert_eq!(p, &proof);
        assert_eq!(id, 99);
        assert_eq!(body, b"body");
        assert_eq!(framed.len(), RPC_HEADER_LEN + body.len());
        // anything shorter than the 32+64+8 fixed header is Truncated.
        assert!(
            decode_rpc(&framed[..32 + 64 + 7]).is_err(),
            "short envelope rejects"
        );
        assert!(decode_rpc(&[]).is_err(), "empty rejects");
    }

    #[test]
    fn sync_proof_signs_and_verifies_only_for_the_signing_key() {
        use commonware_cryptography::{Signer as _, ed25519};
        let signer = ed25519::PrivateKey::from_seed(42);
        let namespace = b"net#deadbeef@feedface";
        let (requester, proof) = sign_sync_proof(&signer, namespace);
        assert_eq!(requester.as_slice(), signer.public_key().as_ref());
        assert!(
            verify_sync_proof(&requester, &proof, namespace),
            "the real key's proof verifies"
        );
        // a different namespace (wrong network) fails.
        assert!(
            !verify_sync_proof(&requester, &proof, b"other-net"),
            "a proof for another network is refused"
        );
        // a substituted requester key fails (the proof is bound to the signer).
        let thief: [u8; 32] = ed25519::PrivateKey::from_seed(43)
            .public_key()
            .as_ref()
            .try_into()
            .unwrap();
        assert!(
            !verify_sync_proof(&thief, &proof, namespace),
            "a substituted key fails the proof"
        );
        // a real standing key with a forged/empty signature is refused (you
        // cannot mint a proof for a key without its private half). (an all-zero
        // key is a valid small-order point whose zero signature verifies — a
        // crypto edge that is harmless here: no node holds the zero key, so the
        // standing gate refuses it regardless.)
        assert!(
            !verify_sync_proof(&requester, &[0u8; 64], namespace),
            "a forged signature for a real key is refused"
        );
    }

    #[test]
    fn forged_manifest_counts_reject_before_allocation() {
        // header: tag 0, height, root_hash, epoch, view_base, then a forged
        // PARTICIPANT count far past the buffer.
        let mut bytes = vec![0u8];
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&[0u8; ROOT_LEN]);
        bytes.extend_from_slice(&2u64.to_le_bytes());
        bytes.extend_from_slice(&3u64.to_le_bytes());
        bytes.extend_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode_response(&bytes).is_err());

        // same header, zero participants + no floor cert, then a forged
        // ENTRY count.
        let mut bytes = vec![0u8];
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&[0u8; ROOT_LEN]);
        bytes.extend_from_slice(&2u64.to_le_bytes());
        bytes.extend_from_slice(&3u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.push(0); // floor_cert: None
        bytes.extend_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode_response(&bytes).is_err());

        // and the REPLAY WINDOW count: capped at the protocol depth, so a
        // forged one is refused before it reserves 40 bytes an entry.
        let mut bytes = vec![0u8];
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&[0u8; ROOT_LEN]);
        bytes.extend_from_slice(&2u64.to_le_bytes());
        bytes.extend_from_slice(&3u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes()); // participants
        bytes.push(0); // floor_cert: None
        bytes.extend_from_slice(&0u64.to_le_bytes()); // entries
        bytes.extend_from_slice(&0u64.to_le_bytes()); // residents
        bytes.extend_from_slice(&(MAX_APPLIED_FRAMES as u64 + 1).to_le_bytes());
        assert!(decode_response(&bytes).is_err());
    }

    #[test]
    fn decode_response_rejects_truncated_manifest_tail() {
        // a manifest frame cut mid-field must fail cleanly (no panic), not
        // silently default.
        let resp = SyncResponse::Manifest(Manifest {
            height: 7,
            root_hash: StateRoot([9u8; ROOT_LEN]),
            epoch: 2,
            view_base: 5,
            participants: vec![],
            residents: vec![],
            floor_cert: None,
            entries: vec![],
            applied_frames: vec![(6, [0xE1; 32])],
            pending_cutover_view: Some(11),
        });
        let bytes = encode_response(&resp);
        // Drop bytes from the trailing entry/resident/replay-window counts.
        for cut in 1..=21 {
            let torn = &bytes[..bytes.len() - cut];
            assert!(
                decode_response(torn).is_err(),
                "truncation at -{cut} must reject"
            );
        }
    }

    // ------------------------------------------------------------------
    // fetch_snapshot: the served `total` and every chunk length are the
    // PEER's choice, so the cap has to hold against a peer that lies in
    // either. a scripted client stands in for the wire: it hands back a
    // canned queue of responses with no server, no transport.
    // ------------------------------------------------------------------

    /// a `SyncClient` that plays back a fixed script of responses, one per
    /// request, ignoring the request itself — enough to drive `fetch_snapshot`
    /// without a real server.
    #[derive(Clone)]
    struct ScriptedClient {
        responses: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<SyncResponse>>>,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ScriptedClient {
        fn new(responses: Vec<SyncResponse>) -> Self {
            Self {
                responses: std::sync::Arc::new(std::sync::Mutex::new(responses.into())),
                calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl SyncClient for ScriptedClient {
        fn request(
            &self,
            _req: SyncRequest,
        ) -> impl std::future::Future<Output = Result<SyncResponse, SyncError>> + Send {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let next = self.responses.lock().expect("script poisoned").pop_front();
            async move { next.ok_or_else(|| SyncError::Transport("script exhausted".into())) }
        }
    }

    fn test_boundary() -> BoundaryId {
        BoundaryId {
            height: 1,
            root_hash: StateRoot([0u8; ROOT_LEN]),
        }
    }

    #[tokio::test]
    async fn fetch_snapshot_refuses_an_oversized_chunk() {
        let client = ScriptedClient::new(vec![SyncResponse::Chunk {
            total: 1024,
            bytes: vec![0u8; CHUNK_LEN + 1],
        }]);
        let err = fetch_snapshot(&client, test_boundary(), "mod")
            .await
            .expect_err("a chunk over CHUNK_LEN must refuse");
        assert!(
            matches!(
                err,
                SyncError::Malformed {
                    reason: "chunk_exceeds_chunk_len",
                    ..
                }
            ),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn fetch_snapshot_refuses_a_stream_that_overruns_its_own_total() {
        // total says 10 bytes; the first chunk honestly delivers part of
        // that, but the second keeps sending well past the declared total.
        let client = ScriptedClient::new(vec![
            SyncResponse::Chunk {
                total: 10,
                bytes: vec![1u8; 5],
            },
            SyncResponse::Chunk {
                total: 10,
                bytes: vec![2u8; 10],
            },
        ]);
        let err = fetch_snapshot(&client, test_boundary(), "mod")
            .await
            .expect_err("a peer that sends past its own total is lying");
        assert!(
            matches!(
                err,
                SyncError::Malformed {
                    reason: "accumulated_exceeds_total",
                    ..
                }
            ),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn fetch_snapshot_assembles_an_honest_stream() {
        let total = 10u64;
        let client = ScriptedClient::new(vec![
            SyncResponse::Chunk {
                total,
                bytes: vec![1u8, 2, 3],
            },
            SyncResponse::Chunk {
                total,
                bytes: vec![4u8, 5, 6, 7],
            },
            SyncResponse::Chunk {
                total,
                bytes: vec![8u8, 9, 10],
            },
        ]);
        let bytes = fetch_snapshot(&client, test_boundary(), "mod")
            .await
            .expect("an honest stream assembles");
        assert_eq!(bytes, vec![1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        assert_eq!(client.call_count(), 3);
    }
}
