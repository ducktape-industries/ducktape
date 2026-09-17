mod frames;
mod manifest;
mod proxy;
pub use frames::*;
pub use manifest::*;
pub use proxy::*;

use std::collections::BTreeSet;

use borsh::{BorshDeserialize, BorshSerialize};
use duckdns::{DuckDnsName, HandleRegistration, ResolvedAccount};
use sdk::codec::{push_bytes, push_opt_str};
use serde::{Deserialize, Serialize};

/// Commonware signing namespace for every gateway route mutation.
pub const GATEWAY_ROUTE_NS: &[u8] = b"ducktape-gateway-route-v1";
/// Commonware signing namespace for every credential-record mutation. A leading
/// operation tag in each preimage keeps a set/remove/grant/revoke signature from
/// being replayed as another operation.
pub const GATEWAY_CREDENTIAL_NS: &[u8] = b"ducktape-gateway-credential-v1";
/// Signing namespace for a caller's proof of possession on a proxied request
/// (see [`caller_pop_preimage`]); verified by the serving node, never stored.
pub const GATEWAY_CALLER_NS: &[u8] = b"ducktape-gateway-caller-v1";
pub const MAX_CREDENTIAL_NAME_BYTES: usize = 64;
pub const MAX_CREDENTIAL_GRANTS: usize = 64;
pub const MAX_CHAIN_ID_BYTES: usize = 256;
pub const NODE_KEY_BYTES: usize = 32;
/// The largest public key any [`identity::KeyScheme`] carries (SEC1
/// uncompressed).
pub const MAX_SIGNER_KEY_BYTES: usize = 65;
/// Bound on a scheme-owned proof: a WebAuthn assertion envelope is a few
/// hundred bytes; an Ed25519/secp256k1 signature is 64/65.
pub const MAX_SIGNATURE_BYTES: usize = 2048;
pub const MAX_ROUTE_LABEL_BYTES: usize = 63;
pub const MAX_ROUTES_PER_ACCOUNT: usize = 64;
pub const MAX_AUDIENCE_ACCOUNTS: usize = 32;
pub const MAX_RESPONSE_BODY_BYTES: u64 = 4 * 1024 * 1024;
/// A statement now carries only scalars plus (at most) a 32-byte manifest hash,
/// so it is tiny; the manifest and file table live off consensus.
pub const MAX_ROUTE_STATEMENT_JSON_BYTES: usize = 4 * 1024;

/// The account apex (`None`) or one DNS-shaped label below it. The account is
/// carried separately so a route name can never cross authority boundaries.
#[derive(
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
)]
#[serde(deny_unknown_fields)]
pub struct RouteName {
    pub label: Option<String>,
}

impl RouteName {
    pub const fn apex() -> Self {
        Self { label: None }
    }

    pub fn named(label: impl Into<String>) -> Self {
        Self {
            label: Some(label.into()),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Some(label) = &self.label {
            validate_route_label(label)?;
        }
        Ok(())
    }

    /// Stable node-local key. `_apex` cannot collide with a legal DNS label.
    pub fn local_key(&self) -> &str {
        self.label.as_deref().unwrap_or("_apex")
    }
}

#[derive(
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RouteMethod {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
}

impl RouteMethod {
    pub const fn as_http_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }

    pub const fn permits_body(self) -> bool {
        matches!(self, Self::Post | Self::Put | Self::Patch | Self::Delete)
    }
}

