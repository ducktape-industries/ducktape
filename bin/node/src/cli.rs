//! Synchronous operator commands for network setup and membership.
//!
//! Command handlers live outside the node runtime so boot orchestration is not
//! coupled to filesystem setup, local RPC calls, or membership ceremonies.

use std::path::PathBuf;

use commonware_cryptography::Signer as _;

use crate::cli_args::{
    AdmitArgs, InitArgs, InviteArgs, JoinCmd, JoinQuery, KeyArgs, MemberCmd, OpCmd, PubkeyArgs,
    ResidentCmd, Selector, SelectorArgs, StatusArgs, WorkCmd, WorkTargetArgs,
};
use crate::config;
use crate::work_admission::{self, AdmitTarget, WorkAdmission};
use config::{hex_bytes, unhex};

type CommandResult = Result<(), Box<dyn std::error::Error>>;

/// how long `node peers` holds between its two samples. rates divide by the
/// measured gap, so this only trades the verb's latency against how many
/// messages a rate averages over.
const PEER_RATE_SAMPLE_GAP: std::time::Duration = std::time::Duration::from_secs(1);

/// route one operator verb to its handler — ONE visible dispatch, nothing in
/// the arms but delegation. (`run` never reaches here; `main.rs` owns the
/// node-boot path.) the grammar itself lives in `cli_args.rs`.
pub(super) fn run(op: OpCmd) -> CommandResult {
    match op {
        OpCmd::Key(args) => cmd_keygen(args),
        OpCmd::Init(args) => cmd_init(args),
        OpCmd::Invite(args) => cmd_invite(args),
        OpCmd::Admit(args) => cmd_admit(args),
        OpCmd::Join(cmd) => dispatch_join(cmd),
        OpCmd::List => cmd_list(),
        OpCmd::Status(args) => cmd_node_status(args),
        OpCmd::Qualify(args) => crate::qualify::run(args),
        OpCmd::Peers(args) => cmd_node_peers(args),
        OpCmd::Resident(cmd) => dispatch_resident(cmd),
        OpCmd::Member(cmd) => dispatch_member(cmd),
        OpCmd::Work(cmd) => dispatch_work(cmd),
        OpCmd::Sandbox(args) => crate::sandbox_cli::run(args),
        OpCmd::LogFilter(args) => cmd_log_filter(args),
        OpCmd::Netstack(cmd) => dispatch_netstack(cmd),
    }
}

fn dispatch_netstack(cmd: crate::cli_args::NetstackCmd) -> CommandResult {
    match cmd {
        crate::cli_args::NetstackCmd::Swap(args) => cmd_netstack_swap(args),
    }
}

/// `ducktape node log-filter <FILTER>` — retune a RUNNING node's tracing
/// filter, signed.
///
/// This verb exists because `POST /v1/log-filter` mutates the process and so
/// requires a user signature (`noded::signed_req`), which `curl` cannot mint.
/// The node key the signature is bound to comes from the node ITSELF
/// (`/v1/status`), not from a local workspace, so `--node <url>` works against
/// a node this host holds no config for.
fn cmd_log_filter(args: crate::cli_args::LogFilterArgs) -> CommandResult {
    let ctx = crate::cred_cli::VerbCtx {
        addr: args.addr,
        key: args.key,
    };
    let base = ctx.http_base()?;
    let key_path = ctx.key_path()?;
    let node_key = crate::node_http::pinned_node_key(&key_path, &base, args.trust_node)?;
    let mut stdin = std::io::BufReader::new(std::io::stdin());
    let signer = crate::userkey_cli::load_user_signer_for(&base, &key_path, &mut stdin)?;

    const PATH: &str = "/v1/log-filter";
    let body = args.filter.into_bytes();
    let mut request = reqwest::blocking::Client::new()
        .post(format!("{base}{PATH}"))
        .body(body.clone());
    for (name, value) in noded::signed_req::request_headers(&signer, "POST", PATH, &node_key, &body)
    {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .map_err(|error| crate::node_http::transport_failure(&base, PATH, &error).to_string())?;
    let status_code = response.status();
    let text = response.text().unwrap_or_default();
    if !status_code.is_success() {
        return Err(format!("{PATH} rejected ({status_code}): {text}").into());
    }
    println!("{text}");
    Ok(())
}

fn dispatch_work(cmd: WorkCmd) -> CommandResult {
    match cmd {
        WorkCmd::List(args) => cmd_work_list(args),
        WorkCmd::Admit(args) => cmd_work_admit(args),
        WorkCmd::Revoke(args) => cmd_work_revoke(args),
    }
}

/// the workspace directory a `node work` verb reads and writes. The policy sits
/// beside `node.toml`, and both the node and the compute daemon read it there.
fn work_workspace(selector: &Selector) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cfg_path = selector.config_path()?;
    Ok(cfg_path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf())
}

/// `anyone` is the literal, everything else is an account NUMBER (the same
/// authority resolution `cred grant` takes — a display name is refused, never
/// matched: it is freely rewritable and not unique, and this decision is
/// whose workload the node runs). A number resolves offline.
fn resolve_work_target(
    workspace: &std::path::Path,
    input: &str,
) -> Result<AdmitTarget, Box<dyn std::error::Error>> {
    if input == work_admission::ANYONE {
        return Ok(AdmitTarget::Anyone);
    }
    let base = config::http_base_in(workspace)?;
    Ok(AdmitTarget::Account(
        crate::account_cli::resolve_account_authority(&base, input)?,
    ))
}

fn cmd_work_list(args: SelectorArgs) -> CommandResult {
    let workspace = work_workspace(&args.selector)?;
    match work_admission::load(&workspace)? {
        WorkAdmission::Anyone => {
            println!("anyone — every network member may run a workload on this node")
        }
        WorkAdmission::Accounts(accounts) => {
            println!(
                "this node's own submissions, plus {} admitted account(s):",
                accounts.len()
            );
            for account in &accounts {
                println!("  {account}");
            }
        }
    }
    println!(
        "policy: {}",
        work_admission::policy_path(&workspace).display()
    );
    Ok(())
}

fn cmd_work_admit(args: WorkTargetArgs) -> CommandResult {
    let workspace = work_workspace(&args.selector)?;
    let target = resolve_work_target(&workspace, &args.target)?;
    let policy = work_admission::load(&workspace)?.with(target.clone());
    work_admission::save(&workspace, &policy)?;
    match target {
        AdmitTarget::Anyone => {
            // the `service enable` consent-screen precedent: the widening that
            // re-opens the hole says so on the way in, on stderr so stdout stays
            // scriptable.
            eprintln!(
                "consent: every network member may now run a workload on this node, and any \
                 workload here may draw on every credential this node has been granted. \
                 Narrow it with `ducktape node work revoke anyone`."
            );
            println!("admitted: anyone");
        }
        AdmitTarget::Account(account) => println!("admitted: {account}"),
    }
    Ok(())
}

fn cmd_work_revoke(args: WorkTargetArgs) -> CommandResult {
    let workspace = work_workspace(&args.selector)?;
    let target = resolve_work_target(&workspace, &args.target)?;
    let current = work_admission::load(&workspace)?;
    // revoking ONE account while the policy admits everyone would write a file
    // that changes nothing and print success: refuse instead of fail-quiet.
    let narrowing_one_from_anyone =
        current == WorkAdmission::Anyone && !matches!(target, AdmitTarget::Anyone);
    if narrowing_one_from_anyone {
        return Err(
            "this node admits anyone, so revoking one account changes nothing — \
                    run `ducktape node work revoke anyone` first"
                .into(),
        );
    }
    work_admission::save(&workspace, &current.without(target.clone()))?;
    match target {
        AdmitTarget::Anyone => println!("revoked: anyone"),
        AdmitTarget::Account(account) => println!("revoked: {account}"),
    }
    Ok(())
}

fn dispatch_resident(cmd: ResidentCmd) -> CommandResult {
    match cmd {
        ResidentCmd::Accept(args) => cmd_invite_accept(args),
        ResidentCmd::Remove(args) => cmd_resident_remove(args),
    }
}

fn dispatch_member(cmd: MemberCmd) -> CommandResult {
    match cmd {
        MemberCmd::Promote(args) => cmd_promote(args),
        MemberCmd::Remove(args) => cmd_member_remove(args),
        MemberCmd::Leave(args) => cmd_member_leave(args),
        MemberCmd::Status(args) => cmd_member_status(args),
    }
}

/// `join` is BOTH a leaf verb (`join <blob>`) and a subfamily prefix
/// (`join requests`, `join state`) — a subcommand token wins.
fn dispatch_join(cmd: JoinCmd) -> CommandResult {
    match cmd.query {
        Some(JoinQuery::Requests(args)) => cmd_join_requests(args),
        Some(JoinQuery::State(args)) => cmd_join_state(args),
        None => cmd_join(cmd),
    }
}

/// `list` — enumerate the workspaces under the ducktape home, one
/// `chain-id<TAB>config-path` line per network on stdout. an empty home
/// prints a friendly notice on stderr and exits 0 (no workspace yet is not
/// an error).
fn cmd_list() -> CommandResult {
    let workspaces = config::list_workspaces()?;
    if workspaces.is_empty() {
        eprintln!("no workspaces under {}", config::ducktape_home()?.display());
        return Ok(());
    }
    for (chain_id, config_path) in workspaces {
        println!("{chain_id}\t{}", config_path.display());
    }
    Ok(())
}

/// `status [--config <path> | -n <chain-id>] [--json]` — read the RUNNING
/// node's tip off its local rpc and print it to stdout, one line per subject
/// ([`status_lines`]):
///
/// ```text
/// height=<h> root_hash=<hex>
/// role=<role> phase=<phase> …
/// follow: behind_by=<n> network_height=<h> (heard <age> ago)
/// ```
///
/// `height=none` means no block has finalized yet. `--json` emits the rpc's
/// full status object (height, root_hash, every module root). requires the
/// node to be up — the same local rpc lane as `member status`.
fn cmd_node_status(args: StatusArgs) -> CommandResult {
    let cfg_path = args.selector.config_path()?;
    let resolved = config::resolve(&cfg_path)?;
    let rpc_addr = resolved
        .rpc_listen
        .clone()
        .ok_or("node status reads the node's local rpc — set `rpc_listen` in node.toml")?;
    let reply = rpc_call(&rpc_addr, &serde_json::json!({ "cmd": "status" }))?;
    if reply["ok"] != true {
        return Err(format!("status: {}", reply["error"]).into());
    }
    let status = &reply["status"];
    if args.json {
        println!("{status}");
        return Ok(());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock is past the epoch")
        .as_secs();
    for line in status_lines(status, now) {
        println!("{line}");
    }
    let Some(seconds) = stalled_past_recovery(status) else {
        return Ok(());
    };
    // NOT an `Err`: the verb did its job, and an `Err` here would print the
    // node's own `FATAL:` marker — the string the desktop app classifies a
    // dead node by — for a node that answered perfectly well. A distinct code
    // says "answered, and the news is bad", which is the difference a script
    // needs.
    eprintln!(
        "the chain has sealed nothing for {seconds}s, past the point it recovers on its own — \
         compare reachable against quorum above, and check the other members"
    );
    std::process::exit(CHAIN_IS_STALLED);
}

/// `node status` exit code for a node that answered and reported a stalled
/// chain. Distinct from 1, which every verb uses for "could not answer at all"
/// — an operator's `ducktape node status || alert` must not treat a node that
/// is merely unreachable as a wedged network, or the reverse.
const CHAIN_IS_STALLED: i32 = 2;

/// What `node status` prints, in order — one `key=value` line per subject, so
/// the whole answer stays greppable.
fn status_lines(status: &serde_json::Value, now: u64) -> Vec<String> {
    let height = match status["height"].as_u64() {
        Some(h) => h.to_string(),
        None => "none".into(),
    };
    let root_hash = status["root_hash"].as_str().unwrap_or("");
    let operations = &status["operations"];
    let mut lines = vec![format!("height={height} root_hash={root_hash}")];
    lines.extend(standing_line(operations));
    lines.push(follow_line(&operations["follow"], now));
    lines.extend(netstack_line(&operations["netstack"]));
    lines
}

/// the `follow:` line: how far this node's height is from the tip a peer last
/// answered with, and how long ago that answer landed — the one comparison
/// that tells a joiner whether `height=` is the tip or far below it. It reads
/// the same [`noded::FollowOperationalStatus`] `--json` serializes, and
/// `behind_by=0` prints like any other gap: "caught up" is an answer too.
///
/// A node no peer has answered yet says so, rather than a zero gap it never
/// measured.
fn follow_line(follow: &serde_json::Value, now: u64) -> String {
    let Ok(follow) = serde_json::from_value::<noded::FollowOperationalStatus>(follow.clone())
    else {
        return "follow: none yet".to_string();
    };
    let heard = human_duration(now.saturating_sub(follow.heard_at));
    format!(
        "follow: behind_by={} network_height={} (heard {heard} ago)",
        follow.behind_by, follow.network_height
    )
}

/// the `role=`/`phase=` line: where this node stands, and — when it is in
/// consensus — whether the chain under it is moving.
///
/// Height and root hash cannot answer that: on a wedged chain they are
/// byte-identical six seconds and six minutes later, which is how a halted
/// network read as a healthy one. `reachable` beside `quorum` is the diagnosis
/// and `stalled_for` is how long it has been true, so both belong on the line
/// an operator was told to run.
///
/// Consensus fields are absent, never zeroed, on a role that has no consensus
/// section — `quorum=0 reachable=0` on a syncing resident reads as a dead
/// chain, and it is not one.
fn standing_line(operations: &serde_json::Value) -> Option<String> {
    let role = operations["role"].as_str()?;
    let phase = operations["phase"].as_str()?;
    let mut line = format!("role={role} phase={phase}");
    let consensus = &operations["consensus"];
    if let Some(quorum) = consensus["quorum"].as_u64() {
        let reachable = consensus["reachable_validators"].as_u64().unwrap_or(0);
        line.push_str(&format!(" quorum={quorum} reachable={reachable}"));
    }
    // 0 is the beating case and prints nothing: a field that is always there
    // is a field nobody reads, and this one has to be noticed.
    if let Some(seconds) = consensus["block_beat_stalled_seconds"]
        .as_u64()
        .filter(|seconds| *seconds > 0)
    {
        line.push_str(&format!(" stalled_for={seconds}s"));
    }
    // the gap that sizes a `phase=behind` is the next line's: [`follow_line`].
    Some(line)
}

/// How long the chain has been silent, once that is past the point it recovers
/// on its own ([`crate::drain_actions::STALL_IS_AN_ERROR_AFTER`], the same
/// threshold the node's own `block_beat_stalled` error fires on).
///
/// `None` is "nothing to report", which covers a beating chain, a brief
/// silence, and a node with no consensus section to ask. The verb exits
/// non-zero on `Some` — the whole point being that a script can tell a wedged
/// chain from a healthy one without parsing anything.
fn stalled_past_recovery(status: &serde_json::Value) -> Option<u64> {
    let seconds = status["operations"]["consensus"]["block_beat_stalled_seconds"].as_u64()?;
    let past_recovery = seconds >= crate::drain_actions::STALL_IS_AN_ERROR_AFTER.as_secs();
    past_recovery.then_some(seconds)
}

/// the `netstack=` line of `node status`: which machine the reachability plane
/// runs on, and — once this process has swapped at all — how the last swap
/// went. `None` on a node with no plane, which prints nothing rather than a
/// misleading `netstack=none`.
///
/// A PLANE THAT IS NOT RUNNING PREEMPTS THE SWAP HISTORY. This node has no
/// overlay at all while that holds — no tunnels, no join door — so the swap it
/// last answered is not what the reader needs; the reason it has no mesh is.
fn netstack_line(netstack: &serde_json::Value) -> Option<String> {
    let backend = netstack["backend"].as_str()?;
    if let Some((reason, detail)) = netstack_failure_in_section(netstack) {
        return Some(format!(
            "netstack={backend} NO MESH reason={reason} — {detail}"
        ));
    }
    let last_swap = &netstack["last_swap"];
    let Some(outcome) = last_swap["outcome"].as_str() else {
        return Some(format!("netstack={backend}"));
    };
    let at_height = last_swap["at_height"].as_u64().unwrap_or(0);
    let reason = match last_swap["reason"].as_str() {
        Some(reason) => format!(" reason={reason}"),
        None => String::new(),
    };
    Some(format!(
        "netstack={backend} last_swap={outcome}@{at_height}{reason}"
    ))
}

/// `ducktape node netstack swap --component <PATH>` — move a
/// RUNNING node's reachability plane onto another netstack backend, mid-life.
///
/// The operator's trigger, next to the governance-delivered one: roll one node
/// forward onto a component or back onto native without waiting for a vote. It
/// presents this node's operator credential (`admin.token`, beside `node.toml`)
/// the way every other `/v1/admin/*` client verb does — so it needs the
/// node's WORKSPACE, not just its address. CEILING: under
/// `DUCKTAPE_ADMIN=public` the namespace wants an owner PoP instead, which this
/// verb does not mint (the same limit `node module` staging has).
fn cmd_netstack_swap(args: crate::cli_args::NetstackSwapArgs) -> CommandResult {
    let cfg_path = args.selector.config_path()?;
    let workspace = cfg_path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf();
    let backend = serde_json::json!({ "component": args.component });
    const PATH: &str = "/v1/admin/netstack/swap";
    let base = config::http_base_in(&workspace)?;
    let token = noded::admin::read_operator_token(&workspace)?;
    let response = reqwest::blocking::Client::new()
        .post(format!("{base}{PATH}"))
        .header(noded::admin::ADMIN_TOKEN_HEADER, token)
        .json(&serde_json::json!({ "backend": backend }))
        .send()
        .map_err(|error| crate::node_http::transport_failure(&base, PATH, &error).to_string())?;
    let status_code = response.status();
    let text = response.text().unwrap_or_default();
    if !status_code.is_success() {
        return Err(format!("netstack swap rejected ({status_code}): {text}").into());
    }
    let reply: serde_json::Value = serde_json::from_str(&text)?;
    println!("netstack={}", reply["backend"].as_str().unwrap_or(""));
    Ok(())
}

/// `peers [--config <path> | -n <chain-id>] [--json]` — the RUNNING node's
/// direct-peer sample off its local rpc: its own height, then one
/// `key=value` line per peer ([`peers_lines`]).
/// `--json` emits one raw [`noded::peers::PeersView`] sample (cumulative
/// counters — consumers derive rates from deltas); the prose form takes a
/// second sample after one second so the line can carry live `…/s` rates.
fn cmd_node_peers(args: StatusArgs) -> CommandResult {
    let cfg_path = args.selector.config_path()?;
    let resolved = config::resolve(&cfg_path)?;
    let rpc_addr = resolved
        .rpc_listen
        .clone()
        .ok_or("node peers reads the node's local rpc — set `rpc_listen` in node.toml")?;
    let first = peers_rpc(&rpc_addr)?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string(&first).expect("peers view serializes")
        );
        return Ok(());
    }
    // cumulative counters only become rates as a delta over time: hold one
    // second, sample again, and let the SECOND sample carry the truth.
    let second = match first.peers.is_empty() {
        true => None,
        false => {
            std::thread::sleep(PEER_RATE_SAMPLE_GAP);
            Some(peers_rpc(&rpc_addr)?)
        }
    };
    for line in peers_lines(&first, second.as_ref()) {
        println!("{line}");
    }
    Ok(())
}

