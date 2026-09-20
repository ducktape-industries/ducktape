//! The per-workspace service grants — `services.toml` beside `node.toml` —
//! and the `ducktape service` family that edits them.
//!
//! A service is offchain muscle (compute, storage, ...) with no sovereign
//! identity: identity and transport belong to the node. So a daemon can only
//! SIGNAL presence to its node (`POST /v1/services/hello`, a volatile catalog
//! — see `noded::services`), and the user decides whether it may act. That
//! decision is this file: `service enable <kind>` mints the grant, `service
//! disable <kind>` retires it, and the node's compute plane gates on the
//! result at boot.
//!
//! Why a sibling file rather than a `[services]` table in node.toml: node.toml
//! is the operator's network plumbing, written once by `init`/`join` and
//! hand-rendered with its own documentation. Everything a CLI verb edits at
//! runtime already lives in its own sibling (`gateway-routes.json`,
//! `invite-fronts.json`, `coord.cap`), and this follows that precedent.

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::cli_args::WorkspaceArgs;
use crate::config;

pub const FILE_NAME: &str = "services.toml";

/// the first-party service kinds. Defined in `noded` because both the node's
/// own surfaces (the ws service link) and this CLI must name them.
pub use noded::services::{AGENT_KIND, AIRLOCK_KIND, COMPUTE_KIND};

/// which first-party daemon a kind names — the ONE discriminant `run` branches
/// on. `None` = a kind this binary hosts no execution plane for: it signals,
/// appears in `list`/`enable`, and executes nothing.
enum Daemon {
    Compute,
    Agent,
    Airlock,
}

fn daemon_for(kind: &str) -> Option<Daemon> {
    match kind {
        COMPUTE_KIND => Some(Daemon::Compute),
        AGENT_KIND => Some(Daemon::Agent),
        AIRLOCK_KIND => Some(Daemon::Airlock),
        _ => None,
    }
}

// `podman_data_dir` lived here: the per-service storage root, runroot and hooks
// directory a private podman service needed, kept per-kind so two daemons did
// not share a `kill_on_drop` service child. There is no service child now — a
// run's VMM is a child of the daemon that started it — so two daemons share no
// state at all rather than carefully sharing none.

/// this service's OWN sandbox backend.
///
/// A pass-through now that every run gets its own microVM: the configured
/// kernel and rootfs ARE the backend, and there is no per-service socket to
/// name because there is no daemon to name it for. The container backend
/// re-rooted the socket here, into the runtime dir rather than the data dir,
/// because a unix socket path is capped near 108 bytes while a workspace path
/// is unbounded. That trap did not disappear — [`provider_host::MicroVm::boot`]
/// carries it for the per-run vsock path instead.
pub(crate) fn sandbox_backend(
    service: &config::ServiceConfig,
) -> Result<provider_host::SandboxBackend, String> {
    // the fix is already IN the file this complains about: `node init` writes
    // the whole `[sandbox]` block commented out, so "uncomment it" is a real
    // instruction rather than a spec to go and look up.
    service.sandbox.clone().ok_or_else(|| {
        format!(
            "no [sandbox] table in node.toml: this host has no configured way to isolate a run \
             — uncomment the [sandbox] block at the end of {}/node.toml, then restart the node",
            service.workspace.display()
        )
    })
}

/// Ask the NODE who it is.
///
/// A daemon needs its node's public identity (it names provider dirs, forge
/// commit authorship and the service instance id), and the obvious place to
/// read it — `identity.key` — is the node's PRIVATE key. So it is asked of the
/// node instead: the process that holds the key is the one that answers for it,
/// and the daemon reaches `/v1/status` over the same localhost surface it
/// already signals on.
///
/// Loud on failure, matching the first-hello contract: a daemon that cannot
/// learn which node it serves must not sit in a retry loop pretending to be
/// configured. The three ways this read can fail are three DIFFERENT operator
/// problems, so they get three different sentences — a malformed key reported
/// as "not started yet" sends the reader to restart a node that is already up.
///
/// TRUST BOUNDARY, and a DIRECTIONAL change worth writing down: the node's
/// identity moved from a file this process could verify (`identity.key`, 0600,
/// owned by the workspace) to an UNAUTHENTICATED local port — `/v1/status`
/// carries no auth. A same-uid process that binds the workspace's `http_listen`
/// before the node does can answer with any `public_key` it likes, and that
/// value goes on to name provider dirs, `execution_node_id`, forge commit
/// authorship and the instance id written into `services.toml`.
///
/// That grants a same-uid attacker nothing they could not already get by
/// reading `identity.key` directly, so it is not a regression and not a
/// blocker. It becomes load-bearing the day a daemon has NO workspace and this
/// port is its only source of the node's identity — at which point this read
/// needs to authenticate the node (the service-link token beside `node.toml` is
/// the obvious carrier), not just parse it.
fn node_identity(base: &str) -> Result<[u8; 32], String> {
    let status = crate::node_http::get_json(base, "/v1/status")
        .map_err(|error| format!("could not read this node's identity: {error}"))?;
    published_identity(&status)
}

/// what a node that is up but has not yet published its mesh identity looks
/// like — and the ONE thing an operator can do about it.
const NOT_PUBLISHED_YET: &str =
    "this node has not published a mesh identity yet — start it, then start the daemon";

/// Decode the node's published mesh identity out of its `/v1/status` document.
///
/// Split from the fetch so every outcome is checkable without standing up a
/// server: this is the decide half, [`node_identity`] is the I/O half.
fn published_identity(status: &serde_json::Value) -> Result<[u8; 32], String> {
    // ABSENT and PRESENT-BUT-EMPTY are ONE operator condition, and the empty
    // case is the one that actually happens: `/v1/status` types `public_key` as
    // a non-Option `String` (`noded::NodeStatus`) which a booting node serves as
    // `""` until it publishes. Without this filter `as_str()` returns
    // `Some("")`, `unhex("")` returns `Ok(vec![])`, and a routine startup race
    // fell through to the length arm below as "0-byte mesh identity" — a
    // malformed-node diagnosis for a node that is merely still booting, while
    // the message written for exactly this case was unreachable.
    //
    // `bin/node` still binds its HTTP listener before publishing, so a
    // supervisor co-starting a node and a daemon reaches this every time.
    let Some(published) = status["public_key"].as_str().filter(|hex| !hex.is_empty()) else {
        return Err(NOT_PUBLISHED_YET.into());
    };
    // the remaining two are malformed-status cases, not startup timing. They
    // deliberately name no gate to go and check: an operator sent to look for a
    // "build mismatch" goes hunting for the daemon/node build check, which is a
    // different mechanism and not what failed here.
    let decoded = config::unhex(published)
        .map_err(|error| format!("this node published a mesh identity that is not hex: {error}"))?;
    <[u8; 32]>::try_from(decoded.as_slice()).map_err(|_| {
        format!(
            "this node published a {}-byte mesh identity, not 32",
            decoded.len()
        )
    })
}

/// bytes of randomness in a grant nonce, matching `INVITE_NONCE_LEN`.
const GRANT_NONCE_LEN: usize = 16;

/// The domain separator for a service instance id.
///
/// The repo has no shared tagged-hash helper and no central registry of
/// numeric kind bytes; the established discipline for minting a NON-object id
/// is a NUL-terminated domain string folded into the preimage
/// (`runs::delegation_id_for`), mirroring the way an invite preimage is bound
/// to `b"ducktape-invite-grant-v1"`. A numeric kind byte is deliberately not
/// used here: service kinds are open string tags (the same shape as capability
/// tags), so there is no closed registry a byte could come from, and widening
/// duckfs's `objects::Kind` — the object store's strict decode gate — would
/// make a grant decodable as a stored object.
const SERVICE_INSTANCE_DOMAIN: &[u8] = b"ducktape/service-instance/v1\0";

/// One granted service: the consent record the user minted at `service
/// enable`, and the whole of its authority. There is no separate token — the
/// presence of this record IS the grant.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServiceGrant {
    /// the service kind tag (`compute`, ...).
    pub kind: String,
    /// the 32-byte instance id, lowercase hex. Minted from the node, the kind
    /// and `nonce`; see [`mint_instance`].
    pub instance: String,
    /// the grant nonce the instance id was minted from, lowercase hex. Kept so
    /// the id is reproducible from the record alone.
    pub nonce: String,
    /// when the grant was minted (unix seconds) — the consent-epoch marker.
    pub granted_unix: u64,
    /// the capability tags the daemon offered in the reviewed hello.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// the grant scopes the daemon requested in the reviewed hello.
    #[serde(default)]
    pub scopes: Vec<String>,
}

impl ServiceGrant {
    /// the house display form, `compute#deadbeef` — the chain-id convention
    /// (`config::mint_chain_id`): the name, `#`, and the first 4 id bytes.
    pub fn display_id(&self) -> String {
        // the record is validated on load, so the prefix is present; a short
        // string would only ever come from an in-memory value under test.
        let head = self.instance.get(..8).unwrap_or(&self.instance);
        format!("{}#{head}", self.kind)
    }
}

/// the whole `services.toml`. `deny_unknown_fields` is the schema guard: a
/// file this build does not understand is refused outright (no version field,
/// no migrations — the remedy is re-granting the services).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Services {
    /// the grants, kind-sorted and unique. `[[service]]` in the file.
    #[serde(default, rename = "service")]
    pub grants: Vec<ServiceGrant>,
}

impl Services {
    fn validate(&self) -> Result<(), String> {
        let mut previous: Option<&str> = None;
        for grant in &self.grants {
            if !kind_is_well_formed(&grant.kind) {
                return Err(format!(
                    "services: {:?} is not a service kind (1..32 chars of [a-z0-9-])",
                    grant.kind
                ));
            }
            if previous.is_some_and(|old| old >= grant.kind.as_str()) {
                return Err("services: kinds must be unique and sorted".into());
            }
            previous = Some(&grant.kind);
            let instance_ok = grant.instance.len() == 64 && is_hex(&grant.instance);
            if !instance_ok {
                return Err(format!(
                    "services: {} instance must be 64 lowercase hex chars",
                    grant.kind
                ));
            }
            let nonce_ok = grant.nonce.len() == GRANT_NONCE_LEN * 2 && is_hex(&grant.nonce);
            if !nonce_ok {
                return Err(format!(
                    "services: {} nonce must be {} lowercase hex chars",
                    grant.kind,
                    GRANT_NONCE_LEN * 2
                ));
            }
        }
        // THE announce these grants imply must be one the registry would take —
        // checked with the very function that derives it, over the WIDEST set
        // they could ever produce.
        //
        // This is what makes both `announce::Refusal` arms properties of the
        // FILE rather than of whichever code path happened to write it: no
        // `services.toml` this node will load can carry an illegal tag or imply
        // more tags than the registry accepts, whoever wrote it. Nothing
        // downstream has to filter, and the watcher's refusal arms are
        // unreachable rather than merely unlikely.
        //
        // Deliberately ONE call rather than a re-implementation of the two
        // rules: a second copy is how a bound drifts from the thing it bounds
        // (the same defect that let a retuned `HEARTBEAT` pass a test pinning
        // it). Capacity is empty here because neither refusal reads it.
        crate::announce::announced_set(
            &self.grants,
            &crate::announce::widest(&self.grants),
            &std::collections::BTreeMap::new(),
        )
        .map_err(|refusal| format!("services: {refusal}"))?;
        Ok(())
    }

    /// the grant for `kind`, if the user has enabled it.
    pub fn grant(&self, kind: &str) -> Option<&ServiceGrant> {
        self.grants.iter().find(|grant| grant.kind == kind)
    }
}

/// A kind tag is lowercase alphanumeric plus `-` — the same rule the node's
/// hello boundary enforces, so a signaling kind and a granted kind are always
/// comparable and a kind can never carry the NUL the id preimage separates on.
fn kind_is_well_formed(kind: &str) -> bool {
    let len_ok = !kind.is_empty() && kind.len() <= 32;
    len_ok
        && kind
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn is_hex(text: &str) -> bool {
    text.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Mint a service instance id: `sha256(domain ‖ node_id ‖ 0 ‖ kind ‖ 0 ‖
/// nonce)`. `node_id` is a FIXED 32 bytes and `kind` cannot contain a NUL
/// (the grammar forbids it), so the preimage parses one way only — the length
/// is carried by the type rather than by a comment. Node-scoped (so two nodes never collide), kind-separated (so one
/// node's compute grant is not its storage grant) and nonce-fresh (so a
/// re-enable after a `disable` is a NEW id — the id doubles as the
/// consent-epoch marker).
pub fn mint_instance(node_id: &[u8; 32], kind: &str, nonce: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(SERVICE_INSTANCE_DOMAIN);
    digest.update(node_id);
    digest.update([0]);
    digest.update(kind.as_bytes());
    digest.update([0]);
    digest.update(nonce);
    digest.finalize().into()
}

/// Read a workspace's grants. An ABSENT file is an empty grant set, not an
/// error — a node that never enabled a service simply has none.
pub fn load(workspace: &Path) -> Result<Services, String> {
    let path = workspace.join(FILE_NAME);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Services::default());
        }
        Err(error) => return Err(format!("read {path:?}: {error}")),
    };
    let services: Services = toml::from_str(&text).map_err(|error| format!("{path:?}: {error}"))?;
    services.validate()?;
    Ok(services)
}

/// The grant for one kind in one workspace — what the node's boot path asks.
pub fn grant_for(workspace: &Path, kind: &str) -> Result<Option<ServiceGrant>, String> {
    Ok(load(workspace)?.grant(kind).cloned())
}

fn save(workspace: &Path, services: &Services) -> Result<(), String> {
    services.validate()?;
    std::fs::create_dir_all(workspace).map_err(|error| format!("create {workspace:?}: {error}"))?;
    let path = workspace.join(FILE_NAME);
    let temporary = workspace.join(format!(".{FILE_NAME}.tmp"));
    // no grants = no file: an empty `services.toml` and a missing one mean the
    // same thing, and leaving the husk behind invites a stale read.
    if services.grants.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("remove {path:?}: {error}")),
        }
        let _ = std::fs::remove_file(&temporary);
        return Ok(());
    }
    let body = toml::to_string_pretty(services).expect("services serialize");
    let text = format!(
        "# the services this workspace's user has granted standing on this node.\n\
         # managed by `ducktape service enable|disable`; the node reads it at boot.\n\
         {body}"
    );
    std::fs::write(&temporary, text).map_err(|error| format!("write {temporary:?}: {error}"))?;
    if let Err(error) = std::fs::rename(&temporary, &path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("replace {path:?}: {error}"));
    }
    Ok(())
}

/// The exclusive lock a read-modify-write of the grants is taken under.
///
/// A READER needs none: [`save`] replaces the file by rename, so a reader sees
/// the old grants or the new ones and never a torn file. What that atomicity
/// cannot survive is two WRITERS, because each writes the whole file back. Two
/// `service run --enable` processes read the same grants, and whichever saves
/// second erases the other's — leaving a daemon that is enabled by its own log,
/// absent from `services.toml`, and ungranted to the node, which answers every
/// saga accept `accept_not_capability_provider` while its log says it is up.
///
/// The lock lives in a file BESIDE the grants, never on the grants themselves:
/// `save` replaces that inode by rename, so a lock taken on it would be held on
/// a file nothing reads any more the moment the first writer finished.
struct GrantLock(std::fs::File);

fn lock_grants(workspace: &Path) -> Result<GrantLock, String> {
    use std::os::unix::io::AsRawFd as _;
    std::fs::create_dir_all(workspace).map_err(|error| format!("create {workspace:?}: {error}"))?;
    let path = workspace.join(format!(".{FILE_NAME}.lock"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("open {path:?}: {error}"))?;
    // BLOCKING. The guarded section is a read and a rename, so a node bringing
    // three daemons up at once waits microseconds; telling an operator to retry
    // would be a worse answer than waiting.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(format!(
            "lock {path:?}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(GrantLock(file))
}

impl Drop for GrantLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd as _;
        // Dropping the file would release it anyway; unlocking explicitly keeps
        // the release at the end of the guarded section rather than at whatever
        // point the descriptor happens to close.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Read, change and write the grants as ONE step under [`lock_grants`]. Every
/// writer of `services.toml` goes through here.
///
/// Anything derived FROM the grants is derived INSIDE the closure — the
/// announced set above all — or it would describe a set the file no longer
/// holds. Network I/O deliberately is not: the caller submits after the guard
/// has dropped, because a `/v1/submit` waits on consensus and holding a
/// cross-process file lock for that long would serialize the very daemons this
/// exists to let start together.
fn with_grants<T>(
    workspace: &Path,
    change: impl FnOnce(&mut Services) -> Result<T, String>,
) -> Result<T, String> {
    let _guard = lock_grants(workspace)?;
    let mut services = load(workspace)?;
    let derived = change(&mut services)?;
    save(workspace, &services)?;
    Ok(derived)
}

// ============================================================================
// list/status state derivation
// ============================================================================

/// What one service's standing is on this node. Exactly three states, because
/// presence and consent are independent: a daemon may signal without a grant,
/// hold a grant while absent, or both.
/// The serde spellings are written out rather than derived from a rename rule:
/// they must equal [`ServiceState::label`] exactly, so `--json` and the table
/// never name one state two ways. `state_tokens_match_the_rendered_labels`
/// pins that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ServiceState {
    /// signaling, no grant — visible to the user, authorized for nothing.
    #[serde(rename = "signaling")]
    Signaling,
    /// granted and signaling — the working state.
    #[serde(rename = "enabled")]
    Enabled,
    /// granted but not signaling. An operational warning, never an error: the
    /// daemon is down, restarting, or was never started.
    #[serde(rename = "enabled-but-absent")]
    EnabledAbsent,
}

/// one rendered row of `service list` / `service status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceRow {
    pub kind: String,
    pub state: ServiceState,
    /// the display id (`compute#deadbeef`); `None` until the kind is granted.
    pub instance: Option<String>,
    /// the signaling daemon's version; `None` when it is not signaling.
    pub version: Option<String>,
    /// the signaling daemon's build stamp; `None` when it is not signaling.
    ///
    /// This is the diagnostic that replaced the build gate. `service status`
    /// prints it beside this node's own, so the ordinary dev-loop skew (edit,
    /// rebuild the node, yesterday's daemon still running) is VISIBLE — where
    /// it used to be a refusal that kept the daemon out of the catalog and
    /// therefore out of this table entirely.
    pub build: Option<String>,
    pub capabilities: Vec<String>,
    pub scopes: Vec<String>,
    /// service kinds this daemon declared it wants present, and the subset of
    /// them that nothing on this node provides.
    ///
    /// Rendered as an informational warning and NOTHING else: an unmet need
    /// never blocks enabling, never orders startup and never gates readiness.
    /// A service whose need is unmet still enables, still runs, still serves.
    pub needs: Vec<String>,
    pub unmet_needs: Vec<String>,
}

