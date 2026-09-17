//! Bounded async client for Ducktape's public node RPC surface.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use reqwest::{Response, Url};
/// Re-exported with [`refusal`], which takes one: a caller that reads a `/v1`
/// route itself should not have to pin this client's reqwest to say so.
pub use reqwest::StatusCode;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio_tungstenite::tungstenite::Message;

const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;
/// one frame of a streamed blob upload: what this process holds of a file
/// while it crosses to the node, whatever the file's size.
const BLOB_UPLOAD_FRAME_BYTES: usize = 1024 * 1024;
const MAX_ERROR_BYTES: usize = 4 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
/// The throughput floor a blob's bytes are budgeted at, on both legs they
/// travel: up to the node ([`Client::put_blob_file`]) and out from it to every
/// other validator before the submit's block can apply ([`submit_budget`]).
/// One number, because it is one pack crossing one class of link — a second
/// floor would be two answers to the same question.
const BLOB_FLOOR_BYTES_PER_SEC: u64 = 64 * 1024;
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_millis(7_500);

/// A node response or transport failure safe to show to a client user: a
/// stable snake_case `reason` to branch on and the `message` to show.
///
/// The two are separate fields because they are BOUNDED separately. This client
/// used to build one string — `RPC returned 400 Bad Request: {"error":"…"}` —
/// and then clip the whole thing, so every character of framing was a character
/// cut off the END of the module's own sentence, which is the half that says
/// what to do about it.
#[derive(Debug)]
pub struct Error {
    reason: String,
    message: String,
}

impl Error {
    /// A failure of THIS CLIENT: it could not send the request, read the
    /// response, or make sense of it. Never a refusal the node authored — those
    /// come from [`Error::refusal`], which reads the node's own token.
    fn new(message: impl Into<String>) -> Self {
        Self {
            reason: "rpc_client".into(),
            message: message.into(),
        }
    }

    /// The node's refusal, as the node classified it.
    fn refused(reason: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            message: message.into(),
        }
    }

    /// What kind of failure this is — the token a caller branches on.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// What it says. A refusal's message is the refusing module's own sentence,
    /// verbatim: nothing here paraphrases it and nothing wraps it.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<Error> for String {
    fn from(error: Error) -> Self {
        error.message
    }
}

/// The node's error body on every non-2xx: the sentence in `error`, its token
/// in `reason` (absent from a refusal no actor classified, and from anything
/// that is not this node — a proxy, a gateway).
#[derive(Deserialize)]
struct NodeRefusal {
    error: String,
    reason: Option<String>,
}

/// Read a non-2xx body as the node's refusal. The sentence is bounded on its
/// OWN budget here, which is the whole point: a long module sentence is cut
/// only by its own length, never by a status line in front of it.
///
/// `pub` for the one caller that reaches a `/v1` route without this client —
/// the app kernel's signed admin POST. It parses the node's refusal HERE
/// rather than writing a second parser, so both lanes split the envelope the
/// same way and no consumer downstream has to.
pub fn refusal(status: StatusCode, body: &[u8]) -> Error {
    match serde_json::from_slice::<NodeRefusal>(body) {
        Ok(node) => Error::refused(
            node.reason.unwrap_or_else(|| "refused".into()),
            bounded_detail(&node.error),
        ),
        // not this node's envelope at all: the status IS the classification and
        // the body is the only evidence there is.
        Err(_) => Error::refused(
            "http_error",
            format!(
                "{status}: {}",
                bounded_detail(&String::from_utf8_lossy(body))
            ),
        ),
    }
}

/// Result returned by the RPC client.
pub type Result<T> = std::result::Result<T, Error>;

/// Why [`Client::submit_frame`] answered no height — and, decisively, whether
/// the op is DEAD or merely unaccounted for.
///
/// The submit lane is the one call where those differ. The node holds the
/// response until the block that includes the frame commits, so a client that
/// stops waiting has learned nothing: the op may be committing at that very
/// moment. A caller that reports the two the same way tells its user a push
/// was rejected while the refs are landing, and the retry then collides with
/// the ref the "rejected" push installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitFailure {
    /// The node answered, and its answer was no — a malformed frame, an
    /// unknown module, a module that refused the op. This is a verdict, and
    /// it is the only failure a caller may relay to a user as a refusal.
    Refused(String),
    /// The exchange never completed: the connection failed, the budget ran
    /// out, or the receipt did not parse. The op's fate is UNKNOWN. Report it
    /// as an error, never as a refusal, and re-read before retrying.
    Unresolved(String),
}

impl fmt::Display for SubmitFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(detail) | Self::Unresolved(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for SubmitFailure {}

/// How long to wait for a submit receipt over a frame carrying `blob_bytes`.
///
/// [`TIMEOUT`] is an RPC's budget and a submit is not an RPC: the node answers
/// only once the block that includes the frame commits, and when the frame
/// requires a blob, that block cannot apply until the node has fanned those
/// bytes out to every other validator. So the wait scales with the blob,
/// exactly as [`Client::put_blob_file`]'s upload does, and over the same links
/// — which is why it reuses that floor rather than inventing a second one.
///
/// A flat budget here is a size limit wearing a clock: at the old 30 s, a
/// repository-sized push was reported failed while it was committing, and
/// landed half a minute later. The floor still fails a dead link; it just
/// refuses to call a slow one dead.
///
/// The bar it has to clear is the NODE's own hold for the same frame:
/// `SUBMIT_HOLD` plus `relay::blob_transfer_allowance(bytes, targets)` — the
/// node budgets the fan-out at 1 MiB/s over hex-inflated bytes, once per
/// target (`bin/node/src/relay.rs`). A client that gives up first turns a
/// submit the node is still honestly working on into a reported failure. At
/// 64 KiB/s this clears that hold up to EIGHT fan-out targets, since the node
/// spends `2 * targets` of its floor against this one's 16.
///
/// ponytail: a constant that outlasts a formula it cannot read. The node knows
/// its own deadline exactly — have it state the hold in the submit response
/// (or a first-byte receipt) and wait for that instead, when a network grows
/// past eight validators.
fn submit_budget(blob_bytes: u64) -> Duration {
    if blob_bytes == 0 {
        return TIMEOUT;
    }
    TIMEOUT + Duration::from_secs(blob_bytes.div_ceil(BLOB_FLOOR_BYTES_PER_SEC))
}

/// The response header the index view lane stamps its fold watermark into.
const FOLDED_HEADER: &str = "x-ducktape-folded";

/// How far a module's fold had consumed its op feed when a view snapshot was
/// taken: the `(height, seq)` of the last op ROW folded.
///
/// `seq` is the block-wide dispatch index, so `(height, seq)` — not height
/// alone — is what makes the comparison exact when a block's ops fold in
/// several batches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FoldTip {
    pub height: u64,
    pub seq: u32,
}

impl FoldTip {
    /// The wire form: `"{height}:{seq}"`. `None` for anything else — a
    /// malformed watermark is unknown, never a guessed position.
    fn parse(value: &str) -> Option<Self> {
        let (height, seq) = value.split_once(':')?;
        Some(Self {
            height: height.parse().ok()?,
            seq: seq.parse().ok()?,
        })
    }

