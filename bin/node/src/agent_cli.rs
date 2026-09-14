//! `ducktape agent` — remote/interactive sandboxed provider sessions, plus the
//! two run-control verbs.
//!
//! Two session verbs, one credential+targeting story:
//!
//! - `agent pty [<harness>] [--host-node <hex>] [--cred <name>] [--cpu <n>] [--mem <gb>]`
//!   attaches THIS terminal to a provider running in a microVM on a host
//!   node (default: this node). The CLI talks ONLY to its own node's ws surface
//!   (`/v1/ws`); the node does the cross-node mesh. Raw terminal mode + resize
//!   forwarding make it feel like ssh.
//! - `agent sched [<harness>] --cred <name> [--host-node <hex>] [--cpu] [--mem] -- "<prompt>"`
//!   submits a durable, node-pinned headless run (a `saga::SagaMsg::Trigger`)
//!   as a frame the USER key signs, and prints its run id. The saga's origin is
//!   the user key, so the lender attributes the run to the user's account
//!   (`OfKey`) when the pinned node asks to draw on `--cred`. The target may be
//!   offline now and execute on reconnect — that durability is the point.
//!
//! Two run-control verbs — `agent cancel <run-id>` and `agent reassign <run-id>
//! [--attempt N]` — act on a run of EITHER lane, and the id itself picks which:
//!
//! - an id inside the signing key's own saga namespace ([`saga::owns_id`]) is a
//!   `sched` run, the one an operator can create here, so cancel/reassign are
//!   `SagaMsg::Cancel`/`SagaMsg::Reassign`. Cancel is the useful one: `agent
//!   sched` PINS its saga to the target node (`pinned_assignee`), and saga
//!   refuses to reassign a pinned saga outright — there is no other provider to
//!   move it to. Reassign on this lane can only fence an attempt;
//! - anything else is a `runs` turn claim from the chat- and jobs-driven lane,
//!   so they are `RunsMsg::CancelRun`/`RunsMsg::ReassignRun`. Both act there.
//!
//! The operator holding a run id has no reason to know which module minted it,
//! and the two id spaces are disjoint, so the CLI asks the id rather than the
//! operator. Both ride the same user-signed frame lane `sched` uses, and neither
//! pre-checks who may act: the module decides on every validator — runs admits
//! the run's creator or the agent's owner, saga only the recorded trigger origin
//! — and its refusal sentence is what comes back.
//!
//! What the lanes do NOT share is how a no-op reads. Runs REFUSES an id it does
//! not hold (`unknown run: …`); saga is deliberately SILENT for a finished,
//! unknown or foreign saga, so a `sched` control op that lands prints
//! "submitted", never "accepted".
//!
//! Which is also how the wrong `--key` reads: a `sched` id in ANOTHER key's
//! namespace is not this key's to control, so it takes the runs lane and comes
//! back `unknown run: ext:<hex>…`. That sentence means "not your run", not "no
//! such run" — sign with the key that submitted the `agent sched`.
//!
//! `<harness>` is optional when `--cred` names a credential: the registry
//! record's kind infers Claude/Codex, or selects explicit Pi's backend at runtime.
//! A native harness contradicting the credential is an error.
//!
//! TWO addressing inputs, deliberately two names. `--node`/`-n`/`DUCKTAPE_NODE`
//! (the shared [`NodeAddr`] group) say which node this CLI DIALS — an http base.
//! `--host-node` says which PEER runs the work: its raw 64-hex node key. They
//! are different types; spelling both `--node` is what made the flag mean two
//! things.
//!
//! Program output stays `println!` (a CLI's stdout is not logging); the pty
//! passthrough writes raw provider bytes straight to stdout.

use std::collections::BTreeMap;
use std::io::BufRead;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use commonware_cryptography::Signer as _;

use crate::cli_args::NodeAddr;
use crate::config::{self, hex_bytes};
use crate::cred_cli::{VerbCtx, query_node};
use crate::userkey_cli::{load_user_signer, user_frame};

type AgentResult = Result<(), Box<dyn std::error::Error>>;

/// `ducktape agent <verb>`. The shared addressing group selects THIS operator's
/// own node — the ws + query surface the CLI dials, never the host the work runs
/// on (that is `--host-node`).
#[derive(Debug, clap::Args)]
pub(crate) struct AgentArgs {
    #[command(subcommand)]
    cmd: AgentCmd,
    #[command(flatten)]
    addr: NodeAddr,
    /// path to the user key file that signs a `sched`, `cancel` or `reassign`
    /// submit (defaults to the keystore's active wallet)
    #[arg(long, value_name = "PATH", global = true)]
    key: Option<std::path::PathBuf>,
}

#[derive(Debug, clap::Subcommand)]
pub(crate) enum AgentCmd {
    /// attach this terminal to a sandboxed provider (raw pty, resize-aware)
    Pty(PtyArgs),
    /// print the current default programmable model-user script as JSON
    ModelProgram {
        #[arg(value_name = "MODEL_ID")]
        model_id: String,
    },
    /// submit a durable headless run pinned to a node; prints its run id
    Sched(SchedArgs),
    /// install the agent CLIs this host's guest image lends to runs
    Install(crate::executors::InstallArgs),
    /// cancel a pending run — a `sched` run of your own, or a `runs` turn
    /// whose creator or agent owner you are
    Cancel(CancelArgs),
    /// fence this attempt and move a pending `runs` turn to another provider
    /// (a `sched` run is PINNED to its node and cannot be moved — cancel it
    /// and submit a new one instead)
    Reassign(ReassignArgs),
}

// EVERY run id both control verbs take carries literal 0x1f separators, so it
// is COPIED, never typed: a `sched` id is `ext:<hex>\x1fsched\x1f<hex>` (what
// `agent sched` printed), a runs turn claim is
// `chat\x1f<channel>\x1f<anchor>\x1f<agent>` or
// `job\x1f<job>\x1f<agent>\x1f<height>` (what the app's run list and the
// `pending_runs` query print). Typing one into bash needs `$'…\x1f…'` quoting.
#[derive(Debug, clap::Args)]
pub(crate) struct CancelArgs {
    /// the run's id: the id `agent sched` printed, or a pending `runs` turn
    /// claim — 0x1f-separated, so quote it: $'chat\x1fgeneral\x1f3\x1fbot'
    #[arg(value_name = "RUN_ID")]
    run_id: String,
}

#[derive(Debug, clap::Args)]
pub(crate) struct ReassignArgs {
    /// the run's id: the id `agent sched` printed, or a pending `runs` turn
    /// claim — 0x1f-separated, so quote it: $'chat\x1fgeneral\x1f3\x1fbot'
    #[arg(value_name = "RUN_ID")]
    run_id: String,
    /// the attempt to FENCE: the run's current attempt, 0 until it has been
    /// reassigned once — and on the runs lane (`RUN_MAX_ATTEMPTS = 2`) the only
    /// one a turn can move. A stale number is a deterministic no-op by design
    /// (that is what stops a delayed click from revoking a newer assignment),
    /// and a no-op still commits, so the printed height names the fence, not a
    /// move.
    #[arg(long, value_name = "N", default_value_t = 0)]
    attempt: u32,
}

