//! `ducktape release` — compose, sign and verify a release manifest with a
//! ducktape wallet key, and tell a network which node release to run.
//!
//! TWO ARTIFACT KINDS ride the same plane and the same key
//! (`--kind app|node`, [`app_update::Kind`]): the desktop app's
//! `stable.json` + `Ducktape-<sha7>-<os>-<arch>.tar.zst`, and the node's
//! `node.json` + `ducktape-<sha7>-<os>-<arch>.tar.zst`. The channel is inside
//! the signed body, so a signature can never be replayed from one onto the
//! other, and each has its own monotonic `sequence`.
//!
//! WHAT a release is, the release key says. WHEN a network runs a node
//! release is a network decision: `release schedule` drives a governance
//! proposal carrying an `app_update::Designation` (artifact sha + activation
//! height) and `release status` reads back what a running node's committed
//! state designates — which is the whole interface `ducktape-node-launcher`
//! has to the chain.
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
//!
//! `release sign-bundle` is the macOS signing step when a release is signed
//! by the airlock gateway rather than a local Developer ID
//! (`DUCKTAPE_SIGN_VIA=airlock` in `make release-app`): it reaches the
//! enclave the way a provider run does, through the node's browser gateway
//! onto the overlay, and comes back with the signed, notarized, stapled
//! bundle. See [`sign_bundle`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use airlock::client::Gateway;
use airlock::wire::WorkRef;
use airlock::{bodyseal, sign};
use app_update::{
    Artifact, Designation, Kind, Manifest, PublicKey, Release, SCHEMA, Sha, Signature, SuccessorKey,
};

use crate::cli_args::NodeAddr;
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
    /// propose, vote and execute the governance decision that runs a node
    /// release from a height (every member passes the same --sha and --at)
    Schedule(ScheduleArgs),
    /// what a RUNNING node's network designates, and where that node is —
    /// the node launcher's whole view of the chain
    Status(StatusArgs),
    /// sign, notarize and staple an UNSIGNED `Ducktape.app` through the
    /// airlock gateway holding an `apple-codesign` credential; writes the
    /// signed `.tar.zst` the enclave returned
    SignBundle(SignBundleArgs),
}

/// Which artifact a manifest publishes, on argv. A clap mirror of
/// [`app_update::Kind`], which stays free of a CLI dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ArtifactKind {
    /// the desktop app (`stable.json`, `Ducktape-…tar.zst`)
    App,
    /// the `ducktape` binary the node and its service daemons run
    /// (`node.json`, `ducktape-…tar.zst`)
    Node,
}

impl From<ArtifactKind> for Kind {
    fn from(kind: ArtifactKind) -> Kind {
        match kind {
            ArtifactKind::App => Kind::App,
            ArtifactKind::Node => Kind::Node,
        }
    }
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
    /// which artifact this manifest publishes — it picks the channel, the
    /// manifest's duckfs name and the archive naming
    #[arg(long, value_enum, default_value_t = ArtifactKind::App)]
    pub kind: ArtifactKind,
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
    /// the manifest JSON file (`stable.json`, `node.json`)
    pub manifest: PathBuf,
    /// the release wallet's key file (default: `$DUCKTAPE_USER_KEY`)
    #[arg(long, value_name = "PATH")]
    pub key: Option<PathBuf>,
    /// the artifact kind this manifest must be — refuses to sign a document
    /// whose channel says otherwise
    #[arg(long, value_enum, default_value_t = ArtifactKind::App)]
    pub kind: ArtifactKind,
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
    /// the artifact kind the manifest must name — the channel is inside the
    /// signed body, so this is the replay check, not a formality
    #[arg(long, value_enum, default_value_t = ArtifactKind::App)]
    pub kind: ArtifactKind,
}

/// `release schedule --sha <hex> --at <height>`: the governance half.
#[derive(Debug, clap::Args)]
pub(crate) struct ScheduleArgs {
    /// the node archive's sha256, as the signed node manifest names it for
    /// the platform this network runs
    #[arg(long, value_name = "HEX")]
    pub sha: Sha,
    /// the block from which every validator runs it. ABSOLUTE, and the same
    /// number for every member co-signing this proposal — it is inside the
    /// text they join each other by. Refused (`activation_lead_too_short`)
    /// when it leads the height this proposal is made at — after the
    /// preflight — by less than one launcher poll of blocks
    #[arg(
        long,
        value_name = "HEIGHT",
        required_unless_present = "lead",
        conflicts_with = "lead"
    )]
    pub at: Option<u64>,
    /// propose `--at` as BLOCKS past the height this proposal is made at,
    /// measured after the preflight so its time is not spent out of the lead.
    /// For the member who proposes: every other member co-signs with the
    /// `--at` this prints
    #[arg(long, value_name = "BLOCKS")]
    pub lead: Option<u64>,
    /// propose without asking the archive whether it can link the components
    /// this network runs. The preflight is the only thing standing between a
    /// WIT change and every validator stopping its node to learn the same
    /// refusal, so say it out loud: the module swap that matches this binary
    /// is already scheduled ahead of `--at`
    #[arg(long)]
    pub skip_preflight_i_know_the_wit_moved: bool,
    #[command(flatten)]
    pub selector: crate::cli_args::Selector,
}

impl ScheduleArgs {
    fn preflight(&self) -> Preflight {
        match self.skip_preflight_i_know_the_wit_moved {
            true => Preflight::Skipped,
            false => Preflight::Compose,
        }
    }
}

/// whether this designation is checked against the modules the network runs
/// before it becomes a ballot.
enum Preflight {
    /// the default: the archive is fetched and asked to link them.
    Compose,
    /// the operator says the WIT moved on purpose.
    Skipped,
}

