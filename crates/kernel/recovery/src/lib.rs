//! restart recovery — the durable half of the ordered lane.
//!
//! a validator's app state is split across substrates with very different
//! persistence: qmdb modules and forge commit to disk per block, the rest are
//! in-memory canonical-bytes, and the consensus engine journals votes but not
//! payloads. this crate makes the COMPOSED state recoverable: a write-ahead
//! op journal (every finalized frame's bytes land on disk before they mutate
//! state), a periodic checkpoint of the in-memory modules, and a boot-time
//! replay that rolls every module forward from its own position to the
//! journal tip.
//!
//! ## the position model — root equality, not op counters
//!
//! every sealed block records the FULL module-root vector after it settled
//! ([`node::BlockSeal`]). on boot each module's live root is compared against
//! those seals: a module whose root equals a seal's entry has that block (and
//! everything before it) applied. this works because the disk substrates'
//! roots never repeat — a qmdb root is an op-log commitment (every commit
//! moves it) and a git head oid commits to its parent chain — while the
//! in-memory modules' roots are pure state commitments restored exactly at
//! the checkpoint. so per-block skip/apply decisions reduce to comparing the
//! CHANGED modules (seal roots vs the previous seal's) against the live host.
//!
//! ## crash windows
//!
//! - crash between a frame's WAL record and its seal: at most one unsealed
//!   [`Record::Block`] can exist at the journal tip (each pre-apply record is
//!   synced, which also makes every earlier append durable). boot ROLLS it
//!   FORWARD: if the live roots still equal the pre-block vector the frame
//!   re-applies; if they moved, the apply completed before the crash and the
//!   block is sealed from the observed roots. a narrower flavor of this
//!   window survives the seal's own fsync — a crash in the TAIL OF APPLY,
//!   after a disk substrate committed the block but before the seal syncs,
//!   and a SIGKILL reaches it (the journal buffers in userspace, so an
//!   un-fsync'd append dies with the process). it is bound-and-verified
//!   through the substrate's per-commit
//!   height cursor (see `trailing.rs`): the cursor must claim exactly the
//!   trailing WAL height, or the state stays fail-closed as [`Error::Torn`].
//! - a TORN SEALED block: a block whose commit spans substrates with different
//!   durability — a qmdb store commits to disk PER BLOCK, while the in-memory
//!   cohort only persists at the periodic checkpoint. a crash (or a hard kill)
//!   after the disk commit but before the next checkpoint leaves the disk
//!   substrate at the block's POST root and the in-memory cohort restored to
//!   its PRE root from the checkpoint. this is no longer "all-or-nothing":
//!   recovery replays such a block by re-running the sealed frame and
//!   committing ONLY the still-at-pre modules (a pure state re-commitment,
//!   deterministic and idempotent) while ABORTING the already-durable ones —
//!   re-committing a qmdb store would MOVE its op-log root and fork us. the
//!   per-module root compare (live vs sealed pre/post) decides the partition;
//!   a changed module at NEITHER pre nor post is genuine damage and still
//!   fail-stops as [`Error::Torn`], and the recomposed-vs-sealed root-hash
//!   check is the final backstop.
//! - a block that touches several DISK substrates crashing BETWEEN their
//!   commits — the classic multi-store atomicity limit. an ORDINARY block
//!   touches several: every store-backed module keeps its own qmdb instance
//!   and the odb modules their own duckfs/forge disks, so one agent-run settle
//!   commits the in-memory `runs` plus chat, tasks and dispatch. what bounds
//!   the window is the SEAL, not a count of substrates: [`Record::Seal`] is
//!   fsync'd only after the block's apply RETURNED, and a per-block-durable
//!   substrate's commit is durable when it returns inside that apply — so for
//!   any SEALED block every disk substrate it changed is durable at (or
//!   beyond) its post-root. the partial-commit window lies strictly ABOVE the
//!   last seal, in the single unsealed WAL block, which `trailing.rs` bounds
//!   and verifies on its own terms — fail-CLOSED terms: it admits only a
//!   claimant carrying a per-commit height cursor, so a trailing qmdb store
//!   commit (no cursor) and more than one claimant BOTH still refuse. inside
//!   the sealed window the NUMBER of durable substrates is evidence of nothing
//!   but how many disk modules the block touched, and the at-pre set is
//!   exactly the cohort the checkpoint rolled back — so selective replay
//!   handles one and many identically. the fail-stops that DO carry evidence
//!   stay: a changed module at neither its pre- nor its post-root (nor ahead)
//!   is damage ([`Error::Torn`]), a torn block with nothing left to re-commit
//!   is damage, and the per-module post-root verify plus the tip root-hash
//!   recompose fail-stop rather than fork if the re-execution diverges. the
//!   residual — the re-execution can read a durable sibling's POST state
//!   where the original read its pre state — is the one the single-substrate
//!   heal always had, with the same backstop.
//! - crash between the engine journaling a finalization and the drain: the
//!   frame's bytes are already durable here — locally-submitted frames are
//!   pinned at submit time ([`Record::Pinned`]), before the engine can ever
//!   propose their digest. boot seeds the consensus content store from these
//!   records so the re-reported finalization resolves and applies.

mod journal;
mod trailing;

use std::collections::{BTreeMap, BTreeSet};
use std::num::{NonZeroU16, NonZeroU64, NonZeroUsize};

use commonware_codec::RangeCfg;
use commonware_runtime::BufferPooler;
use commonware_runtime::ReadOptions;
use commonware_runtime::buffer::paged::CacheRef;
use commonware_storage::journal::contiguous::{Contiguous, variable};
use commonware_storage::metadata;
use commonware_utils::sequence::U64;
use futures::{StreamExt as _, pin_mut};

use host::{
    BlockContext, DispatchRecord, Host, Observed, PreparedCall, PreparedDelivery, PreparedWork,
    SubmitError, Trace, Witness,
};
use node::{BlockSeal, BlockSink, Disposition, decode_batch};
use sdk::{ModuleId, StateRoot};

/// runtime bounds every store here needs (same alias the storage crate uses:
/// `Storage + Clock + Metrics`).
pub use commonware_storage::Context;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// a storage-layer fault (journal/metadata io). fail-stop territory.
    #[error("recovery storage: {0}")]
    Storage(String),
    /// a record or manifest that does not decode — the journal is damaged.
    #[error("recovery journal corrupt: {0}")]
    Corrupt(String),
    /// the on-disk substrates disagree with the journal in a way replay cannot
    /// reconcile (a module neither at a block's pre- nor post-roots).
    #[error("recovery state torn: {0}")]
    Torn(String),
    /// replay reached the tip but the recomposed state does not match what was
    /// sealed — the recovered node would fork, so it must not start.
    #[error("recovery verification failed: {0}")]
    Verify(String),
    /// a checkpoint manifest carries a field longer than this crate's own
    /// reader accepts — writing it would replace a restorable checkpoint with
    /// one that can never be read back.
    #[error("recovery checkpoint field over cap: {0}")]
    FieldOverCap(String),
    /// the caller asked for records below the retained checkpoint/journal
    /// suffix boundary. a statesync joiner must refetch a fresher manifest.
    #[error("recovery journal range pruned after {after_height}; retained from {retained_start}")]
    RangePruned {
        after_height: u64,
        retained_start: u64,
    },
}

impl From<Error> for node::Error {
    fn from(e: Error) -> Self {
        node::Error::Journal(e.to_string())
    }
}

/// a shared-cursor decode failure (truncation, a forged length over the field
/// cap, trailing bytes) is a damaged/forged journal record — the same class
/// this crate's own `Cursor` reported as [`Error::Corrupt`]. mapping it here
/// lets the record/manifest decoders below use `sdk::codec::Cursor` with `?`.
impl From<sdk::Error> for Error {
    fn from(e: sdk::Error) -> Self {
        Error::Corrupt(e.to_string())
    }
}

// ============================================================================
// wire — hand-rolled little-endian records (statesync-wire discipline:
// length-prefixed, bounds-checked, no partial reads).
// ============================================================================

/// Journal records never legitimately exceed one framed operation plus its
/// replay metadata.
const MAX_RECORD_FIELD_LEN: usize = 1 << 21; // 2 MiB: > the p2p frame cap + framing.

/// A checkpoint embeds the IN-MEMORY cohort's self-contained snapshots — a
/// module's whole state as one field, which is legitimately far larger than an
/// operation/frame. The disk cohort (forge's git pack closure, the qmdb
/// stores) is NOT here: it reopens its own substrate, so nothing in a manifest
/// grows with a repository any more (#1308).
const MAX_CHECKPOINT_FIELD_LEN: usize = 512 * 1024 * 1024;

/// the sanity cap every count-prefixed list in this codec is read under: a
/// corrupt count must not make the reader pre-allocate the world. One number,
/// so the write-side refusal ([`Manifest::check_field_caps`]) and the three
/// readers below cannot drift apart.
const MAX_LIST_LEN: usize = 4096;

// WRITE side: raw fixed-width ints stay inline one-liners; the length-prefixed
// byte writer IS `sdk::codec::push_bytes` (verbatim `u64`-LE length + bytes), so
// it delegates rather than keep a byte-for-byte duplicate.
fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    sdk::codec::push_bytes(out, b);
}

fn put_root(out: &mut Vec<u8>, r: &StateRoot) {
    out.extend_from_slice(&r.0);
}

// READ side: the shared `sdk::codec::Cursor`, opened `with_cap` so recovery's
// per-field length caps survive (`MAX_RECORD_FIELD_LEN` for op-journal records,
// `MAX_CHECKPOINT_FIELD_LEN` for checkpoint manifests carrying forge snapshots).
// a `StateRoot` is a fixed 32-byte field (no length prefix), read via `array`.
fn read_root(c: &mut sdk::codec::Cursor, what: &str) -> Result<StateRoot, Error> {
    Ok(StateRoot(c.array::<32>(what)?))
}

fn put_roots(out: &mut Vec<u8>, roots: &[(ModuleId, StateRoot)]) {
    put_u64(out, roots.len() as u64);
    for (id, root) in roots {
        put_bytes(out, id.as_bytes());
        put_root(out, root);
    }
}

fn get_roots(c: &mut sdk::codec::Cursor) -> Result<Vec<(ModuleId, StateRoot)>, Error> {
    let n = c.u64("module roots count")? as usize;
    if n > MAX_LIST_LEN {
        return Err(Error::Corrupt(format!(
            "{n} module roots exceeds sanity cap"
        )));
    }
    let mut roots = Vec::with_capacity(n);
    for _ in 0..n {
        let id = c.string("module id")?;
        roots.push((id, read_root(c, "module root")?));
    }
    Ok(roots)
}

fn put_keys(out: &mut Vec<u8>, keys: &[Vec<u8>]) {
    put_u64(out, keys.len() as u64);
    for k in keys {
        put_bytes(out, k);
    }
}

fn get_keys(c: &mut sdk::codec::Cursor) -> Result<Vec<Vec<u8>>, Error> {
    let n = c.u64("participant keys count")? as usize;
    if n > MAX_LIST_LEN {
        return Err(Error::Corrupt(format!(
            "{n} participant keys exceeds sanity cap"
        )));
    }
    let mut keys = Vec::with_capacity(n);
    for _ in 0..n {
        keys.push(c.bytes("participant key")?.to_vec());
    }
    Ok(keys)
}

// ============================================================================
// records — the op journal's entries.
// ============================================================================

const TAG_PINNED: u8 = 1;
const TAG_BLOCK: u8 = 2;
const TAG_SEAL: u8 = 3;
const TAG_CUTOVER: u8 = 4;
const TAG_PREPARED_CALL: u8 = 5;
const TAG_PREPARED_DELIVERY: u8 = 6;
const TAG_SCHEDULE: u8 = 7;
const TAG_TRACE: u8 = 8;
const TAG_WITNESSED: u8 = 9;

const DISP_APPLIED: u8 = 0;
const DISP_REJECTED: u8 = 1;

/// Local consensus establishes a frame's order; catch-up must additionally
/// reproduce the peer's claimed seal. That expectation belongs to the first
/// WAL record so no interrupted schedule or restart can discard it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockSource {
    Consensus,
    Catchup {
        disposition: Disposition,
        roots: Vec<(ModuleId, StateRoot)>,
        root_hash: StateRoot,
    },
}

impl BlockSource {
    fn verify(&self, seal: &BlockSeal) -> Result<(), Error> {
        let Self::Catchup {
            disposition,
            roots,
            root_hash,
        } = self
        else {
            return Ok(());
        };
        let matches = seal.disposition == *disposition
            && seal.roots == *roots
            && seal.root_hash == *root_hash;
        if !matches {
            return Err(Error::Verify(format!(
                "served seal mismatch at height {}: replay cannot settle this catch-up block",
                seal.height
            )));
        }
        Ok(())
    }
}

/// one op-journal entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    /// a locally-submitted frame's bytes, durable BEFORE the consensus engine
    /// may propose their digest (closes the finalized-before-drained window).
    Pinned { frame: Vec<u8> },
    /// WAL: a finalized frame about to be applied at `height`. followed by
    /// the block's prepared internal work — one [`Record::PreparedCall`] /
    /// [`Record::PreparedDelivery`] per unit — and the [`Record::Schedule`]
    /// that closes the list; physical chunks obey the journal codec cap. The
    /// frame is not applied before all of them are durable.
    Block {
        height: u64,
        frame: Vec<u8>,
        source: BlockSource,
    },
    /// one queued call the block at `height` runs (read from committed state
    /// before the block; see `host::PreparedWork`).
    PreparedCall { height: u64, call: PreparedCall },
    /// one queued item the block at `height` delivers.
    PreparedDelivery {
        height: u64,
        delivery: PreparedDelivery,
    },
    /// the close of a block's prepared list: how many units precede it. a
    /// block whose schedule never closed never began applying.
    Schedule {
        height: u64,
        calls: u64,
        deliveries: u64,
        advance: Option<sdk::Msg>,
    },
    /// One read or dispatch of unit `unit`, in attempt order. Its original
    /// answer or effects survive recovery even when a sibling or source
    /// already committed. Durable before the block's first module commits;
    /// large entries are split into bounded physical journal chunks.
    Trace {
        height: u64,
        unit: u64,
        trace: Trace,
    },
    /// the close of a block's witness: how many units it holds. a witness
    /// that never closed never preceded a commit.
    Witnessed { height: u64, units: u64 },
    /// a settled block: how it landed plus the post-block replay positions.
    Seal {
        height: u64,
        disposition: Disposition,
        roots: Vec<(ModuleId, StateRoot)>,
        root_hash: StateRoot,
    },
    /// an epoch cutover: the new epoch, its app-height base, and the engine
    /// participant set it was spawned over. the set rides the record so a
    /// restart in the window between the cutover and the next checkpoint
    /// respawns the engine with the EPOCH'S set, never the instantaneous
    /// valset projection (which may already stage the next change).
    Cutover {
        epoch: u64,
        view_base: u64,
        participants: Vec<Vec<u8>>,
        /// the epoch's RESIDENT set (transport standing, no quorum seat).
        residents: Vec<Vec<u8>>,
    },
}

