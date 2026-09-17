//! `ducktape agent install` — the agent CLIs a guest image lends to runs.
//!
//! A run executes inside a Linux microVM on EVERY host, so the CLI baked into
//! the image must be a Linux build for the GUEST's architecture. On Linux the
//! operator's own CLI happens to be exactly that, which is why reading the host
//! `PATH` worked for as long as nobody built an image on a Mac — where the
//! vendor's installer produces a Mach-O binary the guest cannot exec at all.
//!
//! So the binary is acquired deliberately, into the WORKSPACE's `executors/`
//! directory — per network, like every other file a node runs with: two
//! networks on one machine lend two independent sets, and a throwaway network
//! takes its CLIs with it when it goes — and the node derives the guest's copy
//! from whatever is there (`sandbox_host::executor_image`). This verb owns that
//! directory and the one approved way to fill it.
//!
//! THREE RULES IT EXISTS TO KEEP:
//!
//! 1. NOTHING IS FETCHED WITHOUT THE OPERATOR ASKING FOR IT. Bare
//!    `agent install` shows what is missing and what installing it would
//!    download — the checklist IS the approval, and unchecking everything is a
//!    complete answer. Nothing else in the tree fetches an executable.
//! 2. WHAT IS INSTALLED IS THE VENDOR'S LATEST, VERIFIED AGAINST THE VENDOR'S
//!    CHECKSUM FOR IT. Each executor's capability spec names its release
//!    channel (`[source]`); this verb asks the channel what is current, reads
//!    the checksum the vendor publishes for that release, and refuses bytes
//!    that do not match it. No version and no hash lives in this tree: a pin
//!    written down anywhere is stale the day after it is written.
//! 3. WHAT WAS INSTALLED IS WRITTEN DOWN. A receipt beside the directory
//!    (`<workspace>/executors.toml`) records, per provider, the release this
//!    verb installed and the sha256 of the bytes it wrote. That is what lets a
//!    newer vendor release show up as `BUMP` on the next `agent install`
//!    instead of a silent `ok` over old bytes, and what keeps the operator's
//!    own build from ever being offered for replacement unasked.
//!
//! WHY THE CHECKSUM MATTERS MORE THAN USUAL: this executable runs inside the
//! sandbox that holds the operator's provider credential. The sandbox is what
//! protects the HOST from the CLI; it is not what protects the CREDENTIAL from
//! it.
//!
//! The operator does not have to use this verb at all — dropping their own
//! Linux build into the directory is equally valid (it reports as `own`), and
//! the image builder's ELF check stays for exactly that case. This is a
//! convenience with a receipt, not a gate.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::IsTerminal as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use provider_host::{CapabilitySpec, ReleaseSource, SpecSet};
use sha2::{Digest as _, Sha256};

use crate::agent_cli::HarnessArg;

type InstallResult = Result<(), Box<dyn std::error::Error>>;

/// Every provider that has a guest CLI. Adding one here is what makes it
/// offerable; its capability spec's `[source]` is what makes it installable.
const ALL: [HarnessArg; 3] = [HarnessArg::Claude, HarnessArg::Codex, HarnessArg::Pi];

#[derive(Debug, clap::Args)]
pub(crate) struct InstallArgs {
    /// which CLIs to install (omitted = a checklist of what is missing or
    /// behind the vendor's latest release)
    #[arg(value_name = "NAME")]
    providers: Vec<HarnessArg>,
    /// install everything the checklist would offer without asking
    #[arg(short, long, conflicts_with = "providers")]
    yes: bool,
}

/// The guest's architecture — the HOST's, because there is no cross-hypervisor:
/// a Mac runs an aarch64 guest, an x86 box an x86_64 one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuestArch {
    Aarch64,
    X86_64,
}

impl GuestArch {
    fn host() -> Result<Self, String> {
        match std::env::consts::ARCH {
            "aarch64" => Ok(Self::Aarch64),
            "x86_64" => Ok(Self::X86_64),
            other => Err(format!("no agent CLIs are published for guest arch {other}")),
        }
    }

    /// the arch as a Rust target triple spells it — what a `{arch}` in a
    /// GitHub release's asset name stands for.
    fn rust_triple_arch(self) -> &'static str {
        match self {
            Self::Aarch64 => "aarch64",
            Self::X86_64 => "x86_64",
        }
    }

    /// Standalone JavaScript-runtime bundles use Node's architecture names.
    fn bundle_arch(self) -> &'static str {
        match self {
            Self::Aarch64 => "arm64",
            Self::X86_64 => "x64",
        }
    }

    /// the arch as Anthropic's release feed spells its Linux platforms.
    fn claude_platform(self) -> &'static str {
        match self {
            Self::Aarch64 => "linux-arm64",
            Self::X86_64 => "linux-x64",
        }
    }
}

// ---- the vendors -------------------------------------------------------------

/// What the artifact IS, which is what decides how it is unpacked.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Payload {
    /// the download is the executable itself, installed under this name.
    Binary(String),
    /// members to lift out of a gzipped tar, by their path inside the archive.
    TarGz(Vec<String>),
    /// A standalone executable with sibling runtime assets; keep its tree.
    Bundle { root: String, bin: String },
}

/// One vendor release, resolved: the channel's latest, and everything the
/// operator is being asked to approve — where the bytes come from and the
/// checksum the vendor published for them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Release {
    version: String,
    url: String,
    sha256: String,
    payload: Payload,
}

impl Release {
    /// the file names this release installs into the executors directory.
    fn files(&self) -> Vec<&str> {
        match &self.payload {
            Payload::Binary(name) => vec![name.as_str()],
            Payload::TarGz(members) => members.iter().map(|m| base_name(m)).collect(),
            Payload::Bundle { bin, .. } => vec![bin.as_str()],
        }
    }
}

/// the last path segment — a tar member's file name, a url's artifact name.
fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The vendors' side of the verb: one HTTP client, and the two release
/// channels the spec format knows how to read.
struct Vendors {
    http: reqwest::blocking::Client,
}

impl Vendors {
    fn new() -> Result<Self, String> {
        // no read timeout: the artifacts are hundreds of megabytes, and
        // reqwest's blocking default (30s) would abort every one of them.
        let http = reqwest::blocking::Client::builder()
            .timeout(None)
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        Ok(Self { http })
    }

    fn get(&self, url: &str) -> Result<reqwest::blocking::Response, String> {
        self.http
            .get(url)
            .send()
            .map_err(|e| format!("fetch {url}: {e}"))?
            .error_for_status()
            .map_err(|e| format!("fetch {url}: {e}"))
    }

    fn text(&self, url: &str) -> Result<String, String> {
        self.get(url)?
            .text()
            .map_err(|e| format!("read {url}: {e}"))
    }

