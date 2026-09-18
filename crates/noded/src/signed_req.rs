//! the ONE per-request proof-of-possession (PoP) this node verifies, and the
//! gate that requires one on every MUTATING `/v1` route.
//!
//! ## why a signature and not a token
//!
//! a mutating request changes committed state, the node's own files, or a
//! process it hosts. "can dial this port" is not authority for any of that:
//! loopback is precisely what the audit found trusted, and widening
//! `http_listen` hands the whole surface to anyone who can open a socket. so a
//! mutation carries a signature by the key that is ACTING, and the node
//! verifies it before the handler runs. reads stay open.
//!
//! ## two namespaces, one verifier
//!
//! `/v1/admin/*` (control: shutdown, module code, log tail) already had this
//! shape ([`crate::admin`]). the data plane needs the same primitive with a
//! DIFFERENT bind, so the verifier lives here and each namespace owns its own
//! signing namespace and message:
//!
//! - control: [`crate::admin::ADMIN_REQ_NS`] over `method ‖ path ‖ node ‖ ts`.
//!   the body is deliberately unsigned there — `module-code/stage` streams a
//!   large artifact the gate must not buffer.
//! - data: [`DATA_REQ_NS`] over `method ‖ path ‖ node ‖ ts ‖ sha256(body)`.
//!   every gated body is already buffered by its handler (they all take
//!   `Bytes`/`Json`), and the WHOLE POINT of this gate is that the bytes a
//!   caller signed for are the bytes that land — an unsigned body would let a
//!   network attacker swap the chunk, the commit's changes, or the object.
//!
//! a signature is bound to the TARGET NODE's consensus key, so one minted for
//! node X never replays against node Y; and to a timestamp inside
//! [`FRESHNESS_SECS`], so a captured request dies with the window.
//!
//! ## the second credential: the node's OWN daemons
//!
//! some mutations are the NODE's, not a person's. a capability announce keys
//! the registry on the submitting node; a compute lease heartbeat and bid are
//! held BY the node; an agent run's provisioner writes duckfs on the node's
//! behalf. a user signature there would name the WRONG actor, so those callers
//! present what `/v1/admin/*` already asks of a local daemon
//! ([`crate::admin::ADMIN_TOKEN_HEADER`]): this boot's 0600 workspace secret,
//! from a loopback peer. "can read the node's own workspace", never "can dial
//! the port" — a sandbox guest, which reaches this listener through a vsock
//! tunnel with no workspace, cannot read it.
//!
//! ## TWO layers, because a PoP is not a credential
//!
//! a PoP proves POSSESSION of the key named in the headers and nothing else:
//! there is no ACL read here, so ANY well-formed key may act — a caller with no
//! standing anywhere mints a keypair and signs. that is the right bar for a
//! MODULE-BOUND mutation, where the verified key becomes the op's
//! `Origin::External` ([`SignedBy`]) and the module's own `check_authority`
//! decides; it is the same bar `/v1/submit/frame` sets.
//!
//! it is NOT a bar at all for a mutation that changes the NODE — minting a
//! mesh invite, retuning the process-wide log filter, spawning a host pty,
//! deleting a managed checkout dir. those handlers read no acting identity,
//! so "any well-formed key" is "anyone who can dial the port", the sandbox
//! guest's tunnelled loopback included. so [`Lane::authority`] classifies every
//! mutating route, and a [`Authority::Operator`] one admits only:
//!
//! - the operator credential above, or
//! - a PoP by the key this node knows as its operator's
//!   ([`crate::AdminConfig::owner_key`] — the active wallet key read from this
//!   host's keystore at boot, which is what `ducktape node log-filter` signs
//!   with). no wallet on the host = no such key = the token is the only way in.
//!
//! nothing else. which credential was presented decides the ACTING identity: a
//! signature acts as its key, the operator credential acts as the node.
//!
//! ponytail: possession per module, operator-key equality for the node's own
//! mutations; 30s replay window; TLS is the op's job.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use commonware_codec::DecodeExt as _;
use commonware_cryptography::{Verifier as _, ed25519};

use crate::NodeHandle;

/// the CLIENT half — the namespace, the header trio, the canonical message
/// and the signing — lives with the kernel's frame codec so every signer in
/// the tree links the one spelling the daemon verifies against.
pub use ::node::signed_req::{
    DATA_REQ_NS, KEY_HEADER, SIG_HEADER, TS_HEADER, now_secs, request_headers, request_message,
    sign_request,
};

/// the header trio one namespace's PoP travels in. control and data use
/// different names so a control credential can never be replayed onto a data
/// route by a proxy that copies headers wholesale.
#[derive(Clone, Copy)]
pub struct PopHeaders {
    pub key: &'static str,
    pub ts: &'static str,
    pub sig: &'static str,
}

/// the data plane's header trio.
pub const DATA_HEADERS: PopHeaders = PopHeaders {
    key: KEY_HEADER,
    ts: TS_HEADER,
    sig: SIG_HEADER,
};

/// max clock skew (seconds) between a request timestamp and this node. ONE
/// window for both namespaces: two would only ever drift apart.
pub const FRESHNESS_SECS: u64 = 30;

/// axum's own default body cap, which is what every gated route that sets no
/// `DefaultBodyLimit` of its own already enforces — the json lanes
/// (`/v1/submit`, `/v1/invite`, the workspace RPC) and the
/// filter string. Spelled here because the middleware runs OUTSIDE the route's
/// layers and so cannot read the limit they install.
const DEFAULT_JSON_BODY_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PopError {
    /// a required header is missing or malformed.
    MissingAuth,
    /// timestamp outside the freshness window.
    Stale,
    /// the account key or signature bytes are not valid ed25519.
    BadKey,
    /// the signature did not verify against the account key.
    BadSig,
}