/// What `node peers` prints: the answering node's own position first — the
/// `height` (and `epoch`) the sample stamps beside the table, the same
/// figures `--json` carries — then one line per peer. `second` is the rate
/// sample, absent when the first one found no peer to rate.
///
/// The mesh gossips no per-peer head, so no row carries a peer's height; the
/// `sync_height=`/`sync_boundary=` a row may carry are what THIS node served
/// that peer over state sync.
fn peers_lines(
    first: &noded::peers::PeersView,
    second: Option<&noded::peers::PeersView>,
) -> Vec<String> {
    let Some(second) = second else {
        return vec![position_line(first), "no direct peers".to_string()];
    };
    let rows = second.peers.iter().map(|peer| {
        let baseline = first.peers.iter().find(|p| p.peer == peer.peer);
        peer_line(peer, baseline, first, second)
    });
    std::iter::once(position_line(second)).chain(rows).collect()
}

/// `height=<h>[ epoch=<e>]` — the sampler's coordinates; a lane with no
/// consensus has no epoch to name.
fn position_line(view: &noded::peers::PeersView) -> String {
    match view.epoch {
        Some(epoch) => format!("height={} epoch={epoch}", view.height),
        None => format!("height={}", view.height),
    }
}

/// one `peers` rpc round-trip, decoded to the shared view.
fn peers_rpc(addr: &str) -> Result<noded::peers::PeersView, String> {
    let reply = rpc_call(addr, &serde_json::json!({ "cmd": "peers" }))?;
    if reply["ok"] != true {
        return Err(format!("peers: {}", reply["error"]));
    }
    serde_json::from_value(reply["peers"].clone()).map_err(|e| format!("peers reply: {e}"))
}

/// one peer's `key=value` prose line. rates appear only when the peer was
/// present in the baseline sample — a peer first seen mid-measurement has no
/// honest denominator.
fn peer_line(
    peer: &noded::peers::PeerView,
    baseline: Option<&noded::peers::PeerView>,
    first: &noded::peers::PeersView,
    second: &noded::peers::PeersView,
) -> String {
    let mut line = format!("peer={}", peer.peer);
    if let Some(role) = &peer.role {
        line.push_str(&format!(" role={role}"));
    }
    // always rendered, `unknown` included: a column that vanished when the
    // stamp was missing would read as "same build as ours", which is exactly
    // the silence the skew diagnostic exists to break.
    line.push_str(&format!(
        " build={}",
        peer.build
            .as_deref()
            .unwrap_or(noded::services::UNKNOWN_BUILD)
    ));
    match peer.connected_since_ms {
        Some(since) => {
            let for_secs = second.sampled_at_ms.saturating_sub(since) / 1000;
            line.push_str(&format!(" connected={}", human_duration(for_secs)));
        }
        None => line.push_str(" connected=no"),
    }
    line.push_str(&format!(
        " msgs_tx={} msgs_rx={}",
        peer.msgs_sent, peer.msgs_received
    ));
    let dt_secs = (second.sampled_at_ms.saturating_sub(first.sampled_at_ms)).max(1) as f64 / 1000.0;
    if let Some(base) = baseline {
        let tx_rate = (peer.msgs_sent.saturating_sub(base.msgs_sent)) as f64 / dt_secs;
        let rx_rate = (peer.msgs_received.saturating_sub(base.msgs_received)) as f64 / dt_secs;
        line.push_str(&format!(" tx/s={tx_rate:.1} rx/s={rx_rate:.1}"));
    }
    let Some(sync) = &peer.statesync else {
        return line;
    };
    line.push_str(&format!(" sync_bytes={}", sync.bytes_tx));
    let baseline_sync = baseline.and_then(|b| b.statesync.as_ref());
    if let Some(base) = baseline_sync {
        let byte_rate = (sync.bytes_tx.saturating_sub(base.bytes_tx)) as f64 / dt_secs;
        line.push_str(&format!(" sync_B/s={byte_rate:.0}"));
    }
    if let Some(height) = sync.served_height {
        line.push_str(&format!(" sync_height={height}"));
    }
    if let Some(boundary) = sync.boundary_height {
        line.push_str(&format!(" sync_boundary={boundary}"));
    }
    line.push_str(&format!(" sync_idle={}s", sync.idle_seconds));
    if let Some(kind) = &sync.last_request_kind {
        line.push_str(&format!(" sync_last={kind}"));
    }
    line
}

/// seconds → compact `42s` / `3m12s` / `2h05m` prose.
fn human_duration(secs: u64) -> String {
    let (hours, minutes, seconds) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if hours > 0 {
        return format!("{hours}h{minutes:02}m");
    }
    if minutes > 0 {
        return format!("{minutes}m{seconds:02}s");
    }
    format!("{seconds}s")
}

// ============================================================================
// onboarding verbs — key / init / invite / admit / join.
// ============================================================================

/// `key` — generate (or reuse) a persisted ed25519 identity. pubkey on stdout
/// (scriptable); provenance on stderr. `--dir <dir>` mints (or reuses)
/// `<dir>/identity.key`, creating the dir: this is the JOIN CODE an invitee
/// hands the inviter so the invite can be locked to this key before the
/// workspace joins anything.
fn cmd_keygen(args: KeyArgs) -> Result<(), Box<dyn std::error::Error>> {
    let out = match args.dir {
        Some(dir) => {
            std::fs::create_dir_all(&dir)?;
            dir.join("identity.key")
        }
        None => args.out.unwrap_or_else(|| PathBuf::from("identity.key")),
    };
    let (key, generated) = config::load_or_generate_identity(&out)?;
    println!("{}", hex_bytes(key.public_key().as_ref()));
    eprintln!(
        "{} identity at {}",
        if generated { "generated" } else { "reusing" },
        out.display()
    );
    Ok(())
}

/// fresh-workspace compute detection with the operator note (`init`; `join`
/// says it from the library's answer). The probe and the table it writes come
/// from `config::platform_sandbox`, so a host can never be probed for one thing
/// and configured for another.
///
/// The table says only HOW runs would be isolated on this host — it grants
/// nothing. Whether this node runs a compute service at all is the user's
/// `ducktape service enable compute`, so detection can stay eager: it makes the
/// interactive terminal plane work out of the box and leaves the compute plane
/// dark until someone consents to it.
fn detect_platform_sandbox(workspace: &std::path::Path) -> Option<config::SandboxToml> {
    let (table, found) = config::detect_platform_sandbox(workspace)?;
    eprintln!(
        "compute plane: {} found at {} — writing a live [sandbox] table \
         (announce stays off; delete the table for a consensus-only node)",
        table.runtime,
        found.display()
    );
    Some(table)
}

/// `init --name <name> [--dir <dir>] [--modules <dir>] [--listen a]
/// [--advertised a] [--http a] [--rpc a] [--primary-coordinator host:port|none]
/// [--wireguard-listen a] [--wireguard-advertised host:port] [--invite-listen a]`
/// — found a network: mint the chain-id, write the descriptor + node config,
/// seed the genesis validator set with this identity, and PIN the genesis wasm
/// set — every component and index guest in `--modules` is composed into
/// `<workspace>/genesis`, whose hash (and every deployment's) is in the
/// descriptor. Every flag is optional:
/// the generated config defaults to a WORKING node — overlay advertise, and
/// every listener at its `config::DEFAULT_*_LISTEN` constant (mesh, HTTP,
/// RPC, gateway, WireGuard), which is the one place those ports are written
/// down — and prints every key, so the file itself documents what to change.
/// without `--dir` the workspace lands under the ducktape home
/// (`~/.ducktape/<chain-id>/`), where `-n <chain-id>` finds it.
fn cmd_init(args: InitArgs) -> Result<(), Box<dyn std::error::Error>> {
    let name = &args.name;
    let explicit_dir = args.dir.is_some();
    // the genesis wasm set. founding PINS it: the founding set's components
    // and index guests compose into the genesis file, whose hash and every
    // component's hash go into the descriptor (both are IN the genesis
    // fingerprint, so a node holding other bytes is a different network), and
    // that file is written into the workspace below.
    //
    // FIRST, before any directory is created: an absent or incomplete founding
    // set is the one refusal that has nothing to do with the flags, and a
    // founding that dies after `create_dir_all` leaves an orphan workspace
    // holding a freshly minted `identity.key` behind on every attempt.
    let founding_set = match args.modules {
        Some(src) => src,
        None => noded::services::founding_set()?,
    };
    let genesis = config::Genesis::compose(&founding_set).map_err(|e| {
        format!("{e} — pass --modules <dir> holding every <id>.component.wasm and <id>.index.wasm")
    })?;
    // discovery above only checked filenames, ids, and the mapper/component
    // framing — a zero-byte or truncated artifact decodes fine and would
    // otherwise mint a network that never boots (#1840). Compile every
    // component and mapper now, before anything is written.
    config::validate_founding_set(&founding_set, &genesis)?;
    let genesis_bytes = genesis.encode();
    let genesis_hash = config::sha256(&genesis_bytes);
    let hashes = genesis.module_hashes();
    // the workspace dir: `--dir` is the explicit escape hatch; the default is
    // the home — `~/.ducktape/<chain-id>/` — so the network is
    // addressable by `-n <chain-id>` (run/invite/list) from the moment it is
    // founded. the default dir is NAMED by the chain id, and the chain id is
    // minted from the identity pubkey, so the key is born in memory and only
    // persisted once the dir exists.
    let (dir, key, generated, chain_id) = match args.dir {
        Some(dir) => {
            std::fs::create_dir_all(&dir)?;
            let (key, generated) = config::load_or_generate_identity(&dir.join("identity.key"))?;
            let chain_id = config::mint_chain_id(name, &key.public_key());
            (dir, key, generated, chain_id)
        }
        None => {
            let key = config::generate_identity();
            let chain_id = config::mint_chain_id(name, &key.public_key());
            let dir = config::default_workspace_dir(&chain_id)?;
            std::fs::create_dir_all(&dir)?;
            config::write_identity(&dir.join("identity.key"), &key)?;
            (dir, key, true, chain_id)
        }
    };
    // re-running init would mint a FRESH chain-id and reset the validator set
    // to just this identity — silently un-founding the network under every
    // holder of an existing invite. founding is once per directory. (the
    // registry default cannot trip this: its dir is named by the fresh id.)
    let descriptor_path = dir.join("network.toml");
    if descriptor_path.exists() {
        return Err(format!(
            "{} already exists — this directory is already a network. use `invite`/`admit` \
             for membership, or delete the file to re-found from scratch",
            descriptor_path.display()
        )
        .into());
    }
    let net = &args.plumbing;
    let primary_coordinator =
        config::primary_coordinator_or_default(net.primary_coordinator.as_deref())?;
    // node.toml is COMPLETE: merged_plumbing pins the same compiled
    // coordinator default `apply_primary_coordinator` bakes into the
    // descriptor, so the two never silently disagree (see `docs`:
    // coordinator is ambient, node-local).
    let fresh_workspace = !dir.join("node.toml").exists();
    let mut plumbing = config::merged_plumbing(&dir, &net.overrides())?;
    // a FRESH workspace detects the platform runtime and writes the table (it
    // describes HOW runs are isolated, and grants nothing); an existing
    // node.toml keeps whatever the operator chose — a deleted table is never
    // resurrected. Turning the compute plane ON is `ducktape service enable
    // compute`, never an init flag.
    if fresh_workspace {
        plumbing.sandbox = detect_platform_sandbox(&dir);
    }

    // write the genesis the node boots from: the SAME bytes just hashed, and
    // the file every joiner is handed (a member by `join --genesis`, a
    // resident by its first boot's fetch).
    config::install_genesis(&dir, &genesis_hash, &hashes, &genesis_bytes)?;
    let mut modules = Vec::with_capacity(hashes.len());
    for (id, hash) in &hashes {
        // ids come from the topology today, but the descriptor codec's
        // delimiter rule is enforced at every entry point — this is one.
        config::validate_module_id(id)?;
        modules.push(config::ModuleCode {
            id: id.clone(),
            code_hash: hex_bytes(hash),
        });
    }

    let me = key.public_key();
    let mut descriptor = config::NetworkDescriptor {
        chain_id: chain_id.clone(),
        validators: vec![hex_bytes(me.as_ref())],
        bootstrap: Vec::new(),
        reach: Vec::new(),
        coordination: None,
        modules,
        genesis: hex_bytes(&genesis_hash),
        // the founding beat: stated once here, carried by the invite, and
        // inherited by every joiner — it is a genesis fact, not plumbing.
        block_time_ms: args.block_time_ms,
    };
    if let Some(addr) = config::dialable(Some(&plumbing.advertised), &plumbing.listen)? {
        descriptor.add_bootstrap(&me, &addr);
    }
    if let Some(coord) = &primary_coordinator {
        descriptor.apply_primary_coordinator(&me, coord)?;
    }
    descriptor.save(&descriptor_path)?;
    config::write_node_toml(&dir, &plumbing)?;
    record_founding_binary(&dir)?;
    eprintln!(
        "{} identity {}",
        if generated { "generated" } else { "reusing" },
        hex_bytes(me.as_ref())
    );
    eprintln!("network {chain_id} initialized in {}", dir.display());
    eprintln!(
        "genesis: {} components and {} index guests from {} written to {}",
        genesis.modules.len(),
        genesis
            .modules
            .iter()
            .filter(|module| genesis.index_guest(&module.id).is_some())
            .count(),
        founding_set.display(),
        config::genesis_path(&dir).display()
    );
    // a registry-default workspace is addressable by chain id; an explicit
    // --dir may live outside the registry, so its hints stay path-based.
    let selector = match explicit_dir {
        true => format!("--config {}/node.toml", dir.display()),
        false => format!("-n '{chain_id}'"),
    };
    eprintln!("start:  {}", launcher_start(&dir));
    eprintln!("invite: ducktape node invite {selector}");
    println!("{chain_id}");
    Ok(())
}

