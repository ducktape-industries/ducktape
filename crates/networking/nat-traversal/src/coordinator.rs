use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::Arc;

use arrayvec::ArrayVec;
use commonware_codec::DecodeExt as _;
use commonware_cryptography::ed25519;
use lru::LruCache;

use crate::AuthRequest;
use crate::advert::{
    Admission, AdmitEvent, AdvertBook, AdvertOutcome, MAX_ADVERTS, MAX_ADVERTS_PER_SOURCE_IP,
    SharedAdverts,
};
use crate::auth::{AuthPolicy, CookieKey, DEFAULT_FRESHNESS_WINDOW_SECS, verify_request_using};
use crate::{Latch, Msg, NodeKey, short_key};

const AUTH_KEY_CACHE_SIZE: NonZeroUsize = NonZeroUsize::new(64).unwrap();
type AuthKeyCache = Option<LruCache<NodeKey, ed25519::PublicKey>>;

/// Small LRU for parsed ed25519 caller keys. Parsing a valid key is roughly a
/// tenth of request verification cost. It allocates lazily, and the
/// library-enforced capacity keeps attacker-selected callers from growing
/// memory without bound.
fn resolve_auth_key(cache: &mut AuthKeyCache, key: NodeKey) -> Option<ed25519::PublicKey> {
    if let Some(cached) = cache.as_mut().and_then(|entries| entries.get(&key)) {
        return Some(cached.clone());
    }

    let parsed = ed25519::PublicKey::decode(key.0.as_slice()).ok()?;
    cache
        .get_or_insert_with(|| LruCache::new(AUTH_KEY_CACHE_SIZE))
        .put(key, parsed.clone());
    Some(parsed)
}

/// Emit the log line for an [`AdmitEvent`] latched by `AdvertBook::observe`
/// or `readvertise`. Callers MUST call this only after dropping the
/// `SharedAdverts` lock (`adverts.take_admit_event()` runs under it, but the
/// actual tracing write — I/O — never should).
fn log_admit_event(event: Option<AdmitEvent>) {
    match event {
        Some(AdmitEvent::SourceCapped {
            source,
            occurrences,
        }) => tracing::warn!(
            target: "ducktape::reachability",
            event = "advert_refused",
            reason = "advert_source_cap",
            source = %source,
            cap = MAX_ADVERTS_PER_SOURCE_IP,
            occurrences,
            "source IP at its per-source advert cap — new key refused"
        ),
        Some(AdmitEvent::BookFull { occurrences }) => tracing::warn!(
            target: "ducktape::reachability",
            event = "advert_evicted",
            reason = "book_full",
            capacity = MAX_ADVERTS,
            occurrences,
            "advert book at capacity — the newest registration lost its slot"
        ),
        None => {}
    }
}

pub type CoordinatorReply = (SocketAddr, Msg);

/// Allocation-free output from the coordinator's bounded request handler.
/// One request can produce at most three datagrams: the lookup response and a
/// `PunchSync` for each peer.
pub type CoordinatorReplies = ArrayVec<CoordinatorReply, 3>;

/// An authenticated request ready for the ordered rendezvous state machine.
/// Authentication workers produce this value; it contains public identity and
/// protocol data only.
pub(crate) struct VerifiedRequest {
    caller: NodeKey,
    inner: Msg,
}

/// Stateful verifier owned by either the inline coordinator or one fixed auth
/// worker. Its only state is policy plus a bounded cache of parsed PUBLIC keys.
pub(crate) struct AuthVerifier {
    policy: Arc<AuthPolicy>,
    auth_keys: AuthKeyCache,
    window: u64,
}

impl AuthVerifier {
    fn new(policy: AuthPolicy) -> Self {
        Self::with_shared_policy(Arc::new(policy))
    }

    pub(crate) fn with_shared_policy(policy: Arc<AuthPolicy>) -> Self {
        Self {
            policy,
            auth_keys: None,
            window: DEFAULT_FRESHNESS_WINDOW_SECS,
        }
    }

    pub(crate) fn verify(&mut self, req: AuthRequest, now: u64) -> Option<VerifiedRequest> {
        let inner_subject = req.inner.subject_key()?;
        // Self-ops may mutate only the authenticated caller's own advert;
        // Lookup intentionally names a different peer.
        let is_self_op = !matches!(req.inner, Msg::Lookup { .. });
        if is_self_op && inner_subject != req.caller {
            return None;
        }

        let inner_bytes = req.inner.encode_inline();
        // Authenticate the caller, never the peer named by Lookup.
        verify_request_using(
            &self.policy,
            now,
            self.window,
            req.caller,
            &inner_bytes,
            &req.auth,
            |caller| resolve_auth_key(&mut self.auth_keys, caller),
        )
        .ok()?;
        Some(VerifiedRequest {
            caller: req.caller,
            inner: req.inner,
        })
    }
}

/// The untrusted entry helper. Maps a node key to the reflexive address the
/// coordinator observed for it, and brokers a simultaneous-open. Holds no key
/// material, no plaintext, no mesh authority — and never carries peer traffic:
/// rendezvous only; the TCP relay lane (`crate::relay`) moves sealed bytes it
/// cannot read, resolving targets from this coordinator's shared book.
pub struct Coordinator {
    /// Behind [`SharedAdverts`] so the TCP relay lane can resolve targets from
    /// the SAME book the UDP rendezvous maintains. Lock scopes are tiny and
    /// never held across an await (every handler here is sync), and the UDP
    /// serving loops are single-threaded, so the lock is uncontended.
    adverts: SharedAdverts,
    auth: AuthVerifier,
    rejects: u64,
    /// Boot-random return-routability key: mints the cookie `BindResponse`
    /// hands out and verifies the one a first-seen/expired `Register` must
    /// echo. See [`CookieKey`].
    cookie_key: CookieKey,
}

impl Default for Coordinator {
    fn default() -> Self {
        Self {
            adverts: SharedAdverts::wrap(AdvertBook::default()),
            auth: AuthVerifier::new(AuthPolicy::default()),
            rejects: 0,
            cookie_key: CookieKey::boot_random(),
        }
    }
}