impl Record {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Record::Pinned { frame } => {
                out.push(TAG_PINNED);
                put_bytes(&mut out, frame);
            }
            Record::Block {
                height,
                frame,
                source,
            } => {
                out.push(TAG_BLOCK);
                put_u64(&mut out, *height);
                put_bytes(&mut out, frame);
                match source {
                    BlockSource::Consensus => out.push(0),
                    BlockSource::Catchup {
                        disposition,
                        roots,
                        root_hash,
                    } => {
                        out.push(1);
                        out.push(match disposition {
                            Disposition::Applied => DISP_APPLIED,
                            Disposition::Rejected => DISP_REJECTED,
                            Disposition::Discarded => {
                                unreachable!("discarded frames are not journaled")
                            }
                        });
                        put_roots(&mut out, roots);
                        put_root(&mut out, root_hash);
                    }
                }
            }
            Record::PreparedCall { height, call } => {
                out.push(TAG_PREPARED_CALL);
                put_u64(&mut out, *height);
                put_bytes(&mut out, &host::encode_prepared_call(call));
            }
            Record::PreparedDelivery { height, delivery } => {
                out.push(TAG_PREPARED_DELIVERY);
                put_u64(&mut out, *height);
                put_bytes(&mut out, &host::encode_prepared_delivery(delivery));
            }
            Record::Schedule {
                height,
                calls,
                deliveries,
                advance,
            } => {
                out.push(TAG_SCHEDULE);
                put_u64(&mut out, *height);
                put_u64(&mut out, *calls);
                put_u64(&mut out, *deliveries);
                put_bytes(&mut out, &host::encode_prepared_advance(advance));
            }
            Record::Trace {
                height,
                unit,
                trace,
            } => {
                out.push(TAG_TRACE);
                put_u64(&mut out, *height);
                put_u64(&mut out, *unit);
                put_bytes(&mut out, &host::encode_trace(trace));
            }
            Record::Witnessed { height, units } => {
                out.push(TAG_WITNESSED);
                put_u64(&mut out, *height);
                put_u64(&mut out, *units);
            }
            Record::Seal {
                height,
                disposition,
                roots,
                root_hash,
            } => {
                out.push(TAG_SEAL);
                put_u64(&mut out, *height);
                // discarded frames are never journaled — the tag is two-valued.
                out.push(match disposition {
                    Disposition::Applied => DISP_APPLIED,
                    Disposition::Rejected => DISP_REJECTED,
                    Disposition::Discarded => unreachable!("discarded frames are not sealed"),
                });
                put_roots(&mut out, roots);
                put_root(&mut out, root_hash);
            }
            Record::Cutover {
                epoch,
                view_base,
                participants,
                residents,
            } => {
                out.push(TAG_CUTOVER);
                put_u64(&mut out, *epoch);
                put_u64(&mut out, *view_base);
                put_keys(&mut out, participants);
                put_keys(&mut out, residents);
            }
        }
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let large_record = matches!(
            bytes.first(),
            Some(&TAG_PREPARED_CALL) | Some(&TAG_PREPARED_DELIVERY) | Some(&TAG_TRACE)
        );
        let cap = match large_record {
            true => usize::MAX,
            false => MAX_RECORD_FIELD_LEN,
        };
        let mut c = sdk::codec::Cursor::with_cap(bytes, cap);
        let tag = c.byte("record tag")?;
        let record = match tag {
            TAG_PINNED => Record::Pinned {
                frame: c.bytes("pinned frame")?.to_vec(),
            },
            TAG_BLOCK => Record::Block {
                height: c.u64("block height")?,
                frame: c.bytes("block frame")?.to_vec(),
                source: match c.byte("block source")? {
                    0 => BlockSource::Consensus,
                    1 => BlockSource::Catchup {
                        disposition: match c.byte("expected disposition")? {
                            DISP_APPLIED => Disposition::Applied,
                            DISP_REJECTED => Disposition::Rejected,
                            d => return Err(Error::Corrupt(format!("unknown disposition {d}"))),
                        },
                        roots: get_roots(&mut c)?,
                        root_hash: read_root(&mut c, "expected root hash")?,
                    },
                    source => return Err(Error::Corrupt(format!("unknown block source {source}"))),
                },
            },
            TAG_PREPARED_CALL => {
                let height = c.u64("prepared call height")?;
                let call = host::decode_prepared_call(c.bytes("prepared call")?)
                    .map_err(|e| Error::Corrupt(e.to_string()))?;
                Record::PreparedCall { height, call }
            }
            TAG_PREPARED_DELIVERY => {
                let height = c.u64("prepared delivery height")?;
                let delivery = host::decode_prepared_delivery(c.bytes("prepared delivery")?)
                    .map_err(|e| Error::Corrupt(e.to_string()))?;
                Record::PreparedDelivery { height, delivery }
            }
            TAG_SCHEDULE => Record::Schedule {
                height: c.u64("schedule height")?,
                calls: c.u64("schedule calls")?,
                deliveries: c.u64("schedule deliveries")?,
                advance: host::decode_prepared_advance(c.bytes("schedule advance")?)
                    .map_err(|e| Error::Corrupt(e.to_string()))?,
            },
            TAG_TRACE => {
                let height = c.u64("read height")?;
                let unit = c.u64("read unit")?;
                let trace = host::decode_trace(c.bytes("trace")?)
                    .map_err(|e| Error::Corrupt(e.to_string()))?;
                Record::Trace {
                    height,
                    unit,
                    trace,
                }
            }
            TAG_WITNESSED => Record::Witnessed {
                height: c.u64("witnessed height")?,
                units: c.u64("witnessed units")?,
            },
            TAG_SEAL => {
                let height = c.u64("seal height")?;
                let disposition = match c.byte("disposition")? {
                    DISP_APPLIED => Disposition::Applied,
                    DISP_REJECTED => Disposition::Rejected,
                    d => return Err(Error::Corrupt(format!("unknown disposition {d}"))),
                };
                let roots = get_roots(&mut c)?;
                let root_hash = read_root(&mut c, "seal root hash")?;
                Record::Seal {
                    height,
                    disposition,
                    roots,
                    root_hash,
                }
            }
            TAG_CUTOVER => {
                let epoch = c.u64("cutover epoch")?;
                let view_base = c.u64("cutover view base")?;
                let participants = get_keys(&mut c)?;
                let residents = get_keys(&mut c)?;
                Record::Cutover {
                    epoch,
                    view_base,
                    participants,
                    residents,
                }
            }
            t => return Err(Error::Corrupt(format!("unknown record tag {t}"))),
        };
        c.finish("record")?;
        Ok(record)
    }
}

// ============================================================================
// the checkpoint manifest.
// ============================================================================

/// the periodic checkpoint: everything a boot needs BESIDES the disk
/// substrates (which recover themselves) and the journal suffix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// the sealed height this checkpoint captures; `None` = genesis, nothing
    /// applied yet. an explicit option — heights start at the engine's first
    /// view, and some orderers stamp that as 0, so 0 cannot double as a
    /// sentinel.
    pub height: Option<u64>,
    /// the consensus epoch whose engine was live at `height`.
    pub epoch: u64,
    /// that epoch's app-height base (`app_height = view_base + engine view`).
    pub view_base: u64,
    /// the epoch's ENGINE PARTICIPANT SET (raw public-key bytes) — what the
    /// live engine was spawned over. a restart respawns with exactly this
    /// set: the checkpointed valset snapshot may already hold a membership
    /// change whose cutover had not happened yet.
    pub participants: Vec<Vec<u8>>,
    /// the epoch's RESIDENT set (transport standing, no quorum seat) — same
    /// epoch-scoped discipline as `participants`.
    pub residents: Vec<Vec<u8>>,
    /// an epoch cutover armed but not yet crossed at checkpoint time (the
    /// ordered lane's discard-ceiling view). a restart re-arms the same
    /// deterministic boundary its peers are converging on.
    pub pending_cutover_view: Option<u64>,
    /// the composed root-hash at `height`.
    pub root_hash: StateRoot,
    /// every module's root at `height` — the replay baseline.
    pub roots: Vec<(ModuleId, StateRoot)>,
    /// Executable commitments at this checkpoint, including the module registry.
    pub codes: Vec<(ModuleId, Vec<u8>)>,
    /// canonical snapshot bytes for the modules that do NOT persist
    /// themselves (the in-memory cohort), keyed by module id. a disk-cohort
    /// module (`sdk::Module::block_durable`) is deliberately absent: it
    /// reopens its own substrate at boot and a restore installs nothing for
    /// it, so its bytes here would be write amplification nothing reads
    /// (#1308). [`Manifest::capture_timed`] asks the host for exactly that
    /// cohort.
    pub snapshots: Vec<(ModuleId, Vec<u8>)>,
    /// the op-journal position at which this manifest was written; everything
    /// below the PREVIOUS manifest's position is prunable once the persisted
    /// finalization floor has passed its height.
    pub oplog_pos: u64,
    /// the node's next local submit sequence at checkpoint time — restored so
    /// a restarted node advances past every sequence it may already have
    /// framed (the exactly-once digest gate does not survive the process, so
    /// a reused (origin, seq, payload) triple would re-apply).
    pub next_seq: u64,
    /// the node's REPLAY WINDOW at `height`: the `(height, batch id)` pairs it
    /// had journaled, oldest first, capped at [`node::REPLAY_WINDOW_HEIGHTS`].
    ///
    /// the journal suffix a checkpoint leaves behind is SHORTER than the
    /// protocol window — that is the point of checkpointing — so a restart
    /// that rebuilt the window from the suffix alone would refuse fewer
    /// replayed batches than a peer that never restarted, and the two fork on
    /// the difference. carrying it here makes the window a property of the
    /// node's persisted state instead of its uptime: restore seeds from this
    /// and the suffix extends it.
    pub applied_frames: Vec<(u64, node::FrameId)>,
}

impl Manifest {
    /// the WRITE-SIDE twin of [`Manifest::decode`]'s per-field cap.
    ///
    /// `decode` opens its cursor `with_cap(MAX_CHECKPOINT_FIELD_LEN)` and
    /// REFUSES any field over it, so encoding one produces a checkpoint this
    /// node can never restore from — and writing it replaces the previous,
    /// still-restorable manifest and re-anchors the journal prune below a
    /// boundary nothing can recover. The realistic overflow is an in-memory
    /// module's snapshot; the key lists and the list COUNTS are checked
    /// because the same reader
    /// refuses those too, and a refusal the writer cannot reach is a rule
    /// nobody keeps.
    ///
    /// Refusing keeps the PREVIOUS checkpoint plus the journal, which still
    /// restores — the same all-or-nothing stance `capture_timed` takes for a
    /// module that could not prepare a snapshot at all.
    fn check_field_caps(&self) -> Result<(), Error> {
        let counts = [
            ("module roots", self.roots.len()),
            ("module codes", self.codes.len()),
            ("participant keys", self.participants.len()),
            ("resident keys", self.residents.len()),
            ("snapshots", self.snapshots.len()),
        ];
        if let Some((what, n)) = counts.iter().find(|(_, n)| *n > MAX_LIST_LEN) {
            return Err(Error::FieldOverCap(format!(
                "{n} {what} is over the {MAX_LIST_LEN}-entry list cap this crate's own reader \
                 enforces"
            )));
        }
        if self.applied_frames.len() > node::REPLAY_WINDOW_HEIGHTS {
            return Err(Error::FieldOverCap(format!(
                "{} replay window entries is over the {}-entry protocol depth this crate's own \
                 reader enforces",
                self.applied_frames.len(),
                node::REPLAY_WINDOW_HEIGHTS
            )));
        }
        let over_cap = |len: usize| len > MAX_CHECKPOINT_FIELD_LEN;
        for (id, hash) in &self.codes {
            let invalid_hash = hash.len() != 32;
            if invalid_hash {
                return Err(Error::FieldOverCap(format!(
                    "module {id}'s code hash must be 32 bytes"
                )));
            }
        }
        let ids = self
            .roots
            .iter()
            .map(|(id, _)| id)
            .chain(self.codes.iter().map(|(id, _)| id))
            .chain(self.snapshots.iter().map(|(id, _)| id));
        for id in ids {
            let oversized_id = over_cap(id.len());
            if oversized_id {
                return Err(Error::FieldOverCap(
                    "module id exceeds checkpoint field cap".into(),
                ));
            }
        }

        if let Some((id, bytes)) = self.snapshots.iter().find(|(_, b)| over_cap(b.len())) {
            return Err(Error::FieldOverCap(format!(
                "module {id}'s snapshot is {} bytes, over the {MAX_CHECKPOINT_FIELD_LEN}-byte \
                 checkpoint field cap this crate's own reader enforces",
                bytes.len()
            )));
        }
        if let Some(k) = self
            .participants
            .iter()
            .chain(self.residents.iter())
            .find(|k| over_cap(k.len()))
        {
            return Err(Error::FieldOverCap(format!(
                "a validator key field is {} bytes, over the {MAX_CHECKPOINT_FIELD_LEN}-byte \
                 checkpoint field cap this crate's own reader enforces",
                k.len()
            )));
        }
        Ok(())
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self.height {
            Some(h) => {
                out.push(1);
                put_u64(&mut out, h);
            }
            None => out.push(0),
        }
        put_u64(&mut out, self.epoch);
        put_u64(&mut out, self.view_base);
        put_keys(&mut out, &self.participants);
        match self.pending_cutover_view {
            Some(v) => {
                out.push(1);
                put_u64(&mut out, v);
            }
            None => out.push(0),
        }
        put_root(&mut out, &self.root_hash);
        put_roots(&mut out, &self.roots);
        put_u64(&mut out, self.codes.len() as u64);
        for (id, hash) in &self.codes {
            put_bytes(&mut out, id.as_bytes());
            put_bytes(&mut out, hash);
        }
        put_u64(&mut out, self.snapshots.len() as u64);
        for (id, bytes) in &self.snapshots {
            put_bytes(&mut out, id.as_bytes());
            put_bytes(&mut out, bytes);
        }
        put_u64(&mut out, self.oplog_pos);
        put_u64(&mut out, self.next_seq);
        put_keys(&mut out, &self.residents);
        put_u64(&mut out, self.applied_frames.len() as u64);
        for (height, id) in &self.applied_frames {
            put_u64(&mut out, *height);
            out.extend_from_slice(id);
        }
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut c = sdk::codec::Cursor::with_cap(bytes, MAX_CHECKPOINT_FIELD_LEN);
        let height = match c.byte("height tag")? {
            0 => None,
            1 => Some(c.u64("height")?),
            t => return Err(Error::Corrupt(format!("bad height tag {t}"))),
        };
        let epoch = c.u64("epoch")?;
        let view_base = c.u64("view base")?;
        let participants = get_keys(&mut c)?;
        let pending_cutover_view = match c.byte("pending-cutover tag")? {
            0 => None,
            1 => Some(c.u64("pending cutover view")?),
            t => return Err(Error::Corrupt(format!("bad pending-cutover tag {t}"))),
        };
        let root_hash = read_root(&mut c, "root hash")?;
        let roots = get_roots(&mut c)?;
        let count = c.u64("module code count")? as usize;
        if count > MAX_LIST_LEN {
            return Err(Error::Corrupt(
                "module code count exceeds sanity cap".into(),
            ));
        }
        let mut codes = Vec::with_capacity(count);
        for _ in 0..count {
            let id = c.string("module code id")?;
            let hash = c.bytes("module code hash")?;
            let valid_hash = hash.len() == 32;
            if !valid_hash {
                return Err(Error::Corrupt("module code hash must be 32 bytes".into()));
            }
            codes.push((id, hash.to_vec()));
        }
        let n = c.u64("snapshots count")? as usize;
        if n > MAX_LIST_LEN {
            return Err(Error::Corrupt(format!("{n} snapshots exceeds sanity cap")));
        }
        let mut snapshots = Vec::with_capacity(n);
        for _ in 0..n {
            let id = c.string("snapshot module id")?;
            snapshots.push((id, c.bytes("snapshot bytes")?.to_vec()));
        }
        let oplog_pos = c.u64("oplog pos")?;
        let next_seq = c.u64("next seq")?;
        let residents = get_keys(&mut c)?;
        let w = c.u64("replay window count")? as usize;
        if w > node::REPLAY_WINDOW_HEIGHTS {
            return Err(Error::Corrupt(format!(
                "{w} replay window entries exceeds the {}-entry protocol depth",
                node::REPLAY_WINDOW_HEIGHTS
            )));
        }
        let mut applied_frames = Vec::with_capacity(w);
        for _ in 0..w {
            let height = c.u64("replay window height")?;
            applied_frames.push((height, c.array::<32>("replay window batch id")?));
        }
        c.finish("manifest")?;
        Ok(Self {
            height,
            epoch,
            view_base,
            participants,
            residents,
            pending_cutover_view,
            root_hash,
            roots,
            codes,
            snapshots,
            oplog_pos,
            next_seq,
            applied_frames,
        })
    }

