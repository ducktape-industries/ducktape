//! HTTP wire types between host and client. Plain HTTP for the PoC; the
//! transport drops onto duckdns + a gateway RouteRecord in the integration
//! spec.
//!
//! Every type here is STRICT: `deny_unknown_fields`, and no `serde(default)`
//! anywhere. The protocol decodes each frame at its boundary, so a producer
//! that is out of step gets a named decode error instead of a silently
//! defaulted field.
//!
//! Nothing here carries an ACCOUNT. A session request names the credential and
//! the session's ephemeral key, and says nothing about who is acting: identity
//! enters the flow at exactly one place, the gateway hop, where the node stamps
//! a mesh-verified caller the request cannot mint. A field for the caller to
//! declare its own account was the whole of the credential-theft defect.
//!
//! [`WorkRef`] is not a reopening of that. It is a POINTER — an id the lender
//! resolves in its OWN committed state — and every fact it yields is read from
//! consensus, never from the request. See its own docs.

use serde::{Deserialize, Serialize};

/// `GET /attestation` — the enclave quote. The seal key used downstream is read
/// out of the quote's verified REPORTDATA, never trusted from a JSON field.
/// `vendor` ("tdx" | "snp") only selects which verifier the client
/// runs; it is not itself trusted — the quote's own format authenticates it.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttestationResponse {
    pub quote_b64: String,
    pub vendor: String,
}

/// Which vendor a credential belongs to. For a model credential this selects
/// the upstream base + auth shape the gateway proxies `/v1/*` to (`Claude` →
/// Anthropic, `Codex` → OpenAI) unless the operator redirected one with
/// `DUCKTAPE_AIRLOCK_{ANTHROPIC,OPENAI}_BASE`. `AppleCodesign` is not a model
/// credential: it is the Developer ID signing identity + App Store Connect key
/// a release is signed and notarized with, held by the gateway and never
/// served on `/v1/*` (a session on it that hits the proxy is refused
/// `credential_kind_mismatch`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    Claude,
    Codex,
    AppleCodesign,
}

/// `POST /credential` — the sealed credential, encrypted to the enclave's seal
/// key. Only the enclave (inside the TD) can open it. `name`/`kind` are cleartext
/// routing metadata (never secret): the gateway keys a NAMED store by `name` and
/// picks the upstream by `kind`.
///
/// Served ONLY by the attested build — see [`crate::server::CredentialUploads`].
/// Sealing is not authentication: the seal public key is published on chain and
/// served at `/attestation`, so anyone can produce a well-formed upload.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialUpload {
    pub name: String,
    pub kind: CredentialKind,
    pub sealed_b64: String,
}

/// Plaintext inside the sealed blob: the upstream credential the enclave holds.
/// `Refresh` carries the subscription's CURRENT access token alongside the
/// rotating refresh token: the enclave serves the access token as-is until
/// `expires_at`, then exchanges the refresh token for a new one. Seeding the
/// live access token means NO refresh fires while it is still valid — so the
/// owner's own local login (which shares the refresh-token chain) is not
/// rotation-invalidated during that window. An empty `access_token` with
/// `expires_at` 0 is the lazy form (refresh on first use), and every producer
/// writes both fields EXPLICITLY — an omitted one is a producer out of step, not
/// a lazy credential. `Bearer` is a STATIC access token used as-is — no refresh,
/// no rotation.
///
/// `AppleCodesign` is the whole signing identity in one blob: the Developer ID
/// Application certificate + private key as a PKCS#12 (`p12_b64`, base64 of
/// the `.p12` bytes) with its password, the App Store Connect API key as the
/// one-file JSON `rcodesign encode-app-store-connect-api-key` writes
/// (`{key_id, issuer_id, private_key}`), and the fixed `team_id` the
/// certificate's subject OU must name. The gateway validates every field
/// before it stores the entry (see `codesign::admit`).
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialPayload {
    Refresh {
        refresh_token: String,
        access_token: String,
        expires_at: u64,
    },
    Bearer {
        access_token: String,
    },
    AppleCodesign {
        p12_b64: String,
        p12_password: String,
        api_key_json: String,
        team_id: String,
    },
}

