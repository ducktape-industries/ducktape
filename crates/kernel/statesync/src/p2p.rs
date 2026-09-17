//! the p2p transport binding: a [`SyncClient`] over one commonware p2p channel.
//!
//! requests ride the rpc envelope (`id || frame`) addressed to ONE serving
//! peer at a time; a background dispatch task drains the channel receiver and
//! routes each response to its awaiting request by id. responses claiming to
//! be from a peer outside the candidate SOURCE set are dropped — the mesh is
//! authenticated, and installable payloads are root-verified anyway, but there
//! is no reason to let an unrelated peer complete someone else's request.
//!
//! SOURCE ROTATION: the client holds a candidate list, not a pinned server.
//! every payload is verified against consensus-agreed roots, so which peer
//! serves is purely an availability question — a request that fails at the
//! transport (unreachable send, reaper timeout) advances the cursor to the
//! next candidate and surfaces the error, and the caller's existing retry
//! (manifest loops, the qmdb refetch ladder) lands on the new source. one
//! failure advances the cursor once, no matter how many concurrent requests
//! observed it.
//!
//! every request is TIMED OUT by the dispatch task's reaper: p2p sends to a
//! peer whose link is not (or no longer) up are silently dropped by the mesh,
//! so an unanswered request must fail — the caller retries (a joiner's
//! manifest loop does exactly that while the mesh warms up) instead of parking
//! forever on a reply that will never come. the reaper lives in the dispatch
//! task because runtime contexts are move-only (not `Clone`): the spawned task
//! owns the only clock, and the request future itself stays runtime-free.
//!
//! BUSY-MESH RETRY: a single missed reaper window does not mean the source is
//! dead — a founder pushing tens of consensus blocks/s over the same overlay
//! can starve a statesync request in flight for a few seconds without either
//! side being wrong. [`send_request`](P2pSyncClient::send_request) therefore
//! retries the SAME request under the SAME rpc id against the SAME source,
//! stepping the reaper window across [`RETRY_WINDOWS`] (3s → 6s → 12s) before
//! it surfaces a timeout and lets [`Sources::advance_past`] rotate the caller
//! onto the next candidate — a dead source is still abandoned within the
//! bounded total (see [`RETRY_WINDOWS`]'s doc), it just survives a merely busy
//! mesh first. The id is the load-bearing part: a server slower than one
//! window answers the id it was asked, and only a stable id lets that answer
//! complete the attempt that replaced the reaped one. every occurrence — each
//! retried attempt and the final surfaced timeout alike — bumps that source's
//! timeout counter and, latched, a `warn!` (see [`TIMEOUT_WARN_EVERY`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use commonware_p2p::{Receiver, Recipients, Sender};
use commonware_runtime::{Clock, IoBuf, Spawner};
use futures::channel::oneshot;
use tracing::{debug, warn};

use crate::{
    SyncClient, SyncError, SyncRequest, SyncResponse, TipCoords, decode_response, decode_rpc,
    encode_request, encode_rpc,
};

/// the reaper's sweep interval. a pending request's survival window (see
/// [`RETRY_WINDOWS`]) is measured in whole sweeps of this.
pub const REAP_INTERVAL: Duration = Duration::from_secs(3);

/// per-attempt survival windows a request rides against the SAME source
/// before [`P2pSyncClient::send_request`] gives up and rotates: 3s → 6s →
/// 12s, each rounded up to whole [`REAP_INTERVAL`] sweeps (so, respectively,
/// 1/2/4 sweeps — a request filed just after a sweep needs one extra sweep to
/// clear its window, exactly like the original fixed reaper). Worst case
/// (filed right after a sweep, every attempt fully spent) sums to
/// 6s + 9s + 15s = 30s: a dead source is abandoned within ~30s, a busy mesh
/// gets three chances first.
pub const RETRY_WINDOWS: [Duration; 3] = [
    Duration::from_secs(3),
    Duration::from_secs(6),
    Duration::from_secs(12),
];

/// how many reaper-timeout occurrences a source must accumulate before the
/// latched `warn!` fires again after its first line — never one line per
/// timeout under a sustained busy mesh (CLAUDE.md: a forever-retry loop logs
/// attempt 1, then every Nth, carrying an `attempts` field).
const TIMEOUT_WARN_EVERY: u64 = 20;

/// answers that reached this process with no pending request to complete —
/// the request was reaped before the source answered it. Latched on the same
/// stride as [`TIMEOUT_WARN_EVERY`]. It exists because the silent version of
/// this drop is indistinguishable, from both ends, from a mesh that never
/// carried the request: the client says "unanswered", the server's serve
/// counters say "answered", and nothing names the gap between them.
static ANSWERS_AFTER_REAP: AtomicU64 = AtomicU64::new(0);

