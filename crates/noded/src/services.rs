//! The node-local service catalog: which offchain service daemons are
//! currently signaling presence to this node.
//!
//! A service (compute, storage, ...) has no sovereign identity — identity and
//! transport belong to the node — so it cannot place itself on chain and it
//! cannot name itself. All it may do is SIGNAL: `POST /v1/services/hello`
//! declaring its kind, version, the capability tags it offers and the grant
//! scopes it needs. The node remembers that for [`HELLO_TTL`] and nothing
//! more. Nothing here is durable, nothing here is consensus state, and an
//! entry confers no standing whatsoever — `ducktape service enable <kind>`
//! (which writes the workspace's `services.toml`) is the consent boundary.
//!
//! ## why a TTL and not a live connection
//!
//! Presence could ride an open websocket, but `stream_session` has no
//! disconnect seam — every teardown path is a bare `return` — so a connection
//! model would be the first of its kind in the daemon. A refreshed deadline is
//! the shape two existing registries already use (`gateway_ws_token`'s
//! prune-on-write, `nat_traversal::AdvertBook`'s expire-on-read), so a hello
//! that stops arriving simply ages out.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// How long a hello keeps its entry alive. A daemon re-sends well inside this;
/// one that dies leaves at most a `HELLO_TTL` ghost, which `service list`
/// reports as signaling and `service status` would then report as
/// enabled-but-absent once it truly goes.
pub const HELLO_TTL: Duration = Duration::from_secs(30);

/// the compute service: dispatch-placed headless runs.
pub const COMPUTE_KIND: &str = "compute";
/// the agent service: user-attached interactive pty sessions. Sibling to
/// compute, not a layer on it — both link the same provider/sandbox/broker
/// libraries and spawn their own sandboxes, and their bus is the chain.
pub const AGENT_KIND: &str = "agent";
/// the airlock service: the credential-LENDING gateway, serving this
/// operator's own store to accounts their on-chain grants name. Unlike the
/// other two it spawns nothing — no sandbox, no container, no provider — so a
/// node that lends credentials needs no container runtime at all.
pub const AIRLOCK_KIND: &str = "airlock";

/// the longest a kind tag may be — kinds are capability-tag shaped.
const MAX_KIND_LEN: usize = 32;
/// the longest a version string may be.
const MAX_VERSION_LEN: usize = 32;
/// the most grant scopes / declared needs one hello may carry. These are
/// small by nature: a service asks for a handful of scopes, not hundreds.
const MAX_LIST_LEN: usize = 32;
/// the longest one offered tag / requested scope may be.
const MAX_ITEM_LEN: usize = 64;

/// the body of `POST /v1/services/hello` — what a daemon declares about
/// itself. There is deliberately NO id field: a service cannot choose its own
/// identity, so the catalog keys on the declared kind alone and an instance id
/// exists only once the user grants one.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    /// the service kind tag (`compute`, `storage`, ...).
    pub kind: String,
    /// the daemon's own version. Metadata only — never part of identity.
    pub version: String,
    /// the daemon's build identity — METADATA, never a gate. Rendered beside
    /// the node's own by `ducktape service status` so an operator can see skew;
    /// see [`build_identity`] for why equality is not an admission rule.
    pub build: String,
    /// the capability tags this daemon offers to run.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// the grant scopes this daemon says it needs.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// service kinds this daemon says it wants present to be fully useful
    /// (an agent daemon wanting `compute` capacity, say).
    ///
    /// INFORMATIONAL ONLY, and deliberately so: an unmet need is rendered as a
    /// warning by `ducktape service list`/`status` and changes nothing else. No
    /// dependency graph, no startup ordering, no readiness gate, no
    /// plug-to-plug call — a service whose need is unmet still enables, still
    /// runs and still serves. That is a standing non-goal, not a gap.
    #[serde(default)]
    pub needs: Vec<String>,
}

