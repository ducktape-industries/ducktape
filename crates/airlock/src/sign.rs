//! The `POST /sign/macos-bundle` pipeline: sign, notarize and staple an
//! UNSIGNED `Ducktape.app` with an admitted [`AppleCodesign`] identity, inside
//! the gateway, so the identity never leaves it.
//!
//! The pipeline is ONE state machine, [`Stage`] by [`Stage`]:
//!
//! ```text
//! Received ─validate─▶ Validated ─sign─▶ Signed ─notarize─▶ Notarized ─staple─▶ Stapled ─pack─▶ done
//!     │                    │               │                    │                  │
//!     └────────────────────┴───────────────┴────────────────────┴──────────────────┴──▶ Refused
//! ```
//!
//! [`advance`] is the table (pure: a stage and what its step reported in, the
//! next stage or the terminal answer out); [`Job::step`] is the one visible
//! dispatch that runs a stage's effect; [`sign_bundle`] is the executor that
//! walks them. Every refusal is a [`Refusal`] token, never prose.
//!
//! Order matters and is the whole design: the bundle's SHAPE is checked
//! before the identity is touched, so a malformed upload never gets the p12
//! written to disk; the identity is materialized into a private 0700
//! directory under the tmpfs root only for the `rcodesign` runs, and that
//! directory is removed on every exit path (it is a [`tempfile::TempDir`],
//! dropped with the job).
//!
//! `rcodesign` is a BINARY in the gateway image, not a library in the
//! measured enclave binary (`ops/airlock-gateway/install-rcodesign.sh`
//! installs the pinned release beside it). A gateway whose image lacks it
//! refuses by name (`tool_missing`) rather than serving a route it cannot
//! honour.
//!
//! Two commands do the Apple half, so a rejection and a staple failure are
//! separate stages with separate tokens:
//!
//! ```text
//! rcodesign sign --p12-file identity.p12 --p12-password-file p12.password \
//!     --code-signature-flags runtime \
//!     --code-signature-flags Contents/MacOS/ducktape-app:runtime \
//!     --entitlements-xml-file <entitlements> \
//!     --entitlements-xml-file Contents/MacOS/ducktape-app:<entitlements> \
//!     Ducktape.app
//! rcodesign notary-submit --api-key-file api-key.json --wait Ducktape.app
//! rcodesign staple Ducktape.app
//! ```
//!
//! The scoped flags mirror `ops/bundle-app-macos.sh`, which signs the nested
//! `ducktape-app` and then the bundle, each with the hardened runtime and the
//! same entitlements: `rcodesign` applies an unscoped setting to the main
//! executable only. `--for-notarization` is NOT passed: it refuses any
//! certificate Apple did not issue, which would make the sign step
//! unexercisable with a throwaway identity, and every check it adds is
//! already made elsewhere — the Developer ID marker and team at admission
//! (`codesign::AppleCodesign::admit`), the runtime flag here, the time-stamp
//! by the default server, and the rest by Apple's notary service itself.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fmt;
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use sha2::{Digest as _, Sha256};

use crate::codesign::AppleCodesign;

/// The one top-level entry an archive may carry.
pub const BUNDLE_NAME: &str = "Ducktape.app";

/// The `CFBundleIdentifier` the bundle's `Info.plist` must name.
pub const BUNDLE_ID: &str = "dev.ducktape.app";

/// Exactly what `Contents/MacOS` holds: the launcher (the
/// `CFBundleExecutable`) and the app beside it. No view: the network serves
/// every view the app draws out of its genesis.
pub const MACOS_ENTRIES: [&str; 2] = ["ducktape-launcher", "ducktape-app"];

/// The nested executable that gets the same runtime flag and entitlements
/// as the main one (`ops/bundle-app-macos.sh` signs it first, then the bundle).
const NESTED_EXECUTABLE: &str = "Contents/MacOS/ducktape-app";

/// One sealed response chunk (`bodyseal::StreamSealer::seal_chunk`), under
/// the opener's 2 MiB ciphertext ceiling.
pub const RESPONSE_CHUNK_BYTES: usize = 1024 * 1024;

/// How often the signing route seals a keepalive into its reply while the
/// pipeline runs. The head goes out before a byte of the bundle is signed,
/// and the archive follows minutes later (Apple's notary wait); in between,
/// every hop on the way back — the publisher node's per-read deadline on its
/// loopback upstream, the overlay drain — needs to see bytes inside its own
/// idle ceiling. `bin/node`'s gateway plane asserts this sits under its
/// `BODY_IDLE_TIMEOUT`.
pub const KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// What the tools in the gateway image are, and where the enclave may write.
/// Parsed ONCE at the binary's boundary (`bin/airlock-gateway`); this module
/// never reads the environment.
pub struct Tools {
    /// The `rcodesign` executable. A bare name resolves on `PATH`.
    pub rcodesign: PathBuf,
    /// `app/packaging/entitlements.plist` as shipped in the image, applied to
    /// the launcher and the nested app alike.
    pub entitlements: PathBuf,
    /// Where per-request work directories are created: a tmpfs (`/dev/shm`
    /// in the image), so neither the identity nor the bundle touches a disk.
    pub work_root: PathBuf,
    /// Apple's notary service, or the test seam standing in for it.
    pub notary: Notary,
}

/// The one seam a test may stand in for: Apple. Signing runs the real
/// `rcodesign sign` on every arm; only the round trips to Apple — the
/// signature time-stamp, the notary submission, the ticket fetched by
/// `staple` — are answered locally under [`Notary::Stub`], which no product
/// build can construct.
pub enum Notary {
    /// `rcodesign notary-submit --wait` then `rcodesign staple`, against
    /// Apple; signatures time-stamped by Apple's server.
    Apple,
    /// The answer Apple would have given, without asking. Signatures carry
    /// no time-stamp (`--timestamp-url none`) — there is no Apple to ask.
    #[cfg(any(test, feature = "testkit"))]
    Stub(StubNotary),
}