/// How every verb that brings a workspace into existence says to start it:
/// UNDER `ducktape-node-launcher`, which seeds the first release from this
/// very binary and then follows the network's node releases — the key they
/// are signed with included, which it pins from the network on first read. A
/// bare `ducktape node run` is the verb the launcher execs; started by hand it
/// follows no release, and no later node release can reach it.
///
/// Real paths: the launcher ships beside `ducktape` in every shape that ships
/// it (the node archive's root, a cargo target directory, an install), so it
/// is named there.
fn launcher_start(workspace: &std::path::Path) -> String {
    let this = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ducktape"));
    let launcher = this.with_file_name("ducktape-node-launcher");
    let (launcher, this) = (launcher.display(), this.display());
    let ws = workspace.display();
    let config = workspace.join("node.toml");
    let config = config.display();
    format!(
        "`'{launcher}' install --workspace '{ws}' --config '{config}' --from '{this}'` then \
         `'{launcher}' run --workspace '{ws}' --config '{config}'`"
    )
}

/// stamp the binary that just materialized `dir` into the workspace's founding
/// record — the identity `node run` refuses a disagreeing binary against
/// (`config::guard_founding_binary`). Written by the two verbs that BRING a
/// workspace into existence, `init` and `join`, and by nothing else.
fn record_founding_binary(dir: &std::path::Path) -> Result<(), String> {
    config::FoundingBinary {
        build: noded::services::build_identity_or_unknown().to_string(),
        module_world: wasm_host::module_world_digest().to_string(),
    }
    .save(dir)
}

/// install the genesis file a joiner was handed (`join --genesis <file>`) into
/// its workspace, verified against the joined descriptor first: the whole
/// file by the descriptor's pin, then every component by the descriptor's
/// hash. A MEMBER boots straight into genesis with no peer to fetch from, so
/// it needs the file at join; a resident may bring one to skip its first
/// boot's fetch.
fn install_joiner_genesis(
    dir: &std::path::Path,
    bytes: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let descriptor = config::NetworkDescriptor::load(&dir.join("network.toml"))?;
    let genesis = config::install_genesis(
        dir,
        &descriptor.genesis_hash()?,
        &descriptor.module_hashes()?,
        bytes,
    )?;
    eprintln!(
        "genesis: {} components and {} index guests written to {}",
        genesis.modules.len(),
        genesis
            .modules
            .iter()
            .filter(|module| genesis.index_guest(&module.id).is_some())
            .count(),
        config::genesis_path(dir).display()
    );
    Ok(())
}

/// `invite [--config node.toml] [--ttl-days N]` — emit the one-line paste
/// blob: the whole join credential. minting IS the admission decision — the
/// blob carries the descriptor with THIS member's dial hint folded in (and
/// persisted, so every future invite carries it), the inviter's WireGuard
/// bootstrap when the reachability plane is configured (`wireguard_listen`),
/// an expiry, and a single-use INVITE TOKEN, the whole envelope signed by
/// this member's identity. the joiner's node redeems the token automatically
/// (governance `Redeem`) — no member approval step follows. an invite grants
/// RESIDENT standing only; submitting ops needs no invite at all.
fn cmd_invite(args: InviteArgs) -> Result<(), Box<dyn std::error::Error>> {
    // every invite is BEARER (the targeted form was dropped): there is no
    // `--target` — whoever redeems the single-use token first wins. the
    // invite is the admission credential itself, kept off the wire by the
    // sealed first-contact intro. `--ttl-days` defaults to and is bounded by
    // `config::{DEFAULT_INVITE_TTL_DAYS, INVITE_TTL_DAYS}` in clap (the same
    // numbers `/v1/invite` resolves), so the value arrives settled.
    let cfg_path = args.selector.config_path()?;
    // the plane's standing lives in the RUNNING node, never in this process: a
    // CLI mint reads the same files the node owns and can see none of its
    // state. So ask it, before minting a credential nobody could redeem.
    if let Some(refusal) = mesh_refusal(&cfg_path) {
        return Err(refusal.into());
    }
    let (blob, notes) = mint_invite_blob(&cfg_path, args.ttl_days)?;
    // the blob first, the notes after it: a note is not a refusal, and an
    // operator who is handed one before the thing they asked for reads it as
    // the reason they did not get it. stdout is flushed between the two so a
    // redirected (block-buffered) stdout keeps that order too.
    println!("{blob}");
    std::io::Write::flush(&mut std::io::stdout())?;
    for note in notes {
        eprintln!("[invite] {note}");
    }
    Ok(())
}

/// The refusal a dead reachability plane earns a mint, on either side of the
/// node boundary: one sentence, so the daemon route and the CLI cannot say
/// different things about the same fact.
fn mesh_down_refusal(reason: &str, detail: &str) -> String {
    format!(
        "reason={reason} this node has no reachability plane, so an invite minted now could \
         never be redeemed — every path it would carry is dead before a joiner tries it. {detail}"
    )
}

/// Ask the node that owns `cfg_path` whether its mesh is up, over the same
/// unauthenticated `/v1/status` the app reads.
///
/// `None` means MINT: either the plane is fine, or NOTHING ANSWERED — a node
/// that is not running has no plane to be dead, and minting before boot is
/// ordinary. Only a node that answers and names a netstack failure refuses.
fn mesh_refusal(cfg_path: &std::path::Path) -> Option<String> {
    let base = config::http_base_in(cfg_path.parent()?).ok()?;
    let status = crate::node_http::get_json(&base, "/v1/status").ok()?;
    let netstack = status.get("operations")?.get("netstack")?;
    netstack_failure_in_section(netstack)
        .map(|(reason, detail)| mesh_down_refusal(&reason, &detail))
}

/// Why this node has no overlay, as its `operations.netstack` section says it
/// (`noded::NetstackOperationalStatus`) — `None` on a node whose plane is
/// starting, running or stopped. THE one reader of those two fields: `node
/// status` prints it and the invite mint refuses on it, and a second reader
/// would be a second answer to one question.
fn netstack_failure_in_section(netstack: &serde_json::Value) -> Option<(String, String)> {
    let reason = netstack.get("failure_reason")?.as_str()?.to_string();
    let detail = netstack
        .get("failure_detail")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some((reason, detail))
}

/// What the mint could not do, said once. A note is never a failure — an
/// invite with no member fronts still admits a joiner through the inviter's own
/// paths — but it changes what the blob can do, so it must reach SOMEBODY.
///
/// It rides back as a value rather than being printed here because this core
/// now has two callers with opposite output surfaces: a CLI whose diagnostics
/// are stderr, and a running daemon where `eprintln!` reaches neither the Logs
/// tab nor `RUST_LOG`.
pub(crate) enum InviteNote {
    /// mesh state exists but names no other member.
    MeshHasNoOtherMembers(std::path::PathBuf),
    /// this member has never persisted mesh state.
    NoMeshStateYet(std::path::PathBuf),
    /// mesh state is present and unreadable.
    MeshStateUnreadable(std::path::PathBuf, String),
    /// nothing in this config names a dialable underlay host, and no
    /// coordinator stands in for one: the blob admits a joiner on this
    /// machine and nowhere else.
    NotDialableOffBox,
}

impl InviteNote {
    /// the stable snake_case token a log line counts.
    pub(crate) fn reason(&self) -> &'static str {
        match self {
            InviteNote::MeshHasNoOtherMembers(_) => "invite_mesh_has_no_other_members",
            InviteNote::NoMeshStateYet(_) => "invite_no_mesh_state",
            InviteNote::MeshStateUnreadable(_, _) => "invite_mesh_state_unreadable",
            InviteNote::NotDialableOffBox => "invite_not_dialable_off_box",
        }
    }
}

impl std::fmt::Display for InviteNote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InviteNote::MeshHasNoOtherMembers(path) => write!(
                f,
                "persisted mesh at {} holds no other members — the invite carries only the \
                 inviter's own paths",
                path.display()
            ),
            InviteNote::NoMeshStateYet(path) => write!(
                f,
                "no persisted mesh state at {} — the invite carries no member fronts (only the \
                 inviter's own paths); mint again once the mesh has peers",
                path.display()
            ),
            InviteNote::MeshStateUnreadable(path, why) => write!(
                f,
                "mesh state at {} unreadable ({why}) — the invite carries no member fronts",
                path.display()
            ),
            InviteNote::NotDialableOffBox => write!(
                f,
                "this invite is reachable on this machine only — set `advertised` (or a \
                 concrete wireguard_listen IP) and mint again to invite over the network"
            ),
        }
    }
}

/// Mint one bearer invite from the workspace `cfg_path` names, answering the
/// paste blob and whatever the mint could not do.
///
/// Public to the crate because the RUNNING daemon mints too: `/v1/invite` is
/// wired to this at boot (see `boot`), so the desktop app asks the node that
/// owns these files instead of starting a second process to race it over them.
pub(crate) fn mint_invite_blob(
    cfg_path: &std::path::Path,
    ttl_days: u64,
) -> Result<(String, Vec<InviteNote>), Box<dyn std::error::Error>> {
    // A NOTE SAYS THE BLOB CARRIES FEWER PATHS; THIS SAYS IT CARRIES NONE. With
    // no reachability plane this member has no overlay at all — no tunnel, no
    // intro door — so every path the blob would offer is dead before a joiner
    // tries one, and the joiner spends ninety seconds finding that out. An
    // invite nobody can redeem is worse than a refusal.
    //
    // In-process only, which is exactly right: the daemon mints through
    // `/v1/invite` and holds the plane, and a CLI mint in a second process
    // asks the running node instead (`mesh_refusal`).
    if let Some((reason, detail)) = crate::reachability_plane::plane_failure() {
        return Err(mesh_down_refusal(reason, &detail).into());
    }
    // the expiry is settled FIRST: a TTL outside the range is refused before
    // the descriptor below is rewritten for a mint that was never going to happen.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock is past the epoch")
        .as_secs();
    let expires = config::invite_expiry(now, ttl_days)?;
    let mut notes = Vec::new();
    let cfg_path = cfg_path.to_path_buf();
    let (raw, base) = config::load_node_toml(&cfg_path)?;
    let descriptor_path = base.join(&raw.network);
    let mut descriptor = config::NetworkDescriptor::load(&descriptor_path)?;
    let key = config::load_identity(&base.join(&raw.key_file))?;
    let dial_hint = config::dialable(Some(&raw.advertised), &raw.listen)?;
    // the coordinator this member rendezvouses through rides the invite, so a
    // joiner registers where the network's members do — its own coordinator,
    // or none — instead of falling back to the compiled public default.
    let coordinator = config::primary_coordinator_or_default(Some(&raw.primary_coordinator))?;
    let descriptor_changed = match &dial_hint {
        Some(addr) => descriptor.add_bootstrap(&key.public_key(), addr),
        None => false,
    };
    if descriptor_changed {
        descriptor.save(&descriptor_path)?;
    }

    // the WireGuard bootstrap: endpoints are minted from the advertised host
    // (the listen IP is usually unspecified) + the plane's UDP ports; the
    // mesh port is where the joiner dials this member's overlay ULA once the
    // tunnel routes. the bootstrap is mandatory (the overlay plane
    // carries the data planes and the sealed first-contact intro) — and the
    // network shape always runs the plane (`wireguard_listen` is required).
    let wg_listen: std::net::SocketAddr = raw
        .wireguard_listen
        .parse()
        .map_err(|e| format!("wireguard_listen: {e}"))?;
    let wireguard = {
        let (wg_keypair, _) =
            reachability::WireGuardKeypair::load_or_generate(&base.join("wireguard.key"))
                .map_err(|e| format!("wireguard key: {e}"))?;
        let mesh_port: u16 = raw
            .listen
            .parse::<std::net::SocketAddr>()
            .map(|a| a.port())
            .map_err(|e| format!("listen {:?}: {e}", raw.listen))?;
        // a config that NAMES no dialable host mints an endpoint-less
        // bootstrap — never a refusal. Only a config that is WRONG still
        // aborts the mint (`endpoint_host`'s `Err`).
        let host = config::endpoint_host(
            Some(&raw.advertised),
            &raw.listen,
            wg_listen,
            raw.wireguard_advertised_value(),
        )?;
        // the tunnel endpoint carries the FULL advertised host:port when
        // `wireguard_advertised` is configured — the external port can
        // differ from the bind port in the port-forwarded setup the key
        // exists for. The intro stays host + intro port.
        let endpoint = config::invite_wireguard_endpoint(
            Some(&raw.advertised),
            &raw.listen,
            wg_listen,
            raw.wireguard_advertised_value(),
        )?;
        match host.zip(endpoint) {
            Some((host, endpoint)) => {
                let intro_port =
                    config::resolved_invite_listen(Some(&raw.invite_listen), wg_listen)?.port();
                config::InviteWireGuard {
                    public_key: wg_keypair.public_key().0,
                    endpoint: Some(endpoint),
                    intro: Some(format!("{host}:{intro_port}")),
                    mesh_port,
                }
            }
            None => {
                // the coordinator the blob carries gives the joiner a
                // rendezvous path, so an endpoint-less blob is still a
                // complete one; without it the joiner has nothing to dial from
                // another machine, and that is what the operator has to be
                // told.
                if coordinator.is_none() {
                    notes.push(InviteNote::NotDialableOffBox);
                }
                config::InviteWireGuard {
                    public_key: wg_keypair.public_key().0,
                    endpoint: None,
                    intro: None,
                    mesh_port,
                }
            }
        }
    };

    // the fronts: every reachable member the inviter already meshes with, read
    // from the persisted mesh state so a joiner can bring its tunnel up against
    // ANY of them, not just the inviter (the unified all-paths invite). A
    // host-capable member rides as a direct front, a NAT'd-but-registered one
    // as a coordinated (by-identity) front. No mesh state yet → no fronts.
    let storage = base.join(&raw.storage_dir);
    let mesh_state_file = storage.join("mesh-state.json");
    let chain_id = descriptor.genesis_namespace();
    let own: [u8; 32] = key
        .public_key()
        .as_ref()
        .try_into()
        .expect("ed25519 public key is 32 bytes");
    let fronts = match reachability::store::load(&mesh_state_file, &chain_id) {
        Ok(Some(mesh)) => {
            let fronts = config::fronts_from_adverts(&mesh.adverts, &own);
            if fronts.is_empty() {
                notes.push(InviteNote::MeshHasNoOtherMembers(mesh_state_file.clone()));
            }
            fronts
        }
        Ok(None) => {
            notes.push(InviteNote::NoMeshStateYet(mesh_state_file.clone()));
            Vec::new()
        }
        Err(e) => {
            notes.push(InviteNote::MeshStateUnreadable(
                mesh_state_file.clone(),
                e.to_string(),
            ));
            Vec::new()
        }
    };

    // the joiner reaches every path through ONE coordinator — its node.toml's,
    // seeded from the `coordinator` this blob carries — never a per-member
    // coordinator baked into the descriptor. Here we only strip Coordinated
    // reach hints from the ENCODED copy — the on-disk descriptor keeps its
    // config.
    let mut invite_descriptor = descriptor.clone();
    invite_descriptor
        .reach
        .retain(|hint| !hint.trim_start().starts_with("coordinated:"));

    // the expiry lives INSIDE the token (signed), not as a separate blob field.
    // every invite is bearer.
    let token = config::mint_invite_token(&key, descriptor.genesis_namespace().as_bytes(), expires);
    let blob_string = config::encode_invite(
        &invite_descriptor,
        &token,
        &wireguard,
        &fronts,
        coordinator.as_deref(),
        &key,
    )?;
    Ok((blob_string, notes))
}