/// This binary's build identity, as `build.rs` stamped it: the short commit,
/// plus a working-tree digest when the tree was dirty. `None` when the build
/// could not be identified at compile time (a source tarball, a vendored
/// build, any checkout without `.git`).
///
/// **Diagnostic only.** The node and a service daemon are separate processes
/// with independent restart timing, so skew is real — an operator upgrades and
/// restarts the node while yesterday's daemon is still running. Naming that
/// skew is worth doing; REFUSING on it is not, and this node does not:
///
/// - it authenticates nobody. A stamp is not a secret: it is compiled into a
///   binary any local process can read, and the caller does not even need to —
///   before this was metadata, the node handed its own stamp back in the
///   refusal body of the first wrong guess.
/// - it excludes every legitimately separately-compiled daemon by
///   construction. A third-party service would have to hardcode this
///   operator's exact commit, and on a dirty tree a `DefaultHasher` digest
///   `build.rs` itself notes is not toolchain-stable, at ITS compile time.
/// - it protected no correctness property that per-frame decoding does not.
///   Under the no-compat doctrine "speak the current protocol or be refused"
///   is enforced at each frame's boundary, where it belongs: [`Hello`],
///   `crate::stream::ClientMsg` and every `agent_service::wire` type carry
///   `deny_unknown_fields` and default nothing, so a field this build does not
///   know is refused and a field it does know cannot go missing. A
///   whole-connection stamp check refused frames a node understood perfectly
///   while catching nothing the decoder does not.
///
///   One honest qualifier: that refusal is one-directional. Daemon→node names
///   the bad field back to the sender; node→daemon drops the command and the
///   node's create then waits unanswered. The gap is documented at the drop
///   site (`bin/node/src/agent/link.rs`'s `classify`) and is skew-only — it
///   needs a node and a daemon built from different trees, which nothing here
///   owes support to.
/// - and `None` FAILING CLOSED made a git-absent build a node with no compute,
///   no agent pty and no airlock, whose only symptom was a bare 503.
///
/// It is deliberately NOT the package version: version numbering is pinned at
/// v1 permanently, so `CARGO_PKG_VERSION` is a constant that could never
/// distinguish two builds.
pub fn build_identity() -> Option<&'static str> {
    option_env!("DUCKTAPE_BUILD").filter(|id| !id.is_empty())
}

/// what a build with no identifiable stamp reports as. An honest "unknown"
/// rather than a value that pretends to name a commit.
pub const UNKNOWN_BUILD: &str = "unknown";

/// This binary's build identity, or [`UNKNOWN_BUILD`] — the rendering form.
pub fn build_identity_or_unknown() -> &'static str {
    build_identity().unwrap_or(UNKNOWN_BUILD)
}

/// The name `crates/noded/build.rs` staged this build's founding set under
/// (`modules%<checkout path>`), baked in by the same run that staged it, so
/// the set a binary resolves ([`workspace_config::modules_dir`]) is fixed at
/// link time and no later build, in this checkout or a sibling sharing the
/// target, can move it.
pub const STAGED_SET: &str = env!(
    "DUCKTAPE_STAGED_SET",
    "no staged set name: a build script compiled from another checkout's source ran for this \
     one (a shared target directory) — touch crates/noded/build.rs and rebuild"
);

/// What restages this checkout's founding set, verbatim, for the refusal below
/// to print.
///
/// The stager only runs when its own `rerun-if-changed` fires, so a build
/// that reuses a unit another run produced does not re-trigger it — the
/// `touch` is the whole remedy and it is not guessable. Deliberately NOT automated: on a
/// shared target directory the binary beside the set belongs to the last
/// builder too, so a silent reclaim would hide the same fact one layer down.
/// The refusal is the place that fact gets said out loud.
///
/// It names the BINARIES and not `-p noded`, though the stager is noded's,
/// because the stamp this is matched against is the one linked INTO a binary.
/// `cargo build -p noded` restages the set without relinking anything, so a
/// checkout whose sources moved since its binary was linked would restage
/// under a build id that binary does not carry — a remedy that prints the same
/// refusal back.
pub const RESTAGE_COMMAND: &str =
    "touch crates/noded/build.rs && cargo build -p node-bin -p noded-bin";

/// The founding set THIS binary's build staged, refusing any other.
///
/// `workspace_config::modules_dir` resolves a directory; this says whether it
/// is ours. The distinction matters because a set is restaged without
/// relinking every binary that reads it: this checkout's `cargo check -p
/// noded` after a commit rewrites the set under the same name, and a binary
/// linked before it then finds bytes a later build staged. Founding from
/// them means founding from wasm this binary was never built with — which
/// reads as a stale guest three weeks after the fact, not as a mistake anyone
/// made today.
///
/// It is a HARD refusal with no second door. `$DUCKTAPE_MODULES_DIR` is the
/// one way to name a set deliberately, it is checked first and is not subject
/// to this, and it is what an operator composing from someone else's artifacts
/// already uses.
pub fn founding_set() -> Result<std::path::PathBuf, String> {
    let dir = workspace_config::modules_dir(STAGED_SET)?;
    let named = std::env::var_os("DUCKTAPE_MODULES_DIR").is_some();
    let staged_by = workspace_config::staged_by(&dir);
    staged_set_verdict(&dir, named, staged_by.as_deref(), build_identity())?;
    Ok(dir)
}

