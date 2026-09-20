//! the blob mesh lane: the SERVE half answering `SyncRequest::Blob` /
//! `BlobInfo` / `BlobRange` from this node's store, the demux that keeps a
//! peer's frames and our own straight, and the REQUESTER that assembles a
//! large blob from ranged windows.
//!
//! the original single-shot requester (`MeshBlobFetcher`, the #298 prompt
//! lane) was deleted when agent personas became duckfs skills. the requester
//! is BACK for the wasm code plane: consensus commits a module's component
//! (or quack capsule) by 32-byte hash, the bytes travel out-of-band, and a
//! node that lacks the committed bytes at a swap boundary, during replay, or
//! at state-sync catch-up fetches them here — [`fetch_blob`] discovers the
//! length ([`SyncRequest::BlobInfo`]), streams bounded windows
//! ([`SyncRequest::BlobRange`]) into a resumable [`blobstore`] staging slot,
//! and publishes only when the assembled whole re-hashes to the digest, so
//! no trust attaches to which peer answered. misses rotate the source and a
//! failed node stays failed-closed — it can never fork.
//!
//! two request paths share this machinery: joiners/catch-up own a
//! [`statesync::p2p::P2pSyncClient`]; a validator's statesync channel
//! receiver is owned by its serve loop, so it fetches through the
//! [`ServeLaneBlobClient`] co-client — sends ride a clone of the serve
//! lane's sender and responses route back through [`classify_mesh_frame`]'s
//! pending-map demux.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use commonware_cryptography::ed25519;
use commonware_p2p::Sender as P2pSender;
use commonware_runtime::IoBuf;
use statesync::{SyncClient, SyncError, SyncRequest, SyncResponse};

/// the whole-blob serve arm answers only blobs up to this size: the mesh
/// channel is sized for statesync's 256 KiB chunks. an oversized blob answers
/// as an honest miss — large blobs are fetched RANGED instead.
pub const MAX_SERVED_BLOB: usize = 1024 * 1024;

/// one ranged window: request size cap on the client, clamp on the server —
/// the same bound statesync's snapshot chunks ride, so a range frame never
/// outgrows the mesh channel either.
pub const MAX_BLOB_RANGE: u64 = 256 * 1024;

/// the co-client's rpc-id floor. the serve lane multiplexes id spaces (our
/// fetch ids beside peers' request ids); drawing this client's ids at or
/// above a high base keeps the spaces disjoint by construction.
pub const COCLIENT_ID_BASE: u64 = 1 << 62;

/// one fresh co-client rpc id: RANDOM in `[COCLIENT_ID_BASE, 1<<63)`, never
/// sequential. a sequential counter tells any peer that has ever received one
/// of our requests what the next ids will be, which is half of pre-answering
/// a request it was never sent (the other half — the peer check in
/// [`classify_coclient_frame`] — is what actually closes it).
fn fresh_coclient_id() -> u64 {
    COCLIENT_ID_BASE | (rand::random::<u64>() >> 2)
}

/// how long the co-client waits for one response before failing the request
/// and rotating — mirrors the p2p binding's 1–2 reaper sweeps (3–6s).
const COCLIENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

/// one outstanding fetch: the waiter AND the peer we addressed it to. the id
/// alone is not enough to complete it — see [`classify_coclient_frame`].
pub struct PendingFetch {
    /// the one peer whose answer may complete this request.
    pub peer: ed25519::PublicKey,
    pub reply: tokio::sync::oneshot::Sender<statesync::SyncResponse>,
}

/// outstanding fetches keyed by rpc id — the serve loop's demux surface: an
/// incoming mesh frame whose id is in here AND whose sender is the peer we
/// addressed is a response to US, everything else is a peer's request.
pub type PendingMap = Arc<Mutex<HashMap<u64, PendingFetch>>>;

/// what the serve loop must do with an incoming frame, decided from the frame's
/// rpc id, its sender, and the peer the matching request (if any) was addressed
/// to. pure — the map lookup happens outside, so this is unit-testable.
#[derive(Debug, PartialEq, Eq)]
pub enum CoClientVerdict {
    /// no co-client business: hand it to the serve path as a peer's request.
    PeerRequest,
    /// the answer we are waiting for, from the peer we asked. complete it.
    Response,
    /// a co-client id we DO hold a waiter for, sent by somebody else. every
    /// other lane this client drives is content-addressed, but `TipCoords`
    /// answers "what does THIS node see" — its whole value is who said it, and
    /// it carries no proof. drop, never complete.
    PeerMismatch,
    /// a reply with no waiter left: the request timed out at
    /// [`COCLIENT_TIMEOUT`] and the peer answered afterwards. drop it quietly —
    /// falling through would run the reply into the proof gate and warn
    /// `sync_proof_invalid` at a peer that did nothing but answer the question
    /// we asked, late.
    LateReply,
}

/// the demux decision. mirrors the joiner-side client's rule ("only a candidate
/// source may complete requests", `statesync::p2p`), tightened to the single
/// peer the request was addressed to.
/// `unsigned` says the frame's envelope carries ZEROED requester+proof — the
/// shape a REPLY rides (the transport authenticates a reply, so the server
/// zero-fills those fields). every genuine request carries a signed standing
/// proof, so a co-client-range id with no waiter and no signature is our own
/// answer landing after its timeout. the id space alone cannot say this: both
/// ends of a link draw their co-client ids from the same range, so a peer's
/// REQUEST arrives here with a high id too.
pub fn classify_coclient_frame(
    rpc_id: u64,
    peer: &ed25519::PublicKey,
    addressed_to: Option<&ed25519::PublicKey>,
    unsigned: bool,
) -> CoClientVerdict {
    let Some(addressed_to) = addressed_to else {
        let we_asked_this = rpc_id >= COCLIENT_ID_BASE && unsigned;
        return match we_asked_this {
            true => CoClientVerdict::LateReply,
            false => CoClientVerdict::PeerRequest,
        };
    };
    if addressed_to != peer {
        return CoClientVerdict::PeerMismatch;
    }
    CoClientVerdict::Response
}

/// answer a peer's blob request from this node's store — the serve loop's
/// intercept arm (blobs are host state; `SyncServer` never sees these).
///
/// the cap is decided from the LENGTH, before any read. this used to read the
/// blob and refuse it afterwards, so a one-line `Blob{digest}` naming a 1 GiB
/// module made the node read and re-hash a gigabyte under the store mutex, in
/// the validator's sequential serve loop, only to answer a miss.
pub fn serve_blob(blobs: &dyn blobstore::Blobs, digest: &[u8; 32]) -> statesync::SyncResponse {
    let servable = blobs
        .chunk_len(digest)
        .is_some_and(|len| len <= MAX_SERVED_BLOB as u64);
    let bytes = servable.then(|| blobs.get_chunk(digest)).flatten();
    statesync::SyncResponse::Blob { bytes }
}

/// answer the ranged lane's discovery half: the blob's total length, or an
/// honest miss. no size refusal here — ranges keep every frame bounded.
pub fn serve_blob_info(blobs: &dyn blobstore::Blobs, digest: &[u8; 32]) -> SyncResponse {
    SyncResponse::BlobInfo {
        len: blobs.chunk_len(digest),
    }
}

/// answer one bounded window of a blob. the requested length is clamped to
/// [`MAX_BLOB_RANGE`] server-side, so a greedy (or lying) requester can never
/// make this node emit an oversized frame.
pub fn serve_blob_range(
    blobs: &dyn blobstore::Blobs,
    digest: &[u8; 32],
    offset: u64,
    len: u64,
) -> SyncResponse {
    let want = len.min(MAX_BLOB_RANGE) as usize;
    SyncResponse::BlobRange {
        bytes: blobs.read_range(digest, offset, want),
    }
}

