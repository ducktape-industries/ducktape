//! The async HTTP client side of the gateway protocol, shared by
//! `ducktape user cred inspect|seal` (the credential-provider verbs) and
//! `broker-host` (the Computation Provider's local api-snatch).
//! Topology-agnostic: a `Gateway` is either LOCAL (same-machine
//! loopback) or REMOTE (a duckdns handle routed by the local node's
//! browser-gateway, carried in `x-duck-authority`).
//!
//! Vendor-specific quote VERIFICATION stays in the binaries (it needs
//! `dcap-qvl` for TDX etc.); this module owns only the transport + the ECDH
//! handshake, which take an already-verified `seal_pk`.

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::handshake;
use crate::seal;
use crate::wire::{
    AttestationResponse, CredentialKind, CredentialPayload, CredentialUpload, SessionRequest,
    SessionResponse, WorkRef,
};

/// TCP+TLS connect deadline for a gateway client (#1668: `Gateway::local`/
/// `remote` built a bare `reqwest::Client::new()` with no timeout at all, so a
/// half-open path to the gateway hung forever). A live connect completes in
/// well under a second.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Total request deadline. Every call this client makes (attestation fetch,
/// the session handshake, a credential upload) is one small request/response —
/// none of them stream a long-lived body — so a single total timeout, not a
/// read/idle one, is the right shape here.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// What failed AFTER the gateway's response arrived.
///
/// A caller classifying a failed handshake reads the `reqwest::Error` out of the
/// error chain — but every step below happens once the response is in hand, so
/// there is either no transport error at all or one with no status. Inferring
/// "the token would not open" from that absence is wrong for a malformed body,
/// and `gateway_seal_pk_mismatch` is the single most expensive name to guess.
/// So the step tags itself, and the caller reads the tag instead of inferring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionResponseFault {
    /// the response body was not the session wire shape: not JSON, wrong shape,
    /// truncated, a non-base64 or non-utf8 token.
    Malformed,
    /// the body was well-formed and its sealed token would not open under the
    /// key we handshook against — the real seal_pk mismatch.
    TokenWouldNotOpen,
}

/// The gateway ANSWERED and refused, carrying its own stable reason token as the
/// body.
///
/// Tagged onto the error the same way [`SessionResponseFault`] is, and for the
/// same reason: `error_for_status` throws the body away, and a bare 403 cannot
/// tell "your grant is missing" from "you claimed an account the transport did
/// not vouch for" — two refusals whose operator actions have nothing in common.
/// The token is the gateway's own; this type only carries it across the
/// boundary and never invents one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRefusedBy {
    pub status: u16,
    /// the response body verbatim. A snake_case token from this crate's own
    /// gateway; free prose from anything else in the path (a node's proxy).
    pub reason: String,
}

impl std::fmt::Display for SessionRefusedBy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the gateway refused the session ({}): {}",
            self.status, self.reason
        )
    }
}

impl std::error::Error for SessionRefusedBy {}

impl std::fmt::Display for SessionResponseFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str("the gateway's session response was malformed"),
            Self::TokenWouldNotOpen => f.write_str(
                "the sealed session token would not open (the gateway's seal key is not the \
                 published one)",
            ),
        }
    }
}

/// Build the gateway HTTP client with the timeouts above. `reqwest::Client::new()`
/// panics internally on a build failure the same way; this keeps that behavior
/// while adding the timeouts it lacked.
fn gateway_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("build airlock gateway http client")
}

/// Topology-agnostic handle to a gateway.
pub struct Gateway {
    base: String,
    /// `Some(authority)` on the remote path — sent as `x-duck-authority` so the
    /// local node's browser-gateway routes the request onto the overlay.
    authority: Option<String>,
    http: reqwest::Client,
}

impl Gateway {
    /// Local: Credential Provider == Computation Provider (same-machine loopback).
    pub fn local(host: String) -> Self {
        Self {
            base: host,
            authority: None,
            http: gateway_http_client(),
        }
    }

    /// Remote: reach `handle` (a duckdns name) through `via` (the local node's
    /// browser-gateway base URL), which routes it onto the overlay.
    pub fn remote(handle: String, via: String) -> Self {
        Self {
            base: via,
            authority: Some(handle),
            http: gateway_http_client(),
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base.trim_end_matches('/'), path)
    }