    /// look up a stored snapshot by module id.
    pub fn snapshot(&self, id: &str) -> Option<&[u8]> {
        self.snapshots
            .iter()
            .find(|(m, _)| m == id)
            .map(|(_, b)| b.as_slice())
    }

    /// look up a module's checkpointed root.
    pub fn root(&self, id: &str) -> Option<StateRoot> {
        self.roots.iter().find(|(m, _)| m == id).map(|(_, r)| *r)
    }

    /// build a checkpoint manifest from a live host at a settled boundary.
    /// reuses the statesync surface, over the IN-MEMORY COHORT: every module
    /// asked that reports [`sdk::StateSyncHandle::SnapshotBytes`] gets its
    /// bytes stored. a disk-cohort module is not asked at all — it reopens its
    /// own substrate at boot, contributes only its root, and asking would cost
    /// a container the restore path discards (#1308).
    #[allow(clippy::too_many_arguments)]
    pub fn capture(
        host: &Host,
        height: Option<u64>,
        epoch: u64,
        view_base: u64,
        participants: Vec<Vec<u8>>,
        residents: Vec<Vec<u8>>,
        pending_cutover_view: Option<u64>,
        oplog_pos: u64,
        next_seq: u64,
    ) -> Result<Self, Error> {
        Self::capture_timed(
            host,
            height,
            epoch,
            view_base,
            participants,
            residents,
            pending_cutover_view,
            oplog_pos,
            next_seq,
            None,
            || std::time::Duration::ZERO,
        )
        .map(|(manifest, _)| manifest)
    }

    /// [`Manifest::capture`] that also reports what each module COST to
    /// capture, read off the caller's clock (`now`) — the checkpoint runs on
    /// the node's select loop, and an aggregate `capture_ms` cannot name the
    /// module that spent it (#1018).
    ///
    /// `sealed_root` is the root the block at `height` SEALED, when the caller
    /// knows it: the capture is then verified against it and refuses rather
    /// than writing a manifest for a root no block committed.
    #[allow(clippy::too_many_arguments)]
    pub fn capture_timed(
        host: &Host,
        height: Option<u64>,
        epoch: u64,
        view_base: u64,
        participants: Vec<Vec<u8>>,
        residents: Vec<Vec<u8>>,
        pending_cutover_view: Option<u64>,
        oplog_pos: u64,
        next_seq: u64,
        sealed_root: Option<StateRoot>,
        now: impl FnMut() -> std::time::Duration,
    ) -> Result<(Self, Vec<(sdk::ModuleId, std::time::Duration)>), Error> {
        // ONE pass over the registry: the capture computes every module root
        // exactly once and the composite root is derived from those. this used
        // to re-read `host.root_hash()` here (twice) on top of the host's own
        // recompute — four full serializations per map-backed module, for a
        // check that compared the host's root against itself.
        let (snapshot, capture_cost) = host.capture_current_snapshot(
            // a genesis manifest has no boundary yet; 0 is a placeholder, not
            // a height.
            height.unwrap_or(0),
            // THE IN-MEMORY COHORT ONLY. a disk-cohort module reopens its own
            // substrate at its committed position and a restore installs
            // nothing for it, so its container was captured, encoded and
            // fsync'd into every checkpoint only to be thrown away on the way
            // back in — forge's is its whole git pack closure, built on the
            // consensus select loop (#1308). the ROOT is still captured: the
            // position model compares live roots against it.
            host::CapturePayloads::InMemoryCohort,
            now,
        );
        let root_hash = snapshot.root_hash;
        // THE CAPTURE COMPUTES A LIVE ROOT, IT DOES NOT VERIFY ONE. a caller
        // that knows which root `height` SEALED passes it here, and a manifest
        // whose live root is anything else is refused: the host can sit ahead of
        // the sealed boundary (a code-swap realization that stalled before
        // applying the block seats a module and moves the registry root), and a
        // manifest labelled `height` carrying a root no block sealed makes
        // recovery's final compare fatal on every subsequent boot.
        if let Some(sealed) = sealed_root
            && sealed != root_hash
        {
            tracing::warn!(
                target: "ducktape::recovery",
                event = "node_checkpoint_failed",
                reason = "root_hash_unsealed",
                height = height.unwrap_or_default(),
                "checkpoint capture refused: the live root is not the one the block sealed"
            );
            return Err(Error::Storage(format!(
                "checkpoint capture at height {}: live root {:?} is not the sealed root {:?} \
                 — refusing to write a manifest no block committed",
                height.unwrap_or_default(),
                root_hash,
                sealed,
            )));
        }
        // a checkpoint is ALL-OR-NOTHING and that is deliberate: restore reads
        // bytes back per module, so a manifest missing one module's snapshot is
        // a checkpoint that cannot restore — and writing it would prune the
        // journal below a floor nothing can recover from. refusing keeps the
        // PREVIOUS checkpoint plus the journal, which still restores. every
        // degraded module is named, not just the first in registry order.
        if !snapshot.degraded.is_empty() {
            let named = snapshot
                .degraded
                .iter()
                .map(|m| format!("{}: {}", m.id, m.reason))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(Error::Storage(format!(
                "checkpoint capture: modules could not prepare a state-sync \
                 handle, so this checkpoint could not restore — {named}"
            )));
        }
        let roots = snapshot
            .modules
            .iter()
            .map(|m| (m.id.clone(), m.root))
            .collect();
        let codes = snapshot
            .modules
            .iter()
            .filter_map(|module| {
                module
                    .code_hash
                    .clone()
                    .map(|hash| (module.id.clone(), hash))
            })
            .collect();
        let snapshots = snapshot
            .modules
            .into_iter()
            .filter_map(|m| match m.state_sync {
                sdk::StateSyncHandle::SnapshotBytes(bytes) => Some((m.id, bytes)),
                _ => None,
            })
            .collect();
        Ok((
            Self {
                height,
                epoch,
                view_base,
                participants,
                residents,
                pending_cutover_view,
                root_hash,
                roots,
                codes,
                snapshots,
                oplog_pos,
                next_seq,
                // the window is the ORDERED LANE's, not the host's: capture
                // reads state, and the node hands its window in with
                // [`Manifest::with_replay_window`].
                applied_frames: Vec::new(),
            },
            capture_cost,
        ))
    }

    /// stamp the capturing node's replay window onto a captured manifest —
    /// the one seam between the ordered lane's guard and the checkpoint that
    /// has to restore it. truncated to the newest [`node::REPLAY_WINDOW_HEIGHTS`]
    /// entries, exactly as the live window bounds itself.
    #[must_use]
    pub fn with_replay_window(mut self, window: Vec<(u64, node::FrameId)>) -> Self {
        let over = window.len().saturating_sub(node::REPLAY_WINDOW_HEIGHTS);
        self.applied_frames = window[over..].to_vec();
        self
    }
}

/// the persisted finalization floor: the newest certificate whose view the
/// app had FULLY drained when it was recorded — safe to respawn the engine on
/// (`Floor::Finalized`) because nothing at or below it is missing from state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FloorCert {
    pub epoch: u64,
    /// the app height of the certificate's view (`view_base + view`).
    pub height: u64,
    /// the scheme-encoded finalization certificate.
    pub cert: Vec<u8>,
}

impl FloorCert {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_u64(&mut out, self.epoch);
        put_u64(&mut out, self.height);
        put_bytes(&mut out, &self.cert);
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut c = sdk::codec::Cursor::with_cap(bytes, MAX_RECORD_FIELD_LEN);
        let out = Self {
            epoch: c.u64("floor epoch")?,
            height: c.u64("floor height")?,
            cert: c.bytes("floor cert")?.to_vec(),
        };
        c.finish("floor cert")?;
        Ok(out)
    }
}

// ============================================================================
// the store — op journal + manifest + floor cert, all under storage_dir.
// ============================================================================

const PARTITION_OPLOG: &str = "recovery-oplog";
const PARTITION_MANIFEST: &str = "recovery-manifest";
const PARTITION_CERT: &str = "recovery-cert";
const KEY: u64 = 0;

type OpJournal<E> = variable::Journal<E, Vec<u8>>;
type Meta<E> = metadata::Metadata<E, U64, Vec<u8>>;

/// the durable recovery store. implements [`node::BlockSink`] (the ordered
/// lane journals through it live) and drives boot-time replay.
pub struct Recovery<E>
where
    E: Context + BufferPooler + commonware_runtime::Supervisor,
{
    /// every durable write consumes the store and hands it back only on
    /// success, so each lives in an `Option`: a failed write leaves `None`,
    /// and the next touch panics through [`Self::journal`] and friends rather
    /// than reading half-applied state — the node restarts and replays.
    journal: Option<OpJournal<E>>,
    manifest_store: Option<Meta<E>>,
    cert_store: Option<Meta<E>>,
    /// `height -> journal position of that height's [`Record::Block`]`,
    /// covering every block currently retained. built once at [`Self::open`]
    /// from a single full-journal scan, then kept current on every append
    /// ([`Self::pre_apply`]) and prune ([`Self::prune_oplog`]) — the seek aid
    /// that lets [`Self::read_finalized_frames`] jump straight to a height's
    /// record instead of decoding the journal from its start on every call.
    height_index: BTreeMap<u64, u64>,
    /// the out-of-band source of component BYTES for code-registry swaps.
    /// replay reconciles running module code against the committed registry
    /// before each re-applied block (`Host::realize_module_swaps`) — a block
    /// sealed after a swap re-executes on the SAME code it ran live, or its
    /// sealed roots cannot reproduce. defaults to [`host::NoCodeSource`]:
    /// fail-closed at the first armed boundary if the node never wired one.
    /// `Arc` (not `Box`) so the replay loop can share it past `&mut self`.
    code_source: std::sync::Arc<dyn host::CodeSource>,
}

fn storage_err(e: impl std::fmt::Display) -> Error {
    Error::Storage(e.to_string())
}

const LOST: &str = "recovery store lost to a failed durable write; restart to replay";

impl<E> Recovery<E>
where
    E: Context + BufferPooler + commonware_runtime::Supervisor,
{
    fn journal(&self) -> &OpJournal<E> {
        self.journal.as_ref().expect(LOST)
    }

    fn manifest_store(&self) -> &Meta<E> {
        self.manifest_store.as_ref().expect(LOST)
    }

    fn cert_store(&self) -> &Meta<E> {
        self.cert_store.as_ref().expect(LOST)
    }

    async fn sync_journal(&mut self) -> Result<(), Error> {
        let journal = self.journal.take().expect(LOST);
        self.journal = Some(journal.sync().await.map_err(storage_err)?);
        Ok(())
    }
}

/// one paired recovery-journal block and seal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalFrame {
    pub height: u64,
    pub frame: Vec<u8>,
    pub disposition: Disposition,
    pub roots: Vec<(ModuleId, StateRoot)>,
    pub root_hash: StateRoot,
}

