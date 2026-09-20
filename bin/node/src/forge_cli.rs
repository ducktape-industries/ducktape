//! `git-remote-duck`, the git remote helper for
//! `duck://<label>-<salt>/forge/<owner>/<repo>`, and `ducktape forge setup`,
//! which puts it on PATH.
//!
//! The helper is a MODE of the one `ducktape` binary, entered when it runs
//! under the name `git-remote-duck` (git runs `git-remote-<scheme> <remote>
//! <url>` for a `<scheme>://` remote). It adds no transport: it resolves the
//! address to forge's Git service as a node already serves it, then hands the
//! whole remote-helper conversation to git's own `git remote-http`.
//!
//! One rule per step:
//! - the address is sdk's `duck-address` + `forge_wire::ForgeRepoAddress`,
//!   never re-parsed here;
//! - the network is the address's chain id against the workspace registry
//!   ([`workspace_config::resolve_chain_in`]): the salt matches, the label
//!   agrees, exactly one entry, local or remote;
//! - the node is that entry's http base, and the git door is the browser
//!   gateway base that node reports on `GET /v1/gateway/browser`;
//! - forge's Git service is a Gateway application an account publishes under
//!   the route label [`GIT_ROUTE_LABEL`], serving the network's forge
//!   repositories at `/<repo>`. Forge's repository namespace is flat, so
//!   `<owner>` names the account whose `git` route serves the repository:
//!   the address becomes the authority `git.<owner>.duck` and the path
//!   `/<repo>`.
//!
//! No credential travels. The browser gateway listens on its node's own
//! 127.0.0.1 and admits what the route's signed audience admits, so a read
//! carries nothing a URL, a lock file or a log line could leak; a write is
//! authorized by the push certificate `git push --signed` signs against the
//! node's nonce, exactly as through any other git client.

use std::path::{Path, PathBuf};

use commonware_cryptography::Signer as _;
use duck_address::{Address, ChainId, Refused};
use forge_wire::ForgeRepoAddress;
use workspace_config::{Registered, RemoteWorkspace};

/// the name git runs the helper by: `git-remote-<scheme>`.
pub(crate) const HELPER: &str = "git-remote-duck";

/// the Gateway route label forge's Git service is published under
/// (`ducktape gateway bind --label git`).
const GIT_ROUTE_LABEL: &str = "git";

/// `ducktape forge` — git over `duck://` addresses.
#[derive(Debug, clap::Subcommand)]
pub enum ForgeCmd {
    /// put `git-remote-duck` on PATH, beside this `ducktape` (idempotent;
    /// prints every change)
    ///
    /// Setup also sets `[net] git-fetch-with-cli = true` in the user's Cargo
    /// configuration: Cargo's built-in fetcher cannot run a remote helper, so
    /// without it Cargo never reaches this machine's `git-remote-duck`.
    ///
    /// `--instead-of <https-prefix>` adds git's own rewrite of that prefix to
    /// `duck://<network>/forge/<owner>/`, in this repository or (`--global`)
    /// for the user. Every dependency under the prefix then resolves through
    /// the network while `Cargo.toml` and `Cargo.lock` keep the URL they
    /// have — a lock names one source, so rewriting at the git layer is what
    /// moves a whole dependency graph at once.
    ///
    /// Setup succeeds only when every registered network resolves the way
    /// the helper will resolve it and has a Git door: an account with a
    /// handle that publishes a `git` route. Otherwise it names what is
    /// missing and the verb that supplies it.
    Setup(SetupArgs),
    /// publish the `git` Gateway route of the wallet's account, served by the
    /// node this verb dials — the node whose `ducktape gateway bind --label
    /// git` points at forge's Git service (idempotent; stdin: wallet password)
    Publish(PublishArgs),
}

#[derive(Debug, clap::Args)]
pub struct SetupArgs {
    /// register the node at this http base as a remote workspace: its chain
    /// id is read off its `/v1/status`
    #[arg(long, value_name = "HTTP-URL")]
    pub node: Option<String>,
    /// the registered workspace `duck://` addresses on its network resolve
    /// to when several are on it (a validator and its resident on one host):
    /// its node.toml, or a remote workspace's remote.toml, as the refusal
    /// lists them
    #[arg(long, value_name = "PATH", conflicts_with = "node")]
    pub config: Option<PathBuf>,
    /// rewrite every git URL under this https prefix to the network's Forge:
    /// `https://github.com/<org>/` becomes `duck://<chain-id>/forge/<owner>/`
    /// without a Cargo file changing a byte
    #[arg(long, value_name = "URL-PREFIX")]
    pub instead_of: Option<String>,
    /// the `<owner>` the rewrite points at, when a door has more than one
    #[arg(long, value_name = "HANDLE", requires = "instead_of")]
    pub owner: Option<String>,
    /// write the rewrite to the user's git configuration instead of the
    /// repository this runs in
    #[arg(long, requires = "instead_of")]
    pub global: bool,
}

#[derive(Debug, clap::Args)]
pub struct PublishArgs {
    #[command(flatten)]
    addr: crate::cli_args::NodeAddr,
    /// path to the user key file (defaults to the keystore's active wallet)
    #[arg(long, value_name = "PATH")]
    key: Option<PathBuf>,
}

pub(crate) fn run(cmd: ForgeCmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        ForgeCmd::Setup(args) => setup(args),
        ForgeCmd::Publish(args) => publish(args),
    }
}

/// did git run this binary as its `duck://` remote helper?
pub(crate) fn invoked_as_helper() -> bool {
    std::env::args_os()
        .next()
        .is_some_and(|argv0| Path::new(&argv0).file_name() == Some(HELPER.as_ref()))
}

/// the helper mode: resolve, then become `git remote-http` for the resolved
/// url. git talks to that process over the stdin/stdout it inherits.
pub(crate) fn run_helper() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let (Some(remote), Some(url), None) = (args.next(), args.next(), args.next()) else {
        return Err(format!(
            "{HELPER} is a git remote helper: git runs it as `{HELPER} <remote> <duck://address>`"
        )
        .into());
    };
    let url = url
        .into_string()
        .map_err(|url| format!("{url:?} is not a duck:// address"))?;
    let target = git_target(&workspace_config::ducktape_home()?, &url, gateway_base)?;
    let status = std::process::Command::new("git")
        .arg("-c")
        .arg(format!(
            "http.extraHeader=x-duck-authority: {}",
            target.authority
        ))
        .arg("remote-http")
        .arg(remote)
        .arg(&target.url)
        .status()
        .map_err(|error| format!("run git remote-http: {error}"))?;
    std::process::exit(status.code().unwrap_or(128));
}