/// Fold the volatile catalog and the durable grants into one kind-sorted view.
///
/// The offered tags come from the LIVE hello when there is one (that is the
/// truth about what the daemon can do right now) and fall back to what the
/// grant recorded at consent time, so an absent service still shows what it
/// was enabled for.
pub fn rows(signaling: &[noded::services::Signaling], grants: &[ServiceGrant]) -> Vec<ServiceRow> {
    // a need is met when SOME service of that kind is enabled here. Local
    // grant state only — resolving "is there capacity anywhere in the network"
    // would mean a registry query on a display path, and a readiness signal is
    // exactly what this must not become.
    let enabled_kinds: std::collections::BTreeSet<&str> =
        grants.iter().map(|grant| grant.kind.as_str()).collect();
    let unmet = |needs: &[String]| -> Vec<String> {
        needs
            .iter()
            .filter(|need| !enabled_kinds.contains(need.as_str()))
            .cloned()
            .collect()
    };
    let mut rows: Vec<ServiceRow> = signaling
        .iter()
        .map(|live| {
            let granted = grants.iter().find(|grant| grant.kind == live.kind);
            ServiceRow {
                needs: live.needs.clone(),
                unmet_needs: unmet(&live.needs),
                kind: live.kind.clone(),
                state: match granted {
                    Some(_) => ServiceState::Enabled,
                    None => ServiceState::Signaling,
                },
                instance: granted.map(ServiceGrant::display_id),
                version: Some(live.version.clone()),
                build: Some(live.build.clone()),
                capabilities: live.capabilities.clone(),
                scopes: live.scopes.clone(),
            }
        })
        .collect();
    let absent = grants
        .iter()
        .filter(|grant| !signaling.iter().any(|live| live.kind == grant.kind))
        .map(|grant| ServiceRow {
            kind: grant.kind.clone(),
            state: ServiceState::EnabledAbsent,
            instance: Some(grant.display_id()),
            version: None,
            // a grant records no build: it is hello metadata, and nothing is
            // signaling for this kind.
            build: None,
            capabilities: grant.capabilities.clone(),
            scopes: grant.scopes.clone(),
            // needs are live hello metadata; an absent daemon declares none.
            needs: Vec::new(),
            unmet_needs: Vec::new(),
        });
    rows.extend(absent);
    rows.sort_by(|a, b| a.kind.cmp(&b.kind));
    rows
}

// ============================================================================
// presentation
// ============================================================================

// Styling vocabulary. These are `anstyle` values, so a renderer below always
// emits the escapes and never asks whether it is talking to a terminal: the
// `anstream` wrapper the verbs print through STRIPS them when the destination
// is not a tty, when `NO_COLOR` is set, or when `TERM=dumb`. One place decides,
// every renderer stays pure and testable.
const BOLD: anstyle::Style = anstyle::Style::new().bold();
const DIM: anstyle::Style = anstyle::Style::new().dimmed();
const RED: anstyle::Style =
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Red)));
const GREEN: anstyle::Style =
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Green)));
const YELLOW: anstyle::Style =
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Yellow)));

/// Style `text`. Callers pad BEFORE styling: escape bytes carry no display
/// width, so styling a column first would break every alignment below it.
fn paint(style: anstyle::Style, text: &str) -> String {
    format!("{style}{text}{style:#}")
}

impl ServiceState {
    /// the stable token `--json` emits and the table prints.
    pub fn label(self) -> &'static str {
        match self {
            ServiceState::Signaling => "signaling",
            ServiceState::Enabled => "enabled",
            ServiceState::EnabledAbsent => "enabled-but-absent",
        }
    }

    /// the state marker. A glyph is plain UTF-8 text, not an escape, so it
    /// survives a pipe intact and stays greppable; only its color is dropped.
    fn glyph(self) -> &'static str {
        match self {
            ServiceState::Signaling => "·",
            ServiceState::Enabled => "✓",
            ServiceState::EnabledAbsent => "!",
        }
    }

    fn style(self) -> anstyle::Style {
        match self {
            ServiceState::Signaling => DIM,
            ServiceState::Enabled => GREEN,
            ServiceState::EnabledAbsent => YELLOW,
        }
    }
}

/// pad to `width` columns, THEN style — never the other way round.
fn column(text: &str, width: usize, style: anstyle::Style) -> String {
    paint(style, &format!("{text:<width$}"))
}

const KIND_WIDTH: usize = 16;
const STATE_WIDTH: usize = 18;

/// `service list` — one aligned row per service.
fn render_list(rows: &[ServiceRow]) -> String {
    if rows.is_empty() {
        return "no services signaling and none enabled\n\
                start one, then: ducktape service enable <kind>\n"
            .into();
    }
    let mut out = paint(
        DIM,
        &format!(
            "  {:<KIND_WIDTH$} {:<STATE_WIDTH$} {}",
            "KIND", "STATE", "INSTANCE"
        ),
    );
    out.push('\n');
    for row in rows {
        out.push_str(&format!(
            "{} {} {} {}\n",
            paint(row.state.style(), row.state.glyph()),
            column(&row.kind, KIND_WIDTH, BOLD),
            column(row.state.label(), STATE_WIDTH, row.state.style()),
            row.instance.as_deref().unwrap_or("-"),
        ));
        if let Some(hint) = unmet_hint(row) {
            out.push_str(&format!("  {}\n", paint(YELLOW, &hint)));
        }
    }
    out
}

/// The informational line for a service whose declared needs nothing here
/// meets. Purely advisory — see [`ServiceRow::unmet_needs`].
fn unmet_hint(row: &ServiceRow) -> Option<String> {
    if row.unmet_needs.is_empty() {
        return None;
    }
    Some(format!(
        "wants {} — not enabled on this node (informational; {} still serves)",
        row.unmet_needs.join(", "),
        row.kind
    ))
}

/// The build column: the signaling daemon's stamp, and — only when it differs
/// from the NODE's — the node's beside it.
///
/// This IS the diagnostic that replaced the build gate. Skew is now visible
/// and informational; it used to keep the daemon out of the catalog entirely,
/// so this row could not have existed to show it.
///
/// `node` is the stamp the NODE reported in its own `/v1/services` document,
/// and reading this binary's `build_identity_or_unknown()` instead was a real
/// defect: a CLI is whichever `ducktape` the operator happened to type, not the
/// one running the node. `service status` from an older binary printed THAT
/// binary's commit as "(this node: …)" — naming a build the node was not
/// running, and calling a daemon skewed that was in step with it.
///
/// One rule for "do these two stamps disagree", and it is [`Skew`]'s: a side
/// that cannot name its build proves nothing, and neither does a node that did
/// not answer.
fn render_build(daemon: Option<&str>, node: Option<&str>) -> String {
    let Some(daemon) = daemon else {
        return "-".into();
    };
    let Some(node) = node else {
        return daemon.to_string();
    };
    match Skew::between(daemon, Some(node)) {
        Skew::Matched | Skew::Unknown => daemon.to_string(),
        Skew::Skewed => format!(
            "{daemon} {}",
            paint(YELLOW, &format!("(this node: {node})"))
        ),
    }
}

/// `service status` — a readable block per service rather than a flat dump.
fn render_status(rows: &[ServiceRow], node_build: Option<&str>) -> String {
    if rows.is_empty() {
        return "no services signaling and none enabled\n".into();
    }
    let mut out = String::new();
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&format!(
            "{} {}  {}\n",
            paint(row.state.style(), row.state.glyph()),
            paint(BOLD, &row.kind),
            paint(row.state.style(), row.state.label()),
        ));
        let fields = [
            (
                "instance",
                row.instance.as_deref().unwrap_or("-").to_string(),
            ),
            ("version", row.version.as_deref().unwrap_or("-").to_string()),
            ("build", render_build(row.build.as_deref(), node_build)),
            ("offers", join_or_dash(&row.capabilities)),
            ("scopes", join_or_dash(&row.scopes)),
            ("needs", join_or_dash(&row.needs)),
        ];
        for (name, value) in fields {
            out.push_str(&format!("    {} {value}\n", column(name, 10, DIM)));
        }
        if row.state == ServiceState::EnabledAbsent {
            // absence has exactly one shape now that no build gate can hide a
            // daemon from the catalog: nothing is signaling for this kind.
            out.push_str(&format!(
                "    {}\n",
                paint(
                    YELLOW,
                    "enabled but not signaling — is its daemon running \
                     (ducktape service run), and pointed at this node's http surface?"
                )
            ));
        }
        if let Some(hint) = unmet_hint(row) {
            out.push_str(&format!("    {}\n", paint(YELLOW, &hint)));
        }
    }
    out
}

/// The consent summary shown before a grant is minted: what is being enabled,
/// on which node, and exactly which offers/scopes the reviewed hello declared.
fn render_enable_summary(plan: &EnablePlan) -> String {
    let signaling = match &plan.offered {
        Some(_) => paint(GREEN, "signaling"),
        None => paint(YELLOW, "not signaling yet"),
    };
    let offered_list = |pick: fn(&noded::services::Signaling) -> &Vec<String>| {
        plan.offered
            .as_ref()
            .map(|offer| join_or_dash(pick(offer)))
            .unwrap_or_else(|| "-".into())
    };
    let rows = [
        ("service", paint(BOLD, &plan.kind)),
        ("node", format!("{} · {}", plan.chain_id, plan.node_hex8())),
        ("status", signaling),
        ("offers", offered_list(|offer| &offer.capabilities)),
        (
            "grant scopes",
            paint(RED, &offered_list(|offer| &offer.scopes)),
        ),
    ];
    let mut out = String::new();
    for (name, value) in rows {
        out.push_str(&format!("  {} {value}\n", column(name, 13, DIM)));
    }
    out.push_str(&render_reconsent_diff(plan));
    out
}

/// The re-consent half of the consent screen: what this enable would CHANGE
/// about a grant that already stands. Empty for a first grant.
///
/// A diff rather than a list because that is the whole question being asked —
/// the operator already consented to most of this, and what deserves their
/// attention is the tag that was not there before. Rendered from the two
/// records, so it says exactly what the file is about to become.
///
/// Covers BOTH persisted fields a re-mint can widen (`capabilities` AND
/// `scopes` — see `remint_grant`/`mint_grant`, which persist `offered.scopes`
/// verbatim): "unchanged" is said only when neither set moved, never when
/// capabilities alone happen to match while scopes grew. A widened scope is
/// rendered in the same `+`/`-`/` ` list as a capability, tagged so the two
/// halves cannot be mistaken for each other.
fn render_reconsent_diff(plan: &EnablePlan) -> String {
    let Some(previous) = &plan.previous else {
        return String::new();
    };
    let tagged = |prefix: &'static str, tags: &[String]| -> Vec<String> {
        tags.iter().map(|t| format!("{prefix}:{t}")).collect()
    };
    let had_tags = [
        tagged("cap", &previous.capabilities),
        tagged("scope", &previous.scopes),
    ]
    .concat();
    let has_tags = [
        tagged("cap", &plan.grant.capabilities),
        tagged("scope", &plan.grant.scopes),
    ]
    .concat();
    let had: std::collections::BTreeSet<&str> = had_tags.iter().map(String::as_str).collect();
    let has: std::collections::BTreeSet<&str> = has_tags.iter().map(String::as_str).collect();
    let mut out = format!(
        "\n  {} is already enabled as {}\n",
        paint(BOLD, &plan.kind),
        previous.display_id()
    );
    if had == has {
        out.push_str(&format!(
            "  {}\n",
            paint(
                DIM,
                "the offered set is unchanged — re-consenting changes nothing"
            )
        ));
        return out;
    }
    out.push_str("  the offered set has CHANGED since you consented:\n\n");
    for tag in has.union(&had) {
        let (mark, styled) = match (has.contains(tag), had.contains(tag)) {
            (true, false) => ("+", paint(GREEN, &format!("{tag}  (new)"))),
            (false, true) => ("-", paint(RED, &format!("{tag}  (no longer installed)"))),
            _ => (" ", paint(DIM, tag)),
        };
        out.push_str(&format!("    {mark} {styled}\n"));
    }
    out.push('\n');
    out
}

/// Print rendered output to stdout THROUGH `anstream`, which strips every
/// escape when the destination is not a terminal, when `NO_COLOR` is set, or
/// under `TERM=dumb`. This is the single place non-tty correctness is
/// enforced — the renderers above always style, and never ask.
fn write_out(rendered: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write as _;
    let mut out = anstream::stdout().lock();
    out.write_all(rendered.as_bytes())?;
    out.flush()?;
    Ok(())
}

/// the stderr twin of [`write_out`], for prose and the consent summary.
fn write_err(rendered: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write as _;
    let mut err = anstream::stderr().lock();
    err.write_all(rendered.as_bytes())?;
    err.flush()?;
    Ok(())
}

// ============================================================================
// the `ducktape service` family
// ============================================================================

/// the `ducktape service` verbs — the consent boundary for offchain service
/// daemons. `run` arrives with daemon mode; today a service is hosted in the
/// node process and `enable` is what turns it on.
#[derive(Debug, clap::Subcommand)]
pub(crate) enum ServiceCmd {
    /// run a service daemon in the foreground (signals; systemd unit target)
    Run(RunArgs),
    /// show services signaling to this node and those enabled in config
    List(ReadArgs),
    /// grant a service standing on this node and announce it (needs a running node)
    Enable(EnableArgs),
    /// revoke a service's grant, retire its id, retract the announce (needs a running node)
    Disable(KindArgs),
    /// per-service state, instance id, offered tags and requested scopes
    Status(ReadArgs),
}

#[derive(Debug, clap::Args)]
pub(crate) struct ReadArgs {
    /// show only this kind (`compute`, `agent`, `airlock`); omitted = all
    #[arg(value_name = "KIND")]
    kind: Option<String>,
    #[command(flatten)]
    workspace: WorkspaceArgs,
    /// emit one machine-readable JSON array instead of a table
    #[arg(long)]
    json: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct KindArgs {
    /// the service kind (`compute`, `agent`, `airlock`)
    #[arg(value_name = "KIND")]
    kind: String,
    #[command(flatten)]
    workspace: WorkspaceArgs,
}

#[derive(Debug, clap::Args)]
pub(crate) struct RunArgs {
    /// the service kind to run (`compute`, `agent`, `airlock`)
    #[arg(value_name = "KIND")]
    kind: String,
    #[command(flatten)]
    workspace: WorkspaceArgs,
    /// grant the service without asking — for scripts and systemd units
    #[arg(long, conflicts_with = "no_enable")]
    enable: bool,
    /// never offer to grant the service; just signal
    #[arg(long = "no-enable")]
    no_enable: bool,
}

/// What `service run` should do about a kind that is not yet granted. ONE
/// discriminant rather than two booleans steering the code below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnableOffer {
    /// `--enable`: mint the grant without asking.
    Always,
    /// `--no-enable`: never offer; signal and say so once.
    Never,
    /// the default: ask, but only where someone can answer.
    AskIfTty,
}

impl RunArgs {
    fn offer(&self) -> EnableOffer {
        // clap makes the pair mutually exclusive, so the fourth combination
        // cannot arrive; it is spelled out rather than hidden behind a `_`.
        match (self.enable, self.no_enable) {
            (true, true) => unreachable!("clap refuses --enable with --no-enable"),
            (true, false) => EnableOffer::Always,
            (false, true) => EnableOffer::Never,
            (false, false) => EnableOffer::AskIfTty,
        }
    }
}

#[derive(Debug, clap::Args)]
pub(crate) struct EnableArgs {
    /// the service kind (`compute`, `agent`, `airlock`)
    #[arg(value_name = "KIND")]
    kind: String,
    #[command(flatten)]
    workspace: WorkspaceArgs,
    /// grant without the interactive confirmation (for scripts and unit files)
    #[arg(long, short = 'y')]
    yes: bool,
}

/// Run one verb of the `ducktape service` family.
pub(super) fn run(cmd: ServiceCmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        ServiceCmd::Run(args) => run_service(args),
        ServiceCmd::List(args) => list(args),
        ServiceCmd::Enable(args) => enable(args),
        ServiceCmd::Disable(args) => disable(args),
        ServiceCmd::Status(args) => status(args),
    }
}

/// what `GET /v1/services` says: who is signaling, and WHICH NODE said so.
#[derive(Debug, Default)]
struct Catalog {
    signaling: Vec<noded::services::Signaling>,
    /// the node's OWN build stamp, straight out of the node process. `None` =
    /// the node did not answer, which is not evidence of anything. Never this
    /// binary's `build_identity_or_unknown()`: see [`render_build`].
    node_build: Option<String>,
}

/// The services signaling to the workspace's own node, or why the node could
/// not be asked. `list` renders the grants off disk either way; what neither
/// verb may do is turn "I could not ask" into "nothing is signaling".
fn catalog_now(base: &str, workspace: &std::path::Path) -> (Catalog, Option<String>) {
    match read_catalog(base) {
        Ok(catalog) => (catalog, None),
        // A node that is not running is the ordinary case — `list` must still
        // render the grants — so it stays quiet on stderr. It is NOT silent in
        // the answer: the caller is told the catalog is unknown rather than
        // empty, because those render differently and exit differently.
        //
        // The DIRECTORY is what says whether a launcher owns this node, and
        // this verb already opened it. `ReadFailure` renders the same sentence
        // from the url instead, by asking the registry which workspace serves
        // it — an answer two networks left on the default `http_listen` make
        // ambiguous. Same renderer either way; this one just has the better
        // input, so it hands it over rather than throwing it away.
        Err(crate::node_http::ReadFailure::Unreachable(_)) => (
            Catalog::default(),
            Some(crate::node_http::not_running_in(Some(workspace)).to_string()),
        ),
        // No warning printed here: the caller ends on this reason, and printing
        // it twice made the second line read like a second fault.
        Err(error) => (Catalog::default(), Some(error.to_string())),
    }
}