    /// What is current on `spec`'s release channel for `arch`.
    fn latest(&self, spec: &CapabilitySpec, arch: GuestArch) -> Result<Release, String> {
        let Some(source) = &spec.source else {
            return Err(format!(
                "{}: its capability spec names no [source] to install it from",
                spec.tag
            ));
        };
        match source {
            ReleaseSource::ClaudeReleases { base } => self.latest_on_feed(base, &spec.bin, arch),
            ReleaseSource::GithubRelease {
                repo,
                asset,
                sums,
                members,
            } => self.latest_github_release(
                repo,
                &asset.replace("{arch}", arch.rust_triple_arch()),
                sums,
                Payload::TarGz(members.clone()),
            ),
            ReleaseSource::GithubBundle {
                repo,
                asset,
                sums,
                root,
            } => self.latest_github_release(
                repo,
                &asset.replace("{arch}", arch.bundle_arch()),
                sums,
                Payload::Bundle {
                    root: root.clone(),
                    bin: spec.bin.clone(),
                },
            ),
        }
    }

    /// Anthropic's feed: `latest` names the version, the version's manifest
    /// carries a checksum per platform, and the binary sits under the
    /// platform.
    fn latest_on_feed(&self, base: &str, bin: &str, arch: GuestArch) -> Result<Release, String> {
        let version = self.text(&format!("{base}/latest"))?.trim().to_string();
        if version.is_empty() {
            return Err(format!("{base}/latest named no version"));
        }
        let manifest = self.text(&format!("{base}/{version}/manifest.json"))?;
        let platform = arch.claude_platform();
        let sha256 = manifest_checksum(&manifest, platform)
            .map_err(|e| format!("{base}/{version}/manifest.json: {e}"))?;
        Ok(Release {
            url: format!("{base}/{version}/{platform}/{bin}"),
            version,
            sha256,
            payload: Payload::Binary(bin.to_string()),
        })
    }

    /// A GitHub release: `releases/latest` REDIRECTS to the latest tag's page,
    /// so the tag is read off the landing url — no API call, no rate limit —
    /// and the tag's download route serves the asset and its sums file.
    fn latest_github_release(
        &self,
        repo: &str,
        asset: &str,
        sums: &str,
        payload: Payload,
    ) -> Result<Release, String> {
        let landing = self.get(&format!("https://github.com/{repo}/releases/latest"))?;
        let tag = release_tag(landing.url().path())?;
        let downloads = format!("https://github.com/{repo}/releases/download/{tag}");
        let sums_text = self.text(&format!("{downloads}/{sums}"))?;
        let sha256 =
            sums_checksum(&sums_text, asset).map_err(|e| format!("{downloads}/{sums}: {e}"))?;
        Ok(Release {
            version: tag,
            url: format!("{downloads}/{asset}"),
            sha256,
            payload,
        })
    }
}

/// `platforms.<platform>.checksum` out of a release manifest.
fn manifest_checksum(manifest: &str, platform: &str) -> Result<String, String> {
    let manifest: serde_json::Value =
        serde_json::from_str(manifest).map_err(|e| format!("not a manifest: {e}"))?;
    let Some(checksum) = manifest["platforms"][platform]["checksum"].as_str() else {
        return Err(format!("no checksum published for platform {platform}"));
    };
    if !is_sha256_hex(checksum) {
        return Err(format!("the checksum for {platform} is not a sha256: {checksum:?}"));
    }
    Ok(checksum.to_string())
}

/// `asset`'s line out of a sha256 sums file: `<hex>  <name>` per line, the
/// `*` a binary-mode `sha256sum` prefixes to the name allowed.
fn sums_checksum(sums: &str, asset: &str) -> Result<String, String> {
    for line in sums.lines() {
        let mut fields = line.split_whitespace();
        let (Some(hex), Some(name)) = (fields.next(), fields.next()) else {
            continue;
        };
        if name.trim_start_matches('*') != asset {
            continue;
        }
        if !is_sha256_hex(hex) {
            return Err(format!("the checksum for {asset} is not a sha256: {hex:?}"));
        }
        return Ok(hex.to_string());
    }
    Err(format!("no checksum published for {asset}"))
}

/// The tag off the url `releases/latest` landed on: `…/releases/tag/<tag>`.
fn release_tag(landing_path: &str) -> Result<String, String> {
    let Some((_, tag)) = landing_path.rsplit_once("/releases/tag/") else {
        return Err(format!(
            "releases/latest did not land on a release tag: {landing_path}"
        ));
    };
    if tag.is_empty() || tag.contains('/') {
        return Err(format!("not a release tag: {tag:?}"));
    }
    Ok(tag.to_string())
}

fn is_sha256_hex(hex: &str) -> bool {
    hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit())
}

// ---- where the bytes go ------------------------------------------------------

/// The download cache: OUTSIDE every workspace, because a workspace is
/// disposable and a download is not — a `make dev` lap founds a fresh
/// workspace and would otherwise fetch the same quarter-gigabyte again — and
/// under the platform's cache dir rather than the ducktape home, which holds
/// workspaces and nothing else. Keyed by provider and version, so a newer
/// release never collides with the file an older one left; every hit is
/// re-verified against the vendor's checksum before it is used.
fn download_cache() -> Result<PathBuf, String> {
    let root = cache_root(
        std::env::var_os("XDG_CACHE_HOME"),
        std::env::var_os("HOME"),
        cfg!(target_os = "macos"),
    )?;
    Ok(root.join("downloads"))
}

/// The platform's cache dir for ducktape: `$XDG_CACHE_HOME/ducktape`, else
/// `~/.cache/ducktape` on Linux and `~/Library/Caches/ducktape` on macOS.
fn cache_root(
    xdg_cache_home: Option<OsString>,
    home: Option<OsString>,
    macos: bool,
) -> Result<PathBuf, String> {
    if let Some(xdg) = xdg_cache_home.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(xdg).join("ducktape"));
    }
    let Some(home) = home.filter(|value| !value.is_empty()) else {
        return Err("neither XDG_CACHE_HOME nor HOME is set; nowhere to cache downloads".into());
    };
    let platform_cache = if macos { "Library/Caches" } else { ".cache" };
    Ok(PathBuf::from(home).join(platform_cache).join("ducktape"))
}

/// Staging for an archive being unpacked, BESIDE the executors directory
/// rather than inside it: the guest's copy is an image built from that
/// directory's whole contents, so an extraction left there by an interrupted
/// install would be baked into it.
fn staging_dir(executors: &Path) -> PathBuf {
    executors.with_extension("staging")
}

// ---- the receipts -----------------------------------------------------------

/// What this verb installed, per provider — `<workspace>/executors.toml`,
/// beside the directory it describes for the same reason the staging dir is:
/// the directory's whole contents become the guest image.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Receipts {
    #[serde(flatten)]
    providers: BTreeMap<String, Receipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Receipt {
    /// the vendor release the download carried
    version: String,
    /// sha256 of the installed primary binary (`<executors>/<provider>`) —
    /// the bytes this verb wrote, so a file the operator has since replaced
    /// reads as their own build rather than as this receipt's
    sha256: String,
}

impl Receipts {
    fn path(executors: &Path) -> PathBuf {
        executors.with_extension("toml")
    }

    fn load(executors: &Path) -> Result<Self, String> {
        let path = Self::path(executors);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("read {}: {e}", path.display())),
        };
        toml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))
    }

    fn save(&self, executors: &Path) -> Result<(), String> {
        let path = Self::path(executors);
        let text = toml::to_string(self).map_err(|e| format!("encode {}: {e}", path.display()))?;
        std::fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))
    }
}

