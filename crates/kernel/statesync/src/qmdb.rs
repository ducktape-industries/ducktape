//! the qmdb half of the state-sync wire: serve a live store's proof-carrying op
//! ranges as opaque bytes, and resolve those bytes back on the joiner —
//! including a [`RemoteQmdbSource`] that plugs straight into commonware's qmdb
//! sync engine as its network [`Source`].
//!
//! every qmdb-backed module in this workspace shares ONE store shape —
//! 32-byte sha256-hashed keys, variable byte values, sha256 merkleization,
//! two-byte translator, sequential strategy — so one wire format serves kv,
//! document, messaging, and the wrappers over them. the wire IS commonware's
//! own sync [`Request`]/[`Response`] codec: the serve side answers with
//! HISTORICAL proofs (anchored at the request's `size`, not the live head), so
//! a source that keeps committing new blocks still serves a joiner's in-flight
//! target consistently until compaction prunes below the target range — at
//! which point the joiner refetches a fresh manifest and retries. trust never
//! comes from the source: the sync engine merkle-verifies every batch against
//! the consensus-committed target root.

use std::num::{NonZeroU16, NonZeroU64, NonZeroUsize};

use commonware_codec::{Decode as _, DecodeExt as _, Encode as _, EncodeSize as _, RangeCfg};
use commonware_cryptography::{Hasher, Sha256};
use commonware_parallel::Sequential;
use commonware_runtime::{Spawner, buffer::paged::CacheRef};
use commonware_storage::{
    Context, journal,
    merkle::{self, Location, MAX_PINNED_NODES, MAX_PROOF_DIGESTS_PER_ELEMENT},
    qmdb::{
        any::{
            VariableConfig,
            unordered::variable::{Db, Operation},
        },
        sync::{
            self, FeedbackTx, Request, Response, Source, SourceFor, Target,
            engine::Config as SyncConfig,
        },
    },
    translator::TwoCap,
};

use crate::wire::WireError;
use crate::{BoundaryId, SyncClient, SyncError, SyncRequest, SyncResponse};

/// the shared digest type: sha256, used as both the hashed key and the proof
/// digest by every qmdb module in the workspace.
pub type SyncDigest = <Sha256 as Hasher>::Digest;

/// the ONE qmdb store shape every module in this workspace uses.
pub type SyncDb<E> = Db<merkle::mmr::Family, E, SyncDigest, Vec<u8>, Sha256, TwoCap, Sequential>;

/// the op type that store's journal carries (what sync batches ship).
pub type SyncOp = Operation<merkle::mmr::Family, SyncDigest, Vec<u8>>;

/// a sync target for that store shape: root + live op range.
pub type SyncTarget = Target<merkle::mmr::Family, SyncDigest>;

/// one request against a store's sync surface, as the engine issues it.
pub type QmdbSyncReq = Request<merkle::mmr::Family>;

/// one authenticated reply, shaped like the request it answers.
pub type QmdbSyncResp = Response<merkle::mmr::Family, SyncOp, SyncDigest>;

/// the codec read-config for [`SyncOp`] — MUST mirror the journal codec config
/// every module's store is built with (fixed-width key => `()`, value bounded
/// at [`sdk::MAX_STORE_VALUE_BYTES`]). a mismatch would reject ops the
/// source's own journal accepted.
fn op_read_cfg() -> ((), (RangeCfg<usize>, ())) {
    ((), (RangeCfg::from(0..=sdk::MAX_STORE_VALUE_BYTES), ()))
}

/// ceiling on ops per fetched batch accepted from a peer — the engine asks for
/// small batches (kv syncs at 64); this only bounds a malicious oversized reply
/// (and, through the codec, the proof digests one may carry).
const MAX_OPS_PER_BATCH: u64 = 4096;

