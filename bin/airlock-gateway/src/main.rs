//! `airlock-gateway` — the TEE enclave binary, and nothing else. A thin CLI
//! wrapper over [`airlock::server`]. Runs inside an Intel TDX / AMD SEV-SNP
//! confidential VM: holds a sealed OAuth refresh token in enclave memory,
//! proxies the vendor messages API, and issues scoped session tokens. The
//! operator cannot read the credential.
//!
//! ## why this stays a separate, minimal binary
//!
//! Attestation measurement covers the WHOLE binary, so every byte in here is a
//! byte a lender re-pins on each rebuild. `ducktape` is deliberately NOT folded
//! in: that would churn the pinned measurement on every unrelated node release
//! and ship borrower/node code into the lender's trust boundary. Small enclave
//! binary = cheap trust. The non-TEE lender path is the airlock service daemon
//! (`ducktape service run airlock`), which never runs from here.
//!
//! There is exactly one subcommand and one attest family, for the same reason.
//! A `self-host` mode here would mint a fresh ephemeral seal key on every boot
//! (an enclave persists no keypair), which no lender could ever pin — broken by
//! construction, and a second path against the service daemon's real one. A
//! mock upstream likewise has no business inside a measured image.

use anyhow::{Context, Result};

use airlock::server::{AttestMode, GatewayConfig};
use airlock::sign::{Notary, Tools};

/// Where `ops/airlock-gateway/build-image.sh` stages the signing toolchain
/// beside this binary: the pinned `rcodesign` and the tree's
/// `app/packaging/entitlements.plist`.
const RCODESIGN_DEFAULT: &str = "/usr/local/bin/rcodesign";
const ENTITLEMENTS_DEFAULT: &str = "/usr/local/share/airlock-gateway/entitlements.plist";
/// The per-request work root: a tmpfs, so the identity written for one
/// `rcodesign` run and the bundle it signs never reach a disk.
const WORK_ROOT_DEFAULT: &str = "/dev/shm";

/// `--flag value` / `--flag=value` lookup over argv (no clap — house rule).
fn arg(name: &str) -> Option<String> {
    let mut it = std::env::args();
    while let Some(a) = it.next() {
        if a == name {
            return it.next();
        }
        if let Some(v) = a.strip_prefix(&format!("{name}=")) {
            return Some(v.to_string());
        }
    }
    None
}

fn arg_or(name: &str, default: &str) -> String {
    arg(name).unwrap_or_else(|| default.to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("serve") => serve().await,
        other => {
            eprintln!(
                "usage: airlock-gateway serve --attest tdx|snp|auto [flags]  (got {:?})",
                other.unwrap_or("")
            );
            std::process::exit(2);
        }
    }
}

async fn serve() -> Result<()> {
    let cfg = GatewayConfig {
        // the configfs-tsm path is the only one: this binary must run inside a
        // confidential VM, so a host that cannot attest fails loudly here
        // rather than serving an unattested credential.
        attest: AttestMode::Tsm(arg("--attest").context("--attest is required (tdx|snp|auto)")?),
        seal_keypair: None,
        anthropic_base: arg_or("--anthropic-base", "https://api.anthropic.com"),
        openai_base: arg_or("--openai-base", "https://chatgpt.com/backend-api/codex"),
        oauth_token_url: arg_or(
            "--oauth-token-url",
            "https://console.anthropic.com/v1/oauth/token",
        ),
        oauth_client_id: arg_or("--oauth-client-id", "9d1c250a-e61b-44d9-88ed-5944d1962f5e"),
        session_ttl_secs: arg("--session-ttl-secs")
            .map(|s| s.parse::<u64>())
            .transpose()
            .context("--session-ttl-secs")?
            .unwrap_or(3600),
        clock: airlock::server::Clock::system(),
        // The enclave image carries the signing toolchain, so this gateway
        // always serves `POST /sign/macos-bundle`; a missing tool at request
        // time is the `tool_missing` refusal, never a silent 404.
        sign: Some(Tools {
            rcodesign: arg_or("--rcodesign", RCODESIGN_DEFAULT).into(),
            entitlements: arg_or("--entitlements", ENTITLEMENTS_DEFAULT).into(),
            work_root: arg_or("--work-root", WORK_ROOT_DEFAULT).into(),
            notary: Notary::Apple,
        }),
    };
    let (app, vendor) = airlock::server::build(cfg)?;
    let listener =
        tokio::net::TcpListener::bind(arg_or("--listen", airlock::server::DEFAULT_LISTEN)).await?;
    eprintln!(
        "[gateway] attest={vendor} listening on {}",
        listener.local_addr()?
    );
    axum::serve(listener, app).await?;
    Ok(())
}