/// A globally signed audience. Transport peers still have to be admitted by
/// the current network set; this policy then judges the caller's ACCOUNT —
/// the one its user proof of possession resolves to through Identity, or none
/// — before a request reaches the local upstream.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RouteAudience {
    /// Only a caller proven to be the owning account.
    Owner,
    /// Any admitted mesh peer, with or without a proven account.
    Network,
    /// Only the sorted, explicit account-number list. The owner is not implicit.
    Accounts { account_ids: Vec<u64> },
}

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RoutePolicy {
    pub audience: RouteAudience,
    /// Strictly sorted and unique. Unknown/unsafe HTTP methods do not exist in
    /// the wire enum, and each method must be explicitly signed into policy.
    pub methods: Vec<RouteMethod>,
    /// What this route accepts as a request body, counted AS IT STREAMS —
    /// nothing materializes a body to measure it. `None` is no cap at all:
    /// a route whose lane is a whole repository's history (a git push) takes
    /// whatever the client sends, exactly as it would from any other server.
    /// `Some(0)` is no body at all; `Some(n)` refuses the byte after `n`.
    ///
    /// There is deliberately no global ceiling above this. A cap here is a
    /// publisher's statement about its OWN lane, never the platform's
    /// statement about how large a request may be.
    pub max_request_bytes: Option<u64>,
    /// `0` means an unbounded (streaming/SSE) response, permitted only for
    /// `LoopbackHttp`. Content routes must set a real cap.
    pub max_response_bytes: u64,
    /// A caller may forward the explicit `Authorization` header only when this
    /// signed bit is true (fail-closed opt-in). `Cookie` flows unconditionally;
    /// the mesh-verified caller account is the authoritative identity.
    pub allow_authorization: bool,
    /// Whether a `LoopbackHttp` route may be upgraded to a WebSocket. Content
    /// routes must leave this false.
    pub allow_upgrade: bool,
}

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RouteTarget {
    /// Static content: the signed SHA-256 of the off-consensus manifest
    /// (`.manifest.json`) that lists the hash-bound files. See `manifest.rs`.
    DuckFs { manifest_sha256: String },
    /// HTTP semantics to one node-local loopback route. No port or URL is
    /// global state.
    LoopbackHttp,
}

impl RouteTarget {
    /// The JSON `kind` tag, as [`RouteSummary`] reports it.
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::DuckFs { .. } => "duck_fs",
            Self::LoopbackHttp => "loopback_http",
        }
    }
}

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RouteDefinition {
    pub target: RouteTarget,
    pub policy: RoutePolicy,
}

/// `route = None` is an authenticated tombstone and still advances the
/// independent per-name revision, preventing replay.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RouteStatement {
    pub chain_id: String,
    pub account_id: u64,
    pub name: RouteName,
    /// The 32-byte key of the node that serves this route. The signing
    /// account vouches for it; no node is bound to an account anywhere.
    pub publisher_node: Vec<u8>,
    pub revision: u64,
    pub route: Option<RouteDefinition>,
}

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemberAuthorization {
    /// A current member key of the statement's account; its stored
    /// [`identity::KeyScheme`] decides how `signature` is verified.
    pub signer: Vec<u8>,
    /// The scheme-owned proof bytes over the statement's preimage.
    pub signature: Vec<u8>,
}

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RouteRecord {
    pub statement: RouteStatement,
    pub authorization: MemberAuthorization,
}

/// Bounded management projection of one live route. In particular, DuckFS
/// manifests and signatures remain behind the exact-name `Get` query.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RouteSummary {
    pub name: RouteName,
    pub publisher_node: Vec<u8>,
    pub revision: u64,
    /// [`RouteTarget::kind_name`]: `"duck_fs"` or `"loopback_http"`.
    pub target: String,
}

/// A named API credential the owner co-hosts through their airlock gateway. The
/// registry stores routing metadata and the gateway's seal PUBLIC key ONLY —
/// never secret material. Grants are managed exclusively through the
/// owner-signed [`GatewayMsg::GrantCredential`]/[`GatewayMsg::RevokeCredential`]
/// ops, so they are never carried unsigned on [`GatewayMsg::SetCredential`].
#[derive(
    BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialKind {
    Claude,
    Codex,
    /// A Developer ID signing identity + App Store Connect key, lent for
    /// signing a release rather than for model calls.
    AppleCodesign,
}

impl CredentialKind {
    const fn tag(self) -> u8 {
        match self {
            Self::Claude => 1,
            Self::Codex => 2,
            Self::AppleCodesign => 3,
        }
    }
}

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CredentialRecord {
    pub name: String,
    pub owner_account: u64,
    pub publisher_node: Vec<u8>,
    pub kind: CredentialKind,
    pub seal_pk: [u8; 32],
    pub grants: BTreeSet<u64>,
}

/// The owner-signed registration statement. `record.grants` MUST be empty — the
/// signed preimage does not cover grants, so registration only carries metadata.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SetCredentialStatement {
    pub chain_id: String,
    pub record: CredentialRecord,
}

/// The owner-signed tombstone statement for [`GatewayMsg::RemoveCredential`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoveCredentialStatement {
    pub chain_id: String,
    pub owner_account: u64,
    pub name: String,
}

/// The owner-signed grant/revoke statement — one target account per op. Grant
/// and revoke share this shape but sign distinct preimages (op tag `3`/`4`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CredentialGrantStatement {
    pub chain_id: String,
    pub owner_account: u64,
    pub name: String,
    pub account: u64,
}

