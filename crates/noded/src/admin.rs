//! the owner-gated control namespace — the node's PRIVATE RPC surface.
//! everything under `/v1/admin/*` is CONTROL, not data: lifecycle,
//! code/upgrade staging, diagnostics. it is a NAMESPACE on the SAME listener
//! (geth `admin_` spirit), never a second port, gated as a unit by one
//! middleware ([`admin_guard`]) layered onto the admin sub-router alone.
//!
//! ## the exposure ladder ([`AdminExposure`], flag `DUCKTAPE_ADMIN`)
//!
//! - `Disabled` unregisters the routes entirely — the surface is simply ABSENT
//!   (`router` never merges them), a 404, not a gated-but-present 403.
//! - `Loopback` (the default): reachable only from loopback peers, AND the peer
//!   must present the operator credential ([`ADMIN_TOKEN_HEADER`]).
//! - `Public`: reachable off-box, so the OWNER gate is the ONLY thing
//!   standing between a remote caller and node control — enforced for every
//!   peer. this is the new capability W2 adds, and the case PoP exists for.
//!
//! the EXPOSURE picks the arm, and nothing else does — ownership is not even
//! looked up under `Loopback`. so a node with a committed owner still takes the
//! operator credential at the default exposure; the owner PoP becomes the
//! credential when, and only when, the operator sets `DUCKTAPE_ADMIN=public`.
//! the two are a LADDER, never an OR: under `Public` with an owner committed,
//! the operator token is not a fallback and not accepted (a token-only request
//! is `owner_signature_invalid`, which is how an operator tells "wrong
//! credential type" from `operator_token_mismatch`'s "wrong secret").
//!
//! ## the operator gate — why loopback presence is NOT authority
//!
//! loopback used to BE the gate here, on the reasoning `origin_guard` states for
//! itself: a local process can read `user.key` off disk anyway. that reasoning
//! broke when compute, agent and airlock became separate local daemon
//! processes — three long-lived programs now sit exactly where "any loopback
//! peer" points, and any of them could `POST /v1/admin/shutdown` or stage module
//! wasm without ever asking for a grant.
//!
//! so admin requires what the node's OWN workspace requires: a secret minted
//! fresh each boot and written 0600 beside `node.toml` ([`mint_operator_token`]).
//! that is the SAME bar, and the same mechanism, `service-link.token` already
//! sets for the interactive plane (`crate::services::mint_link_token`) — the
//! compare is literally `crate::services::token_matches`. it raises admin from
//! "can dial loopback" to "can read the node's own workspace".
//!
//! CEILING, stated rather than assumed — and it is NOT that the service daemons
//! are now excluded. `ducktape service run <kind>` resolves a workspace and
//! reads `service-link.token` out of it (`bin/node/src/agent/link.rs`), so a
//! daemon launched with the workspace path sits in the same directory, at the
//! same uid, behind the same 0600 mode as `admin.token`: it still clears this
//! bar, because nothing in-process can gate a `read(2)` by the same uid. what
//! this gate actually excludes is EVERY OTHER local process on the box — which
//! is the "can dial loopback" half, and was the whole of the gate before. the
//! residual is the uid/workspace boundary, and bounding it is the launch
//! contract's job (start a daemon with a base URL, never with `--config`).
//!
//! ## the owner gate (A5, only under `Public`)
//!
//! a per-request proof-of-possession (PoP) by the owner account's key, verified
//! statelessly against a PUBLIC pin — the coordinator auth pattern (#197). the
//! owner is the OPERATOR's statement plus a chain fact: identity binds no node
//! to an account, so the node boots with the operator's own wallet key
//! ([`AdminConfig::owner_key`]) and the account THAT key is on (identity
//! `OfKey`) owns the control plane; the request's key must be one of that
//! account's member keys.
//!
//! ## the bootstrap window
//!
//! a `Public` node whose operator key is on NO account yet (fresh network,
//! before `ducktape account create`) has nobody to authenticate against, so
//! admin falls back to the operator gate until the account exists — never
//! drivable off-box with no check at all, collapsing to the full owner gate
//! the instant the account does. the embedded single-writer daemon
//! (`node_key = None`, no consensus) and a host with no wallet
//! (`owner_key = None`) can only ever be operator-gated.
//!
//! ## the PoP wire (mirrors `nat-traversal::auth`)
//!
//! sign [`ADMIN_REQ_NS`] over
//! `method ‖ 0x1f ‖ path_and_query ‖ 0x1f ‖ node_key ‖ 0x1f ‖ ts_be` — the
//! TARGET NODE's consensus key is folded in, so a signature minted for node X
//! can never be replayed against another node the same owner controls.
//! headers carry the account key, the timestamp and the signature. the BODY is
//! deliberately NOT signed: `module-code/stage` streams a large artifact the
//! store never parks in memory, and buffering it in middleware to hash it would
//! regress that. ceiling: on a non-TLS `Public` exposure a network attacker can
//! tamper an owner-issued request's body within the freshness window — TLS
//! termination is the operator's job for hostile-network exposure.
//! ponytail: method+path+node+ts PoP, not body; 30s replay window; TLS is the op's job.
//!
//! NEVER front a `Loopback`-exposure node with a reverse proxy: the proxy's
//! loopback dial would launder every remote caller into a trusted local peer.
//! Off-box access is `Public` + owner PoP (+ operator TLS), nothing else.