impl Coordinator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct with an explicit authorization policy.
    pub fn with_policy(policy: AuthPolicy) -> Self {
        Self::with_shared_policy(Arc::new(policy))
    }

    /// Construct over an already-shared policy — the seam for serving the SAME
    /// policy on both the UDP loops and the TCP relay lane without cloning the
    /// (possibly large) genesis set.
    pub fn with_shared_policy(policy: Arc<AuthPolicy>) -> Self {
        Self {
            adverts: SharedAdverts::wrap(AdvertBook::default()),
            auth: AuthVerifier::with_shared_policy(policy),
            rejects: 0,
            cookie_key: CookieKey::boot_random(),
        }
    }

    /// Construct with an explicit policy AND registration TTL (seconds). The
    /// default is [`crate::advert::REGISTRATION_TTL_SECS`]; tests and
    /// short-lived rigs shrink it.
    pub fn with_policy_and_ttl(policy: AuthPolicy, ttl_secs: u64) -> Self {
        Self {
            adverts: SharedAdverts::wrap(AdvertBook::with_ttl(ttl_secs)),
            ..Self::with_policy(policy)
        }
    }

    /// A cloneable read handle on this coordinator's advert book — what the
    /// TCP relay lane resolves targets through, so a relayed intro reaches the
    /// member exactly where the UDP rendezvous currently believes it lives.
    pub fn adverts(&self) -> SharedAdverts {
        self.adverts.clone()
    }

    /// The policy `Arc` this coordinator verifies against (shared with the
    /// worker pool by the `_using` serving seam in `client.rs`).
    pub(crate) fn shared_policy(&self) -> Arc<AuthPolicy> {
        self.auth.policy.clone()
    }

    /// Count of requests dropped by the auth gate (observability).
    pub fn rejects(&self) -> u64 {
        self.rejects
    }

    /// Authenticate then handle one authenticated request. `now` is wall-clock
    /// seconds. A failed authenticator produces NO reply and bumps the counter.
    pub fn handle_auth(
        &mut self,
        from: SocketAddr,
        req: AuthRequest,
        now: u64,
    ) -> Vec<(SocketAddr, Msg)> {
        self.handle_auth_replies(from, req, now)
            .into_iter()
            .collect()
    }

    /// Allocation-free authenticated handler used by the live UDP loop.
    pub fn handle_auth_replies(
        &mut self,
        from: SocketAddr,
        req: AuthRequest,
        now: u64,
    ) -> CoordinatorReplies {
        match self.auth.verify(req, now) {
            Some(req) => self.handle_verified_replies(from, req, now),
            None => {
                self.record_reject();
                CoordinatorReplies::new()
            }
        }
    }

    pub(crate) fn handle_verified_replies(
        &mut self,
        from: SocketAddr,
        req: VerifiedRequest,
        now: u64,
    ) -> CoordinatorReplies {
        self.handle_replies(req.caller, from, req.inner, now)
    }

    /// The auth gate dropped one request — the inline verifier's own refusal or
    /// a worker's. Unauthenticated traffic is exactly what an untrusted control
    /// port attracts, so this is latched: the count separates a misconfigured
    /// member from a flood.
    pub(crate) fn record_reject(&mut self) {
        self.rejects += 1;
        static REJECTS: Latch = Latch::new();
        if let Some(occurrences) = REJECTS.hit("unauthenticated_request") {
            tracing::warn!(
                target: "ducktape::reachability",
                event = "coordinator_request_refused",
                reason = "unauthenticated_request",
                occurrences,
                "request failed the coordinator auth gate"
            );
        }
    }

    /// The one gate shared by every write that could CREATE a live mapping —
    /// `Register`'s nonce-0 baseline and `Readvertise`'s rebind both call
    /// this before touching the book, so the return-routability rule lives
    /// in exactly one place instead of once per arm (and can't quietly drift
    /// out of sync between the two, which is exactly how `Readvertise` first
    /// slipped past it: `AdvertBook::readvertise` falls into the same
    /// `insert_fresh` `observe` does for a key with no live entry, but only
    /// `observe`'s caller was gated).
    ///
    /// A write against an EXISTING live mapping (a same-source refresh, or a
    /// nonce-superseding readvertise from the same source) needs no cookie:
    /// `observe`/`readvertise` already re-derive that liveness+source check
    /// themselves, so requiring a cookie there would add nothing.
    fn admit_write(&self, key: NodeKey, from: SocketAddr, now: u64, cookie: &[u8; 32]) -> bool {
        let creates_mapping = self.adverts.current(key, now).is_none();
        if !creates_mapping || self.cookie_key.verify(from, now, cookie) {
            return true;
        }
        static NO_COOKIE: Latch = Latch::new();
        if let Some(occurrences) = NO_COOKIE.hit("advert_missing_cookie") {
            tracing::warn!(
                target: "ducktape::reachability",
                event = "advert_refused",
                reason = "advert_missing_cookie",
                source = %from,
                key = short_key(key),
                occurrences,
                "a write that would create a live mapping is missing a valid return-routability cookie"
            );
        }
        false
    }

    /// Auth-bypassing seam for this module's own ordered-state tests, which
    /// exercise the pure handler without minting signatures.
    #[cfg(test)]
    pub(crate) fn handle_verified_at(
        &mut self,
        caller: NodeKey,
        from: SocketAddr,
        msg: Msg,
        now: u64,
    ) -> Vec<(SocketAddr, Msg)> {
        self.handle_replies(caller, from, msg, now)
            .into_iter()
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn handle_verified(
        &mut self,
        caller: NodeKey,
        from: SocketAddr,
        msg: Msg,
    ) -> Vec<(SocketAddr, Msg)> {
        self.handle_verified_at(caller, from, msg, 0)
    }

    /// The pure ordered state step. `caller` is the authenticated requesting
    /// identity, used both to bind a `Lookup` to the caller's own registered
    /// source before answering it, and for the peer-directed `PunchSync` so
    /// the passive side learns the caller's real key.
    fn handle_replies(
        &mut self,
        caller: NodeKey,
        from: SocketAddr,
        msg: Msg,
        now: u64,
    ) -> CoordinatorReplies {
        // Each arm below takes its own guard, scoped to the single book
        // operation it needs (this handler is sync, so nothing holds it
        // across an await): a call that never touches the book — like
        // `BindRequest` — never locks it, and every arm that does lock it
        // drops the guard before its tracing call, so a stalled log
        // consumer never parks a relay session's read-only resolution.
        match msg {
            Msg::BindRequest { .. } => {
                let cookie = self.cookie_key.mint(from, now);
                CoordinatorReplies::from_iter([(
                    from,
                    Msg::BindResponse {
                        reflexive: from,
                        cookie,
                    },
                )])
            }
            Msg::Register { key, cookie } => {
                if !self.admit_write(key, from, now, &cookie) {
                    return CoordinatorReplies::new();
                }
                // The registered reflexive address IS the observed source: the
                // coordinator never trusts a self-reported address.
                let Admission { outcome, event } = self.adverts.lock().observe(key, from, now);
                // The book is done with; a log write (stderr, and a node's
                // LogRing) must not happen under its lock.
                log_admit_event(event);
                match outcome {
                    AdvertOutcome::Superseded => tracing::debug!(
                        target: "ducktape::reachability",
                        event = "advert_registered",
                        key = short_key(key),
                        reflexive = %from,
                        "registered a member at its observed source"
                    ),
                    // the per-source cap already logged above via `admit_event`.
                    AdvertOutcome::Refused => {}
                    // a live mapping already ahead of this bare Register (a
                    // superseding nonce, or a different source): nothing
                    // changed, so nothing was "registered".
                    AdvertOutcome::NoOp => {}
                    AdvertOutcome::Stale => {}
                    // `observe` never returns this — only `readvertise` does.
                    AdvertOutcome::SourceMismatch => {}
                }
                CoordinatorReplies::new()
            }
            Msg::Readvertise {
                key,
                nonce,
                cookie,
            } => {
                // The wire-level rebind path AND the keepalive: a node re-runs
                // STUN and republishes its reflexive (the observed `from`, never
                // a self-reported address) under a strictly-higher `nonce`. The
                // `AdvertBook` staleness guard rejects an equal-or-lower nonce, so
                // a replayed/reordered datagram cannot supersede a fresh mapping
                // — nor extend its life. A key with no LIVE mapping (first-seen,
                // or expired) needs the SAME return-routability proof `Register`
                // does: `AdvertBook::readvertise` falls straight into
                // `insert_fresh` for one exactly like `observe` would, so a
                // spoofed `Readvertise{nonce: 1}` for an unknown key is just as
                // effective a reflector-planting primitive as a bare `Register`.
                if !self.admit_write(key, from, now, &cookie) {
                    return CoordinatorReplies::new();
                }
                let Admission { outcome, event } =
                    self.adverts.lock().readvertise(key, from, nonce, now);
                // The book is done with; a log write must not happen under
                // its lock.
                log_admit_event(event);
                match outcome {
                    // the 25 s keepalive of every member: per-frame traffic.
                    AdvertOutcome::Superseded => tracing::trace!(
                        target: "ducktape::reachability",
                        key = short_key(key),
                        reflexive = %from,
                        nonce,
                        "re-advertisement superseded the stored mapping"
                    ),
                    AdvertOutcome::Stale => tracing::debug!(
                        target: "ducktape::reachability",
                        event = "advert_refused",
                        reason = "stale_nonce",
                        key = short_key(key),
                        reflexive = %from,
                        nonce,
                        "re-advertisement did not beat the stored nonce"
                    ),
                    // an on-path replay of a captured keepalive from a
                    // different source: indistinguishable from a genuine
                    // rebind by nonce alone, so the live mapping stays put.
                    AdvertOutcome::SourceMismatch => tracing::debug!(
                        target: "ducktape::reachability",
                        event = "advert_refused",
                        reason = "source_mismatch",
                        key = short_key(key),
                        reflexive = %from,
                        nonce,
                        "re-advertisement source differs from the live mapping"
                    ),
                    // the per-source cap already logged above via `admit_event`.
                    AdvertOutcome::Refused => {}
                    // `readvertise` never returns this — only `observe` does.
                    AdvertOutcome::NoOp => {}
                }
                CoordinatorReplies::new()
            }
            Msg::Lookup { key } => {
                // `from` is the raw datagram source and `verify_request_using`
                // never binds it to `caller` — it only proves possession of
                // `caller`'s key. Answer a Lookup only when `caller` already
                // has a LIVE advert observed at exactly this source: the same
                // binding the Register/Readvertise `SourceMismatch` guard
                // enforces on the write side, reused here instead of adding a
                // second mechanism. Every legitimate caller registers before
                // it looks up (the client always `register`s on the socket it
                // then calls `lookup`/`send_lookup` from), so this costs
                // nothing on the real join/rendezvous path. Without it, a
                // spoofed `from` with a fresh, unregistered `caller` key turns
                // this arm into a three-datagram reflector aimed at whatever
                // address the attacker names.
                let caller_bound_here = self.adverts.lock().current(caller, now) == Some(from);
                if !caller_bound_here {
                    // Unauthenticated by definition — any key can claim any
                    // source until it has registered from one — so this is
                    // latched like every other refusal in this crate; the
                    // count is the diagnosis. Nothing is sent back: a reply to
                    // an unbound source IS the defect this guard closes.
                    static UNBOUND_LOOKUP: Latch = Latch::new();
                    if let Some(occurrences) = UNBOUND_LOOKUP.hit("lookup_source_unbound") {
                        tracing::warn!(
                            target: "ducktape::reachability",
                            event = "lookup_refused",
                            reason = "source_unbound",
                            caller = short_key(caller),
                            occurrences,
                            "lookup refused — caller has no live advert from this source"
                        );
                    }
                    return CoordinatorReplies::new();
                }

                // Decide under the lock, then drop it before anything else —
                // building the reply and the tracing call are both pure once
                // `target` is known, and a stalled log write must never park
                // a relay session's read-only resolution on this mutex.
                let target = self.adverts.lock().current(key, now);
                let response = (
                    from,
                    Msg::LookupResponse {
                        key,
                        reflexive: target,
                    },
                );
                let Some(peer_addr) = target else {
                    // the everyday reason a join stalls: the peer never
                    // registered, or its registration aged out of the book.
                    tracing::debug!(
                        target: "ducktape::reachability",
                        event = "lookup_unresolved",
                        reason = "target_unregistered",
                        key = short_key(key),
                        caller = short_key(caller),
                        "lookup answered None"
                    );
                    return CoordinatorReplies::from_iter([response]);
                };
                CoordinatorReplies::from([
                    response,
                    (
                        from,
                        Msg::PunchSync {
                            peer: key,
                            peer_reflexive: peer_addr,
                        },
                    ),
                    (
                        peer_addr,
                        Msg::PunchSync {
                            peer: caller,
                            peer_reflexive: from,
                        },
                    ),
                ])
            }
            // The coordinator never routes these through `handle`:
            // BindResponse/LookupResponse/PunchSync/Punch are node-directed.
            // Ignore defensively.
            Msg::BindResponse { .. }
            | Msg::LookupResponse { .. }
            | Msg::PunchSync { .. }
            | Msg::Punch { .. } => CoordinatorReplies::new(),
        }
    }

    /// Reachability-plane rebind re-advertisement. A node re-runs STUN and
    /// calls this under a strictly-higher `nonce` to supersede its stale
    /// reflexive; an equal-or-lower nonce is rejected as stale (a replay
    /// cannot clobber the fresh mapping). A DIFFERENT source against a still
    /// LIVE mapping is rejected too (`SourceMismatch`) — the authenticator is
    /// not bound to the datagram source, so an on-path replay of a captured
    /// keepalive from elsewhere is indistinguishable from a genuine rebind by
    /// nonce alone; only the old mapping's own expiry opens the slot to a new
    /// source. After a `Superseded`, a peer's `Lookup` resolves the new
    /// reflexive.
    pub fn readvertise(
        &mut self,
        key: NodeKey,
        src: SocketAddr,
        nonce: u64,
        now: u64,
    ) -> AdvertOutcome {
        // The MutexGuard `.lock()` produces is a temporary scoped to this
        // statement — it drops before `log_admit_event` runs on the next
        // line, so the log write still lands outside the book's lock.
        let Admission { outcome, event } = self.adverts.lock().readvertise(key, src, nonce, now);
        log_admit_event(event);
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AdvertOutcome, Msg, NodeKey};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn addr(o: u8, p: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, o)), p)
    }

    /// A `Register` carrying a cookie that verifies for `src` at `now` — the
    /// tests exercise `handle_replies` directly (bypassing the wire auth
    /// envelope), so they mint against the coordinator's own key rather than
    /// going through a `BindRequest` round trip.
    fn reg(c: &Coordinator, key: NodeKey, src: SocketAddr, now: u64) -> Msg {
        Msg::Register {
            key,
            cookie: c.cookie_key.mint(src, now),
        }
    }

    #[test]
    fn readvertise_supersedes_stale_mapping_and_lookup_reflects_it() {
        let mut c = Coordinator::new();
        let a_src = addr(1, 1111);
        let b_src = addr(2, 2222);
        let a = NodeKey([0xaa; 32]);
        let b = NodeKey([0xbb; 32]);
        c.handle_verified(a, a_src, reg(&c, a, a_src, 0));
        c.handle_verified(b, b_src, reg(&c, b, b_src, 0));

        // A keepalives from its OWN reflexive under a higher nonce.
        assert_eq!(c.readvertise(a, a_src, 1, 0), AdvertOutcome::Superseded);

        // B's lookup resolves A's reflexive, and the fan-out PunchSync to A
        // targets it.
        let out = c.handle_verified(b, b_src, Msg::Lookup { key: a });
        assert!(out.contains(&(
            b_src,
            Msg::LookupResponse {
                key: a,
                reflexive: Some(a_src)
            }
        )));
        assert!(out.contains(&(
            a_src,
            Msg::PunchSync {
                peer: b,
                peer_reflexive: b_src
            }
        )));

        // A replayed/equal-nonce re-advert is stale and does not move the mapping.
        assert_eq!(c.readvertise(a, addr(1, 7777), 1, 0), AdvertOutcome::Stale);
        let out2 = c.handle_verified(b, b_src, Msg::Lookup { key: a });
        assert!(out2.contains(&(
            b_src,
            Msg::LookupResponse {
                key: a,
                reflexive: Some(a_src)
            }
        )));

        // A DIFFERENT source replaying a captured, still-fresh higher-nonce
        // re-advert cannot hijack the live mapping either — the guard that
        // protects `Register` protects the keepalive path too.
        assert_eq!(
            c.readvertise(a, addr(9, 6666), 2, 0),
            AdvertOutcome::SourceMismatch
        );
        let out3 = c.handle_verified(b, b_src, Msg::Lookup { key: a });
        assert!(out3.contains(&(
            b_src,
            Msg::LookupResponse {
                key: a,
                reflexive: Some(a_src)
            }
        )));
    }

    #[test]
    fn wire_readvertise_supersedes_and_replayed_register_cannot_roll_it_back() {
        // Everything here goes through `handle` — the SAME dispatch the real UDP
        // loop uses — so this proves the nonce-gated rebind is reachable over the
        // wire protocol, not only via the in-process `readvertise` API.
        let mut c = Coordinator::new();
        let a = NodeKey([0xaa; 32]);
        let b = NodeKey([0xbb; 32]);
        let b_src = addr(2, 2222);
        let old = addr(1, 1111);
        let attacker_src = addr(9, 6666);

        // Boot: A registers from its old mapping (implicit nonce 0).
        assert!(
            c.handle_verified(a, old, reg(&c, a, old, 0))
                .is_empty()
        );
        // B registers from its own source too — a Lookup is only answered to a
        // caller the coordinator has already bound to that source.
        assert!(
            c.handle_verified(b, b_src, reg(&c, b, b_src, 0))
                .is_empty()
        );

        // A keepalives from the SAME mapping over the wire under nonce 1.
        assert!(
            c.handle_verified(a, old, Msg::Readvertise {
                    key: a,
                    nonce: 1,
                    cookie: [0u8; 32],
                })
                .is_empty()
        );
        let out = c.handle_verified(b, b_src, Msg::Lookup { key: a });
        assert!(
            out.contains(&(
                b_src,
                Msg::LookupResponse {
                    key: a,
                    reflexive: Some(old)
                }
            )),
            "a wire Readvertise supersedes the stale nonce"
        );

        // A DIFFERENT source replaying a captured, still-fresh higher-nonce
        // Readvertise cannot hijack the live mapping over the wire either.
        assert!(
            c.handle_verified(a, attacker_src, Msg::Readvertise {
                    key: a,
                    nonce: 2,
                    cookie: [0u8; 32],
                })
                .is_empty()
        );
        let out_attacker = c.handle_verified(b, b_src, Msg::Lookup { key: a });
        assert!(
            out_attacker.contains(&(
                b_src,
                Msg::LookupResponse {
                    key: a,
                    reflexive: Some(old)
                }
            )),
            "a different-source readvertise cannot hijack the live mapping"
        );

        // A duplicated/reordered/replayed Register from the OLD mapping arrives
        // late. It must NOT roll the fresh {old, nonce=1} mapping back to nonce 0.
        assert!(
            c.handle_verified(a, old, reg(&c, a, old, 0))
                .is_empty()
        );
        let out2 = c.handle_verified(b, b_src, Msg::Lookup { key: a });
        assert!(
            out2.contains(&(
                b_src,
                Msg::LookupResponse {
                    key: a,
                    reflexive: Some(old)
                }
            )),
            "a replayed nonce-0 Register must not clobber a higher-nonce readvertised mapping"
        );

        // A wire Readvertise at an equal-or-lower nonce is likewise stale.
        assert!(
            c.handle_verified(a, old, Msg::Readvertise {
                    key: a,
                    nonce: 1,
                    cookie: [0u8; 32],
                })
                .is_empty()
        );
        let out3 = c.handle_verified(b, b_src, Msg::Lookup { key: a });
        assert!(out3.contains(&(
            b_src,
            Msg::LookupResponse {
                key: a,
                reflexive: Some(old)
            }
        )));
    }

    #[test]
    fn bind_request_echoes_observed_source() {
        let mut c = Coordinator::new();
        let src = addr(7, 40000);
        let caller = NodeKey([1u8; 32]);
        let out = c.handle_verified(caller, src, Msg::BindRequest { from: caller });
        let cookie = c.cookie_key.mint(src, 0);
        assert_eq!(out, vec![(src, Msg::BindResponse { reflexive: src, cookie })]);
    }

    #[test]
    fn register_then_lookup_returns_reflexive() {
        let mut c = Coordinator::new();
        let a_src = addr(1, 1111);
        let b_src = addr(2, 2222);
        let a = NodeKey([0xaa; 32]);
        let b = NodeKey([0xbb; 32]);
        assert!(
            c.handle_verified(a, a_src, reg(&c, a, a_src, 0))
                .is_empty()
        );
        assert!(
            c.handle_verified(b, b_src, reg(&c, b, b_src, 0))
                .is_empty()
        );

        // A looks up B: coordinator replies to A with B's reflexive AND tells
        // both sides to punch simultaneously.
        let out = c.handle_verified(a, a_src, Msg::Lookup { key: b });
        assert!(out.contains(&(
            a_src,
            Msg::LookupResponse {
                key: b,
                reflexive: Some(b_src)
            }
        )));
        assert!(out.contains(&(
            a_src,
            Msg::PunchSync {
                peer: b,
                peer_reflexive: b_src
            }
        )));
        assert!(out.contains(&(
            b_src,
            Msg::PunchSync {
                peer: a,
                peer_reflexive: a_src
            }
        )));
    }

    #[test]
    fn lookup_unknown_returns_none() {
        let mut c = Coordinator::new();
        let a_src = addr(1, 1111);
        let a = NodeKey([0xaa; 32]);
        let missing = NodeKey([0xcc; 32]);
        // The caller must be bound to `a_src` before any Lookup is answered.
        assert!(
            c.handle_verified(a, a_src, reg(&c, a, a_src, 0))
                .is_empty()
        );
        let out = c.handle_verified(a, a_src, Msg::Lookup { key: missing });
        assert_eq!(
            out,
            vec![(
                a_src,
                Msg::LookupResponse {
                    key: missing,
                    reflexive: None
                }
            )]
        );
    }

    #[test]
    fn private_policy_admits_authorized_register_and_lookup_but_drops_unauthorized() {
        use crate::AuthRequest;
        use crate::auth::{AuthPolicy, mint_coord_cap, now_secs, sign_authenticator};
        use commonware_cryptography::{Signer as _, ed25519};

        let g = ed25519::PrivateKey::from_seed(100);
        let node = ed25519::PrivateKey::from_seed(200);
        let mut nb = [0u8; 32];
        nb.copy_from_slice(node.public_key().as_ref());
        let subject = NodeKey(nb);
        let now = now_secs();

        let mut c = Coordinator::with_policy(AuthPolicy::Private {
            genesis_set: vec![g.public_key()],
        });
        let src = addr(1, 1111);

        // Authorized: joiner with a valid genesis cap registers -> mapping created.
        let reg = Msg::Register { key: subject, cookie: c.cookie_key.mint(src, now) };
        let cap = mint_coord_cap(&g, subject, now + 3600);
        let auth = sign_authenticator(&node, &reg.encode(), now, Some(cap));
        let out = c.handle_auth(
            src,
            AuthRequest {
                caller: subject,
                inner: reg,
                auth,
            },
            now,
        );
        assert!(out.is_empty());
        // A lookup from the same authorized node resolves it. The caller is the
        // authenticated principal; here it looks up its OWN key.
        let lk = Msg::Lookup { key: subject };
        let lauth = sign_authenticator(
            &node,
            &lk.encode(),
            now,
            Some(mint_coord_cap(&g, subject, now + 3600)),
        );
        let out = c.handle_auth(
            src,
            AuthRequest {
                caller: subject,
                inner: lk,
                auth: lauth,
            },
            now,
        );
        assert!(out.iter().any(|(_, m)| matches!(
            m,
            Msg::LookupResponse {
                reflexive: Some(_),
                ..
            }
        )));

        // Unauthorized: outsider (no cap) -> dropped, no mapping, reject counted.
        let outsider = ed25519::PrivateKey::from_seed(201);
        let mut ob = [0u8; 32];
        ob.copy_from_slice(outsider.public_key().as_ref());
        let osub = NodeKey(ob);
        let before = c.rejects();
        let oreg = Msg::Register { key: osub, cookie: c.cookie_key.mint(addr(2, 2222), now) };
        let oauth = sign_authenticator(&outsider, &oreg.encode(), now, None);
        let out = c.handle_auth(
            addr(2, 2222),
            AuthRequest {
                caller: osub,
                inner: oreg,
                auth: oauth,
            },
            now,
        );
        assert!(out.is_empty());
        assert_eq!(c.rejects(), before + 1);
        // The outsider's key never entered the book: an AUTHORIZED lookup for it
        // resolves to None (the dropped register created no mapping). The
        // looker-up is `subject` — already bound to `src` by its own earlier
        // Register — since an unregistered caller (the outsider itself) gets
        // no reply to any Lookup at all, regardless of what it names.
        let lk = Msg::Lookup { key: osub };
        let lauth = sign_authenticator(
            &node,
            &lk.encode(),
            now,
            Some(mint_coord_cap(&g, subject, now + 3600)),
        );
        let out = c.handle_auth(
            src,
            AuthRequest {
                caller: subject,
                inner: lk,
                auth: lauth,
            },
            now,
        );
        assert!(out.iter().any(|(_, m)| matches!(
            m,
            Msg::LookupResponse {
                reflexive: None,
                ..
            }
        )));
    }

    #[test]
    fn parsed_key_cache_is_lazy_and_never_skips_signature_verification() {
        use crate::AuthRequest;
        use crate::auth::{AuthPolicy, sign_authenticator};
        use commonware_cryptography::{Signer as _, ed25519};

        let node = ed25519::PrivateKey::from_seed(250);
        let attacker = ed25519::PrivateKey::from_seed(251);
        let mut key = [0; 32];
        key.copy_from_slice(node.public_key().as_ref());
        let caller = NodeKey(key);
        let source = addr(1, 1111);
        let now = 1_000_000;
        let inner = Msg::BindRequest { from: caller };
        let mut coordinator = Coordinator::with_policy(AuthPolicy::Public);

        assert!(coordinator.auth.auth_keys.is_none(), "cache starts lazy");
        let good = AuthRequest {
            caller,
            inner: inner.clone(),
            auth: sign_authenticator(&node, &inner.encode(), now, None),
        };
        assert_eq!(coordinator.handle_auth_replies(source, good, now).len(), 1);
        assert!(coordinator.auth.auth_keys.is_some(), "valid key was cached");

        // This hits the cached parsed public key but carries a signature from a
        // different private key. The cache saves only key parsing; it must not
        // cache or bypass the per-request proof-of-possession decision.
        let forged = AuthRequest {
            caller,
            inner: inner.clone(),
            auth: sign_authenticator(&attacker, &inner.encode(), now, None),
        };
        let before = coordinator.rejects();
        assert!(
            coordinator
                .handle_auth_replies(source, forged, now)
                .is_empty()
        );
        assert_eq!(coordinator.rejects(), before + 1);
    }

    #[test]
    fn cross_peer_lookup_authenticates_the_caller_and_fans_out() {
        // The previously-impossible path: caller A (admitted) looks up a
        // DIFFERENT peer B. Authentication is against A's key (the caller), so
        // A's PoP — signed with A's own key — validates, and the coordinator
        // returns B's mapping plus a PunchSync fan-out carrying A's REAL key.
        use crate::AuthRequest;
        use crate::auth::{AuthPolicy, mint_coord_cap, now_secs, sign_authenticator};
        use commonware_cryptography::{Signer as _, ed25519};

        let g = ed25519::PrivateKey::from_seed(300);
        let a = ed25519::PrivateKey::from_seed(301);
        let b = ed25519::PrivateKey::from_seed(302);
        let a_key = {
            let mut k = [0u8; 32];
            k.copy_from_slice(a.public_key().as_ref());
            NodeKey(k)
        };
        let b_key = {
            let mut k = [0u8; 32];
            k.copy_from_slice(b.public_key().as_ref());
            NodeKey(k)
        };
        let now = now_secs();

        let mut c = Coordinator::with_policy(AuthPolicy::Private {
            genesis_set: vec![g.public_key()],
        });
        let a_src = addr(1, 1111);
        let b_src = addr(2, 2222);

        // Both register (self-ops, caller == inner key).
        let a_reg = Msg::Register { key: a_key, cookie: c.cookie_key.mint(a_src, now) };
        let a_auth = sign_authenticator(
            &a,
            &a_reg.encode(),
            now,
            Some(mint_coord_cap(&g, a_key, now + 3600)),
        );
        assert!(
            c.handle_auth(
                a_src,
                AuthRequest {
                    caller: a_key,
                    inner: a_reg,
                    auth: a_auth
                },
                now
            )
            .is_empty()
        );
        let b_reg = Msg::Register { key: b_key, cookie: c.cookie_key.mint(b_src, now) };
        let b_auth = sign_authenticator(
            &b,
            &b_reg.encode(),
            now,
            Some(mint_coord_cap(&g, b_key, now + 3600)),
        );
        assert!(
            c.handle_auth(
                b_src,
                AuthRequest {
                    caller: b_key,
                    inner: b_reg,
                    auth: b_auth
                },
                now
            )
            .is_empty()
        );

        // A looks up B: inner key is B, caller (and PoP signer) is A.
        let lk = Msg::Lookup { key: b_key };
        let lauth = sign_authenticator(
            &a,
            &lk.encode(),
            now,
            Some(mint_coord_cap(&g, a_key, now + 3600)),
        );
        let out = c.handle_auth(
            a_src,
            AuthRequest {
                caller: a_key,
                inner: lk,
                auth: lauth,
            },
            now,
        );
        // A receives B's reflexive and its own PunchSync toward B.
        assert!(out.contains(&(
            a_src,
            Msg::LookupResponse {
                key: b_key,
                reflexive: Some(b_src)
            }
        )));
        assert!(out.contains(&(
            a_src,
            Msg::PunchSync {
                peer: b_key,
                peer_reflexive: b_src
            }
        )));
        // The peer-directed fan-out to B carries A's AUTHENTICATED key, not a
        // reverse-mapped or zero key.
        assert!(out.contains(&(
            b_src,
            Msg::PunchSync {
                peer: a_key,
                peer_reflexive: a_src
            }
        )));
    }

    #[test]
    fn anti_poisoning_rejects_self_op_with_mismatched_caller() {
        // A member cannot register or re-advertise ANOTHER node's key: for a
        // self-op the inner key must equal the authenticated caller, so an
        // AuthRequest whose caller differs from the inner Register key is
        // rejected and the reject counter increments.
        use crate::AuthRequest;
        use crate::auth::{AuthPolicy, mint_coord_cap, now_secs, sign_authenticator};
        use commonware_cryptography::{Signer as _, ed25519};

        let g = ed25519::PrivateKey::from_seed(400);
        let attacker = ed25519::PrivateKey::from_seed(401);
        let attacker_key = {
            let mut k = [0u8; 32];
            k.copy_from_slice(attacker.public_key().as_ref());
            NodeKey(k)
        };
        let victim_key = {
            let mut k = [0u8; 32];
            k.copy_from_slice(ed25519::PrivateKey::from_seed(402).public_key().as_ref());
            NodeKey(k)
        };
        let now = now_secs();

        let mut c = Coordinator::with_policy(AuthPolicy::Private {
            genesis_set: vec![g.public_key()],
        });
        let src = addr(1, 1111);
        let before = c.rejects();

        // Attacker (validly admitted for its OWN key) tries to Register the
        // victim's key. The PoP verifies against the caller, but the inner key
        // is the victim's — a self-op mismatch, rejected before dispatch.
        let reg = Msg::Register { key: victim_key, cookie: c.cookie_key.mint(src, now) };
        let auth = sign_authenticator(
            &attacker,
            &reg.encode(),
            now,
            Some(mint_coord_cap(&g, attacker_key, now + 3600)),
        );
        let out = c.handle_auth(
            src,
            AuthRequest {
                caller: attacker_key,
                inner: reg,
                auth,
            },
            now,
        );
        assert!(out.is_empty());
        assert_eq!(
            c.rejects(),
            before + 1,
            "self-op with mismatched caller is rejected"
        );

        // The attacker legitimately registers its OWN key from `src` — needed
        // only so the Lookup below is answered at all (a Lookup is refused
        // outright from a caller the book has not bound to its source).
        let self_reg = Msg::Register { key: attacker_key, cookie: c.cookie_key.mint(src, now) };
        let self_auth = sign_authenticator(
            &attacker,
            &self_reg.encode(),
            now,
            Some(mint_coord_cap(&g, attacker_key, now + 3600)),
        );
        assert!(
            c.handle_auth(
                src,
                AuthRequest {
                    caller: attacker_key,
                    inner: self_reg,
                    auth: self_auth,
                },
                now,
            )
            .is_empty()
        );

        // The victim's key never entered the book: an authenticated self-lookup
        // by the attacker for the victim's key resolves to None (no mapping was
        // poisoned into existence).
        let lk = Msg::Lookup { key: victim_key };
        let lauth = sign_authenticator(
            &attacker,
            &lk.encode(),
            now,
            Some(mint_coord_cap(&g, attacker_key, now + 3600)),
        );
        let out = c.handle_auth(
            src,
            AuthRequest {
                caller: attacker_key,
                inner: lk,
                auth: lauth,
            },
            now,
        );
        assert!(out.iter().any(
            |(_, m)| matches!(m, Msg::LookupResponse { key, reflexive: None } if *key == victim_key)
        ));
    }

    #[test]
    fn expired_registration_lookup_is_none_and_fans_no_punch_sync() {
        let mut c = Coordinator::with_policy_and_ttl(crate::auth::AuthPolicy::Public, 120);
        let a = NodeKey([0xaa; 32]);
        let b = NodeKey([0xbb; 32]);
        let a_src = addr(1, 1111);
        let b_src = addr(2, 2222);
        assert!(
            c.handle_verified_at(a, a_src, reg(&c, a, a_src, 1_000), 1_000)
                .is_empty()
        );
        assert!(
            c.handle_verified_at(b, b_src, reg(&c, b, b_src, 1_000), 1_000)
                .is_empty()
        );

        // Within TTL: resolves and fans. B refreshes its own registration
        // (same source, own key) so its Lookup keeps being bound and answered
        // — this test is about A's mapping expiring, not B's.
        assert!(
            c.handle_verified_at(b, b_src, reg(&c, b, b_src, 1_100), 1_100)
                .is_empty()
        );
        let out = c.handle_verified_at(b, b_src, Msg::Lookup { key: a }, 1_100);
        assert!(out.contains(&(
            b_src,
            Msg::LookupResponse {
                key: a,
                reflexive: Some(a_src)
            }
        )));
        assert!(
            out.iter()
                .any(|(dst, m)| *dst == a_src && matches!(m, Msg::PunchSync { .. }))
        );

        // Past TTL: honest None, and crucially NO PunchSync toward the dead pinhole.
        assert!(
            c.handle_verified_at(b, b_src, reg(&c, b, b_src, 1_121), 1_121)
                .is_empty()
        );
        let out = c.handle_verified_at(b, b_src, Msg::Lookup { key: a }, 1_121);
        assert_eq!(
            out,
            vec![(
                b_src,
                Msg::LookupResponse {
                    key: a,
                    reflexive: None
                }
            )]
        );
    }

    #[test]
    fn keepalive_readvertise_extends_registration_life() {
        let mut c = Coordinator::with_policy_and_ttl(crate::auth::AuthPolicy::Public, 120);
        let a = NodeKey([0xaa; 32]);
        let b = NodeKey([0xbb; 32]);
        let a_src = addr(1, 1111);
        let b_src = addr(2, 2222);
        assert!(
            c.handle_verified_at(a, a_src, reg(&c, a, a_src, 1_000), 1_000)
                .is_empty()
        );
        assert!(
            c.handle_verified_at(a, a_src, Msg::Readvertise {
                    key: a,
                    nonce: 1,
                    cookie: [0u8; 32],
                }, 1_100)
                .is_empty()
        );
        assert!(
            c.handle_verified_at(a, a_src, Msg::Readvertise {
                    key: a,
                    nonce: 2,
                    cookie: [0u8; 32],
                }, 1_200)
                .is_empty()
        );
        // B registers right before its Lookup — a Lookup is only answered to
        // a caller the book has bound to its source.
        assert!(
            c.handle_verified_at(b, b_src, reg(&c, b, b_src, 1_300), 1_300)
                .is_empty()
        );
        let out = c.handle_verified_at(b, b_src, Msg::Lookup { key: a }, 1_300);
        assert!(
            out.contains(&(
                b_src,
                Msg::LookupResponse {
                    key: a,
                    reflexive: Some(a_src)
                }
            )),
            "keepalive readvertises kept the mapping alive well past the boot TTL"
        );
    }

    #[test]
    fn replayed_authenticated_register_from_another_source_cannot_hijack() {
        // H3 at the authenticated handler: an on-path attacker captures a
        // victim's valid AuthRequest{Register} and replays the IDENTICAL bytes
        // from its OWN socket within the freshness window. The PoP still verifies
        // (it is the victim's real signature), so dispatch reaches
        // observe(victim, attacker_src) — which must NOT repoint the live mapping.
        use crate::AuthRequest;
        use crate::auth::{AuthPolicy, now_secs, sign_authenticator};
        use commonware_cryptography::{Signer as _, ed25519};

        let node = ed25519::PrivateKey::from_seed(555);
        let mut nb = [0u8; 32];
        nb.copy_from_slice(node.public_key().as_ref());
        let victim = NodeKey(nb);
        let now = now_secs();

        let mut c = Coordinator::with_policy(AuthPolicy::Public);
        let victim_src = addr(1, 4000);
        let attacker_src = addr(9, 6666);
        let b_src = addr(2, 2222);

        // The victim registers from its own source (nonce-0 baseline, live).
        let victim_reg = Msg::Register {
            key: victim,
            cookie: c.cookie_key.mint(victim_src, now),
        };
        let auth = sign_authenticator(&node, &victim_reg.encode(), now, None);
        let authreq = AuthRequest {
            caller: victim,
            inner: victim_reg,
            auth,
        };
        assert!(c.handle_auth(victim_src, authreq.clone(), now).is_empty());

        // The attacker replays the IDENTICAL captured AuthRequest from its own
        // source. PoP re-verifies, but observe must refuse to repoint the mapping.
        assert!(c.handle_auth(attacker_src, authreq, now).is_empty());

        // B registers from its own source too, so its Lookup is answered at
        // all — a Lookup is refused outright from a caller the book has not
        // bound to its source.
        let b = NodeKey([0xbb; 32]);
        assert!(
            c.handle_verified_at(b, b_src, reg(&c, b, b_src, now), now)
                .is_empty()
        );

        // A lookup still resolves the victim's ORIGINAL reflexive, and the punch
        // fan-out targets it — never the attacker.
        let out = c.handle_verified_at(b, b_src, Msg::Lookup { key: victim }, now);
        assert!(
            out.contains(&(
                b_src,
                Msg::LookupResponse {
                    key: victim,
                    reflexive: Some(victim_src)
                }
            )),
            "a replayed register from another source must not hijack the mapping"
        );
        assert!(
            out.iter()
                .any(|(dst, m)| *dst == victim_src && matches!(m, Msg::PunchSync { .. })),
            "the punch fan-out targets the victim's real source"
        );
        assert!(
            !out.iter().any(|(dst, _)| *dst == attacker_src),
            "nothing is directed at the attacker's source"
        );
    }

    #[test]
    fn first_seen_register_without_cookie_is_refused() {
        // The reflection issue #1787 closes: one spoofed-source datagram
        // with no return-routability proof must not plant a mapping for a
        // key the coordinator has never seen before.
        let mut c = Coordinator::new();
        let attacker_key = NodeKey([0x11; 32]);
        let victim_src = addr(9, 9999);
        let out = c.handle_verified(
            attacker_key,
            victim_src,
            Msg::Register {
                key: attacker_key,
                cookie: [0u8; 32],
            },
        );
        assert!(out.is_empty());
        assert_eq!(
            c.adverts().current(attacker_key, 0),
            None,
            "no cookie -> no mapping, so the relay resolves nothing for this key either"
        );
    }

    #[test]
    fn first_seen_register_with_wrong_cookie_is_refused() {
        let mut c = Coordinator::new();
        let key = NodeKey([0x22; 32]);
        let src = addr(9, 9999);
        // A cookie that verifies for a DIFFERENT source is exactly as useless
        // as no cookie at all.
        let wrong_cookie = c.cookie_key.mint(addr(1, 1), 0);
        let out = c.handle_verified(
            key,
            src,
            Msg::Register {
                key,
                cookie: wrong_cookie,
            },
        );
        assert!(out.is_empty());
        assert_eq!(c.adverts().current(key, 0), None);
    }

    #[test]
    fn first_seen_register_with_correct_cookie_is_admitted() {
        let mut c = Coordinator::new();
        let key = NodeKey([0x33; 32]);
        let src = addr(9, 9999);
        let out = c.handle_verified(key, src, reg(&c, key, src, 0));
        assert!(out.is_empty());
        assert_eq!(c.adverts().current(key, 0), Some(src));
    }

    #[test]
    fn same_source_refresh_needs_no_cookie() {
        let mut c = Coordinator::new();
        let key = NodeKey([0x44; 32]);
        let src = addr(9, 9999);
        // First-seen registration with a valid cookie establishes the LIVE
        // mapping.
        c.handle_verified(key, src, reg(&c, key, src, 0));
        assert_eq!(c.adverts().current(key, 0), Some(src));

        // A same-source refresh with an all-zero (invalid) cookie must still
        // succeed: `observe`'s own same-source check is the proof here, not
        // the cookie.
        let out = c.handle_verified(
            key,
            src,
            Msg::Register {
                key,
                cookie: [0u8; 32],
            },
        );
        assert!(out.is_empty());
        assert_eq!(c.adverts().current(key, 0), Some(src));
    }

    #[test]
    fn spoofed_first_readvertise_for_an_unknown_key_without_cookie_is_refused() {
        // `AdvertBook::readvertise` falls into the same `insert_fresh` path
        // `observe` does for a key with no live entry — a spoofed
        // `Readvertise{nonce: 1}` for a never-registered key must be gated
        // exactly like a spoofed first-seen `Register` (#1787's fix, applied
        // to the arm it missed).
        let mut c = Coordinator::new();
        let attacker_key = NodeKey([0x55; 32]);
        let victim_src = addr(9, 9999);
        let out = c.handle_verified(
            attacker_key,
            victim_src,
            Msg::Readvertise {
                key: attacker_key,
                nonce: 1,
                cookie: [0u8; 32],
            },
        );
        assert!(out.is_empty());
        assert_eq!(
            c.adverts().current(attacker_key, 0),
            None,
            "no cookie -> no mapping, so the relay resolves nothing for this key either"
        );
    }

    #[test]
    fn same_source_readvertise_of_a_live_mapping_needs_no_cookie() {
        let mut c = Coordinator::new();
        let key = NodeKey([0x66; 32]);
        let src = addr(9, 9999);
        c.handle_verified(key, src, reg(&c, key, src, 0));
        assert_eq!(c.adverts().current(key, 0), Some(src));

        // A same-source rebind under a higher nonce, with an all-zero
        // (invalid) cookie, must still supersede the mapping: it is not
        // creating a live entry, only extending the one that already exists.
        let out = c.handle_verified(
            key,
            src,
            Msg::Readvertise {
                key,
                nonce: 1,
                cookie: [0u8; 32],
            },
        );
        assert!(out.is_empty());
        assert_eq!(c.adverts().current(key, 0), Some(src));
    }

    #[test]
    fn spoofed_source_lookup_gets_no_reply_and_no_punch_sync() {
        // The reflection this guard closes: a fresh, never-registered caller
        // key riding a datagram whose source is forged to a victim address.
        // `verify_request_using` proves possession of the caller key but never
        // binds it to the datagram source, so without the source-binding guard
        // the coordinator would answer `victim_src` with a LookupResponse and a
        // PunchSync, and tell the resolved member to punch `victim_src` too —
        // three unsolicited datagrams from two legitimate IPs, aimed by the
        // attacker at a target of its choosing.
        let mut c = Coordinator::new();
        let member = NodeKey([0xaa; 32]);
        let member_src = addr(1, 1111);
        let fresh_caller = NodeKey([0xee; 32]);
        let victim_src = addr(9, 9999);

        assert!(
            c.handle_verified(member, member_src, reg(&c, member, member_src, 0))
                .is_empty()
        );

        let out = c.handle_verified(fresh_caller, victim_src, Msg::Lookup { key: member });
        assert!(
            out.is_empty(),
            "an unbound caller's Lookup must produce zero datagrams, not even a LookupResponse"
        );
    }

    #[test]
    fn registered_caller_lookup_from_registered_source_still_resolves() {
        // The legitimate flow the guard must not cost anything: a caller that
        // registered from a source, then looks up a peer FROM THAT SAME
        // source, still gets its full three-datagram fan-out.
        let mut c = Coordinator::new();
        let caller = NodeKey([0xaa; 32]);
        let caller_src = addr(1, 1111);
        let peer = NodeKey([0xbb; 32]);
        let peer_src = addr(2, 2222);

        assert!(
            c.handle_verified(caller, caller_src, reg(&c, caller, caller_src, 0))
                .is_empty()
        );
        assert!(
            c.handle_verified(peer, peer_src, reg(&c, peer, peer_src, 0))
                .is_empty()
        );

        let out = c.handle_verified(caller, caller_src, Msg::Lookup { key: peer });
        assert!(out.contains(&(
            caller_src,
            Msg::LookupResponse {
                key: peer,
                reflexive: Some(peer_src)
            }
        )));
        assert!(out.contains(&(
            caller_src,
            Msg::PunchSync {
                peer,
                peer_reflexive: peer_src
            }
        )));
        assert!(out.contains(&(
            peer_src,
            Msg::PunchSync {
                peer: caller,
                peer_reflexive: caller_src
            }
        )));
        assert_eq!(
            out.len(),
            3,
            "the full fan-out, unaffected by the source guard"
        );
    }
}
