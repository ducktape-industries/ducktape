//! `ducktape agent` — remote/interactive sandboxed provider sessions, plus the
//! two run-control verbs.
//!
//! Two session verbs, one credential+targeting story:
//!
//! - `agent pty [<harness>] --account <n> --route <label> [--cred <name>]
//!   [--cpu <n>] [--mem <gb>]` attaches THIS terminal to a provider running in a
//!   microVM, inside the independently installed `ducktape-terminal` process the
//!   named `(account, route)` publishes. The CLI talks ONLY to its own node's
//!   operator door (`/v1/gateway/operator`); the gateway reaches the publisher.
//!   Raw terminal mode + resize forwarding make it feel like ssh.
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
    /// path to the user key file signing Chief, `sched`, `cancel` and
    /// `reassign` submits (defaults to the keystore's active wallet)
    #[arg(long, value_name = "PATH", global = true)]
    key: Option<std::path::PathBuf>,
}

#[derive(Debug, clap::Subcommand)]
pub(crate) enum AgentCmd {
    /// explicitly install and control a network-resident Chief
    Chief(crate::chief_cli::ChiefArgs),
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
    /// the account whose signed gateway route publishes the terminal service.
    /// Required: nothing here picks a default service.
    #[arg(long, value_name = "ACCOUNT")]
    account: u64,
    /// that account's route label for the installed `ducktape-terminal`.
    /// Required: there is no default terminal route.
    #[arg(long, value_name = "LABEL")]
    route: String,
    /// credential name to serve the session (required when the route's
    /// publisher is not the node this CLI dials — the service decides)
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
        // pty takes the whole group, not just the resolved base: the operator
        // door takes this node's own operator credential, which lives in the
        // WORKSPACE, and only the ladder knows which workspace the address it
        // just resolved belongs to.
        AgentCmd::Chief(chief) => crate::chief_cli::run(chief, &ctx, &mut stdin),
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
// pty — create the session on the installed terminal service, then attach this
// terminal in raw mode
// ============================================================================
//
// There is no node-side terminal plane. `agent pty` names an installed
// application by its signed gateway route — `--account` + `--route`, both
// required, because a hidden default would silently pick whose machine runs the
// session — and every byte crosses the common operator door
// (`/v1/gateway/operator`): a POST for the create, a WebSocket upgrade for the
// attachment. The node forwards this CLI's EXISTING operator authentication as
// an attestation and never the credential itself; the `ducktape-terminal`
// process on the other side decides admission from its own work policy.

/// The installed application this session runs on: the `(account, route)` the
/// operator named, resolved to the revision of the route record the gateway
/// will match the request against.
///
/// The revision is read ONCE, before the create: it is what binds this whole
/// session to the policy the operator saw, and a route republished mid-session
/// answers the next request with the gateway's own `409` rather than silently
/// moving the session to a new policy.
struct Destination {
    account: u64,
    name: gateway::RouteName,
    revision: u64,
}

fn cmd_pty(args: PtyArgs, base: &str, addr: &NodeAddr) -> AgentResult {
    // the node's own operator credential, exactly as before: creating and
    // driving a session MUTATES a host, so the operator door takes the same
    // proof every other mutating `/v1` route does. Nothing about it reaches the
    // service — the node attests the caller instead.
    let operator = workspace_operator(addr);
    let capability = resolve_harness(base, args.harness, args.cred.as_deref())?;
    let destination = resolve_destination(base, args.account, &args.route)?;

    let session = create_session(
        base,
        operator.as_deref(),
        &destination,
        capability,
        args.cred.as_deref(),
        args.cpu,
        args.mem,
    )?;
    eprintln!("attached to {session}");

    // A dedicated single-thread runtime drives the ws pump; the raw-mode guard
    // lives on the pump's own stack so it restores the tty on normal exit AND on
    // a panic unwinding through it.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("attach runtime: {e}"))?;
    let outcome = runtime.block_on(attach(base, operator.as_deref(), &destination, &session));
    // `shutdown_background`, NOT drop: the attach loop's stdin forwarder reads
    // `tokio::io::stdin()`, which parks a BLOCKING thread on `read(0)`. On a real
    // tty that read never returns, and `abort()` cannot interrupt an OS-level
    // blocking read — so a normal runtime drop WAITS for that thread forever,
    // wedging `agent pty` AFTER the session already ended. Detach instead: the
    // stuck reader dies with the process.
    runtime.shutdown_background();
    outcome
}

/// Resolve the operator's `(account, route)` to the published route record.
///
/// A label that names no live route is refused HERE, before a container exists,
/// with the sentence that says which half is wrong.
fn resolve_destination(
    base: &str,
    account: u64,
    label: &str,
) -> Result<Destination, Box<dyn std::error::Error>> {
    let name = gateway::RouteName::named(label.to_string());
    name.validate().map_err(|why| format!("--route: {why}"))?;
    let query = gateway::GatewayQuery::Get {
        account_id: account,
        name: name.clone(),
    };
    let value = query_node(base, "gateway", serde_json::to_value(&query)?)?;
    let gateway::GatewayReply::Route(record) = serde_json::from_value(value)? else {
        return Err("unexpected gateway reply to a route query".into());
    };
    let published = record
        .filter(|record| record.statement.route.is_some())
        .ok_or_else(|| format!("account {account} publishes no live gateway route {label:?}"))?;
    Ok(Destination {
        account,
        name,
        revision: published.statement.revision,
    })
}

/// One request head for the operator door. `operator` stays FALSE here: the
/// assertion is the node's to make after it has checked this caller's operator
/// credential, and a client-set flag would be a claim nothing verified.
fn operator_head(
    destination: &Destination,
    method: gateway::RouteMethod,
    path_and_query: String,
    headers: Vec<gateway::ProxyHeader>,
    body_len: u64,
    upgrade: bool,
) -> gateway::ProxyRequestHead {
    gateway::ProxyRequestHead {
        operator: false,
        account_id: destination.account,
        name: destination.name.clone(),
        revision: destination.revision,
        method,
        path_and_query,
        headers,
        body_len,
        upgrade,
        user_pop: None,
    }
}

/// `POST /sessions` on the installed terminal service, through the operator
/// door. The reply carries `session_id` and NOTHING else — output rides the
/// attachment's own WebSocket, so there is no topic to hand back.
fn create_session(
    base: &str,
    operator: Option<&str>,
    destination: &Destination,
    provider: &str,
    cred: Option<&str>,
    cpu: Option<u64>,
    mem_gb: Option<u64>,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut request = serde_json::json!({ "agent": provider });
    if let Some(cred) = cred {
        request["cred"] = serde_json::Value::String(cred.to_string());
    }
    if let Some(cpu) = cpu {
        request["cpu"] = serde_json::Value::Number(cpu.into());
    }
    if let Some(mem_gb) = mem_gb {
        request["mem_gb"] = serde_json::Value::Number(mem_gb.into());
    }
    let payload = serde_json::to_vec(&request)?;
    let head = operator_head(
        destination,
        gateway::RouteMethod::Post,
        "/sessions".into(),
        vec![gateway::ProxyHeader {
            name: "content-type".into(),
            value: "application/json".into(),
        }],
        payload.len() as u64,
        false,
    );
    let text = operator_proxy(base, operator, &head, &payload)?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|_| format!("create reply is not JSON: {text}"))?;
    Ok(value["session_id"]
        .as_str()
        .ok_or_else(|| format!("create reply missing session_id: {text}"))?
        .to_string())
}