    /// Whether the fold has reached block `height` — the question a caller
    /// that just wrote at `height` asks of a view snapshot.
    ///
    /// NECESSARY, and sufficient in every case a client can distinguish:
    ///
    /// - Necessary because the fold consumes op rows in `(height, seq)` order,
    ///   so it cannot hold the caller's row without having reached the block.
    /// - Not a PROOF, because the engine cuts a block's rows at its trigger
    ///   batch (fluent31: 512), so a tip of exactly `(height, s)` can sit
    ///   before the caller's own `seq`. Only `tip.height > height` proves
    ///   coverage outright — and that is unreachable on a quiet module, since
    ///   the fold advances only on op traffic. Demanding it would mean waiting
    ///   out the timeout on every block whose single op is the caller's own,
    ///   which is the ordinary case.
    /// - The gap needs one module to take 512+ dispatches in ONE block, and it
    ///   is a transient drain state (the runner re-invokes immediately for the
    ///   remainder), not a resting one. Nothing reports an op's `seq` to its
    ///   submitter, so a client cannot close it by comparing — callers keep
    ///   their own correction for the residue and treat this as what it is: a
    ///   way to make the correction almost never fire.
    pub fn reached_block(&self, height: u64) -> bool {
        self.height >= height
    }
}

/// A live module event from the node's resumable `/v1/ws` stream.
#[derive(Clone, Debug, PartialEq)]
pub enum ModuleEvent {
    /// The requested topics are active. Cursors are suitable for reconnect.
    Ready { cursors: BTreeMap<String, String> },
    /// One committed operation applied to a module — the full feed row, so a
    /// follower folds the delta instead of refetching a snapshot.
    Changed {
        module: String,
        cursor: String,
        op: Box<StreamOp>,
    },
    /// Replay history was unavailable; hydrate a fresh snapshot at this cursor.
    Lagged { module: String, cursor: String },
    /// One topic will not deliver, and the node named which and why.
    ///
    /// NOT the connection's failure. The node refuses PER TOPIC and keeps
    /// serving the rest; collapsing that into a stream error took chat and
    /// pages down over a module this node never indexed, then reconnected
    /// forever to be refused again. Only a subscribe-time refusal arrives as
    /// this variant — a mid-session drop is transient (a scan error, a poisoned
    /// index) and still tears the stream down so the reconnect can recover it.
    Refused { module: String, code: String },
    /// The chain head, as the node's heartbeat reports it.
    ///
    /// The heartbeat rides EVERY block wake — nop fillers included, which feed
    /// no topic at all (`crates/noded/src/stream.rs`, the `block_rx` arm) — so this
    /// is the only event that moves a follower's head on a chain whose
    /// subscribed modules are quiet. Folding an op is not a substitute: an idle
    /// chain finalizes nop blocks forever and emits no ops, which used to freeze
    /// the console's head until someone typed.
    ///
    /// It carries the height and NOTHING ELSE on purpose. A read of `/v1/status`
    /// triggered by this event would be a poll wearing a consensus costume: an
    /// idle chain nop-fills once per block time (network.toml `block_time_ms`),
    /// so "on every tip" is a timer with extra steps.
    Tip { height: u64 },
}

/// One applied-op feed row as the stream serves it: the block coordinates,
/// the dispatch origin, the op payload verbatim (`payload` when it is JSON,
/// `payload_hex` otherwise), and the module-assigned stamp the module
/// declared while applying it (chat's message seq, an edit's rev) — absent
/// when the op assigned nothing.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct StreamOp {
    pub height: u64,
    /// the block-wide dispatch index.
    pub seq: u32,
    /// the block's consensus time.
    pub time: u64,
    pub origin: StreamOrigin,
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
    #[serde(default)]
    pub payload_hex: Option<String>,
    #[serde(default)]
    pub assigned: Option<serde_json::Value>,
    #[serde(default)]
    pub assigned_hex: Option<String>,
}

/// The dispatch origin of one applied op. External ids arrive rendered as
/// user handles.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct StreamOrigin {
    pub kind: StreamOriginKind,
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StreamOriginKind {
    External,
    Program,
    Module,
    System,
}

/// One connected module-event session. Reconnect policy belongs to the caller.
pub type ModuleEventStream = futures::stream::BoxStream<'static, Result<ModuleEvent>>;

/// Route one server frame for a single snapshot topic: the document, a
/// failure, or `None` for a frame this subscription does not consume.
///
/// ONE SOCKET, ONE TOPIC. `want` is both the frame filter and the key the
/// document sits under, so a frame for the other snapshot topic can never be
/// read at the wrong key.
///
/// ADMITTING NONE IS THE CONNECTION FAILING — the same rule
/// `module_event_stream` states below. A console pointed at a daemon that
/// predates this topic gets `subscribed: {}`; swallowing it wedges the pane in
/// silence, because the node heartbeats every block and the idle timeout never
/// fires.
fn snapshot_from_frame(text: &str, want: &str) -> Option<Result<serde_json::Value>> {
    #[derive(Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum SnapshotFrame {
        Subscribed {
            topics: BTreeMap<String, String>,
        },
        Tail {
            topic: String,
            item: serde_json::Value,
        },
        #[serde(other)]
        Other,
    }
    match serde_json::from_str::<SnapshotFrame>(text).ok()? {
        SnapshotFrame::Subscribed { topics } if topics.is_empty() => {
            Some(Err(Error::new("RPC stream admitted no requested topic")))
        }
        SnapshotFrame::Subscribed { .. } | SnapshotFrame::Other => None,
        // `get`, not `item["…"]`: indexing a `Value` with a &str PANICS on a
        // non-object, and a missing key would otherwise read as an empty
        // sample rather than being ignored.
        SnapshotFrame::Tail { topic, item } if topic == want => Some(Ok(item.get(want)?.clone())),
        SnapshotFrame::Tail { .. } => None,
    }
}

/// The stable portion of `GET /v1/status` needed by public clients.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct Status {
    pub height: u64,
    pub public_key: String,
}

/// An HTTP(S) origin serving Ducktape's `/v1` endpoints.
#[derive(Clone)]
pub struct Client {
    origin: String,
    base: Url,
    http: reqwest::Client,
    /// The node's operator credential, when this caller can read the node's own
    /// workspace. Every MUTATING `/v1` route refuses a caller that presents
    /// neither it nor a per-request user signature, so a client without one
    /// READS — its writes come back as the node's 401 naming the credential.
    operator_token: Option<String>,
    /// The PERSON's proof for a raw-bytes write lane (`/v1/files/blob`): signs each request with the acting key, bound to the
    /// target node, so the node records the person as the writer and charges
    /// the write to them. Preferred over the operator credential when both
    /// are held — a credentialed write is the node's, not the person's.
    write_auth: Option<WriteAuth>,
}

/// Signs one mutating request: `(method, path_and_query, sha256 of the body)`
/// in, the headers that prove possession out. The signing itself lives with
/// the kernel's frame codec (`node::signed_req::request_headers_digest`); this
/// crate carries the hook only, and stays free of node internals.
///
/// The DIGEST rather than the body, because one caller here uploads a file it
/// never holds: a git push's packfile streams from disk, and the only thing a
/// signature can bind is what the bytes hash to.
pub type WriteAuth =
    std::sync::Arc<dyn Fn(&str, &str, &[u8; 32]) -> Vec<(String, String)> + Send + Sync>;