/// a retry window's survival in whole reaper sweeps, rounded up so a window
/// shorter than [`REAP_INTERVAL`] still survives at least one sweep.
fn window_ticks(window: Duration) -> u64 {
    window.as_secs().div_ceil(REAP_INTERVAL.as_secs()).max(1)
}

/// sink for inbound frames whose rpc id matches no pending request: another
/// owner may multiplex its own id space over the same channel (id spaces are
/// disjoint by construction — this client's ids are small and sequential, a
/// co-owner picks a range that can't collide). the hook gets the raw
/// `(id, body)`; the kernel stays payload-agnostic.
pub type UnmatchedFrameHook = Arc<dyn Fn(u64, &[u8]) + Send + Sync>;

/// a pending request: the reply slot, the reaper tick it was filed under, and
/// how many whole sweeps THIS attempt survives before the reaper reaps it
/// (its [`RETRY_WINDOWS`] entry, in [`window_ticks`]).
struct PendingEntry {
    reply: oneshot::Sender<Vec<u8>>,
    filed_at_tick: u64,
    survive_ticks: u64,
}

/// the dispatch task's lane-reclaim seam: fire `stop` and the task hands the
/// p2p receiver back over `handback`, then exits. built by
/// [`LaneReclaim::arm`]; the caller keeps the returned trigger halves and
/// passes the seam into [`P2pSyncClient::with_sources`]. reclaim is only
/// sound once the caller has quiesced its own requests — any frame the
/// revoked `recv()` had in flight is dropped, which with an empty pending map
/// and no unmatched hook is exactly what the dispatch loop would have done.
pub struct LaneReclaim<R> {
    stop: oneshot::Receiver<()>,
    handback: oneshot::Sender<R>,
}

impl<R> LaneReclaim<R> {
    /// build the seam: the client-side halves (trigger, receiver-return) and
    /// the task-side seam itself.
    pub fn arm() -> (oneshot::Sender<()>, oneshot::Receiver<R>, Self) {
        let (stop_tx, stop_rx) = oneshot::channel();
        let (handback_tx, handback_rx) = oneshot::channel();
        (
            stop_tx,
            handback_rx,
            Self {
                stop: stop_rx,
                handback: handback_tx,
            },
        )
    }
}

struct Shared {
    pending: Mutex<HashMap<u64, PendingEntry>>,
    /// monotonically increasing reaper tick (written only by the reaper).
    tick: AtomicU64,
    next_id: AtomicU64,
}

/// The cursor, membership, and timeout counts change under one lock so a
/// replacement cannot overwrite a concurrent source rotation.
struct SourceSet<P> {
    candidates: Vec<P>,
    cursor: usize,
    /// see the module doc's BUSY-MESH RETRY.
    timeout_counts: Vec<u64>,
}

/// the rotating candidate source set (see the module doc's SOURCE ROTATION).
struct Sources<P> {
    set: std::sync::RwLock<SourceSet<P>>,
}

impl<P: Clone + PartialEq> Sources<P> {
    fn new(candidates: Vec<P>, cursor: usize) -> Self {
        let timeout_counts = vec![0; candidates.len()];
        Self {
            set: std::sync::RwLock::new(SourceSet {
                candidates,
                cursor,
                timeout_counts,
            }),
        }
    }

    /// the RAW cursor and the candidate it selects. the token is the raw
    /// counter, never the modular index: [`Sources::advance_past`] compares
    /// it against the same raw counter, so folding the wrap in here would
    /// freeze the rotation on index 0 for the process's life the moment the
    /// cursor passed `len`.
    fn current(&self) -> (usize, P) {
        let set = self.set.read().expect("sources poisoned");
        let raw = set.cursor;
        (raw, set.candidates[raw % set.candidates.len()].clone())
    }

    /// may `peer` complete a request filed by this client?
    fn is_candidate(&self, peer: &P) -> bool {
        self.set
            .read()
            .expect("sources poisoned")
            .candidates
            .contains(peer)
    }

    /// swap the candidate list for the CURRENT membership. an empty list is
    /// ignored (the seed set stays; `current()` indexes unconditionally).
    ///
    /// Every changed membership advances the token, including replacement
    /// of the selected peer at the same index. A request from the old set
    /// cannot rotate a new source that never failed. Identical membership
    /// reads preserve the token and timeout counts.
    fn replace(&self, candidates: Vec<P>) {
        if candidates.is_empty() {
            return;
        }
        let mut set = self.set.write().expect("sources poisoned");
        if set.candidates == candidates {
            return;
        }
        let raw = set.cursor;
        let serving = set.candidates[raw % set.candidates.len()].clone();
        let wanted = candidates.iter().position(|k| *k == serving).unwrap_or(0);
        let len = candidates.len();
        let next = raw + 1;
        set.cursor = next + (wanted + len - next % len) % len;
        set.timeout_counts = candidates
            .iter()
            .map(|candidate| {
                set.candidates
                    .iter()
                    .position(|existing| existing == candidate)
                    .map_or(0, |index| set.timeout_counts[index])
            })
            .collect();
        set.candidates = candidates;
    }

