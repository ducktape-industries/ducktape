//! Coordinator authorization: a per-request authenticator
//! (proof-of-possession plus an optional validator-issued capability) verified
//! statelessly against PUBLIC keys: the genesis set pinned at boot and the live
//! validator set the coordinator's node reports. The coordinator holds no
//! secret; every check here is a clock read plus one or two ed25519
//! verifications against public keys.

use std::net::SocketAddr;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use commonware_cryptography::{Signer as _, Verifier as _, ed25519};
use smallvec::SmallVec;

use crate::NodeKey;

/// Bucket width (seconds) for the return-routability cookie: a `Register` for
/// a key with no live mapping must echo a cookie minted for `from` at the
/// CURRENT or PREVIOUS bucket — wide enough that a bind-then-register that
/// straddles a bucket boundary never spuriously fails, narrow enough that a
/// captured cookie is useless within a couple of minutes.
const COOKIE_EPOCH_SECS: u64 = 60;

/// Deterministic byte encoding of a `SocketAddr` for the cookie MAC input.
/// Any injective, stable mapping works here — this one just reuses `Display`,
/// which already varies by IP family and port.
fn addr_bytes(addr: SocketAddr) -> String {
    addr.to_string()
}

/// The coordinator's boot-random cookie-minting key. A stateless
/// return-routability proof: `BindResponse` hands the caller
/// `keyed_hash(key, src ‖ epoch_minute)`, and a first-seen (or expired)
/// `Register` must echo one that verifies for its OWN observed source at the
/// current or previous minute. Nothing here is persisted — a coordinator
/// restart simply invalidates every outstanding cookie, which only costs a
/// fresh joiner one extra `BindRequest` round trip.
pub struct CookieKey([u8; 32]);

impl CookieKey {
    /// A fresh key drawn from the OS RNG at coordinator boot.
    #[cfg(feature = "runtime")]
    pub fn boot_random() -> Self {
        use rand::RngCore as _;
        let mut key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut key);
        Self(key)
    }

    fn mac(&self, src: SocketAddr, epoch: u64) -> [u8; 32] {
        let mut msg = addr_bytes(src).into_bytes();
        msg.extend_from_slice(&epoch.to_be_bytes());
        *blake3::keyed_hash(&self.0, &msg).as_bytes()
    }

    /// Mint a cookie for `src` valid at `now`'s bucket.
    pub fn mint(&self, src: SocketAddr, now: u64) -> [u8; 32] {
        self.mac(src, now / COOKIE_EPOCH_SECS)
    }

    /// Does `cookie` verify for `src` at `now`'s bucket or the one before it?
    /// The previous bucket is accepted so a cookie minted just before a
    /// boundary still verifies immediately after it.
    pub fn verify(&self, src: SocketAddr, now: u64, cookie: &[u8; 32]) -> bool {
        let epoch = now / COOKIE_EPOCH_SECS;
        *cookie == self.mac(src, epoch) || *cookie == self.mac(src, epoch.saturating_sub(1))
    }
}

/// PoP signing namespace: `sign(COORD_REQ_NS, inner_request_bytes ‖ timestamp)`.
pub const COORD_REQ_NS: &[u8] = b"ducktape-coord-req-v1";
/// Capability signing namespace: `sign(COORD_CAP_NS, subject ‖ not_after)`.
pub const COORD_CAP_NS: &[u8] = b"ducktape-coord-cap-v1";
/// Max clock skew (seconds) between a request timestamp and the coordinator.
pub const DEFAULT_FRESHNESS_WINDOW_SECS: u64 = 30;
/// Lifetime of a minted admission capability (`mint_coord_cap`), in seconds:
/// one year. Deliberately long-lived — there is NO cap-rotation flow yet (a
/// joiner receives exactly one cap over its `Admitted` gate reply and never refreshes
/// it), so a short TTL would strand admitted nodes. Rotation (re-minting +
/// re-delivering a fresh cap before expiry) is DEFERRED; when it lands this
/// TTL should shrink to match the rotation cadence.
pub const COORD_CAP_TTL_SECS: u64 = 365 * 24 * 3600;