/// The header the operator credential travels in — the same one `/v1/admin/*`
/// takes, because it is the same secret and the same bar ("can read the node's
/// own workspace"). Spelled here rather than taken from `noded`: this crate is
/// the thin public client and depends on no node internals.
pub const OPERATOR_TOKEN_HEADER: &str = "x-ducktape-admin-token";

/// The authenticated read lane's path — ONE spelling, because a request's
/// signature binds the path it was minted for. A second spelling is a
/// signature that verifies against nothing.
pub const QUERY_READER_PATH: &str = "/v1/query/reader";

#[derive(Serialize)]
struct QueryRequest<'a, Q> {
    target: &'a str,
    query: &'a Q,
}

#[derive(Serialize)]
struct SubscribeRequest {
    op: &'static str,
    topics: Vec<String>,
    resume: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamFrame {
    Subscribed {
        topics: BTreeMap<String, String>,
    },
    Event {
        topic: String,
        cursor: String,
        op: Box<StreamOp>,
    },
    Lagged {
        topic: String,
        cursor: String,
    },
    Heartbeat {
        height: u64,
    },
    Error {
        topic: String,
        /// the node's stable snake_case refusal token (`unknown_module`,
        /// `topic_not_admitted`, `unavailable`). It has been on the wire all
        /// along and was thrown away here, leaving only a sentence to branch on.
        #[serde(default)]
        code: String,
        detail: String,
    },
}

impl Client {
    /// Build a client from an HTTP(S) origin without credentials, path, or query.
    pub fn new(origin: &str) -> Result<Self> {
        let mut base = Url::parse(origin).map_err(|_| Error::new("RPC endpoint is not a URL"))?;
        let invalid_origin = !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || !matches!(base.path(), "" | "/");
        if invalid_origin {
            return Err(Error::new(
                "RPC endpoint must be an http(s) origin without credentials or a path",
            ));
        }
        base.set_path("/");
        let origin = base.as_str().trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .map_err(|error| Error::new(format!("could not initialize RPC client: {error}")))?;
        Ok(Self {
            origin,
            base,
            http,
            operator_token: None,
            write_auth: None,
        })
    }

    /// Carry `token` — the node's `admin.token`, read out of its workspace — on
    /// every request, which is what makes this client's WRITES land.
    pub fn with_operator_token(mut self, token: String) -> Self {
        self.operator_token = Some(token);
        self
    }

    /// Sign every raw-bytes write with the person's key — see [`WriteAuth`].
    pub fn with_write_auth(mut self, auth: WriteAuth) -> Self {
        self.write_auth = Some(auth);
        self
    }

    /// Attach the operator credential when this client holds one. Harmless on a
    /// read (the gate never looks) and required on every write.
    fn credentialed(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.operator_token.as_deref() {
            Some(token) => request.header(OPERATOR_TOKEN_HEADER, token),
            None => request,
        }
    }

    /// Prove a raw-bytes write: the person's signature over exactly these
    /// bytes when a signer is held, else the operator credential.
    fn proven(
        &self,
        request: reqwest::RequestBuilder,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> reqwest::RequestBuilder {
        use sha2::Digest as _;
        self.proven_digest(request, method, path, &sha2::Sha256::digest(body).into())
    }

    /// the same proof for a body this process never holds — a streamed upload
    /// hashes as it sends and signs that.
    fn proven_digest(
        &self,
        request: reqwest::RequestBuilder,
        method: &str,
        path: &str,
        digest: &[u8; 32],
    ) -> reqwest::RequestBuilder {
        let Some(sign) = self.write_auth.as_ref() else {
            return self.credentialed(request);
        };
        sign(method, path, digest)
            .into_iter()
            .fold(request, |request, (name, value)| {
                request.header(name, value)
            })
    }

    /// Canonical origin without a trailing slash.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Read the node's current committed height.
    pub async fn status(&self) -> Result<Status> {
        let response = self
            .http
            .get(self.url("v1/status")?)
            .send()
            .await
            .map_err(|error| Error::new(format!("RPC status failed: {error}")))?;
        decode_json(response).await
    }

    /// The whole `GET /v1/status` document, verbatim. [`Status`] is the stable
    /// two-field contract every client can rely on; the operational projection
    /// beside it (`root_hash`, `operations.consensus`, `operations.storage`) is
    /// node-owned and evolves with the daemon, so it is served as JSON rather
    /// than frozen into a type here.
    pub async fn status_json(&self) -> Result<serde_json::Value> {
        let response = self
            .http
            .get(self.url("v1/status")?)
            .send()
            .await
            .map_err(|error| Error::new(format!("RPC status failed: {error}")))?;
        decode_json(response).await
    }

    /// Submit one typed module query and decode its typed reply.
    pub async fn query<Q: Serialize, R: DeserializeOwned>(
        &self,
        target: &str,
        query: &Q,
    ) -> Result<R> {
        let response = self
            .http
            .post(self.url("v1/query")?)
            .json(&QueryRequest { target, query })
            .send()
            .await
            .map_err(|error| Error::new(format!("{target} query failed: {error}")))?;
        decode_json(response).await
    }

    /// Submit one typed module query AS THE HOLDER OF THIS CLIENT'S SIGNING
    /// KEY (`POST /v1/query/reader`) and decode its typed reply.
    ///
    /// [`Client::query`]'s authenticated sibling. The open lane proves nothing
    /// about who is asking, so a module answering it sees the system origin and
    /// must refuse protected content; this lane carries the signer's verified
    /// key to the module as its origin, which is what lets a mailbox or a
    /// conversation page be served at all.
    ///
    /// The signer is REQUIRED, not optional: the operator credential is the
    /// node's own and would ask as somebody else. Without a [`WriteAuth`] this
    /// refuses rather than falling back — an anonymous read of protected state
    /// is a refusal wearing an empty answer's clothes.
    pub async fn query_as_reader<Q: Serialize, R: DeserializeOwned>(
        &self,
        target: &str,
        query: &Q,
    ) -> Result<R> {
        let sign = self.write_auth.as_ref().ok_or_else(|| {
            Error::new("an authenticated read needs this device's key; unlock it and try again")
        })?;
        // serialized ONCE: the bytes that are signed are the bytes that are
        // sent. Re-serializing for the wire would let map ordering or float
        // formatting differ from what the signature covered, and the node
        // would reject a read this caller did in fact authorize.
        let body = serde_json::to_vec(&QueryRequest { target, query })
            .map_err(|error| Error::new(format!("{target} query did not encode: {error}")))?;
        let request = self
            .http
            .post(self.url(QUERY_READER_PATH.trim_start_matches('/'))?)
            .header("content-type", "application/json")
            .body(body.clone());
        let digest: [u8; 32] = {
            use sha2::Digest as _;
            sha2::Sha256::digest(&body).into()
        };
        let response = sign("POST", QUERY_READER_PATH, &digest)
            .into_iter()
            .fold(request, |request, (name, value)| request.header(name, value))
            .send()
            .await
            .map_err(|error| Error::new(format!("{target} authenticated query failed: {error}")))?;
        decode_json(response).await
    }

    /// Submit one typed index-tier view request and decode its typed reply —
    /// the read model behind every human-facing list, page, and search.
    ///
    /// Drops the reply's fold watermark. Use [`Client::view_folded`] when the
    /// caller just wrote and needs to know whether this snapshot already
    /// contains its own op; every other read genuinely does not care.
    pub async fn view<Q: Serialize, R: DeserializeOwned>(
        &self,
        module: &str,
        query: &Q,
    ) -> Result<R> {
        Ok(self.view_folded(module, query).await?.0)
    }

    /// [`Client::view`] keeping the reply's fold watermark: how far the
    /// module's fold had consumed the op feed when this snapshot was taken.
    ///
    /// It answers exactly one question — "has the fold reached MY op at
    /// `(H, seq)`", i.e. read-after-your-own-write. It is NOT general
    /// freshness: the fold only advances on op traffic, so a quiet module's
    /// tip is arbitrarily old while its view is perfectly current. `None` is
    /// UNKNOWN (a module that ships no index guest, a fresh database, a
    /// boundary stamp that wiped the tip) — never height 0, and never a reason
    /// to keep waiting.
    pub async fn view_folded<Q: Serialize, R: DeserializeOwned>(
        &self,
        module: &str,
        query: &Q,
    ) -> Result<(R, Option<FoldTip>)> {
        let response = self
            .http
            .post(self.url(&format!("v1/index/{module}/view"))?)
            .json(query)
            .send()
            .await
            .map_err(|error| Error::new(format!("{module} view failed: {error}")))?;
        let folded = response
            .headers()
            .get(FOLDED_HEADER)
            .and_then(|value| value.to_str().ok())
            .and_then(FoldTip::parse);
        Ok((decode_json(response).await?, folded))
    }

    /// Read the recent block rows (`GET /v1/blocks`), oldest-first — the
    /// explorer surface. Rows are the node's own JSON projection, and they are
    /// NOT all op-carrying: a node following from a checkpoint records its
    /// ascension boundary as a row with an empty `hash` and no `ops`. Filter
    /// for what you present (the app's `explorer_window` does).
    pub async fn blocks(&self, limit: usize) -> Result<Vec<serde_json::Value>> {
        let mut url = self.url("v1/blocks")?;
        url.set_query(Some(&format!("limit={limit}")));
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|error| Error::new(format!("RPC blocks failed: {error}")))?;
        #[derive(Deserialize)]
        struct Blocks {
            blocks: Vec<serde_json::Value>,
        }
        let reply: Blocks = decode_json(response).await?;
        Ok(reply.blocks)
    }

