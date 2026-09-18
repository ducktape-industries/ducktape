//! `ducktape user cred` — named, grantable, owner-hosted API credentials.
//!
//! `cred add claude|codex` wraps the vendor's OWN login CLI (`claude auth login`,
//! `codex login`) on a local pty, captures the login artifact directly into this
//! node's disk-backed gateway store (`<storage>/airlock-creds/<name>/`), and
//! registers the record on-chain (name → owner account, publisher node, kind,
//! seal_pk) so any granted node's broker can resolve the name and complete a
//! round-trip through the owner's co-hosted gateway WITHOUT ever holding the
//! secret. `cred add apple-codesign` enrols a Developer ID signing identity
//! the same way, from files the operator exported (no vendor login exists),
//! after running the gateway's own admission checks locally so a refused
//! identity never reaches the store or the chain. `grant`/`revoke`
//! lend/rescind by account; `list` reads committed records; `remove`
//! tombstones one.
//!
//! `inspect`/`seal` are the ENCLAVE half of the same family and live in
//! [`crate::cred_seal`]: they talk to a TEE airlock gateway
//! (`bin/airlock-gateway`) whose quote they verify themselves. They are the only
//! two verbs here that neither sign an on-chain statement nor touch the store.
//!
//! Every other verb runs CO-HOSTED with the node whose credential it manages: it reads
//! the node's own workspace (chain id, consensus key, storage) to build the
//! owner-signed statement, then submits it as a USER-signed frame over
//! `/v1/submit/frame` — the frame's signer is the op origin, which the gateway
//! resolves to the owner account through `OfKey`; the record's
//! `publisher_node` is a statement field the account vouches for. Program
//! output stays `println!` (a CLI's stdout is not logging).

use std::io::BufRead;
use std::path::Path;

use commonware_cryptography::Signer as _;

use crate::account_cli::resolve_account_authority;
use crate::cli_args::NodeAddr;
use crate::config;
use crate::userkey_cli::load_user_signer;

pub(crate) type CredResult = Result<(), Box<dyn std::error::Error>>;

/// `ducktape user cred <verb>` — the credential subfamily. `--node`/`-n` are the
/// shared [`NodeAddr`] group every family carries, `global` so they attach in
/// any position (`cred add claude -n net` reads naturally).
#[derive(Debug, clap::Args)]
pub(crate) struct CredArgs {
    #[command(subcommand)]
    cmd: CredCmd,
    #[command(flatten)]
    addr: NodeAddr,
    /// path to the user key file (defaults to the keystore's active wallet)
    #[arg(long, value_name = "PATH", global = true)]
    key: Option<std::path::PathBuf>,
}

#[derive(Debug, clap::Subcommand)]
pub(crate) enum CredCmd {
    /// capture a credential into this node's store and register the record
    Add {
        #[command(subcommand)]
        what: AddCmd,
    },
    /// list every registered credential record
    List {
        /// print the records as JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// tombstone one owned credential record
    Remove {
        /// the credential name
        name: String,
    },
    /// lend a credential to an account: work that account SUBMITS may draw on
    /// it while it executes on the node the account pinned it to (owner-signed)
    Grant {
        /// the credential name
        name: String,
        /// an account NUMBER. A display name is refused: it is freely
        /// rewritable and not unique, so it cannot name who this credential
        /// trusts (look the number up with `ducktape account show`). The
        /// grant reaches exactly the runs this account's keys sign (`agent
        /// sched --host-node`, which names this credential in the committed
        /// work), on the node each run is pinned to, until that run ends.
        /// Nobody else's work.
        account: String,
    },
    /// rescind a lend (owner-signed). In flight sessions keep working until their
    /// token expires; this stops new ones opening
    Revoke {
        /// the credential name
        name: String,
        /// the account NUMBER to stop lending to — the same account `grant`
        /// named (a display name is refused; see `grant`'s help)
        account: String,
    },
    /// read a TEE gateway's enclave measurement out of its quote, so it can be
    /// pinned as `seal --measurement` (needs a `--features verify` build)
    Inspect {
        #[command(flatten)]
        gateway: crate::cred_seal::GatewayArgs,
        #[command(flatten)]
        attest: crate::cred_seal::AttestArgs,
    },
    /// verify a TEE gateway's quote, then seal a credential under the attested
    /// key and upload it (needs a `--features verify` build)
    Seal {
        #[command(flatten)]
        gateway: crate::cred_seal::GatewayArgs,
        #[command(flatten)]
        attest: crate::cred_seal::AttestArgs,
        #[command(flatten)]
        seal: crate::cred_seal::SealArgs,
    },
}

/// What `cred add` captures. The two model vendors run their own login CLI;
/// the signing identity is enrolled from exported files.
#[derive(Debug, clap::Subcommand)]
pub(crate) enum AddCmd {
    /// wrap `claude auth login`, store the artifact, register the record
    Claude {
        /// the credential name (default `<display>-claude-<n>`)
        name: Option<String>,
    },
    /// wrap `codex login`, store the artifact, register the record
    Codex {
        /// the credential name (default `<display>-codex-<n>`)
        name: Option<String>,
    },
    /// enrol a Developer ID Application identity + App Store Connect key for
    /// release signing (validated here exactly as the gateway validates it)
    AppleCodesign {
        /// the credential name (default `<display>-apple-codesign-<n>`)
        name: Option<String>,
        #[command(flatten)]
        identity: AppleCodesignArgs,
    },
}

/// The four inputs of an `apple-codesign` credential, as files: the PKCS#12
/// and its password (a file, never argv — a password on argv is in `ps` and
/// shell history), the App Store Connect key as the one-file JSON
/// `rcodesign encode-app-store-connect-api-key` writes, and the Team ID the
/// certificate must name. Shared by `cred add apple-codesign` (this node's
/// store) and `cred seal --vendor apple-codesign` (a TEE gateway).
#[derive(Debug, clap::Args)]
pub(crate) struct AppleCodesignArgs {
    /// the Developer ID Application certificate + key as PKCS#12
    #[arg(long, value_name = "PATH")]
    pub(crate) p12: std::path::PathBuf,
    /// a file holding the PKCS#12 password (one line)
    #[arg(long, value_name = "PATH")]
    pub(crate) p12_password_file: std::path::PathBuf,
    /// the App Store Connect API key JSON ({key_id, issuer_id, private_key})
    #[arg(long, value_name = "PATH")]
    pub(crate) api_key: std::path::PathBuf,
    /// the Apple Team ID the certificate's OU must equal
    #[arg(long, value_name = "ID")]
    pub(crate) team_id: String,
}

/// The material an `apple-codesign` enrolment read and admitted: the wire
/// payload the gateway takes, plus the raw bytes the store keeps.
pub(crate) struct AppleCodesignMaterial {
    pub(crate) p12: Vec<u8>,
    pub(crate) p12_password: String,
    pub(crate) api_key_json: String,
    pub(crate) team_id: String,
}

impl AppleCodesignMaterial {
    pub(crate) fn payload(&self) -> airlock::wire::CredentialPayload {
        use base64::Engine as _;
        airlock::wire::CredentialPayload::AppleCodesign {
            p12_b64: base64::engine::general_purpose::STANDARD.encode(&self.p12),
            p12_password: self.p12_password.clone(),
            api_key_json: self.api_key_json.clone(),
            team_id: self.team_id.clone(),
        }
    }
}

impl AppleCodesignArgs {
    pub(crate) fn read_and_admit(
        &self,
    ) -> Result<AppleCodesignMaterial, Box<dyn std::error::Error>> {
        read_and_admit_apple_codesign(
            &self.p12,
            &self.p12_password_file,
            &self.api_key,
            &self.team_id,
        )
    }
}

/// Read the four inputs and run the gateway's admission on them
/// (`airlock::codesign`), so a refusal is named HERE by the same token the
/// gateway would answer with, before anything is written, sealed or signed.
pub(crate) fn read_and_admit_apple_codesign(
    p12_path: &Path,
    p12_password_file: &Path,
    api_key_path: &Path,
    team_id: &str,
) -> Result<AppleCodesignMaterial, Box<dyn std::error::Error>> {
    let read =
        |path: &Path| std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()));
    let p12 = read(p12_path)?;
    let p12_password = String::from_utf8(read(p12_password_file)?)
        .map_err(|_| format!("{}: not utf-8", p12_password_file.display()))?
        .trim_end_matches(['\r', '\n'])
        .to_string();
    let api_key_json = String::from_utf8(read(api_key_path)?)
        .map_err(|_| format!("{}: not utf-8", api_key_path.display()))?;
    let team_id = team_id.trim().to_string();
    let material = AppleCodesignMaterial {
        p12,
        p12_password,
        api_key_json,
        team_id,
    };
    {
        let airlock::wire::CredentialPayload::AppleCodesign {
            p12_b64,
            p12_password,
            api_key_json,
            team_id,
        } = material.payload()
        else {
            unreachable!("payload() builds the apple-codesign arm");
        };
        airlock::codesign::AppleCodesign::admit(&p12_b64, &p12_password, &api_key_json, &team_id)
            .map_err(|refusal| format!("apple-codesign identity refused: {refusal}"))?;
    }
    Ok(material)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ProviderArg {
    Claude,
    Codex,
}

