//! the capability spec: a TOML file describing one executor capability.
//!
//! everything the provider used to hardcode in Rust — which binary to
//! probe, the argv to invoke it, how to parse its output — is data in a spec
//! file. the built-in executors are embedded spec FILES (globbed by build.rs
//! — no Rust source names an executor) parsed by this exact module, so an
//! operator adding another executor writes a TOML file, not Rust:
//!
//! ```toml
//! spec = 1
//! [capability]
//! tag = "ollama"
//! description = "local ollama daemon via its cli"
//! [detect]
//! bin = "ollama"
//! [invoke]
//! args = ["run", "llama4"]
//! prompt = "stdin"
//! [output]
//! format = "text"
//! ```
//!
//! dispatch is by EXPLICIT capability tag — nothing is ever inferred from a
//! model name. a job names the tag it needs ("ollama"); the spec's argv says,
//! literally and completely, what running that tag means on this host (which
//! binary, which flags, which model — all operator policy). a finer-grained
//! need ("this executor, but with these exact flags") is a finer tag with its own
//! spec, not a routing rule — `[[variants]]` (see [`crate::variants`]) is
//! load-time sugar for writing a family of such finer tags in one file, each
//! still one tag with one fixed argv. no executor name appears anywhere in
//! this crate's code or tests — executors exist only as spec data.
//!
//! ## trust model — read this before adding spec sources
//!
//! a spec names an arbitrary local binary and the argv to run it with: loading
//! a spec is EXECUTING CODE by proxy. specs are operator-trusted configuration
//! — the same trust class as a shell profile or systemd unit. they load from
//! exactly two places, both local and operator-controlled: the specs embedded
//! in this crate at compile time, and the workspace's own `capabilities/`
//! directory, handed to `discover` by the daemon serving it. specs are
//! NEVER fetched from the network, and nothing consensus-side may ever read
//! one (host-local files are non-deterministic input; the consensus capability
//! module sees only the announced TAGS, never the specs behind them).
//!
//! ## how an executor authenticates — two paths, never both
//!
//! a provider child needs the operator's model credential, and there are
//! exactly two ways a spec can arrange that:
//!
//! ```toml
//! [isolation]                     # the credential never enters the child. the
//! config_home_env = "CODEX_HOME"  # host reads it and the child gets an opaque
//! broker = "codex-responses"      # per-run bearer aimed at a loopback
//!                                 # endpoint. the fresh config home is what
//!                                 # FORCES that: without it the CLI would just
//!                                 # read ~/.codex.
//! ```
//!
//! There is no second path. A spec cannot ask for a host directory to be
//! mounted into the run: a run executes inside a microVM that shares no
//! filesystem with the host, so the only things that cross are the per-run
//! devices the backend builds. An executor that can only authenticate by
//! reading its own dotfiles cannot be lent by a node.
//!
//! ## override precedence
//!
//! embedded specs load first; operator specs load second and REPLACE an
//! embedded spec with the same tag wholesale (no field merging — a spec is
//! the unit of override). duplicate tags WITHIN the operator dir are a hard
//! error: two files claiming one tag is operator confusion, not a precedence
//! question.

use std::collections::BTreeMap;
use std::path::Path;

use capability::validate_tag;
use serde::Deserialize;

use crate::variants::{self, RawVariant};

/// the one spec format version this build understands. parsing rejects any
/// other value loudly — an operator on a newer format gets "unsupported spec
/// version", never silently misread fields.
const SPEC_VERSION: u64 = 1;

/// bounds for `[invoke].timeout_secs` — a zero timeout would kill every job
/// at spawn; anything over an hour is a hang, not a job.
const TIMEOUT_RANGE: std::ops::RangeInclusive<u64> = 1..=3600;

/// bounds for `[invoke].hard_timeout_factor` — a zero factor would kill every
/// child at spawn; past 1000 idle windows the ceiling stops bounding anything.
const HARD_TIMEOUT_FACTOR_RANGE: std::ops::RangeInclusive<u32> = 1..=1000;

/// a validated capability spec — the parsed, checked form of one TOML file.
/// construction goes through [`CapabilitySpec::parse`] only, so holding one
/// is proof the invariants documented on each field hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilitySpec {
    /// the registry tag announced network-wide. `[a-z0-9._-]`, non-empty,
    /// ≤ 64 bytes — exactly the consensus capability module's rules.
    pub tag: String,
    /// human-facing one-liner for docs and status surfaces.
    pub description: String,
    /// the binary name probed on `PATH`.
    pub bin: String,
    /// executable files the binary resolves beside itself at runtime. Each is
    /// a plain sibling name; discovery requires every one, and sandbox backends
    /// mount only these declared files beside the primary executable.
    pub companions: Vec<String>,
    /// optional env var naming an explicit binary path. an override that
    /// points at nothing is a loud warning + absent capability, never a
    /// silent fallback to the `PATH` probe.
    pub env: Option<String>,
    /// argv (after the binary), passed verbatim to exec — fully literal, no
    /// placeholders, NEVER shell-interpreted: no quoting, no expansion, no
    /// injection surface. which model (if any) the executor runs is encoded
    /// here as ordinary flags — operator policy, invisible to consensus.
    pub args: Vec<String>,
    /// per-job wall-clock budget; the child is killed at the deadline.
    /// `DUCKTAPE_PROVIDER_TIMEOUT_SECS` overrides ALL specs at once.
    pub timeout_secs: u64,
    /// the hard ceiling on one child's lifetime, as a multiple of its idle
    /// timeout: `timeout_secs` bounds SILENCE (any output refreshes it — long
    /// agentic runs that keep streaming are never killed mid-work), and
    /// `idle × this` bounds even a continuously-chatty child, guarding the
    /// host's own resources. the RUN's committed outcome is bounded by the
    /// saga's consensus deadline regardless — this factor only decides how
    /// long this host keeps paying for one child.
    pub hard_timeout_factor: u32,
    /// which named stdout parser extracts the assistant's final text.
    pub output: OutputFormat,
    /// optional `[isolation]` — the auth path: a host-owned broker
    /// holding the credential, and the fresh executor config home that forces
    /// the CLI through it (see [`IsolationSpec`]).
    pub isolation: IsolationSpec,
    /// optional `[context]` — where THIS CLI auto-loads ambient instructions
    /// from, so the run's assembled context document (the agent's soul) can be
    /// delivered by the executor's own convention (see [`ContextLocation`]).
    /// absent = the doc rides the stdin prompt instead; a raw provider needs no
    /// convention.
    pub context: Option<ContextLocation>,
    /// optional `[interactive]` — the argv for an INTERACTIVE (pty-backed) TUI
    /// session, as opposed to the headless `[invoke]` argv. absent means this
    /// executor cannot be driven interactively (the historical posture); present
    /// makes it eligible for [`crate::interactive`]. the broker/config-home
    /// isolation is shared with the headless path — only the argv differs.
    pub interactive: Option<InteractiveSpec>,
    /// optional `[source]` — the vendor release channel this executor's Linux
    /// build comes from, which is what makes it installable by `ducktape
    /// agent install` (see [`ReleaseSource`]). absent = the operator puts the
    /// binary in the executors directory themselves.
    pub source: Option<ReleaseSource>,
}