/// What the directory holds for one provider, measured against the vendor's
/// latest release and the receipts.
#[derive(Debug, PartialEq, Eq)]
enum Installed {
    /// no complete, executable set of the provider's files
    Missing,
    /// installed by this verb, at the vendor's latest
    Current { sha256: String },
    /// installed by this verb at an earlier release: the bump the checklist
    /// offers
    Behind { installed: String },
    /// executable bytes this verb did not write, or wrote and the operator has
    /// since replaced — their own Linux build. Never offered; replaced only by
    /// naming it (`agent install <name>`).
    Foreign { sha256: String },
}

impl Installed {
    /// what the checklist proposes: absent, or behind the vendor's latest.
    fn is_offered(&self) -> bool {
        matches!(self, Self::Missing | Self::Behind { .. })
    }
}

fn installed(provider: HarnessArg, latest: &Release, dir: &Path, receipts: &Receipts) -> Installed {
    // A partial install reports as missing rather than as present: codex
    // without its Code Mode companion is a codex that dies at startup inside
    // the guest.
    let every_file_executable = latest.files().iter().all(|f| is_executable(&dir.join(f)));
    if !every_file_executable {
        return Installed::Missing;
    }
    let sha256 = sha256_file(&dir.join(provider.token())).unwrap_or_else(|_| "?".into());
    let Some(receipt) = receipts.providers.get(provider.token()) else {
        return Installed::Foreign { sha256 };
    };
    let bytes_are_the_receipts = sha256 == receipt.sha256;
    if !bytes_are_the_receipts {
        return Installed::Foreign { sha256 };
    }
    if receipt.version == latest.version {
        return Installed::Current { sha256 };
    }
    Installed::Behind {
        installed: receipt.version.clone(),
    }
}