/// `admit <hex pubkey> [--config node.toml]` — pre-genesis membership: add an
/// identity to the descriptor's validator set. once the network has state,
/// membership changes go through governance (AddValidator), not genesis edits.
fn cmd_admit(args: AdmitArgs) -> Result<(), Box<dyn std::error::Error>> {
    let pubkey_hex = &args.pubkey;
    let key = config::decode_key(pubkey_hex)?;
    let cfg_path = args.selector.config_path()?;
    let (raw, base) = config::load_node_toml(&cfg_path)?;
    let storage = base.join(&raw.storage_dir);
    if storage.exists() {
        return Err(format!(
            "{} already has state — a running network admits members via governance \
             (AddValidator), not by editing genesis",
            storage.display()
        )
        .into());
    }
    let descriptor_path = base.join(&raw.network);
    let mut descriptor = config::NetworkDescriptor::load(&descriptor_path)?;
    descriptor.admit(&key);
    descriptor.save(&descriptor_path)?;
    eprintln!("admitted {pubkey_hex} into {}", descriptor.chain_id);
    eprintln!(
        "re-run `ducktape node invite` and share the REFRESHED invite — genesis must be \
         identical on every member"
    );
    Ok(())
}

// ---- resident accept: post-genesis admission over the local rpc -----------

/// one blocking json-lines rpc round-trip against the LOCAL node.
pub(super) fn rpc_call(addr: &str, req: &serde_json::Value) -> Result<serde_json::Value, String> {
    use std::io::{BufRead as _, BufReader, Write as _};
    // the same calm sentence the http lane gives, for the same condition: an
    // `os error 111` with a port in it is a diagnosis nobody asked for. It is
    // reached the same way too — one renderer, asked which workspace this
    // address is, so every verb on this lane names a launcher when there is
    // one instead of recommending a second `node run` into its restart loop.
    let conn = std::net::TcpStream::connect(addr).map_err(|error| match error.kind() {
        std::io::ErrorKind::ConnectionRefused => {
            let workspace = crate::cli_args::workspace_for_rpc(addr).ok();
            crate::node_http::not_running_in(workspace.as_deref()).to_string()
        }
        _ => format!("cannot reach this node's operator rpc on {addr}: {error}"),
    })?;
    let read_timeout = crate::constants::RPC_CLIENT_READ_TIMEOUT;
    conn.set_read_timeout(Some(read_timeout))
        .map_err(|e| format!("rpc timeout: {e}"))?;
    let mut writer = conn.try_clone().map_err(|e| format!("rpc clone: {e}"))?;
    let mut line = serde_json::to_string(req).expect("rpc request serializes");
    line.push('\n');
    writer
        .write_all(line.as_bytes())
        .map_err(|e| format!("rpc write: {e}"))?;
    let mut reply = String::new();
    BufReader::new(conn)
        .read_line(&mut reply)
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => format!(
                "no answer from this node's operator rpc on {addr} in {} s",
                read_timeout.as_secs()
            ),
            _ => format!("rpc read: {error}"),
        })?;
    serde_json::from_str(reply.trim()).map_err(|e| format!("rpc reply: {e}"))
}

/// query a module through the rpc; the reply's hex payload, decoded.
pub(super) fn rpc_query(addr: &str, target: &str, req: &[u8]) -> Result<Vec<u8>, String> {
    let reply = rpc_call(
        addr,
        &serde_json::json!({ "cmd": "query", "target": target, "req_hex": hex_bytes(req) }),
    )?;
    if reply["ok"] != true {
        return Err(format!("query {target}: {}", reply["error"]));
    }
    unhex(
        reply["reply_hex"]
            .as_str()
            .ok_or("query reply carries no payload")?,
    )
}

/// submit an op through the rpc (accepted != finalized — poll afterwards).
fn rpc_submit(addr: &str, target: &str, payload: &[u8]) -> Result<(), String> {
    let reply = rpc_call(
        addr,
        &serde_json::json!({ "cmd": "submit", "target": target, "payload_hex": hex_bytes(payload) }),
    )?;
    if reply["ok"] != true {
        return Err(format!("submit to {target}: {}", reply["error"]));
    }
    Ok(())
}

pub(super) fn read_members(addr: &str) -> Result<Vec<Vec<u8>>, String> {
    use valset::{ValsetQuery, ValsetReply, decode_reply, encode_query};
    let raw = rpc_query(addr, "valset", &encode_query(&ValsetQuery::Validators))?;
    match decode_reply(&raw)? {
        ValsetReply::Validators(v) => Ok(v),
        other => Err(format!("expected Validators, got {other:?}")),
    }
}

fn read_residents(addr: &str) -> Result<Vec<Vec<u8>>, String> {
    use valset::{ValsetQuery, ValsetReply, decode_reply, encode_query};
    let raw = rpc_query(addr, "valset", &encode_query(&ValsetQuery::Residents))?;
    match decode_reply(&raw)? {
        ValsetReply::Residents(v) => Ok(v),
        other => Err(format!("expected Residents, got {other:?}")),
    }
}

/// the account number `key` belongs to, if any — through `OfKey`, the one
/// resolver (a node key is never on an account).
fn account_of_key(addr: &str, key: &[u8]) -> Result<Option<u64>, String> {
    use identity::{IdentityQuery, IdentityReply, decode_reply, encode_query};
    let raw = rpc_query(
        addr,
        "identity",
        &encode_query(&IdentityQuery::OfKey { key: key.to_vec() }),
    )?;
    match decode_reply(&raw)? {
        IdentityReply::Account(account) => Ok(account.map(|account| account.number)),
        IdentityReply::Accounts(_) | IdentityReply::Resolved(_) | IdentityReply::Gen(_) => {
            Err("expected an Account reply from identity".into())
        }
    }
}

fn read_shares(addr: &str) -> Result<governance::SharesView, String> {
    use governance::{GovQuery, GovReply, decode_reply, encode_query};
    let raw = rpc_query(addr, "governance", &encode_query(&GovQuery::Shares))?;
    match decode_reply(&raw)? {
        GovReply::Shares(view) => Ok(view),
        other => Err(format!("expected Shares, got {other:?}")),
    }
}

/// WHO signs this node's governance ops, decided ONCE per ceremony from the
/// mode the module is in. A validator-mode ballot is the node key's, over the
/// local rpc (the node re-signs). A share-mode ballot is the active user key's
/// ACCOUNT, and only a user-signed frame can carry that origin — so the key is
/// unlocked here (password on stdin) and its ops go over the node's http lane
/// as frames.
// one per ceremony, on the stack — variant size is noise.
#[allow(clippy::large_enum_variant)]
pub(super) enum GovSigner {
    Node {
        key: Vec<u8>,
    },
    User {
        key: commonware_cryptography::ed25519::PrivateKey,
        principal: Vec<u8>,
        http_base: String,
    },
}

impl GovSigner {
    /// the electorate kind this signer's ballots count under.
    fn kind(&self) -> governance::VoterKind {
        match self {
            GovSigner::Node { .. } => governance::VoterKind::ValidatorNode,
            GovSigner::User { .. } => governance::VoterKind::Account,
        }
    }

    /// the principal a proposal records this signer's ballot under.
    fn principal(&self) -> &[u8] {
        match self {
            GovSigner::Node { key } => key,
            GovSigner::User { principal, .. } => principal,
        }
    }

    /// submit one governance op through this signer's lane (accepted !=
    /// finalized on the rpc lane — callers poll afterwards).
    fn submit(&self, rpc_addr: &str, msg: &governance::GovMsg) -> Result<(), String> {
        let payload = governance::encode_msg(msg);
        match self {
            GovSigner::Node { .. } => rpc_submit(rpc_addr, "governance", &payload),
            GovSigner::User { key, http_base, .. } => crate::node_http::submit_frame(
                http_base,
                &crate::userkey_cli::user_frame(key, "governance", payload),
            )
            .map(|_height| ())
            .map_err(|e| e.to_string()),
        }
    }
}

/// resolve the signer for a ceremony on the node at `cfg_path`.
pub(super) fn gov_signer(
    rpc_addr: &str,
    cfg_path: &std::path::Path,
    resolved: &config::Resolved,
) -> Result<GovSigner, Box<dyn std::error::Error>> {
    let shares_govern = read_shares(rpc_addr)?.active;
    if !shares_govern {
        return Ok(GovSigner::Node {
            key: resolved.signer.public_key().as_ref().to_vec(),
        });
    }
    let workspace = cfg_path.parent().unwrap_or(std::path::Path::new("."));
    let http_base = config::http_base_in(workspace)?;
    let mut stdin = std::io::BufReader::new(std::io::stdin());
    let key = crate::userkey_cli::load_user_signer(
        &keystore::wallet::active_user_key(workspace)?,
        &mut stdin,
    )?;
    let number = account_of_key(rpc_addr, key.public_key().as_ref())?.ok_or(
        "shares govern this network and the active user key belongs to no Identity account — \
         `ducktape account create` first",
    )?;
    Ok(GovSigner::User {
        key,
        principal: identity::account_principal(number),
        http_base,
    })
}

/// a proposal is decided by the mode frozen when it was opened; a ballot from
/// the other mode's signer would be refused by the module, so say so here.
fn require_frozen_kind(
    proposal: &governance::ProposalView,
    signer: &GovSigner,
) -> Result<(), String> {
    let frozen_kind_matches = proposal.voter_kind == signer.kind();
    if frozen_kind_matches {
        return Ok(());
    }
    Err(format!(
        "proposal {} was frozen for {:?} ballots, but this node now governs by {:?} — it cannot vote on it",
        proposal.proposal_id,
        proposal.voter_kind,
        signer.kind()
    ))
}

fn proposal_progress(proposal: &governance::ProposalView, members: &[Vec<u8>]) -> (u64, u64, bool) {
    let powers: std::collections::BTreeMap<&[u8], u64> = if proposal.electorate.is_empty() {
        members
            .iter()
            .map(|member| (member.as_slice(), 1))
            .collect()
    } else {
        proposal
            .electorate
            .iter()
            .map(|(principal, power)| (principal.as_slice(), *power))
            .collect()
    };
    let mut yes = 0u64;
    let mut no = 0u64;
    for (voter, approve) in &proposal.votes {
        let power = powers.get(voter.as_slice()).copied().unwrap_or(0);
        if *approve {
            yes += power;
        } else {
            no += power;
        }
    }
    let total: u64 = powers.values().sum();
    match proposal.voting_rule {
        governance::VotingRule::Threshold { required_yes } => {
            (yes, required_yes, yes >= required_yes)
        }
        governance::VotingRule::ParticipatingMajority { quorum } => {
            let ready = yes + no >= quorum && yes > total - yes;
            (yes, quorum, ready)
        }
    }
}

/// `join requests [--config node.toml]` — the verified join announces parked
/// joiners delivered to THIS member's running node, as one JSON array on
/// stdout (machine-parseable — the app's members view renders it). approving
/// is a separate, deliberate act: `resident accept <joiner>` (or the app's
/// approve button) casts this account's governance ballot; the proposal's
/// frozen rule decides admission.
fn cmd_join_requests(args: SelectorArgs) -> Result<(), Box<dyn std::error::Error>> {
    let cfg_path = args.selector.config_path()?;
    let resolved = config::resolve(&cfg_path)?;
    let addr = resolved
        .rpc_listen
        .ok_or("join requests reads the node's local rpc — set `rpc_listen` in node.toml")?;
    let reply = rpc_call(&addr, &serde_json::json!({ "cmd": "join_requests" }))?;
    if reply["ok"] != true {
        return Err(format!("join requests: {}", reply["error"]).into());
    }
    println!(
        "{}",
        reply
            .get("join_requests")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([]))
    );
    Ok(())
}

/// `join state [--config node.toml]` — the node's AUTHORITATIVE onboarding
/// phase over its local rpc: `parked | admitted | synced | promoted`, derived
/// from committed standing (not log markers), so it is restart-proof. the
/// desktop app reads this instead of parsing daemon.log, which loses the
/// admission markers across a restart and mis-reads a re-syncing resident as
/// unjoined. prints the `join_state` projection as JSON.
fn cmd_join_state(args: SelectorArgs) -> Result<(), Box<dyn std::error::Error>> {
    let cfg_path = args.selector.config_path()?;
    let resolved = config::resolve(&cfg_path)?;
    let addr = resolved
        .rpc_listen
        .ok_or("join state reads the node's local rpc — set `rpc_listen` in node.toml")?;
    let reply = rpc_call(&addr, &serde_json::json!({ "cmd": "join_state" }))?;
    if reply["ok"] != true {
        return Err(format!("join state: {}", reply["error"]).into());
    }
    println!(
        "{}",
        reply
            .get("join_state")
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    );
    Ok(())
}