impl<E> Recovery<E>
where
    E: Context + BufferPooler + commonware_runtime::Supervisor,
{
    /// open (or create) the recovery store under `context`'s storage root.
    pub async fn open(context: E) -> Result<Self, Error> {
        let page_cache = CacheRef::from_pooler(
            &context,
            NonZeroU16::new(128).expect("nonzero"),
            NonZeroUsize::new(64).expect("nonzero"),
        );
        let journal = OpJournal::<E>::init(
            context.child("oplog"),
            variable::Config {
                partition: PARTITION_OPLOG.into(),
                items_per_section: NonZeroU64::new(64).expect("nonzero"),
                write_buffer: NonZeroUsize::new(1024).expect("nonzero"),
                replay_buffer: NonZeroUsize::new(1 << 16).expect("nonzero"),
                compression: None,
                codec_config: (RangeCfg::from(0..=MAX_RECORD_FIELD_LEN), ()),
                page_cache,
            },
        )
        .await
        .map_err(storage_err)?;
        let manifest_store = Meta::<E>::init(
            context.child("manifest"),
            metadata::Config {
                partition: PARTITION_MANIFEST.into(),
                codec_config: (RangeCfg::from(0..=usize::MAX), ()),
            },
        )
        .await
        .map_err(storage_err)?;
        let cert_store = Meta::<E>::init(
            context.child("cert"),
            metadata::Config {
                partition: PARTITION_CERT.into(),
                codec_config: (RangeCfg::from(0..=MAX_RECORD_FIELD_LEN), ()),
            },
        )
        .await
        .map_err(storage_err)?;
        let height_index = Self::build_height_index(&journal).await?;
        Ok(Self {
            journal: Some(journal),
            manifest_store: Some(manifest_store),
            cert_store: Some(cert_store),
            height_index,
            code_source: std::sync::Arc::new(host::NoCodeSource),
        })
    }

    /// scan the retained journal once (at boot) to map every retained
    /// height to the journal position of its [`Record::Block`]. this is the
    /// one place that still pays a full-journal decode — every later
    /// [`Self::read_finalized_frames`] call seeks straight to a position
    /// this index (as maintained by append/prune) already names.
    async fn build_height_index(journal: &OpJournal<E>) -> Result<BTreeMap<u64, u64>, Error> {
        let mut index = BTreeMap::new();
        let bounds = journal.bounds();
        let stream = journal
            .replay(
                bounds.start,
                NonZeroUsize::new(1 << 16).expect("nonzero"),
                ReadOptions::default(),
            )
            .await
            .map_err(storage_err)?;
        pin_mut!(stream);
        let mut decoder = journal::Records::new(bounds.start);
        while let Some(item) = stream.next().await {
            let (position, bytes) = item.map_err(storage_err)?;
            let Some((position, Record::Block { height, .. })) = decoder.push(position, &bytes)?
            else {
                continue;
            };
            index.insert(height, position);
        }
        Ok(index)
    }

    /// wire the out-of-band component-byte source for code-registry swaps (the
    /// node injects a blobstore-backed one). the default is
    /// [`host::NoCodeSource`] — see the field doc.
    pub fn set_code_source(&mut self, src: std::sync::Arc<dyn host::CodeSource>) {
        self.code_source = src;
    }

    /// the wired code source (the catch-up applier realizes swaps through the
    /// same source replay uses, so every path reconciles identically).
    pub fn code_source(&self) -> std::sync::Arc<dyn host::CodeSource> {
        std::sync::Arc::clone(&self.code_source)
    }

    /// Persist the served outcome with the frame before any suffix execution.
    /// Recovery must reproduce it even when the process stops before sealing.
    pub async fn pre_apply_catchup(
        &mut self,
        frame: &[u8],
        prepared: &PreparedWork,
        expected: &BlockSeal,
    ) -> Result<(), node::Error> {
        let source = BlockSource::Catchup {
            disposition: expected.disposition,
            roots: expected.roots.clone(),
            root_hash: expected.root_hash,
        };
        self.write_pre_apply(expected.height, frame, prepared, source)
            .await
    }

    async fn write_pre_apply(
        &mut self,
        height: u64,
        frame: &[u8],
        prepared: &PreparedWork,
        source: BlockSource,
    ) -> Result<(), node::Error> {
        let mut records = vec![
            Record::Block {
                height,
                frame: frame.to_vec(),
                source,
            }
            .encode(),
        ];
        for call in &prepared.calls {
            records.push(
                Record::PreparedCall {
                    height,
                    call: call.clone(),
                }
                .encode(),
            );
        }
        for delivery in &prepared.deliveries {
            records.push(
                Record::PreparedDelivery {
                    height,
                    delivery: delivery.clone(),
                }
                .encode(),
            );
        }
        records.push(
            Record::Schedule {
                height,
                calls: prepared.calls.len() as u64,
                deliveries: prepared.deliveries.len() as u64,
                advance: prepared.advance.clone(),
            }
            .encode(),
        );
        let position = self.journal().size();
        for record in &records {
            self.append_record(record).await?;
        }
        self.sync_journal().await?;
        self.height_index.insert(height, position);
        Ok(())
    }

    async fn append_record(&mut self, record: &[u8]) -> Result<(), Error> {
        for piece in journal::pieces(record) {
            let journal = self.journal.take().expect(LOST);
            let (journal, _pos) = journal.append(&piece).await.map_err(storage_err)?;
            self.journal = Some(journal);
        }
        Ok(())
    }

    /// Decode complete logical records and identify an incomplete tail. A
    /// recovery writer removes that tail before appending another record.
    async fn records(&self) -> Result<(Vec<(u64, Record)>, Option<u64>), Error> {
        let journal = self.journal();
        let bounds = journal.bounds();
        let stream = journal
            .replay(
                bounds.start,
                NonZeroUsize::new(1 << 16).expect("nonzero"),
                ReadOptions::default(),
            )
            .await
            .map_err(storage_err)?;
        pin_mut!(stream);
        let mut decoder = journal::Records::new(bounds.start);
        let mut records = Vec::new();
        while let Some(item) = stream.next().await {
            let (position, bytes) = item.map_err(storage_err)?;
            if let Some(record) = decoder.push(position, &bytes)? {
                records.push(record);
            }
        }
        Ok((records, decoder.incomplete_position()))
    }

    /// the persisted checkpoint, if any. `None` means this storage dir has
    /// never run with recovery — a fresh genesis boot.
    pub fn manifest(&self) -> Result<Option<Manifest>, Error> {
        self.manifest_store()
            .get(&U64::new(KEY))
            .map(|b| Manifest::decode(b))
            .transpose()
    }

    /// true when the op journal holds any records (fresh-boot guard: a
    /// journal without a manifest is damaged state, not a fresh dir).
    pub async fn journal_is_empty(&self) -> bool {
        self.journal().size() == 0
    }

    /// atomically persist a checkpoint manifest. syncs the op journal first
    /// so the manifest can never be newer than the journal it summarizes.
    ///
    /// REFUSES a manifest this crate's own reader would reject (see
    /// [`Manifest::check_field_caps`]) before the store write, so nothing
    /// unreadable ever reaches disk and the previous checkpoint stays exactly
    /// where it is. The journal sync happens FIRST and unconditionally: it is
    /// the shutdown barrier `graceful_checkpoint` leans on, and a refused
    /// manifest must not leave buffered journal appends unflushed.
    pub async fn write_manifest(&mut self, manifest: &Manifest) -> Result<(), Error> {
        self.sync_journal().await?;
        if let Err(e) = manifest.check_field_caps() {
            tracing::error!(
                target: "ducktape::recovery",
                reason = "checkpoint_field_over_cap",
                height = manifest.height.unwrap_or_default(),
                error = %e,
                "checkpoint refused: this node cannot restore from a manifest it \
                 cannot read back, so the previous one stays on disk"
            );
            return Err(e);
        }
        let store = self.manifest_store.take().expect(LOST);
        let store = store
            .put_sync(U64::new(KEY), manifest.encode())
            .await
            .map_err(storage_err)?;
        self.manifest_store = Some(store);
        Ok(())
    }

    /// the current op-journal append position (recorded into manifests).
    pub async fn oplog_pos(&self) -> u64 {
        self.journal().size()
    }

    /// force every buffered journal append durable. NOT part of any live path:
    /// every record the sink writes fsyncs where it is written, so nothing in
    /// the node needs a separate barrier. it survives for the power-loss
    /// harness, which wraps this sink to swallow a seal and then syncs the
    /// inner journal by hand — the only way to build "durable except the seal".
    pub async fn sync(&mut self) -> Result<(), Error> {
        self.sync_journal().await
    }

    /// the persisted finalization floor, if any.
    pub fn floor_cert(&self) -> Result<Option<FloorCert>, Error> {
        self.cert_store()
            .get(&U64::new(KEY))
            .map(|b| FloorCert::decode(b))
            .transpose()
    }

    /// persist the finalization floor (see [`FloorCert`]). callers only write
    /// this when the ordered lane has fully drained everything at or below
    /// the certificate's view.
    pub async fn write_floor_cert(&mut self, cert: &FloorCert) -> Result<(), Error> {
        let store = self.cert_store.take().expect(LOST);
        let store = store
            .put_sync(U64::new(KEY), cert.encode())
            .await
            .map_err(storage_err)?;
        self.cert_store = Some(store);
        Ok(())
    }

    /// prune op-journal records below `pos` (a PREVIOUS manifest's
    /// `oplog_pos`). callers gate this on the persisted floor cert having
    /// passed that manifest's height — pruned frames must never be needed to
    /// resolve a re-reported finalization.
    pub async fn prune_oplog(&mut self, pos: u64) -> Result<(), Error> {
        let journal = self.journal.take().expect(LOST);
        let (journal, _pruned) = journal.prune(pos).await.map_err(storage_err)?;
        self.journal = Some(journal);
        self.height_index
            .retain(|_, &mut record_pos| record_pos >= pos);
        Ok(())
    }

    /// read at most `limit` sealed recovery frames in `(after_height,
    /// up_to_height]`.
    ///
    /// This is the durable equivalent of a restart's replay suffix. The local
    /// checkpoint height is the retained suffix boundary: asking below it is a
    /// pruned-range condition, even if old journal bytes have not yet been
    /// physically removed.
    ///
    /// seeks straight to `after_height`'s successor via [`Self::height_index`]
    /// and stops decoding once `limit` frames are collected — a caller no
    /// longer pays for the retained journal's full size, only for the batch
    /// it asked for.
    pub async fn read_finalized_frames(
        &self,
        after_height: u64,
        up_to_height: u64,
        limit: usize,
    ) -> Result<Vec<JournalFrame>, Error> {
        let manifest_height = self.manifest()?.and_then(|m| m.height);
        read_finalized_frames_from(
            self.journal(),
            &self.height_index,
            manifest_height,
            after_height,
            up_to_height,
            limit,
        )
        .await
    }

    /// a cheap, self-contained handle for serving [`Self::read_finalized_frames`]
    /// off whatever loop holds `&mut Recovery`: the journal hands out an owned
    /// `'static` snapshot reader over the blobs as they stand now, and the
    /// height index is a bounded snapshot. a caller hands this to a spawned
    /// task and moves on immediately, instead of awaiting the decode inline.
    pub async fn frame_reader(&mut self) -> Result<FrameReader<E>, Error> {
        let manifest_height = self.manifest()?.and_then(|m| m.height);
        let journal = self.journal.take().expect(LOST);
        let (journal, reader) = journal.snapshot().await.map_err(storage_err)?;
        self.journal = Some(journal);
        Ok(FrameReader {
            reader,
            height_index: self.height_index.clone(),
            manifest_height,
        })
    }
}

/// [`Recovery::frame_reader`]'s handle: everything [`read_finalized_frames_from`]
/// needs, owned independently of `Recovery` itself.
pub struct FrameReader<E: Context> {
    reader: variable::Reader<'static, E, Vec<u8>>,
    height_index: BTreeMap<u64, u64>,
    manifest_height: Option<u64>,
}

impl<E: Context> FrameReader<E> {
    /// see [`Recovery::read_finalized_frames`].
    pub async fn read_finalized_frames(
        &self,
        after_height: u64,
        up_to_height: u64,
        limit: usize,
    ) -> Result<Vec<JournalFrame>, Error> {
        read_finalized_frames_from(
            &self.reader,
            &self.height_index,
            self.manifest_height,
            after_height,
            up_to_height,
            limit,
        )
        .await
    }
}

/// shared body of [`Recovery::read_finalized_frames`] and
/// [`FrameReader::read_finalized_frames`]: seeks the journal to the position
/// of the first retained record above `after_height` (via `height_index`)
/// rather than decoding from the journal's start, and stops once `up_to_height`
/// or `limit` frames have been collected.
/// test-only tripwire: counts every record [`read_finalized_frames_from`]
/// decodes, so a unit test can assert the height-index seek keeps that count
/// near the requested batch instead of the whole retained journal (#1814).
#[cfg(test)]
static TEST_DECODED_RECORDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

async fn read_finalized_frames_from<J: Contiguous<Item = Vec<u8>>>(
    journal: &J,
    height_index: &BTreeMap<u64, u64>,
    manifest_height: Option<u64>,
    after_height: u64,
    up_to_height: u64,
    limit: usize,
) -> Result<Vec<JournalFrame>, Error> {
    if after_height > up_to_height {
        return Err(Error::Corrupt(format!(
            "invalid frame range ({after_height}, {up_to_height}]"
        )));
    }
    if after_height == up_to_height || limit == 0 {
        return Ok(Vec::new());
    }

    // the honest retention floor is the journal's own first retained block
    // (the lowest key the height index carries). the latest MANIFEST height
    // is only a proxy: it advances on every periodic checkpoint even when
    // the physical prune is deferred (the sync retention lease), and
    // refusing against it starves a slow syncer of frames that are still
    // right here — the rebootstrap treadmill. an empty index has no floor of
    // its own, so the manifest boundary remains the anchor there.
    let Some(&first_retained) = height_index.keys().next() else {
        if let Some(retained_start) = manifest_height
            && after_height < retained_start
        {
            return Err(Error::RangePruned {
                after_height,
                retained_start,
            });
        }
        return Ok(Vec::new());
    };
    // report the lowest ANCHORABLE height: a client at `first - 1` can be
    // served (its next frame is the first retained one), so that is the
    // floor it can act on — and it matches the checkpoint the physical
    // prune trails in the steady state.
    let retained_start = first_retained.saturating_sub(1);
    if after_height < retained_start {
        return Err(Error::RangePruned {
            after_height,
            retained_start,
        });
    }

    // the seek target: the position of the first retained block strictly
    // above `after_height`. nothing in range means nothing to decode.
    let seek = height_index
        .range((
            std::ops::Bound::Excluded(after_height),
            std::ops::Bound::Unbounded,
        ))
        .next();
    let Some((&start_height, &start_pos)) = seek else {
        return Ok(Vec::new());
    };
    if start_height > up_to_height {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    let mut pending: Option<(u64, Vec<u8>, BlockSource)> = None;
    {
        let stream = journal
            .replay(
                start_pos,
                NonZeroUsize::new(1 << 16).expect("nonzero"),
                ReadOptions::default(),
            )
            .await
            .map_err(storage_err)?;
        pin_mut!(stream);
        let mut decoder = journal::Records::new(start_pos);
        while out.len() < limit {
            let Some(item) = stream.next().await else {
                break;
            };
            let (position, bytes) = item.map_err(storage_err)?;
            #[cfg(test)]
            TEST_DECODED_RECORDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Some((_, record)) = decoder.push(position, &bytes)? else {
                continue;
            };
            match record {
                Record::Block {
                    height,
                    frame,
                    source,
                } => {
                    if height <= after_height {
                        continue;
                    }
                    if height > up_to_height {
                        break;
                    }
                    if let Some((prev, _, _)) = pending {
                        return Err(Error::Corrupt(format!(
                            "block {height} appeared before block {prev} was sealed"
                        )));
                    }
                    pending = Some((height, frame, source));
                }
                Record::Seal {
                    height,
                    disposition,
                    roots,
                    root_hash,
                } => {
                    if height <= after_height {
                        continue;
                    }
                    if height > up_to_height {
                        break;
                    }
                    let Some((block_height, frame, source)) = pending.take() else {
                        return Err(Error::Corrupt(format!(
                            "seal at height {height} without its block record"
                        )));
                    };
                    if block_height != height {
                        return Err(Error::Corrupt(format!(
                            "seal height {height} does not match block {block_height}"
                        )));
                    }
                    source.verify(&BlockSeal {
                        height,
                        disposition,
                        roots: roots.clone(),
                        root_hash,
                    })?;
                    out.push(JournalFrame {
                        height,
                        frame,
                        disposition,
                        roots,
                        root_hash,
                    });
                }
                Record::Pinned { .. }
                | Record::Cutover { .. }
                | Record::PreparedCall { .. }
                | Record::PreparedDelivery { .. }
                | Record::Schedule { .. }
                | Record::Trace { .. }
                | Record::Witnessed { .. } => {}
            }
        }
    }
    if let Some((height, _, _)) = pending {
        return Err(Error::Corrupt(format!(
            "block {height} in requested range is missing its seal"
        )));
    }
    Ok(out)
}

// the live sink: append and sync records as the ordered lane drives it. sync
// policy: every record this sink writes is a BARRIER. `pin` and `pre_apply`
// must be durable before the engine/host may act; `seal` must be durable
// because a store-backed tenant has ALREADY durably committed the block to its
// own disk by the time it is written, and the seal is the only record that
// vouches for that state. the residual window is the tail of block apply —
// between the first tenant's commit and this sync — and is not closable by a
// per-store cursor: `trailing.rs` refuses a block claimed by more than one
// substrate, because there is no cross-substrate atomicity. the seal IS the
// cross-substrate barrier.
impl<E> BlockSink for Recovery<E>
where
    E: Context + BufferPooler + commonware_runtime::Supervisor,
{
    fn pin(&mut self, frame: &[u8]) -> impl std::future::Future<Output = Result<(), node::Error>> {
        let record = Record::Pinned {
            frame: frame.to_vec(),
        }
        .encode();
        async move {
            self.append_record(&record).await?;
            self.sync_journal().await?;
            Ok(())
        }
    }

    fn pre_apply(
        &mut self,
        height: u64,
        frame: &[u8],
        prepared: &PreparedWork,
    ) -> impl std::future::Future<Output = Result<(), node::Error>> {
        self.write_pre_apply(height, frame, prepared, BlockSource::Consensus)
    }

    fn witness(
        &mut self,
        height: u64,
        witness: &Witness,
    ) -> impl std::future::Future<Output = Result<(), node::Error>> {
        // One logical record per trace entry, then the unit count — all
        // durable before the host commits its first module.
        let mut records = Vec::new();
        for (unit, observed) in witness.units.iter().enumerate() {
            for trace in &observed.trace {
                records.push(
                    Record::Trace {
                        height,
                        unit: unit as u64,
                        trace: trace.clone(),
                    }
                    .encode(),
                );
            }
        }
        records.push(
            Record::Witnessed {
                height,
                units: witness.units.len() as u64,
            }
            .encode(),
        );
        async move {
            for record in &records {
                self.append_record(record).await?;
            }
            // the witness is a BARRIER like the seal: the host commits its
            // first module the moment this returns.
            self.sync_journal().await?;
            Ok(())
        }
    }

    fn seal(
        &mut self,
        seal: &BlockSeal,
    ) -> impl std::future::Future<Output = Result<(), node::Error>> {
        let record = Record::Seal {
            height: seal.height,
            disposition: seal.disposition,
            roots: seal.roots.clone(),
            root_hash: seal.root_hash,
        }
        .encode();
        async move {
            self.append_record(&record).await?;
            // the seal is a BARRIER, not a plain append. a store-backed tenant
            // durably commits its own disk during apply (`MerkleStore::commit_batch`
            // is "apply + durably commit"), so from the moment the first one
            // returns, this node holds state that only the seal can vouch for.
            // syncing HERE — rather than deferring to the next block's
            // `pre_apply`, or to a barrier the drain loop remembers to take —
            // is what makes that vouching durable at the same instant the
            // state is. it is also the only place that cannot be forgotten.
            self.sync_journal().await?;
            Ok(())
        }
    }

    fn cutover(
        &mut self,
        epoch: u64,
        view_base: u64,
        participants: &[Vec<u8>],
        residents: &[Vec<u8>],
    ) -> impl std::future::Future<Output = Result<(), node::Error>> {
        let record = Record::Cutover {
            epoch,
            view_base,
            participants: participants.to_vec(),
            residents: residents.to_vec(),
        }
        .encode();
        async move {
            self.append_record(&record).await?;
            self.sync_journal().await?;
            Ok(())
        }
    }
}

#[async_trait::async_trait(?Send)]
impl<E> host::CommitWitness for Recovery<E>
where
    E: Context + BufferPooler + commonware_runtime::Supervisor,
{
    async fn record(&mut self, height: u64, witness: &Witness) -> Result<(), String> {
        BlockSink::witness(self, height, witness)
            .await
            .map_err(|e| e.to_string())
    }
}

// ============================================================================
// boot-time replay.
// ============================================================================

/// one re-executed sealed block, as replay hands it to a [`ReplaySink`]: the
/// SEALED frame bytes, how the block landed, the composed root-hash it left
/// behind (the seal's recorded value), and the deterministic dispatch trace
/// (empty for a rejected block). the frame + disposition + root-hash ride
/// along because a node-layer observer (the explorer's blocks database)
/// derives its row from the frame's content, not the dispatch trace — the
/// trace alone cannot reproduce it.
pub struct FoldedBlock<'a> {
    /// The deployments realized for this replayed block.
    pub host: &'a Host,
    pub height: u64,
    pub frame: &'a [u8],
    pub disposition: Disposition,
    pub root_hash: StateRoot,
    pub dispatches: &'a [DispatchRecord],
    /// the host's module set after this block — what the seal records. a
    /// module the block's boundary admitted appears here for the first
    /// time, so a derived tier can cover it before its first op folds.
    pub roots: &'a [(ModuleId, StateRoot)],
}

