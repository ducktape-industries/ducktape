//! `ducktape service run agent` — the standalone collaboration-delivery daemon.
//!
//! It takes its node's service link and serves the messaging bus over it: the
//! node sends `Msg*` commands, this process places each admitted message into
//! the provider session its binding names, and reports the receipt back. The
//! node's collaboration pump turns each receipt into an on-chain
//! `Acknowledge`.
//!
//! It runs no terminal and opens no sandbox. An interactive terminal session
//! belongs to an independently installed `ducktape-terminal` process reached
//! through its signed gateway route; nothing here spawns, drives or interprets
//! one.
//!
//! This daemon owns no keypair; it never needs one, because it never submits
//! anything. A binding names a provider session it did not start and must not
//! end, so a dropped link sweeps nothing.

use std::sync::Arc;

use crate::config;
use crate::services::ServiceGrant;

mod link;

/// Everything the daemon needs, resolved before any of it runs.
///
/// `service` is [`config::ServiceConfig`], NOT `config::Resolved`: the latter
/// carries the node's ed25519 private key, and the module doc above claims this
/// daemon owns no keypair — a claim the TYPE now makes true. `node_key` is the
/// node's PUBLIC identity, learned from the node over `/v1/status`; it names
/// this host's execution id and signs nothing.
pub(crate) struct Agent {
    pub(crate) grant: ServiceGrant,
    pub(crate) service: config::ServiceConfig,
    pub(crate) http_base: String,
    pub(crate) node_key: [u8; 32],
    /// where `node.toml` and the node's 0600 service-link token live.
    pub(crate) workspace: std::path::PathBuf,
}

/// Serve until the process is stopped. Returns only on a fatal
/// misconfiguration — a node that is merely down is retried forever, because
/// that is an operational state, not an error.
///
/// Through [`crate::services::serve_until_stopped`], which owns the runtime and
/// arms SIGTERM/SIGINT before a line of this daemon runs: a session's VMM is a
/// child of this process and must never outlive it holding guest memory.
pub(crate) fn serve(agent: Agent) -> Result<(), Box<dyn std::error::Error>> {
    crate::services::serve_until_stopped(std::future::pending(), |stop| run(agent, stop))
}

async fn run(agent: Agent, stop: crate::services::Stop) -> Result<(), Box<dyn std::error::Error>> {
    let Agent {
        grant,
        service,
        http_base,
        node_key,
        workspace,
    } = agent;
    let _ = node_key;
    let (events, event_rx) = tokio::sync::mpsc::channel(link::EVENT_LANE);
    // A daemon whose outbox will not open serves no messaging rather than
    // serving it without a durable record — the record is what makes a crash
    // reportable instead of replayable, so running without one is worse than
    // not running it.
    let deliveries = collab_plane(&service, events).await;

    tracing::info!(
        target: "ducktape::service",
        instance = %grant.display_id(),
        messaging = deliveries.is_some(),
        "agent daemon serving"
    );

    // the link never returns on its own: a dropped socket is ordinary (the node
    // restarts, the operator upgrades) and is redialed forever. A stop is what
    // ends this daemon.
    tokio::select! {
        () = link::attach(ws_url(&http_base), workspace, deliveries, event_rx) => {}
        () = stop => {}
    }
    tracing::info!(
        target: "ducktape::service",
        instance = %grant.display_id(),
        "agent daemon stopped"
    );
    Ok(())
}

/// build the collaboration delivery plane, or serve without one.
///
/// `None` is an operational state, not a failure to start: a daemon whose
/// outbox will not open answers no binding at all rather than answering one it
/// cannot record. The node hears about it either way — a bind simply gets no
/// reply, exactly as it does from a node with no daemon attached.
async fn collab_plane(
    service: &config::ServiceConfig,
    events: tokio::sync::mpsc::Sender<agent_service::wire::Event>,
) -> Option<Arc<agent_service::messaging::Deliveries>> {
    // the receipt address is best-effort by design. Without it every Claude
    // delivery settles `DeliveryUnknown` — degraded, and honest, which is the
    // right trade against reclaiming an address that may belong to a live
    // session.
    let receipts =
        match agent_service::messaging::claude::Receipts::bind(&claude_sockets_dir()).await {
            Ok(receipts) => Some(receipts),
            Err(error) => {
                tracing::warn!(
                    target: "ducktape::collab",
                    reason = "no_receipt_address",
                    %error,
                    "provider holds and refusals will not be observable; deliveries settle unknown"
                );
                None
            }
        };
    let plane = agent_service::messaging::Deliveries::open(
        &service.storage_dir.join("collab"),
        // the chain id, which is what this network IS. A directory that has
        // been pointed at a second network holds a delivery record whose
        // conversation ids and sequences belong to the first, and reading it
        // would answer a fresh message from another network's history.
        &service.chain_id,
        service.storage_dir.join("attachments.json"),
        claude_registry()?,
        std::path::PathBuf::from("codex"),
        receipts,
        events,
    )
    .await;
    match plane {
        Ok(plane) => Some(Arc::new(plane)),
        Err(error) => {
            tracing::error!(
                target: "ducktape::collab",
                reason = "outbox_unavailable",
                %error,
                "serving without messaging: a delivery record that cannot be written \
                 makes a crash replayable instead of reportable"
            );
            None
        }
    }
}