use std::net::SocketAddr;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use commonware_cryptography::{Signer as _, ed25519};
use serde::Deserialize;

use crate::NodeHandle;
use crate::handle::NodeCommand;
use crate::signed_req::{self, PopError, PopHeaders, header_str, now_secs};

/// PoP signing namespace: `sign(ADMIN_REQ_NS, method ‖ 0x1f ‖ path ‖ 0x1f ‖ ts)`.
pub const ADMIN_REQ_NS: &[u8] = b"ducktape-admin-req-v1";
/// max clock skew (seconds) between a request timestamp and this node. ONE
/// window for both PoP namespaces (see [`crate::signed_req::FRESHNESS_SECS`]).
pub const ADMIN_FRESHNESS_SECS: u64 = crate::signed_req::FRESHNESS_SECS;

/// the control plane's header trio. distinct names from the data plane's so a
/// control credential can never be replayed onto a data route.
const ADMIN_HEADERS: PopHeaders = PopHeaders {
    key: ADMIN_KEY_HEADER,
    ts: ADMIN_TS_HEADER,
    sig: ADMIN_SIG_HEADER,
};

/// hex ed25519 owner-account public key (32 bytes).
pub const ADMIN_KEY_HEADER: &str = "x-ducktape-admin-key";
/// decimal unix seconds the request was signed at.
pub const ADMIN_TS_HEADER: &str = "x-ducktape-admin-ts";
/// hex ed25519 signature (64 bytes) over the canonical request bytes.
pub const ADMIN_SIG_HEADER: &str = "x-ducktape-admin-sig";

/// the operator credential — the secret [`mint_operator_token`] wrote into the
/// node's workspace. a HEADER, never a path or query parameter: the logging
/// doctrine forbids logging URIs precisely because a capability in a path is
/// unredactable.
pub const ADMIN_TOKEN_HEADER: &str = "x-ducktape-admin-token";

/// the file a node writes its operator credential into, beside `node.toml`.
pub const ADMIN_TOKEN_FILE: &str = "admin.token";

/// the default identity module id ownership resolves against.
pub const DEFAULT_IDENTITY_MODULE: &str = "identity";

/// Mint this node's operator credential and write it 0600 into `dir` (the
/// workspace beside `node.toml`, or the storage root on the embedded daemon).
///
/// A thin caller of [`crate::services::mint_secret_file`] — ONE writer for the
/// service link and this, because two byte-identical copies is exactly the
/// shape where an fsync lands in one and silently not the other.
pub fn mint_operator_token(dir: &std::path::Path) -> Result<String, String> {
    crate::services::mint_secret_file(dir, ADMIN_TOKEN_FILE)
}

/// Read a node's operator credential — what an operator client presents in
/// [`ADMIN_TOKEN_HEADER`].
pub fn read_operator_token(dir: &std::path::Path) -> Result<String, String> {
    crate::services::read_secret_file(dir, ADMIN_TOKEN_FILE)
}

/// how the node exposes its admin (control) namespace.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AdminExposure {
    /// no admin namespace at all — the routes are never registered.
    Disabled,
    /// present, but reachable only from loopback peers (the default).
    #[default]
    Loopback,
    /// present and reachable from any peer the owner PoP gate admits.
    Public,
}

impl AdminExposure {
    /// parse the `DUCKTAPE_ADMIN` flag: `off | loopback | public`. anything
    /// unrecognized (including empty) falls to the safe default, `Loopback`.
    pub fn from_flag(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "disabled" | "none" => Self::Disabled,
            "public" | "expose" | "remote" => Self::Public,
            _ => Self::Loopback,
        }
    }

    /// read the exposure from the `DUCKTAPE_ADMIN` env flag.
    pub fn from_env() -> Self {
        std::env::var("DUCKTAPE_ADMIN")
            .map(|raw| Self::from_flag(&raw))
            .unwrap_or_default()
    }

    /// are the admin routes registered at all?
    pub fn enabled(self) -> bool {
        self != Self::Disabled
    }
}

/// the admin namespace's config, carried on [`NodeHandle`].
#[derive(Clone, Debug)]
pub struct AdminConfig {
    pub exposure: AdminExposure,
    /// this node's own consensus key — the salt every owner PoP is bound to,
    /// so a signature for one node never replays against another. `None` on
    /// the embedded daemon (no consensus): admin stays operator-gated there.
    pub node_key: Option<Vec<u8>>,
    /// the LOCAL user key whose account owns this node's control plane — the
    /// operator's active wallet key, read from the keystore at boot. Identity
    /// binds no node to an account, so ownership is the operator's own
    /// statement: "the account my key is on drives this node". `None` = no
    /// wallet on this host, admin stays operator-gated.
    pub owner_key: Option<Vec<u8>>,
    /// identity module id ownership resolves against.
    pub identity_module: String,
    /// this boot's operator credential ([`mint_operator_token`]). `None` FAILS
    /// CLOSED — a node that could not mint one refuses every admin request
    /// rather than falling back to the loopback trust this replaced.
    pub operator_token: Option<String>,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            exposure: AdminExposure::default(),
            node_key: None,
            owner_key: None,
            identity_module: DEFAULT_IDENTITY_MODULE.to_string(),
            operator_token: None,
        }
    }
}

