//! `ducktape release` — compose, sign and verify the desktop app's release
//! manifest with a ducktape wallet key.
//!
//! The manifest (`app_update::Manifest`, one JSON file per channel) names
//! each platform's archive by sha256 and size and seals itself with
//! `release.sha256_id` (the sha256 of its canonical bytes — only this tool
//! computes that, never a shell). It is signed by the `release` wallet under
//! `app_update::RELEASE_NS`, and the signature is published beside it
//! (`stable.json.sig`) under `/shared/releases` on the network's duckfs by
//! `ducktape fs put`; `ops/release/publish.sh` runs the three steps.
//!
//! `/shared/**` is OPEN-WRITE: any member of the network can overwrite
//! `stable.json`, its `.sig`, or an archive. Nothing about the path is
//! trusted. What the app trusts is (1) this signature under the release
//! public key it pinned at install, and (2) the manifest's `sequence`
//! exceeding the one it last verified. A replaced file is a bad signature; a
//! replayed older manifest is `sequence_not_newer`; a replaced archive fails
//! the manifest's sha256. Withholding is the only thing an open directory
//! lets a stranger do.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use app_update::{
    Artifact, Manifest, PublicKey, Release, SCHEMA, Sha, Signature, SuccessorKey, layout,
};

use crate::userkey_cli;

type CommandResult = Result<(), Box<dyn std::error::Error>>;

#[derive(Debug, clap::Subcommand)]
pub(crate) enum ReleaseCmd {
    /// compose a sealed manifest from built archives; prints one
    /// `<local archive>\t<duckfs path>` line per artifact
    Manifest(ManifestArgs),
    /// sign a manifest with the release wallet — stdin: the wallet password.
    /// writes `<manifest>.sig`, prints the release public key (hex)
    Sign(SignArgs),
    /// verify a manifest against its `.sig` under a release public key
    Verify(VerifyArgs),
}

#[derive(Debug, clap::Args)]
pub(crate) struct ManifestArgs {
    /// where to write the manifest (`stable.json`)
    #[arg(long, value_name = "PATH")]
    pub out: PathBuf,
    /// the monotonic downgrade guard: strictly above the last published
    #[arg(long)]
    pub sequence: u64,
    /// banner text, e.g. `2026.09.2+9d71b254a`
    #[arg(long)]
    pub display: String,
    /// the app<->node contract number this release expects (default: this
    /// binary's `noded::NODE_CONTRACT`)
    #[arg(long, value_name = "N")]
    pub node_contract: Option<u32>,
    /// release notes link (banner text only)
    #[arg(long, default_value = "")]
    pub notes_url: String,
    /// the channel the manifest names (default: `stable`)
    #[arg(long, default_value = layout::CHANNEL)]
    pub channel: String,
    /// a built archive, as `<os>-<arch>=<path>` (repeatable), e.g.
    /// `macos-aarch64=target/Ducktape.tar.zst`
    #[arg(long = "archive", value_name = "OS-ARCH=PATH", required = true)]
    pub archives: Vec<String>,
    /// rotate: the successor release key (hex) that signs from
    /// `--successor-from` on
    #[arg(long, value_name = "HEX", requires = "successor_from")]
    pub successor_key: Option<PublicKey>,
    /// the sequence the successor key takes over at
    #[arg(long, value_name = "N", requires = "successor_key")]
    pub successor_from: Option<u64>,
}

#[derive(Debug, clap::Args)]
pub(crate) struct SignArgs {
    /// the manifest JSON file (`stable.json`)
    pub manifest: PathBuf,
    /// the release wallet's key file (default: `$DUCKTAPE_USER_KEY`)
    #[arg(long, value_name = "PATH")]
    pub key: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
pub(crate) struct VerifyArgs {
    /// the manifest JSON file
    pub manifest: PathBuf,
    /// the signature file (default: `<manifest>.sig`)
    #[arg(long, value_name = "PATH")]
    pub sig: Option<PathBuf>,
    /// the release public key, 64 hex characters (what `sign` printed)
    #[arg(long, value_name = "HEX")]
    pub pubkey: PublicKey,
}

pub(crate) fn run(cmd: ReleaseCmd) -> CommandResult {
    let mut stdin = std::io::BufReader::new(std::io::stdin());
    match cmd {
        ReleaseCmd::Manifest(args) => manifest(args),
        ReleaseCmd::Sign(args) => sign(args, &mut stdin),
        ReleaseCmd::Verify(args) => verify(args),
    }
}

/// `<manifest>.sig`, beside the manifest.
fn sig_path(manifest: &Path) -> PathBuf {
    let mut name = manifest.as_os_str().to_owned();
    name.push(".sig");
    PathBuf::from(name)
}

/// The manifest bytes exactly as they will be published — the signature is
/// over the FILE, so the file is checked to be a well-formed, sealed,
/// current-schema manifest before a key is ever opened.
fn read_manifest(path: &Path) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .map_err(|e| format!("{} is not a release manifest: {e}", path.display()))?;
    let schema_is_current = manifest.schema == SCHEMA;
    if !schema_is_current {
        return Err(format!(
            "{} carries schema {}, expected {SCHEMA}",
            path.display(),
            manifest.schema
        )
        .into());
    }
    if !manifest.sha256_id_is_consistent() {
        return Err(format!(
            "{}: release.sha256_id is not the sha256 of the canonical bytes (expected {})",
            path.display(),
            manifest.computed_sha256_id()
        )
        .into());
    }
    Ok(bytes)
}