/// where Claude publishes its session registry: `$CLAUDE_CONFIG_DIR/sessions`,
/// or `$HOME/.claude/sessions`.
fn claude_registry() -> Option<std::path::PathBuf> {
    let home = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::path::PathBuf::from(std::env::var_os("HOME")?).join(".claude"),
    };
    Some(home.join("sessions"))
}

/// where Claude Code binds a session's inbox on this host, by its own rule —
/// and so where this daemon binds the address a recipient answers verdicts
/// to: a reply address beside the session's own socket is one a session
/// answers without further checks. The rule: `$XDG_RUNTIME_DIR`, else the
/// temp dir (`$TMPDIR`, `$TMP`, `$TEMP`, else `/tmp`), plus `cc-socks`; and
/// when a socket path there would not fit a socket address,
/// `/tmp/cc-socks-<uid>` instead. Every input is an environment fact, so the
/// answer is the same rule on every host rather than a host's own case.
fn claude_sockets_dir() -> std::path::PathBuf {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from);
    let temp_dir = ["TMPDIR", "TMP", "TEMP"]
        .iter()
        .find_map(std::env::var_os)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    // SAFETY: `getuid` reads the calling process's real uid and cannot fail.
    let uid = unsafe { libc::getuid() };
    claude_sockets_dir_from(runtime_dir.as_deref(), &temp_dir, std::process::id(), uid)
}

/// The longest socket path Claude Code binds under the default directory;
/// past it the per-user directory under `/tmp` is used. A unix socket
/// address holds 104 bytes on macOS, NUL included.
const SOCKET_PATH_BYTES: usize = 103;

fn claude_sockets_dir_from(
    runtime_dir: Option<&std::path::Path>,
    temp_dir: &std::path::Path,
    pid: u32,
    uid: u32,
) -> std::path::PathBuf {
    let default = runtime_dir.unwrap_or(temp_dir).join("cc-socks");
    let socket_fits = default.join(format!("{pid}.sock")).as_os_str().len() <= SOCKET_PATH_BYTES;
    if socket_fits {
        return default;
    }
    std::path::PathBuf::from(format!("/tmp/cc-socks-{uid}"))
}

/// `http(s)://host:port` → `ws(s)://host:port/v1/ws`.
fn ws_url(base: &str) -> String {
    let ws_base = match base.strip_prefix("https://") {
        Some(rest) => format!("wss://{rest}"),
        None => match base.strip_prefix("http://") {
            Some(rest) => format!("ws://{rest}"),
            None => base.to_string(),
        },
    };
    format!("{}/v1/ws", ws_base.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ws_url_tracks_the_http_scheme() {
        assert_eq!(ws_url("http://127.0.0.1:8844"), "ws://127.0.0.1:8844/v1/ws");
        assert_eq!(ws_url("https://node.example"), "wss://node.example/v1/ws");
        assert_eq!(
            ws_url("http://127.0.0.1:8844/"),
            "ws://127.0.0.1:8844/v1/ws"
        );
    }

    /// The receipt address goes where Claude Code puts a session's own
    /// socket on this host: the runtime dir where there is one, the temp
    /// dir where there is not, and the per-user `/tmp` directory when a
    /// socket path under the temp dir would not fit a socket address.
    #[test]
    fn the_receipt_address_sits_where_claude_code_binds_its_own() {
        use std::path::Path;
        assert_eq!(
            claude_sockets_dir_from(Some(Path::new("/run/user/1000")), Path::new("/tmp"), 4242, 1000),
            Path::new("/run/user/1000/cc-socks")
        );
        assert_eq!(
            claude_sockets_dir_from(
                None,
                Path::new("/var/folders/9k/1kcv0c8s6xz2c9b1f4s7x0y40000gn/T/"),
                4242,
                501
            ),
            Path::new("/var/folders/9k/1kcv0c8s6xz2c9b1f4s7x0y40000gn/T/cc-socks")
        );
        let deep = format!("/{}", "d".repeat(SOCKET_PATH_BYTES));
        assert_eq!(
            claude_sockets_dir_from(None, Path::new(&deep), 4242, 501),
            Path::new("/tmp/cc-socks-501")
        );
    }

    #[test]
    fn the_managed_label_separates_agent_from_compute() {
        // the co-tenancy guarantee as an assertion. It is the SECOND line of
        // defence — per-service graph roots already mean neither daemon can even
        // enumerate the other's containers — but a shared socket would be an
        // easy future mistake, and this is what would catch it.
        let agent = provider_host::managed_label("agent#deadbeef");
        assert_eq!(agent, "io.ducktape.managed=agent#deadbeef");
        assert_ne!(agent, provider_host::managed_label("compute#deadbeef"));
    }
}