/// where git reaches a `duck://` repository: an http url and the Gateway
/// authority the browser gateway routes it by.
#[derive(Debug, PartialEq, Eq)]
struct GitTarget {
    url: String,
    authority: String,
}

/// resolve an address to its [`GitTarget`]. `gateway` asks the resolved node
/// for its browser gateway base — the one network read, a seam so the rest
/// is testable against a registry on disk.
fn git_target(
    root: &Path,
    text: &str,
    gateway: impl FnOnce(&str) -> Result<String, String>,
) -> Result<GitTarget, String> {
    let sentence = |refused: Refused| refused.sentence;
    let address = Address::parse(text).map_err(sentence)?;
    let repository = ForgeRepoAddress::try_from(&address).map_err(sentence)?;
    let door = network_door(root, &address.chain, gateway)?;
    Ok(GitTarget {
        url: format!("{}/{}", door.via.trim_end_matches('/'), repository.repo),
        authority: format!("{GIT_ROUTE_LABEL}.{}.duck", repository.owner),
    })
}

/// where a network's `duck://` Git addresses go through on this machine: the
/// registered node and the browser gateway base it reports.
struct NetworkDoor {
    registered: Registered,
    node: String,
    via: String,
}

/// THE resolution of a network to its door — the helper's, and the one
/// `forge setup` verifies every registered network by, so setup can never
/// pass a network the helper refuses.
fn network_door(
    root: &Path,
    chain: &ChainId,
    gateway: impl FnOnce(&str) -> Result<String, String>,
) -> Result<NetworkDoor, String> {
    let sentence = |refused: Refused| refused.sentence;
    let (_chain_id, registered) =
        workspace_config::resolve_chain_in(root, chain).map_err(sentence)?;
    let node = registered.node_base()?;
    let via = gateway(&node)?;
    reachable_gateway(&node, &via).map_err(sentence)?;
    Ok(NetworkDoor {
        registered,
        node,
        via,
    })
}

/// the browser gateway base the node at `node` reports.
fn gateway_base(node: &str) -> Result<String, String> {
    let reply = crate::node_http::get_json(node, "/v1/gateway/browser")
        .map_err(|error| format!("read the browser gateway of the node at {node}: {error}"))?;
    reply["base"].as_str().map(str::to_string).ok_or_else(|| {
        format!("the node at {node} serves no browser gateway, so it routes no git remote")
    })
}

/// A node binds its browser gateway to 127.0.0.1 and nothing else
/// (`gateway_listen must bind exactly 127.0.0.1`), so the base it reports is
/// reachable from its own machine only. A node this machine dials on a
/// loopback base — its own, or one whose ports are forwarded here — shares
/// that loopback. A node dialed across the network does not, and dialing its
/// gateway base would reach THIS machine instead. The base is dialed as
/// reported or refused: no host is substituted into it.
fn reachable_gateway(node: &str, via: &str) -> Result<(), Refused> {
    let loopback = |base: &str| {
        let url = reqwest::Url::parse(base).ok();
        let host = url
            .as_ref()
            .and_then(|url| url.host_str())
            .unwrap_or_default();
        let ip = host.trim_matches(['[', ']']).parse::<std::net::IpAddr>();
        host == "localhost" || ip.is_ok_and(|ip| ip.is_loopback())
    };
    let across_the_network = !loopback(node) && loopback(via);
    if !across_the_network {
        return Ok(());
    }
    Err(Refused::new(
        "gateway_not_local",
        format!(
            "The node at {node} serves git only through its browser gateway, which listens on \
             that node's own loopback ({via}). Register a node this machine reaches on its \
             loopback: your own, or that node's http and gateway ports forwarded here at the \
             same numbers (`ssh -L`)."
        ),
    ))
}

/// `ducktape forge setup [--node <url>]`.
fn setup(args: SetupArgs) -> Result<(), Box<dyn std::error::Error>> {
    let root = workspace_config::ducktape_home()?;
    if let Some(node) = &args.node {
        let node = crate::cli_args::checked_base("--node", node)?;
        let chain_id = status_chain_id(&node)?;
        println!("{}", register_remote(&root, &chain_id, &node)?);
    }
    if let Some(config) = &args.config {
        println!("{}", pick_workspace(&root, config)?);
    }
    if workspace_config::registered_networks_in(&root)?.is_empty() {
        return Err(format!(
            "no network is registered under {} — redeem an invite first (`ducktape node join \
             <invite>`), or register a node you can reach with `ducktape forge setup --node <url>`",
            root.display()
        )
        .into());
    }
    let doors = git_doors(&root, gateway_base, door_owners)?;
    for door in &doors {
        println!("{door}");
    }
    if let Some(prefix) = &args.instead_of {
        let base = rewrite_base(&doors, args.owner.as_deref())?;
        let scope = match args.global {
            true => Scope::User,
            false => Scope::Repository,
        };
        println!("{}", rewrite_url(Path::new("."), scope, prefix, &base)?);
    }
    println!("{}", cargo_fetch_with_cli(&cargo_home()?)?);
    let ducktape = invoked_binary()?;
    let dir = ducktape.parent().unwrap_or(Path::new("."));
    let name = ducktape
        .file_name()
        .ok_or_else(|| format!("{} names no file", ducktape.display()))?;
    println!("{}", link_helper(dir, Path::new(name))?);
    let path = std::env::var_os("PATH").unwrap_or_default();
    let on_path = std::env::split_paths(&path).any(|entry| entry == dir);
    if !on_path {
        println!(
            "{} is not on PATH: add it, or git will not find {HELPER}",
            dir.display()
        );
    }
    Ok(())
}

