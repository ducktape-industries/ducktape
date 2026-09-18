//! the join-gate wire format (the join protocol) — how a not-yet-
//! admitted joiner asks to join, and how a gating member answers
//! AUTHORITATIVELY.
//!
//! transport: the joiner's SEALED first-contact intro ([`IntroRequest`]) IS
//! the gate request — it rides the WireGuard-tunnel doorbell, never the mesh
//! (a fresh joiner has no mesh standing to speak from). every claim
//! in an intro is verified against the INVITE TOKEN it carries (issuer
//! signature over the genesis namespace) and the joiner's proof-of-
//! possession. the gate stays synchronous: a member runs the V1–V9 checklist,
//! settles `Redeem` through consensus, and its [`IntroReply::Admitted`] IS
//! the admission — pass the gate and you already hold standing, fail it and
//! you get nothing (no tunnel, no residence, no chain state).
//!
//! json on the wire: matches the module-interface idiom, and this lane is
//! low-volume (one intro retransmit every couple of seconds per attempt).

use commonware_codec::DecodeExt as _;
use commonware_cryptography::ed25519;
use serde::{Deserialize, Serialize};

use crate::config::{INVITE_NONCE_LEN, InviteToken};

/// why a gate refused. wire-stable identifiers (the `detail` prose
/// is free to change; these are not). the terminal bit — whether the joiner
/// stops (exits) rather than failing over — is set by the member per its
/// verification checklist, not baked into the code, since `IssuerUnknown` is
/// non-terminal while every other code is terminal.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RejectCode {
    /// the request did not decode / was not well-formed.
    BadEncoding,
    /// the token signature does not verify for this network's binding.
    BadToken,
    /// the invite expired against the member's wall clock.
    Expired,
    /// the joiner's proof-of-possession does not verify.
    BadProof,
    /// the invite nonce is already redeemed (or lost a consensus race).
    Spent,
    /// the issuer is not in this member's committed valset. NON-TERMINAL —
    /// a lagging view cannot tell removed from not-yet-seen; the joiner fails
    /// over to another member.
    IssuerUnknown,
    /// the member could not settle the gate in time (timeout / submit
    /// failure). NON-TERMINAL — the joiner tries another member.
    Busy,
}

/// map a governance `Redeem` consensus-reject reason to a gate
/// reject code + terminal bit. these fire only on a race that slips past the
/// member's verification filter (chiefly a nonce another joiner redeemed first).
/// an unrecognized reason is a transient `Busy` (non-terminal) rather than a
/// permanent kill — the joiner retries and re-runs the checklist.
pub fn redeem_reject_outcome(reason: Option<&str>) -> (RejectCode, bool) {
    let r = reason.unwrap_or("");
    if r.contains("already redeemed") {
        (RejectCode::Spent, true)
    } else if r.contains("proof-of-possession") {
        (RejectCode::BadProof, true)
    } else if r.contains("does not verify for this network") {
        (RejectCode::BadToken, true)
    } else if r.contains("no longer part of this network") {
        (RejectCode::IssuerUnknown, false) // V7 stays non-terminal
    } else {
        // "already a validator" / "already holds resident standing" (the joiner
        // gained standing between the V9 check and the drain) or any unknown
        // reason: let it retry — it hits the V9 idempotent Admitted next round.
        (RejectCode::Busy, false)
    }
}

/// a decoded, signature-verified join request — what a member records for
/// approval. `verify_join_request` is the only constructor.
#[derive(Debug, Clone, PartialEq)]
pub struct VerifiedJoinRequest {
    pub joiner: ed25519::PublicKey,
    pub issuer: ed25519::PublicKey,
    pub nonce: [u8; INVITE_NONCE_LEN],
    /// the token's unix-seconds expiry, carried through to the redeem op.
    pub expires_unix_secs: u64,
}