    /// advance past the source at `observed` (a RAW cursor token from
    /// [`Sources::current`]) — exactly once per failure wave: a concurrent
    /// request that saw the same source fail leaves the cursor where the
    /// first advance put it.
    fn advance_past(&self, observed: usize) {
        let mut set = self.set.write().expect("sources poisoned");
        if set.cursor == observed {
            set.cursor += 1;
        }
    }

    fn advance(&self) {
        let mut set = self.set.write().expect("sources poisoned");
        set.cursor += 1;
    }

    /// Count the peer that timed out, even if its position has changed.
    /// Retired peers cannot charge timeouts to their replacements.
    fn record_timeout(&self, peer: &P) -> Option<u64> {
        let mut set = self.set.write().expect("sources poisoned");
        let slot = set
            .candidates
            .iter()
            .position(|candidate| candidate == peer)?;
        set.timeout_counts[slot] += 1;
        Some(set.timeout_counts[slot])
    }
}

/// a [`SyncClient`] whose requests cross a real p2p channel to one of a
/// candidate set of serving peers, failing over on transport failure.
pub struct P2pSyncClient<S: Sender> {
    sender: S,
    sources: Arc<Sources<S::PublicKey>>,
    shared: Arc<Shared>,
    /// the caller's real-key standing proof, signed ONCE via
    /// [`crate::sign_sync_proof`] and attached to every request. the server
    /// verifies it against committed standing and fail-closes on a mismatch.
    requester: [u8; 32],
    proof: [u8; 64],
}

impl<S: Sender> Clone for P2pSyncClient<S> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            sources: Arc::clone(&self.sources),
            shared: Arc::clone(&self.shared),
            requester: self.requester,
            proof: self.proof,
        }
    }
}

