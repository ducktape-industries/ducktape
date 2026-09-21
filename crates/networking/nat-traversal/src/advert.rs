use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::NodeKey;

/// One node's latest reflexive advertisement: the reflexive `SocketAddr` a node
/// published and the monotonic `nonce` that orders it. The nonce is an ordering
/// token only — the address is always the coordinator-observed source, never a
/// self-reported one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReflexiveAdvert {
    pub reflexive: SocketAddr,
    /// Freshness for THIS key and nothing else: a strictly-higher nonce
    /// supersedes this key's own stored mapping. It is sender-chosen, so it
    /// never ranks one key against another — comparing two keys' nonces would
    /// hand a stranger the ordering.
    pub nonce: u64,
    /// Wall-clock seconds of the last ACCEPTED advert (an `observe` or a
    /// superseding `readvertise`). A stale-nonce replay never refreshes this —
    /// only fresh proof of life extends a mapping.
    pub last_seen: u64,
}

/// How long a registration stays resolvable after its last accepted advert.
/// A NAT's UDP pinhole dies in ~30 s of silence, so a mapping the node has
/// not refreshed for two minutes (≈5 missed 25 s keepalives —
/// `reachability::RENDEZVOUS_KEEPALIVE`) points at a dead hole: answering
/// lookups with it, or fanning `PunchSync` at it, is worse than an honest
/// `None`.
pub const REGISTRATION_TTL_SECS: u64 = 120;

/// Result of applying a re-advertisement: it either superseded the stored
/// mapping or was rejected as stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdvertOutcome {
    Superseded,
    Stale,
    /// `observe` found a mapping already ahead of the baseline (a superseding
    /// nonce, or a live mapping from a different source) and left it
    /// untouched. Not a refusal — nothing was refused, there was simply
    /// nothing to do.
    NoOp,
    /// `readvertise` found a LIVE mapping whose stored reflexive differs from
    /// the datagram's observed source, even though the nonce clears the
    /// staleness check. `verify_request_using` is not bound to the datagram
    /// source (same as `Register`'s authenticator), so a captured keepalive
    /// replayed from a different source is otherwise indistinguishable from a
    /// genuine rebind by nonce alone — this mirrors `observe`'s own
    /// different-source guard. The stored mapping is left untouched.
    SourceMismatch,
}

/// The reachability-plane reflexive registry: for each node key, the latest
/// accepted `ReflexiveAdvert`. `observe` is the unconditional boot/live
/// registration (the observed source is authoritative); `readvertise` is the
/// nonce-gated rebind path.
///
/// The nonce rule deliberately MIRRORS `wireguard_upgrade::MeshView::verify`'s
/// duplicate-advertisement rule (`nonce <= prev => StaleDuplicateAdvertisement`)
/// so a NAT-rebound node re-advertises under a strictly-higher nonce to
/// supersede its stale mapping — WITHOUT this crate depending on
/// `wireguard-upgrade` or any validator-identity type (the Slice 2 invariant).
pub struct AdvertBook {
    latest: HashMap<NodeKey, ReflexiveAdvert>,
    ttl: u64,
}

impl Default for AdvertBook {
    fn default() -> Self {
        Self::with_ttl(REGISTRATION_TTL_SECS)
    }
}

impl AdvertBook {
    /// A book with an explicit TTL (seconds). Tests and short-lived rigs
    /// shrink it; production uses [`REGISTRATION_TTL_SECS`] via `Default`.
    pub fn with_ttl(ttl: u64) -> Self {
        Self {
            latest: HashMap::new(),
            ttl,
        }
    }

    fn expired(&self, advert: &ReflexiveAdvert, now: u64) -> bool {
        now.saturating_sub(advert.last_seen) > self.ttl
    }

    /// Boot/live registration at the nonce-0 baseline. The coordinator-observed
    /// `src` is authoritative. This establishes the baseline for a first-seen key
    /// and refreshes it while still at the baseline, but it is NOT unconditional:
    /// a nonce-0 `Register` must never REPOINT a still-ALIVE mapping, because the
    /// authenticator is not bound to the datagram source, so a captured `Register`
    /// replayed from a DIFFERENT source within the freshness window would
    /// otherwise hijack the owner's mapping to the attacker's observed address.
    /// Two cases keep a live mapping fixed. When `nonce > 0`, a rebind
    /// re-advertisement already superseded the baseline, so a later (necessarily
    /// nonce-0) `Register` is stale by construction. When `nonce == 0` but the
    /// source DIFFERS, a live baseline mapping is not repointed by a bare
    /// `Register` from elsewhere: a genuine NAT rebind re-advertises under a
    /// strictly-higher nonce (`readvertise`, the keepalive path), never a bare
    /// nonce-0 `Register`, so no legitimate node needs this — while a SAME-source
    /// `Register` still refreshes liveness. An EXPIRED mapping is dead weight (its
    /// pinhole is gone), so both guards yield and the fresh register takes the
    /// slot back — the reboot case.
    pub fn observe(&mut self, key: NodeKey, src: SocketAddr, now: u64) -> AdvertOutcome {
        match self.latest.get(&key) {
            Some(prev) if !self.expired(prev, now) && (prev.nonce > 0 || prev.reflexive != src) => {
                AdvertOutcome::NoOp
            }
            _ => {
                self.insert_fresh(key, src, 0, now);
                AdvertOutcome::Superseded
            }
        }
    }