/// Where an executor's releases come from: the vendor channel `agent install`
/// resolves the LATEST release from at install time. No version and no hash
/// is ever written down ahead of time — the channel names the release and the
/// vendor publishes the checksum for it, so what gets installed is whatever
/// is current the moment the operator approves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseSource {
    /// Anthropic's release feed: `<base>/latest` names the version,
    /// `<base>/<version>/manifest.json` carries a sha256 per platform, and
    /// `<base>/<version>/<platform>/<detect.bin>` is the executable itself.
    /// A single-binary feed, so the spec declares no companions.
    ClaudeReleases { base: String },
    /// A GitHub release: `releases/latest` names the tag, `asset` (with
    /// `{arch}` filled in as the Rust triple's arch) is a gzipped tar the
    /// release publishes a `sums` file beside, and `members` are the archive
    /// paths of `detect.bin` and its companions, in any order.
    GithubRelease {
        repo: String,
        asset: String,
        sums: String,
        members: Vec<String>,
    },
    /// A self-contained release bundle whose sibling assets must remain beside
    /// the executable. `{arch}` uses x64/arm64. `root` is the archive directory
    /// containing `detect.bin`; the installer preserves that whole directory.
    GithubBundle {
        repo: String,
        asset: String,
        sums: String,
        root: String,
    },
}

/// the argv for an interactive, pty-backed TUI session. deliberately its own
/// section, not a flag on `[invoke]`: the headless argv (`codex exec --json …`)
/// is the wrong shape for a human-driven TUI, and mixing them invites a spec
/// that half-declares one. an empty `args` is legal and meaningful — `codex`
/// with no subcommand launches its TUI; the broker's `-c` overrides are spliced
/// in host-side (see [`crate::interactive`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractiveSpec {
    pub args: Vec<String>,
    /// argv for a RESTRICTED interactive session (a SHARED, command-lane
    /// session): read-only + non-prompting, because no single member can answer
    /// a TUI approval prompt when the session is command-driven, so the CLI must
    /// never stop to ask AND must be unable to do damage while not asking (codex
    /// `--sandbox read-only --ask-for-approval never`, claude
    /// `--permission-mode plan`). `None` = this capability supports only the
    /// unrestricted (solo) interactive mode; a restricted spawn is then refused
    /// rather than silently running unrestricted.
    pub restricted_args: Option<Vec<String>>,
}

/// where an executor auto-loads its ambient instructions from — a CLOSED set of
/// location KINDS, like [`OutputFormat`], never a raw path:
/// a spec that could name any path could aim a host-written file anywhere on the
/// box, and there would be nothing left to validate. the file component is a
/// plain name; the DIRECTORY is the host's to choose, per kind.
///
/// both kinds sit OUTSIDE the commit scan, which is what makes the delivery safe:
/// the config home lives under the reserved run-runtime dir the provisioner
/// deletes before scanning, and the workspace parent is beside the checkout,
/// which `commit` only scans UNDER. so the soul never lands in an agent's
/// snapshot or PR — and it never overwrites a repository's own instructions
/// file, which stays inside the checkout and layers on top of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextLocation {
    /// `"config-home:<file>"` → `<the run's fresh config home>/<file>`. requires
    /// the spec to also declare `[isolation] config_home_env` (a hard load error
    /// otherwise): without it there IS no fresh config home, so the path would
    /// name a directory that never exists and the CLI would run unsouled.
    ConfigHome(String),
    /// `"workspace-parent:<file>"` → `<parent of the run's checkout>/<file>`.
    /// the parent, not the checkout: a CLI that merges parent-directory
    /// instructions with project ones then layers the repo's own file ON TOP of
    /// the soul instead of having it overwritten.
    WorkspaceParent(String),
}

/// how an executor authenticates WITHOUT its credential entering the child.
/// isolation is data-described, but credentials never are: `broker` names
/// host-owned Rust, not a URL or a command a spec could point anywhere.
///
/// the two fields are one mechanism, not two features. `broker` holds the
/// operator's credential in THIS process and hands the child only an opaque
/// per-run bearer; `config_home_env` names the executor's config-home variable
/// (e.g. `CODEX_HOME`) so the child gets a FRESH, empty one — which is what
/// stops the CLI reading `~/.codex/auth.json` and forces it through the broker.
/// a broker without the fresh config home would be decorative.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IsolationSpec {
    /// the executor's config-home env var (e.g. `CODEX_HOME`). the child gets a
    /// fresh run-local directory there instead of the operator's.
    pub config_home_env: Option<String>,
    /// the host-owned credential broker this executor speaks to, if any.
    pub broker: Option<BrokerKind>,
}

/// the brokers this build knows how to BE. a CLOSED set, like [`OutputFormat`]:
/// each name is host code that holds a real credential, so adding one is a code
/// change with tests — never a string an operator can invent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerKind {
    /// the OpenAI Responses API, as `codex exec` speaks it (see [`crate::broker`]).
    CodexResponses,
    /// the Anthropic Messages API (`POST /v1/messages`, SSE), as Claude Code
    /// speaks it. the child is aimed by ENV (`ANTHROPIC_BASE_URL`) + a
    /// `claudeAiOauth` creds file seeded into the config home, not argv (see
    /// [`crate::broker`]).
    AnthropicMessages,
    /// Pi speaks either existing model API, selected by the borrowed credential.
    /// Its run-local models/auth configuration is distinct from either vendor CLI.
    Pi,
}

/// the named stdout parsers. a CLOSED set on purpose: each name is a tested
/// parser for a real CLI's output contract, not a config-described guess.
/// adding a name is a code change with tests — that is the point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Pi's authoritative `message_end` events, including per-turn usage.
    PiJson,
    /// a JSONL event stream; the LAST `agent_message` item wins.
    JsonlEvents,
    /// a single `{"type":"result",...}` object (the contract of
    /// `--output-format json`-style print modes).
    JsonResult,
    /// raw stdout, trimmed. the generic escape hatch: any CLI that prints
    /// the answer plainly is wireable with this and zero code.
    Text,
}

// ---- raw (serde) shapes ------------------------------------------------------
// the on-disk TOML shape, deliberately separate from CapabilitySpec: serde
// gets a dumb mirror of the file, validation turns it into the checked type.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSpec {
    spec: u64,
    capability: RawCapability,
    detect: RawDetect,
    invoke: RawInvoke,
    output: RawOutput,
    /// optional `[isolation]` — the host-owned credential broker + fresh
    /// executor config home.
    #[serde(default)]
    isolation: Option<RawIsolation>,
    /// optional `[context]` — the executor's native ambient-instructions file.
    #[serde(default)]
    context: Option<RawContext>,
    /// optional `[interactive]` — the pty-backed TUI argv.
    #[serde(default)]
    interactive: Option<RawInteractive>,
    /// optional `[tools]` — argv injected into EVERY argv this file produces
    /// (see [`inject_tool_args`]).
    #[serde(default)]
    tools: Option<RawTools>,
    /// optional `[source]` — the vendor release channel, validated in
    /// [`parse_source`].
    #[serde(default)]
    source: Option<RawSource>,
    /// optional `[[variants]]` — finer tags expanded at load time, validated
    /// in [`crate::variants`].
    #[serde(default)]
    variants: Vec<RawVariant>,
}

/// the on-disk `[isolation]` shape — a dumb serde mirror; unknown fields fail
/// loud like everywhere else in the spec format.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawIsolation {
    #[serde(default)]
    config_home_env: Option<String>,
    #[serde(default)]
    broker: Option<String>,
}

/// the on-disk `[source]` shape: `kind` picks the channel and the rest of the
/// table is that kind's fields. a field from the other kind is an unknown
/// field and fails loud like everywhere else in the format.
#[derive(Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum RawSource {
    #[serde(rename = "claude-releases")]
    ClaudeReleases { base: String },
    #[serde(rename = "github-release")]
    GithubRelease {
        repo: String,
        asset: String,
        sums: String,
        members: Vec<String>,
    },
    #[serde(rename = "github-bundle")]
    GithubBundle {
        repo: String,
        asset: String,
        sums: String,
        root: String,
    },
}

