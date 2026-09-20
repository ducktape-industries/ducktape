//! The gateway (credential-side) HTTP service, behind the `server` feature. The
//! `airlock-gateway` binary is a thin wrapper over [`build`]/[`serve`]; tests
//! drive the same router in-process. Enclave keys are minted per process and
//! never leave memory; the operator cannot read the sealed credential back out.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, OriginalUri, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header::AUTHORIZATION};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::Json;
pub use axum::Router;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand_core::OsRng;

use crate::attest;
use crate::bodyseal;
use crate::codesign;
use crate::handshake;
use crate::seal::{self, SealKeypair};
use crate::sign;
use crate::token::{self, Claims};
use crate::wire::{
    AttestationResponse, CredentialKind, CredentialPayload, CredentialUpload, SessionRequest,
    SessionResponse, WorkRef,
};

/// Where `airlock-gateway serve` binds when `--listen` is absent: loopback,
/// so nothing outside the confidential VM reaches the gateway unless the
/// operator binds it wider on purpose.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:9100";

/// How the gateway proves its seal key to the broker.
#[derive(Clone)]
pub enum AttestMode {
    /// configfs-tsm hardware attestation. Carries the operator's requested
    /// vendor: `tdx` | `snp` | `auto` (auto probes the silicon).
    Tsm(String),
    /// No TEE. There is no quote; the trust anchor is the seal_pk published on
    /// consensus, which the broker pins. The gateway serves an empty quote under
    /// vendor `self-host`.
    SelfHost,
}

/// Everything the gateway needs to serve. Keys are minted inside [`build`]
/// unless `seal_keypair` injects one (the self-host path pins the on-chain key).
pub struct GatewayConfig {
    pub attest: AttestMode,
    /// The enclave seal keypair. `None` mints a fresh one — the TEE path binds it
    /// into the quote. Self-host injects the on-chain-published keypair so the
    /// broker's pinned seal_pk matches what this gateway seals under.
    pub seal_keypair: Option<SealKeypair>,
    /// Upstream base for `CredentialKind::Claude` credentials.
    pub anthropic_base: String,
    /// Upstream base for `CredentialKind::Codex` credentials.
    pub openai_base: String,
    pub oauth_token_url: String,
    pub oauth_client_id: String,
    pub session_ttl_secs: u64,
    pub max_requests: u32,
    /// The signing toolchain, when this gateway serves `POST /sign/macos-bundle`.
    /// `None` mounts no signing route at all (see [`assemble`]): the model
    /// lenders — the self-host daemon, the node embed, tests — hold no
    /// `rcodesign`; the enclave image does, and names it here.
    pub sign: Option<sign::Tools>,
}

struct Config {
    anthropic_base: String,
    openai_base: String,
    oauth_token_url: String,
    oauth_client_id: String,
    session_ttl_secs: u64,
    max_requests: u32,
}

struct Oauth {
    access_token: String,
    refresh_token: String,
    expires_at: u64,
}

/// A model vendor the `/v1/*` proxy can reach: the two arms of
/// [`CredentialKind`] that name an upstream. Decided once at admission so the
/// proxy branches on "which upstream", never on "is this a model credential".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ModelVendor {
    Claude,
    Codex,
}

/// A model credential's token state: the (refreshable) tokens and a
/// single-flight gate so two concurrent proxied calls never double-spend one
/// rotating refresh token.
struct OauthState {
    oauth: Mutex<Oauth>,
    refresh_gate: tokio::sync::Mutex<()>,
}

/// What a named credential IS, decided at admission from `(kind, payload)`.
/// ONE discriminant: a model credential is served on `/v1/*` and nothing else;
/// a signing identity is held for a signing route and refused on `/v1/*`.
enum CredMaterial {
    Oauth {
        vendor: ModelVendor,
        state: OauthState,
    },
    /// A signing identity, handed to the `/sign/macos-bundle` pipeline and
    /// read by nothing else. Shared, so the pipeline's blocking task holds
    /// it without the store's entry.
    AppleCodesign(Arc<codesign::AppleCodesign>),
}

/// One named credential: its kind (what the audit line and the store loader
/// name it by) and the material admitted under it.
struct CredEntry {
    kind: CredentialKind,
    material: CredMaterial,
}

impl CredEntry {
    /// The model token state, or the refusal a non-model credential answers
    /// the proxy with. An `apple-codesign` session that reaches `/v1/*` is a
    /// caller pointing a signing identity at a model upstream.
    fn oauth(&self) -> Result<(ModelVendor, &OauthState), AppErr> {
        match &self.material {
            CredMaterial::Oauth { vendor, state } => Ok((*vendor, state)),
            CredMaterial::AppleCodesign(_) => Err(AppErr(
                StatusCode::FORBIDDEN,
                "credential_kind_mismatch".into(),
            )),
        }
    }

    /// The signing identity, or the refusal a model credential answers the
    /// signing route with: the mirror of [`Self::oauth`].
    fn apple_codesign(&self) -> Result<Arc<codesign::AppleCodesign>, AppErr> {
        match &self.material {
            CredMaterial::AppleCodesign(identity) => Ok(identity.clone()),
            CredMaterial::Oauth { .. } => Err(AppErr(
                StatusCode::FORBIDDEN,
                "credential_kind_mismatch".into(),
            )),
        }
    }
}

/// Replay-set key: the credential name plus the session's own ephemeral pk
/// (base64), so a set is never shared across two different sessions of the
/// same credential.
type ReplayKey = (String, String);

struct AppState {
    seal_kp: SealKeypair,
    sess_sk: SigningKey,
    sess_pk: VerifyingKey,
    quote: Vec<u8>,
    /// "tdx" | "snp" | "self-host" — advertised so the client picks the right
    /// verifier (or, for self-host, pins the on-chain seal_pk instead).
    vendor: String,
    http: reqwest::Client,
    cfg: Config,
    /// The named credential store, keyed by credential name (== session `sub`).
    /// Seeded at build and/or filled by sealed `/credential` uploads.
    creds: Mutex<HashMap<String, Arc<CredEntry>>>,
    /// Remaining request budget per session `sub` (credential NAME), refilled by
    /// every `/session` open. Deliberately named for what it is: a per-credential
    /// throttle shared by all of that credential's borrowers, NOT a per-session
    /// cap and NOT an authorization boundary — reopening a session refills it.
    /// The boundary is [`AppState::grant_check`], which decides who may open a
    /// session at all.
    budgets: Mutex<HashMap<String, u32>>,
    /// Sealed-request nonces already served — replay dedupe, keyed by
    /// `(sub, eph)`: the credential name PLUS the session's own ephemeral pk,
    /// not the name alone. A bearer minted for one session stays live for the
    /// full TTL regardless of how many other `/session` calls that credential
    /// name sees, so the set a bearer's replays are checked against must only
    /// ever be cleared by that SAME session's own token expiring — never by a
    /// later, unrelated `/session` open (which mints a fresh `eph` and so gets
    /// its own, empty entry here). An entry lands beside a spent request (the
    /// nonce is recorded after the AEAD opened the blob and after the budget
    /// spend), so each session's entry holds at most `max_requests` nonces;
    /// recording an unauthenticated 12-byte prefix would let anyone holding a
    /// bearer grow this map for free, forever.
    /// ponytail: entries for a forgotten session are pruned only on
    /// credential removal, not on token expiry, so a credential lent to many
    /// short sessions accumulates one small entry per session for the life of
    /// the process; add TTL-keyed eviction if that growth ever matters.
    seen_nonces: Mutex<HashMap<ReplayKey, std::collections::HashSet<Vec<u8>>>>,
    /// The co-hosted-lending grant gate (see [`GrantCheck`]). `None` on gateways
    /// that never lend, where every known credential opens without a grant check.
    grant_check: Option<GrantCheck>,
    /// Lazy store loader (see [`ReloadCredential`]): consulted before every
    /// session open and every proxied request, so a credential the operator
    /// added, rotated or REMOVED after boot takes effect without a restart.
    /// `None` on gateways with no backing store (TEE, tests).
    reload: Option<ReloadCredential>,
    /// The signing toolchain behind `POST /sign/macos-bundle`; the route is
    /// mounted only when this is `Some` (see [`GatewayConfig::sign`]).
    sign: Option<Arc<sign::Tools>>,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// Quote generation, injected. Production uses configfs-tsm; the testkit
/// injects a minted-chain quoter. A process that injects a quoter already
/// controls the process — clients only trust what verifies against pinned
/// vendor roots, so this seam grants no forgery power.
pub type Quoter = Box<dyn Fn(&[u8; attest::REPORT_DATA_LEN]) -> Result<Vec<u8>> + Send + Sync>;

/// What the injected grant gate answered. THREE states, not two: a `bool` has
/// no room for "I could not ask", and folding that into a refusal is the
/// expensive mistake — it tells the borrower's operator to go add a grant that
/// already exists, which is the one thing provably not wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantAnswer {
    /// the authority answered, and its committed record admits this account.
    Granted,
    /// the authority answered, and it does not admit this account — no such
    /// record, or an account that is neither the owner nor a grantee.
    Refused,
    /// the authority could not be ASKED: a node link that timed out, a refused
    /// connection, a resident that is not serving, a reply that would not
    /// decode. Nothing at all is known about the grant.
    Undetermined,
}

/// Everything the injected gate is given about one session-open. A struct, not a
/// positional list: `credential` and `caller_node` are both opaque identifiers,
/// and a security gate whose two arguments can be silently transposed is a
/// defect waiting to happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantQuestion {
    /// the credential NAME the session names (`SessionRequest::sub`).
    pub credential: String,
    /// the NODE the TRANSPORT vouched for — see [`CALLER_NODE_HEADER`]. The
    /// only identity input, and the request contributes nothing to it. A node
    /// is never an account: the gate reaches an account only through the
    /// committed work the session points at.
    pub caller_node: Vec<u8>,
    /// WHICH WORK the session draws for. A pointer into the lender's own
    /// committed state, never a claim — see [`WorkRef`].
    pub work: WorkRef,
}