/// the digest of the pack this node last staged to answer a forge object
/// request, per repo. one slot per repo, replaced (and the old bytes released)
/// as the next answer lands — serving catch-up must not grow this node's
/// store one pack per request.
pub type ServedPacks = Arc<Mutex<HashMap<String, [u8; 32]>>>;

/// how many forge packs this node builds at once: ONE. See
/// [`serve_forge_objects`] — the build is whole-repo libgit2 delta
/// compression across every core it can find, so a second concurrent one
/// buys nothing and costs the box.
static PACK_BUILD: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// the ask a pack answers, canonicalized: the normalized repo name, the head,
/// and the base set sorted and deduped. It is the single-flight KEY, so two
/// peers — or one peer's three retries — asking for the same objects must
/// produce the same tuple.
type PackAsk = (String, [u8; statesync::FORGE_OID_LEN], Vec<[u8; statesync::FORGE_OID_LEN]>);

/// what a build settled on: the staged digest, `None` for an honest miss
/// (this node does not hold the head either), or a reason token this node
/// refuses with. `Copy`, so one build's answer is handed to every waiter.
type PackOutcome = Result<Option<[u8; 32]>, &'static str>;

/// the repo's pack slot: a build running now, or the digest the last one
/// staged. [`settle`] keeps at most one `Done` per repo, because
/// [`record_served`] keeps at most one staged pack per repo — remembering a
/// second would hand out bytes that were already released.
enum PackFlight {
    Building(tokio::sync::broadcast::Sender<PackOutcome>),
    Done(PackOutcome),
}

static PACK_FLIGHT: Mutex<std::collections::BTreeMap<PackAsk, PackFlight>> =
    Mutex::new(std::collections::BTreeMap::new());

/// the wait expired before the pack did. NOT a miss: the build is still
/// running under its ask, so the requester's next attempt is answered from
/// the slot instead of starting a second one.
pub(crate) const PACK_OVER_BUDGET: &str = "forge_pack_over_budget";

/// the build task died (a panic in libgit2, or the runtime shutting down).
/// Never remembered, so the next ask rebuilds rather than inheriting it.
pub(crate) const PACK_BUILD_LOST: &str = "forge_pack_build_lost";

/// how long a forge answer waits on a pack before it refuses instead.
///
/// Derived from the REQUESTER's patience, not from the repo: a client
/// abandons a request after [`RETRY_WINDOWS`] in total, so a pack that lands
/// later than that is work nobody is left to receive. Refusing inside the
/// first two windows leaves the requester a whole attempt to come back — and
/// by then the build this refusal abandoned has settled, so the retry is
/// answered from the slot.
///
/// This is also the only SIZE bound the lane needs. A pack too big to build
/// in the budget is refused by the clock, which calibrates itself to the box;
/// a byte ceiling would have to be guessed, and guessed low it makes a large
/// repo permanently unsyncable rather than slow.
const PACK_BUDGET: std::time::Duration = statesync::p2p::WINDOWS_BEFORE_LAST_ATTEMPT;

/// answer a peer's [`SyncRequest::ForgeObjects`]: build the pack that carries
/// `head`'s objects (bounded by the `bases` the peer already holds), stage it,
/// and hand back the digest so the peer pulls the bytes over the ranged lane.
///
/// this is the half that makes a committed head recoverable after the PUSHED
/// pack is gone from every store. pack bytes are not reproducible, so the
/// digest consensus pinned names exactly one node's file; the objects behind
/// it live on every node that materialized the head, and any of them can
/// rebuild an equivalent pack here.
///
/// `None` is an honest miss — this node does not hold the head either. it
/// never packs a walk it has not itself committed to (see
/// [`forge::build_objects`]), so the lane cannot be turned into an amplifier.
pub async fn serve_forge_objects(
    forge_repo: &std::path::Path,
    blobs: &blobstore::BlobHandle,
    served: &ServedPacks,
    repo: &str,
    head: [u8; statesync::FORGE_OID_LEN],
    bases: &[[u8; statesync::FORGE_OID_LEN]],
) -> SyncResponse {
    let miss = SyncResponse::ForgeObjects { digest: None };
    let (Ok(name), Ok(oid)) = (forge::norm_repo(repo), forge::Oid::from_bytes(&head)) else {
        return miss;
    };
    // CANONICAL, because this tuple is the single-flight key: drop the bases
    // this node cannot parse (the build drops them too), then sort and dedup.
    // One peer's three retries, and two peers asking for the same head with
    // the same haves in a different order, are all ONE build.
    let mut known: Vec<[u8; statesync::FORGE_OID_LEN]> = bases
        .iter()
        .filter(|base| forge::Oid::from_bytes(*base).is_ok())
        .copied()
        .collect();
    known.sort_unstable();
    known.dedup();

    let staging = Staging {
        dir: forge_repo.to_path_buf(),
        blobs: blobs.clone(),
        served: Arc::clone(served),
    };
    let ask: PackAsk = (name, head, known);
    let building = ask.clone();
    let outcome = one_pack_per_ask(ask, PACK_BUDGET, move || async move {
        build_and_stage(staging, building, oid).await
    })
    .await;

    match outcome {
        Ok(Some(digest)) => SyncResponse::ForgeObjects {
            digest: Some(digest),
        },
        Ok(None) => miss,
        // a refusal the requester can act on AND retry into: the build this
        // abandoned keeps running, and its digest answers the next ask from
        // the slot. Silence here is what a 60s build looks like from the
        // other end.
        Err(reason) => SyncResponse::Error(reason.into()),
    }
}

/// where a built pack lands: the repo it is built from, the store it is
/// staged in, and the one served-pack slot it takes.
struct Staging {
    dir: std::path::PathBuf,
    blobs: blobstore::BlobHandle,
    served: ServedPacks,
}

/// build one ask's pack and stage it. Runs on its OWN task, so no waiter's
/// deadline can cancel it: the bytes it stages are what the requester's next
/// ask is answered from, which is the only reason a bounded refusal is
/// bounded rather than permanent.
async fn build_and_stage(staging: Staging, ask: PackAsk, head: forge::Oid) -> PackOutcome {
    let (name, _, bases) = ask;
    let oids: Vec<forge::Oid> = bases
        .iter()
        .filter_map(|base| forge::Oid::from_bytes(base).ok())
        .collect();
    let build_dir = staging.dir;
    let build_repo = name.clone();
    // ONE at a time, and the permit is taken HERE rather than by the waiter:
    // a second concurrent whole-repo delta compression buys nothing (libgit2
    // already uses every core) and would take the box instead of the runtime.
    let _one_at_a_time = PACK_BUILD.acquire().await;
    // a joiner has no bases, so this is `pack_closure_many` over the whole
    // repo: libgit2 delta compression, tens of seconds of SYNCHRONOUS CPU on
    // a real mirror, across every worker `set_threads(0)` can find. Run on an
    // async worker it does not merely block this lane — it blocks the runtime
    // thread the consensus loop is scheduled on, and the chain stops beating
    // (#2481: `block_beat_stalled` for 174s while one worker sat in
    // `ll_find_deltas`). It leaves the runtime, always.
    let built = match tokio::task::spawn_blocking(move || {
        forge::build_objects(&build_dir, &build_repo, head, &oids)
    })
    .await
    {
        Ok(Ok(Some(pack))) => pack,
        Ok(Ok(None)) => return Ok(None),
        Ok(Err(e)) => {
            tracing::debug!(
                target: "ducktape::forge",
                reason = "objects_build_failed",
                repo = %name,
                head = %head,
                error = %e,
                "could not build the objects a peer asked for"
            );
            return Ok(None);
        }
        Err(e) => {
            tracing::warn!(
                target: "ducktape::forge",
                reason = "objects_build_panicked",
                repo = %name,
                head = %head,
                error = %e,
                "the pack build for a peer's objects request died"
            );
            return Err(PACK_BUILD_LOST);
        }
    };
    let digest = staging.blobs.put_chunk(built);
    if let Some(previous) = record_served(&staging.served, name, digest) {
        // a requester mid-pull of the released digest misses, retries, and is
        // handed this one — the conversation is per-attempt anyway.
        staging.blobs.forget(&previous);
    }
    Ok(Some(digest))
}