/// One buffered exchange through `/v1/gateway/operator`, answered with the
/// service's own response body.
///
/// Two refusals can reach an operator here and they mean different things, so
/// neither is flattened into the other: the node's (a route that moved, a
/// missing overlay, a credential this CLI could not present) comes back as the
/// outer status, and the service's (`not_operator`, `work_not_admitted`,
/// `unknown_provider`, …) as the envelope's upstream status.
fn operator_proxy(
    base: &str,
    operator: Option<&str>,
    head: &gateway::ProxyRequestHead,
    body: &[u8],
) -> Result<String, Box<dyn std::error::Error>> {
    let envelope = serde_json::json!({
        "head": head,
        "body_b64": STANDARD.encode(body),
    });
    let response = with_operator(
        reqwest::blocking::Client::new()
            .post(format!("{base}/v1/gateway/operator"))
            .json(&envelope),
        operator,
    )
    .send()
    .map_err(|e| format!("POST {base}/v1/gateway/operator: {e}"))?;
    let status = response.status();
    let text = response.text().unwrap_or_default();
    if !status.is_success() {
        return Err(error_field(&text).into());
    }
    let reply: serde_json::Value = serde_json::from_str(&text)?;
    let upstream = reply["head"]["status"]
        .as_u64()
        .ok_or_else(|| format!("gateway reply missing an upstream status: {text}"))?;
    let served =
        String::from_utf8(STANDARD.decode(reply["body_b64"].as_str().unwrap_or_default())?)?;
    let refused = !(200..300).contains(&upstream);
    if refused {
        return Err(format!(
            "the terminal service refused ({upstream}): {}",
            error_field(&served)
        )
        .into());
    }
    Ok(served)
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

/// How one attachment ended — the redial loop's one discriminant.
#[derive(Debug, PartialEq, Eq)]
enum Detached {
    /// The session itself is over: the service reported `ended` and this
    /// attachment drained it, or the operator's signal closed it.
    Ended,
    /// The socket dropped with the session still live. `served` is how many
    /// frames this attachment consumed, which is what makes a redial PROGRESS
    /// rather than spin against a service that upgrades and hangs up.
    Dropped { served: u64 },
}

/// Attach this terminal to the session on the installed service and forward
/// keystrokes and resizes. Raw mode is entered on this stack so its guard
/// restores the tty whichever way this future ends — including a refused
/// upgrade, which returns before any of it.
async fn attach(
    base: &str,
    operator: Option<&str>,
    destination: &Destination,
    session: &str,
) -> AgentResult {
    use tokio::io::AsyncReadExt as _;
    use tokio::signal::unix::{SignalKind, signal};

    // ONE stdin reader for the whole attach, across every redial: it parks a
    // blocking thread on `read(0)`, so a reader per connection would leak one
    // per drop. Keystrokes typed while disconnected queue here and are sent
    // when the socket returns — they are the operator's input, never a replay.
    let (keys, typed) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    let mut typed = Some(typed);
    let reader = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            let read = match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if keys.send(buf[..read].to_vec()).await.is_err() {
                break;
            }
        }
    });
    let mut winch = signal(SignalKind::window_change()).map_err(|e| format!("SIGWINCH: {e}"))?;
    let mut term = signal(SignalKind::terminate()).map_err(|e| format!("SIGTERM: {e}"))?;
    let mut hup = signal(SignalKind::hangup()).map_err(|e| format!("SIGHUP: {e}"))?;

    let _raw = crate::tty::RawGuard::enter();
    // What this attachment has actually CONSUMED — the resume position, and the
    // only thing a reconnect may present. A snapshot's `head` is a BOUND it
    // announces, never a receipt: this advances on the frame that was written
    // to the terminal, so a socket that dies between the bound and its frames
    // resumes at the byte the operator last saw.
    let mut cursor = 0u64;
    let outcome = loop {
        let detached = attached(
            base,
            operator,
            destination,
            session,
            &mut cursor,
            &mut typed,
            &mut winch,
            &mut term,
            &mut hup,
        )
        .await?;
        match detached {
            Detached::Ended => break Ok(()),
            // a redial that consumed nothing consumed nothing the next one
            // would either: report the loss instead of looping on it.
            Detached::Dropped { served: 0 } => {
                break Err(
                    "the terminal attachment dropped before the service served a frame".into(),
                );
            }
            Detached::Dropped { .. } => continue,
        }
    };
    reader.abort();
    outcome
}