impl AdminConfig {
    /// what every real serve path builds: mint this boot's operator credential
    /// into `dir` and carry it. The full node adds its own `node_key` on top;
    /// the embedded daemon and the sim have none (no consensus, no on-chain
    /// owner), so the credential is their whole gate.
    ///
    /// Minted UNCONDITIONALLY, `DUCKTAPE_ADMIN=off` included. The credential is
    /// no longer the admin namespace's alone: it is the node's local-daemon
    /// credential, and the data plane's write gate
    /// ([`crate::signed_req::signed_write_guard`]) accepts it as the second way
    /// past — so an operator who turns the CONTROL surface off must not thereby
    /// leave this node's own announce, lease and provisioner writes with no
    /// credential to present.
    ///
    /// A mint failure leaves `operator_token: None`, which REFUSES every admin
    /// request rather than falling back to the loopback trust this replaced.
    /// That fallback would be the whole bug.
    pub fn minted(exposure: AdminExposure, dir: &std::path::Path) -> Self {
        let operator_token = mint_operator_token(dir)
            .inspect_err(|error| {
                tracing::error!(
                    target: "ducktape::admin",
                    reason = "operator_token_unwritable",
                    "admin and every mutating /v1 route will refuse this node's own daemons: {error}"
                );
            })
            .ok();
        Self {
            exposure,
            operator_token,
            ..Self::default()
        }
    }
}

/// the canonical bytes an admin request's PoP signs / verifies — method, the
/// path+query, the TARGET NODE's consensus key, and the timestamp. the node
/// key pins the signature to one node (no cross-node replay by a multi-node
/// owner); the fixed-width tail (32-byte key, 8-byte ts) keeps the layout
/// unambiguous even though the key is raw bytes.
fn pop_message(method: &str, path_and_query: &str, node_key: &[u8], ts: u64) -> Vec<u8> {
    let mut m = Vec::with_capacity(method.len() + path_and_query.len() + node_key.len() + 11);
    m.extend_from_slice(method.as_bytes());
    m.push(0x1f);
    m.extend_from_slice(path_and_query.as_bytes());
    m.push(0x1f);
    m.extend_from_slice(node_key);
    m.push(0x1f);
    m.extend_from_slice(&ts.to_be_bytes());
    m
}

/// sign one admin request with the owner account's key, bound to the TARGET
/// node's consensus key. exposed so the app's signing verb (and tests) produce
/// the exact bytes [`verify_pop`] checks — one source of truth.
pub fn sign_admin(
    signer: &ed25519::PrivateKey,
    method: &str,
    path_and_query: &str,
    node_key: &[u8],
    ts: u64,
) -> ed25519::Signature {
    signer.sign(
        ADMIN_REQ_NS,
        &pop_message(method, path_and_query, node_key, ts),
    )
}

/// the account key that a well-formed, fresh, correctly-signed CONTROL request
/// proves possession of — or why it failed. does NOT decide ownership; that is
/// a separate chain read in [`resolve_owner`].
///
/// the verifier itself is [`crate::signed_req::verify_pop`], shared with the
/// data plane: two copies of a signature check is exactly the shape where one
/// gets a fix and the other does not. only the MESSAGE differs, and it differs
/// on purpose — see [`pop_message`].
fn verify_pop(
    headers: &HeaderMap,
    method: &str,
    path_and_query: &str,
    node_key: &[u8],
    now: u64,
) -> Result<Vec<u8>, PopError> {
    signed_req::verify_pop(headers, ADMIN_HEADERS, ADMIN_REQ_NS, now, |ts| {
        pop_message(method, path_and_query, node_key, ts)
    })
}

/// the outcome of resolving this node's owning account.
enum OwnerResolve {
    /// the operator's key is on an account; these are its member keys.
    Owned(Vec<Vec<u8>>),
    /// the operator's key is on no account yet — the bootstrap window.
    NoOwner,
    /// the identity module could not be reached (actor gone / not registered).
    Unavailable,
}

/// resolve the account the operator's `owner_key` belongs to from committed
/// state, over the same actor query lane every read uses. NEVER trusts the
/// connection — membership is read from `identity` `OfKey`.
async fn resolve_owner(
    handle: &NodeHandle,
    identity_module: &str,
    owner_key: &[u8],
) -> OwnerResolve {
    let req = identity::encode_query(&identity::IdentityQuery::OfKey {
        key: owner_key.to_vec(),
    });
    let (reply, rx) = futures::channel::oneshot::channel();
    if handle
        .send(NodeCommand::Query {
            target: identity_module.to_string(),
            req,
            reply,
        })
        .await
        .is_err()
    {
        return OwnerResolve::Unavailable;
    }
    let bytes = match rx.await {
        Ok(Ok(bytes)) => bytes,
        // a module error (e.g. no identity module registered) means we cannot
        // prove ownership; fail closed to Unavailable, not open.
        _ => return OwnerResolve::Unavailable,
    };
    match identity::decode_reply(&bytes) {
        Ok(identity::IdentityReply::Account(Some(view))) => {
            OwnerResolve::Owned(view.keys.into_iter().map(|key| key.pubkey).collect())
        }
        Ok(identity::IdentityReply::Account(None)) => OwnerResolve::NoOwner,
        Ok(
            identity::IdentityReply::Accounts(_)
            | identity::IdentityReply::Resolved(_)
            | identity::IdentityReply::Gen(_),
        ) => OwnerResolve::Unavailable,
        Err(_) => OwnerResolve::Unavailable,
    }
}

