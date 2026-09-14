//! `ducktape user cred inspect|seal` — the credential-provider verbs against a
//! TEE airlock gateway (`bin/airlock-gateway`, running in a confidential VM).
//!
//! These are the ENCLAVE half of credential provision. The self-host half is
//! `cred add`, which captures a login artifact into this node's own store and
//! needs no attestation because its trust anchor is the on-chain seal_pk. Here
//! there is a real enclave, so the credential is released only after its quote
//! proves the audited measurement:
//!
//!   inspect  read the measurement out of the quote, so it can be pinned
//!   seal     verify the quote, then seal + upload the credential under the
//!            attested seal key
//!
//! ## the node is uninvolved in the trust decision
//!
//! Attestation is strictly bilateral: this CLI fetches the quote, verifies it
//! against roots it resolves itself (Intel's pinned inside dcap-qvl, AMD's from
//! the sev builtins), and derives seal_pk from the verified REPORTDATA. Nothing
//! is asked of the node, and nothing about the decision is submitted to it. The
//! node is used for ONE thing, and only in remote mode: reading its own browser
//! gateway base, which is the transport that carries `<handle>.duck` onto the
//! overlay.

// The verbs and their flags exist in EVERY build so the CLI surface (and the
// shipped completions) never depends on how the binary was compiled; only the
// bodies need the `verify` feature, and without it the verb refuses at
// dispatch with the same sentence its help text carries.
#[cfg(feature = "verify")]
use anyhow::Context as _;
#[cfg(feature = "verify")]
use commonware_cryptography::Signer as _;

#[cfg(feature = "verify")]
use airlock::attest::{self, AttestMode, Measurement};
#[cfg(feature = "verify")]
use airlock::client::Gateway;
#[cfg(feature = "verify")]
use airlock::verify::{SnpProduct, SnpRoots, TdxRoots, TrustRoots, VcekSource};
#[cfg(feature = "verify")]
use airlock::wire::{CredentialKind, CredentialPayload};

use crate::cred_cli::CredResult;

/// Where the gateway is: on this box, or an account's `.duck` handle reached
/// through the node's browser gateway. ONE discriminant, so no verb has to
/// infer the mode from which flags happen to be set.
#[derive(Debug, clap::Args)]
pub(crate) struct GatewayArgs {
    /// a LOCAL gateway's base url
    #[arg(long, value_name = "URL", conflicts_with = "remote")]
    host: Option<String>,
    /// a REMOTE gateway's duck handle (e.g. `airlock.alice.duck`), routed onto
    /// the overlay through this node's own browser gateway
    #[arg(long, value_name = "HANDLE")]
    remote: Option<String>,
}