/// why [`verify_intro`] refused an intro: [`IntroRefusal::reason`] is the
/// stable snake_case token a member logs it under, `Display` the prose.
#[derive(Debug, Clone, PartialEq)]
pub enum IntroRefusal {
    /// a key, nonce or signature field does not decode; carries which, and why.
    Malformed(String),
    /// the token signature does not verify for this network's binding.
    BadToken,
    /// the joiner's proof-of-possession does not verify.
    BadProof,
    /// the WireGuard-key binding signature does not verify.
    BadWgBinding,
    /// the signed `issued_unix_secs` is more than [`INTRO_FRESHNESS_SECS`] off
    /// the member's clock. Every signature verified first, so the joiner's
    /// WireGuard key is PROVEN: the member can seal it a refusal naming its
    /// clock as the thing to fix.
    Stale { wg_public_key: [u8; 32] },
}

impl IntroRefusal {
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "intro_malformed",
            Self::BadToken => "intro_bad_token",
            Self::BadProof => "intro_bad_proof",
            Self::BadWgBinding => "intro_bad_wg_binding",
            Self::Stale { .. } => INTRO_STALE,
        }
    }
}

impl std::fmt::Display for IntroRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(what) => f.write_str(what),
            Self::BadToken => {
                f.write_str("invite token signature does not verify for this network")
            }
            Self::BadProof => f.write_str("joiner proof-of-possession does not verify"),
            Self::BadWgBinding => f.write_str("wireguard key binding does not verify"),
            Self::Stale { .. } => write!(
                f,
                "intro signed more than {INTRO_FRESHNESS_SECS} s away from this member's clock"
            ),
        }
    }
}

/// verify an intro's join-request half against this network's binding: token
/// issuer signature and the joiner's proof-of-possession (the WireGuard-key
/// binding is [`verify_intro`]'s). membership checks (issuer still a member?
/// joiner already one?) are the CALLER's — they need current state, this
/// needs only crypto.
pub fn verify_join_request(
    msg: &IntroRequest,
    binding: &[u8],
) -> Result<VerifiedJoinRequest, IntroRefusal> {
    let IntroRequest {
        issuer,
        nonce,
        token_sig,
        joiner,
        proof,
        expires_unix_secs,
        ..
    } = msg;
    let malformed =
        |what: &str, e: commonware_codec::Error| IntroRefusal::Malformed(format!("{what}: {e}"));
    let issuer =
        ed25519::PublicKey::decode(issuer.as_slice()).map_err(|e| malformed("issuer key", e))?;
    let joiner =
        ed25519::PublicKey::decode(joiner.as_slice()).map_err(|e| malformed("joiner key", e))?;
    if nonce.len() != INVITE_NONCE_LEN {
        return Err(IntroRefusal::Malformed(format!(
            "nonce must be {INVITE_NONCE_LEN} bytes"
        )));
    }
    let mut nonce_arr = [0u8; INVITE_NONCE_LEN];
    nonce_arr.copy_from_slice(nonce);
    let sig = ed25519::Signature::decode(token_sig.as_slice())
        .map_err(|e| malformed("token signature", e))?;
    let proof =
        ed25519::Signature::decode(proof.as_slice()).map_err(|e| malformed("join proof", e))?;

    let token = InviteToken {
        issuer: issuer.clone(),
        nonce: nonce_arr,
        expires_unix_secs: *expires_unix_secs,
        sig,
    };
    // signature first (kills a tampered expiry), then proof-of-possession.
    // Every invite is bearer — no target lock — the join proof binds the
    // announcing key, and the sealed intro keeps the token off the wire.
    if !crate::config::verify_invite_token(&token, binding) {
        return Err(IntroRefusal::BadToken);
    }
    // NO expiry check here: this fn stays pure crypto (same division as
    // the membership checks) — decode enforces wall-clock expiry, consensus
    // enforces block-time expiry.
    if !crate::config::verify_join_proof(&joiner, binding, &token, &proof) {
        return Err(IntroRefusal::BadProof);
    }
    Ok(VerifiedJoinRequest {
        joiner,
        issuer,
        nonce: nonce_arr,
        expires_unix_secs: *expires_unix_secs,
    })
}

