//! Coordinator authorization: a per-request authenticator
//! (proof-of-possession plus an optional capability chain) verified
//! statelessly against PUBLIC keys: the genesis set pinned at boot. The
//! coordinator holds no secret and dials nothing; every check here is a clock
//! read plus at most one ed25519 verification per chain link.

use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use arrayvec::ArrayVec;
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

/// The most links a capability chain carries: a root a genesis validator
/// signed, plus up to three delegations under it. A deeper chain is refused
/// by the decoder and by admission alike.
pub const MAX_CAP_CHAIN: usize = 4;

/// A signed admission capability: `issuer` vouches that `subject` (implied —
/// the request's key, or the issuer of the cap whose `parent` this is) is
/// authorized until `not_after`. `parent` is the cap that admits `issuer`;
/// `None` means `issuer` must itself be a genesis validator.
#[derive(Clone, Debug, PartialEq)]
pub struct CoordCap {
    pub issuer: ed25519::PublicKey,
    pub not_after: u64,
    pub issuer_sig: ed25519::Signature,
    pub parent: Option<Box<CoordCap>>,
}

impl CoordCap {
    /// This cap and every parent above it, the link naming the subject first
    /// and the root last.
    pub fn links(&self) -> impl Iterator<Item = &CoordCap> {
        std::iter::successors(Some(self), |cap| cap.parent.as_deref())
    }
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
    /// Private coordination: PoP + admission against the genesis set pinned
    /// at boot — the subject is a genesis validator, or presents a cap chain
    /// a genesis validator roots.
    Private {
        genesis_set: Vec<ed25519::PublicKey>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    /// Timestamp outside the freshness window.
    Stale,
    /// Proof-of-possession signature did not verify against the subject key.
    BadPop,
    /// Private mode: subject is neither a genesis validator nor holds a valid
    /// cap chain one roots.
    NotAdmitted,
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

/// `subject` is 32 key bytes: a request's `NodeKey`, or a parent link's
/// subject — the issuer of the link below it.
fn cap_msg(subject: &[u8], not_after: u64) -> [u8; 40] {
    let mut m = [0; 40];
    m[..32].copy_from_slice(subject);
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

/// Mint a root capability binding `subject` (a node's ed25519 key) to
/// `not_after`, signed by `issuer` — a genesis validator's private key.
pub fn mint_coord_cap(issuer: &ed25519::PrivateKey, subject: NodeKey, not_after: u64) -> CoordCap {
    CoordCap {
        issuer: issuer.public_key(),
        not_after,
        issuer_sig: issuer.sign(COORD_CAP_NS, &cap_msg(&subject.0, not_after)),
        parent: None,
    }
}

/// Mint a capability for `subject` under `parent`, the cap that admits
/// `issuer` itself. `None` when `parent` already holds [`MAX_CAP_CHAIN`]
/// links: the chain it would make is one no coordinator admits.
pub fn delegate_coord_cap(
    parent: &CoordCap,
    issuer: &ed25519::PrivateKey,
    subject: NodeKey,
    not_after: u64,
) -> Option<CoordCap> {
    let parent_full = parent.links().nth(MAX_CAP_CHAIN - 1).is_some();
    if parent_full {
        return None;
    }
    Some(CoordCap {
        parent: Some(Box::new(parent.clone())),
        ..mint_coord_cap(issuer, subject, not_after)
    })
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

/// Private-mode admission: the subject IS a genesis validator, or presents a
/// cap chain where each link's issuer signed the one below it (the first link
/// the subject), every link is unexpired, and the root's issuer is a genesis
/// validator.
fn admit(
    genesis_set: &[ed25519::PublicKey],
    subject: NodeKey,
    cap: Option<&CoordCap>,
    now: u64,
) -> Result<(), AuthError> {
    if in_genesis(genesis_set, &subject.0) {
        return Ok(());
    }
    let Some(chain) = cap.and_then(|cap| chain_to_verify(genesis_set, cap, now)) else {
        return Err(AuthError::NotAdmitted);
    };
    let subjects =
        std::iter::once(subject.0.as_slice()).chain(chain.iter().map(|link| link.issuer.as_ref()));
    let vouched = chain
        .iter()
        .zip(subjects)
        .all(|(link, subject)| link_signed_for(link, subject));
    match vouched {
        true => Ok(()),
        false => Err(AuthError::NotAdmitted),
    }
}

fn in_genesis(genesis_set: &[ed25519::PublicKey], key: &[u8]) -> bool {
    genesis_set.iter().any(|g| g.as_ref() == key)
}

/// The links of `cap` whose signatures decide admission, or `None` when the
/// chain is refused on public facts alone: deeper than [`MAX_CAP_CHAIN`], a
/// link expired, or rooted in a key outside the genesis set. Deciding these
/// first is what refuses a stranger's chain without paying for an ed25519
/// verification.
fn chain_to_verify<'a>(
    genesis_set: &[ed25519::PublicKey],
    cap: &'a CoordCap,
    now: u64,
) -> Option<ArrayVec<&'a CoordCap, MAX_CAP_CHAIN>> {
    let mut chain = ArrayVec::new();
    for link in cap.links() {
        // a link past the bound: the chain is over-deep.
        chain.try_push(link).ok()?;
    }
    let root = chain.last()?;
    let rooted = in_genesis(genesis_set, root.issuer.as_ref());
    let unexpired = chain.iter().all(|link| link.not_after > now);
    (rooted && unexpired).then_some(chain)
}

fn link_signed_for(link: &CoordCap, subject: &[u8]) -> bool {
    link.issuer.verify(
        COORD_CAP_NS,
        &cap_msg(subject, link.not_after),
        &link.issuer_sig,
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
        AuthPolicy::Private { genesis_set } => admit(genesis_set, subject, auth.cap.as_ref(), now),
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

    /// A private policy pinned to `genesis`.
    fn private(genesis: &ed25519::PrivateKey) -> AuthPolicy {
        AuthPolicy::Private {
            genesis_set: vec![genesis.public_key()],
        }
    }

    const NOW: u64 = 2_000_000;

    /// The cap chain `issuers[0]` roots for `joiner`: each later issuer holds
    /// a cap from the one before it, and the last mints for `joiner`.
    fn chain(issuers: &[&ed25519::PrivateKey], joiner: &ed25519::PrivateKey) -> CoordCap {
        let subjects = issuers[1..].iter().copied().chain([joiner]);
        let mut held: Option<CoordCap> = None;
        for (issuer, subject) in issuers.iter().zip(subjects) {
            let subject = nk(&subject.public_key());
            held = Some(match held {
                None => mint_coord_cap(issuer, subject, NOW + 3600),
                Some(parent) => delegate_coord_cap(&parent, issuer, subject, NOW + 3600)
                    .expect("within MAX_CAP_CHAIN"),
            });
        }
        held.expect("at least one issuer")
    }

    /// `joiner`'s verdict at `NOW` presenting `cap` to a coordinator pinned to
    /// `genesis`.
    fn verdict(
        genesis: &ed25519::PrivateKey,
        joiner: &ed25519::PrivateKey,
        cap: CoordCap,
    ) -> Result<(), AuthError> {
        let auth = sign_authenticator(joiner, INNER, NOW, Some(cap));
        let subject = nk(&joiner.public_key());
        verify_request(&private(genesis), NOW, 30, subject, INNER, &auth)
    }

    #[test]
    fn a_chain_of_two_three_or_four_links_rooted_in_genesis_admits() {
        let (g, a, b, c, joiner) = (key(10), key(11), key(12), key(13), key(20));
        for issuers in [vec![&g, &a], vec![&g, &a, &b], vec![&g, &a, &b, &c]] {
            let cap = chain(&issuers, &joiner);
            assert_eq!(cap.links().count(), issuers.len());
            assert_eq!(verdict(&g, &joiner, cap), Ok(()));
        }
    }

    #[test]
    fn a_chain_that_never_reaches_genesis_is_refused() {
        let (g, a, b, joiner) = (key(10), key(11), key(12), key(20));
        // a stranger roots it, even with a genesis validator in the middle.
        for issuers in [vec![&a], vec![&a, &b], vec![&a, &g, &b]] {
            let cap = chain(&issuers, &joiner);
            assert_eq!(verdict(&g, &joiner, cap), Err(AuthError::NotAdmitted));
        }
        // a genesis validator's name on a root it never signed buys nothing.
        let mut forged = chain(&[&a], &joiner);
        forged.issuer = g.public_key();
        assert_eq!(verdict(&g, &joiner, forged), Err(AuthError::NotAdmitted));
    }

    #[test]
    fn one_expired_middle_link_refuses_the_chain() {
        let (g, a, b, joiner) = (key(10), key(11), key(12), key(20));
        let root = mint_coord_cap(&g, nk(&a.public_key()), NOW + 3600);
        let middle = delegate_coord_cap(&root, &a, nk(&b.public_key()), NOW).unwrap();
        let leaf = delegate_coord_cap(&middle, &b, nk(&joiner.public_key()), NOW + 3600).unwrap();
        assert_eq!(verdict(&g, &joiner, leaf), Err(AuthError::NotAdmitted));
    }

    #[test]
    fn one_bad_middle_signature_refuses_the_chain() {
        let (g, a, b, joiner) = (key(10), key(11), key(12), key(20));
        let mut cap = chain(&[&g, &a, &b], &joiner);
        let middle = cap.parent.as_mut().expect("three links");
        // the right issuer, over the wrong subject.
        middle.issuer_sig = a.sign(
            COORD_CAP_NS,
            &cap_msg(joiner.public_key().as_ref(), NOW + 3600),
        );
        assert_eq!(verdict(&g, &joiner, cap), Err(AuthError::NotAdmitted));
    }

    #[test]
    fn an_over_deep_chain_is_refused_at_admit_and_at_decode() {
        let (g, a, b, c, d, joiner) = (key(10), key(11), key(12), key(13), key(14), key(20));
        let full = chain(&[&g, &a, &b, &c], &d);
        let subject = nk(&joiner.public_key());
        assert_eq!(delegate_coord_cap(&full, &d, subject, NOW + 3600), None);

        // every link signed and unexpired, one too many of them.
        let over = CoordCap {
            parent: Some(Box::new(full.clone())),
            ..mint_coord_cap(&d, subject, NOW + 3600)
        };
        assert_eq!(over.links().count(), MAX_CAP_CHAIN + 1);
        assert_eq!(verdict(&g, &joiner, over), Err(AuthError::NotAdmitted));

        let full_bytes = full.encode();
        assert_eq!(CoordCap::decode(&full_bytes), Ok(full));
        let leaf_bytes = mint_coord_cap(&d, subject, NOW + 3600).encode();
        let mut over_bytes = vec![(MAX_CAP_CHAIN + 1) as u8];
        over_bytes.extend_from_slice(&leaf_bytes[1..]);
        over_bytes.extend_from_slice(&full_bytes[1..]);
        assert!(CoordCap::decode(&over_bytes).is_err());
    }

    #[test]
    fn a_stranger_rooted_chain_costs_no_signature_verification() {
        let (g, a, b, joiner) = (key(10), key(11), key(12), key(20));
        let genesis_set = [g.public_key()];
        // every link validly signed: only the root's membership refuses it,
        // and that is decided before any signature is checked.
        let stranger_rooted = chain(&[&a, &b], &joiner);
        assert!(chain_to_verify(&genesis_set, &stranger_rooted, NOW).is_none());
        let rooted = chain(&[&g, &a, &b], &joiner);
        let to_verify = chain_to_verify(&genesis_set, &rooted, NOW).expect("genesis roots it");
        assert_eq!(to_verify.len(), 3);
    }

    #[test]
    fn private_admits_genesis_member_without_cap() {
        let g = key(10);
        let subject = nk(&g.public_key());
        let policy = AuthPolicy::Private {
            genesis_set: vec![g.public_key()],
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