/// ceiling on ONE encoded reply (proof + ops, or proof + op + pinned nodes) a
/// module's `serve_sync` hands back. the reply rides the mesh as a
/// `SyncResponse::Module` body, and the p2p sender ASSERTS on its 2 MiB
/// message cap — an op COUNT cap alone let a batch of adjacent ~1 MiB records
/// (tasks, pages) walk straight into that assert. [`serve`] trims a batch to
/// the largest op prefix that fits; the sync engine resumes by location, so a
/// shorter batch is progress, never an error. sized so one op carrying the
/// largest value the journal codec admits ([`sdk::MAX_STORE_VALUE_BYTES`])
/// plus its proof and pinned nodes at their decode ceilings always fits.
/// bin/node compile-asserts this stays under its `MAX_MESSAGE_SIZE` with
/// rpc-envelope headroom.
pub const MAX_MODULE_REPLY_BYTES: usize = (1 << 21) - (64 << 10);
const _: () = assert!(
    MAX_MODULE_REPLY_BYTES
        >= sdk::MAX_STORE_VALUE_BYTES
            + (MAX_PROOF_DIGESTS_PER_ELEMENT + MAX_PINNED_NODES) * 32
            + 4096
);

// ============================================================================
// the wire: commonware's sync codec, bounded on decode.
// ============================================================================

/// the op-range request the engine issues first and most: `max_ops` ops from
/// `start`, proved against the store as it stood at `op_count` ops.
pub fn ops_request(op_count: u64, start: u64, max_ops: u64) -> QmdbSyncReq {
    Request::Operations {
        size: Location::new(op_count),
        start: Location::new(start),
        max_ops: NonZeroU64::new(max_ops).expect("a request asks for at least one op"),
    }
}

pub fn encode_qmdb_req(req: &QmdbSyncReq) -> Vec<u8> {
    req.encode().to_vec()
}

pub fn decode_qmdb_req(bytes: &[u8]) -> Result<QmdbSyncReq, WireError> {
    QmdbSyncReq::decode(bytes).map_err(|e| WireError::Codec(format!("request: {e}")))
}

pub fn encode_qmdb_resp(resp: &QmdbSyncResp) -> Vec<u8> {
    resp.encode().to_vec()
}

/// decode a reply under the batch ceilings: at most [`MAX_OPS_PER_BATCH`]
/// ops, values at the journal's 1 MiB cap, proofs and pinned nodes at
/// commonware's own per-element bounds. a forged length fails here instead of
/// allocating.
pub fn decode_qmdb_resp(bytes: &[u8]) -> Result<QmdbSyncResp, WireError> {
    QmdbSyncResp::decode_cfg(bytes, &(MAX_OPS_PER_BATCH as usize, op_read_cfg()))
        .map_err(|e| WireError::Codec(format!("response: {e}")))
}

// ============================================================================
// SERVE — answer a decoded request against a live store (module side).
// ============================================================================

/// serve one [`QmdbSyncReq`] from a live store. read-only; historical proofs
/// keep older manifest-pinned targets servable while the store keeps advancing.
/// this is what a qmdb-backed module's [`sdk::Module::serve_sync`] delegates to.
pub async fn serve<E>(db: &SyncDb<E>, req: &QmdbSyncReq) -> Result<Vec<u8>, sdk::Error>
where
    E: Context + Spawner,
{
    let mut req = *req;
    if let Request::Operations { max_ops, .. } = &mut req {
        *max_ops = (*max_ops).min(NonZeroU64::new(MAX_OPS_PER_BATCH).unwrap());
    }
    // the BYTE budget ([`MAX_MODULE_REPLY_BYTES`]): a batch that encodes over
    // it is re-proved over the largest op prefix that fits. the proof is a
    // range proof over exactly the ops served, so a shorter batch is a fresh
    // proof, never a sliced reply. the prefix is strictly shorter each round,
    // so this settles in a couple of rounds at most.
    loop {
        let (resp, _feedback) = db
            .serve(req)
            .await
            .map_err(|e| sdk::Error::module("qmdb_serve", format!("serve failed: {e}")))?;
        let encoded = encode_qmdb_resp(&resp);
        let fits_budget = encoded.len() <= MAX_MODULE_REPLY_BYTES;
        if fits_budget {
            return Ok(encoded);
        }
        let Response::Operations { operations, .. } = &resp else {
            return Err(sdk::Error::module(
                "reply_budget",
                format!("one op exceeds the {MAX_MODULE_REPLY_BYTES}-byte module reply budget"),
            ));
        };
        let Some(shorter) = NonZeroU64::new(fitting_op_prefix(operations, encoded.len())) else {
            return Err(sdk::Error::module(
                "reply_budget",
                format!("one op exceeds the {MAX_MODULE_REPLY_BYTES}-byte module reply budget"),
            ));
        };
        let Request::Operations { max_ops, .. } = &mut req else {
            unreachable!("an Operations reply answers an Operations request");
        };
        debug_assert!(
            shorter < *max_ops,
            "an over-budget batch shrinks every round"
        );
        *max_ops = shorter;
    }
}