// ============================================================================
// the UDP intro — the joiner's first contact AND its gate request: a
// fresh joiner has NO path to the mesh yet (that is what the tunnel is for),
// so it announces its keys in a single sealed datagram to the inviter's intro
// listener. the token authenticates the request (mint was the admission
// decision), the join proof binds the announced ed25519 key to its holder,
// and a third signature binds the announced WIREGUARD key to that same
// identity — the receiving node installs the tunnel peer, forwards the
// request into consensus, and the acked outcome rides the same doorbell.
// everything after (redemption settle, statesync) rides the tunnel-borne
// mesh under the joiner's REAL key.
// ============================================================================

/// ed25519 signing namespace binding the announced X25519 WireGuard key to
/// the joiner identity: `sign(INTRO_WG_NAMESPACE, binding ‖ nonce ‖ wg_key)`.
pub const INTRO_WG_NAMESPACE: &[u8] = b"ducktape-invite-intro-v1";

/// the joiner's first-contact datagram: the join-request fields ("this key
/// asks to join, invited by `issuer`" — the token's fields plus the joiner
/// key and its proof-of-possession, all raw bytes; every invite is bearer,
/// so there is no target — the proof binds the announcing key and single-use
/// bounds it) plus the WireGuard half.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct IntroRequest {
    pub issuer: Vec<u8>,
    pub nonce: Vec<u8>,
    pub token_sig: Vec<u8>,
    pub joiner: Vec<u8>,
    pub proof: Vec<u8>,
    pub expires_unix_secs: u64,
    /// the joiner's X25519 WireGuard public key, raw.
    pub wg_public_key: Vec<u8>,
    /// the joiner's wall clock when it minted THIS datagram — signature
    /// covered inside `wg_sig` alongside the WireGuard key, so a captured
    /// intro cannot be replayed past [`INTRO_FRESHNESS_SECS`] ([`verify_intro`]).
    pub issued_unix_secs: u64,
    /// the joiner's signature binding `wg_public_key` and `issued_unix_secs`
    /// to its identity.
    pub wg_sig: Vec<u8>,
}

/// what a member tells a joiner in answer to a first-contact intro: the
/// sealed intro IS the gate request, so a member installs
/// the tunnel and forwards the request into consensus. The first ack reports
/// the tunnel is up while the gate settles (`Installed`); a later ack — once
/// `Redeem` commits or is refused — carries the AUTHORITATIVE outcome.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum IntroReply {
    /// tunnel installed; the gate is settling in consensus — keep waiting.
    Installed,
    /// the doorbell refused before installing a tunnel (bad token / expired).
    /// terminal for this candidate; carries no secret.
    Refused { detail: String },
    /// the AUTHORITATIVE admission: `Redeem` committed at `height` — the
    /// joiner now holds standing.
    ///
    /// `cap` carries an OPAQUE genesis-issued coordinator capability (packed
    /// `CoordCap` bytes) minted for the joiner when this network coordinates
    /// PRIVATELY and the answering member is a genesis validator — the joiner
    /// cannot receive it on the invite (its key does not exist at invite-mint
    /// time), so this reply is its only delivery channel; the seal is what
    /// keeps it off the wire. join_gate.rs stays crypto-agnostic: it moves bytes
    /// and never depends on the cap types.
    Admitted { height: u64, cap: Option<Vec<u8>> },
    /// the gate refused (member checklist or consensus). `terminal` ⇒ the joiner
    /// stops instead of failing over.
    Rejected {
        code: RejectCode,
        detail: String,
        terminal: bool,
    },
}

/// the member's answer, matched to the request by the echoed `nonce`. Post-verify
/// replies are sealed to the joiner's WG key by the caller before hitting the
/// wire; this type stays transport-agnostic.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct IntroAck {
    pub nonce: Vec<u8>,
    pub reply: IntroReply,
}

/// a verified gate request a member's doorbell forwards to its validator run
/// loop: exactly the fields `governance::Redeem` needs, lifted from the
/// opened intro. The loop runs the committed-state checks (V6/V7/V9), submits
/// `Redeem`, settles; the outcome returns via the shared gate-outcome map.
#[derive(Debug, Clone, PartialEq)]
pub struct GateForward {
    pub issuer: Vec<u8>,
    pub nonce: Vec<u8>,
    pub token_sig: Vec<u8>,
    pub joiner: Vec<u8>,
    pub proof: Vec<u8>,
    pub expires_unix_secs: u64,
}