/// Whether `dir` is a set this binary may found from. Pure, so the refusal is
/// testable without a profile directory: the inputs ARE the question.
///
/// Both sides are read through [`UNKNOWN_BUILD`] on purpose. A build with no
/// git to identify itself with — a source tarball, a vendored build, Docker
/// without `.git` — stages a set stamped `unknown` and reports itself as
/// `unknown`, so it founds from its own set exactly as it always did. Two such
/// builds sharing a target directory cannot be told apart by anything, and
/// this does not pretend otherwise: the guard separates identifiable builds,
/// and refusing every unidentifiable one would break the case
/// `no_admission_path_reads_this_node_s_build_stamp` exists to protect.
fn staged_set_verdict(
    dir: &std::path::Path,
    named_by_operator: bool,
    staged_by: Option<&str>,
    build: Option<&str>,
) -> Result<(), String> {
    let same_build = staged_by.unwrap_or(UNKNOWN_BUILD) == build.unwrap_or(UNKNOWN_BUILD);
    if named_by_operator || same_build {
        return Ok(());
    }
    Err(format!(
        "reason=foreign_founding_set {} was staged by build {}, and this binary is build {} — \
         a build restaged the set after this binary was linked, so its wasm is not what this \
         binary was built with.\n\
         \x20   restage this checkout:    {RESTAGE_COMMAND}\n\
         \x20   or name the set you mean: DUCKTAPE_MODULES_DIR=<dir>",
        dir.display(),
        staged_by.unwrap_or(UNKNOWN_BUILD),
        build.unwrap_or(UNKNOWN_BUILD),
    ))
}

/// the file a node writes its service-link secret into, next to `node.toml`.
pub const LINK_TOKEN_FILE: &str = "service-link.token";

/// the HTTP header a local service daemon presents that secret in, on the
/// routes that are its own (`crate::signed_req`'s service-link lane:
/// `POST /v1/services/hello`). The ws link carries the same secret in its
/// attach frame instead.
pub const LINK_TOKEN_HEADER: &str = "x-ducktape-service-link";

/// Mint this node's service-link secret and write it 0600 next to `node.toml`.
///
/// Holding the link means BECOMING this node's interactive plane: the holder
/// receives every `TermCreate`, lent-credential records included. Before the
/// carve nothing outside the node process could hold that, so requiring a file
/// read raises the bar from "can dial loopback" — which any local process can —
/// back to "can read the node's own workspace", which is the same bar the node
/// key already sets.
///
/// Freshly minted each boot rather than persisted: a node restart should
/// invalidate a stale holder, and the daemon re-reads the file on every attach,
/// so it costs nothing.
pub fn mint_link_token(workspace: &std::path::Path) -> Result<String, String> {
    mint_secret_file(workspace, LINK_TOKEN_FILE)
}

/// Read a node's service-link secret — what a daemon presents when it attaches.
pub fn read_link_token(workspace: &std::path::Path) -> Result<String, String> {
    read_secret_file(workspace, LINK_TOKEN_FILE)
}

/// A fresh 32-byte secret, hex — the one generator behind every workspace
/// credential. Exposed for the embedder that holds a node IN-PROCESS and so IS
/// the operator (the test harness): it has no workspace to write into, but its
/// credential must be the same unguessable 32 bytes as a real one.
pub fn new_secret() -> String {
    use rand::RngCore as _;
    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    raw.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Mint a fresh secret and write it 0600 into `dir/name`.
///
/// THE writer for every workspace credential — the service link
/// ([`LINK_TOKEN_FILE`]) and the admin namespace's operator token
/// ([`crate::admin::ADMIN_TOKEN_FILE`]). One function on purpose: the two were
/// byte-identical copies, so an fsync, an atomic mint-to-temp-then-rename, or
/// an EINTR fix would land in one and silently not the other.
///
/// Freshly minted each boot rather than persisted: a node restart must
/// invalidate a stale holder, and every reader opens the file per call, so it
/// costs nothing.
///
/// The error names the PATH, never the secret — the path already carries the
/// file name, so no second label is needed to tell the two apart.
pub(crate) fn mint_secret_file(dir: &std::path::Path, name: &str) -> Result<String, String> {
    let secret = new_secret();
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("secret dir {}: {error}", dir.display()))?;
    let path = dir.join(name);
    // create 0600 from the start — a world-readable window, however short, is
    // the whole thing these files exist to avoid.
    write_owner_only(&path, &secret)
        .map_err(|error| format!("secret file {}: {error}", path.display()))?;
    Ok(secret)
}

/// Read a secret [`mint_secret_file`] wrote. Trims, because an operator who
/// `echo`s the file into a variable picks up the newline their shell added.
pub(crate) fn read_secret_file(dir: &std::path::Path, name: &str) -> Result<String, String> {
    let path = dir.join(name);
    std::fs::read_to_string(&path)
        .map(|secret| secret.trim().to_string())
        .map_err(|error| format!("secret file {}: {error}", path.display()))
}

#[cfg(unix)]
fn write_owner_only(path: &std::path::Path, secret: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let _ = std::fs::remove_file(path);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(secret.as_bytes())
}

#[cfg(not(unix))]
fn write_owner_only(path: &std::path::Path, secret: &str) -> std::io::Result<()> {
    std::fs::write(path, secret)
}