/// What is signaling right now, with "could not ask" flattened to "nothing".
///
/// Only for the enable/disable paths: those already hold a plan the node
/// answered for, so an unreadable catalog there is a node that went away
/// mid-verb, and the announce set they build is refused on its own terms. The
/// read verbs must NOT use this — for them the difference between an empty
/// catalog and an unread one is the whole answer.
fn signaling_now(base: &str) -> Vec<noded::services::Signaling> {
    read_catalog(base).unwrap_or_default().signaling
}

fn read_catalog(base: &str) -> Result<Catalog, crate::node_http::ReadFailure> {
    use crate::node_http::ReadFailure;
    let body = crate::node_http::get_json(base, "/v1/services")?;
    let signaling = body
        .get("signaling")
        .ok_or_else(|| ReadFailure::Rejected("/v1/services carries no `signaling` field".into()))?;
    Ok(Catalog {
        signaling: serde_json::from_value(signaling.clone())
            .map_err(|e| ReadFailure::Rejected(format!("unexpected /v1/services shape: {e}")))?,
        node_build: body["build"].as_str().map(str::to_string),
    })
}

/// the rendered rows and the node build they are to be judged against.
/// What the read verbs render: the rows, the node's own build stamp, and —
/// when the node could not be asked — why. The third is not decoration: the
/// rows alone cannot distinguish an empty catalog from an unread one.
struct View {
    rows: Vec<ServiceRow>,
    node_build: Option<String>,
    /// why the catalog is unknown, already carrying what to do about it —
    /// `ReadFailure` renders that, so this verb neither reconstructs the
    /// sentence nor guesses at a launcher it cannot see.
    unread: Option<String>,
}

fn view(args: &ReadArgs) -> Result<View, Box<dyn std::error::Error>> {
    let workspace = args.workspace.dir()?;
    let grants = load(&workspace)?;
    let service = config::resolve_service(&args.workspace.config_file()?)?;
    let (catalog, unread) = match service.http_listen.as_deref() {
        Some(listen) => catalog_now(&config::http_base_of(listen), &workspace),
        None => (
            Catalog::default(),
            Some("this workspace's config names no http address to ask".into()),
        ),
    };
    let all = rows(&catalog.signaling, &grants.grants);
    Ok(View {
        rows: only_kind(all, args.kind.as_deref())?,
        node_build: catalog.node_build,
        unread,
    })
}

/// Narrow the rendered rows to one kind, or say that kind is not here.
///
/// `service status compute` reads as the obvious invocation — it is the shape
/// `enable`, `disable` and `run` all take — and answered
/// `error: unexpected argument 'compute' found`, which reads like the command
/// is broken rather than the invocation. Split out so the "named a kind that
/// isn't there" message is drivable without a node.
fn only_kind(rows: Vec<ServiceRow>, kind: Option<&str>) -> Result<Vec<ServiceRow>, String> {
    let Some(kind) = kind else { return Ok(rows) };
    let known: Vec<String> = rows.iter().map(|row| row.kind.clone()).collect();
    let matched: Vec<ServiceRow> = rows.into_iter().filter(|row| row.kind == kind).collect();
    if !matched.is_empty() {
        return Ok(matched);
    }
    match known.as_slice() {
        [] => Err(format!(
            "{kind} is neither signaling nor enabled on this node — start it with: \
             ducktape service run {kind}"
        )),
        here => Err(format!(
            "no service {kind:?} on this node; there is: {}",
            here.join(", ")
        )),
    }
}

fn list(args: ReadArgs) -> Result<(), Box<dyn std::error::Error>> {
    let view = view(&args)?;
    // With nothing on disk AND nothing read, the renderer's line is "no
    // services signaling and none enabled" — an assertion about the world this
    // call did not establish. The refusal below is then the whole answer.
    let has_rows = !view.rows.is_empty();
    if args.json {
        println!("{}", serde_json::to_string(&view.rows)?);
    } else if has_rows || view.unread.is_none() {
        // no build column in `list`: it is the one-line-per-service view, and
        // the node's stamp belongs beside the daemon's, which only `status`
        // has room for.
        write_out(&render_list(&view.rows))?;
    }
    match view.unread {
        None => Ok(()),
        Some(reason) => Err(unread_refusal(&reason, has_rows).into()),
    }
}

fn status(args: ReadArgs) -> Result<(), Box<dyn std::error::Error>> {
    let view = view(&args)?;
    // See `list`: an empty render is an assertion, and an unread catalog did
    // not earn it. A caller that asked for `--json` and got `[]` must not read
    // it as "no services" either, which is why the exit code covers both.
    let has_rows = !view.rows.is_empty();
    if args.json {
        println!("{}", serde_json::to_string_pretty(&view.rows)?);
    } else if has_rows || view.unread.is_none() {
        write_out(&render_status(&view.rows, view.node_build.as_deref()))?;
    }
    // Whatever printed came off disk and is real. What is SIGNALING is not
    // known, and every state word in that render is about signaling — so a
    // verb that could not ask must not exit as though it had.
    match view.unread {
        None => Ok(()),
        Some(reason) => Err(unread_refusal(&reason, has_rows).into()),
    }
}

/// The sentence a read verb ends on when the node never answered: what is
/// still true (whatever is on disk), what is not known (what is signaling
/// right now), and then `reason` on its own line — which already carries the
/// next step, whether that is starting the node or reading the log of the
/// launcher that is starting it.
fn unread_refusal(reason: &str, printed_grants: bool) -> String {
    let disk = match printed_grants {
        true => "the grants above are what this workspace holds on disk",
        false => "this workspace holds no grants on disk",
    };
    format!("{disk}; what is SIGNALING could not be read.\n{reason}")
}

fn join_or_dash(items: &[String]) -> String {
    match items.is_empty() {
        true => "-".into(),
        false => items.join(", "),
    }
}

/// How many capability tags a one-line startup banner may name before it stops
/// being a line and becomes a wall.
const BANNER_TAGS: usize = 6;

/// The daemon's startup banner, which is NOT a consent surface: compute offers
/// one tag per model × effort, so a real host signals ~37 of them and the whole
/// banner — the `signaling to <chain>` half included — scrolls off the right of
/// the terminal.
///
/// Deliberately NOT applied to the `enable` consent screen or to
/// `service status`: those exist to show you exactly what you are agreeing to
/// and exactly what is offered, and a truncated list there would be a lie of
/// omission. This one only says "it started".
fn summarize_capabilities(kind: &str, tags: &[String]) -> String {
    if tags.len() <= BANNER_TAGS {
        return join_or_dash(tags);
    }
    format!(
        "{}, +{} more (ducktape service status {kind})",
        tags[..BANNER_TAGS].join(", "),
        tags.len() - BANNER_TAGS,
    )
}

/// Everything `enable` needs to decide, gathered without writing anything.
///
/// Split from the commit deliberately: the CLI verb renders this, asks the
/// user, and only then commits — and `service run`'s inline "enable now?"
/// prompt (step 2) drives the same two calls, so there is exactly one grant
/// mint and one consent record no matter which surface the user came through.
#[derive(Debug)]
pub(crate) struct EnablePlan {
    pub kind: String,
    pub chain_id: String,
    pub node_id: [u8; 32],
    /// the reviewed hello, when the daemon is currently signaling.
    pub offered: Option<noded::services::Signaling>,
    /// the grant this kind already holds, when this enable is a RE-CONSENT.
    /// `None` is a first grant. What makes the consent screen a diff rather
    /// than a list, and what `commit_enable` replaces in place.
    pub(crate) previous: Option<ServiceGrant>,
    /// the grant this enable would mint. Decided here — minting is randomness
    /// and a clock read, not a write — so the commit below is purely two
    /// writers in order (announce, then persist) with nothing left to decide.
    pub(crate) grant: ServiceGrant,
    /// the capacity this node announces beside its tags, from the same resolved
    /// config the consent screen was rendered from.
    pub(crate) capacity: std::collections::BTreeMap<String, u64>,
}

impl EnablePlan {
    /// the node's own short form, matching the `#hex8` display convention.
    fn node_hex8(&self) -> String {
        config::hex_bytes(&self.node_id[..4])
    }
}

/// Decide what enabling `kind` would mean. Writes nothing.
///
/// `node_id` is supplied by the caller ([`node_identity`]) rather than read out
/// of `identity.key`: minting an instance id needs the node's PUBLIC key, and
/// resolving it from the key file would put the node's private key in every
/// process that offers a consent screen.
///
/// `service` is passed in rather than re-resolved from
/// `<workspace>/node.toml`, for the reason `run_service` gives: a workspace
/// does not always CONTAIN the config that names it (`--config` points
/// elsewhere; the dev shape's workspace is its `storage_dir`). Re-reading would
/// mint the consent screen's chain id from a different file than the one this
/// verb was pointed at — or fail on a file that is not there.
pub(crate) fn plan_enable(
    workspace: &Path,
    kind: &str,
    service: &config::ServiceConfig,
    node_id: [u8; 32],
) -> Result<EnablePlan, String> {
    let listen = service
        .http_listen
        .as_deref()
        .ok_or("this node serves no http surface")?;
    plan_enable_from(
        workspace,
        kind,
        service,
        node_id,
        signaling_now(&config::http_base_of(listen)),
    )
}

/// The decide half, with the signaling catalog SUPPLIED rather than fetched.
///
/// Split so the consent boundary's two refusals — an illegal tag and a
/// cap-crossing union — are reachable from a test. `catalog_now` reads
/// `/v1/services` over HTTP, so with it inlined every rule in here could only be
/// exercised against a running node, which in practice meant not at all.
fn plan_enable_from(
    workspace: &Path,
    kind: &str,
    service: &config::ServiceConfig,
    node_id: [u8; 32],
    signaling: Vec<noded::services::Signaling>,
) -> Result<EnablePlan, String> {
    if !kind_is_well_formed(kind) {
        return Err(format!(
            "{kind:?} is not a service kind (1..32 chars of [a-z0-9-])"
        ));
    }
    // an existing grant is RE-CONSENT, not a refusal: the operator installs an
    // executor and runs `enable` again to offer it. One grant per kind still
    // holds — the id, its nonce and the standing it carries are kept, and only
    // the reviewed set is re-minted (`remint_grant`), so re-consenting never
    // mints a second id for a kind that already has live standing.
    //
    // It used to refuse with "disable first", which made widening a grant a
    // revoke-and-regrant: a window with no consent on chain, a new instance id
    // for every added CLI, and the announce retracted and re-placed in between.
    let previous = load(workspace)?.grant(kind).cloned();
    // the grant is minted FROM a reviewed hello, so there must BE one. A
    // grant invented for an absent daemon would record no offered tags and no
    // requested scopes — the consent screen would show nothing and the
    // announce set would be empty — which is consent in name only.
    let offered = signaling
        .into_iter()
        .find(|entry| entry.kind == kind)
        .ok_or_else(|| {
            format!(
                "{kind} is not signaling to this node, so there is nothing to consent to — \
                 start it first: ducktape service run {kind}"
            )
        })?;
    let grant = match &previous {
        Some(standing) => remint_grant(standing, &offered),
        None => mint_grant(kind, node_id, &offered),
    };
    // REFUSE here if the registry could not take what these grants imply.
    //
    // Bounded against the WIDEST set the grants could ever produce — every
    // granted kind signaling everything it was granted — not against whoever
    // happens to be signaling right now. Checking the live set would make this
    // order-dependent: enabling `compute` while `agent`'s daemon was down would
    // pass, and the union would cross the cap later when `agent` started, with
    // no verb running to refuse it and no way for the watcher to do anything
    // but announce a truncated set or nothing at all.
    // the prospective set REPLACES a re-consented kind rather than pushing a
    // second grant for it — the file is unique-and-sorted by kind, and a
    // doubled kind would both fail `validate` and double-count against the cap.
    let mut prospective = load(workspace)?.grants;
    prospective.retain(|existing| existing.kind != grant.kind);
    prospective.push(grant.clone());
    crate::announce::announced_set(
        &prospective,
        &crate::announce::widest(&prospective),
        &service.sandbox_capacity,
    )
    .map_err(|refusal| format!("{kind} cannot be announced: {refusal}"))?;
    Ok(EnablePlan {
        kind: kind.to_string(),
        chain_id: service.chain_id.clone(),
        node_id,
        offered: Some(offered),
        previous,
        grant,
        capacity: service.sandbox_capacity.clone(),
    })
}

/// Mint one grant from a reviewed hello. Randomness and a clock read — it
/// writes nothing, which is why it belongs to the plan rather than the commit.
fn mint_grant(kind: &str, node_id: [u8; 32], offered: &noded::services::Signaling) -> ServiceGrant {
    let mut nonce = [0u8; GRANT_NONCE_LEN];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);
    ServiceGrant {
        kind: kind.to_string(),
        instance: config::hex_bytes(&mint_instance(&node_id, kind, &nonce)),
        nonce: config::hex_bytes(&nonce),
        granted_unix: now_unix(),
        capabilities: offered.capabilities.clone(),
        scopes: offered.scopes.clone(),
    }
}

/// Re-mint a grant this kind ALREADY holds against a freshly reviewed hello.
///
/// The identity is kept — same instance id, same nonce — because the standing
/// is the same standing: work already placed on `compute#3f9a1c02` still names
/// a grant that exists, and a run in flight is not orphaned by the operator
/// offering one more CLI. Only the reviewed set moves, and `granted_unix`
/// moves with it: that field is the consent-epoch marker, and this IS a new
/// consent.
fn remint_grant(previous: &ServiceGrant, offered: &noded::services::Signaling) -> ServiceGrant {
    ServiceGrant {
        kind: previous.kind.clone(),
        instance: previous.instance.clone(),
        nonce: previous.nonce.clone(),
        granted_unix: now_unix(),
        capabilities: offered.capabilities.clone(),
        scopes: offered.scopes.clone(),
    }
}

/// Commit the plan: PERSIST first, then announce. THE enable code path.
///
/// The order is the opposite of what it was, and the reason is the watcher.
/// `/v1/submit` answers only once consensus has settled, so there is a real
/// interval between the submit returning and the file landing — and a watcher
/// tick inside it reads the NEW set from the chain and the OLD set from disk,
/// concludes the chain is wrong, and submits the opposite. On `enable` that
/// retracts the kind just enabled; on `disable` it re-announces consent that
/// was just revoked. "Submit first" was safe against a crash and unsafe against
/// a concurrent reader, and there is a concurrent reader now.
///
/// Persisting first makes the same interval benign: the watcher reads the new
/// set from disk and the old one from the chain, and submits exactly what this
/// verb is about to submit. Worst case is one redundant frame the module stages
/// nothing for.
///
/// It also changes what a failed announce leaves behind, and for the better:
/// the grant stands, un-announced, and the watcher retries it every tick until
/// it lands. That is the honest outcome — the operator DID consent; what failed
/// was reaching a network. The verb still reports the failure, and `service
/// status` still shows the kind as granted, so nothing is silent. The old
/// ordering's failure mode was the opposite direction and worse: an announce on
/// chain with no grant behind it, which places work on a node that then refuses
/// to serve it.
///
/// The submit carries no key: `/v1/submit` re-frames the op with the NODE's key
/// inside the node process, which is the identity `capability` keys the registry
/// on. That is why this whole family stays keyless.
pub(crate) fn commit_enable(
    workspace: &Path,
    base: &str,
    plan: &EnablePlan,
) -> Result<u64, String> {
    let announce = with_grants(workspace, |services| {
        // REPLACE on a re-consent, insert on a first grant — one record per kind
        // either way, which is what `Services::validate` enforces on the next load.
        match services
            .grants
            .binary_search_by(|existing| existing.kind.as_str().cmp(&plan.kind))
        {
            Ok(standing) => services.grants[standing] = plan.grant.clone(),
            Err(position) => services.grants.insert(position, plan.grant.clone()),
        }
        // derived HERE, from the grants as they stand now — not carried down from
        // the plan. A human may have sat on the consent prompt for a while, and
        // another `enable` on this node could have landed in the meantime;
        // announcing a set decided before that pause would retract it. Cannot be
        // refused at this point: `plan_enable` bounded the widest set these grants
        // can produce, and this is a subset of it.
        crate::announce::announced_set(&services.grants, &signaling_now(base), &plan.capacity)
            .map_err(|refusal| format!("{} was not enabled: {refusal}", plan.kind))
    })?;
    let height = crate::announce::submit(base, workspace, &announce).map_err(|error| {
        format!(
            "{} is granted but NOT announced, so nothing will be placed on it yet — this node \
             retries every {}s until it lands: {error}",
            plan.kind,
            HEARTBEAT.as_secs(),
        )
    })?;
    // the grant mint is the audit-relevant event, and `service run` installs a
    // subscriber before it can reach here, so this is recorded in daemon.log
    // and the log ring on the daemon path. The one-shot CLI verb has no
    // subscriber by design — there it is the printed output that informs.
    tracing::info!(
        target: "ducktape::service",
        kind = %plan.grant.kind,
        instance = %plan.grant.display_id(),
        capabilities = plan.grant.capabilities.len(),
        height,
        "service enabled"
    );
    Ok(height)
}

/// How often the daemon re-signals. A third of the TTL, so two consecutive
/// lost heartbeats still leave the entry alive. Also the beat the airlock
/// daemon re-asserts its gateway route on, so a daemon's two liveness signals
/// travel together — and the period the node's announce watcher samples on
/// (`crate::announce::TICK` IS this constant), because a watcher that sampled
/// on its own copy of the formula would drift from the thing it is watching.
pub(crate) const HEARTBEAT: std::time::Duration =
    std::time::Duration::from_secs(noded::services::HELLO_TTL.as_secs() / 3);