/// How the stubbed Apple answers.
#[cfg(any(test, feature = "testkit"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StubNotary {
    /// Notarized and stapled.
    Accepts,
    /// Notarized and stapled, after Apple's wait: the notarize step holds
    /// the pipeline for `delay` first. The seam a test uses to make the
    /// answer arrive later than a proxy's head deadline.
    AcceptsAfter(std::time::Duration),
    /// The submission is rejected.
    Rejects,
    /// Notarized, but the ticket cannot be stapled.
    StapleFails,
}

/// Why a signing request was refused. Stable snake_case tokens: the response
/// body and the audit line carry [`Refusal::as_str`] and nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The archive is not exactly one unsigned `Ducktape.app` of the shape
    /// `ops/bundle-app-macos.sh` stages (see [`validate_layout`]).
    BundleShapeRefused,
    /// `rcodesign sign` failed.
    CodesignFailed,
    /// Apple's notary service rejected the submission; `submission_id` is
    /// the id it assigned, when one was.
    NotaryRejected { submission_id: Option<String> },
    /// The notarization ticket could not be stapled.
    StapleFailed,
    /// The image lacks `rcodesign`, the entitlements file, or a writable
    /// work root.
    ToolMissing,
}

impl Refusal {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BundleShapeRefused => "bundle_shape_refused",
            Self::CodesignFailed => "codesign_failed",
            Self::NotaryRejected { .. } => "notary_rejected",
            Self::StapleFailed => "staple_failed",
            Self::ToolMissing => "tool_missing",
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for Refusal {}

/// A signed, notarized, stapled bundle.
pub struct Signed {
    /// The `.tar.zst` of the finished `Ducktape.app`.
    pub archive: Vec<u8>,
    /// The notary submission Apple accepted (`None` under the stub).
    pub submission_id: Option<String>,
}

// -------- the state machine --------

/// Where a job is. Each stage names the step that leaves it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// The sealed archive arrived; `validate` unpacks and checks its shape.
    Received,
    /// The bundle is the right shape; `sign` runs `rcodesign sign`.
    Validated,
    /// Every Mach-O carries a signature; `notarize` submits to Apple.
    Signed,
    /// Apple issued a ticket; `staple` attaches it.
    Notarized,
    /// The bundle is complete; `pack` archives it for the response.
    Stapled,
}

/// What a stage's step reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepOutcome {
    Advanced,
    Refused(Refusal),
}

/// What the executor does next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Next {
    Run(Stage),
    Done,
    Refuse(Refusal),
}

/// The transition table. Pure: no stage skips, no stage repeats, and a
/// refusal at any stage ends the job with that stage's token.
pub fn advance(stage: Stage, outcome: StepOutcome) -> Next {
    match (stage, outcome) {
        (Stage::Received, StepOutcome::Advanced) => Next::Run(Stage::Validated),
        (Stage::Validated, StepOutcome::Advanced) => Next::Run(Stage::Signed),
        (Stage::Signed, StepOutcome::Advanced) => Next::Run(Stage::Notarized),
        (Stage::Notarized, StepOutcome::Advanced) => Next::Run(Stage::Stapled),
        (Stage::Stapled, StepOutcome::Advanced) => Next::Done,
        (Stage::Received, StepOutcome::Refused(refusal))
        | (Stage::Validated, StepOutcome::Refused(refusal))
        | (Stage::Signed, StepOutcome::Refused(refusal))
        | (Stage::Notarized, StepOutcome::Refused(refusal))
        | (Stage::Stapled, StepOutcome::Refused(refusal)) => Next::Refuse(refusal),
    }
}

/// Run one signing job to its end. `archive` is the OPENED request body (the
/// `.tar.zst`); the identity is read only inside the `sign`/`notarize`
/// steps, after the shape check passed.
pub fn sign_bundle(
    tools: &Tools,
    identity: &AppleCodesign,
    archive: &[u8],
) -> Result<Signed, Refusal> {
    let work = WorkDir::create(&tools.work_root).map_err(|_| Refusal::ToolMissing)?;
    let mut job = Job {
        tools,
        identity,
        work,
        archive,
        submission_id: None,
        packed: Vec::new(),
    };
    let mut stage = Stage::Received;
    loop {
        let outcome = job.step(stage);
        match advance(stage, outcome) {
            Next::Run(next) => stage = next,
            Next::Done => {
                return Ok(Signed {
                    archive: job.packed,
                    submission_id: job.submission_id,
                });
            }
            Next::Refuse(refusal) => return Err(refusal),
        }
    }
}

/// One request's state while it runs: the tools, the identity, the private
/// work directory, and what the stages so far produced.
struct Job<'a> {
    tools: &'a Tools,
    identity: &'a AppleCodesign,
    work: WorkDir,
    archive: &'a [u8],
    submission_id: Option<String>,
    packed: Vec<u8>,
}