/// ONE build per ask, and a waiter that refuses on a clock rather than
/// waiting out a build nobody is left to receive.
///
/// The two halves only work together. Single-flight alone still leaves the
/// first asker parked for the whole build; a budget alone would abandon the
/// build with it, so the retry that follows starts another one and the peer
/// never converges. Together: the burst collapses to one build, the waiter is
/// answered inside the requester's patience, and the build it left running
/// settles into the slot for the retry.
async fn one_pack_per_ask<F, Fut>(
    ask: PackAsk,
    budget: std::time::Duration,
    build: F,
) -> PackOutcome
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = PackOutcome> + Send + 'static,
{
    let mut waiting = match join_or_start(ask, build) {
        Joined::Settled(outcome) => return outcome,
        Joined::Waiting(rx) => rx,
    };
    match tokio::time::timeout(budget, waiting.recv()).await {
        Ok(Ok(outcome)) => outcome,
        // the builder went away without answering — its slot is gone too, so
        // the next ask rebuilds instead of inheriting the silence.
        Ok(Err(_)) => Err(PACK_BUILD_LOST),
        Err(_) => Err(PACK_OVER_BUDGET),
    }
}

/// what the slot had for this ask.
enum Joined {
    Settled(PackOutcome),
    Waiting(tokio::sync::broadcast::Receiver<PackOutcome>),
}

/// take the ask's slot: join the build already running for it, take the
/// digest it already produced, or become the one that runs it.
fn join_or_start<F, Fut>(ask: PackAsk, build: F) -> Joined
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = PackOutcome> + Send + 'static,
{
    let mut flight = PACK_FLIGHT.lock().expect("pack flight poisoned");
    match flight.get(&ask) {
        Some(PackFlight::Done(outcome)) => return Joined::Settled(*outcome),
        Some(PackFlight::Building(tx)) => return Joined::Waiting(tx.subscribe()),
        None => {}
    }
    let (tx, rx) = tokio::sync::broadcast::channel(1);
    flight.insert(ask.clone(), PackFlight::Building(tx.clone()));
    let running = build();
    tokio::spawn(async move {
        let outcome = running.await;
        // settle BEFORE the send: a waiter woken by the send must not find
        // the slot still claiming the build is running.
        settle(&ask, outcome);
        let _ = tx.send(outcome);
    });
    Joined::Waiting(rx)
}

/// record what a build settled on, under its ask.
///
/// Only a STAGED digest is remembered. A miss is not — this node may
/// materialize the head later, and a remembered miss would refuse it forever.
/// A dead build is not — it would refuse every later ask without ever
/// retrying. And at most one digest per repo survives, because
/// [`record_served`] released the bytes of the one it superseded.
fn settle(ask: &PackAsk, outcome: PackOutcome) {
    let mut flight = PACK_FLIGHT.lock().expect("pack flight poisoned");
    let Ok(Some(_)) = outcome else {
        flight.remove(ask);
        return;
    };
    flight.retain(|other, state| other.0 != ask.0 || !matches!(state, PackFlight::Done(_)));
    flight.insert(ask.clone(), PackFlight::Done(outcome));
}

/// take the repo's one served-pack slot, returning the digest this answer
/// SUPERSEDES and whose bytes the caller must release. re-serving the same
/// pack (same head, same bases) supersedes nothing — releasing it there would
/// throw away the very bytes just handed out.
fn record_served(served: &ServedPacks, repo: String, digest: [u8; 32]) -> Option<[u8; 32]> {
    served
        .lock()
        .expect("served packs poisoned")
        .insert(repo, digest)
        .filter(|previous| *previous != digest)
}

// ---- the ranged requester ----------------------------------------------------

/// why one blob fetch conversation failed. `Miss` and `Transport` are
/// rotate-and-retry; `Corrupt` indicts the source and also rotates; `Stage`
/// is local disk trouble.
#[derive(Debug)]
pub enum BlobFetchError {
    Transport(SyncError),
    /// the source does not hold the digest (or lost it mid-transfer).
    Miss,
    Stage(blobstore::StageError),
    /// the assembled bytes do not hash to the digest — a lying source.
    Corrupt,
}

impl std::fmt::Display for BlobFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "blob fetch transport: {e}"),
            Self::Miss => write!(f, "source does not hold the blob"),
            Self::Stage(e) => write!(f, "blob staging: {e}"),
            Self::Corrupt => write!(f, "assembled bytes do not hash to the digest"),
        }
    }
}

impl From<SyncError> for BlobFetchError {
    fn from(e: SyncError) -> Self {
        Self::Transport(e)
    }
}

/// a client whose serving source can be advanced on an HONEST miss (the
/// transports rotate on failure themselves, but a peer that answers "don't
/// have it" is not a transport failure — it never trips that path).
pub trait SourceRotate {
    fn rotate_source(&self) {}
}

impl<S: P2pSender> SourceRotate for statesync::p2p::P2pSyncClient<S> {
    fn rotate_source(&self) {
        self.advance_source();
    }
}

/// fetch one content-addressed blob into the local store via the ranged mesh
/// lane, retrying across sources up to `attempts` conversations. already
/// resident (verified) → immediate `Ok`. a partial transfer stays staged, so
/// the next attempt resumes at its high-water instead of restarting.
pub async fn fetch_blob<C: SyncClient + SourceRotate>(
    client: &C,
    blobs: &blobstore::BlobHandle,
    digest: &[u8; 32],
    attempts: usize,
) -> Result<(), BlobFetchError> {
    // VERIFIED-resident, not merely present. this gate is the one place where a
    // stat-shaped answer wedges the node: a corrupt local file has a size, so a
    // cheap check returns Ok here, the readiness probe's `get_chunk` keeps
    // missing on the hash, and the swap waiting on these bytes retries forever
    // against a fetch that never runs. the verifying query drops the bad file,
    // so the attempt below re-lands it.
    if blobs.has_verified_chunk(digest) {
        return Ok(());
    }
    let mut last = BlobFetchError::Miss;
    for _ in 0..attempts.max(1) {
        match fetch_once(client, blobs, digest).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                client.rotate_source();
                last = e;
            }
        }
    }
    Err(last)
}

/// one source conversation: discover the length, stream windows into the
/// (resumable) staging slot, verify-then-publish.
async fn fetch_once<C: SyncClient>(
    client: &C,
    blobs: &blobstore::BlobHandle,
    digest: &[u8; 32],
) -> Result<(), BlobFetchError> {
    let len = match client
        .request(SyncRequest::BlobInfo { digest: *digest })
        .await?
    {
        SyncResponse::BlobInfo { len: Some(len) } => len,
        SyncResponse::BlobInfo { len: None } => return Err(BlobFetchError::Miss),
        SyncResponse::Error(e) => return Err(SyncError::Server(e).into()),
        other => return Err(SyncError::UnexpectedResponse(other.kind_name()).into()),
    };
    let mut slot = blobs.stage(*digest, len).map_err(BlobFetchError::Stage)?;
    while slot.offset() < len {
        let window = client
            .request(SyncRequest::BlobRange {
                digest: *digest,
                offset: slot.offset(),
                len: MAX_BLOB_RANGE,
            })
            .await?;
        match window {
            SyncResponse::BlobRange { bytes: Some(bytes) } if !bytes.is_empty() => {
                slot.append(&bytes).map_err(BlobFetchError::Stage)?;
            }
            // a miss or an empty window mid-transfer: the source no longer
            // serves the blob. staging stays for the next source to resume.
            SyncResponse::BlobRange { .. } => return Err(BlobFetchError::Miss),
            SyncResponse::Error(e) => return Err(SyncError::Server(e).into()),
            other => return Err(SyncError::UnexpectedResponse(other.kind_name()).into()),
        }
    }
    match slot.finish() {
        Ok(_) => Ok(()),
        Err(blobstore::StageError::HashMismatch) => Err(BlobFetchError::Corrupt),
        Err(e) => Err(BlobFetchError::Stage(e)),
    }
}