    /// Rebind re-advertisement. A strictly-higher `nonce` supersedes the stored
    /// mapping (store `src`, return `Superseded`); an equal-or-lower nonce
    /// against a LIVE mapping is stale and leaves the stored advert untouched
    /// (`Stale`) — and deliberately does not refresh `last_seen`, so a replayed
    /// datagram cannot keep a mapping alive. No prior entry, or an EXPIRED one,
    /// -> accepted as a first advert (the nonce guard protects live mappings,
    /// not corpses — a rebooted node restarts its nonce sequence).
    ///
    /// A THIRD case sits alongside those two: a strictly-higher nonce against
    /// a LIVE mapping from a DIFFERENT source (`SourceMismatch`). The
    /// authenticator is not bound to the datagram source, so this is exactly
    /// as unverifiable as the register-hijack `observe` already guards
    /// against — an on-path attacker who captures a victim's keepalive and
    /// replays the identical bytes from its own socket produces this same
    /// shape (fresh nonce, wrong source) as a genuine NAT rebind would. Since
    /// the two are indistinguishable from the wire alone, the live mapping is
    /// never repointed by it; only the mapping's own expiry (the old pinhole
    /// going silent) frees the slot for a new source.
    pub fn readvertise(
        &mut self,
        key: NodeKey,
        src: SocketAddr,
        nonce: u64,
        now: u64,
    ) -> AdvertOutcome {
        let live = self
            .latest
            .get(&key)
            .filter(|prev| !self.expired(prev, now));
        let stale = matches!(live, Some(prev) if nonce <= prev.nonce);
        if stale {
            return AdvertOutcome::Stale;
        }
        let source_mismatch = matches!(live, Some(prev) if prev.reflexive != src);
        if source_mismatch {
            return AdvertOutcome::SourceMismatch;
        }
        self.insert_fresh(key, src, nonce, now);
        AdvertOutcome::Superseded
    }

    /// The one accepted-advert write path (`observe` and `readvertise` both
    /// end here): store the advert with its life restarted at `now`.
    fn insert_fresh(&mut self, key: NodeKey, src: SocketAddr, nonce: u64, now: u64) {
        self.latest.insert(
            key,
            ReflexiveAdvert {
                reflexive: src,
                nonce,
                last_seen: now,
            },
        );
    }

    /// The key's live reflexive, if its registration has not expired. An
    /// expired mapping resolves to `None` — the honest answer, since its NAT
    /// pinhole died with the silence.
    pub fn current(&self, key: NodeKey, now: u64) -> Option<SocketAddr> {
        self.latest
            .get(&key)
            .filter(|a| !self.expired(a, now))
            .map(|a| a.reflexive)
    }
}

/// One [`AdvertBook`] shared between the UDP rendezvous state machine (the
/// [`crate::Coordinator`], which owns every write) and the TCP relay lane
/// (`crate::relay`, which only resolves targets). The relay MUST read the same
/// book the rendezvous maintains: a member's reflexive is wherever its live
/// keepalives say it is, and a second book would drift.
///
/// A `std::sync::Mutex`, not tokio's: every lock scope is a single book
/// operation and is NEVER held across an await. Both UDP serving loops are
/// single-threaded, so contention is limited to the (rare) relay resolution.
#[derive(Clone)]
pub struct SharedAdverts(Arc<Mutex<AdvertBook>>);

impl SharedAdverts {
    #[cfg(feature = "runtime")]
    pub(crate) fn wrap(book: AdvertBook) -> Self {
        Self(Arc::new(Mutex::new(book)))
    }

    /// The key's live reflexive (`None` once expired) —
    /// [`AdvertBook::current`] behind the shared lock.
    pub fn current(&self, key: NodeKey, now: u64) -> Option<SocketAddr> {
        self.lock().current(key, now)
    }