impl Job<'_> {
    /// The one visible dispatch: a stage to the step that leaves it.
    fn step(&mut self, stage: Stage) -> StepOutcome {
        match stage {
            Stage::Received => self.validate(),
            Stage::Validated => self.sign(),
            Stage::Signed => self.notarize(),
            Stage::Notarized => self.staple(),
            Stage::Stapled => self.pack(),
        }
    }

    fn validate(&mut self) -> StepOutcome {
        let unpacked = unpack_bundle(self.archive, &self.work.bundle_root());
        match unpacked {
            Ok(()) => {}
            Err(refusal) => return StepOutcome::Refused(refusal),
        }
        match validate_layout(&self.work.bundle()) {
            Ok(()) => StepOutcome::Advanced,
            Err(refusal) => StepOutcome::Refused(refusal),
        }
    }

    fn sign(&mut self) -> StepOutcome {
        let entitlements_present = self.tools.entitlements.is_file();
        if !entitlements_present {
            return StepOutcome::Refused(Refusal::ToolMissing);
        }
        let materialized = self.work.materialize(self.identity);
        if materialized.is_err() {
            return StepOutcome::Refused(Refusal::CodesignFailed);
        }
        let entitlements = self.tools.entitlements.as_os_str().to_os_string();
        let mut nested_entitlements = std::ffi::OsString::from(format!("{NESTED_EXECUTABLE}:"));
        nested_entitlements.push(&entitlements);
        let mut args: Vec<std::ffi::OsString> = vec![
            "sign".into(),
            "--p12-file".into(),
            self.work.p12_path().into(),
            "--p12-password-file".into(),
            self.work.password_path().into(),
            "--code-signature-flags".into(),
            "runtime".into(),
            "--code-signature-flags".into(),
            format!("{NESTED_EXECUTABLE}:runtime").into(),
            "--entitlements-xml-file".into(),
            entitlements,
            "--entitlements-xml-file".into(),
            nested_entitlements,
        ];
        args.extend(self.timestamp_args());
        args.push(BUNDLE_NAME.into());
        match self.rcodesign(&args) {
            ToolRun::Ok(_) => StepOutcome::Advanced,
            ToolRun::Failed(_) => StepOutcome::Refused(Refusal::CodesignFailed),
            ToolRun::Missing => StepOutcome::Refused(Refusal::ToolMissing),
        }
    }

    fn notarize(&mut self) -> StepOutcome {
        match &self.tools.notary {
            Notary::Apple => self.notarize_with_apple(),
            #[cfg(any(test, feature = "testkit"))]
            Notary::Stub(answer) => match answer {
                StubNotary::Accepts | StubNotary::StapleFails => StepOutcome::Advanced,
                StubNotary::AcceptsAfter(delay) => {
                    // the pipeline runs on a blocking thread; Apple's wait is
                    // a blocking wait there too.
                    std::thread::sleep(*delay);
                    StepOutcome::Advanced
                }
                StubNotary::Rejects => StepOutcome::Refused(Refusal::NotaryRejected {
                    submission_id: None,
                }),
            },
        }
    }

    fn notarize_with_apple(&mut self) -> StepOutcome {
        let args: Vec<std::ffi::OsString> = vec![
            "notary-submit".into(),
            "--api-key-file".into(),
            self.work.api_key_path().into(),
            "--wait".into(),
            BUNDLE_NAME.into(),
        ];
        match self.rcodesign(&args) {
            ToolRun::Ok(output) => {
                self.submission_id = submission_id(&output);
                StepOutcome::Advanced
            }
            ToolRun::Failed(output) => StepOutcome::Refused(Refusal::NotaryRejected {
                submission_id: submission_id(&output),
            }),
            ToolRun::Missing => StepOutcome::Refused(Refusal::ToolMissing),
        }
    }

    fn staple(&mut self) -> StepOutcome {
        match &self.tools.notary {
            Notary::Apple => self.staple_with_apple(),
            #[cfg(any(test, feature = "testkit"))]
            // a rejected submission never reaches this stage: the table
            // sent it to `Refuse` from `Signed`.
            Notary::Stub(answer) => match answer {
                StubNotary::Accepts | StubNotary::AcceptsAfter(_) | StubNotary::Rejects => {
                    StepOutcome::Advanced
                }
                StubNotary::StapleFails => StepOutcome::Refused(Refusal::StapleFailed),
            },
        }
    }

    fn staple_with_apple(&mut self) -> StepOutcome {
        let args: Vec<std::ffi::OsString> = vec!["staple".into(), BUNDLE_NAME.into()];
        match self.rcodesign(&args) {
            ToolRun::Ok(_) => StepOutcome::Advanced,
            ToolRun::Failed(_) => StepOutcome::Refused(Refusal::StapleFailed),
            ToolRun::Missing => StepOutcome::Refused(Refusal::ToolMissing),
        }
    }

    fn pack(&mut self) -> StepOutcome {
        match pack_bundle(&self.work.bundle()) {
            Ok(archive) => {
                self.packed = archive;
                StepOutcome::Advanced
            }
            Err(_) => StepOutcome::Refused(Refusal::CodesignFailed),
        }
    }

    /// Only Apple time-stamps a signature; the stub has no server to ask.
    fn timestamp_args(&self) -> Vec<std::ffi::OsString> {
        match &self.tools.notary {
            Notary::Apple => Vec::new(),
            #[cfg(any(test, feature = "testkit"))]
            Notary::Stub(_) => vec!["--timestamp-url".into(), "none".into()],
        }
    }

    /// Run `rcodesign` in the bundle's parent directory, so the bundle is
    /// named by its bare name and every scoped path is bundle-relative. Its
    /// output is `debug` — per-request, and it names paths, never the
    /// identity.
    fn rcodesign(&self, args: &[std::ffi::OsString]) -> ToolRun {
        let run = Command::new(&self.tools.rcodesign)
            .args(args)
            .current_dir(self.work.bundle_root())
            .output();
        let output = match run {
            Ok(output) => output,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return ToolRun::Missing,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                return ToolRun::Missing;
            }
            Err(_) => return ToolRun::Failed(String::new()),
        };
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        let subcommand = args
            .first()
            .map(|arg| arg.to_string_lossy().into_owned())
            .unwrap_or_default();
        tracing::debug!(
            target: "ducktape::airlock",
            subcommand = %subcommand,
            status = ?output.status.code(),
            output = %text,
            "rcodesign"
        );
        if output.status.success() {
            ToolRun::Ok(text)
        } else {
            ToolRun::Failed(text)
        }
    }
}