/// How long a reading of the live validator set admits after it was taken, in
/// seconds. The coordinator re-reads its node well inside this; a reading older
/// than it describes a set that may have moved on without the coordinator
/// hearing, so a key known ONLY from it no longer admits.
pub const LIVE_VALSET_TTL_SECS: u64 = 60;

/// A signed admission capability. A validator (`issuer`) vouches that
/// `subject` (implied — the request's key) is authorized until `not_after`.
#[derive(Clone, Debug, PartialEq)]
pub struct CoordCap {
    pub issuer: ed25519::PublicKey,
    pub not_after: u64,
    pub issuer_sig: ed25519::Signature,
}

/// The per-request authenticator — the wire's "authorization header".
#[derive(Clone, Debug, PartialEq)]
pub struct Authenticator {
    pub timestamp: u64,
    pub pop_sig: ed25519::Signature,
    pub cap: Option<CoordCap>,
}

/// The coordinator's authorization policy. PUBLIC data only.
#[derive(Clone, Debug, Default)]
pub enum AuthPolicy {
    /// Public coordination: every request proves possession of its node key.
    #[default]
    Public,
    /// Private coordination: PoP + admission against the network's validators —
    /// the genesis set pinned at boot, which always admits, plus the `live` set
    /// the coordinator's node last reported, which admits inside
    /// [`LIVE_VALSET_TTL_SECS`].
    Private {
        genesis_set: Vec<ed25519::PublicKey>,
        live: LiveValset,
    },
}

/// The network's CURRENT validator set as the node a private coordinator is
/// configured to follow last reported it, and when. It is what lets a cap
/// signed by a validator promoted after genesis admit. Empty until the first
/// reading; one task records readings, every verifier reads them.
#[derive(Clone, Debug, Default)]
pub struct LiveValset(Arc<RwLock<Option<ValsetReading>>>);

#[derive(Debug)]
struct ValsetReading {
    validators: Vec<ed25519::PublicKey>,
    read_at: u64,
}

/// What the admission set says about one key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Standing {
    /// Neither a genesis validator nor named by any live reading.
    Stranger,
    /// Named ONLY by a live reading past its TTL: the node that vouched for it
    /// has not answered since, so it admits nothing.
    Stale,
    /// A genesis validator, or one a live reading names inside its TTL.
    Validator,
}

impl LiveValset {
    /// Replace the reading with `validators`, read at `read_at` (unix seconds).
    pub fn record(&self, validators: Vec<ed25519::PublicKey>, read_at: u64) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = Some(ValsetReading {
            validators,
            read_at,
        });
    }

    /// Is there no reading inside its TTL at `now`?
    pub fn expired(&self, now: u64) -> bool {
        let reading = self.0.read().unwrap_or_else(PoisonError::into_inner);
        reading
            .as_ref()
            .is_none_or(|reading| !reading.fresh_at(now))
    }

    fn standing(&self, key: &[u8], now: u64) -> Standing {
        let reading = self.0.read().unwrap_or_else(PoisonError::into_inner);
        let Some(reading) = reading.as_ref() else {
            return Standing::Stranger;
        };
        let named = reading.validators.iter().any(|v| v.as_ref() == key);
        match (named, reading.fresh_at(now)) {
            (false, _) => Standing::Stranger,
            (true, false) => Standing::Stale,
            (true, true) => Standing::Validator,
        }
    }
}