/// The co-hosted-lending grant gate, injected by the node. Given a
/// [`GrantQuestion`] it answers whether that session may open — the node
/// resolves it against its own COMMITTED state (the gateway-module credential
/// record, and for a delegated pointer the saga module and the identity module
/// too).
///
/// The node handed here is ALWAYS the one the node's proxy vouched for in
/// [`CALLER_NODE_HEADER`]. There is no other source, and there must not be
/// one: the record's `owner_account` is a public field of the very record a
/// borrower must read to learn `seal_pk`, so a gate keyed on anything the
/// request could carry admits everyone who can read the chain. See
/// [`session_gate`].
///
/// The `work` pointer is different in kind and that difference is the whole of
/// the delegation design: it is an ID, and every fact derived from it is READ
/// FROM CONSENSUS by the answering node. A caller that names a saga still
/// cannot say who submitted it, who holds its lease, or whether that submitter
/// is granted.
///
/// `None` on gateways that never lend (owner-local, TEE): there is no subject to
/// check. See [`GrantAnswer`] for what each answer costs the borrower.
pub type GrantCheck =
    Arc<dyn Fn(GrantQuestion) -> Pin<Box<dyn Future<Output = GrantAnswer> + Send>> + Send + Sync>;

/// What the on-disk store holds for one credential name, as the lazy loader
/// found it.
///
/// THREE answers, not two: "the artifact has not changed" and "there is no
/// artifact" are opposite instructions to a running gateway, and an `Option`
/// spelled them the same way — which is how `user cred remove` left a deleted
/// credential being spent by every outstanding session until a restart.
pub enum StoreLoad {
    /// An artifact the live entry was not built from: first sight of the name,
    /// or one a re-login rotated in place. Adopt it.
    Loaded(CredentialKind, CredentialPayload),
    /// The store holds exactly what this gateway already serves.
    Unchanged,
    /// The store holds nothing under that name — `user cred remove`/`revoke`
    /// deleted it. Forget it.
    Absent,
}

/// Lazy store loader: given a credential name, say what the on-disk store holds
/// for it. Lets a running gateway track `user cred add`/re-login/`remove`
/// without a node restart — the session and proxy handlers both call it before
/// resolving a name. `None` on gateways with no backing store (TEE, tests).
pub type ReloadCredential = Arc<dyn Fn(&str) -> StoreLoad + Send + Sync>;

/// Whether this gateway serves `POST /credential`.
///
/// ONE discriminant, decided at build time by which build path ran, because the
/// two answers come from genuinely different topologies:
///
/// - [`Self::Accepted`] — the ATTESTED build. An enclave has no other way to
///   receive a credential: there is a real host-vs-enclave boundary and the
///   upload is the only thing that crosses it. Its listener is the CVM's own,
///   not something a network member is routed to.
/// - [`Self::Refused`] — the SELF-HOST lender. Its credentials come from the
///   operator's own disk store (`ducktape user cred add`), which the lazy
///   [`ReloadCredential`] picks up without a restart, so nothing legitimate ever
///   posts here. Its router, by contrast, IS reachable by any admitted network
///   member through the owner's signed `airlock` gateway route — and sealing is
///   not authentication, since the seal public key is on chain and served at
///   `/attestation`. An open upload endpoint there is a way for any member to
///   overwrite a lender's credential with an attacker-chosen bearer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CredentialUploads {
    Accepted,
    Refused,
}

/// The unforgeable half of a session request.
///
/// The node's gateway proxy mints `x-duck-caller-node` (the hex node key) from
/// the mesh-verified WireGuard peer identity (`bin/node/src/gateway_plane.rs`),
/// and the proxy's own decode REFUSES a caller-supplied `x-duck-*`, so a
/// borrower cannot write it. Everything else on a [`SessionRequest`] is chosen
/// by whoever composed the JSON.
pub const CALLER_NODE_HEADER: &str = "x-duck-caller-node";

/// Build the gateway router and report the vendor ("tdx"/"snp"/"self-host").
pub fn build(cfg: GatewayConfig) -> Result<(Router, String)> {
    build_seeded(cfg, Vec::new())
}

/// Self-host build with a lazy store loader: seeds the gateway with whatever the
/// store holds now AND lets it load credentials added later (see
/// [`ReloadCredential`]). The credential-lending node embed calls this so
/// `cred add` takes effect without a restart.
pub fn build_self_host_reloadable(
    cfg: GatewayConfig,
    seeds: Vec<(String, CredentialKind, CredentialPayload)>,
    grant_check: Option<GrantCheck>,
    reload: ReloadCredential,
) -> Result<(Router, String)> {
    build_self_host(cfg, seeds, grant_check, Some(reload))
}

/// Like [`build_seeded`], but with the co-hosted-lending grant gate wired: the
/// node's own committed-state grant lookup. Only the credential-lending node
/// embed calls this; every other build path leaves the gate off.
pub fn build_seeded_gated(
    cfg: GatewayConfig,
    seeds: Vec<(String, CredentialKind, CredentialPayload)>,
    grant_check: Option<GrantCheck>,
) -> Result<(Router, String)> {
    match cfg.attest.clone() {
        AttestMode::Tsm(spec) => {
            let mode = if spec == "auto" {
                tsm_probe_provider()?
            } else {
                spec.parse::<attest::AttestMode>()?
            };
            build_with_quoter_gated(cfg, mode.as_str(), tsm_quoter(mode), seeds, grant_check)
        }
        AttestMode::SelfHost => build_self_host(cfg, seeds, grant_check, None),
    }
}

/// Like [`build`], but seed the store with named credentials directly instead of
/// waiting for sealed `/credential` uploads. Used when the credential provider IS
/// the gateway process (the node embed): there is no host-vs-enclave boundary to
/// seal across, so credentials are handed in-process. A `Bearer` is static (no
/// rotation); a claude `Refresh` is refreshed lazily; a codex `Refresh` is
/// refused (bearer-only lane).
pub fn build_seeded(
    cfg: GatewayConfig,
    seeds: Vec<(String, CredentialKind, CredentialPayload)>,
) -> Result<(Router, String)> {
    build_seeded_gated(cfg, seeds, None)
}

fn tsm_quoter(expected: attest::AttestMode) -> Quoter {
    Box::new(move |rd| tsm_gen_quote(Some(expected), rd).map(|(_, quote)| quote))
}

/// Build the gateway with an injected quote generator (see [`build_seeded`]).
/// Mints/takes the enclave seal key, mints the session key, and calls `quoter`
/// once on the freshly bound REPORTDATA.
pub fn build_with_quoter(
    cfg: GatewayConfig,
    vendor: &str,
    quoter: Quoter,
    seeds: Vec<(String, CredentialKind, CredentialPayload)>,
) -> Result<(Router, String)> {
    build_with_quoter_gated(cfg, vendor, quoter, seeds, None)
}

fn build_with_quoter_gated(
    mut cfg: GatewayConfig,
    vendor: &str,
    quoter: Quoter,
    seeds: Vec<(String, CredentialKind, CredentialPayload)>,
    grant_check: Option<GrantCheck>,
) -> Result<(Router, String)> {
    let seal_kp = cfg.seal_keypair.take().unwrap_or_else(SealKeypair::generate);
    let sess_sk = SigningKey::generate(&mut OsRng);
    let sess_pk = sess_sk.verifying_key();

    let report_data = attest::make_report_data(&seal_kp.public_bytes(), &sess_pk.to_bytes());
    let quote = quoter(&report_data)?;
    assemble(Assembly {
        cfg,
        vendor: vendor.to_string(),
        quote,
        seal_kp,
        sess_sk,
        sess_pk,
        seeds,
        grant_check,
        reload: None,
        uploads: CredentialUploads::Accepted,
    })
}

/// Non-TEE build: no quote, vendor "self-host". The broker pins the seal_pk from
/// consensus, so there is nothing to attest here.
fn build_self_host(
    mut cfg: GatewayConfig,
    seeds: Vec<(String, CredentialKind, CredentialPayload)>,
    grant_check: Option<GrantCheck>,
    reload: Option<ReloadCredential>,
) -> Result<(Router, String)> {
    let seal_kp = cfg.seal_keypair.take().unwrap_or_else(SealKeypair::generate);
    let sess_sk = SigningKey::generate(&mut OsRng);
    let sess_pk = sess_sk.verifying_key();
    assemble(Assembly {
        cfg,
        vendor: "self-host".to_string(),
        quote: Vec::new(),
        seal_kp,
        sess_sk,
        sess_pk,
        seeds,
        grant_check,
        reload,
        uploads: CredentialUploads::Refused,
    })
}

/// Everything [`assemble`] needs, already resolved by whichever build path ran.
struct Assembly {
    cfg: GatewayConfig,
    vendor: String,
    quote: Vec<u8>,
    seal_kp: SealKeypair,
    sess_sk: SigningKey,
    sess_pk: VerifyingKey,
    seeds: Vec<(String, CredentialKind, CredentialPayload)>,
    grant_check: Option<GrantCheck>,
    reload: Option<ReloadCredential>,
    uploads: CredentialUploads,
}