/// Compare two secrets without leaking their common prefix through timing.
pub fn token_matches(presented: &str, expected: &str) -> bool {
    if presented.len() != expected.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in presented.bytes().zip(expected.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Why a hello was turned away. A typed reason rather than a bare string: the
/// status and the stable `reason` token are derived from the variant, so they
/// cannot drift apart and a typo cannot silently downgrade a refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelloRefusal {
    /// the hello itself is not well-formed.
    Malformed(&'static str),
}

impl HelloRefusal {
    /// the stable snake_case token — greppable, countable, never prose.
    pub fn reason(self) -> &'static str {
        match self {
            HelloRefusal::Malformed(_) => "malformed_hello",
        }
    }

    /// 400 for "you sent nonsense"; 503 for "this node cannot serve you right
    /// now".
    pub fn status(self) -> axum::http::StatusCode {
        match self {
            HelloRefusal::Malformed(_) => axum::http::StatusCode::BAD_REQUEST,
        }
    }

    /// the operator-facing sentence. It describes only what the CALLER sent or
    /// what this node's capacity is — never a fact about this node the caller
    /// did not already have.
    pub fn message(self) -> String {
        match self {
            HelloRefusal::Malformed(detail) => detail.to_string(),
        }
    }
}

/// A kind tag is lowercase alphanumeric plus `-`. This is a trust boundary:
/// the value is caller-supplied, lands in a map key, is rendered into CLI
/// output and is written into `services.toml`, so it must not carry a NUL
/// (which would make the instance-id preimage ambiguous), a path separator, or
/// terminal control bytes.
fn kind_is_well_formed(kind: &str) -> bool {
    let len_ok = !kind.is_empty() && kind.len() <= MAX_KIND_LEN;
    len_ok
        && kind
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// One offered tag / requested scope: printable ascii, bounded, no control
/// bytes (the same reasoning as the kind, minus the preimage concern).
fn item_is_well_formed(item: &str) -> bool {
    let len_ok = !item.is_empty() && item.len() <= MAX_ITEM_LEN;
    len_ok && item.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
}

impl Hello {
    /// Reject a malformed hello at the boundary, naming one stable reason.
    pub fn validate(&self) -> Result<(), HelloRefusal> {
        if !kind_is_well_formed(&self.kind) {
            return Err(HelloRefusal::Malformed(
                "kind must be 1..32 chars of [a-z0-9-]",
            ));
        }
        if self.version.len() > MAX_VERSION_LEN || !item_is_well_formed(&self.version) {
            return Err(HelloRefusal::Malformed(
                "version must be 1..32 printable ascii chars",
            ));
        }
        // the build is no longer compared, but it IS rendered — `service
        // status` prints it — so it stays a validated trust boundary: a
        // caller-supplied string reaching a terminal must carry no control
        // bytes and no unbounded length.
        //
        // Sized as an ITEM (1..64), not a version (1..32), and deliberately: a
        // stamp is `<sha>-<u64 hex>`, which reaches 57 chars under
        // `core.abbrev = 40`. A cap that refused an honest daemon's own stamp
        // would be the same fail-closed trap the build gate was.
        if !item_is_well_formed(&self.build) {
            return Err(HelloRefusal::Malformed(
                "build must be 1..64 printable ascii chars",
            ));
        }
        let lists_ok = self.scopes.len() <= MAX_LIST_LEN && self.needs.len() <= MAX_LIST_LEN;
        if !lists_ok {
            return Err(HelloRefusal::Malformed("at most 32 scopes and 32 needs"));
        }
        let items_ok = self
            .capabilities
            .iter()
            .chain(self.scopes.iter())
            .all(|item| item_is_well_formed(item));
        if !items_ok {
            return Err(HelloRefusal::Malformed(
                "each capability/scope must be 1..64 printable ascii chars",
            ));
        }
        // a need names a KIND, so it obeys the kind grammar — that is what
        // makes it comparable against the grants without any normalizing.
        if !self.needs.iter().all(|need| kind_is_well_formed(need)) {
            return Err(HelloRefusal::Malformed(
                "each need must be a service kind (1..32 chars of [a-z0-9-])",
            ));
        }
        Ok(())
    }
}

/// one live catalog row as `GET /v1/services` renders it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Signaling {
    pub kind: String,
    pub version: String,
    /// the daemon's build stamp, carried through for display. This is the
    /// diagnostic that replaced the old build gate: `service status` prints it
    /// beside the node's own, so ordinary dev-loop skew is visible instead of
    /// being a refusal.
    pub build: String,
    pub capabilities: Vec<String>,
    pub scopes: Vec<String>,
    /// the kinds this daemon declared it wants present. Display only.
    #[serde(default)]
    pub needs: Vec<String>,
}

/// the deadline-carrying catalog entry.
#[derive(Clone, Debug)]
struct Entry {
    hello: Hello,
    expires_at: Instant,
}

/// The volatile signaling catalog. `std::sync::Mutex` like every other
/// registry on the handle — each critical section is a map op with no `.await`
/// inside it.
#[derive(Clone, Default)]
pub struct ServiceCatalog(Arc<Mutex<HashMap<String, Entry>>>);

impl ServiceCatalog {
    /// Record (or refresh) a daemon's presence. Returns the TTL the caller
    /// must re-signal within. Full catalog + a NEW kind is refused: refreshing
    /// an existing kind must never fail on capacity, or a full map would
    /// starve the daemons already in it.
    pub fn hello(&self, hello: Hello, now: Instant) -> Result<Duration, HelloRefusal> {
        self.admit(hello, now).inspect_err(|refusal| {
            tracing::warn!(
                target: "ducktape::service",
                reason = refusal.reason(),
                "service hello refused"
            );
        })
    }

    // NOTE: admission consults [`build_identity`] nowhere, deliberately. A
    // build stamp is not a credential and equality was never a correctness
    // rule; making it one turned every git-absent build into a node that
    // refused all three service planes. Skew is a DIAGNOSTIC now — the hello
    // OK body carries this node's build so a daemon can name its own.
    fn admit(&self, hello: Hello, now: Instant) -> Result<Duration, HelloRefusal> {
        hello.validate()?;
        let mut entries = self.0.lock().expect("service catalog lock poisoned");
        expire(&mut entries, now);
        let kind = hello.kind.clone();
        let known = entries.contains_key(&kind);
        entries.insert(
            kind.clone(),
            Entry {
                hello,
                expires_at: now + HELLO_TTL,
            },
        );
        // a kind ENTERING the catalog is a once-per-daemon-session lifecycle
        // fact; the refreshes that follow are per-request and must not evict
        // the ring, so they stay at debug.
        match known {
            true => {
                tracing::debug!(target: "ducktape::service", %kind, "service hello refreshed")
            }
            false => {
                tracing::info!(target: "ducktape::service", %kind, "service signaling")
            }
        }
        Ok(HELLO_TTL)
    }

    /// The live entries, kind-sorted so the CLI's output is stable.
    pub fn live(&self, now: Instant) -> Vec<Signaling> {
        let mut entries = self.0.lock().expect("service catalog lock poisoned");
        expire(&mut entries, now);
        let mut live: Vec<Signaling> = entries
            .values()
            .map(|entry| Signaling {
                kind: entry.hello.kind.clone(),
                version: entry.hello.version.clone(),
                build: entry.hello.build.clone(),
                capabilities: entry.hello.capabilities.clone(),
                scopes: entry.hello.scopes.clone(),
                needs: entry.hello.needs.clone(),
            })
            .collect();
        live.sort_by(|a, b| a.kind.cmp(&b.kind));
        live
    }
}

/// Drop every entry whose deadline has passed. Called on both paths, so the
/// catalog needs no sweeper task: an expired entry is unobservable.
fn expire(entries: &mut HashMap<String, Entry>, now: Instant) {
    entries.retain(|kind, entry| {
        let live = entry.expires_at > now;
        if !live {
            tracing::info!(target: "ducktape::service", %kind, reason = "hello_expired", "service gone");
        }
        live
    });
}

// AUTH: a hello takes this node's SERVICE-LINK token, on the signed-write
// table's service-link lane (`crate::signed_req`), and the handler below reads
// nothing more: the lane is the whole gate. An entry grants nothing by itself
// (volatile presence that ages out on its own TTL; consent happens in
// `ducktape service enable`), but it lands in the catalog `service list` puts
// in front of the operator to enable from, so "can dial the port" — any host
// that reaches `http_listen` — must not be able to occupy a kind in it. The
// daemon already holds the token: it reads the same 0600 file beside
// `node.toml` to take its ws link. The browser `origin_guard` + CORS allowlist
// still run in front of it.

/// POST /v1/services/hello — a local service daemon declares (or refreshes)
/// its presence. Returns the TTL it must re-signal within, and this node's own
/// build so the daemon can name any skew between them.
///
/// The build rides the OK body and never a refusal body, and a 200 is not what
/// authenticates the caller: the service-link lane did that before this ran
/// (see the AUTH note above). A stamp is compiled into a binary any local
/// process can already read, so answering it to a daemon that holds the token
/// closes nothing and opens nothing.
///
/// The reason is that a body must answer the request it is on. A refusal
/// describes what the CALLER sent or what this node's capacity is, and adding
/// an unrelated fact about this node to it is noise on the one response a
/// confused caller has to read. The OK body pairs the stamp with the hello it
/// answers, which is the pairing that makes it a diagnostic at all.
///
/// That diagnostic is honest-daemon-only, by construction: a daemon that wanted
/// to look in step could read this stamp from its own OK body and echo it back
/// as its `build` on the next hello, and `service status` would render a clean
/// match. Nothing here is evidence, and no decision reads it.
pub async fn hello(
    axum::extract::State(handle): axum::extract::State<crate::handle::NodeHandle>,
    axum::Json(body): axum::Json<Hello>,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    match handle.services().hello(body, Instant::now()) {
        Ok(ttl) => (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({
                "ttl_secs": ttl.as_secs(),
                "build": build_identity_or_unknown(),
            })),
        )
            .into_response(),
        Err(refusal) => (
            refusal.status(),
            axum::Json(serde_json::json!({
                "error": refusal.message(),
                "reason": refusal.reason(),
            })),
        )
            .into_response(),
    }
}