/// the key that a well-formed, fresh, correctly-signed request proves
/// possession of — or why it failed. `message` builds the canonical bytes from
/// the timestamp the headers carry, which is why it is a closure: the ts must
/// be parsed and range-checked BEFORE it can be folded into a signed message.
///
/// decides possession only. membership/ownership is a separate read (see
/// `admin::resolve_owner`); this function never consults state.
pub(crate) fn verify_pop(
    headers: &HeaderMap,
    names: PopHeaders,
    ns: &[u8],
    now: u64,
    message: impl FnOnce(u64) -> Vec<u8>,
) -> Result<Vec<u8>, PopError> {
    let claim = read_pop(headers, names, now)?;
    let message = message(claim.ts);
    claim.against(ns, &message)
}

/// a well-formed, fresh proof-of-possession whose signature has not been
/// checked against a message yet — everything about a request that can be
/// judged WITHOUT its body.
pub(crate) struct PopClaim {
    key_bytes: Vec<u8>,
    pubkey: ed25519::PublicKey,
    sig: ed25519::Signature,
    /// the timestamp the signed message must be built from.
    pub(crate) ts: u64,
}

/// read the proof out of the headers: present, fresh, and decodable. A caller
/// that cannot have the message yet — the blob lane hashes its body as it
/// streams to disk — runs this FIRST, so a request with no headers, a stale
/// timestamp or an undecodable key is refused at byte zero instead of after a
/// whole upload.
pub(crate) fn read_pop(
    headers: &HeaderMap,
    names: PopHeaders,
    now: u64,
) -> Result<PopClaim, PopError> {
    let key_hex = header_str(headers, names.key).ok_or(PopError::MissingAuth)?;
    let ts_str = header_str(headers, names.ts).ok_or(PopError::MissingAuth)?;
    let sig_hex = header_str(headers, names.sig).ok_or(PopError::MissingAuth)?;

    let ts: u64 = ts_str.parse().map_err(|_| PopError::MissingAuth)?;
    if now.abs_diff(ts) > FRESHNESS_SECS {
        return Err(PopError::Stale);
    }

    let key_bytes = from_hex(key_hex).ok_or(PopError::BadKey)?;
    let sig_bytes = from_hex(sig_hex).ok_or(PopError::BadKey)?;
    let pubkey = ed25519::PublicKey::decode(key_bytes.as_slice()).map_err(|_| PopError::BadKey)?;
    let sig = ed25519::Signature::decode(sig_bytes.as_slice()).map_err(|_| PopError::BadKey)?;

    Ok(PopClaim {
        key_bytes,
        pubkey,
        sig,
        ts,
    })
}

impl PopClaim {
    /// the last question, and the only one that needs the message: does this
    /// signature actually cover these bytes?
    pub(crate) fn against(self, ns: &[u8], message: &[u8]) -> Result<Vec<u8>, PopError> {
        match self.pubkey.verify(ns, message, &self.sig) {
            true => Ok(self.key_bytes),
            false => Err(PopError::BadSig),
        }
    }
}

pub(crate) fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

/// decode an even-length lowercase/uppercase hex string to bytes.
pub(crate) fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// the verified acting identity, put on the request by [`signed_write_guard`]
/// and read by any gated handler that submits on the caller's behalf. its
/// presence IS the proof the gate ran: a handler that finds none was reached
/// on an ungated route.
#[derive(Clone, Debug)]
pub struct SignedBy(pub Vec<u8>);

/// a proof the gate could not check for itself, handed to the handler that
/// can. ONE lane travels this way — the blob upload, whose body is unbounded
/// and streams to disk, so the digest the signature binds only exists once the
/// handler has written every byte. The handler MUST call
/// [`DeferredProof::verify`] before it publishes anything: a request carrying
/// this extension has been admitted by nobody yet.
#[derive(Clone, Debug)]
pub struct DeferredProof {
    method: Method,
    path_and_query: String,
    headers: HeaderMap,
}

impl DeferredProof {
    /// everything about this request that can be refused BEFORE its body:
    /// this node has a key to bind against, the headers are there, the
    /// timestamp is fresh, and the key and signature decode. Only the digest
    /// binding has to wait, so a caller with no proof at all never gets to
    /// stream a byte onto this node's disk.
    pub fn precheck(&self, handle: &NodeHandle) -> Result<(), WriteRefusal> {
        self.claim(handle).map(|_| ())
    }

    fn claim(&self, handle: &NodeHandle) -> Result<(PopClaim, Vec<u8>), WriteRefusal> {
        let Some(node_key) = handle.admin.node_key.clone().filter(|k| !k.is_empty()) else {
            return Err(WriteRefusal::NodeUnidentified);
        };
        let claim = read_pop(&self.headers, DATA_HEADERS, now_secs())?;
        Ok((claim, node_key))
    }

    /// the acting key that signed for a body with this digest, or the refusal
    /// to answer the caller with.
    pub fn verify(&self, handle: &NodeHandle, digest: &[u8; 32]) -> Result<Vec<u8>, WriteRefusal> {
        let (claim, node_key) = self.claim(handle)?;
        let message = node::signed_req::request_message_digest(
            self.method.as_str(),
            &self.path_and_query,
            &node_key,
            claim.ts,
            digest,
        );
        Ok(claim.against(DATA_REQ_NS, &message)?)
    }
}