/// observer of every sealed block the journal replay walks, in height order —
/// the seam a derived tier (the per-module read-model index) folds from, so a
/// restart re-derives exactly what the live drain would have fed it.
///
/// two shapes, because replay cannot always reproduce a block's content:
/// - a RE-EXECUTED block surfaces its sealed frame and the deterministic
///   dispatch trace consensus applied ([`ReplaySink::folded_block`]; the
///   trace is empty for a rejected block);
/// - a block replay SKIPS — its state already durable, or root-idempotent —
///   has no reproducible trace ([`ReplaySink::opaque_block`]). the observer
///   decides what an unreproducible height means for its tier (the index
///   stops folding and lets its from-state rebuild repair the gap).
///
/// observation is best-effort by design: sink calls return nothing and MUST
/// not fail — recovery's own verification never depends on them.
pub trait ReplaySink {
    fn folded_block(&mut self, block: &FoldedBlock<'_>);
    fn opaque_block(&mut self, height: u64);
}

/// what a completed recovery hands back to the boot path.
#[derive(Debug)]
pub struct Recovered {
    /// the journal tip: the last sealed height (`None` = nothing applied) and
    /// the verified composed root-hash there.
    pub height: Option<u64>,
    pub root_hash: StateRoot,
    /// the consensus epoch to respawn and its app-height base.
    pub epoch: u64,
    pub view_base: u64,
    /// the epoch's engine participant set: the manifest's, superseded by any
    /// newer [`Record::Cutover`] the journal retained.
    pub participants: Vec<Vec<u8>>,
    /// the epoch's resident set, recovered with the same precedence.
    pub residents: Vec<Vec<u8>>,
    /// every retained frame's bytes (pins and blocks) — the boot path seeds
    /// the consensus content store with these so re-reported finalizations
    /// resolve locally instead of wedging the ordered gate.
    pub frames: Vec<Vec<u8>>,
    /// every post-checkpoint sealed block's `(height, post-block roots)`, in
    /// order — the boot path scans these to re-derive a cutover that was
    /// armed by a block ABOVE the checkpoint (the checkpoint itself records
    /// one armed at or below it via `pending_cutover_view`).
    pub blocks: Vec<(u64, Vec<(ModuleId, StateRoot)>)>,
    /// the restored REPLAY WINDOW — the checkpoint's own
    /// [`Manifest::applied_frames`] extended by every sealed block the journal
    /// suffix walked, one entry per height in ascending order, bounded to
    /// [`node::REPLAY_WINDOW_HEIGHTS`]. the boot path hands these to
    /// `OrderedNode::seed_replay_window`, so a restarted node's window is as
    /// deep as a peer that never restarted rather than as deep as whatever
    /// journal the last checkpoint happened to leave behind.
    pub applied_frames: Vec<(u64, node::FrameId)>,
    /// replay accounting, for the boot log line.
    pub applied: usize,
    pub skipped: usize,
    pub rolled_forward: bool,
}

impl<E> Recovery<E>
where
    E: Context + BufferPooler + commonware_runtime::Supervisor,
{
    /// roll `host` forward from the checkpoint to the journal tip.
    ///
    /// the caller has already installed `manifest`'s snapshots into the
    /// in-memory modules and reopened the disk substrates at their own
    /// positions; this replays the journal suffix, skipping blocks a module
    /// already contains (root equality) and re-applying the rest, verifying
    /// each re-applied block against its sealed roots and the final state
    /// against the tip's sealed root-hash.
    pub async fn recover(
        &mut self,
        host: &mut Host,
        manifest: &Manifest,
    ) -> Result<Recovered, Error> {
        self.recover_with_sink(host, manifest, None).await
    }

    /// [`Recovery::recover`], additionally reporting every sealed block the
    /// replay walks to `sink` (see [`ReplaySink`]). recovery's own behavior
    /// and verification are identical with or without an observer.
    pub async fn recover_with_sink(
        &mut self,
        host: &mut Host,
        manifest: &Manifest,
        mut sink: Option<&mut dyn ReplaySink>,
    ) -> Result<Recovered, Error> {
        // shared past the `&mut self` journal borrows below (see the field doc).
        let code_source = std::sync::Arc::clone(&self.code_source);
        // the checkpoint's roots. the ADOPTED baseline — every module the
        // manifest never captured — is seeded below, after the durable floors
        // that place the disk cohort exist (a floor is a RECORD; the adopted
        // baseline is an inference, and the record wins).
        let mut expected: BTreeMap<ModuleId, StateRoot> = manifest.roots.iter().cloned().collect();
        let mut tip_height: Option<u64> = manifest.height;
        let mut tip_hash = manifest.root_hash;
        let mut epoch = manifest.epoch;
        let mut view_base = manifest.view_base;
        let mut participants = manifest.participants.clone();
        let mut residents = manifest.residents.clone();
        let mut frames: Vec<Vec<u8>> = Vec::new();
        let mut blocks: Vec<(u64, Vec<(ModuleId, StateRoot)>)> = Vec::new();
        // the replay window starts at the CHECKPOINT's, not empty: the journal
        // suffix below only reaches back to the checkpoint, and a window
        // shorter than a peer's is a fork waiting on a replayed batch.
        let mut applied_frames: Vec<(u64, node::FrameId)> = manifest.applied_frames.clone();
        let mut pending: Option<PendingBlock> = None;
        let mut applied = 0usize;
        let mut skipped = 0usize;

        // decode the retained journal into records first: replay borrows the
        // reader, and applying blocks needs `&mut host` with no borrow held.
        let (located, tail) = self.records().await?;
        if let Some(position) = tail {
            let journal = self.journal.take().expect(LOST);
            self.journal = Some(journal.rewind(position).await.map_err(storage_err)?);
            self.sync_journal().await?;
        }
        let last_block_position = located.iter().rev().find_map(|(position, record)| {
            matches!(record, Record::Block { .. }).then_some(*position)
        });
        let records: Vec<Record> = located.into_iter().map(|(_, record)| record).collect();

        // forward pre-scan — seed each per-block-durable disk substrate's
        // "durable floor". a disk-cohort (`block_durable`) module commits to
        // its OWN disk every block, but the checkpoint only persists on a cadence
        // (default 32 blocks), so at boot a disk module can legitimately sit N
        // blocks AHEAD of the checkpoint: its live root equals a recorded
        // post-root well above the last checkpoint, matching NEITHER the
        // checkpoint pre-root NOR the first replayed block's post-root. the
        // sequential per-height classifier has no forward lookahead, so it would
        // false-Torn (BRICK) such a module at the first replayed height. so here,
        // BEFORE the loop, record for each disk module the LATEST sealed height
        // whose recorded post-root EXACTLY equals the module's live root, and let
        // the loop treat every block up to that height as already-durable for it,
        // replaying only STRICTLY above it. exact-match only: a disk module whose
        // live root matches NO recorded post-root keeps no floor and still
        // fail-stops as `Error::Torn` below (genuine corruption / a torn write) —
        // recovery never heals from a nearest/approximate record. a mis-read
        // root could only mis-seed a floor and trip the final root-hash
        // recompose (fail-stop), never fork.
        let disk_cohort = host.block_durable_ids();
        let mut disk_floor: BTreeMap<ModuleId, u64> = BTreeMap::new();
        if !disk_cohort.is_empty() {
            for record in &records {
                let Record::Seal { height, roots, .. } = record else {
                    continue;
                };
                if manifest.height.is_some_and(|h| *height <= h) {
                    continue; // pre-checkpoint remnant, not this replay window.
                }
                for (id, root) in roots {
                    if disk_cohort.contains(id) && host.module_root(id) == Some(*root) {
                        // ascending replay order: a later match overwrites, so
                        // this ends at the LATEST height the disk root matches.
                        disk_floor.insert(id.clone(), *height);
                    }
                }
            }
        }
        // TRAILING bound-and-verify (see `trailing.rs`). `seal` fsyncs where it
        // is written, so the window is the TAIL OF BLOCK APPLY — between a disk
        // module's own durable commit and that sync — and not everything up to
        // the next pre-apply. a crash there leaves the module's live root
        // matching NO recorded post-root, which the exact-match scan above
        // rightly refuses to floor. a disk module carrying a per-commit height
        // cursor (persisted atomically with its own commit) is floored AT the
        // trailing unsealed WAL height iff the cursor claims exactly that
        // height — binding the live root to the one finalized frame the WAL
        // still holds durably. everything else stays floorless (Torn).
        let trailing_claims = trailing::seed_trailing_claims(
            host,
            &disk_cohort,
            &expected,
            trailing::trailing_wal_height(&records, manifest.height),
            &mut disk_floor,
        );

        // the ADOPTED baseline. a module the manifest never captured was
        // ADMITTED above the checkpoint, so the block that activates it — and
        // may carry its first op — must be found `at_pre`, never torn: its
        // pre-root is what a FRESHLY SEATED substrate holds. that is the live
        // root at boot for the in-memory cohort (reinstalled from the
        // checkpoint, or adopted empty), and for a per-block-durable module
        // whose own commit for the admission block died with the crash.
        //
        // the exception is the per-block-durable module a RECORD already
        // places — a durable floor above, from the pre-scan's exact post-root
        // match or a verified height-cursor claim. its seat reopened its own
        // canonical store, durably at or past the admission block, and seeding
        // THAT live root as a pre-root made the activating block look `at_pre`:
        // the torn branch then re-committed the block into a store that already
        // held it, moving its op-log root and fail-stopping the boot at the
        // final recompose. the floor places it instead — `ahead`/durable, as it
        // places every other per-block-durable module.
        for (id, root) in host.module_roots() {
            if disk_floor.contains_key(&id) {
                continue;
            }
            expected.entry(id).or_insert(root);
        }

        for record in records {
            match record {
                Record::Pinned { frame } => frames.push(frame),
                Record::Cutover {
                    epoch: e,
                    view_base: b,
                    participants: p,
                    residents: o,
                } => {
                    // monotone: a stale record retained from below the
                    // checkpoint must not regress the manifest's values.
                    if e > epoch {
                        epoch = e;
                        view_base = b;
                        participants = p;
                        residents = o;
                    }
                }
                Record::Block {
                    height,
                    frame,
                    source,
                } => {
                    if manifest.height.is_some_and(|h| height <= h) {
                        continue; // pre-checkpoint remnant, not yet pruned.
                    }
                    if let Some(open) = &pending {
                        return Err(Error::Corrupt(format!(
                            "two unsealed blocks ({} then {height}) — the WAL never leaves \
                             more than one apply in flight",
                            open.height
                        )));
                    }
                    frames.push(frame.clone());
                    pending = Some(PendingBlock::open(height, frame, source));
                }
                Record::PreparedCall { height, call } => {
                    if manifest.height.is_some_and(|h| height <= h) {
                        continue;
                    }
                    pending_block_mut(&mut pending, height, "prepared call")?
                        .prepared
                        .calls
                        .push(call);
                }
                Record::PreparedDelivery { height, delivery } => {
                    if manifest.height.is_some_and(|h| height <= h) {
                        continue;
                    }
                    pending_block_mut(&mut pending, height, "prepared delivery")?
                        .prepared
                        .deliveries
                        .push(delivery);
                }
                Record::Schedule {
                    height,
                    calls,
                    deliveries,
                    advance,
                } => {
                    if manifest.height.is_some_and(|h| height <= h) {
                        continue;
                    }
                    pending_block_mut(&mut pending, height, "schedule")?
                        .close(calls, deliveries, advance)?;
                }
                Record::Trace {
                    height,
                    unit,
                    trace,
                } => {
                    if manifest.height.is_some_and(|h| height <= h) {
                        continue;
                    }
                    pending_block_mut(&mut pending, height, "read")?.trace(unit, trace)?;
                }
                Record::Witnessed { height, units } => {
                    if manifest.height.is_some_and(|h| height <= h) {
                        continue;
                    }
                    pending_block_mut(&mut pending, height, "witness")?.witnessed(units)?;
                }
                Record::Seal {
                    height,
                    disposition,
                    roots,
                    root_hash,
                } => {
                    if manifest.height.is_some_and(|h| height <= h) {
                        continue;
                    }
                    let Some(block) = pending.take() else {
                        return Err(Error::Corrupt(format!(
                            "seal at height {height} without its block record"
                        )));
                    };
                    if block.height != height {
                        return Err(Error::Corrupt(format!(
                            "seal height {height} does not match its block record {}",
                            block.height
                        )));
                    }
                    block.source.verify(&BlockSeal {
                        height,
                        disposition,
                        roots: roots.clone(),
                        root_hash,
                    })?;
                    // Applied blocks have a complete precommit witness;
                    // rejected blocks may never have entered the host.
                    let (frame, prepared, witness) = block.sealed(disposition)?;
                    // the modules this block CHANGED: the replay unit. a pure
                    // comparison of RECORDED root values.
                    let changed: Vec<(ModuleId, StateRoot)> = roots
                        .iter()
                        .filter(|(id, root)| expected.get(id) != Some(root))
                        .cloned()
                        .collect();
                    if changed.is_empty() {
                        // a rejected or root-idempotent block: nothing durable
                        // to redo (re-applying could MOVE a history-committed
                        // root and fork us).
                        skipped += 1;
                        if let Some(sink) = sink.as_mut() {
                            match disposition {
                                // a rejected block never had content anywhere.
                                Disposition::Rejected => sink.folded_block(&FoldedBlock {
                                    host,
                                    height,
                                    frame: &frame,
                                    disposition,
                                    root_hash,
                                    dispatches: &[],
                                    roots: &roots,
                                }),
                                // an applied block whose ops moved no root:
                                // its trace existed at runtime but is not
                                // re-executed here — unreproducible.
                                _ => sink.opaque_block(height),
                            }
                        }
                    } else {
                        // classify each CHANGED module once: "still at the
                        // pre-block roots?" vs "already at the sealed post-roots?"
                        // (a disk substrate that applied this block before the
                        // crash).
                        let at_pre_of: Vec<bool> = changed
                            .iter()
                            .map(|(id, _)| host.module_root(id) == expected.get(id).copied())
                            .collect();
                        let at_post_of: Vec<bool> = changed
                            .iter()
                            .map(|(id, root)| host.module_root(id) == Some(*root))
                            .collect();
                        // a disk-cohort module whose live root exactly matches a
                        // recorded post-root STRICTLY ABOVE this height raced past
                        // this block (its durable floor, seeded by the forward
                        // pre-scan). the checkpoint-to-tip gap for a per-block-
                        // durable disk substrate is bounded only by checkpoint
                        // cadence, so recovery must trust the disk module's own
                        // self-durable root and replay only strictly above it;
                        // re-committing it here would move its op-log root and fork.
                        // (`at_post_of`, floor == height, is the single-block-ahead
                        // special case; this generalizes it to N blocks ahead.)
                        let ahead_of: Vec<bool> = changed
                            .iter()
                            .map(|(id, _)| disk_floor.get(id).is_some_and(|m| *m > height))
                            .collect();
                        // "durable" = already at this block's post-root OR raced
                        // strictly past it: either way a per-block-durable disk
                        // substrate committed here and must NOT be re-committed.
                        let all_durable = (0..changed.len()).all(|i| at_post_of[i] || ahead_of[i]);
                        let all_pre = at_pre_of.iter().all(|&b| b);
                        if all_durable {
                            // nothing to re-commit: the disk cohort already holds
                            // this block at or beyond its post-root. (subsumes the
                            // old all-at-post fast path; adds the N-ahead case.)
                            skipped += 1;
                            if let Some(sink) = sink.as_mut() {
                                sink.opaque_block(height);
                            }
                        } else if all_pre {
                            let (_, dispatches) = apply_block(
                                host,
                                height,
                                &frame,
                                prepared,
                                witness,
                                Some(disposition),
                                code_source.as_ref(),
                                &mut host::NoWitness,
                            )
                            .await?;
                            if let Some(sink) = sink.as_mut() {
                                sink.folded_block(&FoldedBlock {
                                    host,
                                    height,
                                    frame: &frame,
                                    disposition,
                                    root_hash,
                                    dispatches: &dispatches,
                                    roots: &host.module_roots(),
                                });
                            }
                            for (id, root) in &changed {
                                let live = host.module_root(id);
                                if live != Some(*root) {
                                    return Err(Error::Verify(format!(
                                        "replayed block {height} left module {id} at \
                                         {live:?}, sealed root was {root:?}"
                                    )));
                                }
                            }
                            applied += 1;
                        } else {
                            // a TORN block: some changed modules are at their
                            // pre-root (rolled back to the checkpoint — the
                            // in-memory cohort), others durable on their own disk
                            // (per-block-durable substrates already at, or raced
                            // past, their sealed post-root). re-run the sealed
                            // frame but commit ONLY the still-at-pre cohort and
                            // ABORT the durable ones — re-committing a qmdb store
                            // would move its op-log root and fork us.
                            let mut commit_only: BTreeSet<ModuleId> = BTreeSet::new();
                            let mut durable = 0usize;
                            for (i, (id, _root)) in changed.iter().enumerate() {
                                // a rolled-back in-memory module is at pre; a disk
                                // substrate at or past its post-root is durable.
                                if at_pre_of[i] {
                                    commit_only.insert(id.clone());
                                } else if at_post_of[i] || ahead_of[i] {
                                    durable += 1;
                                }
                                // else: neither — genuine damage, caught below.
                            }
                            // `durable` is NOT capped here, and that is the
                            // point: this block is SEALED, and the seal is
                            // fsync'd only after the apply returned, so every
                            // per-block-durable substrate the block changed
                            // committed durably before the seal existed. the
                            // count is just how many disk modules the block
                            // touched — an agent-run settle touches several —
                            // and the at-pre set is exactly the checkpoint's
                            // rolled-back cohort. the crash-between-commits
                            // window lives strictly above the last seal, where
                            // `trailing.rs` bounds it. (see the crate docblock.)
                            //
                            // a changed module at NEITHER pre nor durable is
                            // genuine damage — still fail-stop. so is a torn
                            // block with nothing left to re-commit (it should
                            // have been caught by the all-durable fast path).
                            if commit_only.len() + durable != changed.len()
                                || commit_only.is_empty()
                            {
                                return Err(Error::Torn(format!(
                                    "block {height}: touched modules are neither all-applied \
                                     nor all-unapplied and cannot be reconciled by selective \
                                     replay — wipe app state and re-sync (keep the consensus \
                                     journal)"
                                )));
                            }
                            let (_, dispatches) = apply_block_committing(
                                host,
                                height,
                                &frame,
                                prepared,
                                witness,
                                Some(disposition),
                                &commit_only,
                                code_source.as_ref(),
                                &mut host::NoWitness,
                            )
                            .await?;
                            if let Some(sink) = sink.as_mut() {
                                // the dispatch trace is the full deterministic
                                // re-execution; only the COMMIT scope was
                                // selective.
                                sink.folded_block(&FoldedBlock {
                                    host,
                                    height,
                                    frame: &frame,
                                    disposition,
                                    root_hash,
                                    dispatches: &dispatches,
                                    roots: &host.module_roots(),
                                });
                            }
                            // every re-committed and exact-post module must now
                            // stand at its sealed post-root. a disk substrate that
                            // raced STRICTLY PAST this block sits at a later
                            // post-root (a stale intermediate here), so skip its
                            // per-block verify — it is verified at its own floor
                            // height, and the final root-hash recompose backstops it
                            // against the sealed tip.
                            for (i, (id, root)) in changed.iter().enumerate() {
                                if ahead_of[i] {
                                    continue;
                                }
                                let live = host.module_root(id);
                                if live != Some(*root) {
                                    return Err(Error::Verify(format!(
                                        "torn-block replay {height} left module {id} at \
                                         {live:?}, sealed root was {root:?}"
                                    )));
                                }
                            }
                            applied += 1;
                        }
                    }
                    blocks.push((height, roots.clone()));
                    applied_frames.push((height, node::frame_id(&frame)));
                    for (id, root) in roots {
                        expected.insert(id, root);
                    }
                    tip_height = Some(height);
                    tip_hash = root_hash;
                }
            }
        }

        // a trailing unsealed block: the crash hit between its WAL record and
        // its seal. roll it forward (or recognize it as already applied) and
        // seal it NOW from the observed outcome.
        let mut rolled_forward = false;
        if let Some(block) = pending {
            let height = block.height;
            let source = block.source.clone();
            // a block whose schedule never closed never began applying (the
            // WAL write returns before the apply starts), so its work is
            // read afresh from the state every module still stands at;
            // a closed schedule is replayed verbatim, with the decisions its
            // witness recorded when it got that far.
            let (frame, prepared, witness) = match block.trailing() {
                Trailing::Scheduled {
                    frame,
                    prepared,
                    witness,
                } => (frame, prepared, witness),
                Trailing::Unscheduled { frame } => {
                    let prepared = host.prepare_work(height).await.map_err(|e| {
                        Error::Torn(format!(
                            "trailing block {height}: preparing its work afresh: {e}"
                        ))
                    })?;
                    (frame, prepared, None)
                }
            };
            let moved: BTreeSet<ModuleId> = host
                .module_roots()
                .iter()
                .filter(|(id, root)| expected.get(id) != Some(root))
                .map(|(id, _)| id.clone())
                .collect();
            if witness.is_none() {
                if !moved.is_empty() {
                    return Err(Error::Torn(format!(
                        "trailing block {height}: state moved without a durable witness"
                    )));
                }
                let position = last_block_position.ok_or_else(|| {
                    Error::Corrupt("trailing block has no journal position".into())
                })?;
                let journal = self.journal.take().expect(LOST);
                self.journal = Some(journal.rewind(position).await.map_err(storage_err)?);
                self.write_pre_apply(height, &frame, &prepared, source.clone())
                    .await
                    .map_err(|e| Error::Storage(e.to_string()))?;
            }
            let mut no_witness = host::NoWitness;
            let hook: &mut dyn host::CommitWitness = match &witness {
                Some(_) => &mut no_witness,
                None => self,
            };
            let (disposition, dispatches) = if moved.is_empty() {
                let (disposition, dispatches) = apply_block(
                    host,
                    height,
                    &frame,
                    prepared,
                    witness,
                    None,
                    code_source.as_ref(),
                    hook,
                )
                .await?;
                (disposition, dispatches)
            } else {
                // classify for its FAIL-CLOSED rules (an unexplained mover
                // alongside a verified claimant, or a >1-substrate claim, is
                // damage). Every accepted case then RE-DERIVES: sealing the
                // observed roots is exact only if every effect of the block is visible in the moved roots
                // — unknowable here, because a member's dispatch follow-ups
                // fan into modules the frame never names, and any in-memory
                // write among them died with the process. sealing observed
                // MIXED roots commits a state no validator ever held and
                // every later fold diverges (observed: a replica restart's
                // trailing [capability + chat] batch); re-derivation is a
                // deterministic no-op when nothing was lost, and the
                // reconstruction when something was.
                trailing::classify_trailing(height, &moved, &trailing_claims)?;
                {
                    // re-execute the durable WAL frame committing ONLY the
                    // still-at-pre cohort — reconstructing the writes the
                    // block fanned out to the in-memory cohort (lost with
                    // RAM) — and aborting every already-moved module, whose
                    // re-commit would move its op-log root and fork us.
                    let commit_only: BTreeSet<ModuleId> = host
                        .module_roots()
                        .into_iter()
                        .map(|(id, _)| id)
                        .filter(|id| !moved.contains(id))
                        .collect();
                    let (_relanded, dispatches) = apply_block_committing(
                        host,
                        height,
                        &frame,
                        prepared,
                        witness,
                        None,
                        &commit_only,
                        code_source.as_ref(),
                        hook,
                    )
                    .await?;
                    // the re-execution's own disposition is NOT a backstop:
                    // members whose effects the movers already hold re-land
                    // as deterministic duplicate-rejects (a solo block whose
                    // only member did so re-lands whole-block Rejected) —
                    // evidence FOR the frame explaining the state. the moved
                    // roots are the durable proof the block APPLIED live, so
                    // that is what the seal records.
                    let disposition = Disposition::Applied;
                    // the one genuine damage detector: a mover the frame
                    // explains is either DISPATCHED by the re-execution or
                    // directly TARGETED by a member; anything else durably
                    // moved without a frame that could have moved it.
                    let targets = frame_targets(&frame);
                    for id in &moved {
                        if !dispatches.iter().any(|d| d.module == *id) && !targets.contains(id) {
                            return Err(Error::Torn(format!(
                                "trailing block {height} neither dispatched nor targeted \
                                 module {id}, which durably committed it — the observed \
                                 state cannot come from this frame. wipe app state and \
                                 re-sync (keep the consensus journal)"
                            )));
                        }
                    }
                    (disposition, dispatches)
                }
            };
            let seal = BlockSeal {
                height,
                disposition,
                roots: host.module_roots(),
                root_hash: host.root_hash(),
            };
            source.verify(&seal)?;
            BlockSink::seal(self, &seal)
                .await
                .map_err(|e| Error::Storage(e.to_string()))?;
            self.sync_journal().await?;
            if let Some(sink) = sink.as_mut() {
                sink.folded_block(&FoldedBlock {
                    host,
                    height,
                    frame: &frame,
                    disposition,
                    root_hash: seal.root_hash,
                    dispatches: &dispatches,
                    roots: &seal.roots,
                });
            }
            blocks.push((height, seal.roots.clone()));
            applied_frames.push((height, node::frame_id(&frame)));
            tip_height = Some(height);
            tip_hash = host.root_hash();
            rolled_forward = true;
        }

        if let Some(h) = tip_height {
            // reconcile running module code to the committed registry at the
            // tip, so a checkpoint AT (or past) a swap boundary — no replayed
            // frame to realize through — still boots on the committed code
            // instead of serving stale genesis components until the next live
            // block. idempotent; fail-closed on missing/tampered bytes.
            host.realize_module_swaps(h, code_source.as_ref())
                .await
                .map_err(|e| {
                    Error::Verify(format!("code-swap realization at recovered tip {h}: {e}"))
                })?;
        }
        // THE verification: the recomposed state must be byte-identical to
        // what consensus sealed at the tip. anything else means the recovered
        // node would fork — refuse to start.
        let live = host.root_hash();
        if live != tip_hash {
            return Err(Error::Verify(format!(
                "recomposed root_hash {live:?} != sealed tip {tip_hash:?} at height {tip_height:?}"
            )));
        }

        // the checkpoint's window and the journal suffix overlap wherever the
        // retained journal reaches back below the checkpoint height (a
        // root-idempotent block is walked and skipped, but still remembered).
        // one entry per height, newest last, bounded to the protocol depth —
        // a duplicate would cost a window slot and shorten this node's reach
        // against a peer's.
        applied_frames.sort_by_key(|(height, _)| *height);
        applied_frames.dedup_by_key(|(height, _)| *height);
        let over = applied_frames
            .len()
            .saturating_sub(node::REPLAY_WINDOW_HEIGHTS);
        applied_frames.drain(..over);

        Ok(Recovered {
            height: tip_height,
            root_hash: tip_hash,
            epoch,
            view_base,
            participants,
            residents,
            frames,
            blocks,
            applied_frames,
            applied,
            skipped,
            rolled_forward,
        })
    }
}

/// re-apply one journaled BATCH frame through the host at its original block
/// coordinate. when `expect` is given, the outcome must reproduce the sealed
/// disposition (the drain is deterministic — anything else is divergence).
/// alongside the disposition, hands back the block's dispatch trace (empty
/// for a rejected block) so a [`ReplaySink`] can fold what the drain would
/// have fed it live.
#[allow(clippy::too_many_arguments)]
async fn apply_block(
    host: &mut Host,
    height: u64,
    frame: &[u8],
    prepared: PreparedWork,
    witness: Option<Witness>,
    expect: Option<Disposition>,
    code_source: &dyn host::CodeSource,
    hook: &mut dyn host::CommitWitness,
) -> Result<(Disposition, Vec<DispatchRecord>), Error> {
    let (disposition, dispatches) = replay_batch(
        host,
        height,
        frame,
        prepared,
        witness,
        None,
        code_source,
        hook,
    )
    .await?;
    if let Some(expect) = expect
        && disposition != expect
    {
        return Err(Error::Verify(format!(
            "replayed block {height} landed as {disposition:?}, sealed as {expect:?}"
        )));
    }
    Ok((disposition, dispatches))
}

/// one WAL block being assembled from its records during replay: the frame,
/// the prepared units that follow it, whether the schedule closed, and the
/// witnessed decisions.
struct PendingBlock {
    height: u64,
    frame: Vec<u8>,
    source: BlockSource,
    prepared: PreparedWork,
    schedule: Schedule,
    /// the reads journaled so far, by unit — the witness being assembled.
    reads: Vec<Observed>,
    witness: Option<Witness>,
}

/// whether a WAL block's prepared list has been closed by its
/// [`Record::Schedule`].
enum Schedule {
    Open,
    Closed,
}

/// what a trailing (unsealed) WAL block can be replayed from.
enum Trailing {
    /// the schedule closed: the block may have begun applying; replay its
    /// journaled work and decisions verbatim.
    Scheduled {
        frame: Vec<u8>,
        prepared: PreparedWork,
        witness: Option<Witness>,
    },
    /// the schedule never closed: the block never began applying; its work
    /// is read afresh.
    Unscheduled { frame: Vec<u8> },
}

impl PendingBlock {
    fn open(height: u64, frame: Vec<u8>, source: BlockSource) -> Self {
        Self {
            height,
            frame,
            source,
            prepared: PreparedWork::default(),
            schedule: Schedule::Open,
            reads: Vec::new(),
            witness: None,
        }
    }