/// how many leading ops of a reply fit [`MAX_MODULE_REPLY_BYTES`] once the
/// reply's non-op bytes (tag, proof, length prefix) are taken out.
/// `encoded_len` is the whole reply's encoded length. zero means the FIRST op
/// alone does not fit (a codec-ceiling value under a ceiling-sized proof —
/// precluded by the const assert on the budget, so an error, never a spin).
fn fitting_op_prefix(operations: &[SyncOp], encoded_len: usize) -> u64 {
    let op_lens: Vec<usize> = operations.iter().map(|op| op.encode_size()).collect();
    let overhead = encoded_len - op_lens.iter().sum::<usize>();
    let mut budget = MAX_MODULE_REPLY_BYTES.saturating_sub(overhead);
    let mut fitting = 0u64;
    for len in op_lens {
        if len > budget {
            break;
        }
        budget -= len;
        fitting += 1;
    }
    fitting
}

/// describe the store's current sync target for manifest capture. this is not a
/// wire request; callers pin the returned target into the manifest before a
/// joiner starts fetching operation ranges.
pub fn resolver_sync_target<E>(db: &SyncDb<E>) -> Result<sdk::ResolverSyncTarget, sdk::Error>
where
    E: Context + Spawner,
{
    let end = db.bounds().end;
    let start = db.sync_boundary();
    let range = commonware_utils::range::NonEmptyRange::new(start..end).map_err(|_| {
        sdk::Error::module(
            "nothing_committed",
            "store has no committed operations to sync",
        )
    })?;
    Ok(sdk::ResolverSyncTarget {
        root: sdk::StateRoot(db.root().0),
        start: range.start().as_u64(),
        op_count: range.end().as_u64(),
    })
}

/// convenience for module `serve_sync` impls: decode + serve in one call.
pub async fn serve_bytes<E>(db: &SyncDb<E>, req: &[u8]) -> Result<Vec<u8>, sdk::Error>
where
    E: Context + Spawner,
{
    let req = decode_qmdb_req(req).map_err(|e| sdk::Error::module("codec", e.to_string()))?;
    serve(db, &req).await
}

pub fn module_lane_error(module_id: &str, error: String) -> SyncError {
    if is_pruned_range_error(&error) {
        SyncError::Pruned {
            module: module_id.to_string(),
            reason: error,
        }
    } else {
        SyncError::Server(error)
    }
}

fn is_pruned_range_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("operation pruned")
        || lower.contains("itempruned")
        || lower.contains("item pruned")
        || lower.contains("historical range pruned")
}

// ============================================================================
// STORE — the host-constructed concrete store, injected into modules as
// `Box<dyn sdk::MerkleStore>`.
// ============================================================================

/// the qmdb configuration for the shared store shape. the key codec cfg is `()`
/// (fixed-width digest); only the variable value carries a [`RangeCfg`].
pub type SyncDbConfig = VariableConfig<TwoCap, ((), (RangeCfg<usize>, ())), Sequential>;