/// `release status`: what the launcher asks a running node.
#[derive(Debug, clap::Args)]
pub(crate) struct StatusArgs {
    #[command(flatten)]
    pub selector: crate::cli_args::Selector,
    /// emit one machine-readable JSON object instead of prose
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct SignBundleArgs {
    /// the UNSIGNED bundle: a `Ducktape.app` directory (what `make app`
    /// stages under `DUCKTAPE_SIGN_VIA=airlock`) or its `.tar.zst`
    pub bundle: PathBuf,
    /// the `apple-codesign` credential's on-chain name — the session's `sub`
    #[arg(long, value_name = "NAME")]
    pub credential: String,
    /// where to write the signed `.tar.zst`, byte for byte as the enclave
    /// returned it
    #[arg(long, value_name = "PATH")]
    pub out: PathBuf,
    /// also unpack the signed `Ducktape.app/` into this directory, replacing
    /// the one already there (what `ops/release/archive.sh --from` packs)
    #[arg(long, value_name = "DIR")]
    pub unpack_into: Option<PathBuf>,
    #[command(flatten)]
    pub addr: NodeAddr,
}

pub(crate) fn run(cmd: ReleaseCmd) -> CommandResult {
    let mut stdin = std::io::BufReader::new(std::io::stdin());
    match cmd {
        ReleaseCmd::Manifest(args) => manifest(args),
        ReleaseCmd::Sign(args) => sign(args, &mut stdin),
        ReleaseCmd::Verify(args) => verify(args),
        ReleaseCmd::Schedule(args) => schedule(args),
        ReleaseCmd::Status(args) => status(args),
        ReleaseCmd::SignBundle(args) => sign_bundle(args),
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
/// current-schema manifest for the KIND asked before a key is ever opened.
/// The channel is what separates the two artifacts, and it is inside the
/// signed body: signing an app manifest as a node release, or verifying one
/// as the other, is refused here rather than discovered on a node.
fn read_manifest(path: &Path, kind: Kind) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
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
    let names_this_kind = manifest.channel == kind.channel();
    if !names_this_kind {
        return Err(format!(
            "{} is a {:?} manifest, not a {:?} one (channel_mismatch)",
            path.display(),
            manifest.channel,
            kind.channel()
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
    let kind = Kind::from(args.kind);
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
            kind.archive_path(&sha256, &platform)
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
        channel: kind.channel().to_string(),
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
    let bytes = read_manifest(&args.manifest, args.kind.into())?;
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
    let bytes = read_manifest(&args.manifest, args.kind.into())?;
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

// ============================================================================
// the node channel's governance half: schedule and status
// ============================================================================

/// The proposal-id space a node-release designation is minted in. It carries
/// NO proposer key on purpose: a settled proposal leaves the open roster, so
/// the only way a launcher can read a passed designation back is to walk ids
/// it can predict. `release status` walks `node-release:0`, `node-release:1`,
/// … to the first id no record exists under.
const DESIGNATION_PREFIX: &str = "node-release";

/// How far that walk goes. A network that has designated this many node
/// releases has outgrown a linear probe, not this plane.
const MAX_DESIGNATIONS: u64 = 1024;

fn designation_id(nth: u64) -> String {
    format!("{DESIGNATION_PREFIX}:{nth}")
}

/// `release schedule --sha <hex> --at <height>` — the network decides WHICH
/// node release it runs and FROM WHEN.
///
/// The carrier is governance's `Signal`, whose text is the designation. That
/// is deliberate: a `Signal` has no on-chain effect beyond its recorded
/// outcome, which is exactly what a binary cutover is — nothing in any root
/// changes, and every validator's launcher reads the recorded outcome and
/// flips its own files. No module id, no registry entry, no new action: a
/// designation can neither halt a block nor wedge a boundary.
///
/// `--at` is ABSOLUTE and the same number for every member: it is inside the
/// text they join each other's proposal by.
///
/// The lead is measured from the committed height AFTER the preflight: the
/// preflight takes as long as fetching and linking the archive takes, and a
/// lead counted from before it is spent while it runs.
fn schedule(args: ScheduleArgs) -> CommandResult {
    let cfg_path = args.selector.config_path()?;
    let resolved = crate::config::resolve(&cfg_path)?;
    let node = crate::cli::DrivenNode::of(&resolved, "release schedule")?;
    match args.preflight() {
        Preflight::Compose => preflight(node.http_base(), &cfg_path, &args.sha)?,
        Preflight::Skipped => {}
    }
    let proposed_at = committed_height(node.http_base())?;
    let at = match (args.at, args.lead) {
        (Some(at), _) => at,
        (None, Some(lead)) => proposed_at.saturating_add(lead),
        (None, None) => {
            return Err("release schedule needs --at <HEIGHT> or --lead <BLOCKS>".into());
        }
    };
    let block_time_ms = u64::try_from(resolved.cadence.block_time.as_millis()).unwrap_or(u64::MAX);
    if let Err(refusal) = check_lead(proposed_at, at, block_time_ms) {
        tracing::warn!(
            target: "ducktape::update",
            event = "release_schedule_refused",
            reason = "activation_lead_too_short",
            release = %args.sha,
            proposed_at,
            at,
            "the activation height is inside one launcher poll of the proposal; nothing was proposed"
        );
        return Err(refusal.into());
    }
    let designation = Designation {
        sha256: args.sha,
        activation_height: at,
    };
    let signer = crate::cli::gov_signer(node.rpc(), &cfg_path, &resolved)?;
    let pubkey_hex = crate::module_cli::signer_pubkey_hex(&signer);
    let wanted = governance::GovAction::Signal {
        text: designation.signal_text(),
    };
    let same_action = {
        let wanted = wanted.clone();
        move |action: &governance::GovAction| *action == wanted
    };
    let outcome = crate::cli::drive_proposal_ceremony(
        &node,
        &signer,
        &pubkey_hex,
        // EMPTY seed: this proposal must be findable by id from any node.
        "",
        "release schedule",
        DESIGNATION_PREFIX,
        wanted,
        &same_action,
    )?;
    match outcome {
        crate::cli::CeremonyOutcome::Passed => {
            println!(
                "designated {} from height {at}; track with: ducktape release status",
                args.sha
            );
            Ok(())
        }
        crate::cli::CeremonyOutcome::AwaitingBallots => {
            println!(
                "proposed {} from height {at}; every other member co-signs with --at {at}",
                args.sha
            );
            Ok(())
        }
    }
}

/// The committed height this node reports — where a designation's lead is
/// measured from.
fn committed_height(base: &str) -> Result<u64, String> {
    let status = crate::node_http::get_json(base, "/v1/status")
        .map_err(|error| format!("read this node's status: {error}"))?;
    status["height"]
        .as_u64()
        .ok_or_else(|| "this node's status carries no height".to_string())
}

/// Refuse an activation height that leads `proposed_at` by less than
/// [`Designation::min_lead`]: a designation no launcher polling at its default
/// can be counted on to stage before the height, so a validator would run the
/// release from whenever it happened to notice rather than from `at`.
fn check_lead(proposed_at: u64, at: u64, block_time_ms: u64) -> Result<(), String> {
    let min = Designation::min_lead(block_time_ms);
    let lead = at.saturating_sub(proposed_at);
    if lead >= min {
        return Ok(());
    }
    Err(format!(
        "activation_lead_too_short: activation height {at} leads height {proposed_at}, where \
         this proposal is made, by {lead} blocks — a launcher polls every {poll} ms, which is {min} blocks at \
         this network's {block_time_ms} ms beat, so nothing was proposed. Pass --at {earliest} \
         or later, or --lead <BLOCKS> to count from the proposal height.",
        poll = app_update::designation::LAUNCHER_POLL_MS,
        earliest = proposed_at.saturating_add(min),
    ))
}

/// Ask the archive being designated whether it can link the components this
/// network RUNS — before a ballot exists.
///
/// A node binary carries the host half of the module WIT world; the
/// components carry the other half, and only the code registry moves those.
/// Nothing about publishing or designating a binary whose world moved says
/// so: every launcher stages it, arms it at the activation height, STOPS its
/// node, and only then hears its qualify refuse — on a designation that stays
/// the network's until another one replaces it. The same answer costs a
/// second here. The archive is read off the network's own duckfs, which is
/// where every launcher will read it, so what is asked is the bytes that will
/// actually run and not a local file that claims to be them; the executable
/// it carries then reads the roster off this node's rpc and the components
/// out of its blob files. Nothing is locked and the node is never stopped.
fn preflight(base: &str, config: &Path, sha: &Sha) -> Result<(), Box<dyn std::error::Error>> {
    let scratch = tempfile::tempdir()?;
    let exe = node_exe(&archive(base, sha)?, scratch.path())?;
    let said = std::process::Command::new(&exe)
        .args(["node", "qualify", "--compose-only"])
        .arg("--config")
        .arg(config)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("run {}: {error}", exe.display()))?;
    if said.status.success() {
        return Ok(());
    }
    // the child's contract: the snake_case reason on stdout, the sentence on
    // stderr. Both are the operator's answer here — there is no launcher
    // between them and it.
    let refusal = String::from_utf8_lossy(&said.stdout).trim().to_string();
    let detail = String::from_utf8_lossy(&said.stderr).trim().to_string();
    tracing::warn!(
        target: "ducktape::update",
        event = "release_schedule_refused",
        reason = "preflight_compose_refused",
        release = %sha,
        refusal = %refusal,
        "the release being designated cannot link the modules this network runs; nothing was proposed"
    );
    Err(format!(
        "preflight_compose_refused: {sha} did not pass the compose preflight — nothing was \
         proposed.\n{detail}\nA release that moves the module WIT ships AFTER the module swap \
         that matches it (`ducktape module update <id> <component.wasm>`)."
    )
    .into())
}

/// the designated node archive, read off the network's duckfs at the path the
/// launchers read, and checked against the sha it is named by.
fn archive(base: &str, sha: &Sha) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use duckfs_client::api::NodeApi as _;
    let path = app_update::Kind::Node.archive_path(sha, &app_update::Platform::HOST.key());
    let node = duckfs_client::http::HttpNode::new(base.to_string());
    let mut bytes = Vec::new();
    loop {
        let (page, eof) = node
            .read(&path, None, bytes.len() as u64, duckfs_core::MAX_READ_BYTES)
            .map_err(|error| format!("read {path}: {error}"))?;
        let empty = page.is_empty();
        bytes.extend_from_slice(&page);
        if eof || empty {
            break;
        }
    }
    let landed = Sha::digest(&bytes);
    let is_the_designation = landed == *sha;
    if !is_the_designation {
        return Err(format!("{path} hashes to {landed}, not {sha}").into());
    }
    Ok(bytes)
}

/// the one executable a node archive carries, unpacked into `scratch`. Every
/// other entry is ignored rather than written: this is a scratch copy the
/// preflight runs once and throws away, so `ducktape` at the root is the only
/// thing it has any use for.
fn node_exe(archive: &[u8], scratch: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let decoder = zstd::Decoder::new(archive)?;
    let mut tar = tar::Archive::new(decoder);
    tar.set_preserve_permissions(false);
    tar.set_preserve_ownerships(false);
    let out = scratch.join("ducktape");
    for entry in tar.entries()? {
        let mut entry = entry?;
        let is_the_executable =
            entry.header().entry_type().is_file() && entry.path()?.as_os_str() == "ducktape";
        if !is_the_executable {
            continue;
        }
        let mut file = std::fs::File::create(&out)?;
        std::io::copy(&mut entry, &mut file)?;
        drop(file);
        #[cfg(unix)]
        std::fs::set_permissions(&out, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
        return Ok(out);
    }
    Err("the node archive carries no `ducktape` executable at its root".into())
}

/// `release status [--json]` — what a RUNNING node's network designates, and
/// where that node is. This is the whole interface `ducktape-node-launcher`
/// has to the chain: the http base it reads duckfs through, the mesh identity
/// whose absence kills a service daemon, the committed height an activation
/// is measured against, and the designation itself.
fn status(args: StatusArgs) -> CommandResult {
    let cfg_path = args.selector.config_path()?;
    // The KEYLESS read: a launcher asks this once a poll, and it has no
    // business opening the node's identity or rehashing the founding set to
    // find out where the node serves.
    let service = crate::config::resolve_service(&cfg_path)?;
    let http_listen = service
        .http_listen
        .as_deref()
        .ok_or("release status reads the node's app surface — set `http_listen` in node.toml")?;
    let base = crate::config::http_base_of(http_listen);
    let live = crate::node_http::get_json(&base, "/v1/status")
        .map_err(|error| format!("read this node's status: {error}"))?;
    let public_key = live["public_key"].as_str().unwrap_or_default().to_string();
    let height = live["height"].as_u64().unwrap_or_default();
    let root_hash = live["root_hash"].as_str().unwrap_or_default().to_string();
    // A node that is up but whose governance module cannot answer yet is
    // still a node the launcher must hear about: the identity seam is the
    // half that matters first, so a designation read that fails reports as
    // "none" rather than failing the whole verb.
    let designation = designated(&base).unwrap_or(None);
    if args.json {
        let reading = serde_json::json!({
            "base": base,
            "public_key": public_key,
            "height": height,
            "root_hash": root_hash,
            "designation": designation,
        });
        println!("{reading}");
        return Ok(());
    }
    println!("base\t{base}");
    println!(
        "node\t{}",
        match public_key.is_empty() {
            true => "(no mesh identity published yet)",
            false => &public_key,
        }
    );
    println!("height\t{height}");
    println!("root_hash\t{root_hash}");
    match designation {
        Some(designation) => println!(
            "designated\t{} from height {} ({})",
            designation.sha256,
            designation.activation_height,
            match designation.armed_at(height) {
                true => "armed",
                false => "pending",
            }
        ),
        None => {
            println!("designated\t(none — this network runs whatever each node was installed with)")
        }
    }
    Ok(())
}

/// The designation this network's committed governance carries: the LAST
/// passed `node-release:<n>` whose signal text decodes. The walk stops at the
/// first id with no record, which is exactly where the ceremony's own mint
/// stops.
fn designated(base: &str) -> Result<Option<Designation>, Box<dyn std::error::Error>> {
    let mut latest = None;
    for nth in 0..MAX_DESIGNATIONS {
        let Some(view) = read_proposal(base, &designation_id(nth))? else {
            break;
        };
        if let Some(designation) = passed_designation(&view) {
            latest = Some(designation);
        }
    }
    Ok(latest)
}

fn read_proposal(
    base: &str,
    proposal_id: &str,
) -> Result<Option<governance::ProposalView>, Box<dyn std::error::Error>> {
    let query = serde_json::to_value(governance::GovQuery::Proposal {
        proposal_id: proposal_id.to_string(),
    })?;
    let reply = crate::node_http::query(base, "governance", query)?;
    match serde_json::from_value::<governance::GovReply>(reply)? {
        governance::GovReply::Proposal(view) => Ok(view),
        other => Err(format!("unexpected governance reply: {other:?}").into()),
    }
}

/// A settled proposal's designation — `None` for everything that is not a
/// PASSED node-release signal, including one still being voted on.
fn passed_designation(view: &governance::ProposalView) -> Option<Designation> {
    let decided = view.status == governance::ProposalStatus::Passed;
    if !decided {
        return None;
    }
    let governance::GovAction::Signal { text } = &view.action else {
        return None;
    };
    Designation::from_signal_text(text)
}

// ============================================================================
// sign-bundle
// ============================================================================

/// The gateway route the enclave's SIGNING lane is served under:
/// `<AIRLOCK_SIGN_ROUTE>.<owner handle>.duck` — the same enclave a provider
/// run reaches under `airlock.<handle>.duck` (`bin/node/src/compute/cred.rs`),
/// under the label whose signed policy admits a bundle rather than a turn.
const AIRLOCK_SIGN_ROUTE: &str = crate::airlock::AIRLOCK_SIGN_ROUTE;
/// The enclave's signing route.
const SIGN_ROUTE: &str = "/sign/macos-bundle";
/// Total deadline for the one signing request. Apple's notary wait runs
/// minutes, not seconds, so the client this crate shares for the handshake
/// (20 s total) is the wrong shape here.
const SIGN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// The bundle's user-facing version keys, read out of `Info.plist` for the
/// shape check.
const VERSION_KEYS: [&str; 2] = ["CFBundleShortVersionString", "CFBundleVersion"];

/// Where the signing enclave is and what the overlay admits on the way there,
/// resolved from committed state the same way a provider run resolves a
/// credential: the record names the owner, the owner's handle names the
/// route, the route's policy caps the request.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SigningGateway {
    /// `airlock-sign.<owner-handle>.duck`
    authority: String,
    /// this node's browser-gateway base — the transport onto the overlay
    via: String,
    /// the on-chain seal key, pinned: the trust anchor for the session
    seal_pk: [u8; 32],
    /// the published route's `max_request_bytes`: what the overlay hop admits
    /// for ONE request, measured from the record rather than assumed. `None`
    /// is a route that declares no cap at all.
    max_request_bytes: Option<u64>,
}

/// What a bundle IS, independent of its signature: the identity and version
/// its `Info.plist` names, the executables it carries and the views it ships.
/// Signing adds `_CodeSignature/` and rewrites the Mach-Os; it changes none of
/// these, so the reply is checked to carry the same shape as the request.
/// Signature validity itself is the host's `codesign`/`spctl` call in
/// `ops/release/archive.sh`, never this side.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BundleShape {
    bundle_id: String,
    versions: BTreeMap<&'static str, Option<String>>,
    macos: BTreeSet<String>,
    views: BTreeSet<String>,
}

impl BundleShape {
    /// Unpack `archive` into a scratch dir (the same rules the enclave unpacks
    /// under) and read its shape. `what` names the side for the error.
    fn of_archive(archive: &[u8], what: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let scratch = tempfile::tempdir().map_err(|e| format!("scratch dir: {e}"))?;
        sign::unpack_bundle(archive, scratch.path())
            .map_err(|refusal| format!("{what} archive: {refusal}"))?;
        let bundle = scratch.path().join(sign::BUNDLE_NAME);
        sign::validate_layout(&bundle).map_err(|refusal| format!("{what} bundle: {refusal}"))?;
        Self::of_bundle(&bundle).map_err(|e| format!("{what} bundle: {e}").into())
    }

    fn of_bundle(bundle: &Path) -> Result<Self, String> {
        let plist = std::fs::read_to_string(bundle.join("Contents/Info.plist"))
            .map_err(|e| format!("read Info.plist: {e}"))?;
        let bundle_id = sign::plist_string(&plist, "CFBundleIdentifier")
            .ok_or("Info.plist names no CFBundleIdentifier")?;
        let versions = VERSION_KEYS
            .iter()
            .map(|key| (*key, sign::plist_string(&plist, key)))
            .collect();
        let macos = entry_names(&bundle.join("Contents/MacOS"))?;
        let views = entry_names(&bundle.join("Contents/Resources/views"))?;
        Ok(Self {
            bundle_id,
            versions,
            macos,
            views,
        })
    }
}

fn entry_names(dir: &Path) -> Result<BTreeSet<String>, String> {
    let listing = std::fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
    let mut names = BTreeSet::new();
    for entry in listing {
        let entry = entry.map_err(|e| format!("read {}: {e}", dir.display()))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| format!("{}: non-UTF-8 entry name", dir.display()))?;
        names.insert(name);
    }
    Ok(names)
}

/// The `.tar.zst` to send: a directory is packed the way the enclave packs its
/// reply (and `ops/release/archive.sh` packs a release); a file is sent as is.
fn load_unsigned_archive(bundle: &Path) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let meta = std::fs::metadata(bundle).map_err(|e| format!("read {}: {e}", bundle.display()))?;
    let archive = if meta.is_dir() {
        let is_the_bundle = bundle
            .file_name()
            .is_some_and(|name| name == sign::BUNDLE_NAME);
        if !is_the_bundle {
            return Err(format!(
                "{} is a directory but not {}",
                bundle.display(),
                sign::BUNDLE_NAME
            )
            .into());
        }
        sign::pack_bundle(bundle).map_err(|e| format!("pack {}: {e}", bundle.display()))?
    } else {
        std::fs::read(bundle).map_err(|e| format!("read {}: {e}", bundle.display()))?
    };
    let too_large = archive.len() > sign::MAX_BUNDLE_BYTES;
    if too_large {
        return Err(format!(
            "bundle_too_large: the archive is {} bytes, the signing route takes at most {}",
            archive.len(),
            sign::MAX_BUNDLE_BYTES
        )
        .into());
    }
    Ok(archive)
}

/// Resolve the credential NAME to its enclave, from committed state on the
/// node this verb dials — the record, the owner's handle, the owner's
/// `airlock` route, and this node's browser-gateway base.
fn resolve_signing_gateway(
    base: &str,
    credential: &str,
) -> Result<SigningGateway, Box<dyn std::error::Error>> {
    let record = match crate::cred_cli::query_gateway(
        base,
        &gateway::GatewayQuery::Credential {
            name: credential.to_string(),
        },
    )? {
        gateway::GatewayReply::Credential(record) => record,
        other => return Err(format!("unexpected gateway reply: {other:?}").into()),
    };
    let Some(record) = record else {
        let registered = crate::cred_cli::list_credential_names(base)?;
        return Err(format!(
            "unknown credential: {credential} (registered: {})",
            registered.join(", ")
        )
        .into());
    };
    // The same refusal the gateway makes (`credential_kind_mismatch`), made
    // here before a session is opened on a model credential.
    let is_signing_identity = record.kind == gateway::CredentialKind::AppleCodesign;
    if !is_signing_identity {
        return Err(format!(
            "credential_kind_mismatch: {credential} is a {:?} credential, not apple-codesign",
            record.kind
        )
        .into());
    }
    let handle = owner_handle(base, record.owner_account)?;
    let name = gateway::RouteName::named(AIRLOCK_SIGN_ROUTE);
    let route = match crate::cred_cli::query_gateway(
        base,
        &gateway::GatewayQuery::Get {
            account_id: record.owner_account,
            name,
        },
    )? {
        gateway::GatewayReply::Route(boxed) => *boxed,
        other => return Err(format!("unexpected gateway reply: {other:?}").into()),
    };
    let policy = route
        .and_then(|route| route.statement.route)
        .map(|definition| definition.policy)
        .ok_or_else(|| {
            format!("credential owner ({handle}.duck) publishes no {AIRLOCK_SIGN_ROUTE} route")
        })?;
    let via = crate::node_http::get_json(base, "/v1/gateway/browser")
        .map_err(|error| format!("read this node's browser gateway base: {error}"))?["base"]
        .as_str()
        .ok_or("this node serves no browser gateway, so it cannot route a .duck authority")?
        .to_string();
    Ok(SigningGateway {
        authority: format!("{AIRLOCK_SIGN_ROUTE}.{handle}.duck"),
        via,
        seal_pk: record.seal_pk,
        max_request_bytes: policy.max_request_bytes,
    })
}

/// The `.duck` handle registered to `account_id`. One page, like the
/// provider-side resolver: a network past `MAX_QUERY_LIMIT` handles needs
/// pagination here.
fn owner_handle(base: &str, account_id: u64) -> Result<String, Box<dyn std::error::Error>> {
    let registrations = match crate::cred_cli::query_gateway(
        base,
        &gateway::GatewayQuery::Registrations {
            from: 0,
            limit: gateway::MAX_QUERY_LIMIT,
        },
    )? {
        gateway::GatewayReply::Registrations(list) => list,
        other => return Err(format!("unexpected gateway reply: {other:?}").into()),
    };
    registrations
        .into_iter()
        .find(|registration| registration.account_id == account_id)
        .map(|registration| registration.handle)
        .ok_or_else(|| "credential owner has no registered duck handle".into())
}

/// The sealed request against what the overlay hop admits. The route's cap is
/// read off the published record, so the refusal names the number the
/// network actually enforces rather than one copied from a crate.
fn admitted_by_route(sealed_len: usize, target: &SigningGateway) -> Result<(), String> {
    let Some(cap) = target.max_request_bytes else {
        return Ok(());
    };
    let fits = sealed_len as u64 <= cap;
    if fits {
        return Ok(());
    }
    Err(format!(
        "bundle_exceeds_route_cap: the sealed archive is {sealed_len} bytes, the {} route admits {cap} per request",
        target.authority
    ))
}

/// One signing exchange: a sealed session on the credential, the sealed
/// archive up, the sealed reply stream down and opened. Returns the signed
/// `.tar.zst` exactly as the enclave produced it.
async fn request_signature(
    target: &SigningGateway,
    credential: &str,
    archive: &[u8],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let gw = Gateway::remote(target.authority.clone(), target.via.clone());
    // The session names the credential and draws for no committed work; the
    // enclave refuses plaintext on a signing credential, so it is sealed.
    let (token, keys) = gw
        .open_session_sealed(&target.seal_pk, credential, &WorkRef::Direct)
        .await
        .map_err(|e| format!("open a signing session on {credential}: {e:#}"))?;
    let aad = bodyseal::request_aad("POST", SIGN_ROUTE);
    let sealed = bodyseal::seal_request(&keys, &aad, archive);
    admitted_by_route(sealed.len(), target)?;
    let binding = bodyseal::request_binding(&sealed);
    tracing::info!(
        target: "ducktape::gateway",
        event = "release_sign_submitted",
        credential = %credential,
        authority = %target.authority,
        sha256_in = %sign::sha256_hex(archive),
        size = archive.len(),
        "release bundle sent for signing"
    );
    let http = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(SIGN_TIMEOUT)
        .build()
        .map_err(|e| format!("build http client: {e}"))?;
    let response = gw
        .route(http.post(gw.url(SIGN_ROUTE)))
        .bearer_auth(&token)
        .header(bodyseal::SEAL_HEADER, bodyseal::SEAL_V1)
        .body(sealed)
        .send()
        .await
        .map_err(|e| format!("signing request: {e}"))?;
    let status = response.status();
    let wire = response
        .bytes()
        .await
        .map_err(|e| format!("signing reply: {e}"))?;
    // Not `error_for_status`: the body IS the refusal token
    // (`bundle_shape_refused`, `notary_rejected`, …), or the proxy's reason.
    if !status.is_success() {
        let reason = String::from_utf8_lossy(&wire).trim().to_string();
        tracing::warn!(
            target: "ducktape::gateway",
            event = "release_sign_refused",
            credential = %credential,
            status = status.as_u16(),
            reason = %reason,
            "release signing refused"
        );
        return Err(format!("the gateway refused signing ({status}): {reason}").into());
    }
    let signed = match open_signed_reply(&keys, &binding, &wire) {
        Ok(signed) => signed,
        Err(error) => {
            tracing::warn!(
                target: "ducktape::gateway",
                event = "release_sign_refused",
                credential = %credential,
                status = status.as_u16(),
                reason = %error,
                "release signing refused"
            );
            return Err(error);
        }
    };
    tracing::info!(
        target: "ducktape::gateway",
        event = "release_sign_received",
        credential = %credential,
        sha256_out = %sign::sha256_hex(&signed),
        size = signed.len(),
        "signed release bundle received"
    );
    Ok(signed)
}

/// Open the sealed chunk stream (salt, head, data…, Final) into the archive.
fn open_signed_reply(
    keys: &airlock::handshake::SessionKeys,
    binding: &[u8],
    wire: &[u8],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut opener = bodyseal::StreamOpener::new(keys, binding);
    let items = opener
        .feed(wire)
        .map_err(|e| format!("unseal the signing reply: {e}"))?;
    if !opener.finished() {
        return Err("reply_truncated: the sealed reply ended before its Final marker".into());
    }
    let mut content_type = None;
    let mut archive = Vec::new();
    for item in items {
        match item {
            bodyseal::OpenedItem::Head(kind) => content_type = Some(kind),
            // keepalives open as empty data and add nothing
            bodyseal::OpenedItem::Data(bytes) => archive.extend(bytes),
            bodyseal::OpenedItem::Final => {}
            // The enclave commits its head before the pipeline runs, so a
            // pipeline refusal (`bundle_shape_refused`, `notary_rejected`,
            // …) arrives as the Final carrying the token, on a 200.
            bodyseal::OpenedItem::Refused(reason) => {
                return Err(format!("the gateway refused signing: {reason}").into());
            }
        }
    }
    let is_archive = content_type.as_deref() == Some("application/zstd");
    if !is_archive {
        return Err(format!(
            "reply_not_an_archive: the enclave replied {}",
            content_type.unwrap_or_default()
        )
        .into());
    }
    Ok(archive)
}

/// Write `bytes` to `out` through a sibling `.partial`, so a failed write
/// never leaves a truncated archive under the final name.
fn write_atomically(out: &Path, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let mut partial = out.as_os_str().to_owned();
    partial.push(".partial");
    let partial = PathBuf::from(partial);
    std::fs::write(&partial, bytes).map_err(|e| format!("write {}: {e}", partial.display()))?;
    std::fs::rename(&partial, out).map_err(|e| format!("rename to {}: {e}", out.display()))?;
    Ok(())
}

/// Unpack the signed `Ducktape.app/` into `dir`, replacing the bundle already
/// there (the unsigned one `make app` staged). Unpacked beside it first, so a
/// refused archive leaves the staged bundle untouched.
fn unpack_replacing(archive: &[u8], dir: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let stage =
        tempfile::tempdir_in(dir).map_err(|e| format!("stage under {}: {e}", dir.display()))?;
    sign::unpack_bundle(archive, stage.path())
        .map_err(|refusal| format!("signed archive: {refusal}"))?;
    let target = dir.join(sign::BUNDLE_NAME);
    if target.exists() {
        std::fs::remove_dir_all(&target)
            .map_err(|e| format!("replace {}: {e}", target.display()))?;
    }
    std::fs::rename(stage.path().join(sign::BUNDLE_NAME), &target)
        .map_err(|e| format!("move into {}: {e}", target.display()))?;
    Ok(target)
}

/// `release sign-bundle` — see the module doc. Sync like its siblings; the
/// airlock client is async, so one current-thread runtime is built here.
fn sign_bundle(args: SignBundleArgs) -> CommandResult {
    // a one-shot verb installs no sink; this one runs for minutes under
    // Apple's notary wait and its lifecycle events are what the operator
    // watches. `RUST_LOG` overrides.
    crate::fs_cli::install_log_sink("info");
    let archive = load_unsigned_archive(&args.bundle)?;
    let sent = BundleShape::of_archive(&archive, "unsigned")?;
    let base = args.addr.resolve()?;
    let target = resolve_signing_gateway(&base, &args.credential)?;
    tracing::info!(
        target: "ducktape::gateway",
        event = "release_sign_resolved",
        credential = %args.credential,
        authority = %target.authority,
        route_max_request_bytes = ?target.max_request_bytes,
        "signing enclave resolved"
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let signed = runtime.block_on(request_signature(&target, &args.credential, &archive))?;
    let received = BundleShape::of_archive(&signed, "signed")?;
    let same_bundle = received == sent;
    if !same_bundle {
        return Err(format!(
            "bundle_shape_changed: the enclave returned a different bundle\n  sent:     {sent:?}\n  received: {received:?}"
        )
        .into());
    }
    write_atomically(&args.out, &signed)?;
    let unpacked = match &args.unpack_into {
        Some(dir) => Some(unpack_replacing(&signed, dir)?),
        None => None,
    };
    println!("signed:  {}", args.out.display());
    println!("sha256:  {}", sign::sha256_hex(&signed));
    println!("size:    {}", signed.len());
    if let Some(bundle) = unpacked {
        println!("bundle:  {}", bundle.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;
    use std::io::Write as _;

    #[derive(Debug, clap::Parser)]
    struct Cli {
        #[command(subcommand)]
        cmd: ReleaseCmd,
    }

    /// A lead of one launcher poll is the floor; a block less is refused by
    /// name, with the three numbers an operator needs to pick again.
    #[test]
    fn a_lead_inside_one_launcher_poll_is_refused_with_its_heights() {
        // 100 ms beat: one 2000 ms poll is 20 blocks.
        assert_eq!(check_lead(1000, 1020, 100), Ok(()));
        let refusal = check_lead(1000, 1019, 100).expect_err("19 blocks is inside the poll");
        assert!(
            refusal.starts_with("activation_lead_too_short:"),
            "{refusal}"
        );
        for number in [
            "activation height 1019",
            "leads height 1000",
            "by 19 blocks",
            "20 blocks",
            "--at 1020",
        ] {
            assert!(
                refusal.contains(number),
                "{number:?} missing from: {refusal}"
            );
        }
        // a height already passed leads by nothing, not by a wrapped u64.
        let passed = check_lead(1000, 900, 100).expect_err("a passed height");
        assert!(passed.contains("by 0 blocks"), "{passed}");
    }

    /// `--at` and `--lead` name one height two ways: exactly one is taken.
    #[test]
    fn schedule_takes_at_or_lead_never_both() {
        let sha = "ab".repeat(32);
        let parse = |extra: &[&str]| {
            let mut argv = vec!["release", "schedule", "--sha", sha.as_str()];
            argv.extend_from_slice(extra);
            Cli::try_parse_from(argv)
        };
        let Ok(Cli {
            cmd: ReleaseCmd::Schedule(lead),
        }) = parse(&["--lead", "150"])
        else {
            panic!("--lead alone parses");
        };
        assert_eq!((lead.at, lead.lead), (None, Some(150)));
        assert!(parse(&["--at", "9"]).is_ok());
        assert!(parse(&[]).is_err(), "one of the two is required");
        assert!(parse(&["--at", "9", "--lead", "150"]).is_err());
    }

    #[test]
    fn sign_bundle_parses_its_flags() {
        let cli = Cli::try_parse_from([
            "release",
            "sign-bundle",
            "target/app-bundle/Ducktape.app",
            "--credential",
            "release-sign",
            "--out",
            "target/app-bundle/Ducktape-signed.tar.zst",
            "--unpack-into",
            "target/app-bundle",
            "-n",
            "chain-1",
        ])
        .unwrap();
        let ReleaseCmd::SignBundle(args) = cli.cmd else {
            panic!("parsed another verb");
        };
        assert_eq!(args.bundle, PathBuf::from("target/app-bundle/Ducktape.app"));
        assert_eq!(args.credential, "release-sign");
        assert_eq!(
            args.out,
            PathBuf::from("target/app-bundle/Ducktape-signed.tar.zst")
        );
        assert_eq!(args.unpack_into, Some(PathBuf::from("target/app-bundle")));
        assert_eq!(args.addr.network.as_deref(), Some("chain-1"));
        assert!(args.addr.node.is_none());
    }

    #[test]
    fn sign_bundle_requires_credential_and_out() {
        for missing in [
            vec![
                "release",
                "sign-bundle",
                "Ducktape.app",
                "--out",
                "x.tar.zst",
            ],
            vec![
                "release",
                "sign-bundle",
                "Ducktape.app",
                "--credential",
                "c",
            ],
            vec!["release", "sign-bundle", "--credential", "c", "--out", "x"],
        ] {
            assert!(
                Cli::try_parse_from(missing.clone()).is_err(),
                "{missing:?} parsed"
            );
        }
    }

    /// A bundle of the staged shape with no Mach-O in it: the shape check
    /// reads names and the plist, never the executables' bytes.
    fn stage(root: &Path, bundle_id: &str, version: &str, views: &[&str]) -> PathBuf {
        let bundle = root.join(sign::BUNDLE_NAME);
        let contents = bundle.join("Contents");
        std::fs::create_dir_all(contents.join("MacOS")).unwrap();
        std::fs::create_dir_all(contents.join("Resources/views")).unwrap();
        for executable in ["ducktape-launcher", "ducktape-app"] {
            std::fs::write(contents.join("MacOS").join(executable), b"\xcf\xfa\xed\xfe").unwrap();
        }
        std::os::unix::fs::symlink("../Resources/views", contents.join("MacOS/views")).unwrap();
        for view in views {
            std::fs::write(contents.join("Resources/views").join(view), b"\0asm").unwrap();
        }
        let mut plist = std::fs::File::create(contents.join("Info.plist")).unwrap();
        write!(
            plist,
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>\
             <key>CFBundleIdentifier</key><string>{bundle_id}</string>\
             <key>CFBundleShortVersionString</key><string>{version}</string>\
             <key>CFBundleVersion</key><string>{version}</string>\
             </dict></plist>"
        )
        .unwrap();
        bundle
    }

    fn shape_of(bundle_id: &str, version: &str, views: &[&str]) -> BundleShape {
        let root = tempfile::tempdir().unwrap();
        let bundle = stage(root.path(), bundle_id, version, views);
        let archive = sign::pack_bundle(&bundle).unwrap();
        BundleShape::of_archive(&archive, "test").unwrap()
    }

    #[test]
    fn the_shape_is_identity_version_executables_and_views() {
        let shape = shape_of(sign::BUNDLE_ID, "2026.9.2", &["chat.wasm", "forge.wasm"]);
        assert_eq!(shape.bundle_id, sign::BUNDLE_ID);
        assert_eq!(
            shape.versions,
            BTreeMap::from([
                ("CFBundleShortVersionString", Some("2026.9.2".to_string())),
                ("CFBundleVersion", Some("2026.9.2".to_string())),
            ])
        );
        assert_eq!(
            shape.macos,
            BTreeSet::from([
                "ducktape-launcher".into(),
                "ducktape-app".into(),
                "views".into()
            ])
        );
        assert_eq!(
            shape.views,
            BTreeSet::from(["chat.wasm".into(), "forge.wasm".into()])
        );
    }

    #[test]
    fn a_signed_reply_with_the_same_shape_matches_what_was_sent() {
        let sent = shape_of(sign::BUNDLE_ID, "2026.9.2", &["chat.wasm"]);
        // signing adds _CodeSignature and rewrites the executables: neither
        // is part of the shape, so a re-staged bundle reads the same.
        let root = tempfile::tempdir().unwrap();
        let bundle = stage(root.path(), sign::BUNDLE_ID, "2026.9.2", &["chat.wasm"]);
        std::fs::create_dir_all(bundle.join("Contents/_CodeSignature")).unwrap();
        std::fs::write(
            bundle.join("Contents/_CodeSignature/CodeResources"),
            b"<plist/>",
        )
        .unwrap();
        std::fs::write(bundle.join("Contents/MacOS/ducktape-app"), b"signed bytes").unwrap();
        let received =
            BundleShape::of_archive(&sign::pack_bundle(&bundle).unwrap(), "signed").unwrap();
        assert_eq!(received, sent);
    }

    #[test]
    fn a_reply_with_another_version_or_view_set_does_not_match() {
        let sent = shape_of(sign::BUNDLE_ID, "2026.9.2", &["chat.wasm"]);
        assert_ne!(shape_of(sign::BUNDLE_ID, "2026.9.3", &["chat.wasm"]), sent);
        assert_ne!(
            shape_of(sign::BUNDLE_ID, "2026.9.2", &["chat.wasm", "extra.wasm"]),
            sent
        );
    }

    #[test]
    fn a_reply_of_another_bundle_id_is_refused_by_the_layout_check() {
        let root = tempfile::tempdir().unwrap();
        let bundle = stage(root.path(), "dev.example.other", "1", &["a.wasm"]);
        let error = BundleShape::of_archive(&sign::pack_bundle(&bundle).unwrap(), "signed")
            .unwrap_err()
            .to_string();
        assert!(error.contains("bundle_shape_refused"), "{error}");
        assert!(error.starts_with("signed bundle"), "{error}");
    }

    #[test]
    fn a_reply_that_is_not_an_archive_is_refused_by_name() {
        let error = BundleShape::of_archive(b"not zstd", "signed")
            .unwrap_err()
            .to_string();
        assert!(error.contains("bundle_shape_refused"), "{error}");
    }

    #[test]
    fn a_directory_is_sent_only_when_it_is_the_bundle() {
        let root = tempfile::tempdir().unwrap();
        let error = load_unsigned_archive(root.path()).unwrap_err().to_string();
        assert!(
            error.contains("is a directory but not Ducktape.app"),
            "{error}"
        );
        let bundle = stage(root.path(), sign::BUNDLE_ID, "1", &["a.wasm"]);
        let packed = load_unsigned_archive(&bundle).unwrap();
        assert_eq!(packed, sign::pack_bundle(&bundle).unwrap());
        // the same bytes on disk are sent as they are
        let file = root.path().join("unsigned.tar.zst");
        std::fs::write(&file, &packed).unwrap();
        assert_eq!(load_unsigned_archive(&file).unwrap(), packed);
    }

    fn target(max_request_bytes: u64) -> SigningGateway {
        SigningGateway {
            authority: "airlock-sign.alice.duck".into(),
            via: "http://127.0.0.1:1".into(),
            seal_pk: [7; 32],
            max_request_bytes: Some(max_request_bytes),
        }
    }

    #[test]
    fn the_route_cap_is_measured_from_the_published_policy() {
        assert!(admitted_by_route(1024, &target(1024)).is_ok());
        let error = admitted_by_route(1025, &target(1024)).unwrap_err();
        assert!(error.starts_with("bundle_exceeds_route_cap"), "{error}");
        assert!(error.contains("1025 bytes"), "{error}");
        assert!(error.contains("admits 1024"), "{error}");
        assert!(error.contains("airlock-sign.alice.duck"), "{error}");
    }

    #[test]
    fn the_sealed_reply_round_trips_and_a_truncated_one_is_refused() {
        let keys = airlock::handshake::client_handshake(&[9; 32]).1;
        let binding = b"binding";
        let (mut sealer, salt) = bodyseal::StreamSealer::new(&keys, binding);
        let mut wire = salt;
        wire.extend(sealer.seal_head("application/zstd"));
        wire.extend(sealer.seal_chunk(b"hello "));
        wire.extend(sealer.seal_chunk(b"world"));
        let without_final = wire.clone();
        wire.extend(sealer.seal_final());
        assert_eq!(
            open_signed_reply(&keys, binding, &wire).unwrap(),
            b"hello world"
        );
        let error = open_signed_reply(&keys, binding, &without_final)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("reply_truncated"), "{error}");
    }

    #[test]
    fn a_refusal_after_the_head_is_refused_by_its_token_and_keepalives_add_nothing() {
        let keys = airlock::handshake::client_handshake(&[9; 32]).1;
        let (mut sealer, salt) = bodyseal::StreamSealer::new(&keys, b"b");
        let mut refused = salt;
        refused.extend(sealer.seal_head("application/zstd"));
        refused.extend(sealer.seal_keepalive());
        refused.extend(sealer.seal_keepalive());
        refused.extend(sealer.seal_refused("notary_rejected"));
        let error = open_signed_reply(&keys, b"b", &refused)
            .unwrap_err()
            .to_string();
        assert_eq!(error, "the gateway refused signing: notary_rejected");

        let (mut sealer, salt) = bodyseal::StreamSealer::new(&keys, b"c");
        let mut completed = salt;
        completed.extend(sealer.seal_head("application/zstd"));
        completed.extend(sealer.seal_keepalive());
        completed.extend(sealer.seal_chunk(b"archive"));
        completed.extend(sealer.seal_final());
        assert_eq!(
            open_signed_reply(&keys, b"c", &completed).unwrap(),
            b"archive"
        );
    }

    /// The signing lane's two caps are one number: what the enclave reads
    /// (`sign::MAX_BUNDLE_BYTES`) and what the `airlock-sign` route pins. The
    /// route's own policy is the only ceiling there is — the gateway module
    /// pins none, so a lane that buffers declares its own.
    #[test]
    fn the_signing_lane_caps_agree() {
        assert_eq!(
            sign::MAX_BUNDLE_BYTES as u64,
            crate::airlock::AIRLOCK_SIGN_REQUEST_BYTES
        );
    }

    #[test]
    fn a_reply_of_another_content_type_is_refused_by_name() {
        let keys = airlock::handshake::client_handshake(&[9; 32]).1;
        let (mut sealer, salt) = bodyseal::StreamSealer::new(&keys, b"b");
        let mut wire = salt;
        wire.extend(sealer.seal_head("text/plain"));
        wire.extend(sealer.seal_final());
        let error = open_signed_reply(&keys, b"b", &wire)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("reply_not_an_archive"), "{error}");
        assert!(error.contains("text/plain"), "{error}");
    }

    #[test]
    fn unpack_replacing_swaps_the_staged_bundle_and_keeps_it_on_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let staged = stage(dir.path(), sign::BUNDLE_ID, "1", &["a.wasm"]);
        std::fs::write(staged.join("Contents/MacOS/ducktape-app"), b"unsigned").unwrap();
        let error = unpack_replacing(b"not zstd", dir.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("bundle_shape_refused"), "{error}");
        assert_eq!(
            std::fs::read(staged.join("Contents/MacOS/ducktape-app")).unwrap(),
            b"unsigned"
        );

        let other = tempfile::tempdir().unwrap();
        let signed = stage(other.path(), sign::BUNDLE_ID, "1", &["a.wasm"]);
        std::fs::write(signed.join("Contents/MacOS/ducktape-app"), b"signed").unwrap();
        let archive = sign::pack_bundle(&signed).unwrap();
        let replaced = unpack_replacing(&archive, dir.path()).unwrap();
        assert_eq!(replaced, staged);
        assert_eq!(
            std::fs::read(staged.join("Contents/MacOS/ducktape-app")).unwrap(),
            b"signed"
        );
        assert!(std::fs::read_link(staged.join("Contents/MacOS/views")).is_ok());
        // nothing but the bundle is left in the directory
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from(sign::BUNDLE_NAME)]);
    }
}