/// is the request's peer a loopback address? FAIL-CLOSED: a request with no
/// `ConnectInfo` (an embedder that forgot `into_make_service_with_connect_info`)
/// is NOT treated as loopback — an unknown peer must never inherit local trust.
/// every real serve path (noded, bin/node, simnode) threads connect-info.
///
/// Takes the request's EXTENSIONS rather than the request, so a handler that
/// extracted its parts (the `/v1/ws` upgrade) asks the same question the same
/// way.
pub(crate) fn peer_is_loopback(extensions: &axum::http::Extensions) -> bool {
    extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip().is_loopback())
        .unwrap_or(false)
}

/// Why an admin request was turned away. Typed exactly like
/// [`crate::services::HelloRefusal`]: the status and the stable snake_case
/// `reason` are DERIVED from the variant, so they cannot drift apart and a typo
/// cannot silently downgrade a refusal.
///
/// No variant's message names the expected credential, any part of it, or the
/// request URI — the caller learns only which check it failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminRefusal {
    /// the namespace is not registered on this node at all.
    NamespaceAbsent,
    /// a non-loopback peer, on a node whose admin is not publicly exposed.
    OffBox,
    /// this node minted no operator credential, so it can verify nothing.
    OperatorTokenUnavailable,
    /// the request carried no operator credential.
    OperatorTokenMissing,
    /// the operator credential presented is not this node's — a guess, OR a
    /// credential cached from before this node restarted.
    ///
    /// Those two CANNOT be told apart, and the reason token does not pretend
    /// to: the node holds only the token it minted this boot, so a stale one and
    /// a guessed one are byte-identical to it. Remembering previous boots' tokens
    /// to say "stale" is the same thing as keeping them valid, which is exactly
    /// what fresh-each-boot exists to prevent. The MESSAGE names both
    /// possibilities instead of guessing between them.
    OperatorTokenMismatch,
    /// the committed owner could not be read (identity module unreachable).
    OwnerUnresolved,
    /// the owner PoP is absent or does not verify.
    OwnerSignatureInvalid,
    /// the owner PoP is outside the freshness window.
    OwnerSignatureStale,
    /// a valid PoP by a key that does not own this node.
    NotTheOwner,
}

impl AdminRefusal {
    /// the stable snake_case token — greppable, countable, never prose.
    pub fn reason(self) -> &'static str {
        match self {
            AdminRefusal::NamespaceAbsent => "admin_namespace_absent",
            AdminRefusal::OffBox => "admin_off_box",
            AdminRefusal::OperatorTokenUnavailable => "operator_token_unavailable",
            AdminRefusal::OperatorTokenMissing => "operator_token_missing",
            AdminRefusal::OperatorTokenMismatch => "operator_token_mismatch",
            AdminRefusal::OwnerUnresolved => "owner_unresolved",
            AdminRefusal::OwnerSignatureInvalid => "owner_signature_invalid",
            AdminRefusal::OwnerSignatureStale => "owner_signature_stale",
            AdminRefusal::NotTheOwner => "not_the_owner",
        }
    }

    /// 404 for "there is nothing here"; 401 for "you presented nothing usable";
    /// 403 for "you presented something, and it is not enough"; 503 for "this
    /// node cannot serve the check at all".
    pub fn status(self) -> StatusCode {
        match self {
            AdminRefusal::NamespaceAbsent => StatusCode::NOT_FOUND,
            AdminRefusal::OffBox => StatusCode::FORBIDDEN,
            AdminRefusal::OperatorTokenUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            AdminRefusal::OperatorTokenMissing => StatusCode::UNAUTHORIZED,
            AdminRefusal::OperatorTokenMismatch => StatusCode::FORBIDDEN,
            AdminRefusal::OwnerUnresolved => StatusCode::SERVICE_UNAVAILABLE,
            AdminRefusal::OwnerSignatureInvalid => StatusCode::UNAUTHORIZED,
            AdminRefusal::OwnerSignatureStale => StatusCode::UNAUTHORIZED,
            AdminRefusal::NotTheOwner => StatusCode::FORBIDDEN,
        }
    }

    /// the operator-facing sentence. names the FILE to read, never its contents.
    pub fn message(self) -> &'static str {
        match self {
            AdminRefusal::NamespaceAbsent => "not found",
            AdminRefusal::OffBox => "admin namespace is not reachable off-box on this node",
            AdminRefusal::OperatorTokenUnavailable => {
                "this node minted no operator credential, so it refuses every admin request"
            }
            AdminRefusal::OperatorTokenMissing => {
                "admin requires the operator credential from admin.token in the node's workspace"
            }
            AdminRefusal::OperatorTokenMismatch => {
                "that operator credential is not this node's — re-read admin.token from the \
                 node's workspace; a restart mints a new one"
            }
            AdminRefusal::OwnerUnresolved => "cannot resolve node owner",
            AdminRefusal::OwnerSignatureInvalid => "admin request needs a valid owner signature",
            AdminRefusal::OwnerSignatureStale => "admin request timestamp is stale",
            AdminRefusal::NotTheOwner => "signer is not the node owner",
        }
    }
}

