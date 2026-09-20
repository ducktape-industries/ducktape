//! invite tokens and the signed invite blob — the whole join credential,
//! plus the on-disk token/bootstrap files a `join` persists.

use std::path::Path;

use commonware_codec::{DecodeExt as _, Encode as _};
use commonware_cryptography::{Signer as _, ed25519};
use serde::{Deserialize, Serialize};

use super::{
    CoordRef, Coordination, ModuleCode, NetworkDescriptor, Reach, ReachHint, decode_key, hex_bytes,
    ingress_of, unhex, validate_block_time_ms, validate_module_id,
};

/// the invite blob prefix. UNVERSIONED on purpose (bootstrapping posture): the
/// network re-mints invites on a format change, and a stale paste fails loudly
/// at decode.
const INVITE_PREFIX: &str = "🦆";

/// the WireGuard/overlay plane is MANDATORY, so an invite always brings up
/// the joiner's reachability plane. Kept as a named predicate
/// (call sites read as intent) rather than inlining a bare `true`.
pub fn invite_requires_reachability_defaults(_invite: &Invite) -> bool {
    true
}

// ============================================================================
// invite tokens — the capability an invite blob carries. minted by a member
// (`invite`), presented by the joiner in its sealed first-contact intro, redeemed
// in-consensus by governance's `Redeem` op: MINTING IS THE ADMISSION
// DECISION, redemption is mechanical and single-use. the canonical types and
// verification live in `governance::invite` (the same code every validator's
// in-consensus check runs); this module re-exports them and owns the
// node-side pieces — minting (OS randomness) and the on-disk token file.
// ============================================================================

pub const INVITE_GRANT_NAMESPACE: &[u8] = b"ducktape-invite-grant-v1";
pub const INVITE_JOIN_NAMESPACE: &[u8] = b"ducktape-invite-join-v1";
pub const INVITE_NONCE_LEN: usize = 16;

#[derive(Clone, Debug, PartialEq)]
pub struct InviteToken {
    pub issuer: ed25519::PublicKey,
    pub nonce: [u8; INVITE_NONCE_LEN],
    pub expires_unix_secs: u64,
    pub sig: ed25519::Signature,
}

pub fn grant_preimage(binding: &[u8], nonce: &[u8], expires: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(binding.len() + nonce.len() + 8);
    out.extend_from_slice(binding);
    out.extend_from_slice(nonce);
    out.extend_from_slice(&expires.to_le_bytes());
    out
}

pub fn verify_invite_token(token: &InviteToken, binding: &[u8]) -> bool {
    use commonware_cryptography::Verifier as _;
    let message = grant_preimage(binding, token.nonce.as_slice(), token.expires_unix_secs);
    token
        .issuer
        .verify(INVITE_GRANT_NAMESPACE, &message, &token.sig)
}

pub fn sign_join_proof(
    joiner: &ed25519::PrivateKey,
    binding: &[u8],
    token: &InviteToken,
) -> ed25519::Signature {
    let message = [
        binding,
        token.nonce.as_slice(),
        joiner.public_key().as_ref(),
    ]
    .concat();
    joiner.sign(INVITE_JOIN_NAMESPACE, &message)
}

pub fn verify_join_proof(
    joiner: &ed25519::PublicKey,
    binding: &[u8],
    token: &InviteToken,
    proof: &ed25519::Signature,
) -> bool {
    use commonware_cryptography::Verifier as _;
    let message = [binding, token.nonce.as_slice(), joiner.as_ref()].concat();
    joiner.verify(INVITE_JOIN_NAMESPACE, &message, proof)
}

/// mint a BEARER invite token binding an invite to `binding` (the genesis
/// namespace) with `expires_unix_secs`: fresh OS randomness for the
/// nonce, signed by this member's identity. minting IS the admission decision —
/// there is no target (the targeted form was dropped); whoever presents a
/// valid join proof for the nonce first redeems it, single-use.
pub fn mint_invite_token(
    signer: &ed25519::PrivateKey,
    binding: &[u8],
    expires_unix_secs: u64,
) -> InviteToken {
    let mut nonce = [0u8; INVITE_NONCE_LEN];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);
    // sign the SAME preimage every verifier checks (`grant_preimage` is `pub`
    // from the governance crate) — no restated shape to drift.
    let msg = grant_preimage(binding, &nonce, expires_unix_secs);
    InviteToken {
        issuer: signer.public_key(),
        nonce,
        expires_unix_secs,
        sig: signer.sign(INVITE_GRANT_NAMESPACE, &msg),
    }
}

const INVITE_TOKEN_FILE: &str = "invite.token";
/// packed token: `issuer(32) ‖ nonce(16) ‖ expires_le(8) ‖ sig(64)` — a
/// fixed 120 bytes (every invite is bearer; no role, no kind, no target).
const INVITE_TOKEN_LEN: usize = 32 + INVITE_NONCE_LEN + 8 + 64;

fn pack_invite_token(t: &InviteToken) -> Vec<u8> {
    let mut out = Vec::with_capacity(INVITE_TOKEN_LEN);
    out.extend_from_slice(t.issuer.as_ref());
    out.extend_from_slice(&t.nonce);
    out.extend_from_slice(&t.expires_unix_secs.to_le_bytes());
    out.extend_from_slice(t.sig.encode().as_ref());
    out
}

fn unpack_invite_token(bytes: &[u8]) -> Result<InviteToken, String> {
    if bytes.len() != INVITE_TOKEN_LEN {
        return Err(format!(
            "invite token must be {INVITE_TOKEN_LEN} bytes, got {}",
            bytes.len()
        ));
    }
    let issuer = ed25519::PublicKey::decode(&bytes[..32])
        .map_err(|e| format!("invite token issuer: {e}"))?;
    let mut nonce = [0u8; INVITE_NONCE_LEN];
    nonce.copy_from_slice(&bytes[32..32 + INVITE_NONCE_LEN]);
    let mut pos = 32 + INVITE_NONCE_LEN;
    let expires_unix_secs = u64::from_le_bytes(bytes[pos..pos + 8].try_into().expect("8 bytes"));
    pos += 8;
    let sig = ed25519::Signature::decode(&bytes[pos..])
        .map_err(|e| format!("invite token signature: {e}"))?;
    Ok(InviteToken {
        issuer,
        nonce,
        expires_unix_secs,
        sig,
    })
}

/// persist the token a `join` received beside the descriptor (0600 like the
/// identity: it is a bearer credential until used). overwrites — a re-join
/// with a fresh invite replaces a stale/spent token.
pub fn save_invite_token(dir: &Path, token: &InviteToken) -> Result<(), String> {
    use std::io::Write as _;
    let path = dir.join(INVITE_TOKEN_FILE);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(&path)
        .map_err(|e| format!("create {path:?}: {e}"))?;
    f.write_all(format!("{}\n", hex_bytes(&pack_invite_token(token))).as_bytes())
        .map_err(|e| format!("write {path:?}: {e}"))
}

/// delete the stored invite token — a CONSUMED credential must not survive to
/// confuse a later boot. called once the join gate reports `Admitted`:
/// the invite has done its one job. best-effort; a missing file is success.
pub fn delete_invite_token(dir: &Path) {
    let path = dir.join(INVITE_TOKEN_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(
            target: "ducktape::join",
            path = %path.display(),
            error = %e,
            reason = "spent_token_delete_failed",
            "spent invite token retained"
        ),
    }
}

/// the token a previous `join` stored, if any — a missing file is the normal
/// state for founders, dev-shape nodes, and manual (token-less) joins.
pub fn load_invite_token(dir: &Path) -> Result<Option<InviteToken>, String> {
    let path = dir.join(INVITE_TOKEN_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("read {path:?}: {e}"))?;
    let raw = unhex(text.trim()).map_err(|e| format!("{path:?}: {e}"))?;
    unpack_invite_token(&raw).map(Some)
}

const INVITE_WIREGUARD_FILE: &str = "invite-wireguard.toml";