impl From<PopError> for WriteRefusal {
    fn from(error: PopError) -> Self {
        match error {
            PopError::MissingAuth => WriteRefusal::SignatureMissing,
            PopError::Stale => WriteRefusal::SignatureStale,
            PopError::BadKey => WriteRefusal::SignatureMalformed,
            PopError::BadSig => WriteRefusal::SignatureInvalid,
        }
    }
}

/// which mutating lane a request path belongs to — the ONE discriminant
/// [`Lane::mutates`] branches on. `Open` is everything else: reads, the
/// self-authenticating `/v1/submit/frame`, the volatile service-hello, the
/// websocket upgrades, and `/v1/admin/*` (which carries its own gate).
///
/// Two of those websocket upgrades are NOT actually unauthenticated:
/// `/v1/presence/ws` (`crate::call`) stays `Open` here — this
/// gate is PoP-by-signature, and a live huddle/page has no acting key to sign
/// with — but each checks its own `?token=` query param against this node's
/// workspace secret before `on_upgrade`, the same [`Admission::Workspace`]
/// proof `/v1/ws`'s gated topics already ask for (see
/// `crate::stream::Admission`).
///
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Lane {
    /// `/v1/fs/workspaces…` — create, commit, delete a managed checkout.
    Workspace,
    /// The node-local content-addressed blob store's upload endpoint.
    Blob,
    /// `/v1/submit` — the frameless op lane ([`SUBMIT_PATH`]).
    Submit,
    /// Operator-authorized opaque module payload; authored as the node.
    RawSubmit,
    /// a fixed path that mutates the NODE ([`NODE_LEVEL_POSTS`]).
    NodeLevel,
    /// Operator-authenticated application HTTP and WebSocket requests.
    GatewayOperator,
    /// `/v1/huddle/node-proof` ([`HUDDLE_PROOF_PATH`]) — this node signs that it
    /// will route the SIGNER's huddle media. the handler reads [`SignedBy`]
    /// and binds exactly that key, so possession is the right bar here:
    /// a device pointed at a node it does not host has no operator credential
    /// and still has to be able to join a huddle through it.
    HuddleProof,
    RunControl,
    Open,
}

/// what a mutating request has to PROVE on its lane — the ONE thing that splits
/// the module-bound routes from the node's own.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Authority {
    /// possession of ANY key. the verified key rides on as [`SignedBy`], the
    /// handler submits under it, and the module's `check_authority` is what
    /// actually decides — so an unknown key gets a refusal from the module, not
    /// from this gate.
    Acting,
    /// this node's OPERATOR. the handler reads no acting identity, so there is
    /// no second decider downstream and possession proves nothing: only the
    /// operator credential, or a PoP by [`crate::AdminConfig::owner_key`], gets
    /// through.
    Operator,
}

/// the exact mutating POST paths that mutate the NODE rather than module state:
/// `/v1/log-filter` retunes this process's tracing filter (a `trace` fills the
/// operator's disk through `daemon.log`) and `/v1/invite` mints a bearer right
/// to join this mesh for up to a year — neither is a module write any acting
/// key should be able to ask for. neither handler reads [`SignedBy`], which is
/// exactly why neither may be admitted on possession alone.
const NODE_LEVEL_POSTS: &[&str] = &["/v1/log-filter", "/v1/invite"];

/// the huddle node-proof mint ([`Lane::HuddleProof`]). an exact path: the
/// handler binds the verified signer, and the account check is its own.
pub(crate) const HUDDLE_PROOF_PATH: &str = "/v1/huddle/node-proof";

/// the frameless op lane. an EXACT match, not a prefix: `/v1/submit/frame`
/// carries its own signature inside the frame and stays open, and the other
/// POSTs that read (`/v1/query`, `/v1/index/{m}/view`, `/v1/gateway/proxy`) are
/// in neither table.
const SUBMIT_PATH: &str = "/v1/submit";

const WORKSPACE_PREFIX: &str = "/v1/fs/workspaces";
const BLOB_PATH: &str = "/v1/files/blob";
/// Disjoint operating-system service path prefixes.
const LANE_PREFIXES: &[(&str, Lane)] = &[(WORKSPACE_PREFIX, Lane::Workspace)];

fn lane_of(path: &str) -> Lane {
    if path == "/v1/gateway/operator" {
        return Lane::GatewayOperator;
    }
    if path.starts_with("/v1/submit/raw/") {
        return Lane::RawSubmit;
    }
    if path == BLOB_PATH {
        return Lane::Blob;
    }
    if path == "/v1/run-control" {
        return Lane::RunControl;
    }
    if let Some((_, lane)) = LANE_PREFIXES
        .iter()
        .find(|(prefix, _)| path.starts_with(prefix))
    {
        return *lane;
    }
    if NODE_LEVEL_POSTS.contains(&path) {
        return Lane::NodeLevel;
    }
    if path == HUDDLE_PROOF_PATH {
        return Lane::HuddleProof;
    }
    match path == SUBMIT_PATH {
        true => Lane::Submit,
        false => Lane::Open,
    }
}