/// How one `rcodesign` invocation ended.
enum ToolRun {
    Ok(String),
    Failed(String),
    /// The executable is not there (or not executable).
    Missing,
}

/// The submission id `rcodesign notary-submit` logs
/// (`created submission ID: <id>`), if the run got that far.
fn submission_id(output: &str) -> Option<String> {
    const MARKER: &str = "created submission ID: ";
    let after = output.find(MARKER).map(|at| &output[at + MARKER.len()..])?;
    let id: String = after
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    (!id.is_empty()).then_some(id)
}

// -------- the private work directory --------

/// A per-request directory under the tmpfs root, mode 0700, removed when
/// dropped — on success, on refusal, and on a panic unwinding through the
/// job alike. Holds the identity files (0600) beside the unpacked bundle.
struct WorkDir {
    dir: tempfile::TempDir,
}

impl WorkDir {
    fn create(root: &Path) -> io::Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("airlock-sign-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(root)?;
        std::fs::create_dir(dir.path().join("bundle"))?;
        Ok(Self { dir })
    }

    /// The directory the bundle is unpacked INTO (`rcodesign` runs here).
    fn bundle_root(&self) -> PathBuf {
        self.dir.path().join("bundle")
    }

    /// The unpacked `Ducktape.app`.
    fn bundle(&self) -> PathBuf {
        self.bundle_root().join(BUNDLE_NAME)
    }

    fn p12_path(&self) -> PathBuf {
        self.dir.path().join("identity.p12")
    }

    fn password_path(&self) -> PathBuf {
        self.dir.path().join("p12.password")
    }

    fn api_key_path(&self) -> PathBuf {
        self.dir.path().join("api-key.json")
    }

    /// Write the three identity files `rcodesign` reads, 0600 each. Called
    /// only after the bundle validated.
    fn materialize(&self, identity: &AppleCodesign) -> io::Result<()> {
        write_private(&self.p12_path(), identity.p12())?;
        write_private(&self.password_path(), identity.p12_password().as_bytes())?;
        write_private(&self.api_key_path(), identity.api_key_json().as_bytes())?;
        Ok(())
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.flush()
}

// -------- the archive --------

/// Unpack the `.tar.zst` into `root`, refusing anything but one
/// `Ducktape.app/` tree of regular files, directories and symlinks that stay
/// inside it. Every entry is checked before it is written.
///
/// Shared with the client side (`ducktape release sign-bundle`), which
/// unpacks the enclave's reply under the same rules before it trusts its shape.
pub fn unpack_bundle(archive: &[u8], root: &Path) -> Result<(), Refusal> {
    let decoder = zstd::Decoder::new(archive).map_err(|_| Refusal::BundleShapeRefused)?;
    let mut tar = tar::Archive::new(decoder);
    tar.set_overwrite(false);
    tar.set_preserve_permissions(false);
    tar.set_unpack_xattrs(false);
    let entries = tar.entries().map_err(|_| Refusal::BundleShapeRefused)?;
    for entry in entries {
        let mut entry = entry.map_err(|_| Refusal::BundleShapeRefused)?;
        let path = entry
            .path()
            .map_err(|_| Refusal::BundleShapeRefused)?
            .into_owned();
        let link = entry
            .link_name()
            .map_err(|_| Refusal::BundleShapeRefused)?
            .map(|link| link.into_owned());
        check_entry(
            &path,
            EntryKind::of(entry.header().entry_type()),
            link.as_deref(),
        )?;
        let written = entry
            .unpack_in(root)
            .map_err(|_| Refusal::BundleShapeRefused)?;
        if !written {
            return Err(Refusal::BundleShapeRefused);
        }
    }
    Ok(())
}

/// What an archive entry is, reduced to the three kinds a bundle holds and
/// everything else. `tar::EntryType` is open-ended (hard links, devices,
/// fifos, pax/GNU extension records, and a hidden catch-all); only these
/// three are a bundle's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

impl EntryKind {
    fn of(kind: tar::EntryType) -> Self {
        let is_file = kind.is_file();
        let is_dir = kind.is_dir();
        let is_symlink = kind.is_symlink();
        match (is_file, is_dir, is_symlink) {
            (true, false, false) => Self::File,
            (false, true, false) => Self::Dir,
            (false, false, true) => Self::Symlink,
            (false, false, false) | (true, true, _) | (true, _, true) | (_, true, true) => {
                Self::Other
            }
        }
    }
}

/// One archive entry against the shape rules: a plain relative path under
/// `Ducktape.app/` (no `..`, `/` or prefix anywhere — `tar` refuses those
/// too, this names them first), one of the three kinds, and a symlink that
/// resolves inside the bundle.
fn check_entry(path: &Path, kind: EntryKind, link: Option<&Path>) -> Result<(), Refusal> {
    let plain = plain_bundle_path(path).ok_or(Refusal::BundleShapeRefused)?;
    match kind {
        EntryKind::File | EntryKind::Dir => Ok(()),
        EntryKind::Symlink => {
            let target = link.ok_or(Refusal::BundleShapeRefused)?;
            let parent = plain.parent().unwrap_or(Path::new(""));
            let resolved = parent.join(target);
            normalized_inside_bundle(&resolved)
                .map(|_| ())
                .ok_or(Refusal::BundleShapeRefused)
        }
        EntryKind::Other => Err(Refusal::BundleShapeRefused),
    }
}

/// `path` with `.` dropped, if every other component is a plain name and
/// the first is `Ducktape.app`. An entry path is written as given, so it
/// gets no `..` at all.
fn plain_bundle_path(path: &Path) -> Option<PathBuf> {
    let mut out: Vec<&OsStr> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    let top_is_bundle = out.first().copied() == Some(OsStr::new(BUNDLE_NAME));
    if !top_is_bundle {
        return None;
    }
    Some(out.iter().collect())
}

/// `path` with `.` dropped and `..` resolved, if it stays under
/// `Ducktape.app/` (the bundle directory itself included) — what a symlink
/// target is checked with, since a relative link inside the bundle is
/// legitimate. `None` for an absolute path, a prefix, a `..` that climbs out, or
/// any other top-level name.
fn normalized_inside_bundle(path: &Path) -> Option<PathBuf> {
    let mut out: Vec<&OsStr> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    let top_is_bundle = out.first().copied() == Some(OsStr::new(BUNDLE_NAME));
    if !top_is_bundle {
        return None;
    }
    Some(out.iter().collect())
}

/// The unpacked bundle against the shape `ops/bundle-app-macos.sh` stages:
/// `Contents/Info.plist` naming [`BUNDLE_ID`], and `Contents/MacOS` holding
/// exactly [`MACOS_ENTRIES`] with both executables regular files.
pub fn validate_layout(bundle: &Path) -> Result<(), Refusal> {
    let plist = std::fs::read_to_string(bundle.join("Contents/Info.plist"))
        .map_err(|_| Refusal::BundleShapeRefused)?;
    let bundle_id = plist_string(&plist, "CFBundleIdentifier");
    let id_matches = bundle_id.as_deref() == Some(BUNDLE_ID);
    if !id_matches {
        return Err(Refusal::BundleShapeRefused);
    }
    let macos = bundle.join("Contents/MacOS");
    let listing = std::fs::read_dir(&macos).map_err(|_| Refusal::BundleShapeRefused)?;
    let mut names = BTreeSet::new();
    for entry in listing {
        let entry = entry.map_err(|_| Refusal::BundleShapeRefused)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Refusal::BundleShapeRefused)?;
        names.insert(name);
    }
    let expected: BTreeSet<String> = MACOS_ENTRIES.iter().map(|s| s.to_string()).collect();
    let exact = names == expected;
    if !exact {
        return Err(Refusal::BundleShapeRefused);
    }
    for executable in ["ducktape-launcher", "ducktape-app"] {
        let meta = std::fs::symlink_metadata(macos.join(executable))
            .map_err(|_| Refusal::BundleShapeRefused)?;
        if !meta.is_file() {
            return Err(Refusal::BundleShapeRefused);
        }
    }
    Ok(())
}