/// The executable harness, independent of the credential's backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum HarnessArg {
    Claude,
    Codex,
    Pi,
}

impl HarnessArg {
    pub(crate) fn token(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
        }
    }
}

#[derive(Debug, clap::Args)]
pub(crate) struct PtyArgs {
    /// harness to launch (claude|codex|pi); omitted = infer from --cred
    harness: Option<HarnessArg>,
    /// host node to RUN on: its raw 64-hex node key (omitted = this node).
    /// NOT `--node`, which is the http base this CLI dials.
    #[arg(long = "host-node", value_name = "HEX")]
    host_node: Option<String>,
    /// credential name to serve the session (required for a cross-node host)
    #[arg(long, value_name = "NAME")]
    cred: Option<String>,
    /// cpu-cores ceiling for the sandbox (minimum 2)
    #[arg(long, value_name = "CORES", value_parser = at_least_the_sandbox_floor)]
    cpu: Option<u64>,
    /// memory ceiling in GB for the sandbox
    #[arg(long, value_name = "GB")]
    mem: Option<u64>,
}

#[derive(Debug, clap::Args)]
pub(crate) struct SchedArgs {
    /// harness to launch (claude|codex|pi); omitted = infer from --cred
    harness: Option<HarnessArg>,
    /// credential name (required: a headless guest run must bring a credential).
    /// With `--host-node`, THIS RUN LETS THAT NODE SPEND YOUR SUBSCRIPTION: the
    /// lender admits the executing node on YOUR grant, for this credential and
    /// this run only, until the run reaches a terminal status.
    #[arg(long, value_name = "NAME")]
    cred: String,
    /// node to PIN the run to: its raw 64-hex node key (omitted = this node).
    /// NOT `--node`, which is the http base this CLI dials. The pin is what
    /// scopes the `--cred` draw — it is the only node that may present this run
    /// as its reason for opening a session.
    #[arg(long = "host-node", value_name = "HEX")]
    host_node: Option<String>,
    /// cpu-cores demand (minimum 2)
    #[arg(long, value_name = "CORES", value_parser = at_least_the_sandbox_floor)]
    cpu: Option<u64>,
    /// memory demand in GB
    #[arg(long, value_name = "GB")]
    mem: Option<u64>,
    /// the prompt, after `--`
    #[arg(last = true, value_name = "PROMPT", required = true)]
    prompt: String,
}

/// Refuse a core count no sandbox will accept, AT SUBMIT.
///
/// A zero-core run is accepted by placement and by the lease, and refused only
/// by the spawn on the executing box — the most expensive possible place to
/// find out, because it burns every `RUN_MAX_ATTEMPTS` retry and fails the saga
/// having told the submitter nothing they could act on.
///
/// The floor is 1: a VM is BUILT at a size, so zero vCPUs is not a smaller
/// machine — it is not a machine.
fn at_least_the_sandbox_floor(value: &str) -> Result<u64, String> {
    let cores: u64 = value
        .parse()
        .map_err(|_| format!("{value:?} is not a number of cores"))?;
    if cores == 0 {
        return Err("a sandboxed run needs at least 1 core — try --cpu 1".to_string());
    }
    Ok(cores)
}

pub(crate) fn run(args: AgentArgs) -> AgentResult {
    let AgentArgs { cmd, addr, key } = args;
    let ctx = VerbCtx { addr, key };
    let mut stdin = std::io::BufReader::new(std::io::stdin());
    // `install` fills a directory in the WORKSPACE and talks to no node, so
    // the ladder is resolved per-verb rather than up front: it answers "which
    // workspace" for `install` and "which node" for everything else.
    match cmd {
        // pty takes the whole group, not just the resolved base: attaching needs
        // the node's WORKSPACE too (its 0600 service-link token admits the
        // session's ws topic), and only the ladder knows which workspace the
        // address it just resolved belongs to.
        AgentCmd::ModelProgram { model_id } => cmd_model_program(&model_id),
        AgentCmd::Pty(pty) => cmd_pty(pty, &ctx.http_base()?, &ctx.addr),
        AgentCmd::Sched(sched) => cmd_sched(sched, &ctx, &mut stdin),
        AgentCmd::Install(install) => crate::executors::run(install, &ctx.addr.workspace()?),
        AgentCmd::Cancel(cancel) => cmd_cancel(cancel, &ctx, &mut stdin),
        AgentCmd::Reassign(reassign) => cmd_reassign(reassign, &ctx, &mut stdin),
    }
}

fn cmd_model_program(model_id: &str) -> AgentResult {
    runs::validate_agent_id(model_id)?;
    println!("{}", serde_json::to_string(&runs::model_program(model_id))?);
    Ok(())
}

// ============================================================================
// pty — create the session, then attach this terminal in raw mode
// ============================================================================

fn cmd_pty(args: PtyArgs, base: &str, addr: &NodeAddr) -> AgentResult {
    // read BEFORE the create: the ws surface admits a session's topic against
    // this secret, so a workspace we cannot read is a session we could never
    // attach to — and failing here costs no container.
    let secret = workspace_secret(addr)?;
    // spawning a pty MUTATES this node (a process, a container, a guest VM), so
    // the create and close carry a credential like every other mutation. This
    // CLI acts as the operator of the node it just addressed, and the proof is
    // the same directory read the ws topic already needs.
    let operator = workspace_operator(addr);
    let capability = resolve_harness(base, args.harness, args.cred.as_deref())?;
    let host_hex = match args.host_node.as_deref() {
        Some(hex) => Some(hex_bytes(&host_node_key(hex)?)),
        None => None,
    };

    let created = create_session(
        base,
        operator.as_deref(),
        capability,
        host_hex.as_deref(),
        args.cred.as_deref(),
        args.cpu,
        args.mem,
    )?;
    eprintln!("attached to {} ({})", created.session_id, created.topic);

    // A dedicated single-thread runtime drives the ws pump; the raw-mode guard
    // lives on the pump's own stack so it restores the tty on normal exit AND on
    // a panic unwinding through it.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("attach runtime: {e}"))?;
    let outcome = runtime.block_on(attach(base, &created.session_id, &created.topic, &secret));
    // `shutdown_background`, NOT drop: the attach loop's stdin forwarder reads
    // `tokio::io::stdin()`, which parks a BLOCKING thread on `read(0)`. On a real
    // tty that read never returns, and `abort()` cannot interrupt an OS-level
    // blocking read — so a normal runtime drop WAITS for that thread forever,
    // wedging `agent pty` AFTER the session already ended (the second half of the
    // wedge; the first was the missing end signal). Detach instead: the stuck
    // reader dies with the process.
    runtime.shutdown_background();

    // Best-effort close (idempotent host-side; the 4 h wall-clock + kill-on-drop
    // are the backstops if it never lands).
    let _ = close_session(base, operator.as_deref(), &created.session_id);
    outcome
}

/// the create reply — `{ session_id, topic }`.
struct Created {
    session_id: String,
    topic: String,
}