/// GET /v1/services — the services currently signaling to this node. Says
/// nothing about what is ENABLED: that lives in the workspace's
/// `services.toml`, which the CLI reads directly off disk.
pub async fn list(
    axum::extract::State(handle): axum::extract::State<crate::handle::NodeHandle>,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    let signaling = handle.services().live(Instant::now());
    (
        axum::http::StatusCode::OK,
        // the NODE's own stamp, beside the daemons it is describing. `service
        // status` renders "(this node: …)" from this field and must not read
        // its own [`build_identity_or_unknown`]: that constant belongs to
        // whichever binary the operator happened to type, which is routinely
        // not the one running the node — it named a build the node was not
        // running and called an in-step daemon skewed.
        axum::Json(serde_json::json!({
            "signaling": signaling,
            "build": build_identity_or_unknown(),
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A set a DIFFERENT build staged is refused, with both stamps in the
    /// message so the remedy is obvious, and an operator naming a directory is
    /// not subject to it. There is no third answer: a set that cannot say who
    /// staged it is not this binary's either, which is what stops a set left
    /// behind by a deleted worktree from being adopted in silence.
    #[test]
    fn a_set_another_build_staged_is_refused() {
        let dir = std::path::Path::new("/t/modules%elsewhere");
        assert!(staged_set_verdict(dir, false, Some("abc1234"), Some("abc1234")).is_ok());

        let refused = staged_set_verdict(dir, false, Some("abc1234"), Some("def5678"))
            .expect_err("a stranger's set");
        assert!(refused.contains("reason=foreign_founding_set"), "{refused}");
        assert!(
            refused.contains("abc1234") && refused.contains("def5678"),
            "{refused}"
        );
        // the remedy is a command to run, not advice to act on: an operator
        // who has to work out that the stager needs a `touch` before it will
        // re-run has been handed a diagnosis instead of a fix.
        assert!(
            refused.contains(RESTAGE_COMMAND) && refused.contains("DUCKTAPE_MODULES_DIR="),
            "{refused}"
        );

        let unrecorded = staged_set_verdict(dir, false, None, Some("def5678"))
            .expect_err("a set with no record is not ours either");
        assert!(unrecorded.contains(UNKNOWN_BUILD), "{unrecorded}");

        // a build with no git stages `unknown` and reports `unknown`: it must
        // still found from its own set, which is the case
        // `no_admission_path_reads_this_node_s_build_stamp` guards.
        assert!(staged_set_verdict(dir, false, Some(UNKNOWN_BUILD), None).is_ok());
        assert!(staged_set_verdict(dir, false, None, None).is_ok());

        assert!(
            staged_set_verdict(dir, true, Some("abc1234"), Some("def5678")).is_ok(),
            "$DUCKTAPE_MODULES_DIR is the operator saying which set they mean"
        );
    }

    fn hello(kind: &str) -> Hello {
        Hello {
            kind: kind.into(),
            version: "1.2.3".into(),
            build: "deadbeef".into(),
            capabilities: vec!["agent.claude".into()],
            scopes: vec!["cred:read".into()],
            needs: Vec::new(),
        }
    }

    #[test]
    fn a_hello_is_live_until_its_ttl_lapses() {
        let catalog = ServiceCatalog::default();
        let start = Instant::now();
        assert_eq!(catalog.hello(hello("compute"), start).unwrap(), HELLO_TTL);

        // still inside the window: present, with the declared metadata intact.
        let live = catalog.live(start + HELLO_TTL - Duration::from_secs(1));
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].kind, "compute");
        assert_eq!(live[0].capabilities, vec!["agent.claude".to_string()]);
        assert_eq!(live[0].scopes, vec!["cred:read".to_string()]);

        // past it: gone, with no sweeper having run.
        assert!(
            catalog
                .live(start + HELLO_TTL + Duration::from_secs(1))
                .is_empty()
        );
    }

    #[test]
    fn a_refresh_extends_the_deadline_and_replaces_the_metadata() {
        let catalog = ServiceCatalog::default();
        let start = Instant::now();
        catalog.hello(hello("compute"), start).unwrap();

        let refreshed_at = start + HELLO_TTL - Duration::from_secs(1);
        let mut second = hello("compute");
        second.version = "2.0.0".into();
        catalog.hello(second, refreshed_at).unwrap();

        // the moment the FIRST hello would have died, the entry is still live.
        let live = catalog.live(start + HELLO_TTL + Duration::from_secs(1));
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].version, "2.0.0");
        // and one kind never becomes two rows.
        assert_eq!(catalog.live(refreshed_at).len(), 1);
    }

    #[test]
    fn a_malformed_hello_is_refused_at_the_boundary() {
        let catalog = ServiceCatalog::default();
        let now = Instant::now();
        for bad_kind in ["", "Compute", "com pute", "compute/../etc", "a\0b"] {
            let mut bad = hello("compute");
            bad.kind = bad_kind.into();
            assert!(
                catalog.hello(bad, now).is_err(),
                "kind {bad_kind:?} must be refused"
            );
        }
        let mut long_kind = hello("compute");
        long_kind.kind = "a".repeat(MAX_KIND_LEN + 1);
        assert!(catalog.hello(long_kind, now).is_err());

        let mut long_item = hello("compute");
        long_item.scopes = vec!["s".repeat(MAX_ITEM_LEN + 1)];
        assert!(catalog.hello(long_item, now).is_err());

        // the build is no longer compared, but it is still RENDERED, so it
        // stays a validated boundary: no control bytes, bounded length.
        for bad_build in ["", "a\0b", "esc\x1b[31m"] {
            let mut bad = hello("compute");
            bad.build = bad_build.into();
            assert!(
                catalog.hello(bad, now).is_err(),
                "build {bad_build:?} must be refused"
            );
        }
        let mut long_build = hello("compute");
        long_build.build = "b".repeat(MAX_ITEM_LEN + 1);
        assert!(catalog.hello(long_build, now).is_err());

        // nothing malformed ever landed.
        assert!(catalog.live(now).is_empty());
    }

    #[test]
    fn declared_needs_ride_the_hello_through_to_the_catalog() {
        let catalog = ServiceCatalog::default();
        let now = Instant::now();
        let mut signal = hello("agent");
        signal.needs = vec!["compute".into()];
        catalog.hello(signal, now).unwrap();
        assert_eq!(catalog.live(now)[0].needs, vec!["compute".to_string()]);

        // a need is a KIND, so a malformed one is refused like any other.
        let mut bad = hello("agent");
        bad.needs = vec!["Not A Kind".into()];
        assert!(catalog.hello(bad, now).is_err());
    }
}