    /// Land raw bytes in the node-local BLOB store (`POST /v1/files/blob`) —
    /// the op-receipt lane forge fetches `PushRefs`/`MergePr` packfiles from by
    /// digest. A distinct plane from module-owned Files chunk staging:
    /// a pack staged there would never be found by a `pack_digest` lookup.
    pub async fn put_blob(&self, bytes: Vec<u8>) -> Result<String> {
        let response = self
            .proven(
                self.http.post(self.url("v1/files/blob")?),
                "POST",
                "/v1/files/blob",
                &bytes,
            )
            .header("content-type", "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .map_err(|error| Error::new(format!("RPC blob put failed: {error}")))?;
        #[derive(Deserialize)]
        struct Stored {
            digest: String,
        }
        let reply: Stored = decode_json(response).await?;
        Ok(reply.digest)
    }

    /// Land a FILE's bytes — from `offset` to its end — in the node-local blob
    /// store, streaming.
    ///
    /// This is the door a git push takes, so it has no size: the packfile was
    /// spooled to disk by whoever received it, one frame of it at a time
    /// crosses to the node, and the node writes it straight back to disk. The
    /// file is hashed first because the signature binds the digest — two
    /// sequential reads of a file, and never the file in memory.
    pub async fn put_blob_file(&self, path: &std::path::Path, offset: u64) -> Result<String> {
        use sha2::Digest as _;
        use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

        let open = |offset: u64| async move {
            let mut file = tokio::fs::File::open(path)
                .await
                .map_err(|error| Error::new(format!("RPC blob put failed: {error}")))?;
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|error| Error::new(format!("RPC blob put failed: {error}")))?;
            Ok::<tokio::fs::File, Error>(file)
        };

        let mut hashing = open(offset).await?;
        let mut hasher = sha2::Sha256::new();
        let mut total = 0u64;
        let mut buf = vec![0u8; BLOB_UPLOAD_FRAME_BYTES];
        loop {
            let read = hashing
                .read(&mut buf)
                .await
                .map_err(|error| Error::new(format!("RPC blob put failed: {error}")))?;
            if read == 0 {
                break;
            }
            hasher.update(&buf[..read]);
            total += read as u64;
        }
        let digest: [u8; 32] = hasher.finalize().into();

        let sending = open(offset).await?;
        let body = futures::stream::unfold(sending, |mut file| async move {
            let mut buf = vec![0u8; BLOB_UPLOAD_FRAME_BYTES];
            match file.read(&mut buf).await {
                Ok(0) => None,
                Ok(read) => {
                    buf.truncate(read);
                    Some((Ok::<Vec<u8>, std::io::Error>(buf), file))
                }
                Err(error) => Some((Err(error), file)),
            }
        });

        // this client's flat [`TIMEOUT`] is an RPC's budget, and a push is not
        // an RPC: its duration scales with the history it carries. The budget
        // here is a floor on THROUGHPUT — [`BLOB_FLOOR_BYTES_PER_SEC`] over the
        // bytes, on top of a five-minute base — so a big push is allowed to
        // take long and a dead link still fails. The submit that follows this
        // upload is budgeted off the same floor ([`submit_budget`]).
        let budget = Duration::from_secs(300 + total.div_ceil(BLOB_FLOOR_BYTES_PER_SEC));
        let response = self
            .proven_digest(
                self.http.post(self.url("v1/files/blob")?),
                "POST",
                "/v1/files/blob",
                &digest,
            )
            .header("content-type", "application/octet-stream")
            .header("content-length", total)
            .timeout(budget)
            .body(reqwest::Body::wrap_stream(body))
            .send()
            .await
            .map_err(|error| Error::new(format!("RPC blob put failed: {error}")))?;
        #[derive(Deserialize)]
        struct Stored {
            digest: String,
        }
        let reply: Stored = decode_json(response).await?;
        let expected: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        if reply.digest != expected {
            return Err(Error::new("blob receipt digest mismatch"));
        }
        Ok(reply.digest)
    }