/// build the qmdb [`VariableConfig`] for module `id` on `context`. partitions
/// are namespaced by `id` so several qmdb-backed modules can share one runtime
/// context without colliding on storage. the single source of truth for a
/// module store's on-disk layout — [`QmdbStore::init`] (fresh open) and
/// [`QmdbStore::sync_from`] (state-sync target) both build from it, so a synced
/// store's storage layout is byte-identical to a freshly-opened one. the
/// partition-name format and every constant are load-bearing: this function
/// replaced per-module copies (kv, pages, chat) and an existing store on disk
/// only reopens if none of them drift.
pub fn store_config<E>(context: &E, id: &str) -> SyncDbConfig
where
    E: Context,
{
    // a single page-cache handle shared by both sub-configs (cheap to clone).
    let page_cache = CacheRef::from_pooler(
        context,
        NonZeroU16::new(128).unwrap(),
        NonZeroUsize::new(64).unwrap(),
    );
    let replay_buffer = NonZeroUsize::new(1 << 20).unwrap();

    VariableConfig {
        merkle_config: merkle::full::Config {
            journal_partition: format!("{id}-merkle-journal"),
            metadata_partition: format!("{id}-merkle-meta"),
            items_per_blob: NonZeroU64::new(64).unwrap(),
            write_buffer: NonZeroUsize::new(1024).unwrap(),
            replay_buffer,
            strategy: Sequential,
            page_cache: page_cache.clone(),
        },
        journal_config: journal::contiguous::variable::Config {
            partition: format!("{id}-log"),
            items_per_section: NonZeroU64::new(64).unwrap(),
            write_buffer: NonZeroUsize::new(1024).unwrap(),
            replay_buffer,
            compression: None,
            // the journal codec config IS the wire read-config (fixed-width key
            // => `()`, value bounded at [`sdk::MAX_STORE_VALUE_BYTES`]);
            // reusing [`op_read_cfg`] keeps
            // the two mirrored by construction instead of by comment.
            codec_config: op_read_cfg(),
            page_cache,
        },
        translator: TwoCap,
        init_cache_size: None,
        init_buffer: replay_buffer,
        init_concurrency: (),
    }
}

/// the concrete qmdb-backed [`sdk::MerkleStore`]. the HOST constructs one per
/// module (it owns the runtime context and storage) and injects it as
/// `Box<dyn MerkleStore>`, so module crates stay pure logic over the trait and
/// never depend on commonware-storage/-runtime themselves.
///
/// qmdb mutations consume the database and hand it back only on success, so
/// the store holds it in an `Option`: a failed commit leaves `None`, and every
/// later call reports the store as lost instead of touching half-applied state
/// — recovery is a fresh [`QmdbStore::init`].
pub struct QmdbStore<E>
where
    E: Context + Spawner,
{
    db: Option<SyncDb<E>>,
}