/// The regression guard for the deleted build gate.
///
/// The gate refused every hello and every service link when `build_identity()`
/// was `None`, which is what a build without `.git` produces — a node with no
/// compute, no agent pty and no airlock, reporting only a bare 503. These tests
/// pin that admission never consults the stamp again.
///
/// `build_identity()` is `option_env!`, resolved at COMPILE time, so no test
/// can make it `None` at runtime. Two halves cover that between them:
///
/// - **behavioural** — the hello path here, and
///   `crate::stream`'s `a_service_link_is_granted_on_the_token_alone` for the
///   service link. These prove no stamp VALUE is ever compared, which is every
///   reintroduction except one.
/// - **source lint** — the exception: `if build_identity().is_none() { refuse }`
///   passes on a stamped build and breaks exactly the git-absent checkouts the
///   gate broke. `no_admission_path_reads_this_node_s_build_stamp` forbids the
///   stamp being NAMED, which is the only place a test can stand.
#[cfg(test)]
mod build_is_metadata_not_a_gate {
    use super::*;

    fn hello_from_build(build: &str) -> Hello {
        Hello {
            kind: "compute".into(),
            version: "1.2.3".into(),
            build: build.into(),
            capabilities: vec!["compute.small".into()],
            scopes: vec![],
            needs: vec![],
        }
    }