impl ProviderArg {
    /// the on-chain kind this provider registers as.
    fn kind(self) -> gateway::CredentialKind {
        match self {
            Self::Claude => gateway::CredentialKind::Claude,
            Self::Codex => gateway::CredentialKind::Codex,
        }
    }

    /// the lowercase token used in `kind` files and default names.
    pub(crate) fn token(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// the vendor login binary (also the which-preflight target).
    fn binary(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// the login subcommand argv the pty runs.
    fn login_args(self) -> &'static [&'static str] {
        match self {
            // `auth login`, NOT `setup-token`. setup-token mints an
            // inference-only token and PRINTS it to the tty — it writes no
            // `.credentials.json`, so the artifact watch never fires and
            // `cred add` hangs forever after the login completes. `auth login`
            // runs the full-scope OAuth and writes `$CLAUDE_CONFIG_DIR/.credentials.json`
            // (`claudeAiOauth` accessToken/refreshToken/expiresAt) — the exact
            // artifact the airlock reads and refreshes.
            Self::Claude => &["auth", "login"],
            // `--device-auth`: a device-code flow (enter a code on any browser)
            // instead of the default localhost:1455 redirect, which needs a
            // browser on THIS host — wrong for a headless / SSH operator node.
            Self::Codex => &["login", "--device-auth"],
        }
    }

    /// the env var pointing the vendor CLI at the per-credential store dir.
    fn config_env(self) -> &'static str {
        match self {
            Self::Claude => "CLAUDE_CONFIG_DIR",
            Self::Codex => "CODEX_HOME",
        }
    }

    /// the login artifact filename the vendor CLI writes into the store dir.
    fn artifact(self) -> &'static str {
        match self {
            Self::Claude => ".credentials.json",
            Self::Codex => "auth.json",
        }
    }

    /// the install hint printed when the binary is missing.
    fn install_hint(self) -> &'static str {
        match self {
            Self::Claude => "npm install -g @anthropic-ai/claude-code",
            Self::Codex => "npm install -g @openai/codex",
        }
    }
}

/// Dispatch one `cred` verb. `stdin` is threaded to [`load_user_signer`], which
/// reads the key password from it only when the key file is encrypted.
pub(crate) fn run(args: CredArgs, stdin: &mut impl BufRead) -> CredResult {
    let CredArgs { cmd, addr, key } = args;
    let ctx = VerbCtx { addr, key };
    match cmd {
        CredCmd::Add { what } => match what {
            AddCmd::Claude { name } => cmd_add(&ctx, ProviderArg::Claude, name, stdin),
            AddCmd::Codex { name } => cmd_add(&ctx, ProviderArg::Codex, name, stdin),
            AddCmd::AppleCodesign { name, identity } => {
                cmd_add_apple_codesign(&ctx, name, &identity, stdin)
            }
        },
        CredCmd::List { json } => cmd_list(&ctx, json),
        CredCmd::Remove { name } => cmd_remove(&ctx, name, stdin),
        CredCmd::Grant { name, account } => cmd_grant(&ctx, name, account, stdin),
        CredCmd::Revoke { name, account } => cmd_revoke(&ctx, name, account, stdin),
        // the enclave verbs resolve the node base LAZILY: only `--remote` needs
        // it (to read this node's browser-gateway base), so a purely local
        // `--host` inspect must not demand a workspace it never reads.
        CredCmd::Inspect { gateway, attest } => {
            crate::cred_seal::cmd_inspect(gateway, attest, || ctx.http_base())
        }
        CredCmd::Seal {
            gateway,
            attest,
            seal,
        } => crate::cred_seal::cmd_seal(&ctx, gateway, attest, seal, stdin),
    }
}

/// The shared node/key context every `cred` and `account` verb resolves against.
pub(crate) struct VerbCtx {
    pub(crate) addr: NodeAddr,
    pub(crate) key: Option<std::path::PathBuf>,
}

impl VerbCtx {
    /// the node's http base, through the one shared addressing ladder.
    pub(crate) fn http_base(&self) -> Result<String, Box<dyn std::error::Error>> {
        Ok(self.addr.resolve()?)
    }