    fn close(
        &mut self,
        calls: u64,
        deliveries: u64,
        advance: Option<sdk::Msg>,
    ) -> Result<(), Error> {
        let counts_match = self.prepared.calls.len() as u64 == calls
            && self.prepared.deliveries.len() as u64 == deliveries;
        if !counts_match {
            return Err(Error::Corrupt(format!(
                "block {}: schedule closes {calls} calls / {deliveries} deliveries but the \
                 journal holds {} / {}",
                self.height,
                self.prepared.calls.len(),
                self.prepared.deliveries.len()
            )));
        }
        if let Schedule::Closed = self.schedule {
            return Err(Error::Corrupt(format!(
                "block {}: schedule closed twice",
                self.height
            )));
        }
        self.prepared.advance = advance;
        self.schedule = Schedule::Closed;
        Ok(())
    }

    /// one journaled read of unit `unit`: units arrive in attempt order, a
    /// unit without reads leaves no record, so the list grows to the unit.
    fn trace(&mut self, unit: u64, trace: Trace) -> Result<(), Error> {
        if let Schedule::Open = self.schedule {
            return Err(Error::Corrupt(format!(
                "block {}: a read before its schedule closed",
                self.height
            )));
        }
        if self.witness.is_some() {
            return Err(Error::Corrupt(format!(
                "block {}: a read after its witness closed",
                self.height
            )));
        }
        let unit = usize::try_from(unit)
            .map_err(|_| Error::Corrupt(format!("block {}: unit {unit}", self.height)))?;
        let out_of_order = unit + 1 < self.reads.len();
        if out_of_order {
            return Err(Error::Corrupt(format!(
                "block {}: unit {unit} read after unit {}",
                self.height,
                self.reads.len() - 1
            )));
        }
        self.reads.resize_with(unit + 1, Observed::default);
        self.reads[unit].trace.push(trace);
        Ok(())
    }