    /// Fetch a blob by digest, bounding both declared and streamed response bytes.
    /// The caller verifies the returned bytes against the expected digest.
    pub async fn get_blob(&self, digest: &[u8; 32], limit: usize) -> Result<Vec<u8>> {
        use std::fmt::Write as _;

        let mut path = String::from("v1/files/blob/");
        for byte in digest {
            write!(&mut path, "{byte:02x}").expect("writing to a string succeeds");
        }
        let response = self
            .http
            .get(self.url(&path)?)
            .send()
            .await
            .map_err(|error| Error::new(format!("RPC blob get failed: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::new(format!("RPC blob get returned {status}")));
        }
        read_bounded(response, limit).await
    }

    /// Read the peers standing (`GET /v1/peers`), the node's own JSON view.
    pub async fn peers(&self) -> Result<serde_json::Value> {
        let response = self
            .http
            .get(self.url("v1/peers")?)
            .send()
            .await
            .map_err(|error| Error::new(format!("RPC peers failed: {error}")))?;
        decode_json(response).await
    }

    /// Subscribe ONE snapshot topic on its own socket.
    ///
    /// One topic per socket is what lets the console hold `status` on every tab
    /// while `peers` lives only on the tab that draws it: the subscription IS
    /// the node's sampling budget, and `peers` composes its document by
    /// encoding the whole metrics registry where `status` is a cell read.
    ///
    /// The document arrives as raw JSON on purpose. `/v1/peers` and
    /// `/v1/status` are already parsed field-by-field by the console, and a
    /// typed mirror here would be a SECOND reader of the same wire to drift
    /// from the first.
    async fn snapshot_events(
        &self,
        topic: &'static str,
    ) -> Result<futures::stream::BoxStream<'static, Result<serde_json::Value>>> {
        let subscribe = serde_json::to_string(&SubscribeRequest {
            op: "subscribe",
            topics: vec![topic.to_string()],
            resume: BTreeMap::new(),
        })
        .map_err(|error| Error::new(format!("could not encode snapshot subscription: {error}")))?;
        let url = self.stream_url()?;
        let (mut socket, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(&url))
            .await
            .map_err(|_| Error::new("RPC stream connection timed out"))?
            .map_err(|error| Error::new(format!("RPC stream connection failed: {error}")))?;
        tokio::time::timeout(TIMEOUT, socket.send(Message::Text(subscribe)))
            .await
            .map_err(|_| Error::new("RPC stream subscription timed out"))?
            .map_err(|error| Error::new(format!("RPC stream subscription failed: {error}")))?;
        let stream = futures::stream::unfold(Some(socket), move |socket| async move {
            let mut socket = socket?;
            loop {
                let message = match tokio::time::timeout(STREAM_IDLE_TIMEOUT, socket.next()).await {
                    Ok(Some(message)) => message,
                    Ok(None) => return Some((Err(Error::new("RPC stream closed")), None)),
                    Err(_) => {
                        return Some((Err(Error::new("RPC stream heartbeat timed out")), None));
                    }
                };
                let Ok(Message::Text(text)) = message else {
                    if matches!(message, Ok(Message::Close(_)) | Err(_)) {
                        return Some((Err(Error::new("RPC stream closed")), None));
                    }
                    continue;
                };
                match snapshot_from_frame(&text, topic) {
                    Some(Ok(document)) => return Some((Ok(document), Some(socket))),
                    // a refused subscription ends the stream so the caller's
                    // backoff can rebuild it, exactly as a dropped socket does.
                    Some(Err(error)) => return Some((Err(error), None)),
                    None => continue,
                }
            }
        })
        .boxed();
        Ok(stream)
    }

    /// The node's own status projection, pushed. Cheap enough to hold open
    /// wherever the console is standing: the node answers it from a cell it
    /// publishes at each boundary.
    pub async fn status_events(
        &self,
    ) -> Result<futures::stream::BoxStream<'static, Result<serde_json::Value>>> {
        self.snapshot_events("status").await
    }

    /// Submit an already-signed operation frame, answering the height of the
    /// block that INCLUDED it.
    ///
    /// The node has always returned that height in its submit receipt and this
    /// client always threw it away, which is why every caller that needed to
    /// read its own write had to guess. Acceptance is not application: the
    /// derived read models fold behind the block loop, so the height is the
    /// coordinate a follow-up read waits on ([`Client::view_folded`]).
    ///
    /// `carried_blob_bytes` is the size of the blob this frame REQUIRES, or 0
    /// for an ordinary op — see [`submit_budget`] for why the wait scales with
    /// it. The caller uploaded those bytes ([`Client::put_blob_file`]), so the
    /// caller is the one that knows.
    pub async fn submit_frame(
        &self,
        frame: Vec<u8>,
        carried_blob_bytes: u64,
    ) -> std::result::Result<u64, SubmitFailure> {
        let response = self
            .http
            .post(
                self.url("v1/submit/frame")
                    .map_err(|error| SubmitFailure::Unresolved(error.to_string()))?,
            )
            .header("content-type", "application/octet-stream")
            .timeout(submit_budget(carried_blob_bytes))
            .body(frame)
            .send()
            .await
            .map_err(|error| {
                SubmitFailure::Unresolved(format!("transaction submission failed: {error}"))
            })?;
        if !response.status().is_success() {
            return Err(SubmitFailure::Refused(
                response_error(response).await.to_string(),
            ));
        }
        #[derive(Deserialize)]
        struct Receipt {
            height: u64,
        }
        let receipt: Receipt = decode_json(response)
            .await
            .map_err(|error| SubmitFailure::Unresolved(error.to_string()))?;
        Ok(receipt.height)
    }

    /// Submit one module op over the FRAMELESS lane, answering the height of
    /// the block that included it.
    ///
    /// The distinction from [`Client::submit_frame`] is authorship, and it is
    /// load-bearing: this node re-signs the op with ITS OWN key, so the
    /// committed origin is the node rather than the person at the keyboard.
    /// That is right for an op the NODE is the actor of — a run pinned to it,
    /// whose saga id is namespaced under its own actor string — and wrong for
    /// anything a user authors, which must carry the user's signature.
    pub async fn submit(&self, target: &str, payload: serde_json::Value) -> Result<u64> {
        let response = self
            .credentialed(self.http.post(self.url("v1/submit")?))
            .json(&serde_json::json!({ "target": target, "payload": payload }))
            .send()
            .await
            .map_err(|error| Error::new(format!("transaction submission failed: {error}")))?;
        if !response.status().is_success() {
            return Err(response_error(response).await);
        }
        #[derive(Deserialize)]
        struct Receipt {
            height: u64,
        }
        let receipt: Receipt = decode_json(response).await?;
        Ok(receipt.height)
    }

    /// Mint one bearer invite valid for `ttl_days` and answer the paste blob.
    ///
    /// The NODE mints it, not the caller: minting folds this member's dial hint
    /// into the network descriptor and SAVES it, and reads the persisted mesh
    /// state for the member fronts a joiner can bring its tunnel up against —
    /// both files the running daemon owns. A daemon with no workspace (an
    /// embedder that wired no minter) answers 503.
    pub async fn mint_invite(&self, ttl_days: u64) -> Result<String> {
        let response = self
            .credentialed(self.http.post(self.url("v1/invite")?))
            .json(&serde_json::json!({ "ttl_days": ttl_days }))
            .send()
            .await
            .map_err(|error| Error::new(format!("minting an invite failed: {error}")))?;
        if !response.status().is_success() {
            return Err(response_error(response).await);
        }
        #[derive(Deserialize)]
        struct Minted {
            invite: String,
        }
        let minted: Minted = decode_json(response).await?;
        Ok(minted.invite)
    }