/// everything the gate decides on, captured from the request BEFORE any await:
/// an axum `Request` is not `Sync`, so a reference to one cannot be held across
/// the owner lookup. Nothing here is the BODY — the gate must never buffer one
/// (`module-code/stage` streams a large artifact).
struct Presented {
    peer_is_loopback: bool,
    method: String,
    path_and_query: String,
    headers: HeaderMap,
}

impl Presented {
    fn of(req: &axum::extract::Request) -> Self {
        Self {
            peer_is_loopback: peer_is_loopback(req.extensions()),
            method: req.method().as_str().to_string(),
            path_and_query: req
                .uri()
                .path_and_query()
                .map(|pq| pq.as_str().to_string())
                .unwrap_or_else(|| req.uri().path().to_string()),
            headers: req.headers().clone(),
        }
    }
}

/// the ONE gate over `/v1/admin/*`: decide, then run or refuse. runs BEFORE the
/// body is read, so a large staged artifact never buffers here.
pub async fn admin_guard(
    State(handle): State<NodeHandle>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    let presented = Presented::of(&req);
    match admit(&handle, &presented).await {
        Ok(()) => next.run(req).await,
        Err(refusal) => refuse(refusal),
    }
}

/// the decide half: one `match` on the exposure, each arm one delegation. reads
/// the request, writes nothing.
async fn admit(handle: &NodeHandle, presented: &Presented) -> Result<(), AdminRefusal> {
    let cfg = &handle.admin;
    match cfg.exposure {
        // `router` never mounts these when disabled; this arm is belt-and-braces.
        AdminExposure::Disabled => Err(AdminRefusal::NamespaceAbsent),
        // LOOPBACK exposure: on-box AND holding the operator credential.
        AdminExposure::Loopback => admit_operator(cfg, presented),
        // PUBLIC exposure: the surface is reachable off-box, so the OWNER PoP is
        // the gate that matters — enforced for every peer, loopback or
        // not. a node with no owner to authenticate against (no consensus, or
        // the pre-bind bootstrap window) falls back to the operator credential:
        // it must not be drivable with no check at all.
        AdminExposure::Public => admit_public(handle, cfg, presented).await,
    }
}

/// PUBLIC exposure: the owner PoP where an owner exists, the operator
/// credential where one does not yet.
async fn admit_public(
    handle: &NodeHandle,
    cfg: &AdminConfig,
    presented: &Presented,
) -> Result<(), AdminRefusal> {
    // both halves or neither: the node key salts the PoP, the owner key names
    // the account that may present one.
    let (Some(node_key), Some(owner_key)) = (cfg.node_key.as_deref(), cfg.owner_key.as_deref())
    else {
        return admit_operator(cfg, presented);
    };
    match resolve_owner(handle, &cfg.identity_module, owner_key).await {
        OwnerResolve::Unavailable => Err(AdminRefusal::OwnerUnresolved),
        OwnerResolve::NoOwner => admit_operator(cfg, presented),
        OwnerResolve::Owned(members) => admit_owner(&members, node_key, presented),
    }
}

/// the operator gate: a loopback peer holding this boot's workspace credential.
/// BOTH, conjunctively — loopback alone is what this file stopped trusting, and
/// the credential alone must not re-open the off-box reach `Loopback` denies.
fn admit_operator(cfg: &AdminConfig, presented: &Presented) -> Result<(), AdminRefusal> {
    if !presented.peer_is_loopback {
        return Err(AdminRefusal::OffBox);
    }
    let Some(expected) = cfg.operator_token.as_deref() else {
        return Err(AdminRefusal::OperatorTokenUnavailable);
    };
    let Some(offered) = header_str(&presented.headers, ADMIN_TOKEN_HEADER) else {
        return Err(AdminRefusal::OperatorTokenMissing);
    };
    // the SAME constant-time compare the service link uses — one implementation.
    let credential_matches = crate::services::token_matches(offered, expected);
    if !credential_matches {
        return Err(AdminRefusal::OperatorTokenMismatch);
    }
    Ok(())
}

/// the owner PoP gate: the request must carry a fresh signature by a key that
/// is a member of the operator's account, bound to THIS node's key.
fn admit_owner(
    members: &[Vec<u8>],
    node_key: &[u8],
    presented: &Presented,
) -> Result<(), AdminRefusal> {
    let account_key = match verify_pop(
        &presented.headers,
        &presented.method,
        &presented.path_and_query,
        node_key,
        now_secs(),
    ) {
        Ok(key) => key,
        Err(PopError::Stale) => return Err(AdminRefusal::OwnerSignatureStale),
        Err(PopError::MissingAuth | PopError::BadKey | PopError::BadSig) => {
            return Err(AdminRefusal::OwnerSignatureInvalid);
        }
    };
    let signer_owns_this_node = members.contains(&account_key);
    if !signer_owns_this_node {
        return Err(AdminRefusal::NotTheOwner);
    }
    Ok(())
}