/// `POST /v1/term/sessions` on the operator's own node. The node routes locally
/// or over the mesh to the host; its refusal strings (`host refused: …`,
/// `a cross-node session requires --cred`, …) come back verbatim.
fn create_session(
    base: &str,
    operator: Option<&str>,
    provider: &str,
    node_hex: Option<&str>,
    cred: Option<&str>,
    cpu: Option<u64>,
    mem_gb: Option<u64>,
) -> Result<Created, Box<dyn std::error::Error>> {
    let mut body = serde_json::json!({ "agent": provider, "mode": "single" });
    if let Some(node) = node_hex {
        body["node"] = serde_json::Value::String(node.to_string());
    }
    if let Some(cred) = cred {
        body["cred"] = serde_json::Value::String(cred.to_string());
    }
    if let Some(cpu) = cpu {
        body["cpu"] = serde_json::Value::Number(cpu.into());
    }
    if let Some(mem_gb) = mem_gb {
        body["mem_gb"] = serde_json::Value::Number(mem_gb.into());
    }

    let resp = with_operator(
        reqwest::blocking::Client::new()
            .post(format!("{base}/v1/term/sessions"))
            .json(&body),
        operator,
    )
    .send()
    .map_err(|e| format!("POST {base}/v1/term/sessions: {e}"))?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        return Err(error_field(&text).into());
    }
    let value: serde_json::Value = serde_json::from_str(&text)?;
    let session_id = value["session_id"]
        .as_str()
        .ok_or_else(|| format!("create reply missing session_id: {text}"))?
        .to_string();
    let topic = value["topic"]
        .as_str()
        .ok_or_else(|| format!("create reply missing topic: {text}"))?
        .to_string();
    Ok(Created { session_id, topic })
}

fn close_session(
    base: &str,
    operator: Option<&str>,
    session_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    with_operator(
        reqwest::blocking::Client::new()
            .post(format!("{base}/v1/term/sessions/{session_id}/close")),
        operator,
    )
    .send()
    .map_err(|e| format!("close session: {e}"))?;
    Ok(())
}

/// attach the node's operator credential when this host could read it.
fn with_operator(
    request: reqwest::blocking::RequestBuilder,
    operator: Option<&str>,
) -> reqwest::blocking::RequestBuilder {
    match operator {
        Some(token) => request.header(noded::admin::ADMIN_TOKEN_HEADER, token),
        None => request,
    }
}

/// Attach this terminal to the session's ws output topic and forward keystrokes
/// and resizes. Raw mode is entered on this stack so its guard restores the tty
/// whichever way this future ends.
async fn attach(base: &str, session_id: &str, topic: &str, secret: &str) -> AgentResult {
    use futures::{SinkExt as _, StreamExt as _};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::signal::unix::{SignalKind, signal};
    use tokio_tungstenite::tungstenite::Message;

    let (socket, _resp) = tokio_tungstenite::connect_async(ws_url(base))
        .await
        .map_err(|e| format!("connect ws /v1/ws: {e}"))?;
    let (mut ws_tx, mut ws_rx) = socket.split();

    // raw mode for the whole attach; the guard restores on drop (normal + panic).
    let _raw = crate::tty::RawGuard::enter();
    let stdin_fd = libc::STDIN_FILENO;

    // one outbound lane: the subscribe carries the node's workspace secret and
    // is what ADMITS this connection to the session, so it must reach the node
    // before any input — every client frame funnels through this ordered mpsc.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(256);
    out_tx.send(subscribe_frame(topic, secret)).await.ok();
    let (cols, rows) = window_size(stdin_fd);
    out_tx.send(resize_frame(session_id, cols, rows)).await.ok();

    let writer = tokio::spawn(async move {
        while let Some(text) = out_rx.recv().await {
            if ws_tx.send(Message::text(text)).await.is_err() {
                break;
            }
        }
        let _ = ws_tx.close().await;
    });

    let input_tx = out_tx.clone();
    let input_session = session_id.to_string();
    let stdin_task = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            let read = match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let data = STANDARD.encode(&buf[..read]);
            if input_tx
                .send(input_frame(&input_session, &data))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let mut winch = signal(SignalKind::window_change()).map_err(|e| format!("SIGWINCH: {e}"))?;
    let mut term = signal(SignalKind::terminate()).map_err(|e| format!("SIGTERM: {e}"))?;
    let mut hup = signal(SignalKind::hangup()).map_err(|e| format!("SIGHUP: {e}"))?;
    let mut stdout = tokio::io::stdout();

    // the loop's outcome, not a steering flag: `Some` means the node refused
    // this attach's topic, which every other exit path is not.
    let mut refused = None;
    loop {
        tokio::select! {
            frame = ws_rx.next() => {
                let Some(Ok(message)) = frame else { break };
                if message.is_close() {
                    break;
                }
                if let Message::Text(text) = message {
                    // the node refused the subscription — this attach can never
                    // receive a byte, and every keystroke it sends is dropped.
                    // Ctrl-C is a keystroke, so without this the terminal is
                    // black and unkillable until SIGTERM: the same wedge class
                    // the `term_ended` signal below closed, through a path the
                    // ws topic gate made reachable for the first time.
                    if let Some(detail) = topic_refusal(&text, topic) {
                        refused = Some(detail);
                        break;
                    }
                    // the session's child exited — the node signals the topic is
                    // over. Stop attaching (the wedge fix): without this the loop
                    // blocks on a dead topic and no keystroke, not even Ctrl-C,
                    // can end it (input is dropped as the session is gone).
                    if is_term_ended(&text) {
                        break;
                    }
                    if let Some(bytes) = decode_term_chunk(&text) {
                        stdout.write_all(&bytes).await.map_err(|e| format!("stdout: {e}"))?;
                        stdout.flush().await.map_err(|e| format!("stdout flush: {e}"))?;
                    }
                }
            }
            _ = winch.recv() => {
                let (cols, rows) = window_size(stdin_fd);
                out_tx.send(resize_frame(session_id, cols, rows)).await.ok();
            }
            _ = term.recv() => break,
            _ = hup.recv() => break,
        }
    }
    stdin_task.abort();
    writer.abort();
    match refused {
        Some(detail) => Err(detail.into()),
        None => Ok(()),
    }
}

/// the tty window size (cols, rows), or an 80x24 fallback when the ioctl fails.
fn window_size(fd: i32) -> (u16, u16) {
    // SAFETY: `ws` is fully written by a successful ioctl; on failure we ignore it.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        let ok = libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col != 0;
        if ok { (ws.ws_col, ws.ws_row) } else { (80, 24) }
    }
}

// ============================================================================
// sched — a node-pinned durable saga trigger
// ============================================================================