/// True when `account` is the owner or a granted account of `record`.
pub fn credential_use_allowed(record: &CredentialRecord, account: u64) -> bool {
    let is_owner = record.owner_account == account;
    let is_grantee = record.grants.contains(&account);
    is_owner || is_grantee
}

pub fn validate_credential_name(name: &str) -> Result<(), String> {
    let len_ok = (1..=MAX_CREDENTIAL_NAME_BYTES).contains(&name.len());
    let charset_ok = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !(len_ok && charset_ok) {
        return Err(format!(
            "gateway: credential name must be 1-{MAX_CREDENTIAL_NAME_BYTES} chars of [a-z0-9-]"
        ));
    }
    Ok(())
}

fn validate_chain_id(chain_id: &str) -> Result<(), String> {
    if chain_id.is_empty() || chain_id.len() > MAX_CHAIN_ID_BYTES {
        return Err(format!(
            "gateway: chain id must be 1..={MAX_CHAIN_ID_BYTES} bytes"
        ));
    }
    Ok(())
}

pub fn validate_set_credential_statement(statement: &SetCredentialStatement) -> Result<(), String> {
    validate_chain_id(&statement.chain_id)?;
    let record = &statement.record;
    validate_credential_name(&record.name)?;
    validate_account_number(record.owner_account)?;
    if record.publisher_node.len() != NODE_KEY_BYTES {
        return Err(format!(
            "gateway: publisher node must be {NODE_KEY_BYTES} bytes"
        ));
    }
    if !record.grants.is_empty() {
        return Err("gateway: credential registration carries no grants".into());
    }
    Ok(())
}

/// Signed under [`GATEWAY_CREDENTIAL_NS`], op tag `1`.
pub fn set_credential_preimage(statement: &SetCredentialStatement) -> Result<Vec<u8>, String> {
    validate_set_credential_statement(statement)?;
    let record = &statement.record;
    let mut out = vec![1u8];
    push_bytes(&mut out, statement.chain_id.as_bytes());
    out.extend_from_slice(&record.owner_account.to_le_bytes());
    push_bytes(&mut out, record.name.as_bytes());
    out.push(record.kind.tag());
    out.extend_from_slice(&record.seal_pk);
    push_bytes(&mut out, &record.publisher_node);
    Ok(out)
}

/// Signed under [`GATEWAY_CREDENTIAL_NS`], op tag `2`.
pub fn remove_credential_preimage(
    statement: &RemoveCredentialStatement,
) -> Result<Vec<u8>, String> {
    validate_chain_id(&statement.chain_id)?;
    validate_credential_name(&statement.name)?;
    validate_account_number(statement.owner_account)?;
    let mut out = vec![2u8];
    push_bytes(&mut out, statement.chain_id.as_bytes());
    out.extend_from_slice(&statement.owner_account.to_le_bytes());
    push_bytes(&mut out, statement.name.as_bytes());
    Ok(out)
}

/// Signed under [`GATEWAY_CREDENTIAL_NS`], op tag `3`.
pub fn grant_credential_preimage(statement: &CredentialGrantStatement) -> Result<Vec<u8>, String> {
    grant_op_preimage(3, statement)
}

/// Signed under [`GATEWAY_CREDENTIAL_NS`], op tag `4`.
pub fn revoke_credential_preimage(statement: &CredentialGrantStatement) -> Result<Vec<u8>, String> {
    grant_op_preimage(4, statement)
}

fn grant_op_preimage(op: u8, statement: &CredentialGrantStatement) -> Result<Vec<u8>, String> {
    validate_chain_id(&statement.chain_id)?;
    validate_credential_name(&statement.name)?;
    validate_account_number(statement.owner_account)?;
    validate_account_number(statement.account)?;
    let mut out = vec![op];
    push_bytes(&mut out, statement.chain_id.as_bytes());
    out.extend_from_slice(&statement.owner_account.to_le_bytes());
    push_bytes(&mut out, statement.name.as_bytes());
    out.extend_from_slice(&statement.account.to_le_bytes());
    Ok(out)
}

/// `sha256` of a request body held in one piece. A streaming caller hashes its
/// frames as they pass instead; both feed the same 32 bytes to
/// [`caller_pop_preimage`].
pub fn body_digest(body: &[u8]) -> [u8; 32] {
    use sha2::Digest as _;
    sha2::Sha256::digest(body).into()
}