fn is_executable(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// One provider's row in the survey: what the vendor has, and what the
/// directory holds against it.
struct Surveyed {
    provider: HarnessArg,
    latest: Release,
    state: Installed,
}

pub(crate) fn run(args: InstallArgs, workspace: &Path) -> InstallResult {
    let arch = GuestArch::host()?;
    let dir = workspace_config::executor_dir(workspace);
    let receipts = Receipts::load(&dir)?;
    // the workspace's specs, the way the compute daemon loads them: an
    // operator override of a built-in spec changes where its build comes from
    // for this verb too.
    let capability_dir = workspace_config::capability_dir(workspace);
    let specs = SpecSet::load(capability_dir.is_dir().then_some(capability_dir.as_path()))?;
    let vendors = Vendors::new()?;
    let survey = ALL
        .into_iter()
        .map(|provider| {
            let Some(spec) = specs.get(provider.token()) else {
                return Err(format!("no capability spec for {}", provider.token()));
            };
            let latest = vendors.latest(spec, arch)?;
            let state = installed(provider, &latest, &dir, &receipts);
            Ok(Surveyed {
                provider,
                latest,
                state,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    print_status(&dir, arch, &survey);

    // Named providers are the operator's explicit ask — already the approval a
    // checklist would collect, so it is not collected twice.
    let chosen: Vec<&Surveyed> = if args.providers.is_empty() {
        let offered: Vec<&Surveyed> = survey.iter().filter(|row| row.state.is_offered()).collect();
        if offered.is_empty() {
            println!("\nnothing to install. `ducktape agent install <name>` reinstalls one.");
            return Ok(());
        }
        // `--yes` is the approval given up front, for the whole checklist.
        if args.yes {
            offered
        } else {
            choose(&offered)?
        }
    } else {
        survey
            .iter()
            .filter(|row| args.providers.contains(&row.provider))
            .collect()
    };
    if chosen.is_empty() {
        return Ok(());
    }
    install_all(&vendors, &chosen, &dir)
}

/// What is here, and — for what is not, or is behind the vendor — exactly what
/// installing it would download. This IS the proposal the checklist below then
/// asks approval for, so it names the vendor url and the checksum the vendor
/// published, in full.
fn print_status(dir: &Path, arch: GuestArch, survey: &[Surveyed]) {
    println!(
        "guest executors ({}, guest arch {})",
        dir.display(),
        arch.rust_triple_arch()
    );
    for row in survey {
        let name = row.provider.token();
        let latest = &row.latest;
        match &row.state {
            Installed::Missing => {
                println!("  MISS    {name:<8} {} (latest)", latest.version);
                println!("          {}", latest.url);
                println!("          sha256 {}", latest.sha256);
            }
            Installed::Behind { installed } => {
                println!("  BUMP    {name:<8} {installed} -> {} (latest)", latest.version);
                println!("          {}", latest.url);
                println!("          sha256 {}", latest.sha256);
            }
            // the hash of what is actually installed, so the image's contents
            // stay attributable to a download without unpacking the image.
            Installed::Current { sha256 } => println!(
                "  ok      {name:<8} {} (latest) sha256:{}…",
                latest.version,
                &sha256[..16.min(sha256.len())]
            ),
            Installed::Foreign { sha256 } => println!(
                "  own     {name:<8} sha256:{}… (not installed by this verb; \
                 `ducktape agent install {name}` replaces it)",
                &sha256[..16.min(sha256.len())]
            ),
        }
    }
}

/// The checklist — the approval step for the downloads [`print_status`] just
/// proposed. Off a terminal there is nobody to approve, so it prints the
/// commands and installs nothing.
fn choose<'a>(offered: &[&'a Surveyed]) -> Result<Vec<&'a Surveyed>, String> {
    if !std::io::stdin().is_terminal() {
        println!("\nnot a terminal — install what you want with:");
        for row in offered {
            println!("  ducktape agent install {}", row.provider.token());
        }
        return Ok(Vec::new());
    }

    println!();
    let items: Vec<String> = offered
        .iter()
        .map(|row| format!("{:<8} {}", row.provider.token(), row.latest.version))
        .collect();
    let picked = dialoguer::MultiSelect::new()
        .with_prompt(
            "agent CLIs to install into this workspace's guest image (space toggles, enter confirms)",
        )
        .items(&items)
        .interact_opt()
        .map_err(|e| format!("checklist: {e}"))?;
    let Some(picked) = picked else {
        return Ok(Vec::new());
    };
    Ok(picked.into_iter().map(|i| offered[i]).collect())
}

fn install_all(vendors: &Vendors, chosen: &[&Surveyed], dir: &Path) -> InstallResult {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let cache = download_cache()?;
    let artifacts = fetch_all(vendors, chosen, &cache)?;

    // Phase 2: unpack/install + receipt, sequential and unchanged, now
    // reading from the cache phase 1 just filled.
    let mut receipts = Receipts::load(dir)?;
    for (row, artifact) in chosen.iter().zip(artifacts) {
        let receipt = install_one(row.provider, &row.latest, dir, &artifact)?;
        receipts
            .providers
            .insert(row.provider.token().to_string(), receipt);
        // saved per install, so a second install failing does not lose the
        // first one's receipt.
        receipts.save(dir)?;
    }
    // Nothing else to do: the node derives the guest's copy from this directory
    // and rebuilds it whenever the directory has moved on, so the next run
    // picks this up on its own.
    Ok(())
}

/// Phase 1 of an install: every chosen artifact fetched concurrently, one
/// thread per row — no pool, since the row count is the checklist's own,
/// never unbounded. `fetch`/`download_to` keep their existing fail-closed
/// checksum semantics and `.part`-then-rename discipline unchanged per
/// thread, so a half-written download still never reads back as a cache hit.
/// Any fetch failing fails the whole install, same as the sequential version
/// did.
fn fetch_all(
    vendors: &Vendors,
    chosen: &[&Surveyed],
    cache: &Path,
) -> Result<Vec<PathBuf>, String> {
    let single_download = chosen.len() == 1;
    let fetched: Vec<Result<PathBuf, String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = chosen
            .iter()
            .map(|row| {
                // printed before spawning, on the main thread, so the header
                // lines themselves never interleave with each other.
                println!(
                    "\n{} {} <- {}",
                    row.provider.token(),
                    row.latest.version,
                    row.latest.url
                );
                let provider = row.provider;
                let latest = &row.latest;
                scope.spawn(move || fetch_one(vendors, provider, latest, cache, single_download))
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("fetch thread panicked"))
            .collect()
    });
    // fail-closed: any fetch failing refuses the whole install, same as the
    // sequential version did.
    fetched.into_iter().collect()
}

/// One row of phase 1: land `latest`'s artifact on its cache shelf, verified
/// against the vendor's checksum.
fn fetch_one(
    vendors: &Vendors,
    provider: HarnessArg,
    latest: &Release,
    cache: &Path,
    single_download: bool,
) -> Result<PathBuf, String> {
    let shelf = cache.join(provider.token()).join(&latest.version);
    std::fs::create_dir_all(&shelf).map_err(|e| format!("create {}: {e}", shelf.display()))?;
    let artifact = shelf.join(base_name(&latest.url));
    fetch(
        vendors,
        &latest.url,
        &artifact,
        &latest.sha256,
        single_download,
    )?;
    println!("  {} sha256 ok", provider.token());
    Ok(artifact)
}

/// Phase 2 of an install: `artifact` is already on the cache shelf and
/// verified: unpack it into `dir` and write the receipt for it.
fn install_one(
    provider: HarnessArg,
    latest: &Release,
    dir: &Path,
    artifact: &Path,
) -> Result<Receipt, Box<dyn std::error::Error>> {
    match &latest.payload {
        Payload::Binary(name) => install_file(artifact, &dir.join(name))?,
        Payload::TarGz(members) => unpack_into(artifact, members, dir)?,
        Payload::Bundle { root, bin } => unpack_bundle(artifact, root, bin, dir)?,
    }
    for file in latest.files() {
        println!("  installed {}", dir.join(file).display());
    }
    Ok(Receipt {
        version: latest.version.clone(),
        sha256: sha256_file(&dir.join(provider.token()))?,
    })
}

/// Download to `dest` unless it is already there, then verify — a cached file
/// is RE-verified rather than trusted, so a half-written or tampered cache
/// entry cannot survive into an image. A mismatch deletes the file and stops:
/// there is no "carry on without it" for an executable that runs beside a
/// credential.
///
/// `single_download` is only about the progress meter: it is the caller
/// saying no other fetch is in flight beside this one, so a redrawing `\r`
/// bar is safe to draw without a second thread's bar splicing into it.
fn fetch(
    vendors: &Vendors,
    url: &str,
    dest: &Path,
    want: &str,
    single_download: bool,
) -> Result<(), String> {
    if !dest.exists() {
        download_to(vendors, url, dest, single_download)?;
    }
    let got = sha256_file(dest)?;
    if got != want {
        let _ = std::fs::remove_file(dest);
        return Err(format!(
            "checksum mismatch for {url}\n  expected {want}\n  got      {got}\n\
             refusing to install an unverified executable"
        ));
    }
    Ok(())
}

/// Stream to `<dest>.part` and rename on success: an interrupted download must
/// never be picked up as a cache hit on the next run.
fn download_to(
    vendors: &Vendors,
    url: &str,
    dest: &Path,
    single_download: bool,
) -> Result<(), String> {
    let part = dest.with_extension("part");
    let mut response = vendors.get(url)?;
    let response_length = response.content_length();
    // The meter draws nothing off a terminal, so say the size once instead: it
    // is the part that tells a long download from a wedged one, and it is the
    // only part a log wants. One line per download is safe to interleave;
    // only the redrawing bar below needs `single_download`.
    if !std::io::stdout().is_terminal() {
        match response_length {
            Some(total) => println!("  downloading {} MiB", mib(total)),
            None => println!("  downloading"),
        }
    }
    let mut file =
        std::fs::File::create(&part).map_err(|e| format!("create {}: {e}", part.display()))?;
    let mut metered = Metered::new(&mut response, response_length, single_download);
    let copied = std::io::copy(&mut metered, &mut file);
    metered.finish();
    if let Err(e) = copied {
        let _ = std::fs::remove_file(&part);
        return Err(format!("download {url}: {e}"));
    }
    std::fs::rename(&part, dest).map_err(|e| format!("rename into {}: {e}", dest.display()))
}

const METER_CELLS: u64 = 24;

/// A `Read` that draws a one-line meter as the bytes go past, so a
/// quarter-gigabyte download does not look like a hung terminal.
///
/// Wrapping the response keeps `io::copy` doing the copying — the alternative
/// was hand-rolling the read/write loop to count in the middle of it.
struct Metered<R> {
    inner: R,
    total: Option<u64>,
    done: u64,
    /// When the line was last rewritten. A redraw per 8 KiB chunk would spend
    /// more time on the terminal than on the socket.
    drawn: std::time::Instant,
    /// Redirected output COLLECTS a line per redraw instead of rewriting one,
    /// and `make dev`'s log is what would collect them.
    live: bool,
}

impl<R: std::io::Read> Metered<R> {
    /// `allow_live` is false when a sibling fetch may be drawing its own bar
    /// at the same time — a redrawing `\r` line only reads correctly when it
    /// is the only one writing to the terminal.
    fn new(inner: R, total: Option<u64>, allow_live: bool) -> Self {
        Self {
            inner,
            total,
            done: 0,
            drawn: std::time::Instant::now(),
            live: allow_live && std::io::stdout().is_terminal(),
        }
    }

    /// A server that sends no `Content-Length` gets a byte count and no bar —
    /// a bar with a guessed denominator is a worse answer than none.
    fn line(&self) -> String {
        let Some(total) = self.total.filter(|total| *total > 0) else {
            return format!("  {} MiB", mib(self.done));
        };
        let filled = (self.done * METER_CELLS / total).min(METER_CELLS) as usize;
        format!(
            "  [{}{}] {} / {} MiB",
            "#".repeat(filled),
            "·".repeat(METER_CELLS as usize - filled),
            mib(self.done),
            mib(total)
        )
    }

    fn draw(&self) {
        if !self.live {
            return;
        }
        print!("\r{}", self.line());
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }

    /// Close the line so the next print does not land on top of it.
    fn finish(&self) {
        if !self.live {
            return;
        }
        self.draw();
        println!();
    }
}

impl<R: std::io::Read> std::io::Read for Metered<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.done += read as u64;
        if self.drawn.elapsed() >= std::time::Duration::from_millis(100) {
            self.drawn = std::time::Instant::now();
            self.draw();
        }
        Ok(read)
    }
}