/// record `config` — a registered workspace's file — as the one `duck://`
/// addresses on its network resolve to, and say what changed.
fn pick_workspace(root: &Path, config: &Path) -> Result<String, String> {
    let wanted = std::fs::canonicalize(config).map_err(|e| format!("{}: {e}", config.display()))?;
    let registered = workspace_config::registered_networks_in(root)?;
    let found = registered
        .iter()
        .find(|(_, entry)| std::fs::canonicalize(entry.file()).is_ok_and(|file| file == wanted));
    let Some((chain_id, entry)) = found else {
        let rows: Vec<(String, PathBuf)> = registered
            .iter()
            .map(|(id, entry)| (id.clone(), entry.file().to_path_buf()))
            .collect();
        return Err(format!(
            "{} is not a workspace registered under {} — pick one of:\n{}",
            config.display(),
            root.display(),
            workspace_config::workspace_choices(&rows)
        ));
    };
    let chain: ChainId = chain_id.parse().map_err(|refused: Refused| {
        format!("network {chain_id} is one a duck:// address cannot name: {refused}")
    })?;
    let file = entry.file();
    let current = workspace_config::picked_workspace_in(root, &chain)?;
    if current.as_deref() == Some(file) {
        return Ok(format!(
            "duck:// addresses on {chain_id} already resolve to {}",
            file.display()
        ));
    }
    workspace_config::pick_workspace_in(root, &chain, file)?;
    Ok(format!(
        "duck:// addresses on {chain_id} resolve to {}",
        file.display()
    ))
}

/// one network's working Git door: the authority an address spells, the
/// workspace the helper resolves it through, and the `<owner>`s it serves.
#[derive(Debug)]
struct Door {
    authority: String,
    workspace: PathBuf,
    owners: Vec<String>,
}

impl std::fmt::Display for Door {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "duck://{}/forge/<owner>/<repo> goes through {} for owner {}",
            self.authority,
            self.workspace.display(),
            self.owners.join(", ")
        )
    }
}

/// every registered network the helper can be asked about, resolved and
/// diagnosed exactly as a `git clone duck://…` on it would be: one line per
/// working door, or ONE refusal naming every network that would fail and
/// what it lacks. A registry id no address can spell is skipped — no
/// address names it, so the helper is never asked.
fn git_doors(
    root: &Path,
    gateway: impl Fn(&str) -> Result<String, String>,
    owners: impl Fn(&str) -> Result<Vec<String>, String>,
) -> Result<Vec<Door>, String> {
    let mut chains: Vec<ChainId> = Vec::new();
    for (chain_id, _) in workspace_config::registered_networks_in(root)? {
        let Ok(chain) = chain_id.parse::<ChainId>() else {
            continue;
        };
        let seen = chains
            .iter()
            .any(|known| known.salt_hex() == chain.salt_hex());
        if !seen {
            chains.push(chain);
        }
    }
    let mut doors = Vec::new();
    let mut missing = Vec::new();
    for chain in chains {
        let authority = chain.authority();
        let door = network_door(root, &chain, &gateway)
            .and_then(|door| owners(&door.node).map(|owners| (door, owners)));
        match door {
            Ok((door, owners)) => doors.push(Door {
                authority,
                workspace: door.registered.file().to_path_buf(),
                owners,
            }),
            Err(lacks) => missing.push(format!("network {authority}: {lacks}")),
        }
    }
    match missing.is_empty() {
        true => Ok(doors),
        false => Err(format!(
            "git-remote-duck would refuse these, so nothing was linked:\n{}",
            missing.join("\n")
        )),
    }
}

/// the owners a `duck://…/forge/<owner>/…` address on the network behind
/// `node` can name, read off the node's committed gateway state.
fn door_owners(node: &str) -> Result<Vec<String>, String> {
    let handles = registered_handles(node)?;
    git_owners(&handles, |account| {
        published_git_route(node, account).map(|route| route.is_some())
    })
}

/// the handles whose account publishes a `git` route — each one an `<owner>`
/// with a door. None is refused by the verb that supplies what is missing.
fn git_owners(
    handles: &[(String, u64)],
    publishes: impl Fn(u64) -> Result<bool, String>,
) -> Result<Vec<String>, String> {
    if handles.is_empty() {
        return Err(
            "no account on it has a handle, so no `<owner>` names anyone — set one with \
             `ducktape account set-handle --handle <name>`"
                .into(),
        );
    }
    let mut owners = Vec::new();
    for (handle, account) in handles {
        if publishes(*account)? {
            owners.push(handle.clone());
        }
    }
    if owners.is_empty() {
        let named: Vec<&str> = handles.iter().map(|(handle, _)| handle.as_str()).collect();
        return Err(format!(
            "no account with a handle ({}) publishes a `{GIT_ROUTE_LABEL}` route — on the node \
             that runs forge's Git service, `ducktape gateway bind --label {GIT_ROUTE_LABEL} …` \
             then `ducktape forge publish`",
            named.join(", ")
        ));
    }
    Ok(owners)
}

/// the gateway's own page ceiling for a handle listing.
const HANDLE_PAGE: u64 = 256;

/// every registered `.duck` handle and the account it names.
fn registered_handles(node: &str) -> Result<Vec<(String, u64)>, String> {
    let mut handles = Vec::new();
    loop {
        let query = gateway::GatewayQuery::Registrations {
            from: handles.len() as u64,
            limit: HANDLE_PAGE,
        };
        let reply = crate::cred_cli::query_gateway(node, &query)
            .map_err(|error| format!("read the handles on the node at {node}: {error}"))?;
        let gateway::GatewayReply::Registrations(page) = reply else {
            return Err(format!("unexpected gateway reply: {reply:?}"));
        };
        let last_page = (page.len() as u64) < HANDLE_PAGE;
        handles.extend(page.into_iter().map(|row| (row.handle, row.account_id)));
        if last_page {
            return Ok(handles);
        }
    }
}

/// `account`'s published `git` route, if any.
fn published_git_route(node: &str, account: u64) -> Result<Option<gateway::RouteRecord>, String> {
    let query = gateway::GatewayQuery::Get {
        account_id: account,
        name: gateway::RouteName::named(GIT_ROUTE_LABEL),
    };
    let reply = crate::cred_cli::query_gateway(node, &query)
        .map_err(|error| format!("read the git route of account {account}: {error}"))?;
    match reply {
        gateway::GatewayReply::Route(route) => Ok(*route),
        other => Err(format!("unexpected gateway reply: {other:?}")),
    }
}