/// The `<string>` that follows `<key>NAME</key>` in an XML plist. The
/// bundle's `Info.plist` is the XML `app/packaging/Info.plist` with two
/// version keys added by `PlistBuddy`, which keeps it XML; a binary plist
/// is not the shape this route signs.
pub fn plist_string(plist: &str, key: &str) -> Option<String> {
    let key_tag = format!("<key>{key}</key>");
    let after_key = &plist[plist.find(&key_tag)? + key_tag.len()..];
    let open = after_key.find("<string>")?;
    // the value must be the very next element: another `<key>` first means
    // this key's value is not a string.
    let next_key = after_key.find("<key>");
    let string_is_next = next_key.is_none_or(|k| open < k);
    if !string_is_next {
        return None;
    }
    let after_open = &after_key[open + "<string>".len()..];
    let close = after_open.find("</string>")?;
    Some(after_open[..close].trim().to_string())
}

/// Archive the finished bundle as `Ducktape.app/…` in a `.tar.zst`, symlinks
/// kept as symlinks, owners and times zeroed as
/// `ops/release/archive.sh` does. The client packs an unsigned bundle with
/// the same function, so what it sends is what this side unpacks.
pub fn pack_bundle(bundle: &Path) -> io::Result<Vec<u8>> {
    let encoder = zstd::Encoder::new(Vec::new(), 3)?;
    let mut tar = tar::Builder::new(encoder);
    tar.mode(tar::HeaderMode::Deterministic);
    tar.follow_symlinks(false);
    tar.append_dir_all(BUNDLE_NAME, bundle)?;
    let encoder = tar.into_inner()?;
    encoder.finish()
}

/// SHA-256 of an archive, hex: the audit handle for a bundle in and out.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Split a finished archive into the chunks the sealed response stream
/// carries; each is under the opener's ciphertext ceiling.
pub fn response_chunks(archive: &[u8]) -> impl Iterator<Item = &[u8]> {
    archive.chunks(RESPONSE_CHUNK_BYTES)
}

/// A test/fixture bundle builder and a minimal Mach-O, so a throwaway
/// identity can sign a real bundle shape on a Linux box. Behind `testkit`.
#[cfg(feature = "testkit")]
pub mod fixture {
    use std::io::Write as _;
    use std::path::Path;

    use super::BUNDLE_NAME;

