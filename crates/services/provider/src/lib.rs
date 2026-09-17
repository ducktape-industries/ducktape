//! host-side capability providers — the I/O half of the capability seam.
//!
//! the capability module (consensus) replicates *who provides what*; this
//! crate is the machine-local counterpart that actually provides it. a
//! [`Provider`] wraps one locally installed executor CLI, and [`discover`]
//! probes the host for the executors the operator brought — BYO by
//! construction: the operator logs their CLI in, the node just spawns it.
//!
//! ## where the credential goes
//!
//! BYO auth used to mean the node never touched a credential at all: the child
//! inherited the operator's environment and HOME, and the CLI found its own
//! dotfiles. that is still the floor, but a spec can now do better, and the two
//! options are mutually exclusive by construction (see [`spec`]):
//!
//!   * `[isolation]` — the only path. the HOST reads the credential and holds
//!     it in this process; a per-run loopback [`broker`] serves the model API
//!     and the child gets only an opaque bearer plus a FRESH, empty config home
//!     (so the CLI cannot fall back to reading the operator's real one). the
//!     credential never enters the child's process tree at all. codex and
//!     claude are both here. There is no second path: a run has no host
//!     filesystem to mount an auth dir from.
//!
//! orthogonally, [`SandboxBackend`] decides HOW the child is spawned (its own
//! microVM — never bare on the host).
//! the two compose: codex in a microVM gets the broker AND the jail.
//!
//! ## executors are data: the capability spec
//!
//! WHICH executors exist, how to detect them, the argv to run them, and how
//! to parse their output is all described by TOML capability specs (see
//! [`spec`] and `docs/records/specs/capability-spec.md`), not by Rust. the built-in
//! executor support ships as embedded spec files parsed by the same code
//! path as operator-provided specs under the workspace's `capabilities/`
//! dir. adding an executor — or retuning a built-in's
//! flags, including which model it runs — is a config change on the
//! operator's machine, never a code change here. dispatch is by EXPLICIT
//! capability tag: [`ProviderSet::resolve`] takes the tag a job names,
//! nothing is inferred from model names.
//!
//! the CLIs are agentic, not plain inference endpoints, so a provider runs
//! them fenced: non-interactive mode, the sandbox flags encoded in the spec's
//! argv, and the workspace the provisioner materialized for this run as the
//! child's cwd (an empty scratch dir when the embedder provisioned none). the
//! child never sees the node's data directory itself. every run starts cold —
//! a run's whole continuity is its prompt envelope, which is what lets any
//! assignee execute it.

pub mod work_admission;

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use rand::RngCore as _;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// how long a cancelled child process group gets to handle SIGTERM before the
/// host escalates to SIGKILL. A microVM needs no such budget — the VMM is a
/// child of this process and `kill_on_drop` takes the whole guest with it.
const TERMINATION_GRACE: Duration = Duration::from_secs(2);
/// how often the teardown paths re-check whether a process or process group is
/// gone. Nothing to do with any particular runtime — it is the granularity of
/// "has this pid disappeared yet", and the waits it drives are bounded by their
/// own callers.
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// mirrors [`saga::MAX_RESULT_BYTES`] (crates/modules/system/saga/src/interface.rs)
/// without depending on it (a host-crate → consensus-module edge). This is
/// the cap `compute::provision::assemble_runner_result` already truncates a
/// run's parsed answer to, WITH a note, before it can land — a run that
/// finishes with an oversized answer already completes today, just trimmed.
const RECORDED_RESULT_CAP_BYTES: usize = 256 * 1024;
/// hard cap on a run's accumulated stdout, checked as each chunk arrives.
/// Deliberately NOT [`RECORDED_RESULT_CAP_BYTES`] itself: that cap is
/// enforced downstream with truncation-plus-a-note, so a run whose full
/// answer is a few hundred KiB over it still succeeds today. Killing the run
/// at the same size would turn "completes, truncated" into "fails outright"
/// for those runs — this cap exists only to stop UNBOUNDED accumulation from
/// a firehose, not to enforce the result size, so it sits an order of
/// magnitude above it. `codex`'s `jsonl-events` output in particular streams
/// one JSON object per tool call/patch/diff for the whole turn, not just the
/// final answer, and can legitimately run to several hundred KiB on an
/// ordinary tool-calling turn. Past this line the run is TERMINATED outright
/// — never truncated, since a truncated JSON/JSONL blob would parse into
/// garbage and land as the run's answer.
const MAX_RUN_OUTPUT_BYTES: usize = 16 * RECORDED_RESULT_CAP_BYTES; // 4 MiB
/// hard cap on the accumulated stderr TAIL (oldest bytes drop first). stderr
/// never becomes the run's answer — only [`excerpt`]'s 400-char slice of it
/// ever leaves this function, and only on the failure path — so a few KiB of
/// trailing context is ample; unlike stdout this is a rolling tail, not a
/// termination trigger.
const MAX_RUN_STDERR_BYTES: usize = 16 * 1024;

/// the ownership tag a provider set stamps on the runs it creates. Its VALUE
/// names the owning service instance, so a compute daemon and an agent daemon
/// on one node stay distinguishable in logs and status.
///
/// It is no longer a REAPING key. Under the container backend a crashed daemon
/// left containers behind and the label was how a successor found its own; a
/// microVM's VMM is a child process that dies with the node, so there is
/// nothing left over to find. The sweep that used this is gone rather than
/// ported.
pub const MANAGED_OWNER_KEY: &str = "io.ducktape.managed";

/// the owner tag for a provider built outside [`discover`] — tests and
/// embedders. Deliberately matches no service instance.
const UNSCOPED_OWNER: &str = "unscoped";

/// the full `key=value` ownership tag for one owner.
pub fn managed_label(owner: &str) -> String {
    format!("{MANAGED_OWNER_KEY}={owner}")
}

/// reserved run-local state INSIDE the run's workdir: the fresh provider config
/// home lives here (see [`CliProvider::prepare_config_home`]). the provisioner's
/// commit bracket removes this directory before duckfs/forge scan the tree, so a
/// provider's own runtime files can never become an agent artifact — which is
/// exactly why the config home is allowed to sit inside the workdir at all.
/// each run owns ONE slot under here, named per run and removed when that run
/// ends ([`RunHome`]) — a workdir outlives its runs and is mounted rw into the
/// next one's sandbox, so what a run leaves here, the next run's child reads.
pub const RUN_RUNTIME_DIR: &str = ".ducktape-run";

/// the env var the provisioner exports to point a run at its read-only W6 skills
/// tree (`crates/noded/src/agent_provision.rs`, read by the tool plane). the sandbox
/// backends read it to know what to MOUNT — see [`CliProvider::sandbox_ro_paths`].
const SKILLS_ROOT_ENV: &str = "DUCKTAPE_RUN_SKILLS";
const RUN_ACTION_URL_ENV: &str = "DUCKTAPE_RUN_ACTION_URL";
/// the bearer that endpoint takes. Host-only, like the url: see
/// [`host_only_env`].
const RUN_ACTION_TOKEN_ENV: &str = "DUCKTAPE_RUN_ACTION_TOKEN";
/// the node this run dials. It is BOTH the run's read surface and, at
/// [`MCP_PATH`], its whole tool plane — the lane serves that one route itself
/// (see [`read_lane`]). A sandbox backend must tunnel its port, or unset it:
/// see [`wire_guest_tunnels`].
const NODE_URL_ENV: &str = "DUCKTAPE_NODE";

/// the opaque per-run bearer the broker hands the child. NOT a credential: it
/// authenticates the child to this host's loopback endpoint and dies with the
/// run. the spec's argv names it (`env_key` in the model-provider block).
const BROKER_TOKEN_ENV: &str = "DUCKTAPE_MODEL_BROKER_TOKEN";
/// Separate run-local capability used only by the MCP control tool. These are
/// reserved: inherited or RunContext-supplied lookalikes are always removed.
const PROVIDER_CONTROL_URL_ENV: &str = "DUCKTAPE_PROVIDER_CONTROL_URL";
const PROVIDER_CONTROL_TOKEN_ENV: &str = "DUCKTAPE_PROVIDER_CONTROL_TOKEN";

/// the upstream credential env vars a broker takes over. the HOST reads these;
/// the child must not see them, or it would dial the provider directly and walk
/// straight past the broker — and an inherited `ANTHROPIC_AUTH_TOKEN` would also
/// force the CLI into API mode, overriding the subscription-shaped `claudeAiOauth`
/// creds file the broker seeds. a sandbox backend's env is an ALLOWLIST, so they
/// never enter a real run; only the test-only bare harness inherits the parent
/// env and must actively remove them (which is exactly what the tests pin).
/// covers both the OpenAI (codex) and Anthropic (claude) upstreams.
#[cfg(any(test, feature = "testkit"))]
const UPSTREAM_CREDENTIAL_ENV: [&str; 4] = [
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
];

/// the `-c` overrides that aim a codex invocation at this run's loopback broker:
/// the model-provider block (base URL + [`BROKER_TOKEN_ENV`] bearer, retries
/// off), the provider selector, and a workspace trust level. shared by the
/// headless [`CliProvider::broker_argv`] (spliced after the subcommand) and the
/// interactive path (prepended — a TUI argv has no subcommand). the child gets a
/// base URL and an opaque bearer; neither recovers the operator's credential.
fn broker_provider_overrides(broker: &broker::BrokerEndpoint, workdir: &Path) -> Vec<String> {
    // the workdir is a path, and codex keys `projects.<key>` by TOML string —
    // so it must be QUOTED as one (a bare path breaks the `-c` parse).
    let project_key = toml::Value::String(workdir.to_string_lossy().into_owned()).to_string();
    vec![
        "-c".into(),
        format!(
            "model_providers.ducktape={{ name=\"Ducktape run broker\", base_url=\"{}\", wire_api=\"responses\", env_key=\"{BROKER_TOKEN_ENV}\", request_max_retries=0, stream_max_retries=0 }}",
            broker.base_url
        ),
        "-c".into(),
        "model_provider=\"ducktape\"".into(),
        "-c".into(),
        format!("projects.{project_key}.trust_level=\"untrusted\""),
    ]
}

/// run env the HOST reads and the guest must never see.
///
/// The scoped action endpoint signs Runs messages as this run's program
/// account, and its token is the whole proof. The only thing that ever used it
/// was the tool plane, which now runs on the host — so the guest holding a copy
/// would be a write capability lent to the model's own sandbox for no reason at
/// all. It is dropped from the manifest env rather than never built, because
/// [`read_lane::ReadLane`] reads it out of this same env to BE the tool plane.
fn host_only_env(key: &str) -> bool {
    matches!(key, RUN_ACTION_URL_ENV | RUN_ACTION_TOKEN_ENV)
}

/// the argv that points ONE run's CLI at ITS tool plane: the MCP endpoint the
/// run's node lane serves, as this CLI spells an MCP server.
///
/// Per RUN, not per spec, and that is the whole reason this is code rather than
/// `[tools].args` in the file: the endpoint is a loopback port drawn when the
/// lane binds, so it does not exist until the run does. The credential broker's
/// `-c` overrides are written the same way and for the same reason.
///
/// `guest_node_url` is the run's node base AS THE GUEST SEES IT — already
/// rewritten to the guest end of the tunnel — so the CLI dials the tool plane
/// over the one socket it has.
///
/// Nothing else crosses: no server command, no environment, no token. A guest
/// that can reach this url is the run the lane belongs to, which is the only
/// identity the tool plane needs.
fn mcp_argv(dialect: McpDialect, guest_node_url: &str) -> Vec<String> {
    let url = format!("{}{}", guest_node_url.trim_end_matches('/'), MCP_PATH);
    match dialect {
        // --allowedTools is NOT optional: in `-p` print mode an unapproved MCP
        // tool call is a DENIAL (there is no human to approve it), so a server
        // that is merely configured would be dead weight.
        McpDialect::Claude => vec![
            "--mcp-config".into(),
            json!({"mcpServers": {"ducktape": {"type": "http", "url": url}}}).to_string(),
            "--allowedTools".into(),
            "mcp__ducktape".into(),
        ],
        // the approval mode is the same requirement in codex's words: without
        // it `exec` cancels every ducktape call rather than asking a human who
        // is not there.
        McpDialect::Codex => vec![
            "-c".into(),
            format!("mcp_servers.ducktape.url={}", toml::Value::String(url)),
            "-c".into(),
            "mcp_servers.ducktape.default_tools_approval_mode=\"approve\"".into(),
        ],
    }
}

/// `argv` with the run's MCP wiring in it, at the one position legal for every
/// executor: after a leading SUBCOMMAND if there is one, otherwise at the
/// front.
///
/// Never the end — codex's headless argv keeps its trailing bare `-` (the stdin
/// marker) LAST. And never blindly after `argv[0]` either: a restricted claude
/// TUI argv opens `--permission-mode plan`, and splitting a flag from its value
/// is an argv the CLI rejects.
fn with_mcp_argv(argv: &[String], dialect: Option<McpDialect>, guest_node_url: &str) -> Vec<String> {
    let Some(dialect) = dialect else {
        return argv.to_vec();
    };
    let leads_with_subcommand = argv.first().is_some_and(|arg| !arg.starts_with('-'));
    let at = usize::from(leads_with_subcommand);
    let mut out = argv[..at].to_vec();
    out.extend(mcp_argv(dialect, guest_node_url));
    out.extend_from_slice(&argv[at..]);
    out
}

// the broker lives in its own crate (crates/services/broker); the alias keeps
// the run loop's `broker::…` call sites reading as the module they were carved
// from.
pub(crate) use broker_host as broker;
// The airlock credential-resolution surface: a consensus-resolved credential and
// the per-run config the broker builds from it (self-host pins the on-chain
// seal_pk). `CredentialKind` is the airlock contract's vendor vocabulary — the
// node maps the gateway module's on-chain tag onto it, so this crate stays
// independent of the gateway module crate.
pub use broker::{AirlockConfig, AirlockTrust, CredentialKind, ResolvedCredential, WorkRef};
// interactive (pty) sessions are unix-only: they use libc pty primitives, which
// are a cfg(unix) dependency. all real node targets (Linux, macOS) are unix.
#[cfg(unix)]
mod interactive;
// the sandbox muscle lives in `sandbox-host` and is re-bound here so the run
// loop's `sandbox::` paths — and every downstream import of the re-exports
// below — resolve through this crate.
pub(crate) use sandbox_host::sandbox;
#[cfg(unix)]
pub use sandbox_host::{GuestAsset, GuestLayout};
#[cfg(unix)]
pub(crate) use sandbox_host::{firecracker_api, guest_manifest, microvm};
mod egress_proxy;
mod pi;
mod read_lane;
pub use read_lane::MCP_PATH;
pub mod run_session;
mod spec;
mod variants;
#[cfg(unix)]
pub use interactive::InteractiveSession;
/// the executors directory's staleness clock, re-exported for the same reason
/// the backend is: a host-side caller reaches the sandbox through this crate.
/// The service daemon re-derives its hello on `newest_mtime`, which is the
/// signal `ensure` rebuilds the guest image from.
pub use sandbox_host::executor_image;
pub use sandbox_host::{SandboxBackend, Vmm};
pub use spec::{
    BrokerKind, CapabilitySpec, ContextLocation, IsolationSpec, McpDialect, OutputFormat,
    ReleaseSource, SpecSet,
};

/// canonical label-safe identity for the node executing a provider run.
pub fn execution_node_id(identity: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(identity.len() * 2);
    for byte in identity {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

/// a cloneable, run-local cancellation signal. cancelling one clone wakes all
/// current waiters, and future waiters observe the already-cancelled state.
#[derive(Debug, Clone)]
pub struct RunCancellation {
    tx: tokio::sync::watch::Sender<bool>,
}

impl RunCancellation {
    pub fn new() -> Self {
        let (tx, _) = tokio::sync::watch::channel(false);
        Self { tx }
    }

    /// idempotently mark the run cancelled and wake every waiter.
    pub fn cancel(&self) {
        self.tx.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.tx.borrow()
    }

    /// wait until this run is cancelled. safe for late subscribers: cancellation
    /// is state, not an edge-triggered notification that can be missed.
    pub async fn cancelled(&self) {
        let mut rx = self.tx.subscribe();
        loop {
            if *rx.borrow() {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

impl Default for RunCancellation {
    fn default() -> Self {
        Self::new()
    }
}

impl PartialEq for RunCancellation {
    fn eq(&self, other: &Self) -> bool {
        self.tx.same_channel(&other.tx)
    }
}

impl Eq for RunCancellation {}

/// This node's operator credential as a run's node lane lends it. Read fresh
/// on every use, because the node re-mints it each boot; never handed to the
/// guest — the lane attaches it to the guest's forge pushes itself, so a push
/// proves itself with the node's authority rather than with a signing key the
/// guest never holds.
#[derive(Clone)]
pub struct OperatorCredential {
    header: &'static str,
    read: Arc<dyn Fn() -> Option<String> + Send + Sync>,
}

impl OperatorCredential {
    /// `header` is the request header the node's operator routes read the
    /// credential from; `read` answers its current value, or `None` when this
    /// node has none to lend.
    pub fn new(
        header: &'static str,
        read: impl Fn() -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            header,
            read: Arc::new(read),
        }
    }

    /// the header name the credential travels under.
    pub fn header_name(&self) -> &'static str {
        self.header
    }

    /// the credential as of now.
    pub fn value(&self) -> Option<String> {
        (self.read)()
    }
}

impl std::fmt::Debug for OperatorCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperatorCredential")
            .field("header", &self.header)
            .finish_non_exhaustive()
    }
}

pub use run_envelope::NativeConversationEvent;

/// Attempt-local projection of a portable conversation. Only these explicitly
/// materialized native artifacts may survive as network history; the fresh
/// provider config home and all credentials remain outside it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct NativeConversationContext {
    pub conversation_id: String,
    /// Logical frozen input range; deliberately independent of a retry run_id.
    pub turn_id: String,
    pub revision: u64,
    pub session_path: PathBuf,
    pub packages: Vec<NativePackage>,
    pub events: Vec<NativeConversationEvent>,
    /// Derived from the committed worker binding, never from model input.
    pub job_reporting: bool,
    pub system_prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct NativePackage {
    pub name: String,
    pub path: PathBuf,
}

/// Host-assembled execution context beside the current input. Committed intent
/// and authenticated events may be projected here, but filesystem paths and
/// capabilities are minted only after host provisioning. A run without an
/// agent identity uses [`RunContext::default`].
#[derive(Debug, Clone, Default)]
pub struct RunContext {
    pub agent_id: Option<String>,
    pub native_conversation: Option<NativeConversationContext>,
    /// the live-output registry key (the dispatch_id half of the saga id) —
    /// set by the oracle pool before provider.run so the output sink can key
    /// a per-run ring the app subscribes as run-output:<dispatch_id>.
    pub run_key: Option<String>,
    /// host-local cancellation for this live run. `None` = the run cannot be
    /// cancelled (runs to completion); cancelling the token terminates the
    /// provider process tree and any live microVM.
    pub cancellation: Option<RunCancellation>,
    /// canonical [`execution_node_id`] of the node running this attempt. It
    /// names the run's directories, so two Ducktape nodes sharing one user
    /// never collide on a run's images or its vsock socket.
    pub executing_node: Option<String>,
    /// an already-materialized workspace this specific run must execute in.
    /// set only by the provisioning wrapper (`compute-service::bind_workspace`)
    /// after a successful per-run duckfs checkout — never a consensus-supplied
    /// path (D7). when set, the provider's cwd is the evidence-backed
    /// workspace; an unusable mount fails the run (W1), never falls back to
    /// the shared scratch dir.
    pub workdir_override: Option<PathBuf>,
    /// run-scoped environment variables for host-provided tool bindings.
    /// these are additive to the process environment and apply only to the
    /// spawned provider child.
    pub env: BTreeMap<String, String>,
    /// host directories whose top-level COMMANDS the run may exec, prepended to
    /// its `PATH`. A sandbox backend copies each one's executables into the
    /// run's read-only asset image, so an entry costs the run its bytes.
    ///
    /// Empty for every run this node provisions: the tool plane is an endpoint
    /// on the run's node lane, not a binary, and nothing else the node ships is
    /// the run's to exec. It stays for an embedder that really does hand a run
    /// a host command.
    pub path_entries: Vec<PathBuf>,
    /// the run's numeric resource demands (`ExecJob.demands`), keyed by
    /// dimension (`cores`, `mem_gb`, ...). the pool fills this before
    /// `provider.run`; `cores` and `mem_gb` become the VM's machine config and
    /// are REQUIRED (a VM is built at a size), the rest are inert (scheduling
    /// already matched them). Default empty.
    pub limits: BTreeMap<String, u64>,
    /// the run's assembled context document — the agent's curated skills, built
    /// into ONE markdown doc by the provisioner (the "soul"). ONE assembly, TWO
    /// doors, and the SPEC picks which: a spec declaring `[context]` gets it
    /// written to the file its CLI already auto-loads (see
    /// [`ContextLocation`]); one that declares none gets it prepended to the
    /// stdin prompt. `None` (no assembled context) means neither door does
    /// anything.
    pub context_doc: Option<String>,
    /// the per-run credential SOURCE the broker draws on: a self-host airlock
    /// config the executing node resolved either from a committed gateway
    /// credential record (`ducktape agent sched --cred`) or for a peer-attached
    /// interactive session. `Some` makes the spawn's broker resolve the upstream
    /// to THIS config instead of `AirlockConfig::from_env()`, so the session
    /// draws on the guest's credential rather than the host's boundary env.
    /// `None` (the default) keeps the env/host-credential path unchanged. Never
    /// consensus data; the resolver builds it host-side from committed state
    /// before the provider spawns.
    pub airlock: Option<broker::AirlockConfig>,
    /// this node's operator credential for the run's node lane to lend to an
    /// admitted forge push. `None` (an embedder with no node behind it) refuses
    /// every push at the lane. Never consensus data, never guest env.
    pub operator_credential: Option<OperatorCredential>,
}

/// which child stream produced one live output line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// one line observed from a provider child as it arrives. `line` does not
/// include the trailing newline, matching `BufReader::lines()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLine {
    pub stream: OutputStream,
    pub line: String,
}

/// provider-reported token counters. cached/reasoning values are subsets of
/// input/output, so totals add only the two top-level fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
}

/// Native execution may handle structured input without a model, or settle a
/// cancellation at a safe boundary. These are distinct protocol results, never
/// empty or fabricated assistant answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputDisposition {
    Answer,
    InputHandled,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderOutput {
    pub text: String,
    pub usage: Option<TokenUsage>,
    pub disposition: OutputDisposition,
}

/// optional live-tail callback for provider output. The run context is passed
/// beside each line so embedders can key their own per-run registry with the
/// host-local identity available to the provider.
pub type OutputSink = Arc<dyn Fn(&RunContext, OutputLine) + Send + Sync>;

/// a machine-local executor for one capability tag. implementations do real
/// I/O (spawn processes); nothing consensus-side may ever hold one. the
/// input is just the fully rendered prompt — everything else about the
/// invocation (binary, flags, model) is the spec's literal argv. rendering
/// (conversation -> text) is the CALLER's business — this crate is
/// deliberately ignorant of chat shapes and saga specs.
///
/// `Send + Sync` (and a Send `run` future) on purpose: provider execution is
/// long (a CLI call can run minutes) and hosts run it on SPAWNED background
/// tasks, never inline on their event loop — so the provider surface must be
/// shareable across tasks. this is a Rust seam bound only; the provider CLI
/// contract (spec argv, stdin prompt, stdout answer) is untouched.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// the capability tag this provider serves — matches the capability
    /// module's registry entries, so "what i can run" and "what i announce"
    /// cannot drift apart.
    fn capability(&self) -> &str;
    /// run one prompt to completion and return the assistant's final text.
    /// `ctx` is host-local run identity (see [`RunContext`]) — implementations
    /// that predate workspaces/sessions may ignore it only when
    /// `ctx.cancellation` is `None`. Once cancellation is present, resolving
    /// this future means every execution resource owned by this run is proven
    /// stopped and its wait/reap completed. If that proof cannot be obtained,
    /// the implementation must remain pending fail-closed rather than return.
    async fn run(&self, prompt: &str, ctx: &RunContext) -> Result<String, String>;
    /// the same run plus optional executor-reported usage. text-only providers
    /// inherit the text-only default without changing their API.
    async fn run_with_usage(
        &self,
        prompt: &str,
        ctx: &RunContext,
    ) -> Result<ProviderOutput, String> {
        self.run(prompt, ctx)
            .await
            .map(|text| ProviderOutput {
                text,
                usage: None,
                disposition: OutputDisposition::Answer,
            })
    }
    /// spawn an INTERACTIVE, pty-backed session driving this executor's TUI (see
    /// [`crate::interactive`]). The default refuses; a spec with an
    /// `[interactive]` argv supports it, and the pty itself is allocated inside
    /// the guest by `duck-guest-init`. `restricted` selects the read-only,
    /// non-prompting argv for a SHARED (command-lane) session.
    #[cfg(unix)]
    async fn spawn_interactive(
        &self,
        ctx: &RunContext,
        restricted: bool,
    ) -> Result<InteractiveSession, String> {
        let _ = (ctx, restricted);
        Err(format!(
            "{}: this capability has no interactive session support",
            self.capability()
        ))
    }
}

/// the host's provider surface: every LOADED spec (for routing — an
/// uninstalled capability still routes, so errors can name it) plus the
/// DISCOVERED providers (for execution — exactly what this node announces).
pub struct ProviderSet {
    specs: SpecSet,
    providers: Vec<Box<dyn Provider>>,
}

impl ProviderSet {
    pub fn assemble(specs: SpecSet, providers: Vec<Box<dyn Provider>>) -> Self {
        Self { specs, providers }
    }

    /// no specs, no providers — a consensus-only node's provider set (no
    /// configured sandbox means nothing to spawn in, so nothing is
    /// discovered or announced). every resolve() fails with a clean error.
    pub fn empty() -> Self {
        Self {
            specs: SpecSet::from_specs(Vec::new()),
            providers: Vec::new(),
        }
    }

    fn find(&self, capability: &str) -> Option<&dyn Provider> {
        self.providers
            .iter()
            .find(|p| p.capability() == capability)
            .map(Box::as_ref)
    }

    /// the sorted tag list of DISCOVERED providers — the truthful payload for
    /// a capability announcement.
    pub fn capabilities(&self) -> Vec<String> {
        let mut tags: Vec<String> = self
            .providers
            .iter()
            .map(|p| p.capability().to_string())
            .collect();
        tags.sort();
        tags
    }

    /// capability-tag resolution, the one entry point callers should use.
    /// dispatch is explicit: the tag arrives with the job, nothing is
    /// inferred. each failure is a distinct, actionable error — a tag no
    /// loaded spec claims (likely an operator typo somewhere), or a
    /// capability this node knows of but does not provide.
    pub fn resolve(&self, capability: &str) -> Result<&dyn Provider, String> {
        if self.specs.get(capability).is_none() {
            let loaded: Vec<&str> = self.specs.iter().map(|s| s.tag.as_str()).collect();
            return Err(format!(
                "no capability spec is loaded for tag {capability:?}; loaded specs: {loaded:?}"
            ));
        }
        let Some(provider) = self.find(capability) else {
            return Err(format!(
                "capability {capability:?} is not provided by this node; \
                 this node provides {:?}",
                self.capabilities()
            ));
        };
        Ok(provider)
    }
}

/// everything one run hands its child so the CLI can authenticate WITHOUT the
/// operator's credential — both `None` for a plain BYO spec (no `[isolation]`),
/// which is the historical posture and still the default.
///
/// this is the whole of it: a directory and run-local broker capabilities. the
/// upstream credential is not here and never will be — it stays in this process,
/// behind the [`broker`].
#[derive(Default)]
struct RunAuth<'a> {
    /// this run's FRESH config home (materialized under [`RUN_RUNTIME_DIR`]),
    /// exported as the spec's `isolation.config_home_env`. auth-load-bearing:
    /// it is what stops the CLI reading the operator's real config home, and so
    /// what forces it through the broker.
    config_home: Option<&'a Path>,
    /// this run's live broker endpoint, when the spec declares one.
    broker: Option<&'a broker::BrokerEndpoint>,
}

/// a [`Provider`] that interprets one [`CapabilitySpec`] against one resolved
/// binary: spawn `bin` with the spec's literal argv, feed the prompt on
/// stdin, parse stdout with the spec's named format.
pub(crate) struct CliProvider {
    spec: CapabilitySpec,
    bin: PathBuf,
    /// the child's FALLBACK working directory — an empty scratch dir, never
    /// the node's data directory, so an agentic CLI has nothing to wander
    /// into. every production run overrides it with the workspace its
    /// provisioner materialized (`RunContext::workdir_override`); this is what
    /// an embedder that provisions none gets.
    workdir: PathBuf,
    /// the IDLE window, not a wall clock: any child output refreshes it, so
    /// a streaming agentic run outlives it freely; only silence this long
    /// kills the child. `idle × hard_timeout_factor` is the absolute cap.
    timeout: Duration,
    /// the spec's `[invoke].hard_timeout_factor`: `timeout × this` is the wall
    /// clock at which even a continuously-chatty child is killed.
    hard_timeout_factor: u32,
    /// optional live per-line output sink. `None` means no output lines are
    /// forwarded; stdout/stderr are still accumulated for the existing parse
    /// and error contracts.
    output_sink: Option<OutputSink>,
    /// how the child is spawned: its own microVM. set once at
    /// discovery for the whole provider set.
    backend: SandboxBackend,
    /// the persistent per-agent cache volume attached to every run of this
    /// provider (`CARGO_HOME`, `RUSTUP_HOME`, `target/`), or `None` for a
    /// provider whose runs get none.
    ///
    /// ATTACHED, never copied: the workspace round trip is 13.8 s for a 1.7 GB
    /// source tree and `target/` alone is 76 GB, so a build cache that rode the
    /// per-run image would cost more than the run. The spec's *Build caches*
    /// section records why this is a separate device and why it is never the
    /// operator's own `~/.cargo`.
    agent_volume: Option<PathBuf>,
    /// which service instance OWNS the runs this provider creates. Set once at
    /// discovery, so a compute daemon and (later) an agent daemon sharing one
    /// node each reap only their own.
    managed_owner: String,
}