/// `ducktape service run <kind>` — the first-party launcher.
///
/// It discovers what this host can actually execute, signals that to the node,
/// offers to enable itself, and then SERVES: for `compute` the process becomes
/// the node's whole compute plane (see [`crate::compute`]). A kind with no
/// granted standing keeps signaling and executes nothing — enable is the
/// consent boundary, and a daemon can never grant itself one.
fn run_service(args: RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    let kind = args.kind.clone();
    if !kind_is_well_formed(&kind) {
        return Err(format!("{kind:?} is not a service kind (1..32 chars of [a-z0-9-])").into());
    }
    let workspace = args.workspace.dir()?;
    // a foreground daemon logs; it is not a one-shot verb printing a value —
    // and it TEES, exactly as `node run` tees `daemon.log`. Without this the
    // airlock lender's record of who spent the operator's subscription lived in
    // whatever terminal happened to launch it, which is not a record. Per KIND,
    // because compute/agent/airlock can share a workspace.
    //
    // ORDER IS LOAD-BEARING and it is the two lines above: `kind` reaches a
    // path join, so the `[a-z0-9-]{1,32}` check has to have refused a traversal
    // first. Everything before this point can only return an `Err` main prints;
    // there is no event to lose.
    noded::log::init(None, Some(workspace.join(format!("{kind}.log"))));
    // THE daemon config path (`config::ServiceConfig`): everything below is
    // derived without ever opening the node's `identity.key`, so this process
    // cannot sign as the node — which is why `/v1/submit` re-signing is a
    // boundary rather than a formality.
    let service = config::resolve_service(&args.workspace.config_file()?)?;
    // the base comes from the SAME resolved config as everything else, rather
    // than a second read of a node.toml the workspace may not contain.
    let base = service
        .http_listen
        .as_deref()
        .map(config::http_base_of)
        .ok_or("this node serves no http surface, so a service daemon has nothing to signal to")?;
    // which node this daemon serves — asked of the node, not read off its key.
    //
    // This is now the FIRST hard dependency on a live node; it used to be
    // `send_hello` below, one `backend.probe()` later. A supervisor that
    // co-starts node and daemon therefore loses the probe's duration of
    // startup slack, and a daemon that loses the race exits loudly instead of
    // spinning — which is the contract on this path, but it is a shorter fuse
    // than before. Order it after the probe only if that slack turns out to
    // matter; do not paper over it with a retry loop here.
    let node_key = node_identity(&base)?;

    let hello = discover_hello(&kind, &service, &node_key)?;
    // the FIRST hello must land: a daemon that cannot signal has nothing to
    // offer and must not sit in a retry loop pretending otherwise. A down node
    // is a loud exit, not a silent spin.
    let skew = send_hello(&base, &service.workspace, &hello)?;
    write_err(&format!(
        "{} {} · signaling to {} · offering {}\n",
        paint(GREEN, "●"),
        paint(BOLD, &kind),
        service.chain_id,
        summarize_capabilities(&kind, &hello.capabilities),
    ))?;

    warn_stale_grant(&workspace, &kind, &hello.capabilities);
    offer_enable(&workspace, &kind, args.offer(), &service, node_key, &base)?;

    // the heartbeat must outlive this call: for compute it runs BESIDE the
    // execution loop, so a long run never lets the node's catalog entry lapse
    // and report the daemon absent.
    let beat_base = base.clone();
    let watch = HelloWatch::new(&kind, &service, node_key);
    std::thread::Builder::new()
        .name("service-hello".into())
        .spawn(move || heartbeat(&beat_base, hello, watch, skew))?;

    match serve_kind(&kind, &workspace, service, &base, node_key)? {
        // nothing to execute (an ungranted kind, or a kind with no first-party
        // daemon): the signal IS the whole job, so park on it.
        Served::SignalOnly => loop {
            std::thread::park();
        },
        Served::Stopped => Ok(()),
    }
}

/// What `run` did beyond signaling. ONE discriminant so the caller never has to
/// infer "did it serve?" from a bool or an Option.
enum Served {
    /// no execution plane for this kind on this node — keep signaling.
    SignalOnly,
    /// the daemon served and its loop returned.
    Stopped,
}

/// Dispatch a signaling daemon to its execution plane, when it has one.
fn serve_kind(
    kind: &str,
    workspace: &Path,
    service: config::ServiceConfig,
    base: &str,
    node_key: [u8; 32],
) -> Result<Served, Box<dyn std::error::Error>> {
    let Some(daemon) = daemon_for(kind) else {
        // every other kind is recorded, listed and signaled — and executes
        // nothing, because no first-party daemon exists for it yet.
        return Ok(Served::SignalOnly);
    };
    let Some(grant) = load(workspace)?.grant(kind).cloned() else {
        // signaling without standing is the designed resting state, not an
        // error: the operator reviews the hello and enables when ready.
        //
        // Silent HERE, deliberately: `offer_enable` ran one call earlier and
        // has already said exactly this, with the command that fixes it. Two
        // lines saying "not enabled" — one of them without the fix — is what
        // `--no-enable` used to print.
        return Ok(Served::SignalOnly);
    };
    let http_base = base.to_string();
    match daemon {
        Daemon::Compute => crate::compute::serve(crate::compute::Compute {
            grant,
            service,
            http_base,
            node_key,
        })?,
        Daemon::Agent => crate::agent::serve(crate::agent::Agent {
            grant,
            service,
            http_base,
            node_key,
            workspace: workspace.to_path_buf(),
        })?,
        // no `node_key`: the lender signs nothing and submits no op. Its only
        // use of the node is a committed READ over `/v1`.
        Daemon::Airlock => crate::airlock::serve(crate::airlock::Airlock {
            grant,
            service,
            http_base,
            workspace: workspace.to_path_buf(),
        })?,
    }
    Ok(Served::Stopped)
}

/// A daemon's stop signal: resolves when SIGTERM or SIGINT lands (see
/// [`arm_stop_requested`]), or when the caller's own `also_stop` does — which is
/// how a test drives the real serve path.
///
/// Boxed rather than a generic parameter: compute's intake pass is `?Send` and
/// every daemon body would otherwise have to name the wrapper's opaque future
/// type. One allocation per process.
pub(crate) type Stop = std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>;

/// Build a daemon's runtime, ARM its stop signals INSIDE it, and run `body`
/// until it returns or a stop lands.
///
/// Every first-party daemon enters here — compute, agent and airlock — so the
/// ordering that matters exists in ONE place: the handlers are installed before
/// `body` is even constructed, which is what closes the window in which a
/// default-disposition SIGTERM kills a daemon that has already published a
/// gateway route — a route file outliving its owner, pointing at a port
/// anything may then bind, is the defect this exists to prevent. Copying the
/// ordering into three `serve` fns is how two of them drift; a body cannot get
/// it wrong here, because it never touches it.
///
/// Multi-thread, because every daemon needs it: compute's pool hands `Send`
/// futures to its `SpawnFn`, agent gives each session's pump and reaper a task
/// of its own, and airlock's route beat runs beside its listener.
pub(crate) fn serve_until_stopped<Body>(
    also_stop: impl std::future::Future<Output = ()> + 'static,
    body: impl FnOnce(Stop) -> Body,
) -> Result<(), Box<dyn std::error::Error>>
where
    Body: std::future::Future<Output = Result<(), Box<dyn std::error::Error>>>,
{
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        // ARMED before `body` runs, and inside the runtime, which installing a
        // signal handler REQUIRES. See [`arm_stop_requested`].
        let signalled = arm_stop_requested();
        let stop: Stop = Box::pin(async move {
            tokio::select! {
                () = signalled => {}
                () = also_stop => {}
            }
        });
        body(stop).await
    })
}

/// Install the stop handlers NOW and return a future that waits on them: SIGTERM
/// is what systemd and a killed shell send, SIGINT is Ctrl-C, and SIGHUP is a
/// dropped ssh session or a closed terminal. All three run the same teardown —
/// a daemon that dies at signal default leaves its published gateway route
/// pointing at a port anything may then bind.
///
/// The split matters. `signal()` installs the handler when it is CALLED; the
/// future it returns only waits. Building that future lazily inside a `select!`
/// would leave a window between the daemon publishing something and the first
/// poll in which a SIGTERM takes its DEFAULT disposition.
///
/// It must also be called INSIDE a runtime: `signal()` PANICS outside a reactor
/// rather than returning `Err`, so hoisting this out of
/// [`serve_until_stopped`]'s `block_on` is a production-only crash with no
/// compile-time complaint.
///
/// SIGKILL is deliberately NOT covered, and cannot be: nothing runs on a
/// `kill -9`. What survives it is only the route file, which the next start of
/// the same kind reclaims. A run's guest does NOT survive it — the VMM is a
/// child of this process, so the kernel takes it down with the daemon. That is
/// the whole reason the container era's orphan sweep is gone rather than
/// ported.
///
/// A handler that will not install is NOT fatal: the daemon then dies at signal
/// default with no teardown, which is the SIGKILL shape above. The future parks
/// so the daemon keeps owning the process.
///
/// The other half is deliberately NOT closed, and should stay open: tokio's
/// handlers remain installed after these `Signal`s drop, so a SECOND SIGTERM
/// arriving during a teardown is swallowed and SIGKILL is the operator's only
/// escape. A teardown is one file write — a hang there means an unwritable
/// workspace, which is the real problem — and a SIGTERM-count escalation is not
/// worth its complexity. Do not "finish" this.
fn arm_stop_requested() -> impl std::future::Future<Output = ()> {
    use tokio::signal::unix::{SignalKind, signal};
    let armed = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::hangup()),
    );
    async move {
        let (Ok(mut terminate), Ok(mut interrupt), Ok(mut hangup)) = armed else {
            tracing::warn!(
                target: "ducktape::service",
                reason = "signal_handler_install_failed",
                "this daemon will not run its teardown on exit"
            );
            return std::future::pending().await;
        };
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
            _ = hangup.recv() => {}
        }
    }
}

// The container-sweep machinery (`Sweep`, `SweepReport`, `sweep_own_containers`)
// lived here. It is deleted, not ported: it existed because a crashed daemon
// left containers running under a podman service at ppid 1, so a successor had
// to find and destroy them by label. A microVM's VMM is a child of the daemon
// and dies with it — including on SIGKILL, which is the case the sweep was
// written for. There is nothing left behind to sweep.

/// the grant scopes a kind's daemon actually exercises — what the consent
/// screen shows and what `service status` lists.
///
/// Each token names a capability the code really uses; inventing one the code
/// does not honor would make the consent screen a lie. The agent daemon's two
/// are exactly what its link carries: it drives interactive pty sessions on this
/// node, and it receives the consensus-resolved LENT credential records those
/// sessions run under (the airlock contact point — the secret itself never
/// leaves the lender's gateway, but the record is what points at it).
///
/// Compute's list is empty here and stays that way until its own seams are
/// audited under the same rule; that is a known gap, not a claim that compute
/// needs nothing.
fn scopes_for(kind: &str) -> Vec<String> {
    match daemon_for(kind) {
        // drives interactive pty sessions on this node, and receives the
        // consensus-resolved LENT credential records those sessions run under
        // (the airlock contact point — the secret stays at the lender's gateway,
        // but the record is what points at it).
        Some(Daemon::Agent) => vec!["term.sessions".into(), "credential.lent".into()],
        // submits signed frames through this node's key (`compute::run` posts
        // saga results and lease heartbeats to /v1/submit), and resolves the
        // same lent-credential records for the runs it executes.
        Some(Daemon::Compute) => vec!["saga.runs".into(), "credential.lent".into()],
        // the minimal-scope service, and deliberately so: it READS this node's
        // committed gateway credential records to decide a grant, and it
        // registers its loopback port under the account's `airlock` route. It
        // submits nothing, executes nothing, and touches no blob — so it asks
        // for no submit scope and no blob scope.
        Some(Daemon::Airlock) => vec!["gateway.credentials".into(), "gateway.routes".into()],
        None => Vec::new(),
    }
}

/// The capability tags a kind's daemon can actually run on this host.
///
/// A kind that SPAWNS must have a working sandbox and is refused loudly without
/// one — advertising a tag it cannot isolate a run for is worse than not
/// starting. A kind that spawns nothing must NOT be held to that: airlock lends
/// credentials and never opens a container, and a lending node is routinely a
/// laptop with no container runtime installed at all. Demanding a `[sandbox]`
/// table from it refused the exact machine shape the service is for.
///
/// A kind with no first-party daemon offers nothing for the same reason: this
/// binary executes nothing for it, so tags discovered here would be a catalog
/// entry no one can act on.
fn offered_capabilities(
    kind: &str,
    service: &config::ServiceConfig,
    node_key: &[u8; 32],
) -> Result<Vec<String>, String> {
    match daemon_for(kind) {
        Some(Daemon::Compute) | Some(Daemon::Agent) => discover_executors(kind, service, node_key),
        Some(Daemon::Airlock) | None => Ok(Vec::new()),
    }
}

/// Real discovery for a spawning kind: probe the sandbox, then read what
/// executor CLIs this host actually carries.
///
/// Discovery for the HELLO only — this set spawns nothing (and dials no
/// socket), so it is named for the kind rather than an instance id: there may
/// be no grant yet, since the hello is what the user reviews before minting
/// one. The serving set is rediscovered under `<kind>#hex8` once a grant exists.
fn discover_executors(
    kind: &str,
    service: &config::ServiceConfig,
    node_key: &[u8; 32],
) -> Result<Vec<String>, String> {
    let backend = sandbox_backend(service)?;
    // the same precondition the node's own boot enforces — a daemon must not
    // advertise tags it has no runnable sandbox for.
    backend
        .probe()
        .map_err(|error| format!("sandbox: {error}"))?;
    let providers = provider_host::discover(
        node_key,
        &workspace_config::capability_dir(&service.workspace),
        None,
        backend,
        kind,
    )?;
    Ok(providers.capabilities())
}

/// Build this host's hello: what it IS, and what it can actually run.
///
/// The capability tags come from real discovery, not a config list — that is
/// the whole point of signaling before enabling.
fn discover_hello(
    kind: &str,
    service: &config::ServiceConfig,
    node_key: &[u8; 32],
) -> Result<noded::services::Hello, String> {
    Ok(noded::services::Hello {
        kind: kind.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        // metadata, so an unidentifiable build says so instead of refusing to
        // start. A tarball or vendored build signals and serves like any other.
        build: noded::services::build_identity_or_unknown().to_string(),
        // the probe+discovery moved into `offered_capabilities`, because a kind
        // that spawns NOTHING must not demand a container runtime to say hello.
        capabilities: offered_capabilities(kind, service, node_key)?,
        scopes: scopes_for(kind),
        // no first-party daemon declares a need. An interactive pty session is
        // self-contained, and so is lending a credential: both are useful on a
        // network with no compute capacity anywhere. They are siblings, not
        // layers.
        needs: Vec::new(),
    })
}

/// Warn when a live hello offers capabilities this standing grant did not
/// include. A grant is consent, so discovery never expands it implicitly.
fn warn_stale_grant(workspace: &Path, kind: &str, offered: &[String]) {
    let Ok(Some(grant)) = grant_for(workspace, kind) else {
        return;
    };
    let Some(missing) = stale_grant_hint(kind, Some(&grant), offered) else {
        return;
    };
    tracing::warn!(
        target: "ducktape::service",
        event = "service_grant_stale",
        reason = "offered_capabilities_not_granted",
        %kind,
        "{missing}"
    );
}

/// Explain why a changed offer is unavailable without changing consent.
fn stale_grant_hint(
    kind: &str,
    standing: Option<&ServiceGrant>,
    offered: &[String],
) -> Option<String> {
    let standing = standing?;
    let missing: Vec<&str> = offered
        .iter()
        .map(String::as_str)
        .filter(|capability| {
            !standing
                .capabilities
                .iter()
                .any(|granted| granted == capability)
        })
        .collect();
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "offered capabilities {} are not in the standing grant; they are unavailable until re-consent with: ducktape service enable {kind}",
        missing.join(", ")
    ))
}

/// Signal once, and report whether the node that answered is on our build.
///
/// The node returns its own stamp in the OK body. That is what makes skew a
/// diagnostic the daemon can NAME rather than a refusal it merely suffers.
/// When either side cannot identify its build the answer is
/// [`Skew::Unknown`] — which says nothing and warns about nothing, rather than
/// inventing a disagreement out of two "unknown"s.
///
/// The hello carries the node's service-link token, read out of `workspace`
/// per call and never latched: a node restart mints a fresh one, and a
/// heartbeat holding a stale token would be refused until the daemon
/// restarted.
fn send_hello(
    base: &str,
    workspace: &std::path::Path,
    hello: &noded::services::Hello,
) -> Result<Skew, String> {
    let token = noded::services::read_link_token(workspace)?;
    let body = crate::node_http::post_json(
        base,
        "/v1/services/hello",
        &serde_json::to_value(hello).unwrap(),
        (noded::services::LINK_TOKEN_HEADER, &token),
    )
    .map_err(|error| error.to_string())?;
    Ok(Skew::between(
        &hello.build,
        body.get("build").and_then(|v| v.as_str()),
    ))
}

/// Whether two build stamps are the same build. ONE discriminant so the
/// latches that compare them (the service heartbeat below; the mesh peer's
/// stamp in `replica::park`) compare states rather than juggling flags.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Skew {
    /// same stamp on both sides.
    Matched,
    /// the two stamps differ — the ordinary dev loop (rebuild the node, leave
    /// yesterday's daemon running). Informational: nothing is refused for it.
    Skewed,
    /// at least one side could not name its build, so there is nothing to
    /// compare. Never warned about — an unknown build is not evidence of skew.
    Unknown,
}

impl Skew {
    pub(crate) fn between(mine: &str, theirs: Option<&str>) -> Skew {
        let unknown = noded::services::UNKNOWN_BUILD;
        let Some(theirs) = theirs.filter(|it| *it != unknown) else {
            return Skew::Unknown;
        };
        if mine == unknown {
            return Skew::Unknown;
        }
        match mine == theirs {
            true => Skew::Matched,
            false => Skew::Skewed,
        }
    }
}

/// Log a build-skew TRANSITION and nothing else.
///
/// Latched against the last observed state, the way `CapabilityAnnouncer`
/// latches `grant_unreadable`: this runs on every heartbeat, and one warn per
/// beat would evict the ring it is supposed to help you read.
fn note_skew(kind: &str, last: &mut Option<Skew>, now: Skew) {
    if last.replace(now) == Some(now) {
        return;
    }
    match now {
        Skew::Skewed => tracing::warn!(
            target: "ducktape::service",
            %kind,
            reason = "build_skew",
            "this daemon and its node are different builds; restart the daemon \
             from the node's build if they disagree about the protocol"
        ),
        Skew::Matched => {
            tracing::info!(target: "ducktape::service", %kind, "daemon and node builds agree")
        }
        Skew::Unknown => {}
    }
}