    /// The smallest x86_64 Mach-O executable `rcodesign` will sign: a header,
    /// `__PAGEZERO`, a `__TEXT` segment with one `__text` section holding a
    /// `ret`, an empty trailing `__LINKEDIT` for the signature to land in, and
    /// an `LC_BUILD_VERSION` naming macOS 11.
    pub fn stub_macho() -> Vec<u8> {
        const MH_MAGIC_64: u32 = 0xfeed_facf;
        const CPU_TYPE_X86_64: u32 = 0x0100_0007;
        const CPU_SUBTYPE_X86_64_ALL: u32 = 3;
        const MH_EXECUTE: u32 = 2;
        const MH_NOUNDEFS_PIE: u32 = 1 | 0x0020_0000;
        const LC_SEGMENT_64: u32 = 0x19;
        const LC_BUILD_VERSION: u32 = 0x32;
        const PAGE: u64 = 0x1000;
        const TEXT_OFF: u64 = 0x800;
        const S_ATTR_PURE_INSTRUCTIONS_SOME: u32 = 0x8000_0400;

        fn name16(name: &str) -> [u8; 16] {
            let mut out = [0u8; 16];
            out[..name.len()].copy_from_slice(name.as_bytes());
            out
        }
        fn segment(
            name: &str,
            vmaddr: u64,
            vmsize: u64,
            fileoff: u64,
            filesize: u64,
            prot: u32,
            sections: &[u8],
        ) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&LC_SEGMENT_64.to_le_bytes());
            out.extend_from_slice(&(72 + sections.len() as u32).to_le_bytes());
            out.extend_from_slice(&name16(name));
            out.extend_from_slice(&vmaddr.to_le_bytes());
            out.extend_from_slice(&vmsize.to_le_bytes());
            out.extend_from_slice(&fileoff.to_le_bytes());
            out.extend_from_slice(&filesize.to_le_bytes());
            out.extend_from_slice(&prot.to_le_bytes());
            out.extend_from_slice(&prot.to_le_bytes());
            out.extend_from_slice(&((sections.len() / 80) as u32).to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(sections);
            out
        }
        let mut section = Vec::new();
        section.extend_from_slice(&name16("__text"));
        section.extend_from_slice(&name16("__TEXT"));
        section.extend_from_slice(&(0x1_0000_0000u64 + TEXT_OFF).to_le_bytes());
        section.extend_from_slice(&16u64.to_le_bytes());
        section.extend_from_slice(&(TEXT_OFF as u32).to_le_bytes());
        section.extend_from_slice(&[0u8; 12]);
        section.extend_from_slice(&S_ATTR_PURE_INSTRUCTIONS_SOME.to_le_bytes());
        section.extend_from_slice(&[0u8; 12]);

        let mut build_version = Vec::new();
        build_version.extend_from_slice(&LC_BUILD_VERSION.to_le_bytes());
        build_version.extend_from_slice(&24u32.to_le_bytes());
        build_version.extend_from_slice(&1u32.to_le_bytes()); // PLATFORM_MACOS
        build_version.extend_from_slice(&(11u32 << 16).to_le_bytes()); // minos 11.0
        build_version.extend_from_slice(&(11u32 << 16).to_le_bytes()); // sdk 11.0
        build_version.extend_from_slice(&0u32.to_le_bytes());

        let mut commands = Vec::new();
        commands.extend(segment("__PAGEZERO", 0, 0x1_0000_0000, 0, 0, 0, &[]));
        commands.extend(segment("__TEXT", 0x1_0000_0000, PAGE, 0, PAGE, 5, &section));
        commands.extend(segment("__LINKEDIT", 0x1_0000_1000, 0, PAGE, 0, 1, &[]));
        commands.extend(build_version);

        let mut out = Vec::new();
        out.extend_from_slice(&MH_MAGIC_64.to_le_bytes());
        out.extend_from_slice(&CPU_TYPE_X86_64.to_le_bytes());
        out.extend_from_slice(&CPU_SUBTYPE_X86_64_ALL.to_le_bytes());
        out.extend_from_slice(&MH_EXECUTE.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&(commands.len() as u32).to_le_bytes());
        out.extend_from_slice(&MH_NOUNDEFS_PIE.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend(commands);
        out.resize(TEXT_OFF as usize, 0);
        out.extend_from_slice(&[0x48, 0x31, 0xc0, 0xc3]); // xor rax, rax; ret
        out.resize(PAGE as usize, 0);
        out
    }