fn validate_github_source(repo: &str, asset: &str, sums: &str, origin: &str) -> Result<(), String> {
    let is_owner_slash_name = matches!(
        repo.split('/').collect::<Vec<_>>().as_slice(),
        [owner, name] if !owner.is_empty() && !name.is_empty()
    );
    if !is_owner_slash_name {
        return Err(format!("{origin}: source.repo {repo:?} is not owner/name"));
    }
    if !asset.contains("{arch}") {
        return Err(format!(
            "{origin}: source.asset {asset:?} names no {{arch}} placeholder"
        ));
    }
    if sums.is_empty() {
        return Err(format!("{origin}: source.sums must be non-empty"));
    }
    Ok(())
}

/// validate `[source]` against `[detect]`: the channel must deliver exactly
/// the files discovery will look for — the binary and every companion, no
/// more and no fewer — or the install verb would write a set the guest cannot
/// run, or leave a declared companion missing.
fn parse_source(
    raw: Option<RawSource>,
    bin: &str,
    companions: &[String],
    origin: &str,
) -> Result<Option<ReleaseSource>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    match raw {
        RawSource::ClaudeReleases { base } => {
            if !base.starts_with("https://") {
                return Err(format!("{origin}: source.base must be an https url"));
            }
            if !companions.is_empty() {
                return Err(format!(
                    "{origin}: a claude-releases source delivers one binary, but \
                     detect.companions declares {companions:?}"
                ));
            }
            Ok(Some(ReleaseSource::ClaudeReleases {
                base: base.trim_end_matches('/').to_string(),
            }))
        }
        RawSource::GithubRelease {
            repo,
            asset,
            sums,
            members,
        } => {
            validate_github_source(&repo, &asset, &sums, origin)?;
            let delivered: std::collections::BTreeSet<&str> = members
                .iter()
                .map(|m| m.rsplit('/').next().unwrap_or(m.as_str()))
                .collect();
            let declared: std::collections::BTreeSet<&str> = std::iter::once(bin)
                .chain(companions.iter().map(String::as_str))
                .collect();
            let delivers_exactly_the_declared_files =
                delivered == declared && delivered.len() == members.len();
            if !delivers_exactly_the_declared_files {
                return Err(format!(
                    "{origin}: source.members {members:?} must deliver exactly detect.bin \
                     and detect.companions {declared:?}"
                ));
            }
            Ok(Some(ReleaseSource::GithubRelease {
                repo,
                asset,
                sums,
                members,
            }))
        }
        RawSource::GithubBundle {
            repo,
            asset,
            sums,
            root,
        } => {
            validate_github_source(&repo, &asset, &sums, origin)?;
            let safe_root = !root.is_empty()
                && root != "."
                && root != ".."
                && root
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
            if !safe_root {
                return Err(format!(
                    "{origin}: source.root must be one safe directory name"
                ));
            }
            if !companions.is_empty() {
                return Err(format!(
                    "{origin}: a github-bundle preserves its assets; detect.companions must be empty"
                ));
            }
            Ok(Some(ReleaseSource::GithubBundle {
                repo,
                asset,
                sums,
                root,
            }))
        }
    }
}

/// an env var name the child will actually see: `[A-Z_][A-Z0-9_]*`. a spec that
/// misspells `CODEX-HOME` would otherwise silently set nothing and the CLI would
/// quietly fall back to reading the operator's real config home — the exact leak
/// the fresh config home exists to prevent.
fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_uppercase() || (i > 0 && b.is_ascii_digit()))
}

/// validate `[isolation]`: the config-home env name is a real env name, and the
/// broker is one this build actually implements (a broker is host CODE, not a
/// URL — an unknown name must never degrade to "no broker", which would silently
/// hand the child the operator's credential instead).
fn parse_isolation(raw: Option<RawIsolation>, origin: &str) -> Result<IsolationSpec, String> {
    let raw = raw.unwrap_or_default();
    if let Some(name) = &raw.config_home_env
        && !valid_env_name(name)
    {
        return Err(format!(
            "{origin}: isolation.config_home_env {name:?} must match [A-Z_][A-Z0-9_]*"
        ));
    }
    let broker = raw
        .broker
        .map(|broker| match broker.as_str() {
            "codex-responses" => Ok(BrokerKind::CodexResponses),
            "anthropic-messages" => Ok(BrokerKind::AnthropicMessages),
            "pi" => Ok(BrokerKind::Pi),
            other => Err(format!(
                "{origin}: isolation.broker {other:?} is unsupported \
                 (want codex-responses | anthropic-messages | pi)"
            )),
        })
        .transpose()?;
    Ok(IsolationSpec {
        config_home_env: raw.config_home_env,
        broker,
    })
}

/// the on-disk `[context]` shape — a dumb serde mirror; unknown fields fail
/// loud like everywhere else in the spec format.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawContext {
    path: String,
}