    /// the close of the witness: `units` entries, the trailing ones without
    /// reads.
    fn witnessed(&mut self, units: u64) -> Result<(), Error> {
        if let Schedule::Open = self.schedule {
            return Err(Error::Corrupt(format!(
                "block {}: witness before its schedule closed",
                self.height
            )));
        }
        if self.witness.is_some() {
            return Err(Error::Corrupt(format!(
                "block {}: witness closed twice",
                self.height
            )));
        }
        let units = usize::try_from(units)
            .map_err(|_| Error::Corrupt(format!("block {}: {units} units", self.height)))?;
        if units < self.reads.len() {
            return Err(Error::Corrupt(format!(
                "block {}: witness closes {units} units but the journal holds reads of {}",
                self.height,
                self.reads.len()
            )));
        }
        let mut entries = std::mem::take(&mut self.reads);
        entries.resize_with(units, Observed::default);
        self.witness = Some(Witness { units: entries });
        Ok(())
    }

    /// a SEALED block applied: its schedule closed and its witness ran.
    fn sealed(
        self,
        disposition: Disposition,
    ) -> Result<(Vec<u8>, PreparedWork, Option<Witness>), Error> {
        if let Schedule::Open = self.schedule {
            return Err(Error::Corrupt(format!(
                "block {}: sealed with an open schedule",
                self.height
            )));
        }
        if disposition == Disposition::Rejected {
            return Ok((self.frame, self.prepared, self.witness));
        }
        let Some(witness) = self.witness else {
            return Err(Error::Corrupt(format!(
                "block {}: sealed without its witness",
                self.height
            )));
        };
        Ok((self.frame, self.prepared, Some(witness)))
    }

    fn trailing(self) -> Trailing {
        match self.schedule {
            Schedule::Closed => Trailing::Scheduled {
                frame: self.frame,
                prepared: self.prepared,
                witness: self.witness,
            },
            Schedule::Open => Trailing::Unscheduled { frame: self.frame },
        }
    }
}

/// the open WAL block a follow-on record (`what`) at `height` belongs to.
fn pending_block_mut<'a>(
    pending: &'a mut Option<PendingBlock>,
    height: u64,
    what: &str,
) -> Result<&'a mut PendingBlock, Error> {
    let Some(block) = pending.as_mut() else {
        return Err(Error::Corrupt(format!(
            "{what} at height {height} without its block record"
        )));
    };
    if block.height != height {
        return Err(Error::Corrupt(format!(
            "{what} at height {height} inside block {}",
            block.height
        )));
    }
    Ok(block)
}

/// re-apply one journaled BATCH frame like [`apply_block`], but commit ONLY the
/// modules in `commit_only` at the block boundary and abort the rest (see
/// [`Host::submit_block_committing`]). used to heal a TORN block whose disk
/// substrates are already durable at their sealed post-root: replay re-commits
/// only the in-memory cohort that was rolled back to the checkpoint.
#[allow(clippy::too_many_arguments)]
async fn apply_block_committing(
    host: &mut Host,
    height: u64,
    frame: &[u8],
    prepared: PreparedWork,
    witness: Option<Witness>,
    expect: Option<Disposition>,
    commit_only: &BTreeSet<ModuleId>,
    code_source: &dyn host::CodeSource,
    hook: &mut dyn host::CommitWitness,
) -> Result<(Disposition, Vec<DispatchRecord>), Error> {
    let (disposition, dispatches) = replay_batch(
        host,
        height,
        frame,
        prepared,
        witness,
        Some(commit_only),
        code_source,
        hook,
    )
    .await?;
    if let Some(expect) = expect
        && disposition != expect
    {
        return Err(Error::Verify(format!(
            "torn-block replay {height} landed as {disposition:?}, sealed as {expect:?}"
        )));
    }
    Ok((disposition, dispatches))
}

/// replay one journaled BATCH frame, reproducing the live drain's per-block
/// apply EXACTLY: decode the members, drop the members that fail to decode (the
/// live drain excludes them as deterministic no-ops), and apply the rest as ONE
/// block via the host's batch API. `commit_only` None = commit every touched
/// module (forward replay); `Some` = the torn-block heal (commit the rolled-back
/// in-memory cohort, abort the already-durable disk substrates).
///
/// returns the BLOCK-LEVEL disposition (`Applied` iff the batch MOVED root-hash —
/// the identical rule the live node sealed under, so the caller's `expect` check
/// is a true divergence detector) plus the aggregate dispatch trace (every
/// applied member's dispatches in member order, then the once-per-block System
/// injections) for the [`ReplaySink`] fold.
///
/// the module ids a frame's decodable members DIRECTLY target — the trailing
/// roll-forward's backstop widener: a moved module whose member re-executes
/// as a duplicate-reject on its own post-state records no dispatch, but the
/// frame still explains it. undecodable members target nothing
/// (deterministic no-ops live and on replay alike).
fn frame_targets(frame: &[u8]) -> BTreeSet<ModuleId> {
    let Ok(members) = decode_batch(frame) else {
        return BTreeSet::new();
    };
    members
        .iter()
        .filter_map(|m| node::decode_frame(m).ok())
        .map(|(_, msg)| msg.target)
        .collect()
}