/// `<os>-<arch>=<path>` → the platform key and the archive's path.
fn parse_archive_arg(arg: &str) -> Result<(String, PathBuf), String> {
    let (platform, path) = arg
        .split_once('=')
        .ok_or_else(|| format!("--archive {arg}: expected <os>-<arch>=<path>"))?;
    let (os, arch) = platform
        .split_once('-')
        .ok_or_else(|| format!("--archive {arg}: platform must be <os>-<arch>"))?;
    let well_formed = !os.is_empty() && !arch.is_empty() && !path.is_empty();
    if !well_formed {
        return Err(format!("--archive {arg}: expected <os>-<arch>=<path>"));
    }
    Ok((platform.to_string(), PathBuf::from(path)))
}

fn manifest(args: ManifestArgs) -> CommandResult {
    let mut artifacts = BTreeMap::new();
    let mut lines = Vec::new();
    for arg in &args.archives {
        let (platform, path) = parse_archive_arg(arg)?;
        let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let sha256 = Sha::digest(&bytes);
        let duplicate = artifacts
            .insert(
                platform.clone(),
                Artifact {
                    sha256,
                    size: bytes.len() as u64,
                },
            )
            .is_some();
        if duplicate {
            return Err(format!("--archive names {platform} twice").into());
        }
        lines.push(format!(
            "{}\t{}",
            path.display(),
            layout::archive_path(&sha256, &platform)
        ));
    }
    let successor_key = match (args.successor_key, args.successor_from) {
        (Some(pubkey), Some(from_sequence)) => Some(SuccessorKey {
            pubkey,
            from_sequence,
        }),
        (None, None) => None,
        // clap's `requires` pairs them; a half is unrepresentable here.
        (Some(_), None) | (None, Some(_)) => unreachable!("clap requires both successor flags"),
    };
    let manifest = Manifest {
        schema: SCHEMA,
        channel: args.channel,
        sequence: args.sequence,
        published_at: published_at_now(),
        release: Release {
            sha256_id: Sha::ZERO,
            display: args.display,
            node_contract: args.node_contract.unwrap_or(noded::NODE_CONTRACT),
            notes_url: args.notes_url,
        },
        artifacts,
        successor_key,
    }
    .sealed();
    let json = serde_json::to_string_pretty(&manifest)?;
    std::fs::write(&args.out, format!("{json}\n"))
        .map_err(|e| format!("write {}: {e}", args.out.display()))?;
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

/// The wall clock as RFC 3339 UTC seconds. Banner text only: nothing orders
/// releases by it (`sequence` does).
fn published_at_now() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let (days, remainder) = (seconds / 86_400, seconds % 86_400);
    let (hour, minute, second) = (remainder / 3600, remainder % 3600 / 60, remainder % 60);
    // civil-from-days (Howard Hinnant), for the proleptic Gregorian calendar.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn sign(args: SignArgs, stdin: &mut impl std::io::BufRead) -> CommandResult {
    let bytes = read_manifest(&args.manifest)?;
    let key_path = match (args.key, keystore::wallet::env_user_key()) {
        (Some(explicit), _) => explicit,
        (None, Some(env)) => env,
        (None, None) => {
            return Err("no release key — pass --key <path> (`ducktape wallet new release --workspace <dir>` mints one at <dir>/keys/release.key) or set DUCKTAPE_USER_KEY".into());
        }
    };
    let signer = userkey_cli::load_user_signer(&key_path, stdin)?;
    let signature = Signature::sign(&signer, &bytes);
    let out = sig_path(&args.manifest);
    std::fs::write(&out, signature.encoded())
        .map_err(|e| format!("write {}: {e}", out.display()))?;
    println!("{}", PublicKey::of(&signer));
    Ok(())
}

fn verify(args: VerifyArgs) -> CommandResult {
    let bytes = read_manifest(&args.manifest)?;
    let sig_path = args.sig.unwrap_or_else(|| sig_path(&args.manifest));
    let sig_text = std::fs::read_to_string(&sig_path)
        .map_err(|e| format!("read {}: {e}", sig_path.display()))?;
    let signature: Signature = sig_text
        .parse()
        .map_err(|e| format!("{}: {e}", sig_path.display()))?;
    let verifies = signature.verifies(&args.pubkey, &bytes);
    if !verifies {
        return Err(format!(
            "{} is not signed by {} (bad_signature)",
            args.manifest.display(),
            args.pubkey
        )
        .into());
    }
    println!("ok");
    Ok(())
}