impl ValsetReading {
    fn fresh_at(&self, now: u64) -> bool {
        now < self.read_at.saturating_add(LIVE_VALSET_TTL_SECS)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    /// Timestamp outside the freshness window.
    Stale,
    /// Proof-of-possession signature did not verify against the subject key.
    BadPop,
    /// Private mode: subject is neither a validator nor holds a valid cap one
    /// signed.
    NotAdmitted,
    /// Private mode: the key that would admit (the subject, or its cap's
    /// issuer) is a validator only by a live reading past its TTL — the
    /// coordinator's node has not answered since, so it fails closed.
    ValsetStale,
    /// The request's NodeKey is not a valid ed25519 public key.
    BadSubjectKey,
}

/// Wall-clock seconds since the Unix epoch (saturating before 1970).
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn cap_msg(subject: NodeKey, not_after: u64) -> [u8; 40] {
    let mut m = [0; 40];
    m[..32].copy_from_slice(&subject.0);
    m[32..].copy_from_slice(&not_after.to_be_bytes());
    m
}

fn pop_msg(inner_bytes: &[u8], timestamp: u64) -> SmallVec<[u8; 64]> {
    // Every real wire message fits inline (Msg::MAX_ENCODED_LEN + timestamp
    // is 61 bytes); SmallVec transparently spills for oversized API callers.
    let mut message = SmallVec::with_capacity(inner_bytes.len() + 8);
    message.extend_from_slice(inner_bytes);
    message.extend_from_slice(&timestamp.to_be_bytes());
    message
}

fn subject_pubkey(subject: NodeKey) -> Option<ed25519::PublicKey> {
    use commonware_codec::DecodeExt as _;
    ed25519::PublicKey::decode(subject.0.as_slice()).ok()
}

/// Mint a capability binding `subject` (a node's ed25519 key) to `not_after`,
/// signed by `issuer` (a current validator's private key).
pub fn mint_coord_cap(issuer: &ed25519::PrivateKey, subject: NodeKey, not_after: u64) -> CoordCap {
    CoordCap {
        issuer: issuer.public_key(),
        not_after,
        issuer_sig: issuer.sign(COORD_CAP_NS, &cap_msg(subject, not_after)),
    }
}

/// Build the authenticator for one request: sign `inner_bytes ‖ timestamp`
/// with the node's identity key and attach `cap` (private mode) or `None`.
pub fn sign_authenticator(
    signer: &ed25519::PrivateKey,
    inner_bytes: &[u8],
    timestamp: u64,
    cap: Option<CoordCap>,
) -> Authenticator {
    Authenticator {
        timestamp,
        pop_sig: signer.sign(COORD_REQ_NS, &pop_msg(inner_bytes, timestamp)),
        cap,
    }
}

/// Private-mode admission: the subject IS a validator, or presents an
/// unexpired cap a validator signed for it. The genesis set always admits; the
/// live set admits inside its TTL, and a key only a lapsed reading names is
/// refused by name ([`AuthError::ValsetStale`]) rather than as a stranger.
fn admit(
    genesis_set: &[ed25519::PublicKey],
    live: &LiveValset,
    subject: NodeKey,
    cap: Option<&CoordCap>,
    now: u64,
) -> Result<(), AuthError> {
    let standing = |key: &[u8]| match genesis_set.iter().any(|g| g.as_ref() == key) {
        true => Standing::Validator,
        false => live.standing(key, now),
    };
    let member = standing(subject.0.as_slice());
    if member == Standing::Validator {
        return Ok(());
    }
    let unexpired_cap = cap.filter(|cap| cap.not_after > now);
    let issuer = unexpired_cap.map_or(Standing::Stranger, |cap| standing(cap.issuer.as_ref()));
    // the signature is checked only for an issuer the set knows: a stranger's
    // cap is refused without paying for an ed25519 verification.
    let issuer_known = issuer != Standing::Stranger;
    let vouched = issuer_known && unexpired_cap.is_some_and(|cap| cap_signed_for(cap, subject));
    let issuer = match vouched {
        true => issuer,
        false => Standing::Stranger,
    };
    match member.max(issuer) {
        Standing::Validator => Ok(()),
        Standing::Stale => Err(AuthError::ValsetStale),
        Standing::Stranger => Err(AuthError::NotAdmitted),
    }
}

fn cap_signed_for(cap: &CoordCap, subject: NodeKey) -> bool {
    cap.issuer.verify(
        COORD_CAP_NS,
        &cap_msg(subject, cap.not_after),
        &cap.issuer_sig,
    )
}

/// Stateless authorization decision for one request. `now`/`window` are seconds.
/// `subject` is the inner request's claimed key; `inner_bytes` is the inner
/// request's `Msg::encode()`.
pub fn verify_request(
    policy: &AuthPolicy,
    now: u64,
    window: u64,
    subject: NodeKey,
    inner_bytes: &[u8],
    auth: &Authenticator,
) -> Result<(), AuthError> {
    verify_request_using(
        policy,
        now,
        window,
        subject,
        inner_bytes,
        auth,
        subject_pubkey,
    )
}

/// Shared verifier with a caller-supplied public-key resolver. The coordinator
/// uses a tiny bounded cache here; standalone callers retain the exact public
/// API above and decode directly.
pub(crate) fn verify_request_using(
    policy: &AuthPolicy,
    now: u64,
    window: u64,
    subject: NodeKey,
    inner_bytes: &[u8],
    auth: &Authenticator,
    resolve_subject: impl FnOnce(NodeKey) -> Option<ed25519::PublicKey>,
) -> Result<(), AuthError> {
    // 1. Freshness.
    if now.abs_diff(auth.timestamp) > window {
        return Err(AuthError::Stale);
    }

    // 2. Proof-of-possession.
    let subj_pk = resolve_subject(subject).ok_or(AuthError::BadSubjectKey)?;
    if !subj_pk.verify(
        COORD_REQ_NS,
        &pop_msg(inner_bytes, auth.timestamp),
        &auth.pop_sig,
    ) {
        return Err(AuthError::BadPop);
    }

    // 3. Admission (private mode only).
    match policy {
        AuthPolicy::Public => Ok(()),
        AuthPolicy::Private { genesis_set, live } => {
            admit(genesis_set, live, subject, auth.cap.as_ref(), now)
        }
    }
}

#[cfg(test)]
mod tests {
    // `Signer` (for `from_seed`/`public_key`) and `ed25519` come in via the
    // parent module's imports through this glob.
    use super::*;