/// The caller signs the publisher and every forwarded request field, including
/// the route revision, headers, upgrade mode and body digest. The proof itself
/// is excluded: installing its signature cannot change the signed bytes.
/// `body_digest` is `sha256` of the request body — never the body itself. The
/// preimage always covered only the digest, so a caller that streams its body
/// hashes it as it passes and proves itself afterwards; nothing on this path
/// needs the bytes in one piece.
pub fn caller_pop_preimage(
    publisher_node: &[u8],
    head: &crate::ProxyRequestHead,
    body_digest: &[u8; 32],
    ts: u64,
) -> Vec<u8> {
    let mut out = Vec::new();
    push_bytes(&mut out, publisher_node);
    out.extend_from_slice(&head.account_id.to_le_bytes());
    push_opt_str(&mut out, head.name.label.as_deref());
    out.extend_from_slice(&head.revision.to_le_bytes());
    push_bytes(&mut out, head.method.as_http_str().as_bytes());
    push_bytes(&mut out, head.path_and_query.as_bytes());
    out.extend_from_slice(&(head.headers.len() as u64).to_le_bytes());
    for header in &head.headers {
        push_bytes(&mut out, header.name.as_bytes());
        push_bytes(&mut out, header.value.as_bytes());
    }
    out.push(u8::from(head.upgrade));
    out.extend_from_slice(body_digest);
    out.extend_from_slice(&ts.to_le_bytes());
    out
}