impl<S> P2pSyncClient<S>
where
    S: Sender,
{
    /// bind a client to one `server` — [`P2pSyncClient::with_sources`] with a
    /// single candidate. `requester`/`proof` are the caller's standing proof
    /// ([`crate::sign_sync_proof`]).
    pub fn new<E, R>(
        context: E,
        sender: S,
        receiver: R,
        server: S::PublicKey,
        requester: [u8; 32],
        proof: [u8; 64],
    ) -> Self
    where
        E: Spawner + Clock + Send + 'static,
        R: Receiver<PublicKey = S::PublicKey> + Send + 'static,
    {
        Self::with_sources(
            context,
            sender,
            receiver,
            vec![server],
            None,
            requester,
            proof,
            None,
        )
    }

    /// bind a client to a non-empty ordered candidate set over a registered
    /// channel pair, spawning the dispatch + reaper task on `context`
    /// (consumed — contexts are move-only). requests go to the cursor's
    /// candidate; a transport failure advances the cursor. `unmatched`
    /// receives frames whose id belongs to no pending request here (see
    /// [`UnmatchedFrameHook`]); `None` keeps the old drop-on-miss behavior.
    /// `reclaim` (see [`LaneReclaim`]) lets the caller revoke the dispatch
    /// task and take the receiver back; `None` = the lane is the task's for
    /// the process's life.
    #[allow(clippy::too_many_arguments)]
    pub fn with_sources<E, R>(
        context: E,
        sender: S,
        mut receiver: R,
        candidates: Vec<S::PublicKey>,
        unmatched: Option<UnmatchedFrameHook>,
        requester: [u8; 32],
        proof: [u8; 64],
        reclaim: Option<LaneReclaim<R>>,
    ) -> Self
    where
        E: Spawner + Clock + Send + 'static,
        R: Receiver<PublicKey = S::PublicKey> + Send + 'static,
    {
        assert!(!candidates.is_empty(), "at least one sync source");
        let sources = Arc::new(Sources::new(candidates, 0));
        let shared = Arc::new(Shared {
            pending: Mutex::new(HashMap::new()),
            tick: AtomicU64::new(0),
            next_id: AtomicU64::new(0),
        });
        let task_shared = Arc::clone(&shared);
        let reap_shared = Arc::clone(&shared);
        let expected = Arc::clone(&sources);
        // ONE loop shape whether or not a reclaim seam was armed: an unarmed
        // client parks a never-firing stop (its trigger rides in the task, so
        // the oneshot can't cancel) and the handback goes nowhere.
        let (stop_rx, handback_tx, _hold_open) = match reclaim {
            Some(LaneReclaim { stop, handback }) => (stop, handback, None),
            None => {
                let (hold, stop) = oneshot::channel::<()>();
                let (handback, _dropped) = oneshot::channel::<R>();
                (stop, handback, Some(hold))
            }
        };
        context.spawn(move |ctx| async move {
            let _hold_open = _hold_open;
            let mut stop_rx = stop_rx;
            // the REAPER runs as its own task: the dispatch loop below stays
            // a bare `recv().await` loop in steady state — select-dropping an
            // actor-backed p2p recv future mid-flight can eat a delivered
            // message, which here would silently turn a served response into
            // a client timeout. the stop arm only ever fires at reclaim,
            // whose contract (see [`LaneReclaim`]) requires quiesced
            // requests, so the one drop it can cause loses nothing a live
            // request was owed.
            ctx.spawn(move |reap_ctx| async move {
                loop {
                    reap_ctx.sleep(REAP_INTERVAL).await;
                    // an entry whose attempt-specific survival window
                    // (`survive_ticks`, one grace sweep added — a request
                    // filed just after a sweep still needs a whole extra
                    // sweep to clear it) has elapsed never got its reply —
                    // dropping its sender fails the caller's await so it can
                    // retry (see the module doc's BUSY-MESH RETRY).
                    let now = reap_shared.tick.fetch_add(1, Ordering::Relaxed) + 1;
                    reap_shared
                        .pending
                        .lock()
                        .expect("pending poisoned")
                        .retain(|_, e| now.saturating_sub(e.filed_at_tick) < e.survive_ticks + 1);
                }
            });
            loop {
                // the recv borrow must end before the receiver can be handed
                // back, so the stop arm resolves to `None` and the handback
                // happens outside the select expression.
                let frame = futures::select_biased! {
                    _ = stop_rx => None,
                    frame = futures::FutureExt::fuse(receiver.recv()) => Some(frame),
                };
                let Some(frame) = frame else {
                    let _ = handback_tx.send(receiver);
                    return;
                };
                let Ok((peer, msg)) = frame else { break };
                // only a candidate source may complete requests.
                if !expected.is_candidate(&peer) {
                    continue;
                }
                let bytes: Vec<u8> = msg.into();
                // replies ride the same authed frame; the auth fields are
                // server zero-fill (the transport-peer check above is the
                // reply's authenticity), so only id + body matter here.
                let Ok((_requester, _proof, id, body)) = decode_rpc(&bytes) else {
                    continue;
                };
                let waiter = task_shared
                    .pending
                    .lock()
                    .expect("pending poisoned")
                    .remove(&id);
                if let Some(entry) = waiter {
                    let _ = entry.reply.send(body.to_vec());
                } else if let Some(hook) = &unmatched {
                    hook(id, body);
                } else {
                    let occurrences = ANSWERS_AFTER_REAP.fetch_add(1, Ordering::Relaxed) + 1;
                    if occurrences == 1 || occurrences.is_multiple_of(TIMEOUT_WARN_EVERY) {
                        debug!(
                            target: "ducktape::statesync",
                            reason = "answer_after_reap",
                            id,
                            occurrences,
                            "a statesync answer arrived with no request left to complete — \
                             this source is slower than the retry window it was given",
                        );
                    }
                }
            }
            // channel closed: drop every waiter so requests fail instead of
            // hanging forever.
            task_shared
                .pending
                .lock()
                .expect("pending poisoned")
                .clear();
        });
        Self {
            sender,
            sources,
            shared,
            requester,
            proof,
        }
    }

    /// a clone of the underlying channel sender — the promotion seam pairs it
    /// with the receiver a [`LaneReclaim`] hands back.
    pub fn lane_sender(&self) -> S {
        self.sender.clone()
    }
}

impl<S> P2pSyncClient<S>
where
    S: Sender,
{
    /// The candidate selected for the next request. Operational surfaces use
    /// this after a successful request so source rotation stays visible.
    ///
    /// NEVER attribute an answer to it: the cursor is an `Arc` shared with
    /// every clone of this client, and any lane holding a clone (the forge
    /// pack sweeper's forever loop, a blob fetch) rotates it the moment its
    /// own request fails — so a read taken after an await can name a peer
    /// that answered nothing. A caller that puts a peer's NAME on what the
    /// answer said must take the name from its own request
    /// ([`P2pSyncClient::fetch_tip_coords`]).
    pub fn current_source(&self) -> S::PublicKey {
        self.sources.current().1
    }

    /// the tip's coordinates AND the peer this client asked for them — the
    /// detection lane's fetch, for a caller that ATTRIBUTES the answer (a
    /// build stamp on the peers surface, a named skew) rather than only
    /// consuming it. see [`P2pSyncClient::current_source`] for why the
    /// attribution rides the request instead.
    pub async fn fetch_tip_coords(&self) -> Result<(TipCoords, S::PublicKey), SyncError> {
        let (response, from) = self.send_request(SyncRequest::TipCoords).await?;
        match response {
            SyncResponse::TipCoords(coords) => Ok((coords, from)),
            SyncResponse::Error(e) => Err(SyncError::Server(e)),
            other => Err(SyncError::UnexpectedResponse(other.kind_name())),
        }
    }

    /// advance the serving cursor one candidate. the transport rotates on
    /// FAILURE by itself ([`Sources::advance_past`], wave-deduped); this is
    /// for callers whose retry policy also rotates on an HONEST answer — a
    /// blob fetch's "don't have it" is a valid response the failure path
    /// never sees.
    pub fn advance_source(&self) {
        self.sources.advance();
    }

    /// point this client (and every clone of it — the candidate set is shared)
    /// at the CURRENT membership. the set a client is built with is the
    /// GENESIS one, which goes stale the moment a founder is rotated out; the
    /// caller refreshes it from the tip it just read. an empty list is
    /// ignored, and the peer being served right now keeps being served.
    pub fn set_sources(&self, candidates: Vec<S::PublicKey>) {
        self.sources.replace(candidates);
    }
}