/// how the GUEST wires the child's stdio. Chosen by the caller of
/// [`CliProvider::microvm_boot`], since it is the difference between the two
/// kinds of run rather than a property of the executor.
///
/// A pty cannot come from the host: a master and its slave are two ends of one
/// kernel object, and the guest runs on a different kernel. So this crosses in
/// the manifest and `duck-guest-init` is what allocates one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuestStdio {
    /// three pipes, stderr kept separate — a headless run's answer and its
    /// diagnostics are different things, and the caller reports them
    /// differently.
    Pipes,
    /// one pty. A CLI that finds no terminal on its stdout draws nothing, or
    /// refuses outright, so this is what makes a TUI session possible at all.
    /// stderr arrives merged into stdout, the way a terminal has always
    /// delivered it.
    Pty,
}

impl CliProvider {
    /// the general constructor: any spec, any resolved binary, and the
    /// sandbox adapter every run of this provider spawns through — required
    /// up front because there is no bare default to fall back to. the
    /// timeout starts at the spec's `timeout_secs`.
    pub fn from_spec(spec: CapabilitySpec, bin: PathBuf, backend: SandboxBackend) -> Self {
        let workdir = std::env::temp_dir().join(format!(
            "ducktape-provider-{}-{}",
            spec.tag,
            std::process::id()
        ));
        let timeout = Duration::from_secs(spec.timeout_secs);
        let hard_timeout_factor = spec.hard_timeout_factor;
        Self {
            spec,
            bin,
            workdir,
            timeout,
            hard_timeout_factor,
            output_sink: None,
            backend,
            agent_volume: None,
            managed_owner: UNSCOPED_OWNER.to_string(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_workdir(mut self, workdir: PathBuf) -> Self {
        self.workdir = workdir;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_output_sink(mut self, output_sink: OutputSink) -> Self {
        self.output_sink = Some(output_sink);
        self
    }

    /// name the service instance that owns this provider's containers — see
    /// [`CliProvider::managed_owner`]. Set by [`discover`] for the whole set.
    pub(crate) fn with_managed_owner(mut self, owner: &str) -> Self {
        self.managed_owner = owner.to_string();
        self
    }

    #[cfg(test)]
    fn command(
        &self,
        args: &[String],
        workdir: &Path,
        ctx: &RunContext,
        auth: &RunAuth<'_>,
    ) -> Result<tokio::process::Command, String> {
        self.prepared_command(args, workdir, ctx, auth)
    }

    // in a non-test build only the two error arms remain, so the inputs the Bare
    // arm consumes are unused — expected, not a defect.
    #[cfg_attr(not(any(test, feature = "testkit")), allow(unused_variables))]
    fn prepared_command(
        &self,
        args: &[String],
        workdir: &Path,
        ctx: &RunContext,
        auth: &RunAuth<'_>,
    ) -> Result<tokio::process::Command, String> {
        // Only the Bare test harness reaches here now: a microVM is booted in
        // [`Self::invoke`], before this seam. (`args`/`ctx`/`auth` are consumed
        // only by the Bare arm, so in a non-test build they are legitimately
        // unused.)
        match &self.backend {
            SandboxBackend::MicroVm { .. } => {
                Err("internal error: a microVM is booted, not spawned as a command".into())
            }
            #[cfg(any(test, feature = "testkit"))]
            SandboxBackend::Bare => {
                let args = self.broker_argv(args, workdir, auth);
                let mut command = self.bare_command(&args, ctx, auth)?;
                command
                    .current_dir(workdir)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true);
                Ok(command)
            }
        }
    }

    /// the test-harness spawn ([`SandboxBackend::Bare`]): the spec's binary
    /// with the spec's argv and an ADDITIVE env overlay (the inherited
    /// environment plus this run's scoped `ctx.env` / PATH bindings, plus
    /// [`Self::apply_auth_env`]). exists so the run loop's env/auth/session
    /// contracts stay unit-testable without a hypervisor; a shipped binary has
    /// no bare spawn — the microVM backend (its own kernel, its own
    /// filesystem, nothing of the host's) is the D7 isolation mechanism on
    /// every real node.
    #[cfg(any(test, feature = "testkit"))]
    fn bare_command(
        &self,
        args: &[String],
        ctx: &RunContext,
        auth: &RunAuth<'_>,
    ) -> Result<tokio::process::Command, String> {
        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.args(args.iter());
        cmd.envs(ctx.env.iter());
        cmd.env_remove(PROVIDER_CONTROL_URL_ENV);
        cmd.env_remove(PROVIDER_CONTROL_TOKEN_ENV);
        // the child INHERITS this process's environment here, so a broker-backed
        // run must actively remove the upstream credential vars — see
        // [`Self::apply_auth_env`], where that subtraction is the load-bearing
        // half. (a sandbox backend has no such problem: its env is an allowlist.)
        self.apply_auth_env(auth, |k, v| {
            cmd.env(k, v);
        })?;
        if auth.broker.is_some() {
            for key in UPSTREAM_CREDENTIAL_ENV {
                cmd.env_remove(key);
            }
        }
        if let Some(path) = self.run_path(ctx)? {
            cmd.env("PATH", path);
        }
        Ok(cmd)
    }

    /// build this run's workspace image and boot its microVM, returning the VM
    /// and its stdio.
    ///
    /// Every host path the run can observe is rewritten to the guest's fixed
    /// layout ([`GuestLayout`]), so the guest never sees the operator's real
    /// paths. There is no mount plan to build: a microVM has no shared
    /// filesystem, so the workspace arrives as a block device and the executor
    /// comes from the read-only rootfs. That deletes the whole bind-mount
    /// surface rather than configuring it.
    ///
    /// `args` are the FINAL executor argv (the caller has already applied
    /// [`Self::broker_argv`] for a headless run or `interactive_argv` for a
    /// TUI); this method only translates their paths.
    async fn microvm_boot(
        &self,
        args: &[String],
        workdir: &Path,
        ctx: &RunContext,
        auth: &RunAuth<'_>,
        stdio: GuestStdio,
    ) -> Result<(microvm::MicroVm, microvm::MicroVmIo, GuestLanes), String> {
        // one discriminant, one match, no wildcard: `Bare` exists only in
        // test/testkit builds, so a `let ... else` here is irrefutable in a
        // shipped build. A future backend fails this match until it is routed.
        let (vmm, kernel, rootfs, executors) = match &self.backend {
            SandboxBackend::MicroVm {
                vmm,
                kernel,
                rootfs,
                executors,
            } => (*vmm, kernel, rootfs, executors),
            #[cfg(any(test, feature = "testkit"))]
            SandboxBackend::Bare => {
                return Err("internal error: microVM boot on a non-microVM backend".into());
            }
        };

        let mut envs = self.sandbox_env(ctx, auth)?;
        // the run's node entry is its OWN read lane, not the node's
        // listener: this binds it and repoints `DUCKTAPE_NODE` at it, so what
        // gets tunnelled below is the lane's port. It lives exactly as long as
        // the returned value, which the run holds beside its VM.
        let read_lane = read_lane::ReadLane::start(
            &mut envs,
            ctx.agent_id.clone(),
            ctx.operator_credential.clone(),
        )
        .await?;
        // the run's way off this host: an egress proxy on its own tunnel,
        // named to the guest through the proxy env every http client reads.
        let egress = egress_proxy::EgressProxy::start(&mut envs).await?;
        // wired HERE, before the env is translated and frozen into the
        // manifest — the name is the warning: it rewrites `envs`.
        let tunnel_ports = wire_guest_tunnels(
            &mut envs,
            auth.broker.map(|broker| broker.base_url.as_str()),
        );
        let workdir = canonical_mount_path(workdir, "microVM workdir")?;
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/nonexistent"));

        let mut layout = GuestLayout::new(&workdir, &home);
        // the run's read-only inputs — PATH entries, the skills tree, the
        // context doc — ride a per-run asset image. Each one's guest path is
        // registered here so an argv or env value naming it is rewritten too; a
        // path that reached the guest untranslated would point at nothing.
        let assets = self.sandbox_ro_paths(ctx, &workdir, auth)?;
        for (index, asset) in assets.iter().enumerate() {
            let host = asset.path();
            let guest = match host.is_file() {
                // a FILE is the workspace-parent context doc: it lands beside
                // the workspace so `../<name>` still resolves.
                true => Path::new(sandbox_host::guest_paths::GUEST_ASSETS)
                    .join(host.file_name().unwrap_or_default()),
                false => PathBuf::from(sandbox_host::guest_paths::guest_asset_dir(index)),
            };
            layout.map(host, &guest);
        }
        let guest_workdir = PathBuf::from(sandbox_host::guest_paths::GUEST_WORKSPACE);
        // the guest cwd is known HERE and nowhere earlier — see the fn doc.
        self.trust_guest_workdir(auth, &guest_workdir)?;

        // HOME is set directly to the guest home; every other value is
        // substring-translated so an embedded host path cannot survive.
        let translated_env: Vec<(String, String)> = envs
            .iter()
            .filter(|(key, _)| !host_only_env(key))
            .map(|(key, value)| {
                let value = if key == "HOME" {
                    sandbox_host::guest_paths::GUEST_HOME.to_string()
                } else {
                    layout.translate(value)
                };
                (key.clone(), value)
            })
            .collect();
        // HERE and nowhere earlier: the tool plane's address is the guest end
        // of this run's node tunnel, which `wire_guest_tunnels` above only just
        // decided. Both argvs — the headless `[invoke]` one and the interactive
        // TUI one — reach the guest through this function, so wiring it here is
        // what makes a SESSION carry the ducktape tools too.
        let guest_node_url = envs
            .iter()
            .find(|(key, _)| key == NODE_URL_ENV)
            .map(|(_, url)| url.clone());
        let args = match &guest_node_url {
            Some(url) => with_mcp_argv(args, self.spec.tools, url),
            // a run with no node url has no lane and therefore no tool plane;
            // configuring a server it could not reach would make the CLI spend
            // its startup failing to connect instead of running.
            None => args.to_vec(),
        };
        let manifest_argv = guest_argv(&self.bin, &args, &layout);

        // THE paid-execution guard, at the last moment before anything is
        // spent. There is no pull or create step here to race — booting the VM
        // IS the spend — so the check belongs immediately in front of it.
        let cancelled = ctx
            .cancellation
            .as_ref()
            .is_some_and(RunCancellation::is_cancelled);
        if cancelled {
            return Err(format!(
                "{} cancelled before start (its attempt was reassigned)",
                self.bin.display()
            ));
        }

        // the run's SIZE is decided before any directory exists: a refusal here
        // is the commonest one there is (a spec that named no `cores`), and it
        // used to leave an empty run dir behind every single time — one per
        // refused attempt, with nothing to reap them.
        let vcpus = vm_cores(&ctx.limits)?;
        let mem_mib = vm_mem_mib(&ctx.limits)?;

        // BEFORE any directory exists: deriving the executors image stats the
        // operator's executors directory and rebuilds it, and it refuses a
        // foreign binary there on every single run. Creating the run's scratch
        // first left one directory pair per refusal, unbounded.
        let executors = sandbox_host::executor_image::ensure(executors)?;

        // ONE slot for both directories, drawn per boot: they are two halves of
        // the same run's scratch and are removed together when the VM drops —
        // and until the VM exists, these guards are what removes them.
        let slot = run_slot();
        let run_scratch = microvm_run_dir(&slot)?;
        let socket_scratch = microvm_socket_dir(&slot)?;
        let run_dir = run_scratch.path();
        let vm_config = firecracker_api::VmConfig {
            vmm,
            kernel: kernel.clone(),
            rootfs: rootfs.clone(),
            manifest: run_dir.join("manifest.bin"),
            agent_volume: self.agent_volume.clone(),
            assets: run_dir.join("assets.ext4"),
            workspace: run_dir.join("workspace.ext4"),
            // Derived above, per boot, rather than at discovery: an operator
            // who installs a CLI mid-life expects the next run to have it, and
            // the check is two stats when the image is already current.
            executors,
            vcpus,
            mem_mib,
            vsock_uds: socket_scratch.path().join(MICROVM_SOCKET_NAME),
            // no tap: the guest has no NIC, so its whole reach is the vsock
            // tunnels above. That is not the same as "no egress" — the node
            // tunnel's read lane passes `/v1/gateway/proxy`, which dispatches a
            // `GatewayJob::Http` over the overlay to a publisher node, and its
            // only gate (`gateway_http::gateway_api_origin_allowed`) is a
            // header check a plain `curl` passes by sending no headers. See
            // [`wire_guest_tunnels`] for the full reach.
            tap: None,
        };

        let manifest = guest_manifest::RunManifest {
            argv: manifest_argv,
            env: translated_env,
            cwd: guest_workdir.display().to_string(),
            mounts: firecracker_api::manifest_mounts(&vm_config),
            tunnel_ports,
            pty: stdio == GuestStdio::Pty,
        };

        // every error path out of `boot` leaves both directories to the guards,
        // which drop with this `?`.
        let booted =
            microvm::MicroVm::boot(run_dir, &workdir, &assets, &vm_config, &manifest).await?;
        // the VM's own `Drop` removes them from here.
        run_scratch.disarm();
        socket_scratch.disarm();
        let (vm, io) = booted;
        Ok((
            vm,
            io,
            GuestLanes {
                _read_lane: read_lane,
                _egress: egress,
            },
        ))
    }

    /// the env carried into a sandbox.
    ///
    /// `HOME` is carried as the HOST's home path and then rewritten to the
    /// guest's fixed home by [`GuestLayout`] — the host directory itself never
    /// crosses (D7). $HOME unset is a loud error, not a silent unsandboxed
    /// fallback.
    ///
    /// this env is an ALLOWLIST — a sandboxed child inherits nothing it is not
    /// handed here — so a broker's upstream credential vars are excluded by
    /// simply never being added, with no subtraction step to forget.
    fn sandbox_env(
        &self,
        ctx: &RunContext,
        auth: &RunAuth<'_>,
    ) -> Result<Vec<(String, String)>, String> {
        let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
            format!(
                "{}: a sandbox backend needs $HOME set to anchor the guest's home",
                self.spec.tag
            )
        })?;
        let home = canonical_mount_path(&home, "sandbox HOME")?;
        let mut envs: Vec<(String, String)> = vec![("HOME".into(), home.display().to_string())];
        if let Some(path) = self.run_path(ctx)? {
            envs.push(("PATH".into(), path.to_string_lossy().into_owned()));
        }
        for (key, value) in &ctx.env {
            if key == "HOME" || key == PROVIDER_CONTROL_URL_ENV || key == PROVIDER_CONTROL_TOKEN_ENV
            {
                continue;
            }
            let value = if key == SKILLS_ROOT_ENV {
                canonical_mount_path(Path::new(value), "sandbox skills root")?
                    .display()
                    .to_string()
            } else {
                value.clone()
            };
            envs.push((key.clone(), value));
        }
        self.apply_auth_env(auth, |k, v| envs.push((k.to_string(), v)))?;
        Ok(envs)
    }

    /// the paths mounted READ-ONLY into a sandbox: the run's PATH entries (its
    /// tool bindings) plus the W6 skills tree, when the provisioner mounted one.
    ///
    /// the provisioner materializes the skill ro-mounts at a SIBLING of the rw
    /// checkout (`<slug>-ro/<name>` — deliberately OUTSIDE the workdir, so
    /// `commit` never scans them) and points the child at it with
    /// [`SKILLS_ROOT_ENV`]. inside a container/VM only what we mount
    /// exists — so without this the agent's own skills would be a DANGLING path:
    /// the env var set, the directory simply absent.
    ///
    /// the run's context doc joins them when it lands OUTSIDE the workdir (a
    /// `workspace-parent:` soul does; a `config-home:` one is under the workdir,
    /// which every backend already mounts, so it crosses for free). without this
    /// the file would exist on the host and simply not be there for the child —
    /// a silently unsouled agent, the one failure mode this feature must not have.
    /// Each asset is tagged with WHAT it is, because a VM copies where a
    /// container bind-mounted and the two kinds cost wildly different amounts:
    /// a PATH entry hands over only the commands it offers, while a skills tree
    /// and a context doc cross entire. See [`GuestAsset`].
    fn sandbox_ro_paths(
        &self,
        ctx: &RunContext,
        workdir: &Path,
        auth: &RunAuth<'_>,
    ) -> Result<Vec<GuestAsset>, String> {
        let mut assets: Vec<GuestAsset> = ctx
            .path_entries
            .iter()
            .cloned()
            .map(GuestAsset::Commands)
            .collect();
        assets.extend(
            ctx.env
                .get(SKILLS_ROOT_ENV)
                .map(|root| GuestAsset::Whole(PathBuf::from(root))),
        );
        if ctx.context_doc.is_some()
            && let Some(doc) = self.context_target(workdir, auth.config_home)?
            && !doc.starts_with(workdir)
        {
            assets.push(GuestAsset::Whole(doc));
        }
        if self.backend.is_bare_test() {
            return Ok(assets);
        }
        assets
            .into_iter()
            .map(|asset| {
                let canonical = canonical_mount_path(asset.path(), "sandbox read-only mount")?;
                Ok(match asset {
                    GuestAsset::Commands(_) => GuestAsset::Commands(canonical),
                    GuestAsset::Whole(_) => GuestAsset::Whole(canonical),
                })
            })
            .collect()
    }

    /// where this run's assembled context doc lands — the spec's [`ContextLocation`]
    /// resolved against THIS run's directories. `None` when the spec names no
    /// `[context]` (that spec's door is the stdin prompt instead).
    fn context_target(
        &self,
        workdir: &Path,
        config_home: Option<&Path>,
    ) -> Result<Option<PathBuf>, String> {
        let dir = match &self.spec.context {
            None => return Ok(None),
            // parse-time guaranteed: `config-home:` requires isolation.config_home_env,
            // which is exactly what makes prepare_config_home materialize one. so this
            // is unreachable-by-construction, and loud rather than silently unsouled.
            Some(ContextLocation::ConfigHome(file)) => config_home
                .ok_or_else(|| {
                    format!(
                        "{}: context.path names the run's config home, but none was \
                         prepared for this run",
                        self.spec.tag
                    )
                })?
                .join(file),
            // Namespaced under the run's own slug (`workdir`'s basename), NOT
            // written directly at `workdir.parent()`: that parent is the node-wide
            // runs root shared by every run of this tag, so two runs alive at once
            // would write the SAME path and each overwrite the other's soul (#1692).
            // The slug is already this run's unique identity — it IS the workdir's
            // name — so keying on it costs no new randomness and stays stable
            // across the two calls (`assemble_assets_for_run` and `deliver_context`)
            // that must agree on this path for one run.
            //
            // The guest never sees this host detail: `stage_whole` copies a file
            // asset by its OWN BASENAME to the asset image root, so the CLI still
            // finds it at `../<file>` relative to its workdir regardless of which
            // host directory it was staged from.
            Some(ContextLocation::WorkspaceParent(file)) => {
                let parent = workdir.parent().ok_or_else(|| {
                    format!(
                        "{}: context.path names the parent of the run's workdir, but \
                         {} has none",
                        self.spec.tag,
                        workdir.display()
                    )
                })?;
                let slug = workdir.file_name().ok_or_else(|| {
                    format!(
                        "{}: run workdir {} has no name to key its context document by",
                        self.spec.tag,
                        workdir.display()
                    )
                })?;
                parent.join(RUN_RUNTIME_DIR).join(slug).join(file)
            }
        };
        Ok(Some(dir))
    }

    /// write the run's assembled context doc where the spec says the executor
    /// auto-loads it from, BEFORE the child starts. a write failure fails the run
    /// loudly — a silently unsouled agent is a different agent, and it would still
    /// answer, so the failure would never surface.
    ///
    /// the returned guard removes the file when it lives outside the workdir (see
    /// [`ContextGuard`]); a doc inside the workdir needs none.
    fn deliver_context(
        &self,
        workdir: &Path,
        config_home: Option<&Path>,
        ctx: &RunContext,
    ) -> Result<Option<ContextGuard>, String> {
        let (Some(doc), Some(path)) = (
            ctx.context_doc.as_ref(),
            self.context_target(workdir, config_home)?,
        ) else {
            return Ok(None);
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "{}: preparing {} for the run's context document failed: {e}",
                    self.spec.tag,
                    parent.display()
                )
            })?;
        }
        std::fs::write(&path, doc).map_err(|e| {
            format!(
                "{}: writing the run's context document to {} failed: {e} — refusing \
                 to run the agent without the context it was assembled with",
                self.spec.tag,
                path.display()
            )
        })?;
        // `then`, NOT `then_some`: the guard DELETES the file on drop, and
        // `then_some` builds its argument eagerly — so a doc inside the workdir
        // (the false arm) would construct a guard, drop it on the spot, and erase
        // the soul it had just written. it did, until this line.
        Ok((!path.starts_with(workdir)).then(|| ContextGuard(path)))
    }

    /// the prompt as the child receives it: for a spec with NO `[context]` (a raw
    /// provider — no ambient-instructions convention to use), the assembled doc
    /// is prepended to the stdin prompt. a spec WITH `[context]` already got it as
    /// a file, so prepending too would ship the soul twice.
    fn prompt_with_context(&self, prompt: &str, ctx: &RunContext) -> String {
        match (&self.spec.context, &ctx.context_doc) {
            (None, Some(doc)) => format!("{doc}\n\n{prompt}"),
            _ => prompt.to_string(),
        }
    }

    /// materialize this run's FRESH executor config home — `None` unless the
    /// spec names one. it lands under [`RUN_RUNTIME_DIR`] INSIDE the workdir
    /// (0700), which is deliberate on two counts: the workdir is the one path
    /// every backend already mounts rw, so the child reaches the dir at the same
    /// path in a container as on the host; and the provisioner's commit bracket
    /// deletes that reserved dir before scanning, so the CLI's own runtime files
    /// never land in a snapshot or a commit.
    ///
    /// the directory is EMPTY, and that is the auth-load-bearing part: pointed
    /// at an empty `CODEX_HOME`, codex cannot read the operator's `auth.json`
    /// and must use the model provider the broker argv names.
    ///
    /// EMPTY is guaranteed by the NAME, not by cleaning: [`runtime_slot`] is
    /// drawn fresh per run, so the directory did not exist a moment ago and
    /// nothing another run wrote can be inside it. it is removed when the
    /// returned [`RunHome`] drops, so this run's state is not left in a workdir
    /// the next account's run mounts.
    fn prepare_config_home(&self, workdir: &Path) -> Result<Option<RunHome>, String> {
        if self.spec.isolation.config_home_env.is_none() {
            return Ok(None);
        }
        let slot = workdir.join(RUN_RUNTIME_DIR).join(runtime_slot());
        let config = slot.join("provider-config");
        create_private_dir(&config)?;
        // built BEFORE the seed writes below, so a failed seed takes the
        // half-materialized home down with it on the `?`.
        let home = RunHome { slot, config };
        if self.spec.isolation.broker == Some(BrokerKind::Pi) {
            pi::stage_tools(home.config())?;
        }
        // The two Claude Code state files this FRESH config home must carry.
        // Written only for the Anthropic broker (codex ignores both).
        //
        // `settings.json` / `skipWebFetchPreflight`: cannot be expressed as an
        // env var. Without it, WebFetch preflights api.anthropic.com DIRECTLY,
        // bypassing the broker, and fails behind isolation.
        //
        // `.claude.json` / `hasCompletedOnboarding`: Claude Code runs its
        // FIRST-RUN WIZARD whenever this file does not say onboarding is done —
        // and a fresh config home never does. Step two of that wizard is
        // "Select login method", which opens a browser OAuth flow the sandbox
        // can neither reach nor complete, so an INTERACTIVE session dead-ends
        // there even though the seeded `.credentials.json` is valid and the
        // broker is up. Headless `claude -p` never runs the wizard, which is
        // exactly why only the TUI lane broke.
        //
        // The claim is TRUE, not a forged auth state. The wizard collects a
        // theme and a login method; both are already decided — the operator
        // logged in on the host and the broker holds that credential. Nothing
        // here asserts the UPSTREAM will accept anything: a bad or revoked
        // credential still fails honestly on the first `/v1/messages`, with the
        // provider's own error, rather than being papered over.
        if self.spec.isolation.broker == Some(BrokerKind::AnthropicMessages) {
            for (name, contents) in [
                ("settings.json", r#"{"skipWebFetchPreflight":true}"#),
                (".claude.json", r#"{"hasCompletedOnboarding":true}"#),
            ] {
                let path = home.config().join(name);
                std::fs::write(&path, contents).map_err(|e| {
                    format!(
                        "{}: write claude {name} {}: {e}",
                        self.spec.tag,
                        path.display()
                    )
                })?;
            }
        }
        Ok(Some(home))
    }

    /// Answer Claude Code's WORKSPACE-TRUST prompt for this run's guest
    /// workdir, so an interactive session reaches a prompt instead of parking
    /// on *"Quick safety check: Is this a project you created or one you
    /// trust?"* forever.
    ///
    /// ## Why this is honestly answerable, and what it is NOT
    ///
    /// The prompt exists to protect an OPERATOR'S OWN MACHINE from a repo they
    /// downloaded: `cd` into a stranger's checkout and its `.claude/settings.json`,
    /// its hooks and its MCP servers would otherwise run against their real
    /// home directory, keys and network.
    ///
    /// None of that is the situation here, and the reason is CONTAINMENT
    /// rather than provenance — this deliberately does NOT claim the workdir's
    /// contents are safe:
    ///
    /// - The child never runs on the host. `[sandbox] runtime` is
    ///   `firecracker` and there is no second arm, so the blast radius of
    ///   "trusted" is a guest this run booted and destroys.
    /// - The guest gets no tap device at all — its whole reach is the vsock
    ///   tunnels (broker + node RPC + public, see [`wire_guest_tunnels`]), so a
    ///   project hook reaches nothing the run was not already given.
    /// - `$HOME` is never mounted (D7): the config home the trust decision is
    ///   written into IS this run's, drawn per run and deleted with it.
    /// - Above all, the process being asked is an agent already executing
    ///   model-directed commands in that sandbox. A `.claude/settings.json`
    ///   inside the workdir cannot widen a boundary the agent is already
    ///   inside — it is strictly less than what the run can do by design.
    ///
    /// So the honest claim is narrow and true: *within this sandbox, this
    /// workdir is as trusted as the run itself*. It asserts nothing about the
    /// operator's machine, because the child cannot reach it.
    ///
    /// Keyed on the GUEST path, which is why this is not folded into
    /// [`Self::prepare_config_home`]: the key is the cwd the executor actually
    /// starts in, and that path is the GUEST's (`/duck/workspace`), not the
    /// host's — so a value decided once at config-home time could be stale for
    /// a later invocation of the same run. It MERGES, so each spawn adds its
    /// own key and none of them fight.
    fn trust_guest_workdir(&self, auth: &RunAuth<'_>, guest_workdir: &Path) -> Result<(), String> {
        // the same gate the rest of the claude state files carry: codex has no
        // such prompt and no such file.
        let for_claude = self.spec.isolation.broker == Some(BrokerKind::AnthropicMessages);
        let Some(config) = auth.config_home.filter(|_| for_claude) else {
            return Ok(());
        };
        let path = config.join(".claude.json");
        let mut state: serde_json::Value = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| format!("{}: {} is not json: {e}", self.spec.tag, path.display()))?,
            // absent is not a failure: `prepare_config_home` only writes it for
            // a claude spec, and this stays correct if that ever changes.
            Err(_) => serde_json::json!({}),
        };
        state["projects"][guest_workdir.to_string_lossy().as_ref()]["hasTrustDialogAccepted"] =
            serde_json::Value::Bool(true);
        std::fs::write(&path, state.to_string()).map_err(|e| {
            format!(
                "{}: write claude trust state {}: {e}",
                self.spec.tag,
                path.display()
            )
        })
    }

    /// start this run's credential broker — `None` unless the spec declares one.
    /// the broker reads the operator's credential HERE, in the host process, and
    /// serves an endpoint the child dials with an opaque per-run bearer; dropping
    /// it (any exit path of [`Self::run_output`]) tears the endpoint down. A
    /// microVM run reaches it over a vsock tunnel, so it binds loopback and
    /// stays unreachable from anywhere but this process.
    /// `airlock` is the per-run credential source — the narrowest seam that
    /// reaches broker construction (RunAuth is built AFTER the broker, from its
    /// endpoint, so it cannot carry this). `Some` pins a consensus-resolved
    /// self-host gateway and takes precedence over `DUCKTAPE_AIRLOCK_*` env;
    /// `None` keeps the env/host-credential path unchanged.
    async fn start_broker(
        &self,
        airlock: Option<&broker::AirlockConfig>,
    ) -> Result<Option<broker::RunBroker>, String> {
        let airlock = airlock.cloned();
        let Some(kind) = self.spec.isolation.broker else {
            return Ok(None);
        };
        // LOOPBACK, always. A microVM has no network device at all: the guest
        // reaches this endpoint through a vsock tunnel whose host end dials
        // 127.0.0.1 and nothing else. Binding a routable interface — which the
        // container backend needed, because a private netns cannot reach host
        // loopback — would now widen the credential endpoint's reach for no
        // caller.
        match kind {
            BrokerKind::Pi => broker::RunBroker::start_pi(airlock).await.map(Some),
            BrokerKind::CodexResponses => broker::RunBroker::start(airlock).await.map(Some),
            BrokerKind::AnthropicMessages => {
                broker::RunBroker::start_anthropic(airlock).await.map(Some)
            }
        }
    }

    /// the run's auth env, backend-independent: the fresh config home (so the
    /// CLI cannot read the operator's real one), the way the child reaches the
    /// broker (codex: an opaque model bearer + its separately-scoped
    /// provider-control capability; claude: ANTHROPIC_BASE_URL + a `claudeAiOauth`
    /// credentials file seeded into the config home). `set` is how the caller
    /// applies one binding — the guest manifest's `env` list.
    ///
    /// NOTE what is NOT here: the REAL credential. that is the whole point — the
    /// host holds it and the broker spends it. what the child gets is only the
    /// OPAQUE per-run bearer (loopback-only, dies with the run); for claude it
    /// rides a `claudeAiOauth` file rather than `ANTHROPIC_AUTH_TOKEN` so Claude
    /// Code runs in OAuth/subscription mode (unlocking subscription-only models
    /// like Fable) instead of API mode. The credential SOURCE, not the base URL,
    /// picks the mode: an env bearer forces API mode; an OAuth file behind a
    /// custom base URL does not.
    ///
    /// The config-home binding is shared; the broker-reach bindings DIFFER by
    /// kind. codex is aimed by ARGV (see [`Self::broker_argv`]) and only needs
    /// the bearer here in [`BROKER_TOKEN_ENV`]; claude is aimed by ENV (base URL +
    /// the fresh config home) plus the seeded credentials file, with hardening
    /// vars that keep Claude Code from dialing out around the broker.
    fn apply_auth_env(
        &self,
        auth: &RunAuth<'_>,
        mut set: impl FnMut(&str, String),
    ) -> Result<(), String> {
        if let (Some(name), Some(dir)) = (
            self.spec.isolation.config_home_env.as_deref(),
            auth.config_home,
        ) {
            set(name, dir.display().to_string());
        }
        let Some(broker) = auth.broker else {
            return Ok(());
        };
        match self.spec.isolation.broker {
            Some(BrokerKind::Pi) => {
                let Some(config) = auth.config_home else {
                    return Err(format!("{}: Pi broker run has no config home", self.spec.tag));
                };
                pi::configure(config, broker, &mut set)?;
            }
            Some(BrokerKind::AnthropicMessages) => {
                set("ANTHROPIC_BASE_URL", broker.base_url.clone());
                // Seed the run bearer as a `claudeAiOauth` credentials file in the
                // config home, NOT as `ANTHROPIC_AUTH_TOKEN`. Same opaque loopback
                // bearer either way, but the OAuth-file shape makes Claude Code run
                // in subscription mode (sends `anthropic-beta: oauth-*`, offers
                // Fable) instead of API mode. `expiresAt` is far-future so the CLI
                // never refreshes — the broker serves only /v1/messages and swaps
                // in the real host-held credential upstream.
                let Some(dir) = auth.config_home else {
                    return Err(format!(
                        "{}: claude broker run has no config home to seed credentials",
                        self.spec.tag
                    ));
                };
                let creds = dir.join(".credentials.json");
                let blob = serde_json::json!({
                    "claudeAiOauth": {
                        "accessToken": broker.run_bearer,
                        "refreshToken": broker.run_bearer,
                        "expiresAt": 4102444800000_i64,
                        "scopes": ["user:inference", "user:profile"],
                        "subscriptionType": "max",
                    }
                })
                .to_string();
                std::fs::write(&creds, blob).map_err(|e| {
                    format!(
                        "{}: write claude credentials {}: {e}",
                        self.spec.tag,
                        creds.display()
                    )
                })?;
                // 0600: a credentials file, even one holding only the throwaway
                // loopback bearer (owner-only, as the old ANTHROPIC_AUTH_TOKEN env
                // was). the config home is already 0700 (create_private_dir) so no
                // other host user can reach it, but the file is mounted into the
                // container — pin it directly too. no exploitable window: the
                // container is not started until later, and the 0700 parent blocks
                // other host users meanwhile.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(&creds, std::fs::Permissions::from_mode(0o600))
                        .map_err(|e| {
                            format!(
                                "{}: restrict claude credentials {} permissions: {e}",
                                self.spec.tag,
                                creds.display()
                            )
                        })?;
                }
                // hardening: kill non-essential outbound traffic + the auto-updater
                // so the CLI never dials Anthropic around the broker.
                //
                // NOTE: CLAUDE_CODE_SUBPROCESS_ENV_SCRUB is deliberately NOT set.
                // It scrubs ANTHROPIC_* from the CLI's subprocesses, but our only
                // ANTHROPIC_* var is ANTHROPIC_BASE_URL (the loopback broker, not a
                // credential — the bearer rides the config-home file), so scrubbing
                // protects nothing. And live-verified it actively breaks the CLI:
                // headless `claude -p` refuses to run with the scrub set unless
                // allowedTools is declared ("Permission mode forced to default").
                // The real credential never enters the container at all.
                set("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".into());
                set("DISABLE_AUTOUPDATER", "1".into());
            }
            // codex (and the defensive no-kind case): argv aims it; the child gets
            // the opaque model bearer + its separately-scoped provider-control cap.
            _ => {
                set(BROKER_TOKEN_ENV, broker.run_bearer.clone());
                set(PROVIDER_CONTROL_URL_ENV, broker.control_url.clone());
                set(PROVIDER_CONTROL_TOKEN_ENV, broker.control_token.clone());
            }
        }
        Ok(())
    }

    /// point the executor at this run's broker, by splicing a custom model
    /// provider in after the subcommand selector (`args[0]`, e.g. `exec`) —
    /// where codex expects its `-c` overrides. a no-op without a broker.
    ///
    /// Model-provider argv overrides are Codex-specific. Claude and Pi use
    /// [`Self::apply_auth_env`]; Pi's headless argv additionally loads its staged
    /// Ducktape tool extension, while Claude's argv passes through unchanged.
    ///
    /// the child is given a base URL and [`BROKER_TOKEN_ENV`], and neither can
    /// recover the operator's credential: the bearer is 32 random bytes minted
    /// for this run, and the endpoint dies with it. The interactive path
    /// ([`crate::interactive`]) shares [`broker_provider_overrides`] but PREPENDS
    /// them (a TUI argv has no `exec` selector to splice after).
    fn broker_argv(&self, args: &[String], workdir: &Path, auth: &RunAuth<'_>) -> Vec<String> {
        match self.spec.isolation.broker {
            Some(BrokerKind::Pi) => {
                let mut argv = args.to_vec();
                if let Some(config) = auth.config_home {
                    argv.extend([
                        "-e".into(),
                        config.join(pi::extension(config)).display().to_string(),
                    ]);
                }
                return argv;
            }
            Some(BrokerKind::CodexResponses) => {}
            Some(BrokerKind::AnthropicMessages) | None => return args.to_vec(),
        }
        let (Some(broker), Some(selector)) = (auth.broker, args.first()) else {
            return args.to_vec();
        };
        let mut argv = vec![selector.clone()];
        argv.extend(broker_provider_overrides(broker, workdir));
        argv.extend(args.iter().skip(1).cloned());
        argv
    }

    /// the run's PATH: `ctx.path_entries` prepended to the GUEST's own
    /// ([`guest_paths::GUEST_PATH`]), which the microVM carries in its manifest
    /// env with each entry translated to its asset mountpoint.
    ///
    /// The host's PATH is not in it. A guest is a fixed filesystem that shares
    /// none of the operator's, so inheriting their `PATH` string only carried
    /// directories that do not exist there — and naming the operator's home
    /// layout while it was at it.
    ///
    /// The test-only bare harness is the other way round, because it IS a host
    /// child: declared entries, then the host's own PATH, and `None` when it
    /// declares none so the child simply inherits.
    fn run_path(&self, ctx: &RunContext) -> Result<Option<OsString>, String> {
        let invalid = |e| {
            format!(
                "run-local PATH for {} contains an invalid path entry: {e}",
                self.spec.tag
            )
        };
        if self.backend.is_bare_test() {
            if ctx.path_entries.is_empty() {
                return Ok(None);
            }
            let mut path = ctx.path_entries.clone();
            if let Some(existing) = std::env::var_os("PATH") {
                path.extend(std::env::split_paths(&existing));
            }
            return std::env::join_paths(path).map(Some).map_err(invalid);
        }
        let mut path = ctx
            .path_entries
            .iter()
            .map(|path| canonical_mount_path(path, "sandbox PATH mount"))
            .collect::<Result<Vec<_>, _>>()?;
        path.extend(std::env::split_paths(OsStr::new(
            sandbox_host::guest_paths::GUEST_PATH,
        )));
        std::env::join_paths(path).map(Some).map_err(invalid)
    }

    /// resolve the run's cwd to a WRITABLE directory, creating it.
    ///
    /// a `workdir_override` is a workspace the provisioner ALREADY materialized
    /// (the only setters are `compute-service`'s `bind_workspace`, after a
    /// successful checkout, and the agent daemon's per-session home — the
    /// envelope itself carries no host path, D7). if creating it fails, the run
    /// must FAIL so the saga can retry elsewhere: falling back would silently
    /// execute the run in `self.workdir` — a single dir shared by every run of
    /// this capability tag on the node, so concurrent runs would collide and
    /// the workspace commit would read the untouched real mount and report a
    /// clean tree (W1 violation, masked).
    ///
    /// no override = an embedder that provisions no workspace, which gets the
    /// shared scratch fence and never promised one.
    fn ensure_writable_workdir(&self, ctx: &RunContext) -> Result<PathBuf, String> {
        let Some(mount) = &ctx.workdir_override else {
            return std::fs::create_dir_all(&self.workdir)
                .map(|()| self.workdir.clone())
                .map_err(|e| format!("provider workdir {}: {e}", self.workdir.display()));
        };
        std::fs::create_dir_all(mount)
            .map(|()| mount.clone())
            .map_err(|e| {
                format!(
                    "provisioned workspace mount {} is unusable: {e}; refusing the \
                     shared scratch fallback for a portable run (W1)",
                    mount.display()
                )
            })
    }
}