/// Keep signaling without a grant, and say why exactly once.
///
/// THE single exit for "this daemon cannot be enabled right now". Both halves
/// of enabling — planning it and committing it — route here, because the rule
/// is the same for both and a second copy is how one of them comes to exit the
/// process instead.
fn decline(kind: &str, hint: &str, reason: &str) -> Result<(), Box<dyn std::error::Error>> {
    tracing::warn!(
        target: "ducktape::service",
        kind = %kind,
        reason = "enable_not_announced",
        "{reason}"
    );
    write_err(hint)
}

/// Offer enablement once, at startup, per the posture the operator chose.
fn offer_enable(
    workspace: &Path,
    kind: &str,
    offer: EnableOffer,
    service: &config::ServiceConfig,
    node_id: [u8; 32],
    base: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if load(workspace)?.grant(kind).is_some() {
        // already granted: straight to serving, never a prompt.
        return Ok(());
    }
    // THE one "not enabled" line — `serve_kind` prints none, so this carries
    // both halves: what it means, and what to type.
    let hint = format!(
        "  {} — nothing will execute; enable it with: ducktape service enable {kind}\n",
        paint(YELLOW, "not enabled")
    );
    let planned = match offer {
        // a unit file and a pipe have no one to ask; say it once and serve.
        EnableOffer::Never => return write_err(&hint),
        EnableOffer::AskIfTty if !crate::tty::stdin_is_tty() => return write_err(&hint),
        EnableOffer::Always | EnableOffer::AskIfTty => {
            plan_enable(workspace, kind, service, node_id)
        }
    };
    // PLANNING can fail too, and it must not be fatal either — this is where
    // the tag-legality and cap refusals live, so a host whose capability spec
    // dir carries one registry-illegal tag would otherwise be unable to
    // `service run` at all. It would exit here, BEFORE the heartbeat thread is
    // spawned, and signal nothing: the operator loses the daemon, the hello,
    // and the `service list` row that would have told them why.
    let plan = match planned {
        Ok(plan) => plan,
        Err(error) => return decline(kind, &hint, &error),
    };
    let asked = matches!(offer, EnableOffer::AskIfTty);
    if asked {
        write_err(&render_enable_summary(&plan))?;
        let question = format!("Enable {kind} on this node now?");
        // declining leaves the daemon running and signaling — never re-asked.
        if !crate::tty::confirm(&question, true, false)? {
            return write_err(&hint);
        }
    }
    // A failed announce must NEVER stop the daemon. Enabling is a transaction
    // now, so it can fail for reasons that have nothing to do with this process
    // — a node not yet admitted to its network, a chain not finalizing — and a
    // daemon that exited on one of those would take down the very signal the
    // operator needs in order to retry. Say it once, keep signaling, serve
    // nothing (no grant was written): the same resting state as declining the
    // prompt. Deliberately NOT a retry loop — the operator's next
    // `service enable` is the retry.
    match commit_enable(workspace, base, &plan) {
        Ok(height) => write_err(&format!(
            "  {} enabled {} · announced at height {height}\n",
            paint(GREEN, ServiceState::Enabled.glyph()),
            plan.grant.display_id()
        )),
        Err(error) => decline(kind, &hint, &error),
    }
}

/// What the heartbeat needs to re-derive its hello, and the clock it decides
/// on: the executors directory's newest mtime — the SAME signal
/// [`provider_host::executor_image::ensure`] rebuilds the guest image from, so
/// what a node ANNOUNCES and what its guest can EXEC can never disagree about
/// whether a CLI was installed.
struct HelloWatch {
    kind: String,
    service: config::ServiceConfig,
    node_key: [u8; 32],
    clock: ExecutorsClock,
}

/// The staleness clock alone: a directory and the newest mtime last seen in it.
///
/// Separate from [`HelloWatch`] because THIS is the decision — "did the
/// operator's executors change?" — and it is answerable without a sandbox
/// backend, a node key, or a running daemon, which is what makes it testable.
struct ExecutorsClock {
    dir: Option<std::path::PathBuf>,
    seen: Option<std::time::SystemTime>,
}

impl ExecutorsClock {
    /// `dir` is `None` for a backend with no executors directory; the clock
    /// then never reports a move.
    fn new(dir: Option<std::path::PathBuf>) -> Self {
        let seen = dir.as_deref().and_then(newest_mtime_of);
        Self { dir, seen }
    }

    /// Has the directory changed since the last look? Marks the new state, so
    /// one move is reported once.
    fn moved(&mut self) -> bool {
        let Some(dir) = self.dir.as_deref() else {
            return false;
        };
        let newest = newest_mtime_of(dir);
        let moved = newest != self.seen;
        self.seen = newest;
        moved
    }
}

impl HelloWatch {
    /// Build the watch beside the hello that was just derived, so the clock
    /// starts at the mtime that hello was read at.
    fn new(kind: &str, service: &config::ServiceConfig, node_key: [u8; 32]) -> Self {
        let dir = match sandbox_backend(service) {
            Ok(provider_host::SandboxBackend::MicroVm { executors, .. }) => Some(executors),
            // no [sandbox] table and the bare test backend both mean there is
            // no directory to watch. A daemon that got this far already has
            // whatever set it will ever offer.
            _ => None,
        };
        Self {
            kind: kind.to_string(),
            service: service.clone(),
            node_key,
            clock: ExecutorsClock::new(dir),
        }
    }

    /// Re-derive `hello` IF the executors directory moved since the last look.
    /// A plain `stat` of the directory's entries per beat; the probe and the
    /// PATH walk behind `discover_hello` run only on a real change, which also
    /// keeps `sandbox_backend_ready` a lifecycle event rather than a per-beat
    /// log bomb.
    fn refresh(&mut self, hello: &mut noded::services::Hello) {
        if !self.clock.moved() {
            return;
        }
        // a discovery that fails leaves the last good hello standing: the
        // executors dir is mid-install, or the sandbox went unprobeable for a
        // moment. Retracting on a transient read would flap the announce.
        let derived = match discover_hello(&self.kind, &self.service, &self.node_key) {
            Ok(derived) => derived,
            Err(error) => {
                tracing::warn!(
                    target: "ducktape::service",
                    kind = %self.kind,
                    reason = "rediscover_failed",
                    "executors changed but re-discovery failed; still offering the previous \
                     set: {error}"
                );
                return;
            }
        };
        if derived.capabilities == hello.capabilities {
            return;
        }
        tracing::info!(
            target: "ducktape::service",
            kind = %self.kind,
            offering = %summarize_capabilities(&self.kind, &derived.capabilities),
            "the executors directory changed — offering a new set"
        );
        warn_stale_grant(&self.service.workspace, &self.kind, &derived.capabilities);
        *hello = derived;
    }
}

/// [`provider_host::executor_image::newest_mtime`], with a read error folded
/// into `None`: an unreadable directory is not a reason to stop beating, and
/// the next beat looks again.
fn newest_mtime_of(dir: &std::path::Path) -> Option<std::time::SystemTime> {
    provider_host::executor_image::newest_mtime(dir)
        .ok()
        .flatten()
}

/// Keep the signal alive until the process is stopped.
///
/// A failed beat is not fatal: the node may be restarting, and the entry simply
/// ages out and returns. Logged on the first failure and every 30th after it,
/// carrying the attempt count — an unconditional warn here would be a log bomb
/// on a node that stays down.
///
/// The hello is RE-DERIVED whenever the executors directory changes, so
/// `ducktape agent install`/uninstall on a running node reaches the node's
/// catalog within one beat. It used to be computed once at startup and re-sent
/// verbatim forever, which left a daemon announcing an executor it no longer
/// carries until someone restarted the process — the node lying about what it
/// can run, in the direction that strands a buyer's run.
fn heartbeat(
    base: &str,
    mut hello: noded::services::Hello,
    mut watch: HelloWatch,
    initial: Skew,
) -> ! {
    const LOG_EVERY: u64 = 30;
    let mut failures: u64 = 0;
    // seeded from the startup beat, so skew is named once at startup and then
    // only when it CHANGES — never once per beat.
    let mut skew: Option<Skew> = None;
    note_skew(&hello.kind, &mut skew, initial);
    loop {
        std::thread::sleep(HEARTBEAT);
        watch.refresh(&mut hello);
        match send_hello(base, &watch.service.workspace, &hello) {
            Ok(observed) => {
                if failures > 0 {
                    tracing::info!(target: "ducktape::service", kind = %hello.kind, "signal restored");
                }
                failures = 0;
                note_skew(&hello.kind, &mut skew, observed);
            }
            Err(error) => {
                failures += 1;
                if failures == 1 || failures.is_multiple_of(LOG_EVERY) {
                    tracing::warn!(
                        target: "ducktape::service",
                        kind = %hello.kind,
                        attempts = failures,
                        reason = "hello_failed",
                        "service signal not reaching the node: {error}"
                    );
                }
            }
        }
    }
}

fn enable(args: EnableArgs) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = args.workspace.dir()?;
    // the SAME resolved value `run` uses, from the SAME file this verb was
    // pointed at. Reading `<workspace>/node.toml` here instead would be a
    // second read of a node.toml the workspace may not contain — with
    // `--config <elsewhere>/alt.toml` that is a different node, a different
    // port, or no file at all.
    let service = config::resolve_service(&args.workspace.config_file()?)?;
    // the consent screen names the node, so it asks the node — the same keyless
    // route `service run` takes. `enable` already requires a live hello, so a
    // reachable node is a precondition either way.
    let base = service
        .http_listen
        .as_deref()
        .map(config::http_base_of)
        .ok_or("this node serves no http surface, so `enable` cannot ask it who it is")?;
    let node_id = node_identity(&base)?;
    let plan = plan_enable(&workspace, &args.kind, &service, node_id)?;

    write_err(&render_enable_summary(&plan))?;
    let question = match &plan.previous {
        Some(standing) => format!("Re-consent and keep {}?", standing.display_id()),
        None => format!("Enable {} on this node?", plan.kind),
    };
    if !crate::tty::confirm(&question, crate::tty::stdin_is_tty(), args.yes)? {
        write_err("not enabled\n")?;
        return Ok(());
    }

    let chain_id = plan.chain_id.clone();
    let height = commit_enable(&workspace, &base, &plan)?;
    // stdout is the id alone, so `$(ducktape service enable compute)` is the
    // instance id and nothing else; the prose goes to stderr.
    println!("{}", plan.grant.display_id());
    write_err(&format!(
        "{} {} {} · {}\n",
        paint(GREEN, ServiceState::Enabled.glyph()),
        match plan.previous {
            Some(_) => "grant updated",
            None => "enabled",
        },
        plan.grant.display_id(),
        announced_line(&chain_id, height),
    ))?;
    if daemon_for(&plan.kind).is_some() {
        // RESTART, not start. `plan_enable` refuses a kind that is not
        // signaling, so the daemon is running by construction every time this
        // prints — and it read its grant once at startup, so the process that
        // is up right now still executes nothing. "start it with" was advice
        // for a situation that cannot occur on this line.
        //
        // It is printed on a re-consent too, and for the same reason: the
        // SERVING half reads the grant at startup (`serve_kind`), so a widened
        // grant is announced before the running process will execute it.
        write_err(&restart_advice(&plan.kind))?;
    }
    Ok(())
}

fn restart_advice(kind: &str) -> String {
    format!(
        "  restart the daemon to pick the grant up: ^C, then ducktape service run {kind}; for service-managed daemons, restart the systemd/launchd unit instead\n"
    )
}

/// How an on-chain announce reads to someone who has never heard the word
/// "height": WHERE it landed and WHICH block. The number is the same one
/// `node status` reports, so the two surfaces stay comparable.
fn announced_line(chain_id: &str, height: u64) -> String {
    format!("announced on {chain_id} at block {height}")
}