    fn key(seed: u64) -> ed25519::PrivateKey {
        ed25519::PrivateKey::from_seed(seed)
    }
    fn nk(pk: &ed25519::PublicKey) -> NodeKey {
        let mut b = [0u8; 32];
        b.copy_from_slice(pk.as_ref());
        NodeKey(b)
    }

    // A fixed "inner request" byte string stands in for Msg::encode() bytes.
    const INNER: &[u8] = b"\x03inner-register-bytes";

    #[test]
    fn pop_only_accepts_self_signed_and_rejects_forged() {
        let node = key(1);
        let subject = nk(&node.public_key());
        let policy = AuthPolicy::Public;
        let now = 1_000_000;

        let good = sign_authenticator(&node, INNER, now, None);
        assert_eq!(
            verify_request(&policy, now, 30, subject, INNER, &good),
            Ok(())
        );

        // Signed by a DIFFERENT key: PoP must fail.
        let attacker = key(2);
        let forged = sign_authenticator(&attacker, INNER, now, None);
        assert_eq!(
            verify_request(&policy, now, 30, subject, INNER, &forged),
            Err(AuthError::BadPop)
        );
    }

    #[test]
    fn stale_timestamp_is_rejected_both_directions() {
        let node = key(1);
        let subject = nk(&node.public_key());
        let policy = AuthPolicy::Public;
        let a = sign_authenticator(&node, INNER, 1_000_000, None);
        // 31s in the past and future both exceed the 30s window.
        assert_eq!(
            verify_request(&policy, 1_000_031, 30, subject, INNER, &a),
            Err(AuthError::Stale)
        );
        assert_eq!(
            verify_request(&policy, 999_969, 30, subject, INNER, &a),
            Err(AuthError::Stale)
        );
        // 30s exactly is still fresh.
        assert_eq!(
            verify_request(&policy, 1_000_030, 30, subject, INNER, &a),
            Ok(())
        );
    }

    /// A private policy pinned to `genesis`, following `live`.
    fn private(genesis: &ed25519::PrivateKey, live: &LiveValset) -> AuthPolicy {
        AuthPolicy::Private {
            genesis_set: vec![genesis.public_key()],
            live: live.clone(),
        }
    }

    /// The request a `joiner` carrying a cap `issuer` minted sends at `now`.
    fn capped(
        joiner: &ed25519::PrivateKey,
        issuer: &ed25519::PrivateKey,
        now: u64,
    ) -> (NodeKey, Authenticator) {
        let subject = nk(&joiner.public_key());
        let cap = mint_coord_cap(issuer, subject, now + 3600);
        (subject, sign_authenticator(joiner, INNER, now, Some(cap)))
    }