    #[test]
    fn a_hello_from_any_build_is_admitted_and_its_stamp_is_carried_through() {
        let catalog = ServiceCatalog::default();
        let now = Instant::now();

        // skew in either direction, and a stamp this node could never have
        // minted: all admitted. There is no equality rule left to fail.
        // the widest stamp `build.rs` can actually mint is in this list on
        // purpose: a 40-char sha under `core.abbrev = 40` plus a u64 digest is
        // 57 chars, so a cap sized for a version string would have refused an
        // honest daemon its own build — the fail-closed trap all over again.
        let widest = format!("{}-{:x}", "a".repeat(40), u64::MAX);
        for foreign in [
            "0.0.0-ancient",
            "99.99.99",
            UNKNOWN_BUILD,
            "a-third-party-daemon",
            widest.as_str(),
        ] {
            catalog
                .hello(hello_from_build(foreign), now)
                .unwrap_or_else(|refusal| {
                    panic!("build {foreign:?} must be admitted, got {refusal:?}")
                });
            let live = catalog.live(now);
            assert_eq!(live.len(), 1, "one kind is one row whatever its build");
            assert_eq!(
                live[0].build, foreign,
                "the daemon's stamp reaches the catalog as metadata"
            );
        }
    }

    #[test]
    fn no_refusal_message_leaks_this_node_s_build() {
        // a refusal body answers the request it is on: every refusal describes
        // the CALLER's input or this node's capacity, and never this node's
        // own build stamp.
        let messages = [HelloRefusal::Malformed("kind must be 1..32 chars of [a-z0-9-]").message()];
        let mine = build_identity_or_unknown();
        for message in messages {
            assert!(
                !message.contains(mine),
                "a refusal body must not publish this node's build: {message:?}"
            );
        }
    }