// ---- the fetching code source -------------------------------------------------

/// a [`host::CodeSource`] that goes to the mesh for bytes the local store
/// lacks: local hit → serve it; miss → [`fetch_blob`] (ranged, resumable,
/// verify-then-publish) and re-read. a digest still missing after the
/// attempts answers `None` — the boundary fails closed exactly as before,
/// it just tried the network first.
pub struct FetchingCodeSource<C> {
    local: blobstore::BlobHandle,
    client: C,
    attempts: usize,
}

impl<C> FetchingCodeSource<C> {
    pub fn new(local: blobstore::BlobHandle, client: C, attempts: usize) -> Self {
        Self {
            local,
            client,
            attempts,
        }
    }
}

#[async_trait::async_trait(?Send)]
impl<C: SyncClient + SourceRotate> host::CodeSource for FetchingCodeSource<C> {
    async fn fetch(&self, code_hash: &[u8]) -> Option<Vec<u8>> {
        let digest: [u8; 32] = code_hash.try_into().ok()?;
        if let Err(e) = fetch_blob(&self.client, &self.local, &digest, self.attempts).await {
            // an honest report, not a panic: the caller (realize) fails
            // closed on the None and says which hash it needed. `debug`: this
            // fires once per hash per park attempt (the replica park's failure
            // arm `warn!`s once per attempt with the reason).
            tracing::debug!(
                target: "ducktape::modules",
                digest = %crate::config::hex_bytes(&digest),
                error = %e,
                "code blob unavailable"
            );
        }
        self.local.get_chunk(&digest)
    }

    fn origin(&self) -> &'static str {
        "blob_mesh"
    }
}

// ---- the validator's serve-lane co-client ------------------------------------

/// a [`SyncClient`] for the node that already OWNS its statesync channel
/// receiver (the validator's serve loop): sends ride a clone of the serve
/// lane's sender, responses route back through [`classify_mesh_frame`]'s
/// pending map. sources are the live peer book the runtime re-tracks at every
/// cutover; an honest miss rotates via [`SourceRotate`].
pub struct ServeLaneBlobClient<S: P2pSender<PublicKey = ed25519::PublicKey>> {
    sender: S,
    pending: PendingMap,
    peers: Arc<RwLock<Vec<ed25519::PublicKey>>>,
    cursor: Arc<AtomicUsize>,
    /// this node's real key + standing proof: every request rides
    /// the authed rpc envelope, and a validator's key is in committed
    /// standing, so the serving peer admits its blob lanes.
    requester: [u8; 32],
    proof: [u8; 64],
}

impl<S: P2pSender<PublicKey = ed25519::PublicKey>> Clone for ServeLaneBlobClient<S> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            pending: self.pending.clone(),
            peers: Arc::clone(&self.peers),
            cursor: Arc::clone(&self.cursor),
            requester: self.requester,
            proof: self.proof,
        }
    }
}

impl<S: P2pSender<PublicKey = ed25519::PublicKey>> ServeLaneBlobClient<S> {
    pub fn new(
        sender: S,
        pending: PendingMap,
        peers: Arc<RwLock<Vec<ed25519::PublicKey>>>,
        requester: [u8; 32],
        proof: [u8; 64],
    ) -> Self {
        Self {
            sender,
            pending,
            peers,
            cursor: Arc::new(AtomicUsize::new(0)),
            requester,
            proof,
        }
    }

    /// the source this request rides, SKIPPING this node's own key: the book
    /// is the transport union (members ∪ residents), which contains us, and a
    /// request to ourselves is never delivered — it would burn a whole
    /// conversation (and, where the caller rotates only on failure, a whole
    /// retry) on a guaranteed timeout. `None` = the book holds nobody else,
    /// which the caller reports as an unreachable source.
    fn current_peer(&self) -> Option<ed25519::PublicKey> {
        let peers = self.peers.read().expect("blob peers lock");
        let start = self.cursor.load(Ordering::Relaxed);
        (0..peers.len()).find_map(|step| {
            let peer = &peers[(start + step) % peers.len()];
            let is_me = peer.as_ref() == self.requester.as_slice();
            (!is_me).then(|| peer.clone())
        })
    }

    /// one request ADDRESSED to a named peer. [`SyncClient::request`] takes
    /// its peer off the rotating cursor, which every clone of this client
    /// shares — the forge pack sweeper rotates it on its own failures — so a
    /// caller that puts the peer's NAME on the answer must choose the peer
    /// itself and carry it through the await.
    pub async fn request_from(
        &self,
        peer: ed25519::PublicKey,
        req: SyncRequest,
    ) -> Result<SyncResponse, SyncError> {
        let mut sender = self.sender.clone();
        let pending = self.pending.clone();
        let id = fresh_coclient_id();
        let (requester, proof) = (self.requester, self.proof);
        let (tx, rx) = tokio::sync::oneshot::channel();
        pending.lock().expect("pending blob lock").insert(
            id,
            PendingFetch {
                peer: peer.clone(),
                reply: tx,
            },
        );
        let frame = statesync::encode_rpc(&requester, &proof, id, &statesync::encode_request(&req));
        // the limiter's "admits at" is on the commonware tokio runtime's
        // clock, which is the wall clock.
        let sent = statesync::p2p::send_within_quota(&mut sender, peer, IoBuf::from(frame), |at| {
            tokio::time::sleep(at.duration_since(SystemTime::now()).unwrap_or_default())
        })
        .await;
        if let Err(refused) = sent {
            pending.lock().expect("pending blob lock").remove(&id);
            return Err(SyncError::Transport(format!(
                "the local mesh refused the blob request ({})",
                refused.reason()
            )));
        }
        match tokio::time::timeout(COCLIENT_TIMEOUT, rx).await {
            Ok(Ok(resp)) => Ok(resp),
            // timed out, or the demux dropped a malformed body: fail the
            // request so the caller rotates — and clear our slot either way.
            _ => {
                pending.lock().expect("pending blob lock").remove(&id);
                Err(SyncError::Transport(format!(
                    "blob request {id} timed out on the serve lane"
                )))
            }
        }
    }
}

impl<S: P2pSender<PublicKey = ed25519::PublicKey>> SourceRotate for ServeLaneBlobClient<S> {
    fn rotate_source(&self) {
        self.cursor.fetch_add(1, Ordering::Relaxed);
    }
}

impl<S: P2pSender<PublicKey = ed25519::PublicKey>> SyncClient for ServeLaneBlobClient<S> {
    fn request(
        &self,
        req: SyncRequest,
    ) -> impl std::future::Future<Output = Result<SyncResponse, SyncError>> + Send {
        // the peer is chosen HERE, before the await, so the request and the
        // answer it is attributed to name the same node even after a clone
        // rotates the shared cursor underneath us.
        let peer = self.current_peer();
        let client = self.clone();
        async move {
            let Some(peer) = peer else {
                return Err(SyncError::Transport("no blob peers tracked".into()));
            };
            client.request_from(peer, req).await
        }
    }
}

// ---- the forge pack sweep -----------------------------------------------------

/// how often the sweep re-reads forge's catch-up map. a missed pack is a
/// quality-of-service gap, not an availability one (the committed head is
/// durable and the root is correct either way), so this is deliberately slow:
/// it costs one `stat` per tick when nothing is outstanding.
const PACK_SWEEP_TICK: std::time::Duration = std::time::Duration::from_secs(30);