    /// The `Info.plist` `ops/bundle-app-macos.sh` installs, under `bundle_id`.
    pub fn info_plist(bundle_id: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleName</key><string>Ducktape</string>
<key>CFBundleIdentifier</key><string>{bundle_id}</string>
<key>CFBundleExecutable</key><string>ducktape-launcher</string>
<key>CFBundleShortVersionString</key><string>0.0.0</string>
<key>CFBundleVersion</key><string>0.0.0</string>
<key>LSMinimumSystemVersion</key><string>11.0</string>
</dict></plist>
"#
        )
    }

    /// Stage an unsigned `Ducktape.app` of the release shape under `root`:
    /// two stub executables and the plist. Returns the bundle path.
    pub fn stage_bundle(root: &Path, bundle_id: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let app = root.join(BUNDLE_NAME);
        let macos = app.join("Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        for executable in ["ducktape-launcher", "ducktape-app"] {
            let path = macos.join(executable);
            std::fs::write(&path, stub_macho()).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(app.join("Contents/Info.plist"), info_plist(bundle_id)).unwrap();
        app
    }

    /// `Ducktape.app/…` as a `.tar.zst`, the way `ops/release/archive.sh`
    /// packs it (symlinks kept, owners dropped).
    pub fn archive(bundle: &Path) -> Vec<u8> {
        super::pack_bundle(bundle).unwrap()
    }

    /// A `.tar.zst` of arbitrary entries, for shape-refusal cases: each is
    /// `(path, kind)`. Paths and link targets are written into the header
    /// fields raw (`tar::Builder` itself refuses `..` and absolute paths,
    /// and the validator must be the one refusing them).
    pub fn archive_of(entries: &[(&str, Entry)]) -> Vec<u8> {
        fn raw(field: &mut [u8], value: &str) {
            field[..value.len()].copy_from_slice(value.as_bytes());
        }
        let encoder = zstd::Encoder::new(Vec::new(), 3).unwrap();
        let mut tar = tar::Builder::new(encoder);
        for (path, entry) in entries {
            let mut header = tar::Header::new_gnu();
            raw(&mut header.as_gnu_mut().unwrap().name, path);
            let (kind, mode, link, data): (tar::EntryType, u32, Option<&str>, &[u8]) = match entry {
                Entry::File(bytes) => (tar::EntryType::Regular, 0o644, None, bytes),
                Entry::Dir => (tar::EntryType::Directory, 0o755, None, &[]),
                Entry::Symlink(target) => (tar::EntryType::Symlink, 0o777, Some(target), &[]),
                Entry::HardLink(target) => (tar::EntryType::Link, 0o644, Some(target), &[]),
            };
            header.set_entry_type(kind);
            header.set_mode(mode);
            header.set_size(data.len() as u64);
            if let Some(target) = link {
                raw(&mut header.as_gnu_mut().unwrap().linkname, target);
            }
            header.set_cksum();
            tar.append(&header, data).unwrap();
        }
        let mut encoder = tar.into_inner().unwrap();
        encoder.flush().unwrap();
        encoder.finish().unwrap()
    }

    /// The `rcodesign` a test signs with: `DUCKTAPE_RCODESIGN`, else the
    /// first on `PATH`. Not optional — a test that skipped here would print
    /// a green nobody could tell from a real one; `make rcodesign` installs
    /// the pinned release into `~/.cargo/bin`.
    pub fn rcodesign() -> std::path::PathBuf {
        if let Some(path) = std::env::var_os("DUCKTAPE_RCODESIGN") {
            return std::path::PathBuf::from(path);
        }
        let on_path = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&on_path)
            .map(|dir| dir.join("rcodesign"))
            .find(|candidate| candidate.is_file())
            .expect(
                "rcodesign is not installed: run `make rcodesign` (ops/airlock-gateway/install-rcodesign.sh) or set DUCKTAPE_RCODESIGN",
            )
    }

    /// One archive entry for [`archive_of`].
    pub enum Entry {
        File(Vec<u8>),
        Dir,
        Symlink(&'static str),
        HardLink(&'static str),
    }
}

#[cfg(all(test, feature = "testkit"))]
mod tests {
    use super::fixture::{self, Entry};
    use super::*;

    fn validate_archive(archive: &[u8]) -> Result<(), Refusal> {
        let work = tempfile::tempdir().unwrap();
        unpack_bundle(archive, work.path())?;
        validate_layout(&work.path().join(BUNDLE_NAME))
    }

    #[test]
    fn the_release_shape_validates() {
        let stage = tempfile::tempdir().unwrap();
        let bundle = fixture::stage_bundle(stage.path(), BUNDLE_ID);
        assert_eq!(validate_archive(&fixture::archive(&bundle)), Ok(()));
    }

    #[test]
    fn a_bundle_of_another_identifier_is_refused() {
        let stage = tempfile::tempdir().unwrap();
        let bundle = fixture::stage_bundle(stage.path(), "dev.ducktape.other");
        assert_eq!(
            validate_archive(&fixture::archive(&bundle)),
            Err(Refusal::BundleShapeRefused)
        );
    }

    #[test]
    fn a_bundle_missing_or_carrying_an_extra_executable_is_refused() {
        for change in ["remove", "add"] {
            let stage = tempfile::tempdir().unwrap();
            let bundle = fixture::stage_bundle(stage.path(), BUNDLE_ID);
            let macos = bundle.join("Contents/MacOS");
            match change {
                "remove" => std::fs::remove_file(macos.join("ducktape-app")).unwrap(),
                "add" => std::fs::write(macos.join("helper"), b"x").unwrap(),
                other => unreachable!("{other}"),
            }
            assert_eq!(
                validate_archive(&fixture::archive(&bundle)),
                Err(Refusal::BundleShapeRefused),
                "{change}"
            );
        }
    }

    #[test]
    fn an_already_signed_shape_is_still_a_bundle_shape() {
        // `_CodeSignature` is a resource directory to this check: shape is
        // about identity and executables; rcodesign re-signs in place.
        let stage = tempfile::tempdir().unwrap();
        let bundle = fixture::stage_bundle(stage.path(), BUNDLE_ID);
        std::fs::create_dir(bundle.join("Contents/_CodeSignature")).unwrap();
        std::fs::write(
            bundle.join("Contents/_CodeSignature/CodeResources"),
            b"<plist/>",
        )
        .unwrap();
        assert_eq!(validate_archive(&fixture::archive(&bundle)), Ok(()));
    }

    #[test]
    fn every_escape_and_foreign_top_level_entry_is_refused() {
        let plist = Entry::File(fixture::info_plist(BUNDLE_ID).into_bytes());
        let cases: Vec<(&str, Vec<(&str, Entry)>)> = vec![
            (
                "second top-level",
                vec![
                    (
                        "Ducktape.app/Contents/Info.plist",
                        Entry::File(fixture::info_plist(BUNDLE_ID).into_bytes()),
                    ),
                    ("README", Entry::File(b"x".to_vec())),
                ],
            ),
            (
                "other top-level name",
                vec![("Other.app/Contents/Info.plist", plist)],
            ),
            (
                "dot-dot climb",
                vec![("Ducktape.app/../escape", Entry::File(b"x".to_vec()))],
            ),
            (
                "absolute",
                vec![(
                    "/Ducktape.app/Contents/Info.plist",
                    Entry::File(b"x".to_vec()),
                )],
            ),
            (
                "symlink escaping",
                vec![
                    ("Ducktape.app/Contents/MacOS/", Entry::Dir),
                    (
                        "Ducktape.app/Contents/MacOS/views",
                        Entry::Symlink("../../../etc"),
                    ),
                ],
            ),
            (
                "symlink absolute",
                vec![
                    ("Ducktape.app/Contents/MacOS/", Entry::Dir),
                    ("Ducktape.app/Contents/MacOS/views", Entry::Symlink("/etc")),
                ],
            ),
            (
                "hard link",
                vec![
                    ("Ducktape.app/Contents/MacOS/", Entry::Dir),
                    (
                        "Ducktape.app/Contents/MacOS/ducktape-app",
                        Entry::HardLink("/etc/passwd"),
                    ),
                ],
            ),
            ("not an archive", vec![]),
        ];
        for (name, entries) in cases {
            let archive = if name == "not an archive" {
                b"not zstd".to_vec()
            } else {
                fixture::archive_of(&entries)
            };
            assert_eq!(
                validate_archive(&archive),
                Err(Refusal::BundleShapeRefused),
                "{name}"
            );
        }
    }

    #[test]
    fn a_symlink_that_stays_inside_the_bundle_is_allowed() {
        assert!(
            normalized_inside_bundle(Path::new("Ducktape.app/Contents/MacOS/../Resources/views"))
                .is_some()
        );
        assert!(normalized_inside_bundle(Path::new("Ducktape.app/./Contents")).is_some());
        assert!(normalized_inside_bundle(Path::new("Ducktape.app")).is_some());
        assert!(normalized_inside_bundle(Path::new("Ducktape.app/..")).is_none());
        assert!(normalized_inside_bundle(Path::new("Ducktape.app2/x")).is_none());
        // an entry path itself gets no `..`, even one that would stay inside
        assert!(plain_bundle_path(Path::new("Ducktape.app/Contents/../Contents")).is_none());
        assert!(plain_bundle_path(Path::new("Ducktape.app/./Contents")).is_some());
    }

    #[test]
    fn the_transition_table_is_linear_and_every_refusal_is_terminal() {
        let steps = [
            (Stage::Received, Next::Run(Stage::Validated)),
            (Stage::Validated, Next::Run(Stage::Signed)),
            (Stage::Signed, Next::Run(Stage::Notarized)),
            (Stage::Notarized, Next::Run(Stage::Stapled)),
            (Stage::Stapled, Next::Done),
        ];
        for (stage, want) in steps {
            assert_eq!(advance(stage, StepOutcome::Advanced), want, "{stage:?}");
            let refusal = Refusal::CodesignFailed;
            assert_eq!(
                advance(stage, StepOutcome::Refused(refusal.clone())),
                Next::Refuse(refusal),
                "{stage:?}"
            );
        }
    }

    #[test]
    fn the_submission_id_is_read_off_the_notary_log_line() {
        let out = "creating Notary API submission for Ducktape.app (sha256: ab)\ncreated submission ID: 1c2d3e4f-0000-1111-2222-333344445555\nwaiting up to 600s\n";
        assert_eq!(
            submission_id(out).as_deref(),
            Some("1c2d3e4f-0000-1111-2222-333344445555")
        );
        assert_eq!(submission_id("nothing here"), None);
    }

    #[test]
    fn plist_strings_are_read_by_key() {
        let plist = fixture::info_plist(BUNDLE_ID);
        assert_eq!(
            plist_string(&plist, "CFBundleIdentifier").as_deref(),
            Some(BUNDLE_ID)
        );
        assert_eq!(
            plist_string(&plist, "CFBundleExecutable").as_deref(),
            Some("ducktape-launcher")
        );
        assert_eq!(plist_string(&plist, "Missing"), None);
        assert_eq!(
            plist_string("<key>A</key><true/><key>B</key><string>b</string>", "A"),
            None,
            "a non-string value is not read as the next key's string"
        );
    }

    fn tools(work_root: &Path, rcodesign: PathBuf, notary: Notary) -> Tools {
        Tools {
            rcodesign,
            entitlements: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/entitlements.plist"),
            work_root: work_root.to_path_buf(),
            notary,
        }
    }

    fn identity() -> AppleCodesign {
        use crate::codesign::fixture::{self as cred, Marker};
        AppleCodesign::admit(
            &cred::p12_b64(cred::TEAM_ID, Marker::DeveloperIdApplication),
            cred::P12_PASSWORD,
            &cred::api_key_json(),
            cred::TEAM_ID,
        )
        .unwrap()
    }

    #[test]
    fn an_absent_tool_is_refused_by_name_after_the_shape_check() {
        let root = tempfile::tempdir().unwrap();
        let bundle = fixture::stage_bundle(root.path(), BUNDLE_ID);
        let archive = fixture::archive(&bundle);
        let work = tempfile::tempdir().unwrap();
        let tools = tools(
            work.path(),
            PathBuf::from("/nonexistent/rcodesign"),
            Notary::Stub(StubNotary::Accepts),
        );
        assert_eq!(
            sign_bundle(&tools, &identity(), &archive).err(),
            Some(Refusal::ToolMissing)
        );
        // the shape check comes first, whatever the tools
        let other_root = tempfile::tempdir().unwrap();
        let other = fixture::stage_bundle(other_root.path(), "x.y");
        assert_eq!(
            sign_bundle(&tools, &identity(), &fixture::archive(&other)).err(),
            Some(Refusal::BundleShapeRefused)
        );
        assert_eq!(
            std::fs::read_dir(work.path()).unwrap().count(),
            0,
            "the work directory is gone on every exit path"
        );
    }

    #[test]
    fn the_stubbed_notary_answers_reach_the_caller_as_tokens() {
        let rcodesign = fixture::rcodesign();
        let root = tempfile::tempdir().unwrap();
        let bundle = fixture::stage_bundle(root.path(), BUNDLE_ID);
        let archive = fixture::archive(&bundle);
        let work = tempfile::tempdir().unwrap();
        let cases = [
            (
                StubNotary::Rejects,
                Err(Refusal::NotaryRejected {
                    submission_id: None,
                }),
            ),
            (StubNotary::StapleFails, Err(Refusal::StapleFailed)),
            (StubNotary::Accepts, Ok(())),
        ];
        for (answer, want) in cases {
            let tools = tools(work.path(), rcodesign.clone(), Notary::Stub(answer));
            let got = sign_bundle(&tools, &identity(), &archive).map(|_| ());
            assert_eq!(got, want, "{answer:?}");
        }
        assert_eq!(std::fs::read_dir(work.path()).unwrap().count(), 0);
    }
}