/// `POST /session` — the Computation Provider asks for a scoped session token.
/// `sub` names the CREDENTIAL the session draws on (the on-chain credential
/// name); the gateway keys budgets, nonces, and upstream selection by it and
/// refuses an unknown name. `client_eph_pk_b64`
/// is this session's ephemeral X25519 public key: the enclave ECDHs it against
/// its static seal key to derive the shared session key (see `handshake`).
///
/// There is deliberately NO account field. The only identity on the hop is the
/// NODE the node's gateway proxy vouched for
/// ([`crate::server::CALLER_NODE_HEADER`]), which is minted from the
/// mesh-verified peer identity and refused if a caller supplies one; the grant
/// subject is reached from `work`, never from the request. A request cannot
/// name a subject: the computation layer does not get to say who it acts for,
/// and the token it ends up holding carries no identity either — only `sub`,
/// which credential.
///
/// `work` is a POINTER, and the distinction from an account field is the whole
/// design — see [`WorkRef`].
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRequest {
    pub sub: String,
    pub client_eph_pk_b64: String,
    /// Sealed-body session: bodies are AEAD'd broker<->enclave (`bodyseal`)
    /// and the enclave refuses plaintext. Echoed into the token claims.
    pub body_seal: bool,
    /// WHICH WORK this session draws for. Required and tagged, never an
    /// `Option`: this wire has no `serde(default)` anywhere by design, so every
    /// producer states which arm it means and a producer out of step gets a
    /// named decode error.
    pub work: WorkRef,
}

/// The work a session draws for — a POINTER the lender resolves, never a claim
/// the caller makes.
///
/// ## why this is not the account field that was deleted
///
/// `POST /v1/submit` on a real node DISCARDS the caller's claimed submitter id
/// and re-signs the op with the node's own signer
/// (`bin/node/src/validator/run/ingress.rs`). So a saga's committed
/// `SagaOrigin::External(bytes)` is **the submitting node's key, proven by a
/// verified signature** — derived, never asserted. That single fact is what
/// makes delegation safe: the executor sends an id and nothing else, and the
/// lender reads WHO submitted, WHO holds the lease, and WHETHER that submitter
/// is granted out of its own committed state. Nothing on this request is
/// trusted beyond "which record to look up".
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkRef {
    /// No delegation: authorize the vouched-for caller's own grant, exactly as
    /// an ungated gateway always has. What an interactive session presents —
    /// the terminal plane has no committed record of who asked, so it has no
    /// pointer to offer and no delegation to claim.
    Direct,
    /// A committed saga on the lender's own chain. The lender resolves it from
    /// its own state; see the type docs.
    Saga { saga_id: String },
}

/// The token, AEAD-sealed under the handshake session key. Only the client that
/// derived the same key (from the attested `seal_pk`) can open it.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionResponse {
    pub sealed_token_b64: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_codesign_payload_round_trips_under_its_tag() {
        let payload = CredentialPayload::AppleCodesign {
            p12_b64: "cDEy".into(),
            p12_password: "pw".into(),
            api_key_json: r#"{"key_id":"k"}"#.into(),
            team_id: "ABCDE12345".into(),
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["kind"], "apple_codesign");
        let back: CredentialPayload = serde_json::from_value(json).unwrap();
        let CredentialPayload::AppleCodesign {
            p12_b64,
            p12_password,
            api_key_json,
            team_id,
        } = back
        else {
            panic!("decoded as another arm");
        };
        assert_eq!((p12_b64.as_str(), p12_password.as_str()), ("cDEy", "pw"));
        assert_eq!(api_key_json, r#"{"key_id":"k"}"#);
        assert_eq!(team_id, "ABCDE12345");
    }

    #[test]
    fn an_apple_codesign_payload_missing_a_field_is_a_decode_error() {
        let missing_team =
            r#"{"kind":"apple_codesign","p12_b64":"","p12_password":"","api_key_json":""}"#;
        assert!(serde_json::from_str::<CredentialPayload>(missing_team).is_err());
        assert_eq!(
            serde_json::to_string(&CredentialKind::AppleCodesign).unwrap(),
            r#""apple_codesign""#
        );
    }
}