/// keep this node's forge substrate healthy, forever: pull the packs forge is
/// waiting on, then collapse the packs it has piled up.
///
/// forge's submit-time fanout reaches only the CURRENT validators, so a
/// resident — or a validator that was down during the push — holds a committed
/// head whose objects never arrive. its on-disk git ref then lags that head
/// indefinitely and `git clone` from this node serves a stale branch. nothing
/// else breaks: the committed head is durable and the root is correct with or
/// without the objects (that decoupling IS forge's fork-safety invariant).
///
/// this closes the gap from the OUTSIDE: read the digests forge published in
/// its own catch-up file, fetch the bytes through the same verified ranged lane
/// module code uses, and stop. forge picks them up from its blob store on its
/// next `commit_block` — nothing here touches the repo, the module, or the
/// host, so it can never influence a root.
///
/// compaction rides the same tick because it needs the same two things (the
/// workspace path and a moment when nothing is mid-block) and has the same
/// standing — node-local, ref-reading, root-blind. it is a `read_dir` on a
/// caught-up workspace until a repo passes [`forge::COMPACT_PACK_LIMIT`], and
/// the repack itself blocks, so it runs off the runtime's blocking pool.
pub async fn sweep_forge_packs<C: SyncClient + SourceRotate>(
    client: C,
    blobs: blobstore::BlobHandle,
    forge_repo: std::path::PathBuf,
    label: String,
) {
    loop {
        tokio::time::sleep(PACK_SWEEP_TICK).await;
        sweep_packs_once(&client, &blobs, &forge_repo, &label).await;
        compact_forge_packs_once(forge_repo.clone(), &label).await;
    }
}

/// one compaction tick, off-thread. a failure is this node's own housekeeping
/// falling behind — the repo still serves every committed head it holds — and
/// every tick retries, so like the pull above this is per-attempt noise rather
/// than a warning that would evict the ring 120 times an hour.
async fn compact_forge_packs_once(forge_repo: std::path::PathBuf, label: &str) {
    let compacted = tokio::task::spawn_blocking(move || {
        forge::compact_repos(&forge_repo, forge::COMPACT_PACK_LIMIT)
    })
    .await;
    let failure = match compacted {
        Ok(Ok(_)) => return,
        Ok(Err(e)) => e.to_string(),
        Err(e) => e.to_string(),
    };
    tracing::debug!(
        target: "ducktape::forge",
        node = %label,
        reason = "compaction_failed",
        error = %failure,
        "could not collapse this node's forge packfiles"
    );
}

/// one sweep tick — the whole body, so it is reachable without a clock.
/// returns how many packs this tick pulled.
async fn sweep_packs_once<C: SyncClient + SourceRotate>(
    client: &C,
    blobs: &blobstore::BlobHandle,
    forge_repo: &std::path::Path,
    label: &str,
) -> usize {
    // a corrupt map is forge's own fail-stop at boot; from out here it is just
    // a sweep with nothing it can act on.
    let outstanding = match forge::pending_branches(forge_repo) {
        Ok(branches) => branches,
        Err(e) => {
            tracing::debug!(
                target: "ducktape::forge",
                node = %label,
                error = %e,
                reason = "pending_unreadable",
                "pack sweep idle"
            );
            return 0;
        }
    };
    let mut pulled = 0usize;
    for pending in outstanding {
        // VERIFYING, not cheap: this decides whether the sweep pulls at all, so
        // a corrupt pack file answering "held" would park the digest here
        // forever — and the verifying query drops it, so the pull below lands.
        if blobs.has_verified_chunk(&pending.digest) {
            continue; // held already; forge materializes it on its own.
        }
        // the pushed pack first: it is the exact answer, and while any node
        // still holds those bytes this costs one ranged pull.
        let by_digest = fetch_blob(
            client,
            blobs,
            &pending.digest,
            crate::constants::BLOB_FETCH_ATTEMPTS,
        )
        .await;
        if by_digest.is_ok() {
            pulled += 1;
            tracing::info!(
                target: "ducktape::forge",
                node = %label,
                repo = %pending.repo,
                refname = %pending.refname,
                "pulled a forge pack this node was missing"
            );
            continue;
        }
        // nobody answers for those bytes any more. the OBJECTS still exist on
        // every peer that materialized the head, so ask one to rebuild them.
        match fetch_forge_objects(client, blobs, forge_repo, &pending).await {
            Ok(()) => {
                pulled += 1;
                tracing::info!(
                    target: "ducktape::forge",
                    node = %label,
                    repo = %pending.repo,
                    refname = %pending.refname,
                    head = %pending.head,
                    "a peer rebuilt the objects for a head whose pack is gone"
                );
            }
            Err(e) => {
                // every tick retries, so this is per-attempt noise, not a
                // failure: the branch stays behind one more tick.
                tracing::debug!(
                    target: "ducktape::forge",
                    node = %label,
                    reason = "objects_fetch_failed",
                    repo = %pending.repo,
                    refname = %pending.refname,
                    error = %e,
                    "neither the pushed pack nor a rebuilt one arrived"
                );
            }
        }
    }
    pulled
}

/// pull the objects a committed head needs from a peer that materialized it,
/// rotating sources like every other fetch here.
///
/// the two steps have to ride ONE source: the peer answers with the digest of
/// a pack IT staged, and no other node has those bytes. `fetch_blob`'s loop
/// rotates between attempts and holds a source within one, so the ask and the
/// ranged pull sit together inside a single attempt.
async fn fetch_forge_objects<C: SyncClient + SourceRotate>(
    client: &C,
    blobs: &blobstore::BlobHandle,
    forge_repo: &std::path::Path,
    pending: &forge::PendingBranch,
) -> Result<(), BlobFetchError> {
    let mut bases = forge::on_disk_heads(forge_repo, &pending.repo).unwrap_or_default();
    bases.truncate(statesync::MAX_FORGE_BASES);
    let mut last = BlobFetchError::Miss;
    for _ in 0..crate::constants::BLOB_FETCH_ATTEMPTS.max(1) {
        match objects_once(client, blobs, forge_repo, pending, &bases).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                client.rotate_source();
                last = e;
            }
        }
    }
    Err(last)
}