fn mib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(hex::encode(hasher.finalize()))
}

/// `tar` rather than a crate: it is on every macOS and Linux host, both
/// flavours extract named members the same way, and a tar reader is a parser
/// this verb does not need to own.
fn unpack_into(archive: &Path, members: &[String], dir: &Path) -> Result<(), String> {
    let unpack = staging_dir(dir);
    let _ = std::fs::remove_dir_all(&unpack);
    std::fs::create_dir_all(&unpack).map_err(|e| format!("create {}: {e}", unpack.display()))?;
    let status = std::process::Command::new("tar")
        .arg("xzf")
        .arg(archive)
        .arg("-C")
        .arg(&unpack)
        .args(members)
        .status()
        .map_err(|e| format!("tar: {e}"))?;
    if !status.success() {
        return Err(format!("tar xzf {} failed", archive.display()));
    }
    for member in members {
        install_file(&unpack.join(member), &dir.join(base_name(member)))?;
    }
    let _ = std::fs::remove_dir_all(&unpack);
    Ok(())
}

/// Preserve the vendor's complete standalone distribution, including its runtime
/// assets. A relative symlink keeps PATH lookup working after the directory is
/// copied into the guest image; its target is the vendor's Linux ELF, not a host
/// Node launcher. Bun resolves package assets beside the real executable.
fn unpack_bundle(archive: &Path, root: &str, bin: &str, dir: &Path) -> Result<(), String> {
    let names_are_safe = safe_component(root) && safe_component(bin);
    if !names_are_safe {
        return Err("bundle root and executable must be plain path components".into());
    }
    let unpack = dir.with_extension(format!(
        "staging-{}-{:x}",
        std::process::id(),
        rand::random::<u64>()
    ));
    std::fs::create_dir(&unpack).map_err(|e| format!("create {}: {e}", unpack.display()))?;
    let result = install_bundle_tree(archive, root, bin, dir, &unpack);
    let recovery_needed = result.is_err() && unpack.join("previous").exists();
    if recovery_needed {
        return result.map_err(|e| {
            format!(
                "{e}; previous bundle retained at {}",
                unpack.join("previous").display()
            )
        });
    }
    let _ = std::fs::remove_dir_all(&unpack);
    result
}

fn install_bundle_tree(
    archive: &Path,
    root: &str,
    bin: &str,
    dir: &Path,
    unpack: &Path,
) -> Result<(), String> {
    let content = unpack.join("content");
    std::fs::create_dir(&content).map_err(|e| format!("create bundle staging: {e}"))?;
    extract_bundle(archive, root, &content)?;
    let tree = content.join(root);
    let executable = tree.join(bin);
    let mut magic = [0; 4];
    let read = std::fs::File::open(&executable)
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut magic));
    let is_linux_executable = is_executable(&executable) && read.is_ok() && magic == *b"\x7fELF";
    if !is_linux_executable {
        return Err(format!("bundle contains no Linux executable {root}/{bin}"));
    }
    let bundle_name = format!(".{bin}");
    let bundle = dir.join(&bundle_name);
    let previous = match std::fs::symlink_metadata(&bundle) {
        Ok(meta) => {
            if !meta.is_dir() {
                return Err(format!("{} is not a bundle directory", bundle.display()));
            }
            let previous = unpack.join("previous");
            std::fs::rename(&bundle, &previous)
                .map_err(|e| format!("move {}: {e}", bundle.display()))?;
            Some(previous)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("inspect {}: {e}", bundle.display())),
    };
    if let Err(e) = std::fs::rename(&tree, &bundle) {
        restore_bundle(previous.as_deref(), &bundle)?;
        return Err(format!("install {}: {e}", bundle.display()));
    }
    let link = unpack.join("entrypoint");
    let publish = std::os::unix::fs::symlink(Path::new(&bundle_name).join(bin), &link)
        .and_then(|()| std::fs::rename(&link, dir.join(bin)));
    if let Err(e) = publish {
        let _ = std::fs::remove_dir_all(&bundle);
        restore_bundle(previous.as_deref(), &bundle)?;
        return Err(format!("publish {bin}: {e}"));
    }
    Ok(())
}

fn restore_bundle(previous: Option<&Path>, bundle: &Path) -> Result<(), String> {
    let Some(previous) = previous else {
        return Ok(());
    };
    std::fs::rename(previous, bundle).map_err(|e| format!("restore {}: {e}", bundle.display()))
}

fn safe_component(name: &str) -> bool {
    let mut parts = Path::new(name).components();
    matches!(parts.next(), Some(std::path::Component::Normal(_)))
        && parts.next().is_none()
        && !name.contains(['/', '\\'])
}

/// Inspect structured tar entries before writing. No links, devices, absolute
/// paths or parent traversal are accepted, even from a checksum-verified vendor.
fn extract_bundle(archive: &Path, root: &str, unpack: &Path) -> Result<(), String> {
    let file = std::fs::File::open(archive).map_err(|e| format!("open archive: {e}"))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let entries = archive
        .entries()
        .map_err(|e| format!("read archive: {e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("read archive entry: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("read archive path: {e}"))?
            .into_owned();
        let normal_components = path
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)));
        let within_root = path.starts_with(root) && normal_components;
        if !within_root {
            return Err(format!(
                "bundle path escapes root {root}: {}",
                path.display()
            ));
        }
        let kind = entry.header().entry_type();
        let supported_entry = kind.is_file() || kind.is_dir();
        if !supported_entry {
            return Err(format!(
                "bundle links and special files are refused: {}",
                path.display()
            ));
        }
        let mode = entry
            .header()
            .mode()
            .map_err(|e| format!("read archive mode: {e}"))?;
        let extracted = entry
            .unpack_in(unpack)
            .map_err(|e| format!("extract {}: {e}", path.display()))?;
        if !extracted {
            return Err(format!("bundle path was not extracted: {}", path.display()));
        }
        let permissions = if kind.is_dir() {
            0o755
        } else {
            0o644 | (mode & 0o111)
        };
        std::fs::set_permissions(
            unpack.join(path),
            std::fs::Permissions::from_mode(permissions),
        )
        .map_err(|e| format!("set bundle permissions: {e}"))?;
    }
    Ok(())
}