/// Which silicon, and the transport bits its roots need. The roots themselves
/// are pinned in-crate; these select the product and where to fetch from.
#[derive(Debug, clap::Args)]
pub(crate) struct AttestArgs {
    /// the enclave's attestation family
    #[arg(long, value_name = "KIND")]
    attest: AttestKind,
    /// TDX only: a PCCS to fetch collateral from (default: Intel PCS)
    #[arg(long, value_name = "URL")]
    pccs_url: Option<String>,
    /// SNP only: the CPU generation the VCEK chains to
    #[arg(long, value_name = "PRODUCT")]
    snp_product: Option<SnpProductArg>,
    /// SNP only: a VCEK DER on disk (default: fetch from AMD's KDS)
    #[arg(long, value_name = "PATH")]
    snp_vcek: Option<std::path::PathBuf>,
    /// SNP only: the ARK (root) certificate PEM to chain to, supplied out of
    /// band together with --snp-ask, instead of AMD's roots pinned in this
    /// binary. Only for a chain that is not AMD's — a test enclave.
    #[arg(long, value_name = "PATH", requires = "snp_ask")]
    snp_ark: Option<std::path::PathBuf>,
    /// SNP only: the ASK (intermediate) certificate PEM beside --snp-ark.
    #[arg(long, value_name = "PATH", requires = "snp_ark")]
    snp_ask: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum AttestKind {
    Tdx,
    Snp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum SnpProductArg {
    Milan,
    Genoa,
    Turin,
}

/// Which credential to seal, and where it comes from.
#[derive(Debug, clap::Args)]
pub(crate) struct SealArgs {
    /// the name the enclave stores it under (a session's `sub`)
    #[arg(long, value_name = "NAME", default_value = "compute-provider")]
    name: String,
    /// which vendor the enclave routes this credential to (`apple-codesign`
    /// is a release-signing identity, given by the --p12/--api-key group)
    #[arg(long, value_name = "VENDOR", default_value = "claude")]
    vendor: VendorArg,
    #[command(flatten)]
    apple: SealAppleArgs,
    /// the measurement the quote must match, lowercase hex. `inspect` prints it.
    #[arg(long, value_name = "HEX")]
    measurement: String,
    /// a vendor login artifact to read the credential out of
    #[arg(long, value_name = "PATH", conflicts_with = "token_stdin")]
    credentials: Option<std::path::PathBuf>,
    /// with --credentials: seal the artifact's CURRENT access token (no
    /// rotation, so the owner's own login keeps working) or its refresh
    /// token. With --token-stdin: which kind the piped token is.
    #[arg(long, value_name = "KIND", default_value = "bearer")]
    cred_kind: SealKind,
    /// seal a bare token read from stdin (one line, no trailing newline) —
    /// the `ducktape user` family's stdin-only rule for secrets: a live
    /// vendor token never crosses argv, where it would leak into shell
    /// history and `ps`/`/proc/<pid>/cmdline`. `--cred-kind` says whether the
    /// piped line is a bearer access token or an OAuth refresh token.
    #[arg(long, conflicts_with = "credentials")]
    token_stdin: bool,
}

/// The `apple-codesign` identity for `--vendor apple-codesign`: the same four
/// inputs `cred add apple-codesign` takes ([`crate::cred_cli::AppleCodesignArgs`]),
/// each required exactly when that vendor is named and refused otherwise.
#[derive(Debug, clap::Args)]
pub(crate) struct SealAppleArgs {
    /// the Developer ID Application certificate + key as PKCS#12
    #[arg(long, value_name = "PATH", required_if_eq("vendor", "apple-codesign"))]
    p12: Option<std::path::PathBuf>,
    /// a file holding the PKCS#12 password (one line)
    #[arg(long, value_name = "PATH", required_if_eq("vendor", "apple-codesign"))]
    p12_password_file: Option<std::path::PathBuf>,
    /// the App Store Connect API key JSON ({key_id, issuer_id, private_key})
    #[arg(long, value_name = "PATH", required_if_eq("vendor", "apple-codesign"))]
    api_key: Option<std::path::PathBuf>,
    /// the Apple Team ID the certificate's OU must equal
    #[arg(long, value_name = "ID", required_if_eq("vendor", "apple-codesign"))]
    team_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum VendorArg {
    Claude,
    Codex,
    AppleCodesign,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum SealKind {
    Bearer,
    Refresh,
}

#[cfg(not(feature = "verify"))]
const NEEDS_VERIFY_BUILD: &str =
    "this verb verifies a TEE quote and needs a `--features verify` build of ducktape";

#[cfg(not(feature = "verify"))]
pub(crate) fn cmd_inspect(
    _gateway: GatewayArgs,
    _attest_args: AttestArgs,
    _node_base: impl FnOnce() -> Result<String, Box<dyn std::error::Error>>,
) -> CredResult {
    Err(NEEDS_VERIFY_BUILD.into())
}

#[cfg(not(feature = "verify"))]
pub(crate) fn cmd_seal(
    _gateway: GatewayArgs,
    _attest_args: AttestArgs,
    _seal: SealArgs,
    _node_base: impl FnOnce() -> Result<String, Box<dyn std::error::Error>>,
    _stdin: &mut impl std::io::BufRead,
) -> CredResult {
    Err(NEEDS_VERIFY_BUILD.into())
}

#[cfg(feature = "verify")]
impl AttestArgs {
    fn mode(&self) -> AttestMode {
        match self.attest {
            AttestKind::Tdx => AttestMode::Tdx,
            AttestKind::Snp => AttestMode::Snp,
        }
    }

    /// Flags → typed trust roots. Resolved BEFORE any network call, so a bad
    /// `--snp-product` / unreadable `--snp-vcek` fails fast rather than after a
    /// round trip.
    fn roots(&self) -> Result<TrustRoots, Box<dyn std::error::Error>> {
        match self.attest {
            AttestKind::Tdx => Ok(TrustRoots::Tdx(TdxRoots {
                pccs_url: self.pccs_url.clone(),
            })),
            AttestKind::Snp => {
                let product = match self
                    .snp_product
                    .ok_or("--attest snp requires --snp-product milan|genoa|turin")?
                {
                    SnpProductArg::Milan => SnpProduct::Milan,
                    SnpProductArg::Genoa => SnpProduct::Genoa,
                    SnpProductArg::Turin => SnpProduct::Turin,
                };
                let vcek = match &self.snp_vcek {
                    Some(path) => VcekSource::Der(
                        std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
                    ),
                    None => VcekSource::Kds,
                };
                let roots = match (&self.snp_ark, &self.snp_ask) {
                    (Some(ark), Some(ask)) => SnpRoots {
                        product,
                        ca: ca_chain_from_pem(ark, ask)?,
                        vcek,
                    },
                    (None, None) => SnpRoots::amd(product, vcek)?,
                    // clap's `requires` pairs the two; one alone is a parser
                    // out of step, named as such.
                    (Some(_), None) | (None, Some(_)) => {
                        return Err("--snp-ark and --snp-ask travel together".into());
                    }
                };
                Ok(TrustRoots::Snp(Box::new(roots)))
            }
        }
    }
}

/// An ARK+ASK chain read off two PEM files — the out-of-band roots.
#[cfg(feature = "verify")]
fn ca_chain_from_pem(
    ark: &std::path::Path,
    ask: &std::path::Path,
) -> Result<airlock::verify::CaChain, Box<dyn std::error::Error>> {
    let read = |path: &std::path::Path| {
        std::fs::read(path).with_context(|| format!("read {}", path.display()))
    };
    Ok(airlock::verify::CaChain::from_pem(&read(ark)?, &read(ask)?)
        .map_err(|e| format!("snp roots: {e}"))?)
}

/// Build the gateway handle. Remote mode reads the node's own browser-gateway
/// base — the transport that carries a `.duck` authority onto the overlay —
/// rather than making the operator paste it.
#[cfg(feature = "verify")]
fn resolve_gateway(
    args: &GatewayArgs,
    node_base: impl FnOnce() -> Result<String, Box<dyn std::error::Error>>,
) -> Result<Gateway, Box<dyn std::error::Error>> {
    let Some(handle) = args.remote.clone() else {
        let host = args
            .host
            .clone()
            .ok_or("give the gateway: --host <url> (local) or --remote <handle>.duck")?;
        return Ok(Gateway::local(host));
    };
    let via = crate::node_http::get_json(&node_base()?, "/v1/gateway/browser")
        .map_err(|error| format!("read this node's browser gateway base: {error}"))?["base"]
        .as_str()
        .ok_or("this node serves no browser gateway, so it cannot route a .duck authority")?
        .to_string();
    Ok(Gateway::remote(handle, via))
}

/// `cred inspect` — print the enclave measurement the quote carries.
///
/// TOFU for bootstrap: in production the measurement comes from the audited
/// build, not from the enclave being asked to describe itself. Printing it is a
/// convenience for pinning, never a verification.
#[cfg(feature = "verify")]
pub(crate) fn cmd_inspect(
    gateway: GatewayArgs,
    attest_args: AttestArgs,
    node_base: impl FnOnce() -> Result<String, Box<dyn std::error::Error>>,
) -> CredResult {
    let gw = resolve_gateway(&gateway, node_base)?;
    let mode = attest_args.mode();
    let (quote, vendor) = block_on(gw.fetch_quote())?;
    let (mrtd_hex, report_data) = airlock::verify::peek_measurement(mode, &quote)?;
    let (seal_pk, sess_pk) = attest::split_report_data(&report_data);

    eprintln!(
        "attest={mode:?} vendor={vendor} quote={} bytes",
        quote.len()
    );
    eprintln!("REPORTDATA seal_pk = {}", hex::encode(seal_pk));
    eprintln!("REPORTDATA sess_pk = {}", hex::encode(sess_pk));
    eprintln!(
        "--- pin the line below as --measurement (TOFU; in prod pin from the audited build) ---"
    );
    // stdout is the measurement alone, so `$(... cred inspect ...)` is usable.
    println!("{mrtd_hex}");
    Ok(())
}

/// `cred seal` — verify the quote, then seal and upload the credential.
///
/// The credential is released ONLY after the quote proves the pinned
/// measurement: seal_pk is trusted because [`airlock::verify::verify_quote`]
/// verified the chain that binds it, never because the gateway asserted it.
///
/// An `apple-codesign` identity is also REGISTERED: the owner-signed
/// on-chain record (`SetCredential`, pinning the attested seal_pk, naming the
/// node this verb dials as the publisher) and the account's two airlock
/// routes, so `ducktape release sign-bundle --credential <name>` resolves
/// the enclave from committed state with no hand-signed statement. A model
/// credential sealed into a TEE is not: its borrower pins the measurement
/// (`DUCKTAPE_AIRLOCK_MEASUREMENT`), never a record.
#[cfg(feature = "verify")]
pub(crate) fn cmd_seal(
    ctx: &crate::cred_cli::VerbCtx,
    gateway: GatewayArgs,
    attest_args: AttestArgs,
    seal: SealArgs,
    stdin: &mut impl std::io::BufRead,
) -> CredResult {
    // resolve everything local and fallible BEFORE the network: bad roots, a bad
    // measurement or an unreadable artifact must fail before a quote is fetched.
    let roots = attest_args.roots()?;
    let expected = Measurement::from_hex(&seal.measurement)?;
    let kind = match seal.vendor {
        VendorArg::Claude => CredentialKind::Claude,
        VendorArg::Codex => CredentialKind::Codex,
        VendorArg::AppleCodesign => CredentialKind::AppleCodesign,
    };
    let credential = match kind {
        CredentialKind::Claude | CredentialKind::Codex => resolve_credential(&seal, stdin)?,
        CredentialKind::AppleCodesign => resolve_apple_codesign(&seal)?,
    };
    let gw = resolve_gateway(&gateway, || ctx.http_base())?;

    let seal_pk = block_on(async {
        let (quote, _vendor) = gw.fetch_quote().await?;
        let report_data = airlock::verify::verify_quote(&quote, &expected, &roots)
            .await
            .map_err(|e| anyhow::anyhow!("attestation_unverified: {e:#}"))?;
        anyhow::Ok(attest::split_report_data(&report_data).0)
    })?;
    println!(
        "quote verified: measurement matches the audited image ({}…), seal key bound",
        &expected.to_hex()[..12]
    );

    block_on(gw.upload_sealed_credential(&seal_pk, &seal.name, kind, &credential))?;
    let rotation = match &credential {
        CredentialPayload::Bearer { .. } => "static access token (no rotation)",
        CredentialPayload::Refresh { .. } => "refresh token (OAuth, rotates in-enclave)",
        CredentialPayload::AppleCodesign { .. } => "apple-codesign identity (held for signing)",
    };
    println!(
        "sealed {rotation} and uploaded as {:?} (the gateway never sees it in clear)",
        seal.name
    );
    match kind {
        CredentialKind::Claude | CredentialKind::Codex => Ok(()),
        CredentialKind::AppleCodesign => register_signing_credential(ctx, &seal.name, seal_pk, stdin),
    }
}

/// The on-chain half of a TEE-held signing identity: the record under the
/// ATTESTED seal key, and the account's `airlock` + `airlock-sign` routes
/// naming the dialed node as their publisher — the node that binds both
/// labels to the enclave's loopback port (`gateway bind`). The node is read
/// for that identity and its chain off `/v1/status`; no workspace is opened,
/// as this verb holds no store.
#[cfg(feature = "verify")]
fn register_signing_credential(
    ctx: &crate::cred_cli::VerbCtx,
    name: &str,
    seal_pk: [u8; 32],
    stdin: &mut impl std::io::BufRead,
) -> CredResult {
    use crate::cred_cli::{AirlockLane, Publisher, ensure_airlock_route, submit_credential_record};
    gateway::validate_credential_name(name)?;
    let base = ctx.http_base()?;
    let publisher = Publisher::of_node(&base)?;
    let user = crate::userkey_cli::load_user_signer(&ctx.key_path()?, stdin)?;
    let owner = crate::cred_cli::query_owner_account_view(&base, user.public_key().as_ref())?;
    submit_credential_record(
        &base,
        &user,
        &publisher,
        owner.number,
        name,
        gateway::CredentialKind::AppleCodesign,
        seal_pk,
    )?;
    for lane in [AirlockLane::Model, AirlockLane::Sign] {
        ensure_airlock_route(&base, &user, &publisher, owner.number, lane)?;
    }
    println!(
        "bind both labels to the enclave's port on this node: ducktape gateway bind --label {} --port <port> --account {}; then --label {}",
        AirlockLane::Model.label(),
        owner.number,
        AirlockLane::Sign.label()
    );
    Ok(())
}

/// Which secret to seal. `--token-stdin` wins; otherwise read the vendor
/// artifact. Both a bare token and the artifact are the same secret class —
/// the difference is only where the bytes come from — so `stdin` is threaded
/// here for the token case and unused otherwise.
#[cfg(feature = "verify")]
fn resolve_credential(
    seal: &SealArgs,
    stdin: &mut impl std::io::BufRead,
) -> Result<CredentialPayload, Box<dyn std::error::Error>> {
    if seal.token_stdin {
        let token = crate::userkey_cli::prompt_stdin_line(stdin, "token")?;
        return Ok(match seal.cred_kind {
            SealKind::Bearer => CredentialPayload::Bearer { access_token: token },
            SealKind::Refresh => CredentialPayload::Refresh {
                refresh_token: token,
                access_token: String::new(),
                expires_at: 0,
            },
        });
    }
    let path = seal
        .credentials
        .as_ref()
        .ok_or("give the secret: --credentials <path> or --token-stdin")?;
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let json: serde_json::Value = serde_json::from_str(&raw).context("credentials json")?;
    let oauth = &json["claudeAiOauth"];
    let field = |key: &str| -> Result<String, String> {
        oauth[key]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("{}: claudeAiOauth.{key} not found", path.display()))
    };
    match seal.cred_kind {
        SealKind::Bearer => Ok(CredentialPayload::Bearer {
            access_token: field("accessToken")?,
        }),
        SealKind::Refresh => Ok(CredentialPayload::Refresh {
            refresh_token: field("refreshToken")?,
            access_token: oauth["accessToken"].as_str().unwrap_or("").to_string(),
            expires_at: oauth["expiresAt"].as_u64().map(|ms| ms / 1000).unwrap_or(0),
        }),
    }
}

/// The signing identity out of the `--p12`/`--api-key` group, admitted
/// locally by the gateway's own checks so a refusal is named before the
/// quote is fetched.
#[cfg(feature = "verify")]
fn resolve_apple_codesign(
    seal: &SealArgs,
) -> Result<CredentialPayload, Box<dyn std::error::Error>> {
    // clap requires all four under `--vendor apple-codesign`; a missing one
    // here is a parser out of step, named as such.
    let SealAppleArgs {
        p12: Some(p12),
        p12_password_file: Some(pw),
        api_key: Some(key),
        team_id: Some(team),
    } = &seal.apple
    else {
        return Err(
            "--vendor apple-codesign takes --p12, --p12-password-file, --api-key and --team-id"
                .into(),
        );
    };
    Ok(crate::cred_cli::read_and_admit_apple_codesign(p12, pw, key, team)?.payload())
}

/// The `cred` family is a synchronous CLI; the airlock client is async. One
/// current-thread runtime per verb, built where it is used.
#[cfg(feature = "verify")]
fn block_on<T>(
    future: impl std::future::Future<Output = anyhow::Result<T>>,
) -> Result<T, Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    Ok(runtime.block_on(future)?)
}