    /// the user key path for the signing verbs: explicit `--key` wins, else
    /// `$DUCKTAPE_USER_KEY`, else the active wallet of the workspace behind
    /// the node this verb dials (a wallet is an identity ON a network, so it
    /// lives in that network's workspace). A MISSING key is a loud error,
    /// never a cue to mint: these verbs sign AS a key, and a silently minted
    /// stranger would be a fresh, accountless identity wearing the right path.
    pub(crate) fn key_path(&self) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
        let path = match (&self.key, keystore::wallet::env_user_key()) {
            (Some(explicit), _) => explicit.clone(),
            (None, Some(env)) => env,
            (None, None) => keystore::wallet::active_key_path(&self.addr.workspace()?)?,
        };
        if !path.exists() {
            return Err(format!(
                "no user key at {} — run `ducktape wallet new <name>` first, or pass --key",
                path.display()
            )
            .into());
        }
        Ok(path)
    }

    /// the co-hosted workspace behind the node this verb dials: chain id, the
    /// node's consensus key, and its storage dir. Required by every verb that
    /// mints an owner-signed statement or writes the store.
    ///
    /// [`NodeAddr::config_file`] is the ONE ladder that answers this, `--node`
    /// included: a bare url names no directory, so it is resolved backwards
    /// through the registry to the workspace that serves it.
    pub(crate) fn workspace(&self) -> Result<config::Resolved, Box<dyn std::error::Error>> {
        Ok(config::resolve(&self.addr.config_file()?)?)
    }
}

// ============================================================================
// list
// ============================================================================

fn cmd_list(ctx: &VerbCtx, json: bool) -> CredResult {
    let base = ctx.http_base()?;
    let records = query_credentials(&base)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&records)?);
        return Ok(());
    }
    if records.is_empty() {
        println!("no credentials registered");
        return Ok(());
    }
    println!("{:<24} {:<14} {:<8} grants", "name", "kind", "owner");
    for record in &records {
        let kind = airlock_service::kind_token(crate::compute::cred::service_kind(record.kind));
        println!(
            "{:<24} {:<14} {:<8} {}",
            record.name,
            kind,
            record.owner_account,
            record.grants.len()
        );
    }
    Ok(())
}

// ============================================================================
// grant / revoke / remove — owner-signed statement, user-signed frame
// ============================================================================

fn cmd_grant(ctx: &VerbCtx, name: String, account: String, stdin: &mut impl BufRead) -> CredResult {
    let base = ctx.http_base()?;
    let resolved = ctx.workspace()?;
    let user = load_user_signer(&ctx.key_path()?, stdin)?;
    let owner_account = query_owner_account(&base, user.public_key().as_ref())?;
    let grantee = resolve_account_authority(&base, &account)?;
    let statement = gateway::CredentialGrantStatement {
        chain_id: resolved.service.chain_id.clone(),
        owner_account,
        name: name.clone(),
        account: grantee,
    };
    let preimage = gateway::grant_credential_preimage(&statement)?;
    let message = gateway::GatewayMsg::GrantCredential {
        statement,
        authorization: authorize(&user, &preimage),
    };
    let height = submit_gateway(&base, &user, &message)?;
    println!("granted at height {height}");
    println!(
        "note: this lends to the work account {grantee} SUBMITS. A run one of its keys \
         signs can draw on {name:?} on the node it is pinned to, until that run ends."
    );
    Ok(())
}

fn cmd_revoke(
    ctx: &VerbCtx,
    name: String,
    account: String,
    stdin: &mut impl BufRead,
) -> CredResult {
    let base = ctx.http_base()?;
    let resolved = ctx.workspace()?;
    let user = load_user_signer(&ctx.key_path()?, stdin)?;
    let owner_account = query_owner_account(&base, user.public_key().as_ref())?;
    let grantee = resolve_account_authority(&base, &account)?;
    let statement = gateway::CredentialGrantStatement {
        chain_id: resolved.service.chain_id.clone(),
        owner_account,
        name,
        account: grantee,
    };
    let preimage = gateway::revoke_credential_preimage(&statement)?;
    let message = gateway::GatewayMsg::RevokeCredential {
        statement,
        authorization: authorize(&user, &preimage),
    };
    let height = submit_gateway(&base, &user, &message)?;
    println!("revoked at height {height}");
    Ok(())
}

fn cmd_remove(ctx: &VerbCtx, name: String, stdin: &mut impl BufRead) -> CredResult {
    gateway::validate_credential_name(&name)?;
    let base = ctx.http_base()?;
    let resolved = ctx.workspace()?;

    // Converge a rerun whose earlier attempt committed the tombstone but failed
    // the local cleanup: resubmitting an already-removed name is Rejected by
    // the gateway ("credential is not registered"), which would strand the
    // local secret dir forever. Names are chain-global, so an absent name means
    // the tombstone is done and only the local half can remain. (A name that
    // exists but is owned by someone else still takes the submit path below and
    // gets the gateway's own refusal.)
    let registered = query_credentials(&base)?
        .iter()
        .any(|record| record.name == name);
    if !registered {
        return finish_local_removal_only(&resolved.service.storage_dir, &name);
    }

    let user = load_user_signer(&ctx.key_path()?, stdin)?;
    let owner_account = query_owner_account(&base, user.public_key().as_ref())?;
    let statement = gateway::RemoveCredentialStatement {
        chain_id: resolved.service.chain_id.clone(),
        owner_account,
        name: name.clone(),
    };
    let preimage = gateway::remove_credential_preimage(&statement)?;
    let message = gateway::GatewayMsg::RemoveCredential {
        statement,
        authorization: authorize(&user, &preimage),
    };
    let height = submit_gateway(&base, &user, &message)?;
    let removed_local = remove_local_credential(&resolved.service.storage_dir, &name)
        .map_err(|error| format!("removed on-chain at height {height}, but {error}"))?;
    if removed_local {
        println!("removed at height {height}");
    } else {
        println!(
            "removed at height {height}; no local credential files on this workspace — \
             if `cred add` ran on another workspace, remove its files there"
        );
    }
    Ok(())
}

/// The rerun tail of `cred remove`: the tombstone already committed, so only the
/// local half is left. Erroring when nothing is local either keeps a mistyped
/// name loud instead of "removing" nothing.
fn finish_local_removal_only(storage: &Path, name: &str) -> CredResult {
    let removed_local = remove_local_credential(storage, name)?;
    if !removed_local {
        return Err(format!(
            "credential {name} is not registered, and no local credential files exist on this workspace"
        )
        .into());
    }
    println!("not registered on-chain (already removed); local credential files cleaned up");
    Ok(())
}

/// Delete exactly one validated credential directory after its consensus
/// tombstone commits, reporting whether one existed. `seal.key` and sibling
/// credentials live beside it and must survive. Already absent is still success
/// (the postcondition holds) — but the caller words its report differently,
/// because on a machine with several registered workspaces an absent dir HERE
/// usually means `cred add` ran on another one and its files survive there.
fn remove_local_credential(storage: &Path, name: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let dir = airlock_service::cred_store_root(storage).join(name);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("remove local credential {}: {error}", dir.display()).into()),
    }
}