    #[test]
    fn a_cap_minted_by_a_live_validator_admits() {
        let (genesis, promoted, joiner) = (key(10), key(11), key(20));
        let live = LiveValset::default();
        let policy = private(&genesis, &live);
        let now = 2_000_000;
        let (subject, auth) = capped(&joiner, &promoted, now);

        live.record(vec![genesis.public_key(), promoted.public_key()], now);
        assert_eq!(
            verify_request(&policy, now, 30, subject, INNER, &auth),
            Ok(())
        );
        // the promoted validator itself needs no cap either.
        let own = sign_authenticator(&promoted, INNER, now, None);
        assert_eq!(
            verify_request(&policy, now, 30, nk(&promoted.public_key()), INNER, &own),
            Ok(())
        );
    }

    #[test]
    fn the_genesis_set_admits_without_any_reading() {
        let (genesis, joiner) = (key(10), key(20));
        let live = LiveValset::default();
        let policy = private(&genesis, &live);
        let now = 2_000_000;
        let (subject, auth) = capped(&joiner, &genesis, now);

        // never read, then read long ago, then read without the founder: the
        // genesis set is the floor under all three.
        assert_eq!(
            verify_request(&policy, now, 30, subject, INNER, &auth),
            Ok(())
        );
        live.record(vec![genesis.public_key()], now - 10 * LIVE_VALSET_TTL_SECS);
        assert_eq!(
            verify_request(&policy, now, 30, subject, INNER, &auth),
            Ok(())
        );
        live.record(vec![key(11).public_key()], now);
        assert_eq!(
            verify_request(&policy, now, 30, subject, INNER, &auth),
            Ok(())
        );
    }

    #[test]
    fn a_cap_minted_outside_both_sets_is_not_admitted() {
        let (genesis, promoted, outsider, joiner) = (key(10), key(11), key(12), key(20));
        let live = LiveValset::default();
        let policy = private(&genesis, &live);
        let now = 2_000_000;
        live.record(vec![genesis.public_key(), promoted.public_key()], now);

        let (subject, auth) = capped(&joiner, &outsider, now);
        assert_eq!(
            verify_request(&policy, now, 30, subject, INNER, &auth),
            Err(AuthError::NotAdmitted)
        );
        // a live validator's name on a cap it never signed buys nothing.
        let (subject, mut forged) = capped(&joiner, &outsider, now);
        forged.cap.as_mut().expect("capped").issuer = promoted.public_key();
        assert_eq!(
            verify_request(&policy, now, 30, subject, INNER, &forged),
            Err(AuthError::NotAdmitted)
        );
    }

    #[test]
    fn a_lapsed_reading_fails_closed_by_name() {
        let (genesis, promoted, joiner) = (key(10), key(11), key(20));
        let live = LiveValset::default();
        let policy = private(&genesis, &live);
        let read_at = 2_000_000;
        live.record(vec![genesis.public_key(), promoted.public_key()], read_at);

        // the node stopped answering: no reading replaces this one, and the
        // clock runs past its TTL.
        let now = read_at + LIVE_VALSET_TTL_SECS;
        assert!(live.expired(now));
        let (subject, auth) = capped(&joiner, &promoted, now);
        assert_eq!(
            verify_request(&policy, now, 30, subject, INNER, &auth),
            Err(AuthError::ValsetStale)
        );
        let own = sign_authenticator(&promoted, INNER, now, None);
        assert_eq!(
            verify_request(&policy, now, 30, nk(&promoted.public_key()), INNER, &own),
            Err(AuthError::ValsetStale)
        );
    }

    #[test]
    fn a_reading_admits_for_its_whole_window_without_a_new_one() {
        let (genesis, promoted, joiner) = (key(10), key(11), key(20));
        let live = LiveValset::default();
        let policy = private(&genesis, &live);
        let read_at = 2_000_000;
        live.record(vec![genesis.public_key(), promoted.public_key()], read_at);

        // every read since failed; the last good one still stands until the
        // final second of its window.
        let now = read_at + LIVE_VALSET_TTL_SECS - 1;
        assert!(!live.expired(now));
        let (subject, auth) = capped(&joiner, &promoted, now);
        assert_eq!(
            verify_request(&policy, now, 30, subject, INNER, &auth),
            Ok(())
        );
    }