fn cmd_sched(args: SchedArgs, ctx: &VerbCtx, stdin: &mut impl BufRead) -> AgentResult {
    let base = &ctx.http_base()?;
    let tag = resolve_harness(base, args.harness, Some(&args.cred))?;

    let target = match args.host_node.as_deref() {
        Some(hex) => host_node_key(hex)?.to_vec(),
        None => own_node_key(base)?,
    };
    preflight_provider(base, &target, tag)?;

    // the USER key signs the trigger: the saga's origin is what the lender
    // resolves to an account (`OfKey`) when the pinned node draws on `--cred`,
    // and a node key is on no account. Unlocked before the id is composed —
    // the id lives under the SIGNER's namespace.
    let user = load_user_signer(&ctx.key_path()?, stdin)?;
    let origin = sdk::Origin::External(user.public_key().as_ref().to_vec());
    let dispatch_id = fresh_dispatch_id();
    // saga's id space is namespaced per trigger origin, so the run's id lives
    // under this user key's own actor namespace and no other member can
    // create or squat it.
    let saga_id = saga::namespaced_id(&origin, &format!("sched\u{1f}{dispatch_id}"));
    let payload =
        compute_service::envelope::compose_headless(&saga_id, &args.prompt, Some(&args.cred))
            .into_bytes();

    let mut demands = BTreeMap::new();
    if let Some(cpu) = args.cpu {
        demands.insert("cores".to_string(), cpu);
    }
    if let Some(mem) = args.mem {
        demands.insert("mem_gb".to_string(), mem);
    }

    let spec = dispatch::WorkSpec {
        kind: dispatch::WORK_SPEC_KIND.to_string(),
        dispatch_id: dispatch_id.clone(),
        capability: tag.to_string(),
        payload,
        demands: demands.clone(),
        admission: dispatch::AdmissionPolicy::Queue,
    };
    let trigger = saga::SagaMsg::Trigger {
        saga_id: saga_id.clone(),
        spec: dispatch::encode_work_spec(&spec),
        reply_to: None,
        reply_payload: Vec::new(),
        deadline: None,
        max_attempts: 3,
        // Provisioning and running a microVM needs the model-work lease.
        lease_views: Some(runs::RUN_LEASE_VIEWS),
        capability: Some(tag.to_string()),
        demands,
        pinned_assignee: Some(target),
    };

    crate::node_http::submit_frame(base, &user_frame(&user, "saga", saga::encode_msg(&trigger)))?;
    println!("{saga_id}");
    Ok(())
}

/// Fail early when the registry KNOWS the target advertises no matching
/// provider. An empty announcement (offline/never-announced node) is NOT a
/// failure — a dark pinned node executes on reconnect; that durability is the
/// contract, so we let the saga carry it.
fn preflight_provider(base: &str, target: &[u8], tag: &str) -> AgentResult {
    let query = capability::CapabilityQuery::Node {
        node: target.to_vec(),
    };
    let value = query_node(base, "capability", serde_json::to_value(&query)?)?;
    let announced = match serde_json::from_value::<capability::CapabilityReply>(value)? {
        capability::CapabilityReply::Node(tags) => tags,
        other => return Err(format!("unexpected capability reply: {other:?}").into()),
    };
    let advertises_something = !announced.is_empty();
    let missing_tag = !announced.iter().any(|t| t == tag);
    if advertises_something && missing_tag {
        return Err(format!(
            "the target node advertises no {tag} provider (announces: {})",
            announced.join(", ")
        )
        .into());
    }
    Ok(())
}

/// A fresh dispatch id: 32 random bytes as 64 hex chars — what
/// `run-output:<id>` keys on.
///
/// The WIDTH is a wire contract, not a taste call. A run's live output reaches
/// the node's ring through the ws `run_output` frame, whose admission gate
/// (`crates/noded/src/stream.rs`) accepts an id of EXACTLY 64 ascii-hex and drops
/// anything else with `reason = "malformed_run_id"`; the agent data plane's
/// `valid_event` enforces the same shape before forwarding a line to a peer.
/// `runs::dispatch_id_for` — the chat-driven lane's id — is a hex sha256 and so
/// satisfies it by construction. This one used to mint 16 bytes, which meant
/// EVERY `ducktape agent sched` run had its live output silently dropped at the
/// node while the committed result landed fine: the ring looked empty for a run
/// that plainly succeeded. Pinned by
/// [`tests::a_fresh_dispatch_id_is_a_wire_admissible_run_id`].
fn fresh_dispatch_id() -> String {
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    hex_bytes(&bytes)
}

// ============================================================================
// cancel / reassign — run control, on whichever module holds the id
// ============================================================================

/// Which module holds the run an operator named. The two id spaces are
/// disjoint, so the ID decides and the operator never has to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlLane {
    /// a `runs` turn claim — the chat- and jobs-driven agent turns.
    Runs,
    /// a saga the SIGNING key triggered: what `agent sched` printed. A saga id
    /// in anyone else's namespace is not this key's to control, so it takes the
    /// `runs` lane and earns that lane's honest `unknown run` refusal rather
    /// than saga's silent foreign-origin no-op.
    Sched,
}

/// The two run-control ops, before the lane names the module that takes them.
#[derive(Debug, Clone, Copy)]
enum ControlVerb {
    Cancel,
    Reassign { attempt: u32 },
}

fn cmd_cancel(args: CancelArgs, ctx: &VerbCtx, stdin: &mut impl BufRead) -> AgentResult {
    println!(
        "{}",
        submit_control(ctx, stdin, ControlVerb::Cancel, &args.run_id)?
    );
    Ok(())
}

fn cmd_reassign(args: ReassignArgs, ctx: &VerbCtx, stdin: &mut impl BufRead) -> AgentResult {
    let verb = ControlVerb::Reassign {
        attempt: args.attempt,
    };
    println!("{}", submit_control(ctx, stdin, verb, &args.run_id)?);
    Ok(())
}

/// Submit one run-control op as a frame the USER key signs, and answer with the
/// sentence the operator reads.
///
/// The user key is the whole authorization: the frame's verified signer becomes
/// the op's `Origin::External`, and the holding module admits only the right
/// origins — runs the run's creator or the agent's owner
/// (`crates/modules/apps/runs/src/admin.rs`, `controlled_dispatch_id`), saga the
/// recorded trigger origin. This CLI deliberately pre-checks NEITHER — a second
/// gate here could only drift from the one that actually decides, and the
/// module's refusal sentence reaches the operator verbatim through the submit
/// lane's error.
fn submit_control(
    ctx: &VerbCtx,
    stdin: &mut impl BufRead,
    verb: ControlVerb,
    run_id: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let base = ctx.http_base()?;
    let user = load_user_signer(&ctx.key_path()?, stdin)?;
    let origin = sdk::Origin::External(user.public_key().as_ref().to_vec());
    let lane = control_lane(&origin, run_id);
    let height = crate::node_http::submit_frame(&base, &control_frame(&user, lane, verb, run_id))?;
    Ok(control_outcome(lane, verb, run_id, height))
}

/// The lane a run id belongs to, asked of the id and the key that signs for it.
fn control_lane(origin: &sdk::Origin, run_id: &str) -> ControlLane {
    let is_this_keys_saga = saga::owns_id(origin, run_id);
    if is_this_keys_saga {
        ControlLane::Sched
    } else {
        ControlLane::Runs
    }
}