/// `ducktape forge publish`.
fn publish(args: PublishArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut stdin = std::io::BufReader::new(std::io::stdin());
    let ctx = crate::cred_cli::VerbCtx {
        addr: args.addr,
        key: args.key,
    };
    let base = ctx.http_base()?;
    let publisher = crate::cred_cli::Publisher::of_node(&base)?;
    let user = ctx.signer(&mut stdin)?;
    let account = crate::account_cli::own_account(&base, user.public_key().as_ref())?.number;
    let current = published_git_route(&base, account)?;
    let Some(statement) = git_route(&publisher, account, current.as_ref()) else {
        println!("the {GIT_ROUTE_LABEL} route of account {account} is already published here");
        return Ok(());
    };
    let preimage = gateway::route_signing_preimage(&statement)?;
    let revision = statement.revision;
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
    let height = crate::cred_cli::submit_gateway(&base, &user, &message)?;
    println!(
        "published the {GIT_ROUTE_LABEL} route of account {account} (revision {revision}) at height {height}"
    );
    Ok(())
}

/// the `git` route `account` publishes on `publisher`, continuing the
/// revision stream of `current`; `None` when `current` already says exactly
/// that. No byte cap either way: a push is a whole history and carries no
/// length to check (who may push is forge's own gate, the push certificate),
/// and a clone is the whole history the Gateway streams back.
fn git_route(
    publisher: &crate::cred_cli::Publisher,
    account: u64,
    current: Option<&gateway::RouteRecord>,
) -> Option<gateway::RouteStatement> {
    let route = Some(gateway::RouteDefinition {
        target: gateway::RouteTarget::LoopbackHttp,
        policy: gateway::RoutePolicy {
            audience: gateway::RouteAudience::Network,
            methods: vec![gateway::RouteMethod::Get, gateway::RouteMethod::Post],
            max_request_bytes: None,
            max_response_bytes: 0,
            allow_authorization: false,
            allow_upgrade: false,
        },
    });
    let current = current.map(|record| &record.statement);
    let unchanged = current.is_some_and(|statement| {
        statement.route == route && statement.publisher_node == publisher.node
    });
    if unchanged {
        return None;
    }
    Some(gateway::RouteStatement {
        chain_id: publisher.chain_id.clone(),
        account_id: account,
        name: gateway::RouteName::named(GIT_ROUTE_LABEL),
        publisher_node: publisher.node.clone(),
        revision: current.map_or(1, |statement| statement.revision + 1),
        route,
    })
}

/// the chain id the node at `node` serves, off its `/v1/status`.
fn status_chain_id(node: &str) -> Result<String, String> {
    let status = crate::node_http::get_json(node, "/v1/status")
        .map_err(|error| format!("read the status of the node at {node}: {error}"))?;
    match status["chain_id"].as_str() {
        Some(chain_id) if !chain_id.is_empty() => Ok(chain_id.to_string()),
        _ => Err(format!("the node at {node} serves no chain")),
    }
}

/// register `node` as the remote workspace of `chain_id`, and say what changed.
fn register_remote(root: &Path, chain_id: &str, node: &str) -> Result<String, String> {
    let chain: ChainId = chain_id.parse().map_err(|refused: Refused| {
        format!(
            "the node at {node} serves network {chain_id}, which a duck:// address cannot name: {refused}"
        )
    })?;
    let registered = workspace_config::registered_on(root, &chain)?;
    let entry = match registered.as_slice() {
        [] => None,
        [(registered_id, entry)] => Some((registered_id, entry)),
        several => {
            let rows: Vec<(String, PathBuf)> = several
                .iter()
                .map(|(id, entry)| (id.clone(), entry.file().to_path_buf()))
                .collect();
            return Err(format!(
                "network {chain_id} is already registered more than once — keep one:\n{}",
                workspace_config::workspace_choices(&rows)
            ));
        }
    };
    let remote = RemoteWorkspace {
        chain_id: chain_id.to_string(),
        node: node.to_string(),
    };
    let Some((registered_id, entry)) = entry else {
        let dir = root.join(chain.authority());
        if dir.join("network.toml").exists() {
            return Err(format!(
                "{} holds a local workspace; not registering a remote one inside it",
                dir.display()
            ));
        }
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let file = dir.join(workspace_config::REMOTE_WORKSPACE_FILE);
        remote.save(&file)?;
        return Ok(format!(
            "registered remote workspace {chain_id}: node {node} ({})",
            file.display()
        ));
    };
    let (file, was) = match entry {
        Registered::Local(node_toml) => {
            return Err(format!(
                "network {chain_id} already has a workspace on this machine ({}); git remotes \
                 on it resolve to that node",
                node_toml.display()
            ));
        }
        Registered::Remote { file, node } => (file, node),
    };
    if registered_id != chain_id {
        return Err(format!(
            "{} registers this network as {registered_id}, but the node at {node} reports \
             {chain_id}",
            file.display()
        ));
    }
    if was == node {
        return Ok(format!(
            "remote workspace {chain_id} already registered: node {node} ({})",
            file.display()
        ));
    }
    remote.save(file)?;
    Ok(format!(
        "remote workspace {chain_id}: node {was} -> {node} ({})",
        file.display()
    ))
}

/// the `ducktape` this process was run as: argv[0] as the shell ran it — a
/// path, or a bare name found on PATH. Not `current_exe()`: that resolves
/// symlinks, and a launcher install's `current` link would then pin the
/// helper to one release directory.
fn invoked_binary() -> Result<PathBuf, String> {
    let argv0 = PathBuf::from(std::env::args_os().next().ok_or("no argv[0]")?);
    if argv0.components().count() > 1 {
        return Ok(argv0);
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(&argv0))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            format!(
                "{} is not on PATH, so a link beside it would not be either — run setup through \
                 the ducktape on your PATH",
                argv0.display()
            )
        })
}