/// `ctx.origin` is unused on the batch path: each member carries its own
/// origin, which the host stamps into that member's `Env`. `prepared` is the
/// block's JOURNALED internal work and `witness` the call decisions its
/// witness recorded — never re-read from the sources, which may already
/// stand past this block.
#[allow(clippy::too_many_arguments)]
async fn replay_batch(
    host: &mut Host,
    height: u64,
    frame: &[u8],
    prepared: PreparedWork,
    witness: Option<Witness>,
    commit_only: Option<&BTreeSet<ModuleId>>,
    code_source: &dyn host::CodeSource,
    hook: &mut dyn host::CommitWitness,
) -> Result<(Disposition, Vec<DispatchRecord>), Error> {
    // CODE-SWAP REALIZATION, mirroring the live drain: a block sealed after a
    // code-registry swap executed on the NEW component, so replay must swap
    // before re-applying or the sealed roots cannot reproduce. the registry
    // is disk-durable and reopens AHEAD of this window — its tip says nothing
    // about which code sealed `height` — so realization keys on the registry's
    // activation HISTORY at `height` (`modules::code_at`): the identical
    // swap points the live node realized, walked in either direction.
    // fail-closed on missing/tampered bytes.
    host.realize_module_swaps(height, code_source)
        .await
        .map_err(|e| Error::Verify(format!("code-swap realization at height {height}: {e}")))?;
    // a batch that never decoded was a whole-block deterministic no-op at runtime
    // too — the live drain sealed it Rejected without touching state.
    let Ok(members) = decode_batch(frame) else {
        return Ok((Disposition::Rejected, Vec::new()));
    };
    // decode exactly as the live drain does, or the replayed root-hash forks.
    let mut ops = Vec::new();
    for member in &members {
        if let Ok(op) = node::decode_member(member) {
            ops.push(op);
        }
    }
    let ctx = BlockContext {
        height,
        consensus_time: height,
        origin: sdk::Origin::System,
    };
    let result = match commit_only {
        None => {
            host.submit_block_replaying(ctx, ops, prepared, witness, hook)
                .await
        }
        Some(set) => {
            host.submit_block_committing(ctx, ops, prepared, witness, set, hook)
                .await
        }
    };
    let outcome = match result {
        Ok(outcome) => outcome,
        // the once-per-block System injection (`Advance`) rejecting is a
        // deterministic no-op — the live drain sealed the block Rejected. (a
        // MEMBER rejection never errors the batch; submit_block folds it into
        // its MemberOutcome.)
        Err(SubmitError::Rejected(_)) => return Ok((Disposition::Rejected, Vec::new())),
        Err(SubmitError::Fatal(f)) => {
            return Err(Error::Torn(format!("boundary fault during replay: {f}")));
        }
    };
    // block-level disposition, DRAIN-based to match the live seal (node's
    // `drain_delivered`): Applied iff the block ran real work — any member
    // applied, or a once-per-block System injection
    // dispatched. NEVER root-hash-based: a torn-heal (`commit_only = Some`)
    // commits only the rolled-back cohort and ABORTS the already-durable mover,
    // so the root-hash cannot move even though the block WAS applied — root-hash
    // movement would spuriously read Rejected and trip the disk-cursor backstop.
    let (ran, dispatches) = outcome.into_trace();
    let disposition = if ran {
        Disposition::Applied
    } else {
        Disposition::Rejected
    };
    Ok((disposition, dispatches))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(pairs: &[(&str, u8)]) -> Vec<(ModuleId, StateRoot)> {
        pairs
            .iter()
            .map(|(id, fill)| (id.to_string(), StateRoot([*fill; 32])))
            .collect()
    }

    #[test]
    fn record_roundtrip() {
        let records = vec![
            Record::Pinned {
                frame: b"frame-bytes".to_vec(),
            },
            Record::Block {
                height: 7,
                frame: vec![0, 1, 2],
                source: BlockSource::Consensus,
            },
            Record::Block {
                height: 8,
                frame: vec![3, 4, 5],
                source: BlockSource::Catchup {
                    disposition: Disposition::Applied,
                    roots: roots(&[("directory", 3), ("kv", 9)]),
                    root_hash: StateRoot([5; 32]),
                },
            },
            Record::Seal {
                height: 7,
                disposition: Disposition::Applied,
                roots: roots(&[("directory", 3), ("kv", 9)]),
                root_hash: StateRoot([5; 32]),
            },
            Record::Seal {
                height: 8,
                disposition: Disposition::Rejected,
                roots: vec![],
                root_hash: StateRoot([6; 32]),
            },
            Record::Cutover {
                epoch: 2,
                view_base: 40,
                participants: vec![vec![7u8; 32], vec![8u8; 32]],
                residents: vec![vec![9u8; 32]],
            },
            Record::PreparedCall {
                height: 9,
                call: PreparedCall {
                    enqueued: 3,
                    id: sdk::CallId {
                        requester: "probe".into(),
                        invocation: "run-1".into(),
                        step: 0,
                    },
                    account: 7,
                    generation: 2,
                    target: "kv".into(),
                    payload: b"write".to_vec(),
                    cause: sdk::Cause::Direct,
                },
            },
            Record::PreparedDelivery {
                height: 9,
                delivery: PreparedDelivery {
                    item: sdk::ItemRef {
                        source: "dispatch".into(),
                        item: 4,
                    },
                    target: "probe".into(),
                    payload: b"result".to_vec(),
                    cause: sdk::Cause::Chain {
                        root: sdk::Root::Item(sdk::ItemRef {
                            source: "dispatch".into(),
                            item: 4,
                        }),
                        hop: sdk::Hop::Delivery(sdk::ItemRef {
                            source: "dispatch".into(),
                            item: 4,
                        }),
                    },
                },
            },
            Record::Schedule {
                height: 9,
                calls: 1,
                deliveries: 1,
                advance: Some(sdk::Msg {
                    target: "modules".into(),
                    payload: modules::encode_msg(&modules::ModulesMsg::Advance),
                }),
            },
            Record::Trace {
                height: 9,
                unit: 2,
                trace: Trace::Read(host::Read::Query {
                    module: "identity".into(),
                    request: b"get 7".to_vec(),
                    answer: Ok(b"program".to_vec()),
                }),
            },
            Record::Trace {
                height: 9,
                unit: 3,
                trace: Trace::Read(host::Read::Root {
                    module: "kv".into(),
                    root: Some(StateRoot([4; 32])),
                }),
            },
            Record::Trace {
                height: 9,
                unit: 3,
                trace: Trace::Read(host::Read::Query {
                    module: "kv".into(),
                    request: b"missing".to_vec(),
                    answer: Err(sdk::Error::QueryUnsupported),
                }),
            },
            Record::Witnessed {
                height: 9,
                units: 5,
            },
        ];
        for r in records {
            let decoded = Record::decode(&r.encode()).expect("roundtrip");
            assert_eq!(decoded, r);
        }
    }

    #[test]
    fn record_rejects_damage() {
        let good = Record::Block {
            height: 7,
            frame: vec![0, 1, 2],
            source: BlockSource::Consensus,
        }
        .encode();
        // truncation
        assert!(Record::decode(&good[..good.len() - 1]).is_err());
        // trailing garbage
        let mut long = good.clone();
        long.push(0);
        assert!(Record::decode(&long).is_err());
        // unknown tag
        let mut bad = good;
        bad[0] = 99;
        assert!(Record::decode(&bad).is_err());
        // a pre-resident-tier cutover record (resident tail absent — the empty
        // set's 8-byte count dropped) fails loud rather than defaulting.
        let cutover = Record::Cutover {
            epoch: 2,
            view_base: 40,
            participants: vec![vec![7u8; 32]],
            residents: vec![],
        }
        .encode();
        assert!(Record::decode(&cutover[..cutover.len() - 8]).is_err());
    }

    #[test]
    fn record_keeps_the_operation_field_cap() {
        let encoded = Record::Block {
            height: 7,
            frame: vec![0; MAX_RECORD_FIELD_LEN + 1],
            source: BlockSource::Consensus,
        }
        .encode();
        assert!(
            Record::decode(&encoded)
                .unwrap_err()
                .to_string()
                .contains("field cap")
        );
    }

    struct DegradedModule(&'static str);

    #[async_trait::async_trait(?Send)]
    impl sdk::Module for DegradedModule {
        fn id(&self) -> ModuleId {
            self.0.into()
        }

        fn root(&self) -> StateRoot {
            StateRoot([5; 32])
        }

        fn state_sync_handle(&self) -> Result<sdk::StateSyncHandle, sdk::Error> {
            Err(sdk::Error::module(
                "no_pack_for_head",
                "no pack for committed head",
            ))
        }

        async fn execute(
            &mut self,
            _ctx: &mut dyn sdk::Ctx,
            _msg: &sdk::Msg,
        ) -> Result<(), sdk::Error> {
            Ok(())
        }
    }

    #[test]
    fn checkpoint_capture_refuses_a_manifest_that_cannot_restore() {
        // the host no longer aborts on a module failure, so the ALL-OR-NOTHING
        // rule now lives here, where it belongs: restore reads snapshot bytes
        // back per module, and writing a manifest without them would prune the
        // journal below a floor nothing can recover from.
        let host = Host::genesis(vec![
            Box::new(DegradedModule("forge")),
            Box::new(DegradedModule("chat")),
        ])
        .expect("genesis");

        let err = Manifest::capture(&host, Some(9), 0, 0, vec![], vec![], None, 0, 1)
            .expect_err("a checkpoint missing a module's bytes must not be written");

        let msg = err.to_string();
        assert!(msg.contains("forge"), "{msg}");
        assert!(
            msg.contains("chat"),
            "every degraded module is named, not just the first: {msg}",
        );
        assert!(msg.contains("no pack for committed head"), "{msg}");
    }

    /// the disk cohort in miniature: per-block durable on its own substrate,
    /// yet shipping ONE self-contained container as its sync surface — forge's
    /// shape exactly (#1308), and the one where the two answers differ.
    struct SelfDurableModule {
        id: &'static str,
        /// counts every `state_sync_handle` call: for forge each one is a full
        /// git pack closure, built on the node's select loop.
        payloads_built: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait(?Send)]
    impl sdk::Module for SelfDurableModule {
        fn id(&self) -> ModuleId {
            self.id.into()
        }

        fn root(&self) -> StateRoot {
            StateRoot([11; 32])
        }

        fn block_durable(&self) -> bool {
            true
        }

        fn state_sync_handle(&self) -> Result<sdk::StateSyncHandle, sdk::Error> {
            self.payloads_built
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(sdk::StateSyncHandle::SnapshotBytes(vec![0xAB; 4096]))
        }

        async fn execute(
            &mut self,
            _ctx: &mut dyn sdk::Ctx,
            _msg: &sdk::Msg,
        ) -> Result<(), sdk::Error> {
            Ok(())
        }
    }

    #[test]
    fn a_checkpoint_never_builds_or_stores_a_self_durable_module_payload() {
        let built = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let host = Host::genesis(vec![
            Box::new(SelfDurableModule {
                id: "forge",
                payloads_built: built.clone(),
            }),
            Box::new(CountingModule {
                id: "runs",
                roots: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
        ])
        .expect("genesis");

        let m = Manifest::capture(&host, Some(9), 0, 0, vec![], vec![], None, 0, 1)
            .expect("the disk cohort is not degraded — it was never asked");

        assert_eq!(
            built.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a checkpoint must not build the container it throws away: that \
             build is forge's whole pack closure, on the select loop",
        );
        assert_eq!(
            m.snapshot("forge"),
            None,
            "restore reopens the disk cohort from its own substrate and \
             installs nothing, so its bytes must never ride the manifest",
        );
        assert_eq!(
            m.root("forge"),
            Some(StateRoot([11; 32])),
            "the ROOT still rides it — the position model compares against it",
        );
        assert_eq!(
            m.snapshot("runs"),
            Some([7].as_ref()),
            "the in-memory cohort is still captured whole",
        );

        // THE READER ON THIS BRANCH READS WHAT THIS BRANCH WRITES.
        assert!(m.check_field_caps().is_ok(), "writable");
        assert_eq!(Manifest::decode(&m.encode()).expect("roundtrip"), m);
    }

    /// a module that COUNTS its own `root()` calls. for a map-backed module
    /// `root()` is a full state serialization + SHA-256, so every extra call is
    /// another whole pass over the module's state — and the capture is holding
    /// the node's select loop while it happens (#1018).
    struct CountingModule {
        id: &'static str,
        roots: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait(?Send)]
    impl sdk::Module for CountingModule {
        fn id(&self) -> ModuleId {
            self.id.into()
        }

        fn root(&self) -> StateRoot {
            self.roots
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            StateRoot([7; 32])
        }

        fn state_sync_handle(&self) -> Result<sdk::StateSyncHandle, sdk::Error> {
            Ok(sdk::StateSyncHandle::SnapshotBytes(vec![7]))
        }

        async fn execute(
            &mut self,
            _ctx: &mut dyn sdk::Ctx,
            _msg: &sdk::Msg,
        ) -> Result<(), sdk::Error> {
            Ok(())
        }
    }

    #[test]
    fn one_capture_computes_each_module_root_exactly_once() {
        let forge = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let chat = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let host = Host::genesis(vec![
            Box::new(CountingModule {
                id: "forge",
                roots: forge.clone(),
            }),
            Box::new(CountingModule {
                id: "chat",
                roots: chat.clone(),
            }),
        ])
        .expect("genesis");

        let count =
            |c: &std::sync::atomic::AtomicUsize| c.load(std::sync::atomic::Ordering::Relaxed);
        let (forge_before, chat_before) = (count(&forge), count(&chat));
        Manifest::capture(&host, Some(9), 0, 0, vec![], vec![], None, 0, 1).expect("capture");

        // the capture used to compute each root FOUR times: twice in this
        // function (the `root_hash` argument and the manifest field) and twice
        // inside the host (its own verification recompute plus the per-module
        // loop). the composite root is now derived from the one pass.
        assert_eq!(
            count(&forge) - forge_before,
            1,
            "forge root recomputed by one capture"
        );
        assert_eq!(
            count(&chat) - chat_before,
            1,
            "chat root recomputed by one capture"
        );
    }

    fn sample_manifest() -> Manifest {
        Manifest {
            codes: vec![("registry".into(), vec![7; 32])],
            height: Some(42),
            epoch: 1,
            view_base: 30,
            participants: vec![vec![7u8; 32], vec![8u8; 32]],
            residents: vec![vec![9u8; 32]],
            pending_cutover_view: Some(15),
            root_hash: StateRoot([1; 32]),
            roots: roots(&[("directory", 2), ("valset", 3)]),
            snapshots: vec![
                ("directory".into(), b"dir-bytes".to_vec()),
                ("valset".into(), vec![]),
            ],
            oplog_pos: 17,
            next_seq: 5,
            applied_frames: vec![(40, [0xA1; 32]), (41, [0xA2; 32]), (42, [0xA3; 32])],
        }
    }

    #[test]
    fn checkpoint_writer_refuses_invalid_deployment_hash_width() {
        let mut manifest = sample_manifest();
        manifest.codes[0].1.pop();
        assert!(manifest.check_field_caps().is_err());
        assert!(Manifest::decode(&manifest.encode()).is_err());
    }

    #[test]
    fn manifest_roundtrip() {
        let m = sample_manifest();
        let decoded = Manifest::decode(&m.encode()).expect("roundtrip");
        assert_eq!(decoded, m);
        assert_eq!(decoded.snapshot("directory"), Some(b"dir-bytes".as_ref()));
        assert_eq!(decoded.root("valset"), Some(StateRoot([3; 32])));
    }

    #[test]
    fn manifest_roundtrips_a_module_snapshot_above_the_operation_cap() {
        let snapshot = vec![0xA5; MAX_RECORD_FIELD_LEN + 1];
        let mut manifest = sample_manifest();
        manifest.snapshots = vec![("forge".into(), snapshot.clone())];

        let decoded = Manifest::decode(&manifest.encode()).expect("large checkpoint decodes");
        assert_eq!(decoded.snapshot("forge"), Some(snapshot.as_slice()));
        assert_eq!(decoded, manifest);
    }

    /// THE WRITER MUST NOT PRODUCE WHAT THE READER REFUSES. A forge snapshot
    /// grows with the repo, and past the field cap the old code wrote a
    /// checkpoint the next boot could never decode — while pruning the journal
    /// below it (#1308).
    #[test]
    fn a_field_over_the_checkpoint_cap_is_refused_at_write_time() {
        let mut manifest = sample_manifest();
        assert!(
            manifest.check_field_caps().is_ok(),
            "the sample is writable"
        );

        // zero-filled: only the length is read, so this never touches a page.
        manifest.snapshots = vec![("forge".into(), vec![0u8; MAX_CHECKPOINT_FIELD_LEN + 1])];
        let refused = manifest
            .check_field_caps()
            .expect_err("an over-cap snapshot must be refused");
        assert!(
            matches!(&refused, Error::FieldOverCap(what) if what.contains("forge")),
            "the refusal must name the module whose snapshot outgrew the cap: {refused}"
        );
        // (deliberately NOT encode+decode'd here: proving the reader refuses it
        // would copy a gigabyte through this test for a bound
        // `manifest_roundtrips_a_module_snapshot_above_the_operation_cap`
        // already pins from the other side.)

        let over_cap_key = Manifest {
            snapshots: Vec::new(),
            residents: vec![vec![0u8; MAX_CHECKPOINT_FIELD_LEN + 1]],
            ..sample_manifest()
        };
        assert!(matches!(
            over_cap_key.check_field_caps(),
            Err(Error::FieldOverCap(_))
        ));

        // the same rule for the list COUNTS the reader caps: an over-long list
        // decodes no better than an over-long field.
        let too_many_roots = Manifest {
            roots: (0..=MAX_LIST_LEN)
                .map(|i| (format!("m{i}"), StateRoot([0; 32])))
                .collect(),
            ..sample_manifest()
        };
        assert!(matches!(
            too_many_roots.check_field_caps(),
            Err(Error::FieldOverCap(_))
        ));
        assert!(matches!(
            Manifest::decode(&too_many_roots.encode()),
            Err(Error::Corrupt(_)),
        ));
    }

    #[test]
    fn manifest_decode_rejects_truncated_tail() {
        // A checkpoint missing or tearing its resident-set tail fails loud.
        let m = Manifest {
            residents: vec![],
            ..sample_manifest()
        };
        let full = m.encode();
        // The empty resident set is one 8-byte count.
        assert!(Manifest::decode(&full[..full.len() - 8]).is_err());
        assert!(Manifest::decode(&full[..full.len() - 4]).is_err());
        // A torn resident key fails loud too.
        let full = sample_manifest().encode();
        let torn = &full[..full.len() - 4];
        assert!(Manifest::decode(torn).is_err());
    }

    #[test]
    fn floor_cert_roundtrip() {
        let c = FloorCert {
            epoch: 3,
            height: 99,
            cert: b"certificate".to_vec(),
        };
        assert_eq!(FloorCert::decode(&c.encode()).expect("roundtrip"), c);
    }

    #[test]
    fn catchup_expectation_survives_interrupted_schedule_and_witness_rewrite() {
        use commonware_cryptography::Signer as _;
        use commonware_runtime::{Runner as _, Supervisor as _};

        for schedule in [Schedule::Open, Schedule::Closed] {
            commonware_runtime::deterministic::Runner::default().start(|context| async move {
                let fresh = || {
                    Host::genesis(vec![Box::new(directory::Directory::new("directory"))]).unwrap()
                };
                let host = fresh();
                let base =
                    Manifest::capture(&host, None, 0, 0, vec![], vec![], None, 0, 1).unwrap();
                let signer = commonware_cryptography::ed25519::PrivateKey::from_seed(79);
                let member = node::encode_frame(
                    &signer,
                    0,
                    &sdk::Msg {
                        target: "directory".into(),
                        payload: directory::encode_msg(&directory::DirMsg::Set {
                            key: "k".into(),
                            value: "v".into(),
                        }),
                    },
                );
                let frame = node::encode_batch(&[member]);
                let expected = BlockSeal {
                    height: 1,
                    disposition: Disposition::Applied,
                    roots: host.module_roots(),
                    root_hash: StateRoot([0xA5; 32]),
                };
                let source = BlockSource::Catchup {
                    disposition: expected.disposition,
                    roots: expected.roots.clone(),
                    root_hash: expected.root_hash,
                };
                let mut recovery = Recovery::open(context.child("before_crash")).await.unwrap();
                recovery.write_manifest(&base).await.unwrap();
                match schedule {
                    Schedule::Open => {
                        let record = Record::Block {
                            height: 1,
                            frame,
                            source: source.clone(),
                        };
                        recovery.append_record(&record.encode()).await.unwrap();
                        recovery.sync().await.unwrap();
                    }
                    Schedule::Closed => {
                        recovery
                            .pre_apply_catchup(&frame, &PreparedWork::default(), &expected)
                            .await
                            .unwrap();
                    }
                }
                drop(recovery);
                for label in ["first_restart", "second_restart"] {
                    let mut recovery = Recovery::open(context.child(label)).await.unwrap();
                    let mut host = fresh();
                    let error = recovery.recover(&mut host, &base).await.unwrap_err();
                    assert!(matches!(error, Error::Verify(_)), "{error}");
                    assert!(error.to_string().contains("served seal"), "{error}");
                    let (records, _) = recovery.records().await.unwrap();
                    assert!(records.iter().any(|(_, record)| matches!(record,
                        Record::Block { source: retained, .. } if retained == &source)));
                    assert!(
                        !records
                            .iter()
                            .any(|(_, record)| matches!(record, Record::Seal { .. })),
                        "neither restart may bless the mismatched suffix"
                    );
                }
            });
        }
    }

    /// #1814: `read_finalized_frames` must SEEK to the requested range via
    /// `height_index`, not decode the retained journal from its start. a
    /// journal of 200 blocks (400 records) asked for its last 3 frames
    /// should decode a small handful of records, not anything close to 400.
    #[test]
    fn read_finalized_frames_seeks_instead_of_scanning_the_whole_journal() {
        use commonware_runtime::{Runner as _, Supervisor as _};

        let executor = commonware_runtime::deterministic::Runner::default();
        executor.start(|context| async move {
            let mut recovery = Recovery::open(context.child("seek_index"))
                .await
                .expect("open recovery");
            const TOTAL: u64 = 200;
            for height in 1..=TOTAL {
                recovery
                    .pre_apply(height, &[height as u8], &PreparedWork::default())
                    .await
                    .expect("wal record");
                recovery
                    .seal(&BlockSeal {
                        height,
                        disposition: Disposition::Applied,
                        roots: vec![],
                        root_hash: StateRoot([0u8; 32]),
                    })
                    .await
                    .expect("seal");
            }

            TEST_DECODED_RECORDS.store(0, std::sync::atomic::Ordering::Relaxed);
            let tail = recovery
                .read_finalized_frames(TOTAL - 3, TOTAL, 10)
                .await
                .expect("tail read");
            assert_eq!(tail.len(), 3);
            assert_eq!(tail[0].height, TOTAL - 2);
            assert_eq!(tail[2].height, TOTAL);

            // The journal holds three records per height: block, schedule, seal.
            // a scan from the journal's start would decode close to all of
            // them. the height index instead lands the stream within a
            // handful of records of the requested tail.
            let decoded = TEST_DECODED_RECORDS.load(std::sync::atomic::Ordering::Relaxed);
            assert!(
                decoded <= 10,
                "expected a bounded tail read, decoded {decoded} of {} records",
                3 * TOTAL
            );
        });
    }
}