pub fn encode_intro(m: &IntroRequest) -> Vec<u8> {
    serde_json::to_vec(m).expect("serializable")
}
pub fn decode_intro(b: &[u8]) -> Result<IntroRequest, String> {
    serde_json::from_slice(b).map_err(|e| e.to_string())
}
pub fn encode_intro_ack(m: &IntroAck) -> Vec<u8> {
    serde_json::to_vec(m).expect("serializable")
}
pub fn decode_intro_ack(b: &[u8]) -> Result<IntroAck, String> {
    serde_json::from_slice(b).map_err(|e| e.to_string())
}

/// how long a joiner's signed `issued_unix_secs` may lag (or lead) the
/// gating member's wall clock before [`verify_intro`] refuses the intro as
/// stale — a captured sealed datagram is opaque and needs no key material to
/// replay, so this is the only thing bounding how long a replay repoints the
/// inviter's tunnel endpoint. The joiner builds its intro ONCE per race and
/// retransmits the SAME signed bytes for up to the join race window
/// (`reachability::INVITE_JOIN_WINDOW_MS`, 90 s) — every retransmit inside
/// that race must still verify, so this MUST exceed the whole window plus
/// clock skew, not just one retransmit interval. Twice the race window is
/// comfortable slack.
pub const INTRO_FRESHNESS_SECS: u64 = 2 * (reachability::INVITE_JOIN_WINDOW_MS / 1_000);

/// the reason token a stale or future-dated intro is refused with.
pub const INTRO_STALE: &str = "intro_stale";

/// build the intro for `token` as `joiner`, announcing `wg_public_key`,
/// signed as issued at `issued_unix_secs` (the joiner's own wall clock).
pub fn intro_request(
    joiner: &ed25519::PrivateKey,
    binding: &[u8],
    token: &InviteToken,
    wg_public_key: [u8; 32],
    issued_unix_secs: u64,
) -> IntroRequest {
    use commonware_codec::Encode as _;
    use commonware_cryptography::Signer as _;
    let proof = crate::config::sign_join_proof(joiner, binding, token);
    let wg_msg = [
        binding,
        token.nonce.as_slice(),
        &wg_public_key,
        &issued_unix_secs.to_be_bytes(),
    ]
    .concat();
    let wg_sig = joiner.sign(INTRO_WG_NAMESPACE, &wg_msg);
    IntroRequest {
        issuer: token.issuer.as_ref().to_vec(),
        nonce: token.nonce.to_vec(),
        token_sig: token.sig.encode().as_ref().to_vec(),
        joiner: joiner.public_key().as_ref().to_vec(),
        proof: proof.encode().as_ref().to_vec(),
        expires_unix_secs: token.expires_unix_secs,
        wg_public_key: wg_public_key.to_vec(),
        issued_unix_secs,
        wg_sig: wg_sig.encode().as_ref().to_vec(),
    }
}

/// a decoded, signature-verified intro — the only constructor is
/// [`verify_intro`].
#[derive(Debug, Clone, PartialEq)]
pub struct VerifiedIntro {
    pub joiner: ed25519::PublicKey,
    pub issuer: ed25519::PublicKey,
    pub nonce: [u8; INVITE_NONCE_LEN],
    pub wg_public_key: [u8; 32],
}