/// one source conversation: ask for the objects, pull the pack the peer
/// staged, install it, and release the courier bytes.
///
/// no trust attaches to which peer answered — [`forge::install_objects`]
/// re-hashes every object as it indexes and then demands the full closure of
/// the head consensus committed, so a lying peer lands nothing.
async fn objects_once<C: SyncClient>(
    client: &C,
    blobs: &blobstore::BlobHandle,
    forge_repo: &std::path::Path,
    pending: &forge::PendingBranch,
    bases: &[forge::Oid],
) -> Result<(), BlobFetchError> {
    let request = SyncRequest::ForgeObjects {
        repo: pending.repo.clone(),
        head: *pending.head.as_bytes(),
        bases: bases.iter().map(|base| *base.as_bytes()).collect(),
    };
    let digest = match client.request(request).await? {
        SyncResponse::ForgeObjects {
            digest: Some(digest),
        } => digest,
        SyncResponse::ForgeObjects { digest: None } => return Err(BlobFetchError::Miss),
        SyncResponse::Error(e) => return Err(SyncError::Server(e).into()),
        other => return Err(SyncError::UnexpectedResponse(other.kind_name()).into()),
    };
    fetch_once(client, blobs, &digest).await?;
    let Some(pack) = blobs.get_chunk(&digest) else {
        return Err(BlobFetchError::Miss);
    };
    let installed = forge::install_objects(forge_repo, &pending.repo, pending.head, &pack);
    // the pack was a courier: the objects live in the odb now, and its digest
    // is this peer's alone — nothing will ever ask for it again.
    blobs.forget(&digest);
    installed.map_err(|_| BlobFetchError::Corrupt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u64) -> ed25519::PublicKey {
        use commonware_cryptography::Signer;
        ed25519::PrivateKey::from_seed(seed).public_key()
    }

    /// the divergence watch puts a peer's NAME on an unproven answer, so the
    /// id alone must never complete a waiter: only the node we addressed can.
    #[test]
    fn only_the_addressed_peer_completes_a_coclient_waiter() {
        let (asked, other) = (key(1), key(2));

        assert_eq!(
            classify_coclient_frame(fresh_coclient_id(), &asked, Some(&asked), true),
            CoClientVerdict::Response,
            "the peer we asked answers our request"
        );
        assert_eq!(
            classify_coclient_frame(fresh_coclient_id(), &other, Some(&asked), true),
            CoClientVerdict::PeerMismatch,
            "a third party may not answer a request addressed elsewhere"
        );
        assert_eq!(
            classify_coclient_frame(7, &other, None, false),
            CoClientVerdict::PeerRequest,
            "a signed frame with no waiter is a peer's own request"
        );
    }

    /// a peer that answers after our 6 s timeout is honest and late. the demux
    /// must recognise its own id space and drop the frame, never let it reach
    /// the proof gate — our replies ride zeroed auth fields, so the gate would
    /// warn `sync_proof_invalid` at a co-validator that did nothing wrong.
    #[test]
    fn a_reply_that_missed_its_timeout_is_dropped_not_refused() {
        assert_eq!(
            classify_coclient_frame(fresh_coclient_id(), &key(3), None, true),
            CoClientVerdict::LateReply,
            "an unsigned frame on our own id with no waiter left is a late reply"
        );
        assert_eq!(
            classify_coclient_frame(fresh_coclient_id(), &key(3), None, false),
            CoClientVerdict::PeerRequest,
            "a peer's own request draws its id from the same range — a SIGNED \
             frame is always a request, whatever its id"
        );
        assert_eq!(
            classify_coclient_frame(COCLIENT_ID_BASE - 1, &key(3), None, true),
            CoClientVerdict::PeerRequest,
            "the id space below the base was never ours to have asked on"
        );
    }

    /// a peer that has seen one of our ids must not be able to name the next.
    #[test]
    fn coclient_ids_are_random_above_the_base() {
        let ids: std::collections::HashSet<u64> = (0..64).map(|_| fresh_coclient_id()).collect();

        assert_eq!(ids.len(), 64, "co-client ids must not repeat or step");
        assert!(
            ids.iter().all(|id| *id >= COCLIENT_ID_BASE),
            "every co-client id stays in the id space peers' request ids never reach"
        );
    }

    /// serving catch-up must not grow this node's store one pack per request:
    /// each answer supersedes the repo's previous one, and re-serving the
    /// SAME pack must supersede nothing (releasing it would throw away the
    /// bytes just handed out).
    #[test]
    fn each_served_pack_supersedes_only_the_repos_previous_one() {
        let served: ServedPacks = Default::default();

        assert_eq!(
            record_served(&served, "demo".into(), [1u8; 32]),
            None,
            "the first answer for a repo supersedes nothing"
        );
        assert_eq!(
            record_served(&served, "demo".into(), [2u8; 32]),
            Some([1u8; 32]),
            "the next answer releases the one it replaced"
        );
        assert_eq!(
            record_served(&served, "demo".into(), [2u8; 32]),
            None,
            "re-serving the same pack must not release it"
        );
        assert_eq!(
            record_served(&served, "other".into(), [3u8; 32]),
            None,
            "repos hold their own slot"
        );
        assert_eq!(
            record_served(&served, "demo".into(), [4u8; 32]),
            Some([2u8; 32]),
            "and each slot still tracks its own repo"
        );
    }

    /// a test [`SyncClient`] answering the ranged lane from a rotating list of
    /// per-source stores — the serve functions ARE the server, so every test
    /// exercises the true serve+fetch pairing.
    #[derive(Clone)]
    struct StoreClient {
        sources: Arc<Vec<blobstore::BlobHandle>>,
        cursor: Arc<AtomicUsize>,
    }

    impl StoreClient {
        fn new(sources: Vec<blobstore::BlobHandle>) -> Self {
            Self {
                sources: Arc::new(sources),
                cursor: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn store(&self) -> &blobstore::BlobHandle {
            &self.sources[self.cursor.load(Ordering::Relaxed) % self.sources.len()]
        }
    }

    impl SourceRotate for StoreClient {
        fn rotate_source(&self) {
            self.cursor.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl SyncClient for StoreClient {
        fn request(
            &self,
            req: SyncRequest,
        ) -> impl std::future::Future<Output = Result<SyncResponse, SyncError>> + Send {
            let resp = match req {
                SyncRequest::BlobInfo { digest } => serve_blob_info(self.store(), &digest),
                SyncRequest::BlobRange {
                    digest,
                    offset,
                    len,
                } => serve_blob_range(self.store(), &digest, offset, len),
                other => SyncResponse::Error(format!("unexpected {}", other.kind_name())),
            };
            async move { Ok(resp) }
        }
    }

    fn payload() -> Vec<u8> {
        // several windows long, not window-aligned.
        (0..(2 * MAX_BLOB_RANGE as usize + 12345))
            .map(|i| (i % 251) as u8)
            .collect()
    }

    #[tokio::test]
    async fn fetch_assembles_a_multi_window_blob_and_verifies() {
        let source = blobstore::BlobHandle::default();
        let digest = source.put_chunk(payload());
        let local = blobstore::BlobHandle::default();
        let client = StoreClient::new(vec![source]);

        fetch_blob(&client, &local, &digest, 1)
            .await
            .expect("fetch succeeds");
        assert_eq!(local.get_chunk(&digest), Some(payload()));
        // idempotent: already resident answers without a conversation.
        fetch_blob(&client, &local, &digest, 1)
            .await
            .expect("resident short-circuits before any conversation");
    }

    #[tokio::test]
    async fn fetch_rotates_past_misses_to_a_serving_source() {
        let truth = payload();
        // sources 0 and 1 hold nothing; source 2 serves. two honest misses
        // must rotate the cursor twice and land the fetch on the third try.
        let good = blobstore::BlobHandle::default();
        let digest = good.put_chunk(truth.clone());
        let local = blobstore::BlobHandle::default();
        let client = StoreClient::new(vec![
            blobstore::BlobHandle::default(),
            blobstore::BlobHandle::default(),
            good,
        ]);
        fetch_blob(&client, &local, &digest, 3)
            .await
            .expect("third source serves");
        assert_eq!(local.get_chunk(&digest), Some(truth));
    }

    /// a client that serves plausible info but corrupted windows.
    #[derive(Clone)]
    struct LiarClient {
        bytes: Arc<Vec<u8>>,
    }

    impl SourceRotate for LiarClient {}

    impl SyncClient for LiarClient {
        fn request(
            &self,
            req: SyncRequest,
        ) -> impl std::future::Future<Output = Result<SyncResponse, SyncError>> + Send {
            let resp = match req {
                SyncRequest::BlobInfo { .. } => SyncResponse::BlobInfo {
                    len: Some(self.bytes.len() as u64),
                },
                SyncRequest::BlobRange { offset, len, .. } => {
                    let end = (offset as usize + len as usize).min(self.bytes.len());
                    SyncResponse::BlobRange {
                        bytes: Some(self.bytes[offset as usize..end].to_vec()),
                    }
                }
                other => SyncResponse::Error(format!("unexpected {}", other.kind_name())),
            };
            async move { Ok(resp) }
        }
    }

    #[tokio::test]
    async fn fetch_drops_corrupt_assembly_fail_closed() {
        let truth = payload();
        let digest = {
            let s = blobstore::BlobHandle::default();
            s.put_chunk(truth.clone())
        };
        let mut lie = truth.clone();
        lie[3] ^= 0x01;
        let local = blobstore::BlobHandle::default();
        let client = LiarClient {
            bytes: Arc::new(lie),
        };
        let err = fetch_blob(&client, &local, &digest, 2)
            .await
            .expect_err("a lying source must never publish");
        assert!(matches!(err, BlobFetchError::Corrupt), "got {err}");
        assert!(!local.has_chunk(&digest));
    }

    /// the wedge the verifying gate exists to stop: a corrupt local file has a
    /// size, so a stat-shaped presence check would answer the fetch "already
    /// held" while the readiness probe's `get_chunk` keeps missing on the hash
    /// — a swap that retries forever against a fetch that never runs.
    #[tokio::test]
    async fn a_corrupt_local_copy_is_refetched_not_reported_held() {
        let root = tempfile::tempdir().expect("tempdir");
        let truth = payload();
        let source = blobstore::BlobHandle::default();
        let digest = source.put_chunk(truth.clone());

        let local = blobstore::BlobHandle::persistent(root.path()).expect("blob root");
        local.put_chunk(truth.clone());
        std::fs::write(root.path().join(hex_name(&digest)), b"rotted").expect("corrupt the file");
        // cold memory: the corrupt file is all this node has.
        let local = blobstore::BlobHandle::persistent(root.path()).expect("blob root");
        assert_eq!(local.get_chunk(&digest), None, "the local copy is unusable");

        fetch_blob(&StoreClient::new(vec![source]), &local, &digest, 1)
            .await
        .expect("the fetch must run and heal the digest");
        assert_eq!(local.get_chunk(&digest), Some(truth));
    }

    fn hex_name(digest: &[u8; 32]) -> String {
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn serve_blob_range_clamps_and_serves_tails() {
        let blobs = blobstore::BlobHandle::default();
        let digest = blobs.put_chunk(vec![9u8; 100]);
        // server-side clamp: an oversized ask answers at most MAX_BLOB_RANGE.
        match serve_blob_range(&blobs, &digest, 0, u64::MAX) {
            SyncResponse::BlobRange { bytes: Some(b) } => assert_eq!(b.len(), 100),
            other => panic!("unexpected {other:?}"),
        }
        // tail read past the end is empty-but-present; offset past end is a miss.
        match serve_blob_range(&blobs, &digest, 100, 10) {
            SyncResponse::BlobRange { bytes: Some(b) } => assert!(b.is_empty()),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            serve_blob_range(&blobs, &digest, 101, 10),
            SyncResponse::BlobRange { bytes: None }
        );
        assert_eq!(
            serve_blob_info(&blobs, &digest),
            SyncResponse::BlobInfo { len: Some(100) }
        );
        assert_eq!(
            serve_blob_info(&blobs, &[0u8; 32]),
            SyncResponse::BlobInfo { len: None }
        );
    }

    #[test]
    fn serve_blob_answers_store_hits_and_honest_misses() {
        let blobs = blobstore::BlobHandle::default();
        let digest = blobs.put_chunk(b"You are quack.".to_vec());
        assert_eq!(
            serve_blob(&blobs, &digest),
            statesync::SyncResponse::Blob {
                bytes: Some(b"You are quack.".to_vec())
            }
        );
        assert_eq!(
            serve_blob(&blobs, &[0u8; 32]),
            statesync::SyncResponse::Blob { bytes: None }
        );
        // oversized blobs answer as a miss — the mesh frame stays bounded.
        let big = blobs.put_chunk(vec![7u8; MAX_SERVED_BLOB + 1]);
        assert_eq!(
            serve_blob(&blobs, &big),
            statesync::SyncResponse::Blob { bytes: None }
        );
    }

    /// a store whose length is free and whose read is fatal: the whole point of
    /// the over-cap refusal is that a peer naming a 1 GiB digest never makes
    /// this node read one.
    struct LengthOnlyBlobs(u64);

    impl blobstore::Blobs for LengthOnlyBlobs {
        fn put_chunk(&self, _: Vec<u8>) -> [u8; 32] {
            unreachable!("serve_blob never writes")
        }
        fn get_chunk(&self, _: &[u8; 32]) -> Option<Vec<u8>> {
            panic!("serve_blob read a blob it had already refused on length")
        }
        fn has_chunk(&self, _: &[u8; 32]) -> bool {
            true
        }
        fn chunk_len(&self, _: &[u8; 32]) -> Option<u64> {
            Some(self.0)
        }
        fn read_range(&self, _: &[u8; 32], _: u64, _: usize) -> Option<Vec<u8>> {
            unreachable!("serve_blob never ranges")
        }
    }

    #[test]
    fn an_over_cap_blob_is_refused_without_reading_it() {
        let blobs = LengthOnlyBlobs(1024 * 1024 * 1024);
        assert_eq!(
            serve_blob(&blobs, &[4u8; 32]),
            statesync::SyncResponse::Blob { bytes: None }
        );
    }

    /// a forge workspace holding a committed head whose pack never arrived —
    /// the exact state a resident lands in, since it is never a submit-time
    /// fanout target. built through forge's real push path, so the catch-up
    /// file is written by the code that owns it.
    fn workspace_missing_a_pack(tag: &str, pack: Vec<u8>) -> (tempfile::TempDir, [u8; 32]) {
        let dir = tempfile::Builder::new().prefix(tag).tempdir().expect("tmp");
        // the digest is content-addressed, so name the pack without holding it.
        let digest: [u8; 32] = <sha2::Sha256 as sha2::Digest>::digest(&pack).into();
        let mut forge = forge::Forge::with_blobs(
            "forge",
            dir.path().to_path_buf(),
            blobstore::BlobHandle::default(),
        )
        .expect("forge");
        let msg = sdk::Msg {
            target: "forge".into(),
            payload: forge::encode_msg(&forge::ForgeMsg::PushRefs {
                repo: "demo".into(),
                updates: vec![forge::RefUpdate {
                    ref_name: "main".into(),
                    prev_oid: None,
                    new_oid: Some(vec![7u8; 20]),
                }],
                tags: Vec::new(),
                pack_digest: Some(digest.to_vec()),
                cert: None,
            }),
        };
        let mut ctx = sdk_testkit::TestCtx::with_env(sdk::Env {
            height: 0,
            consensus_time: 1,
            origin: sdk::Origin::External(vec![1u8; 32]),
            me: "forge".into(),
            cause: sdk::Cause::Direct,
        });
        futures::executor::block_on(async {
            <forge::Forge as sdk::Module>::execute(&mut forge, &mut ctx, &msg)
                .await
                .expect("push");
            <forge::Forge as sdk::Module>::commit_block(&mut forge)
                .await
                .expect("commit");
        });
        (dir, digest)
    }

    #[tokio::test]
    async fn the_sweep_pulls_a_pack_forge_is_waiting_on_and_then_goes_quiet() {
        let pack = payload();
        let (dir, digest) = workspace_missing_a_pack("sweep-pull", pack.clone());

        let source = blobstore::BlobHandle::default();
        assert_eq!(source.put_chunk(pack.clone()), digest, "content-addressed");
        let local = blobstore::BlobHandle::default();
        let client = StoreClient::new(vec![source]);

        assert_eq!(
            sweep_packs_once(&client, &local, dir.path(), "n").await,
            1,
            "the outstanding pack is pulled"
        );
        assert_eq!(local.get_chunk(&digest), Some(pack));

        // forge has not run a block yet, so the head is still outstanding — but
        // the bytes are held now, so the sweep must not re-fetch them.
        assert_eq!(
            sweep_packs_once(&client, &local, dir.path(), "n").await,
            0,
            "a held pack is forge's to materialize, not ours to re-pull"
        );
    }

    #[tokio::test]
    async fn the_sweep_is_inert_without_a_forge_workspace() {
        let dir = tempfile::Builder::new()
            .prefix("sweep-none")
            .tempdir()
            .unwrap();
        let client = StoreClient::new(vec![blobstore::BlobHandle::default()]);
        assert_eq!(
            sweep_packs_once(&client, &blobstore::BlobHandle::default(), dir.path(), "n").await,
            0,
        );
    }
}

#[cfg(test)]
mod pack_flight {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::time::Duration;

    /// distinct per test: the slot map is process-global, and `settle` evicts
    /// by repo name.
    fn ask(repo: &str) -> PackAsk {
        (repo.to_string(), [7u8; statesync::FORGE_OID_LEN], Vec::new())
    }

    /// A RETRY BURST IS ONE PACK. The client re-sends the same request up to
    /// `RETRY_WINDOWS.len()` times, and every peer catching up asks for the
    /// same head. Keyed only by the permit, those queued behind each other and
    /// each rebuilt the whole repo; the last one to finish won and the rest
    /// was heat.
    #[tokio::test(start_paused = true)]
    async fn a_retry_burst_builds_one_pack_not_one_per_attempt() {
        let builds = Arc::new(AtomicUsize::new(0));
        let asked = ask("burst");
        let waiting: Vec<_> = (0..4)
            .map(|_| {
                let counted = Arc::clone(&builds);
                one_pack_per_ask(asked.clone(), Duration::from_secs(30), move || async move {
                    counted.fetch_add(1, AtomicOrdering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Ok(Some([1u8; 32]))
                })
            })
            .collect();

        let answers = futures::future::join_all(waiting).await;
        assert_eq!(
            builds.load(AtomicOrdering::SeqCst),
            1,
            "four asks for one head must start one build"
        );
        for answer in answers {
            assert_eq!(answer, Ok(Some([1u8; 32])), "and all four get its digest");
        }
    }

    /// THE REFUSAL IS ON A CLOCK, AND THE BUILD OUTLIVES IT. An impatient
    /// waiter must be told `forge_pack_over_budget` inside the requester's
    /// patience rather than held for the whole build — and abandoning it must
    /// not abandon the work, or the retry that follows starts another one.
    #[tokio::test(start_paused = true)]
    async fn an_impatient_waiter_refuses_while_the_build_runs_on() {
        let builds = Arc::new(AtomicUsize::new(0));
        let asked = ask("over-budget");

        let counted = Arc::clone(&builds);
        let impatient = one_pack_per_ask(asked.clone(), Duration::from_millis(50), move || async move {
            counted.fetch_add(1, AtomicOrdering::SeqCst);
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(Some([2u8; 32]))
        });
        let counted = Arc::clone(&builds);
        let patient = one_pack_per_ask(asked, Duration::from_secs(300), move || async move {
            counted.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(Some([9u8; 32]))
        });

        let (refused, held) = futures::future::join(impatient, patient).await;
        assert_eq!(
            refused,
            Err(PACK_OVER_BUDGET),
            "the waiter answers on its budget, it does not wait out the build"
        );
        assert_eq!(
            held,
            Ok(Some([2u8; 32])),
            "the build the refusal abandoned still settles, and still answers"
        );
        assert_eq!(builds.load(AtomicOrdering::SeqCst), 1, "one build, not two");
    }

    /// ...AND ITS DIGEST IS WHAT THE RETRY GETS. This is the whole reason the
    /// refusal above is bounded rather than permanent: without the slot, every
    /// ask for a repo slower than the budget starts a fresh build and the peer
    /// never converges.
    #[tokio::test]
    async fn a_settled_digest_answers_the_next_ask_without_a_second_build() {
        let asked = ask("settled");
        settle(&asked, Ok(Some([3u8; 32])));

        let builds = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&builds);
        let answer = one_pack_per_ask(asked, Duration::from_millis(1), move || async move {
            counted.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(Some([9u8; 32]))
        })
        .await;

        assert_eq!(answer, Ok(Some([3u8; 32])), "the slot answers");
        assert_eq!(
            builds.load(AtomicOrdering::SeqCst),
            0,
            "and nothing is rebuilt to produce it"
        );
    }

    /// A MISS AND A DEAD BUILD ARE NOT ANSWERS TO REMEMBER. This node can
    /// materialize the head later, and a build that panicked says nothing
    /// about the next one — remembering either would refuse every later ask
    /// for that head without ever retrying it.
    #[tokio::test]
    async fn neither_a_miss_nor_a_dead_build_is_remembered() {
        for (name, outcome) in [("miss", Ok(None)), ("lost", Err(PACK_BUILD_LOST))] {
            let asked = ask(name);
            settle(&asked, outcome);

            let builds = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&builds);
            let answer = one_pack_per_ask(asked, Duration::from_secs(30), move || async move {
                counted.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(Some([4u8; 32]))
            })
            .await;

            assert_eq!(answer, Ok(Some([4u8; 32])), "{name}: the next ask builds");
            assert_eq!(
                builds.load(AtomicOrdering::SeqCst),
                1,
                "{name}: and it really did build"
            );
        }
    }
}

#[cfg(test)]
mod forge_pack_shape_lint {
    /// THE PACK BUILD NEVER RUNS ON AN ASYNC WORKER. A joiner sends no bases,
    /// so [`super::serve_forge_objects`] reaches `pack_closure_many` — whole-
    /// repo libgit2 delta compression, tens of seconds of synchronous CPU.
    /// Called straight from the serve task it blocked the runtime thread the
    /// consensus loop was scheduled on, and the chain stopped beating with it
    /// (#2481: `block_beat_stalled ... stalled_ms=173873` while one
    /// `tokio-rt-worker` sat in `ll_find_deltas`).
    #[test]
    fn the_pack_build_is_the_first_thing_inside_spawn_blocking() {
        const OPEN: &str = "spawn_blocking(move || {";
        // split so this needle does not match itself in the file it scans.
        const CALL: &str = concat!("forge::", "build_objects(");
        let src = include_str!("blob_fetch.rs");
        let calls: Vec<usize> = src.match_indices(CALL).map(|(at, _)| at).collect();
        assert_eq!(
            calls.len(),
            1,
            "one call site, so one place to keep off the runtime: {calls:?}"
        );
        let at = calls[0];
        let opened = src[..at]
            .rfind(OPEN)
            .expect("the pack build runs on the blocking pool");
        let between = &src[opened + OPEN.len()..at];
        assert!(
            between.trim().is_empty(),
            "the build must be the body of the `spawn_blocking`, not reached \
             past {between:?}"
        );
    }

    /// AND IT NEVER RUNS ON THE SERVE LOOP. `spawn_blocking` keeps the runtime
    /// beating; it does not keep the statesync serve lane free, because the
    /// loop would still await the join handle. Awaited inline, one whole-repo
    /// pack is head-of-line blocking for every other kind a peer is waiting
    /// on — which is why a resident's `tip_coords` and `frames` timed out too.
    #[test]
    fn the_serve_loop_hands_a_forge_answer_to_its_own_task() {
        let src = include_str!("validator/wiring.rs");
        let served: Vec<usize> = src
            .match_indices("serve_forge_objects(")
            .map(|(at, _)| at)
            .collect();
        assert_eq!(
            served.len(),
            1,
            "the serve fn is called once, from the spawned answer: {served:?}"
        );
        let spawner = src
            .find("fn spawn_forge_answer(")
            .expect("forge answers have their own task");
        assert!(
            spawner < served[0],
            "the only call must be inside `spawn_forge_answer`, not on the loop"
        );
        let arm_at = src
            .find("SyncRequest::ForgeObjects { repo, head, bases } => {")
            .expect("the serve loop routes ForgeObjects");
        let arm = &src[arm_at..src.len().min(arm_at + 1200)];
        assert!(
            arm.contains("spawn_forge_answer(") && arm.contains("continue;"),
            "the arm must delegate and leave the loop, not produce a response"
        );
    }
}