fn read_proposal(addr: &str, id: &str) -> Result<Option<governance::ProposalView>, String> {
    use governance::{GovQuery, GovReply, decode_reply, encode_query};
    let raw = rpc_query(
        addr,
        "governance",
        &encode_query(&GovQuery::Proposal {
            proposal_id: id.into(),
        }),
    )?;
    match decode_reply(&raw)? {
        GovReply::Proposal(view) => Ok(view),
        other => Err(format!("unexpected governance reply: {other:?}")),
    }
}

/// the ceremony's failure when an executed proposal never leaves `Open`: the
/// target module refused the action inside governance's `Execute` op, so the
/// op was rejected whole. named so a verb can recognise it and say what the
/// target's rules are.
pub(super) const TALLY_SETTLE_TIMEOUT: &str = "timed out waiting for the tally to settle";

/// the running node a ceremony drives. reads and writes go over its operator
/// rpc; its http surface stages bytes and, over ws, announces every block
/// wake — the moment committed state may have changed, and the only clock a
/// ceremony waits on.
pub(super) struct DrivenNode {
    rpc: String,
    http_base: String,
}

impl DrivenNode {
    /// both surfaces of the node `resolved` describes; `verb` names the
    /// ceremony in the refusal when the config leaves one unset.
    pub(super) fn of(resolved: &config::Resolved, verb: &str) -> Result<Self, String> {
        let rpc = resolved.rpc_listen.clone().ok_or_else(|| {
            format!("{verb} drives the node's local rpc — set `rpc_listen` in node.toml")
        })?;
        let http_listen = resolved.service.http_listen.as_deref().ok_or_else(|| {
            format!("{verb} waits on the node's block feed — set `http_listen` in node.toml")
        })?;
        Ok(Self {
            rpc,
            http_base: config::http_base_of(http_listen),
        })
    }

    pub(super) fn rpc(&self) -> &str {
        &self.rpc
    }

    pub(super) fn http_base(&self) -> &str {
        &self.http_base
    }

    /// attach to the node's block wakes — BEFORE the read they guard, so a
    /// block landing between the two is buffered on the socket, never missed.
    fn block_wakes(&self, budget: std::time::Duration) -> Result<BlockWakes, String> {
        use tokio_tungstenite::tungstenite::stream::MaybeTlsStream;
        let url = crate::agent_cli::ws_url(&self.http_base);
        let (socket, _response) = tokio_tungstenite::tungstenite::connect(&url)
            .map_err(|e| format!("attach to the node's block feed: {e}"))?;
        // the failure path's bound, not a poll interval: the node heartbeats
        // on an interval as well as per block, so a live node never lets it
        // expire.
        if let MaybeTlsStream::Plain(tcp) = socket.get_ref() {
            tcp.set_read_timeout(Some(budget))
                .map_err(|e| format!("bound the node's block feed: {e}"))?;
        }
        Ok(BlockWakes { socket })
    }
}

/// one node's block wakes, in order.
struct BlockWakes {
    socket: tokio_tungstenite::tungstenite::WebSocket<
        tokio_tungstenite::tungstenite::stream::MaybeTlsStream<std::net::TcpStream>,
    >,
}

impl BlockWakes {
    /// block until the node reports its next block wake.
    fn next(&mut self) -> Result<(), String> {
        use tokio_tungstenite::tungstenite::Message;
        loop {
            let frame = self
                .socket
                .read()
                .map_err(|e| format!("the node's block feed closed: {e}"))?;
            // an unsubscribed connection carries heartbeats and nothing else,
            // but a control frame still has to be stepped over.
            let Message::Text(text) = frame else { continue };
            let woke = serde_json::from_str::<serde_json::Value>(&text)
                .is_ok_and(|frame| frame["type"] == "heartbeat");
            if woke {
                return Ok(());
            }
        }
    }
}

/// how long a ceremony waits on one proposal transition before naming the
/// failure: ops finalize within a few blocks; the budget covers a mesh still
/// forming quorum.
const PROPOSAL_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// re-read a proposal on every block wake until `pred` accepts its view.
/// `timed_out` is the whole failure sentence once [`PROPOSAL_BUDGET`] passes.
///
/// the wake is the only clock here on purpose: a validator this ceremony
/// removes halts a fixed few views after the change commits, and a read that
/// rides the settling block's wake lands inside that margin at any block
/// time, where a wall-clock poll need not.
fn await_proposal(
    node: &DrivenNode,
    id: &str,
    timed_out: &str,
    mut pred: impl FnMut(&Option<governance::ProposalView>) -> bool,
) -> Result<Option<governance::ProposalView>, String> {
    let deadline = std::time::Instant::now() + PROPOSAL_BUDGET;
    let mut wakes = node.block_wakes(PROPOSAL_BUDGET)?;
    loop {
        let view = read_proposal(node.rpc(), id)?;
        if pred(&view) {
            return Ok(view);
        }
        if std::time::Instant::now() >= deadline {
            return Err(timed_out.to_string());
        }
        wakes.next()?;
    }
}

fn cast_yes_once(
    node: &DrivenNode,
    proposal_id: &str,
    opened: governance::ProposalView,
    signer: &GovSigner,
) -> Result<governance::ProposalView, String> {
    use governance::{GovMsg, ProposalStatus};

    if opened.status != ProposalStatus::Open {
        return Ok(opened);
    }
    require_frozen_kind(&opened, signer)?;
    let principal = signer.principal();
    if opened
        .votes
        .iter()
        .any(|(voter, yes)| voter == principal && *yes)
    {
        eprintln!("ballot already cast as {}", hex_bytes(principal));
        return Ok(opened);
    }
    signer.submit(
        node.rpc(),
        &GovMsg::Vote {
            proposal_id: proposal_id.into(),
            approve: true,
        },
    )?;
    let proposal = await_proposal(
        node,
        proposal_id,
        "timed out waiting for this ballot to finalize",
        |p| {
            p.as_ref().is_some_and(|proposal| {
                proposal.status != ProposalStatus::Open
                    || proposal
                        .votes
                        .iter()
                        .any(|(voter, yes)| voter == principal && *yes)
            })
        },
    )?
    .ok_or_else(|| format!("proposal {proposal_id} disappeared"))?;
    eprintln!("ballot cast as {}", hex_bytes(principal));
    Ok(proposal)
}

/// how a driven ceremony left the proposal.
pub(super) enum CeremonyOutcome {
    /// passed and executed. what that execution CHANGED is the caller's to
    /// confirm: a membership set turns over at the next epoch cutover, a
    /// module swap lands in the modules registry.
    Passed,
    /// this ballot landed but the proposal's frozen threshold is outstanding.
    AwaitingBallots,
}

/// the open proposal a member should JOIN rather than duplicate. the matcher
/// decides which fields identify "the same proposal": membership verbs match
/// the whole action, module verbs match (variant, module_id, code_hash) and
/// ignore the activation height each member computed for itself.
pub(super) fn open_proposal_matching<'a>(
    views: &'a [governance::ProposalView],
    matches: &dyn Fn(&governance::GovAction) -> bool,
) -> Option<&'a governance::ProposalView> {
    views
        .iter()
        .find(|p| p.status == governance::ProposalStatus::Open && matches(&p.action))
}

/// drive a governance proposal ceremony for `wanted` through this eligible
/// account's running node: adopt an existing OPEN proposal `matches` accepts
/// (else mint an unused `<id_prefix><id_seed>:<n>` id and propose), cast a yes
/// ballot, and execute once decidable. idempotent across
/// members — each runs the same verb; the run landing the deciding ballot
/// executes. shared by the membership verbs — `resident accept`
/// (AddResident), `member promote` (AddValidator), `resident remove`
/// (RemoveResident) — the module verbs `module update`/`module register`
/// (UpdateModule/RegisterModule), and `release schedule` (Signal).
///
/// `id_seed` is what keeps two members minting at the same instant off each
/// other's id: the proposer's own key for a per-member verb. A ceremony whose
/// settled proposal must be FOUND AGAIN by id from any node — the node
/// release designation, which every launcher reads back — passes an EMPTY
/// seed instead, so the id space is `<id_prefix>:<n>` and a reader can walk
/// it. A concurrent second proposer then simply mints `:<n+1>` for the same
/// decision rather than colliding.
#[allow(clippy::too_many_arguments)]
pub(super) fn drive_proposal_ceremony(
    node: &DrivenNode,
    signer: &GovSigner,
    pubkey_hex: &str,
    id_seed: &str,
    verb: &str,
    id_prefix: &str,
    wanted: governance::GovAction,
    matches: &dyn Fn(&governance::GovAction) -> bool,
) -> Result<CeremonyOutcome, Box<dyn std::error::Error>> {
    // a matcher that rejects its own action would make every member mint a
    // fresh proposal that no one else joins — the exact failure the matcher
    // exists to prevent.
    debug_assert!(
        matches(&wanted),
        "the matcher must accept the action it proposes"
    );
    use governance::{GovMsg, ProposalStatus};
    use governance::{GovQuery, GovReply, decode_reply, encode_query};
    let proposals = match decode_reply(&rpc_query(
        node.rpc(),
        "governance",
        &encode_query(&GovQuery::Proposals),
    )?)? {
        GovReply::Proposals(views) => views,
        other => return Err(format!("unexpected governance reply: {other:?}").into()),
    };
    let proposal_id = match open_proposal_matching(&proposals, matches) {
        Some(p) => {
            eprintln!("joining open proposal {}", p.proposal_id);
            p.proposal_id.clone()
        }
        None => {
            let prefix: String = id_seed.chars().take(16).collect();
            // MINT AGAINST THE RECORD, not the roster. `GovQuery::Proposals`
            // walks the OPEN roster, but a settled proposal's record is kept
            // forever under its id — so an id missing from that list can still
            // be taken by an earlier ceremony for the same key. Reusing one
            // makes every wait below adopt that stale record: `await_proposal`
            // sees it the instant it is asked, `cast_yes_once` returns early on
            // its settled status, `Execute` is skipped, and the verb reports
            // the PREVIOUS ceremony's outcome while this one votes on nothing
            // (a re-grant that silently changes no state — #1766).
            let id = (0u64..)
                .map(|n| format!("{id_prefix}{prefix}:{n}"))
                .find_map(|candidate| match read_proposal(node.rpc(), &candidate) {
                    Ok(None) => Some(Ok(candidate)),
                    Ok(Some(_)) => None,
                    Err(e) => Some(Err(e)),
                })
                .expect("the id space is unbounded")?;
            signer.submit(
                node.rpc(),
                &GovMsg::Propose {
                    proposal_id: id.clone(),
                    action: wanted,
                    // a far horizon in consensus-time units (heights advance
                    // about one per finalized op): admission must not expire
                    // under a slow second ballot.
                    voting_period: 1_000_000,
                },
            )?;
            await_proposal(
                node,
                &id,
                "timed out waiting for the proposal to finalize",
                |p| p.is_some(),
            )?;
            eprintln!("proposed {id}");
            id
        }
    };

    let opened = read_proposal(node.rpc(), &proposal_id)?
        .ok_or_else(|| format!("proposal {proposal_id} disappeared"))?;
    let after_vote = cast_yes_once(node, &proposal_id, opened, signer)?;

    // Execute only when the proposal's frozen rule says the yes power is
    // irreversible. A shortfall is the normal intermediate state, not an error.
    let members = read_members(node.rpc())?;
    let (yes, required, ready) = proposal_progress(&after_vote, &members);
    if after_vote.status == ProposalStatus::Open && !ready {
        eprintln!(
            "{yes} of {required} required voting power — waiting on other voters. each runs:\n    \
             ducktape {verb} {pubkey_hex} --config <their node.toml>"
        );
        // `verb` is the full two-token spelling (`node resident accept`, ...)
        // so the guidance reads `ducktape node resident accept <hex>`.
        return Ok(CeremonyOutcome::AwaitingBallots);
    }
    if after_vote.status == ProposalStatus::Open {
        signer.submit(
            node.rpc(),
            &GovMsg::Execute {
                proposal_id: proposal_id.clone(),
            },
        )?;
    }
    let settled = await_proposal(node, &proposal_id, TALLY_SETTLE_TIMEOUT, |p| {
        p.as_ref().is_some_and(|v| v.status != ProposalStatus::Open)
    })?
    .expect("the poll only accepts a present proposal");
    match settled.status {
        ProposalStatus::Passed => Ok(CeremonyOutcome::Passed),
        status => Err(format!("proposal {proposal_id} settled as {status:?}").into()),
    }
}

/// `resident accept <hex pubkey> [--config node.toml]` — approve a join request
/// as RESIDENT standing (the staged-admission tier): drive a governance
/// AddResident proposal for `pubkey` through this account's own RUNNING node.
/// the passing proposal's valset Grant schedules the epoch cutover that
/// admits the key to the mesh, at which point its parked node PRE-SYNCS
/// state on a stride cadence. promotion into the quorum is the separate,
/// deliberate `member promote` verb — run it once the resident is warm.
fn cmd_invite_accept(args: PubkeyArgs) -> Result<(), Box<dyn std::error::Error>> {
    use governance::GovAction;

    let pubkey_hex = &args.pubkey;
    let key = config::decode_key(pubkey_hex)?;
    let key_bytes = key.as_ref().to_vec();
    let cfg_path = args.selector.config_path()?;
    // Full config resolution derives the same node identity the daemon signs
    // with; governance resolves it to an account when shares are active.
    let resolved = config::resolve(&cfg_path)?;
    let node = DrivenNode::of(&resolved, "resident accept")?;
    let signer = gov_signer(node.rpc(), &cfg_path, &resolved)?;

    let members = read_members(node.rpc())?;
    if members.contains(&key_bytes) {
        eprintln!("{pubkey_hex} is already a validator — nothing to do");
        return Ok(());
    }
    if read_residents(node.rpc())?.contains(&key_bytes) {
        eprintln!(
            "{pubkey_hex} already holds resident standing — promote with \
             `ducktape node member promote {pubkey_hex}` once it is synced"
        );
        return Ok(());
    }
    let wanted = GovAction::AddResident { key: key_bytes };
    let same_action = {
        let wanted = wanted.clone();
        move |a: &GovAction| *a == wanted
    };
    match drive_proposal_ceremony(
        &node,
        &signer,
        pubkey_hex,
        pubkey_hex,
        "node resident accept",
        "resident:",
        wanted,
        &same_action,
    )? {
        CeremonyOutcome::Passed => {
            eprintln!(
                "granted resident standing to {pubkey_hex}: the mesh admits it at the next \
                 epoch cutover and its parked node pre-syncs state. promote it into the \
                 quorum once warm:\n    ducktape node member promote {pubkey_hex}"
            );
            Ok(())
        }
        CeremonyOutcome::AwaitingBallots => Ok(()),
    }
}