// ============================================================================
// add — preflight, pty login wrap, artifact capture, register
// ============================================================================

/// What every `cred add` resolves before it captures anything: the node, the
/// workspace, the signing user, their account, and the (defaulted, validated)
/// credential name with its 0700 store dir.
struct Enrolment {
    base: String,
    resolved: config::Resolved,
    user: commonware_cryptography::ed25519::PrivateKey,
    account: identity::AccountView,
    name: String,
    store: std::path::PathBuf,
    dir: std::path::PathBuf,
}

/// Resolve the enrolment context and prepare the credential's store dir.
/// `kind_token` is the default name's middle (`<display>-<token>-<n>`).
fn begin_enrolment(
    ctx: &VerbCtx,
    name: Option<String>,
    kind_token: &str,
    stdin: &mut impl BufRead,
) -> Result<Enrolment, Box<dyn std::error::Error>> {
    let base = ctx.http_base()?;
    let resolved = ctx.workspace()?;
    let user = load_user_signer(&ctx.key_path()?, stdin)?;
    let user_pub = user.public_key().as_ref().to_vec();

    // owner account (membership check) + existing names (for the default's
    // counter) — one identity query, one gateway query.
    let account = query_owner_account_view(&base, &user_pub)?;
    let existing = query_credentials(&base)?;
    let existing_names: Vec<&str> = existing.iter().map(|r| r.name.as_str()).collect();
    let name = match name {
        Some(name) => name,
        None => derive_default_name(&account.name, kind_token, &existing_names),
    };
    gateway::validate_credential_name(&name)?;

    // the on-disk store, keyed by name. Both `airlock-creds/` and the
    // credential's own dir are 0700: what lands inside is a live vendor OAuth
    // secret or a signing identity, worth strictly more than the `seal.key`
    // beside it.
    let store = airlock_service::cred_store_root(&resolved.service.storage_dir);
    let dir = store.join(&name);
    airlock_service::create_private_dir(&store)?;
    airlock_service::create_private_dir(&dir)?;
    Ok(Enrolment {
        base,
        resolved,
        user,
        account,
        name,
        store,
        dir,
    })
}

/// `cred add apple-codesign`: admit the identity locally, write its four
/// files 0600 into the store, then register the record. The kind marker is
/// written LAST, as every kind does, so the lender's loader never sees a
/// half-written credential as a registered one.
fn cmd_add_apple_codesign(
    ctx: &VerbCtx,
    name: Option<String>,
    identity: &AppleCodesignArgs,
    stdin: &mut impl BufRead,
) -> CredResult {
    // the cheapest refusal first: an identity the gateway would refuse must
    // fail before a key password is asked for or a dir is created.
    let material = identity.read_and_admit()?;
    let kind = gateway::CredentialKind::AppleCodesign;
    let enrolment = begin_enrolment(
        ctx,
        name,
        airlock_service::kind_token(crate::compute::cred::service_kind(kind)),
        stdin,
    )?;
    write_apple_codesign_files(&enrolment.dir, &material)?;
    register_credential(&enrolment, kind)
}

/// Write the admitted identity into `dir` as the files the lender reads
/// (`airlock_service::apple_codesign_files`). A retry after a failed attempt
/// replaces every file, so a stale one never survives beside fresh ones.
fn write_apple_codesign_files(dir: &Path, material: &AppleCodesignMaterial) -> CredResult {
    let files = airlock_service::apple_codesign_files();
    let entries: [(&str, &[u8]); 4] = [
        (files.p12, &material.p12),
        (files.p12_password, material.p12_password.as_bytes()),
        (files.api_key, material.api_key_json.as_bytes()),
        (files.team_id, material.team_id.as_bytes()),
    ];
    for (file, bytes) in entries {
        let path = dir.join(file);
        let _ = std::fs::remove_file(&path);
        airlock_service::write_secret_0600(&path, bytes)?;
    }
    Ok(())
}

fn cmd_add(
    ctx: &VerbCtx,
    provider: ProviderArg,
    name: Option<String>,
    stdin: &mut impl BufRead,
) -> CredResult {
    let enrolment = begin_enrolment(ctx, name, provider.token(), stdin)?;
    let dir = &enrolment.dir;
    // capture the login artifact into the store dir.
    // `DUCKTAPE_CRED_REUSE_ARTIFACT=<path>` imports an ALREADY-authenticated
    // vendor login artifact (a `.credentials.json` / `auth.json` the operator
    // already produced) instead of driving the vendor's browser OAuth flow —
    // for headless hosts and re-registration without another auth round-trip.
    // Everything downstream (artifact check, kind, seal, record, sign, submit)
    // is identical to the browser path; the browser flow remains the default.
    let reuse = std::env::var("DUCKTAPE_CRED_REUSE_ARTIFACT")
        .ok()
        .filter(|src| !src.is_empty());
    match reuse {
        Some(src) => {
            // Read-then-write-0600 rather than `std::fs::copy`, which on Unix
            // replicates the SOURCE file's mode onto the destination — a
            // 0644-umask export would otherwise land world-readable inside a
            // 0700 dir. Stale-file removal mirrors the browser arm below, so a
            // retry after a prior failed attempt is unambiguous, not a
            // silent leftover.
            let bytes = std::fs::read(&src).map_err(|e| format!("reuse artifact {src}: {e}"))?;
            let artifact_path = dir.join(provider.artifact());
            let _ = std::fs::remove_file(&artifact_path);
            airlock_service::write_secret_0600(&artifact_path, &bytes)
                .map_err(|e| format!("reuse artifact {src}: {e}"))?;
        }
        None => {
            // The vendor binary is a requirement of THIS arm only: the reuse
            // path never execs it, and demanding it there refuses the headless
            // host the reuse path exists for.
            preflight_binary(provider)?;
            // Start clean so the login-completion watch (the artifact appearing)
            // is unambiguous — a stale file from a prior attempt would otherwise
            // read as instant success.
            let _ = std::fs::remove_file(dir.join(provider.artifact()));
            run_vendor_login(provider, dir)?
        }
    }

    let artifact = dir.join(provider.artifact());
    if !artifact.exists() {
        return Err(format!(
            "{} did not write {} — login did not complete; nothing registered",
            provider.binary(),
            provider.artifact()
        )
        .into());
    }
    register_credential(&enrolment, provider.kind())
}