/// this run's subdirectory under [`RUN_RUNTIME_DIR`]. distinct runs can share a
/// workdir (the scratch fence is shared per tag), so the slot keeps two runs
/// from stepping on each other's config home.
///
/// DRAWN FRESH PER RUN, never derived from the run's coordinates. a name derived
/// from (run key, agent, workdir) is a name a LATER run can produce again — and a
/// config home it can therefore inherit, credentials and transcripts included.
/// two runs sharing a workdir are no longer two runs of the same operator: a run
/// submitted by one account executes on another account's node, so an inherited
/// config home is one account's credential material handed to another's child.
/// randomness is what makes that unrepresentable; the cases that would collide
/// under a derived name are exactly the real ones (an unkeyed rerun, a retry of
/// the same dispatch, two concurrent runs of one agent).
fn runtime_slot() -> String {
    let mut slot = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut slot);
    slot.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// this run's private slot under [`RUN_RUNTIME_DIR`] and the config home inside
/// it, REMOVED when the run that owns it ends — every exit path (success, error,
/// timeout, panic), because a Drop runs on all of them.
///
/// the executor writes its whole session state in there: the seeded
/// `.credentials.json`, the conversation transcripts, whatever else it caches.
/// that belongs to this run alone, and the workdir it sits inside is mounted rw
/// into the sandbox and shared with later runs — of other accounts. so it is not
/// left behind for the next child to read.
///
/// the fresh name ([`runtime_slot`]) and this guard cover each other's hole: the
/// name means a leftover can never be INHERITED (nothing can name it again), the
/// guard means there is no leftover to READ through the shared workdir mount. a
/// SIGKILLed host is the one case that leaves the directory standing, unnamed by
/// anything and swept with the workdir.
struct RunHome {
    /// `<workdir>/<RUN_RUNTIME_DIR>/<slot>` — this run's, removed on drop.
    slot: PathBuf,
    /// the config home inside the slot: what the child is pointed at.
    config: PathBuf,
}

impl RunHome {
    fn config(&self) -> &Path {
        &self.config
    }
}

impl Drop for RunHome {
    fn drop(&mut self) {
        let Err(error) = std::fs::remove_dir_all(&self.slot) else {
            return;
        };
        if !removal_left_a_leak(&error) {
            return;
        }
        // the path is deliberately absent: it names a directory that held
        // credential material. the run's own workdir is the diagnosis.
        tracing::warn!(
            target: "ducktape::provider",
            reason = "config_home_not_removed",
            %error,
            "the run's config home outlived its run"
        );
    }
}

/// did a failed removal actually leave the directory standing?
///
/// ALREADY GONE is a guard's POSTCONDITION, not a failure. An interactive
/// session's owner removes the session's whole workdir when the session ends
/// and this slot lives inside it, while the `Arc<InteractiveSession>` a pump
/// task holds means the [`RunHome`] drop can run after that. Reporting it would
/// fire once per pty session and say nothing.
fn removal_left_a_leak(error: &std::io::Error) -> bool {
    error.kind() != std::io::ErrorKind::NotFound
}

/// create a directory only this user can read. the provider's config home holds
/// whatever session/state the CLI writes for the run; 0700 keeps it off other
/// local accounts even on a shared box.
fn create_private_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|e| format!("create isolated provider directory {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
            format!(
                "restrict isolated provider directory {} permissions: {e}",
                path.display()
            )
        })?;
    }
    Ok(())
}

/// where the agent CLIs appear inside the guest: a read-only image built from
/// the operator's executors directory and mounted there for the run. The host
/// executor file is never handed across — only its NAME, which is the same on
/// both sides because both come from that one directory.
const GUEST_BIN_DIR: &str = sandbox_host::guest_paths::GUEST_BIN_DIR;

/// the guest path for a host executor, by basename.
fn guest_executor_path(host_bin: &Path) -> String {
    let name = host_bin
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("executor"));
    Path::new(GUEST_BIN_DIR).join(name).display().to_string()
}

/// the guest's argv: the executor's path inside the rootfs, then the spec's
/// arguments with every host path rewritten to its guest mountpoint.
///
/// `args` is what follows the executable, NEVER argv[0] itself, so the guest
/// path is prepended. Substituting `args[0]` instead silently eats the CLI's
/// first real argument — measured, as a spec whose `args = ["-c", script]`
/// reached the guest as `sh <script>` and made the shell open the script as a
/// file. Only a run with no arguments at all is unaffected, which is exactly
/// the shape the first end-to-end test had.
fn guest_argv(bin: &Path, args: &[String], layout: &GuestLayout) -> Vec<String> {
    let mut argv = vec![guest_executor_path(bin)];
    argv.extend(args.iter().map(|arg| layout.translate(arg)));
    argv
}

/// the run's per-run directory for its block devices, boot config and console.
///
/// ON DISK, under the system temp directory. `XDG_RUNTIME_DIR` is the obvious
/// home for run-scoped state and it is the WRONG one for this: it is a tmpfs
/// sized at a fraction of RAM, and a run's images are as large as the run's
/// inputs. Measured, a run whose PATH entry was a build directory filled the
/// whole 9.1 GB of it and died with `No space left on device` — with the node's
/// memory as the thing consumed. Only the socket belongs there; see
/// [`microvm_socket`].
fn microvm_run_dir(slot: &str) -> Result<microvm::ScratchDir, String> {
    microvm::ScratchDir::create(std::env::temp_dir().join(format!("dt-vm-{slot}")))
}

/// the leaf the guest dials, inside [`microvm_socket_dir`]. Firecracker appends
/// `_<port>` to it.
const MICROVM_SOCKET_NAME: &str = "v.sock";

/// the run's vsock socket directory, which is the ONE thing that must be short.
///
/// A unix socket path is capped near 108 bytes (`SUN_LEN`), and Firecracker
/// appends `_<port>` to it. `XDG_RUNTIME_DIR` is the shortest per-user
/// directory that exists on a normal host; the node's data directory under a
/// long home blows straight through the cap, and the failure is
/// `path must be shorter than SUN_LEN` at bind time — after the images have
/// already been built.
///
/// It is a tmpfs, so a leaked directory here is the node's own RAM: the
/// returned guard is what makes a refused run cost nothing.
fn microvm_socket_dir(slot: &str) -> Result<microvm::ScratchDir, String> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    microvm::ScratchDir::create(base.join(format!("dt-vm-{slot}")))
}

/// this run's directory name, DRAWN FRESH — never derived from the run's
/// coordinates.
///
/// It used to be an FNV hash of `(executing_node, run_key)`, which reads like
/// collision avoidance and is the opposite: `run_key` is optional, so every
/// keyless run on a node hashed to the SAME name. Two concurrent ones then
/// shared one directory and overwrote each other's workspace image, asset image
/// and manifest — and once the directory is cleaned up on teardown, the first to
/// finish deletes the images out from under the other's live VM.
///
/// 8 bytes, hex: the same 16 characters the hash occupied, so the socket path
/// underneath stays the same length and the `SUN_LEN` budget is unchanged.
fn run_slot() -> String {
    let mut slot = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut slot);
    slot.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// the run's vcpu count.
///
/// A VM has no "unlimited": every VM is given a size at configuration time, so
/// a missing dimension is a config error rather than a value to guess from
/// probed host totals. Under a container an absent key meant an unlimited run;
/// that state is unrepresentable here and must not be silently invented.
fn vm_cores(limits: &BTreeMap<String, u64>) -> Result<u32, String> {
    let cores = limits.get("cores").copied().ok_or_else(|| {
        "a microVM run needs an explicit `cores` limit; a VM has no unlimited size".to_string()
    })?;
    u32::try_from(cores.max(1)).map_err(|_| format!("cores {cores} does not fit a vcpu count"))
}

fn vm_mem_mib(limits: &BTreeMap<String, u64>) -> Result<u64, String> {
    let gb = limits.get("mem_gb").copied().ok_or_else(|| {
        "a microVM run needs an explicit `mem_gb` limit; a VM has no unlimited size".to_string()
    })?;
    Ok(gb.max(1) * 1024)
}

/// Resolve every sandbox mount before handing it to a container/VM. Relative
/// paths and symlink aliases make containment checks lie about which host tree
/// is exposed, so they fail before a sandbox command is built.
fn canonical_mount_path(path: &Path, purpose: &str) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!(
            "{purpose} must be absolute, got {}",
            path.display()
        ));
    }
    std::fs::canonicalize(path)
        .map_err(|error| format!("resolve {purpose} {}: {error}", path.display()))
}

/// removes a run's context document on drop — every exit path (success, error,
/// timeout, panic). only built for a doc OUTSIDE the workdir (`workspace-parent:`):
/// it sits beside the checkout, in this run's own slug-named directory under
/// `RUN_RUNTIME_DIR` (see [`Provider::context_target`]), where nothing else
/// would ever clean it up. Removing the whole directory rather than just the
/// file means the slug directory itself does not linger empty. a `config-home:`
/// doc needs no guard of its own: it is inside the run's [`RunHome`], which
/// removes the whole directory when the run ends.
struct ContextGuard(PathBuf);

impl Drop for ContextGuard {
    fn drop(&mut self) {
        let dir = self.0.parent().unwrap_or(&self.0);
        if let Err(error) = std::fs::remove_dir_all(dir) {
            // Its twin, `RunHome::drop`, has always been a `tracing::warn` —
            // this one printed to raw stderr, which reaches neither the app's
            // Logs tab nor `RUST_LOG`.
            tracing::warn!(
                target: "ducktape::provider",
                reason = "context_doc_not_removed",
                document = %self.0.display(),
                %error,
                "the run's context document outlived its run"
            );
        }
    }
}

/// Pick the guest's tunnel allowlist AND point `envs` at it — `DUCKTAPE_NODE`
/// is rewritten to the tunnel's own end, or removed when there is no tunnel to
/// carry it. Both halves are one decision, so they are one call.
///
/// The loopback services the guest may reach, tunnelled over vsock: this run's
/// credential broker, its egress proxy, and the node lane it dials through
/// `DUCKTAPE_NODE` — which is both its read plane and, at [`MCP_PATH`], its
/// whole tool plane. The guest serves the SAME port numbers on its own
/// loopback, so `http://127.0.0.1:<port>` needs no rewriting on either side —
/// which is why the container backend's `host.containers.internal`
/// substitution is gone rather than ported.
///
/// This list IS the allowlist: the host binds one vsock listener per entry and
/// closes over the destination, so a port that is not here is not reachable
/// from the guest by any means. It is also why this is the ONE place the node
/// URL is decided: a `DUCKTAPE_NODE` whose port is not in this list names a
/// socket the guest cannot open, so [`aim_node_at_guest`] takes the variable
/// away rather than leave the run half-planed — writes landing over the
/// run-action lane while every read dies on the guest's own loopback.
///
/// **The node entry is this run's read lane, not the node's listener.** The VM
/// has no NIC, so these tunnels ARE the guest's attack surface — and the node
/// end of them terminates in [`read_lane::ReadLane`], a loopback proxy bound to
/// THIS run, which refuses `/v1/ws` outright (see that module). What is
/// reachable from any process in the guest, with no credential:
/// * the reads the plane exists for — `/v1/query`, `/v1/status`, `/v1/peers`,
///   `/v1/blocks`, `/v1/index/*`, `/metrics`, the `/v1/files/*` duckfs reads;
/// * `/v1/submit/frame` — self-authenticating: the frame's own signature IS
///   its origin, so a guest with no key can put nothing through it;
/// * `/v1/services/hello`, volatile presence that ages out on its own TTL;
/// * EGRESS OFF THIS HOST — `/v1/gateway/proxy` dispatches a `GatewayJob::Http`
///   over the overlay to a publisher node, and its only gate,
///   `gateway_http::gateway_api_origin_allowed`, is a header check a native
///   caller passes by sending no headers. `/v1/gateway/browser` likewise.
///
/// Out of reach: every other port and every other address on this host (no
/// listener is bound for them, with or without a NIC); `/v1/admin/*`; and the
/// NODE-LEVEL mutations — `/v1/invite`, `/v1/log-filter`, `/v1/term/sessions`
/// and `DELETE /v1/fs/workspaces/{id}` — which take either this node's
/// operator credential (a 0600 file in a workspace the guest has no path to)
/// or a signature by the key the node knows as its operator's
/// (`noded::signed_req`), and a run's env carries neither. The forge's
/// `git-receive-pack` takes the same two proofs in git's own shapes — a
/// `git push --signed` certificate or that operator credential in a header —
/// and a guest can present neither ITSELF: the lane presents the operator
/// credential on the guest's behalf for every push (a fetch needs no proof).
///
/// Off the host altogether, the guest has one more tunnel: its egress proxy
/// ([`egress_proxy`]), named through `HTTP_PROXY`/`HTTPS_PROXY`, which dials
/// the network for it and refuses to dial this host.
///
/// **In reach, on purpose:** the MODULE-BOUND mutations — `/v1/submit`, the
/// duckfs writes, the object facade's PUT/DELETE, `POST /v1/fs/workspaces` and
/// its commit. Those take a per-request signature by ANY key, because the
/// verified key becomes the op's `Origin::External` and the module's
/// `check_authority` is what decides. A guest can mint a keypair and sign, and
/// what it can then do is exactly what that key is authorized to do on-chain —
/// which for a fresh key is nothing.
///
/// The gateway egress above is what remains: a run's own credential-less reach
/// off this host, gated only by a header check.
fn wire_guest_tunnels(envs: &mut Vec<(String, String)>, broker_base: Option<&str>) -> Vec<u16> {
    // NOT the scoped action endpoint: the tool plane that used it runs on the
    // host now ([`read_lane`]), so tunnelling it would put this run's own
    // signer on a port inside the guest for nobody to call. See
    // [`host_only_env`], which drops its address and token from the manifest
    // env for the same reason.
    let mut ports = Vec::new();
    ports.extend(broker_base.and_then(url_port));
    if let Some((_, proxy)) = envs
        .iter()
        .find(|(key, _)| key == egress_proxy::HTTPS_PROXY_ENV)
    {
        ports.extend(url_port(proxy));
    }
    // distinct listeners on one host, so distinct ports: nothing to dedup,
    // and a duplicate would be a bug upstream rather than a collision to
    // absorb here.
    ports.extend(aim_node_at_guest(envs));
    ports
}

/// Point `DUCKTAPE_NODE` at the guest end of its tunnel, or take it away —
/// returning the host port to tunnel when there is one.
///
/// A run that keeps an unreachable node URL fails one socket at a time, deep
/// inside an agent's tool call; a run with no node URL is TOLD it has no node
/// (`bin/node`'s MCP server answers `NodeError::Unbound` by name). The second
/// is the only honest half-plane.
fn aim_node_at_guest(envs: &mut Vec<(String, String)>) -> Option<u16> {
    let index = envs.iter().position(|(key, _)| key == NODE_URL_ENV)?;
    match guest_node_url(&envs[index].1) {
        Ok((port, guest_url)) => {
            envs[index].1 = guest_url;
            Some(port)
        }
        Err(reason) => {
            // once per RUN boot — a node whose http base is not v4 loopback
            // refuses this on every sandboxed run it ever starts — and it costs
            // that run its whole read plane.
            tracing::warn!(
                target: "ducktape::sandbox",
                reason,
                "this node's http base cannot be tunnelled into a guest; the run gets no read plane"
            );
            envs.remove(index);
            None
        }
    }
}

/// The guest-side form of a node http base, and the host port that carries it.
///
/// The tunnel is loopback-to-loopback and BOTH ends are fixed: the guest binds
/// `127.0.0.1:<port>` (`duck-guest-init`'s `start_tunnel`) and the host end
/// dials `127.0.0.1:<port>` (`sandbox::microvm::serve_tunnel`). So a base that
/// names no port, or a host this node does not serve on v4 loopback, cannot be
/// carried at all — whatever it means on the host.
fn guest_node_url(url: &str) -> Result<(u16, String), &'static str> {
    let (authority, path) = split_authority(url);
    let (host, port) = authority.rsplit_once(':').ok_or("node_url_no_port")?;
    let port: u16 = port.parse().map_err(|_| "node_url_no_port")?;
    // a v6 authority keeps its brackets here (`[::1]`) and fails this parse,
    // which is the intent: `serve_tunnel` has no v6 dial to offer it.
    let host_is_v4_loopback = host
        .parse::<std::net::Ipv4Addr>()
        .is_ok_and(|ip| ip.is_loopback());
    let host_is_loopback_name = host == "localhost";
    if !(host_is_v4_loopback || host_is_loopback_name) {
        return Err("node_url_not_loopback");
    }
    Ok((port, format!("http://127.0.0.1:{port}{path}")))
}

/// the TCP port in an `http://host:port/...` URL, if any — the tunnel
/// allowlist needs the broker, run-action and node http ports as bare numbers.
fn url_port(url: &str) -> Option<u16> {
    authority_port(split_authority(url).0)
}

/// split `scheme://authority/rest` into its authority and everything from the
/// first `/` on (`""` when there is none).
fn split_authority(url: &str) -> (&str, &str) {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let end = after_scheme.find('/').unwrap_or(after_scheme.len());
    after_scheme.split_at(end)
}

fn authority_port(authority: &str) -> Option<u16> {
    authority.rsplit(':').next()?.parse().ok()
}

#[cfg(unix)]
fn configure_process_group(command: &mut tokio::process::Command) {
    use std::os::unix::process::CommandExt as _;
    command.as_std_mut().process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut tokio::process::Command) {}