/// What `member promote` decides before it proposes anything.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Promotion {
    /// the key already holds the seat — an idempotent re-run.
    AlreadySeated,
    /// it stands in the resident tier: propose the promotion.
    Proceed,
}

/// Governance refuses a promotion of a key with no resident record, and it
/// refuses it at APPLY — inside a block, long after `/v1/submit/frame`
/// answered — so the refusal reaches the daemon log (`reason=not_a_resident`)
/// and never the terminal: the ceremony just polls for a proposal that will
/// never exist and reports its own deadline, thirty seconds later, saying
/// nothing about why.
///
/// So read the rosters and say it here, before anything is submitted, the way
/// [`crate::module_cli`]'s own precheck reads the registry before proposing.
/// This is the SENTENCE and never the gate: a CLI check is walked past by the
/// app, by a script and by `curl` on `/v1`, and consensus owns the safety
/// property.
pub(super) fn precheck_promotion(
    pubkey_hex: &str,
    key: &[u8],
    members: &[Vec<u8>],
    residents: &[Vec<u8>],
) -> Result<Promotion, String> {
    let already_seated = members.iter().any(|m| m == key);
    if already_seated {
        return Ok(Promotion::AlreadySeated);
    }
    let stands_for_promotion = residents.iter().any(|r| r == key);
    if stands_for_promotion {
        return Ok(Promotion::Proceed);
    }
    Err(format!(
        "not_a_resident: {pubkey_hex} holds no resident standing, and a validator is promoted \
         out of that tier — grant it first with `ducktape node resident accept {pubkey_hex}`, \
         then promote it once its node is synced"
    ))
}

/// `member promote <hex pubkey> [--config node.toml]` — seat a key in the
/// consensus quorum: drive a governance AddValidator proposal through this
/// account's own RUNNING node. the passing proposal's valset Join clears the
/// key's resident standing in the same block and schedules the epoch cutover; a
/// pre-synced resident then catches up a small delta and reboots as a
/// validator, so the quorum only ever gains a warm member. the resident tier is
/// the only way in: consensus refuses a promotion of a key it has never met,
/// and [`precheck_promotion`] says so before this verb submits anything.
fn cmd_promote(args: PubkeyArgs) -> Result<(), Box<dyn std::error::Error>> {
    use governance::GovAction;

    let pubkey_hex = &args.pubkey;
    let key = config::decode_key(pubkey_hex)?;
    let key_bytes = key.as_ref().to_vec();
    let cfg_path = args.selector.config_path()?;
    let resolved = config::resolve(&cfg_path)?;
    let node = DrivenNode::of(&resolved, "promote")?;
    let signer = gov_signer(node.rpc(), &cfg_path, &resolved)?;

    let members = read_members(node.rpc())?;
    let residents = read_residents(node.rpc())?;
    match precheck_promotion(pubkey_hex, &key_bytes, &members, &residents)? {
        Promotion::AlreadySeated => {
            eprintln!("{pubkey_hex} is already a validator — nothing to do");
            return Ok(());
        }
        Promotion::Proceed => {}
    }
    let wanted = GovAction::AddValidator { key: key_bytes };
    let same_action = {
        let wanted = wanted.clone();
        move |a: &GovAction| *a == wanted
    };
    match drive_proposal_ceremony(
        &node,
        &signer,
        pubkey_hex,
        pubkey_hex,
        "node member promote",
        "admit:",
        wanted,
        &same_action,
    )? {
        CeremonyOutcome::Passed => {
            eprintln!(
                "admitted {pubkey_hex}: the next epoch cutover seats it in the consensus \
                 quorum, and the chain PAUSES at that cutover until its node seats itself \
                 and votes. a warm (pre-synced) resident seats in-process from its own \
                 folded state within moments; a cold node first syncs the frozen boundary. \
                 watch its log for `promoted: validator at epoch …; seating in-process`"
            );
            Ok(())
        }
        CeremonyOutcome::AwaitingBallots => Ok(()),
    }
}

/// What `resident remove` decides before it proposes anything.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ResidentRemoval {
    /// the key holds no resident standing — the desired state already holds.
    NoStanding,
    /// it stands in the resident tier: propose the revocation.
    Proceed,
}

/// [`precheck_promotion`]'s mirror. A seated validator is REFUSED, not a no-op:
/// nothing is removed and the operator is sent to another verb, so a script
/// must not read success — unlike a key with no standing at all, where the
/// state asked for already holds.
pub(super) fn precheck_resident_removal(
    pubkey_hex: &str,
    key: &[u8],
    members: &[Vec<u8>],
    residents: &[Vec<u8>],
) -> Result<ResidentRemoval, String> {
    let seated = members.iter().any(|m| m == key);
    if seated {
        return Err(format!(
            "{pubkey_hex} is a seated validator, not a resident — remove it with \
             `ducktape node member remove {pubkey_hex}`"
        ));
    }
    let resident = residents.iter().any(|r| r == key);
    match resident {
        true => Ok(ResidentRemoval::Proceed),
        false => Ok(ResidentRemoval::NoStanding),
    }
}

/// `resident remove <hex pubkey> [--config node.toml]` — revoke resident
/// standing: drive a governance RemoveResident proposal through this account's
/// own RUNNING node. the mirror of `resident accept` with inverted guards — a
/// no-op when the key holds no resident standing, and only the governance
/// electorate may drive it. the passing proposal's valset Revoke schedules the
/// epoch cutover that drops the key from the mesh; its node falls back to a
/// parked joiner, and `resident accept` re-grants. a seated validator is
/// `member remove`'s job — standing never overlaps (Grant refuses validators,
/// Join clears standing).
fn cmd_resident_remove(args: PubkeyArgs) -> Result<(), Box<dyn std::error::Error>> {
    use governance::GovAction;

    let pubkey_hex = &args.pubkey;
    let key = config::decode_key(pubkey_hex)?;
    let key_bytes = key.as_ref().to_vec();
    let cfg_path = args.selector.config_path()?;
    // Full config resolution derives the same node identity the daemon signs
    // with; governance resolves it to an account when shares are active.
    let resolved = config::resolve(&cfg_path)?;
    let node = DrivenNode::of(&resolved, "resident remove")?;
    let signer = gov_signer(node.rpc(), &cfg_path, &resolved)?;

    let members = read_members(node.rpc())?;
    let residents = read_residents(node.rpc())?;
    match precheck_resident_removal(pubkey_hex, &key_bytes, &members, &residents)? {
        ResidentRemoval::NoStanding => {
            eprintln!("{pubkey_hex} holds no resident standing — nothing to do");
            return Ok(());
        }
        ResidentRemoval::Proceed => {}
    }
    let wanted = GovAction::RemoveResident { key: key_bytes };
    let same_action = {
        let wanted = wanted.clone();
        move |a: &GovAction| *a == wanted
    };
    match drive_proposal_ceremony(
        &node,
        &signer,
        pubkey_hex,
        pubkey_hex,
        "node resident remove",
        "revoke:",
        wanted,
        &same_action,
    )? {
        CeremonyOutcome::Passed => {
            eprintln!(
                "revoked resident standing from {pubkey_hex}: the mesh drops it at the next \
                 epoch cutover and its node parks again. a member re-grants with:\n    \
                 ducktape node resident accept {pubkey_hex}"
            );
            Ok(())
        }
        CeremonyOutcome::AwaitingBallots => Ok(()),
    }
}

// ---- member remove: post-genesis removal over the local rpc ---------------

/// `member remove <hex pubkey> [--config node.toml]` — post-genesis removal:
/// drive a governance RemoveValidator proposal for `pubkey` through this
/// account's own RUNNING node. the mirror of `resident accept` with inverted
/// guards — a no-op when the key is NOT a member, and only the governance
/// electorate may drive it. idempotent across voters: each runs the same
/// command (propose if absent, cast a yes ballot, execute once decidable); the
/// run that lands the deciding ballot executes. the passing proposal's valset
/// Leave schedules the epoch cutover that drops the key from the tracked set.
fn cmd_member_remove(args: PubkeyArgs) -> Result<(), Box<dyn std::error::Error>> {
    use governance::{GovAction, GovMsg, ProposalStatus};

    let pubkey_hex = &args.pubkey;
    let key = config::decode_key(pubkey_hex)?;
    let key_bytes = key.as_ref().to_vec();
    let cfg_path = args.selector.config_path()?;
    // Full config resolution derives the same node identity the daemon signs
    // with; governance resolves it to an account when shares are active.
    let resolved = config::resolve(&cfg_path)?;
    let node = DrivenNode::of(&resolved, "member remove")?;
    let signer = gov_signer(node.rpc(), &cfg_path, &resolved)?;

    let members = read_members(node.rpc())?;
    // Inverted admission guard: nothing to remove if the key is not a member.
    if !members.contains(&key_bytes) {
        eprintln!("{pubkey_hex} is not a validator — nothing to do");
        return Ok(());
    }
    // adopt an existing OPEN proposal for exactly this action, else mint an
    // unused id (settled proposals keep their ids forever — a re-removed key
    // gets a fresh suffix).
    use governance::{GovQuery, GovReply, decode_reply, encode_query};
    let proposals = match decode_reply(&rpc_query(
        node.rpc(),
        "governance",
        &encode_query(&GovQuery::Proposals),
    )?)? {
        GovReply::Proposals(views) => views,
        other => return Err(format!("unexpected governance reply: {other:?}").into()),
    };
    let wanted = GovAction::RemoveValidator {
        key: key_bytes.clone(),
    };
    let proposal_id = match proposals
        .iter()
        .find(|p| p.status == ProposalStatus::Open && p.action == wanted)
    {
        Some(p) => {
            eprintln!("joining open proposal {}", p.proposal_id);
            p.proposal_id.clone()
        }
        None => {
            let prefix: String = pubkey_hex.chars().take(16).collect();
            let id = (0u64..)
                .map(|n| format!("remove:{prefix}:{n}"))
                .find(|id| !proposals.iter().any(|p| &p.proposal_id == id))
                .expect("the id space is unbounded");
            signer.submit(
                node.rpc(),
                &GovMsg::Propose {
                    proposal_id: id.clone(),
                    action: wanted,
                    // a far horizon in consensus-time units: removal must not
                    // expire under a slow second ballot.
                    voting_period: 1_000_000,
                },
            )?;
            await_proposal(
                &node,
                &id,
                "timed out waiting for the proposal to finalize",
                |p| p.is_some(),
            )?;
            eprintln!("proposed {id}");
            id
        }
    };

    let opened = read_proposal(node.rpc(), &proposal_id)?
        .ok_or_else(|| format!("proposal {proposal_id} disappeared"))?;
    let after_vote = cast_yes_once(&node, &proposal_id, opened, &signer)?;

    // Execute only once the proposal's own frozen voting rule is satisfied.
    let members = read_members(node.rpc())?;
    let (yes, required, ready) = proposal_progress(&after_vote, &members);
    if after_vote.status == ProposalStatus::Open && !ready {
        eprintln!(
            "{yes} of {required} required voting power — waiting on other voters. each runs:\n    \
             ducktape node member remove {pubkey_hex} --config <their node.toml>"
        );
        return Ok(());
    }
    if after_vote.status == ProposalStatus::Open {
        signer.submit(
            node.rpc(),
            &GovMsg::Execute {
                proposal_id: proposal_id.clone(),
            },
        )?;
    }
    let settled = await_proposal(&node, &proposal_id, TALLY_SETTLE_TIMEOUT, |p| {
        p.as_ref().is_some_and(|v| v.status != ProposalStatus::Open)
    })?
    .expect("the poll only accepts a present proposal");
    match settled.status {
        ProposalStatus::Passed => {
            eprintln!("removed {pubkey_hex}: the validator set changes at the next epoch cutover");
            Ok(())
        }
        status => Err(format!("proposal {proposal_id} settled as {status:?}").into()),
    }
}

// ---- member leave: this node drives its OWN removal from the set ----------

/// `member leave [--config node.toml]` — a member drives its OWN removal:
/// resolve this node's identity and route it through the EXACT SAME governance
/// path as `member remove` (a RemoveValidator proposal targeting self). there
/// is no separate governance logic — it hands off to [`cmd_member_remove`] with
/// this node's own pubkey.
///
/// honesty: leaving is NOT unilateral when this account lacks the proposal's
/// required power. This casts only its account ballot, and member remove
/// prints the remaining threshold plus the command
/// other voters run (`member remove <this key>`).
fn cmd_member_leave(args: SelectorArgs) -> Result<(), Box<dyn std::error::Error>> {
    let cfg_path = args.selector.config_path()?;
    // resolve the running node's identity — the key it signs ballots with, and
    // the one this verb submits for removal.
    let resolved = config::resolve(&cfg_path)?;
    let me_hex = hex_bytes(resolved.signer.public_key().as_ref());
    eprintln!("leaving the network: opening a self-removal for {me_hex}");
    // delegate to member remove targeting SELF — same propose+vote+execute
    // path, same strict-majority honesty, same selector.
    cmd_member_remove(PubkeyArgs {
        pubkey: me_hex,
        selector: args.selector,
    })
}

// ---- member status: is THIS node still in the validator set? --------------

/// `member status [--config <path> | -n <chain-id>] [--json]` — read this
/// node's OWN membership off its RUNNING node's rpc and print one
/// machine-parseable line to stdout:
///
/// ```text
/// in-set=<true|false> validators=<count>
/// ```
///
/// `--json` emits the same two facts as `{"in_set": <bool>, "validators": <n>}`.
///
/// this is the read to take before TEARING A NODE DOWN for good: stopping one
/// that is still a current validator of a set of two-or-more strands its
/// pending removal and halts quorum (a live network still needs its
/// signature). `in-set=true` with `validators>=2` means `member remove` (or
/// `leave`) first; a lone validator (`validators=1`) or an already-removed key
/// (`in-set=false`) can simply be stopped. requires the node to be up (it
/// serves this over the same local rpc as `member remove`).
fn cmd_member_status(args: StatusArgs) -> Result<(), Box<dyn std::error::Error>> {
    let cfg_path = args.selector.config_path()?;
    let resolved = config::resolve(&cfg_path)?;
    let rpc_addr = resolved
        .rpc_listen
        .clone()
        .ok_or("member status reads the node's local rpc — set `rpc_listen` in node.toml")?;
    let me_bytes = resolved.signer.public_key().as_ref().to_vec();
    let members = read_members(&rpc_addr)?;
    let in_set = members.contains(&me_bytes);
    let validators = members.len();
    if args.json {
        println!(
            "{}",
            serde_json::json!({ "in_set": in_set, "validators": validators })
        );
        return Ok(());
    }
    println!("in-set={in_set} validators={validators}");
    Ok(())
}

/// read a pasted invite blob from stdin: every line up to EOF or the first
/// empty line after content. A terminal paste may arrive wrapped across
/// several lines; the decoder strips the whitespace, so the lines are simply
/// collected. The prompt goes to stderr (stdout stays program output).
fn read_invite_blob_from_stdin() -> Result<String, Box<dyn std::error::Error>> {
    use std::io::IsTerminal as _;
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        eprintln!("paste the invite blob (wrapped lines are fine), then press Enter:");
    }
    let collected = collect_blob_lines(stdin.lock())?;
    if collected.is_empty() {
        return Err("join needs an invite blob (or a `requests`/`state` subcommand)".into());
    }
    Ok(collected)
}