/// verify an intro against this network's binding: the token issuer
/// signature, the joiner's proof-of-possession, the WireGuard-key binding
/// signature, and the signed `issued_unix_secs` freshness against `now`
/// (the gating member's wall clock) — a datagram signed more than
/// [`INTRO_FRESHNESS_SECS`] in the past or the future is refused, so a
/// captured sealed intro cannot be replayed indefinitely. membership checks
/// are the CALLER's, exactly like [`verify_join_request`].
pub fn verify_intro(
    msg: &IntroRequest,
    binding: &[u8],
    now_unix_secs: u64,
) -> Result<VerifiedIntro, IntroRefusal> {
    use commonware_cryptography::Verifier as _;
    let verified = verify_join_request(msg, binding)?;
    let wg_public_key: [u8; 32] = msg
        .wg_public_key
        .as_slice()
        .try_into()
        .map_err(|_| IntroRefusal::Malformed("wireguard key must be 32 bytes".into()))?;
    let wg_sig = ed25519::Signature::decode(msg.wg_sig.as_slice())
        .map_err(|e| IntroRefusal::Malformed(format!("wireguard key signature: {e}")))?;
    let wg_msg = [
        binding,
        verified.nonce.as_slice(),
        &wg_public_key,
        &msg.issued_unix_secs.to_be_bytes(),
    ]
    .concat();
    if !verified.joiner.verify(INTRO_WG_NAMESPACE, &wg_msg, &wg_sig) {
        return Err(IntroRefusal::BadWgBinding);
    }
    let too_old = now_unix_secs.saturating_sub(msg.issued_unix_secs) > INTRO_FRESHNESS_SECS;
    let too_new = msg.issued_unix_secs.saturating_sub(now_unix_secs) > INTRO_FRESHNESS_SECS;
    if too_old || too_new {
        return Err(IntroRefusal::Stale { wg_public_key });
    }
    Ok(VerifiedIntro {
        joiner: verified.joiner,
        issuer: verified.issuer,
        nonce: verified.nonce,
        wg_public_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::mint_invite_token;
    use commonware_cryptography::Signer as _;

    const BINDING: &[u8] = b"net#00000000@feedface";

    /// mint a far-future bearer token — these tests' default.
    fn mint_for(issuer: &ed25519::PrivateKey) -> InviteToken {
        mint_invite_token(issuer, BINDING, u64::MAX)
    }

    /// the WireGuard key the join-request tests announce — its binding
    /// signature is [`verify_intro`]'s to check, not [`verify_join_request`]'s.
    const WG_KEY: [u8; 32] = [9u8; 32];

    /// a fixed "now" the freshness tests build issued-at timestamps around.
    const NOW: u64 = 1_700_000_000;

    #[test]
    fn a_join_request_verifies() {
        let issuer = ed25519::PrivateKey::from_seed(1);
        let joiner = ed25519::PrivateKey::from_seed(2);
        let token = mint_for(&issuer);
        let msg = intro_request(&joiner, BINDING, &token, WG_KEY, NOW);

        let verified = verify_join_request(&msg, BINDING).expect("verifies");
        assert_eq!(verified.joiner, joiner.public_key());
        assert_eq!(verified.issuer, issuer.public_key());
        assert_eq!(verified.nonce, token.nonce);
        assert_eq!(verified.expires_unix_secs, u64::MAX);
    }

    #[test]
    fn any_key_may_claim_a_bearer_token_but_the_proof_must_hold() {
        // every invite is bearer: ANY key may present the token (no target
        // lock), so the containment is single-use + the join proof binding the
        // ANNOUNCING key. two different keys each verify with their own proof.
        let issuer = ed25519::PrivateKey::from_seed(1);
        let token = mint_for(&issuer);
        for seed in [2u64, 3, 4] {
            let joiner = ed25519::PrivateKey::from_seed(seed);
            let msg = intro_request(&joiner, BINDING, &token, WG_KEY, NOW);
            let v = verify_join_request(&msg, BINDING).expect("bearer verifies for any key");
            assert_eq!(v.joiner, joiner.public_key());
        }
    }

    #[test]
    fn a_foreign_binding_or_forged_proof_is_refused() {
        let issuer = ed25519::PrivateKey::from_seed(1);
        let joiner = ed25519::PrivateKey::from_seed(2);
        let token = mint_for(&issuer);
        let msg = intro_request(&joiner, BINDING, &token, WG_KEY, NOW);

        // another network refuses the same announce.
        assert!(verify_join_request(&msg, b"other-net").is_err());

        // a proof signed by a DIFFERENT key than the announced joiner fails the
        // proof-of-possession — a bearer token is not a blank cheque, the
        // announcer must hold the key it names.
        let bad_proof =
            crate::config::sign_join_proof(&ed25519::PrivateKey::from_seed(3), BINDING, &token);
        use commonware_codec::Encode as _;
        assert_eq!(
            verify_join_request(&msg, b"other-net").expect_err("refused"),
            IntroRefusal::BadToken
        );
        let forged = IntroRequest {
            proof: bad_proof.encode().as_ref().to_vec(),
            ..msg
        };
        let err = verify_join_request(&forged, BINDING).expect_err("refused");
        assert_eq!(err, IntroRefusal::BadProof);
    }

    /// each refusal logs under its own stable token: pin the strings, since
    /// an operator's grep (and a dashboard) keys on them.
    #[test]
    fn intro_refusal_reasons_are_stable_tokens() {
        let cases = [
            (
                IntroRefusal::Malformed("joiner key: short".into()),
                "intro_malformed",
            ),
            (IntroRefusal::BadToken, "intro_bad_token"),
            (IntroRefusal::BadProof, "intro_bad_proof"),
            (IntroRefusal::BadWgBinding, "intro_bad_wg_binding"),
            (
                IntroRefusal::Stale {
                    wg_public_key: WG_KEY,
                },
                "intro_stale",
            ),
        ];
        for (refusal, token) in cases {
            assert_eq!(refusal.reason(), token, "{refusal:?}");
        }
        // a field that does not decode names WHICH field in its prose.
        let issuer = ed25519::PrivateKey::from_seed(1);
        let joiner = ed25519::PrivateKey::from_seed(2);
        let mut msg = intro_request(&joiner, BINDING, &mint_for(&issuer), WG_KEY, NOW);
        msg.joiner.truncate(3);
        let err = verify_intro(&msg, BINDING, NOW).expect_err("refused");
        assert_eq!(err.reason(), "intro_malformed");
        assert!(err.to_string().starts_with("joiner key: "), "{err}");
    }

    #[test]
    fn an_admitted_intro_ack_roundtrips() {
        // the gate outcome rides the intro-ack wire now.
        let ack = IntroAck {
            nonce: vec![7u8; INVITE_NONCE_LEN],
            reply: IntroReply::Admitted {
                height: 42,
                cap: None,
            },
        };
        assert_eq!(
            decode_intro_ack(&encode_intro_ack(&ack)).expect("roundtrip"),
            ack
        );
    }

    #[test]
    fn a_rejected_intro_ack_roundtrips_each_code() {
        for (code, terminal) in [
            (RejectCode::BadEncoding, true),
            (RejectCode::BadToken, true),
            (RejectCode::Expired, true),
            (RejectCode::BadProof, true),
            (RejectCode::Spent, true),
            (RejectCode::IssuerUnknown, false),
            (RejectCode::Busy, false),
        ] {
            let ack = IntroAck {
                nonce: vec![7u8; INVITE_NONCE_LEN],
                reply: IntroReply::Rejected {
                    code,
                    detail: "prose".into(),
                    terminal,
                },
            };
            assert_eq!(
                decode_intro_ack(&encode_intro_ack(&ack)).expect("roundtrip"),
                ack
            );
        }
    }

    #[test]
    fn reject_codes_are_wire_stable() {
        // the code identifiers are wire-stable: pin the snake_case
        // strings so a rename cannot silently break a deployed joiner.
        let cases = [
            (RejectCode::BadEncoding, "bad_encoding"),
            (RejectCode::BadToken, "bad_token"),
            (RejectCode::Expired, "expired"),
            (RejectCode::BadProof, "bad_proof"),
            (RejectCode::Spent, "spent"),
            (RejectCode::IssuerUnknown, "issuer_unknown"),
            (RejectCode::Busy, "busy"),
        ];
        for (code, wire) in cases {
            let json = serde_json::to_string(&code).expect("serialize");
            assert_eq!(json, format!("\"{wire}\""), "{code:?}");
            let back: RejectCode = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, code);
        }
    }

    #[test]
    fn an_intro_roundtrips_verifies_and_pins_the_wireguard_key() {
        let issuer = ed25519::PrivateKey::from_seed(1);
        let joiner = ed25519::PrivateKey::from_seed(2);
        let token = mint_for(&issuer);
        let wg_key = [9u8; 32];
        let msg = intro_request(&joiner, BINDING, &token, wg_key, NOW);
        let decoded = decode_intro(&encode_intro(&msg)).expect("roundtrip");
        assert_eq!(decoded, msg);

        let verified = verify_intro(&decoded, BINDING, NOW).expect("verifies");
        assert_eq!(verified.joiner, joiner.public_key());
        assert_eq!(verified.issuer, issuer.public_key());
        assert_eq!(verified.wg_public_key, wg_key);

        // a substituted WireGuard key fails its binding signature.
        let mut forged = msg.clone();
        forged.wg_public_key = vec![8u8; 32];
        let err = verify_intro(&forged, BINDING, NOW).expect_err("refused");
        assert_eq!(err, IntroRefusal::BadWgBinding);

        // another network refuses the same intro.
        assert!(verify_intro(&msg, b"other-net", NOW).is_err());
    }

    /// a captured intro cannot be replayed indefinitely: past
    /// [`INTRO_FRESHNESS_SECS`] the gating member refuses it with the
    /// stable [`INTRO_STALE`] reason, in either direction (stale in the
    /// past, or implausibly signed in the future), carrying the proven
    /// WireGuard key its refusal is sealed to; a fresh one still verifies at
    /// the same wall clock.
    #[test]
    fn a_stale_or_future_intro_is_refused_a_fresh_one_verifies() {
        let issuer = ed25519::PrivateKey::from_seed(1);
        let joiner = ed25519::PrivateKey::from_seed(2);
        let token = mint_for(&issuer);

        let beyond = INTRO_FRESHNESS_SECS + 1;
        let stale = intro_request(&joiner, BINDING, &token, WG_KEY, NOW - beyond);
        let err = verify_intro(&stale, BINDING, NOW).expect_err("refused");
        assert_eq!(
            err,
            IntroRefusal::Stale {
                wg_public_key: WG_KEY
            }
        );
        assert_eq!(err.reason(), INTRO_STALE);

        let future = intro_request(&joiner, BINDING, &token, WG_KEY, NOW + beyond);
        let err = verify_intro(&future, BINDING, NOW).expect_err("refused");
        assert_eq!(
            err,
            IntroRefusal::Stale {
                wg_public_key: WG_KEY
            }
        );

        // inside the bound (e.g. a retransmit late in the join race) still
        // verifies — this is exactly what makes the window wide enough for
        // the once-signed intro this joiner retransmits for the whole race.
        let late_in_race =
            intro_request(&joiner, BINDING, &token, WG_KEY, NOW - INTRO_FRESHNESS_SECS);
        assert!(verify_intro(&late_in_race, BINDING, NOW).is_ok());

        let fresh = intro_request(&joiner, BINDING, &token, WG_KEY, NOW);
        assert!(verify_intro(&fresh, BINDING, NOW).is_ok());
    }

    #[test]
    fn an_admitted_reply_carrying_a_cap_roundtrips() {
        // the cap is opaque bytes to join_gate.rs — any blob roundtrips verbatim.
        let ack = IntroAck {
            nonce: vec![7u8; INVITE_NONCE_LEN],
            reply: IntroReply::Admitted {
                height: 7,
                cap: Some(vec![1, 2, 3, 4, 5]),
            },
        };
        assert_eq!(
            decode_intro_ack(&encode_intro_ack(&ack)).expect("roundtrip"),
            ack
        );
    }

    #[test]
    fn redeem_reasons_map_to_codes_and_terminal_bits() {
        // the classic race the gate must not mis-report: a spent nonce is a
        // permanent Spent, not a transient Busy.
        assert_eq!(
            redeem_reject_outcome(Some("invite already redeemed")),
            (RejectCode::Spent, true)
        );
        assert_eq!(
            redeem_reject_outcome(Some(
                "the inviting member is no longer part of this network"
            )),
            (RejectCode::IssuerUnknown, false)
        );
        // an unrecognized / already-standing reason retries rather than kills.
        assert_eq!(
            redeem_reject_outcome(Some("joiner is already a validator")),
            (RejectCode::Busy, false)
        );
        assert_eq!(redeem_reject_outcome(None), (RejectCode::Busy, false));
    }

    #[test]
    fn an_unrecognized_gate_reply_does_not_decode_as_an_ack() {
        let unexpected = br#"{"admitted":{"height":42,"cap":null}}"#;
        assert!(decode_intro_ack(unexpected).is_err());
    }
}