#[cfg(unix)]
fn signal_process_group(group: u32, signal: libc::c_int) -> std::io::Result<()> {
    // SAFETY: kill is called with a valid signal and the negative child pid,
    // which targets only the process group created for this provider child.
    if unsafe { libc::kill(-(group as libc::pid_t), signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(unix)]
fn process_group_alive(group: u32) -> bool {
    // Signal 0 performs existence/permission checking without delivering a
    // signal. EPERM still means the group exists; ESRCH means it is gone.
    if unsafe { libc::kill(-(group as libc::pid_t), 0) } != 0 {
        return std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
    }

    // Linux keeps an orphaned zombie visible to kill(-pgid, 0) until PID 1
    // reaps it. A containerized test runner may itself be PID 1 and never reap
    // grandchildren, but zombies own no compute and cannot spawn descendants.
    // Treat a group containing only dead members as absent; any unreadable
    // /proc state stays fail-closed. Other Unix platforms retain kill(0).
    #[cfg(target_os = "linux")]
    return linux_process_group_has_live_member(group).unwrap_or(true);
    #[cfg(not(target_os = "linux"))]
    true
}

#[cfg(target_os = "linux")]
fn linux_process_group_has_live_member(group: u32) -> Option<bool> {
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries {
        let entry = entry.ok()?;
        if entry.file_name().to_str()?.parse::<u32>().is_err() {
            continue;
        }
        let stat = match std::fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        let (state, process_group) = linux_process_state_and_group(&stat)?;
        if process_group == group && !matches!(state, 'Z' | 'X' | 'x') {
            return Some(true);
        }
    }
    Some(false)
}

#[cfg(target_os = "linux")]
fn linux_process_state_and_group(stat: &str) -> Option<(char, u32)> {
    // /proc/<pid>/stat starts `pid (comm) state ppid pgrp`; `comm` may contain
    // spaces and parentheses, so split after its final closing delimiter.
    let (_, fields) = stat.rsplit_once(") ")?;
    let mut fields = fields.split_whitespace();
    let state = fields.next()?.chars().next()?;
    fields.next()?; // ppid
    let process_group = fields.next()?.parse().ok()?;
    Some((state, process_group))
}

#[cfg(unix)]
fn leader_exited_unreaped(pid: u32) -> std::io::Result<bool> {
    // WNOWAIT observes exit without consuming it. Keeping the leader as a
    // zombie pins its PID/PGID while we decide whether its exact group still
    // needs a signal; only the final Child::wait opens a reuse window.
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let info = unsafe { info.assume_init() };
    Ok(unsafe { info.si_pid() } != 0)
}

#[cfg(unix)]
async fn wait_leader_exit_unreaped(pid: u32, label: &str) {
    let mut failures = 0u64;
    loop {
        match leader_exited_unreaped(pid) {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                failures += 1;
                if failures == 1 || failures.is_multiple_of(16) {
                    tracing::warn!(
                        target: "ducktape::provider",
                        reason = "leader_exit_unobserved",
                        %label,
                        attempts = failures,
                        %error,
                        "failed to observe whether the process leader exited"
                    );
                }
            }
        }
        tokio::time::sleep(PROCESS_POLL_INTERVAL).await;
    }
}

#[cfg(unix)]
async fn wait_process_group_gone(group: u32, label: &str) {
    let mut observations = 0u64;
    while process_group_alive(group) {
        observations += 1;
        if observations == 1 || observations.is_multiple_of(160) {
            tracing::warn!(
                target: "ducktape::provider",
                reason = "process_group_lingering",
                %label,
                group,
                attempts = observations,
                "waiting for the process group to disappear"
            );
        }
        tokio::time::sleep(PROCESS_POLL_INTERVAL).await;
    }
}

#[cfg(unix)]
fn wait_process_group_gone_blocking(group: u32, label: &str) {
    let mut observations = 0u64;
    while process_group_alive(group) {
        observations += 1;
        if observations == 1 || observations.is_multiple_of(400) {
            tracing::warn!(
                target: "ducktape::provider",
                reason = "process_group_lingering",
                %label,
                group,
                attempts = observations,
                "waiting for the process group to disappear"
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

async fn wait_tokio_child_fail_closed(
    child: &mut tokio::process::Child,
    label: &str,
) -> std::process::ExitStatus {
    let mut failures = 0u64;
    loop {
        match child.wait().await {
            Ok(status) => return status,
            Err(error) => {
                failures += 1;
                if failures == 1 || failures.is_multiple_of(16) {
                    tracing::warn!(
                        target: "ducktape::provider",
                        reason = "child_wait_failed",
                        %label,
                        attempts = failures,
                        %error,
                        "failed to wait on the child process"
                    );
                }
                tokio::time::sleep(PROCESS_POLL_INTERVAL).await;
            }
        }
    }
}

#[cfg(unix)]
async fn reap_observed_child_group(
    child: &mut tokio::process::Child,
    group: u32,
    label: &str,
    leader_reaped: &mut bool,
) -> std::process::ExitStatus {
    // The leader was observed with WNOWAIT and still pins the group identity.
    // SIGKILL is a no-op for the zombie itself and removes any helper keeping
    // pipes or compute alive after the nominal command exited.
    if process_group_alive(group) {
        let _ = signal_process_group(group, libc::SIGKILL);
    }
    let status = wait_tokio_child_fail_closed(child, label).await;
    *leader_reaped = true;
    wait_process_group_gone(group, label).await;
    status
}

async fn wait_owned_child_complete(
    child: &mut tokio::process::Child,
    process_group: Option<u32>,
    label: &str,
    leader_reaped: &mut bool,
) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(unix)]
    {
        let group = process_group.ok_or_else(|| {
            std::io::Error::other(format!("{label} has no owned Unix process group"))
        })?;
        wait_leader_exit_unreaped(group, label).await;
        return Ok(reap_observed_child_group(child, group, label, leader_reaped).await);
    }
    #[cfg(not(unix))]
    {
        let _ = process_group;
        let _ = label;
        let status = child.wait().await?;
        *leader_reaped = true;
        Ok(status)
    }
}

/// Gracefully terminate the child tree, then forcibly reap anything that
/// ignored TERM. A SIGKILLed leader is always waited before return: a zombie is
/// therefore reaped, while an uninterruptible D-state leader intentionally
/// keeps this future (and its resource reservation) pending fail-closed. This is
/// the local-child (Bare test harness) path; a microVM run is killed by
/// dropping its VMM (see [`RunControl::terminate`]), never here.
async fn terminate_child(
    child: &mut tokio::process::Child,
    process_group: Option<u32>,
    leader_reaped: &mut bool,
) {
    let deadline = tokio::time::Instant::now() + TERMINATION_GRACE;
    #[cfg(unix)]
    if let Some(group) = process_group
        && let Err(error) = signal_process_group(group, libc::SIGTERM)
    {
        tracing::warn!(
            target: "ducktape::provider",
            reason = "sigterm_failed",
            group,
            %error,
            "failed to SIGTERM the provider process group"
        );
    }

    #[cfg(unix)]
    {
        if let Some(group) = process_group {
            let mut observe_failures = 0u64;
            loop {
                match leader_exited_unreaped(group) {
                    Ok(true) => {
                        // The zombie leader still pins this PGID. Kill any
                        // helper that outlived it before performing the reap.
                        if process_group_alive(group) {
                            let _ = signal_process_group(group, libc::SIGKILL);
                        }
                        break;
                    }
                    Ok(false) if tokio::time::Instant::now() < deadline => {}
                    Ok(false) => {
                        if let Err(error) = signal_process_group(group, libc::SIGKILL) {
                            tracing::warn!(
                                target: "ducktape::provider",
                                reason = "sigkill_failed",
                                group,
                                %error,
                                "failed to SIGKILL the provider process group"
                            );
                        }
                        let _ = child.start_kill();
                        break;
                    }
                    Err(error) => {
                        // ECHILD or an unreadable wait state makes ownership
                        // unverifiable. Retain the reservation fail-closed.
                        observe_failures += 1;
                        if observe_failures == 1 || observe_failures.is_multiple_of(16) {
                            tracing::warn!(
                                target: "ducktape::provider",
                                reason = "leader_observe_failed",
                                group,
                                attempts = observe_failures,
                                %error,
                                "failed to observe the cancelled provider leader"
                            );
                        }
                    }
                }
                tokio::time::sleep(PROCESS_POLL_INTERVAL).await;
            }
            let _ = wait_tokio_child_fail_closed(child, "killed provider child").await;
            *leader_reaped = true;
            // Never signal after reap: only observe until the exact old group
            // is gone, so PID reuse cannot turn cleanup into an unrelated kill.
            wait_process_group_gone(group, "provider").await;
        } else {
            let _ = child.start_kill();
            let _ = wait_tokio_child_fail_closed(child, "killed provider child").await;
            *leader_reaped = true;
        }
    }

    #[cfg(not(unix))]
    {
        let _ = child.start_kill();
        let _ = wait_tokio_child_fail_closed(child, "killed provider child").await;
        *leader_reaped = true;
    }
}

async fn cancellation_requested(cancellation: Option<&RunCancellation>) {
    match cancellation {
        Some(cancellation) => cancellation.cancelled().await,
        None => std::future::pending().await,
    }
}

struct GroupChild {
    child: Option<tokio::process::Child>,
    process_group: Option<u32>,
    leader_reaped: bool,
    cleaned: bool,
}

impl GroupChild {
    fn kill_and_wait_blocking(&mut self) {
        if self.cleaned {
            return;
        }
        let Some(child) = self.child.as_mut() else {
            return;
        };
        #[cfg(unix)]
        if self.leader_reaped {
            if let Some(group) = self.process_group {
                // The PGID may now be reused. Never signal it; retain the
                // reservation until absence is observed fail-closed.
                wait_process_group_gone_blocking(group, "reaped setup");
            }
            self.cleaned = true;
            return;
        }
        #[cfg(unix)]
        if let Some(group) = self.process_group {
            let mut inspect_failures = 0u64;
            loop {
                match leader_exited_unreaped(group) {
                    Ok(_) => {
                        // Both a live and a zombie leader pin this exact PGID.
                        let _ = signal_process_group(group, libc::SIGKILL);
                        let _ = child.start_kill();
                        break;
                    }
                    Err(error) => {
                        inspect_failures += 1;
                        if inspect_failures == 1 || inspect_failures.is_multiple_of(16) {
                            tracing::warn!(
                                target: "ducktape::provider",
                                reason = "inspect_setup_child_failed",
                                group,
                                attempts = inspect_failures,
                                %error,
                                "failed to inspect the setup child before killing it"
                            );
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }
            let mut wait_failures = 0u64;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                    Err(error) => {
                        wait_failures += 1;
                        if wait_failures == 1 || wait_failures.is_multiple_of(16) {
                            tracing::warn!(
                                target: "ducktape::provider",
                                reason = "reap_setup_child_failed",
                                group,
                                attempts = wait_failures,
                                %error,
                                "failed to reap the killed setup child"
                            );
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }
            self.leader_reaped = true;
            wait_process_group_gone_blocking(group, "setup");
            self.cleaned = true;
            return;
        }

        #[cfg(not(unix))]
        {
            let _ = child.start_kill();
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) | Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
            self.cleaned = true;
            return;
        }

        #[cfg(unix)]
        let mut inspect_failures = 0u64;
        #[cfg(unix)]
        loop {
            match child.try_wait() {
                Ok(Some(_)) => {
                    self.leader_reaped = true;
                    self.cleaned = true;
                    return;
                }
                Ok(None) => break,
                Err(error) => {
                    inspect_failures += 1;
                    if inspect_failures == 1 || inspect_failures.is_multiple_of(16) {
                        tracing::warn!(
                            target: "ducktape::provider",
                            reason = "inspect_setup_child_failed",
                            attempts = inspect_failures,
                            %error,
                            "failed to inspect the setup child before killing it"
                        );
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        #[cfg(unix)]
        let _ = child.start_kill();
        #[cfg(unix)]
        let mut wait_failures = 0u64;
        #[cfg(unix)]
        loop {
            match child.try_wait() {
                Ok(Some(_)) => {
                    self.leader_reaped = true;
                    self.cleaned = true;
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(error) => {
                    wait_failures += 1;
                    if wait_failures == 1 || wait_failures.is_multiple_of(16) {
                        tracing::warn!(
                            target: "ducktape::provider",
                            reason = "reap_setup_child_failed",
                            attempts = wait_failures,
                            %error,
                            "failed to reap the killed setup child"
                        );
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
}

impl Drop for GroupChild {
    fn drop(&mut self) {
        if !self.cleaned {
            self.kill_and_wait_blocking();
        }
    }
}

/// Drop owner for one live provider invocation. This is intentionally
/// synchronous on unexpected future destruction: returning from Drop would
/// release the outer dispatch reservation while descendants or a container
/// could still consume it.
struct LiveChild {
    process: GroupChild,
    cleaned: bool,
}

impl LiveChild {
    fn new(child: tokio::process::Child) -> Self {
        let process_group = child.id();
        Self {
            process: GroupChild {
                child: Some(child),
                process_group,
                leader_reaped: false,
                cleaned: false,
            },
            cleaned: false,
        }
    }

    fn child_mut(&mut self) -> &mut tokio::process::Child {
        self.process
            .child
            .as_mut()
            .expect("a live invocation owns its child")
    }

    /// The child's process-group leader pid (its own pid), for observing exit
    /// without reaping — the reap stays with [`Self::terminate`].
    pub(crate) fn leader_pid(&self) -> Option<u32> {
        self.process.process_group
    }

    async fn terminate(&mut self) {
        let process_group = self.process.process_group;
        if self.process.cleaned {
            // already cleaned
        } else if self.process.leader_reaped {
            #[cfg(unix)]
            if let Some(group) = process_group {
                wait_process_group_gone(group, "provider").await;
            }
            self.process.cleaned = true;
        } else {
            let child = self
                .process
                .child
                .as_mut()
                .expect("a live invocation owns its child");
            terminate_child(child, process_group, &mut self.process.leader_reaped).await;
        }
        self.process.cleaned = true;
        self.cleaned = true;
    }

    async fn wait_complete(&mut self, label: &str) -> std::io::Result<std::process::ExitStatus> {
        let process_group = self.process.process_group;
        let child = self
            .process
            .child
            .as_mut()
            .expect("a live invocation owns its child");
        let status =
            wait_owned_child_complete(child, process_group, label, &mut self.process.leader_reaped)
                .await?;
        self.process.cleaned = true;
        self.cleaned = true;
        Ok(status)
    }
}

impl Drop for LiveChild {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        self.process.kill_and_wait_blocking();
        self.cleaned = true;
    }
}

/// how [`CliProvider::invoke`] waits for and tears down a run, unifying the two
/// backends: a `Local` child (the Bare test harness), reaped through its
/// process group; or a `MicroVm`, waited on the guest's own exit frame and
/// torn down by dropping its VMM. The output loop drives whichever one this is
/// identically.
enum RunControl {
    Local(LiveChild),
    /// boxed: the handle carries the VM, its io pump and the guest's tunnels,
    /// and a run holds exactly one of these.
    MicroVm(Box<MicroVmHandle>),
}

/// a running microVM: the VMM child, the guest's exit channel, the background
/// vsock pump, and where the workspace has to be written back to.
///
/// There is no image to remove and no daemon to tell — the VMM is a child of
/// this process and dies with it. After the guest reports its exit, the caller
/// collects the workspace before releasing a completed invocation.
struct MicroVmHandle {
    vm: microvm::MicroVm,
    exit: Option<tokio::sync::oneshot::Receiver<i32>>,
    pump: Option<tokio::task::JoinHandle<()>>,
    /// the HOST directory the run's workspace image is walked back into.
    workdir: PathBuf,
    /// held for the VM's lifetime: the guest's node lane and egress proxy,
    /// which must not outlive the guest they answer.
    _lanes: GuestLanes,
}

/// the host ends of a guest's tunnels that this crate itself terminates: the
/// node lane behind `DUCKTAPE_NODE`, and the egress proxy behind
/// `HTTPS_PROXY`. Each lives exactly as long as the run holding it.
pub(crate) struct GuestLanes {
    _read_lane: Option<read_lane::ReadLane>,
    _egress: egress_proxy::EgressProxy,
}

impl RunControl {
    /// kill (if still running) and remove the run, on every error/cancel/timeout
    /// path. Idempotent enough to call once at teardown.
    async fn terminate(&mut self) {
        match self {
            RunControl::Local(live) => live.terminate().await,
            RunControl::MicroVm(handle) => {
                handle.vm.terminate().await;
                if let Some(pump) = handle.pump.take() {
                    pump.abort();
                }
            }
        }
    }

    /// wait for exit; returns `(success, exit_description)`.
    ///
    /// For a microVM this waits for the guest's exit frame. Workspace collection
    /// then waits for the guest to sync, unmount and halt before reading back.
    async fn wait_success(&mut self, label: &str) -> std::io::Result<(bool, String)> {
        match self {
            RunControl::Local(live) => {
                let status = live.wait_complete(label).await?;
                Ok((status.success(), status.to_string()))
            }
            RunControl::MicroVm(handle) => {
                // The guest's own exit frame, not the VMM's status: the
                // hypervisor exits 0 for a guest that returned 1.
                let exit = handle
                    .exit
                    .take()
                    .ok_or_else(|| std::io::Error::other("microVM already waited on"))?;
                let code = exit.await.map_err(|_| {
                    std::io::Error::other(format!(
                        "{label} guest halted without reporting an exit code"
                    ))
                })?;
                if let Some(pump) = handle.pump.take() {
                    pump.abort();
                }
                // THE run event, and the only `info` this lane spends: one line
                // per run, carrying the one fact every other diagnosis starts
                // from. A run spans many blocks, so this cannot fire more than
                // once per block per run.
                tracing::info!(
                    target: "ducktape::sandbox",
                    event = "sandbox_run_finished",
                    run = %label,
                    exit_code = code,
                    "the guest's child exited"
                );
                Ok((code == 0, format!("exit code {code}")))
            }
        }
    }

    /// walk a finished run's workspace image back onto the host. Separate from
    /// [`Self::wait_success`] so a caller that is abandoning the run does not
    /// pay for it — and so the ordering (VMM gone, then read) is owned in one
    /// place.
    async fn collect_workspace(self) -> Result<(), String> {
        match self {
            RunControl::Local(_) => Ok(()),
            RunControl::MicroVm(handle) => {
                let handle = *handle;
                handle.vm.collect(&handle.workdir).await
            }
        }
    }
}

/// one finished invocation: the parsed answer and its token usage.
struct Invocation {
    text: String,
    usage: Option<TokenUsage>,
    disposition: OutputDisposition,
}

/// append one raw chunk to `pending` and forward every newline-completed
/// line to the sink (strips `\n`/`\r\n`, lossy on invalid utf-8 — matching
/// the final-output accumulation). a no-op without a sink.
fn forward_lines(
    pending: &mut Vec<u8>,
    chunk: &[u8],
    stream: OutputStream,
    sink: &Option<OutputSink>,
    ctx: &RunContext,
) {
    let Some(sink) = sink else { return };
    pending.extend_from_slice(chunk);
    while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
        let mut line: Vec<u8> = pending.drain(..=pos).collect();
        line.pop(); // the newline
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        sink(
            ctx,
            OutputLine {
                stream,
                line: String::from_utf8_lossy(&line).into_owned(),
            },
        );
    }
}

/// the stream closed: a trailing newline-less line still reaches the sink.
fn flush_pending_line(
    pending: &mut Vec<u8>,
    stream: OutputStream,
    sink: &Option<OutputSink>,
    ctx: &RunContext,
) {
    let Some(sink) = sink else {
        pending.clear();
        return;
    };
    if pending.is_empty() {
        return;
    }
    let line = std::mem::take(pending);
    sink(
        ctx,
        OutputLine {
            stream,
            line: String::from_utf8_lossy(&line).into_owned(),
        },
    );
}

/// append `chunk` to `buf`, keeping only the last `cap` bytes: a bounded
/// rolling tail rather than a truncation trigger. Used for stderr, which is
/// never the run's answer and only ever surfaces as [`excerpt`]'s short slice.
fn push_bounded_tail(buf: &mut Vec<u8>, chunk: &[u8], cap: usize) {
    buf.extend_from_slice(chunk);
    if buf.len() > cap {
        buf.drain(..buf.len() - cap);
    }
}

fn effective_provider_deadline(
    last_activity: tokio::time::Instant,
    idle: Duration,
    explicit: Option<tokio::time::Instant>,
    hard: tokio::time::Instant,
) -> tokio::time::Instant {
    (last_activity + idle)
        .max(explicit.unwrap_or(last_activity + idle))
        .min(hard)
}

impl CliProvider {
    /// one child process, start to parsed answer, with an explicit argv and
    /// working directory — the shared engine under the cold and resume paths.
    async fn invoke(
        &self,
        prompt: &str,
        args: &[String],
        workdir: &Path,
        ctx: &RunContext,
        config_home: Option<&Path>,
        broker: Option<&broker::RunBroker>,
    ) -> Result<Invocation, String> {
        if ctx
            .cancellation
            .as_ref()
            .is_some_and(RunCancellation::is_cancelled)
        {
            return Err(format!("{} cancelled before spawn", self.bin.display()));
        }
        let broker_invocation = broker.map(broker::RunBroker::begin_invocation);
        let auth = RunAuth {
            config_home,
            broker: broker_invocation
                .as_ref()
                .map(|invocation| &invocation.endpoint),
        };
        let idle = self.timeout;
        let hard = tokio::time::Instant::now() + idle.saturating_mul(self.hard_timeout_factor);
        if let Some(invocation) = &broker_invocation {
            invocation.arm(hard);
        }
        // backend split. Firecracker boots a microVM and carries its stdio over
        // vsock; the test-only bare harness spawns a local child. Both expose
        // the run's stdio as boxed streams so the refreshable-timeout output
        // loop below is byte-identical, and both yield a `RunControl` that
        // knows how to wait for exit and terminate.
        type BoxRead = Box<dyn tokio::io::AsyncRead + Send + Unpin>;
        type BoxWrite = Box<dyn tokio::io::AsyncWrite + Send + Unpin>;
        let (mut stdin, mut stdout_pipe, mut stderr_pipe, mut control): (
            BoxWrite,
            BoxRead,
            BoxRead,
            RunControl,
        ) = if matches!(self.backend, SandboxBackend::MicroVm { .. }) {
            let final_args = self.broker_argv(args, workdir, &auth);
            let (vm, io, lanes) = self
                .microvm_boot(&final_args, workdir, ctx, &auth, GuestStdio::Pipes)
                .await?;
            (
                Box::new(io.stdin),
                Box::new(io.stdout),
                Box::new(io.stderr),
                RunControl::MicroVm(Box::new(MicroVmHandle {
                    vm,
                    exit: Some(io.exit),
                    pump: Some(io.pump),
                    workdir: workdir.to_path_buf(),
                    _lanes: lanes,
                })),
            )
        } else {
            let mut command = self.prepared_command(args, workdir, ctx, &auth)?;
            command
                .current_dir(workdir)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            configure_process_group(&mut command);
            let child = command
                .spawn()
                .map_err(|e| format!("spawn {} failed: {e}", self.bin.display()))?;
            let mut live = LiveChild::new(child);
            let stdin = live
                .child_mut()
                .stdin
                .take()
                .ok_or_else(|| "child stdin was not piped".to_string())?;
            let stdout = live
                .child_mut()
                .stdout
                .take()
                .ok_or_else(|| "child stdout was not piped".to_string())?;
            let stderr = live
                .child_mut()
                .stderr
                .take()
                .ok_or_else(|| "child stderr was not piped".to_string())?;
            (
                Box::new(stdin),
                Box::new(stdout),
                Box::new(stderr),
                RunControl::Local(live),
            )
        };

        let protocol = match self.spec.output {
            OutputFormat::CodexSession => Some(run_session::Protocol::Codex),
            OutputFormat::ClaudeSession => Some(run_session::Protocol::Claude),
            // Pi is a one-shot `--print` run whose events arrive on stdout; it
            // drives no bidirectional session protocol.
            OutputFormat::PiJson
            | OutputFormat::JsonlEvents
            | OutputFormat::JsonResult
            | OutputFormat::Text => None,
        };
        if let Some(protocol) = protocol {
            let result = run_session::drive(
                protocol,
                prompt,
                (stdin, stdout_pipe, stderr_pipe),
                ctx,
                self.output_sink.clone(),
                (idle, hard),
                broker_invocation.as_ref(),
            )
            .await;
            if let Some(invocation) = &broker_invocation {
                invocation.revoke();
            }
            if result.is_err() {
                control.terminate().await;
                return result;
            }
            let exited = tokio::time::timeout(
                Duration::from_secs(10),
                control.wait_success("provider session"),
            )
            .await;
            match exited {
                Ok(Ok((true, _))) => {
                    control.collect_workspace().await?;
                    return result;
                }
                Ok(Ok((false, code))) => {
                    control.terminate().await;
                    return Err(format!("provider session exited unsuccessfully: {code:?}"));
                }
                Ok(Err(error)) => {
                    control.terminate().await;
                    return Err(error.to_string());
                }
                Err(_) => {
                    control.terminate().await;
                    return Err("provider session did not exit after its result".into());
                }
            }
        }

        // feed the prompt CONCURRENTLY with collecting output: a prompt larger
        // than the pipe buffer would deadlock a sequential write-then-wait if
        // the CLI streams output before draining stdin.
        let feed = async {
            stdin.write_all(prompt.as_bytes()).await?;
            stdin.shutdown().await?;
            drop(stdin); // EOF: the prompt is complete
            Ok::<(), std::io::Error>(())
        };
        // the live tail rides the SAME chunk reads the refreshable timeout
        // watches: complete lines forward to the sink as they arrive, a
        // trailing partial line flushes when its stream closes.
        let output_sink = self.output_sink.clone();
        let mut out_pending: Vec<u8> = Vec::new();
        let mut err_pending: Vec<u8> = Vec::new();

        // REFRESHABLE timeout: `self.timeout` is an IDLE window, not a wall
        // clock — any child output (either stream) refreshes it, so a
        // long-running agentic loop that keeps streaming events (codex --json)
        // or emitting tool output is never killed mid-work; only a SILENT
        // child dies at the window. a CLI that is quiet by design (claude -p
        // prints one result object at the end) keeps exactly the old
        // semantics: its silence budget is the spec's timeout. the hard
        // ceiling (the spec's `hard_timeout_factor` × idle) guards this host's
        // resources against a chatty-forever child; the RUN's outcome is
        // bounded by the saga's consensus deadline regardless.
        let mut explicit_deadline = broker_invocation
            .as_ref()
            .map(|invocation| invocation.idle_deadline.clone());
        let mut feed = std::pin::pin!(feed);
        let mut fed: Option<Result<(), std::io::Error>> = None;
        let mut out_bytes: Vec<u8> = Vec::new();
        let mut err_bytes: Vec<u8> = Vec::new();
        let (mut out_open, mut err_open) = (true, true);
        let mut obuf = [0u8; 8192];
        let mut ebuf = [0u8; 8192];
        let mut last_activity = tokio::time::Instant::now();
        while out_open || err_open {
            let explicit = explicit_deadline
                .as_ref()
                .and_then(|deadline| *deadline.borrow());
            let deadline = effective_provider_deadline(last_activity, idle, explicit, hard);
            tokio::select! {
                r = &mut feed, if fed.is_none() => fed = Some(r),
                r = stdout_pipe.read(&mut obuf), if out_open => match r {
                    Ok(0) => {
                        out_open = false;
                        flush_pending_line(&mut out_pending, OutputStream::Stdout, &output_sink, ctx);
                    }
                    Ok(n) => {
                        out_bytes.extend_from_slice(&obuf[..n]);
                        forward_lines(&mut out_pending, &obuf[..n], OutputStream::Stdout, &output_sink, ctx);
                        last_activity = tokio::time::Instant::now();
                        if out_bytes.len() > MAX_RUN_OUTPUT_BYTES {
                            if let Some(invocation) = &broker_invocation {
                                invocation.revoke();
                            }
                            control.terminate().await;
                            tracing::warn!(
                                target: "ducktape::provider",
                                reason = "output_cap_exceeded",
                                bin = %self.bin.display(),
                                bytes = out_bytes.len(),
                                cap = MAX_RUN_OUTPUT_BYTES,
                                "run stdout exceeded the output cap (child killed)"
                            );
                            return Err(format!(
                                "{} stdout exceeded the {MAX_RUN_OUTPUT_BYTES}-byte output cap \
                                 (child killed): output_cap_exceeded",
                                self.bin.display()
                            ));
                        }
                    }
                    Err(e) => {
                        if let Some(invocation) = &broker_invocation {
                            invocation.revoke();
                        }
                        control.terminate().await;
                        return Err(format!(
                            "reading {} stdout failed: {e}",
                            self.bin.display()
                        ));
                    }
                },
                r = stderr_pipe.read(&mut ebuf), if err_open => match r {
                    Ok(0) => {
                        err_open = false;
                        flush_pending_line(&mut err_pending, OutputStream::Stderr, &output_sink, ctx);
                    }
                    Ok(n) => {
                        push_bounded_tail(&mut err_bytes, &ebuf[..n], MAX_RUN_STDERR_BYTES);
                        forward_lines(&mut err_pending, &ebuf[..n], OutputStream::Stderr, &output_sink, ctx);
                        last_activity = tokio::time::Instant::now();
                    }
                    Err(e) => {
                        if let Some(invocation) = &broker_invocation {
                            invocation.revoke();
                        }
                        control.terminate().await;
                        return Err(format!(
                            "reading {} stderr failed: {e}",
                            self.bin.display()
                        ));
                    }
                },
                _ = cancellation_requested(ctx.cancellation.as_ref()) => {
                    if let Some(invocation) = &broker_invocation {
                        invocation.revoke();
                    }
                    control.terminate().await;
                    return Err(format!(
                        "{} cancelled (child terminated)",
                        self.bin.display()
                    ));
                },
                changed = async {
                    match explicit_deadline.as_mut() {
                        Some(deadline) => deadline.changed().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if changed.is_err() {
                        explicit_deadline = None;
                    }
                },
                _ = tokio::time::sleep_until(deadline) => {
                    if ctx
                        .cancellation
                        .as_ref()
                        .is_some_and(RunCancellation::is_cancelled)
                    {
                        if let Some(invocation) = &broker_invocation {
                            invocation.revoke();
                        }
                        control.terminate().await;
                        return Err(format!(
                            "{} cancelled (child terminated)",
                            self.bin.display()
                        ));
                    }
                    // A control grant and the old timer can become ready in
                    // the same scheduler turn. The timer wake is provisional:
                    // re-check under the controller's own mutex so either its
                    // grant wins or this timeout revokes before it can grant.
                    let now = tokio::time::Instant::now();
                    let should_continue = broker_invocation.as_ref().map_or_else(
                        || {
                            now < effective_provider_deadline(
                                last_activity,
                                idle,
                                explicit_deadline
                                    .as_ref()
                                    .and_then(|deadline| *deadline.borrow()),
                                hard,
                            )
                        },
                        |invocation| {
                            invocation.continue_after_timeout_wake(
                                last_activity,
                                idle,
                                hard,
                                now,
                            )
                        },
                    );
                    if should_continue {
                        continue;
                    }
                    control.terminate().await;
                    return Err(if now >= hard {
                        format!(
                            "{} timed out: still running at the hard cap of {:?} \
                             ({}x the idle window; child killed)",
                            self.bin.display(),
                            idle.saturating_mul(self.hard_timeout_factor),
                            self.hard_timeout_factor
                        )
                    } else {
                        format!(
                            "{} timed out after {:?} with no output (child killed); \
                             an actively-streaming run refreshes this window",
                            self.bin.display(),
                            idle
                        )
                    });
                }
            }
        }
        // both streams closed: the child is done (or moments from it) — a
        // bounded wait, never indefinite.
        if let Some(invocation) = &broker_invocation {
            invocation.revoke();
        }
        enum WaitOutcome {
            Exited(std::io::Result<(bool, String)>),
            Cancelled,
            TimedOut,
        }
        let label = self.bin.display().to_string();
        let (success, exit_desc) = match tokio::select! {
            outcome = control.wait_success(&label) => {
                WaitOutcome::Exited(outcome)
            },
            _ = cancellation_requested(ctx.cancellation.as_ref()) => WaitOutcome::Cancelled,
            _ = tokio::time::sleep(idle) => WaitOutcome::TimedOut,
        } {
            WaitOutcome::Exited(Ok(pair)) => pair,
            WaitOutcome::Exited(Err(error)) => {
                if let Some(invocation) = &broker_invocation {
                    invocation.revoke();
                }
                control.terminate().await;
                return Err(format!("waiting on {} failed: {error}", self.bin.display()));
            }
            WaitOutcome::Cancelled => {
                if let Some(invocation) = &broker_invocation {
                    invocation.revoke();
                }
                control.terminate().await;
                return Err(format!(
                    "{} cancelled (child terminated)",
                    self.bin.display()
                ));
            }
            WaitOutcome::TimedOut => {
                if let Some(invocation) = &broker_invocation {
                    invocation.revoke();
                }
                control.terminate().await;
                return Err(format!(
                    "{} closed its output but did not exit within {idle:?} (child killed)",
                    self.bin.display()
                ));
            }
        };
        // The run is over and the VMM is gone, so the workspace image can be
        // walked back onto the host. Done BEFORE the failure check on purpose:
        // a run that exited non-zero still produced work — partial edits, a
        // half-written file, a log — and throwing that away because the exit
        // code was 1 loses the buyer's own data along with the diagnosis.
        control.collect_workspace().await?;

        // an unfinished feed at this point means the child exited without
        // draining stdin — the exit status below is the primary diagnostic.
        let fed = fed.unwrap_or(Ok(()));

        if !success {
            // a failed exit is the primary diagnostic — it subsumes any
            // stdin write error (an early-exiting child EPIPEs the feed).
            return Err(format!(
                "{} exited with {exit_desc}: {}",
                self.bin.display(),
                excerpt(&String::from_utf8_lossy(&err_bytes))
            ));
        }
        if let Err(e) = fed {
            return Err(format!(
                "writing the prompt to {} failed: {e}",
                self.bin.display()
            ));
        }
        let stdout = String::from_utf8_lossy(&out_bytes).into_owned();
        let (text, usage, disposition) = match self.spec.output {
            OutputFormat::PiJson => {
                let output = pi::parse_output(&stdout)?;
                (output.text, output.usage, output.disposition)
            }
            OutputFormat::JsonlEvents => (
                parse_jsonl_events(&stdout)?,
                parse_token_usage(&stdout),
                OutputDisposition::Answer,
            ),
            OutputFormat::JsonResult => (
                parse_json_result(&stdout)?,
                parse_token_usage(&stdout),
                OutputDisposition::Answer,
            ),
            // Plain stdout is model-authored answer text, not a provider
            // telemetry envelope. Never infer usage from answer content.
            OutputFormat::Text => (parse_text_output(&stdout)?, None, OutputDisposition::Answer),
            OutputFormat::CodexSession | OutputFormat::ClaudeSession => {
                unreachable!("session driver returned above")
            }
        };
        let unexpected_native_terminal = disposition != OutputDisposition::Answer
            && ctx.native_conversation.is_none();
        if unexpected_native_terminal {
            return Err("provider returned a native terminal result outside a conversation".into());
        }
        Ok(Invocation {
            text,
            usage,
            disposition,
        })
    }

    async fn run_output(&self, prompt: &str, ctx: &RunContext) -> Result<ProviderOutput, String> {
        if ctx
            .cancellation
            .as_ref()
            .is_some_and(RunCancellation::is_cancelled)
        {
            return Err(format!("{} cancelled before start", self.bin.display()));
        }
        let workdir = self.ensure_writable_workdir(ctx)?;
        let workdir = if self.backend.is_bare_test() {
            workdir
        } else {
            canonical_mount_path(&workdir, "sandbox workdir")?
        };
        // the run's auth materials, prepared once and shared by every invocation
        // below (a resume and its cold retry are the SAME run — one config home,
        // one broker). `home` and `broker` are held here so the endpoint and the
        // config home outlive the child and are torn down when this call returns,
        // however it returns.
        let home = self.prepare_config_home(&workdir)?;
        let config_home = home.as_ref().map(RunHome::config);
        // the assembled soul, delivered by whichever door the SPEC names: a file
        // the CLI auto-loads, or the stdin prompt. the guard (held for the whole
        // call) removes a doc that lives outside the workdir, on every exit path.
        let _context = self.deliver_context(&workdir, config_home, ctx)?;
        let prompt_buf = match &ctx.native_conversation {
            Some(conversation) => {
                if self.spec.isolation.broker != Some(BrokerKind::Pi) {
                    return Err("native conversations require the Pi provider".into());
                }
                let config = config_home
                    .ok_or("native conversation requires a fresh Pi config home")?;
                pi::prepare_conversation(config, conversation, prompt)?
            }
            None => self.prompt_with_context(prompt, ctx),
        };
        let prompt = prompt_buf.as_str();
        // the per-run credential source rides `ctx.airlock` (unifies the
        // headless `sched --cred` and peer-attached spawn paths); `None` for
        // every existing headless run, so the env/host-credential path is
        // unchanged. A present config takes precedence over env.
        let broker = self.start_broker(ctx.airlock.as_ref()).await?;

        // One invocation per execution attempt. A conversation's native SDK
        // transport restores network history; an ordinary run remains cold.
        let run = self
            .invoke(
                prompt,
                &self.spec.args,
                &workdir,
                ctx,
                config_home,
                broker.as_ref(),
            )
            .await?;
        Ok(ProviderOutput {
            text: run.text,
            usage: run.usage,
            disposition: run.disposition,
        })
    }
}

#[async_trait::async_trait]
impl Provider for CliProvider {
    fn capability(&self) -> &str {
        &self.spec.tag
    }

    async fn run(&self, prompt: &str, ctx: &RunContext) -> Result<String, String> {
        self.run_output(prompt, ctx).await.map(|output| output.text)
    }

    async fn run_with_usage(
        &self,
        prompt: &str,
        ctx: &RunContext,
    ) -> Result<ProviderOutput, String> {
        self.run_output(prompt, ctx).await
    }

    #[cfg(unix)]
    async fn spawn_interactive(
        &self,
        ctx: &RunContext,
        restricted: bool,
    ) -> Result<InteractiveSession, String> {
        self.spawn_interactive_session(ctx, restricted).await
    }
}

/// the JSON objects of a JSONL-ish stream: trimmed lines that parse; non-json
/// noise skipped. shared by the output parser and the session capture.
pub(crate) fn json_lines(stdout: &str) -> impl Iterator<Item = Value> + '_ {
    stdout.lines().filter_map(|line| {
        let line = line.trim();
        if !line.starts_with('{') {
            return None;
        }
        serde_json::from_str::<Value>(line).ok()
    })
}

/// the candidate `{"type":"result",..}` objects of a single-JSON-result print
/// mode: the whole trimmed output first, then per-line for robustness against
/// banner noise. shared by the output parser and the session capture.
pub(crate) fn result_objects(stdout: &str) -> impl Iterator<Item = Value> + '_ {
    std::iter::once(stdout.trim())
        .chain(stdout.lines().rev().map(str::trim))
        .filter_map(|candidate| serde_json::from_str::<Value>(candidate).ok())
        .filter(|v| v.get("type").and_then(Value::as_str) == Some("result"))
}

fn token_usage_from(value: &Value) -> Option<TokenUsage> {
    let usage = value.get("usage")?.as_object()?;
    let read = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let has_tokens = [
        "input_tokens",
        "cached_input_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
        "output_tokens",
        "reasoning_output_tokens",
    ]
    .iter()
    .any(|key| usage.contains_key(*key));
    if !has_tokens {
        return None;
    }

    let raw_input = read("input_tokens");
    let cached_input = usage
        .get("cached_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| read("cache_read_input_tokens"));
    let cache_write = read("cache_creation_input_tokens");
    // Codex reports cached_input_tokens as a subset of input_tokens. Claude's
    // cache read/write counters are additional to its input_tokens field.
    let input_tokens = if usage.contains_key("cached_input_tokens") {
        raw_input
    } else {
        raw_input
            .saturating_add(cached_input)
            .saturating_add(cache_write)
    };
    Some(TokenUsage {
        input_tokens,
        cached_input_tokens: cached_input,
        cache_write_input_tokens: cache_write,
        output_tokens: read("output_tokens"),
        reasoning_output_tokens: read("reasoning_output_tokens"),
    })
}

fn parse_token_usage(stdout: &str) -> Option<TokenUsage> {
    json_lines(stdout)
        .filter_map(|value| token_usage_from(&value))
        .last()
        .or_else(|| {
            result_objects(stdout)
                .filter_map(|value| token_usage_from(&value))
                .next()
        })
}

/// the LAST agent message in a JSONL event stream. tolerant of
/// the two shapes the CLI has shipped (item events with `type` or `item_type`,
/// and the older `msg` envelope) and of non-json noise lines; anything else is
/// an explicit error carrying an output excerpt, never a silent empty answer.
fn parse_jsonl_events(stdout: &str) -> Result<String, String> {
    let mut last: Option<String> = None;
    for v in json_lines(stdout) {
        if let Some(item) = v.get("item") {
            let kind = item
                .get("type")
                .or_else(|| item.get("item_type"))
                .and_then(Value::as_str);
            if kind == Some("agent_message")
                && let Some(text) = item.get("text").and_then(Value::as_str)
            {
                last = Some(text.to_string());
            }
        }
        if let Some(msg) = v.get("msg")
            && msg.get("type").and_then(Value::as_str) == Some("agent_message")
            && let Some(text) = msg.get("message").and_then(Value::as_str)
        {
            last = Some(text.to_string());
        }
    }
    last.ok_or_else(|| {
        format!(
            "executor event stream carried no agent message: {}",
            excerpt(stdout)
        )
    })
}

/// the result text of a single-JSON-result print mode: one result object,
/// whole-output first, then per-line for robustness against banner noise. an
/// `is_error` result is surfaced as the error it is.
fn parse_json_result(stdout: &str) -> Result<String, String> {
    for v in result_objects(stdout) {
        if v.get("is_error").and_then(Value::as_bool) == Some(true) {
            return Err(format!(
                "executor reported an error result: {}",
                excerpt(&v.to_string())
            ));
        }
        if let Some(text) = v.get("result").and_then(Value::as_str) {
            return Ok(text.to_string());
        }
    }
    Err(format!(
        "executor output carried no result object: {}",
        excerpt(stdout)
    ))
}

/// `format = "text"`: the CLI's trimmed stdout IS the answer — the generic
/// escape hatch for wiring any plain-printing CLI with zero code. empty
/// output on a zero exit is still an error: "ran fine, said nothing" is a
/// broken executor, not an answer.
fn parse_text_output(stdout: &str) -> Result<String, String> {
    let text = stdout.trim();
    if text.is_empty() {
        return Err("executor exited successfully but produced no output".into());
    }
    Ok(text.to_string())
}

/// a bounded, char-boundary-safe slice of diagnostic output for error strings.
fn excerpt(s: &str) -> String {
    const MAX: usize = 400;
    let s = s.trim();
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut end = MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

// ---- discovery --------------------------------------------------------------

/// load this host's capability specs and probe for their binaries.
///
/// spec sources: the embedded built-ins, then the workspace's `capability_dir`
/// when it exists (an absent dir is a node offering only the built-ins). a
/// broken spec is a hard `Err`: an operator config error fails the boot
/// loudly, it does not silently drop an executor.
///
/// per spec: the `detect.env` override wins (broken override = loud warning +
/// absent capability), else the first executable `detect.bin` on `PATH`.
/// `DUCKTAPE_PROVIDER_TIMEOUT_SECS` overrides every spec's IDLE timeout at
/// once (refreshed by child output; the spec's `hard_timeout_factor` caps it).
/// what discovery finds is exactly what the node announces.
///
/// `output_sink` installs a live tail on every discovered CLI provider.
/// `node_identity` is the verified local signer/origin bytes, kept for the run
/// labels. `managed_owner` names the SERVICE INSTANCE that owns every container
/// this set creates ([`managed_label`]) — `compute#deadbeef` for the compute
/// daemon, `agent#deadbeef` for the agent daemon. Crash-orphan cleanup reaps
/// exactly that label ([`reap_by_label`]), so one service can never sweep
/// another's containers. (Each daemon also has its own private graph root, so
/// this is the second line of defence, not the only one.)
/// where [`discover`] looks for an executor: a PATH-shaped search list, and the
/// env lookup permitted alongside it. Both are the backend's answer, together.
type ExecutorLookup<'a> = (Option<OsString>, &'a dyn Fn(&str) -> Option<OsString>);

pub fn discover(
    node_identity: &[u8],
    capability_dir: &Path,
    output_sink: Option<OutputSink>,
    backend: SandboxBackend,
    managed_owner: &str,
) -> Result<ProviderSet, String> {
    // the workspace's operator specs, when the workspace has any: an absent
    // directory is a node offering only the built-in specs, not an error.
    let specs = SpecSet::load(capability_dir.is_dir().then_some(capability_dir))?;
    let _executing_node = execution_node_id(node_identity);
    let timeout = std::env::var("DUCKTAPE_PROVIDER_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs);
    // WHERE AN EXECUTOR IS LOOKED FOR IS THE BACKEND'S QUESTION, because a run
    // executes wherever the backend puts it. A microVM run execs the guest's
    // copy of the CLI, so the ONLY directory whose contents this node may
    // announce is the one that copy is built from — announcing the host `PATH`
    // is what let a Mac advertise `claude` (a Mach-O binary its guest could not
    // exec) while its image carried only `codex`.
    //
    // The `env` override goes with it: a spec's `DUCKTAPE_<X>_BIN` names a HOST
    // path, which is meaningless to a guest, and honouring one here would
    // re-open exactly that gap.
    let (lookup, env): ExecutorLookup<'_> = match &backend {
        SandboxBackend::MicroVm { executors, .. } => (Some(executors.into()), &|_| None),
        #[cfg(any(test, feature = "testkit"))]
        SandboxBackend::Bare => (std::env::var_os("PATH"), &|k| std::env::var_os(k)),
    };
    Ok(discover_with_sink(
        specs,
        lookup,
        env,
        timeout,
        output_sink,
        backend,
        managed_owner,
    ))
}

/// the parameterized core of [`discover`]: specs in, providers out, all env
/// access injected so tests never mutate process state.
///
/// probing is per unique `(bin, env-override, companions)` identity, not per
/// tag: a spec
/// family (`[[variants]]`) puts dozens of tags over a handful of binaries,
/// and each PATH walk is a stat per directory — so tags sharing a probe
/// identity and declared companion set are grouped, the executor bundle is
/// resolved ONCE, and the result fans out to every tag in the group.
#[cfg(test)]
fn discover_with(
    specs: SpecSet,
    path: Option<OsString>,
    env: &dyn Fn(&str) -> Option<OsString>,
    global_timeout: Option<Duration>,
) -> ProviderSet {
    discover_with_sink(
        specs,
        path,
        env,
        global_timeout,
        None,
        SandboxBackend::Bare,
        UNSCOPED_OWNER,
    )
}

#[allow(clippy::too_many_arguments)]
fn discover_with_sink(
    specs: SpecSet,
    path: Option<OsString>,
    env: &dyn Fn(&str) -> Option<OsString>,
    global_timeout: Option<Duration>,
    output_sink: Option<OutputSink>,
    backend: SandboxBackend,
    managed_owner: &str,
) -> ProviderSet {
    let mut groups: BTreeMap<_, Vec<&CapabilitySpec>> = BTreeMap::new();
    for spec in specs.iter() {
        groups
            .entry((
                spec.bin.as_str(),
                spec.env.as_deref(),
                spec.companions.as_slice(),
            ))
            .or_default()
            .push(spec);
    }
    let mut providers: Vec<Box<dyn Provider>> = Vec::new();
    for group in groups.values() {
        let Some(bin) = resolve_bin(group, path.as_deref(), env) else {
            continue;
        };
        for spec in group {
            let mut provider =
                CliProvider::from_spec((*spec).clone(), bin.clone(), backend.clone())
                    .with_managed_owner(managed_owner);
            if let Some(t) = global_timeout {
                provider = provider.with_timeout(t);
            }
            if let Some(output_sink) = &output_sink {
                provider = provider.with_output_sink(output_sink.clone());
            }
            providers.push(Box::new(provider));
        }
    }
    drop(groups);
    ProviderSet::assemble(specs, providers)
}

/// resolve the executor bundle for one probe group — specs sharing one
/// `(bin, env, companions)`
/// identity: the env override wins (and a BROKEN override is a loud warning
/// naming every affected tag + absent capabilities, never a silent fallback
/// to PATH — the operator said "use this", and this does not exist), else
/// the first executable `detect.bin` on `path`.
///
/// `path` is a PATH-shaped search list, and WHICH list is the backend's call
/// ([`discover`]): the operator's executors directory under a microVM, the host
/// `PATH` under the bare test backend. Under a microVM the env override is
/// disabled at the call site rather than here, because "no host path can name a
/// guest executor" is a fact about the backend, not about this resolution.
fn resolve_bin(
    group: &[&CapabilitySpec],
    path: Option<&OsStr>,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    let spec = group.first().expect("probe groups are never empty");
    let bin = if let Some(explicit) = spec.env.as_deref().and_then(env) {
        let p = PathBuf::from(&explicit);
        if is_executable(&p) {
            p
        } else {
            let tags: Vec<&str> = group.iter().map(|s| s.tag.as_str()).collect();
            tracing::warn!(
                target: "ducktape::compute",
                reason = "executor_not_executable",
                capabilities = ?tags,
                "capabilities unavailable because their configured executor is not executable"
            );
            return None;
        }
    } else {
        let path = path?;
        std::env::split_paths(path)
            .map(|dir| dir.join(&spec.bin))
            .find(|candidate| is_executable(candidate))?
    };
    if let Err(error) = resolve_companion_bins(&bin, &spec.companions) {
        let tags: Vec<&str> = group.iter().map(|s| s.tag.as_str()).collect();
        tracing::warn!(
            target: "ducktape::compute",
            reason = "executor_companion_missing",
            capabilities = ?tags,
            error = %error,
            "capabilities unavailable because their executor bundle is incomplete"
        );
        return None;
    }
    Some(bin)
}

/// Resolve the files a spec declares beside its primary executable. The main
/// path is canonicalized first because PATH/env overrides commonly name a
/// symlink while self-locating executors search beside their real installed
/// binary. Every companion is required and executable: a partial bundle is an
/// absent capability at discovery and a loud run failure if files change later.
fn resolve_companion_bins(bin: &Path, names: &[String]) -> Result<Vec<PathBuf>, String> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let resolved_bin = canonical_mount_path(bin, "executor companion lookup")?;
    let parent = resolved_bin.parent().ok_or_else(|| {
        format!(
            "executor {} has no parent directory for its companions",
            resolved_bin.display()
        )
    })?;
    names
        .iter()
        .map(|name| {
            let candidate = parent.join(name);
            if !is_executable(&candidate) {
                return Err(format!(
                    "required companion {name:?} is not executable beside {}",
                    resolved_bin.display()
                ));
            }
            let resolved = canonical_mount_path(&candidate, "executor companion")?;
            let keeps_declared_name = resolved.file_name() == Some(OsStr::new(name));
            if !keeps_declared_name {
                return Err(format!(
                    "required companion {name:?} resolves to {}; companion symlinks must keep \
                     their declared sibling file name",
                    resolved.display()
                ));
            }
            Ok(resolved)
        })
        .collect()
}

// shared with the sandbox runtime probe, so it lives with the sandbox muscle.
pub(crate) use sandbox_host::is_executable;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    // tests exercise this crate ONLY through arbitrary mock specs and mock
    // binaries — never the embedded executor specs or their tags. the crate
    // is executor-agnostic and the tests hold it to that. (the embedded spec
    // FILES are validated as data in spec.rs, without naming them either.)

    /// a per-test scratch dir under the system temp root; unique by pid +
    /// test name so parallel tests never collide.
    fn scratch(test: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("provider-host-test-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// write an executable /bin/sh script standing in for an arbitrary
    /// executor CLI. discovery tests probe it (`is_executable`, hence the
    /// chmod); run() tests must NOT exec it directly — see [`sh_provider`]
    /// for why they run it through `/bin/sh` instead.
    fn fake_cli(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).expect("create fake cli");
        writeln!(f, "#!/bin/sh\n{body}").expect("write fake cli");
        f.set_permissions(std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake cli");
        path
    }

    fn no_env(_: &str) -> Option<OsString> {
        None
    }

    /// a workspace capability directory that does not exist: `discover`
    /// then announces the built-in specs only, which is what every test
    /// here wants (the crate is executor-agnostic; the tests never name an
    /// operator spec).
    fn no_specs() -> PathBuf {
        scratch("no-operator-specs").join("capabilities")
    }

    /// the microVM backend a hardware test uses, from the artifacts
    /// `ops/build-guest-rootfs.sh` produces.
    ///
    /// Shaped, not probed: constructing one costs nothing and asserts nothing
    /// about the host, so a unit test can build a provider with it while a
    /// hardware test additionally runs [`SandboxBackend::probe`] and skips when
    /// the images or `/dev/kvm` are absent.
    fn firecracker_backend() -> SandboxBackend {
        firecracker_backend_with(installed_executor_dir())
    }

    /// the executors directory a hardware test lends its guest:
    /// `$DUCKTAPE_EXECUTOR_DIR`, a workspace's `executors/` on the box running
    /// the hardware lane. A test-harness variable, not the product's — a node
    /// resolves its executors from its workspace and reads no env for them.
    /// Unset, an empty directory: the test announces nothing and skips.
    fn installed_executor_dir() -> PathBuf {
        match std::env::var_os("DUCKTAPE_EXECUTOR_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => executors_of("no-installed-executors", &[]),
        }
    }

    /// the live backend with an explicit executors directory — the one whose
    /// contents the guest finds at `/opt/duck/bin`.
    fn firecracker_backend_with(executors: PathBuf) -> SandboxBackend {
        live_backend(sandbox_host::Vmm::Firecracker, executors)
    }

    /// the live backend for THIS host's hypervisor: Firecracker on Linux, the
    /// vz shim on macOS. What a real node resolves, and the only shape that
    /// exercises a Mac's device model.
    fn platform_backend() -> SandboxBackend {
        live_backend(
            sandbox_host::Vmm::platform_default(),
            installed_executor_dir(),
        )
    }

    /// the guest images the rootfs builder wrote: `$DUCKTAPE_GUEST_DIR`, a
    /// workspace's `guest/` on the box running the hardware lane. A
    /// test-harness variable like `DUCKTAPE_EXECUTOR_DIR` above. Unset, a
    /// directory holding no images: `probe()` refuses and the test skips.
    fn live_backend(vmm: sandbox_host::Vmm, executors: PathBuf) -> SandboxBackend {
        let dir = match std::env::var_os("DUCKTAPE_GUEST_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => scratch("no-guest-images").join("guest"),
        };
        SandboxBackend::MicroVm {
            vmm,
            kernel: dir.join("vmlinux"),
            rootfs: dir.join("rootfs.ext4"),
            executors,
        }
    }

    /// an executors directory holding symlinks to the named host tools.
    ///
    /// The guest's `/opt/duck/bin` is an image built from the executors
    /// directory, so a live test that execs an ordinary tool by basename must
    /// put that tool in one — which is the mechanism itself under test, not a
    /// workaround for it. The symlinks point into the guest's own rootfs
    /// (`/bin/sh` resolves there as readily as here), so the image stays a few
    /// kilobytes.
    fn executors_of(test: &str, tools: &[&str]) -> PathBuf {
        let dir = scratch(test).join("executors");
        std::fs::create_dir_all(&dir).expect("executors dir");
        for tool in tools {
            std::os::unix::fs::symlink(format!("/bin/{tool}"), dir.join(tool)).expect("symlink");
        }
        dir
    }

    /// A microVM node announces what its GUEST can exec, never what its host
    /// happens to have installed.
    ///
    /// This is the defect the executors image exists to end. The node used to
    /// resolve capabilities against the host `PATH` while a run exec'd the
    /// guest's own copy of the CLI, so the two could disagree completely — a
    /// Mac measured with `claude` on its PATH and `codex` in its image
    /// advertised exactly the one it could not run. The host running this test
    /// may well have both CLIs on its PATH; an empty executors directory must
    /// still announce nothing.
    #[test]
    fn a_microvm_node_announces_its_guest_and_never_the_host_path() {
        let empty = scratch("announce-empty").join("executors");
        std::fs::create_dir_all(&empty).expect("executors dir");
        let announced = discover(
            b"n",
            &no_specs(),
            None,
            firecracker_backend_with(empty),
            "test",
        )
        .expect("discover")
        .capabilities();
        assert!(
            announced.is_empty(),
            "an empty executors directory announces nothing, whatever is on PATH: {announced:?}"
        );

        // and the same directory, filled, announces the bundle it holds.
        // Real files, not symlinks: a spec's companions are resolved beside the
        // executor's CANONICAL path, which is what `agent install` writes and
        // what a self-locating CLI needs to find its own sibling.
        let installed = scratch("announce-installed").join("executors");
        std::fs::create_dir_all(&installed).expect("executors dir");
        for name in ["codex", "codex-code-mode-host"] {
            fake_cli(&installed, name, "exit 0");
        }
        let announced = discover(
            b"n",
            &no_specs(),
            None,
            firecracker_backend_with(installed),
            "test",
        )
        .expect("discover")
        .capabilities();
        assert!(
            announced.iter().any(|tag| tag == "codex"),
            "an installed executor bundle is announced: {announced:?}"
        );
        assert!(
            !announced.iter().any(|tag| tag == "claude"),
            "a CLI that is only on the host PATH is not: {announced:?}"
        );
    }

    /// an inline mock spec: arbitrary tag and binary, one of the named
    /// output parsers, no env override.
    fn mock_spec(tag: &str, bin: &str, format: &str) -> CapabilitySpec {
        CapabilitySpec::parse(
            &format!(
                r#"
spec = 1
[capability]
tag = "{tag}"
[detect]
bin = "{bin}"
[invoke]
args = []
prompt = "stdin"
[output]
format = "{format}"
"#
            ),
            "test",
        )
        .unwrap()
    }

    /// a provider that runs `spec` by exec'ing `/bin/sh` with `script`
    /// prepended to the spec's argv — NEVER by exec'ing the script itself.
    ///
    /// exec'ing a script this process just wrote races under parallel tests
    /// (#226): while one test holds the script's write fd open, another
    /// test's `Command::spawn` forks, and the forked child inherits a copy
    /// of that fd (O_CLOEXEC drops it only at that child's own exec). if
    /// the writer then execs its script inside that fork→exec window, the
    /// kernel refuses with ETXTBSY ("Text file busy"). which test loses is
    /// pure scheduling — the rotating single failure. `/bin/sh` is never
    /// open for writing, and ETXTBSY guards only the exec'd image, not the
    /// script it reads as data.
    fn sh_provider(mut spec: CapabilitySpec, script: PathBuf, wd: &str) -> CliProvider {
        spec.args.insert(0, script.display().to_string());
        CliProvider::from_spec(spec, PathBuf::from("/bin/sh"), SandboxBackend::Bare)
            .with_workdir(scratch(wd))
    }

    fn mock_provider(tag: &str, format: &str, script: PathBuf, wd: &str) -> CliProvider {
        sh_provider(mock_spec(tag, tag, format), script, wd)
    }

    /// LIVE end-to-end on a real VMM: build a provider whose executor is `cat`,
    /// boot a microVM for it, and confirm the prompt comes back the way it went
    /// in. Exercises the whole headless path — the workspace and asset
    /// images, the boot config, the vsock stdio, the exit frame and the
    /// workspace read-back — against a real VMM.
    ///
    /// `#[ignore]`: needs `/dev/kvm` and the guest artifacts.
    ///   DUCKTAPE_GUEST_DIR=… cargo test -p provider-host --lib -- --ignored \
    ///     --nocapture microvm_echo
    #[tokio::test]
    #[ignore = "live: needs /dev/kvm and a built guest rootfs"]
    async fn microvm_echo_round_trips_through_invoke() {
        // the executor rides its own read-only image, not the host: the microVM
        // mounts no host directory, so there is nothing to bind a script from.
        let backend = firecracker_backend_with(executors_of("microvm-echo-bin", &["cat"]));
        if let Err(why) = backend.probe() {
            eprintln!("skipping: {why}");
            return;
        }
        let root = scratch("microvm-echo");
        let provider = CliProvider::from_spec(
            mock_spec("echo", "cat", "text"),
            // resolved by basename to /opt/duck/bin/cat, which is where the
            // executors image the backend above names gets mounted.
            PathBuf::from("/bin/cat"),
            backend,
        )
        .with_workdir(root.join("wd"));
        std::fs::create_dir_all(root.join("wd")).unwrap();

        let ctx = RunContext {
            executing_node: Some(execution_node_id(b"e2e-node")),
            // a VM has no unlimited size; both dimensions are required.
            limits: [("cores".to_string(), 1u64), ("mem_gb".to_string(), 1u64)]
                .into_iter()
                .collect(),
            ..RunContext::default()
        };
        let answer = provider
            .run("PONG-FROM-A-MICROVM", &ctx)
            .await
            .expect("run inside a microVM");
        eprintln!("--- microVM echo answer: {answer:?} ---");
        assert!(
            answer.contains("PONG-FROM-A-MICROVM"),
            "the guest echoed the prompt back over vsock: {answer:?}"
        );
    }

    /// a sandbox spec with no auth section — the shape both skills tests want.
    fn sandbox_spec(tag: &str) -> CapabilitySpec {
        CapabilitySpec::parse(
            &format!(
                r#"
spec = 1
[capability]
tag = "{tag}"
[detect]
bin = "{tag}"
[invoke]
args = []
prompt = "stdin"
[output]
format = "text"
"#
            ),
            "test",
        )
        .unwrap()
    }

    /// the smoke run's whole behaviour, as a shell one-liner.
    ///
    /// The CLI is `sh` and not a script written here, because a microVM mounts
    /// NOTHING from the host: an executor the node lends has to already be in
    /// the guest rootfs, at [`GUEST_BIN_DIR`]. A test that wrote a script into a
    /// host `bin/` directory would be testing a delivery path that does not
    /// exist — measured, as `execve /opt/duck/bin/sandbox-smoke` and exit 126.
    const SMOKE_SCRIPT: &str = "prompt=$(cat); \
         printf '%s' \\\"$prompt\\\" > sandbox-marker.txt; \
         printf 'sandbox-ok:%s' \\\"$prompt\\\"";

    // ---- the assembled context document (the agent's "soul") ----------------

    /// a mock spec plus whatever extra sections the test needs — the `[context]`
    /// / `[isolation]` shapes below, without a fixture per shape.
    fn spec_with(tag: &str, extra: &str) -> CapabilitySpec {
        CapabilitySpec::parse(
            &format!(
                r#"
spec = 1
[capability]
tag = "{tag}"
[detect]
bin = "{tag}"
[invoke]
args = []
prompt = "stdin"
[output]
format = "text"
{extra}
"#
            ),
            "test",
        )
        .unwrap()
    }

    const SOUL: &str = "# soul\nbe kind\n";

    #[tokio::test]
    async fn a_workspace_parent_context_spec_writes_the_soul_beside_the_checkout_and_cleans_it_up()
    {
        // the doc lands under a slug-named directory beside the run's checkout —
        // still OUTSIDE the tree `commit` scans, layered UNDER a repository's own
        // instructions file, and namespaced per run (#1692) rather than at the
        // shared parent every run of this tag lands in.
        let root = scratch("soul-parent");
        // the mock CLI prints the delivered file, a separator, then its stdin.
        let script = fake_cli(
            &root,
            "soul.sh",
            &format!("cat ../{RUN_RUNTIME_DIR}/checkout/SOUL.md; echo ---; cat"),
        );
        let provider = sh_provider(
            spec_with("soul", "[context]\npath = \"workspace-parent:SOUL.md\"\n"),
            script,
            "soul-parent-scratch",
        );
        let ctx = RunContext {
            workdir_override: Some(root.join("checkout")),
            context_doc: Some(SOUL.to_string()),
            ..RunContext::default()
        };

        let out = provider
            .run("PROMPT", &ctx)
            .await
            .expect("the run succeeds");
        // the soul reached the FILE, and the stdin half is the bare prompt: a
        // spec with a native context door must not ALSO inflate the prompt.
        assert_eq!(out, "# soul\nbe kind\n---\nPROMPT");
        // and the file is gone: it lives outside the workdir, where nothing else
        // would ever clean it up and a stale soul would join the next run.
        assert!(
            !root.join(RUN_RUNTIME_DIR).join("checkout").exists(),
            "the context doc's per-run directory is removed when the run ends"
        );
    }

    #[tokio::test]
    async fn two_concurrent_workspace_parent_runs_never_share_a_soul() {
        // #1692: two runs of the same tag share `workdir.parent()` — the node's
        // whole runs root — so a shared host path for the context doc would let
        // whichever run writes second clobber the first's soul before either
        // child reads it, and whichever run's guard drops first delete it out
        // from under the other. Namespacing the doc under each run's own slug
        // (see `Provider::context_target`) means neither happens.
        let root = scratch("soul-parent-concurrent");
        let script_a = fake_cli(
            &root,
            "soul-a.sh",
            &format!("cat ../{RUN_RUNTIME_DIR}/run-a/SOUL.md; echo ---; cat"),
        );
        let script_b = fake_cli(
            &root,
            "soul-b.sh",
            &format!("cat ../{RUN_RUNTIME_DIR}/run-b/SOUL.md; echo ---; cat"),
        );
        let provider_a = sh_provider(
            spec_with("soul", "[context]\npath = \"workspace-parent:SOUL.md\"\n"),
            script_a,
            "soul-parent-concurrent-scratch-a",
        );
        let provider_b = sh_provider(
            spec_with("soul", "[context]\npath = \"workspace-parent:SOUL.md\"\n"),
            script_b,
            "soul-parent-concurrent-scratch-b",
        );
        let ctx_a = RunContext {
            workdir_override: Some(root.join("run-a")),
            context_doc: Some("# soul A\n".to_string()),
            ..RunContext::default()
        };
        let ctx_b = RunContext {
            workdir_override: Some(root.join("run-b")),
            context_doc: Some("# soul B\n".to_string()),
            ..RunContext::default()
        };

        // both runs execute concurrently, sharing `root` as their workdir parent —
        // exactly the shape #1692 describes.
        let (out_a, out_b) = tokio::join!(
            provider_a.run("PROMPT-A", &ctx_a),
            provider_b.run("PROMPT-B", &ctx_b),
        );

        // each child saw ITS OWN soul, never the other's.
        assert_eq!(out_a.expect("run A succeeds"), "# soul A\n---\nPROMPT-A");
        assert_eq!(out_b.expect("run B succeeds"), "# soul B\n---\nPROMPT-B");
        // and one run's guard dropping did not remove the other's still-live
        // per-run directory.
        assert!(!root.join(RUN_RUNTIME_DIR).join("run-a").exists());
        assert!(!root.join(RUN_RUNTIME_DIR).join("run-b").exists());
    }

    #[tokio::test]
    async fn a_config_home_context_spec_writes_the_soul_into_the_runs_fresh_config_home() {
        // resolved against the per-run config home [isolation] already materializes
        // — inside the workdir's reserved runtime dir, which the provisioner deletes
        // before scanning, so no guard and no artifact.
        let root = scratch("soul-config-home");
        let script = fake_cli(
            &root,
            "soul.sh",
            "cat \"$TEST_HOME/AGENTS.md\"; echo ---; cat",
        );
        let provider = sh_provider(
            spec_with(
                "soul",
                "[isolation]\nconfig_home_env = \"TEST_HOME\"\n\n\
                 [context]\npath = \"config-home:AGENTS.md\"\n",
            ),
            script,
            "soul-config-home-scratch",
        );
        let workdir = root.join("checkout");
        let ctx = RunContext {
            workdir_override: Some(workdir.clone()),
            context_doc: Some(SOUL.to_string()),
            ..RunContext::default()
        };

        let out = provider
            .run("PROMPT", &ctx)
            .await
            .expect("the run succeeds");
        // the soul reached the CLI's own config home, and the stdin half is the
        // bare prompt — a native context door must not ALSO inflate the prompt.
        assert_eq!(out, "# soul\nbe kind\n---\nPROMPT");
        // no ContextGuard for this door: the doc lands INSIDE the config home,
        // so the run's own [`RunHome`] takes it away with the rest of that
        // directory — and the reserved dir it hung under is one the
        // provisioner's commit bracket removes anyway.
        assert!(workdir.join(RUN_RUNTIME_DIR).is_dir());
    }

    #[tokio::test]
    async fn a_spec_with_no_context_section_prepends_the_soul_to_the_prompt() {
        // the raw-provider door: a CLI with no ambient-instructions convention
        // (ollama, any plain `text` executor) still gets the soul — on stdin.
        let root = scratch("soul-prompt");
        let script = fake_cli(&root, "soul.sh", "cat");
        let provider = sh_provider(spec_with("raw", ""), script, "soul-prompt-scratch");
        let ctx = RunContext {
            context_doc: Some(SOUL.to_string()),
            ..RunContext::default()
        };

        let out = provider
            .run("PROMPT", &ctx)
            .await
            .expect("the run succeeds");
        assert_eq!(out, format!("{SOUL}\n\nPROMPT").trim());
        // ... and a run with no doc at all is byte-for-byte the old behavior.
        let out = provider
            .run("PROMPT", &RunContext::default())
            .await
            .expect("the run succeeds");
        assert_eq!(out, "PROMPT", "no doc, no prepend");
    }

    // ---- the credential broker ----------------------------------------------

    /// a broker-backed spec: the only auth path there is.
    fn broker_spec(tag: &str) -> CapabilitySpec {
        CapabilitySpec::parse(
            &format!(
                r#"
spec = 1
[capability]
tag = "{tag}"
[detect]
bin = "{tag}"
[invoke]
args = ["exec", "--json", "-"]
prompt = "stdin"
[output]
format = "text"
[isolation]
config_home_env = "CODEX_HOME"
broker = "codex-responses"
"#
            ),
            "test",
        )
        .unwrap()
    }

    fn anthropic_broker_spec(tag: &str) -> CapabilitySpec {
        CapabilitySpec::parse(
            &format!(
                r#"
spec = 1
[capability]
tag = "{tag}"
[detect]
bin = "{tag}"
[invoke]
args = ["-p", "--output-format", "json"]
prompt = "stdin"
[output]
format = "json-result"
[isolation]
config_home_env = "CLAUDE_CONFIG_DIR"
broker = "anthropic-messages"
"#
            ),
            "test",
        )
        .unwrap()
    }

    fn argv_of(cmd: &tokio::process::Command) -> String {
        cmd.as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[tokio::test]
    async fn every_backend_accepts_the_credential_broker_shape() {
        // A missing host credential may still fail startup, but no backend is
        // structurally rejected: both reach a LOOPBACK broker — the bare
        // harness directly, a microVM through the vsock tunnel.
        for backend in [SandboxBackend::Bare, firecracker_backend()] {
            let provider = CliProvider::from_spec(
                broker_spec("c"),
                PathBuf::from("/usr/bin/c"),
                backend.clone(),
            );
            if let Err(e) = provider.start_broker(None).await {
                assert!(
                    !e.contains("cannot host a credential broker"),
                    "{backend:?} reached credential loading, not a backend veto: {e:?}"
                );
            }
        }
    }

    #[test]
    fn a_broker_aims_the_child_at_loopback_and_hands_it_no_credential() {
        // what the child gets: a base URL, an opaque bearer, and a fresh config
        // home. what it does NOT get: the credential — which is why the argv is
        // rewritten to point at the broker at all.
        let provider = CliProvider::from_spec(
            broker_spec("c"),
            PathBuf::from("/usr/bin/c"),
            SandboxBackend::Bare,
        );
        let endpoint = broker::BrokerEndpoint {
            kind: CredentialKind::Codex,
            base_url: "http://127.0.0.1:54321/v1".into(),
            run_bearer: "opaque-run-bearer".into(),
            control_url: "http://127.0.0.1:54321/v1/control/provider-idle".into(),
            control_token: "opaque-control-token".into(),
        };
        let config_home = PathBuf::from("/tmp/wd/.ducktape-run/slot/provider-config");
        let auth = RunAuth {
            config_home: Some(&config_home),
            broker: Some(&endpoint),
        };
        let cmd = provider
            .command(
                &["exec".into(), "--json".into(), "-".into()],
                Path::new("/tmp/wd"),
                &RunContext::default(),
                &auth,
            )
            .expect("command builds");

        // the model provider is spliced in after args[0] (`exec`), and the
        // trailing "-" (prompt on stdin) is still last.
        let joined = argv_of(&cmd);
        assert!(
            joined.starts_with("exec -c model_providers.ducktape="),
            "{joined}"
        );
        assert!(
            joined.contains("base_url=\"http://127.0.0.1:54321/v1\""),
            "{joined}"
        );
        assert!(joined.contains("model_provider=\"ducktape\""), "{joined}");
        assert!(
            joined.ends_with("--json -"),
            "the stdin marker stays last: {joined}"
        );

        let envs: BTreeMap<String, Option<String>> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        // the fresh config home is what stops the CLI reading ~/.codex/auth.json.
        assert_eq!(
            envs.get("CODEX_HOME").cloned().flatten().as_deref(),
            Some(config_home.to_str().unwrap())
        );
        assert_eq!(
            envs.get(BROKER_TOKEN_ENV).cloned().flatten().as_deref(),
            Some("opaque-run-bearer")
        );
        assert_eq!(
            envs.get(PROVIDER_CONTROL_URL_ENV)
                .cloned()
                .flatten()
                .as_deref(),
            Some("http://127.0.0.1:54321/v1/control/provider-idle")
        );
        assert_eq!(
            envs.get(PROVIDER_CONTROL_TOKEN_ENV)
                .cloned()
                .flatten()
                .as_deref(),
            Some("opaque-control-token")
        );
        assert!(
            !joined.contains("opaque-control-token"),
            "the control credential stays out of argv: {joined}"
        );
        // and the upstream credential is REMOVED, not merely unset: a bare child
        // inherits this process's env, and one that still saw OPENAI_API_KEY would
        // dial OpenAI directly, straight past the broker holding it.
        assert_eq!(
            envs.get("OPENAI_API_KEY"),
            Some(&None),
            "the inherited upstream credential is explicitly removed: {envs:?}"
        );
    }

    #[test]
    fn without_a_broker_has_no_control_capability() {
        // BYO providers get no model-provider splice or bearer. Reserved
        // control lookalikes are actively removed from the inherited env so a
        // stale operator value cannot turn into authority for another run.
        let provider = CliProvider::from_spec(
            sandbox_spec("plain"),
            PathBuf::from("/usr/bin/x"),
            SandboxBackend::Bare,
        );
        let cmd = provider
            .command(
                &["run".into()],
                Path::new("/tmp/wd"),
                &RunContext::default(),
                &RunAuth::default(),
            )
            .expect("command builds");
        assert_eq!(argv_of(&cmd), "run");
        let envs: BTreeMap<String, Option<String>> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(
            envs,
            BTreeMap::from([
                (PROVIDER_CONTROL_TOKEN_ENV.to_string(), None),
                (PROVIDER_CONTROL_URL_ENV.to_string(), None),
            ])
        );
    }

    #[test]
    fn sandbox_context_cannot_supply_reserved_control_env() {
        let provider = CliProvider::from_spec(
            sandbox_spec("plain"),
            PathBuf::from("/usr/bin/x"),
            SandboxBackend::Bare,
        );
        let mut ctx = RunContext::default();
        ctx.env
            .insert(PROVIDER_CONTROL_URL_ENV.into(), "http://foreign".into());
        ctx.env
            .insert(PROVIDER_CONTROL_TOKEN_ENV.into(), "foreign-token".into());
        let envs = provider.sandbox_env(&ctx, &RunAuth::default()).unwrap();
        assert!(envs.iter().all(|(key, _)| {
            key != PROVIDER_CONTROL_URL_ENV && key != PROVIDER_CONTROL_TOKEN_ENV
        }));
    }

    #[test]
    fn pi_config_and_argv_use_only_the_selected_run_broker() {
        let spec = spec::builtin_specs()
            .into_iter()
            .find(|spec| spec.tag == "pi")
            .unwrap();
        let provider = CliProvider::from_spec(spec, PathBuf::from("/usr/bin/pi"), SandboxBackend::Bare);
        let workdir = scratch("pi-config");
        for kind in [CredentialKind::Claude, CredentialKind::Codex] {
            let home = provider.prepare_config_home(&workdir).unwrap().unwrap();
            let endpoint = broker::BrokerEndpoint {
                kind,
                base_url: "http://127.0.0.1:54321".into(),
                run_bearer: "opaque-run-capability-not-an-upstream-token".into(),
                control_url: "http://127.0.0.1:54321/control/provider-idle".into(),
                control_token: "separate-control-capability".into(),
            };
            let auth = RunAuth {
                config_home: Some(home.config()),
                broker: Some(&endpoint),
            };
            let env: std::collections::BTreeMap<_, _> = provider
                .sandbox_env(&RunContext::default(), &auth)
                .unwrap()
                .into_iter()
                .collect();
            assert_eq!(
                env["PI_CODING_AGENT_DIR"],
                home.config().display().to_string()
            );
            assert_eq!(env[BROKER_TOKEN_ENV], endpoint.run_bearer);
            assert_eq!(env["PI_OFFLINE"], "1");
            for name in [
                "ANTHROPIC_API_KEY",
                "OPENAI_API_KEY",
                "CLAUDE_CODE_OAUTH_TOKEN",
            ] {
                assert!(!env.contains_key(name));
            }
            let models: Value =
                serde_json::from_slice(&std::fs::read(home.config().join("models.json")).unwrap())
                    .unwrap();
            let settings: Value =
                serde_json::from_slice(&std::fs::read(home.config().join("settings.json")).unwrap())
                    .unwrap();
            let selected = match kind {
                CredentialKind::Claude => "anthropic",
                CredentialKind::Codex => "openai-codex",
                CredentialKind::AppleCodesign => {
                    unreachable!("a signing identity has no model wire")
                }
            };
            assert_eq!(models["providers"].as_object().unwrap().len(), 1);
            assert_eq!(models["providers"][selected]["baseUrl"], endpoint.base_url);
            assert_eq!(
                models["providers"][selected]["apiKey"],
                format!("${BROKER_TOKEN_ENV}")
            );
            assert_eq!(settings["defaultProvider"], selected);
            assert_eq!(settings["transport"], "sse");
            assert!(!home.config().join("auth.json").exists());
            assert!(!home.config().join(".credentials.json").exists());
            let extension = home.config().join(pi::TOOL_EXTENSION);
            assert!(extension.is_file());
            let argv = provider.broker_argv(&provider.spec.args, &workdir, &auth);
            assert!(
                argv.windows(2)
                    .any(|pair| pair[0] == "-e" && pair[1] == extension.display().to_string())
            );
            assert!(!argv.iter().any(|arg| arg.contains(&endpoint.run_bearer)));
            let config = home.config().to_path_buf();
            drop(home);
            assert!(!config.exists());
        }
    }

    /// A fresh claude config home is not usable EMPTY: Claude Code decides it is
    /// on its first run and starts the onboarding wizard, whose second step is a
    /// browser login the sandbox cannot complete — an interactive session
    /// dead-ends there with a perfectly valid seeded credential. Both state files
    /// are part of making the home usable, so both are asserted here.
    #[test]
    fn a_fresh_claude_config_home_is_seeded_past_the_first_run_wizard() {
        let provider = CliProvider::from_spec(
            anthropic_broker_spec("cl"),
            PathBuf::from("/usr/bin/cl"),
            SandboxBackend::Bare,
        );
        let workdir = scratch("claude_config_home_seed");
        let home = provider
            .prepare_config_home(&workdir)
            .expect("config home materializes")
            .expect("a claude spec names a config home");
        let dir = home.config();

        let read = |name: &str| -> serde_json::Value {
            serde_json::from_str(
                &std::fs::read_to_string(dir.join(name))
                    .unwrap_or_else(|e| panic!("{name} seeded into the config home: {e}")),
            )
            .unwrap_or_else(|e| panic!("{name} is json: {e}"))
        };
        assert_eq!(
            read(".claude.json")["hasCompletedOnboarding"].as_bool(),
            Some(true),
            "without this, the TUI runs the first-run wizard and lands on \
             'Select login method' instead of a prompt"
        );
        assert_eq!(
            read("settings.json")["skipWebFetchPreflight"].as_bool(),
            Some(true),
            "without this, WebFetch preflights api.anthropic.com around the broker"
        );
    }

    /// One screen past the login wizard is the WORKSPACE-TRUST prompt, and it
    /// is the same failure shape: the TUI parks on "Quick safety check…" with
    /// no error and no output, forever, on any unattended session.
    ///
    /// It is keyed on the GUEST cwd, so this drives the real spawn-path seam
    /// and asserts that a SECOND spawn under a DIFFERENT guest cwd does not
    /// lose the first one's answer. The seed merges by project key, so the
    /// guarantee has to hold for any two distinct paths, whatever mints them.
    #[test]
    fn each_spawns_guest_workdir_is_trusted_and_the_previous_one_survives() {
        let provider = CliProvider::from_spec(
            anthropic_broker_spec("cl"),
            PathBuf::from("/usr/bin/cl"),
            SandboxBackend::Bare,
        );
        let workdir = scratch("claude_trust_seed");
        let home = provider
            .prepare_config_home(&workdir)
            .expect("config home materializes")
            .expect("a claude spec names a config home");
        let auth = RunAuth {
            config_home: Some(home.config()),
            broker: None,
        };
        let read = || -> serde_json::Value {
            serde_json::from_str(
                &std::fs::read_to_string(home.config().join(".claude.json"))
                    .expect("the seeded state file"),
            )
            .expect("json")
        };

        // nothing is trusted until a spawn names a cwd.
        assert!(read()["projects"].as_object().is_none_or(|p| p.is_empty()));

        let guest_cwd = std::path::Path::new("/duck/workspace");
        provider
            .trust_guest_workdir(&auth, guest_cwd)
            .expect("the guest cwd is answerable");
        assert_eq!(
            read()["projects"]["/duck/workspace"]["hasTrustDialogAccepted"].as_bool(),
            Some(true),
            "without this the TUI parks on the trust prompt with no error"
        );

        // a second spawn of the SAME run under a different guest cwd: the seed
        // must MERGE, not replace.
        let second = std::path::Path::new("/duck/workspace-2");
        provider
            .trust_guest_workdir(&auth, second)
            .expect("a second guest cwd is answerable too");
        let state = read();
        assert_eq!(
            state["projects"]["/duck/workspace-2"]["hasTrustDialogAccepted"].as_bool(),
            Some(true)
        );
        assert_eq!(
            state["projects"]["/duck/workspace"]["hasTrustDialogAccepted"].as_bool(),
            Some(true),
            "the earlier spawn's answer must survive the later one"
        );
        // and the onboarding seed is still there beside it.
        assert_eq!(state["hasCompletedOnboarding"].as_bool(), Some(true));
    }

    /// The trust claim is scoped to claude and to a run that HAS an isolated
    /// config home. A spec with neither writes nothing — there is no file to
    /// assert into and no prompt to answer.
    #[test]
    fn nothing_is_trusted_on_a_spec_that_has_no_isolated_config_home() {
        let provider = CliProvider::from_spec(
            anthropic_broker_spec("cl"),
            PathBuf::from("/usr/bin/cl"),
            SandboxBackend::Bare,
        );
        provider
            .trust_guest_workdir(&RunAuth::default(), std::path::Path::new("/duck/workspace"))
            .expect("no config home is not an error");
    }

    /// A CODEX config home stays EMPTY — the claude state files are not merely
    /// unused there, an empty `CODEX_HOME` is what forces codex onto the broker
    /// instead of the operator's real `auth.json`.
    #[test]
    fn a_codex_config_home_is_not_seeded_with_claude_state() {
        let provider = CliProvider::from_spec(
            broker_spec("cx"),
            PathBuf::from("/usr/bin/cx"),
            SandboxBackend::Bare,
        );
        let workdir = scratch("codex_config_home_seed");
        let home = provider
            .prepare_config_home(&workdir)
            .expect("config home materializes")
            .expect("a codex spec names a config home");
        let dir = home.config();
        let entries: Vec<_> = std::fs::read_dir(dir)
            .expect("read config home")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert!(
            entries.is_empty(),
            "codex config home must be empty, got {entries:?}"
        );
    }

    /// EMPTY is a claim about the NAME. The slot was once
    /// `sha256(run_key, agent_id, workdir)` — every coordinate that a rerun, a
    /// retry of the same dispatch, and two concurrent runs of one agent all
    /// share — so a second run walked into the first one's directory and read
    /// its credentials file and transcripts. Since a run submitted by one
    /// account executes on another account's node, that is one operator's
    /// material handed to another's child.
    ///
    /// both homes are held ALIVE here: uniqueness must come from the name, not
    /// from one guard having cleaned up before the other looked.
    #[test]
    fn two_runs_sharing_every_coordinate_get_different_config_homes() {
        let provider = CliProvider::from_spec(
            broker_spec("cx"),
            PathBuf::from("/usr/bin/cx"),
            SandboxBackend::Bare,
        );
        let workdir = scratch("config_home_collision");
        let first = provider
            .prepare_config_home(&workdir)
            .expect("config home materializes")
            .expect("a codex spec names a config home");
        let second = provider
            .prepare_config_home(&workdir)
            .expect("config home materializes")
            .expect("a codex spec names a config home");
        assert_ne!(
            first.config(),
            second.config(),
            "a second run must not be able to NAME the first run's config home"
        );
        for home in [&first, &second] {
            let entries: Vec<_> = std::fs::read_dir(home.config())
                .expect("read config home")
                .map(|e| e.expect("entry").file_name())
                .collect();
            assert!(entries.is_empty(), "got {entries:?}");
        }
    }

    /// a run home whose whole tree was already taken away is NOT a leak.
    ///
    /// An interactive session's workdir is removed by its owner when the session
    /// ends, and this run's slot lives inside it — while the pump task's
    /// `Arc<InteractiveSession>` means the [`RunHome`] drop can run afterwards.
    /// Calling that a leak would log a warn per pty session that names nothing.
    #[test]
    fn a_slot_that_is_already_gone_is_not_a_leak() {
        let absent = scratch("run-home-already-gone").join("slot");
        let error = std::fs::remove_dir_all(&absent).expect_err("nothing is there");
        assert!(!removal_left_a_leak(&error));
        // anything else IS the guard failing to keep its promise, and says so.
        assert!(removal_left_a_leak(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
    }

    /// the two-runs-in-one-workspace proof, end to end: a fake CLI reports
    /// whether its config home already held state, then writes some (standing in
    /// for the `.credentials.json` and transcripts a real executor leaves). Both
    /// runs share a workspace and every context coordinate, and the second must
    /// be as clean as the first — and neither may leave its state in the workdir,
    /// which is mounted rw into the sandbox of whatever run comes next.
    #[tokio::test]
    async fn a_rerun_in_one_workspace_neither_inherits_nor_leaves_a_config_home() {
        let root = scratch("config-home-rerun");
        let script = fake_cli(
            &root,
            "state.sh",
            "if [ -e \"$TEST_HOME/state\" ]; then echo INHERITED; else echo FRESH; fi\n\
             echo pretend-credential > \"$TEST_HOME/state\"\n\
             cat >/dev/null",
        );
        let provider = sh_provider(
            spec_with("rerun", "[isolation]\nconfig_home_env = \"TEST_HOME\"\n"),
            script,
            "config-home-rerun-scratch",
        );
        let workspace = root.join("workspace");
        let ctx = RunContext {
            workdir_override: Some(workspace.clone()),
            ..RunContext::default()
        };

        let first = provider.run("PROMPT", &ctx).await.expect("first run");
        let second = provider.run("PROMPT", &ctx).await.expect("second run");
        assert_eq!(first, "FRESH");
        assert_eq!(
            second, "FRESH",
            "the second run walked into the first run's config home"
        );

        let leftovers: Vec<_> = std::fs::read_dir(workspace.join(RUN_RUNTIME_DIR))
            .expect("read the run-runtime dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert!(
            leftovers.is_empty(),
            "a finished run left its config home in a shared workdir: {leftovers:?}"
        );
    }

    #[test]
    fn a_claude_broker_aims_the_child_by_env_not_argv() {
        // the Anthropic broker's opposite of codex: the argv is UNTOUCHED (a
        // claude argv has no `-c model_providers` splice), and the child is aimed
        // by env — base URL and fresh config home — plus a `claudeAiOauth` creds
        // file seeded into that config home (so the CLI runs subscription mode,
        // not API mode), plus the hardening vars that keep Claude Code from
        // dialing out around the broker.
        let provider = CliProvider::from_spec(
            anthropic_broker_spec("cl"),
            PathBuf::from("/usr/bin/cl"),
            SandboxBackend::Bare,
        );
        let endpoint = broker::BrokerEndpoint {
            kind: CredentialKind::Claude,
            // NOTE: no `/v1` — ANTHROPIC_BASE_URL is the API root.
            base_url: "http://127.0.0.1:54321".into(),
            run_bearer: "opaque-run-bearer".into(),
            control_url: String::new(),
            control_token: String::new(),
        };
        // a real dir: apply_auth_env WRITES the creds file into the config home.
        let config_home = scratch("claude_broker_creds");
        let auth = RunAuth {
            config_home: Some(&config_home),
            broker: Some(&endpoint),
        };
        let cmd = provider
            .command(
                &["-p".into(), "--output-format".into(), "json".into()],
                Path::new("/tmp/wd"),
                &RunContext::default(),
                &auth,
            )
            .expect("command builds");

        // argv verbatim — no codex-style splice for a claude broker.
        assert_eq!(argv_of(&cmd), "-p --output-format json");

        let envs: BTreeMap<String, Option<String>> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        let got = |k: &str| envs.get(k).cloned().flatten();
        assert_eq!(
            got("ANTHROPIC_BASE_URL").as_deref(),
            Some("http://127.0.0.1:54321")
        );
        // the run bearer rides a `claudeAiOauth` creds file, NOT ANTHROPIC_AUTH_TOKEN:
        // an env bearer would force API mode and override subscription mode.
        assert_eq!(got("ANTHROPIC_AUTH_TOKEN"), None);
        let creds: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(config_home.join(".credentials.json"))
                .expect("creds file seeded into config home"),
        )
        .expect("creds file is json");
        assert_eq!(
            creds["claudeAiOauth"]["accessToken"].as_str(),
            Some("opaque-run-bearer"),
            "the loopback run bearer is the seeded OAuth accessToken"
        );
        // owner-only, like the ANTHROPIC_AUTH_TOKEN env it replaces.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(config_home.join(".credentials.json"))
                .expect("creds metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "credentials file must be owner-only");
        }
        assert_eq!(
            got("CLAUDE_CONFIG_DIR").as_deref(),
            Some(config_home.to_str().unwrap()),
            "the fresh config home blocks the ~/.claude fallback"
        );
        // SUBPROCESS_ENV_SCRUB is deliberately NOT set — it breaks headless
        // `claude -p` and protects nothing here (the only ANTHROPIC_* var is
        // ANTHROPIC_BASE_URL; the bearer rides the creds file). See apply_auth_env.
        assert_eq!(got("CLAUDE_CODE_SUBPROCESS_ENV_SCRUB"), None);
        assert_eq!(
            got("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC").as_deref(),
            Some("1")
        );
        assert_eq!(got("DISABLE_AUTOUPDATER").as_deref(), Some("1"));
        // the codex bearer var is NOT set for a claude broker.
        assert_eq!(envs.get(BROKER_TOKEN_ENV), None);
        // and the inherited Anthropic credential is REMOVED, not merely unset: a
        // bare child that still saw it would dial api.anthropic.com directly,
        // straight past the broker holding the real credential.
        assert_eq!(
            envs.get("ANTHROPIC_API_KEY"),
            Some(&None),
            "the inherited upstream credential is explicitly removed: {envs:?}"
        );
    }

    // ---- discovery ----------------------------------------------------------

    #[test]
    fn discovery_announces_installed_binaries_only() {
        let dir = scratch("discovery-path");
        fake_cli(&dir, "alpha-cli", "exit 0");
        let set = discover_with(
            SpecSet::from_specs(vec![
                mock_spec("alpha", "alpha-cli", "text"),
                mock_spec("beta", "beta-cli", "text"),
            ]),
            Some(dir.clone().into_os_string()),
            &no_env,
            None,
        );
        assert_eq!(set.capabilities(), vec!["alpha"], "only the installed one");
        assert!(set.find("alpha").is_some());
        assert!(set.find("beta").is_none(), "no binary, no provider");
    }

    #[test]
    fn discovery_sorts_announced_tags() {
        let dir = scratch("discovery-both");
        fake_cli(&dir, "beta-cli", "exit 0");
        fake_cli(&dir, "alpha-cli", "exit 0");
        let set = discover_with(
            SpecSet::from_specs(vec![
                mock_spec("beta", "beta-cli", "text"),
                mock_spec("alpha", "alpha-cli", "text"),
            ]),
            Some(dir.into_os_string()),
            &no_env,
            None,
        );
        assert_eq!(set.capabilities(), vec!["alpha", "beta"], "sorted tag list");
    }

    #[test]
    fn discovery_requires_every_declared_executor_companion() {
        let dir = scratch("discovery-companions");
        fake_cli(&dir, "bundle-cli", "exit 0");
        let spec = CapabilitySpec::parse(
            r#"
spec = 1
[capability]
tag = "bundle"
[detect]
bin = "bundle-cli"
companions = ["bundle-helper"]
[invoke]
args = []
prompt = "stdin"
[output]
format = "text"
"#,
            "test",
        )
        .unwrap();

        let absent = discover_with(
            SpecSet::from_specs(vec![spec.clone()]),
            Some(dir.clone().into_os_string()),
            &no_env,
            None,
        );
        assert!(
            absent.find("bundle").is_none(),
            "a partial executor bundle must not be announced"
        );

        fake_cli(&dir, "bundle-helper", "exit 0");
        let present = discover_with(
            SpecSet::from_specs(vec![spec]),
            Some(dir.into_os_string()),
            &no_env,
            None,
        );
        assert_eq!(present.capabilities(), ["bundle"]);
    }

    #[test]
    fn explicit_override_wins_and_a_broken_override_announces_nothing() {
        let dir = scratch("discovery-override");
        let spec = CapabilitySpec::parse(
            r#"
spec = 1
[capability]
tag = "alpha"
[detect]
bin = "alpha-cli"
env = "MOCK_ALPHA_BIN"
[invoke]
args = []
prompt = "stdin"
[output]
format = "text"
"#,
            "test",
        )
        .unwrap();

        // the env override names a real executable: discovered, PATH unused.
        let real = fake_cli(&dir, "my-alpha", "exit 0");
        let real_os = real.into_os_string();
        let env = move |k: &str| (k == "MOCK_ALPHA_BIN").then(|| real_os.clone());
        let set = discover_with(SpecSet::from_specs(vec![spec.clone()]), None, &env, None);
        assert_eq!(set.capabilities(), vec!["alpha"]);

        // ... and a dangling override is absent, not a silent PATH fallback.
        let missing = dir.join("nope").into_os_string();
        let env = move |k: &str| (k == "MOCK_ALPHA_BIN").then(|| missing.clone());
        fake_cli(&dir, "alpha-cli", "exit 0");
        let set = discover_with(
            SpecSet::from_specs(vec![spec]),
            Some(dir.into_os_string()),
            &env,
            None,
        );
        assert!(
            set.find("alpha").is_none(),
            "broken override must not fall back to PATH"
        );
    }

    #[test]
    fn discovery_probes_once_per_unique_binary_and_fans_out() {
        // a [[variants]] family puts many tags over one binary; discovery
        // must resolve that binary ONCE and fan the result out. the env
        // closure is the observable probe: with an env override set, every
        // probe consults it exactly once.
        let dir = scratch("discovery-dedup");
        let real = fake_cli(&dir, "shared-cli", "exit 0");
        let calls = std::cell::Cell::new(0u32);
        let real_os = real.into_os_string();
        let env = |k: &str| {
            (k == "MOCK_SHARED_BIN").then(|| {
                calls.set(calls.get() + 1);
                real_os.clone()
            })
        };
        let shared = |tag: &str| {
            CapabilitySpec::parse(
                &format!(
                    r#"
spec = 1
[capability]
tag = "{tag}"
[detect]
bin = "shared-cli"
env = "MOCK_SHARED_BIN"
[invoke]
args = []
prompt = "stdin"
[output]
format = "text"
"#
                ),
                "test",
            )
            .unwrap()
        };
        let set = discover_with(
            SpecSet::from_specs(vec![shared("alpha"), shared("beta"), shared("gamma")]),
            None,
            &env,
            None,
        );
        assert_eq!(
            set.capabilities(),
            vec!["alpha", "beta", "gamma"],
            "one resolved binary serves every tag sharing it"
        );
        assert_eq!(calls.get(), 1, "one probe for three tags, not three");
    }

    #[test]
    fn non_executable_files_are_not_discovered() {
        let dir = scratch("discovery-noexec");
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("alpha-cli");
        std::fs::write(&path, "not a program").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let set = discover_with(
            SpecSet::from_specs(vec![mock_spec("alpha", "alpha-cli", "text")]),
            Some(dir.into_os_string()),
            &no_env,
            None,
        );
        assert!(set.find("alpha").is_none(), "mode 644 is not executable");
    }

    #[test]
    fn an_operator_spec_discovers_a_custom_executor() {
        // the whole point of the spec format: a new executor with ZERO code.
        let dir = scratch("custom-exec");
        fake_cli(&dir, "myllm", "cat > /dev/null\necho hi");
        let custom = CapabilitySpec::parse(
            r#"
spec = 1
[capability]
tag = "myllm"
[detect]
bin = "myllm"
[invoke]
args = []
prompt = "stdin"
[output]
format = "text"
"#,
            "test",
        )
        .unwrap();
        let specs = SpecSet::from_specs(vec![custom]);
        let set = discover_with(specs, Some(dir.into_os_string()), &no_env, None);
        assert_eq!(set.capabilities(), vec!["myllm"]);
    }

    /// This crate never resolves an operator root itself.
    ///
    /// A source-parsing lint over every `src/*.rs` because the SHAPE is the
    /// property: the capability directory is the WORKSPACE's
    /// (`<workspace>/capabilities`), handed to [`discover`] by the daemon that
    /// knows which workspace it serves. Re-derive a root here and a node
    /// under `DUCKTAPE_HOME=/srv/duck` finds its keys, executors and images
    /// in its workspace while reading its capability specs out of `$HOME` —
    /// silently, since an absent dir just means "no operator specs". The
    /// needles carry their own quotes, so the escaped spellings on these
    /// lines are not themselves hits.
    #[test]
    fn operator_spec_root_is_never_resolved_in_this_crate() {
        const OVERRIDE_NEEDLE: &str = "\"DUCKTAPE_HOME\"";
        const DEFAULT_NEEDLE: &str = "\".ducktape\"";

        let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(&src_dir).expect("the crate's src directory") {
            let path = entry.expect("a src directory entry").path();
            let is_rust_source = path.extension().is_some_and(|ext| ext == "rs");
            if !is_rust_source {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("a readable source file");
            let reads_the_override = src.contains(OVERRIDE_NEEDLE);
            let spells_the_default = src.contains(DEFAULT_NEEDLE);
            if reads_the_override || spells_the_default {
                offenders.push(path.display().to_string());
            }
        }
        assert!(
            offenders.is_empty(),
            "these files resolve an operator root themselves; the capability directory is \
             the caller's workspace to name (`discover` takes it): {offenders:?}"
        );
    }

    // ---- resolve() ----------------------------------------------------------

    #[test]
    fn resolve_finds_installed_capabilities_and_names_every_failure() {
        let dir = scratch("resolve");
        fake_cli(&dir, "alpha-cli", "exit 0");
        let set = discover_with(
            SpecSet::from_specs(vec![
                mock_spec("alpha", "alpha-cli", "text"),
                mock_spec("beta", "beta-cli", "text"),
            ]),
            Some(dir.into_os_string()),
            &no_env,
            None,
        );

        // an installed capability resolves to its provider.
        let p = set.resolve("alpha").unwrap();
        assert_eq!(p.capability(), "alpha");

        // known-but-not-installed names the capability and what IS provided.
        let err = set.resolve("beta").err().expect("beta is not installed");
        assert!(err.contains("\"beta\" is not provided"), "got: {err}");
        assert!(err.contains("alpha"), "names what the node provides: {err}");

        // a tag no loaded spec claims is a distinct error naming the loaded set.
        let err = set.resolve("gamma").err().expect("no gamma spec");
        assert!(err.contains("no capability spec is loaded"), "got: {err}");
        assert!(err.contains("alpha"), "names the loaded specs: {err}");

        // an empty set fails cleanly, not a panic.
        let err = ProviderSet::empty()
            .resolve("anything")
            .err()
            .expect("empty set");
        assert!(err.contains("no capability spec is loaded"), "got: {err}");
    }

    // ---- providers end-to-end ------------------------------------------------

    #[tokio::test]
    async fn jsonl_provider_round_trips_the_prompt() {
        let dir = scratch("jsonl-run");
        // echo the stdin prompt back inside an agent_message event, plus noise
        // lines the parser must skip.
        let bin = fake_cli(
            &dir,
            "events",
            r#"prompt=$(cat)
echo "not json"
printf '{"type":"item.completed","item":{"type":"agent_message","text":"echo: %s"}}\n' "$prompt""#,
        );
        let p = mock_provider("events", "jsonl-events", bin, "jsonl-run-wd");
        let text = p.run("ping", &RunContext::default()).await.unwrap();
        assert_eq!(text, "echo: ping", "prompt fed on stdin, text parsed back");
    }

    #[tokio::test]
    async fn jsonl_parser_takes_the_last_agent_message() {
        let dir = scratch("jsonl-last");
        let bin = fake_cli(
            &dir,
            "events",
            r#"cat > /dev/null
printf '{"type":"item.completed","item":{"type":"agent_message","text":"first"}}\n'
printf '{"type":"item.completed","item":{"item_type":"agent_message","text":"second"}}\n'
printf '{"type":"turn.completed","usage":{"input_tokens":24763,"cached_input_tokens":24448,"output_tokens":122,"reasoning_output_tokens":7}}\n'"#,
        );
        let p = mock_provider("events", "jsonl-events", bin, "jsonl-last-wd");
        let output = p.run_with_usage("x", &RunContext::default()).await.unwrap();
        assert_eq!(output.text, "second");
        assert_eq!(
            output.usage,
            Some(TokenUsage {
                input_tokens: 24763,
                cached_input_tokens: 24448,
                cache_write_input_tokens: 0,
                output_tokens: 122,
                reasoning_output_tokens: 7,
            })
        );
    }

    #[tokio::test]
    async fn json_result_provider_parses_the_result_object() {
        let dir = scratch("result-run");
        let bin = fake_cli(
            &dir,
            "result",
            r#"cat > /dev/null
printf '{"type":"result","subtype":"success","is_error":false,"result":"pong","usage":{"input_tokens":10,"cache_read_input_tokens":20,"cache_creation_input_tokens":30,"output_tokens":4}}\n'"#,
        );
        let p = mock_provider("result", "json-result", bin, "result-run-wd");
        let output = p
            .run_with_usage("ping", &RunContext::default())
            .await
            .unwrap();
        assert_eq!(output.text, "pong");
        assert_eq!(output.usage.unwrap().input_tokens, 60);
    }

    #[tokio::test]
    async fn json_result_errors_surface_as_errors() {
        let dir = scratch("result-err");
        let bin = fake_cli(
            &dir,
            "result",
            r#"cat > /dev/null
printf '{"type":"result","subtype":"error_max_turns","is_error":true,"result":"boom"}\n'"#,
        );
        let p = mock_provider("result", "json-result", bin, "result-err-wd");
        let err = p.run("ping", &RunContext::default()).await.unwrap_err();
        assert!(err.contains("error result"), "got: {err}");
    }

    #[tokio::test]
    async fn text_format_returns_trimmed_stdout_and_rejects_empty() {
        let dir = scratch("text-run");
        let bin = fake_cli(&dir, "plain", "cat > /dev/null\necho '  the answer  '");
        let p = mock_provider("plain", "text", bin, "text-run-wd");
        assert_eq!(
            p.run("q", &RunContext::default()).await.unwrap(),
            "the answer"
        );

        // "ran fine, said nothing" is a broken executor, not an answer.
        let silent = fake_cli(&dir, "silent", "cat > /dev/null");
        let p = mock_provider("silent", "text", silent, "text-silent-wd");
        let err = p.run("q", &RunContext::default()).await.unwrap_err();
        assert!(err.contains("no output"), "got: {err}");

        let usage_shaped = fake_cli(
            &dir,
            "plain-json",
            r#"cat > /dev/null
printf '{"usage":{"input_tokens":999},"answer":"still plain text"}\n'"#,
        );
        let output = mock_provider("plain-json", "text", usage_shaped, "plain-json-wd")
            .run_with_usage("q", &RunContext::default())
            .await
            .unwrap();
        assert_eq!(output.usage, None, "answer JSON is not telemetry");
    }

    #[tokio::test]
    async fn spec_args_are_passed_verbatim_no_placeholders() {
        let dir = scratch("arg-verbatim");
        // the fake prints its FIRST ARG — including braces, which used to be
        // substitution syntax and must now arrive untouched.
        let spec = CapabilitySpec::parse(
            r#"
spec = 1
[capability]
tag = "argecho"
[detect]
bin = "argecho"
[invoke]
args = ["{model}"]
prompt = "stdin"
[output]
format = "text"
"#,
            "test",
        )
        .unwrap();
        let bin = fake_cli(
            &dir,
            "argecho",
            r#"cat > /dev/null
echo "arg=$1""#,
        );
        // run via sh_provider: the script becomes $0 and the spec's
        // untouched args follow, so "{model}" still arrives as $1.
        let p = sh_provider(spec, bin, "arg-verbatim-wd");
        assert_eq!(
            p.run("q", &RunContext::default()).await.unwrap(),
            "arg={model}",
            "argv is literal"
        );
    }

    #[tokio::test]
    async fn a_failing_cli_surfaces_status_and_stderr() {
        let dir = scratch("cli-fail");
        let bin = fake_cli(
            &dir,
            "flaky",
            "cat > /dev/null\necho 'auth missing' >&2\nexit 3",
        );
        let p = mock_provider("flaky", "text", bin, "cli-fail-wd");
        let err = p.run("x", &RunContext::default()).await.unwrap_err();
        assert!(err.contains("auth missing"), "stderr in error: {err}");
        assert!(err.contains("exited with"), "status in error: {err}");
    }

    #[tokio::test]
    async fn output_without_an_agent_message_is_an_error_not_empty() {
        let dir = scratch("no-message");
        let bin = fake_cli(
            &dir,
            "events",
            r#"cat > /dev/null
printf '{"type":"turn.completed"}\n'"#,
        );
        let p = mock_provider("events", "jsonl-events", bin, "no-message-wd");
        let err = p.run("x", &RunContext::default()).await.unwrap_err();
        assert!(err.contains("no agent message"), "got: {err}");
    }

    #[tokio::test]
    async fn a_hung_cli_is_killed_at_the_timeout() {
        // a SILENT child dies at the idle window — the pre-refresh contract,
        // unchanged for CLIs that emit nothing while stuck.
        let dir = scratch("hang");
        let bin = fake_cli(&dir, "sleeper", "sleep 30");
        let p = mock_provider("sleeper", "text", bin, "hang-wd")
            .with_timeout(Duration::from_millis(200));
        let err = p.run("x", &RunContext::default()).await.unwrap_err();
        assert!(err.contains("timed out"), "got: {err}");
        assert!(err.contains("no output"), "names the idle window: {err}");
    }

    #[test]
    fn an_accepted_extension_wins_when_the_old_timer_wakes() {
        let start = tokio::time::Instant::now();
        let idle = Duration::from_millis(200);
        let old_timer = start + idle;
        let hard = start + Duration::from_secs(2);
        assert_eq!(
            effective_provider_deadline(start, idle, None, hard),
            old_timer
        );

        // Model the scheduler boundary directly: the old sleep is ready at
        // old_timer while the watch already carries the synchronously granted
        // later deadline. The timeout branch's mandatory re-read must continue.
        let granted = old_timer + Duration::from_millis(500);
        let refreshed = effective_provider_deadline(start, idle, Some(granted), hard);
        assert_eq!(refreshed, granted);
        assert!(refreshed > old_timer);
        assert_eq!(
            effective_provider_deadline(start, idle, Some(hard + Duration::from_secs(1)), hard,),
            hard,
            "the same re-read never outranks the hard cap"
        );
    }

    #[tokio::test]
    async fn a_silent_cli_can_extend_only_its_live_broker_invocation() {
        let dir = scratch("idle-control");
        let endpoint_file = dir.join("control-endpoint");
        let bin = fake_cli(
            &dir,
            "controlled-sleeper",
            &format!(
                "cat > /dev/null\n\
                 printf '%s\\n%s\\n' \"$DUCKTAPE_PROVIDER_CONTROL_URL\" \
                   \"$DUCKTAPE_PROVIDER_CONTROL_TOKEN\" > {}\n\
                 sleep 0.45\n\
                 echo survived",
                endpoint_file.display()
            ),
        );
        let provider = sh_provider(broker_spec("controlled-sleeper"), bin, "idle-control-wd")
            .with_timeout(Duration::from_millis(200));
        let broker = broker::RunBroker::start_for_test().await;
        let args = provider.spec.args.clone();
        let workdir = provider.workdir.clone();
        let running = tokio::spawn(async move {
            provider
                .invoke(
                    "x",
                    &args,
                    &workdir,
                    &RunContext::default(),
                    None,
                    Some(&broker),
                )
                .await
        });

        let (url, token) = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(contents) = std::fs::read_to_string(&endpoint_file) {
                    let values: Vec<_> = contents.lines().collect();
                    if values.len() == 2 && values.iter().all(|value| !value.is_empty()) {
                        break (values[0].to_string(), values[1].to_string());
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the provider receives its ambient control endpoint");
        let reply: serde_json::Value = reqwest::Client::new()
            .post(url)
            .header("x-ducktape-provider-control", token)
            .json(&serde_json::json!({
                "request_id":"silent-phase",
                "requested_secs":1,
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(reply["status"], "granted");
        let invocation = tokio::time::timeout(Duration::from_secs(2), running)
            .await
            .expect("the extension is bounded")
            .expect("provider task does not panic")
            .unwrap();
        assert_eq!(invocation.text, "survived");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_kills_the_whole_provider_process_group() {
        let dir = scratch("cancel-process-group");
        let pidfile = dir.join("group.pid");
        let bin = fake_cli(
            &dir,
            "term-ignorer",
            &format!(
                "cat > /dev/null\ntrap '' TERM\necho $$ > {}\nsleep 30 &\nwait",
                pidfile.display()
            ),
        );
        let provider = mock_provider("term-ignorer", "text", bin, "cancel-process-group-wd")
            .with_timeout(Duration::from_secs(30));
        let cancellation = RunCancellation::new();
        let ctx = RunContext {
            cancellation: Some(cancellation.clone()),
            ..RunContext::default()
        };
        let running = tokio::spawn(async move { provider.run("x", &ctx).await });

        let group = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(&pidfile)
                    && let Ok(pid) = pid.trim().parse::<u32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake provider reports its process group");

        cancellation.cancel();
        let error = tokio::time::timeout(Duration::from_secs(6), running)
            .await
            .expect("TERM grace is bounded and escalates to KILL")
            .expect("provider task does not panic")
            .unwrap_err();
        assert!(error.contains("cancelled"), "got: {error}");

        tokio::time::timeout(Duration::from_secs(1), async {
            while process_group_alive(group) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("no TERM-ignoring descendant survives the run");
    }

    #[tokio::test]
    async fn a_streaming_cli_refreshes_the_timeout_and_outlives_the_window() {
        // THE refreshable-timeout property: total runtime (≈1s) is far past
        // the idle window (200ms), but the child emits a heartbeat every
        // 100ms — activity refreshes the window, so a long agentic run that
        // keeps streaming is never killed mid-work.
        let dir = scratch("heartbeat");
        let bin = fake_cli(
            &dir,
            "beats",
            "cat > /dev/null\n\
             for i in 1 2 3 4 5 6 7 8 9 10; do echo tick-$i >&2; sleep 0.1; done\n\
             printf '%s\\n' 'survived the window'",
        );
        let p = mock_provider("beats", "text", bin, "heartbeat-wd")
            .with_timeout(Duration::from_millis(200));
        assert_eq!(
            p.run("x", &RunContext::default()).await.unwrap(),
            "survived the window",
            "a streaming run outlives many idle windows"
        );
    }

    #[tokio::test]
    async fn a_chatty_forever_cli_is_killed_at_the_hard_cap() {
        // the ceiling behind the refresh: a child that streams forever is
        // still bounded — idle × the spec's hard_timeout_factor ends it.
        let dir = scratch("chatty");
        let bin = fake_cli(
            &dir,
            "chatterbox",
            "cat > /dev/null\nwhile true; do echo tick >&2; sleep 0.01; done",
        );
        let p = mock_provider("chatterbox", "text", bin, "chatty-wd")
            .with_timeout(Duration::from_millis(50));
        let start = std::time::Instant::now();
        let err = p.run("x", &RunContext::default()).await.unwrap_err();
        assert!(err.contains("hard cap"), "names the ceiling: {err}");
        assert!(
            start.elapsed() >= Duration::from_millis(1500)
                && start.elapsed() < Duration::from_secs(5),
            "killed at ~idle × {}, not the idle window: {:?}",
            p.hard_timeout_factor,
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn a_run_writing_past_the_output_cap_is_terminated_not_truncated() {
        // a continuously-writing guest must be TERMINATED at the cap, never
        // truncated and parsed anyway — a truncated JSON/JSONL blob would
        // otherwise land as the run's "answer". idle stays generous (5s) so
        // the output cap fires first, not the idle/hard timeout.
        let dir = scratch("output-cap");
        let bin = fake_cli(
            &dir,
            "firehose",
            // a 100_000-byte chunk per iteration (no per-byte forking) clears
            // the 4 MiB cap in ~42 writes rather than thousands of small ones.
            "cat > /dev/null\n\
             big=$(printf '%0100000d' 0)\n\
             while true; do printf '%s' \"$big\"; done",
        );
        let p = mock_provider("firehose", "text", bin, "output-cap-wd")
            .with_timeout(Duration::from_secs(10));
        let err = p.run("x", &RunContext::default()).await.unwrap_err();
        assert!(
            err.contains("output_cap_exceeded"),
            "names the outcome: {err}"
        );
    }

    #[test]
    fn push_bounded_tail_keeps_the_last_bytes_only() {
        let mut buf = Vec::new();
        push_bounded_tail(&mut buf, b"0123456789", 4);
        assert_eq!(buf, b"6789", "oldest bytes drop first");
        push_bounded_tail(&mut buf, b"ABC", 4);
        assert_eq!(buf, b"9ABC", "the tail keeps rolling as more arrives");
    }

    #[tokio::test]
    async fn a_prompt_larger_than_the_pipe_buffer_does_not_deadlock() {
        let dir = scratch("big-prompt");
        // the fake streams output BEFORE draining stdin — the deadlock shape a
        // sequential write-then-wait would hit with a >64KiB prompt.
        let bin = fake_cli(
            &dir,
            "events",
            r#"printf '{"type":"item.completed","item":{"type":"agent_message","text":"ok"}}\n'
cat > /dev/null"#,
        );
        let p = mock_provider("events", "jsonl-events", bin, "big-prompt-wd")
            .with_timeout(Duration::from_secs(10));
        let big = "x".repeat(256 * 1024);
        assert_eq!(p.run(&big, &RunContext::default()).await.unwrap(), "ok");
    }

    #[tokio::test]
    async fn output_sink_receives_lines_before_child_exit_and_stdout_still_parses() {
        let dir = scratch("live-output");
        let gate = dir.join("continue");
        let bin = fake_cli(
            &dir,
            "tailer",
            r#"cat > /dev/null
printf 'first\n'
while [ ! -f "$1" ]; do
  sleep 0.05
done
printf 'diagnostic\n' >&2
printf 'second\n'"#,
        );
        let spec = CapabilitySpec::parse(
            &format!(
                r#"
spec = 1
[capability]
tag = "tailer"
[detect]
bin = "tailer"
[invoke]
args = ["{}"]
prompt = "stdin"
[output]
format = "text"
"#,
                gate.display()
            ),
            "test",
        )
        .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink: OutputSink = Arc::new(move |_ctx, line| {
            tx.send(line).expect("test receiver is alive");
        });
        let p = sh_provider(spec, bin, "live-output-wd").with_output_sink(sink);

        let run = tokio::spawn(async move {
            let ctx = RunContext::default();
            p.run("q", &ctx).await
        });
        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("first line should arrive before the child exits")
            .expect("sink remains connected");
        assert_eq!(
            first,
            OutputLine {
                stream: OutputStream::Stdout,
                line: "first".into()
            }
        );
        assert!(
            !run.is_finished(),
            "the child must still be blocked after the first live line"
        );

        std::fs::write(&gate, b"go").expect("release the child");
        let text = tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .expect("child should finish after the gate is released")
            .expect("run task should not panic")
            .expect("provider run succeeds");
        assert_eq!(
            text, "first\nsecond",
            "stdout's final text contract is preserved"
        );

        let mut rest = Vec::new();
        while let Ok(line) = rx.try_recv() {
            rest.push(line);
        }
        assert!(
            rest.contains(&OutputLine {
                stream: OutputStream::Stderr,
                line: "diagnostic".into()
            }),
            "stderr is tailed too: {rest:?}"
        );
        assert!(
            rest.contains(&OutputLine {
                stream: OutputStream::Stdout,
                line: "second".into()
            }),
            "later stdout lines are tailed too: {rest:?}"
        );
    }

    #[test]
    fn spec_timeout_seeds_the_provider_and_global_override_wins() {
        let spec = CapabilitySpec::parse(
            r#"
spec = 1
[capability]
tag = "slowpoke"
[detect]
bin = "slowpoke-cli"
[invoke]
args = []
prompt = "stdin"
timeout_secs = 42
[output]
format = "text"
"#,
            "test",
        )
        .unwrap();
        let p = CliProvider::from_spec(spec.clone(), PathBuf::from("/x"), SandboxBackend::Bare);
        assert_eq!(p.timeout, Duration::from_secs(42), "spec seeds the timeout");

        let dir = scratch("global-timeout");
        fake_cli(&dir, "slowpoke-cli", "exit 0");
        let set = discover_with(
            SpecSet::from_specs(vec![spec]),
            Some(dir.into_os_string()),
            &no_env,
            Some(Duration::from_secs(7)),
        );
        assert_eq!(
            set.capabilities(),
            vec!["slowpoke"],
            "override plumbed without error"
        );
    }

    #[tokio::test]
    async fn workdir_override_env_and_path_entries_apply_to_one_run() {
        let dir = scratch("portable-context");
        let bin = fake_cli(
            &dir,
            "ctx",
            r#"cat > /dev/null
printf '%s\n' "$(pwd)"
printf '%s\n' "$DUCKTAPE_RUN_WORKSPACE"
printf '%s\n' "$PATH"
"#,
        );
        let override_dir = scratch("portable-context-workdir");
        let path_entry = scratch("portable-context-tools");
        let p = sh_provider(
            mock_spec("ctx", "ctx", "text"),
            bin,
            "portable-context-scratch",
        );
        let ctx = RunContext {
            agent_id: Some("bot".into()),
            native_conversation: None,
            run_key: None,
            cancellation: None,
            executing_node: None,
            workdir_override: Some(override_dir.clone()),
            env: BTreeMap::from([(
                "DUCKTAPE_RUN_WORKSPACE".to_string(),
                override_dir.display().to_string(),
            )]),
            path_entries: vec![path_entry.clone()],
            limits: BTreeMap::new(),
            context_doc: None,
            airlock: None,
            operator_credential: None,
        };

        let output = p.run("q", &ctx).await.unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(
            PathBuf::from(lines[0]),
            override_dir.canonicalize().unwrap()
        );
        assert_eq!(lines[1], override_dir.display().to_string());
        assert!(
            lines[2].starts_with(path_entry.to_str().unwrap()),
            "PATH starts with the run-local tool binding path: {:?}",
            lines[2]
        );
    }

    #[tokio::test]
    async fn an_unusable_workdir_override_fails_the_run_never_shares_scratch() {
        // W1: a workdir_override is a mount the provisioner already
        // materialized; if it is unusable the run must FAIL (the saga retries)
        // rather than silently fall back to the capability's SHARED scratch dir
        // — where concurrent runs of different agents would collide and the
        // workspace commit would report a clean tree while the agent's work
        // vanished. an override whose parent is a FILE is
        // `create_dir_all`-impossible regardless of privilege, reproducible as
        // an unprivileged test.
        let dir = scratch("w1-hard-fail");
        let bin = fake_cli(&dir, "wd", "cat > /dev/null\npwd");
        let blocker = scratch("w1-hard-fail-blocker").join("a-file");
        std::fs::write(&blocker, b"x").unwrap();
        let uncreatable = blocker.join("child");
        assert!(
            std::fs::create_dir_all(&uncreatable).is_err(),
            "the override must be genuinely uncreatable for this test to mean anything"
        );

        let p = sh_provider(mock_spec("wd", "wd", "text"), bin, "w1-hard-fail-scratch");
        let ctx = RunContext {
            workdir_override: Some(uncreatable.clone()),
            ..RunContext::default()
        };
        let err = p.run("q", &ctx).await.unwrap_err();
        assert!(
            err.contains("provisioned workspace mount") && err.contains("refusing"),
            "the run fails loudly on an unusable mount: {err}"
        );
        assert!(
            !scratch("w1-hard-fail-scratch").join("anything").exists(),
            "nothing executed in the shared scratch dir"
        );
    }

    // ---- D7 isolation floor (portable env) --------------------------------------

    /// a run's env overlay is ADDITIVE: no `env_clear`, no HOME override.
    /// get_envs (the explicitly-set overlay) is exactly ctx.env — so a headless
    /// CLI's BYO-auth (ambient ANTHROPIC_API_KEY, ~/.claude, &c) survives into
    /// the child.
    #[test]
    fn the_run_env_overlay_is_additive_except_reserved_control_capabilities() {
        let spec = mock_spec("iso", "iso-cli", "text");
        let p = CliProvider::from_spec(spec, PathBuf::from("/bin/true"), SandboxBackend::Bare);
        let workdir = scratch("iso-portable-wd");

        let mut env = BTreeMap::new();
        env.insert("AGENT_TOKEN".to_string(), "abc".to_string());
        let mut expected: BTreeMap<String, Option<String>> = env
            .iter()
            .map(|(k, v)| (k.clone(), Some(v.clone())))
            .collect();
        expected.insert(PROVIDER_CONTROL_URL_ENV.into(), None);
        expected.insert(PROVIDER_CONTROL_TOKEN_ENV.into(), None);

        for workdir_override in [Some(workdir.clone()), None] {
            let ctx = RunContext {
                workdir_override,
                env: env.clone(),
                ..Default::default()
            };
            let cmd = p
                .command(&[], &workdir, &ctx, &RunAuth::default())
                .expect("command");
            let envs: BTreeMap<String, Option<String>> = cmd
                .as_std()
                .get_envs()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.map(|v| v.to_string_lossy().into_owned()),
                    )
                })
                .collect();
            assert_eq!(
                envs, expected,
                "env is additive except reserved control capabilities"
            );
        }
    }

    /// a run INHERITS the ambient `$HOME` — with or without a provisioned
    /// workspace mount — so the headless claude/codex CLI finds its BYO
    /// credentials. This test covers the bare test harness; a microVM run
    /// crosses the D7 filesystem boundary only through its own block devices.
    #[tokio::test]
    async fn runs_inherit_the_ambient_home_for_byo_auth() {
        let dir = scratch("portable-home");
        let bin = fake_cli(&dir, "home", "cat > /dev/null\nprintf '%s' \"$HOME\"");
        let p = mock_provider("home", "text", bin, "portable-home-wd");

        let real_home = std::env::var("HOME").expect("the test env has HOME set");
        let inherited = p.run("x", &RunContext::default()).await.unwrap();
        assert_eq!(inherited, real_home, "a scratch-dir run inherits HOME");

        let mount = scratch("portable-home-mount");
        let ctx = RunContext {
            workdir_override: Some(mount.clone()),
            ..Default::default()
        };
        let mounted_home = p.run("x", &ctx).await.unwrap();
        assert_eq!(
            mounted_home, real_home,
            "a mounted-workspace run ALSO inherits the ambient HOME so BYO-auth works"
        );
    }

    /// The paid-execution guard must stay in front of the boot.
    ///
    /// A source-parsing lint (the `clock_lint` shape) because the SHAPE is the
    /// property and no unit test can reach the seam — it needs `/dev/kvm` and
    /// the guest artifacts. What matters is only that nothing can boot a VM
    /// after the run's attempt was reassigned. Delete the check and this fails,
    /// which is the whole job: the bug it prevents costs a second paid provider
    /// call and leaves no trace in committed state.
    #[test]
    fn a_cancelled_attempt_can_never_boot_a_vm() {
        let src = include_str!("lib.rs");
        let (before_boot, _) = src
            .split_once("microvm::MicroVm::boot(")
            .expect("the microVM boot call");
        let (_, in_this_fn) = before_boot
            .rsplit_once("async fn microvm_boot(")
            .expect("microvm_boot");
        assert!(
            in_this_fn.contains("RunCancellation::is_cancelled"),
            "no cancellation check before the VM boots: a run whose lease \
             expired while its workspace image was being built would boot \
             anyway, and the operator would pay twice for work another node \
             already claimed — the late OracleResult lands as a deterministic \
             no-op, so committed state shows one result and two invoices"
        );
    }

    /// Every argument the spec declares has to arrive, in order, after the
    /// executor. The bug this pins was invisible to a run with no arguments.
    #[test]
    fn the_guest_argv_prepends_the_executor_and_keeps_every_argument() {
        let layout = GuestLayout::new(Path::new("/host/wd"), Path::new("/host/home"));
        let argv = guest_argv(
            Path::new("/usr/bin/claude"),
            &["-c".to_string(), "/host/wd/script.sh".to_string()],
            &layout,
        );
        assert_eq!(argv[0], "/opt/duck/bin/claude");
        assert_eq!(argv[1], "-c", "args[0] is an ARGUMENT, not argv[0]");
        assert_eq!(
            argv[2], "/duck/workspace/script.sh",
            "a host path in an argument is rewritten"
        );
        assert_eq!(argv.len(), 3);
    }

    /// Two runs never share a scratch directory, INCLUDING two runs that carry
    /// no distinguishing coordinates at all.
    ///
    /// The name used to be an FNV hash of `(executing_node, run_key)`, which
    /// looks like collision avoidance and is the opposite: `run_key` is
    /// optional, so every keyless run on one node produced the SAME name. Two
    /// concurrent ones overwrote each other's workspace image, asset image and
    /// manifest; with the directory now removed on teardown, the first to
    /// finish would delete the images out from under the other's live VM.
    ///
    /// The paths are also checked for LENGTH here, because the socket lives
    /// under one: a unix socket path is capped near `SUN_LEN` (108) and
    /// Firecracker appends `_<port>` to it.
    #[test]
    fn two_runs_never_share_a_scratch_directory() {
        let slots: std::collections::BTreeSet<String> = (0..64).map(|_| run_slot()).collect();
        assert_eq!(slots.len(), 64, "every run draws its own slot");

        for slot in &slots {
            assert_eq!(slot.len(), 16, "the slot is 16 hex chars: {slot}");
            let socket_dir = microvm_socket_dir(slot).expect("socket dir");
            let dialled = format!(
                "{}_{}",
                socket_dir.path().join(MICROVM_SOCKET_NAME).display(),
                1024
            );
            assert!(
                dialled.len() < 108,
                "the guest dials {dialled} ({} bytes), past SUN_LEN",
                dialled.len()
            );
            // dropping the guard removes it, which is the property the next
            // test pins.
        }
    }

    /// Nothing is created before the run can be REFUSED.
    ///
    /// `executor_image::ensure` refuses a foreign binary in the operator's
    /// executors directory on every single run, and it used to run after both
    /// of the run's directories existed — so a node in that state leaked a
    /// directory pair per refused attempt, one of them on a tmpfs. A source
    /// lint because the seam needs `/dev/kvm` and the guest artifacts: what is
    /// checkable is the order.
    #[test]
    fn the_executors_image_is_derived_before_any_scratch_exists() {
        let src = include_str!("lib.rs");
        let (body, _) = src
            .split_once("microvm::MicroVm::boot(")
            .expect("the microVM boot call");
        let (_, body) = body
            .rsplit_once("async fn microvm_boot(")
            .expect("microvm_boot");
        let ensure = body
            .find("executor_image::ensure(")
            .expect("microvm_boot derives the executors image");
        let scratch = body
            .find("microvm_run_dir(")
            .expect("microvm_boot creates the run directory");
        assert!(
            ensure < scratch,
            "a refusal from executor_image::ensure must cost no directory"
        );
    }

    /// A refused run leaves neither of its directories behind — the whole
    /// point of handing both to guards.
    #[test]
    fn a_refused_run_leaves_neither_directory() {
        let slot = run_slot();
        let (run_dir, socket_dir) = {
            let run = microvm_run_dir(&slot).expect("run dir");
            let socket = microvm_socket_dir(&slot).expect("socket dir");
            assert!(run.path().is_dir() && socket.path().is_dir());
            // #1693: the run directory (holding manifest.bin and the run's
            // workspace/asset images once boot writes them) must not be
            // world- or group-readable — a bare `create_dir_all` under a
            // normal umask lands 0755.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let mode = std::fs::metadata(run.path())
                    .expect("run dir metadata")
                    .permissions()
                    .mode();
                assert_eq!(
                    mode & 0o077,
                    0,
                    "{} is readable by group/other: {mode:o}",
                    run.path().display()
                );
            }
            (run.path().to_path_buf(), socket.path().to_path_buf())
            // both guards drop here, as they do on every `?` in `microvm_boot`
            // before the VM exists.
        };
        assert!(!run_dir.exists(), "{} survived", run_dir.display());
        assert!(!socket_dir.exists(), "{} survived", socket_dir.display());
    }

    /// The production shape end to end: the operator's OWN installed CLI, in an
    /// image derived from their executors directory, exec'd inside the guest.
    ///
    /// The live tests around this one put a symlink to a rootfs tool in a
    /// scratch directory — enough to prove the mount, not enough to prove the
    /// thing an operator actually gets: a 300 MB `mke2fs -d` of real binaries,
    /// attached to a rootfs that carries no CLI at all. `--version` because it
    /// needs no credential and no network: the question here is whether the
    /// bytes exec, not what they do.
    ///
    /// On THIS host's hypervisor, not Firecracker's: a Mac attaching the
    /// executors device makes six block devices on a VZ machine, and whether
    /// Virtualization.framework takes six is a question only a Mac can answer.
    #[tokio::test]
    #[ignore = "live: needs a hypervisor, a built guest rootfs, and `ducktape agent install codex`"]
    async fn the_installed_cli_execs_from_its_derived_image() {
        let backend = platform_backend();
        if let Err(why) = backend.probe() {
            eprintln!("skipping: {why}");
            return;
        }
        let executors = installed_executor_dir();
        let bin = executors.join("codex");
        if !bin.is_file() {
            eprintln!("skipping: no codex in {}", executors.display());
            return;
        }

        let root = scratch("installed-cli");
        std::fs::create_dir_all(root.join("wd")).unwrap();
        let spec = CapabilitySpec::parse(
            r#"
spec = 1
[capability]
tag = "installed-cli"
[detect]
bin = "codex"
[invoke]
args = ["--version"]
prompt = "stdin"
[output]
format = "text"
"#,
            "test",
        )
        .unwrap();
        let provider = CliProvider::from_spec(spec, bin, backend).with_workdir(root.join("wd"));

        let ctx = RunContext {
            executing_node: Some(execution_node_id(b"installed-cli")),
            limits: [("cores".to_string(), 1u64), ("mem_gb".to_string(), 1u64)]
                .into_iter()
                .collect(),
            ..RunContext::default()
        };
        let answer = provider
            .run("", &ctx)
            .await
            .expect("the installed CLI runs inside a microVM");
        eprintln!("--- installed CLI answer: {answer:?} ---");
        assert!(
            answer.contains("codex"),
            "the guest exec'd the codex from the executors image: {answer:?}"
        );
    }

    #[tokio::test]
    #[ignore = "live: needs a hypervisor, guest rootfs, and an installed Pi bundle"]
    async fn installed_pi_bundle_execs_inside_a_microvm() {
        let backend = platform_backend();
        backend
            .probe()
            .expect("configured hypervisor and guest images");
        let bin = installed_executor_dir().join("pi");
        assert!(bin.is_file(), "install Pi in DUCKTAPE_EXECUTOR_DIR");
        let root = scratch("installed-pi");
        let mut spec = spec::builtin_specs()
            .into_iter()
            .find(|spec| spec.tag == "pi")
            .unwrap();
        spec.args = vec!["--version".into()];
        spec.output = OutputFormat::Text;
        spec.isolation.broker = None;
        let provider = CliProvider::from_spec(spec, bin, backend).with_workdir(root.join("wd"));
        let ctx = RunContext {
            executing_node: Some(execution_node_id(b"installed-pi")),
            limits: [("cores".into(), 1), ("mem_gb".into(), 1)]
                .into_iter()
                .collect(),
            ..RunContext::default()
        };
        let answer = provider
            .run("", &ctx)
            .await
            .expect("Pi bundle runs in the guest");
        assert_ne!(answer, "0.0.0", "Pi must find its sibling package metadata");
        assert!(answer.split('.').count() >= 3, "Pi version: {answer:?}");
    }

    /// The full hardware path on a real VMM: a spec's argv and prompt reach a
    /// CLI inside the guest, and the file it wrote comes back on the host.
    #[tokio::test]
    #[ignore = "live: needs /dev/kvm and a built guest rootfs"]
    async fn firecracker_hardware_smoke() {
        let backend = firecracker_backend_with(executors_of("firecracker-hardware-bin", &["sh"]));
        if let Err(why) = backend.probe() {
            eprintln!("skipping: {why}");
            return;
        }

        let root = scratch("firecracker-hardware");
        let workdir = root.join("workspace");
        std::fs::create_dir_all(&workdir).unwrap();

        let spec = CapabilitySpec::parse(
            &format!(
                r#"
spec = 1
[capability]
tag = "hardware-smoke"
[detect]
bin = "sh"
[invoke]
args = ["-c", "{SMOKE_SCRIPT}"]
prompt = "stdin"
[output]
format = "text"
"#
            ),
            "test",
        )
        .unwrap();
        // resolved by basename to /opt/duck/bin/sh, which is where the
        // executors image the backend above names gets mounted.
        let provider = CliProvider::from_spec(spec, PathBuf::from("/bin/sh"), backend);
        let ctx = RunContext {
            workdir_override: Some(workdir.clone()),
            limits: BTreeMap::from([("cores".into(), 2), ("mem_gb".into(), 4)]),
            executing_node: Some(execution_node_id(b"hardware-smoke")),
            ..RunContext::default()
        };

        let answer = provider
            .run("hardware-prompt", &ctx)
            .await
            .expect("real sandbox provider cycle");
        assert_eq!(answer, "sandbox-ok:hardware-prompt");
        assert_eq!(
            std::fs::read_to_string(workdir.join("sandbox-marker.txt")).unwrap(),
            "hardware-prompt",
            "the sandbox must sync its writable workspace back to the host"
        );
        std::fs::remove_dir_all(root).ok();
    }

    /// one of this host's off-loopback addresses, or `None` when it has none:
    /// the route a connected UDP socket picks (connecting sends nothing) names
    /// the address the kernel would speak from.
    fn off_loopback_address() -> Option<std::net::Ipv4Addr> {
        let probe = std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).ok()?;
        // TEST-NET-1: a destination that exists only to pick the route.
        probe.connect("192.0.2.1:9").ok()?;
        match probe.local_addr().ok()?.ip() {
            std::net::IpAddr::V4(ip) if egress_proxy::off_host(std::net::IpAddr::V4(ip)) => {
                Some(ip)
            }
            _ => None,
        }
    }

    /// LIVE: the guest's way off this host is the egress proxy on its own
    /// tunnel. A responder bound on one of this host's off-loopback addresses
    /// (which the proxy admits) answers a guest `curl` through `$HTTPS_PROXY`
    /// twice — absolute-form, then through CONNECT — and the link-local
    /// metadata address is refused, because the proxy never dials this host
    /// or anything beside it. The guest has no network device: every byte of this went over
    /// the vsock tunnel `wire_guest_tunnels` opened for the proxy.
    ///
    /// `#[ignore]`: needs `/dev/kvm`, the guest artifacts, and an off-loopback
    /// address on this host.
    ///   DUCKTAPE_GUEST_DIR=… cargo test -p provider-host --lib -- --ignored \
    ///     --nocapture firecracker_egress
    #[tokio::test]
    #[ignore = "live: needs /dev/kvm, a built guest rootfs and an off-loopback address"]
    async fn firecracker_egress_proxy_relays_off_host_and_refuses_this_host() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let backend = firecracker_backend_with(executors_of("firecracker-egress-bin", &["sh"]));
        if let Err(why) = backend.probe() {
            eprintln!("skipping: {why}");
            return;
        }
        let Some(lan) = off_loopback_address() else {
            eprintln!("skipping: this host has no off-loopback address");
            return;
        };
        let responder = tokio::net::TcpListener::bind((lan, 0))
            .await
            .expect("bind the responder on an off-loopback address");
        let port = responder.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = responder.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut head = [0u8; 4096];
                    let _ = stream.read(&mut head).await;
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 9\r\nconnection: close\r\n\r\negress-ok",
                        )
                        .await;
                });
            }
        });

        let root = scratch("firecracker-egress");
        let workdir = root.join("workspace");
        std::fs::create_dir_all(&workdir).unwrap();
        // the three legs: absolute-form through the proxy, CONNECT through the
        // proxy (`-p`), and the link-local metadata address beside this host,
        // which the proxy refuses. (The host's own loopback is not a leg: the
        // proxy names it in NO_PROXY so the guest's node lane and broker stay
        // direct, and curl honours that by not asking the proxy at all.)
        let script = format!(
            "cat >/dev/null; \
             a=$(curl -s --proxy $HTTPS_PROXY http://{lan}:{port}/); \
             b=$(curl -s -p --proxy $HTTPS_PROXY http://{lan}:{port}/); \
             c=$(curl -s -o /dev/null -w %{{http_code}} --proxy $HTTPS_PROXY http://169.254.169.254/); \
             printf %s_%s_%s $a $b $c"
        );
        let spec = CapabilitySpec::parse(
            &format!(
                r#"
spec = 1
[capability]
tag = "egress-smoke"
[detect]
bin = "sh"
[invoke]
args = ["-c", "{script}"]
prompt = "stdin"
[output]
format = "text"
"#
            ),
            "test",
        )
        .unwrap();
        let provider = CliProvider::from_spec(spec, PathBuf::from("/bin/sh"), backend);
        let ctx = RunContext {
            workdir_override: Some(workdir),
            limits: BTreeMap::from([("cores".into(), 2), ("mem_gb".into(), 4)]),
            executing_node: Some(execution_node_id(b"egress-smoke")),
            ..RunContext::default()
        };

        let answer = provider
            .run("egress-prompt", &ctx)
            .await
            .expect("real sandbox provider cycle");
        eprintln!("--- microVM egress answer: {answer:?} ---");
        assert_eq!(
            answer, "egress-ok_egress-ok_403",
            "absolute-form and CONNECT reach the off-host responder; link-local is refused"
        );
        std::fs::remove_dir_all(root).ok();
    }

    // ---- live model turns ---------------------------------------------------
    //
    // A REAL turn against the operator's own subscription, inside a real VM.
    // What only these can prove: the credential stays on the HOST. The guest
    // has no network device at all, so the CLI's only route to the model API is
    // the vsock tunnel to this run's broker — it dials `127.0.0.1:<port>`,
    // never an address it chose, and the upstream token is attached host-side.
    // An empty answer here means the tunnel, the broker or the argv is wrong;
    // the run cannot silently fall back to a direct connection, because there
    // is nothing to fall back to.

    /// the discovered providers against a live Firecracker backend, or `None`
    /// when this host cannot run one (no `/dev/kvm`, no guest artifacts).
    fn live_microvm_set() -> Option<ProviderSet> {
        let backend = firecracker_backend();
        if let Err(why) = backend.probe() {
            eprintln!("skipping: {why}");
            return None;
        }
        Some(
            discover(
                b"verify-node-000000000000000000000",
                &no_specs(),
                None,
                backend,
                "verify",
            )
            .expect("discover against the microVM backend"),
        )
    }

    /// Both limits are REQUIRED for a VM: unlike a container, there is no
    /// "unbounded" — the hypervisor is told exactly how many vCPUs and how much
    /// memory to build, so a missing limit is a refusal rather than a default.
    fn live_ctx(agent: &str) -> RunContext {
        let mut env = vec![("TERM".to_string(), "xterm-256color".to_string())];
        // let the operator pin a less-throttled tier without editing the test
        if let Ok(model) = std::env::var("ANTHROPIC_MODEL") {
            env.push(("ANTHROPIC_MODEL".to_string(), model));
        }
        RunContext {
            agent_id: Some(agent.into()),
            executing_node: Some(execution_node_id(b"verify-node-000000000000000000000")),
            limits: BTreeMap::from([("cores".into(), 2), ("mem_gb".into(), 4)]),
            env: env.into_iter().collect(),
            ..RunContext::default()
        }
    }

    async fn live_model_turn(tag: &str) {
        let Some(set) = live_microvm_set() else {
            return;
        };
        let provider = match set.resolve(tag) {
            Ok(provider) => provider,
            Err(why) => {
                eprintln!("skipping: no {tag} provider on this host: {why}");
                return;
            }
        };
        let answer = provider
            .run(
                "Reply with exactly one word: PONG. Nothing else.",
                &live_ctx(&format!("verify-{tag}")),
            )
            .await
            .unwrap_or_else(|e| panic!("{tag} model turn inside a microVM: {e}"));
        eprintln!("--- {tag} answer ---\n{answer}\n--- end ---");
        assert!(
            !answer.trim().is_empty(),
            "{tag} returned an empty answer: the guest reached the broker but \
             the model produced nothing"
        );
    }

    /// `#[ignore]`: spends a little claude quota, and needs `/dev/kvm`, the
    /// guest artifacts, and a claude logged in ON THE HOST.
    ///   DUCKTAPE_GUEST_DIR=… cargo test -p provider-host --lib -- --ignored \
    ///     --nocapture claude_model_turn
    #[tokio::test]
    #[ignore = "live model turn: spends claude quota; needs /dev/kvm and a built guest rootfs"]
    async fn claude_model_turn_in_a_microvm() {
        live_model_turn("claude").await;
    }

    /// `#[ignore]`: spends a little codex quota, and needs `/dev/kvm`, the
    /// guest artifacts, and `~/.codex/auth.json` on the host.
    #[tokio::test]
    #[ignore = "live model turn: spends codex quota; needs /dev/kvm and a built guest rootfs"]
    async fn codex_model_turn_in_a_microvm() {
        live_model_turn("codex").await;
    }

    fn env_of(envs: &[(String, String)], key: &str) -> Option<String> {
        envs.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.to_string())
    }

    /// each CLI's own MCP syntax, aimed at the run's own lane — and NOTHING
    /// that names a command, because a command is what used to require a
    /// ducktape binary inside the guest.
    #[test]
    fn each_dialect_points_its_cli_at_this_runs_endpoint_and_nothing_else() {
        let claude = mcp_argv(McpDialect::Claude, "http://127.0.0.1:41999/");
        assert_eq!(claude[0], "--mcp-config");
        let configured: Value = serde_json::from_str(&claude[1]).expect("literal json");
        assert_eq!(configured["mcpServers"]["ducktape"]["type"], "http");
        assert_eq!(
            configured["mcpServers"]["ducktape"]["url"],
            "http://127.0.0.1:41999/mcp",
            "one slash, whatever the base carried"
        );
        assert!(
            configured["mcpServers"]["ducktape"]["command"].is_null(),
            "an http server has no command to exec: {configured}"
        );
        assert_eq!(
            claude[2..],
            ["--allowedTools", "mcp__ducktape"],
            "print mode cannot prompt, so the server must be pre-allowed"
        );

        let codex = mcp_argv(McpDialect::Codex, "http://127.0.0.1:41999");
        assert_eq!(
            codex,
            [
                "-c",
                "mcp_servers.ducktape.url=\"http://127.0.0.1:41999/mcp\"",
                "-c",
                "mcp_servers.ducktape.default_tools_approval_mode=\"approve\"",
            ],
            "the url is TOML, because codex parses a -c override as TOML"
        );
    }

    /// WHERE the wiring lands: after a leading subcommand when there is one,
    /// at the front when there is not. Never blindly after argv[0] — a
    /// restricted claude session opens `--permission-mode plan`, and splitting
    /// a flag from its value is an argv the CLI rejects.
    #[test]
    fn the_mcp_wiring_lands_where_every_executor_accepts_it() {
        let arg = |argv: &[&str]| {
            with_mcp_argv(
                &argv.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
                Some(McpDialect::Codex),
                "http://127.0.0.1:1",
            )
        };
        // codex headless: the subcommand stays first, the trailing stdin
        // marker stays last.
        let headless = arg(&["app-server", "-c", "sandbox_mode=\"x\"", "-"]);
        assert_eq!(headless[0], "app-server");
        assert_eq!(headless[1], "-c");
        assert_eq!(headless.last().unwrap(), "-");

        // a flag-led argv (an interactive TUI one) takes it at the front, so a
        // flag is never separated from its value.
        let restricted = arg(&["--permission-mode", "plan"]);
        assert_eq!(restricted[restricted.len() - 2..], ["--permission-mode", "plan"]);
        assert_eq!(restricted[0], "-c");

        // an empty argv is the bare TUI launch, and gets the wiring alone.
        assert_eq!(arg(&[]).len(), 4);

        // a spec that declares no dialect is untouched, byte for byte.
        let untouched = with_mcp_argv(&["run".to_string()], None, "http://127.0.0.1:1");
        assert_eq!(untouched, ["run"]);
    }

    /// the node lane, which is the run's whole surface: its reads AND, at
    /// [`MCP_PATH`], its tool plane. Without this port the guest has neither.
    ///
    /// The run-action endpoint is deliberately NOT tunnelled: the only thing
    /// that ever called it was the tool plane, which runs on the host now, so
    /// a port into the guest would expose this run's own signer to the model's
    /// sandbox for nobody to use.
    #[test]
    fn the_guest_allowlist_carries_the_node_lane_and_not_the_run_signer() {
        let mut envs = vec![
            (NODE_URL_ENV.into(), "http://127.0.0.1:8844".into()),
            (
                RUN_ACTION_URL_ENV.into(),
                "http://127.0.0.1:41111/v1/run-action".into(),
            ),
        ];
        let ports = wire_guest_tunnels(&mut envs, Some("http://127.0.0.1:54321/v1"));
        assert_eq!(ports, vec![54321, 8844]);
        assert_eq!(
            env_of(&envs, NODE_URL_ENV).as_deref(),
            Some("http://127.0.0.1:8844"),
            "the guest dials the tunnel's own end"
        );
    }

    /// the run's signer is the HOST's: its address and its bearer are read by
    /// the lane to BE the tool plane, and then dropped before the manifest env
    /// is written. A guest holding them would hold a write capability for its
    /// own program account.
    #[test]
    fn the_run_signers_address_and_token_never_reach_the_guest() {
        for key in [RUN_ACTION_URL_ENV, RUN_ACTION_TOKEN_ENV] {
            assert!(host_only_env(key), "{key} must not cross");
        }
        for key in [NODE_URL_ENV, SKILLS_ROOT_ENV, "DUCKTAPE_RUN_AGENT", "HOME"] {
            assert!(!host_only_env(key), "{key} is the run's to read");
        }
    }

    /// a wildcard bind reaches here as loopback already ([`node_http_base`] in
    /// `crates/noded`), so the interesting case is the one that does not: the
    /// var must go with the tunnel it depended on.
    #[test]
    fn a_node_url_that_cannot_be_tunnelled_is_taken_away() {
        for base in [
            "http://192.168.1.5:8844", // a concrete LAN bind: no tunnel reaches it
            "http://[::1]:8844",       // `serve_tunnel` has no v6 dial
            "http://node.local",       // no port to bind either end on
        ] {
            let mut envs = vec![(NODE_URL_ENV.into(), base.to_string())];
            let ports = wire_guest_tunnels(&mut envs, None);
            assert!(ports.is_empty(), "{base} was tunnelled: {ports:?}");
            assert_eq!(
                env_of(&envs, NODE_URL_ENV),
                None,
                "{base} left the guest a node URL it cannot dial"
            );
        }
    }

    /// `localhost:<port>` is the operator's own string (node.toml's
    /// `http_listen` is trusted verbatim when it is not a socket address), and
    /// the guest has no resolver — so the port is tunnelled and the name is
    /// rewritten, path and all.
    #[test]
    fn a_named_loopback_node_url_is_rewritten_for_the_guest() {
        let mut envs = vec![(NODE_URL_ENV.into(), "http://localhost:8844/base".into())];
        let ports = wire_guest_tunnels(&mut envs, None);
        assert_eq!(ports, vec![8844]);
        assert_eq!(
            env_of(&envs, NODE_URL_ENV).as_deref(),
            Some("http://127.0.0.1:8844/base")
        );
    }

    /// no node URL at all (a node serving no http surface) is not a failure:
    /// nothing is tunnelled and nothing is invented.
    #[test]
    fn a_run_with_no_node_url_tunnels_only_its_broker() {
        let mut envs = vec![("HOME".into(), "/home/duck".into())];
        let ports = wire_guest_tunnels(&mut envs, Some("http://127.0.0.1:54321/v1"));
        assert_eq!(ports, vec![54321]);
        assert_eq!(envs.len(), 1);
    }
}