impl<E> QmdbStore<E>
where
    E: Context + Spawner,
{
    /// open (or recover) the store on `context` under module identity `id`.
    /// async because qmdb opens its log and writes an initial commit floor.
    pub async fn init(context: E, id: &str) -> Self {
        let cfg = store_config(&context, id);
        let db = SyncDb::<E>::init(context, cfg)
            .await
            .expect("qmdb init failed");
        Self { db: Some(db) }
    }

    /// reconstruct a store at `id` on `context` whose qmdb root EQUALS
    /// `target.root`, by pulling `target`'s op range from `source`. the sync
    /// engine merkle-verifies every fetched batch against `target.root`, so a
    /// byzantine source cannot produce a store with a matching root but forged
    /// contents — the root is the trust anchor. reuses [`store_config`] so the
    /// synced store's storage layout matches a freshly-opened one.
    pub async fn sync_from<S>(
        context: E,
        id: &str,
        target: SyncTarget,
        source: S,
    ) -> Result<Self, String>
    where
        S: SourceFor<SyncDb<E>>,
    {
        let db_config = store_config(&context, id);
        let config = SyncConfig {
            context,
            source,
            target,
            max_outstanding_requests: 1,
            fetch_batch_size: NonZeroU64::new(64).unwrap(),
            apply_batch_size: NonZeroU64::new(1024).unwrap(),
            db_config,
            update_rx: None,
            finish_rx: None,
            reached_target_tx: None,
            max_retained_roots: 8,
        };
        // a sync failure (transport blip, dropped source) is the caller's
        // retry loop to own — never a process kill.
        let db = sync::sync(config)
            .await
            .map_err(|e| format!("qmdb sync: {e:?}"))?;
        Ok(Self { db: Some(db) })
    }

    fn db(&self) -> Result<&SyncDb<E>, sdk::Error> {
        self.db
            .as_ref()
            .ok_or_else(|| sdk::Error::module("store_lost", "qmdb store lost to a failed commit"))
    }

    /// the engine-native [`SyncTarget`] for this store: its qmdb merkle root
    /// plus the LIVE operation range `[sync_boundary, end)`. hand it to
    /// [`QmdbStore::sync_from`] to rebuild a store with an identical root.
    ///
    /// the range starts at `sync_boundary()`, not `0`: qmdb compacts
    /// overwritten history below its inactivity floor, so only the active tail
    /// ships (pinned merkle nodes cover the pruned prefix). that IS checkpoint
    /// semantics — the snapshot half of snapshot-plus-replay-tail.
    pub async fn sync_boundary_target(&self) -> SyncTarget {
        let db = self.db().expect("a store that lost its qmdb has no target");
        let end = db.bounds().end;
        let start = db.sync_boundary();
        Target {
            root: db.root(),
            range: commonware_utils::range::NonEmptyRange::new(start..end)
                .expect("a committed store has a non-empty op range"),
        }
    }

    /// consume this store into an `Arc`-wrapped raw qmdb that serves as a sync
    /// source: it answers a joiner's requests with proof-carrying batches. a
    /// LIVE source still taking writes would instead wrap
    /// `Arc<AsyncRwLock<..>>`; this consuming form is the handoff / test source.
    pub fn into_resolver(self) -> std::sync::Arc<SyncDb<E>> {
        std::sync::Arc::new(self.db.expect("a store that lost its qmdb cannot serve"))
    }
}

#[async_trait::async_trait(?Send)]
impl<E> sdk::MerkleStore for QmdbStore<E>
where
    E: Context + Spawner,
{
    async fn get(&self, key: &[u8; sdk::ROOT_LEN]) -> Result<Option<Vec<u8>>, sdk::Error> {
        self.db()?
            .get(&SyncDigest::from(*key))
            .await
            .map_err(|e| sdk::Error::module("qmdb_get", format!("qmdb get failed: {e}")))
    }

    /// apply ONE ordered batch: write every hashed key, merkleize, apply,
    /// commit — the exact call sequence the modules issued inline before the
    /// store was injected, and in the caller's given order, so committed roots
    /// stay byte-identical across the cutover.
    async fn commit_batch(
        &mut self,
        writes: Vec<([u8; sdk::ROOT_LEN], Option<Vec<u8>>)>,
    ) -> Result<(), sdk::Error> {
        let db = self.db.take().ok_or_else(|| {
            sdk::Error::module("store_lost", "qmdb store lost to a failed commit")
        })?;
        let mut batch = db.new_batch();
        for (key, value) in writes {
            batch = batch.write(SyncDigest::from(key), value);
        }
        let batch = match batch.merkleize(&db, None::<Vec<u8>>).await {
            Ok(batch) => batch,
            Err(e) => {
                self.db = Some(db);
                return Err(sdk::Error::module(
                    "qmdb_merkleize",
                    format!("merkleize failed: {e}"),
                ));
            }
        };
        let (db, _applied) = db.apply_batch(batch).await.map_err(|e| {
            sdk::Error::module("qmdb_apply_batch", format!("apply_batch failed: {e}"))
        })?;
        let db = db
            .commit()
            .await
            .map_err(|e| sdk::Error::module("qmdb_commit", format!("commit failed: {e}")))?;
        self.db = Some(db);
        Ok(())
    }

    /// the REAL qmdb merkle root over all committed keys — qmdb caches it, so
    /// this is sync and by-value (sha256 digest == 32 bytes == ROOT_LEN).
    fn root(&self) -> sdk::StateRoot {
        let db = self.db().expect("a store that lost its qmdb has no root");
        sdk::StateRoot(db.root().0)
    }

    async fn sync_target(&self) -> Result<sdk::ResolverSyncTarget, sdk::Error> {
        resolver_sync_target(self.db()?)
    }

    async fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, sdk::Error> {
        serve_bytes(self.db()?, req).await
    }
}