/// The registration tail every kind shares: the `kind` marker (written last,
/// so the loader's "registered" test is the whole capture having landed), the
/// on-chain record under this node's seal key, and the account's airlock route.
fn register_credential(enrolment: &Enrolment, kind: gateway::CredentialKind) -> CredResult {
    let Enrolment {
        base,
        resolved,
        user,
        account,
        name,
        store,
        dir,
    } = enrolment;
    std::fs::write(
        dir.join("kind"),
        format!(
            "{}\n",
            airlock_service::kind_token(crate::compute::cred::service_kind(kind))
        ),
    )
    .map_err(|e| format!("write kind marker: {e}"))?;

    // the seal PUBLIC key the node co-hosts under (minted on first add, then
    // stable) is what the record pins for the compute broker.
    let seal = airlock_service::load_or_create_seal_keypair(store)?;
    let publisher = Publisher::of_workspace(resolved);
    submit_credential_record(
        base,
        user,
        &publisher,
        account.number,
        name,
        kind,
        seal.public_bytes(),
    )?;
    // A lent credential is only reachable once the co-hosted airlock gateway has
    // a signed on-chain route. That route is per-ACCOUNT (one `airlock` route
    // serves every credential this account co-hosts), so publish it once and
    // skip on later `cred add`s — the operator never hand-signs a RouteStatement.
    ensure_airlock_route(base, user, &publisher, account.number, AirlockLane::Model)?;
    // The record and the route are committed, but neither LENDS anything: the
    // credential is only reachable while the daemon that serves the store runs,
    // and nothing else in this flow — nor `cred list`, nor `gateway list` —
    // ever mentions it.
    println!(
        "lend it by running: ducktape service run {}",
        crate::services::AIRLOCK_KIND
    );
    Ok(())
}

/// One lane of the account's airlock gateway as a published route: its label
/// and what the overlay admits per request on it. Two exist — the model
/// lane every credential kind is lent over, and the signing lane a TEE-held
/// `apple-codesign` identity is reached over — both dialing the same enclave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AirlockLane {
    Model,
    // only `cred seal --vendor apple-codesign` publishes the signing lane,
    // and that verb's body exists only in a `verify` build.
    #[cfg_attr(not(feature = "verify"), allow(dead_code))]
    Sign,
}

impl AirlockLane {
    /// The route label, the constant the daemon/operator binds the loopback
    /// port under: one definition, so the publisher and the registrar cannot
    /// drift apart.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Model => crate::airlock::AIRLOCK_ROUTE,
            Self::Sign => crate::airlock::AIRLOCK_SIGN_ROUTE,
        }
    }

    /// The signed per-request cap: a model turn, or a release bundle.
    fn max_request_bytes(self) -> u64 {
        match self {
            Self::Model => crate::airlock::AIRLOCK_MODEL_REQUEST_BYTES,
            Self::Sign => crate::airlock::AIRLOCK_SIGN_REQUEST_BYTES,
        }
    }
}

/// The node a credential record and an airlock route name as their
/// publisher — the one whose loopback map carries the gateway's port — and
/// the chain the statements are minted for.
pub(crate) struct Publisher {
    pub(crate) chain_id: String,
    pub(crate) node: Vec<u8>,
}

impl Publisher {
    /// The co-hosted workspace's own node: what `cred add` publishes under,
    /// since the store it wrote lives beside that node.
    pub(crate) fn of_workspace(resolved: &config::Resolved) -> Self {
        Self {
            chain_id: resolved.service.chain_id.clone(),
            node: resolved.signer.public_key().as_ref().to_vec(),
        }
    }

    /// The node this verb dials, as `/v1/status` reports it: what `cred
    /// seal` publishes under, which holds no store and needs no workspace —
    /// the operator binds the enclave's port on that node.
    #[cfg(feature = "verify")]
    pub(crate) fn of_node(base: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let status = crate::node_http::get_json(base, "/v1/status")
            .map_err(|error| format!("read the node's status: {error}"))?;
        let chain_id = status["chain_id"]
            .as_str()
            .filter(|chain| !chain.is_empty())
            .ok_or("the node serves no chain, so it can publish nothing")?
            .to_string();
        let node = status["public_key"]
            .as_str()
            .and_then(|key| hex::decode(key).ok())
            .ok_or("the node reports no mesh identity, so it can serve no route")?;
        Ok(Self { chain_id, node })
    }
}

/// Submit the owner-signed on-chain record of one credential: its name, its
/// kind, the node that serves the gateway holding it, and the seal PUBLIC key
/// a borrower pins — the node's own store key for a self-hosted lender, the
/// attested enclave key for a TEE-held credential. Grants come later, through
/// `cred grant`. Returns nothing but prints the committed height.
pub(crate) fn submit_credential_record(
    base: &str,
    user: &commonware_cryptography::ed25519::PrivateKey,
    publisher: &Publisher,
    owner_account: u64,
    name: &str,
    kind: gateway::CredentialKind,
    seal_pk: [u8; 32],
) -> CredResult {
    let record = gateway::CredentialRecord {
        name: name.to_string(),
        owner_account,
        publisher_node: publisher.node.clone(),
        kind,
        seal_pk,
        grants: std::collections::BTreeSet::new(),
    };
    let statement = gateway::SetCredentialStatement {
        chain_id: publisher.chain_id.clone(),
        record,
    };
    let preimage = gateway::set_credential_preimage(&statement)?;
    let message = gateway::GatewayMsg::SetCredential {
        statement,
        authorization: authorize(user, &preimage),
    };
    let height = submit_gateway(base, user, &message)?;
    println!("registered {name} at height {height}");
    Ok(())
}

/// Publish one lane of the account's airlock gateway route if it is not
/// already published — the one signed statement that makes this account's
/// enclave reachable over the overlay under that label. Idempotent: a route
/// already present is left untouched.
pub(crate) fn ensure_airlock_route(
    base: &str,
    user: &commonware_cryptography::ed25519::PrivateKey,
    publisher: &Publisher,
    account_id: u64,
    lane: AirlockLane,
) -> CredResult {
    let name = gateway::RouteName::named(lane.label());
    let existing = query_gateway(
        base,
        &gateway::GatewayQuery::Get {
            account_id,
            name: name.clone(),
        },
    )?;
    let already_published =
        matches!(existing, gateway::GatewayReply::Route(ref boxed) if boxed.is_some());
    if already_published {
        return Ok(());
    }
    // The airlock upstream is a streaming loopback: unbounded response
    // (`max_response_bytes = 0` — a model reply is SSE, a signed bundle is a
    // sealed chunk stream), GET+POST, and it forwards the scoped session
    // bearer (`allow_authorization`). The request cap is the lane's own.
    let statement = gateway::RouteStatement {
        chain_id: publisher.chain_id.clone(),
        account_id,
        name,
        publisher_node: publisher.node.clone(),
        revision: 1,
        route: Some(gateway::RouteDefinition {
            target: gateway::RouteTarget::LoopbackHttp,
            policy: gateway::RoutePolicy {
                audience: gateway::RouteAudience::Network,
                methods: vec![gateway::RouteMethod::Get, gateway::RouteMethod::Post],
                max_request_bytes: Some(lane.max_request_bytes()),
                max_response_bytes: 0,
                allow_authorization: true,
                allow_upgrade: false,
            },
        }),
    };
    let preimage = gateway::route_signing_preimage(&statement)?;
    let message = gateway::GatewayMsg::SetRoute {
        statement,
        authorization: gateway::MemberAuthorization {
            signer: user.public_key().as_ref().to_vec(),
            signature: user
                .sign(gateway::GATEWAY_ROUTE_NS, &preimage)
                .as_ref()
                .to_vec(),
        },
    };
    let height = submit_gateway(base, user, &message)?;
    println!("published {} route at height {height}", lane.label());
    Ok(())
}