/// gather blob lines from any reader: stop at EOF, or at the first empty line
/// once some content has arrived (the Enter that ends an interactive paste).
fn collect_blob_lines(reader: impl std::io::BufRead) -> std::io::Result<String> {
    let mut collected = String::new();
    for line in reader.lines() {
        let line = line?;
        let paste_finished = line.trim().is_empty() && !collected.is_empty();
        if paste_finished {
            break;
        }
        collected.push_str(line.trim());
    }
    Ok(collected)
}

/// `join [invite blob...] [--dir <dir>] [--genesis <file>] [--listen a]
/// [--advertised a] [--http a] [--rpc a] [--wireguard-listen a]
/// [--wireguard-advertised host:port] [--invite-listen a]
/// [--primary-coordinator host:port|none]` — materialize a workspace
/// from an invite: descriptor + identity (kept across re-joins) + node
/// config, defaulting into the registry dir named by the invite's chain id.
/// With no blob argv the blob is read from stdin (interactive paste prompt,
/// or a pipe). prints this identity for the inviter's pre-genesis `admit`.
/// `--genesis` is the founder's `<workspace>/genesis`: required for a member
/// (it boots into genesis with no peer to fetch from), optional for a
/// resident (its first boot fetches the file off the mesh otherwise).
/// `--primary-coordinator` overrides the coordinator the invite names for a
/// fresh workspace (the inviter's own, or "none"); it never touches the
/// joined descriptor.
fn cmd_join(args: JoinCmd) -> Result<(), Box<dyn std::error::Error>> {
    // read BEFORE anything lands on disk: a mistyped path is refused with
    // nothing written, like a bad blob.
    let genesis_bytes = match &args.genesis {
        Some(file) => {
            Some(std::fs::read(file).map_err(|e| format!("read genesis {}: {e}", file.display()))?)
        }
        None => None,
    };
    // argv words are rejoined (a blob pasted unquoted splits on its wrapped
    // spaces); no argv at all reads the blob from stdin. decode strips ALL
    // whitespace, so both paths tolerate a line-wrapped paste verbatim.
    //
    // READING the blob is what stays here: a terminal prompt is the CLI's
    // business, and it is exactly what `join_workspace` refuses to know about.
    let blob = match args.blob.is_empty() {
        false => args.blob.concat(),
        true => read_invite_blob_from_stdin()?,
    };
    let overrides = args.plumbing.overrides();
    let joined = config::join_workspace(&blob, args.dir.clone(), &overrides)?;
    record_founding_binary(&joined.dir)?;
    match (&genesis_bytes, joined.is_member) {
        (Some(bytes), _) => install_joiner_genesis(&joined.dir, bytes)?,
        (None, true) => {
            return Err(format!(
                "this identity is a member, and a member boots from its own genesis — \
                 re-run `ducktape node join <invite> --genesis <file>` with the founder's \
                 {} (the workspace in {} keeps its identity across re-joins)",
                config::GENESIS_FILE,
                joined.dir.display()
            )
            .into());
        }
        (None, false) => {}
    }

    if let Some(runtime) = &joined.compute_runtime {
        eprintln!(
            "compute plane: {runtime} found — writing a live [sandbox] table \
             (announce stays off; delete the table for a consensus-only node)"
        );
    }
    eprintln!(
        "{} identity {}",
        if joined.generated {
            "generated"
        } else {
            "reusing"
        },
        joined.identity
    );
    eprintln!(
        "workspace for {} written to {}",
        joined.chain_id,
        joined.dir.display()
    );
    let start = launcher_start(&joined.dir);
    if joined.is_member {
        eprintln!("this identity is a member — start it under the launcher: {start}");
    } else {
        eprintln!(
            "NOT yet a member. this wrote a workspace; nothing was checked with the network. \
             start now, under the launcher that follows the network's node releases — {start}. \
             the node presents the invite on first contact: an invite admits exactly one node, \
             so if it was already used or has expired the node refuses within seconds \
             (`invite already redeemed`) and you need a fresh one from the inviter; otherwise \
             it joins the network's VPN, syncs state, and comes up as a full node. no approval \
             step follows (minting the invite WAS the approval); a member can later promote it \
             into the quorum with `ducktape node member promote {}`.",
            joined.identity
        );
    }
    println!("{}", joined.identity);
    Ok(())
}