/// The frame one control verb submits: the lane names the module, the verb the
/// op, and the USER key signs either way.
fn control_frame(
    user: &commonware_cryptography::ed25519::PrivateKey,
    lane: ControlLane,
    verb: ControlVerb,
    run_id: &str,
) -> Vec<u8> {
    match (lane, verb) {
        (ControlLane::Runs, ControlVerb::Cancel) => user_frame(
            user,
            "runs",
            runs::encode_msg(&runs::RunsMsg::CancelRun {
                run_id: run_id.to_string(),
            }),
        ),
        (ControlLane::Runs, ControlVerb::Reassign { attempt }) => user_frame(
            user,
            "runs",
            runs::encode_msg(&runs::RunsMsg::ReassignRun {
                run_id: run_id.to_string(),
                attempt,
            }),
        ),
        (ControlLane::Sched, ControlVerb::Cancel) => user_frame(
            user,
            "saga",
            saga::encode_msg(&saga::SagaMsg::Cancel {
                saga_id: run_id.to_string(),
            }),
        ),
        (ControlLane::Sched, ControlVerb::Reassign { attempt }) => user_frame(
            user,
            "saga",
            saga::encode_msg(&saga::SagaMsg::Reassign {
                saga_id: run_id.to_string(),
                attempt,
            }),
        ),
    }
}

/// What a committed run-control op prints — the sentence says exactly what the
/// block agreed to and no more, which differs per lane and per verb.
///
/// "accepted", never "cancelled": this block is where the module TOOK the op and
/// told the dispatch plane. The run ends a block later, when the plane's
/// `Err("cancelled")` delivery prunes the entry and posts the agent's ⚠ reply —
/// and an already-delivered run accepts the very same op as a deterministic
/// no-op. Claiming the run is over here would be a sentence the chain has not
/// agreed to yet.
///
/// A `sched` op only ever says "submitted": saga answers an unknown, finished or
/// foreign saga with a SILENT no-op rather than an error, so a committed frame
/// there proves the chain read the op, not that it moved anything. Reassign says
/// which attempt it fenced for the same reason — a stale attempt commits and
/// changes nothing on either lane, and a LIVE `sched` saga never reaches this
/// sentence at all: it is pinned, so saga refuses the reassign outright and the
/// operator reads that refusal instead.
fn control_outcome(lane: ControlLane, verb: ControlVerb, run_id: &str, height: u64) -> String {
    match (lane, verb) {
        (ControlLane::Runs, ControlVerb::Cancel) => format!(
            "cancel accepted for run {run_id} at height {height} \
             (a run whose turn was already taken cancels nothing)"
        ),
        (ControlLane::Runs, ControlVerb::Reassign { attempt }) => format!(
            "reassign accepted for run {run_id} at height {height}, fencing attempt {attempt} \
             (a stale attempt moves nothing)"
        ),
        (ControlLane::Sched, ControlVerb::Cancel) => format!(
            "cancel submitted for sched run {run_id} at height {height} \
             (a finished or unknown run is a silent no-op)"
        ),
        (ControlLane::Sched, ControlVerb::Reassign { attempt }) => format!(
            "reassign submitted for sched run {run_id} at height {height}, fencing attempt \
             {attempt} (a pinned, finished or stale run moves nothing)"
        ),
    }
}

// ============================================================================
// shared resolution
// ============================================================================

/// the service-link secret of the node this CLI is dialling — what its ws
/// surface admits a session's `term:<id>` topic against.
///
/// Reading it is the whole proof: the file is 0600 beside `node.toml`, so a
/// caller that can read it is the operator of that node — the same bar the node
/// key already sets, and the same secret the agent daemon presents to attach.
/// The DIRECTORY comes from the shared addressing ladder
/// ([`NodeAddr::workspace`]), so "which node" is answered once for both the url
/// this CLI dials and the files behind it.
fn workspace_secret(addr: &NodeAddr) -> Result<String, Box<dyn std::error::Error>> {
    let workspace = addr
        .workspace()
        .map_err(|why| format!("attaching a pty needs this node's workspace: {why}"))?;
    noded::services::read_link_token(&workspace).map_err(Into::into)
}

/// this boot's operator credential for the node being addressed — the second
/// thing behind that same 0600 directory, and what a MUTATING `/v1` route wants
/// from a caller acting as the node's operator rather than as an account.
///
/// `None` rather than an error: an unreadable credential surfaces as the node's
/// own 401, which names it precisely, instead of a guess made one layer early.
fn workspace_operator(addr: &NodeAddr) -> Option<String> {
    let workspace = addr.workspace().ok()?;
    noded::admin::read_operator_token(&workspace).ok()
}

/// This node's own 32-byte mesh key, from `/v1/status`'s `public_key`.
fn own_node_key(base: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let resp = reqwest::blocking::Client::new()
        .get(format!("{base}/v1/status"))
        .send()
        .map_err(|e| format!("GET {base}/v1/status: {e}"))?;
    let value: serde_json::Value = resp.json().map_err(|e| format!("status reply: {e}"))?;
    let hex = value["public_key"]
        .as_str()
        .filter(|hex| !hex.is_empty())
        .ok_or("this node has no mesh identity — pass --node to pick a host")?;
    config::unhex(hex).map_err(|e| format!("node key hex: {e}").into())
}

/// Resolve a harness and credential backend to the capability the host offers.
fn resolve_harness(
    base: &str,
    harness: Option<HarnessArg>,
    cred: Option<&str>,
) -> Result<&'static str, Box<dyn std::error::Error>> {
    let Some(name) = cred else {
        return Ok(harness_capability(harness, None)?);
    };
    let record = query_credential(base, name)?
        .ok_or_else(|| format!("unknown credential {name:?} — {}", credential_hint(base)))?;
    harness_capability(harness, Some(record.kind))
        .map_err(|e| format!("credential {name:?}: {e}").into())
}

fn harness_capability(
    harness: Option<HarnessArg>,
    kind: Option<gateway::CredentialKind>,
) -> Result<&'static str, String> {
    use gateway::CredentialKind;
    match (harness, kind) {
        // a signing identity answers no harness: it signs releases, it does
        // not run a model session.
        (
            None | Some(HarnessArg::Pi) | Some(HarnessArg::Claude) | Some(HarnessArg::Codex),
            Some(CredentialKind::AppleCodesign),
        ) => {
            Err("credential kind apple-codesign is a signing identity, not a model provider".into())
        }
        (
            Some(HarnessArg::Pi),
            None | Some(CredentialKind::Claude) | Some(CredentialKind::Codex),
        ) => Ok("pi"),
        (None | Some(HarnessArg::Claude), Some(CredentialKind::Claude))
        | (Some(HarnessArg::Claude), None) => Ok("claude"),
        (None | Some(HarnessArg::Codex), Some(CredentialKind::Codex))
        | (Some(HarnessArg::Codex), None) => Ok("codex"),
        (Some(HarnessArg::Claude), Some(CredentialKind::Codex)) => {
            Err("harness claude contradicts credential kind codex".into())
        }
        (Some(HarnessArg::Codex), Some(CredentialKind::Claude)) => {
            Err("harness codex contradicts credential kind claude".into())
        }
        (None, None) => Err("a harness (claude|codex|pi) is required without --cred".into()),
    }
}

fn query_credential(
    base: &str,
    name: &str,
) -> Result<Option<gateway::CredentialRecord>, Box<dyn std::error::Error>> {
    let query = gateway::GatewayQuery::Credential {
        name: name.to_string(),
    };
    let value = query_node(base, "gateway", serde_json::to_value(&query)?)?;
    match serde_json::from_value::<gateway::GatewayReply>(value)? {
        gateway::GatewayReply::Credential(record) => Ok(record),
        other => Err(format!("unexpected gateway reply: {other:?}").into()),
    }
}