/// the write half: one refusal body, mirroring `services::hello`'s shape. The
/// `reason` token is the greppable half; the URI never appears.
///
/// The level splits exactly the way `error_response` splits it, for the same
/// reason. 4xx is `debug`: it is per-request and any local process can drive one
/// in a loop, so an unconditional `warn!` would evict the 4096-line ring. 5xx is
/// `warn` but LATCHED — a 503 says this node cannot serve the check AT ALL
/// (nothing minted a credential, the identity module is unreachable), which is a
/// standing fault an operator's retry loop mints one line per request for. First
/// occurrence, then every 50th, carrying `occurrences`: the outage is visible on
/// line one, and the counter is what says "still broken" rather than "flapped".
fn refuse(refusal: AdminRefusal) -> Response {
    let status = refusal.status();
    // keyed by the reason token, which comes from a FIXED set of variants — no
    // caller-supplied string reaches this key, so it cannot be varied to mint
    // unbounded "first occurrences" the way a per-message key could.
    static UNSERVABLE: crate::log::Latch = crate::log::Latch::new(50);
    match status.is_server_error() {
        true => {
            if let Some(occurrences) = UNSERVABLE.hit(refusal.reason()) {
                tracing::warn!(
                    target: "ducktape::admin",
                    reason = refusal.reason(),
                    status = status.as_u16(),
                    occurrences,
                    "admin cannot serve its own gate"
                );
            }
        }
        false => tracing::debug!(
            target: "ducktape::admin",
            reason = refusal.reason(),
            status = status.as_u16(),
            "admin request refused"
        ),
    }
    (
        status,
        Json(serde_json::json!({
            "error": refusal.message(),
            "reason": refusal.reason(),
        })),
    )
        .into_response()
}

/// the admin sub-router: control routes plus the owner gate. merged into the
/// main router only when exposure is enabled (see [`crate::router`]).
pub fn admin_router(handle: NodeHandle) -> Router<NodeHandle> {
    Router::new()
        .route("/v1/admin/ping", get(ping))
        .route("/v1/admin/shutdown", post(crate::shutdown))
        .route("/v1/admin/logs/tail", get(logs_tail))
        .route("/v1/admin/netstack/swap", post(netstack_swap))
        // upgrade staging: ingest + fan a wasm artifact out to members.
        .route(
            "/v1/admin/module-code/stage",
            post(crate::module_code::stage_module_code),
        )
        .route(
            "/v1/admin/module-code/{digest}",
            get(crate::module_code::module_code_status),
        )
        // route_layer, NOT layer: `layer` would also wrap this sub-router's
        // fallback, which `merge` adopts — sending every unmatched path on the
        // whole surface through the admin gate (a 403 where a 404 belongs).
        .route_layer(axum::middleware::from_fn_with_state(handle, admin_guard))
}

/// GET /v1/admin/ping — a cheap authenticated liveness probe. reaching it (a
/// 200) is exactly "admin namespace reachable" for the app's control predicate.
async fn ping() -> Response {
    Json(serde_json::json!({ "ok": true })).into_response()
}

/// POST /v1/admin/netstack/swap — move the reachability plane onto another
/// netstack backend, mid-life, without a retarget or a tunnel flap.
///
/// The operator can select a component without waiting for governance. Body
/// is `{"backend": {"component": "<path on the node disk>"}}`.
/// The path is read by the NODE, so a caller off-box never ships
/// bytes through this route.
///
/// A refused swap is a 409 and the running machine CONTINUES untouched (the
/// executor's contract); nothing here retries, because a component built
/// against another contract will be refused by name every time.
async fn netstack_swap(State(handle): State<NodeHandle>, body: Bytes) -> Response {
    let request = match serde_json::from_slice::<SwapBody>(&body) {
        Ok(body) => body.backend,
        Err(error) => {
            return swap_refusal(
                StatusCode::BAD_REQUEST,
                "netstack_swap_body_invalid",
                &error.to_string(),
            );
        }
    };
    let Some(outcome) = handle.status.swap_netstack(request).await else {
        return swap_refusal(
            StatusCode::SERVICE_UNAVAILABLE,
            "netstack_plane_absent",
            "this node runs no reachability plane, so it has no netstack backend to swap",
        );
    };
    match outcome {
        Ok(backend) => Json(serde_json::json!({ "backend": backend })).into_response(),
        Err(reason) => swap_refusal(StatusCode::CONFLICT, "netstack_swap_refused", &reason),
    }
}

/// the swap route's body. `deny_unknown_fields` so a misspelled key is a 400
/// rather than a silently ignored request that swapped nothing.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SwapBody {
    backend: crate::NetstackSwapRequest,
}