/// Shared assembly: build the named store from the seeds, wire the state and the
/// router. The two build paths differ only in vendor/quote/keys and whether they
/// serve `/credential`, all resolved before this point.
fn assemble(assembly: Assembly) -> Result<(Router, String)> {
    let Assembly {
        cfg,
        vendor,
        quote,
        seal_kp,
        sess_sk,
        sess_pk,
        seeds,
        grant_check,
        reload,
        uploads,
    } = assembly;
    let mut creds = HashMap::new();
    for (name, kind, payload) in seeds {
        creds.insert(name, Arc::new(cred_entry(kind, payload)?));
    }

    let state = Arc::new(AppState {
        seal_kp,
        sess_sk,
        sess_pk,
        quote,
        vendor: vendor.clone(),
        http: reqwest::Client::new(),
        cfg: Config {
            anthropic_base: cfg.anthropic_base,
            openai_base: cfg.openai_base,
            oauth_token_url: cfg.oauth_token_url,
            oauth_client_id: cfg.oauth_client_id,
            session_ttl_secs: cfg.session_ttl_secs,
            max_requests: cfg.max_requests,
        },
        creds: Mutex::new(creds),
        budgets: Mutex::new(HashMap::new()),
        seen_nonces: Mutex::new(HashMap::new()),
        grant_check,
        reload,
        sign: cfg.sign.map(Arc::new),
    });

    let app = Router::new()
        .route("/attestation", get(attestation))
        .route("/session", post(session))
        // Proxy the whole /v1/* surface (Claude Code calls /v1/messages and
        // /v1/messages/count_tokens, not just messages).
        //
        // axum's `Bytes` extractor (the `proxy` handler's `body` argument)
        // applies a 2 MiB default limit unless overridden — every neighbour
        // in this lane (the broker in front of it, the route policy in front
        // of that) is sized for real model traffic, so this must match the
        // broker's cap or a lent-credential run 413s on a body the broker and
        // the route policy both already accepted.
        .route(
            "/v1/{*rest}",
            any(proxy).layer(DefaultBodyLimit::max(crate::MAX_REQUEST_BYTES)),
        );
    // NOT mounted-then-guarded: a route that exists and refuses is one bad
    // refactor away from a route that exists and accepts. See
    // [`CredentialUploads`] for why only the attested build has one.
    let app = match uploads {
        CredentialUploads::Accepted => app.route("/credential", post(credential)),
        CredentialUploads::Refused => app,
    };
    // Same rule for the signing route: mounted only where the image holds
    // the toolchain. Its body is read by the handler under its own cap
    // (`sign::MAX_BUNDLE_BYTES`) so the over-size answer is a named refusal,
    // not the extractor's 413 text; it is not under `/v1`, which is the
    // model surface.
    let serves_signing = state.sign.is_some();
    let app = if serves_signing {
        app.route("/sign/macos-bundle", post(sign_macos_bundle))
    } else {
        app
    };
    Ok((app.with_state(state), vendor))
}

/// Turn a seed/upload payload into a [`CredEntry`]. A `Bearer` is static
/// (`expires_at = MAX`, never refreshed); a claude `Refresh` starts empty and is
/// exchanged lazily; a codex `Refresh` is rejected — codex is bearer-only in v1.
/// An `apple-codesign` payload is admitted by [`codesign::AppleCodesign::admit`]
/// and refused by its token otherwise. A payload under the wrong kind is
/// `credential_kind_mismatch`: the kind is routing metadata the payload must
/// agree with, not a hint.
fn cred_entry(kind: CredentialKind, payload: CredentialPayload) -> Result<CredEntry> {
    let material = match (kind, payload) {
        (CredentialKind::Claude, CredentialPayload::Bearer { access_token }) => {
            oauth_material(ModelVendor::Claude, static_bearer(access_token))
        }
        (CredentialKind::Codex, CredentialPayload::Bearer { access_token }) => {
            oauth_material(ModelVendor::Codex, static_bearer(access_token))
        }
        (
            CredentialKind::Claude,
            CredentialPayload::Refresh {
                refresh_token,
                access_token,
                expires_at,
            },
        ) => oauth_material(
            ModelVendor::Claude,
            Oauth {
                access_token,
                refresh_token,
                expires_at,
            },
        ),
        (CredentialKind::Codex, CredentialPayload::Refresh { .. }) => {
            bail!("codex credentials must be a static bearer token; oauth refresh is not supported")
        }
        (
            CredentialKind::AppleCodesign,
            CredentialPayload::AppleCodesign {
                p12_b64,
                p12_password,
                api_key_json,
                team_id,
            },
        ) => CredMaterial::AppleCodesign(Arc::new(codesign::AppleCodesign::admit(
            &p12_b64,
            &p12_password,
            &api_key_json,
            &team_id,
        )?)),
        (CredentialKind::AppleCodesign, CredentialPayload::Bearer { .. })
        | (CredentialKind::AppleCodesign, CredentialPayload::Refresh { .. })
        | (CredentialKind::Claude, CredentialPayload::AppleCodesign { .. })
        | (CredentialKind::Codex, CredentialPayload::AppleCodesign { .. }) => {
            bail!("credential_kind_mismatch")
        }
    };
    Ok(CredEntry { kind, material })
}

fn static_bearer(access_token: String) -> Oauth {
    Oauth {
        access_token,
        refresh_token: String::new(),
        expires_at: u64::MAX,
    }
}

fn oauth_material(vendor: ModelVendor, oauth: Oauth) -> CredMaterial {
    CredMaterial::Oauth {
        vendor,
        state: OauthState {
            oauth: Mutex::new(oauth),
            refresh_gate: tokio::sync::Mutex::new(()),
        },
    }
}

/// The audit line for a credential entering the store, from either door (a
/// sealed upload or the lender's disk store). Kind and name only — never a
/// token, a key, or a team.
fn log_credential_added(name: &str, kind: CredentialKind) {
    tracing::info!(
        target: "ducktape::airlock",
        event = "credential_added",
        credential = %name,
        kind = ?kind,
        "credential added"
    );
}

/// Serve an already-built gateway router. The node embed BUILDS (and thus
/// attests) at boot — before registering its route or claiming to listen —
/// and only serves here; a box that cannot attest never reaches this point.
pub async fn serve_router(listener: tokio::net::TcpListener, app: Router) -> Result<()> {
    axum::serve(listener, app).await?;
    Ok(())
}

/// Generate a real confidential-VM quote via `configfs-tsm` (kernel >= 6.7,
/// only inside a CVM guest). Vendor-generic: Intel TDX and AMD SEV-SNP both write
/// REPORTDATA to `inblob` and read the raw report/quote from `outblob`; the
/// `provider` attribute names the vendor. `expected` is the operator's requested
/// vendor (`None` = auto); a mismatch errors. Untested off-hardware.
fn tsm_gen_quote(
    expected: Option<attest::AttestMode>,
    report_data: &[u8; attest::REPORT_DATA_LEN],
) -> Result<(attest::AttestMode, Vec<u8>)> {
    use std::fs;
    let dir = format!("/sys/kernel/config/tsm/report/airlock-{}", std::process::id());
    fs::create_dir(&dir)
        .with_context(|| format!("create {dir} (are we inside a TDX/SEV-SNP guest?)"))?;
    let result = (|| -> Result<(attest::AttestMode, Vec<u8>)> {
        let provider =
            fs::read_to_string(format!("{dir}/provider")).context("read configfs-tsm provider")?;
        let detected = provider_to_mode(provider.trim())?;
        if let Some(want) = expected
            && want != detected
        {
            bail!(
                "attest={} but the guest reports {} ({provider:?})",
                want.as_str(),
                detected.as_str()
            );
        }
        fs::write(format!("{dir}/inblob"), report_data).context("write inblob")?;
        let quote = fs::read(format!("{dir}/outblob")).context("read outblob")?;
        Ok((detected, quote))
    })();
    let _ = fs::remove_dir(&dir);
    result
}

fn provider_to_mode(provider: &str) -> Result<attest::AttestMode> {
    match provider {
        "tdx_guest" => Ok(attest::AttestMode::Tdx),
        "sev_guest" => Ok(attest::AttestMode::Snp),
        other => bail!("unsupported configfs-tsm provider {other:?} (want tdx_guest/sev_guest)"),
    }
}

/// Probe the configfs-tsm provider to learn the hardware vendor without
/// generating a quote (the `auto` mode).
fn tsm_probe_provider() -> Result<attest::AttestMode> {
    use std::fs;
    let dir = format!("/sys/kernel/config/tsm/report/airlock-probe-{}", std::process::id());
    fs::create_dir(&dir)
        .with_context(|| format!("create {dir} (are we inside a TDX/SEV-SNP guest?)"))?;
    let provider = fs::read_to_string(format!("{dir}/provider"));
    let _ = fs::remove_dir(&dir);
    provider_to_mode(provider.context("read configfs-tsm provider")?.trim())
}

// -------- handlers --------