// ============================================================================
// RESOLVE — the joiner-side network source for the qmdb sync engine.
// ============================================================================

/// a network-backed [`Source`] for the shared store shape: every request
/// becomes a [`SyncRequest::Module`] round-trip through a [`SyncClient`], and
/// the reply bytes decode back into the proof-carrying [`Response`] the sync
/// engine verifies against its target root. the engine's merkle verification
/// is the trust boundary — a lying server fails verification, never installs.
#[derive(Clone)]
pub struct RemoteQmdbSource<C> {
    client: C,
    boundary: BoundaryId,
    module_id: String,
}

impl<C> RemoteQmdbSource<C> {
    pub fn new(client: C, boundary: BoundaryId, module_id: impl Into<String>) -> Self {
        Self {
            client,
            boundary,
            module_id: module_id.into(),
        }
    }
}

impl<C> Source for RemoteQmdbSource<C>
where
    C: SyncClient,
{
    type Family = merkle::mmr::Family;
    type Digest = SyncDigest;
    type Op = SyncOp;
    type Error = SyncError;

    async fn serve(&self, request: QmdbSyncReq) -> Result<(QmdbSyncResp, FeedbackTx), SyncError> {
        let resp = self
            .client
            .request(SyncRequest::Module {
                boundary: self.boundary,
                module_id: self.module_id.clone(),
                body: encode_qmdb_req(&request),
            })
            .await?;
        let bytes = match resp {
            SyncResponse::Module(bytes) => bytes,
            SyncResponse::Error(e) => return Err(module_lane_error(&self.module_id, e)),
            other => return Err(SyncError::UnexpectedResponse(other.kind_name())),
        };
        Ok((decode_qmdb_resp(&bytes)?, None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
    use sdk::MerkleStore as _;

    /// the byte budget: three records at the tasks module's record ceiling
    /// (~1 MiB each — two already exceed the mesh cap) do not ride one reply.
    /// the server trims the batch to the prefix that fits, and the cursor
    /// walk the sync engine performs still reaches every op.
    #[test]
    fn an_op_batch_is_trimmed_to_the_module_reply_budget_and_resumes_by_cursor() {
        deterministic::Runner::default().start(|context| async move {
            let mut store = QmdbStore::init(context.child("tasks"), "tasks").await;
            let value = vec![0x5A; (1 << 20) - 4096];
            for i in 0u8..3 {
                store
                    .commit_batch(vec![([i; sdk::ROOT_LEN], Some(value.clone()))])
                    .await
                    .expect("commit");
            }
            let db = store.db().expect("live store");
            let target = resolver_sync_target(db).expect("target");

            let mut cursor = target.start;
            let mut served = 0u64;
            let mut pages = 0u32;
            while cursor < target.op_count {
                let req = ops_request(target.op_count, cursor, 64);
                let bytes = serve(db, &req).await.expect("serve");
                assert!(
                    bytes.len() <= MAX_MODULE_REPLY_BYTES,
                    "page {pages} encodes to {} bytes, over the {MAX_MODULE_REPLY_BYTES} budget",
                    bytes.len()
                );
                let Response::Operations { operations, .. } =
                    decode_qmdb_resp(&bytes).expect("reply decodes")
                else {
                    panic!("an ops request is answered with ops");
                };
                assert!(!operations.is_empty(), "a page always makes progress");
                cursor += operations.len() as u64;
                served += operations.len() as u64;
                pages += 1;
            }
            assert_eq!(
                served,
                target.op_count - target.start,
                "every op is served once"
            );
            assert!(
                pages > 1,
                "three ~1 MiB records cannot ride one 2 MiB reply"
            );
        });
    }
}