/// one refusal in [`refuse`]'s shape: a stable snake_case `reason` plus the
/// detail. The detail here is the PLANE's own string, which is why it is not
/// the reason token.
///
/// Level splits the way [`refuse`] splits it: 4xx is per-request and any
/// operator retry loop mints one, so it is `debug`; the 503 says this node
/// cannot serve the route at all, which is a standing fault worth a `warn`.
/// The plane logs `netstack_backend_swap_refused` itself either way.
fn swap_refusal(status: StatusCode, reason: &'static str, detail: &str) -> Response {
    match status.is_server_error() {
        true => tracing::warn!(
            target: "ducktape::reachability",
            reason,
            status = status.as_u16(),
            "netstack swap cannot be served on this node"
        ),
        false => tracing::debug!(
            target: "ducktape::reachability",
            reason,
            status = status.as_u16(),
            "netstack swap refused"
        ),
    }
    (
        status,
        Json(serde_json::json!({ "error": detail, "reason": reason })),
    )
        .into_response()
}

/// how many ring lines a tail returns by default / at most.
const TAIL_DEFAULT: usize = 200;
const TAIL_MAX: usize = 2000;

#[derive(Debug, Deserialize)]
struct TailParams {
    /// return lines with seq strictly greater than this. `0` (default) tails
    /// the most recent `limit` lines.
    #[serde(default)]
    after: u64,
    #[serde(default)]
    limit: Option<usize>,
}

/// GET /v1/admin/logs/tail — a simple read of the same in-memory log ring the
/// ws `logs` topic streams. diagnostics for an owner whose node is misbehaving.
async fn logs_tail(State(handle): State<NodeHandle>, Query(params): Query<TailParams>) -> Response {
    let limit = params.limit.unwrap_or(TAIL_DEFAULT).clamp(1, TAIL_MAX);
    let ring = handle.hub.log_ring();
    let after = if params.after == 0 {
        ring.latest_seq().saturating_sub(limit as u64)
    } else {
        params.after
    };
    let (rows, floor) = ring.read_after(after, limit);
    let lines: Vec<_> = rows
        .into_iter()
        .map(|(seq, line)| serde_json::json!({ "seq": seq, "line": line }))
        .collect();
    Json(serde_json::json!({ "floor": floor, "lines": lines })).into_response()
}

#[cfg(test)]
mod tests {
    // `Signer` (for `from_seed`/`public_key`) and `ed25519` arrive via this glob.
    use super::*;

    fn key(seed: u64) -> ed25519::PrivateKey {
        ed25519::PrivateKey::from_seed(seed)
    }