/// The next keystrokes this terminal typed, or a future that never completes
/// once stdin has ended.
///
/// A closed channel is READY forever, so an attachment that kept selecting on
/// one would spin at 100% CPU the moment stdin hit EOF — which is every
/// `agent pty < script` and every closed tty. Ending the attachment there would
/// be wrong the other way: the operator's input is over, the session is not, so
/// the receiver is dropped and this arm parks.
async fn typed_keys(typed: &mut Option<tokio::sync::mpsc::Receiver<Vec<u8>>>) -> Vec<u8> {
    if let Some(keys) = typed.as_mut() {
        if let Some(bytes) = keys.recv().await {
            return bytes;
        }
        *typed = None;
    }
    std::future::pending().await
}

/// One attachment: open the session's WebSocket at the cursor this terminal has
/// consumed, then pump until it ends or drops.
#[allow(clippy::too_many_arguments)]
async fn attached(
    base: &str,
    operator: Option<&str>,
    destination: &Destination,
    session: &str,
    cursor: &mut u64,
    typed: &mut Option<tokio::sync::mpsc::Receiver<Vec<u8>>>,
    winch: &mut tokio::signal::unix::Signal,
    term: &mut tokio::signal::unix::Signal,
    hup: &mut tokio::signal::unix::Signal,
) -> Result<Detached, Box<dyn std::error::Error>> {
    use futures::{SinkExt as _, StreamExt as _};
    use tokio::io::AsyncWriteExt as _;
    use tokio_tungstenite::tungstenite::Message;

    let head = operator_head(
        destination,
        gateway::RouteMethod::Get,
        format!("/sessions/{session}?after={cursor}"),
        Vec::new(),
        0,
        true,
    );
    let mut socket = open_attachment(base, operator, &head).await?.split();
    let stdin_fd = libc::STDIN_FILENO;
    let (cols, rows) = window_size(stdin_fd);
    socket
        .0
        .send(Message::text(resize_command(cols, rows)))
        .await
        .map_err(|e| format!("resize: {e}"))?;

    let mut stdout = tokio::io::stdout();
    let mut served = 0u64;
    // `Some` once a snapshot reported the session over; the loop still drains
    // that snapshot's frames before it agrees.
    let mut complete: Option<u64> = None;
    loop {
        tokio::select! {
            frame = socket.1.next() => {
                let Some(Ok(message)) = frame else { return Ok(Detached::Dropped { served }) };
                let Message::Text(text) = message else {
                    let closed = message.is_close();
                    if closed { return Ok(Detached::Dropped { served }); }
                    continue;
                };
                served += 1;
                match serve_frame(&text, cursor)? {
                    Served::Output(bytes) => {
                        stdout.write_all(&bytes).await.map_err(|e| format!("stdout: {e}"))?;
                        stdout.flush().await.map_err(|e| format!("stdout flush: {e}"))?;
                    }
                    Served::Snapshot(bound) => complete = bound,
                    Served::Nothing => {}
                }
                if complete.is_some_and(|bound| *cursor >= bound) {
                    return Ok(Detached::Ended);
                }
            }
            keys = typed_keys(typed) => {
                socket.0.send(Message::text(input_command(&STANDARD.encode(keys)))).await
                    .map_err(|e| format!("input: {e}"))?;
            }
            _ = winch.recv() => {
                let (cols, rows) = window_size(stdin_fd);
                socket.0.send(Message::text(resize_command(cols, rows))).await
                    .map_err(|e| format!("resize: {e}"))?;
            }
            _ = term.recv() => break,
            _ = hup.recv() => break,
        }
    }
    // the operator ended it: ask the service to close the session rather than
    // leaving a pty running behind a socket this process is about to drop.
    let _ = socket
        .0
        .send(Message::text(CLOSE_COMMAND.to_string()))
        .await;
    let _ = socket.0.close().await;
    Ok(Detached::Ended)
}