fn disable(args: KindArgs) -> Result<(), Box<dyn std::error::Error>> {
    let kind = args.kind;
    let workspace = args.workspace.dir()?;
    let service = config::resolve_service(&args.workspace.config_file()?)?;
    let base = service
        .http_listen
        .as_deref()
        .map(config::http_base_of)
        .ok_or(
            "this node serves no http surface, so `disable` cannot retract the announce. \
             Revoking consent is a transaction now, so it needs a reachable node and a \
             finalizing chain — a grant cannot be revoked while the node is down",
        )?;
    // PERSIST first, retract second — the same order `commit_enable` uses and
    // for the same reason: a watcher tick between the two must never read a
    // revoked grant off the chain and a live one off disk, and re-announce
    // consent the operator has just withdrawn. Revocation lands on disk first,
    // so the worst a concurrent tick can do is retract it slightly early.
    let (retired, announce) = with_grants(&workspace, |services| {
        let position = services
            .grants
            .iter()
            .position(|grant| grant.kind == kind)
            .ok_or_else(|| format!("{kind} is not enabled in {}", workspace.display()))?;
        let retired = services.grants.remove(position);
        // A refusal here cannot come from this removal (a disable only shrinks
        // the set); it would mean the file already held something the registry
        // refuses, which `Services::validate` prevents on load.
        let announce = crate::announce::announced_set(
            &services.grants,
            &signaling_now(&base),
            &service.sandbox_capacity,
        )
        .map_err(|refusal| format!("{kind} was not disabled: {refusal}"))?;
        Ok((retired, announce))
    })?;
    let height = crate::announce::submit(&base, &workspace, &announce).map_err(|error| {
        format!(
            "{kind}'s grant is revoked but the announce was NOT retracted — this node retries \
             every {}s until it lands: {error}",
            HEARTBEAT.as_secs(),
        )
    })?;
    println!("{}", retired.display_id());
    write_err(&format!(
        "disabled {kind}; {} is retired (a re-enable mints a fresh id) · retracted on {} at \
         block {height}\n",
        retired.display_id(),
        service.chain_id,
    ))?;
    // the announce is already retracted above, but a RUNNING daemon keeps
    // executing the work it already holds: it read its grant once, at its own
    // boot. Stopping it is the operator's act, so say so rather than implying
    // revocation is instant.
    if daemon_for(&kind).is_some() {
        write_err(&format!(
            "  stop the daemon too: a running `service run {kind}` keeps \
             serving what it already holds\n"
        ))?;
    }
    Ok(())
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn enable_reads_the_resolved_node_without_a_workspace_config() {
        use std::io::{Read, Write};
        let (dir, mut service) = planning_workspace(&[]);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        service.http_listen = Some(listener.local_addr().unwrap().to_string());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let count = stream.read(&mut request).unwrap();
            assert!(
                std::str::from_utf8(&request[..count])
                    .unwrap()
                    .starts_with("GET /v1/services ")
            );
            let body = serde_json::json!({"signaling": [hello_offering("compute", &["claude"])], "build": "test"}).to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        assert!(!dir.path().join("node.toml").exists());
        let plan = plan_enable(dir.path(), "compute", &service, [1; 32]).unwrap();
        assert_eq!(plan.grant.capabilities, ["claude"]);
        server.join().unwrap();
    }

    /// A daemon used to compute its hello ONCE and re-send it verbatim until
    /// the process was restarted, so `ducktape agent install`/uninstall on a
    /// running node never reached the node's catalog. This is the clock that
    /// unfroze it — and the asserts avoid the wall clock entirely (an empty
    /// directory and a populated one differ as `None` vs `Some`, whatever the
    /// filesystem's mtime granularity is).
    #[test]
    fn the_executors_clock_reports_a_change_once_and_a_quiet_directory_never() {
        let dir = std::env::temp_dir().join(format!("dt-exec-clock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");

        let mut clock = ExecutorsClock::new(Some(dir.clone()));
        assert!(
            !clock.moved(),
            "an unchanged directory is quiet — this runs every beat"
        );

        std::fs::write(dir.join("codex"), b"#!/bin/sh\n").expect("install an executor");
        assert!(clock.moved(), "an install is a move");
        assert!(
            !clock.moved(),
            "and it is reported ONCE — a re-derive per beat would re-probe forever"
        );

        std::fs::remove_file(dir.join("codex")).expect("uninstall it");
        assert!(
            clock.moved(),
            "an uninstall is a move too: this is the direction that strands a buyer's run"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The bare test backend has no executors directory. A clock with nothing
    /// to watch must never claim a move, or the daemon would re-probe forever.
    #[test]
    fn a_clock_with_no_directory_never_moves() {
        let mut clock = ExecutorsClock::new(None);
        assert!(!clock.moved());
        assert!(!clock.moved());
    }

    #[test]
    fn a_stale_grant_names_new_offers_and_the_exact_reconsent_command() {
        let mut empty = grant("compute", NODE_A);
        empty.capabilities = Vec::new();
        let offered = ["claude", "codex", "pi"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        let hint = stale_grant_hint("compute", Some(&empty), &offered)
            .expect("an empty-to-nonempty addition needs a hint");
        assert!(
            offered.iter().all(|capability| hint.contains(capability)),
            "every missing offer is named: {hint}"
        );
        assert!(
            hint.contains("ducktape service enable compute"),
            "the remedy is exact: {hint}"
        );

        let mut standing = empty.clone();
        standing.capabilities = vec!["claude".into(), "pi".into()];
        let mut all = standing.clone();
        all.capabilities = offered.clone();
        assert!(stale_grant_hint("compute", Some(&all), &offered).is_none());
        let reordered = ["pi", "claude"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        assert!(stale_grant_hint("compute", Some(&all), &reordered).is_none());
        assert!(
            stale_grant_hint("compute", Some(&all), &["claude".into()]).is_none(),
            "an offered subset needs no warning"
        );
        assert!(
            stale_grant_hint("compute", None, &offered).is_none(),
            "the existing no-grant instructions cover a first grant"
        );

        let mixed = ["claude", "codex"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        let mixed_hint = stale_grant_hint("compute", Some(&standing), &mixed)
            .expect("an addition mixed with a removal still needs a hint");
        assert!(
            mixed_hint.contains("codex"),
            "the addition is named: {mixed_hint}"
        );
    }

    #[test]
    fn stale_grant_warning_is_on_startup_and_offer_change_not_heartbeat() {
        let source = include_str!("services.rs");
        let startup = source
            .split("fn run_service(")
            .nth(1)
            .and_then(|rest| rest.split("\n/// ").next())
            .expect("run_service has a body");
        assert!(
            startup.contains("warn_stale_grant"),
            "startup must check the grant"
        );

        let refresh = source
            .split("fn refresh(")
            .nth(1)
            .and_then(|rest| rest.split("\n/// ").next())
            .expect("HelloWatch::refresh has a body");
        assert!(
            refresh.contains("warn_stale_grant"),
            "changed offers must check the grant"
        );

        let heartbeat = source
            .split("fn heartbeat(")
            .nth(1)
            .and_then(|rest| rest.split("\nfn enable(").next())
            .expect("heartbeat has a body");
        assert!(
            !heartbeat.contains("warn_stale_grant"),
            "a heartbeat must not repeat the lifecycle warning"
        );
    }

    #[test]
    fn service_enable_restart_advice_covers_foreground_and_units() {
        let advice = restart_advice("compute");
        assert!(advice.contains("^C, then ducktape service run compute"));
        assert!(advice.contains("restart the systemd/launchd unit"));
        assert!(advice.contains("service-managed"));
    }

    fn row(kind: &str) -> ServiceRow {
        ServiceRow {
            kind: kind.into(),
            state: ServiceState::Signaling,
            instance: None,
            version: None,
            build: None,
            capabilities: Vec::new(),
            scopes: Vec::new(),
            needs: Vec::new(),
            unmet_needs: Vec::new(),
        }
    }

    /// `service status compute` is the invocation everyone types — `enable`,
    /// `disable` and `run` all take a KIND there — and it answered
    /// `error: unexpected argument 'compute' found`, which reads like the
    /// command is broken rather than the call.
    #[test]
    fn a_read_verb_takes_the_kind_every_other_service_verb_takes() {
        let all = || vec![row("agent"), row("compute")];

        assert_eq!(only_kind(all(), None).unwrap().len(), 2, "no kind = all");
        let one = only_kind(all(), Some("compute")).unwrap();
        assert_eq!(
            one.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(),
            ["compute"]
        );

        // a kind this node HAS heard of, just not that one: name what there is.
        let wrong = only_kind(all(), Some("airlock")).expect_err("not on this node");
        assert!(
            wrong.contains("agent") && wrong.contains("compute"),
            "{wrong}"
        );

        // nothing at all: the only useful answer is how to start it.
        let empty = only_kind(Vec::new(), Some("compute")).expect_err("nothing here");
        assert!(
            empty.contains("ducktape service run compute"),
            "an empty node must be told what to run: {empty}"
        );
    }

    /// A 37-tag capability list is not a line, it is a wall — it pushes the
    /// `signaling to <chain>` half of the same banner off the terminal. The
    /// summary keeps the count honest and says where the full list lives.
    #[test]
    fn the_startup_banner_summarizes_a_wall_of_tags_but_never_a_short_list() {
        let few: Vec<String> = (0..BANNER_TAGS).map(|i| format!("tag{i}")).collect();
        assert_eq!(
            summarize_capabilities("compute", &few),
            few.join(", "),
            "a list that already fits is printed whole"
        );

        let many: Vec<String> = (0..37).map(|i| format!("tag{i}")).collect();
        let line = summarize_capabilities("compute", &many);
        assert!(line.contains("tag0") && line.contains("+31 more"), "{line}");
        assert!(
            !line.contains("tag36"),
            "the point is that it stops: {line}"
        );
        assert!(
            line.contains("ducktape service status compute"),
            "and says where the whole list is: {line}"
        );
    }

    /// The ungranted daemon says "not enabled" ONCE. `offer_enable` owns that
    /// line because it is the half that also knows the command to fix it;
    /// `serve_kind` used to print a second, fix-less copy right after it, which
    /// is what `--no-enable` showed you.
    ///
    /// Source-parsed rather than eyeballed: the two writers are in different
    /// functions, so nothing else can notice them drifting back apart.
    #[test]
    fn the_ungranted_daemon_says_not_enabled_exactly_once() {
        let source = include_str!("services.rs");
        let serve_kind = source
            .split("fn serve_kind(")
            .nth(1)
            .and_then(|rest| rest.split("\n/// ").next())
            .expect("serve_kind has a body");
        assert!(
            !serve_kind.contains("write_err("),
            "serve_kind must print nothing about the grant — offer_enable already \
             said it, WITH the command that fixes it"
        );
        let offer = source
            .split("fn offer_enable(")
            .nth(1)
            .and_then(|rest| rest.split("\n/// ").next())
            .expect("offer_enable has a body");
        assert!(
            offer.contains("nothing will execute") && offer.contains("ducktape service enable"),
            "the one line must carry both halves: what it means and what to type"
        );
    }

    /// The stop arming, driven through the REAL entry every daemon serves from
    /// — not a replica of the call, which is the guard that once shipped green
    /// while the production ordering was broken.
    ///
    /// The body raises a REAL SIGTERM at this process the moment it starts, i.e.
    /// at the first instant a daemon could have published a route or booted a
    /// run's VM. Delete the arming and the default disposition
    /// ends the test binary right there; hoist it out of `block_on` and
    /// `signal()` panics for want of a reactor. It then returns only because the
    /// armed handler is WIRED into the stop the body was handed — `also_stop` is
    /// `pending`, so nothing else can resolve it.
    ///
    /// It waits on the signal, never on a clock.
    #[test]
    fn a_daemon_starts_with_its_stop_handlers_already_armed() {
        let served = serve_until_stopped(std::future::pending(), |stop| async move {
            // SAFETY: `raise(3)` signals this process and has no memory effects.
            // Delivery is synchronous, so an unarmed SIGTERM ends the process
            // here rather than at some later, harder-to-read moment.
            unsafe { libc::raise(libc::SIGTERM) };
            stop.await;
            Ok(())
        });
        served.expect("a signalled daemon stops cleanly");
    }

    /// The shape, guarded where a comment cannot reach: every daemon must ENTER
    /// through [`serve_until_stopped`]. One that builds its own runtime again is
    /// one whose gateway route outlives it — and no test above would notice,
    /// because the arming it skipped still works.
    #[test]
    fn every_daemon_enters_through_the_one_armed_entry() {
        for (daemon, source) in [
            ("compute", include_str!("compute/mod.rs")),
            ("agent", include_str!("agent/mod.rs")),
            ("airlock", include_str!("airlock.rs")),
        ] {
            assert!(
                source.contains("serve_until_stopped"),
                "the {daemon} daemon does not serve through the armed entry"
            );
            assert!(
                !source.contains("tokio::runtime::Builder"),
                "the {daemon} daemon builds its own runtime, so its stop handlers are its own to get wrong"
            );
        }
    }

    const NODE_A: [u8; 32] = [7u8; 32];
    const NODE_B: [u8; 32] = [9u8; 32];
    const NONCE: [u8; GRANT_NONCE_LEN] = [3u8; GRANT_NONCE_LEN];

    fn grant(kind: &str, instance: [u8; 32]) -> ServiceGrant {
        ServiceGrant {
            kind: kind.into(),
            instance: config::hex_bytes(&instance),
            nonce: config::hex_bytes(&NONCE),
            granted_unix: 1_700_000_000,
            capabilities: vec!["agent.claude".into()],
            scopes: vec!["cred:read".into()],
        }
    }

    fn signaling(kind: &str) -> noded::services::Signaling {
        noded::services::Signaling {
            kind: kind.into(),
            version: "1.2.3".into(),
            build: "deadbeef".into(),
            capabilities: vec!["agent.codex".into()],
            scopes: vec![],
            needs: vec![],
        }
    }

    /// a workspace holding `grants`, plus a `ServiceConfig` pointed at it.
    fn planning_workspace(
        grants: &[(&str, &[&str])],
    ) -> (tempfile::TempDir, config::ServiceConfig) {
        let dir = tempfile::tempdir().expect("scratch workspace");
        let mut services = Services::default();
        for (kind, capabilities) in grants {
            services.grants.push(ServiceGrant {
                kind: (*kind).into(),
                instance: "aa".repeat(32),
                nonce: "bb".repeat(16),
                granted_unix: 1,
                capabilities: capabilities.iter().map(|t| t.to_string()).collect(),
                scopes: Vec::new(),
            });
        }
        services.grants.sort_by(|a, b| a.kind.cmp(&b.kind));
        save(dir.path(), &services).expect("write grants");
        let service = config::ServiceConfig {
            workspace: dir.path().to_path_buf(),
            storage_dir: dir.path().to_path_buf(),
            chain_id: "test#00000000".into(),
            http_listen: Some("127.0.0.1:1".into()),
            sandbox: None,
            sandbox_capacity: Default::default(),
        };
        (dir, service)
    }

    fn hello_offering(kind: &str, capabilities: &[&str]) -> noded::services::Signaling {
        noded::services::Signaling {
            kind: kind.into(),
            version: "1".into(),
            build: "b".into(),
            capabilities: capabilities.iter().map(|t| t.to_string()).collect(),
            scopes: Vec::new(),
            needs: Vec::new(),
        }
    }

    #[test]
    fn planning_refuses_a_tag_the_registry_would_reject() {
        // the hello boundary admits a space; the registry does not. The refusal
        // has to land HERE, before the operator is asked to consent to a set
        // this node can never announce.
        let (dir, service) = planning_workspace(&[]);
        let error = plan_enable_from(
            dir.path(),
            "compute",
            &service,
            NODE_A,
            vec![hello_offering("compute", &["Claude Sonnet"])],
        )
        .expect_err("an illegal tag must refuse the plan");
        assert!(
            error.contains("Claude Sonnet"),
            "the offending tag is named: {error}"
        );
    }

    #[test]
    fn planning_bounds_the_cap_independently_of_who_is_signaling() {
        // THE order-dependence guard. `agent` is already granted a full budget
        // of executors but its daemon is DOWN, so it contributes nothing to the
        // live signaling set. Bounding the live union would let this enable
        // through and let the total cross the cap later, when `agent` restarts
        // and no verb is running to refuse it.
        let many: Vec<String> = (0..63).map(|n| format!("e{n}")).collect();
        let borrowed: Vec<&str> = many.iter().map(String::as_str).collect();
        let (dir, service) = planning_workspace(&[("agent", &borrowed)]);
        let error = plan_enable_from(
            dir.path(),
            "compute",
            &service,
            NODE_A,
            // only compute is signaling; agent is absent.
            vec![hello_offering("compute", &["codex"])],
        )
        .expect_err("the widest union crosses the cap, so the plan must refuse");
        assert!(
            error.contains("at most") || error.contains("64"),
            "the refusal names the registry cap: {error}"
        );
    }

    #[test]
    fn planning_succeeds_when_the_widest_union_fits() {
        // the same shape, under the cap — so the test above is pinning the
        // bound rather than a plan that could never succeed.
        let (dir, service) = planning_workspace(&[("agent", &["claude"])]);
        let plan = plan_enable_from(
            dir.path(),
            "compute",
            &service,
            NODE_A,
            vec![hello_offering("compute", &["codex"])],
        )
        .expect("a union well under the cap plans fine");
        assert_eq!(plan.grant.kind, "compute");
        assert_eq!(plan.grant.capabilities, vec!["codex".to_string()]);
    }

    /// #1242's consent half. `agent install` widens what a host CAN run;
    /// `announced_set` intersects the live hello with the GRANT, so the new tag
    /// stays unannounced until the operator consents to it again. `enable` used
    /// to refuse a kind that already had a grant ("disable first"), which made
    /// that a revoke-and-regrant: a window with no consent on chain and a new
    /// instance id for every added CLI.
    #[test]
    fn re_enabling_re_consents_in_place_and_keeps_the_instance_id() {
        let (dir, service) = planning_workspace(&[("compute", &["claude"])]);
        let standing = load(dir.path())
            .expect("the fixture wrote a grant")
            .grant("compute")
            .cloned()
            .expect("compute is granted");

        let plan = plan_enable_from(
            dir.path(),
            "compute",
            &service,
            NODE_A,
            // the operator has since installed codex, and removed nothing.
            vec![hello_offering("compute", &["claude", "codex"])],
        )
        .expect("a second enable is a RE-CONSENT, not a refusal");

        assert_eq!(
            plan.previous.as_ref().map(|p| p.display_id()),
            Some(standing.display_id()),
            "the screen is a diff against the grant that already stands"
        );
        assert_eq!(
            plan.grant.instance, standing.instance,
            "the id is KEPT — work already placed on it still names a live grant"
        );
        assert_eq!(
            plan.grant.nonce, standing.nonce,
            "and so is the nonce the id is reproducible from"
        );
        assert_eq!(
            plan.grant.capabilities,
            vec!["claude".to_string(), "codex".to_string()],
            "the newly installed executor is what the re-consent is FOR"
        );
    }

    /// The diff is the question being asked, so it must name the new tag — and
    /// the dropped one, which is the direction that strands a buyer's run.
    #[test]
    fn the_re_consent_screen_marks_what_changed_in_both_directions() {
        let (dir, service) = planning_workspace(&[("compute", &["aider", "claude"])]);
        let plan = plan_enable_from(
            dir.path(),
            "compute",
            &service,
            NODE_A,
            vec![hello_offering("compute", &["claude", "codex"])],
        )
        .expect("re-consent plans");

        // through the same adapter a pipe gets, so the asserts read the TEXT
        // rather than tripping over the per-tag styling.
        let screen = through_anstream(&render_enable_summary(&plan), anstream::ColorChoice::Never);
        assert!(screen.contains("already enabled as"), "{screen}");
        assert!(
            screen.contains("+ cap:codex  (new)"),
            "the added tag is marked: {screen}"
        );
        assert!(
            screen.contains("- cap:aider  (no longer installed)"),
            "the tag that went away is marked, and says why: {screen}"
        );
        assert!(
            screen.contains("  cap:claude\n"),
            "an unchanged tag rides along unmarked: {screen}"
        );
    }

    /// Re-running `enable` with nothing installed or removed must not read as a
    /// change — the operator should see "nothing to do", not a diff of one.
    #[test]
    fn re_consenting_an_unchanged_set_says_so() {
        let (dir, service) = planning_workspace(&[("compute", &["claude"])]);
        let plan = plan_enable_from(
            dir.path(),
            "compute",
            &service,
            NODE_A,
            vec![hello_offering("compute", &["claude"])],
        )
        .expect("re-consent plans");
        let screen = through_anstream(&render_enable_summary(&plan), anstream::ColorChoice::Never);
        assert!(screen.contains("unchanged"), "{screen}");
        assert!(!screen.contains("CHANGED"), "{screen}");
    }

    /// A widened GRANT SCOPE must render as a change even when capabilities
    /// happen to match exactly — the ruling this pins is that the diff covers
    /// every persisted field (`ServiceGrant::capabilities` AND `::scopes`),
    /// never capabilities alone, so a daemon that widens what it asks for is
    /// never re-consented under "unchanged".
    #[test]
    fn a_widened_scope_renders_as_a_change_even_when_capabilities_match() {
        let (dir, service) = planning_workspace(&[]);
        let mut services = Services::default();
        services.grants.push(ServiceGrant {
            kind: "compute".into(),
            instance: "aa".repeat(32),
            nonce: "bb".repeat(16),
            granted_unix: 1,
            capabilities: vec!["claude".into()],
            scopes: vec!["cred:read".into()],
        });
        save(dir.path(), &services).expect("write grants");

        let offered = noded::services::Signaling {
            kind: "compute".into(),
            version: "1".into(),
            build: "b".into(),
            capabilities: vec!["claude".into()],
            scopes: vec!["cred:read".into(), "cred:write".into()],
            needs: Vec::new(),
        };
        let plan = plan_enable_from(dir.path(), "compute", &service, NODE_A, vec![offered])
            .expect("re-consent plans");
        let screen = through_anstream(&render_enable_summary(&plan), anstream::ColorChoice::Never);
        assert!(!screen.contains("unchanged"), "{screen}");
        assert!(screen.contains("CHANGED"), "{screen}");
        assert!(
            screen.contains("+ scope:cred:write  (new)"),
            "the widened scope is named: {screen}"
        );
    }

    /// A file the registry would refuse must not LOAD, which means it fails the
    /// node's boot rather than only its announce.
    ///
    /// The state this replaces is the one to keep in mind: before it, an
    /// over-cap `services.toml` booted a healthy-looking node whose watcher then
    /// refused every tick behind a warn throttled to one line per five minutes —
    /// boots, looks fine, silently does nothing. Refusing loudly is strictly
    /// better, and the only writer of this file already bounds it, so a file
    /// that exceeds the cap means a hand edit or a bug. Both deserve to be loud.
    #[test]
    fn a_file_the_registry_would_refuse_does_not_load() {
        let over_cap = Services {
            grants: vec![ServiceGrant {
                kind: "compute".into(),
                instance: "aa".repeat(32),
                nonce: "bb".repeat(16),
                granted_unix: 1,
                // 64 executors + the kind tag = one over the registry's cap.
                capabilities: (0..64).map(|n| format!("e{n}")).collect(),
                scopes: Vec::new(),
            }],
        };
        let error = over_cap
            .validate()
            .expect_err("an over-cap grant set must not load");
        assert!(
            error.contains("64"),
            "the refusal names the registry's cap: {error}"
        );

        let illegal = Services {
            grants: vec![ServiceGrant {
                kind: "compute".into(),
                instance: "aa".repeat(32),
                nonce: "bb".repeat(16),
                granted_unix: 1,
                capabilities: vec!["Claude Sonnet".into()],
                scopes: Vec::new(),
            }],
        };
        let error = illegal
            .validate()
            .expect_err("a tag the registry refuses must not load");
        assert!(
            error.contains("Claude Sonnet"),
            "the offending tag is named: {error}"
        );
    }

    #[test]
    fn a_grant_set_within_the_cap_loads() {
        // so the test above pins the bound rather than a file that could never
        // load: one under the cap is fine.
        let ok = Services {
            grants: vec![ServiceGrant {
                kind: "compute".into(),
                instance: "aa".repeat(32),
                nonce: "bb".repeat(16),
                granted_unix: 1,
                capabilities: (0..63).map(|n| format!("e{n}")).collect(),
                scopes: Vec::new(),
            }],
        };
        ok.validate()
            .expect("63 executors + the kind tag is exactly the cap");
    }

    /// Three daemons granting themselves at once keep all three grants.
    ///
    /// The real shape is a setup script or a supervisor starting `compute`,
    /// `agent` and `airlock` in one loop: they land in the same millisecond,
    /// every time, and each writes the WHOLE file back. Unlocked, the last
    /// writer's copy is the one that survives and the others' grants are gone —
    /// with no error anywhere, because each writer succeeded. The daemon then
    /// runs enabled by its own log and ungranted to the node, refusing every
    /// saga accept with `accept_not_capability_provider`.
    ///
    /// Threads rather than processes because `flock` is per open file
    /// description: each call opens the lock file itself, so two threads
    /// contend exactly as two processes do.
    #[test]
    fn concurrent_grants_all_survive() {
        let home = tempfile::tempdir().expect("a workspace");
        let workspace = home.path().to_path_buf();
        let kinds = ["agent", "airlock", "compute"];
        let start = std::sync::Barrier::new(kinds.len());
        std::thread::scope(|scope| {
            for (n, kind) in kinds.iter().enumerate() {
                let workspace = workspace.clone();
                let start = &start;
                scope.spawn(move || {
                    // every thread reaches the read-modify-write together
                    start.wait();
                    with_grants(&workspace, |services| {
                        let fresh = grant(kind, [n as u8 + 1; 32]);
                        match services
                            .grants
                            .binary_search_by(|existing| existing.kind.as_str().cmp(kind))
                        {
                            Ok(standing) => services.grants[standing] = fresh,
                            Err(position) => services.grants.insert(position, fresh),
                        }
                        Ok(())
                    })
                    .expect("grant under the lock");
                });
            }
        });

        let survived = load(&workspace).expect("the grants");
        let kept: Vec<&str> = survived
            .grants
            .iter()
            .map(|grant| grant.kind.as_str())
            .collect();
        assert_eq!(
            kept, kinds,
            "a concurrent grant was lost: each writer rewrites the whole file, so without a \
             lock across load..save the last one to save erases the others"
        );
    }

    /// Consent lands on DISK before it lands on chain, in both verbs.
    ///
    /// A source lint because the property is an ORDER between two writers, not
    /// a value: `/v1/submit` answers only once consensus has settled, so there
    /// is a real interval between the two. A watcher tick inside it compares the
    /// chain against the file, and if the chain moved first it reads the verb's
    /// own change as drift and submits the OPPOSITE — retracting a kind just
    /// enabled, or re-announcing consent just revoked. Persisting first makes
    /// that interval benign: the watcher then agrees with the verb.
    #[test]
    fn both_verbs_persist_before_they_announce() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/services.rs"))
                .expect("this file");
        for (verb, marker) in [
            ("commit_enable", "pub(crate) fn commit_enable("),
            ("disable", "fn disable(args: KindArgs)"),
        ] {
            let body = source
                .split(marker)
                .nth(1)
                .and_then(|rest| rest.split("\nfn ").next())
                .unwrap_or_else(|| panic!("{verb} has a body"));
            // `with_grants(` IS the persist: it is the only writer of the file
            // (it holds the lock across load..save), so a verb that reaches
            // `announce::submit(` without having gone through it has either
            // announced before persisting or bypassed the lock.
            let saved = body
                .find("with_grants(")
                .unwrap_or_else(|| panic!("{verb} persists"));
            let announced = body
                .find("announce::submit(")
                .unwrap_or_else(|| panic!("{verb} announces"));
            assert!(
                saved < announced,
                "{verb} announces before it persists — a watcher tick in between reads the \
                 chain as ahead of the file and submits the opposite, undoing this verb"
            );
        }
    }

    /// The daemon must SURVIVE every way enabling can fail.
    ///
    /// A source lint rather than a behavioural test, because `offer_enable`
    /// prompts on a TTY and writes to stderr, and the property is about control
    /// flow rather than a value: NEITHER half of enabling may use `?`, because
    /// this runs BEFORE the heartbeat thread is spawned, so an early return
    /// exits the process and takes the signal with it — losing the daemon, the
    /// hello, and the `service list` row that would have said why.
    #[test]
    fn neither_half_of_enabling_may_abort_the_daemon() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/services.rs"))
                .expect("this file");
        let body = source
            .split("fn offer_enable(")
            .nth(1)
            .and_then(|rest| rest.split("\nfn ").next())
            .expect("offer_enable has a body");
        for call in ["plan_enable(", "commit_enable("] {
            let site = body
                .split(call)
                .nth(1)
                .unwrap_or_else(|| panic!("offer_enable calls {call}"));
            let tail: String = site.chars().take(200).collect();
            assert!(
                !tail.contains(")?"),
                "offer_enable must not `?` on {call} — it runs before the heartbeat \
                 thread is spawned, so an early return kills the daemon instead of \
                 leaving it signaling. Route the failure through `decline`."
            );
        }
        assert!(
            body.matches("decline(").count() >= 2,
            "both halves of enabling must route their failure through `decline`"
        );
    }

    /// Each way `/v1/status` can fail to name a node must produce its OWN
    /// message, and the empty string must reach the startup one.
    ///
    /// The empty case is why this test exists rather than four eyeballed
    /// `format!`s: `public_key` is a non-Option `String` that a booting node
    /// serves as `""`, so it silently took the wrong-length arm and reported a
    /// routine startup race as a malformed identity — while the message written
    /// for it was dead code no test executed.
    #[test]
    fn every_status_identity_outcome_gets_its_own_message() {
        let absent = published_identity(&serde_json::json!({})).expect_err("no field at all");
        let booting = published_identity(&serde_json::json!({ "public_key": "" }))
            .expect_err("up, but not published yet");
        assert_eq!(absent, NOT_PUBLISHED_YET);
        assert_eq!(
            booting, NOT_PUBLISHED_YET,
            "an empty public_key is a booting node, not a malformed one"
        );
        // the specific regression: it must NOT be diagnosed by length.
        assert!(
            !booting.contains("byte"),
            "the empty case must not fall through to the length arm: {booting}"
        );

        let not_hex = published_identity(&serde_json::json!({ "public_key": "zz" }))
            .expect_err("not hex at all");
        assert!(not_hex.contains("not hex"), "{not_hex}");

        let short = published_identity(&serde_json::json!({ "public_key": "abcd" }))
            .expect_err("hex, but not 32 bytes");
        assert!(short.contains("2-byte"), "{short}");

        // all four failures are distinguishable from one another.
        let messages = [&absent, &not_hex, &short];
        assert_eq!(
            messages
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            3,
            "each outcome needs its own sentence: {messages:?}"
        );

        // and the happy path still decodes.
        let key = "ab".repeat(32);
        let decoded = published_identity(&serde_json::json!({ "public_key": key }))
            .expect("a published 32-byte identity decodes");
        assert_eq!(decoded, [0xab; 32]);
    }

    /// collect `.rs` files under `dir`, RECURSIVELY.
    ///
    /// A flat `read_dir` would leave `compute/sub/keys.rs` unscanned — a
    /// one-directory hiding place inside the very tree the lint is about.
    fn rust_files_under(dir: &Path, prefix: &str, into: &mut Vec<(String, PathBuf)>) {
        for entry in std::fs::read_dir(dir).expect("read daemon module dir") {
            let path = entry.expect("dir entry").path();
            let name = format!("{prefix}/{}", path.file_name().unwrap().to_string_lossy());
            if path.is_dir() {
                rust_files_under(&path, &name, into);
                continue;
            }
            if path.extension().is_some_and(|ext| ext == "rs") {
                into.push((name, path));
            }
        }
    }

    /// every `.rs` on the daemon path: this file, plus both daemon module trees.
    fn daemon_path_sources() -> Vec<(String, String)> {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = vec![("services.rs".to_string(), src.join("services.rs"))];
        for module in ["compute", "agent"] {
            rust_files_under(&src.join(module), module, &mut files);
        }
        files
            .into_iter()
            .map(|(name, path)| {
                let source = std::fs::read_to_string(&path).expect("read daemon source");
                // NOTHING is truncated. This used to cut the file at its first
                // `#[cfg(test)]`, which meant one test helper near the top of a
                // file blinded the scan to every line below it — the largest
                // hole this lint had. Test code is daemon-path code for the
                // purpose of naming the key, so it is scanned too; the fixtures
                // below are assembled from pieces for exactly that reason.
                //
                // Line comments ARE stripped: this is a lint on CODE, and the
                // doc comments deliberately NAME the thing they forbid.
                let code = source
                    .lines()
                    .map(|line| line.split("//").next().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("\n");
                (name, code)
            })
            .collect()
    }

    /// Rust identifier tokens in `code`.
    fn identifiers(code: &str) -> impl Iterator<Item = &str> {
        code.split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|token| !token.is_empty())
    }

    /// Every name this code pulls out of `crate::config`.
    ///
    /// The daemon path reaches the node's key through exactly one module, and
    /// both ways of reaching into it end in the same eight characters:
    /// `use ...config::{A as X, b as y}` and a qualified `config::b(..)`. So
    /// one scan for `config::` and a read of whatever follows — a braced list,
    /// or a single name — sees the REAL names regardless of what they are then
    /// aliased to. There is no alias-of-an-alias: the import has to spell the
    /// true name once, right here, or it does not compile.
    ///
    /// Scanning names rather than whole files is what keeps the ban honest in
    /// the other direction too: `compute_service::Resolved` (a resolved
    /// CREDENTIAL) and the `CredentialResolver::resolve` trait method share
    /// their spelling with the banned config names and have nothing to do with
    /// them, so a bare token ban would fire on `compute/cred.rs` and teach the
    /// next reader to delete the lint.
    fn config_names(code: &str) -> Vec<&str> {
        const PREFIX: &str = "config::";
        let mut names = Vec::new();
        let mut rest = code;
        while let Some(at) = rest.find(PREFIX) {
            let tail = &rest[at + PREFIX.len()..];
            let chunk = match tail.strip_prefix('{') {
                // a use-list: every name in it, aliases and all.
                Some(list) => &list[..list.find('}').unwrap_or(list.len())],
                // a single qualified name: read to the end of the identifier,
                // so `config::resolve_service` yields ONE name and can never
                // be read as `resolve`.
                None => {
                    let end = tail
                        .find(|c: char| !c.is_alphanumeric() && c != '_')
                        .unwrap_or(tail.len());
                    &tail[..end]
                }
            };
            names.extend(identifiers(chunk));
            rest = tail;
        }
        names
    }

    /// the config names that hand over the node's private key, or a type that
    /// carries it. `resolve_service`/`ServiceConfig` are the whole legal
    /// surface.
    const KEYED_CONFIG_NAMES: &[&str] = &[
        "Resolved",
        "resolve",
        "load_identity",
        "load_or_generate_identity",
    ];

    /// A TRIPWIRE on the daemon path, not the guarantee — and the difference
    /// matters, because a guard described as load-bearing is one the next
    /// reader stops looking behind.
    ///
    /// WHAT ACTUALLY HOLDS THE LINE, in order:
    /// 1. The TYPE. [`crate::compute::Compute`] and [`crate::agent::Agent`]
    ///    hold a `config::ServiceConfig`, which has no field a secret can live
    ///    in. Nothing on this path is handed the key, so nothing has to refrain
    ///    from using it.
    /// 2. The BEHAVIORAL proof,
    ///    `config::resolve::tests::the_service_path_never_reads_the_node_key`:
    ///    it resolves a workspace with `identity.key` ABSENT and asserts the
    ///    daemon path succeeds where the node path fails. Add a key loader to
    ///    the service path and it goes red on the spot, whatever the loader is
    ///    called.
    /// 3. This lint, which catches the remaining case the other two cannot:
    ///    NEW code on the daemon path that reaches back into `config` for the
    ///    keyed resolver.
    ///
    /// WHAT IT CANNOT DO. It is a string matcher over source text, so it is
    /// defence in depth and nothing more. It reads `config::`-qualified paths
    /// and use-lists, which covers every honest spelling and every aliased one
    /// (an alias must still write the true name at the import). It does not
    /// parse Rust: a glob `use crate::config::*`, a macro that assembles the
    /// path, a re-export through a third module, or a helper living outside the
    /// scanned tree all walk past it. Treat a green run here as "nobody typed
    /// the obvious thing", never as proof; the proof is (1) and (2).
    ///
    /// Why it is worth keeping anyway: `config::resolve` yields a
    /// `config::Resolved`, whose `signer` field IS the node's ed25519 PRIVATE
    /// key. A daemon holding that needs no node surface at all — it can sign
    /// frames itself, which caps every authorization boundary anyone draws on
    /// `/v1` later. And now that `Resolved` CONTAINS a `ServiceConfig`, calling
    /// `resolve()` looks like a perfectly reasonable way to get one, while
    /// handing over the key in the same breath. That is the mistake this catches.
    ///
    /// Not scanned, and it does not need to be: `crates/services/**` cannot
    /// name `config` at all — those crates do not depend on this binary, so the
    /// module is unreachable from them by construction rather than by promise.
    #[test]
    fn the_daemon_path_cannot_name_the_node_key() {
        // ASSEMBLED, not written out: this file is scanned by the loop below,
        // so a literal here would be a hit on the lint's own assertion text.
        // That is the honest cost of scanning test code instead of truncating
        // at the first `#[cfg(test)]` — a truncation that used to blind the
        // scan to every line beneath it.
        let key_type = ["Private", "Key"].concat();
        let key_file = ["identity", ".key"].concat();
        for (file, code) in daemon_path_sources() {
            for name in config_names(&code) {
                assert!(
                    !KEYED_CONFIG_NAMES.contains(&name),
                    "{file} takes `{name}` out of `config`: the daemon path must not be \
                     able to reach the node's private key — resolve through \
                     `config::resolve_service` and ask the node who it is over `/v1/status`"
                );
            }
            // the route around the resolver entirely: open the key file and
            // decode it. One type can hold the result, and one file holds it.
            assert!(
                identifiers(&code).all(|token| token != key_type),
                "{file} names `{key_type}`: a daemon must not decode the node's key"
            );
            assert!(
                !code.contains(&key_file),
                "{file} names `{key_file}`: a daemon must not open the node's key file"
            );
        }
    }

    /// The lint is only worth having if it catches a REAL attempt, so this
    /// feeds it the exact aliased steal that the previous substring form let
    /// through: `use crate::config::{Resolved as NodeCfg, resolve as node_cfg}`
    /// plus `let NodeCfg { signer, .. } = node_cfg(path)?` contains none of
    /// `config::Resolved`, `config::resolve(` or `.signer`, so
    /// `code.contains(needle)` passed while the key was being loaded. It was
    /// not a hypothetical — that code was compiled into `compute/mod.rs` and
    /// the old lint stayed green.
    ///
    /// Without this, a later "simplification" back to substring matching would
    /// leave every daemon-path test green again.
    ///
    /// The fixtures are ASSEMBLED rather than written out, because this file is
    /// itself scanned by the lint above: a fixture that spelled the banned
    /// import literally would trip the very test it exercises.
    #[test]
    fn the_lint_catches_an_aliased_key_steal() {
        let qualified = ["config", "::"].concat();
        let steal = format!(
            "use crate::{qualified}{{Resolved as NodeCfg, resolve as node_cfg}};\n\
             let NodeCfg {{ signer, .. }} = node_cfg(path).ok()?;"
        );
        let caught: Vec<&str> = config_names(&steal)
            .into_iter()
            .filter(|name| KEYED_CONFIG_NAMES.contains(name))
            .collect();
        assert_eq!(
            caught,
            ["Resolved", "resolve"],
            "aliases must not hide the real names"
        );

        // a bare `config::resolve (p)` — one space defeated the old ban.
        let spaced = format!("{qualified}resolve (p)?");
        assert!(
            config_names(&spaced).contains(&"resolve"),
            "whitespace must not hide the call"
        );

        // and the legal daemon spellings must NOT trip it: `resolve_service` is
        // ONE identifier, and another crate's `Resolved` is not config's.
        let legal = format!(
            "use compute_service::{{CredentialResolver, Resolved}};\n\
             let service = {qualified}resolve_service(path)?;\n\
             async fn resolve(&self) -> Result<Resolved, String> {{}}"
        );
        assert!(
            config_names(&legal)
                .into_iter()
                .all(|name| !KEYED_CONFIG_NAMES.contains(&name)),
            "only names taken out of `config` are banned: {:?}",
            config_names(&legal)
        );
    }

    /// Build skew is a DIAGNOSTIC now, not a refusal: a daemon on another build
    /// still signals, still enables, and `service status` names the difference.
    ///
    /// Every stamp here is a value the NODE reported. This CLI's own
    /// `build_identity_or_unknown()` appears nowhere, which is the whole point
    /// of the second test below.
    #[test]
    fn a_skewed_build_is_rendered_beside_the_nodes_own_and_refuses_nothing() {
        let node = "node-build-1";

        // not signaling: nothing to show, and no skew claim invented.
        assert_eq!(render_build(None, Some(node)), "-");
        // agreeing: the stamp alone, with no noise on the ordinary case.
        assert_eq!(render_build(Some(node), Some(node)), node);
        // disagreeing: both stamps, so the operator can act on it.
        let skewed = render_build(Some("0.0.0-ancient"), Some(node));
        assert!(skewed.contains("0.0.0-ancient"), "{skewed}");
        assert!(skewed.contains(node), "{skewed}");
        // a node that did not answer names no build: the daemon's stamp alone,
        // never a skew claim against a value we do not have.
        assert_eq!(render_build(Some("0.0.0-ancient"), None), "0.0.0-ancient");

        // and the row itself is `enabled`, never withheld: the old gate kept a
        // skewed daemon out of the catalog entirely, so it could not be seen.
        let mut live = signaling("compute");
        live.build = "0.0.0-ancient".into();
        let rows = rows(
            &[live],
            &[grant("compute", mint_instance(&NODE_A, "compute", &NONCE))],
        );
        assert_eq!(rows[0].state, ServiceState::Enabled);
        assert_eq!(rows[0].build.as_deref(), Some("0.0.0-ancient"));
    }

    /// "(this node: …)" must name the NODE's build, never the build of whatever
    /// `ducktape` binary the operator typed.
    ///
    /// Measured on a live box: `service status` run from an older binary against
    /// a current node printed that binary's commit as the node's, and labelled
    /// an agent daemon skewed that was on the node's build exactly. The daemon
    /// stamp here IS this CLI's own build — the value the defect read — so a
    /// render that reaches for it again matches, prints no skew, and reddens.
    #[test]
    fn this_binarys_own_stamp_is_never_passed_off_as_the_nodes() {
        let cli = noded::services::build_identity_or_unknown();
        let node = "the-node-actually-running";
        let rendered = render_build(Some(cli), Some(node));
        assert!(
            rendered.contains(node),
            "the node's own stamp must be the one compared against: {rendered}"
        );
        assert!(
            rendered.starts_with(cli),
            "the daemon's stamp is still what the column leads with: {rendered}"
        );
    }

    #[test]
    fn skew_is_only_claimed_when_both_sides_actually_named_a_build() {
        let unknown = noded::services::UNKNOWN_BUILD;
        assert_eq!(Skew::between("abc123", Some("abc123")), Skew::Matched);
        assert_eq!(Skew::between("abc123", Some("def456")), Skew::Skewed);
        // a side that cannot name its build proves nothing either way — the
        // git-absent build must not spend its life warning about itself.
        assert_eq!(Skew::between("abc123", Some(unknown)), Skew::Unknown);
        assert_eq!(Skew::between(unknown, Some("abc123")), Skew::Unknown);
        assert_eq!(Skew::between("abc123", None), Skew::Unknown);
    }

    #[test]
    fn the_instance_id_is_deterministic_and_domain_separated() {
        let mine = mint_instance(&NODE_A, "compute", &NONCE);
        // determinism: the same three inputs always mint the same id, so a
        // daemon restart under one grant keeps its identity.
        assert_eq!(mine, mint_instance(&NODE_A, "compute", &NONCE));

        // every input separates: node, kind and nonce each change the id.
        assert_ne!(mine, mint_instance(&NODE_B, "compute", &NONCE));
        assert_ne!(mine, mint_instance(&NODE_A, "storage", &NONCE));
        assert_ne!(
            mine,
            mint_instance(&NODE_A, "compute", &[4u8; GRANT_NONCE_LEN])
        );

        // the domain prefix is real: the id is NOT a bare sha256 of the parts.
        let mut undomained = Sha256::new();
        undomained.update(NODE_A);
        undomained.update(b"compute");
        undomained.update(NONCE);
        let undomained: [u8; 32] = undomained.finalize().into();
        assert_ne!(mine, undomained);

        // and the separators make the preimage unambiguous: a kind that
        // borrows a byte from the nonce must not collide with the honest
        // split. (`kind ‖ nonce` concatenated blindly would collide here.)
        let split_a = mint_instance(&NODE_A, "ab", &[0xcd; GRANT_NONCE_LEN]);
        let split_b = mint_instance(&NODE_A, "abc", &[0xcd; GRANT_NONCE_LEN]);
        assert_ne!(split_a, split_b);
    }

    #[test]
    fn the_display_id_is_the_chain_id_convention() {
        let id = mint_instance(&NODE_A, "compute", &NONCE);
        let display = grant("compute", id).display_id();
        let (kind, head) = display.split_once('#').expect("kind#hex8");
        assert_eq!(kind, "compute");
        assert_eq!(head.len(), 8, "the display tail is the first 4 bytes");
        assert_eq!(head, config::hex_bytes(&id[..4]));
    }

    #[test]
    fn list_state_derives_all_three_standings() {
        let enabled = grant("compute", mint_instance(&NODE_A, "compute", &NONCE));
        let absent = grant("storage", mint_instance(&NODE_A, "storage", &NONCE));
        // compute: granted AND signaling. airlock: signaling, no grant.
        // storage: granted, nothing signaling.
        let live = vec![signaling("compute"), signaling("airlock")];
        let rows = rows(&live, &[enabled.clone(), absent.clone()]);

        let states: Vec<(&str, ServiceState)> = rows
            .iter()
            .map(|row| (row.kind.as_str(), row.state))
            .collect();
        assert_eq!(
            states,
            vec![
                ("airlock", ServiceState::Signaling),
                ("compute", ServiceState::Enabled),
                ("storage", ServiceState::EnabledAbsent),
            ],
            "rows are kind-sorted and each standing is derived independently"
        );

        // a signaling-only row has no id; a granted one always does.
        assert_eq!(rows[0].instance, None);
        assert_eq!(rows[1].instance, Some(enabled.display_id()));
        assert_eq!(rows[2].instance, Some(absent.display_id()));

        // the LIVE hello wins for offered tags; an absent service falls back
        // to what its grant recorded at consent time.
        assert_eq!(rows[1].capabilities, vec!["agent.codex".to_string()]);
        assert_eq!(rows[2].capabilities, vec!["agent.claude".to_string()]);
        assert_eq!(rows[2].version, None);
    }

    #[test]
    fn grants_round_trip_through_the_file_and_an_empty_set_leaves_none() {
        let dir = tempfile::tempdir().unwrap();
        // an absent file is an empty grant set, never an error.
        assert_eq!(load(dir.path()).unwrap(), Services::default());
        assert!(grant_for(dir.path(), COMPUTE_KIND).unwrap().is_none());

        let services = Services {
            grants: vec![
                grant("compute", mint_instance(&NODE_A, "compute", &NONCE)),
                grant("storage", mint_instance(&NODE_A, "storage", &NONCE)),
            ],
        };
        save(dir.path(), &services).unwrap();
        assert_eq!(load(dir.path()).unwrap(), services);
        assert_eq!(
            grant_for(dir.path(), COMPUTE_KIND).unwrap().unwrap().kind,
            "compute"
        );

        // removing the last grant removes the file rather than leaving a husk.
        save(dir.path(), &Services::default()).unwrap();
        assert!(!dir.path().join(FILE_NAME).exists());
        assert!(grant_for(dir.path(), COMPUTE_KIND).unwrap().is_none());
    }

    /// the three-state fixture every renderer test shares.
    fn every_state() -> Vec<ServiceRow> {
        rows(
            &[signaling("compute"), signaling("airlock")],
            &[
                grant("compute", mint_instance(&NODE_A, "compute", &NONCE)),
                grant("storage", mint_instance(&NODE_A, "storage", &NONCE)),
            ],
        )
    }

    /// Render `text` the way a real verb would, to a destination with the
    /// given color choice — i.e. through the SAME `anstream` adapter
    /// `write_out` uses. `Never` is what a pipe, a file, `NO_COLOR` and
    /// `TERM=dumb` all resolve to.
    fn through_anstream(text: &str, choice: anstream::ColorChoice) -> String {
        use std::io::Write as _;
        let mut stream = anstream::AutoStream::new(Vec::new(), choice);
        stream.write_all(text.as_bytes()).expect("write");
        String::from_utf8(stream.into_inner()).expect("utf8")
    }

    #[test]
    fn a_non_terminal_destination_receives_no_escape_sequences() {
        let rows = every_state();
        let offered = signaling("compute");
        let plan = EnablePlan {
            kind: "compute".into(),
            chain_id: "ducktape#a1b2c3d4".into(),
            node_id: NODE_A,
            grant: mint_grant("compute", NODE_A, &offered),
            capacity: Default::default(),
            previous: None,
            offered: Some(offered.clone()),
        };
        // the same screen on a RE-CONSENT: the diff is styled per tag, so it
        // is the shape most likely to leak an escape into a pipe.
        let reconsent = EnablePlan {
            previous: Some(grant("compute", NODE_A)),
            offered: Some(offered),
            ..EnablePlan {
                kind: "compute".into(),
                chain_id: "ducktape#a1b2c3d4".into(),
                node_id: NODE_A,
                grant: mint_grant("compute", NODE_A, &signaling("compute")),
                capacity: Default::default(),
                previous: None,
                offered: None,
            }
        };
        let rendered = [
            render_list(&rows),
            render_status(&rows, Some("node-build-1")),
            render_enable_summary(&plan),
            render_enable_summary(&reconsent),
            render_list(&[]),
            render_status(&[], None),
        ];
        for one in &rendered {
            let piped = through_anstream(one, anstream::ColorChoice::Never);
            assert!(
                !piped.contains('\x1b'),
                "a non-terminal must never receive ANSI:\n{piped:?}"
            );
        }
        // and the facts a script or a human greps for survive the stripping.
        let list = through_anstream(&rendered[0], anstream::ColorChoice::Never);
        assert!(list.contains("enabled-but-absent"));
        assert!(list.contains("compute"));
        assert!(
            through_anstream(&rendered[1], anstream::ColorChoice::Never).contains("agent.codex")
        );
        assert!(
            through_anstream(&rendered[2], anstream::ColorChoice::Never)
                .contains("ducktape#a1b2c3d4")
        );
    }

    #[test]
    fn a_terminal_destination_keeps_the_state_colors_and_stays_aligned() {
        let rows = every_state();
        let painted = through_anstream(&render_list(&rows), anstream::ColorChoice::Always);
        assert!(painted.contains('\x1b'), "a terminal gets color");

        // alignment is computed on the TEXT, never on escape bytes, so the
        // colored rendering and the piped one differ ONLY by escapes.
        let piped = through_anstream(&render_list(&rows), anstream::ColorChoice::Never);
        assert_eq!(strip_ansi(&painted), piped);

        // every state carries its own distinct styling.
        let styles: Vec<String> = rows
            .iter()
            .map(|row| paint(row.state.style(), row.state.glyph()))
            .collect();
        assert_eq!(
            styles
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            3,
            "signaling / enabled / enabled-but-absent must be visually distinct"
        );
    }

    /// drop SGR sequences (`ESC [ ... m`) so a styled string can be compared
    /// against its stripped twin.
    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            for skipped in chars.by_ref() {
                if skipped == 'm' {
                    break;
                }
            }
        }
        out
    }

    #[test]
    fn the_consent_prompt_is_skipped_where_no_one_can_answer() {
        // automation: a pipe has nobody to ask, and --yes says so explicitly.
        // Neither may block on stdin, so both proceed without reading it.
        assert!(crate::tty::confirm("Enable compute?", false, false).unwrap());
        assert!(crate::tty::confirm("Enable compute?", false, true).unwrap());
        assert!(crate::tty::confirm("Enable compute?", true, true).unwrap());
    }

    /// The hello cap must admit what a REAL host offers. The built-in specs
    /// alone expand to one tag per `[[variants]]` entry, which a 32-tag cap
    /// silently refused — a live `service run` died on its first hello with
    /// `malformed_hello`. Pinned against the actual spec set so shrinking the
    /// cap, or growing the specs past it, fails here instead of in the field.
    #[test]
    fn the_hello_cap_admits_a_real_hosts_capability_set() {
        // the built-in spec set only (operator dir explicitly excluded, so the
        // test does not depend on what this host has installed).
        let builtin_tags = provider_host::SpecSet::load(None)
            .expect("built-in specs load")
            .iter()
            .count();
        assert!(
            builtin_tags > 32,
            "this test is only meaningful while the built-ins exceed the old cap ({builtin_tags})"
        );
        let hello = noded::services::Hello {
            kind: "compute".into(),
            version: "1.0.0".into(),
            build: noded::services::build_identity().unwrap_or("test").into(),
            capabilities: (0..builtin_tags).map(|i| format!("tag{i}")).collect(),
            scopes: Vec::new(),
            needs: Vec::new(),
        };
        assert!(
            hello.validate().is_ok(),
            "a real host's tag set must be admitted, not refused as malformed"
        );
    }

    #[test]
    fn state_tokens_match_the_rendered_labels() {
        // one state, one name: a script keying on the `--json` token and a
        // human reading the table must never see two different spellings.
        for state in [
            ServiceState::Signaling,
            ServiceState::Enabled,
            ServiceState::EnabledAbsent,
        ] {
            let json = serde_json::to_string(&state).expect("serialize state");
            assert_eq!(
                json,
                format!("\"{}\"", state.label()),
                "the json token and the printed label must agree"
            );
        }
    }

    /// A node that cannot be asked is not a node with nothing to say. The
    /// refusal has to name which of the two it is, because the rows above it
    /// are about grants on disk and every state word in them is about
    /// signaling, which is exactly what went unread.
    #[test]
    fn the_unread_refusal_never_claims_the_catalog_was_empty() {
        // what a live `Unreachable` renders: the sentence arrives WITH its next
        // step already in it, which is why this verb no longer appends one.
        let reason = crate::node_http::not_running_in(Some(std::path::Path::new(
            "/home/duck/.ducktape/dognet",
        )))
        .to_string();
        for (printed_grants, expected) in [
            (
                true,
                "the grants above are what this workspace holds on disk",
            ),
            (false, "this workspace holds no grants on disk"),
        ] {
            let said = unread_refusal(&reason, printed_grants);
            assert!(said.starts_with(expected), "{said}");
            assert!(said.contains(&reason), "the reason must survive: {said}");
            assert!(
                said.contains("could not be read"),
                "it must say the catalog went UNREAD, never that it was empty: {said}"
            );
            assert!(
                !said.contains("none enabled") && !said.contains("nothing is signaling"),
                "it must not assert what it failed to establish: {said}"
            );
        }
    }

    /// #2511, end to end through both read verbs: a workspace whose node is
    /// not running is refused in the not-running sentence at a non-zero exit
    /// (an `Err` out of the verb), never rendered as a healthy node with
    /// nothing enabled at exit 0.
    #[test]
    fn a_stopped_node_is_refused_by_both_read_verbs() {
        let dir = tempfile::tempdir().expect("tempdir");
        // bound only to learn a port, then closed: the connect is REFUSED.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind loopback")
            .local_addr()
            .expect("addr")
            .port();
        let config = dir.path().join("node.toml");
        std::fs::write(
            &config,
            format!(
                "id = 7\nlisten = \"127.0.0.1:0\"\nnamespace = \"demo\"\npeer_seeds = [7]\n\
                 storage_dir = {:?}\nhttp_listen = \"127.0.0.1:{port}\"\n",
                dir.path().display().to_string()
            ),
        )
        .expect("write node.toml");
        for (name, verb) in [("list", list as fn(_) -> _), ("status", status)] {
            let args = ReadArgs {
                kind: None,
                workspace: WorkspaceArgs {
                    config: Some(config.clone()),
                    workspace: None,
                    network: None,
                },
                json: false,
            };
            let refused = verb(args)
                .expect_err("a node that did not answer is not an answer")
                .to_string();
            assert!(
                refused.contains("the node is not running"),
                "service {name}: {refused}"
            );
            assert!(
                !refused.contains("none enabled"),
                "service {name}: {refused}"
            );
        }
    }

    #[test]
    fn the_instance_id_survives_a_daemon_restart() {
        let dir = tempfile::tempdir().unwrap();
        // enabling mints the id ONCE and writes it down ...
        let minted = grant("compute", mint_instance(&NODE_A, "compute", &NONCE));
        save(
            dir.path(),
            &Services {
                grants: vec![minted.clone()],
            },
        )
        .unwrap();
        let first = minted.display_id();

        // ... so every later read — a daemon restart, a node restart, a fresh
        // CLI process — resolves the SAME kind#hex8. The id's lifetime is the
        // grant's, not the process's.
        for _ in 0..3 {
            let reloaded = grant_for(dir.path(), COMPUTE_KIND).unwrap().unwrap();
            assert_eq!(reloaded.display_id(), first);
            assert_eq!(reloaded, minted, "the whole grant round-trips unchanged");
            // and it is reproducible from the record alone.
            let nonce = (0..GRANT_NONCE_LEN)
                .map(|i| {
                    u8::from_str_radix(&reloaded.nonce[i * 2..i * 2 + 2], 16).expect("hex nonce")
                })
                .collect::<Vec<u8>>();
            assert_eq!(
                config::hex_bytes(&mint_instance(&NODE_A, "compute", &nonce)),
                reloaded.instance,
                "the id re-derives from node + kind + the recorded nonce"
            );
        }

        // disable retires it; a re-enable must NOT resurrect the same id.
        save(dir.path(), &Services::default()).unwrap();
        assert!(grant_for(dir.path(), COMPUTE_KIND).unwrap().is_none());
        let fresh = mint_instance(&NODE_A, "compute", &[0xab; GRANT_NONCE_LEN]);
        assert_ne!(
            config::hex_bytes(&fresh),
            minted.instance,
            "a fresh grant nonce means a fresh consent epoch"
        );
    }

    #[test]
    fn an_unmet_declared_need_is_a_warning_and_nothing_more() {
        let mut agent = signaling("agent");
        agent.needs = vec!["compute".into()];
        let agent_grant = grant("agent", mint_instance(&NODE_A, "agent", &NONCE));

        // nothing named `compute` is enabled here, so the need is unmet ...
        let unmet_rows = rows(&[agent.clone()], std::slice::from_ref(&agent_grant));
        assert_eq!(unmet_rows[0].unmet_needs, vec!["compute".to_string()]);
        // ... and yet the service is fully enabled: a need gates NOTHING.
        assert_eq!(unmet_rows[0].state, ServiceState::Enabled);
        assert!(unmet_rows[0].instance.is_some());

        let hint = unmet_hint(&unmet_rows[0]).expect("an unmet need is surfaced");
        assert!(hint.contains("compute"));
        assert!(hint.contains("informational"));
        assert!(
            through_anstream(&render_list(&unmet_rows), anstream::ColorChoice::Never)
                .contains("compute"),
            "the warning reaches the rendered list"
        );

        // enable a compute service and the need is met — same states, no hint.
        let with_compute = rows(
            &[agent],
            &[
                agent_grant,
                grant("compute", mint_instance(&NODE_A, "compute", &NONCE)),
            ],
        );
        let agent_row = with_compute
            .iter()
            .find(|row| row.kind == "agent")
            .expect("agent row");
        assert!(agent_row.unmet_needs.is_empty());
        assert_eq!(unmet_hint(agent_row), None);
    }

    #[test]
    fn a_malformed_grant_file_is_refused_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);

        // unsorted / duplicate kinds
        let duplicate = Services {
            grants: vec![grant("compute", [1u8; 32]), grant("compute", [2u8; 32])],
        };
        assert!(duplicate.validate().is_err());

        // a short instance id
        let mut short = grant("compute", [1u8; 32]);
        short.instance.truncate(10);
        assert!(
            Services {
                grants: vec![short],
            }
            .validate()
            .is_err()
        );

        // an unknown key never parses silently — the retired version field
        // included.
        std::fs::write(&path, "surprise = true\n").unwrap();
        assert!(load(dir.path()).is_err());
        std::fs::write(&path, "version = 1\n").unwrap();
        assert!(load(dir.path()).is_err());
    }
}