    fn headers_for(sig_hex: &str, key_hex: &str, ts: u64) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(ADMIN_KEY_HEADER, key_hex.parse().unwrap());
        h.insert(ADMIN_SIG_HEADER, sig_hex.parse().unwrap());
        h.insert(ADMIN_TS_HEADER, ts.to_string().parse().unwrap());
        h
    }

    /// the target node's key in these tests.
    const NODE: [u8; 32] = [0xab; 32];

    #[test]
    fn pop_message_binds_method_path_node_and_time() {
        // method, path, node key, and timestamp each move the signed bytes —
        // no field can bleed into another.
        assert_ne!(
            pop_message("POST", "/v1/admin/shutdown", &NODE, 100),
            pop_message("GET", "/v1/admin/shutdown", &NODE, 100),
        );
        assert_ne!(
            pop_message("POST", "/v1/admin/shutdown", &NODE, 100),
            pop_message("POST", "/v1/admin/logs/tail", &NODE, 100),
        );
        assert_ne!(
            pop_message("POST", "/v1/admin/shutdown", &NODE, 100),
            pop_message("POST", "/v1/admin/shutdown", &[0xcd; 32], 100),
        );
        assert_ne!(
            pop_message("POST", "/v1/admin/shutdown", &NODE, 100),
            pop_message("POST", "/v1/admin/shutdown", &NODE, 101),
        );
    }

    #[test]
    fn owner_signed_request_verifies_and_forged_fails() {
        let owner = key(1);
        let now = 1_000_000;
        let sig = sign_admin(&owner, "POST", "/v1/admin/shutdown", &NODE, now);
        let key_hex = duckfs_core::to_hex(owner.public_key().as_ref());
        let sig_hex = duckfs_core::to_hex(sig.as_ref());
        let headers = headers_for(&sig_hex, &key_hex, now);
        let subject = verify_pop(&headers, "POST", "/v1/admin/shutdown", &NODE, now)
            .expect("owner sig verifies");
        assert_eq!(subject, owner.public_key().as_ref().to_vec());

        // a different key signed it: PoP must fail.
        let attacker = key(2);
        let forged = sign_admin(&attacker, "POST", "/v1/admin/shutdown", &NODE, now);
        let bad = headers_for(
            &duckfs_core::to_hex(forged.as_ref()),
            &key_hex, // claims the owner's key ...
            now,
        );
        assert_eq!(
            verify_pop(&bad, "POST", "/v1/admin/shutdown", &NODE, now),
            Err(PopError::BadSig)
        );
    }

    #[test]
    fn a_signature_bound_to_a_different_path_is_rejected() {
        let owner = key(3);
        let now = 2_000_000;
        // signed for shutdown, replayed against logs/tail.
        let sig = sign_admin(&owner, "POST", "/v1/admin/shutdown", &NODE, now);
        let headers = headers_for(
            &duckfs_core::to_hex(sig.as_ref()),
            &duckfs_core::to_hex(owner.public_key().as_ref()),
            now,
        );
        assert_eq!(
            verify_pop(&headers, "GET", "/v1/admin/logs/tail", &NODE, now),
            Err(PopError::BadSig)
        );
    }

    /// the cross-NODE replay: a signature minted for node X, replayed verbatim
    /// against node Y by the same owner's traffic being captured. the node-key
    /// binding is exactly what must make this fail.
    #[test]
    fn a_signature_bound_to_a_different_node_is_rejected() {
        let owner = key(5);
        let now = 3_000_000;
        let sig = sign_admin(&owner, "POST", "/v1/admin/shutdown", &NODE, now);
        let headers = headers_for(
            &duckfs_core::to_hex(sig.as_ref()),
            &duckfs_core::to_hex(owner.public_key().as_ref()),
            now,
        );
        // same method, same path, same ts — a DIFFERENT node verifying.
        assert_eq!(
            verify_pop(&headers, "POST", "/v1/admin/shutdown", &[0xcd; 32], now),
            Err(PopError::BadSig)
        );
        // sanity: the intended node still accepts it.
        assert!(verify_pop(&headers, "POST", "/v1/admin/shutdown", &NODE, now).is_ok());
    }

    #[test]
    fn stale_timestamp_is_rejected_both_directions() {
        let owner = key(4);
        let signed_at = 5_000_000;
        let sig = sign_admin(&owner, "POST", "/v1/admin/shutdown", &NODE, signed_at);
        let headers = headers_for(
            &duckfs_core::to_hex(sig.as_ref()),
            &duckfs_core::to_hex(owner.public_key().as_ref()),
            signed_at,
        );
        // one second past the window, both directions.
        assert_eq!(
            verify_pop(
                &headers,
                "POST",
                "/v1/admin/shutdown",
                &NODE,
                signed_at + ADMIN_FRESHNESS_SECS + 1
            ),
            Err(PopError::Stale)
        );
        assert_eq!(
            verify_pop(
                &headers,
                "POST",
                "/v1/admin/shutdown",
                &NODE,
                signed_at - ADMIN_FRESHNESS_SECS - 1
            ),
            Err(PopError::Stale)
        );
        // exactly the window is still fresh.
        assert!(
            verify_pop(
                &headers,
                "POST",
                "/v1/admin/shutdown",
                &NODE,
                signed_at + ADMIN_FRESHNESS_SECS
            )
            .is_ok()
        );
    }

    #[test]
    fn missing_headers_are_missing_auth_not_a_crash() {
        let empty = HeaderMap::new();
        assert_eq!(
            verify_pop(&empty, "POST", "/v1/admin/shutdown", &NODE, 1),
            Err(PopError::MissingAuth)
        );
    }

    #[test]
    fn exposure_flag_parses_and_defaults_safe() {
        assert_eq!(AdminExposure::from_flag("off"), AdminExposure::Disabled);
        assert_eq!(AdminExposure::from_flag("public"), AdminExposure::Public);
        assert_eq!(
            AdminExposure::from_flag("loopback"),
            AdminExposure::Loopback
        );
        // unknown / empty ⇒ the safe default.
        assert_eq!(AdminExposure::from_flag("garbage"), AdminExposure::Loopback);
        assert_eq!(AdminExposure::from_flag(""), AdminExposure::Loopback);
        assert!(!AdminExposure::Disabled.enabled());
        assert!(AdminExposure::Loopback.enabled());
    }

    /// the operator credential is OWNER-ONLY on disk and round-trips — a
    /// world-readable one would hand admin to every process on the box, which
    /// is the exact thing this gate exists to stop.
    #[test]
    #[cfg(unix)]
    fn the_operator_token_is_owner_only_and_round_trips() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let minted = mint_operator_token(dir.path()).expect("mint");
        assert_eq!(
            read_operator_token(dir.path()).as_deref(),
            Ok(minted.as_str())
        );
        let mode = std::fs::metadata(dir.path().join(ADMIN_TOKEN_FILE))
            .expect("token file")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // fresh each boot: a second mint over the same dir replaces the secret,
        // so a restart invalidates a stale holder.
        let reminted = mint_operator_token(dir.path()).expect("re-mint");
        assert_ne!(reminted, minted);
        // and it is a real 32-byte secret, not a guessable constant.
        assert_eq!(minted.len(), 64);
    }

    #[test]
    fn a_refusal_names_one_stable_reason_per_variant() {
        // the reason token is the machine contract; no two variants may share
        // one, or a dashboard cannot tell "no credential" from "wrong one".
        let all = [
            AdminRefusal::NamespaceAbsent,
            AdminRefusal::OffBox,
            AdminRefusal::OperatorTokenUnavailable,
            AdminRefusal::OperatorTokenMissing,
            AdminRefusal::OperatorTokenMismatch,
            AdminRefusal::OwnerUnresolved,
            AdminRefusal::OwnerSignatureInvalid,
            AdminRefusal::OwnerSignatureStale,
            AdminRefusal::NotTheOwner,
        ];
        let mut reasons: Vec<&str> = all.iter().map(|r| r.reason()).collect();
        reasons.sort_unstable();
        let unique = reasons.len();
        reasons.dedup();
        assert_eq!(reasons.len(), unique, "two refusals share a reason token");
        // nothing is a 2xx, and nothing leaks prose that could carry a secret.
        assert!(all.iter().all(|r| !r.status().is_success()));
    }
}