/// Open the session WebSocket through the operator door, surfacing the node's
/// own refusal sentence when it declines the upgrade.
///
/// The destination and the resume cursor live in the SIGNED uri query, never in
/// a side header: changing either changes what this caller's operator
/// credential covers.
async fn open_attachment(
    base: &str,
    operator: Option<&str>,
    head: &gateway::ProxyRequestHead,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Box<dyn std::error::Error>,
> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

    let encoded = gateway::encode_proxy_request_head(head)?;
    let mut url = reqwest::Url::parse(&operator_ws_url(base))?;
    url.query_pairs_mut()
        .append_pair("head", std::str::from_utf8(&encoded)?);
    let mut request = url.as_str().into_client_request()?;
    if let Some(token) = operator {
        request
            .headers_mut()
            .insert(noded::admin::ADMIN_TOKEN_HEADER, token.parse()?);
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((socket, _)) => Ok(socket),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            let status = response.status();
            let body = response.body().as_deref().unwrap_or_default();
            Err(format!(
                "the terminal attachment was refused ({status}): {}",
                error_field(&String::from_utf8_lossy(body))
            )
            .into())
        }
        Err(error) => Err(format!("open the terminal attachment: {error}").into()),
    }
}

/// What one server frame gave this terminal, after the cursor advanced past it.
enum Served {
    /// raw pty bytes to write.
    Output(Vec<u8>),
    /// a replay snapshot: `Some(head)` when it reported the session over. The
    /// session is over once the cursor has consumed every frame up to that
    /// head — reading `ended` is not the same as having read the final screen
    /// the service already queued behind it.
    Snapshot(Option<u64>),
    /// a command result, or anything else this client does not draw.
    Nothing,
}