/// Refuse early (nonzero exit) when the vendor binary is absent, naming the
/// install command — running the pty against a missing binary would only fail
/// opaquely.
fn preflight_binary(provider: ProviderArg) -> CredResult {
    let ok = std::process::Command::new(provider.binary())
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        return Err(format!(
            "install {} first ({})",
            provider.binary(),
            provider.install_hint()
        )
        .into());
    }
    Ok(())
}

/// Run the vendor login on a local pty, pointing its config home at `dir`. The
/// wrap is a TRANSPARENT terminal: this terminal goes raw, the vendor CLI's own
/// full-screen TUI is mirrored byte-for-byte, and keystrokes are forwarded
/// verbatim — so its interactive authorize-and-paste flow renders and behaves
/// exactly as if the vendor login were run directly.
fn run_vendor_login(provider: ProviderArg, dir: &Path) -> CredResult {
    let mut command = tokio::process::Command::new(provider.binary());
    command.args(provider.login_args());
    command.env(provider.config_env(), dir);
    let artifact = dir.join(provider.artifact());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("login runtime: {e}"))?;
    let result = rt.block_on(pump_login(command, &artifact));
    // `shutdown_background`, not the implicit drop: the stdin forwarder reads
    // `tokio::io::stdin()`, which parks a BLOCKING thread on `read(0)`. Aborting
    // the task can't interrupt that OS-level read, so once the login ends with no
    // further keypress the thread stays stuck — and a normal runtime drop WAITS
    // for its blocking pool, hanging `cred add` after the pty session is over.
    // Detach instead: the stuck reader dies with the process.
    rt.shutdown_background();
    result?;
    Ok(())
}

/// The Ctrl-C byte. Raw mode disables ISIG, so a Ctrl-C is delivered to us as
/// this byte rather than a SIGINT — we treat it as "cancel the login".
const CTRL_C: u8 = 0x03;

/// Pump the pty: child output → stdout (mirrored), this process's stdin → child.
/// Returns when the login artifact is written, the child exits, or the pty
/// closes; Ctrl-C cancels. `artifact` is the credential file the vendor login
/// writes — its appearance IS success.
async fn pump_login(command: tokio::process::Command, artifact: &Path) -> CredResult {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::AsyncReadExt as _;

    // Raw mode for the whole login: `claude setup-token` / `codex login` drive a
    // full-screen TUI, so this terminal must pass keystrokes through verbatim —
    // otherwise the pasted auth code is line-buffered, locally echoed, and its
    // Enter arrives as `\n` instead of the `\r` the child's prompt submits on, so
    // the code never registers. The guard restores the tty on return AND on panic.
    let _raw = crate::tty::RawGuard::enter();

    let session = Arc::new(provider_host::InteractiveSession::spawn_local(command)?);

    // Forward this terminal's stdin to the child until the session ends. A Ctrl-C
    // (byte, not SIGINT — ISIG is off in raw mode) CANCELS: without this it would
    // just flow to the child's TUI, which may ignore it, leaving no way out. It
    // closes the session (killing the child) so the pump unwinds, and flags the
    // cancel so `cred add` errors instead of registering an incomplete login.
    let cancelled = Arc::new(AtomicBool::new(false));
    let writer = session.clone();
    let cancel = cancelled.clone();
    let input = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 1024];
        loop {
            let n = match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if buf[..n].contains(&CTRL_C) {
                cancel.store(true, Ordering::SeqCst);
                writer.close().await;
                break;
            }
            if writer.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
    });

    // Mirror the vendor login's own full-screen TUI verbatim — it already
    // presents the authorize URL and prompts for the code interactively. We add
    // nothing to the stream (injecting a line mid-redraw would corrupt its
    // layout); raw mode above lets its UI render exactly as if run directly.
    let mirror = async {
        let mut buf = [0u8; 4096];
        loop {
            let n = session
                .read(&mut buf)
                .await
                .map_err(|e| format!("read login output: {e}"))?;
            if n == 0 {
                return CredResult::Ok(());
            }
            mirror_stdout(&buf[..n])?;
        }
    };

    // The vendor login has written the credential once this file is present and
    // its size has stopped growing — that IS success, and it is the signal we
    // trust: `claude setup-token` (a single process, no forked helper) prints
    // "created successfully" and then does NOT exit — it sits waiting on the tty
    // — so neither pty EOF nor child-exit ever fires and `cred add` would hang
    // forever before registering. Watching the artifact ends the login the moment
    // the token lands; `close` below kills the still-running child.
    let watch = async {
        let mut last = 0u64;
        loop {
            let size = std::fs::metadata(artifact).map(|m| m.len()).unwrap_or(0);
            if size > 0 && size == last {
                return;
            }
            last = size;
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    };

    // End on whichever comes first: the credential written (`watch`), the child
    // exiting (some vendors do), or the pty closing (`mirror`). `close` then
    // terminates the process group, reaping any child still holding the tty.
    tokio::select! {
        result = mirror => result?,
        () = session.wait_child_exit() => {}
        () = watch => {}
    }
    input.abort();
    session.close().await;
    if cancelled.load(Ordering::SeqCst) {
        return Err("login cancelled (ctrl-c)".into());
    }
    Ok(())
}

/// Write raw child bytes straight through to this terminal.
fn mirror_stdout(bytes: &[u8]) -> CredResult {
    use std::io::Write as _;
    let mut out = std::io::stdout();
    out.write_all(bytes).map_err(|e| format!("stdout: {e}"))?;
    out.flush().map_err(|e| format!("stdout flush: {e}"))?;
    Ok(())
}

// ============================================================================
// pure helpers (unit-tested)
// ============================================================================