    #[test]
    fn private_admits_genesis_member_without_cap() {
        let g = key(10);
        let subject = nk(&g.public_key());
        let policy = AuthPolicy::Private {
            genesis_set: vec![g.public_key()],
            live: LiveValset::default(),
        };
        let now = 2_000_000;
        let auth = sign_authenticator(&g, INNER, now, None);
        assert_eq!(
            verify_request(&policy, now, 30, subject, INNER, &auth),
            Ok(())
        );
    }

    #[test]
    fn private_rejects_non_member_without_cap() {
        let g = key(10);
        let outsider = key(11);
        let subject = nk(&outsider.public_key());
        let policy = AuthPolicy::Private {
            genesis_set: vec![g.public_key()],
            live: LiveValset::default(),
        };
        let now = 2_000_000;
        let auth = sign_authenticator(&outsider, INNER, now, None); // valid PoP, but not admitted
        assert_eq!(
            verify_request(&policy, now, 30, subject, INNER, &auth),
            Err(AuthError::NotAdmitted)
        );
    }

    #[test]
    fn private_admits_joiner_with_valid_genesis_cap() {
        let g = key(10);
        let joiner = key(20);
        let subject = nk(&joiner.public_key());
        let policy = AuthPolicy::Private {
            genesis_set: vec![g.public_key()],
            live: LiveValset::default(),
        };
        let now = 2_000_000;
        let cap = mint_coord_cap(&g, subject, now + 3600);
        let auth = sign_authenticator(&joiner, INNER, now, Some(cap));
        assert_eq!(
            verify_request(&policy, now, 30, subject, INNER, &auth),
            Ok(())
        );
    }

    #[test]
    fn cap_rejected_when_expired_wrong_issuer_or_wrong_subject() {
        let g = key(10);
        let notg = key(99);
        let joiner = key(20);
        let subject = nk(&joiner.public_key());
        let policy = AuthPolicy::Private {
            genesis_set: vec![g.public_key()],
            live: LiveValset::default(),
        };
        let now = 2_000_000;

        // Expired.
        let expired = mint_coord_cap(&g, subject, now - 1);
        let a1 = sign_authenticator(&joiner, INNER, now, Some(expired));
        assert_eq!(
            verify_request(&policy, now, 30, subject, INNER, &a1),
            Err(AuthError::NotAdmitted)
        );

        // Issuer not in the pinned genesis set.
        let wrong_issuer = mint_coord_cap(&notg, subject, now + 3600);
        let a2 = sign_authenticator(&joiner, INNER, now, Some(wrong_issuer));
        assert_eq!(
            verify_request(&policy, now, 30, subject, INNER, &a2),
            Err(AuthError::NotAdmitted)
        );

        // Cap minted for a DIFFERENT subject (attacker replays someone else's cap).
        let other = nk(&key(21).public_key());
        let wrong_subject = mint_coord_cap(&g, other, now + 3600);
        let a3 = sign_authenticator(&joiner, INNER, now, Some(wrong_subject));
        assert_eq!(
            verify_request(&policy, now, 30, subject, INNER, &a3),
            Err(AuthError::NotAdmitted)
        );
    }

    #[test]
    fn invalid_subject_key_bytes_are_rejected_under_pop() {
        // A NodeKey that is not a valid ed25519 point cannot verify PoP. `[2u8;
        // 32]` is not a decompressable curve25519 point in this build's
        // `ed25519::PublicKey::decode`, so admission fails at the key check
        // BEFORE PoP verification. (The plan's `[0xff; 32]` decompresses to a
        // valid point in this commonware version — its non-canonical y reduces
        // mod p — so it would surface as `BadPop`, not `BadSubjectKey`.)
        let subject = NodeKey([2u8; 32]);
        let policy = AuthPolicy::Public;
        let node = key(1);
        let auth = sign_authenticator(&node, INNER, 1_000_000, None);
        assert_eq!(
            verify_request(&policy, 1_000_000, 30, subject, INNER, &auth),
            Err(AuthError::BadSubjectKey)
        );
    }
}