    /// Every way this source can name the node's own build stamp.
    ///
    /// `option_env!` written out by hand is in the list on purpose: it is the
    /// same read spelled without the helper, and a guard that only knew the
    /// helper's name would wave it through.
    const STAMP_TOKENS: [&str; 2] = ["build_identity", "DUCKTAPE_BUILD"];

    /// The source lint, at FILE granularity — because `option_env!` resolves at
    /// compile time, so no runtime test can produce the git-absent case the
    /// deleted gate broke.
    ///
    /// File granularity, and not the function-body parse this replaced, for two
    /// reasons the old shape got wrong:
    ///
    /// - **it fell open silently.** The parse counted raw `{`/`}` bytes, so a
    ///   brace inside a string literal (`"expected `}` here"`) ended the body
    ///   early and the lint read — and passed — a few lines. Matching braces in
    ///   Rust honestly means lexing raw strings and telling `'}'` from a
    ///   lifetime; that is a parser, not a guard, and a subtly-wrong one is
    ///   worse than none.
    /// - **it only looked in one frame.** A one-line wrapper around
    ///   `build_identity`, or the same check moved up to the `ServiceAttach`
    ///   arm that CALLS `take_service_link`, both left the body clean. A whole
    ///   file cannot be stepped out of.
    #[test]
    fn no_admission_path_reads_this_node_s_build_stamp() {
        // the service-link path. Nothing in stream.rs may name the stamp at
        // all: the file has no honest use for it, so the allowlist is empty and
        // every evasion above fails here.
        assert_eq!(
            lines_naming_the_stamp(&read_source("crates/noded/src/stream.rs")),
            Vec::<String>::new(),
            "the service-link path must not read this node's build stamp — a \
             git-absent build would refuse every link and leave the node with no \
             interactive plane"
        );

        // this file DEFINES the stamp and RENDERS it twice — in `hello`'s OK
        // body and in the `/v1/services` catalog document — so its rule is an
        // allowlist. Both renders are the same act: handing this node's own
        // stamp to a caller that would otherwise substitute its own (the CLI's
        // `service status` did exactly that, and named the wrong build). It
        // reads only the shipping half, which is also what keeps the literals
        // below from matching themselves.
        //
        // `founding_set` is the one READER, and it is on the allowlist because
        // what it decides is which directory of wasm on THIS box this binary
        // reads — a set is staged beside a binary by a build, so "did this
        // build stage it" is the only question that can be asked about one.
        // Nothing about a peer, a link or a caller is decided here, and a
        // git-absent build compares `unknown` to `unknown` and founds from its
        // own set exactly as before (`staged_set_verdict`).
        assert_eq!(
            lines_naming_the_stamp(&shipping_half(file!())),
            [
                "pub fn build_identity() -> Option<&'static str> {",
                r#"option_env!("DUCKTAPE_BUILD").filter(|id| !id.is_empty())"#,
                "pub fn build_identity_or_unknown() -> &'static str {",
                "build_identity().unwrap_or(UNKNOWN_BUILD)",
                "staged_set_verdict(&dir, named, staged_by.as_deref(), build_identity())?;",
                r#""build": build_identity_or_unknown(),"#,
                r#""build": build_identity_or_unknown(),"#,
            ],
            "the stamp is DEFINED and RENDERED here, and read only where a set \
             beside this binary is matched to the build that staged it — a new \
             mention is a new reader, and `admit` in particular must never become \
             one"
        );
    }

    /// Read a repo-relative source file, or fail the test saying which.
    fn read_source(relative: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(relative);
        std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{relative}: {error}"))
    }

    /// The part of a file that SHIPS: everything above its first top-level
    /// `#[cfg(test)]`.
    ///
    /// Panics unless that attribute starts a line at column zero. An indented
    /// one is a test-only item mid-file, and cutting there would hand the lint a
    /// shorter file than it thinks it has — the silent-fall-open failure this
    /// whole guard exists not to have.
    fn shipping_half(relative: &str) -> String {
        const MARKER: &str = "#[cfg(test)]";
        let source = read_source(relative);
        let Some(cut) = source.find(MARKER) else {
            panic!("{relative} has no {MARKER} — this lint lives in one");
        };
        assert_eq!(
            cut,
            source[..cut].rfind('\n').map_or(0, |newline| newline + 1),
            "{relative}: the first {MARKER} is indented, so it is a test-only \
             item mid-file. Move the stamp lint's scan, or this cut silently \
             reads less than the whole shipping half."
        );
        source[..cut].to_string()
    }

    /// Every line of `source` that names the stamp, comments excluded — the
    /// stamp is discussed at length in the docs above and none of that is a
    /// read.
    fn lines_naming_the_stamp(source: &str) -> Vec<String> {
        source
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("//"))
            .filter(|line| STAMP_TOKENS.iter().any(|token| line.contains(token)))
            .map(str::to_string)
            .collect()
    }
}