#[derive(Debug)]
struct AppErr(StatusCode, String);
impl IntoResponse for AppErr {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

async fn attestation(State(st): State<Arc<AppState>>) -> Json<AttestationResponse> {
    Json(AttestationResponse {
        quote_b64: BASE64.encode(&st.quote),
        vendor: st.vendor.clone(),
    })
}

/// The refusal an upload under a name the store already holds answers. An
/// upload only ever CREATES: this gateway serves no route that replaces or
/// removes a credential, and holds them in memory only, so a held name stays
/// exactly what was first uploaded under it until the gateway restarts.
fn credential_exists(name: &str) -> AppErr {
    AppErr(
        StatusCode::CONFLICT,
        format!(
            "credential_exists: a credential named {name:?} is already held here; an upload \
             never replaces one, so upload under another name"
        ),
    )
}

async fn credential(
    State(st): State<Arc<AppState>>,
    Json(up): Json<CredentialUpload>,
) -> Result<StatusCode, AppErr> {
    // refused before anything else runs: the refresh probe below would spend
    // the uploader's refresh token upstream for an upload that cannot land.
    if st.creds.lock().unwrap().contains_key(&up.name) {
        return Err(credential_exists(&up.name));
    }
    let blob = BASE64
        .decode(up.sealed_b64)
        .map_err(|e| AppErr(StatusCode::BAD_REQUEST, format!("bad base64: {e}")))?;
    let pt = seal::unseal(&st.seal_kp, &blob)
        .map_err(|e| AppErr(StatusCode::BAD_REQUEST, e.to_string()))?;
    let payload: CredentialPayload = serde_json::from_slice(&pt)
        .map_err(|e| AppErr(StatusCode::BAD_REQUEST, format!("bad payload: {e}")))?;

    let entry = Arc::new(
        cred_entry(up.kind, payload).map_err(|e| AppErr(StatusCode::BAD_REQUEST, e.to_string()))?,
    );
    // A claude refresh credential starts with no access token — prove it works
    // now, so a later /v1/messages isn't the first time we learn it's broken. A
    // static bearer already holds its token, and a signing identity has no
    // upstream to probe: nothing to do for either.
    let probe = match &entry.material {
        CredMaterial::Oauth { state, .. } => {
            let empty = state.oauth.lock().unwrap().access_token.is_empty();
            empty.then_some(state)
        }
        CredMaterial::AppleCodesign(_) => None,
    };
    if let Some(state) = probe {
        refresh_now(&st.cfg, &st.http, state).await.map_err(|e| {
            AppErr(
                StatusCode::BAD_GATEWAY,
                format!("initial refresh failed: {e}"),
            )
        })?;
    }
    // asked again under the lock: two uploads of one name may both have passed
    // the check above while the probe ran.
    let kind = entry.kind;
    match st.creds.lock().unwrap().entry(up.name.clone()) {
        std::collections::hash_map::Entry::Occupied(_) => return Err(credential_exists(&up.name)),
        std::collections::hash_map::Entry::Vacant(slot) => slot.insert(entry),
    };
    log_credential_added(&up.name, kind);
    Ok(StatusCode::OK)
}

/// The node the TRANSPORT vouched for, or `None` when nothing did.
///
/// This is the whole of the identity input, and a session request contributes
/// nothing to it: the node's gateway proxy mints [`CALLER_NODE_HEADER`] from
/// the mesh-verified WireGuard peer and refuses a caller-supplied copy at its own
/// decode, so it is the one field on the request that its sender cannot choose.
fn vouched_caller(headers: &HeaderMap) -> Option<Vec<u8>> {
    headers
        .get(CALLER_NODE_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|encoded| hex::decode(encoded).ok())
}

/// What a `/session` request is answered with. FOUR outcomes: the grant answer
/// is only reachable once the transport has vouched for a caller, and "nobody
/// vouched for you" is a refusal an operator acts on differently from a missing
/// grant. Every one is a stable snake_case token, and none echoes back the value
/// that would have been accepted.
enum SessionGate {
    Open,
    CallerUnverified,
    NotGranted,
    AuthorityUnavailable,
}

/// Decide whether one session may open. Pure decision: it reads state and the
/// injected authority, and writes nothing.
///
/// Co-hosted lending: when a grant gate is wired, the caller must be a node
/// the node's proxy VOUCHED for, and the authority must find a grant for the
/// session — before any handshake work; a session nobody's grant covers never
/// opens.
///
/// The caller is the node that made the hop, which is the node running the
/// sandbox — and the ONE identity this flow can establish, since the session
/// token the sandbox ends up holding names a credential and nothing about who
/// is acting. A node is never an account, so it holds no grant of its own.
///
/// The request's [`WorkRef`] is how a grant is reached: the authority admits a
/// session on the grant held by the account whose user-signed frame submitted
/// the committed work. That is not a second identity input: it is an id, and
/// the authority derives every fact from it out of its own state.
///
/// With no gate wired (owner-local, TEE) this gateway lends to nobody across
/// accounts, so there is no subject to check.
async fn session_gate(
    grant_check: &Option<GrantCheck>,
    headers: &HeaderMap,
    req: &SessionRequest,
) -> SessionGate {
    let Some(check) = grant_check else {
        return SessionGate::Open;
    };
    let Some(caller_node) = vouched_caller(headers) else {
        return SessionGate::CallerUnverified;
    };
    let question = GrantQuestion {
        credential: req.sub.clone(),
        caller_node,
        work: req.work.clone(),
    };
    match check(question).await {
        GrantAnswer::Granted => SessionGate::Open,
        GrantAnswer::Refused => SessionGate::NotGranted,
        GrantAnswer::Undetermined => SessionGate::AuthorityUnavailable,
    }
}

/// Track the on-disk store for `name`: adopt what it added or rotated, forget
/// what it removed.
///
/// The loader is the one that decides there is anything to do (see
/// [`StoreLoad`]), so this runs on every session AND every proxied request
/// rather than only on a store miss. All three restart-free cases are the same
/// one call: a credential `cred add` wrote after boot, one a re-login rotated
/// in place, and one `cred remove`/`revoke` deleted — which must stop being
/// spent by the sessions already holding a token for it, not at the end of
/// their TTL.
///
/// A no-op when no loader is wired: an enclave has no backing store, so its
/// credentials only ever arrive over the sealed upload.
fn refresh_credential(st: &AppState, name: &str) {
    let Some(reload) = &st.reload else {
        return;
    };
    match reload(name) {
        StoreLoad::Loaded(kind, payload) => adopt_credential(st, name, kind, payload),
        StoreLoad::Unchanged => {}
        StoreLoad::Absent => forget_credential(st, name),
    }
}

/// Parse the store's artifact into live token state and serve it under `name`.
/// An artifact that will not parse leaves the previous entry in place — the
/// loader already logged what it skipped, and dropping a working credential on
/// a half-written file would be the worse failure.
fn adopt_credential(st: &AppState, name: &str, kind: CredentialKind, payload: CredentialPayload) {
    let Ok(entry) = cred_entry(kind, payload) else {
        return;
    };
    log_credential_added(name, entry.kind);
    st.creds
        .lock()
        .unwrap()
        .insert(name.to_string(), Arc::new(entry));
}

/// Drop every trace of a credential the store no longer holds: the parsed entry
/// (which carries the live access and refresh tokens), the budget its sessions
/// spend, and their replay set. The next proxied request on an outstanding
/// session finds no credential and is refused, so `user cred remove` stops the
/// spend when the operator runs it rather than when the last token expires.
fn forget_credential(st: &AppState, name: &str) {
    st.creds.lock().unwrap().remove(name);
    st.budgets.lock().unwrap().remove(name);
    st.seen_nonces.lock().unwrap().retain(|(sub, _eph), _| sub != name);
}

async fn session(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<SessionRequest>,
) -> Result<Json<SessionResponse>, AppErr> {
    // A session names the credential it draws on. The store loader gets first
    // refusal on every session, so both restart-free cases are covered: a
    // credential `cred add` wrote after boot, and one a re-login rotated in
    // place. A name that is still absent afterwards is a race or a stale record
    // and 404s.
    refresh_credential(&st, &req.sub);
    let known_credential = st.creds.lock().unwrap().contains_key(&req.sub);
    if !known_credential {
        return Err(AppErr(StatusCode::NOT_FOUND, "credential_not_found".into()));
    }
    // The one visible dispatch for "may this session open at all". Five answers,
    // and each names its OWN closed door: a 403 sends the borrower's operator to
    // go get a grant, so neither an authority we could not REACH nor a caller the
    // transport never vouched for may wear that one.
    match session_gate(&st.grant_check, &headers, &req).await {
        SessionGate::Open => {}
        SessionGate::CallerUnverified => {
            return Err(AppErr(StatusCode::FORBIDDEN, "caller_node_unverified".into()));
        }
        SessionGate::NotGranted => {
            return Err(AppErr(StatusCode::FORBIDDEN, "credential_not_granted".into()));
        }
        SessionGate::AuthorityUnavailable => {
            return Err(AppErr(
                StatusCode::SERVICE_UNAVAILABLE,
                "grant_authority_unavailable".into(),
            ));
        }
    }
    // Enclave side of the handshake: derive the shared key from the client's
    // ephemeral key and our static seal secret, then seal the token under it — so
    // only the client that ECDH'd against the *attested* seal_pk can open it.
    let eph = BASE64
        .decode(&req.client_eph_pk_b64)
        .ok()
        .and_then(|v| <[u8; 32]>::try_from(v).ok())
        .ok_or_else(|| AppErr(StatusCode::BAD_REQUEST, "bad client_eph_pk".into()))?;
    let keys = handshake::enclave_session_keys(&st.seal_kp, &eph);

    let now = now_secs();
    let claims = Claims {
        sub: req.sub.clone(),
        iat: now,
        exp: now + st.cfg.session_ttl_secs,
        eph: req.client_eph_pk_b64.clone(),
        seal: req.body_seal,
    };
    // The budget is shared per credential NAME and refills on every open, by
    // design (see [`AppState::budgets`]). The replay set is NOT shared: it is
    // keyed by `(sub, eph)`, and `req.client_eph_pk_b64` is freshly random per
    // open, so this call only ever touches a brand-new, empty entry of its
    // own — it must never clear another session's entry, because that
    // session's already-issued bearer stays valid (and replayable against
    // whatever it already recorded) for the rest of its TTL regardless of
    // what this call does.
    st.budgets.lock().unwrap().insert(req.sub.clone(), st.cfg.max_requests);
    let token = token::issue(&st.sess_sk, &claims);
    let sealed = handshake::seal_token(&keys.session, token.as_bytes());
    Ok(Json(SessionResponse { sealed_token_b64: BASE64.encode(sealed) }))
}

async fn proxy(
    State(st): State<Arc<AppState>>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match proxy_inner(&st, method, &uri, &headers, body).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

/// `POST /sign/macos-bundle`: sign, notarize and staple an unsigned
/// `Ducktape.app` with the session's `apple-codesign` identity, inside the
/// enclave. The session must be a SEALED one on a signing credential; the
/// body is the `.tar.zst` of the bundle, the reply the `.tar.zst` of the
/// finished one, sealed under the response stream key exactly as a proxied
/// model reply is. One request = one signature; the session's `max_requests`
/// caps signatures per session. Refusals are `sign::Refusal` tokens: before
/// the pipeline starts (the session, its kind, the body's admission) they
/// are the HTTP status and body; once it runs the head is already committed
/// and a refusal is the sealed stream's `Final` carrying the token.
async fn sign_macos_bundle(
    State(st): State<Arc<AppState>>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    match sign_macos_bundle_inner(&st, method, &uri, &headers, body).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn sign_macos_bundle_inner(
    st: &AppState,
    method: Method,
    uri: &axum::http::Uri,
    headers: &HeaderMap,
    body: Body,
) -> Result<Response, AppErr> {
    let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or(uri.path());
    let Some(tools) = st.sign.clone() else {
        // the route is only mounted with a toolchain (see `assemble`)
        return Err(AppErr(StatusCode::NOT_FOUND, "not_found".into()));
    };
    // Resolve the session and its kind before the body is read at all: a
    // model credential pointed here is refused by name, and costs nothing.
    let (claims, entry) = resolve_session(st, headers)?;
    let identity = entry.apple_codesign()?;
    // A signing session is sealed or it is nothing: the identity's output
    // rides back under the response stream key, and there is no plaintext
    // shape of this exchange.
    let Some(keys) = session_keys(st, &claims)? else {
        return Err(AppErr(
            StatusCode::BAD_REQUEST,
            "airlock: signing requires a sealed session".into(),
        ));
    };
    let body = axum::body::to_bytes(body, sign::MAX_BUNDLE_BYTES)
        .await
        .map_err(|_| AppErr(StatusCode::PAYLOAD_TOO_LARGE, "bundle_too_large".into()))?;
    let AdmittedBody { body, binding } = admit_body(
        st,
        &claims,
        Some(&keys),
        &method,
        path_and_query,
        headers,
        body,
    )?;
    let sha256_in = sign::sha256_hex(&body);
    let caller_node = vouched_caller(headers).map(hex::encode).unwrap_or_default();
    tracing::info!(
        target: "ducktape::airlock",
        event = "release_sign_requested",
        sub = %claims.sub,
        session = %claims.eph,
        caller_node = %caller_node,
        sha256_in = %sha256_in,
        size = body.len(),
        "release signing requested"
    );
    // The head goes out NOW, before a byte is signed. The pipeline is
    // blocking work (process spawns, tmpfs IO) that runs for minutes under
    // Apple's notary wait, and every hop between here and the caller bounds
    // its wait for a response head in seconds; the outcome rides the sealed
    // stream instead — the archive as data chunks then Final, a refusal as a
    // Final carrying its token — with a keepalive sealed in every
    // `sign::KEEPALIVE_INTERVAL` so no hop's idle deadline fires meanwhile.
    let (mut sealer, salt) = bodyseal::StreamSealer::new(&keys, &binding);
    let mut opening = salt;
    opening.extend(sealer.seal_head("application/zstd"));
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    tx.try_send(Ok(Bytes::from(opening)))
        .expect("a fresh channel takes its first item");
    let pipeline_identity = identity.clone();
    let pipeline =
        tokio::task::spawn_blocking(move || sign::sign_bundle(&tools, &pipeline_identity, &body));
    let audit = SignAudit {
        sub: claims.sub.clone(),
        session: claims.eph.clone(),
        sha256_in,
        team_id: identity.team_id().to_string(),
    };
    tokio::spawn(stream_signing_reply(sealer, pipeline, tx, audit));
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .body(Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)))
        .map_err(|e| AppErr(StatusCode::INTERNAL_SERVER_ERROR, format!("build response: {e}")))
}

/// What the signing audit lines name: the session and the request's
/// identity, fixed before the pipeline starts.
struct SignAudit {
    sub: String,
    session: String,
    sha256_in: String,
    team_id: String,
}

/// What the running pipeline sends next on the sealed reply stream.
enum SignEvent {
    /// The pipeline is still running; the stream must show liveness.
    Keepalive,
    /// The pipeline ended, with the archive or a refusal.
    Finished(Result<sign::Signed, sign::Refusal>),
}

/// Drain the pipeline onto the sealed reply stream: keepalives while it
/// runs, then the archive and `Final`, or the refusal token as the `Final`.
/// A caller that went away ends the drain; the pipeline runs to its end on
/// its blocking thread regardless (its work dir is dropped there).
async fn stream_signing_reply(
    mut sealer: bodyseal::StreamSealer,
    pipeline: tokio::task::JoinHandle<Result<sign::Signed, sign::Refusal>>,
    tx: tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    audit: SignAudit,
) {
    let mut ticks = tokio::time::interval(sign::KEEPALIVE_INTERVAL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticks.tick().await; // the first tick is immediate; the head just went out
    tokio::pin!(pipeline);
    let outcome = loop {
        let event = tokio::select! {
            joined = &mut pipeline => SignEvent::Finished(
                // a panicked pipeline is a codesign failure: the thread died
                // before it could name anything better.
                joined.unwrap_or(Err(sign::Refusal::CodesignFailed)),
            ),
            _ = ticks.tick() => SignEvent::Keepalive,
        };
        match event {
            SignEvent::Keepalive => {
                if tx.send(Ok(Bytes::from(sealer.seal_keepalive()))).await.is_err() {
                    return;
                }
            }
            SignEvent::Finished(outcome) => break outcome,
        }
    };
    let signed = match outcome {
        Ok(signed) => signed,
        Err(refusal) => {
            let submission_id = match &refusal {
                sign::Refusal::NotaryRejected { submission_id } => submission_id.clone(),
                sign::Refusal::BundleShapeRefused
                | sign::Refusal::BundleTooLarge
                | sign::Refusal::CodesignFailed
                | sign::Refusal::StapleFailed
                | sign::Refusal::ToolMissing => None,
            };
            tracing::warn!(
                target: "ducktape::airlock",
                event = "release_sign_refused",
                sub = %audit.sub,
                session = %audit.session,
                sha256_in = %audit.sha256_in,
                reason = %refusal.as_str(),
                submission_id = %submission_id.unwrap_or_default(),
                "release signing refused"
            );
            let _ = tx.send(Ok(Bytes::from(sealer.seal_refused(refusal.as_str())))).await;
            return;
        }
    };
    let sha256_out = sign::sha256_hex(&signed.archive);
    tracing::info!(
        target: "ducktape::airlock",
        event = "release_sign_signed",
        sub = %audit.sub,
        session = %audit.session,
        sha256_in = %audit.sha256_in,
        sha256_out = %sha256_out,
        team_id = %audit.team_id,
        "release bundle signed"
    );
    tracing::info!(
        target: "ducktape::airlock",
        event = "release_sign_notarized",
        sub = %audit.sub,
        session = %audit.session,
        sha256_out = %sha256_out,
        submission_id = %signed.submission_id.clone().unwrap_or_default(),
        "release bundle notarized and stapled"
    );
    for chunk in sign::response_chunks(&signed.archive) {
        if tx.send(Ok(Bytes::from(sealer.seal_chunk(chunk)))).await.is_err() {
            return;
        }
    }
    let _ = tx.send(Ok(Bytes::from(sealer.seal_final()))).await;
}

/// Map the caller's request path onto the vendor upstream. Anthropic serves the
/// `/v1/...` shape the caller sends, so it passes through. The ChatGPT Codex
/// backend serves `/responses` under `/backend-api/codex` (no `/v1`), but codex
/// posts to its `.../v1` broker base so the path arrives as `/v1/responses` —
/// stripping the `/v1` prefix lands it on `.../codex/responses` instead of a
/// 404'd `.../codex/v1/responses`.
fn upstream_path(vendor: ModelVendor, caller_path: &str) -> &str {
    match vendor {
        ModelVendor::Claude => caller_path,
        ModelVendor::Codex => caller_path.strip_prefix("/v1").unwrap_or(caller_path),
    }
}

/// The session a bearer names, resolved to its claims and credential. Shared
/// by the model proxy and the signing route: everything here happens before
/// a body is opened or a budget spent, so a refusal costs the caller nothing.
///
/// The store gets first refusal here as it does at `/session`, and for the
/// same reason: a token minted an hour ago says nothing about whether the
/// credential still exists. Checking only at session open let `user cred
/// remove` be followed by a whole TTL of spending on the deleted credential.
/// One stat per request, the same one the session path pays.
fn resolve_session(st: &AppState, headers: &HeaderMap) -> Result<(Claims, Arc<CredEntry>), AppErr> {
    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or_else(|| AppErr(StatusCode::UNAUTHORIZED, "missing bearer token".into()))?;

    let claims = token::verify(&st.sess_pk, bearer)
        .map_err(|e| AppErr(StatusCode::UNAUTHORIZED, format!("bad session token: {e}")))?;

    let now = now_secs();
    if claims.exp < now {
        return Err(AppErr(StatusCode::UNAUTHORIZED, "session token expired".into()));
    }
    refresh_credential(st, &claims.sub);
    let entry = st
        .creds
        .lock()
        .unwrap()
        .get(&claims.sub)
        .cloned()
        .ok_or_else(|| AppErr(StatusCode::NOT_FOUND, "credential_not_found".into()))?;
    Ok((claims, entry))
}

/// The handshake keys of a sealed session, re-derived statelessly from the
/// claims' ephemeral pk; `None` on a plaintext session.
fn session_keys(st: &AppState, claims: &Claims) -> Result<Option<handshake::SessionKeys>, AppErr> {
    if !claims.seal {
        return Ok(None);
    }
    let eph = BASE64
        .decode(&claims.eph)
        .ok()
        .and_then(|v| <[u8; 32]>::try_from(v).ok())
        .ok_or_else(|| AppErr(StatusCode::UNAUTHORIZED, "bad eph in claims".into()))?;
    Ok(Some(handshake::enclave_session_keys(&st.seal_kp, &eph)))
}

/// A request body the session admitted: opened if the session is sealed,
/// as sent if not, with the budget spent and the nonce recorded.
struct AdmittedBody {
    /// The plaintext body.
    body: Bytes,
    /// The sealed request's nonce, which the response stream binds to.
    binding: Vec<u8>,
}

/// Open one request body under the session's rules and charge it to the
/// session — the three admission steps every credential-spending route
/// shares, in the one order that keeps each free of the others' abuse:
///
/// 1. Sealed-body session (`seal_keys` from [`session_keys`]): unseal the
///    request, and REFUSE plaintext — a stolen bearer alone (visible to path
///    hosts) cannot produce a sealable body. A plaintext session must not
///    send the seal header. The raw blob's nonce becomes (a) the per-sub
///    replay-dedup key and (b) the binding the response stream key is
///    derived under, so an authentic response cannot be replayed as the
///    answer to a different request.
/// 2. Budget spends only AFTER the sealed body validated — a path host
///    feeding garbage blobs must not burn the session's requests.
/// 3. Replay dedupe LAST: the AEAD proved the blob is this session's request,
///    and the spend paid for it, so one entry here always costs one request
///    of the budget. Recording the nonce first made an unauthenticated
///    12-byte body a free, permanent allocation for any bearer holder.
fn admit_body(
    st: &AppState,
    claims: &Claims,
    seal_keys: Option<&handshake::SessionKeys>,
    method: &Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<AdmittedBody, AppErr> {
    let sealed_request = headers
        .get(bodyseal::SEAL_HEADER)
        .and_then(|v| v.to_str().ok())
        == Some(bodyseal::SEAL_V1);
    let binding = bodyseal::request_binding(&body);
    let body = match (seal_keys, sealed_request) {
        (Some(keys), true) => Bytes::from(
            bodyseal::open_request(keys, &bodyseal::request_aad(method.as_str(), path_and_query), &body)
                .map_err(|e| AppErr(StatusCode::BAD_REQUEST, format!("airlock: {e}")))?,
        ),
        // A sealed session requires a sealed body on EVERY request, bodyless
        // ones included: `bodyseal::seal_request` seals an empty plaintext
        // into a non-empty AEAD blob (the tag alone), so a bodyless request
        // still costs the caller possession of the handshake keys. Without
        // this arm, a stolen bearer with no key material could still spend
        // the owner's credential on every bodyless request shape (GET/HEAD/
        // DELETE across the whole proxied surface) for the rest of the
        // token's TTL — the residual `bodyseal.rs` used to document.
        (Some(_), false) => {
            return Err(AppErr(
                StatusCode::BAD_REQUEST,
                "airlock: sealed session requires a sealed body".into(),
            ));
        }
        (None, true) => {
            return Err(AppErr(
                StatusCode::BAD_REQUEST,
                "airlock: session was not opened for sealed bodies".into(),
            ));
        }
        (None, false) => body,
    };

    {
        let mut b = st.budgets.lock().unwrap();
        let rem = b
            .get_mut(&claims.sub)
            .ok_or_else(|| AppErr(StatusCode::FORBIDDEN, "no budget for sub".into()))?;
        // A spent budget ENDS the session, exactly as its TTL lapsing does, and
        // the remedy is identical: open a new one. So it answers 401 like the
        // expiry above, not 429.
        //
        // 429 said "back off and retry later", which is false — waiting never
        // refills this session, the budget is per-session and only a new
        // handshake resets it. Worse, the caller cannot tell that 429 from the
        // VENDOR's rate-limit 429, which is relayed from upstream and MUST pass
        // through untouched. So the broker's re-handshake-on-401 never fired
        // here and a borrowed credential simply died at `max_requests`, with the
        // sandbox seeing a rate limit that would never clear.
        if *rem == 0 {
            return Err(AppErr(
                StatusCode::UNAUTHORIZED,
                "session budget spent".into(),
            ));
        }
        *rem -= 1;
    }

    // This session's entry only ever grows while THIS bearer (this `sub`+`eph`
    // pair) is spending it, and only a credential removal or process restart
    // clears it.
    if sealed_request {
        let fresh = st
            .seen_nonces
            .lock()
            .unwrap()
            .entry((claims.sub.clone(), claims.eph.clone()))
            .or_default()
            .insert(binding.clone());
        if !fresh {
            return Err(AppErr(
                StatusCode::BAD_REQUEST,
                "airlock: replayed sealed request".into(),
            ));
        }
    }
    Ok(AdmittedBody { body, binding })
}

async fn proxy_inner(
    st: &AppState,
    method: Method,
    uri: &axum::http::Uri,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, AppErr> {
    let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or(uri.path());
    // The session's `sub` names the credential it draws on; resolve it now — its
    // kind selects the upstream and its own token state is what we refresh/spend.
    let (claims, entry) = resolve_session(st, headers)?;
    // Only a model credential has an upstream here. Decided before the body is
    // opened or the budget spent: a signing identity pointed at `/v1/*` is
    // refused by name, and costs the session nothing.
    let (vendor, oauth) = entry.oauth()?;
    let seal_keys = session_keys(st, &claims)?;
    let AdmittedBody { body, binding } = admit_body(
        st,
        &claims,
        seal_keys.as_ref(),
        &method,
        path_and_query,
        headers,
        body,
    )?;
    let now = now_secs();

    let stale = {
        let o = oauth.oauth.lock().unwrap();
        o.access_token.is_empty() || o.expires_at <= now
    };
    if stale {
        refresh_now(&st.cfg, &st.http, oauth)
            .await
            .map_err(|e| AppErr(StatusCode::BAD_GATEWAY, format!("refresh: {e}")))?;
    }
    let access = {
        let o = oauth.oauth.lock().unwrap();
        if o.access_token.is_empty() {
            return Err(AppErr(
                StatusCode::BAD_GATEWAY,
                "no credential loaded".into(),
            ));
        }
        o.access_token.clone()
    };

    // Upstream base is the credential's vendor endpoint; the caller's path maps
    // onto it per vendor (see `upstream_path`).
    let upstream_base = match vendor {
        ModelVendor::Claude => &st.cfg.anthropic_base,
        ModelVendor::Codex => &st.cfg.openai_base,
    };
    let url = format!(
        "{}{}",
        upstream_base.trim_end_matches('/'),
        upstream_path(vendor, path_and_query)
    );
    let mut rb = st.http.request(method, &url).body(body.to_vec());
    // Forward the caller's headers verbatim, minus ones we own or that would
    // break the relay. bearer_auth then plants the real credential.
    for (name, value) in headers.iter() {
        if matches!(
            name.as_str(),
            "authorization"
                | "x-api-key"
                | "chatgpt-account-id"
                | "host"
                | "content-length"
                | "accept-encoding"
                | "x-airlock-body-seal"
        ) {
            continue;
        }
        rb = rb.header(name, value);
    }
    // Account routing belongs to the credential holder, never the child or
    // the broker (Pi supplies a deliberately fictitious account in its token).
    let account_id = match vendor {
        ModelVendor::Claude => None,
        ModelVendor::Codex => codex_account_id(&access),
    };
    if let Some(account_id) = account_id {
        rb = rb.header("chatgpt-account-id", account_id);
    }
    let resp = rb
        .bearer_auth(&access)
        .send()
        .await
        .map_err(|e| AppErr(StatusCode::BAD_GATEWAY, format!("upstream: {e}")))?;

    let status = resp.status();
    let ct = resp.headers().get("content-type").cloned();
    let Some(keys) = seal_keys else {
        // Plaintext session: passthrough, streamed.
        let mut builder = Response::builder().status(status.as_u16());
        if let Some(v) = ct {
            builder = builder.header("content-type", v);
        }
        return builder
            .body(Body::from_stream(resp.bytes_stream()))
            .map_err(|e| AppErr(StatusCode::INTERNAL_SERVER_ERROR, format!("build response: {e}")));
    };

    // Sealed session: re-seal the upstream stream chunk by chunk. The inner
    // content-type rides the sealed head chunk; the outer body is opaque. An
    // upstream error mid-stream ends WITHOUT the final marker — the broker
    // sees authenticated truncation, never a silent clean EOF.
    let inner_ct = ct
        .as_ref()
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let (mut sealer, salt) = bodyseal::StreamSealer::new(&keys, &binding);
    let head_chunk = sealer.seal_head(&inner_ct);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    tokio::spawn(async move {
        use futures_util::StreamExt as _;
        let mut opening = salt;
        opening.extend(head_chunk);
        if tx.send(Ok(Bytes::from(opening))).await.is_err() {
            return;
        }
        let mut upstream = resp.bytes_stream();
        while let Some(chunk) = upstream.next().await {
            match chunk {
                Ok(chunk) => {
                    if tx.send(Ok(Bytes::from(sealer.seal_chunk(&chunk)))).await.is_err() {
                        return;
                    }
                }
                Err(_) => return, // no Final marker -> authenticated truncation
            }
        }
        let _ = tx.send(Ok(Bytes::from(sealer.seal_final()))).await;
    });
    Response::builder()
        .status(status.as_u16())
        .header("content-type", "application/octet-stream")
        .body(Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)))
        .map_err(|e| AppErr(StatusCode::INTERNAL_SERVER_ERROR, format!("build response: {e}")))
}

/// Read account routing metadata only from the credential we hold. This does
/// not authenticate a JWT: the vendor validates the bearer. An opaque bearer
/// carries no account metadata and must never inherit a caller's account.
fn codex_account_id(access: &str) -> Option<String> {
    let mut parts = access.split('.');
    let (_header, payload, _signature) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let account_id = claims["https://api.openai.com/auth"]["chatgpt_account_id"].as_str()?;
    let usable_account = !account_id.is_empty() && HeaderValue::from_str(account_id).is_ok();
    usable_account.then(|| account_id.to_owned())
}

/// Exchange one credential's refresh token for a fresh access token (and rotated
/// refresh token), single-flighted per credential so concurrent callers never
/// double-spend it.
async fn refresh_now(cfg: &Config, http: &reqwest::Client, entry: &OauthState) -> Result<()> {
    let _gate = entry.refresh_gate.lock().await;
    // Re-check under the gate — a caller we queued behind may have just done it.
    let refresh = {
        let o = entry.oauth.lock().unwrap();
        if !o.access_token.is_empty() && o.expires_at > now_secs() {
            return Ok(());
        }
        o.refresh_token.clone()
    };

    let resp = http
        .post(&cfg.oauth_token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.as_str()),
            ("client_id", cfg.oauth_client_id.as_str()),
        ])
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        bail!("oauth token endpoint {status}: {text}");
    }
    let j: serde_json::Value = serde_json::from_str(&text).context("oauth response json")?;
    let access = j["access_token"].as_str().context("no access_token")?.to_string();
    let new_refresh = j["refresh_token"].as_str().map(|s| s.to_string());
    let expires_in = j["expires_in"].as_u64().unwrap_or(3600);

    let now = now_secs();
    let mut o = entry.oauth.lock().unwrap();
    o.access_token = access;
    if let Some(r) = new_refresh {
        o.refresh_token = r; // memory-only; lost on restart, re-seal to recover
    }
    o.expires_at = now + expires_in.saturating_sub(60);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_account_metadata_comes_only_from_a_well_formed_credential() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let payload = URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"real-account"}}"#);
        assert_eq!(
            codex_account_id(&format!("e30.{payload}.sig")).as_deref(),
            Some("real-account")
        );
        for invalid in [
            "opaque-bearer",
            "e30.bad.sig",
            "e30.e30.sig",
            "e30.e30.sig.extra",
        ] {
            assert_eq!(codex_account_id(invalid), None);
        }
        for account in ["", "bad\naccount"] {
            let payload = URL_SAFE_NO_PAD.encode(
                serde_json::json!({"https://api.openai.com/auth": {"chatgpt_account_id": account}})
                    .to_string(),
            );
            assert_eq!(codex_account_id(&format!("e30.{payload}.sig")), None);
        }
    }

    #[test]
    fn codex_upstream_path_strips_v1_but_claude_passes_through() {
        // Codex: `.../backend-api/codex` + this path must be `/responses`, not
        // `/v1/responses` (the 404 the ChatGPT backend returns otherwise).
        assert_eq!(
            upstream_path(ModelVendor::Codex, "/v1/responses"),
            "/responses"
        );
        assert_eq!(
            upstream_path(ModelVendor::Codex, "/v1/responses?stream=true"),
            "/responses?stream=true"
        );
        // Claude: `api.anthropic.com` + `/v1/messages` is correct — pass through.
        assert_eq!(
            upstream_path(ModelVendor::Claude, "/v1/messages"),
            "/v1/messages"
        );
        // A codex path already without `/v1` is left alone.
        assert_eq!(
            upstream_path(ModelVendor::Codex, "/responses"),
            "/responses"
        );
    }

    /// A gateway with one seeded bearer credential and an upstream that cannot
    /// be reached: every admission decision this module owns happens before the
    /// proxied call, so a dead upstream is the cheapest way to observe them.
    fn test_state(name: &str, max_requests: u32) -> Arc<AppState> {
        let seal_kp = SealKeypair::generate();
        let sess_sk = SigningKey::generate(&mut OsRng);
        let sess_pk = sess_sk.verifying_key();
        let entry = cred_entry(
            CredentialKind::Claude,
            CredentialPayload::Bearer { access_token: "tok".into() },
        )
        .unwrap();
        Arc::new(AppState {
            seal_kp,
            sess_sk,
            sess_pk,
            quote: Vec::new(),
            vendor: "self-host".into(),
            http: reqwest::Client::new(),
            cfg: Config {
                // port 1 is never listening: connect is refused at once, so the
                // proxied call fails without waiting on anything.
                anthropic_base: "http://127.0.0.1:1".into(),
                openai_base: String::new(),
                oauth_token_url: String::new(),
                oauth_client_id: String::new(),
                session_ttl_secs: 3600,
                max_requests,
            },
            creds: Mutex::new(HashMap::from([(name.to_string(), Arc::new(entry))])),
            budgets: Mutex::new(HashMap::new()),
            seen_nonces: Mutex::new(HashMap::new()),
            grant_check: None,
            reload: None,
            sign: None,
        })
    }

    #[tokio::test]
    async fn gateway_replaces_caller_account_headers_with_credential_metadata() {
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
        let upstream = Router::new().route(
            "/responses",
            post(move |headers: HeaderMap| {
                let sent = sent.clone();
                async move {
                    sent.send(headers).unwrap();
                    "ok"
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"real-account"}}"#);
        for access in [format!("e30.{payload}.sig"), "opaque-bearer".into()] {
            let mut st = test_state("a", 8);
            Arc::get_mut(&mut st).unwrap().cfg.openai_base = url.clone();
            st.creds.lock().unwrap().insert(
                "a".into(),
                Arc::new(
                    cred_entry(
                        CredentialKind::Codex,
                        CredentialPayload::Bearer {
                            access_token: access.clone(),
                        },
                    )
                    .unwrap(),
                ),
            );
            let (eph, keys) = handshake::client_handshake(&st.seal_kp.public_bytes());
            let eph = BASE64.encode(eph);
            let _session = session(
                State(st.clone()),
                HeaderMap::new(),
                Json(SessionRequest {
                    sub: "a".into(),
                    client_eph_pk_b64: eph.clone(),
                    body_seal: true,
                    work: WorkRef::Direct,
                }),
            )
            .await
            .unwrap_or_else(|error| panic!("session: {}", error.1));
            let token = token::issue(
                &st.sess_sk,
                &Claims {
                    sub: "a".into(),
                    iat: now_secs(),
                    exp: now_secs() + 3600,
                    eph,
                    seal: true,
                },
            );
            let mut headers = sealed_headers(&token);
            headers.append("chatgpt-account-id", "dummy-pi-account".parse().unwrap());
            headers.append("chatgpt-account-id", "attacker-account".parse().unwrap());
            headers.insert("x-api-key", "attacker-key".parse().unwrap());
            let aad = bodyseal::request_aad("POST", "/v1/responses");
            let body = bodyseal::seal_request(&keys, &aad, b"{}");
            let response = proxy_inner(
                &st,
                Method::POST,
                &"/v1/responses".parse().unwrap(),
                &headers,
                Bytes::from(body),
            )
            .await
            .unwrap_or_else(|error| panic!("proxy: {}", error.1));
            assert_eq!(response.status(), StatusCode::OK);
            // Drain the sealed response to synchronize with the upstream task.
            axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            let headers = received.recv().await.unwrap();
            assert_eq!(headers["authorization"], format!("Bearer {access}"));
            assert!(!headers.contains_key("x-api-key"));
            let accounts: Vec<_> = headers.get_all("chatgpt-account-id").iter().collect();
            match codex_account_id(&access) {
                Some(account) => assert_eq!(accounts, vec![account.as_str()]),
                None => assert!(accounts.is_empty()),
            }
        }
        task.abort();
    }

    fn recorded_nonces(st: &AppState, sub: &str, eph_b64: &str) -> usize {
        st.seen_nonces
            .lock()
            .unwrap()
            .get(&(sub.to_string(), eph_b64.to_string()))
            .map_or(0, |set| set.len())
    }

    fn sealed_headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
        headers.insert(bodyseal::SEAL_HEADER, bodyseal::SEAL_V1.parse().unwrap());
        headers
    }

    async fn post_sealed(st: &Arc<AppState>, token: &str, body: Vec<u8>) -> StatusCode {
        let uri: axum::http::Uri = "/v1/messages".parse().unwrap();
        proxy_inner(st, Method::POST, &uri, &sealed_headers(token), Bytes::from(body))
            .await
            .expect_err("the upstream is unreachable, so every call ends in an error")
            .0
    }

    /// The replay set costs a request of the budget to grow and nothing else:
    /// a body the AEAD refuses records NOTHING (that write was free for anyone
    /// holding a bearer, and permanent). Reopening the SAME session (same
    /// `eph`) must NOT clear it — the whole point of this set is that a
    /// bearer's own already-spent nonces stay recorded for as long as that
    /// bearer is live.
    #[tokio::test]
    async fn the_replay_set_only_grows_with_authenticated_spent_requests() {
        let st = test_state("a", 8);
        let (client_eph_pk, keys) = handshake::client_handshake(&st.seal_kp.public_bytes());
        let eph_b64 = BASE64.encode(client_eph_pk);
        let open = |st: &Arc<AppState>| {
            session(
                State(st.clone()),
                HeaderMap::new(),
                Json(SessionRequest {
                    sub: "a".into(),
                    client_eph_pk_b64: eph_b64.clone(),
                    body_seal: true,
                    work: WorkRef::Direct,
                }),
            )
        };
        assert!(open(&st).await.is_ok(), "a seeded credential opens a session");
        let claims = Claims {
            sub: "a".into(),
            iat: now_secs(),
            exp: now_secs() + 3600,
            eph: eph_b64.clone(),
            seal: true,
        };
        let token = token::issue(&st.sess_sk, &claims);

        // Garbage under the seal header: refused, and it leaves no trace.
        let status = post_sealed(&st, &token, vec![7u8; 32]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            recorded_nonces(&st, "a", &eph_b64),
            0,
            "an unauthenticated body must record nothing"
        );
        assert_eq!(st.budgets.lock().unwrap()["a"], 8, "and must cost nothing");

        // Authentic sealed bodies each record one nonce and spend one request.
        let aad = bodyseal::request_aad("POST", "/v1/messages");
        let blobs: Vec<Vec<u8>> =
            (0..3u8).map(|i| bodyseal::seal_request(&keys, &aad, &[i; 16])).collect();
        for blob in &blobs {
            let status = post_sealed(&st, &token, blob.clone()).await;
            assert_eq!(status, StatusCode::BAD_GATEWAY, "admitted, then the upstream is dead");
        }
        assert_eq!(recorded_nonces(&st, "a", &eph_b64), 3);
        assert_eq!(st.budgets.lock().unwrap()["a"], 5);

        // The same blob again is the replay this set exists to catch.
        let status = post_sealed(&st, &token, blobs[0].clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(recorded_nonces(&st, "a", &eph_b64), 3, "a replay adds nothing");

        // Reopening the SAME session refills the budget but must NOT clear
        // the nonces recorded under this `eph` — the original token is still
        // live and its already-spent nonces must stay blocked.
        assert!(open(&st).await.is_ok(), "the session reopens");
        assert_eq!(
            recorded_nonces(&st, "a", &eph_b64),
            3,
            "a same-session reopen must not erase this bearer's replay history"
        );
        assert_eq!(st.budgets.lock().unwrap()["a"], 8);
        let status = post_sealed(&st, &token, blobs[0].clone()).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "the original token's replay must still be refused after a reopen"
        );
    }

    /// Regression for the reported bug: a SECOND, unrelated `/session` open
    /// for the same credential (a different borrower's session, or the
    /// broker's own automatic re-handshake on a 401) must not let a captured
    /// sealed request be replayed under the FIRST session's still-live
    /// bearer.
    #[tokio::test]
    async fn a_second_session_open_does_not_revive_the_first_sessions_replays() {
        let st = test_state("a", 8);
        let (eph_a, keys_a) = handshake::client_handshake(&st.seal_kp.public_bytes());
        let eph_a_b64 = BASE64.encode(eph_a);

        // Session A opens and spends one sealed request.
        assert!(
            session(
                State(st.clone()),
                HeaderMap::new(),
                Json(SessionRequest {
                    sub: "a".into(),
                    client_eph_pk_b64: eph_a_b64.clone(),
                    body_seal: true,
                    work: WorkRef::Direct,
                }),
            )
            .await
            .is_ok(),
            "session A opens"
        );
        let claims_a = Claims {
            sub: "a".into(),
            iat: now_secs(),
            exp: now_secs() + 3600,
            eph: eph_a_b64.clone(),
            seal: true,
        };
        let token_a = token::issue(&st.sess_sk, &claims_a);
        let blob_a = bodyseal::seal_request(
            &keys_a,
            &bodyseal::request_aad("POST", "/v1/messages"),
            b"request from A",
        );
        let status = post_sealed(&st, &token_a, blob_a.clone()).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "admitted, then the upstream is dead");

        // Session B opens against the SAME credential with its own (any)
        // ephemeral key — e.g. the broker's automatic reauth on a 401.
        let (eph_b, _keys_b) = handshake::client_handshake(&st.seal_kp.public_bytes());
        assert!(
            session(
                State(st.clone()),
                HeaderMap::new(),
                Json(SessionRequest {
                    sub: "a".into(),
                    client_eph_pk_b64: BASE64.encode(eph_b),
                    body_seal: true,
                    work: WorkRef::Direct,
                }),
            )
            .await
            .is_ok(),
            "session B opens"
        );

        // Replaying A's already-spent blob under A's still-valid bearer must
        // still be refused as a replay, not admitted (BAD_GATEWAY).
        let status = post_sealed(&st, &token_a, blob_a).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "session B's open must not have wiped session A's replay set"
        );
    }

    /// an upload only creates: a name the store already holds is refused by
    /// name, and what it holds stays exactly what it was. A new name lands.
    #[tokio::test]
    async fn an_upload_under_a_held_name_is_refused_and_changes_nothing() {
        let st = test_state("a", 8);
        let held = st.creds.lock().unwrap().get("a").cloned().unwrap();
        let upload = |name: &str| {
            let pt = serde_json::to_vec(&CredentialPayload::Bearer {
                access_token: "other".into(),
            })
            .unwrap();
            credential(
                State(st.clone()),
                Json(CredentialUpload {
                    name: name.into(),
                    kind: CredentialKind::Claude,
                    sealed_b64: BASE64.encode(seal::seal(&st.seal_kp.public_bytes(), &pt)),
                }),
            )
        };

        let AppErr(status, message) = upload("a").await.unwrap_err();
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(message.starts_with("credential_exists"), "{message}");
        let now = st.creds.lock().unwrap().get("a").cloned().unwrap();
        assert!(Arc::ptr_eq(&held, &now), "the held credential is untouched");

        assert_eq!(upload("b").await.unwrap(), StatusCode::OK);
        assert!(st.creds.lock().unwrap().contains_key("b"));
    }

    // -------- apple-codesign admission through the upload door --------

    /// Post one sealed `apple-codesign` upload straight into the handler, as
    /// the attested build serves it.
    #[cfg(feature = "testkit")]
    async fn upload_apple(
        st: &Arc<AppState>,
        name: &str,
        payload: CredentialPayload,
    ) -> Result<StatusCode, AppErr> {
        let pt = serde_json::to_vec(&payload).unwrap();
        let sealed = seal::seal(&st.seal_kp.public_bytes(), &pt);
        credential(
            State(st.clone()),
            Json(CredentialUpload {
                name: name.into(),
                kind: CredentialKind::AppleCodesign,
                sealed_b64: BASE64.encode(sealed),
            }),
        )
        .await
    }

    #[cfg(feature = "testkit")]
    fn apple_payload(p12_b64: String, api_key_json: String, team_id: &str) -> CredentialPayload {
        CredentialPayload::AppleCodesign {
            p12_b64,
            p12_password: codesign::fixture::P12_PASSWORD.into(),
            api_key_json,
            team_id: team_id.into(),
        }
    }

    #[cfg(feature = "testkit")]
    #[tokio::test]
    async fn a_valid_apple_codesign_upload_is_stored_under_its_kind() {
        use codesign::fixture::{self, Marker};
        let st = test_state("a", 8);
        let payload = apple_payload(
            fixture::p12_b64(fixture::TEAM_ID, Marker::DeveloperIdApplication),
            fixture::api_key_json(),
            fixture::TEAM_ID,
        );
        assert_eq!(
            upload_apple(&st, "release-sign", payload).await.unwrap(),
            StatusCode::OK
        );
        let entry = st
            .creds
            .lock()
            .unwrap()
            .get("release-sign")
            .cloned()
            .expect("stored");
        assert_eq!(entry.kind, CredentialKind::AppleCodesign);
        assert!(matches!(entry.material, CredMaterial::AppleCodesign(_)));
    }

    #[cfg(feature = "testkit")]
    #[tokio::test]
    async fn each_apple_codesign_refusal_answers_with_its_token_and_stores_nothing() {
        use codesign::fixture::{self, Marker};
        let st = test_state("a", 8);
        let good_p12 = fixture::p12_b64(fixture::TEAM_ID, Marker::DeveloperIdApplication);
        let cases = [
            (
                apple_payload(
                    BASE64.encode(b"junk"),
                    fixture::api_key_json(),
                    fixture::TEAM_ID,
                ),
                "p12_unparseable",
            ),
            (
                apple_payload(
                    fixture::p12_b64(fixture::TEAM_ID, Marker::None),
                    fixture::api_key_json(),
                    fixture::TEAM_ID,
                ),
                "not_developer_id_application",
            ),
            (
                apple_payload(good_p12.clone(), fixture::api_key_json(), "OTHER00000"),
                "team_id_mismatch",
            ),
            (
                apple_payload(good_p12.clone(), "{}".into(), fixture::TEAM_ID),
                "api_key_malformed",
            ),
            // a model token under the signing kind, and the signing identity
            // under a model kind: the kind must agree with the payload.
            (
                CredentialPayload::Bearer {
                    access_token: "tok".into(),
                },
                "credential_kind_mismatch",
            ),
        ];
        for (payload, want) in cases {
            let AppErr(status, body) = upload_apple(&st, "release-sign", payload)
                .await
                .expect_err(want);
            assert_eq!(status, StatusCode::BAD_REQUEST, "{want}");
            assert_eq!(body, want);
            assert!(
                !st.creds.lock().unwrap().contains_key("release-sign"),
                "{want}: stored"
            );
        }
        let under_model_kind = cred_entry(
            CredentialKind::Claude,
            apple_payload(good_p12, fixture::api_key_json(), fixture::TEAM_ID),
        );
        assert_eq!(
            under_model_kind.err().map(|e| e.to_string()).as_deref(),
            Some("credential_kind_mismatch")
        );
    }

    /// A session on a signing identity opens (the grant model is the same),
    /// but the model proxy refuses it by name before opening the body or
    /// spending the budget.
    #[cfg(feature = "testkit")]
    #[tokio::test]
    async fn the_model_proxy_refuses_an_apple_codesign_session_by_kind() {
        use codesign::fixture::{self, Marker};
        let st = test_state("a", 8);
        let payload = apple_payload(
            fixture::p12_b64(fixture::TEAM_ID, Marker::DeveloperIdApplication),
            fixture::api_key_json(),
            fixture::TEAM_ID,
        );
        upload_apple(&st, "release-sign", payload).await.unwrap();
        let (client_eph_pk, _keys) = handshake::client_handshake(&st.seal_kp.public_bytes());
        let eph_b64 = BASE64.encode(client_eph_pk);
        let opened = session(
            State(st.clone()),
            HeaderMap::new(),
            Json(SessionRequest {
                sub: "release-sign".into(),
                client_eph_pk_b64: eph_b64.clone(),
                body_seal: true,
                work: WorkRef::Direct,
            }),
        )
        .await;
        assert!(
            opened.is_ok(),
            "a signing credential opens a session like any other"
        );
        let claims = Claims {
            sub: "release-sign".into(),
            iat: now_secs(),
            exp: now_secs() + 3600,
            eph: eph_b64.clone(),
            seal: true,
        };
        let token = token::issue(&st.sess_sk, &claims);
        let uri: axum::http::Uri = "/v1/messages".parse().unwrap();
        let AppErr(status, body) = proxy_inner(
            &st,
            Method::POST,
            &uri,
            &sealed_headers(&token),
            Bytes::from(vec![7u8; 32]),
        )
        .await
        .expect_err("refused");
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, "credential_kind_mismatch");
        assert_eq!(
            st.budgets.lock().unwrap().get("release-sign").copied(),
            Some(8),
            "the refusal spent nothing"
        );
    }
}