    /// Add the overlay-routing header on the remote path; a no-op locally.
    pub fn route(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.authority {
            Some(a) => rb.header("x-duck-authority", a),
            None => rb,
        }
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Fetch the enclave quote and the advertised vendor. The caller verifies
    /// the quote per vendor before trusting anything read out of it.
    pub async fn fetch_quote(&self) -> Result<(Vec<u8>, String)> {
        let att: AttestationResponse = self
            .route(self.http.get(self.url("/attestation")))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let quote = BASE64.decode(&att.quote_b64).context("quote base64")?;
        Ok((quote, att.vendor))
    }

    /// Run the session-key handshake against an ALREADY-VERIFIED `seal_pk` and
    /// return the scoped session token. ECDHs the attested key, so the token can
    /// only be opened by this client — a relaying node cannot read it.
    ///
    /// There is no "as this account" variant, and there must not be one: on a
    /// lending gateway the grant subject is the account the node's proxy vouched
    /// for on the hop, and a caller that could name its own subject is the
    /// credential-theft defect.
    ///
    /// `work` names WHICH WORK the session draws for and is a pointer, not a
    /// claim — see [`WorkRef`].
    pub async fn open_session(
        &self,
        seal_pk: &[u8; 32],
        sub: &str,
        work: &WorkRef,
    ) -> Result<String> {
        let (token, _keys) = self.open_session_with(seal_pk, sub, work, false).await?;
        Ok(token)
    }

    /// Sealed-body session: bodies must be AEAD'd under the returned keys
    /// (`bodyseal`); the enclave refuses plaintext on this token. What the
    /// production broker opens on every self-host path.
    pub async fn open_session_sealed(
        &self,
        seal_pk: &[u8; 32],
        sub: &str,
        work: &WorkRef,
    ) -> Result<(String, handshake::SessionKeys)> {
        self.open_session_with(seal_pk, sub, work, true).await
    }

    async fn open_session_with(
        &self,
        seal_pk: &[u8; 32],
        sub: &str,
        work: &WorkRef,
        body_seal: bool,
    ) -> Result<(String, handshake::SessionKeys)> {
        let (client_eph_pk, keys) = handshake::client_handshake(seal_pk);
        // Everything from `.json()` down runs on an ARRIVED response, so each
        // step tags itself (see [`SessionResponseFault`]) — the caller must not
        // have to guess which one failed from an absent transport error.
        let response = self
            .route(self.http.post(self.url("/session")))
            .json(&SessionRequest {
                sub: sub.to_string(),
                client_eph_pk_b64: BASE64.encode(client_eph_pk),
                body_seal,
                work: work.clone(),
            })
            .send()
            .await?;
        // Not `error_for_status`: it discards the body, which is where the
        // gateway's own refusal token lives. See [`SessionRefusedBy`].
        let status = response.status();
        if !status.is_success() {
            let reason = response.text().await.unwrap_or_default();
            return Err(SessionRefusedBy {
                status: status.as_u16(),
                reason,
            }
            .into());
        }
        let resp: SessionResponse = response
            .json()
            .await
            .context(SessionResponseFault::Malformed)?;
        let sealed = BASE64
            .decode(&resp.sealed_token_b64)
            .context(SessionResponseFault::Malformed)?;
        let token = handshake::open_token(&keys.session, &sealed)
            .context(SessionResponseFault::TokenWouldNotOpen)?;
        let token = String::from_utf8(token).context(SessionResponseFault::Malformed)?;
        Ok((token, keys))
    }

    /// Seal `payload` (a refresh token or a static bearer) to the ALREADY-VERIFIED
    /// `seal_pk` and upload it under the credential `name`/`kind`. The gateway
    /// never sees the secret in the clear; the name/kind are cleartext routing.
    pub async fn upload_sealed_credential(
        &self,
        seal_pk: &[u8; 32],
        name: &str,
        kind: CredentialKind,
        payload: &CredentialPayload,
    ) -> Result<()> {
        let payload = serde_json::to_vec(payload)?;
        let sealed = seal::seal(seal_pk, &payload);
        let status = self
            .route(self.http.post(self.url("/credential")))
            .json(&CredentialUpload {
                name: name.to_string(),
                kind,
                sealed_b64: BASE64.encode(sealed),
            })
            .send()
            .await?
            .status();
        if !status.is_success() {
            anyhow::bail!("credential upload failed: {status}");
        }
        Ok(())
    }
}