/// What to say after "unknown credential": the names that DO exist, or the
/// command that makes the first one.
///
/// Best-effort by construction — this only ever runs on a path that is already
/// failing, so a second query that also fails must not replace the real error
/// with its own. It then says the one thing that is true regardless.
fn credential_hint(base: &str) -> String {
    const REGISTER_ONE: &str = "register one with: ducktape user cred add claude";
    let Ok(records) = crate::cred_cli::list_credential_names(base) else {
        return REGISTER_ONE.into();
    };
    match records.as_slice() {
        [] => format!("no credentials are registered on this node — {REGISTER_ONE}"),
        names => format!("registered here: {}", names.join(", ")),
    }
}

/// The `--host-node` target as a 32-byte node key. Hex only: no node is bound
/// to an account, so a name cannot resolve to one — `ducktape node peers`
/// lists the keys.
fn host_node_key(text: &str) -> Result<[u8; 32], Box<dyn std::error::Error>> {
    decode_node_key(text)
        .ok_or_else(|| format!("--host-node must be a 64-hex node key, not {text:?}").into())
}

/// Decode a 64-hex string to 32 bytes, or `None` when it is not one.
fn decode_node_key(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let bytes = config::unhex(text).ok()?;
    <[u8; 32]>::try_from(bytes).ok()
}

/// `http(s)://host:port` → `ws(s)://host:port/v1/ws`, the node's own ws surface.
pub(crate) fn ws_url(base: &str) -> String {
    let ws_base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    format!("{}/v1/ws", ws_base.trim_end_matches('/'))
}

/// Pull the `"error"` field out of a node error body, or fall back to the raw
/// text — so the node's verbatim refusal strings reach the operator.
fn error_field(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_string))
        .unwrap_or_else(|| body.to_string())
}

/// the subscribe that ADMITS this connection to a session's output topic. The
/// `token` is the node's own 0600 service-link secret; without it the node
/// refuses the topic and this connection has nothing to send keystrokes on.
fn subscribe_frame(topic: &str, token: &str) -> String {
    serde_json::json!({ "op": "subscribe", "topics": [topic], "token": token }).to_string()
}

fn input_frame(session: &str, data_b64: &str) -> String {
    serde_json::json!({ "op": "term_input", "session": session, "data": data_b64 }).to_string()
}

fn resize_frame(session: &str, cols: u16, rows: u16) -> String {
    serde_json::json!({ "op": "term_resize", "session": session, "cols": cols, "rows": rows })
        .to_string()
}

/// Decode one server ws text frame to the raw pty bytes it carries, or `None`
/// for any non-output frame (subscribed, heartbeat, module event, error).
fn decode_term_chunk(text: &str) -> Option<Vec<u8>> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if value["type"].as_str() != Some("event") {
        return None;
    }
    let item = value["item"].as_str()?;
    STANDARD.decode(item).ok()
}

/// The node's refusal of THIS attach's topic, if that is what this frame is.
///
/// `ServerFrame::Error` is `{type:"error", topic, code, detail}`; the `detail`
/// is the node's own sentence and reaches the operator verbatim, like every
/// other refusal string this CLI surfaces.
///
/// Matched on the topic, not just the type: an error about some other topic on
/// a shared connection is not this attach's business. `agent pty` holds one
/// topic, but keying on it is what keeps that true.
fn topic_refusal(text: &str, topic: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let is_our_refusal =
        value["type"].as_str() == Some("error") && value["topic"].as_str() == Some(topic);
    if !is_our_refusal {
        return None;
    }
    let detail = value["detail"]
        .as_str()
        .unwrap_or("the node refused this session's topic");
    Some(detail.to_string())
}