/// link `<dir>/git-remote-duck` to `target` (a name in `dir`), and say what
/// changed. A link is relative, so it follows whatever `ducktape` in `dir`
/// becomes; a file that is not a link is someone else's and is left alone.
fn link_helper(dir: &Path, target: &Path) -> Result<String, String> {
    let link = dir.join(HELPER);
    let shown = link.display();
    let current = match std::fs::symlink_metadata(&link) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("inspect {shown}: {e}")),
        Ok(meta) if !meta.file_type().is_symlink() => {
            return Err(format!(
                "{shown} exists and is not a link to ducktape — move it aside and run setup again"
            ));
        }
        Ok(_) => Some(std::fs::read_link(&link).map_err(|e| format!("read {shown}: {e}"))?),
    };
    let Some(current) = current else {
        std::os::unix::fs::symlink(target, &link).map_err(|e| format!("link {shown}: {e}"))?;
        return Ok(format!("linked {shown} -> {}", target.display()));
    };
    if current == target {
        return Ok(format!("{shown} already links to {}", target.display()));
    }
    std::fs::remove_file(&link).map_err(|e| format!("replace {shown}: {e}"))?;
    std::os::unix::fs::symlink(target, &link).map_err(|e| format!("link {shown}: {e}"))?;
    Ok(format!(
        "relinked {shown} -> {} (was -> {})",
        target.display(),
        current.display()
    ))
}

/// which configuration a rewrite is written to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scope {
    Repository,
    User,
}

impl Scope {
    fn flag(self) -> &'static str {
        match self {
            Scope::Repository => "--local",
            Scope::User => "--global",
        }
    }

    fn shown(self) -> &'static str {
        match self {
            Scope::Repository => "this repository's git configuration",
            Scope::User => "the user's git configuration",
        }
    }
}

/// the `duck://<network>/forge/<owner>/` a rewrite points at: the one open
/// door and owner, narrowed by `--owner` when a machine has several.
fn rewrite_base(doors: &[Door], owner: Option<&str>) -> Result<String, String> {
    let mut bases: Vec<String> = doors
        .iter()
        .flat_map(|door| {
            door.owners
                .iter()
                .filter(|have| owner.is_none_or(|want| have.as_str() == want))
                .map(|have| format!("duck://{}/forge/{have}/", door.authority))
        })
        .collect();
    bases.sort();
    bases.dedup();
    match bases.as_slice() {
        [base] => Ok(base.clone()),
        [] => Err(match owner {
            Some(want) => format!("no registered network has a Git door for owner {want}"),
            None => "no registered network has a Git door to rewrite to".into(),
        }),
        many => Err(format!(
            "a rewrite points at one Forge and these are open: {} — pick one with --owner <handle>",
            many.join(", ")
        )),
    }
}

/// run `git config` in `dir` at `scope`.
fn git_config(dir: &Path, scope: Scope, args: &[&str]) -> Result<std::process::Output, String> {
    std::process::Command::new("git")
        .current_dir(dir)
        .arg("config")
        .arg(scope.flag())
        .args(args)
        .output()
        .map_err(|e| format!("run git config: {e}"))
}

/// point every git URL under `prefix` at `base`, and say what changed. A
/// rewrite already in place is left alone; a prefix another base already
/// claims is refused rather than doubled, because git picking between two
/// claims on one prefix is not something a dependency graph can depend on.
fn rewrite_url(dir: &Path, scope: Scope, prefix: &str, base: &str) -> Result<String, String> {
    let read = git_config(dir, scope, &["--get-regexp", r"^url\..*\.insteadof$"])?;
    // one match exits 0, no match exits 1, and anything else is git refusing
    // to read that scope at all (no repository here, say).
    let listed = match read.status.code() {
        Some(0) => String::from_utf8_lossy(&read.stdout).into_owned(),
        Some(1) => String::new(),
        _ => {
            return Err(format!(
                "git config {}: {}",
                scope.flag(),
                String::from_utf8_lossy(&read.stderr).trim()
            ));
        }
    };
    let key = format!("url.{base}.insteadOf");
    for line in listed.lines() {
        let Some((name, value)) = line.split_once(' ') else {
            continue;
        };
        if value != prefix {
            continue;
        }
        if name.eq_ignore_ascii_case(&key) {
            return Ok(format!(
                "{} already rewrites {prefix} to {base}",
                scope.shown()
            ));
        }
        return Err(format!(
            "{} already rewrites {prefix} to {} — unset that one first (`git config {} --unset \
             {name} {prefix}`)",
            scope.shown(),
            name.trim_start_matches("url.")
                .strip_suffix(".insteadof")
                .unwrap_or(name),
            scope.flag()
        ));
    }
    let added = git_config(dir, scope, &["--add", &key, prefix])?;
    if !added.status.success() {
        return Err(format!(
            "git config {} --add {key}: {}",
            scope.flag(),
            String::from_utf8_lossy(&added.stderr).trim()
        ));
    }
    Ok(format!("{} rewrites {prefix} to {base}", scope.shown()))
}

/// the cargo home whose configuration this machine's cargo reads.
fn cargo_home() -> Result<PathBuf, String> {
    cargo_home_from(std::env::var_os("CARGO_HOME"), std::env::var_os("HOME"))
}