/// the inviter's WireGuard bootstrap a `join` stored beside the token — what
/// the joining node dials BEFORE any p2p. `issuer` names the inviter (its
/// overlay ULA derives from it), the rest mirrors [`InviteWireGuard`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredInviteWireGuard {
    /// the inviter's ed25519 identity, hex.
    pub issuer: String,
    /// the inviter's X25519 WireGuard public key, hex.
    pub public_key: String,
    /// the inviter's underlay WireGuard UDP endpoint, `host:port`. Absent for
    /// coordinated invites, where the endpoint is resolved through the
    /// rendezvous coordinator at run time.
    pub endpoint: Option<String>,
    /// the inviter's underlay UDP intro endpoint, `host:port`. Absent for
    /// coordinated invites; the intro rides the shared WireGuard underlay
    /// socket after rendezvous.
    pub intro: Option<String>,
    /// the inviter's control-mesh listen port on the overlay.
    pub mesh_port: u16,
}

impl StoredInviteWireGuard {
    /// the inviter's X25519 key, decoded.
    pub fn public_key_bytes(&self) -> Result<[u8; 32], String> {
        let raw = unhex(&self.public_key)?;
        raw.try_into()
            .map_err(|_| "invite wireguard public_key must be 32 bytes".to_string())
    }

    /// the inviter's ed25519 identity, decoded.
    pub fn issuer_key(&self) -> Result<ed25519::PublicKey, String> {
        decode_key(&self.issuer)
    }
}