/// The single message surface of the merged gateway module: the `.duck` handle
/// plane (`SetHandle`, the human-name → AccountId facet absorbed from duckdns)
/// and the route plane (`SetRoute`) are ops on ONE consensus module.
// the route variant dwarfs the handle one, but this enum is decoded once per op
// (never held in bulk), so boxing every SetRoute allocation to even the sizes
// out would only add indirection on a cold path.
#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayMsg {
    /// Declaratively replace the authenticated account's optional `.duck`
    /// handle. `None` unregisters it without changing Identity.
    SetHandle { handle: Option<String> },
    SetRoute {
        statement: RouteStatement,
        authorization: MemberAuthorization,
    },
    /// Register a named credential record (owner-signed). First registration in
    /// consensus order wins the name; a same-owner re-registration replaces the
    /// metadata and clears grants (grants are re-added with `GrantCredential`).
    SetCredential {
        statement: SetCredentialStatement,
        authorization: MemberAuthorization,
    },
    RemoveCredential {
        statement: RemoveCredentialStatement,
        authorization: MemberAuthorization,
    },
    GrantCredential {
        statement: CredentialGrantStatement,
        authorization: MemberAuthorization,
    },
    RevokeCredential {
        statement: CredentialGrantStatement,
        authorization: MemberAuthorization,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayQuery {
    /// Resolve one `.duck` name to the account number it aliases. Node
    /// selection is deliberately absent — resolution stops at the account.
    Resolve {
        name: DuckDnsName,
    },
    /// The optional-handle registration listing, paginated.
    Registrations {
        from: u64,
        limit: u64,
    },
    Get {
        account_id: u64,
        name: RouteName,
    },
    /// Every currently published route owned by one account, in canonical
    /// [`RouteName`] order. Tombstones remain queryable through `Get` so a
    /// publisher can continue the revision stream, but are not management
    /// surface entries.
    List {
        account_id: u64,
    },
    /// Resolve one credential name to its record, or `None`.
    Credential {
        name: String,
    },
    /// Every registered credential record, in canonical name order.
    Credentials {},
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayReply {
    /// The account number a `.duck` name resolves to, or `None`.
    Resolved(Option<ResolvedAccount>),
    Registrations(Vec<HandleRegistration>),
    /// Boxed only to keep the Rust reply enum bounded after adding the compact
    /// list variant; `Box` is transparent in the JSON wire representation.
    Route(Box<Option<RouteRecord>>),
    Routes(Vec<RouteSummary>),
    Credential(Option<CredentialRecord>),
    Credentials(Vec<CredentialRecord>),
}

pub fn encode_msg(message: &GatewayMsg) -> Vec<u8> {
    serde_json::to_vec(message).expect("serializable")
}

pub fn decode_msg(bytes: &[u8]) -> Result<GatewayMsg, String> {
    serde_json::from_slice(bytes).map_err(|error| error.to_string())
}

pub fn encode_query(query: &GatewayQuery) -> Vec<u8> {
    serde_json::to_vec(query).expect("serializable")
}

pub fn decode_query(bytes: &[u8]) -> Result<GatewayQuery, String> {
    serde_json::from_slice(bytes).map_err(|error| error.to_string())
}

pub fn encode_reply(reply: &GatewayReply) -> Vec<u8> {
    serde_json::to_vec(reply).expect("serializable")
}

pub fn decode_reply(bytes: &[u8]) -> Result<GatewayReply, String> {
    serde_json::from_slice(bytes).map_err(|error| error.to_string())
}

pub fn validate_route_statement(statement: &RouteStatement) -> Result<(), String> {
    if statement.chain_id.is_empty() || statement.chain_id.len() > MAX_CHAIN_ID_BYTES {
        return Err(format!(
            "gateway: chain id must be 1..={MAX_CHAIN_ID_BYTES} bytes"
        ));
    }
    validate_account_number(statement.account_id)?;
    statement.name.validate()?;
    if statement.publisher_node.len() != NODE_KEY_BYTES {
        return Err(format!(
            "gateway: publisher node must be {NODE_KEY_BYTES} bytes"
        ));
    }
    if statement.revision == 0 {
        return Err("gateway: route revision starts at 1".into());
    }
    if let Some(route) = &statement.route {
        validate_route(route)?;
    }
    if serde_json::to_vec(statement)
        .expect("route statement serializes")
        .len()
        > MAX_ROUTE_STATEMENT_JSON_BYTES
    {
        return Err(format!(
            "gateway: route statement exceeds {MAX_ROUTE_STATEMENT_JSON_BYTES} bytes"
        ));
    }
    Ok(())
}

/// Shape bounds only: which scheme the signer is, and whether the proof
/// verifies, is decided against the signer's stored Identity key.
pub fn validate_authorization(authorization: &MemberAuthorization) -> Result<(), String> {
    let signer_fits = (1..=MAX_SIGNER_KEY_BYTES).contains(&authorization.signer.len());
    if !signer_fits {
        return Err(format!(
            "gateway: signer must be 1..={MAX_SIGNER_KEY_BYTES} bytes"
        ));
    }
    let signature_fits = (1..=MAX_SIGNATURE_BYTES).contains(&authorization.signature.len());
    if !signature_fits {
        return Err(format!(
            "gateway: signature must be 1..={MAX_SIGNATURE_BYTES} bytes"
        ));
    }
    Ok(())
}

pub fn validate_route_label(label: &str) -> Result<(), String> {
    if label.is_empty() || label.len() > MAX_ROUTE_LABEL_BYTES {
        return Err(format!(
            "gateway: route label must be 1..={MAX_ROUTE_LABEL_BYTES} bytes"
        ));
    }
    if label.starts_with('-')
        || label.ends_with('-')
        || !label
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(format!("gateway: invalid route label: {label:?}"));
    }
    Ok(())
}

/// Identity numbers accounts from 1; `0` is never an account.
pub fn validate_account_number(number: u64) -> Result<(), String> {
    if number == 0 {
        return Err("gateway: account number must be non-zero".into());
    }
    Ok(())
}

pub fn validate_route(route: &RouteDefinition) -> Result<(), String> {
    validate_policy(&route.policy)?;
    match &route.target {
        RouteTarget::DuckFs { manifest_sha256 } => {
            if !is_canonical_sha256(manifest_sha256) {
                return Err("gateway: manifest_sha256 must be 64 lowercase hex chars".into());
            }
            // The file table is off consensus, so per-file fit against the
            // response cap is enforced at serve time (against MAX_FILE_BYTES),
            // not here. Content stays GET+HEAD, bodyless, no auth, no upgrade,
            // and must set a real (non-streaming) response cap.
            if route.policy.methods != [RouteMethod::Get, RouteMethod::Head]
                || route.policy.max_request_bytes != Some(0)
                || route.policy.allow_authorization
                || route.policy.allow_upgrade
                || route.policy.max_response_bytes == 0
            {
                return Err(
                    "gateway: content routes require GET+HEAD, no request body, no Authorization, \
                     no upgrade, and a bounded response cap"
                        .into(),
                );
            }
        }
        RouteTarget::LoopbackHttp => {}
    }
    Ok(())
}

pub fn validate_policy(policy: &RoutePolicy) -> Result<(), String> {
    if policy.methods.is_empty() {
        return Err("gateway: policy must allow at least one method".into());
    }
    let mut previous = None;
    for method in &policy.methods {
        if previous.is_some_and(|old| old >= method) {
            return Err("gateway: methods must be strictly sorted and unique".into());
        }
        previous = Some(method);
    }
    // `max_response_bytes == 0` means an unbounded stream (SSE); a non-zero
    // value is a publisher-chosen cap the serving side counts against. Real
    // byte ceilings are enforced at serve time, not signed into consensus —
    // which is also why `max_request_bytes` has no ceiling to check here: a
    // request body is counted as it streams, so a cap bounds what this route
    // accepts and nothing bounds what the transport can carry.
    let forbids_a_body = policy.max_request_bytes == Some(0);
    if policy.methods.iter().any(|method| method.permits_body()) && forbids_a_body {
        return Err("gateway: a body-bearing method may not pin a zero request cap".into());
    }
    match &policy.audience {
        RouteAudience::Owner | RouteAudience::Network => {}
        RouteAudience::Accounts { account_ids } => {
            if account_ids.is_empty() || account_ids.len() > MAX_AUDIENCE_ACCOUNTS {
                return Err(format!(
                    "gateway: explicit audience needs 1..={MAX_AUDIENCE_ACCOUNTS} accounts"
                ));
            }
            let mut previous: Option<u64> = None;
            for &account in account_ids {
                validate_account_number(account)?;
                if previous.is_some_and(|old| old >= account) {
                    return Err(
                        "gateway: audience accounts must be strictly sorted and unique".into(),
                    );
                }
                previous = Some(account);
            }
        }
    }
    Ok(())
}

pub fn is_canonical_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Deterministic, append-only bytes signed under [`GATEWAY_ROUTE_NS`].
pub fn route_signing_preimage(statement: &RouteStatement) -> Result<Vec<u8>, String> {
    validate_route_statement(statement)?;
    let mut out = Vec::new();
    push_bytes(&mut out, statement.chain_id.as_bytes());
    out.extend_from_slice(&statement.account_id.to_le_bytes());
    push_opt_str(&mut out, statement.name.label.as_deref());
    push_bytes(&mut out, &statement.publisher_node);
    out.extend_from_slice(&statement.revision.to_le_bytes());
    let Some(route) = &statement.route else {
        out.push(0);
        return Ok(out);
    };
    out.push(1);
    encode_policy(&mut out, &route.policy);
    match &route.target {
        RouteTarget::DuckFs { manifest_sha256 } => {
            out.push(1);
            out.extend_from_slice(&decode_lower_hex_32(manifest_sha256)?);
        }
        RouteTarget::LoopbackHttp => out.push(2),
    }
    Ok(out)
}

fn encode_policy(out: &mut Vec<u8>, policy: &RoutePolicy) {
    match &policy.audience {
        RouteAudience::Owner => out.push(1),
        RouteAudience::Network => out.push(2),
        RouteAudience::Accounts { account_ids } => {
            out.push(3);
            out.extend_from_slice(&(account_ids.len() as u64).to_le_bytes());
            for account in account_ids {
                out.extend_from_slice(&account.to_le_bytes());
            }
        }
    }
    out.extend_from_slice(&(policy.methods.len() as u64).to_le_bytes());
    for method in &policy.methods {
        out.push(match method {
            RouteMethod::Get => 1,
            RouteMethod::Head => 2,
            RouteMethod::Post => 3,
            RouteMethod::Put => 4,
            RouteMethod::Patch => 5,
            RouteMethod::Delete => 6,
        });
    }
    // an absent cap and a cap of any value must never sign the same bytes, so
    // the tag goes in ahead of the number.
    match policy.max_request_bytes {
        None => out.push(0),
        Some(cap) => {
            out.push(1);
            out.extend_from_slice(&cap.to_le_bytes());
        }
    }
    out.extend_from_slice(&policy.max_response_bytes.to_le_bytes());
    out.push(u8::from(policy.allow_authorization));
    out.push(u8::from(policy.allow_upgrade));
}

fn decode_lower_hex_32(value: &str) -> Result<[u8; 32], String> {
    if !is_canonical_sha256(value) {
        return Err("gateway: SHA-256 must be 64 lowercase hex characters".into());
    }
    let mut out = [0u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        out[index] = (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]);
    }
    Ok(out)
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => unreachable!("validated lowercase hex"),
    }
}