fn cargo_home_from(
    cargo_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Result<PathBuf, String> {
    if let Some(dir) = cargo_home.filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = home
        .filter(|dir| !dir.is_empty())
        .ok_or("neither CARGO_HOME nor HOME is set, so this machine has no cargo configuration")?;
    Ok(PathBuf::from(home).join(".cargo"))
}

/// set `net.git-fetch-with-cli` in `<cargo home>/config.toml`, and say what
/// changed. Cargo's own fetcher cannot run a remote helper, so without this
/// a `duck://` dependency never reaches `git-remote-duck`. A `[net]` table
/// that is already there is left for the human: appending a second one is
/// not valid TOML, and rewriting the file would cost every comment in it.
fn cargo_fetch_with_cli(cargo_home: &Path) -> Result<String, String> {
    let legacy = cargo_home.join("config");
    let file = match cargo_home.join("config.toml") {
        // cargo reads `config` only while `config.toml` is absent, so
        // creating one beside it would silently orphan the human's file.
        preferred if preferred.exists() || !legacy.exists() => preferred,
        _ => legacy,
    };
    let shown = file.display();
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("read {shown}: {e}")),
    };
    let parsed: toml::Value = toml::from_str(&text).map_err(|e| format!("{shown}: {e}"))?;
    let net = parsed.get("net");
    if let Some(net) = net {
        return match net.get("git-fetch-with-cli").and_then(toml::Value::as_bool) {
            Some(true) => Ok(format!("{shown} already sets net.git-fetch-with-cli")),
            _ => Err(format!(
                "{shown} has a [net] table already — set `git-fetch-with-cli = true` under it, \
                 which is what cargo needs to run git-remote-duck"
            )),
        };
    }
    std::fs::create_dir_all(cargo_home).map_err(|e| format!("{}: {e}", cargo_home.display()))?;
    let mut next = text;
    if !next.is_empty() {
        if !next.ends_with('\n') {
            next.push('\n');
        }
        next.push('\n');
    }
    next.push_str("[net]\ngit-fetch-with-cli = true\n");
    std::fs::write(&file, next).map_err(|e| format!("write {shown}: {e}"))?;
    Ok(format!("{shown} now sets net.git-fetch-with-cli = true"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> tempfile::TempDir {
        tempfile::tempdir().expect("scratch ducktape home")
    }

    fn gateway(base: &'static str) -> impl FnOnce(&str) -> Result<String, String> {
        move |_node| Ok(base.to_string())
    }

    #[derive(clap::Parser)]
    struct TestCli {
        #[command(subcommand)]
        cmd: ForgeCmd,
    }

    fn parse(line: &str) -> Result<ForgeCmd, clap::Error> {
        use clap::Parser as _;
        TestCli::try_parse_from(line.split(' ')).map(|cli| cli.cmd)
    }

    /// `setup` picks a workspace by the path the refusal lists, never beside
    /// `--node` (which registers one); `publish` addresses its node the way
    /// every signing verb does and takes no password argument.
    #[test]
    fn setup_and_publish_parse_their_selectors() {
        let ForgeCmd::Setup(setup) = parse("t setup --config /h/net/node.toml").unwrap() else {
            panic!("a setup");
        };
        assert_eq!(setup.config, Some(PathBuf::from("/h/net/node.toml")));
        assert!(setup.node.is_none());
        assert!(
            parse("t setup --config /h/net/node.toml --node http://127.0.0.1:1").is_err(),
            "one selector"
        );
        let ForgeCmd::Publish(publish) =
            parse("t publish --config /h/net/node.toml --key /h/k").unwrap()
        else {
            panic!("a publish");
        };
        assert_eq!(publish.addr.config, Some(PathBuf::from("/h/net/node.toml")));
        assert_eq!(publish.key, Some(PathBuf::from("/h/k")));
        let ForgeCmd::Publish(publish) = parse("t publish -n forgedry#65e7feac").unwrap() else {
            panic!("a publish");
        };
        assert_eq!(publish.addr.network.as_deref(), Some("forgedry#65e7feac"));
        assert!(parse("t publish --password x").is_err(), "no password flag");
        assert!(parse("t publish --label api").is_err(), "the label is git");
    }

    /// The first publish is revision 1 on the dialed node; a rerun that would
    /// say the same is nothing; a move to another node continues the stream.
    #[test]
    fn publishing_the_git_route_continues_its_revision_stream() {
        let here = crate::cred_cli::Publisher {
            chain_id: "forgedry#65e7feac".into(),
            node: vec![1; 32],
        };
        let first = git_route(&here, 7, None).expect("nothing published yet");
        assert_eq!(first.revision, 1);
        assert_eq!(first.account_id, 7);
        assert_eq!(first.name, gateway::RouteName::named(GIT_ROUTE_LABEL));
        let policy = &first.route.as_ref().unwrap().policy;
        assert_eq!(policy.max_request_bytes, None, "a push is uncapped");
        assert_eq!(policy.max_response_bytes, 0, "a clone is uncapped");
        let record = |statement: gateway::RouteStatement| gateway::RouteRecord {
            statement,
            authorization: gateway::MemberAuthorization {
                signer: Vec::new(),
                signature: Vec::new(),
            },
        };
        let published = record(first);
        assert_eq!(git_route(&here, 7, Some(&published)), None, "already so");
        let there = crate::cred_cli::Publisher {
            chain_id: here.chain_id.clone(),
            node: vec![2; 32],
        };
        let moved = git_route(&there, 7, Some(&published)).expect("another node");
        assert_eq!(moved.revision, 2);
        assert_eq!(moved.publisher_node, vec![2; 32]);
    }

    /// A door needs an owner: a handle, whose account publishes `git`. Each
    /// missing piece is named with the verb that supplies it.
    #[test]
    fn a_door_without_a_handle_or_a_git_route_names_the_verb_it_lacks() {
        let none = git_owners(&[], |_| panic!("no account to ask")).expect_err("no handle");
        assert!(none.contains("ducktape account set-handle"), "{none}");
        let handles = [("alice".to_string(), 3), ("bob".to_string(), 4)];
        let unpublished = git_owners(&handles, |_| Ok(false)).expect_err("no route");
        assert!(unpublished.contains("alice, bob"), "{unpublished}");
        assert!(
            unpublished.contains("ducktape forge publish"),
            "{unpublished}"
        );
        assert!(
            unpublished.contains("gateway bind --label git"),
            "{unpublished}"
        );
        assert_eq!(
            git_owners(&handles, |account| Ok(account == 4)).unwrap(),
            ["bob"]
        );
    }

    /// THE BUG (#2714): a validator and its resident of ONE network in one
    /// home made setup succeed and every address fail. Setup resolves each
    /// network exactly as the helper does, so it refuses what the helper
    /// would, naming the flag; once picked, both resolve to the pick.
    #[test]
    fn setup_refuses_a_network_the_helper_would_and_passes_it_once_picked() {
        let root = home();
        let mut files = Vec::new();
        for (dir, node) in [
            ("net", "http://127.0.0.1:18844"),
            ("net-joiner", "http://127.0.0.1:18944"),
        ] {
            let dir = root.path().join(dir);
            std::fs::create_dir_all(&dir).unwrap();
            let file = dir.join(workspace_config::REMOTE_WORKSPACE_FILE);
            RemoteWorkspace {
                chain_id: "forgedry#65e7feac".into(),
                node: node.into(),
            }
            .save(&file)
            .unwrap();
            files.push(file);
        }
        let gateway = |_: &str| -> Result<String, String> { Ok("http://127.0.0.1:18845".into()) };
        let alice = |_: &str| -> Result<Vec<String>, String> { Ok(vec!["alice".into()]) };
        let address = "duck://forgedry-65e7feac/forge/alice/ducktape";

        let refused = git_doors(root.path(), gateway, alice).expect_err("ambiguous");
        let helper = git_target(root.path(), address, gateway).expect_err("ambiguous");
        assert!(
            refused.contains(&helper),
            "setup says what the helper says:\n{refused}"
        );
        assert!(refused.contains("--config"), "{refused}");

        let picked = pick_workspace(root.path(), &files[1]).unwrap();
        assert!(picked.contains("resolve to"), "{picked}");
        let again = pick_workspace(root.path(), &files[1]).unwrap();
        assert!(again.contains("already"), "{again}");
        let doors = git_doors(root.path(), gateway, alice).expect("picked");
        assert_eq!(doors.len(), 1, "{doors:?}");
        assert!(doors[0].to_string().contains("net-joiner"), "{doors:?}");
        let target = git_target(root.path(), address, |node| {
            assert_eq!(node, "http://127.0.0.1:18944", "the pick's node");
            Ok("http://127.0.0.1:18845".into())
        })
        .expect("the helper resolves the pick");
        assert_eq!(target.authority, "git.alice.duck");

        let dead = git_doors(root.path(), gateway, |_| Err("no handle".into())).unwrap_err();
        assert!(
            dead.contains("network forgedry-65e7feac: no handle"),
            "{dead}"
        );
        let stranger = root.path().join("stranger.toml");
        std::fs::write(&stranger, "").unwrap();
        let unregistered = pick_workspace(root.path(), &stranger).expect_err("not registered");
        assert!(unregistered.contains("net-joiner"), "{unregistered}");
    }

    /// The address becomes the `git` route of the owner's account, the
    /// repository its path, the node the ONE registered entry on that network
    /// — here a remote one, which resolves exactly as a local one does.
    #[test]
    fn an_address_resolves_to_the_owners_git_route_on_the_registered_node() {
        let root = home();
        register_remote(root.path(), "dognet#b5b6ea90", "http://127.0.0.1:18844").unwrap();
        let target = git_target(
            root.path(),
            "duck://dognet-b5b6ea90/forge/alice/my-crate",
            |node| {
                assert_eq!(
                    node, "http://127.0.0.1:18844",
                    "the registered node is asked"
                );
                Ok("http://127.0.0.1:18845".into())
            },
        )
        .expect("resolves");
        assert_eq!(
            target,
            GitTarget {
                url: "http://127.0.0.1:18845/my-crate".into(),
                authority: "git.alice.duck".into(),
            }
        );
    }

    /// Each refusal is the parser's or the registry's own sentence, and the
    /// node is never asked when the address or the network is refused.
    #[test]
    fn a_refused_address_or_network_never_reaches_a_node() {
        let root = home();
        register_remote(root.path(), "dognet#b5b6ea90", "http://127.0.0.1:18844").unwrap();
        let never = |_: &str| -> Result<String, String> { panic!("no node is asked") };
        for (address, says) in [
            (
                "duck://dognet-b5b6ea90/forge/alice/my-crate.git",
                "Drop the `.git`",
            ),
            ("duck://dognet-b5b6ea90/forge/my-crate", "two segments"),
            (
                "duck://dognet-b5b6ea90:443/forge/alice/my-crate",
                "no credentials, port",
            ),
            (
                "duck://other-0badf00d/forge/alice/my-crate",
                "on network other-0badf00d",
            ),
            (
                "duck://mainnet-b5b6ea90/forge/alice/my-crate",
                "knows b5b6ea90 as dognet#b5b6ea90",
            ),
        ] {
            let refused = git_target(root.path(), address, never).expect_err(address);
            assert!(refused.contains(says), "{address}: {refused}");
        }
    }

    /// A node dialed across the network reports a gateway on ITS loopback;
    /// dialing that base here would reach this machine, so it is refused by
    /// name. A loopback node base (this machine's node, or a forwarded one)
    /// shares the gateway's loopback and passes.
    #[test]
    fn a_remote_nodes_loopback_gateway_is_refused_not_dialed_here() {
        let root = home();
        register_remote(root.path(), "far#0badf00d", "http://10.0.0.5:8844").unwrap();
        let refused = git_target(
            root.path(),
            "duck://far-0badf00d/forge/alice/my-crate",
            gateway("http://127.0.0.1:8845"),
        )
        .expect_err("a loopback gateway on another machine");
        assert!(refused.contains("own loopback"), "{refused}");
        assert!(reachable_gateway("http://localhost:8844", "http://127.0.0.1:8845").is_ok());
        assert!(reachable_gateway("http://[::1]:8844", "http://127.0.0.1:8845").is_ok());
        assert_eq!(
            reachable_gateway("http://10.0.0.5:8844", "http://127.0.0.1:8845")
                .expect_err("refused")
                .reason,
            "gateway_not_local"
        );
    }

    /// Setup's registration is idempotent and says what it did each time:
    /// added, unchanged, moved. A network with a local workspace is refused —
    /// a second entry would make every address on it ambiguous.
    #[test]
    fn registering_a_remote_node_is_idempotent_and_names_each_change() {
        let root = home();
        let first =
            register_remote(root.path(), "dognet#b5b6ea90", "http://10.0.0.5:8844").unwrap();
        assert!(
            first.starts_with("registered remote workspace dognet#b5b6ea90"),
            "{first}"
        );
        let again =
            register_remote(root.path(), "dognet#b5b6ea90", "http://10.0.0.5:8844").unwrap();
        assert!(again.contains("already registered"), "{again}");
        let moved =
            register_remote(root.path(), "dognet#b5b6ea90", "http://10.0.0.6:8844").unwrap();
        assert!(
            moved.contains("http://10.0.0.5:8844 -> http://10.0.0.6:8844"),
            "{moved}"
        );
        assert_eq!(
            workspace_config::registered_networks_in(root.path())
                .unwrap()
                .len(),
            1,
            "one entry, however often setup ran"
        );
        let relabelled = register_remote(root.path(), "mainnet#b5b6ea90", "http://10.0.0.6:8844")
            .expect_err("another label for a registered salt");
        assert!(
            relabelled.contains("registers this network as dognet#b5b6ea90"),
            "{relabelled}"
        );

        let local = root.path().join("founder");
        std::fs::create_dir_all(&local).unwrap();
        workspace_config::NetworkDescriptor {
            chain_id: "kitchen#99887766".into(),
            validators: vec![],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: workspace_config::DEFAULT_BLOCK_TIME_MS,
            genesis: String::new(),
            modules: Vec::new(),
        }
        .save(&local.join("network.toml"))
        .unwrap();
        let shadowed = register_remote(root.path(), "kitchen#99887766", "http://10.0.0.7:8844")
            .expect_err("a network with a local workspace");
        assert!(shadowed.contains("already has a workspace"), "{shadowed}");
    }

    /// The link is idempotent: created, then left alone, then repointed when
    /// the binary's name changed; a real file in its place is never touched.
    #[test]
    fn linking_the_helper_is_idempotent_and_never_clobbers_a_file() {
        let bin = tempfile::tempdir().unwrap();
        let created = link_helper(bin.path(), Path::new("ducktape")).unwrap();
        assert!(created.starts_with("linked "), "{created}");
        assert_eq!(
            std::fs::read_link(bin.path().join(HELPER)).unwrap(),
            Path::new("ducktape")
        );
        let unchanged = link_helper(bin.path(), Path::new("ducktape")).unwrap();
        assert!(
            unchanged.contains("already links to ducktape"),
            "{unchanged}"
        );
        let relinked = link_helper(bin.path(), Path::new("ducktape-next")).unwrap();
        assert!(relinked.contains("(was -> ducktape)"), "{relinked}");

        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join(HELPER), "#!/bin/sh\n").unwrap();
        let refused = link_helper(other.path(), Path::new("ducktape")).expect_err("a file");
        assert!(refused.contains("is not a link"), "{refused}");
        assert_eq!(
            std::fs::read_to_string(other.path().join(HELPER)).unwrap(),
            "#!/bin/sh\n"
        );
    }

    fn door(authority: &str, owners: &[&str]) -> Door {
        Door {
            authority: authority.into(),
            workspace: PathBuf::from("/tmp/net/node.toml"),
            owners: owners.iter().map(|o| (*o).to_string()).collect(),
        }
    }

    #[test]
    fn a_rewrite_points_at_one_forge_and_names_the_others() {
        let one = [door("forgenet-ec8c6586", &["alice"])];
        assert_eq!(
            rewrite_base(&one, None).unwrap(),
            "duck://forgenet-ec8c6586/forge/alice/"
        );

        let several = [door("forgenet-ec8c6586", &["alice", "bob"])];
        let refused = rewrite_base(&several, None).expect_err("ambiguous");
        assert!(refused.contains("--owner"), "{refused}");
        assert!(refused.contains("forge/bob/"), "{refused}");
        assert_eq!(
            rewrite_base(&several, Some("bob")).unwrap(),
            "duck://forgenet-ec8c6586/forge/bob/"
        );

        let unknown = rewrite_base(&several, Some("carol")).expect_err("no such owner");
        assert!(unknown.contains("carol"), "{unknown}");
    }

    #[test]
    fn a_rewrite_is_written_once_and_never_doubled() {
        let repo = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(repo.path())
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8(out.stdout).unwrap()
        };
        git(&["init", "-q"]);
        let prefix = "https://github.com/ducktape-industries/";
        let base = "duck://forgenet-ec8c6586/forge/alice/";

        let wrote = rewrite_url(repo.path(), Scope::Repository, prefix, base).unwrap();
        assert!(wrote.contains("rewrites"), "{wrote}");
        assert_eq!(
            git(&[
                "config",
                "--local",
                "--get-all",
                &format!("url.{base}.insteadOf")
            ])
            .trim(),
            prefix
        );

        let again = rewrite_url(repo.path(), Scope::Repository, prefix, base).unwrap();
        assert!(again.contains("already"), "{again}");
        assert_eq!(
            git(&[
                "config",
                "--local",
                "--get-all",
                &format!("url.{base}.insteadOf")
            ])
            .lines()
            .count(),
            1,
            "a rerun writes nothing"
        );

        let other = "duck://forgenet-ec8c6586/forge/bob/";
        let refused = rewrite_url(repo.path(), Scope::Repository, prefix, other)
            .expect_err("the prefix is claimed");
        assert!(refused.contains("alice"), "{refused}");
        assert!(refused.contains("--unset"), "{refused}");
    }

    #[test]
    fn cargo_home_is_the_env_then_home() {
        use std::ffi::OsString;
        assert_eq!(
            cargo_home_from(Some(OsString::from("/opt/cargo")), Some("/home/a".into())).unwrap(),
            PathBuf::from("/opt/cargo")
        );
        assert_eq!(
            cargo_home_from(Some(OsString::new()), Some("/home/a".into())).unwrap(),
            PathBuf::from("/home/a/.cargo")
        );
        let refused = cargo_home_from(None, None).expect_err("nowhere to write");
        assert!(refused.contains("CARGO_HOME"), "{refused}");
    }

    #[test]
    fn the_cargo_setting_is_written_once_and_an_existing_net_table_is_left_alone() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".cargo");
        let file = dir.join("config.toml");

        let wrote = cargo_fetch_with_cli(&dir).unwrap();
        assert!(wrote.contains("now sets"), "{wrote}");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "[net]\ngit-fetch-with-cli = true\n"
        );

        let again = cargo_fetch_with_cli(&dir).unwrap();
        assert!(again.contains("already"), "{again}");

        std::fs::write(&file, "[net]\nretry = 3\n").unwrap();
        let refused = cargo_fetch_with_cli(&dir).expect_err("someone else's [net]");
        assert!(refused.contains("git-fetch-with-cli = true"), "{refused}");

        // keeping what is there: the setting is appended after it, not over it
        std::fs::remove_file(&file).unwrap();
        std::fs::write(&file, "[build]\njobs = 4").unwrap();
        cargo_fetch_with_cli(&dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "[build]\njobs = 4\n\n[net]\ngit-fetch-with-cli = true\n"
        );
    }

    #[test]
    fn a_legacy_cargo_config_is_written_in_place() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config"), "[build]\njobs = 4\n").unwrap();
        let wrote = cargo_fetch_with_cli(dir.path()).unwrap();
        assert!(
            wrote.ends_with("now sets net.git-fetch-with-cli = true"),
            "{wrote}"
        );
        assert!(
            !dir.path().join("config.toml").exists(),
            "a config.toml beside it would orphan the operator's config"
        );
        assert!(
            std::fs::read_to_string(dir.path().join("config"))
                .unwrap()
                .contains("git-fetch-with-cli = true")
        );
    }
}