/// the node's terminal frame for this topic: the session's child exited and the
/// `term:<id>` topic is complete (`{"type":"term_ended",...}`). The signal the
/// attach loop ends on — see [`stream::ServerFrame::TermEnded`].
fn is_term_ended(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .as_ref()
        .and_then(|v| v["type"].as_str())
        == Some("term_ended")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sched run's id must be admissible on the wire that carries its LIVE
    /// output, or the ring stays empty for a run that succeeded. The node's ws
    /// `run_output` gate takes exactly 64 ascii-hex; so does the agent data
    /// plane's peer forwarder. See [`fresh_dispatch_id`].
    #[test]
    fn a_fresh_dispatch_id_is_a_wire_admissible_run_id() {
        let id = fresh_dispatch_id();
        assert_eq!(id.len(), 64, "the ws run_output gate drops any other width");
        assert!(
            id.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "the gate also requires ascii-hex: {id}"
        );
        assert_ne!(id, fresh_dispatch_id(), "a fresh id is fresh");
    }

    /// `sched` composes the run's saga id under the SIGNING user key's own
    /// actor namespace, because the frame's verified signer is the trigger's
    /// origin and saga refuses a trigger for anybody else's namespace.
    /// Composing a bare `sched\x1f<id>` here would make every scheduled run
    /// reject; composing it under the NODE key (what this used to do) would
    /// too, now that the node no longer re-signs the submit.
    #[test]
    fn a_sched_saga_id_is_owned_by_the_signing_user_key() {
        let user = sdk::Origin::External(vec![0xAB; 32]);
        let id = saga::namespaced_id(&user, &format!("sched\u{1f}{}", fresh_dispatch_id()));
        assert!(saga::owns_id(&user, &id), "saga would refuse {id:?}");
        assert!(
            !saga::owns_id(&sdk::Origin::Module("dispatch".into()), &id),
            "and it belongs to nobody else"
        );
    }

    #[test]
    fn host_node_is_hex_only() {
        let hex = "ab".repeat(32);
        assert_eq!(host_node_key(&hex).unwrap(), [0xab; 32]);
        let err = host_node_key("alice").unwrap_err().to_string();
        assert!(err.contains("64-hex node key"), "{err}");
    }

    #[test]
    fn harness_and_credential_select_the_capability() {
        use gateway::CredentialKind::{Claude, Codex};
        for (harness, kind, expected) in [
            (None, Claude, "claude"),
            (None, Codex, "codex"),
            (Some(HarnessArg::Claude), Claude, "claude"),
            (Some(HarnessArg::Codex), Codex, "codex"),
            (Some(HarnessArg::Pi), Claude, "pi"),
            (Some(HarnessArg::Pi), Codex, "pi"),
        ] {
            assert_eq!(harness_capability(harness, Some(kind)).unwrap(), expected);
        }
        for (harness, kind) in [(HarnessArg::Claude, Codex), (HarnessArg::Codex, Claude)] {
            assert!(
                harness_capability(Some(harness), Some(kind))
                    .unwrap_err()
                    .contains("contradicts")
            );
        }
    }

    #[test]
    fn explicit_harnesses_work_without_a_credential() {
        for harness in [HarnessArg::Claude, HarnessArg::Codex, HarnessArg::Pi] {
            assert_eq!(
                harness_capability(Some(harness), None).unwrap(),
                harness.token()
            );
        }
        assert!(harness_capability(None, None).is_err());
    }

    #[test]
    fn pty_and_sched_accept_pi_positionally() {
        use clap::{Args as _, FromArgMatches as _};
        let pty = PtyArgs::augment_args(clap::Command::new("pty"))
            .try_get_matches_from(["pty", "pi", "--cred", "work"])
            .unwrap();
        assert_eq!(
            PtyArgs::from_arg_matches(&pty).unwrap().harness,
            Some(HarnessArg::Pi)
        );
        let sched = SchedArgs::augment_args(clap::Command::new("sched"))
            .try_get_matches_from(["sched", "pi", "--cred", "work", "--", "hello"])
            .unwrap();
        assert_eq!(
            SchedArgs::from_arg_matches(&sched).unwrap().harness,
            Some(HarnessArg::Pi)
        );
    }

    #[test]
    fn pi_is_a_harness_not_a_credential_kind() {
        use clap::ValueEnum as _;
        assert_eq!(HarnessArg::from_str("pi", false).unwrap(), HarnessArg::Pi);
        assert!(crate::cred_cli::ProviderArg::from_str("pi", false).is_err());
    }

    /// An `apple-codesign` credential runs no harness, whichever one is
    /// named — including pi, which otherwise takes any model credential.
    #[test]
    fn a_signing_credential_answers_no_harness() {
        for harness in [
            None,
            Some(HarnessArg::Pi),
            Some(HarnessArg::Claude),
            Some(HarnessArg::Codex),
        ] {
            let err = harness_capability(harness, Some(gateway::CredentialKind::AppleCodesign))
                .unwrap_err();
            assert!(err.contains("signing identity"), "{harness:?}: {err}");
        }
        assert_eq!(
            harness_capability(Some(HarnessArg::Pi), Some(gateway::CredentialKind::Claude)),
            Ok("pi")
        );
    }

    #[test]
    fn node_key_hex_round_trips_and_rejects_bad_len() {
        let hex = "ab".repeat(32); // 64 hex chars = 32 bytes
        assert_eq!(decode_node_key(&hex), Some([0xab; 32]));
        assert_eq!(decode_node_key("abcd"), None); // too short
        assert_eq!(decode_node_key(&"zz".repeat(32)), None); // not hex
    }

    #[test]
    fn ws_url_maps_scheme_and_appends_path() {
        assert_eq!(ws_url("http://127.0.0.1:8080"), "ws://127.0.0.1:8080/v1/ws");
        assert_eq!(ws_url("https://host:9/"), "wss://host:9/v1/ws");
    }

    #[test]
    fn term_chunk_decodes_only_output_frames() {
        let chunk = serde_json::json!({
            "type": "event", "topic": "term:abc", "cursor": "3", "item": STANDARD.encode(b"hi")
        })
        .to_string();
        assert_eq!(decode_term_chunk(&chunk), Some(b"hi".to_vec()));

        let module_event = serde_json::json!({
            "type": "event", "topic": "chat", "cursor": "1", "op": { "height": 1 }
        })
        .to_string();
        assert_eq!(decode_term_chunk(&module_event), None);

        let heartbeat = serde_json::json!({ "type": "heartbeat", "height": 1 }).to_string();
        assert_eq!(decode_term_chunk(&heartbeat), None);
    }

    #[test]
    fn term_ended_frame_ends_the_attach_but_output_chunks_do_not() {
        // the node's terminal signal (ServerFrame::TermEnded) — the attach loop
        // breaks on it; an output chunk or any other frame keeps it running.
        let ended = serde_json::json!({ "type": "term_ended", "topic": "term:abc" }).to_string();
        assert!(is_term_ended(&ended));

        let chunk = serde_json::json!({
            "type": "event", "topic": "term:abc", "cursor": "3", "item": STANDARD.encode(b"hi")
        })
        .to_string();
        assert!(!is_term_ended(&chunk));
        assert!(!is_term_ended(
            &serde_json::json!({ "type": "heartbeat" }).to_string()
        ));
    }

    /// The node's refusal must END the attach, not be swallowed.
    ///
    /// By the time a topic refusal can arrive, `cmd_pty` has already created a
    /// real pty holding a lent credential, entered raw mode and printed
    /// "attached". Every keystroke after that is dropped by the node — Ctrl-C
    /// included, since it is just a keystroke — so ignoring this frame is a
    /// black, unkillable terminal, the same wedge class the `term_ended` signal
    /// closed. The ws topic gate is what made this frame reachable for `term:`
    /// at all, so the handler ships with it.
    #[test]
    fn a_topic_refusal_ends_the_attach_and_carries_the_nodes_own_sentence() {
        let refusal = serde_json::json!({
            "type": "error",
            "topic": "term:abc",
            "code": "forbidden",
            "detail": "this topic requires the node's service-link token",
        })
        .to_string();
        assert_eq!(
            topic_refusal(&refusal, "term:abc").as_deref(),
            Some("this topic requires the node's service-link token"),
            "the node's sentence must reach the operator verbatim"
        );

        // an error about ANOTHER topic is not this attach's business ...
        assert_eq!(topic_refusal(&refusal, "term:other"), None);
        // ... and no ordinary frame is mistaken for one. A term chunk in
        // particular rides `type:"event"` on the very same topic.
        for benign in [
            serde_json::json!({ "type": "event", "topic": "term:abc", "item": "aGk=" }),
            serde_json::json!({ "type": "term_ended", "topic": "term:abc" }),
            serde_json::json!({ "type": "subscribed", "topics": { "term:abc": "0" } }),
            serde_json::json!({ "type": "heartbeat", "height": 1 }),
        ] {
            assert_eq!(
                topic_refusal(&benign.to_string(), "term:abc"),
                None,
                "{benign}"
            );
        }
        assert_eq!(topic_refusal("not json", "term:abc"), None);

        // a refusal with no detail still ends the attach rather than wedging it.
        let bare = serde_json::json!({ "type": "error", "topic": "term:abc" }).to_string();
        assert!(topic_refusal(&bare, "term:abc").is_some());
    }

    #[test]
    fn client_frames_carry_the_snake_case_op_tags() {
        let sub: serde_json::Value =
            serde_json::from_str(&subscribe_frame("term:x", "s3cr3t")).unwrap();
        assert_eq!(sub["op"], "subscribe");
        assert_eq!(sub["topics"][0], "term:x");
        // the field the node's topic gate reads. Without it the node refuses
        // `term:` and this client has nothing to send keystrokes on, so its
        // absence is a broken pty, not a cosmetic omission.
        assert_eq!(sub["token"], "s3cr3t");

        let input: serde_json::Value =
            serde_json::from_str(&input_frame("sid", "ZGF0YQ==")).unwrap();
        assert_eq!(input["op"], "term_input");
        assert_eq!(input["session"], "sid");
        assert_eq!(input["data"], "ZGF0YQ==");

        let resize: serde_json::Value =
            serde_json::from_str(&resize_frame("sid", 120, 40)).unwrap();
        assert_eq!(resize["op"], "term_resize");
        assert_eq!(resize["cols"], 120);
        assert_eq!(resize["rows"], 40);
    }

    /// The RUN ID picks the module, and the USER key signs either way — the
    /// authority check on both lanes IS the frame's verified origin. Getting the
    /// lane wrong is not a cosmetic slip: `CancelRun` on a `sched` id walks
    /// `controlled_dispatch_id` to an entry that was never minted and answers
    /// "unknown run", which is exactly the hole this verb exists to close.
    #[test]
    fn the_run_id_picks_the_module_and_the_user_key_signs_the_op() {
        use commonware_codec::DecodeExt as _;
        let user = commonware_cryptography::ed25519::PrivateKey::decode([3u8; 32].as_slice())
            .expect("a 32-byte seed");
        let signer = sdk::Origin::External(user.public_key().as_ref().to_vec());
        let turn_claim = "chat\u{1f}general\u{1f}3\u{1f}bot";
        // exactly what `agent sched` printed: saga's namespaced id under the
        // signing key's own actor string.
        let sched_run = saga::namespaced_id(&signer, "sched\u{1f}deadbeef");

        assert_eq!(control_lane(&signer, turn_claim), ControlLane::Runs);
        assert_eq!(control_lane(&signer, &sched_run), ControlLane::Sched);
        // another key's saga is not this key's to control: it takes the runs
        // lane, where an id nobody holds is REFUSED rather than silently
        // swallowed by saga's foreign-origin no-op.
        let stranger = sdk::Origin::External(vec![9u8; 32]);
        assert_eq!(
            control_lane(&signer, &saga::namespaced_id(&stranger, "sched\u{1f}x")),
            ControlLane::Runs
        );

        let submitted = |verb, run_id: &str| {
            let lane = control_lane(&signer, run_id);
            let frame = control_frame(&user, lane, verb, run_id);
            let (origin, msg) = node::decode_frame(&frame).expect("the frame verifies");
            assert_eq!(origin, signer, "the module admits by ORIGIN");
            (msg.target, msg.payload)
        };

        let (target, payload) = submitted(ControlVerb::Cancel, turn_claim);
        assert_eq!(target, "runs");
        assert_eq!(
            runs::decode_msg(&payload),
            Ok(runs::RunsMsg::CancelRun {
                run_id: turn_claim.to_string()
            })
        );
        let (target, payload) = submitted(ControlVerb::Reassign { attempt: 2 }, turn_claim);
        assert_eq!(target, "runs");
        assert_eq!(
            runs::decode_msg(&payload),
            Ok(runs::RunsMsg::ReassignRun {
                run_id: turn_claim.to_string(),
                attempt: 2
            })
        );

        let (target, payload) = submitted(ControlVerb::Cancel, &sched_run);
        assert_eq!(target, "saga", "a sched run lives in saga, not runs");
        assert_eq!(
            saga::decode_msg(&payload),
            Ok(saga::SagaMsg::Cancel {
                saga_id: sched_run.clone()
            })
        );
        let (target, payload) = submitted(ControlVerb::Reassign { attempt: 1 }, &sched_run);
        assert_eq!(target, "saga");
        assert_eq!(
            saga::decode_msg(&payload),
            Ok(saga::SagaMsg::Reassign {
                saga_id: sched_run,
                attempt: 1
            })
        );
    }

    /// What a COMMITTED control op is allowed to claim. Every sentence here
    /// prints at a height the chain agreed to, and the failure mode is claiming
    /// more than that height proves.
    ///
    /// "accepted", never "cancelled" — the run ends a block later, on the
    /// dispatch plane's delivery. And every lane that can commit a no-op says
    /// so: runs cancels nothing when the turn was already taken, saga is
    /// deliberately silent for a finished, unknown or foreign saga, and a
    /// pinned `sched` saga cannot be reassigned at all. The refusal path needs
    /// no case here — the module's own sentence rides the submit lane's error
    /// verbatim, which is `node_http::submit_frame`'s contract and its tests.
    #[test]
    fn a_committed_control_op_claims_only_what_its_height_proves() {
        let run_id = "chat\u{1f}general\u{1f}3\u{1f}bot";

        let cancel = control_outcome(ControlLane::Runs, ControlVerb::Cancel, run_id, 42);
        assert!(
            cancel.starts_with(&format!("cancel accepted for run {run_id} at height 42")),
            "{cancel}"
        );
        assert!(cancel.contains("cancels nothing"), "{cancel}");
        // saga swallows an unknown, finished or foreign cancel WITHOUT an
        // error, so a committed sched frame proves the chain read the op and
        // nothing more. Saying "accepted" there would be the CLI inventing a
        // verdict the block never reached.
        let sched = control_outcome(ControlLane::Sched, ControlVerb::Cancel, run_id, 42);
        assert!(sched.contains("submitted"), "{sched}");
        assert!(!sched.contains("accepted"), "{sched}");
        // reassign names the attempt it fenced on either lane: a stale one
        // commits and moves nothing, and the operator cannot tell from a bare
        // height.
        for lane in [ControlLane::Runs, ControlLane::Sched] {
            let line = control_outcome(lane, ControlVerb::Reassign { attempt: 3 }, run_id, 42);
            assert!(line.contains("attempt 3"), "{line}");
        }
        // a `sched` reassign must never promise a move: every `agent sched`
        // saga is pinned, and saga refuses a pinned reassign outright.
        let pinned = control_outcome(
            ControlLane::Sched,
            ControlVerb::Reassign { attempt: 0 },
            run_id,
            42,
        );
        assert!(pinned.contains("pinned"), "{pinned}");
    }

    /// a Parser wrapper so the tests exercise the derived verb SHAPE the same
    /// way `main.rs`'s integrator will.
    #[derive(clap::Parser)]
    struct TestAgentCli {
        #[command(flatten)]
        args: AgentArgs,
    }

    /// `--attempt` defaults to 0 — the run's FIRST attempt, and the only one a
    /// reassignment can move (`RUN_MAX_ATTEMPTS = 2`, so attempt 1 answers
    /// "reassignment attempts exhausted"). A wrong default is the worst
    /// failure this verb has: saga treats a stale attempt as a deterministic
    /// no-op, so the operator would read "accepted" and watch the run carry on.
    #[test]
    fn reassign_fences_the_first_attempt_unless_told_otherwise() {
        use clap::Parser as _;
        let reassign = |argv: &[&str]| {
            let cli = TestAgentCli::try_parse_from(argv).expect("the verb parses");
            match cli.args.cmd {
                AgentCmd::Reassign(args) => args,
                other => panic!("expected reassign, got {other:?}"),
            }
        };
        let default = reassign(&["agent", "reassign", "run-1"]);
        assert_eq!(default.run_id, "run-1");
        assert_eq!(default.attempt, 0);
        assert_eq!(
            reassign(&["agent", "reassign", "run-1", "--attempt", "1"]).attempt,
            1
        );

        let cancel =
            TestAgentCli::try_parse_from(["agent", "cancel", "run-1"]).expect("the verb parses");
        assert!(matches!(cancel.args.cmd, AgentCmd::Cancel(args) if args.run_id == "run-1"));
    }

    #[test]
    fn error_field_prefers_the_error_key() {
        assert_eq!(
            error_field(r#"{"error":"host refused: no_sandbox"}"#),
            "host refused: no_sandbox"
        );
        assert_eq!(error_field("raw text"), "raw text");
    }
}