/// persist the invite's WireGuard bootstrap beside the token. overwrites —
/// a re-join with a fresh invite replaces a stale one.
pub fn save_invite_wireguard(
    dir: &Path,
    issuer: &ed25519::PublicKey,
    wg: &InviteWireGuard,
) -> Result<(), String> {
    let stored = StoredInviteWireGuard {
        issuer: hex_bytes(issuer.as_ref()),
        public_key: hex_bytes(&wg.public_key),
        endpoint: wg.endpoint.clone(),
        intro: wg.intro.clone(),
        mesh_port: wg.mesh_port,
    };
    let path = dir.join(INVITE_WIREGUARD_FILE);
    let text = toml::to_string_pretty(&stored).map_err(|e| format!("encode {path:?}: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("write {path:?}: {e}"))
}

/// the WireGuard bootstrap a previous `join` stored, if any.
pub fn load_invite_wireguard(dir: &Path) -> Result<Option<StoredInviteWireGuard>, String> {
    let path = dir.join(INVITE_WIREGUARD_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("read {path:?}: {e}"))?;
    toml::from_str(&text)
        .map(Some)
        .map_err(|e| format!("{path:?}: {e}"))
}

const INVITE_FRONTS_FILE: &str = "invite-fronts.json";

/// the on-disk shape of a persisted [`Front`] — raw key arrays as hex so the
/// file is human-readable and stable, mirroring [`StoredInviteWireGuard`].
#[derive(Serialize, Deserialize)]
struct StoredFront {
    member_key: String,
    wireguard_public_key: String,
    mesh_port: u16,
    endpoint: Option<String>,
}

/// persist the invite's fronts beside the token so a later `run` can race the
/// whole union of first-contact paths, not just the inviter. Empty fronts write
/// nothing (absence decodes as empty). Overwrites — a re-join replaces them.
pub fn save_invite_fronts(dir: &Path, fronts: &[Front]) -> Result<(), String> {
    let path = dir.join(INVITE_FRONTS_FILE);
    if fronts.is_empty() {
        // a re-join with a front-less invite must not leave a stale set behind.
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    let stored: Vec<StoredFront> = fronts
        .iter()
        .map(|f| StoredFront {
            member_key: hex_bytes(&f.member_key),
            wireguard_public_key: hex_bytes(&f.wireguard_public_key),
            mesh_port: f.mesh_port,
            endpoint: f.endpoint.clone(),
        })
        .collect();
    let text =
        serde_json::to_string_pretty(&stored).map_err(|e| format!("encode {path:?}: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("write {path:?}: {e}"))
}

/// the fronts a previous `join` stored; empty when absent. Fail-closed decode.
pub fn load_invite_fronts(dir: &Path) -> Result<Vec<Front>, String> {
    let path = dir.join(INVITE_FRONTS_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("read {path:?}: {e}")),
    };
    let stored: Vec<StoredFront> =
        serde_json::from_str(&text).map_err(|e| format!("decode {path:?}: {e}"))?;
    stored
        .into_iter()
        .map(|s| {
            let member_key = unhex(&s.member_key)?
                .try_into()
                .map_err(|_| "front member_key must be 32 bytes".to_string())?;
            let wireguard_public_key = unhex(&s.wireguard_public_key)?
                .try_into()
                .map_err(|_| "front wireguard_public_key must be 32 bytes".to_string())?;
            Ok(Front {
                member_key,
                wireguard_public_key,
                mesh_port: s.mesh_port,
                endpoint: s.endpoint,
            })
        })
        .collect()
}

// ============================================================================
// the invite blob — the whole join credential packed into one signed line.
//
// ONE format, unversioned (bootstrapping posture — a format change re-mints
// invites). the blob is a CAPABILITY, not a doorbell: it carries the descriptor (chain-id +
// genesis validators + typed reach hints), the inviter's WireGuard bootstrap
// (when the inviter runs the reachability plane), an expiry, and the invite
// token whose mint IS the admission decision. the whole envelope is signed by
// the token's issuer in a dedicated namespace, and decode FAILS CLOSED:
// envelope signature, then token-against-computed-binding (which transitively
// pins chain-id + validators — tampering either changes the genesis
// fingerprint and kills the token), then expiry.
// ============================================================================

/// ed25519 signing namespace for the invite envelope: the issuer signs every
/// packed byte that precedes the signature.
pub const INVITE_ENVELOPE_NAMESPACE: &[u8] = b"ducktape-invite-envelope";
/// how long a minted invite stays redeemable unless `--ttl-days` says
/// otherwise. single-use bounds the damage of a leaked blob; expiry bounds a
/// LOST one. THE default: the CLI verb, `POST /v1/invite` and the app all
/// resolve "no TTL given" to this one number.
pub const DEFAULT_INVITE_TTL_DAYS: u64 = 7;

/// the invite TTL bounds, in days, every minting door validates against. a
/// zero-day invite is expired the moment it is pasted; a decade-long bearer
/// credential is not an invite, it is a key left under the mat.
pub const INVITE_TTL_DAYS: std::ops::RangeInclusive<u64> = 1..=365;

const _: () = assert!(
    *INVITE_TTL_DAYS.start() <= DEFAULT_INVITE_TTL_DAYS
        && DEFAULT_INVITE_TTL_DAYS <= *INVITE_TTL_DAYS.end(),
    "the default invite TTL must sit inside the validated range"
);

/// the unix-seconds expiry of an invite minted at `now_unix_secs` for
/// `ttl_days`: refuses a TTL outside [`INVITE_TTL_DAYS`] and never wraps —
/// a wrapped expiry is an already-past one that only ever reads as "expired".
pub fn invite_expiry(now_unix_secs: u64, ttl_days: u64) -> Result<u64, String> {
    if !INVITE_TTL_DAYS.contains(&ttl_days) {
        return Err(format!(
            "ttl_days must be between {} and {}",
            INVITE_TTL_DAYS.start(),
            INVITE_TTL_DAYS.end()
        ));
    }
    ttl_days
        .checked_mul(24 * 60 * 60)
        .and_then(|secs| now_unix_secs.checked_add(secs))
        .ok_or_else(|| "invite expiry overflows the clock".to_string())
}

const INVITE_B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// the inviter's WireGuard bootstrap — everything a joiner needs to bring its
/// tunnel to the inviter up BEFORE any p2p: the inviter's X25519 public key,
/// its underlay UDP WireGuard endpoint, the UDP intro endpoint the joiner
/// announces its own keys to (token-authenticated first contact), and the
/// inviter's control-mesh listen port on the overlay (the joiner dials the
/// inviter's derived ULA at this port once the tunnel routes).
#[derive(Clone, Debug, PartialEq)]
pub struct InviteWireGuard {
    /// the inviter's X25519 WireGuard public key, raw.
    pub public_key: [u8; 32],
    /// the inviter's underlay WireGuard UDP endpoint, `host:port`. `None`
    /// means the invite uses coordinated rendezvous instead of baking in a
    /// direct endpoint.
    pub endpoint: Option<String>,
    /// the inviter's underlay UDP intro endpoint, `host:port`. `None` means
    /// the intro is sent over the coordinated WireGuard underlay socket.
    pub intro: Option<String>,
    /// the inviter's control-mesh listen port, dialed at its overlay ULA.
    pub mesh_port: u16,
}

/// one member the inviter offers as an ADDITIONAL first-contact path: the
/// joiner may bring its tunnel up against this member instead of the inviter
/// (the unified all-paths invite).
/// Only PUBLIC keys ever ride the wire; the WireGuard private key never leaves
/// the node. `endpoint` is the member's routable WireGuard UNDERLAY endpoint
/// (`host:wg_port`) when it is host-capable — the joiner dials it directly and
/// announces its intro at `wg_port + 1` (the product-wide `invite_listen`
/// default). `None` means the member is only reachable BY IDENTITY through the
/// joiner's ambient coordinator (a punchable, NAT'd member).
///
/// Fronts live OUTSIDE the genesis fingerprint (they are advisory reachability,
/// never validator identity) — see the fingerprint-exclusion test.
#[derive(Clone, Debug, PartialEq)]
pub struct Front {
    /// the member's real ed25519 node identity the joiner authenticates.
    pub member_key: [u8; 32],
    /// the member's X25519 WireGuard public key, raw.
    pub wireguard_public_key: [u8; 32],
    /// the member's control-mesh listen port, dialed at its overlay ULA once
    /// the tunnel routes.
    pub mesh_port: u16,
    /// the member's routable WireGuard underlay endpoint `host:wg_port`, or
    /// `None` for a punchable member reached by identity via the coordinator.
    pub endpoint: Option<String>,
}

/// Map a persisted mesh's signed adverts to invite [`Front`]s, skipping the
/// inviter's own advert (`own`, its raw ed25519 identity — it already rides
/// the invite as the `wireguard` bootstrap). A member with a concrete routable
/// WireGuard underlay endpoint becomes a DIRECT front (`endpoint:
/// Some(host:wg_port)` — the joiner dials it and announces its intro at
/// `wg_port + 1`); a member with no dialable underlay becomes a COORDINATED
/// front (`endpoint: None`, reached BY IDENTITY through the joiner's ambient
/// coordinator). Every registered member is at least punchable, so all
/// non-self adverts are offered as fronts.
pub fn fronts_from_adverts(
    adverts: &[wireguard::EndpointAdvertisement],
    own: &[u8; 32],
) -> Vec<Front> {
    adverts
        .iter()
        .map(|advert| &advert.record)
        .filter(|record| &record.validator_identity.0 != own)
        .map(|record| Front {
            member_key: record.validator_identity.0,
            wireguard_public_key: record.wireguard_public_key.0,
            mesh_port: record.control_endpoint.port,
            endpoint: record
                .wireguard_endpoint
                .as_ref()
                .map(|ep| ep.socket_addr().to_string()),
        })
        .collect()
}

/// a decoded, VERIFIED invite — the only constructor is [`decode_invite`].
#[derive(Clone, Debug, PartialEq)]
pub struct Invite {
    pub descriptor: NetworkDescriptor,
    pub token: InviteToken,
    /// The inviter's mandatory WireGuard bootstrap. The `endpoint`
    /// inside stays reachability-dependent (a fully-NAT'd inviter has none and
    /// is reached by identity via the coordinator), but the key material is
    /// always present so the joiner can seal its first-contact intro.
    pub wireguard: InviteWireGuard,
    /// additional first-contact paths the inviter offers (its reachable
    /// members). Empty when the inviter has no persisted mesh state. Never part
    /// of `genesis_namespace`.
    pub fronts: Vec<Front>,
    /// the coordinator the inviter rendezvouses through, `host:port` — its
    /// resolved `primary_coordinator`. `None` means the inviter runs with
    /// coordination off. A joiner's fresh node.toml takes this in place of the
    /// compiled default, so a NAT'd joiner registers with the same coordinator
    /// as the inviter instead of an unrelated public relay.
    pub coordinator: Option<String>,
    pub expires_unix_secs: u64,
}

/// encode an invite blob, signing the envelope as the token's issuer (the
/// caller must pass the same identity that minted `token`). `coordinator` is
/// the inviter's resolved primary coordinator (`None` = coordination off).
pub fn encode_invite(
    descriptor: &NetworkDescriptor,
    token: &InviteToken,
    wireguard: &InviteWireGuard,
    fronts: &[Front],
    coordinator: Option<&str>,
    signer: &ed25519::PrivateKey,
) -> Result<String, String> {
    use base64::Engine as _;
    if signer.public_key() != token.issuer {
        return Err("invite envelope must be signed by the token's issuer".into());
    }
    let mut out = pack_invite(descriptor, token, wireguard, fronts, coordinator)?;
    let sig = signer.sign(INVITE_ENVELOPE_NAMESPACE, &out);
    out.extend_from_slice(sig.encode().as_ref());
    Ok(format!("{INVITE_PREFIX}{}", INVITE_B64.encode(out)))
}

/// decode an invite blob against the real clock. fail-closed: see
/// [`decode_invite_at`].
pub fn decode_invite(blob: &str) -> Result<Invite, String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock is past the epoch")
        .as_secs();
    decode_invite_at(blob, now)
}

/// decode an invite blob at an injected clock (deterministic expiry tests).
/// fail-closed order: ① envelope signature by the embedded token issuer over
/// every preceding byte, ② token signature against the binding COMPUTED from
/// the decoded descriptor (pins chain-id + validators transitively), ③ expiry.
pub fn decode_invite_at(blob: &str, now_unix_secs: u64) -> Result<Invite, String> {
    use base64::Engine as _;
    use commonware_cryptography::Verifier as _;
    // a pasted blob routinely arrives wrapped: terminals and chat clients
    // insert newlines and spaces mid-blob. neither the 🦆 prefix nor the
    // base64url alphabet contains whitespace, so stripping ALL of it is
    // lossless — do it here so every decode path tolerates a wrapped paste.
    let compact: String = blob.chars().filter(|c| !c.is_whitespace()).collect();
    let body = compact.strip_prefix(INVITE_PREFIX).ok_or_else(|| {
        format!("not a ducktape invite (expected {INVITE_PREFIX}...) — ask for a fresh invite")
    })?;
    let bytes = INVITE_B64
        .decode(body)
        .map_err(|e| format!("invite is not valid base64url: {e}"))?;
    // the trailing 64 bytes are the envelope signature over everything before.
    let Some(signed_len) = bytes.len().checked_sub(64) else {
        return Err("invite payload truncated".into());
    };
    let (signed, sig_bytes) = bytes.split_at(signed_len);
    let sig =
        ed25519::Signature::decode(sig_bytes).map_err(|e| format!("envelope signature: {e}"))?;
    let invite = unpack_invite(signed, now_unix_secs)?;
    if !invite
        .token
        .issuer
        .verify(INVITE_ENVELOPE_NAMESPACE, signed, &sig)
    {
        return Err("invite envelope signature does not verify".into());
    }
    let binding = invite.descriptor.genesis_namespace();
    if !verify_invite_token(&invite.token, binding.as_bytes()) {
        return Err(
            "invite token does not verify against this blob's own network — the blob was \
             tampered with"
                .into(),
        );
    }
    Ok(invite)
}

/// pack the signed portion of the invite. validator hex and module code hashes
/// are decoded to raw bytes (rejecting a malformed descriptor here rather than
/// shipping it); the typed reach hints come from
/// [`NetworkDescriptor::reach_hints`] (the union of `reach` and
/// `bootstrap`-synthesised Direct hints), so a founder that only ever ran
/// `add_bootstrap` still ships a well-formed invite.
fn pack_invite(
    d: &NetworkDescriptor,
    token: &InviteToken,
    wireguard: &InviteWireGuard,
    fronts: &[Front],
    coordinator: Option<&str>,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();

    let cid = d.chain_id.as_bytes();
    out.push(
        u8::try_from(cid.len()).map_err(|_| format!("chain_id too long ({} bytes)", cid.len()))?,
    );
    out.extend_from_slice(cid);

    let vkeys = d.validator_keys()?; // hex -> raw, deduped, rejects malformed here
    out.push(
        u8::try_from(vkeys.len()).map_err(|_| format!("too many validators ({})", vkeys.len()))?,
    );
    for k in &vkeys {
        out.extend_from_slice(k.as_ref());
    }

    // the genesis module set — the OTHER half of `genesis_namespace`, so it
    // rides the invite for the same reason the validator set does: a joiner
    // that cannot recompute the founder's namespace cannot even handshake.
    // the hashes ride, the component bytes do not. `module_hashes` is the
    // mirror of `validator_keys` above — hex -> raw, id-sorted, deduped, and a
    // malformed hash is rejected HERE (naming its module) rather than shipped.
    let mhashes = d.module_hashes()?;
    out.push(
        u8::try_from(mhashes.len()).map_err(|_| format!("too many modules ({})", mhashes.len()))?,
    );
    for (id, hash) in &mhashes {
        put_str_u8(&mut out, id)?;
        out.extend_from_slice(hash);
    }
    // the genesis file's pin — the third half of `genesis_namespace`, and
    // what a joiner checks the fetched (or handed-out) genesis against
    // before installing a byte of it.
    out.extend_from_slice(&d.genesis_hash()?);
    // the network's beat — the last fingerprint fact, and the one the joiner
    // has no other source for: it runs consensus off this before it holds a
    // block.
    out.extend_from_slice(&d.block_time_ms.to_le_bytes());

    let hints = d.reach_hints()?;
    out.push(
        u8::try_from(hints.len()).map_err(|_| format!("too many reach hints ({})", hints.len()))?,
    );
    for h in &hints {
        out.extend_from_slice(h.expected_key.as_ref());
        match &h.reach {
            Reach::Direct(a) => {
                out.push(0);
                put_str_u8(&mut out, a)?;
            }
            Reach::Coordinated(c) => {
                out.push(1);
                put_str_u8(&mut out, &c.coord_addr)?;
                out.extend_from_slice(c.coord_key.as_ref());
            }
        }
    }

    // the WireGuard bootstrap is mandatory. flag 1 = a direct underlay
    // endpoint (+ intro), flag 2 = coordinated (key material only, reached by
    // identity via the coordinator). there is no flag-0 "no wireguard" shape.
    match (&wireguard.endpoint, &wireguard.intro) {
        (Some(endpoint), Some(intro)) => {
            out.push(1);
            out.extend_from_slice(&wireguard.public_key);
            put_str_u8(&mut out, endpoint)?;
            put_str_u8(&mut out, intro)?;
            out.extend_from_slice(&wireguard.mesh_port.to_le_bytes());
        }
        (None, None) => {
            out.push(2);
            out.extend_from_slice(&wireguard.public_key);
            out.extend_from_slice(&wireguard.mesh_port.to_le_bytes());
        }
        _ => return Err("wireguard invite must carry both endpoint and intro, or neither".into()),
    }

    // the coordination-mode echo: one byte inside the SIGNED envelope,
    // sourced from the descriptor (None resolves to Private, the safe
    // default) — a fresh joiner learns off the invite alone whether the
    // coordinator is private, so it knows to expect (and present) a CoordCap.
    out.push(match d.coordination() {
        Coordination::Public => 0,
        Coordination::Private => 1,
    });

    // the inviter's coordinator: flag 0 = coordination off, flag 1 = its
    // `host:port`. Inside the signed envelope and outside the fingerprint —
    // it is where members rendezvous, not who they are.
    match coordinator {
        None => out.push(0),
        Some(addr) => {
            out.push(1);
            put_str_u8(&mut out, addr)?;
        }
    }

    // the token now carries its OWN expiry (no separate blob-level field) —
    // decode enforces it from `token.expires_unix_secs`.
    // length-prefixed: the token is variable-width (bearer carries no
    // target bytes), so the reader learns how much to take.
    let tok = pack_invite_token(token);
    out.push(u8::try_from(tok.len()).expect("a packed token fits u8"));
    out.extend_from_slice(&tok);

    // the fronts block rides after the token inside the signed envelope, but is
    // never fed to `genesis_namespace` (advisory reachability, not validator
    // identity). The count is always present, including zero.
    out.push(
        u8::try_from(fronts.len()).map_err(|_| format!("too many fronts ({})", fronts.len()))?,
    );
    for f in fronts {
        out.extend_from_slice(&f.member_key);
        out.extend_from_slice(&f.wireguard_public_key);
        out.extend_from_slice(&f.mesh_port.to_le_bytes());
        match &f.endpoint {
            Some(endpoint) => {
                out.push(1);
                put_str_u8(&mut out, endpoint)?;
            }
            None => out.push(0),
        }
    }
    Ok(out)
}

/// length-prefix (u8) a short utf-8 string into the packed buffer.
fn put_str_u8(out: &mut Vec<u8>, s: &str) -> Result<(), String> {
    let b = s.as_bytes();
    out.push(
        u8::try_from(b.len()).map_err(|_| format!("string too long ({} bytes): {s:?}", b.len()))?,
    );
    out.extend_from_slice(b);
    Ok(())
}

/// inverse of [`pack_invite`] (the signed portion — the caller has already
/// split the envelope signature off); yields a descriptor canonicalized
/// exactly as [`NetworkDescriptor::from_toml`] would (sorted validators,
/// id-sorted modules, sorted canonical reach) so the genesis fingerprint of a
/// decoded invite matches the founder's. signature verification is the CALLER's
/// ([`decode_invite_at`]) — this only parses and enforces expiry.
fn unpack_invite(bytes: &[u8], now_unix_secs: u64) -> Result<Invite, String> {
    let mut r = InviteReader::new(bytes);
    let cid_len = r.u8()? as usize;
    let chain_id =
        String::from_utf8(r.take(cid_len)?.to_vec()).map_err(|e| format!("chain_id: {e}"))?;

    let vcount = r.u8()? as usize;
    let mut validators = Vec::with_capacity(vcount);
    for _ in 0..vcount {
        validators.push(hex_bytes(r.take(32)?));
    }
    validators.sort();

    let mcount = r.u8()? as usize;
    let mut modules = Vec::with_capacity(mcount);
    for _ in 0..mcount {
        let id = r.take_str_u8()?;
        // a blob is untrusted input: an id carrying `=` or a newline could
        // otherwise smuggle two module lines into one, folding a foreign set
        // onto this network's fingerprint.
        validate_module_id(&id)?;
        modules.push(ModuleCode {
            id,
            code_hash: hex_bytes(r.take(32)?),
        });
    }
    modules.sort_by(|a, b| a.id.cmp(&b.id));
    let genesis = hex_bytes(r.take(32)?);
    let block_time_ms = u64::from_le_bytes(r.take(8)?.try_into().expect("8 bytes"));
    validate_block_time_ms(block_time_ms)?;

    let hcount = r.u8()? as usize;
    let mut reach = Vec::with_capacity(hcount);
    for _ in 0..hcount {
        let expected_key = r.take_key()?;
        let reach_val = match r.u8()? {
            0 => Reach::Direct(r.take_str_u8()?),
            1 => {
                let coord_addr = r.take_str_u8()?;
                let coord_key = r.take_key()?;
                Reach::Coordinated(CoordRef {
                    coord_addr,
                    coord_key,
                })
            }
            other => return Err(format!("unknown reach tag {other} in invite")),
        };
        reach.push(
            ReachHint {
                expected_key,
                reach: reach_val,
            }
            .to_canonical(),
        );
    }
    reach.sort();

    // the WireGuard bootstrap is mandatory; flag 0 is invalid.
    let wireguard = match r.u8()? {
        1 => {
            let mut public_key = [0u8; 32];
            public_key.copy_from_slice(r.take(32)?);
            let endpoint = r.take_str_u8()?;
            let intro = r.take_str_u8()?;
            let mesh_port = u16::from_le_bytes(r.take(2)?.try_into().expect("2 bytes"));
            InviteWireGuard {
                public_key,
                endpoint: Some(endpoint),
                intro: Some(intro),
                mesh_port,
            }
        }
        2 => {
            let mut public_key = [0u8; 32];
            public_key.copy_from_slice(r.take(32)?);
            let mesh_port = u16::from_le_bytes(r.take(2)?.try_into().expect("2 bytes"));
            InviteWireGuard {
                public_key,
                endpoint: None,
                intro: None,
                mesh_port,
            }
        }
        0 => {
            return Err(
                "this invite carries no WireGuard bootstrap — the reachability plane is required"
                    .into(),
            );
        }
        other => return Err(format!("unknown wireguard flag {other} in invite")),
    };

    // the coordination-mode echo — decoded back into the descriptor so the
    // joiner's workspace records whether the coordinator expects a CoordCap.
    let coordination = match r.u8()? {
        0 => Some("public".to_string()),
        1 => Some("private".to_string()),
        other => return Err(format!("unknown coordination mode {other} in invite")),
    };

    // the inviter's coordinator. The joiner writes it into node.toml and dials
    // it, and a blob is untrusted input: it must name a dialable host:port.
    let coordinator = match r.u8()? {
        0 => None,
        1 => {
            let addr = r.take_str_u8()?;
            if ingress_of(&addr)?.is_none() {
                return Err(format!(
                    "invite names coordinator {addr:?}, which is not dialable"
                ));
            }
            Some(addr)
        }
        other => return Err(format!("unknown coordinator flag {other} in invite")),
    };

    // the token carries its own expiry now (no separate blob-level field);
    // enforce it against the injected clock right after unpacking.
    let tok_len = r.u8()? as usize;
    let token = unpack_invite_token(r.take(tok_len)?)?;
    let expires_unix_secs = token.expires_unix_secs;
    if now_unix_secs >= expires_unix_secs {
        return Err("this invite has expired — ask for a fresh one".into());
    }

    let fcount = r.u8()? as usize;
    let mut fronts = Vec::with_capacity(fcount);
    for _ in 0..fcount {
        let mut member_key = [0u8; 32];
        member_key.copy_from_slice(r.take(32)?);
        let mut wireguard_public_key = [0u8; 32];
        wireguard_public_key.copy_from_slice(r.take(32)?);
        let mesh_port = u16::from_le_bytes(r.take(2)?.try_into().expect("2 bytes"));
        let endpoint = match r.u8()? {
            0 => None,
            1 => Some(r.take_str_u8()?),
            other => return Err(format!("unknown front endpoint flag {other} in invite")),
        };
        fronts.push(Front {
            member_key,
            wireguard_public_key,
            mesh_port,
            endpoint,
        });
    }
    if !r.done() {
        return Err("invite payload has trailing bytes".into());
    }
    Ok(Invite {
        descriptor: NetworkDescriptor {
            chain_id,
            validators,
            // a decoded invite carries dial hints as typed `reach`; `bootstrap`
            // stays empty and both feed one dial source via `reach_hints`.
            bootstrap: Vec::new(),
            reach,
            coordination,
            genesis,
            block_time_ms,
            modules,
        },
        token,
        wireguard,
        fronts,
        coordinator,
        expires_unix_secs,
    })
}

/// a bounds-checked forward cursor over the packed invite bytes.
struct InviteReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> InviteReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn u8(&mut self) -> Result<u8, String> {
        let byte = *self
            .bytes
            .get(self.pos)
            .ok_or_else(|| "invite payload truncated".to_string())?;
        self.pos += 1;
        Ok(byte)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| "invite length overflow".to_string())?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or_else(|| "invite payload truncated".to_string())?;
        self.pos = end;
        Ok(slice)
    }

    fn done(&self) -> bool {
        self.pos == self.bytes.len()
    }

    /// take a u8-length-prefixed utf-8 string (inverse of [`put_str_u8`]).
    fn take_str_u8(&mut self) -> Result<String, String> {
        let len = self.u8()? as usize;
        Ok(std::str::from_utf8(self.take(len)?)
            .map_err(|e| format!("invite string: {e}"))?
            .to_string())
    }

    /// take a raw 32-byte ed25519 public key.
    fn take_key(&mut self) -> Result<ed25519::PublicKey, String> {
        ed25519::PublicKey::decode(self.take(32)?).map_err(|e| format!("invite public key: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ducktape-config-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    #[test]
    fn coordinated_invite_needs_reachability_defaults_without_tunnel_bootstrap() {
        let issuer = ed25519::PrivateKey::from_seed(7);
        let mut d = NetworkDescriptor {
            chain_id: "ducktape#a1b2c3d4".into(),
            validators: vec![hex_bytes(issuer.public_key().as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: crate::DEFAULT_BLOCK_TIME_MS,
            genesis: "ab".repeat(32),
            modules: Vec::new(),
        };
        d.apply_primary_coordinator(&issuer.public_key(), "relay.ducktape.industries:3478")
            .expect("coordinator hint");
        let invite = decode_invite(&encode_test_invite(&d, &issuer, None)).expect("decode");

        assert_eq!(invite.wireguard.endpoint, None);
    }

    /// a minimal coordinated (endpoint-less) WireGuard bootstrap — the default a
    /// test invite carries when it does not specify one.
    fn coordinated_test_wg() -> InviteWireGuard {
        InviteWireGuard {
            public_key: [0u8; 32],
            endpoint: None,
            intro: None,
            mesh_port: 52200,
        }
    }

    /// mint + encode with the test defaults: issuer-signed, bearer,
    /// far-future expiry. A caller that passes no WireGuard gets the minimal
    /// coordinated default, since invites always carry a bootstrap.
    fn encode_test_invite(
        d: &NetworkDescriptor,
        issuer: &ed25519::PrivateKey,
        wireguard: Option<&InviteWireGuard>,
    ) -> String {
        let token = mint_invite_token(issuer, d.genesis_namespace().as_bytes(), u64::MAX);
        let default_wg = coordinated_test_wg();
        let wg = wireguard.unwrap_or(&default_wg);
        encode_invite(d, &token, wg, &[], None, issuer).expect("encode")
    }

    /// a two-module genesis set, deliberately NOT in id order so the codec's
    /// canonical sort is what makes the two sides agree.
    fn test_modules() -> Vec<ModuleCode> {
        vec![
            ModuleCode {
                id: "pages".into(),
                code_hash: "11".repeat(32),
            },
            ModuleCode {
                id: "chat".into(),
                code_hash: "22".repeat(32),
            },
        ]
    }

    #[test]
    fn the_invite_carries_the_genesis_module_hashes() {
        let issuer = ed25519::PrivateKey::from_seed(7);
        let d = NetworkDescriptor {
            chain_id: "ducktape#a1b2c3d4".into(),
            validators: vec![hex_bytes(issuer.public_key().as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: crate::DEFAULT_BLOCK_TIME_MS,
            genesis: "ab".repeat(32),
            modules: test_modules(),
        };
        let invite = decode_invite(&encode_test_invite(&d, &issuer, None)).expect("roundtrip");
        // the genesis pin rides too: a joiner checks the file it gets against it.
        assert_eq!(invite.descriptor.genesis, d.genesis);
        // ids and hashes survive, id-sorted on both sides.
        assert_eq!(
            invite.descriptor.modules,
            vec![
                ModuleCode {
                    id: "chat".into(),
                    code_hash: "22".repeat(32),
                },
                ModuleCode {
                    id: "pages".into(),
                    code_hash: "11".repeat(32),
                },
            ]
        );
        // and therefore the joiner computes the founder's namespace — without
        // the module block it would compute the validators-only one instead.
        assert_eq!(invite.descriptor.genesis_namespace(), d.genesis_namespace());
    }

    /// the beat rides the blob, because a joiner has no other source for it —
    /// it runs consensus off the invite before it holds a single block. It is
    /// also fingerprinted, so a member that somehow disagrees cannot handshake.
    #[test]
    fn the_invite_carries_the_networks_beat() {
        let issuer = ed25519::PrivateKey::from_seed(7);
        let d = NetworkDescriptor {
            chain_id: "ducktape#a1b2c3d4".into(),
            validators: vec![hex_bytes(issuer.public_key().as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: 250,
            genesis: "ab".repeat(32),
            modules: test_modules(),
        };
        let invite = decode_invite(&encode_test_invite(&d, &issuer, None)).expect("roundtrip");
        assert_eq!(invite.descriptor.block_time_ms, 250);
        assert_eq!(invite.descriptor.genesis_namespace(), d.genesis_namespace());
    }

    /// a zero beat collapses every consensus timer, and a blob is untrusted
    /// input — the decoder refuses it before the envelope is even verified.
    #[test]
    fn a_zero_beat_is_refused_at_decode() {
        let issuer = ed25519::PrivateKey::from_seed(7);
        let d = NetworkDescriptor {
            chain_id: "ducktape#a1b2c3d4".into(),
            validators: vec![hex_bytes(issuer.public_key().as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: 0,
            genesis: "ab".repeat(32),
            modules: Vec::new(),
        };
        let err = decode_invite(&encode_test_invite(&d, &issuer, None))
            .expect_err("a zero beat never decodes");
        assert!(err.contains("block_time_ms"), "{err}");
    }

    #[test]
    fn a_malformed_module_hash_is_refused_at_encode_naming_the_module() {
        let issuer = ed25519::PrivateKey::from_seed(7);
        let d = NetworkDescriptor {
            chain_id: "ducktape#a1b2c3d4".into(),
            validators: vec![hex_bytes(issuer.public_key().as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: crate::DEFAULT_BLOCK_TIME_MS,
            genesis: "ab".repeat(32),
            modules: vec![ModuleCode {
                id: "pages".into(),
                code_hash: "11".repeat(16), // 32 hex chars — half a sha256.
            }],
        };
        let token = mint_invite_token(&issuer, d.genesis_namespace().as_bytes(), u64::MAX);
        let err = encode_invite(&d, &token, &coordinated_test_wg(), &[], None, &issuer)
            .expect_err("a short hash never ships");
        assert!(err.contains("pages"), "{err}");
    }

    #[test]
    fn a_module_id_carrying_a_fingerprint_delimiter_is_refused_at_decode() {
        let issuer = ed25519::PrivateKey::from_seed(7);
        let d = NetworkDescriptor {
            chain_id: "ducktape#a1b2c3d4".into(),
            validators: vec![hex_bytes(issuer.public_key().as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: crate::DEFAULT_BLOCK_TIME_MS,
            genesis: "ab".repeat(32),
            modules: vec![ModuleCode {
                id: "pages".into(),
                code_hash: "11".repeat(32),
            }],
        };
        let token = mint_invite_token(&issuer, d.genesis_namespace().as_bytes(), u64::MAX);
        let mut bytes = pack_invite(&d, &token, &coordinated_test_wg(), &[], None).unwrap();
        // rewrite the packed id "pages" -> "p=ges" in place: same length, so
        // the framing stays intact and only the id turns hostile. a blob is
        // untrusted input — the encoder's guard is not the decoder's.
        let at = bytes
            .windows(5)
            .position(|w| w == b"pages")
            .expect("the module id rides the packed blob");
        bytes[at + 1] = b'=';
        let err = unpack_invite(&bytes, 0).expect_err("a delimiter in an id never decodes");
        assert!(err.contains("p=ges"), "{err}");
    }

    #[test]
    fn invite_blob_roundtrips_and_verifies() {
        let issuer = ed25519::PrivateKey::from_seed(7);
        let me = issuer.public_key();
        let mut d = NetworkDescriptor {
            chain_id: "ducktape#a1b2c3d4".into(),
            validators: vec![hex_bytes(me.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: crate::DEFAULT_BLOCK_TIME_MS,
            genesis: "ab".repeat(32),
            modules: test_modules(),
        };
        d.add_bootstrap(&me, "127.0.0.1:52200");
        // decode NORMALISES bootstrap hints into typed Direct reach hints, so
        // the decoded descriptor dials the same members via `reach` even
        // though it carries no `bootstrap`.
        let invite = decode_invite(&encode_test_invite(&d, &issuer, None)).expect("roundtrip");
        assert_eq!(
            invite.descriptor.reach_hints().expect("decoded hints"),
            d.reach_hints().expect("source hints"),
            "the dial hints survive as typed reach hints"
        );
        assert_eq!(invite.wireguard, coordinated_test_wg());
        let binding = d.genesis_namespace();
        // the token was minted under the FOUNDER's namespace (`cli.rs` mints
        // against the on-disk descriptor) and `decode_invite` verifies it
        // against the namespace it recomputes from the DECODED one — so a
        // decode that succeeded already proves the two agree. assert it
        // outright, because that agreement is what the module block buys.
        assert_eq!(invite.descriptor.genesis_namespace(), binding);
        assert!(verify_invite_token(&invite.token, binding.as_bytes()));

        // a HOSTNAME dial hint survives verbatim (stored as a string, resolved
        // only at dial time — never at encode/decode).
        let other = ed25519::PrivateKey::from_seed(8).public_key();
        d.add_bootstrap(&other, "node.ducktape.industries:443");
        let invite = decode_invite(&encode_test_invite(&d, &issuer, None)).expect("roundtrip");
        assert!(
            invite
                .descriptor
                .reach
                .iter()
                .any(|r| r.ends_with("@node.ducktape.industries:443"))
        );

        // the joiner's proof-of-possession verifies for the signing key only.
        let joiner = ed25519::PrivateKey::from_seed(9);
        let proof = sign_join_proof(&joiner, binding.as_bytes(), &invite.token);
        assert!(verify_join_proof(
            &joiner.public_key(),
            binding.as_bytes(),
            &invite.token,
            &proof
        ));
        let thief = ed25519::PrivateKey::from_seed(10).public_key();
        assert!(
            !verify_join_proof(&thief, binding.as_bytes(), &invite.token, &proof),
            "a substituted key fails the proof"
        );
    }

    #[test]
    fn a_line_wrapped_paste_decodes() {
        let issuer = ed25519::PrivateKey::from_seed(7);
        let me = issuer.public_key();
        let mut d = NetworkDescriptor {
            chain_id: "ducktape#a1b2c3d4".into(),
            validators: vec![hex_bytes(me.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: crate::DEFAULT_BLOCK_TIME_MS,
            genesis: "ab".repeat(32),
            modules: Vec::new(),
        };
        d.add_bootstrap(&me, "127.0.0.1:52200");
        let blob = encode_test_invite(&d, &issuer, None);

        // re-wrap the blob the way a terminal paste arrives: hard-wrapped
        // lines mid-body, stray spaces, and surrounding blank space.
        let mut wrapped = String::from("  \n");
        for (i, c) in blob.chars().enumerate() {
            wrapped.push(c);
            if i % 41 == 40 {
                wrapped.push('\n');
            }
        }
        wrapped.push_str(" \n");

        let invite = decode_invite(&wrapped).expect("wrapped paste decodes");
        assert_eq!(invite.descriptor.chain_id, d.chain_id);
    }

    #[test]
    fn a_bearer_token_roundtrips_the_file_and_blob_codecs() {
        let issuer = ed25519::PrivateKey::from_seed(7);
        let me = issuer.public_key();
        let mut d = NetworkDescriptor {
            chain_id: "ducktape#a1b2c3d4".into(),
            validators: vec![hex_bytes(me.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: crate::DEFAULT_BLOCK_TIME_MS,
            genesis: "ab".repeat(32),
            modules: Vec::new(),
        };
        d.add_bootstrap(&me, "127.0.0.1:52200");
        let binding = d.genesis_namespace();
        let token = mint_invite_token(&issuer, binding.as_bytes(), u64::MAX);
        assert!(verify_invite_token(&token, binding.as_bytes()));

        // packed-token codec: fixed width, exact roundtrip.
        let packed = pack_invite_token(&token);
        assert_eq!(packed.len(), INVITE_TOKEN_LEN);
        assert_eq!(
            unpack_invite_token(&packed).expect("token roundtrip"),
            token
        );

        // token FILE roundtrip (the joining side persists it 0600).
        let dir = tmp("bearer-token-file");
        save_invite_token(&dir, &token).expect("save");
        assert_eq!(load_invite_token(&dir).expect("load"), Some(token.clone()));

        // full blob roundtrip: the variable-width token rides length-prefixed.
        let blob =
            encode_invite(&d, &token, &coordinated_test_wg(), &[], None, &issuer).expect("encode");
        let invite = decode_invite(&blob).expect("decode");
        assert_eq!(invite.token, token);

        // ANY key may prove possession against a bearer token.
        let redeemer = ed25519::PrivateKey::from_seed(11);
        let proof = sign_join_proof(&redeemer, binding.as_bytes(), &invite.token);
        assert!(verify_join_proof(
            &redeemer.public_key(),
            binding.as_bytes(),
            &invite.token,
            &proof
        ));
    }

    #[test]
    fn invite_blob_carries_the_wireguard_bootstrap() {
        let issuer = ed25519::PrivateKey::from_seed(7);
        let d = NetworkDescriptor {
            chain_id: "ducktape#a1b2c3d4".into(),
            validators: vec![hex_bytes(issuer.public_key().as_ref())],
            bootstrap: vec![],
            reach: vec![],
            // the invite echoes the coordination mode;
            // a `None` source resolves to Private, so it decodes as an EXPLICIT
            // "private" (semantically identical — see `coordination()`).
            coordination: Some("private".into()),
            block_time_ms: crate::DEFAULT_BLOCK_TIME_MS,
            genesis: "ab".repeat(32),
            modules: Vec::new(),
        };
        let wg = InviteWireGuard {
            public_key: [42u8; 32],
            endpoint: Some("203.0.113.7:51820".into()),
            intro: Some("203.0.113.7:51821".into()),
            mesh_port: 52200,
        };
        let invite = decode_invite(&encode_test_invite(&d, &issuer, Some(&wg))).expect("decode");
        assert_eq!(invite.wireguard, wg);
        // a wireguard invite with no fronts decodes to an empty set.
        assert!(invite.fronts.is_empty());
    }

    /// The inviter's coordinator rides the signed envelope both ways: a named
    /// `host:port` and coordination off. A blob naming a coordinator nobody
    /// can dial is refused at decode, before a joiner writes it anywhere.
    #[test]
    fn invite_blob_carries_the_inviters_coordinator() {
        let issuer = ed25519::PrivateKey::from_seed(7);
        let d = front_descriptor(&issuer);
        let token = mint_invite_token(&issuer, d.genesis_namespace().as_bytes(), u64::MAX);
        let wg = coordinated_test_wg();
        let decoded = |coordinator: Option<&str>| {
            let blob = encode_invite(&d, &token, &wg, &[], coordinator, &issuer).expect("encode");
            decode_invite(&blob)
        };

        let named = decoded(Some("coord.example.net:3478")).expect("decode");
        assert_eq!(named.coordinator.as_deref(), Some("coord.example.net:3478"));
        let off = decoded(None).expect("decode");
        assert_eq!(off.coordinator, None);

        let err = decoded(Some("coord.example.net:0")).expect_err("an undialable coordinator");
        assert!(err.contains("not dialable"), "{err}");
    }

    #[test]
    fn invite_without_the_front_count_is_rejected() {
        let issuer = ed25519::PrivateKey::from_seed(7);
        let d = NetworkDescriptor {
            chain_id: "ducktape#a1b2c3d4".into(),
            validators: vec![hex_bytes(issuer.public_key().as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: Some("private".into()),
            block_time_ms: crate::DEFAULT_BLOCK_TIME_MS,
            genesis: "ab".repeat(32),
            modules: Vec::new(),
        };
        let token = mint_invite_token(&issuer, d.genesis_namespace().as_bytes(), u64::MAX);
        let mut bytes = pack_invite(&d, &token, &coordinated_test_wg(), &[], None).unwrap();
        bytes.pop();
        assert!(unpack_invite(&bytes, 0).is_err());
    }

    /// mint + encode with the test defaults, carrying a set of fronts. A caller
    /// that passes no WireGuard gets the minimal coordinated default.
    fn encode_test_invite_with_fronts(
        d: &NetworkDescriptor,
        issuer: &ed25519::PrivateKey,
        wireguard: Option<&InviteWireGuard>,
        fronts: &[Front],
    ) -> String {
        let token = mint_invite_token(issuer, d.genesis_namespace().as_bytes(), u64::MAX);
        let default_wg = coordinated_test_wg();
        let wg = wireguard.unwrap_or(&default_wg);
        encode_invite(d, &token, wg, fronts, None, issuer).expect("encode")
    }

    fn front_descriptor(issuer: &ed25519::PrivateKey) -> NetworkDescriptor {
        NetworkDescriptor {
            chain_id: "ducktape#a1b2c3d4".into(),
            validators: vec![hex_bytes(issuer.public_key().as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: Some("private".into()),
            block_time_ms: crate::DEFAULT_BLOCK_TIME_MS,
            genesis: "ab".repeat(32),
            modules: Vec::new(),
        }
    }

    #[test]
    fn invite_blob_roundtrips_fronts() {
        let issuer = ed25519::PrivateKey::from_seed(7);
        let d = front_descriptor(&issuer);
        // one host-capable member (direct endpoint), one punchable (coordinated).
        let fronts = vec![
            Front {
                member_key: [11u8; 32],
                wireguard_public_key: [12u8; 32],
                mesh_port: 52201,
                endpoint: Some("198.51.100.9:51820".into()),
            },
            Front {
                member_key: [21u8; 32],
                wireguard_public_key: [22u8; 32],
                mesh_port: 52202,
                endpoint: None,
            },
        ];
        let blob = encode_test_invite_with_fronts(&d, &issuer, None, &fronts);
        let invite = decode_invite(&blob).expect("decode");
        assert_eq!(invite.fronts, fronts);
    }

    fn sample_advert(
        seed: u64,
        octet: u8,
        wireguard_endpoint: Option<u16>,
    ) -> wireguard::EndpointAdvertisement {
        use std::net::{IpAddr, Ipv4Addr};
        use wireguard::{
            AdmissionRoot, Endpoint, EndpointRecord, MeshVersion, PortPolicy, Root, Transport,
            ValidatorIdentity, X25519PublicKey,
        };
        let policy = PortPolicy::production();
        let signer = ed25519::PrivateKey::from_seed(seed);
        let endpoint = |port: u16, transport| {
            Endpoint::new(
                IpAddr::V4(Ipv4Addr::new(8, 8, 8, octet)),
                port,
                transport,
                &policy,
            )
            .unwrap()
        };
        let record = EndpointRecord {
            namespace: "net#fronts".into(),
            epoch: 3,
            valset_root: Root([1; 32]),
            admission_root: AdmissionRoot([2; 32]),
            validator_identity: ValidatorIdentity::try_from(signer.public_key().as_ref()).unwrap(),
            wireguard_public_key: X25519PublicKey([octet; 32]),
            control_endpoint: endpoint(443, Transport::Tcp),
            wireguard_endpoint: wireguard_endpoint.map(|port| endpoint(port, Transport::Udp)),
            nonce: 1,
        };
        wireguard::EndpointAdvertisement::sign(record, MeshVersion([7; 32]), &signer)
    }

    #[test]
    fn fronts_from_adverts_maps_reachable_members_and_skips_self() {
        // three adverts: self (skipped), a host-capable member (direct
        // endpoint), and a punchable member (no underlay endpoint → coordinated).
        let me = ed25519::PrivateKey::from_seed(1);
        let host_capable = sample_advert(2, 20, Some(51820));
        let punchable = sample_advert(3, 30, None);
        let adverts = vec![
            sample_advert(1, 10, Some(51820)),
            host_capable.clone(),
            punchable.clone(),
        ];

        let own: [u8; 32] = me.public_key().as_ref().try_into().unwrap();
        let fronts = fronts_from_adverts(&adverts, &own);

        assert_eq!(fronts.len(), 2, "the inviter's own advert is skipped");
        let direct = fronts
            .iter()
            .find(|f| f.member_key == host_capable.record.validator_identity.0)
            .expect("host-capable front");
        assert_eq!(direct.endpoint.as_deref(), Some("8.8.8.20:51820"));
        assert_eq!(direct.mesh_port, 443);
        assert_eq!(direct.wireguard_public_key, [20u8; 32]);
        let coordinated = fronts
            .iter()
            .find(|f| f.member_key == punchable.record.validator_identity.0)
            .expect("punchable front");
        assert_eq!(coordinated.endpoint, None);
    }

    #[test]
    fn fronts_are_excluded_from_the_genesis_fingerprint() {
        // two invites that differ ONLY in their fronts must fingerprint
        // identically: fronts are advisory reachability, never validator
        // identity, so `genesis_namespace` cannot see them.
        let issuer = ed25519::PrivateKey::from_seed(7);
        let d = front_descriptor(&issuer);
        let a = decode_invite(&encode_test_invite_with_fronts(&d, &issuer, None, &[]))
            .expect("decode a");
        let b = decode_invite(&encode_test_invite_with_fronts(
            &d,
            &issuer,
            None,
            &[Front {
                member_key: [11u8; 32],
                wireguard_public_key: [12u8; 32],
                mesh_port: 52201,
                endpoint: Some("198.51.100.9:51820".into()),
            }],
        ))
        .expect("decode b");
        assert_ne!(a.fronts, b.fronts, "the two invites differ only in fronts");
        assert_eq!(
            a.descriptor.genesis_namespace(),
            b.descriptor.genesis_namespace(),
            "fronts must not perturb the genesis fingerprint"
        );
        // Non-tautological both ways: the round-tripped fingerprint equals the
        // source descriptor's (fronts on the wire never fold into it), AND the
        // fingerprint IS sensitive to validator identity — proving it tracks the
        // consensus set, not the advisory reachability payload.
        assert_eq!(
            a.descriptor.genesis_namespace(),
            d.genesis_namespace(),
            "encoding/decoding fronts must not change the source fingerprint"
        );
        let mut with_extra_validator = front_descriptor(&issuer);
        with_extra_validator.validators.push(hex_bytes(
            ed25519::PrivateKey::from_seed(8).public_key().as_ref(),
        ));
        assert_ne!(
            d.genesis_namespace(),
            with_extra_validator.genesis_namespace(),
            "the fingerprint must change when the validator set changes"
        );
    }

    #[test]
    fn a_tampered_or_expired_or_stale_prefix_invite_is_refused() {
        let issuer = ed25519::PrivateKey::from_seed(7);
        let d = NetworkDescriptor {
            chain_id: "ducktape#a1b2c3d4".into(),
            validators: vec![hex_bytes(issuer.public_key().as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: crate::DEFAULT_BLOCK_TIME_MS,
            genesis: "ab".repeat(32),
            modules: Vec::new(),
        };
        // the token carries its OWN expiry now (no separate blob param).
        let token = mint_invite_token(&issuer, d.genesis_namespace().as_bytes(), 1_000);

        // expiry is enforced at decode, deterministically via the injected clock.
        let blob =
            encode_invite(&d, &token, &coordinated_test_wg(), &[], None, &issuer).expect("encode");
        assert!(decode_invite_at(&blob, 999).is_ok());
        let err = decode_invite_at(&blob, 1_000).expect_err("expired");
        assert!(err.contains("expired"), "{err}");

        // a flipped payload bit kills the envelope signature.
        use base64::Engine as _;
        let body = blob.strip_prefix(INVITE_PREFIX).unwrap();
        let mut bytes = INVITE_B64.decode(body).unwrap();
        bytes[2] ^= 0x01;
        let tampered = format!("{INVITE_PREFIX}{}", INVITE_B64.encode(&bytes));
        let err = decode_invite_at(&tampered, 0).expect_err("tampered");
        assert!(
            err.contains("signature") || err.contains("tampered"),
            "{err}"
        );

        // an envelope signed by someone other than the token's issuer is
        // refused at encode (and would fail decode's issuer verify anyway).
        let outsider = ed25519::PrivateKey::from_seed(8);
        assert!(encode_invite(&d, &token, &coordinated_test_wg(), &[], None, &outsider).is_err());

        // a non-invite paste fails loudly with re-mint guidance.
        let err = decode_invite_at("not-an-invite:AAAA", 0).expect_err("bad prefix");
        assert!(err.contains("fresh invite"), "{err}");
    }

    #[test]
    fn invite_echoes_the_coordination_mode() {
        // the mode byte rides the SIGNED envelope: a joiner learns off the
        // invite alone whether the coordinator expects a CoordCap.
        let issuer = ed25519::PrivateKey::from_seed(7);
        let base = NetworkDescriptor {
            chain_id: "ducktape#a1b2c3d4".into(),
            validators: vec![hex_bytes(issuer.public_key().as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: crate::DEFAULT_BLOCK_TIME_MS,
            genesis: "ab".repeat(32),
            modules: Vec::new(),
        };
        for (mode, expect) in [
            ("public", Coordination::Public),
            ("private", Coordination::Private),
        ] {
            let mut d = base.clone();
            d.coordination = Some(mode.to_string());
            let invite = decode_invite(&encode_test_invite(&d, &issuer, None)).expect("roundtrip");
            assert_eq!(
                invite.descriptor.coordination(),
                expect,
                "the {mode} mode byte roundtrips"
            );
            assert_eq!(invite.descriptor.coordination.as_deref(), Some(mode));
        }
        // an unset source resolves to Private, the safe default, and decodes
        // as the EXPLICIT "private" (semantically identical).
        let invite = decode_invite(&encode_test_invite(&base, &issuer, None)).expect("roundtrip");
        assert_eq!(invite.descriptor.coordination.as_deref(), Some("private"));
    }

    /// helper: a descriptor whose only validator is `issuer` (the minimal
    /// well-formed shape the invite tests need).
    fn front_test_descriptor(issuer: &ed25519::PrivateKey) -> NetworkDescriptor {
        NetworkDescriptor {
            chain_id: "ducktape#a1b2c3d4".into(),
            validators: vec![hex_bytes(issuer.public_key().as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: crate::DEFAULT_BLOCK_TIME_MS,
            genesis: "ab".repeat(32),
            modules: Vec::new(),
        }
    }

    #[test]
    fn invite_blob_roundtrips_member_fronts() {
        // (a) two fronts — one with a direct endpoint, one reachable only by
        // identity (endpoint None) — survive the signed envelope byte-for-byte.
        let issuer = ed25519::PrivateKey::from_seed(7);
        let d = front_test_descriptor(&issuer);
        let token = mint_invite_token(&issuer, d.genesis_namespace().as_bytes(), u64::MAX);
        let fronts = vec![
            Front {
                member_key: [11u8; 32],
                wireguard_public_key: [12u8; 32],
                mesh_port: 52210,
                endpoint: Some("198.51.100.9:51820".into()),
            },
            Front {
                member_key: [21u8; 32],
                wireguard_public_key: [22u8; 32],
                mesh_port: 52211,
                endpoint: None,
            },
        ];
        let blob = encode_invite(&d, &token, &coordinated_test_wg(), &fronts, None, &issuer)
            .expect("encode");
        let invite = decode_invite(&blob).expect("decode");
        assert_eq!(
            invite.fronts, fronts,
            "fronts roundtrip through the envelope"
        );
    }

    #[test]
    fn invite_expiry_adds_the_ttl_and_refuses_the_out_of_range() {
        assert_eq!(
            invite_expiry(1_000, DEFAULT_INVITE_TTL_DAYS).expect("default"),
            1_000 + 7 * 24 * 60 * 60
        );
        assert_eq!(invite_expiry(0, 365).expect("the ceiling"), 365 * 86_400);
        assert!(invite_expiry(1_000, 0).is_err(), "expired on paste");
        assert!(invite_expiry(1_000, 366).is_err(), "a key under the mat");
        assert!(invite_expiry(u64::MAX, 1).is_err(), "never wraps");
    }

    #[test]
    fn invite_token_file_roundtrips() {
        let dir = tmp("invitetoken");
        assert_eq!(load_invite_token(&dir).expect("absent is fine"), None);
        let issuer = ed25519::PrivateKey::from_seed(7);
        let token = mint_invite_token(&issuer, b"net#00000000@feedface", u64::MAX);
        save_invite_token(&dir, &token).expect("save");
        assert_eq!(load_invite_token(&dir).expect("load"), Some(token));
    }
}