/// The default credential name `<display>-<provider>-<n>`, where `n` is one past
/// the highest existing counter for that display+provider prefix (1 when none).
fn derive_default_name(display: &str, kind_token: &str, existing: &[&str]) -> String {
    let prefix = format!("{display}-{kind_token}-");
    let highest = existing
        .iter()
        .filter_map(|name| name.strip_prefix(&prefix)?.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    format!("{prefix}{}", highest + 1)
}

// ============================================================================
// node round-trips
// ============================================================================

/// Build the owner authorization over `preimage`, signed under the gateway
/// credential namespace — the exact primitive the gateway `SetRoute` family uses.
fn authorize(
    user: &commonware_cryptography::ed25519::PrivateKey,
    preimage: &[u8],
) -> gateway::MemberAuthorization {
    gateway::MemberAuthorization {
        signer: user.public_key().as_ref().to_vec(),
        signature: user
            .sign(gateway::GATEWAY_CREDENTIAL_NS, preimage)
            .as_ref()
            .to_vec(),
    }
}

/// Submit one gateway op as a frame `user` signed over `/v1/submit/frame` (the
/// user key is the origin the gateway resolves to the owner account) and
/// return the committed height.
fn submit_gateway(
    base: &str,
    user: &commonware_cryptography::ed25519::PrivateKey,
    message: &gateway::GatewayMsg,
) -> Result<u64, Box<dyn std::error::Error>> {
    let frame = crate::userkey_cli::user_frame(user, "gateway", gateway::encode_msg(message));
    crate::node_http::submit_frame(base, &frame)
}

/// Read every registered credential record from committed gateway state.
/// the registered credential names — what a verb that could not find one says
/// instead of leaving the reader to guess.
pub(crate) fn list_credential_names(base: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    Ok(query_credentials(base)?
        .into_iter()
        .map(|record| record.name)
        .collect())
}

fn query_credentials(
    base: &str,
) -> Result<Vec<gateway::CredentialRecord>, Box<dyn std::error::Error>> {
    let reply = query_gateway(base, &gateway::GatewayQuery::Credentials {})?;
    match reply {
        gateway::GatewayReply::Credentials(records) => Ok(records),
        other => Err(format!("unexpected gateway reply: {other:?}").into()),
    }
}

pub(crate) fn query_gateway(
    base: &str,
    query: &gateway::GatewayQuery,
) -> Result<gateway::GatewayReply, Box<dyn std::error::Error>> {
    let value = query_node(base, "gateway", serde_json::to_value(query)?)?;
    Ok(serde_json::from_value(value)?)
}

/// The account number the local user key belongs to (owner of any record it
/// signs), resolved through `OfKey` — the one resolver.
fn query_owner_account(base: &str, member_key: &[u8]) -> Result<u64, Box<dyn std::error::Error>> {
    Ok(query_owner_account_view(base, member_key)?.number)
}

pub(crate) fn query_owner_account_view(
    base: &str,
    member_key: &[u8],
) -> Result<identity::AccountView, Box<dyn std::error::Error>> {
    crate::account_cli::own_account(base, member_key)
}

/// One `/v1/query` round-trip: `{target, query}` in, the module reply JSON out.
pub(crate) fn query_node(
    base: &str,
    target: &str,
    query: serde_json::Value,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    crate::node_http::query(base, target, query)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The login pump must return the moment the credential artifact lands, even
    /// when the vendor login process NEVER exits (the `claude setup-token` shape:
    /// it writes the token, prints success, then sits on the tty). A never-exiting
    /// `sleep` stands in; the artifact is written mid-flight from another task.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pump_login_returns_when_the_artifact_lands_even_if_the_child_never_exits() {
        let dir = std::env::temp_dir().join(format!("ducktape-cred-pump-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join("cred.json");
        let _ = std::fs::remove_file(&artifact);

        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("300");

        let art = artifact.clone();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            std::fs::write(&art, b"{\"token\":\"x\"}").unwrap();
        });

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            pump_login(cmd, &artifact),
        )
        .await;

        writer.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            matches!(outcome, Ok(Ok(()))),
            "pump_login should return Ok once the artifact lands; got {outcome:?}"
        );
    }

    #[derive(clap::Parser)]
    struct TestCli {
        #[command(flatten)]
        cred: CredArgs,
    }

    fn parse(line: &str) -> Result<CredCmd, clap::Error> {
        use clap::Parser as _;
        TestCli::try_parse_from(line.split(' ')).map(|cli| cli.cred.cmd)
    }

    /// `cred add` names its kind as a subcommand: the two vendor logins keep
    /// their `[NAME]`, and `apple-codesign` takes exactly the four identity
    /// inputs — every one required, the password as a FILE, never a value.
    #[test]
    fn cred_add_parses_each_kind_and_apple_codesign_needs_all_four_inputs() {
        assert!(matches!(
            parse("t add claude").unwrap(),
            CredCmd::Add {
                what: AddCmd::Claude { name: None }
            }
        ));
        assert!(matches!(
            parse("t add codex alice-codex-9").unwrap(),
            CredCmd::Add { what: AddCmd::Codex { name: Some(name) } } if name == "alice-codex-9"
        ));
        let CredCmd::Add {
            what: AddCmd::AppleCodesign { name, identity },
        } = parse(
            "t add apple-codesign release-sign --p12 id.p12 --p12-password-file pw.txt \
             --api-key key.json --team-id ABCDE12345",
        )
        .unwrap()
        else {
            panic!("the apple-codesign arm");
        };
        assert_eq!(name.as_deref(), Some("release-sign"));
        assert_eq!(identity.p12, Path::new("id.p12"));
        assert_eq!(identity.p12_password_file, Path::new("pw.txt"));
        assert_eq!(identity.api_key, Path::new("key.json"));
        assert_eq!(identity.team_id, "ABCDE12345");

        let missing_key = parse(
            "t add apple-codesign --p12 id.p12 --p12-password-file pw.txt --team-id ABCDE12345",
        )
        .unwrap_err();
        assert_eq!(
            missing_key.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        let password_on_argv = parse(
            "t add apple-codesign --p12 id.p12 --p12-password hunter2 --api-key k --team-id T",
        )
        .unwrap_err();
        assert_eq!(
            password_on_argv.kind(),
            clap::error::ErrorKind::UnknownArgument
        );
    }

    /// A signing identity the gateway would refuse is refused HERE, by the
    /// gateway's own token, before a store dir or an on-chain record exists.
    #[test]
    fn apple_codesign_enrolment_refuses_by_the_gateways_token_before_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let p12 = tmp.path().join("id.p12");
        let pw = tmp.path().join("pw");
        let key = tmp.path().join("key.json");
        std::fs::write(&p12, b"not a pkcs12").unwrap();
        std::fs::write(&pw, "hunter2\n").unwrap();
        std::fs::write(&key, r#"{"key_id":"k","issuer_id":"i","private_key":"-----BEGIN PRIVATE KEY-----\nx\n-----END PRIVATE KEY-----"}"#).unwrap();
        let err = read_and_admit_apple_codesign(&p12, &pw, &key, "ABCDE12345")
            .err()
            .map(|e| e.to_string())
            .expect("refused");
        assert_eq!(err, "apple-codesign identity refused: p12_unparseable");
        std::fs::write(&key, "{}").unwrap();
        let err = read_and_admit_apple_codesign(&p12, &pw, &key, "ABCDE12345")
            .err()
            .map(|e| e.to_string())
            .expect("refused");
        assert_eq!(err, "apple-codesign identity refused: api_key_malformed");
    }

    /// The store files `cred add apple-codesign` writes are exactly what the
    /// lender's loader reads back into the wire payload — and that payload
    /// seals and unseals to the same bytes the attested upload would carry.
    #[test]
    fn apple_codesign_store_files_round_trip_through_the_loader_and_the_seal() {
        let material = AppleCodesignMaterial {
            p12: vec![0x30, 0x82, 0x01, 0x02],
            p12_password: "hunter2".into(),
            api_key_json: r#"{"key_id":"k","issuer_id":"i","private_key":"pem"}"#.into(),
            team_id: "ABCDE12345".into(),
        };
        let tmp = tempfile::tempdir().unwrap();
        let root = airlock_service::cred_store_root(tmp.path());
        let dir = root.join("release-sign");
        airlock_service::create_private_dir(&dir).unwrap();
        write_apple_codesign_files(&dir, &material).unwrap();
        // a retry replaces, never fails on the leftover
        write_apple_codesign_files(&dir, &material).unwrap();
        std::fs::write(dir.join("kind"), "apple-codesign\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let files = airlock_service::apple_codesign_files();
            for file in [files.p12, files.p12_password, files.api_key, files.team_id] {
                let mode = std::fs::metadata(dir.join(file))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o600, "{file} must be owner-only");
            }
        }
        let seeds = airlock_service::load_seeds(&root).unwrap();
        let (name, kind, loaded) = &seeds[0];
        assert_eq!(name, "release-sign");
        assert_eq!(*kind, airlock::wire::CredentialKind::AppleCodesign);
        let expected = serde_json::to_vec(&material.payload()).unwrap();
        assert_eq!(serde_json::to_vec(loaded).unwrap(), expected);

        let keypair = airlock::seal::SealKeypair::generate();
        let sealed = airlock::seal::seal(&keypair.public_bytes(), &expected);
        assert_eq!(airlock::seal::unseal(&keypair, &sealed).unwrap(), expected);
    }

    /// The whole enrolment path on a throwaway Developer-ID-shaped identity:
    /// read, admit, and the payload the gateway would open. Needs the airlock
    /// testkit (the `verify` feature), which mints the identity in-process.
    #[cfg(feature = "verify")]
    #[test]
    fn a_fixture_identity_is_admitted_and_its_payload_carries_the_p12() {
        use airlock::codesign::fixture::{self, Marker};
        let tmp = tempfile::tempdir().unwrap();
        let p12 = tmp.path().join("id.p12");
        let pw = tmp.path().join("pw");
        let key = tmp.path().join("key.json");
        let p12_bytes = fixture::p12(fixture::TEAM_ID, Marker::DeveloperIdApplication);
        std::fs::write(&p12, &p12_bytes).unwrap();
        std::fs::write(&pw, format!("{}\n", fixture::P12_PASSWORD)).unwrap();
        std::fs::write(&key, fixture::api_key_json()).unwrap();
        let material = read_and_admit_apple_codesign(&p12, &pw, &key, fixture::TEAM_ID).unwrap();
        assert_eq!(material.p12, p12_bytes);
        assert_eq!(
            material.p12_password,
            fixture::P12_PASSWORD,
            "the file's newline is not the password's"
        );
        let err = read_and_admit_apple_codesign(&p12, &pw, &key, "OTHER00000")
            .err()
            .map(|e| e.to_string())
            .expect("refused");
        assert_eq!(err, "apple-codesign identity refused: team_id_mismatch");
    }

    #[test]
    fn default_name_is_display_provider_counter() {
        let existing = ["alice-claude-1", "alice-claude-2", "alice-codex-1"];
        assert_eq!(
            derive_default_name("alice", "claude", &existing),
            "alice-claude-3"
        );
        assert_eq!(
            derive_default_name("alice", "codex", &existing),
            "alice-codex-2"
        );
        assert_eq!(derive_default_name("jess", "claude", &[]), "jess-claude-1");
    }

    #[test]
    fn remove_deletes_only_the_named_local_credential() {
        let tmp = tempfile::tempdir().unwrap();
        let root = airlock_service::cred_store_root(tmp.path());
        let target = root.join("alice-codex-1");
        let sibling = root.join("alice-claude-1");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(target.join("auth.json"), "secret").unwrap();
        std::fs::write(sibling.join(".credentials.json"), "other").unwrap();
        std::fs::write(root.join("seal.key"), "stable").unwrap();

        let removed = remove_local_credential(tmp.path(), "alice-codex-1").unwrap();

        assert!(removed);
        assert!(!target.exists());
        assert!(sibling.exists());
        assert!(root.join("seal.key").exists());

        // a rerun (or a workspace `cred add` never touched) has nothing to
        // delete: still success, but reported as absent so the CLI can say so.
        let removed_again = remove_local_credential(tmp.path(), "alice-codex-1").unwrap();
        assert!(!removed_again);
    }

    /// Claude must log in with `auth login` (writes `.credentials.json`, which
    /// the artifact watch keys on and the airlock reads), NEVER `setup-token`
    /// (prints an inference-only token, writes no file → `cred add` hangs).
    #[test]
    fn claude_login_writes_the_credentials_artifact_not_setup_token() {
        assert_eq!(ProviderArg::Claude.login_args(), &["auth", "login"]);
        assert_eq!(ProviderArg::Claude.artifact(), ".credentials.json");
        assert_eq!(ProviderArg::Codex.login_args(), &["login", "--device-auth"]);
    }

    /// An explicit `--key` that EXISTS resolves to itself; one that is ABSENT
    /// is a loud error, not a silent fresh mint — the regression that let a
    /// stray path clobber a real, bound user key.
    #[test]
    fn key_path_uses_explicit_key_but_refuses_a_missing_one() {
        let dir = std::env::temp_dir().join(format!("ducktape-keypath-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let present = dir.join("user.key");
        std::fs::write(&present, b"deadbeef").unwrap();

        let ctx = VerbCtx {
            addr: NodeAddr::default(),
            key: Some(present.clone()),
        };
        assert_eq!(ctx.key_path().unwrap(), present);

        let missing = dir.join("nope.key");
        let ctx = VerbCtx {
            addr: NodeAddr::default(),
            key: Some(missing),
        };
        let err = ctx.key_path().unwrap_err().to_string();
        assert!(
            err.contains("no user key at"),
            "expected absent-key error, got {err:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