/// validate `[context] path` into the closed [`ContextLocation`] set. the string
/// is `<kind>:<file>` and BOTH halves are checked: an unknown kind is a load
/// error (never a silently undelivered soul), and the file half must be a plain
/// name — a `/` or a `..` would let a spec steer a host-written file out of the
/// directory the kind picked, which is the whole traversal surface this closed
/// set exists to not have.
///
/// `config-home:` additionally REQUIRES `[isolation] config_home_env`: the kind
/// resolves against the fresh config home, and a spec that never asks for one
/// names a directory that will not exist. failing at LOAD (not at the first run)
/// is the point — an unsouled agent is a silently different agent.
fn parse_context(
    raw: Option<RawContext>,
    isolation: &IsolationSpec,
    origin: &str,
) -> Result<Option<ContextLocation>, String> {
    let Some(raw) = raw else { return Ok(None) };
    let Some((kind, file)) = raw.path.split_once(':') else {
        return Err(format!(
            "{origin}: context.path {:?} must be \"<kind>:<file>\" \
             (want config-home:<file> | workspace-parent:<file>)",
            raw.path
        ));
    };
    if file.is_empty() || file.contains('/') || file == "." || file == ".." {
        return Err(format!(
            "{origin}: context.path {:?}: the file component must be a plain file \
             name (no \"/\", no \"..\") — the directory is the kind's to choose",
            raw.path
        ));
    }
    match kind {
        "config-home" if isolation.config_home_env.is_none() => Err(format!(
            "{origin}: context.path {:?} resolves against the run's fresh config \
             home, but this spec declares no [isolation] config_home_env — there \
             would be no such directory and the executor would run with no context",
            raw.path
        )),
        "config-home" => Ok(Some(ContextLocation::ConfigHome(file.to_string()))),
        "workspace-parent" => Ok(Some(ContextLocation::WorkspaceParent(file.to_string()))),
        other => Err(format!(
            "{origin}: context.path kind {other:?} is not a known location \
             (want config-home | workspace-parent)"
        )),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCapability {
    tag: String,
    #[serde(default)]
    description: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDetect {
    bin: String,
    #[serde(default)]
    companions: Vec<String>,
    #[serde(default)]
    env: Option<String>,
}

/// A companion is resolved relative to the primary executable's REAL parent.
/// Keeping this to a plain file name makes `[detect]` describe one install
/// bundle without turning a trusted spec into an arbitrary extra host mount.
fn validate_companions(names: &[String], bin: &str, origin: &str) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for name in names {
        let is_plain_name =
            !name.is_empty() && Path::new(name).file_name() == Some(std::ffi::OsStr::new(name));
        if !is_plain_name || matches!(name.as_str(), "." | "..") {
            return Err(format!(
                "{origin}: detect.companions entry {name:?} must be a plain file name"
            ));
        }
        let repeats_primary = Path::new(bin).file_name() == Some(std::ffi::OsStr::new(name));
        if repeats_primary {
            return Err(format!(
                "{origin}: detect.companions repeats the primary binary {name:?}"
            ));
        }
        if !seen.insert(name) {
            return Err(format!(
                "{origin}: duplicate detect.companions entry {name:?}"
            ));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInvoke {
    #[serde(default)]
    args: Vec<String>,
    prompt: String,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
    #[serde(default = "default_hard_timeout_factor")]
    hard_timeout_factor: u32,
}

fn default_timeout() -> u64 {
    300
}

fn default_hard_timeout_factor() -> u32 {
    36
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOutput {
    format: String,
}

/// the on-disk `[tools]` shape. `args` is required: a `[tools]` section that
/// injects nothing is operator confusion, and the format fails loud rather
/// than quietly doing nothing.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTools {
    args: Vec<String>,
}

/// the on-disk `[interactive]` shape — a dumb serde mirror. `args` defaults to
/// empty (a bare TUI launch); unknown fields fail loud like the rest.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInteractive {
    #[serde(default)]
    args: Vec<String>,
    /// argv for the restricted (shared, command-lane) mode; absent = solo-only.
    #[serde(default)]
    restricted_args: Option<Vec<String>>,
}

/// splice `[tools].args` into every argv this spec can produce, ONCE, at load
/// time — so nothing downstream (discovery, invoke, resume) knows tools exist:
/// a spec in hand already has them.
///
/// the insertion point is IMMEDIATELY AFTER `args[0]`, never the end: an argv
/// like codex's must keep its trailing bare `-` (the stdin marker) LAST, so
/// appending is not an option. `args[0]` is always the mode/subcommand
/// selector (`exec`, `-p`), so right after it is the one position that is
/// simultaneously legal for every executor and stable across variants. an argv
/// with fewer than 1 arg has no such position and is left alone.
fn inject_tool_args(spec: &mut CapabilitySpec, tools: &[String]) {
    let splice = |argv: &mut Vec<String>| {
        if !argv.is_empty() {
            argv.splice(1..1, tools.iter().cloned());
        }
    };
    if tools.is_empty() {
        return;
    }
    splice(&mut spec.args);
}

impl CapabilitySpec {
    /// parse and validate one SINGLE-tag spec's TOML — the convenience for
    /// callers holding a spec that declares no `[[variants]]` (a file that
    /// does is a hard error here, never a silent drop of its variants; load
    /// whole files through [`CapabilitySpec::parse_all`]).
    pub fn parse(toml_text: &str, origin: &str) -> Result<Self, String> {
        let (mut base, variants, tools) = Self::parse_raw(toml_text, origin)?;
        if !variants.is_empty() {
            return Err(format!(
                "{origin}: spec declares [[variants]] and expands to multiple \
                 tags; load it via parse_all"
            ));
        }
        inject_tool_args(&mut base, &tools);
        Ok(base)
    }

    /// parse and validate one spec FILE's TOML into every spec it defines:
    /// the base spec first, then its `[[variants]]` expansions in declaration
    /// order. this is the loaders' entry point — one file, 1+ tags.
    ///
    /// `[tools]` injection runs LAST, over the expanded set, so a file's tool
    /// args reach the base tag and every variant tag alike — variants inherit
    /// them the same way they inherit the rest of the file, without
    /// [`crate::variants`] knowing tools exist.
    pub fn parse_all(toml_text: &str, origin: &str) -> Result<Vec<Self>, String> {
        let (base, raw_variants, tools) = Self::parse_raw(toml_text, origin)?;
        let mut specs = variants::expand(&base, &raw_variants, origin)?;
        specs.insert(0, base);
        for spec in &mut specs {
            inject_tool_args(spec, &tools);
        }
        Ok(specs)
    }

    /// the shared parse core: the validated base spec, its still-raw variant
    /// entries, and the file's `[tools]` args (empty when the section is
    /// absent) — the base is returned UNINJECTED, because expansion copies it
    /// into the variants and injecting twice would double the tool flags.
    /// `origin` names the source (a file path or "embedded:<file>") so every
    /// error says WHICH spec is broken. unknown fields are rejected
    /// (`deny_unknown_fields`): a typo like `patern` fails loud instead of
    /// silently changing routing.
    fn parse_raw(
        toml_text: &str,
        origin: &str,
    ) -> Result<(Self, Vec<RawVariant>, Vec<String>), String> {
        let raw: RawSpec =
            toml::from_str(toml_text).map_err(|e| format!("{origin}: not a valid spec: {e}"))?;
        if raw.spec != SPEC_VERSION {
            return Err(format!(
                "{origin}: unsupported spec version {} (this build understands {SPEC_VERSION})",
                raw.spec
            ));
        }
        let tag = raw.capability.tag;
        // THE consensus tag rule (shared, not mirrored): a tag that validates
        // here can never bounce off the capability registry's Announce.
        validate_tag(&tag).map_err(|e| format!("{origin}: {e}"))?;
        if raw.detect.bin.is_empty() {
            return Err(format!("{origin}: detect.bin must be non-empty"));
        }
        validate_companions(&raw.detect.companions, &raw.detect.bin, origin)?;
        // the prompt always reaches the child via stdin (an argv placeholder
        // would leak prompts into `ps` output and hit ARG_MAX); the field is
        // validated but carries no choice.
        if raw.invoke.prompt != "stdin" {
            return Err(format!(
                "{origin}: invoke.prompt {:?} is not supported (only \"stdin\")",
                raw.invoke.prompt
            ));
        }
        if !TIMEOUT_RANGE.contains(&raw.invoke.timeout_secs) {
            return Err(format!(
                "{origin}: invoke.timeout_secs must be within {TIMEOUT_RANGE:?}, got {}",
                raw.invoke.timeout_secs
            ));
        }
        if !HARD_TIMEOUT_FACTOR_RANGE.contains(&raw.invoke.hard_timeout_factor) {
            return Err(format!(
                "{origin}: invoke.hard_timeout_factor must be within \
                 {HARD_TIMEOUT_FACTOR_RANGE:?}, got {}",
                raw.invoke.hard_timeout_factor
            ));
        }
        let output = match raw.output.format.as_str() {
            "pi-json" => OutputFormat::PiJson,
            "jsonl-events" => OutputFormat::JsonlEvents,
            "json-result" => OutputFormat::JsonResult,
            "text" => OutputFormat::Text,
            other => {
                return Err(format!(
                    "{origin}: output.format {other:?} is not a known parser \
                     (want pi-json | jsonl-events | json-result | text)"
                ));
            }
        };
        let isolation = parse_isolation(raw.isolation, origin)?;
        // AFTER isolation: `config-home:` is only meaningful when the spec asked
        // for a fresh config home, so the check needs the parsed block.
        let context = parse_context(raw.context, &isolation, origin)?;
        let source = parse_source(raw.source, &raw.detect.bin, &raw.detect.companions, origin)?;
        Ok((
            Self {
                tag,
                description: raw.capability.description,
                bin: raw.detect.bin,
                companions: raw.detect.companions,
                env: raw.detect.env,
                args: raw.invoke.args,
                timeout_secs: raw.invoke.timeout_secs,
                hard_timeout_factor: raw.invoke.hard_timeout_factor,
                output,
                isolation,
                context,
                interactive: raw.interactive.map(|i| InteractiveSpec {
                    args: i.args,
                    restricted_args: i.restricted_args,
                }),
                source,
            },
            raw.variants,
            raw.tools.map(|t| t.args).unwrap_or_default(),
        ))
    }
}

// ---- the loaded spec set -----------------------------------------------------

/// every spec this host loaded — embedded built-ins plus the operator dir —
/// keyed by tag, override already applied. the LOADED set is wider than the
/// DISCOVERED one (a spec whose binary is absent still loads), so a request
/// for an uninstalled capability errors by name instead of shrugging.
#[derive(Debug, Clone)]
pub struct SpecSet {
    specs: BTreeMap<String, CapabilitySpec>,
}

/// the built-in specs compiled into this binary — every `specs/*.toml`,
/// globbed and embedded by build.rs, so no Rust source names an executor.
/// parsed at runtime through the same [`CapabilitySpec::parse_all`] path as
/// operator files (one file may expand into a base tag plus `[[variants]]`);
/// a unit test asserts validity so a broken embedded spec fails CI, not a
/// node boot — the `expect` here is unreachable past that test.
pub fn builtin_specs() -> Vec<CapabilitySpec> {
    include!(concat!(env!("OUT_DIR"), "/builtin_specs.rs"))
        .into_iter()
        .flat_map(|(origin, text): (&str, &str)| {
            CapabilitySpec::parse_all(text, origin).expect("embedded specs are CI-validated")
        })
        .collect()
}

impl SpecSet {
    /// compose the host's spec set: built-ins, then the operator dir (if it
    /// exists), same-tag operator specs replacing built-ins wholesale.
    pub fn load(operator_dir: Option<&Path>) -> Result<Self, String> {
        let mut specs: BTreeMap<String, CapabilitySpec> = builtin_specs()
            .into_iter()
            .map(|s| (s.tag.clone(), s))
            .collect();
        if let Some(dir) = operator_dir {
            for spec in load_dir(dir)? {
                // operator overrides embedded silently by design — replacing a
                // built-in is the documented way to retune its flags.
                specs.insert(spec.tag.clone(), spec);
            }
        }
        Ok(Self { specs })
    }

    /// a set from explicit specs — the test seam, no filesystem involved.
    pub fn from_specs(list: Vec<CapabilitySpec>) -> Self {
        Self {
            specs: list.into_iter().map(|s| (s.tag.clone(), s)).collect(),
        }
    }

    pub fn get(&self, tag: &str) -> Option<&CapabilitySpec> {
        self.specs.get(tag)
    }

    pub fn iter(&self) -> impl Iterator<Item = &CapabilitySpec> {
        self.specs.values()
    }
}

/// load and validate every `*.toml` in `dir`, sorted by file name for stable
/// error ordering. duplicate tags in one dir are a hard error — a file's
/// `[[variants]]` expansions count like any other tag, so a variant of one
/// file colliding with another file's tag is caught the same way.
fn load_dir(dir: &Path) -> Result<Vec<CapabilitySpec>, String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("capability spec dir {}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    entries.sort();
    let mut seen: BTreeMap<String, std::path::PathBuf> = BTreeMap::new();
    let mut specs = Vec::new();
    for path in entries {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("capability spec {}: {e}", path.display()))?;
        for spec in CapabilitySpec::parse_all(&text, &path.display().to_string())? {
            if let Some(prev) = seen.insert(spec.tag.clone(), path.clone()) {
                return Err(format!(
                    "duplicate capability tag {:?}: {} and {}",
                    spec.tag,
                    prev.display(),
                    path.display()
                ));
            }
            specs.push(spec);
        }
    }
    Ok(specs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_toml(tag: &str) -> String {
        format!(
            r#"
spec = 1
[capability]
tag = "{tag}"
[detect]
bin = "{tag}-cli"
[invoke]
args = ["run"]
prompt = "stdin"
[output]
format = "text"
"#
        )
    }

    #[test]
    fn embedded_specs_parse_with_valid_unique_tags() {
        // the CI gate for the expect() in builtin_specs(): a broken embedded
        // spec fails HERE, never at node boot. deliberately tag-agnostic —
        // WHICH executors ship as built-ins is data, and no test hardcodes
        // executor names.
        let specs = builtin_specs();
        assert!(!specs.is_empty(), "the crate ships built-in specs");
        let tags: std::collections::BTreeSet<&str> = specs.iter().map(|s| s.tag.as_str()).collect();
        assert_eq!(tags.len(), specs.len(), "embedded tags are unique");
    }

    /// `[source]` is validated against `[detect]`: a GitHub release must
    /// deliver exactly the binary and its companions, a single-binary feed
    /// cannot carry companions, and a field from the other kind — or a kind
    /// this build does not know — fails loud like any unknown field.
    #[test]
    fn a_source_delivers_exactly_what_detect_declares() {
        let github = spec_toml("pkg").replacen(
            "bin = \"pkg-cli\"",
            "bin = \"pkg-cli\"\ncompanions = [\"pkg-helper\"]",
            1,
        ) + r#"
[source]
kind = "github-release"
repo = "vendor/pkg"
asset = "pkg-{arch}-unknown-linux-musl.tar.gz"
sums = "pkg_SHA256SUMS"
members = ["bin/pkg-helper", "bin/pkg-cli"]
"#;
        let spec = CapabilitySpec::parse(&github, "t").unwrap();
        assert_eq!(
            spec.source,
            Some(ReleaseSource::GithubRelease {
                repo: "vendor/pkg".into(),
                asset: "pkg-{arch}-unknown-linux-musl.tar.gz".into(),
                sums: "pkg_SHA256SUMS".into(),
                members: vec!["bin/pkg-helper".into(), "bin/pkg-cli".into()],
            })
        );

        let feed = spec_toml("solo")
            + r#"
[source]
kind = "claude-releases"
base = "https://releases.example/feed/"
"#;
        let spec = CapabilitySpec::parse(&feed, "t").unwrap();
        assert_eq!(
            spec.source,
            Some(ReleaseSource::ClaudeReleases {
                base: "https://releases.example/feed".into()
            })
        );
        assert_eq!(
            CapabilitySpec::parse(&spec_toml("none"), "t")
                .unwrap()
                .source,
            None
        );

        let refused = [
            (
                "members missing the companion",
                github.replacen("\"bin/pkg-helper\", ", "", 1),
                "must deliver exactly",
            ),
            (
                "members naming an extra file",
                github.replacen("\"bin/pkg-cli\"]", "\"bin/pkg-cli\", \"bin/extra\"]", 1),
                "must deliver exactly",
            ),
            (
                "an asset with no arch placeholder",
                github.replacen("{arch}", "x86_64", 1),
                "{arch} placeholder",
            ),
            (
                "a repo that is not owner/name",
                github.replacen("vendor/pkg", "pkg", 1),
                "owner/name",
            ),
            (
                "a feed field on a github source",
                github.replacen("sums = ", "base = \"https://x\"\nsums = ", 1),
                "unknown field",
            ),
            (
                "a kind this build does not know",
                github.replacen("github-release", "ftp-mirror", 1),
                "unknown variant",
            ),
            (
                "a feed with companions",
                feed.replacen(
                    "bin = \"solo-cli\"",
                    "bin = \"solo-cli\"\ncompanions = [\"solo-helper\"]",
                    1,
                ),
                "delivers one binary",
            ),
            (
                "a feed off https",
                feed.replacen("https://", "http://", 1),
                "https url",
            ),
        ];
        for (case, toml, expected) in refused {
            let err = CapabilitySpec::parse(&toml, "t").unwrap_err();
            assert!(err.contains(expected), "{case}: {err}");
        }
    }

    #[test]
    fn pi_spec_declares_both_run_modes_and_isolated_auth() {
        let specs = builtin_specs();
        let pi = specs.iter().find(|spec| spec.tag == "pi").unwrap();
        assert_eq!(pi.output, OutputFormat::PiJson);
        assert_eq!(pi.isolation.broker, Some(BrokerKind::Pi));
        assert_eq!(
            pi.isolation.config_home_env.as_deref(),
            Some("PI_CODING_AGENT_DIR")
        );
        assert!(matches!(
            pi.source,
            Some(ReleaseSource::GithubBundle { .. })
        ));
        let interactive = pi.interactive.as_ref().unwrap();
        for args in [&pi.args, &interactive.args] {
            assert!(args.iter().any(|arg| arg == "--no-extensions"));
            assert!(args.iter().any(|arg| arg == "--offline"));
        }
        assert!(
            interactive.restricted_args.is_none(),
            "Pi's interactive !shell bypasses model-tool allowlists; shared sessions must be refused"
        );
    }

    #[test]
    fn bundle_source_requires_one_safe_root_and_no_flattened_companions() {
        let bundle = spec_toml("bundle")
            + r#"
    [source]
    kind = "github-bundle"
    repo = "vendor/pkg"
    asset = "pkg-{arch}.tar.gz"
    sums = "SHA256SUMS"
    root = "pkg"
    "#;
        assert!(CapabilitySpec::parse(&bundle, "t").is_ok());
        for root in ["", ".", "..", "../pkg", "/pkg", "pkg/sub"] {
            let invalid = bundle.replace("root = \"pkg\"", &format!("root = {root:?}"));
            assert!(
                CapabilitySpec::parse(&invalid, "t")
                    .unwrap_err()
                    .contains("safe directory")
            );
        }
        let companions = bundle.replace(
            "bin = \"bundle-cli\"",
            "bin = \"bundle-cli\"\ncompanions = [\"helper\"]",
        );
        assert!(
            CapabilitySpec::parse(&companions, "t")
                .unwrap_err()
                .contains("companions must be empty")
        );
    }

    #[test]
    fn detect_companions_are_plain_sibling_names() {
        let with_companion = spec_toml("bundle").replacen(
            "bin = \"bundle-cli\"",
            "bin = \"bundle-cli\"\ncompanions = [\"bundle-helper\"]",
            1,
        );
        let spec = CapabilitySpec::parse(&with_companion, "t").unwrap();
        assert_eq!(spec.companions, ["bundle-helper"]);

        for bad in ["../helper", "subdir/helper", ".", ""] {
            let invalid = spec_toml("bundle").replacen(
                "bin = \"bundle-cli\"",
                &format!("bin = \"bundle-cli\"\ncompanions = [{bad:?}]"),
                1,
            );
            let error = CapabilitySpec::parse(&invalid, "t").unwrap_err();
            assert!(error.contains("plain file name"), "{bad:?}: {error}");
        }
    }

    #[test]
    fn interactive_restricted_args_round_trip_and_default_to_none() {
        // guards the serde key: a typo (`restricted-args`) would silently leave
        // it None, making every SHARED session spawn error "unsupported" — a
        // vacuous pass. A plain [interactive] (no restricted key) is solo-only.
        let solo = spec_toml("solo") + "\n[interactive]\nargs = []\n";
        let spec = CapabilitySpec::parse(&solo, "t").expect("solo interactive parses");
        let i = spec.interactive.expect("[interactive] present");
        assert_eq!(i.args, Vec::<String>::new());
        assert_eq!(i.restricted_args, None, "no key ⇒ solo-only");

        let shared = spec_toml("shared")
            + "\n[interactive]\nargs = []\nrestricted_args = [\"-s\", \"read-only\"]\n";
        let spec = CapabilitySpec::parse(&shared, "t").expect("shared interactive parses");
        let i = spec.interactive.expect("[interactive] present");
        assert_eq!(
            i.restricted_args.as_deref(),
            Some(["-s".to_string(), "read-only".to_string()].as_slice()),
        );
    }

    #[test]
    fn version_tag_prompt_format_and_timeout_validate() {
        let base = spec_toml("ok");
        assert!(CapabilitySpec::parse(&base, "t").is_ok());

        for (needle, replacement, expect) in [
            ("spec = 1", "spec = 2", "unsupported spec version"),
            (r#"tag = "ok""#, r#"tag = "OK""#, "invalid characters"),
            (r#"tag = "ok""#, r#"tag = """#, "non-empty"),
            (r#"prompt = "stdin""#, r#"prompt = "argv""#, "not supported"),
            (
                r#"format = "text""#,
                r#"format = "yaml""#,
                "not a known parser",
            ),
        ] {
            let broken = base.replace(needle, replacement);
            let err = CapabilitySpec::parse(&broken, "t").unwrap_err();
            assert!(err.contains(expect), "wanted {expect:?} in {err:?}");
        }

        let long_tag = spec_toml(&"x".repeat(65));
        assert!(
            CapabilitySpec::parse(&long_tag, "t")
                .unwrap_err()
                .contains("64 bytes")
        );

        let bad_timeout = base.replace(
            r#"prompt = "stdin""#,
            "prompt = \"stdin\"\ntimeout_secs = 0",
        );
        assert!(
            CapabilitySpec::parse(&bad_timeout, "t")
                .unwrap_err()
                .contains("timeout_secs")
        );

        // the hard ceiling rides the same table: absent = 36, out of range = refused.
        assert_eq!(
            CapabilitySpec::parse(&base, "t")
                .unwrap()
                .hard_timeout_factor,
            36,
            "absent hard_timeout_factor defaults"
        );
        let tuned = base.replace(
            r#"prompt = "stdin""#,
            "prompt = \"stdin\"\nhard_timeout_factor = 12",
        );
        assert_eq!(
            CapabilitySpec::parse(&tuned, "t")
                .unwrap()
                .hard_timeout_factor,
            12
        );
        for bad in ["0", "1001"] {
            let broken = base.replace(
                r#"prompt = "stdin""#,
                &format!("prompt = \"stdin\"\nhard_timeout_factor = {bad}"),
            );
            let err = CapabilitySpec::parse(&broken, "t").unwrap_err();
            assert!(
                err.contains("hard_timeout_factor"),
                "wanted the field named in {err:?}"
            );
        }
    }

    #[test]
    fn unknown_fields_fail_loud() {
        // a typo — or a field from the retired [models] routing era — must
        // never be silently ignored: the operator wrote config that does
        // nothing, and the boot error is what tells them.
        let typo = spec_toml("ok").replace("[invoke]", "[invoke]\ntimeout_sec = 1");
        let err = CapabilitySpec::parse(&typo, "t").unwrap_err();
        assert!(err.contains("not a valid spec"), "got {err:?}");

        let stale = format!("{}\n[models]\npatterns = [\"*\"]\n", spec_toml("ok"));
        let err = CapabilitySpec::parse(&stale, "t").unwrap_err();
        assert!(err.contains("not a valid spec"), "got {err:?}");
    }

    /// A RETIRED section is an unknown one: a stale operator spec that still
    /// declares it fails loud at load rather than parsing into a block nothing
    /// would ever read. (`<ducktape home>/capabilities` is operator-writable, so
    /// this is the door a leftover file comes in.)
    #[test]
    fn retired_sections_fail_loud_at_load() {
        let base = spec_toml("ok");
        for (extra, expect) in [
            // the RETIRED [workspace] per-agent-persistence lane. every run's
            // cwd is the workspace its provisioner materialized, so the section
            // selected nothing even while it parsed.
            ("[workspace]\nmode = \"persistent\"\n", "not a valid spec"),
            ("[workspace]\nmode = \"scratch\"\n", "not a valid spec"),
            // the RETIRED [session] resume lane.
            (
                "[session]\ncapture = \"jsonl-events\"\nresume_args = [\"{session_id}\"]\n",
                "not a valid spec",
            ),
            (
                "[[variants]]\nsuffix = \"m_low\"\nargs = [\"a\"]\nresume_args = [\"{session_id}\"]\n",
                "not a valid spec",
            ),
        ] {
            let toml = format!("{base}\n{extra}");
            let err = CapabilitySpec::parse(&toml, "t").unwrap_err();
            assert!(err.contains(expect), "wanted {expect:?} in {err:?}");
        }
    }

    /// A spec cannot ask for a host directory in the run. There is no
    /// `[sandbox]` table any more: a run executes inside a microVM that shares
    /// no filesystem with the host, so mounting `~/.claude` into it is not a
    /// thing the backend can do at all.
    ///
    /// This refuses at LOAD rather than being ignored at spawn, which is the
    /// whole point of pinning it: an accepted-then-discarded `rw_dirs` would
    /// leave a BYO CLI looking for a credential dir that silently was not
    /// there, and the run would still answer.
    #[test]
    fn a_spec_may_not_ask_for_a_host_directory_in_the_run() {
        for table in [
            "[sandbox]\nrw_dirs = [\"~/.claude\"]\n",
            "[sandbox]\nrw_dirs = [\"/etc/passwd\"]\n",
            "[sandbox]\n",
        ] {
            let spec = format!("{}\n{table}", spec_toml("ok"));
            let err = CapabilitySpec::parse(&spec, "t").unwrap_err();
            assert!(err.contains("not a valid spec"), "{table:?} -> {err:?}");
        }
    }

    #[test]
    fn isolation_parses_and_refuses_unknown_brokers_or_env_names() {
        // absent [isolation] = no broker, no config home (the BYO posture).
        let plain = CapabilitySpec::parse(&spec_toml("ok"), "t").unwrap();
        assert_eq!(plain.isolation, IsolationSpec::default());

        let valid = format!(
            "{}\n[isolation]\nconfig_home_env = \"CODEX_HOME\"\nbroker = \"codex-responses\"\n",
            spec_toml("ok")
        );
        let spec = CapabilitySpec::parse(&valid, "t").unwrap();
        assert_eq!(
            spec.isolation.config_home_env.as_deref(),
            Some("CODEX_HOME")
        );
        assert_eq!(spec.isolation.broker, Some(BrokerKind::CodexResponses));

        // the Anthropic broker is a distinct closed-set member (Claude Code).
        let claude = valid
            .replace("CODEX_HOME", "CLAUDE_CONFIG_DIR")
            .replace("codex-responses", "anthropic-messages");
        let spec = CapabilitySpec::parse(&claude, "t").unwrap();
        assert_eq!(
            spec.isolation.config_home_env.as_deref(),
            Some("CLAUDE_CONFIG_DIR")
        );
        assert_eq!(spec.isolation.broker, Some(BrokerKind::AnthropicMessages));

        // a broker is host CODE: an unknown name must never degrade to "no
        // broker" (that would hand the child the operator's credential), and a
        // malformed env name must never degrade to "no config home" (same leak,
        // via the CLI's real config dir).
        for (needle, replacement) in [
            ("CODEX_HOME", "CODEX-HOME"),
            ("codex-responses", "generic-forwarder"),
        ] {
            let err = CapabilitySpec::parse(&valid.replace(needle, replacement), "t").unwrap_err();
            assert!(err.contains("isolation"), "got {err:?}");
        }
    }

    /// THE credential invariant, from the other side: the broker is the ONLY
    /// auth path, so both halves of it parse on their own and neither can be
    /// paired with a request for a host directory (which no longer parses at
    /// all — see [`a_spec_may_not_ask_for_a_host_directory_in_the_run`]).
    #[test]
    fn the_broker_and_its_config_home_are_the_whole_auth_surface() {
        let broker_only = format!(
            "{}\n[isolation]\nbroker = \"codex-responses\"\n",
            spec_toml("ok")
        );
        assert!(CapabilitySpec::parse(&broker_only, "t").is_ok());

        let config_home_only = format!(
            "{}\n[isolation]\nconfig_home_env = \"CLI_HOME\"\n",
            spec_toml("ok")
        );
        assert!(CapabilitySpec::parse(&config_home_only, "t").is_ok());
    }

    #[test]
    fn context_parses_into_the_closed_location_set() {
        // absent [context] = the doc rides the stdin prompt (the raw-provider door).
        let plain = CapabilitySpec::parse(&spec_toml("ok"), "t").unwrap();
        assert_eq!(plain.context, None);

        let parent = format!(
            "{}\n[context]\npath = \"workspace-parent:CLAUDE.md\"\n",
            spec_toml("ok")
        );
        assert_eq!(
            CapabilitySpec::parse(&parent, "t").unwrap().context,
            Some(ContextLocation::WorkspaceParent("CLAUDE.md".into()))
        );

        // `config-home:` needs the fresh config home to exist at all.
        let home = format!(
            "{}\n[isolation]\nconfig_home_env = \"CLI_HOME\"\n\n[context]\npath = \"config-home:AGENTS.md\"\n",
            spec_toml("ok")
        );
        assert_eq!(
            CapabilitySpec::parse(&home, "t").unwrap().context,
            Some(ContextLocation::ConfigHome("AGENTS.md".into()))
        );
    }

    #[test]
    fn context_rejects_unknown_kinds_traversal_and_a_config_home_that_does_not_exist() {
        let base = spec_toml("ok");
        for (path, expect) in [
            // a raw path is not a location: the kind is what picks the directory.
            ("/etc/cron.d/soul", "<kind>:<file>"),
            ("/etc:passwd", "not a known location"),
            ("home:AGENTS.md", "not a known location"),
            ("AGENTS.md", "<kind>:<file>"),
            // the file half is a plain NAME — the traversal surface the closed
            // set exists to not have.
            ("workspace-parent:../../.bashrc", "plain file"),
            ("workspace-parent:sub/dir/CLAUDE.md", "plain file"),
            ("workspace-parent:..", "plain file"),
            ("workspace-parent:", "plain file"),
        ] {
            let toml = format!("{base}\n[context]\npath = \"{path}\"\n");
            let err = CapabilitySpec::parse(&toml, "t").unwrap_err();
            assert!(
                err.contains(expect),
                "wanted {expect:?} for {path:?}: {err:?}"
            );
        }

        // config-home: without [isolation] config_home_env names a directory
        // that will never exist — a LOAD error, because at run time it would be
        // a silently unsouled agent that still answers.
        let orphan = format!("{base}\n[context]\npath = \"config-home:AGENTS.md\"\n");
        let err = CapabilitySpec::parse(&orphan, "t").unwrap_err();
        assert!(err.contains("config_home_env"), "got {err:?}");

        // and an unknown field under [context] fails loud like everywhere else.
        let typo = format!("{base}\n[context]\npaths = \"workspace-parent:X.md\"\n");
        let err = CapabilitySpec::parse(&typo, "t").unwrap_err();
        assert!(err.contains("not a valid spec"), "got {err:?}");
    }

    #[test]
    fn single_spec_parse_refuses_a_variants_file() {
        // parse() is the single-tag convenience; silently dropping a file's
        // variants would be exactly the quiet misread this loader forbids.
        let toml = format!(
            "{}\n[[variants]]\nsuffix = \"m_low\"\nargs = [\"a\"]\n",
            spec_toml("ok")
        );
        let err = CapabilitySpec::parse(&toml, "t").unwrap_err();
        assert!(err.contains("parse_all"), "got {err:?}");
    }

    #[test]
    fn operator_files_expand_variants_and_override_stays_wholesale_by_tag() {
        let dir =
            std::env::temp_dir().join(format!("capspec-variants-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // an operator family file expands exactly like an embedded one.
        let family = format!(
            "{}\n[[variants]]\nsuffix = \"m1_low\"\nargs = [\"run\", \"--fast\"]\n",
            spec_toml("fam")
        );
        std::fs::write(dir.join("family.toml"), family).unwrap();

        // overriding a base tag replaces THAT tag only: a variant tag is its
        // own tag, so the embedded siblings stay. parent derived from the
        // data, never hardcoded.
        let builtins = builtin_specs();
        let variant = builtins
            .iter()
            .find(|s| s.tag.contains('_'))
            .expect("built-ins ship variant tags");
        let parent = variant.tag.split('_').next().unwrap().to_string();
        std::fs::write(dir.join("override.toml"), spec_toml(&parent)).unwrap();

        let set = SpecSet::load(Some(&dir)).unwrap();
        assert_eq!(
            set.get("fam_m1_low").unwrap().args,
            vec!["run", "--fast"],
            "operator variants expand into their own tags"
        );
        assert_eq!(
            set.get("fam").unwrap().bin,
            "fam-cli",
            "operator base loads too"
        );
        assert_eq!(
            set.get(&parent).unwrap().bin,
            format!("{parent}-cli"),
            "operator spec replaced the base tag wholesale"
        );
        assert_eq!(
            set.get(&variant.tag).unwrap().args,
            variant.args,
            "sibling embedded variant tags are untouched by a base override"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// a family file whose every argv ends in a trailing "-" (the stdin
    /// marker shape that makes appending tool args impossible). `tools` is
    /// spliced in verbatim by the caller so the same fixture serves the with-
    /// and without-[tools] cases.
    fn family_with_tools(tools: &str) -> String {
        format!(
            r#"{base}
{tools}
[[variants]]
suffix = "m_low"
args = ["run", "--model", "m", "-"]

[[variants]]
suffix = "m_high"
args = ["run", "--model", "m", "--hard", "-"]
"#,
            base = spec_toml("ok").replace(r#"args = ["run"]"#, r#"args = ["run", "-"]"#),
        )
    }

    const TOOLS_SECTION: &str = "[tools]\nargs = [\"--mcp\", \"srv\"]\n";

    #[test]
    fn tool_args_splice_in_after_args0_of_every_argv_the_file_produces() {
        let specs = CapabilitySpec::parse_all(&family_with_tools(TOOLS_SECTION), "t").unwrap();
        assert_eq!(specs.len(), 3, "base + two variants");

        for spec in &specs {
            // args[0] is the mode/subcommand selector and STAYS first; the
            // tool args follow it; the trailing stdin marker stays LAST.
            assert_eq!(spec.args[0], "run", "{}: selector first", spec.tag);
            assert_eq!(
                spec.args[1..3],
                ["--mcp", "srv"],
                "{}: tools right after args[0]",
                spec.tag
            );
            assert_eq!(
                spec.args.last().unwrap(),
                "-",
                "{}: the stdin marker is still last",
                spec.tag
            );
        }

        // the variant argvs are otherwise verbatim — injection ADDS, never
        // reorders or drops.
        let get = |tag: &str| specs.iter().find(|s| s.tag == tag).unwrap();
        assert_eq!(
            get("ok_m_high").args,
            ["run", "--mcp", "srv", "--model", "m", "--hard", "-"]
        );
    }

    #[test]
    fn a_spec_without_tools_is_untouched() {
        // the regression guard for every spec that predates [tools]: no
        // section, no insertion — argv byte-identical to the file.
        let specs = CapabilitySpec::parse_all(&family_with_tools(""), "t").unwrap();
        assert_eq!(specs[0].args, ["run", "-"], "base argv verbatim");
        assert_eq!(specs[1].args, ["run", "--model", "m", "-"]);

        // an empty [tools].args is inert too — and a [tools] with no args at
        // all is a loud parse error, not a silent no-op.
        let empty =
            CapabilitySpec::parse_all(&family_with_tools("[tools]\nargs = []\n"), "t").unwrap();
        assert_eq!(empty[0].args, ["run", "-"]);
        let err = CapabilitySpec::parse_all(&family_with_tools("[tools]\n"), "t").unwrap_err();
        assert!(err.contains("not a valid spec"), "got {err:?}");
    }

    #[test]
    fn embedded_specs_carry_their_mcp_tool_args() {
        // a DATA pin, like the curated-matrix test in [`crate::variants`]:
        // the built-ins' whole point is that an agent run gets the ducktape
        // MCP server, and the invariant is per-executor — codex takes a `-c`
        // config override, claude needs the server BOTH configured and
        // pre-allowed (an unapproved MCP call in -p print mode is a denial).
        let specs = builtin_specs();
        let get = |tag: &str| specs.iter().find(|s| s.tag == tag).unwrap();

        let claude = &get("claude").args;
        assert_eq!(claude[0], "-p", "the mode selector stays first");
        assert!(
            claude.windows(2).any(|w| w[0] == "--mcp-config"
                && w[1].contains("\"command\":\"ducktape\"")
                && w[1].contains("\"args\":[\"mcp\"]")),
            "claude configures the ducktape MCP server: {claude:?}"
        );
        assert!(
            claude
                .windows(2)
                .any(|w| w == ["--allowedTools", "mcp__ducktape"]),
            "claude pre-allows it (print mode cannot prompt): {claude:?}"
        );

        // codex's approval mode is LOAD-BEARING and its name reads backwards.
        // `codex exec` is non-interactive: an MCP tool call that wants approval
        // is auto-cancelled ("user cancelled MCP tool call") and the agent
        // silently loses its entire tool plane. Of auto/prompt/writes/approve,
        // only `approve` means "already approved — do not ask". Verified live
        // against codex-cli 0.144.1. Dropping this flag does not fail a build;
        // it fails every codex agent run, quietly. Hence a pin.
        for spec in specs.iter().filter(|s| s.tag.starts_with("codex")) {
            assert!(
                spec.args.windows(2).any(|w| w == [
                    "-c",
                    "mcp_servers.ducktape.env_vars=[\"DUCKTAPE_NODE\",\"DUCKTAPE_RUN_AGENT\",\"DUCKTAPE_RUN_WORKSPACE\",\"DUCKTAPE_RUN_SKILLS\",\"DUCKTAPE_RUN_ACTION_URL\",\"DUCKTAPE_RUN_ACTION_TOKEN\",\"DUCKTAPE_RUN_ID\",\"DUCKTAPE_PROVIDER_CONTROL_URL\",\"DUCKTAPE_PROVIDER_CONTROL_TOKEN\"]"
                ]),
                "{}: codex passes only the run env names to its MCP child: {:?}",
                spec.tag,
                spec.args
            );
            assert!(
                spec.args.windows(2).any(|w| w[0] == "-c"
                    && w[1] == "mcp_servers.ducktape.default_tools_approval_mode=\"approve\""),
                "{}: codex must PRE-APPROVE the ducktape tools, or exec cancels every call: {:?}",
                spec.tag,
                spec.args
            );
        }

        // every codex argv keeps its trailing bare "-" LAST — the reason the
        // tool args splice after args[0] instead of being appended.
        for spec in specs.iter().filter(|s| s.tag.starts_with("codex")) {
            assert_eq!(
                spec.args[..5],
                [
                    "exec",
                    "-c",
                    "mcp_servers.ducktape.command=\"ducktape\"",
                    "-c",
                    "mcp_servers.ducktape.args=[\"mcp\"]"
                ],
                "{}: mcp override right after the subcommand",
                spec.tag
            );
            assert_eq!(
                spec.args.last().unwrap(),
                "-",
                "{}: the stdin marker survives injection",
                spec.tag
            );
        }
    }

    #[test]
    fn operator_dir_overrides_embedded_and_rejects_duplicates() {
        let dir = std::env::temp_dir().join(format!("capspec-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // override SOME embedded spec wholesale — the tag is discovered from
        // the built-in set, never hardcoded.
        let builtins = builtin_specs();
        let (first, rest) = (&builtins[0].tag, &builtins[1].tag);
        std::fs::write(dir.join("my-override.toml"), spec_toml(first)).unwrap();
        let set = SpecSet::load(Some(&dir)).unwrap();
        assert_eq!(
            set.get(first).unwrap().bin,
            format!("{first}-cli"),
            "operator spec won"
        );
        assert!(set.get(rest).is_some(), "untouched built-in remains");

        // two operator files claiming one tag: hard error naming both files.
        std::fs::write(dir.join("zz-dup.toml"), spec_toml(first)).unwrap();
        let err = SpecSet::load(Some(&dir)).unwrap_err();
        assert!(err.contains("duplicate capability tag"), "got {err:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