/// Decode one service frame and advance the cursor by exactly what it carried.
///
/// A refused command (`result` with an `Err`) is reported and does NOT end the
/// attachment: a resize the service declined is not a dead session, and the
/// `ended` bound is the one thing that says a session is over.
fn serve_frame(text: &str, cursor: &mut u64) -> Result<Served, Box<dyn std::error::Error>> {
    let frame: serde_json::Value = match serde_json::from_str(text) {
        Ok(frame) => frame,
        Err(_) => return Ok(Served::Nothing),
    };
    match frame["event"].as_str() {
        Some("output") => {
            let Some(seq) = frame["seq"].as_u64() else {
                return Ok(Served::Nothing);
            };
            let bytes = STANDARD.decode(frame["data_b64"].as_str().unwrap_or_default())?;
            *cursor = (*cursor).max(seq);
            Ok(Served::Output(bytes))
        }
        Some("replay") => Ok(Served::Snapshot(snapshot_bound(&frame, *cursor))),
        Some("result") => {
            if let Some(refusal) = frame["result"]["Err"].as_str() {
                eprint!("\r\n-- the terminal service refused a command: {refusal}\r\n");
            }
            Ok(Served::Nothing)
        }
        _ => Ok(Served::Nothing),
    }
}

/// Read a replay snapshot: report output this attachment can never see, and
/// answer with the completion bound when the snapshot says the session ended.
///
/// `first` is the oldest sequence the service still retains. A `first` past the
/// cursor means the frames between them are gone, which is a visible hole in the
/// terminal — say so once, where it happened, rather than printing bytes that
/// silently skip.
fn snapshot_bound(frame: &serde_json::Value, cursor: u64) -> Option<u64> {
    let first = frame["first"].as_u64().unwrap_or(cursor + 1);
    let lost = first > cursor + 1;
    if lost {
        eprint!(
            "\r\n-- output {}..{} was dropped by the terminal service\r\n",
            cursor + 1,
            first - 1
        );
    }
    frame["ended"]
        .as_bool()
        .unwrap_or(false)
        .then(|| frame["head"].as_u64().unwrap_or(cursor))
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

/// `http(s)://host:port` → the operator door's ws url. The destination and the
/// resume cursor ride the query this url is completed with, because the signed
/// uri is what the node's operator gate covers.
fn operator_ws_url(base: &str) -> String {
    let ws_base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    format!("{}/v1/gateway/operator", ws_base.trim_end_matches('/'))
}

/// The three commands an attachment may send the terminal service. Each names
/// its session by the socket it arrived on, so none carries a session id.
fn input_command(data_b64: &str) -> String {
    serde_json::json!({ "op": "input", "data_b64": data_b64 }).to_string()
}

fn resize_command(cols: u16, rows: u16) -> String {
    serde_json::json!({ "op": "resize", "cols": cols, "rows": rows }).to_string()
}

const CLOSE_COMMAND: &str = r#"{"op":"close"}"#;

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
            .try_get_matches_from([
                "pty",
                "pi",
                "--account",
                "12",
                "--route",
                "terminal",
                "--cred",
                "work",
            ])
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
        assert_eq!(
            operator_ws_url("http://127.0.0.1:8080"),
            "ws://127.0.0.1:8080/v1/gateway/operator"
        );
        assert_eq!(
            operator_ws_url("https://host:9/"),
            "wss://host:9/v1/gateway/operator"
        );
    }

    fn destination() -> Destination {
        Destination {
            account: 12,
            name: gateway::RouteName::named("terminal".to_string()),
            revision: 7,
        }
    }

    /// The operator door stamps the operator assertion; a client that stamped
    /// it itself would be asserting something nothing verified. The head must
    /// therefore leave it false — and carry the destination the operator named,
    /// not a default one.
    #[test]
    fn an_operator_head_names_the_route_and_asserts_nothing() {
        let head = operator_head(
            &destination(),
            gateway::RouteMethod::Post,
            "/sessions".into(),
            vec![gateway::ProxyHeader {
                name: "content-type".into(),
                value: "application/json".into(),
            }],
            2,
            false,
        );
        assert!(!head.operator);
        assert!(head.user_pop.is_none());
        assert_eq!(head.account_id, 12);
        assert_eq!(head.name.label.as_deref(), Some("terminal"));
        assert_eq!(head.revision, 7);
        gateway::validate_proxy_request_head(&head).unwrap();
    }

    /// The attachment's destination AND its resume cursor live in the head the
    /// signed uri query carries, so the whole path is what the operator
    /// credential covers.
    #[test]
    fn an_attachment_head_is_a_bodyless_upgrade_carrying_its_cursor() {
        let cursor = 41u64;
        let head = operator_head(
            &destination(),
            gateway::RouteMethod::Get,
            format!("/sessions/{}?after={cursor}", "0000000000000001"),
            Vec::new(),
            0,
            true,
        );
        gateway::validate_proxy_request_head(&head).unwrap();
        assert_eq!(head.path_and_query, "/sessions/0000000000000001?after=41");
        assert_eq!(head.body_len, 0);
        assert!(head.upgrade);
    }

    /// The cursor advances on the frame that reached the terminal, never on the
    /// snapshot bound announced ahead of it: a socket that dies between the
    /// `replay` head and its chunks must resume at the last byte SHOWN.
    #[test]
    fn the_cursor_advances_only_past_frames_this_terminal_consumed() {
        let mut cursor = 0u64;
        let snapshot = serde_json::json!({
            "event": "replay", "first": 1, "head": 3, "ended": false,
        })
        .to_string();
        assert!(matches!(
            serve_frame(&snapshot, &mut cursor).unwrap(),
            Served::Snapshot(None)
        ));
        assert_eq!(cursor, 0, "a head is a bound, not a receipt");

        let chunk = serde_json::json!({
            "event": "output", "seq": 1, "data_b64": STANDARD.encode(b"hi"),
        })
        .to_string();
        let Served::Output(bytes) = serve_frame(&chunk, &mut cursor).unwrap() else {
            panic!("output frame");
        };
        assert_eq!(bytes, b"hi");
        assert_eq!(cursor, 1);
    }

    /// An `ended` snapshot is a bound too: the final screen the service already
    /// queued behind it still has to reach the terminal, so the attachment ends
    /// when the cursor REACHES the bound, not when it reads it.
    #[test]
    fn an_ended_snapshot_ends_the_attachment_only_once_its_frames_are_drained() {
        let mut cursor = 0u64;
        let ended = serde_json::json!({
            "event": "replay", "first": 1, "head": 2, "ended": true,
        })
        .to_string();
        let Served::Snapshot(Some(bound)) = serve_frame(&ended, &mut cursor).unwrap() else {
            panic!("ended snapshot");
        };
        assert!(cursor < bound);
        for seq in 1..=2 {
            let chunk = serde_json::json!({
                "event": "output", "seq": seq, "data_b64": STANDARD.encode(b"x"),
            })
            .to_string();
            serve_frame(&chunk, &mut cursor).unwrap();
        }
        assert!(cursor >= bound);

        // an already-drained ended snapshot ends the attachment immediately.
        let mut drained = 2u64;
        let Served::Snapshot(Some(bound)) = serve_frame(&ended, &mut drained).unwrap() else {
            panic!("ended snapshot");
        };
        assert!(drained >= bound);
    }

    /// A refused command is reported, never mistaken for the session ending —
    /// and an unknown frame is ignored rather than breaking the pump.
    #[test]
    fn a_refused_command_and_an_unknown_frame_both_keep_the_attachment() {
        let mut cursor = 0u64;
        for frame in [
            serde_json::json!({"event":"result","result":{"Err":"session is not running"}})
                .to_string(),
            serde_json::json!({"event":"result","result":{"Ok":null}}).to_string(),
            serde_json::json!({"event":"who-knows"}).to_string(),
            "not json".to_string(),
        ] {
            assert!(matches!(
                serve_frame(&frame, &mut cursor).unwrap(),
                Served::Nothing
            ));
        }
        assert_eq!(cursor, 0);
    }

    #[test]
    fn client_commands_carry_the_snake_case_op_tags() {
        let input: serde_json::Value = serde_json::from_str(&input_command("ZGF0YQ==")).unwrap();
        assert_eq!(input["op"], "input");
        assert_eq!(input["data_b64"], "ZGF0YQ==");
        // the session is the socket's, never a field a client may retarget.
        assert!(input.get("session").is_none());

        let resize: serde_json::Value = serde_json::from_str(&resize_command(120, 40)).unwrap();
        assert_eq!(resize["op"], "resize");
        assert_eq!(resize["cols"], 120);
        assert_eq!(resize["rows"], 40);

        let close: serde_json::Value = serde_json::from_str(CLOSE_COMMAND).unwrap();
        assert_eq!(close["op"], "close");
    }

    /// Both halves of the selection are required: nothing here defaults to an
    /// account or invents a terminal route label.
    #[test]
    fn pty_refuses_to_guess_which_installed_service_runs_the_session() {
        use clap::Args as _;
        let command = PtyArgs::augment_args(clap::Command::new("pty"));
        for missing in [
            vec!["pty", "claude"],
            vec!["pty", "claude", "--account", "12"],
            vec!["pty", "claude", "--route", "terminal"],
        ] {
            assert!(
                command
                    .clone()
                    .try_get_matches_from(missing.clone())
                    .is_err(),
                "{missing:?} must be refused"
            );
        }
        assert!(
            command
                .try_get_matches_from(["pty", "claude", "--account", "12", "--route", "terminal"])
                .is_ok()
        );
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