    /// A poisoned lock only means another holder panicked mid-operation; the
    /// book is a plain map whose worst partial state is one stale entry, so
    /// keep serving joins rather than wedging every future lock on it.
    pub(crate) fn lock(&self) -> MutexGuard<'_, AdvertBook> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn addr(o: u8, p: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, o)), p)
    }

    #[test]
    fn first_observe_then_higher_nonce_supersedes() {
        let key = NodeKey([0xaa; 32]);
        let mut book = AdvertBook::default();
        book.observe(key, addr(1, 4000), 0); // boot: nonce 0
        assert_eq!(book.current(key, 0), Some(addr(1, 4000)));

        // A same-source keepalive under a strictly-higher nonce supersedes the
        // baseline. (A cross-source rebind against this still-live mapping is
        // exercised separately by the source-mismatch tests below — it is
        // refused, not applied.)
        assert_eq!(
            book.readvertise(key, addr(1, 4000), 1, 0),
            AdvertOutcome::Superseded
        );
        assert_eq!(book.current(key, 0), Some(addr(1, 4000)));
    }

    #[test]
    fn equal_or_lower_nonce_is_stale_and_does_not_change_mapping() {
        let key = NodeKey([0xbb; 32]);
        let mut book = AdvertBook::default();
        book.observe(key, addr(1, 4000), 0); // nonce 0
        assert_eq!(
            book.readvertise(key, addr(1, 4000), 2, 0),
            AdvertOutcome::Superseded
        );

        // A replayed / equal-nonce advert must not clobber the fresher mapping
        // (mirrors StaleDuplicateAdvertisement: nonce <= prev). Sent from a
        // different source too, to show staleness is checked BEFORE source.
        assert_eq!(
            book.readvertise(key, addr(9, 9999), 2, 0),
            AdvertOutcome::Stale
        );
        assert_eq!(
            book.readvertise(key, addr(9, 9999), 1, 0),
            AdvertOutcome::Stale
        );
        assert_eq!(
            book.current(key, 0),
            Some(addr(1, 4000)),
            "stale adverts leave state untouched"
        );
    }

    #[test]
    fn observe_does_not_roll_back_a_superseded_higher_nonce_mapping() {
        let key = NodeKey([0xdd; 32]);
        let mut book = AdvertBook::default();
        book.observe(key, addr(1, 4000), 0); // boot: nonce 0
        assert_eq!(
            book.readvertise(key, addr(1, 4000), 1, 0),
            AdvertOutcome::Superseded
        );

        // A replayed/reordered boot Register (observe) from the STALE source must
        // NOT roll the fresh nonce-1 mapping back to the old one.
        book.observe(key, addr(1, 4000), 0);
        assert_eq!(
            book.current(key, 0),
            Some(addr(1, 4000)),
            "a stale nonce-0 register cannot clobber a rebind re-advertisement"
        );
    }

    #[test]
    fn observe_refreshes_the_baseline_from_the_same_source_but_never_repoints_it() {
        // A SAME-source nonce-0 re-register refreshes liveness (a legitimate node
        // re-registering from its own pinhole while still at the baseline)...
        let key = NodeKey([0xee; 32]);
        let mut book = AdvertBook::with_ttl(120);
        book.observe(key, addr(1, 4000), 1_000);
        book.observe(key, addr(1, 4000), 1_050);
        assert_eq!(
            book.current(key, 1_160),
            Some(addr(1, 4000)),
            "life extended to 1_170"
        );
        assert_eq!(
            book.current(key, 1_171),
            None,
            "the same-source refresh moved last_seen"
        );

        // ...but a DIFFERENT-source nonce-0 register does NOT repoint a live
        // baseline mapping. A genuine NAT rebind re-advertises under a higher
        // nonce (readvertise); only a replayed/spoofed bare Register lands here.
        let key = NodeKey([0xef; 32]);
        let mut book = AdvertBook::default();
        book.observe(key, addr(1, 4000), 0);
        book.observe(key, addr(3, 7000), 0);
        assert_eq!(
            book.current(key, 0),
            Some(addr(1, 4000)),
            "a different-source nonce-0 register cannot repoint a live baseline mapping"
        );
    }

    #[test]
    fn replayed_register_from_another_source_cannot_hijack_a_live_mapping() {
        // The H3 register-hijack: an on-path attacker captures a victim's valid
        // Register and replays the identical (still-freshly-PoP'd) datagram from
        // its OWN socket. At the coordinator that lands as observe(victim, attacker_src).
        // The victim is at the nonce-0 baseline (registered, not yet keepalived),
        // and its mapping is live — so the attacker's source must NOT take over.
        let victim = NodeKey([0x77; 32]);
        let victim_src = addr(1, 4000);
        let attacker_src = addr(9, 6666);
        let mut book = AdvertBook::with_ttl(120);
        book.observe(victim, victim_src, 1_000); // victim boots, registers
        book.observe(victim, attacker_src, 1_010); // attacker replays from its own src
        assert_eq!(
            book.current(victim, 1_010),
            Some(victim_src),
            "the replayed register cannot hijack the victim's reflexive mapping"
        );
        // The victim's own keepalive readvertise (strictly-higher nonce, SAME
        // source) still works normally afterward.
        assert_eq!(
            book.readvertise(victim, victim_src, 1_011, 1_020),
            AdvertOutcome::Superseded
        );
        assert_eq!(book.current(victim, 1_020), Some(victim_src));
    }

    #[test]
    fn replayed_readvertise_from_another_source_cannot_hijack_a_live_mapping() {
        // The same H3 shape as the register-hijack, on the keepalive path: an
        // on-path attacker captures V's Readvertise (whose nonce is now
        // strictly higher than the stored one) and replays the identical
        // datagram from its own socket. `readvertise`'s nonce check alone
        // would accept it (nonce > prev.nonce), so the different-source guard
        // must catch it before `insert_fresh` ever runs.
        let victim = NodeKey([0x88; 32]);
        let victim_src = addr(1, 4000);
        let attacker_src = addr(9, 6666);
        let mut book = AdvertBook::with_ttl(120);
        book.observe(victim, victim_src, 1_000);
        assert_eq!(
            book.readvertise(victim, attacker_src, 1, 1_010),
            AdvertOutcome::SourceMismatch,
            "a replayed keepalive from a different source is refused, not applied"
        );
        assert_eq!(
            book.current(victim, 1_010),
            Some(victim_src),
            "the replayed keepalive cannot hijack the victim's reflexive mapping"
        );
        // The victim's own next keepalive, from its own source, still works.
        assert_eq!(
            book.readvertise(victim, victim_src, 2, 1_020),
            AdvertOutcome::Superseded
        );
    }

    #[test]
    fn unknown_key_has_no_current() {
        let book = AdvertBook::default();
        assert_eq!(book.current(NodeKey([0xcc; 32]), 0), None);
    }

    #[test]
    fn registration_expires_after_ttl() {
        let key = NodeKey([0x01; 32]);
        let mut book = AdvertBook::with_ttl(120);
        book.observe(key, addr(1, 4000), 1_000);
        assert_eq!(book.current(key, 1_000), Some(addr(1, 4000)));
        assert_eq!(
            book.current(key, 1_120),
            Some(addr(1, 4000)),
            "alive at exactly ttl"
        );
        assert_eq!(book.current(key, 1_121), None, "expired past ttl");
    }

    #[test]
    fn readvertise_refreshes_last_seen() {
        let key = NodeKey([0x02; 32]);
        let mut book = AdvertBook::with_ttl(120);
        book.observe(key, addr(1, 4000), 1_000);
        // keepalive at t=1_100 extends life to 1_220.
        assert_eq!(
            book.readvertise(key, addr(1, 4000), 1, 1_100),
            AdvertOutcome::Superseded
        );
        assert_eq!(book.current(key, 1_200), Some(addr(1, 4000)));
        assert_eq!(book.current(key, 1_221), None);
    }

    #[test]
    fn stale_nonce_does_not_extend_life() {
        // A replayed lower-nonce datagram must not keep a mapping alive: only a
        // fresh (strictly-higher-nonce) readvertise or a baseline observe counts.
        let key = NodeKey([0x03; 32]);
        let mut book = AdvertBook::with_ttl(120);
        book.observe(key, addr(1, 4000), 1_000);
        assert_eq!(
            book.readvertise(key, addr(1, 4000), 5, 1_010),
            AdvertOutcome::Superseded
        );
        assert_eq!(
            book.readvertise(key, addr(9, 9999), 5, 1_100),
            AdvertOutcome::Stale
        );
        assert_eq!(
            book.current(key, 1_131),
            None,
            "life still ends 120s after the LAST accepted advert"
        );
    }

    #[test]
    fn expired_entry_is_replaceable_regardless_of_nonce() {
        // The anti-rollback guard (nonce > 0 blocks a nonce-0 observe) only makes
        // sense for a LIVE mapping. Once expired, the entry is dead — a rebooted
        // node re-registering at the baseline must take the slot back.
        let key = NodeKey([0x04; 32]);
        let mut book = AdvertBook::with_ttl(120);
        book.observe(key, addr(1, 4000), 1_000);
        assert_eq!(
            book.readvertise(key, addr(1, 4000), 999_999, 1_010),
            AdvertOutcome::Superseded
        );
        // Within TTL the high-nonce guard still holds:
        book.observe(key, addr(2, 5000), 1_050);
        assert_eq!(book.current(key, 1_050), Some(addr(1, 4000)));
        // After expiry the fresh register wins:
        book.observe(key, addr(2, 5000), 2_000);
        assert_eq!(book.current(key, 2_000), Some(addr(2, 5000)));
        // ...and a fresh low-nonce readvertise also wins over an expired corpse:
        assert_eq!(
            book.readvertise(key, addr(3, 6000), 1, 3_000),
            AdvertOutcome::Superseded
        );
        assert_eq!(book.current(key, 3_000), Some(addr(3, 6000)));
    }
}