impl<S> P2pSyncClient<S>
where
    S: Sender,
{
    /// one request/response round trip, reporting the peer it was ADDRESSED
    /// to beside the answer. the whole transport body lives here;
    /// [`SyncClient::request`] is this with the peer dropped.
    /// one logical request, riding up to [`RETRY_WINDOWS`] attempts against
    /// the SAME source (see the module doc's BUSY-MESH RETRY) before
    /// surfacing a timeout and letting the caller's rotation move on.
    async fn send_request(
        &self,
        req: SyncRequest,
    ) -> Result<(SyncResponse, S::PublicKey), SyncError> {
        let sources = &self.sources;
        let shared = &self.shared;
        let (at, server) = sources.current();
        let kind = req.kind_name();
        let body = encode_request(&req);

        // ONE id for the whole logical request, re-filed by every attempt.
        // A server slower than an attempt's window answers the id it was
        // ASKED, which is this one — a fresh id per attempt would make that
        // answer match nothing in `pending`, and the dispatch loop drops an
        // unmatched frame. The caller then re-asks, which only lengthens the
        // server's queue, which makes the next answer later still: the retry
        // feeds the very lateness it is retrying, and a resident never
        // converges while the validator answers every request it sends.
        let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
        let mut attempt = 0usize;
        loop {
            let window = RETRY_WINDOWS[attempt];
            let mut sender = self.sender.clone();
            let (tx, rx) = oneshot::channel();
            {
                let mut pending = shared.pending.lock().expect("pending poisoned");
                pending.insert(
                    id,
                    PendingEntry {
                        reply: tx,
                        filed_at_tick: shared.tick.load(Ordering::Relaxed),
                        survive_ticks: window_ticks(window),
                    },
                );
            }
            let frame = encode_rpc(&self.requester, &self.proof, id, &body);
            let attempted = sender.send(Recipients::One(server.clone()), IoBuf::from(frame), false);
            if attempted.is_empty() {
                // the source is offline/unreachable, rate-limited, or its
                // mailbox refused the send under local backpressure right
                // now — fail fast instead of waiting out the reaper, and
                // rotate. this is a distinct, immediately-visible drop (see
                // the module doc), not the reaper's "sent but unanswered"
                // case below, so it does not retry against the same source.
                shared.pending.lock().expect("pending poisoned").remove(&id);
                debug!(
                    target: "ducktape::statesync",
                    reason = "send_rejected",
                    kind,
                    attempt = attempt + 1,
                    "statesync request send accepted no recipients (rate-limited, closed sender, or local backpressure)",
                );
                sources.advance_past(at);
                return Err(SyncError::Transport(
                    "sync source unreachable (send attempted no recipients)".into(),
                ));
            }
            // resolves when the response routes back — or errs when the
            // reaper drops the slot (dropped send / dead server), retrying
            // against the same source before rotating.
            match rx.await {
                Ok(bytes) => return Ok((decode_response(&bytes)?, server)),
                Err(_) => {
                    let occurrences = sources.record_timeout(&server);
                    let latched = occurrences
                        .filter(|count| *count == 1 || count.is_multiple_of(TIMEOUT_WARN_EVERY));
                    if let Some(occurrences) = latched {
                        warn!(
                            target: "ducktape::statesync",
                            reason = "request_timeout",
                            kind,
                            attempt = attempt + 1,
                            attempts_total = RETRY_WINDOWS.len(),
                            occurrences,
                            "statesync request went unanswered within its window (the mesh ACCEPTED the send — the source is slow or dead)",
                        );
                    }
                    attempt += 1;
                    if attempt == RETRY_WINDOWS.len() {
                        sources.advance_past(at);
                        return Err(SyncError::Transport(format!(
                            "request {id} went unanswered after {attempt} attempts (the mesh accepted every send — the source is slow or dead)"
                        )));
                    }
                }
            }
        }
    }
}