impl Lane {
    /// does this method MUTATE on this lane, and what must it prove? `None` is
    /// a read. one discriminant, one match — a new route that mutates is added
    /// to [`lane_of`]'s table and classified HERE, never at a call site.
    fn authority(self, method: &Method) -> Option<Authority> {
        let posts = *method == Method::POST;
        let removes = *method == Method::DELETE;
        match self {
            // POST creates and commits a managed checkout AS the acting key
            // (`workspaces::acting_origin` → the duckfs authority check).
            // DELETE `remove_dir_all`s the dir and reads no identity at all, so
            // possession would let any caller wipe another run's checkout.
            Lane::Workspace => posts
                .then_some(Authority::Acting)
                .or(removes.then_some(Authority::Operator)),
            Lane::Blob => posts.then_some(Authority::Acting),
            // `/v1/submit` is the FRAMELESS lane: unlike Files/Workspace, the
            // verified `SignedBy` key does NOT ride on as the op's origin — the
            // validator re-signs the framed op with ITS OWN consensus key
            // (`bin/node/src/validator/run/ingress.rs`, `node_link.rs`), the
            // shape the node's own daemons need for an op that must carry the
            // NODE's identity (a capability announce, a lease bid, a run bind).
            // possession-of-any-key was therefore enough to mint an op under
            // the VALIDATOR's own key (#1808) — so this lane is Operator-only,
            // and a user submits through the self-authenticating
            // `/v1/submit/frame` instead, whose signature IS the op's origin.
            Lane::Submit | Lane::RawSubmit => posts.then_some(Authority::Operator),
            // the two fixed node mutations.
            Lane::NodeLevel => posts.then_some(Authority::Operator),
            // the proof binds the SIGNER; the handler refuses a key that holds
            // no account, so possession is the gate's whole job here.
            Lane::HuddleProof | Lane::RunControl => posts.then_some(Authority::Acting),
            Lane::GatewayOperator => {
                let exchange = posts || *method == Method::GET;
                exchange.then_some(Authority::Operator)
            }
            Lane::Open => None,
        }
    }

    /// the largest body this gate will read in order to hash it.
    ///
    /// Buffering precedes signature verification, so each lane spends only
    /// the body budget of its underlying endpoint. A body this gate would
    /// have to hold is therefore never the same question as how much the
    /// ROUTE accepts: the blob route takes an unbounded stream, and the
    /// operator credential — the one a push arrives with — skips this gate
    /// entirely and streams to the handler.
    fn max_body(self) -> usize {
        match self {
            // the blob lane never asks: [`signed_write_guard`] hands its proof
            // to the handler and returns before any buffering, because the
            // body is an unbounded stream the handler hashes to disk. A number
            // here would be a cap nothing applies.
            Lane::Blob => {
                unreachable!("the blob lane defers its proof instead of buffering a body")
            }
            Lane::RawSubmit => node::MAX_PAYLOAD_BYTES,
            Lane::GatewayOperator => {
                crate::gateway_http::JSON_LANE_REQUEST_BYTES * 2 + gateway::MAX_PROXY_HEAD_BYTES
            }
            // json bodies and the log-filter string. `Open` never reaches here
            // (the guard returns before asking), and takes the small cap so a
            // table that ever disagreed fails closed rather than wide.
            Lane::Workspace | Lane::Submit | Lane::NodeLevel | Lane::HuddleProof | Lane::Open => {
                DEFAULT_JSON_BODY_BYTES
            }
            Lane::RunControl => 64 * 1024,
        }
    }
}

/// what this request must prove, or `None` if it is a read. the guard itself
/// asks [`Lane::authority`] on the lane it already resolved, so this pairing of
/// the two is only ever what the tests below assert against.
#[cfg(test)]
fn required_authority(method: &Method, path: &str) -> Option<Authority> {
    lane_of(path).authority(method)
}

/// the plane a refusal is logged on. a files/duckfs mutation belongs to
/// `ducktape::files`, everything else to the node's own plane.
fn plane_of(path: &str) -> Plane {
    let is_files = path.starts_with("/v1/files") || path.starts_with(WORKSPACE_PREFIX);
    match is_files {
        true => Plane::Files,
        false => Plane::Node,
    }
}

#[derive(Clone, Copy)]
enum Plane {
    Files,
    Node,
}

/// Why a mutating request was turned away. status and the stable snake_case
/// `reason` are DERIVED from the variant, exactly like [`crate::admin::AdminRefusal`],
/// so they cannot drift apart. no message names the expected credential or the
/// request URI — the caller learns only which check it failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteRefusal {
    /// no signature headers at all, or one of them is unparseable.
    SignatureMissing,
    /// the timestamp is outside the freshness window.
    SignatureStale,
    /// the key or signature bytes are not valid ed25519.
    SignatureMalformed,
    /// a well-formed signature that does not bind this request.
    SignatureInvalid,
    /// a VALID signature, by a key this node does not know as its operator's,
    /// on a route that changes the node rather than module state.
    NotOperator,
    /// the body is larger than any gated route accepts (or its stream broke).
    BodyOverCap,
    /// this node carries no consensus key to salt the signature with — an
    /// embedded daemon binding the empty salt would verify a signature minted
    /// for ANY such daemon, so it refuses instead of falling back to a
    /// wildcard.
    NodeUnidentified,
}