    /// Mint this node's `node_proof` for a `JoinHuddle`: its own mesh-identity
    /// key signing `channel_id` ‖ the key that SIGNED this request. The NODE
    /// mints it, not the caller — proof of possession needs the private key
    /// this node holds, never sent over the wire — and it binds the signer,
    /// so this client must carry a [`WriteAuth`]: the person joining is
    /// whoever signs, and the node answers only a key that holds an account.
    /// Answers `(node, node_proof)`, both hex. 503 on a daemon with no mesh
    /// identity.
    pub async fn huddle_node_proof(&self, channel_id: &str) -> Result<(String, String)> {
        let body = serde_json::to_vec(&serde_json::json!({ "channel_id": channel_id }))
            .expect("a json literal serializes");
        let response = self
            .proven(
                self.http.post(self.url("v1/huddle/node-proof")?),
                "POST",
                "/v1/huddle/node-proof",
                &body,
            )
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|error| Error::new(format!("minting a huddle node proof failed: {error}")))?;
        if !response.status().is_success() {
            return Err(response_error(response).await);
        }
        #[derive(Deserialize)]
        struct Minted {
            node: String,
            node_proof: String,
        }
        let minted: Minted = decode_json(response).await?;
        Ok((minted.node, minted.node_proof))
    }

    /// Connect to the node stream and subscribe to committed module changes.
    pub async fn module_events(
        &self,
        modules: Vec<String>,
        resume: BTreeMap<String, String>,
    ) -> Result<ModuleEventStream> {
        if modules.is_empty() {
            return Err(Error::new("at least one module stream is required"));
        }
        let topics = modules
            .into_iter()
            .map(|module| format!("module:{module}"))
            .collect::<Vec<_>>();
        let expected = topics.iter().cloned().collect::<BTreeSet<_>>();
        let subscribe = serde_json::to_string(&SubscribeRequest {
            op: "subscribe",
            topics,
            resume,
        })
        .map_err(|error| Error::new(format!("could not encode stream subscription: {error}")))?;
        let url = self.stream_url()?;
        let (mut socket, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(&url))
            .await
            .map_err(|_| Error::new("RPC stream connection timed out"))?
            .map_err(|error| Error::new(format!("RPC stream connection failed: {error}")))?;
        tokio::time::timeout(TIMEOUT, socket.send(Message::Text(subscribe)))
            .await
            .map_err(|_| Error::new("RPC stream subscription timed out"))?
            .map_err(|error| Error::new(format!("RPC stream subscription failed: {error}")))?;
        Ok(module_event_stream(socket, expected, STREAM_IDLE_TIMEOUT))
    }

    fn url(&self, path: &str) -> Result<Url> {
        self.base
            .join(path)
            .map_err(|_| Error::new("could not build RPC URL"))
    }

    fn stream_url(&self) -> Result<String> {
        let mut url = self.url("v1/ws")?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|_| Error::new("could not build RPC stream URL"))?;
        Ok(url.to_string())
    }
}

fn module_event_stream<S>(
    socket: S,
    expected: BTreeSet<String>,
    idle_timeout: Duration,
) -> ModuleEventStream
where
    S: futures::Stream<Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Send
        + Unpin
        + 'static,
{
    // `subscribed` is what separates a topic that never started from one that
    // stopped: the node refuses at subscribe time and reports later trouble the
    // same way, and only the clock tells them apart.
    futures::stream::unfold(Some((socket, expected, false)), move |state| async move {
        let (mut socket, expected, mut subscribed) = state?;
        loop {
            let message = match tokio::time::timeout(idle_timeout, socket.next()).await {
                Ok(Some(message)) => message,
                Ok(None) => return Some((Err(Error::new("RPC stream closed")), None)),
                Err(_) => {
                    return Some((Err(Error::new("RPC stream heartbeat timed out")), None));
                }
            };
            if let Some(event) = decode_stream_message(message, &expected, subscribed) {
                subscribed = subscribed || matches!(event, Ok(ModuleEvent::Ready { .. }));
                return Some((event, Some((socket, expected, subscribed))));
            }
        }
    })
    .boxed()
}

fn decode_stream_message(
    message: std::result::Result<Message, tokio_tungstenite::tungstenite::Error>,
    expected: &BTreeSet<String>,
    subscribed: bool,
) -> Option<Result<ModuleEvent>> {
    let message = match message {
        Ok(message) => message,
        Err(error) => return Some(Err(Error::new(format!("RPC stream failed: {error}")))),
    };
    let Message::Text(text) = message else {
        return match message {
            Message::Close(_) => Some(Err(Error::new("RPC stream closed"))),
            Message::Binary(_) => Some(Err(Error::new("RPC stream returned binary data"))),
            _ => None,
        };
    };
    if text.len() > MAX_JSON_BYTES {
        return Some(Err(Error::new("RPC stream frame exceeds the client limit")));
    }
    let frame = match serde_json::from_str::<StreamFrame>(&text) {
        Ok(frame) => frame,
        Err(error) => {
            return Some(Err(Error::new(format!(
                "RPC stream returned invalid JSON: {error}"
            ))));
        }
    };
    match frame {
        StreamFrame::Subscribed { topics } => {
            // ADMITTING NONE IS THE CONNECTION FAILING; admitting some is one
            // plane degrading. Anything refused already arrived as its own
            // `Error` frame ahead of this one, so the caller has been told
            // which and why by the time this lands.
            if topics.is_empty() {
                return Some(Err(Error::new("RPC stream admitted no requested topic")));
            }
            Some(Ok(ModuleEvent::Ready { cursors: topics }))
        }
        StreamFrame::Event { topic, cursor, op } => Some(
            module_name(&topic, expected).map(|module| ModuleEvent::Changed { module, cursor, op }),
        ),
        StreamFrame::Lagged { topic, cursor } => {
            Some(module_name(&topic, expected).map(|module| ModuleEvent::Lagged { module, cursor }))
        }
        // BEFORE `subscribed`, this topic never started and the others did:
        // degrade that plane and keep the connection. AFTER, it is a scan error
        // or a poisoned index — transient, and tearing down is what lets the
        // reconnect recover it. Same frame, two meanings, told apart by when.
        StreamFrame::Error {
            topic,
            code,
            detail,
        } => match subscribed {
            false => Some(
                module_name(&topic, expected).map(|module| ModuleEvent::Refused { module, code }),
            ),
            true => Some(Err(Error::new(format!(
                "RPC stream topic {topic} failed: {detail}"
            )))),
        },
        StreamFrame::Heartbeat { height } => Some(Ok(ModuleEvent::Tip { height })),
    }
}

fn module_name(topic: &str, expected: &BTreeSet<String>) -> Result<String> {
    if !expected.contains(topic) {
        return Err(Error::new("RPC stream returned an unexpected topic"));
    }
    topic
        .strip_prefix("module:")
        .map(str::to_string)
        .ok_or_else(|| Error::new("RPC stream returned an invalid module topic"))
}