impl<S> SyncClient for P2pSyncClient<S>
where
    S: Sender,
{
    fn request(
        &self,
        req: SyncRequest,
    ) -> impl std::future::Future<Output = Result<SyncResponse, SyncError>> + Send {
        // owned so the future stays `'static`, exactly as it was when this
        // body lived here.
        let this = self.clone();
        async move { this.send_request(req).await.map(|(response, _)| response) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_wave_advances_the_cursor_exactly_once() {
        let sources = Sources::new(vec!["a", "b", "c"], 0);
        let (at, first) = sources.current();
        assert_eq!((at, first), (0, "a"));

        // three concurrent requests all observed source 0 fail — one advance.
        sources.advance_past(at);
        sources.advance_past(at);
        sources.advance_past(at);
        assert_eq!(sources.current().1, "b");

        // the list wraps: a dead tail rotates back to the head.
        sources.advance_past(1);
        assert_eq!(sources.current().1, "c");
        sources.advance_past(2);
        assert_eq!(sources.current().1, "a");

        // and it keeps rotating PAST the wrap: the token stays the raw
        // counter (3), so the fourth failure lands on "b" rather than
        // freezing the cursor on the head forever.
        let (at, wrapped) = sources.current();
        assert_eq!((at, wrapped), (3, "a"));
        sources.advance_past(at);
        assert_eq!(sources.current(), (4, "b"));
    }

    #[test]
    fn an_out_of_band_bump_does_not_freeze_the_rotation() {
        // `advance_source` bumps the same raw cursor from another lane; the
        // failure path must still rotate from wherever it left it.
        let sources = Sources::new(vec!["a", "b", "c"], 7);
        let (at, server) = sources.current();
        assert_eq!((at, server), (7, "b"));
        sources.advance_past(at);
        assert_eq!(sources.current().1, "c");
    }

    #[test]
    fn record_timeout_latches_first_then_every_nth() {
        let sources = Sources::new(vec!["a", "b"], 0);
        // the first occurrence always latches (the caller's `occurrences ==
        // 1` check).
        assert_eq!(sources.record_timeout(&"a"), Some(1));
        // 2..TIMEOUT_WARN_EVERY are silent — the caller's `% N == 0` check
        // is false for all of them — then the Nth relatches.
        for expected in 2..TIMEOUT_WARN_EVERY {
            let occurrences = sources.record_timeout(&"a").unwrap();
            assert_eq!(occurrences, expected);
            assert!(!occurrences.is_multiple_of(TIMEOUT_WARN_EVERY));
        }
        let latched_again = sources.record_timeout(&"a").unwrap();
        assert_eq!(latched_again, TIMEOUT_WARN_EVERY);
        assert!(latched_again.is_multiple_of(TIMEOUT_WARN_EVERY));
        // a different source's counter is independent.
        assert_eq!(sources.record_timeout(&"b"), Some(1));
    }

    #[test]
    fn a_removed_source_cannot_rotate_its_same_position_replacement() {
        let sources = Sources::new(vec!["a", "b"], 0);
        let (old, _) = sources.current();
        sources.replace(vec!["c", "d"]);
        let replacement = sources.current();
        assert_eq!(replacement.1, "c");
        assert!(replacement.0 > old);
        sources.advance_past(old);
        assert_eq!(sources.current(), replacement);
    }

    #[test]
    fn repeated_membership_read_preserves_timeout_latching() {
        let sources = Sources::new(vec!["a", "b"], 0);
        assert_eq!(sources.record_timeout(&"a"), Some(1));
        sources.replace(vec!["a", "b"]);
        assert_eq!(sources.current(), (0, "a"));
        assert_eq!(sources.record_timeout(&"a"), Some(2));
    }

    #[test]
    fn membership_changes_keep_timeout_counts_with_the_actual_peer() {
        let sources = Sources::new(vec!["a", "b"], 0);
        assert_eq!(sources.record_timeout(&"a"), Some(1));
        sources.replace(vec!["b", "a", "c"]);
        assert_eq!(sources.record_timeout(&"a"), Some(2));
        assert_eq!(sources.record_timeout(&"b"), Some(1));
        sources.replace(vec!["b", "c"]);
        assert_eq!(sources.record_timeout(&"a"), None);
        assert_eq!(sources.record_timeout(&"b"), Some(2));
    }

    #[test]
    fn replacing_the_candidates_keeps_the_cursor_serving_and_monotonic() {
        let sources = Sources::new(vec!["a", "b", "c"], 7);
        let (before, serving) = sources.current();
        assert_eq!((before, serving), (7, "b"));

        // the membership rotated: "a" is gone, "d" was promoted. the peer we
        // were serving from survives, so it keeps serving — and the raw
        // cursor never moved backwards.
        sources.replace(vec!["b", "c", "d"]);
        let (after, still_serving) = sources.current();
        assert_eq!(still_serving, "b");
        assert!(after > before);
        // rotation still works off the fresh token, and only off it: the
        // stale pre-swap token can no longer rotate anyone.
        sources.advance_past(before);
        assert_eq!(sources.current().1, "b");
        sources.advance_past(after);
        assert_eq!(sources.current().1, "c");

        // the serving peer is dropped by the swap: the head takes over.
        sources.replace(vec!["d", "e"]);
        assert_eq!(sources.current().1, "d");
        assert_eq!(sources.record_timeout(&"e"), Some(1));
        // an empty membership reading is ignored, never a panic-on-index.
        sources.replace(vec![]);
        assert_eq!(sources.current().1, "d");
        // only a candidate may complete a request.
        assert!(sources.is_candidate(&"e"));
        assert!(!sources.is_candidate(&"a"));
    }

    // ========================================================================
    // reaper integration: a real `P2pSyncClient` over commonware's
    // deterministic simulated mesh network — the same rig `tests/
    // binding_parity.rs`'s mesh leg uses, so drops are genuine transport
    // drops (the server simply never answers), not a fake in-process stand-in.
    // ========================================================================

    use commonware_cryptography::ed25519;

    const TEST_CHANNEL: u64 = 0;

    fn zero_tip_coords() -> TipCoords {
        TipCoords {
            height: 0,
            root_hash: sdk::StateRoot::ZERO,
            epoch: 0,
            view_base: 0,
            participants: Vec::new(),
            residents: Vec::new(),
            has_floor: false,
            generation: 0,
            mesh_window: Vec::new(),
            build: None,
        }
    }

    /// stand up a two-peer deterministic mesh (`server`, `joiner`) on one
    /// registered channel and hand back both sides' sender/receiver halves.
    async fn mesh_pair(
        context: &commonware_runtime::deterministic::Context,
    ) -> (
        ed25519::PublicKey,
        (
            impl Sender<PublicKey = ed25519::PublicKey>,
            impl Receiver<PublicKey = ed25519::PublicKey>,
        ),
        (
            impl Sender<PublicKey = ed25519::PublicKey>,
            impl Receiver<PublicKey = ed25519::PublicKey>,
        ),
    ) {
        use commonware_cryptography::Signer as _;
        use commonware_p2p::simulated::{self, Link};
        use commonware_runtime::Supervisor as _;
        use commonware_utils::{NZU32, NZUsize};

        let server = ed25519::PrivateKey::from_seed(101).public_key();
        let joiner = ed25519::PrivateKey::from_seed(102).public_key();

        let (network, oracle) = simulated::Network::new_with_peers(
            context.child("network"),
            simulated::Config {
                max_size: 1024 * 1024,
                disconnect_on_block: true,
                max_peers_per_set: NZUsize!(32),
                tracked_peer_sets: NZUsize!(1),
            },
            vec![server.clone(), joiner.clone()],
        )
        .await;
        network.start();

        let link = Link {
            latency: Duration::from_millis(2),
            jitter: Duration::from_millis(0),
            success_rate: commonware_utils::probability!(1.0),
        };
        oracle
            .add_link(server.clone(), joiner.clone(), link.clone())
            .await
            .expect("link server -> joiner");
        oracle
            .add_link(joiner.clone(), server.clone(), link)
            .await
            .expect("link joiner -> server");

        let quota = commonware_runtime::Quota::per_second(NZU32!(128));
        let server_side = oracle
            .control(server.clone())
            .register(TEST_CHANNEL, quota)
            .await
            .expect("server channel registration");
        let joiner_side = oracle
            .control(joiner.clone())
            .register(TEST_CHANNEL, quota)
            .await
            .expect("joiner channel registration");

        (server, server_side, joiner_side)
    }

    #[test]
    fn retry_survives_two_dropped_sends_without_rotation() {
        use commonware_runtime::{Runner as _, Spawner as _, Supervisor as _, deterministic};

        deterministic::Runner::timed(Duration::from_secs(60)).start(|context| async move {
            let (server, (mut server_tx, mut server_rx), (joiner_tx, joiner_rx)) =
                mesh_pair(&context).await;

            // the first two requests the server receives are silently
            // dropped (never answered) — exactly the busy-mesh shape the
            // reaper must survive without rotating; the third is answered.
            context.child("serve").spawn(move |_ctx| async move {
                let mut seen = 0u32;
                while let Ok((peer, msg)) = server_rx.recv().await {
                    seen += 1;
                    if seen < 3 {
                        continue;
                    }
                    let bytes: Vec<u8> = msg.into();
                    let Ok((_requester, _proof, id, _body)) = decode_rpc(&bytes) else {
                        continue;
                    };
                    let resp = crate::encode_response(&SyncResponse::TipCoords(zero_tip_coords()));
                    let _ = server_tx.send(
                        Recipients::One(peer),
                        IoBuf::from(encode_rpc(&[0u8; 32], &[0u8; 64], id, &resp)),
                        false,
                    );
                    return;
                }
            });

            let client = P2pSyncClient::new(
                context.child("client"),
                joiner_tx,
                joiner_rx,
                server,
                [0u8; 32],
                [0u8; 64],
            );
            let before = client.current_source();
            let result = client.request(SyncRequest::TipCoords).await;
            assert!(
                result.is_ok(),
                "the third attempt against the same source must succeed: {result:?}"
            );
            assert_eq!(
                client.current_source(),
                before,
                "two dropped attempts must not rotate the source — only the \
                 final surfaced timeout may"
            );
        });
    }

    #[test]
    fn a_dead_source_surfaces_timeout_within_the_bounded_budget() {
        use commonware_runtime::{
            Clock as _, Runner as _, Spawner as _, Supervisor as _, deterministic,
        };

        deterministic::Runner::timed(Duration::from_secs(60)).start(|context| async move {
            let (server, (_server_tx, mut server_rx), (joiner_tx, joiner_rx)) =
                mesh_pair(&context).await;

            // a genuinely dead source: every request lands and nothing ever
            // answers.
            context
                .child("serve")
                .spawn(move |_ctx| async move { while server_rx.recv().await.is_ok() {} });

            let client = P2pSyncClient::new(
                context.child("client"),
                joiner_tx,
                joiner_rx,
                server,
                [0u8; 32],
                [0u8; 64],
            );
            let started = context.current();
            let result = client.request(SyncRequest::TipCoords).await;
            let elapsed = context
                .current()
                .duration_since(started)
                .unwrap_or_default();

            assert!(result.is_err(), "a dead source must eventually time out");
            // worst case per attempt is (ticks+1) sweeps; summed across all
            // three attempts that is exactly 30s (see `RETRY_WINDOWS`'s
            // doc) — plus a small allowance for the link's own latency.
            let bound = RETRY_WINDOWS.iter().map(|w| w.as_secs()).sum::<u64>()
                + RETRY_WINDOWS.len() as u64 * REAP_INTERVAL.as_secs();
            let budget = Duration::from_secs(bound) + Duration::from_millis(100);
            assert!(
                elapsed <= budget,
                "abandonment must stay bounded: took {elapsed:?}, budget {budget:?}"
            );
        });
    }

    #[test]
    fn one_late_answer_completes_the_attempt_that_replaced_the_reaped_one() {
        use commonware_runtime::{
            Clock as _, Runner as _, Spawner as _, Supervisor as _, deterministic,
        };

        // past the first attempt's survival (3s window = 2 sweeps = 6s) and
        // inside the second's (6s window = 3 sweeps), so the answer lands
        // while attempt 2 is the outstanding one.
        const LATE: Duration = Duration::from_secs(9);

        deterministic::Runner::timed(Duration::from_secs(60)).start(|context| async move {
            let (server, (mut server_tx, mut server_rx), (joiner_tx, joiner_rx)) =
                mesh_pair(&context).await;

            // a SLOW source, not a dead one: it answers the FIRST request it
            // is asked, once, after that attempt has already been reaped —
            // the shape of a validator whose serve lane is queued behind a
            // busy consensus loop. It never answers the re-sends, so the only
            // way this request can complete is if the re-sends carry the id
            // the server was asked.
            context.child("serve").spawn(move |ctx| async move {
                let Ok((peer, msg)) = server_rx.recv().await else {
                    return;
                };
                let bytes: Vec<u8> = msg.into();
                let Ok((_requester, _proof, id, _body)) = decode_rpc(&bytes) else {
                    return;
                };
                ctx.sleep(LATE).await;
                let resp = crate::encode_response(&SyncResponse::TipCoords(zero_tip_coords()));
                let _ = server_tx.send(
                    Recipients::One(peer),
                    IoBuf::from(encode_rpc(&[0u8; 32], &[0u8; 64], id, &resp)),
                    false,
                );
            });

            let client = P2pSyncClient::new(
                context.child("client"),
                joiner_tx,
                joiner_rx,
                server,
                [0u8; 32],
                [0u8; 64],
            );
            let started = context.current();
            let result = client.request(SyncRequest::TipCoords).await;
            let elapsed = context
                .current()
                .duration_since(started)
                .unwrap_or_default();

            assert!(
                result.is_ok(),
                "a source slower than one window still ANSWERED — the retry \
                 must be able to consume that answer, not discard it and \
                 re-ask forever: {result:?}"
            );
            assert!(
                elapsed < Duration::from_secs(RETRY_WINDOWS.iter().map(|w| w.as_secs()).sum()),
                "the answer must complete the request when it lands, not \
                 after the whole retry budget: took {elapsed:?}"
            );
        });
    }
}