impl WriteRefusal {
    /// the stable snake_case token — greppable, countable, never prose.
    pub fn reason(self) -> &'static str {
        match self {
            Self::SignatureMissing => "signature_missing",
            Self::SignatureStale => "signature_stale",
            Self::SignatureMalformed => "signature_malformed",
            Self::SignatureInvalid => "signature_invalid",
            Self::NotOperator => "not_operator",
            Self::BodyOverCap => "body_over_cap",
            Self::NodeUnidentified => "node_unidentified",
        }
    }

    /// 401 for "you presented nothing usable"; 403 for a credential that IS
    /// usable and is not the one this route wants — retrying with a fresher
    /// signature would not help, so it must not read as 401; 413 for a body
    /// this gate could not read to hash.
    pub fn status(self) -> StatusCode {
        match self {
            Self::SignatureMissing
            | Self::SignatureStale
            | Self::SignatureMalformed
            | Self::SignatureInvalid => StatusCode::UNAUTHORIZED,
            Self::NotOperator => StatusCode::FORBIDDEN,
            Self::BodyOverCap => StatusCode::PAYLOAD_TOO_LARGE,
            // this node's own condition, not a defect in what the caller
            // presented — retrying with any signature cannot fix it.
            Self::NodeUnidentified => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::SignatureMissing => {
                "this route mutates state, so it requires a signed request \
                 (x-ducktape-key / -ts / -sig) or this node's operator credential"
            }
            Self::SignatureStale => "the request timestamp is outside this node's freshness window",
            Self::SignatureMalformed => "the request key or signature is not a valid ed25519 value",
            Self::SignatureInvalid => {
                "the signature does not bind this method, path, node, timestamp and body"
            }
            Self::NotOperator => {
                "this route changes the node itself, so it requires this node's operator \
                 credential or a signature by its operator key"
            }
            Self::BodyOverCap => "the request body is larger than this node accepts",
            Self::NodeUnidentified => {
                "this node has no consensus key to bind the signature to, so it cannot \
                 verify any mutating request"
            }
        }
    }
}

/// does this request carry THIS node's operator credential, presented by a
/// loopback peer? the SAME conjunction `/v1/admin/*` requires
/// ([`crate::admin`]), compared the same constant-time way — "can read the
/// node's own workspace", never "can dial the port".
///
/// a node that minted no credential verifies none: it fails closed here and the
/// request falls through to the signature check, exactly as if nothing had been
/// presented.
pub(crate) fn operator_credential_matches(
    cfg: &crate::AdminConfig,
    headers: &HeaderMap,
    on_box: bool,
) -> bool {
    let Some(expected) = cfg.operator_token.as_deref() else {
        return false;
    };
    let Some(offered) = header_str(headers, crate::admin::ADMIN_TOKEN_HEADER) else {
        return false;
    };
    on_box && crate::services::token_matches(offered, expected)
}

/// is this verified acting key the OPERATOR's? the one thing a node knows about
/// who holds it: [`crate::AdminConfig::owner_key`] — the active wallet key read
/// out of this host's keystore at boot (`bin/node`'s `operator_wallet_key`),
/// which is the same key `ducktape node log-filter` signs its request with.
///
/// a node with no wallet on its host carries no such key and admits NOBODY
/// here: the operator credential stays the only way past an
/// [`Authority::Operator`] route, which is the closed direction to fail in.
/// public keys, so an ordinary comparison — there is no secret to leak a
/// timing on.
pub(crate) fn operator_key_matches(cfg: &crate::AdminConfig, acting: &[u8]) -> bool {
    cfg.owner_key.as_deref() == Some(acting)
}

/// the ONE gate over every mutating `/v1` route: decide, then run or refuse.
/// an open (read) route is passed straight through, body untouched.
pub(crate) async fn signed_write_guard(
    State(handle): State<NodeHandle>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let on_box = crate::admin::peer_is_loopback(&req);
    let is_operator = operator_credential_matches(&handle.admin, req.headers(), on_box);
    let lane = lane_of(&path);
    let Some(authority) = lane.authority(&method) else {
        return next.run(req).await;
    };
    // admitted without touching the body: nothing is signed over it on this
    // path, so a blob still streams to its handler instead of buffering in
    // middleware.
    if is_operator {
        return next.run(req).await;
    }
    // the blob lane's body is UNBOUNDED and streams to disk, so this gate
    // cannot hold it to hash it. The proof travels to the handler instead,
    // which hashes the bytes as they land and admits them only once the
    // signature binds that digest — nothing is addressable before it does.
    if matches!(lane, Lane::Blob) {
        let proof = DeferredProof {
            method,
            path_and_query: req
                .uri()
                .path_and_query()
                .map(|pq| pq.as_str().to_string())
                .unwrap_or(path.clone()),
            headers: req.headers().clone(),
        };
        // only the digest binding waits for the body. Everything else about
        // the proof is answerable now, and answering it now is what keeps an
        // unsigned caller from streaming a whole upload onto this node's disk
        // before being told no.
        if let Err(refusal) = proof.precheck(&handle) {
            return refuse(&path, refusal);
        }
        let mut req = req;
        req.extensions_mut().insert(proof);
        return next.run(req).await;
    }
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| path.clone());
    let headers = req.headers().clone();
    let (parts, body) = req.into_parts();
    let body = match axum::body::to_bytes(body, lane.max_body()).await {
        Ok(bytes) => bytes,
        // `to_bytes` collapses "over the cap" and "the stream broke" into one
        // error, and the cap is the likelier of the two by far.
        Err(_) => return refuse(&path, WriteRefusal::BodyOverCap),
    };
    let acting = match verify_signed_request(&handle, &method, &path_and_query, &headers, &body) {
        Ok(key) => key,
        Err(refusal) => return refuse(&path, refusal),
    };
    // possession is the WHOLE proof on an `Acting` lane, because the module
    // downstream reads the key and decides. a node-level handler reads nothing,
    // so the key itself has to be one this node recognises.
    let admitted = match authority {
        Authority::Acting => true,
        Authority::Operator => operator_key_matches(&handle.admin, &acting),
    };
    if !admitted {
        return refuse(&path, WriteRefusal::NotOperator);
    }
    let mut req = axum::extract::Request::from_parts(parts, axum::body::Body::from(body));
    req.extensions_mut().insert(SignedBy(acting));
    next.run(req).await
}