fn install_file(src: &Path, dest: &Path) -> Result<(), String> {
    std::fs::copy(src, dest)
        .map_err(|e| format!("install {} -> {}: {e}", src.display(), dest.display()))?;
    std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod {}: {e}", dest.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle_archive(dir: &Path, entries: &[(&str, &[u8], tar::EntryType, u32)]) -> PathBuf {
        let path = dir.join("bundle.tar.gz");
        let gzip = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).unwrap(),
            flate2::Compression::default(),
        );
        let mut archive = tar::Builder::new(gzip);
        for (name, bytes, kind, mode) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(*mode);
            header.set_entry_type(*kind);
            if kind.is_symlink() || kind.is_hard_link() {
                header.set_link_name("../../outside").unwrap();
            }
            // Raw names deliberately exercise hostile paths the builder's
            // normal set_path API would reject before the extractor sees them.
            header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            header.set_cksum();
            archive.append(&header, *bytes).unwrap();
        }
        archive.into_inner().unwrap().finish().unwrap();
        path
    }

    #[test]
    fn bundle_preserves_assets_and_relative_entrypoint_after_relocation() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("executors");
        std::fs::create_dir(&dir).unwrap();
        let archive = bundle_archive(
            temp.path(),
            &[
                ("pi/pi", b"\x7fELFbinary", tar::EntryType::Regular, 0o755),
                (
                    "pi/package.json",
                    b"{\"version\":\"1.0\"}",
                    tar::EntryType::Regular,
                    0o644,
                ),
                ("pi/theme/dark.json", b"{}", tar::EntryType::Regular, 0o644),
                (
                    "pi/photon_rs_bg.wasm",
                    b"wasm",
                    tar::EntryType::Regular,
                    0o644,
                ),
                (
                    "pi/node_modules/native/addon.node",
                    b"addon",
                    tar::EntryType::Regular,
                    0o644,
                ),
            ],
        );
        unpack_bundle(&archive, "pi", "pi", &dir).unwrap();
        assert_eq!(
            std::fs::read_link(dir.join("pi")).unwrap(),
            Path::new(".pi/pi")
        );
        assert!(is_executable(&dir.join("pi")));
        assert!(!is_executable(&dir.join(".pi/package.json")));
        let relocated = temp.path().join("guest-executors");
        std::fs::rename(&dir, &relocated).unwrap();
        let executable = std::fs::canonicalize(relocated.join("pi")).unwrap();
        let package = executable.parent().unwrap();
        assert_eq!(
            std::fs::read(package.join("theme/dark.json")).unwrap(),
            b"{}"
        );
        assert_eq!(
            std::fs::read(package.join("node_modules/native/addon.node")).unwrap(),
            b"addon"
        );
        // Updating replaces the complete tree, rather than retaining old deps.
        let archive = bundle_archive(
            temp.path(),
            &[
                ("pi/pi", b"\x7fELFupdated", tar::EntryType::Regular, 0o755),
                ("pi/package.json", b"{}", tar::EntryType::Regular, 0o644),
            ],
        );
        unpack_bundle(&archive, "pi", "pi", &relocated).unwrap();
        assert!(!relocated.join(".pi/theme").exists());
        assert_eq!(
            std::fs::read(relocated.join("pi")).unwrap(),
            b"\x7fELFupdated"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires PI_TEST_ARCHIVE pointing to the official host-arch Linux Pi tarball"]
    fn official_pi_bundle_runs_without_a_host_node_runtime() {
        let archive =
            PathBuf::from(std::env::var_os("PI_TEST_ARCHIVE").expect("set PI_TEST_ARCHIVE"));
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("executors");
        std::fs::create_dir(&dir).unwrap();
        unpack_bundle(&archive, "pi", "pi", &dir).unwrap();
        let output = std::process::Command::new(dir.join("pi"))
            .arg("--version")
            .env_clear()
            .env("HOME", temp.path())
            .env("PATH", "/nonexistent")
            .env("PI_OFFLINE", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let version = String::from_utf8(output.stdout).unwrap();
        assert!(!version.trim().is_empty());
        assert_ne!(
            version.trim(),
            "0.0.0",
            "Pi did not find its sibling package.json"
        );
    }

    #[test]
    #[ignore = "requires e2fsprogs (mke2fs and debugfs)"]
    fn bundle_survives_the_guest_image_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("executors");
        std::fs::create_dir(&dir).unwrap();
        let archive = bundle_archive(
            temp.path(),
            &[
                ("pi/pi", b"\x7fELFbinary", tar::EntryType::Regular, 0o755),
                ("pi/package.json", b"{}", tar::EntryType::Regular, 0o644),
                (
                    "pi/theme/dark.json",
                    b"theme",
                    tar::EntryType::Regular,
                    0o644,
                ),
            ],
        );
        unpack_bundle(&archive, "pi", "pi", &dir).unwrap();
        let image = sandbox_host::executor_image::ensure(&dir).unwrap().unwrap();
        let guest = temp.path().join("guest");
        sandbox_host::workspace_image::read_back(&image, &guest).unwrap();
        assert_eq!(
            std::fs::read_link(guest.join("pi")).unwrap(),
            Path::new(".pi/pi")
        );
        assert!(is_executable(&guest.join("pi")));
        assert_eq!(
            std::fs::read(guest.join(".pi/theme/dark.json")).unwrap(),
            b"theme"
        );
    }

    #[test]
    fn bundle_refuses_traversal_links_and_special_files_before_publication() {
        use tar::EntryType;
        for (name, kind) in [
            ("../outside", EntryType::Regular),
            ("/absolute", EntryType::Regular),
            ("pi/../../outside", EntryType::Regular),
            ("other/file", EntryType::Regular),
            ("pi/link", EntryType::Symlink),
            ("pi/link", EntryType::Link),
            ("pi/device", EntryType::Char),
            ("pi/fifo", EntryType::Fifo),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let dir = temp.path().join("executors");
            std::fs::create_dir(&dir).unwrap();
            let archive = bundle_archive(temp.path(), &[(name, b"", kind, 0o755)]);
            assert!(
                unpack_bundle(&archive, "pi", "pi", &dir).is_err(),
                "accepted {name} {kind:?}"
            );
            assert!(!dir.join("pi").exists());
            assert!(!temp.path().join("outside").exists());
        }
    }

    #[test]
    fn bundle_refuses_unsafe_names_and_non_linux_entrypoints() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("executors");
        std::fs::create_dir(&dir).unwrap();
        let archive = bundle_archive(
            temp.path(),
            &[(
                "pi/pi",
                b"#!/usr/bin/env node",
                tar::EntryType::Regular,
                0o755,
            )],
        );
        for name in ["../pi", "/pi", ".", "..", "pi/sub", "pi\\sub", ""] {
            assert!(unpack_bundle(&archive, name, "pi", &dir).is_err());
            assert!(unpack_bundle(&archive, "pi", name, &dir).is_err());
        }
        assert!(
            unpack_bundle(&archive, "pi", "pi", &dir)
                .unwrap_err()
                .contains("Linux executable")
        );
        assert!(!dir.join("pi").exists());
    }

    #[test]
    fn bundle_refuses_an_existing_directory_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("executors");
        std::fs::create_dir(&dir).unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join(".pi")).unwrap();
        let archive = bundle_archive(
            temp.path(),
            &[("pi/pi", b"\x7fELFbinary", tar::EntryType::Regular, 0o755)],
        );
        assert!(unpack_bundle(&archive, "pi", "pi", &dir).is_err());
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
    }

    #[test]
    fn bundle_arch_names_and_pi_install_selection() {
        assert_eq!(GuestArch::Aarch64.bundle_arch(), "arm64");
        assert_eq!(GuestArch::X86_64.bundle_arch(), "x64");
        assert!(ALL.contains(&HarnessArg::Pi));
        use clap::{Args as _, FromArgMatches as _};
        let matches = InstallArgs::augment_args(clap::Command::new("install"))
            .try_get_matches_from(["install", "pi"])
            .unwrap();
        assert_eq!(
            InstallArgs::from_arg_matches(&matches).unwrap().providers,
            vec![HarnessArg::Pi]
        );
    }

    fn scratch(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dt-exec-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// a stand-in executable: any file with the exec bit reads as installed,
    /// and a small one keeps the hashing out of the test's clock.
    fn stand_in(dir: &Path, name: &str) {
        let src = dir.join(format!("{name}.src"));
        std::fs::write(&src, name).unwrap();
        install_file(&src, &dir.join(name)).unwrap();
    }

    fn release(version: &str, files: &[&str]) -> Release {
        Release {
            version: version.into(),
            url: format!("https://vendor.example/{version}/artifact"),
            sha256: "0".repeat(64),
            payload: Payload::TarGz(files.iter().map(|f| format!("bin/{f}")).collect()),
        }
    }

    /// The meter's only real arithmetic is the fill, and it has to hold at both
    /// ends and on a server that sends no `Content-Length` — the bar is drawn
    /// against a quarter-gigabyte download nobody can otherwise tell from a
    /// hung terminal.
    #[test]
    fn the_meter_fills_end_to_end_and_degrades_without_a_length() {
        let meter = |done: u64, total: Option<u64>| {
            let mut meter = Metered::new(std::io::empty(), total, true);
            meter.done = done;
            meter.line()
        };
        let mib = 1024 * 1024;
        assert_eq!(
            meter(0, Some(100 * mib)),
            "  [························] 0.0 / 100.0 MiB"
        );
        assert_eq!(
            meter(50 * mib, Some(100 * mib)),
            "  [############············] 50.0 / 100.0 MiB"
        );
        assert_eq!(
            meter(100 * mib, Some(100 * mib)),
            "  [########################] 100.0 / 100.0 MiB"
        );
        // a length that undercounts must not run the bar off its own end.
        assert_eq!(
            meter(200 * mib, Some(100 * mib)),
            "  [########################] 200.0 / 100.0 MiB"
        );
        // no length, and a zero length (which would divide by it).
        assert_eq!(meter(7 * mib, None), "  7.0 MiB");
        assert_eq!(meter(7 * mib, Some(0)), "  7.0 MiB");
    }

    /// The three readers of what a vendor publishes, on the shapes the vendors
    /// actually serve: a manifest's per-platform checksum, a sums file's line
    /// for one asset (binary-mode `*` prefix included), and the tag off the
    /// page `releases/latest` lands on. Each refuses a checksum that is not a
    /// sha256 rather than passing it on to be "verified" against.
    #[test]
    fn the_vendor_readers_take_what_is_published_and_nothing_else() {
        let sha = "26d020351e8112f4006790f3cfce43b4c9df0c1bb1d0e542364d64151b81d5ba";
        let manifest = format!(
            r#"{{"version":"2.1.263","platforms":{{"linux-x64":{{"binary":"claude","checksum":"{sha}","size":1}},"darwin-arm64":{{"checksum":"short"}}}}}}"#
        );
        assert_eq!(manifest_checksum(&manifest, "linux-x64").unwrap(), sha);
        assert!(
            manifest_checksum(&manifest, "linux-arm64")
                .unwrap_err()
                .contains("no checksum published for platform linux-arm64")
        );
        assert!(
            manifest_checksum(&manifest, "darwin-arm64")
                .unwrap_err()
                .contains("not a sha256")
        );
        assert!(manifest_checksum("{", "linux-x64").unwrap_err().contains("not a manifest"));

        let sums = format!(
            "{}  codex-package-aarch64-apple-darwin.tar.gz\n\
             {sha} *codex-package-x86_64-unknown-linux-musl.tar.gz\n\
             nonsense\n\
             short  codex-package-aarch64-unknown-linux-musl.tar.gz\n",
            "1".repeat(64)
        );
        assert_eq!(
            sums_checksum(&sums, "codex-package-x86_64-unknown-linux-musl.tar.gz").unwrap(),
            sha
        );
        assert!(
            sums_checksum(&sums, "codex-package-aarch64-unknown-linux-musl.tar.gz")
                .unwrap_err()
                .contains("not a sha256")
        );
        assert!(
            sums_checksum(&sums, "codex-x86_64-unknown-linux-musl.tar.gz")
                .unwrap_err()
                .contains("no checksum published for codex-x86_64")
        );

        assert_eq!(
            release_tag("/openai/codex/releases/tag/rust-v0.153.4").unwrap(),
            "rust-v0.153.4"
        );
        assert!(release_tag("/openai/codex/releases").is_err());
        assert!(release_tag("/openai/codex/releases/tag/").is_err());
    }

    /// The cache is the platform's, never a workspace's: `$XDG_CACHE_HOME`
    /// when set and non-empty, else the OS convention under `$HOME`.
    #[test]
    fn the_download_cache_is_the_platforms_cache_dir() {
        let root = |xdg: Option<&str>, home: Option<&str>, macos: bool| {
            cache_root(xdg.map(OsString::from), home.map(OsString::from), macos)
        };
        assert_eq!(
            root(Some("/var/cache/op"), Some("/home/op"), false).unwrap(),
            PathBuf::from("/var/cache/op/ducktape")
        );
        assert_eq!(
            root(Some(""), Some("/home/op"), false).unwrap(),
            PathBuf::from("/home/op/.cache/ducktape")
        );
        assert_eq!(
            root(None, Some("/Users/op"), true).unwrap(),
            PathBuf::from("/Users/op/Library/Caches/ducktape")
        );
        assert!(root(None, None, false).is_err());
    }

    /// codex is useless in the guest without its Code Mode companion, so a
    /// directory holding only `codex` must report as missing, not installed —
    /// and a complete set nobody wrote a receipt for is the operator's own.
    #[test]
    fn a_partial_install_reports_as_missing() {
        let dir = scratch("partial");
        let latest = release("rust-v1.0.0", &["codex", "codex-code-mode-host"]);
        let receipts = Receipts::default();

        stand_in(&dir, "codex");
        assert_eq!(
            installed(HarnessArg::Codex, &latest, &dir, &receipts),
            Installed::Missing
        );
        stand_in(&dir, "codex-code-mode-host");
        assert!(matches!(
            installed(HarnessArg::Codex, &latest, &dir, &receipts),
            Installed::Foreign { .. }
        ));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The receipt is what tells current from behind from the operator's own
    /// build: the same bytes read as `ok` under a receipt at the vendor's
    /// latest, as `BUMP` under an older one, and as `own` under a receipt
    /// whose hash the file no longer matches — the operator replaced it, and a
    /// checklist that offered to replace THAT back would be doing the one
    /// thing this verb exists not to do.
    #[test]
    fn a_receipt_tells_current_from_behind_from_the_operators_own_build() {
        let workspace = scratch("receipts");
        let dir = workspace_config::executor_dir(&workspace);
        std::fs::create_dir_all(&dir).unwrap();
        let latest = release("2.1.263", &["claude"]);
        stand_in(&dir, "claude");
        let sha256 = sha256_file(&dir.join("claude")).unwrap();

        let mut receipts = Receipts::default();
        receipts.providers.insert(
            "claude".into(),
            Receipt {
                version: "2.1.263".into(),
                sha256: sha256.clone(),
            },
        );
        assert_eq!(
            installed(HarnessArg::Claude, &latest, &dir, &receipts),
            Installed::Current {
                sha256: sha256.clone()
            }
        );

        receipts.providers.get_mut("claude").unwrap().version = "2.1.231".into();
        assert_eq!(
            installed(HarnessArg::Claude, &latest, &dir, &receipts),
            Installed::Behind {
                installed: "2.1.231".into()
            }
        );
        assert!(installed(HarnessArg::Claude, &latest, &dir, &receipts).is_offered());

        receipts.providers.get_mut("claude").unwrap().sha256 = "0".repeat(64);
        let own = installed(HarnessArg::Claude, &latest, &dir, &receipts);
        assert_eq!(own, Installed::Foreign { sha256 });
        assert!(!own.is_offered());

        // the receipts round-trip through the file beside the directory.
        receipts.save(&dir).unwrap();
        assert_eq!(Receipts::path(&dir), workspace.join("executors.toml"));
        let reloaded = Receipts::load(&dir).unwrap();
        assert_eq!(reloaded.providers, receipts.providers);
        // and an absent file is simply no receipts.
        assert!(
            Receipts::load(&scratch("no-receipts").join("executors"))
                .unwrap()
                .providers
                .is_empty()
        );

        std::fs::remove_dir_all(&workspace).unwrap();
    }

    /// The verify gate: an unexpected hash deletes the file and refuses. This
    /// is the whole point of the vendor's checksum, so it is checked without a
    /// network — a present `dest` skips the download.
    #[test]
    fn a_mismatched_download_is_deleted_and_refused() {
        let dir = scratch("fetch");
        let dest = dir.join("artifact");
        std::fs::write(&dest, b"not what the vendor published").unwrap();
        let vendors = Vendors::new().unwrap();

        let err = fetch(
            &vendors,
            "https://example.invalid/x",
            &dest,
            &"0".repeat(64),
            true,
        )
        .unwrap_err();
        assert!(
            err.contains("refusing to install an unverified executable"),
            "{err}"
        );
        assert!(!dest.exists(), "a mismatched artifact must not survive");

        // and the matching case installs: sha256("") is the empty-file hash.
        std::fs::write(&dest, b"").unwrap();
        fetch(
            &vendors,
            "https://example.invalid/x",
            &dest,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            true,
        )
        .unwrap();
        assert!(dest.exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn sha256_bytes(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex::encode(hasher.finalize())
    }

    struct RendezvousVendor {
        port: u16,
        handle: std::thread::JoinHandle<()>,
    }

    /// A vendor stand-in for the concurrency test below: accepts one HTTP
    /// request, then blocks on `barrier` before answering — so its response
    /// only goes out once its rendezvous partner also has a request in hand.
    /// A fetcher that requests its rows one after another (send request,
    /// read the full response, only then send the next request) can never
    /// land both halves of the barrier, so it hangs forever; one that fires
    /// every row's request up front satisfies it immediately.
    fn rendezvous_vendor(
        barrier: std::sync::Arc<std::sync::Barrier>,
        body: &'static [u8],
    ) -> RendezvousVendor {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            barrier.wait();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            stream.flush().unwrap();
        });
        RendezvousVendor { port, handle }
    }

    /// The bug this proves absent: `install_all` used to fetch its chosen
    /// providers one after another, so a second provider's download only
    /// began once the first one's response had been read in full. The two
    /// vendor stand-ins here only answer once BOTH have a request in hand,
    /// which a sequential fetcher can never arrange — this test would hang
    /// under the old behaviour, and completes under `fetch_all`'s
    /// one-thread-per-row fetch.
    #[test]
    fn fetch_all_downloads_every_row_concurrently() {
        let body_a: &[u8] = b"claude release bytes";
        let body_b: &[u8] = b"codex release bytes";
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let server_a = rendezvous_vendor(std::sync::Arc::clone(&barrier), body_a);
        let server_b = rendezvous_vendor(std::sync::Arc::clone(&barrier), body_b);

        let rows = [
            Surveyed {
                provider: HarnessArg::Claude,
                latest: Release {
                    version: "1".into(),
                    url: format!("http://127.0.0.1:{}/claude", server_a.port),
                    sha256: sha256_bytes(body_a),
                    payload: Payload::Binary("claude".into()),
                },
                state: Installed::Missing,
            },
            Surveyed {
                provider: HarnessArg::Codex,
                latest: Release {
                    version: "1".into(),
                    url: format!("http://127.0.0.1:{}/codex", server_b.port),
                    sha256: sha256_bytes(body_b),
                    payload: Payload::Binary("codex".into()),
                },
                state: Installed::Missing,
            },
        ];
        let cache = scratch("fetch-all-cache");
        let cache_cleanup = cache.clone();
        let vendors = Vendors::new().unwrap();

        // `fetch_all` blocks the calling thread until every row lands, so the
        // call itself runs on a throwaway thread and the test waits on it
        // with a bound: a regression to sequential fetching hangs that thread
        // forever (the barrier's other half never arrives), and this test
        // must fail rather than hang the suite.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let refs: Vec<&Surveyed> = rows.iter().collect();
            let _ = tx.send(fetch_all(&vendors, &refs, &cache));
        });
        let artifacts = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect(
                "fetch_all did not return within 10s — it fetched its rows one \
                 after another instead of concurrently",
            )
            .unwrap();

        assert_eq!(std::fs::read(&artifacts[0]).unwrap(), body_a);
        assert_eq!(std::fs::read(&artifacts[1]).unwrap(), body_b);

        server_a.handle.join().unwrap();
        server_b.handle.join().unwrap();
        std::fs::remove_dir_all(&cache_cleanup).unwrap();
    }

    /// Both built-in channels answer, live: the feed names a version and a
    /// checksum for each Linux platform, and the GitHub release names a tag
    /// and a checksum for each arch's asset. The one test that talks to the
    /// vendors, so it runs on request:
    ///
    ///   cargo test -p node-bin --bin ducktape -- --ignored executors::tests::the_vendors
    #[test]
    #[ignore = "live: resolves the latest release from each vendor over the network"]
    fn the_vendors_publish_a_latest_release_for_both_guest_arches() {
        let specs = SpecSet::load(None).unwrap();
        let vendors = Vendors::new().unwrap();
        for provider in ALL {
            for arch in [GuestArch::Aarch64, GuestArch::X86_64] {
                let latest = vendors
                    .latest(specs.get(provider.token()).unwrap(), arch)
                    .unwrap();
                assert!(!latest.version.is_empty(), "{} {arch:?}", provider.token());
                assert!(latest.url.starts_with("https://"), "{}", latest.url);
                assert!(is_sha256_hex(&latest.sha256), "{} {arch:?}", provider.token());
                assert!(
                    latest.files().contains(&provider.token()),
                    "the release delivers the binary named for its provider"
                );
                eprintln!("{} {arch:?}: {} {}", provider.token(), latest.version, latest.url);
            }
        }
    }
}