async fn decode_json<T: DeserializeOwned>(response: Response) -> Result<T> {
    let status = response.status();
    let bytes = read_bounded(response, MAX_JSON_BYTES).await?;
    if !status.is_success() {
        return Err(refusal(status, &bytes));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| Error::new(format!("RPC returned invalid JSON: {error}")))
}

async fn response_error(response: Response) -> Error {
    let status = response.status();
    match read_bounded(response, MAX_ERROR_BYTES).await {
        Ok(bytes) => refusal(status, &bytes),
        // the body itself could not be read: that is this client's failure, not
        // a refusal anyone authored.
        Err(error) => Error::new(format!("a rejection ({status}) could not be read: {error}")),
    }
}

async fn read_bounded(response: Response, limit: usize) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(Error::new("RPC response exceeds the client limit"));
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|error| Error::new(format!("could not read RPC response: {error}")))?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(Error::new("RPC response exceeds the client limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn bounded_detail(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return "no detail".into();
    }
    value.chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE WATERMARK IS A POSITION OR IT IS NOTHING. A header this client
    /// cannot read is UNKNOWN — parsing half of it, or defaulting the missing
    /// half to zero, would hand a caller a tip that claims coverage the fold
    /// never reported. (The wire itself — header on, receipt off — is driven
    /// end to end by the app's stub-node tests and noded's daemon e2e.)
    #[test]
    fn a_fold_tip_parses_only_a_complete_position() {
        assert_eq!(FoldTip::parse("12:3"), Some(FoldTip { height: 12, seq: 3 }));
        for malformed in ["", "12", "12:", ":3", "12:3:4", "twelve:3", "12:-1"] {
            assert_eq!(FoldTip::parse(malformed), None, "{malformed:?} is unknown");
        }
    }

    /// REACHING THE BLOCK IS THE ANSWERABLE QUESTION. A tip BELOW the caller's
    /// block cannot hold its op — the fold consumes rows in (height, seq)
    /// order — so that half is a proof. A tip AT the block is where the honest
    /// limit sits: it is the ordinary case (one op, one block, nothing after
    /// it to push the tip further), so demanding strictly-past would time out
    /// every single time and make the whole watermark useless.
    #[test]
    fn a_tip_reaches_a_block_at_it_not_only_past_it() {
        let tip = FoldTip { height: 7, seq: 9 };
        assert!(tip.reached_block(6));
        assert!(tip.reached_block(7), "at the block is reaching it");
        assert!(!tip.reached_block(8), "a block the fold has not seen");
    }

    /// A NODE THAT ADMITS THE TOPIC MUST FAIL THE STREAM, NOT GO QUIET.
    ///
    /// These topics are newer than some daemons, so a console pointed at an
    /// older one gets `subscribed: {}`. If that is swallowed the socket stays
    /// open forever — the node heartbeats every block and every 3 s, so the
    /// idle timeout never fires — and the pane holds its connect-time values
    /// looking exactly like a quiet node.
    #[test]
    fn an_empty_subscribe_ack_fails_the_snapshot_stream() {
        let refused = snapshot_from_frame(r#"{"type":"subscribed","topics":{}}"#, "peers");
        assert!(
            matches!(refused, Some(Err(_))),
            "admitting no topic is the connection failing"
        );

        let admitted =
            snapshot_from_frame(r#"{"type":"subscribed","topics":{"peers":"0"}}"#, "peers");
        assert!(
            admitted.is_none(),
            "an admitted subscribe keeps the stream: {admitted:?}"
        );
    }

    /// ONE SOCKET, ONE TOPIC. A frame for a topic this subscription did not
    /// ask for is ignored rather than read at the wrong key — the `files:watch`
    /// failure is a topic that subscribes cleanly and delivers nothing usable.
    #[test]
    fn snapshot_frames_route_by_topic_and_survive_a_malformed_item() {
        let peers = snapshot_from_frame(
            r#"{"type":"tail","topic":"peers","cursor":"1","item":{"time_ms":1,"peers":{"peers":[]}}}"#,
            "peers",
        );
        assert!(matches!(peers, Some(Ok(_))), "{peers:?}");

        let status = snapshot_from_frame(
            r#"{"type":"tail","topic":"status","cursor":"1","item":{"time_ms":1,"status":{"version":"0.1.0"}}}"#,
            "status",
        );
        assert!(matches!(status, Some(Ok(_))), "{status:?}");

        // A FRAME FOR THE OTHER TOPIC IS REFUSED BY ITS TOPIC, not by luck.
        // This document carries a `peers` key, so the key lookup alone would
        // hand it over; only the topic test refuses it. Two snapshot documents
        // that happened to share a field name would otherwise be routed into
        // each other.
        assert!(
            snapshot_from_frame(
                r#"{"type":"tail","topic":"status","item":{"peers":{"peers":[]}}}"#,
                "peers"
            )
            .is_none(),
            "routing is by topic, never by which key the payload happens to have"
        );

        // A MISSING KEY IS NOT AN EMPTY SAMPLE. Reading it as one would blank
        // the surface every tick with no error anywhere.
        assert!(
            snapshot_from_frame(
                r#"{"type":"tail","topic":"peers","item":{"time_ms":1}}"#,
                "peers"
            )
            .is_none()
        );

        // and a non-object `item` must not panic the subscription task —
        // `Value` string-indexing does exactly that.
        assert!(
            snapshot_from_frame(r#"{"type":"tail","topic":"peers","item":42}"#, "peers").is_none()
        );

        // heartbeats and per-topic refusals are simply not ours.
        assert!(snapshot_from_frame(r#"{"type":"heartbeat","height":9}"#, "peers").is_none());
        assert!(
            snapshot_from_frame(
                r#"{"type":"error","topic":"peers","code":"unavailable"}"#,
                "peers"
            )
            .is_none()
        );
    }

    #[test]
    fn accepts_only_bare_http_origins() {
        let client = Client::new("http://127.0.0.1:8844/").unwrap();
        assert_eq!(client.origin(), "http://127.0.0.1:8844");

        for invalid in [
            "not-a-url",
            "ftp://127.0.0.1",
            "http://user@127.0.0.1",
            "http://127.0.0.1/path",
            "http://127.0.0.1?query",
            "http://127.0.0.1#fragment",
        ] {
            assert!(Client::new(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[tokio::test]
    async fn rejects_view_path_injection_before_transport() {
        let client = Client::new("http://127.0.0.1:1").unwrap();
        assert!(
            client
                .view::<_, serde_json::Value>("../status", &())
                .await
                .is_err()
        );
    }

    /// THE HEARTBEAT CARRIES THE HEAD, AND IT USED TO BE DROPPED HERE.
    ///
    /// `StreamFrame::Heartbeat` was a UNIT variant, so `height` never survived
    /// deserialization and the frame decoded to nothing. A follower's head then
    /// moved only when one of its subscribed modules committed an op — and an
    /// idle chain nop-fills forever without feeding any topic, so the console
    /// sat on a frozen block number until someone typed.
    ///
    /// The frame below is the node's own, verbatim: `bin/noded/tests/daemon_e2e.rs`
    /// pins that it serves `type`/`height`/`interval_ms`, so this test fails if
    /// either side moves. `root_hash`, `time_ms` and `interval_ms` are not
    /// decoded — the tip's whole contract is the height.
    #[test]
    fn a_heartbeat_decodes_to_the_head_it_carries() {
        let expected = BTreeSet::from(["module:chat".to_string()]);
        let heartbeat = r#"{"type":"heartbeat","height":41,"root_hash":"ab","time_ms":1752000000000,"interval_ms":3000}"#;
        let tip = decode_stream_message(Ok(Message::Text(heartbeat.into())), &expected, true)
            .expect("a heartbeat is an event, not a skipped frame")
            .expect("a heartbeat is not an error");
        assert_eq!(tip, ModuleEvent::Tip { height: 41 });
    }

    /// ONE REFUSED TOPIC MUST NOT TAKE THE OTHERS DOWN.
    ///
    /// The node answers PER TOPIC — a refusal is its own `Error` frame ahead of
    /// `Subscribed`, and the admitted topics still carry cursors. This required
    /// EVERY requested topic to come back or it errored the whole stream, and
    /// the app's reconnect loop then re-asked forever and was refused again —
    /// so subscribing to one module a node does not index took chat and pages
    /// dark for the life of the process.
    #[test]
    fn a_refused_topic_degrades_its_plane_and_spares_the_rest() {
        let expected = BTreeSet::from(["module:chat".to_string(), "module:valset".to_string()]);

        // subscribe time: the node names the topic and the reason.
        let refusal = r#"{"type":"error","topic":"module:valset","code":"unknown_module","detail":"this node indexes no such module"}"#;
        let event = decode_stream_message(Ok(Message::Text(refusal.into())), &expected, false)
            .expect("a refusal is an event")
            .expect("a refusal is not the connection failing");
        assert_eq!(
            event,
            ModuleEvent::Refused {
                module: "valset".into(),
                code: "unknown_module".into(),
            },
            "the TYPED code, not the sentence — a reason is greppable or it is prose"
        );

        // and the rest of the subscription still starts.
        let subscribed = r#"{"type":"subscribed","topics":{"module:chat":"c1"}}"#;
        let ready = decode_stream_message(Ok(Message::Text(subscribed.into())), &expected, false)
            .unwrap()
            .expect("a partial subscription is not a failure");
        assert_eq!(
            ready,
            ModuleEvent::Ready {
                cursors: BTreeMap::from([("module:chat".into(), "c1".into())]),
            }
        );

        // admitting NOTHING is the connection failing, not a plane degrading.
        let none = r#"{"type":"subscribed","topics":{}}"#;
        assert!(
            decode_stream_message(Ok(Message::Text(none.into())), &expected, false)
                .unwrap()
                .is_err()
        );

        // the SAME frame after `subscribed` is a live topic dropping out — a
        // scan error or a poisoned index. Transient, and the reconnect is what
        // recovers it, so it must still tear the stream down.
        assert!(
            decode_stream_message(Ok(Message::Text(refusal.into())), &expected, true)
                .unwrap()
                .is_err(),
            "a mid-session drop must not be mistaken for a permanent refusal"
        );
    }

    #[test]
    fn decodes_only_subscribed_module_topics() {
        let expected = BTreeSet::from(["module:chat".to_string(), "module:pages".to_string()]);
        let subscribed =
            r#"{"type":"subscribed","topics":{"module:chat":"c1","module:pages":"p1"}}"#;
        let ready = decode_stream_message(Ok(Message::Text(subscribed.into())), &expected, false)
            .unwrap()
            .unwrap();
        assert_eq!(
            ready,
            ModuleEvent::Ready {
                cursors: BTreeMap::from([
                    ("module:chat".into(), "c1".into()),
                    ("module:pages".into(), "p1".into()),
                ]),
            }
        );

        let unexpected =
            r#"{"type":"event","topic":"module:files","cursor":"f1","op":{"height":7}}"#;
        assert!(
            decode_stream_message(Ok(Message::Text(unexpected.into())), &expected, true)
                .unwrap()
                .is_err()
        );

        let unknown = r#"{"type":"retired_frame"}"#;
        assert!(
            decode_stream_message(Ok(Message::Text(unknown.into())), &expected, true)
                .unwrap()
                .is_err()
        );
    }

    #[test]
    fn status_requires_the_current_shape() {
        assert!(
            serde_json::from_str::<Status>(r#"{"height":7}"#).is_err(),
            "a status without public_key must not receive a removed default"
        );
    }

    /// A submit's wait is the node's, not the network's: the node answers only
    /// once the block commits, and a blob-bearing frame's block cannot apply
    /// until the pack has reached every other validator. So the only budget
    /// that is not a guess is one that outlasts the node's own hold.
    #[test]
    fn a_submit_budget_outlasts_the_node_still_fanning_the_pack_out() {
        // the node's hold, from `bin/node/src/relay.rs`: a 10 s base plus the
        // pack at 1 MiB/s, hex-inflated (2x) and counted once per target.
        let node_hold = |bytes: u64, targets: u64| {
            Duration::from_secs(10 + (bytes * 2 * targets).div_ceil(1024 * 1024))
        };
        // ducktape's own history, the pack that exposed this.
        let repository = 133 * 1024 * 1024;

        assert!(
            submit_budget(repository) > Duration::from_secs(30),
            "the flat budget is exactly what reported a committing push as failed"
        );
        for targets in 1..=8 {
            assert!(
                submit_budget(repository) > node_hold(repository, targets),
                "at {targets} targets the client gives up before the node does"
            );
        }
        // and an op that carries no pack is an ordinary RPC, budgeted as one.
        assert_eq!(submit_budget(0), TIMEOUT);
    }

    /// The two failures a caller must never confuse. `Refused` is a verdict it
    /// may relay; `Unresolved` means nobody said no and the op may be landing.
    #[test]
    fn only_an_answered_submit_reads_as_a_refusal() {
        let refused = SubmitFailure::Refused("forge: non-fast-forward".into());
        let unresolved = SubmitFailure::Unresolved("transaction submission failed".into());
        assert_ne!(refused, unresolved);
        assert!(matches!(refused, SubmitFailure::Refused(_)));
        assert!(matches!(unresolved, SubmitFailure::Unresolved(_)));
        // both render as their detail — the distinction is the variant, so a
        // caller that only prints one cannot accidentally branch on prose.
        assert_eq!(refused.to_string(), "forge: non-fast-forward");
        assert_eq!(unresolved.to_string(), "transaction submission failed");
    }

    #[tokio::test(start_paused = true)]
    async fn silent_module_stream_times_out() {
        let socket = futures::stream::pending::<
            std::result::Result<Message, tokio_tungstenite::tungstenite::Error>,
        >();
        let expected = BTreeSet::from(["module:chat".to_string()]);
        let mut stream = module_event_stream(socket, expected, STREAM_IDLE_TIMEOUT);
        let waiting = tokio::spawn(async move { stream.next().await.unwrap() });
        tokio::task::yield_now().await;
        tokio::time::advance(STREAM_IDLE_TIMEOUT).await;

        let error = waiting.await.unwrap().unwrap_err();
        assert_eq!(error.to_string(), "RPC stream heartbeat timed out");
    }
}