/// the data-plane PoP over ONE request, answering the key that signed it.
///
/// the node key SALTS the signature: a mutation signed for this node cannot
/// be replayed against another node the same key acts on. an embedded
/// daemon with no consensus identity has nothing to salt with — refuse
/// rather than bind the empty salt, which would verify a signature minted
/// for ANY such keyless daemon.
///
/// the guard above runs it on every gated route; the realtime upgrades
/// (`crate::call`) run it on a `GET` with an empty body, because a websocket
/// upgrade is admitted by the handler, not by this middleware.
pub(crate) fn verify_signed_request(
    handle: &NodeHandle,
    method: &Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Vec<u8>, WriteRefusal> {
    let Some(node_key) = handle.admin.node_key.clone().filter(|k| !k.is_empty()) else {
        return Err(WriteRefusal::NodeUnidentified);
    };
    let verified = verify_pop(headers, DATA_HEADERS, DATA_REQ_NS, now_secs(), |ts| {
        request_message(method.as_str(), path_and_query, &node_key, ts, body)
    });
    verified.map_err(|error| match error {
        PopError::MissingAuth => WriteRefusal::SignatureMissing,
        PopError::Stale => WriteRefusal::SignatureStale,
        PopError::BadKey => WriteRefusal::SignatureMalformed,
        PopError::BadSig => WriteRefusal::SignatureInvalid,
    })
}

/// the write half: one refusal body, the plane's `reason` token, and nothing
/// about the URI.
///
/// LATCHED at `warn`: a refusal is client-driven and any process that can dial
/// the port can mint one per request, so an unconditional line here would evict
/// the 4096-line ring between two useful ones. first occurrence, then every
/// 50th, carrying `occurrences` — the counter is the diagnosis anyway. keyed by
/// the reason token, which comes from a FIXED variant set, so no caller string
/// can vary the key to mint unbounded "first occurrences".
pub(crate) fn refuse(path: &str, refusal: WriteRefusal) -> Response {
    static REFUSED: crate::log::Latch = crate::log::Latch::new(50);
    if let Some(occurrences) = REFUSED.hit(refusal.reason()) {
        match plane_of(path) {
            Plane::Files => tracing::warn!(
                target: "ducktape::files",
                reason = refusal.reason(),
                status = refusal.status().as_u16(),
                occurrences,
                "mutating request refused"
            ),
            Plane::Node => tracing::warn!(
                target: "ducktape::node",
                reason = refusal.reason(),
                status = refusal.status().as_u16(),
                occurrences,
                "mutating request refused"
            ),
        }
    }
    (
        refusal.status(),
        Json(serde_json::json!({
            "error": refusal.message(),
            "reason": refusal.reason(),
        })),
    )
        .into_response()
}

/// the acting identity a gated handler submits under. `None` is unreachable on
/// a gated route (the guard inserts it or refuses), and the fallback is the
/// node's own name rather than a panic — a handler must never 500 because a
/// route table and this table disagreed.
pub(crate) fn acting_origin(signed: Option<&SignedBy>) -> Vec<u8> {
    match signed {
        Some(SignedBy(key)) => key.clone(),
        None => crate::DEFAULT_ORIGIN.as_bytes().to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_cryptography::Signer as _;

    fn key(seed: u64) -> ed25519::PrivateKey {
        ed25519::PrivateKey::from_seed(seed)
    }

    const NODE: [u8; 32] = [0xab; 32];

    fn headers_for(signer: &ed25519::PrivateKey, sig: &ed25519::Signature, ts: u64) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            KEY_HEADER,
            duckfs_core::to_hex(signer.public_key().as_ref())
                .parse()
                .unwrap(),
        );
        h.insert(TS_HEADER, ts.to_string().parse().unwrap());
        h.insert(
            SIG_HEADER,
            duckfs_core::to_hex(sig.as_ref()).parse().unwrap(),
        );
        h
    }

    fn verify(
        headers: &HeaderMap,
        method: &str,
        path: &str,
        node: &[u8],
        body: &[u8],
        now: u64,
    ) -> Result<Vec<u8>, PopError> {
        verify_pop(headers, DATA_HEADERS, DATA_REQ_NS, now, |ts| {
            request_message(method, path, node, ts, body)
        })
    }

    #[test]
    fn the_message_binds_method_path_node_time_and_body() {
        // every field moves the signed bytes; none can bleed into another.
        let base = request_message("POST", "/v1/submit", &NODE, 100, b"body");
        assert_ne!(
            base,
            request_message("PUT", "/v1/submit", &NODE, 100, b"body")
        );
        assert_ne!(
            base,
            request_message("POST", "/v1/query", &NODE, 100, b"body")
        );
        assert_ne!(
            base,
            request_message("POST", "/v1/submit", &[0xcd; 32], 100, b"body")
        );
        assert_ne!(
            base,
            request_message("POST", "/v1/submit", &NODE, 101, b"body")
        );
        assert_ne!(
            base,
            request_message("POST", "/v1/submit", &NODE, 100, b"other")
        );
    }

    #[test]
    fn a_signed_request_verifies_and_a_forged_one_does_not() {
        let caller = key(1);
        let now = 1_000_000;
        let sig = sign_request(&caller, "POST", "/v1/files/blob", &NODE, now, b"chunk");
        let headers = headers_for(&caller, &sig, now);
        assert_eq!(
            verify(&headers, "POST", "/v1/files/blob", &NODE, b"chunk", now),
            Ok(caller.public_key().as_ref().to_vec())
        );
        // the attacker signs, but claims the caller's key.
        let attacker = key(2);
        let forged = sign_request(&attacker, "POST", "/v1/files/blob", &NODE, now, b"chunk");
        let mut bad = headers_for(&caller, &sig, now);
        bad.insert(
            SIG_HEADER,
            duckfs_core::to_hex(forged.as_ref()).parse().unwrap(),
        );
        assert_eq!(
            verify(&bad, "POST", "/v1/files/blob", &NODE, b"chunk", now),
            Err(PopError::BadSig)
        );
    }

    /// THE reason the data plane hashes the body: a signature that authenticates
    /// the caller must not leave the bytes swappable.
    #[test]
    fn a_swapped_body_is_rejected() {
        let caller = key(3);
        let now = 2_000_000;
        let sig = sign_request(
            &caller,
            "POST",
            "/v1/files/blob",
            &NODE,
            now,
            b"the real chunk",
        );
        let headers = headers_for(&caller, &sig, now);
        assert_eq!(
            verify(
                &headers,
                "POST",
                "/v1/files/blob",
                &NODE,
                b"a swapped chunk",
                now
            ),
            Err(PopError::BadSig)
        );
        assert!(
            verify(
                &headers,
                "POST",
                "/v1/files/blob",
                &NODE,
                b"the real chunk",
                now
            )
            .is_ok()
        );
    }

    /// the cross-node replay: captured traffic for node X, aimed at node Y.
    #[test]
    fn a_signature_bound_to_another_node_is_rejected() {
        let caller = key(4);
        let now = 3_000_000;
        let sig = sign_request(&caller, "POST", "/v1/invite", &NODE, now, b"{}");
        let headers = headers_for(&caller, &sig, now);
        assert_eq!(
            verify(&headers, "POST", "/v1/invite", &[0xcd; 32], b"{}", now),
            Err(PopError::BadSig)
        );
        assert!(verify(&headers, "POST", "/v1/invite", &NODE, b"{}", now).is_ok());
    }

    #[test]
    fn a_stale_timestamp_is_rejected_both_directions() {
        let caller = key(5);
        let signed_at = 5_000_000;
        let sig = sign_request(
            &caller,
            "POST",
            "/v1/log-filter",
            &NODE,
            signed_at,
            b"debug",
        );
        let headers = headers_for(&caller, &sig, signed_at);
        let at = |now| verify(&headers, "POST", "/v1/log-filter", &NODE, b"debug", now);
        assert_eq!(at(signed_at + FRESHNESS_SECS + 1), Err(PopError::Stale));
        assert_eq!(at(signed_at - FRESHNESS_SECS - 1), Err(PopError::Stale));
        assert!(at(signed_at + FRESHNESS_SECS).is_ok());
    }

    #[test]
    fn missing_headers_are_missing_auth_not_a_crash() {
        assert_eq!(
            verify(&HeaderMap::new(), "POST", "/v1/invite", &NODE, b"", 1),
            Err(PopError::MissingAuth)
        );
    }

    /// the whole table, in one place: every mutating `/v1` route WITH the
    /// authority it demands, and the reads that must not be dragged in with
    /// them. the pairs that matter most are the ones that share a path with
    /// their own read — `/v1/files/blob` (POST writes, GET fetches) and `/v1/submit` vs
    /// `/v1/submit/frame` — and the DELETE that shares its prefix with two
    /// module-bound POSTs on the workspace lane.
    #[test]
    fn the_gate_covers_every_mutating_route() {
        let gated: &[(Method, &str, Authority)] = &[
            // module-bound: the acting key rides on as `SignedBy` and the
            // module decides.
            (Method::POST, "/v1/fs/workspaces", Authority::Acting),
            (
                Method::POST,
                "/v1/fs/workspaces/abc/commit",
                Authority::Acting,
            ),
            (Method::POST, "/v1/files/blob", Authority::Acting),
            // node-level: the handler reads no identity, so possession of a
            // self-chosen key must not be enough.
            (Method::POST, "/v1/log-filter", Authority::Operator),
            (Method::POST, "/v1/invite", Authority::Operator),
            (Method::POST, "/v1/gateway/operator", Authority::Operator),
            (Method::GET, "/v1/gateway/operator", Authority::Operator),
            // the huddle proof binds the SIGNER, and a remote device has no
            // operator credential to offer: possession, then the handler's
            // own account check.
            (Method::POST, "/v1/huddle/node-proof", Authority::Acting),
            // the frameless op lane: the framed op is re-signed as the NODE,
            // never the caller (#1808), so it takes the same operator-only bar.
            (Method::POST, "/v1/submit", Authority::Operator),
            (
                Method::POST,
                "/v1/submit/raw/new-product",
                Authority::Operator,
            ),
            (Method::DELETE, "/v1/fs/workspaces/abc", Authority::Operator),
        ];
        for (method, path, wanted) in gated {
            assert_eq!(
                required_authority(method, path),
                Some(*wanted),
                "{method} {path} is classified wrong"
            );
        }
        let open: &[(Method, &str)] = &[
            (Method::GET, "/v1/status"),
            (Method::GET, "/v1/release"),
            (Method::GET, "/v1/files/blob/aa"),
            (Method::POST, "/v1/query"),
            (Method::POST, "/v1/index/chat/view"),
            (Method::POST, "/v1/gateway/proxy"),
            // self-authenticating: the frame carries its own signature.
            (Method::POST, "/v1/submit/frame"),
            (Method::POST, "/v1/services/hello"),
            (Method::GET, "/v1/ws"),
        ];
        for (method, path) in open {
            assert_eq!(
                required_authority(method, path),
                None,
                "{method} {path} must stay open"
            );
        }
    }

    /// the operator key is an EQUALITY against what the node was configured
    /// with — not "a well-formed key", which is the bug this layer exists for.
    /// a node that knows no operator key admits nobody by signature.
    #[test]
    fn only_the_configured_operator_key_matches() {
        let operator = key(11);
        let stranger = key(12);
        let cfg = crate::AdminConfig {
            owner_key: Some(operator.public_key().as_ref().to_vec()),
            ..Default::default()
        };
        assert!(operator_key_matches(&cfg, operator.public_key().as_ref()));
        assert!(!operator_key_matches(&cfg, stranger.public_key().as_ref()));
        assert!(!operator_key_matches(&cfg, &[]));

        let no_wallet = crate::AdminConfig::default();
        assert!(!operator_key_matches(
            &no_wallet,
            operator.public_key().as_ref()
        ));
    }

    /// the hashing cap is reached by an UNAUTHENTICATED caller, so no lane may
    /// inherit a wider one than its own route accepts — a shared ceiling would
    /// let anyone make the node hold that much for a small-bodied route.
    ///
    /// The blob lane is absent on purpose: it never buffers, so it has no cap
    /// to compare (asking for one panics, which is the point).
    #[test]
    fn no_lane_buffers_more_than_its_own_route_accepts() {
        let cap = |path: &str| lane_of(path).max_body();
        assert_eq!(cap("/v1/submit"), DEFAULT_JSON_BODY_BYTES);
        assert_eq!(cap("/v1/submit/raw/new-product"), node::MAX_PAYLOAD_BYTES);
        assert_eq!(cap("/v1/fs/workspaces"), DEFAULT_JSON_BODY_BYTES);
        // the gateway-operator lane is the widest that still buffers, and it
        // is wide because its own route is: two json bodies plus a proxy head.
        // Nothing may quietly climb past it.
        let widest = cap("/v1/gateway/operator");
        for path in [
            "/v1/submit",
            "/v1/submit/raw/new-product",
            "/v1/fs/workspaces",
        ] {
            assert!(
                cap(path) <= widest,
                "{path} buffers more than the gateway lane"
            );
        }
    }

    /// the blob lane costs nothing to refuse: an upload with no proof on it is
    /// turned away by the gate, so the handler never opens an ingest and no
    /// unauthenticated byte reaches this node's disk.
    #[tokio::test]
    async fn an_unsigned_blob_upload_is_refused_before_its_body_is_read() {
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::post;
        use tower::ServiceExt as _;

        async fn never_reached() -> StatusCode {
            panic!("the handler must never see an unsigned upload")
        }

        let (mut handle, _cmds, _hub) = crate::NodeHandle::channel();
        handle.admin.node_key = Some(vec![7u8; 32]);
        let app = axum::Router::new()
            .route("/v1/files/blob", post(never_reached))
            .route_layer(axum::middleware::from_fn_with_state(
                handle,
                signed_write_guard,
            ));

        let response = app
            .oneshot(
                Request::post("/v1/files/blob")
                    .body(Body::from(vec![0u8; 1024]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), WriteRefusal::SignatureMissing.status());
    }

    /// the embedded-daemon shape this refusal exists for: no consensus key
    /// means no salt to bind a signature to, so the guard must refuse rather
    /// than fall back to verifying against the empty salt — which would
    /// admit a signature minted for ANY other keyless daemon.
    #[tokio::test]
    async fn a_keyless_daemon_refuses_rather_than_binding_an_empty_salt() {
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::post;
        use tower::ServiceExt as _;

        // `NodeHandle::channel()` carries `AdminConfig::default()`, whose
        // `node_key` is `None` — exactly the embedded-daemon condition.
        let (handle, _cmds, _hub) = crate::NodeHandle::channel();
        let app = axum::Router::new()
            .route("/v1/invite", post(|| async { StatusCode::OK }))
            .route_layer(axum::middleware::from_fn_with_state(
                handle,
                signed_write_guard,
            ));

        let caller = key(9);
        let now = 9_000_000;
        let sig = sign_request(&caller, "POST", "/v1/invite", &[], now, b"{}");
        let mut req = Request::builder()
            .method("POST")
            .uri("/v1/invite")
            .body(Body::from("{}"))
            .unwrap();
        *req.headers_mut() = headers_for(&caller, &sig, now);

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn a_refusal_names_one_stable_reason_per_variant() {
        let all = [
            WriteRefusal::SignatureMissing,
            WriteRefusal::SignatureStale,
            WriteRefusal::SignatureMalformed,
            WriteRefusal::SignatureInvalid,
            WriteRefusal::NotOperator,
            WriteRefusal::BodyOverCap,
            WriteRefusal::NodeUnidentified,
        ];
        let mut reasons: Vec<&str> = all.iter().map(|r| r.reason()).collect();
        reasons.sort_unstable();
        let unique = reasons.len();
        reasons.dedup();
        assert_eq!(reasons.len(), unique, "two refusals share a reason token");
        assert!(all.iter().all(|r| !r.status().is_success()));
    }

    #[test]
    fn hex_roundtrips_and_rejects_malformed() {
        assert_eq!(from_hex("00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(from_hex(""), Some(vec![]));
        assert_eq!(from_hex("abc"), None);
        assert_eq!(from_hex("zz"), None);
    }
}