#[cfg(test)]
mod json_output_tests {
    /// member-status `--json` carries the same two facts as the prose line.
    #[test]
    fn member_status_json_shape() {
        let in_set = true;
        let validators = 3usize;
        let v = serde_json::json!({ "in_set": in_set, "validators": validators });
        assert_eq!(v["in_set"], true);
        assert_eq!(v["validators"], 3);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    /// `node status`'s netstack line: nothing at all on a node with no plane,
    /// the backend alone before the first swap, and the outcome with the height
    /// it landed at afterwards — a refusal carrying its reason.
    #[test]
    fn the_netstack_status_line_reports_only_what_the_node_answered() {
        assert_eq!(super::netstack_line(&serde_json::Value::Null), None);
        assert_eq!(
            super::netstack_line(&serde_json::json!({ "backend": "native", "last_swap": null })),
            Some("netstack=native".to_string())
        );
        assert_eq!(
            super::netstack_line(&serde_json::json!({
                "backend": "guest",
                "last_swap": { "outcome": "swapped", "at_height": 12 },
            })),
            Some("netstack=guest last_swap=swapped@12".to_string())
        );
        assert_eq!(
            super::netstack_line(&serde_json::json!({
                "backend": "native",
                "last_swap": { "outcome": "refused", "reason": "foreign contract", "at_height": 4 },
            })),
            Some("netstack=native last_swap=refused@4 reason=foreign contract".to_string())
        );
    }

    /// A NODE WITH NO OVERLAY SAYS SO WHEREVER ITS STATUS IS READ. A founder
    /// whose plane never started keeps sealing blocks and keeps answering
    /// `/v1/status`, so the swap history is not the fact a reader needs — and
    /// the same two fields refuse an invite mint that nobody could redeem.
    #[test]
    fn a_dead_mesh_preempts_the_swap_history_and_refuses_a_mint() {
        let down = serde_json::json!({
            "backend": "failed",
            "failure_reason": "netstack_guest_unreadable",
            "failure_detail": "no founding set beside the binary",
            "last_swap": { "outcome": "swapped", "at_height": 12 },
        });
        assert_eq!(
            super::netstack_line(&down),
            Some(
                "netstack=failed NO MESH reason=netstack_guest_unreadable — no founding set \
                 beside the binary"
                    .to_string()
            )
        );
        let (reason, detail) =
            super::netstack_failure_in_section(&down).expect("the section names its failure");
        let refusal = super::mesh_down_refusal(&reason, &detail);
        assert!(refusal.contains("reason=netstack_guest_unreadable"), "{refusal}");
        assert!(refusal.contains("never be redeemed"), "{refusal}");

        // a running plane names no failure, and nothing is refused.
        assert_eq!(
            super::netstack_failure_in_section(&serde_json::json!({
                "backend": "guest",
                "last_swap": null,
            })),
            None
        );
    }

    /// A node answers `status` with everything an operator needs to tell a
    /// wedged chain from a healthy one, so the verb prints it. Height and root
    /// hash alone are identical on both, forever.
    #[test]
    fn a_stalled_chain_reads_as_stalled_and_a_beating_one_does_not() {
        let stalled = serde_json::json!({
            "height": 399,
            "root_hash": "2170",
            "operations": {
                "role": "validator", "phase": "validating",
                "consensus": {
                    "epoch": 2, "view": 0, "validators": 2, "quorum": 2,
                    "reachable_validators": 1, "pending_ops": 3,
                    "block_beat_stalled_seconds": 116,
                },
            },
        });
        assert_eq!(
            super::status_lines(&stalled, HEARD_AT),
            [
                "height=399 root_hash=2170",
                // `reachable=1` under `quorum=2` IS the diagnosis, and the
                // seconds say how long it has been true.
                "role=validator phase=validating quorum=2 reachable=1 stalled_for=116s",
                "follow: none yet",
            ]
        );
        assert_eq!(super::stalled_past_recovery(&stalled), Some(116));

        let beating = serde_json::json!({
            "height": 400,
            "root_hash": "2171",
            "operations": {
                "role": "validator", "phase": "validating",
                "consensus": {
                    "epoch": 2, "view": 1, "validators": 2, "quorum": 2,
                    "reachable_validators": 2, "pending_ops": 0,
                    "block_beat_stalled_seconds": 0,
                },
            },
        });
        assert_eq!(
            super::status_lines(&beating, HEARD_AT),
            [
                "height=400 root_hash=2171",
                "role=validator phase=validating quorum=2 reachable=2",
                "follow: none yet",
            ]
        );
        assert_eq!(super::stalled_past_recovery(&beating), None);
    }

    /// A resident that stopped following prints WHY its screen is stale: the
    /// phase names it and the gap sizes it. Height and root hash alone are the
    /// same two numbers a healthy node prints.
    #[test]
    fn a_node_that_stopped_following_prints_the_gap_it_stopped_at() {
        let frozen = serde_json::json!({
            "height": 155, "root_hash": "2170",
            "operations": {
                "role": "resident", "phase": "behind",
                "follow": { "network_height": 756, "behind_by": 601, "heard_at": HEARD_AT },
            },
        });
        assert_eq!(
            super::status_lines(&frozen, HEARD_AT + 12)[1..],
            [
                "role=resident phase=behind",
                "follow: behind_by=601 network_height=756 (heard 12s ago)",
            ]
        );
    }

    /// A joiner's first question is "have I caught up?", and `height=` alone
    /// cannot answer it: the follow line prints the gap `--json` carries —
    /// while it is catching up, once it has, and before any peer answered.
    #[test]
    fn a_joiner_reads_whether_it_has_caught_up() {
        let joiner = |phase: &str, height: u64, follow: serde_json::Value| {
            serde_json::json!({
                "height": height, "root_hash": "2171",
                "operations": { "role": "resident", "phase": phase, "follow": follow },
            })
        };
        let catching_up = joiner(
            "syncing",
            155,
            serde_json::json!({ "network_height": 756, "behind_by": 601, "heard_at": HEARD_AT }),
        );
        assert_eq!(
            super::status_lines(&catching_up, HEARD_AT + 3)[2],
            "follow: behind_by=601 network_height=756 (heard 3s ago)"
        );

        let caught_up = joiner(
            "serving",
            756,
            serde_json::json!({ "network_height": 756, "behind_by": 0, "heard_at": HEARD_AT }),
        );
        assert_eq!(
            super::status_lines(&caught_up, HEARD_AT + 185)[1..],
            [
                "role=resident phase=serving",
                "follow: behind_by=0 network_height=756 (heard 3m05s ago)",
            ]
        );

        let unheard = joiner("syncing", 0, serde_json::Value::Null);
        assert_eq!(
            super::status_lines(&unheard, HEARD_AT)[2],
            "follow: none yet"
        );
    }

    /// the unix second the fixtures' tip landed at.
    const HEARD_AT: u64 = 1_758_000_000;

    /// A silence shorter than the point the chain recovers on its own is
    /// PRINTED and not exited on: a view change or a slow disk is not a dead
    /// chain, and a verb that exits non-zero on one teaches an operator to
    /// ignore it.
    #[test]
    fn a_brief_silence_is_reported_without_a_verdict() {
        let blipping = serde_json::json!({
            "height": 399, "root_hash": "2170",
            "operations": {
                "role": "validator", "phase": "validating",
                "consensus": {
                    "validators": 2, "quorum": 2, "reachable_validators": 2,
                    "block_beat_stalled_seconds": 12,
                },
            },
        });
        assert_eq!(
            super::status_lines(&blipping, HEARD_AT)[1],
            "role=validator phase=validating quorum=2 reachable=2 stalled_for=12s"
        );
        assert_eq!(super::stalled_past_recovery(&blipping), None);
    }

    /// A role with no consensus section gets no consensus fields, rather than
    /// zeroes that read as "quorum 0, nothing reachable" — the projection omits
    /// what does not apply and so does the line.
    #[test]
    fn a_node_outside_consensus_prints_what_it_has() {
        let syncing = serde_json::json!({
            "height": 12, "root_hash": "aa",
            "operations": {
                "role": "resident", "phase": "syncing",
                "netstack": { "backend": "native", "last_swap": null },
            },
        });
        assert_eq!(
            super::status_lines(&syncing, HEARD_AT),
            [
                "height=12 root_hash=aa",
                "role=resident phase=syncing",
                "follow: none yet",
                "netstack=native",
            ]
        );
        assert_eq!(super::stalled_past_recovery(&syncing), None);

        // a daemon that answers no operations at all still answers a tip,
        // and has heard none.
        let bare = serde_json::json!({ "height": 1, "root_hash": "bb" });
        assert_eq!(
            super::status_lines(&bare, HEARD_AT),
            ["height=1 root_hash=bb", "follow: none yet"]
        );
        assert_eq!(super::stalled_past_recovery(&bare), None);
    }

    #[test]
    fn blob_lines_join_a_wrapped_paste_and_stop_at_the_closing_enter() {
        let pasted = "\n  \n\u{1f986}DWRlbW8j\nMGZiZGQ5\n ZTUB \n\nnot-part-of-the-blob\n";
        let collected = super::collect_blob_lines(pasted.as_bytes()).expect("collect");
        assert_eq!(collected, "\u{1f986}DWRlbW8jMGZiZGQ5ZTUB");
    }

    #[test]
    fn blob_lines_are_empty_on_empty_input() {
        let collected = super::collect_blob_lines("\n \n".as_bytes()).expect("collect");
        assert_eq!(collected, "");
    }

    fn completions() -> (String, String) {
        let bash = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../ops/completions/ducktape.bash"
        ))
        .expect("read ops/completions/ducktape.bash");
        let zsh = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../ops/completions/ducktape.zsh"
        ))
        .expect("read ops/completions/ducktape.zsh");
        (bash, zsh)
    }

    /// Exact tokens declared by one completion variable family. Both shipped
    /// files keep each `local` assignment on one line, so the same tiny parser
    /// covers Bash's quoted words and Zsh's parenthesized words.
    fn declaration_tokens(text: &str, stem: &str) -> BTreeSet<String> {
        text.lines()
            .filter_map(|line| line.trim_start().strip_prefix("local "))
            .filter_map(|declaration| declaration.split_once('='))
            .filter(|(name, _)| {
                let exact_stem = *name == stem;
                let nested_stem = name
                    .strip_prefix(stem)
                    .is_some_and(|suffix| suffix.starts_with('_'));
                exact_stem || nested_stem
            })
            .flat_map(|(_, words)| {
                words
                    .trim()
                    .trim_matches(|c| matches!(c, '"' | '(' | ')'))
                    .split_whitespace()
            })
            .map(str::to_string)
            .collect()
    }

    fn grammar_tokens(cmd: &clap::Command, tokens: &mut BTreeSet<String>) {
        for arg in cmd.get_arguments().filter(|arg| !arg.is_hide_set()) {
            if let Some(long) = arg.get_long().filter(|long| *long != "help") {
                tokens.insert(format!("--{long}"));
            }
            if let Some(short) = arg.get_short().filter(|short| *short != 'h') {
                tokens.insert(format!("-{short}"));
            }
        }
        for sub in cmd.get_subcommands().filter(|sub| !sub.is_hide_set()) {
            tokens.insert(sub.get_name().to_string());
            grammar_tokens(sub, tokens);
        }
    }

    fn assert_same_tokens(
        file: &str,
        scope: &str,
        declared: &BTreeSet<String>,
        required: &BTreeSet<String>,
        allowed: &BTreeSet<String>,
    ) {
        for token in required {
            assert!(
                declared.contains(token),
                "{file}: {scope} is missing {token:?}"
            );
        }
        for token in declared {
            assert!(
                allowed.contains(token),
                "{file}: {scope} advertises stale token {token:?}"
            );
        }
    }

    /// The drift guard is bidirectional: every visible Clap verb/flag appears
    /// in both completion files, and every advertised token still exists in
    /// that family's grammar. This catches stale extras as well as omissions.
    #[test]
    fn completion_files_match_the_clap_tree_per_family() {
        let (bash, zsh) = completions();
        let cli = <crate::Cli as clap::CommandFactory>::command();
        let mut top = cli
            .get_subcommands()
            .filter(|family| !family.is_hide_set())
            .map(|family| family.get_name().to_string())
            .collect::<BTreeSet<_>>();
        top.extend(["help", "--help", "-h", "--version", "-V"].map(str::to_string));
        for (file, text) in [("ducktape.bash", &bash), ("ducktape.zsh", &zsh)] {
            let declared = declaration_tokens(text, "families");
            assert_same_tokens(file, "top level", &declared, &top, &top);
        }

        for family in cli.get_subcommands().filter(|family| !family.is_hide_set()) {
            let name = family.get_name();
            if name == "help" {
                continue;
            }
            let mut required = BTreeSet::new();
            grammar_tokens(family, &mut required);
            required.remove("--version");
            required.remove("-V");
            let mut allowed = required.clone();
            allowed.insert("help".into());
            if name == "service" {
                allowed.extend(["compute", "agent", "airlock"].map(str::to_string));
            }
            for (file, text) in [("ducktape.bash", &bash), ("ducktape.zsh", &zsh)] {
                let declared = declaration_tokens(text, name);
                let bare_family = required.is_empty() && declared.is_empty();
                if bare_family {
                    continue;
                }
                assert_same_tokens(file, name, &declared, &required, &allowed);
            }
        }
    }

    /// the guard must actually bite: a verb a sibling family happens to use is
    /// NOT coverage. This is the hole the whole-file match had.
    #[test]
    fn the_family_scope_does_not_borrow_a_siblings_verb() {
        let (bash, _zsh) = completions();
        let service = declaration_tokens(&bash, "service");
        assert!(service.contains("run"), "service declares its own run verb");
        // `promote` lives under `node member`; it must not read as covered here.
        assert!(
            !service.contains("promote"),
            "the service scope must not see node's verbs"
        );
        assert!(
            declaration_tokens(&bash, "gateway").contains("bind"),
            "a family scope still finds its own verbs"
        );
    }

    /// a module verb must JOIN the founder's open proposal by whichever
    /// fields ITS OWN matcher cares about, not full action equality — a
    /// caller whose matcher ignores a field (here, `activation_lead`) still
    /// finds the right open proposal among unrelated ones.
    #[test]
    fn open_proposal_matching_ignores_fields_the_matcher_ignores() {
        use super::open_proposal_matching;
        use governance::{GovAction, ProposalStatus, ProposalView, VoterKind, VotingRule};
        let view = |id: &str, status: ProposalStatus, action: GovAction| ProposalView {
            proposal_id: id.into(),
            action,
            proposer: vec![1],
            created_at: 0,
            deadline: 10,
            status,
            votes: vec![],
            voter_kind: VoterKind::ValidatorNode,
            electorate: vec![],
            voting_rule: VotingRule::Threshold { required_yes: 1 },
        };
        let hash = vec![7u8; 32];
        let founders = view(
            "module:aa:0",
            ProposalStatus::Open,
            GovAction::UpdateModule {
                name: "x".into(),
                module_id: "hello".into(),
                // a lead the matcher below does not compare against.
                activation_lead: 61,
                code_hash: hash.clone(),
            },
        );
        let settled = view(
            "module:bb:0",
            ProposalStatus::Passed,
            GovAction::UpdateModule {
                name: "x".into(),
                module_id: "hello".into(),
                activation_lead: 60,
                code_hash: hash.clone(),
            },
        );
        let other = view(
            "module:cc:0",
            ProposalStatus::Open,
            GovAction::RegisterModule {
                name: "x".into(),
                module_id: "hello".into(),
                kind: modules::Kind::Module,
                activation_lead: 60,
                code_hash: hash.clone(),
                lanes: Vec::new(),
            },
        );
        let views = vec![settled, other, founders];
        // the matcher below cares only about module_id and code_hash — a
        // different activation_lead must not stop it from joining "founders".
        let matches = |a: &GovAction| {
            matches!(a, GovAction::UpdateModule { module_id, code_hash, .. }
                if module_id == "hello" && *code_hash == hash)
        };
        let found = open_proposal_matching(&views, &matches).expect("the open update proposal");
        assert_eq!(found.proposal_id, "module:aa:0");
        let none = open_proposal_matching(&views, &|a| {
            matches!(a, GovAction::CancelModuleUpdate { .. })
        });
        assert!(none.is_none());
    }

    /// the grammar's own consistency check (conflicting ids, broken flatten,
    /// missing subcommand settings all panic here instead of at first use).
    #[test]
    fn the_clap_tree_is_internally_consistent() {
        <crate::Cli as clap::CommandFactory>::command().debug_assert();
    }

    use super::{human_duration, peer_line};

    /// the peers prose line: rates only with a baseline, statesync tokens
    /// only when the lane reports, durations compacted.
    #[test]
    fn peer_line_carries_rates_only_with_a_baseline() {
        let sample = |sent, bytes| noded::peers::PeerView {
            peer: "ab".repeat(32),
            connected: true,
            connected_since_ms: Some(1_000),
            role: Some("validator".into()),
            build: Some("test-build".into()),
            msgs_sent: sent,
            msgs_received: 0,
            statesync: Some(noded::peers::StatesyncServeView {
                bytes_tx: bytes,
                frames_served: 2,
                boundary_height: Some(230),
                served_height: Some(230),
                idle_seconds: 4,
                age_seconds: 90,
                last_request_kind: Some("tip_coords".into()),
            }),
        };
        let first = noded::peers::PeersView {
            sampled_at_ms: 10_000,
            height: 5,
            epoch: Some(1),
            peers: vec![sample(100, 1_000)],
        };
        let second = noded::peers::PeersView {
            sampled_at_ms: 12_000,
            height: 6,
            epoch: Some(1),
            peers: vec![sample(150, 3_000)],
        };

        let with_baseline = peer_line(&second.peers[0], Some(&first.peers[0]), &first, &second);
        assert_eq!(
            with_baseline,
            format!(
                "peer={} role=validator build=test-build connected=11s msgs_tx=150 msgs_rx=0 \
                 tx/s=25.0 rx/s=0.0 sync_bytes=3000 sync_B/s=1000 sync_height=230 \
                 sync_boundary=230 sync_idle=4s sync_last=tip_coords",
                "ab".repeat(32)
            )
        );

        let without_baseline = peer_line(&second.peers[0], None, &first, &second);
        assert!(!without_baseline.contains("tx/s="), "{without_baseline}");
        assert!(
            !without_baseline.contains("sync_B/s="),
            "{without_baseline}"
        );

        // a peer that never named a build says so — the column never goes
        // missing, because an absent column reads as agreement.
        let mut unstamped = second.peers[0].clone();
        unstamped.build = None;
        assert!(
            peer_line(&unstamped, None, &first, &second).contains("build=unknown"),
            "an unreported stamp renders as unknown"
        );
    }

    /// `node peers` opens with the node's OWN position — the `height` (and
    /// `epoch`) its `--json` carries beside the peer table — whether or not a
    /// peer answered, so the rows below are anchored to a block.
    #[test]
    fn peers_prose_opens_with_the_height_its_json_carries() {
        let view = |height, epoch, peers| noded::peers::PeersView {
            sampled_at_ms: 10_000,
            height,
            epoch,
            peers,
        };
        let peer = noded::peers::PeerView {
            peer: "cd".repeat(32),
            connected: false,
            connected_since_ms: None,
            role: None,
            build: None,
            msgs_sent: 0,
            msgs_received: 0,
            statesync: None,
        };

        let first = view(5, Some(1), vec![peer.clone()]);
        let second = view(6, Some(1), vec![peer]);
        let lines = super::peers_lines(&first, Some(&second));
        assert_eq!(
            lines[0], "height=6 epoch=1",
            "the rate sample's own position"
        );
        assert!(lines[1].starts_with("peer=cdcd"), "{lines:?}");
        assert_eq!(lines.len(), 2, "{lines:?}");

        // no peer to rate: the height still leads, and a lane with no
        // consensus has no epoch to name.
        let alone = view(5, None, Vec::new());
        assert_eq!(
            super::peers_lines(&alone, None),
            ["height=5", "no direct peers"]
        );
    }

    #[test]
    fn durations_compact_by_magnitude() {
        assert_eq!(human_duration(42), "42s");
        assert_eq!(human_duration(192), "3m12s");
        assert_eq!(human_duration(7500), "2h05m");
    }

    /// #2526: the node refuses a promotion of a key it has never met, but it
    /// refuses it at APPLY, so the operator used to get thirty seconds of
    /// silence and then `timed out waiting for the proposal to finalize`. The
    /// precheck reads the same two rosters consensus reads and says it first.
    #[test]
    fn a_promotion_of_a_key_with_no_standing_is_refused_before_anything_is_submitted() {
        use super::{Promotion, precheck_promotion};
        let hex = "0000000000000000000000000000000000000000000000000000000000000000";
        let stranger = vec![0u8; 32];
        let seated = vec![1u8; 32];
        let resident = vec![2u8; 32];
        let members = vec![seated.clone()];
        let residents = vec![resident.clone()];

        let refusal = precheck_promotion(hex, &stranger, &members, &residents)
            .expect_err("a key in neither tier has nothing to be promoted out of");
        assert!(refusal.starts_with("not_a_resident: "), "{refusal}");
        assert!(refusal.contains(hex), "the refusal names the key: {refusal}");
        assert!(
            refusal.contains("ducktape node resident accept"),
            "and the command that fixes it: {refusal}"
        );

        // a resident is exactly what a promotion is for.
        assert_eq!(
            precheck_promotion(hex, &resident, &members, &residents),
            Ok(Promotion::Proceed)
        );
        // an already-seated key stays the idempotent re-run it has always been,
        // NOT a refusal — the desired state already holds.
        assert_eq!(
            precheck_promotion(hex, &seated, &members, &residents),
            Ok(Promotion::AlreadySeated)
        );
        // empty rosters refuse rather than wave everything through.
        assert!(precheck_promotion(hex, &stranger, &[], &[]).is_err());
    }

    /// #2512: `resident remove` of a seated validator printed its redirect
    /// and exited 0, so a script read "removed". It is a refusal — an `Err`,
    /// which `main` turns into exit 1 — while a key with no standing stays the
    /// idempotent no-op it always was.
    #[test]
    fn removing_a_seated_validator_as_a_resident_is_refused_not_skipped() {
        use super::{ResidentRemoval, precheck_resident_removal};
        let hex = "0101010101010101010101010101010101010101010101010101010101010101";
        let seated = vec![1u8; 32];
        let resident = vec![2u8; 32];
        let stranger = vec![3u8; 32];
        let members = vec![seated.clone()];
        let residents = vec![resident.clone()];

        let refusal = precheck_resident_removal(hex, &seated, &members, &residents)
            .expect_err("a seated key is not a resident to remove");
        assert!(
            refusal.contains(&format!("`ducktape node member remove {hex}`")),
            "it names the verb that does remove it: {refusal}"
        );
        assert_eq!(
            precheck_resident_removal(hex, &resident, &members, &residents),
            Ok(ResidentRemoval::Proceed)
        );
        assert_eq!(
            precheck_resident_removal(hex, &stranger, &members, &residents),
            Ok(ResidentRemoval::NoStanding)
        );
    }

    /// A founder workspace on disk, complete enough for `mint_invite_blob`:
    /// node.toml, the descriptor it names, and an identity. `advertised` is
    /// the one value under test; the rest is the shape `node init` writes on
    /// a single box (unspecified binds, no coordinator).
    fn founder_workspace(name: &str, advertised: &str) -> std::path::PathBuf {
        use super::config;
        use commonware_cryptography::Signer as _;

        let dir = std::env::temp_dir().join(format!(
            "ducktape-invite-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        std::fs::write(
            dir.join("node.toml"),
            format!(
                "network = \"network.toml\"\nkey_file = \"identity.key\"\n\
                 listen = \"[::]:52330\"\nadvertised = {advertised:?}\n\
                 storage_dir = \"storage\"\nhttp_listen = \"127.0.0.1:0\"\n\
                 gateway_listen = \"127.0.0.1:0\"\nrpc_listen = \"127.0.0.1:0\"\n\
                 wireguard_listen = \"0.0.0.0:52333\"\ninvite_listen = \"0.0.0.0:52334\"\n\
                 wireguard_advertised = \"auto\"\nprimary_coordinator = \"none\"\n\
                 coordinator_relay = \"none\"\ncheckpoint_blocks = 32\n"
            ),
        )
        .expect("write node.toml");
        let (me, _) = config::load_or_generate_identity(&dir.join("identity.key")).expect("keygen");
        config::NetworkDescriptor {
            chain_id: "net#25662566".into(),
            validators: vec![super::hex_bytes(me.public_key().as_ref())],
            bootstrap: Vec::new(),
            reach: Vec::new(),
            coordination: None,
            block_time_ms: config::DEFAULT_BLOCK_TIME_MS,
            modules: vec![config::ModuleCode {
                id: "pages".into(),
                code_hash: "11".repeat(32),
            }],
            genesis: "ab".repeat(32),
        }
        .save(&dir.join("network.toml"))
        .expect("save descriptor");
        dir
    }

    /// A single-box founder — `advertised = "overlay"`, unspecified binds, no
    /// coordinator — has no dialable host to bake into an invite. That is a
    /// NOTE beside a minted blob, never a refusal: the join over loopback
    /// works, and an operator told "no dialable host" INSTEAD of being handed
    /// the credential reads a working node as a broken one. Name a dialable
    /// `advertised` and there is nothing to say.
    #[test]
    fn a_founder_naming_no_host_still_mints_and_is_told_what_the_blob_cannot_do() {
        let reasons = |dir: &std::path::Path| -> Vec<&'static str> {
            let (blob, notes) = super::mint_invite_blob(&dir.join("node.toml"), 7)
                .expect("a founder always gets its invite");
            assert!(!blob.is_empty(), "the blob is the whole point");
            notes.iter().map(super::InviteNote::reason).collect()
        };

        let overlay = founder_workspace("overlay", "overlay");
        assert!(
            reasons(&overlay).contains(&"invite_not_dialable_off_box"),
            "a blob that reaches this machine only says so"
        );

        let dialable = founder_workspace("dialable", "127.0.0.1:52330");
        assert!(
            !reasons(&dialable).contains(&"invite_not_dialable_off_box"),
            "an advertised host IS the endpoint — nothing to note"
        );
    }
}
