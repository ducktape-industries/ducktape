//! the typed `ducktape node` grammar — clap derive. this file is only the
//! SHAPE (verbs, flags, help text); the handlers live in `cli.rs`, the run
//! path in `main.rs`. required arguments are enforced (and rendered) by clap,
//! optional ones are optional because every one has a working default.

use std::path::PathBuf;

use crate::config;

/// the `ducktape node` verb tree. `run` is the node-boot path (owned by
/// `main.rs`); every other verb is a synchronous operator command.
// parsed once on the stack and immediately consumed — variant size is noise.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, clap::Subcommand)]
pub enum NodeCmd {
    /// run a workspace's node until killed (^C checkpoints and exits)
    Run(RunArgs),
    #[command(flatten)]
    Op(OpCmd),
}

/// the operator verbs — everything under `ducktape node` except `run`.
#[derive(Debug, clap::Subcommand)]
pub enum OpCmd {
    /// generate or reuse a node identity (prints its pubkey)
    Key(KeyArgs),
    /// found a new network (default dir: ~/.ducktape/<chain-id>)
    Init(InitArgs),
    /// mint a single-use bearer invite blob
    Invite(InviteArgs),
    /// pre-genesis: add a key to the validator set
    Admit(AdmitArgs),
    /// materialize a workspace from an invite blob
    Join(JoinCmd),
    /// list registered workspaces (chain-id + config path)
    List,
    /// the running node's tip, and how far behind the network it is (reads
    /// the local rpc)
    Status(StatusArgs),
    /// can THIS binary run this workspace? reopens the checkpoint offline and
    /// recomposes its committed root hash — what a release launcher asks a
    /// staged binary before it flips. the node must be STOPPED
    Qualify(QualifyArgs),
    /// the running node's height and direct peers: connection, traffic, and
    /// the heights this node served each over state sync
    ///
    /// no row carries a peer's own height: the mesh gossips none. how far
    /// behind the network this node is: `ducktape node status`
    Peers(StatusArgs),
    /// resident standing: the staged-admission tier
    #[command(subcommand)]
    Resident(ResidentCmd),
    /// consensus-quorum membership
    #[command(subcommand)]
    Member(MemberCmd),
    /// whose work this node will execute
    #[command(subcommand)]
    Work(WorkCmd),
    /// whether this node isolates provider runs, and turn it on
    Sandbox(SandboxArgs),
    /// retune a RUNNING node's tracing filter (the 3am verb)
    LogFilter(LogFilterArgs),
    /// the reachability plane's netstack backend
    #[command(subcommand)]
    Netstack(NetstackCmd),
}

#[derive(Debug, clap::Subcommand)]
pub enum NetstackCmd {
    /// move a RUNNING node's plane onto another backend, mid-life
    Swap(NetstackSwapArgs),
}

/// `ducktape node netstack swap --component <PATH>` — the operator
/// client for `POST /v1/admin/netstack/swap`.
///
/// The swap is node-local and epoch-safe: the running machine's snapshot
/// restores into the new backend and the epoch continues, with no retarget and
/// no tunnel flap. A backend that cannot restore the snapshot is REFUSED and
/// the current machine keeps running — this verb reports that and does not
/// retry, because a component built against another contract is refused by
/// name every time.
#[derive(Debug, clap::Args)]
pub struct NetstackSwapArgs {
    /// run a `ducktape:netstack` component at this path ON THE NODE's disk
    #[arg(long, value_name = "PATH")]
    pub component: PathBuf,
    #[command(flatten)]
    pub selector: Selector,
}

/// `ducktape node log-filter <FILTER>` — the signed client for
/// `POST /v1/log-filter`.
///
/// The route MUTATES the running process (a `trace` filter writes into an
/// unrotated `daemon.log` as fast as the disk takes it), so it requires a
/// user-signed request like every other mutating route. `curl` cannot mint one;
/// this verb can.
#[derive(Debug, clap::Args)]
pub struct LogFilterArgs {
    /// the tracing filter to install, e.g. `info,ducktape::join=debug`
    #[arg(value_name = "FILTER")]
    pub filter: String,
    #[command(flatten)]
    pub addr: NodeAddr,
    /// the user key that signs the request (default: the active wallet)
    #[arg(long, value_name = "PATH")]
    pub key: Option<PathBuf>,
    /// re-pin this node's identity to whatever it answers with now — the
    /// only way an already-trusted key changes (see `known_nodes`)
    #[arg(long)]
    pub trust_node: bool,
}

/// `ducktape node sandbox` — reconcile "can this HOST isolate a run" with
/// "does this WORKSPACE say to".
///
/// They are decided at different moments and can disagree for a long time
/// without saying so: the `[sandbox]` table is written once, when `node
/// init`/`join` probed the host, and nothing revisits it. A machine that gained
/// its hypervisor afterwards keeps a node.toml that refuses every provider run,
/// and the only symptom is the compute daemon's boot FATAL — after the setup
/// steps have all reported ready.
#[derive(Debug, clap::Args)]
pub struct SandboxArgs {
    /// write the table without asking (for scripts and non-interactive hosts)
    #[arg(long)]
    pub yes: bool,
    #[command(flatten)]
    pub selector: Selector,
}

/// `ducktape node work` — this node's own answer to "whose workload do I run?".
///
/// A credential GRANT and a work ADMISSION are two consents in OPPOSITE
/// directions, and conflating them is the first thing to get wrong:
/// `user cred grant` is the lender telling the network *which node may draw on
/// my credential*; `node work admit` is a host telling the network *whose work
/// I will run at all*. A cross-node run needs both, on different boxes.
#[derive(Debug, clap::Subcommand)]
pub enum WorkCmd {
    /// print this node's admission policy
    List(SelectorArgs),
    /// run an account's work on this node (or `anyone`)
    Admit(WorkTargetArgs),
    /// stop running an account's work on this node (or `anyone`)
    Revoke(WorkTargetArgs),
}

/// one account, or the literal `anyone`.
#[derive(Debug, clap::Args)]
pub struct WorkTargetArgs {
    /// an account NUMBER, or the literal `anyone`. `anyone` admits every
    /// network member — and lets a stranger's workload draw on every
    /// credential this node has been granted. A display name is refused: it
    /// is freely rewritable and not unique, so it cannot name who this node
    /// trusts (look the number up with `ducktape account show`).
    pub target: String,
    #[command(flatten)]
    pub selector: Selector,
}

#[derive(Debug, clap::Subcommand)]
pub enum ResidentCmd {
    /// grant resident standing to a joiner (drives governance on the running node)
    Accept(PubkeyArgs),
    /// revoke resident standing
    Remove(PubkeyArgs),
}

#[derive(Debug, clap::Subcommand)]
pub enum MemberCmd {
    /// seat a key in the consensus quorum
    Promote(PubkeyArgs),
    /// remove a validator from the set
    Remove(PubkeyArgs),
    /// this node drives its own removal
    Leave(SelectorArgs),
    /// print in-set + validator count for this node
    Status(StatusArgs),
}

/// which workspace a verb operates on. resolution ladder, first hit wins:
/// `-n/--network` (registry), `--config`, `./node.toml` when present, then
/// the single registered workspace when exactly one exists.
#[derive(Debug, Default, clap::Args)]
pub struct Selector {
    /// a registered workspace's chain id — unique prefix ok (`node list`)
    #[arg(
        short = 'n',
        long = "network",
        value_name = "CHAIN-ID",
        conflicts_with = "config"
    )]
    pub network: Option<String>,
    /// explicit path to a workspace's node.toml
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
}

impl Selector {
    /// resolve the ladder to a node.toml path. steps 1–3 are the historical
    /// behavior; step 4 (the lone registered workspace) is what lets a
    /// freshly-init'd machine run `ducktape node run` with no flags at all.
    pub fn config_path(&self) -> Result<PathBuf, String> {
        if let Some(needle) = &self.network {
            return config::find_workspace_config(needle);
        }
        if let Some(path) = &self.config {
            return Ok(path.clone());
        }
        let local = PathBuf::from("node.toml");
        if local.exists() {
            return Ok(local);
        }
        let mut workspaces = config::list_workspaces()?;
        match workspaces.len() {
            1 => {
                let (chain_id, path) = workspaces.swap_remove(0);
                eprintln!("using workspace {chain_id} ({})", path.display());
                Ok(path)
            }
            0 => Err(
                "no workspace selected: no ./node.toml here and no registered workspaces — \
                 found one with `ducktape node init` or `ducktape node join <invite>`"
                    .into(),
            ),
            _ => Err(format!(
                "no workspace selected and several are registered — pick one with -n, or \
                 --config <path> when two share a chain id:\n{}",
                config::workspace_choices(&workspaces)
            )),
        }
    }
}

/// which NODE a verb DIALS: the http base of a running node's `/v1` surface.
/// Deliberately a different question from [`Selector`], which resolves a
/// workspace's node.toml PATH for the daemon that IS the node.
///
/// which WORKSPACE a verb reads or edits — the directory itself, for the verb
/// families whose subject is a file in it (a service grant, a gateway route,
/// the keystore) rather than a node to dial. An explicit `--workspace` wins,
/// else `-n/--network` names one under the ducktape home, else the lone
/// workspace on the box.
#[derive(Debug, clap::Args)]
pub(crate) struct WorkspaceArgs {
    /// this node's config file (`ducktape node run --config`'s twin)
    #[arg(long, value_name = "FILE", global = true)]
    pub(crate) config: Option<PathBuf>,
    /// explicit workspace dir (wins over -n)
    #[arg(long, value_name = "DIR", global = true)]
    pub(crate) workspace: Option<PathBuf>,
    /// a workspace's chain id (`ducktape node list`)
    #[arg(short = 'n', long = "network", value_name = "CHAIN-ID", global = true)]
    pub(crate) network: Option<String>,
}

impl WorkspaceArgs {
    /// the node config this workspace's node is described by.
    ///
    /// `--config` exists because a workspace dir does not always CONTAIN its
    /// config: the dev shape's workspace is its `storage_dir`, named BY a
    /// config that lives elsewhere. `ducktape node run --config` has always
    /// taken the file directly; a daemon serving that node needs the same.
    pub(crate) fn config_file(&self) -> Result<PathBuf, String> {
        match &self.config {
            Some(file) => Ok(file.clone()),
            None => Ok(self.dir()?.join("node.toml")),
        }
    }

    /// the workspace directory — the config's own answer, so the CLI and the
    /// node can never disagree about which directory a file lives in.
    pub(crate) fn dir(&self) -> Result<PathBuf, String> {
        if let Some(file) = &self.config {
            // the keyless read: every verb on this group answers "which
            // workspace?" without ever opening the node's identity.
            return Ok(config::resolve_service(file)?.workspace);
        }
        if let Some(dir) = &self.workspace {
            return Ok(dir.clone());
        }
        if let Some(needle) = &self.network {
            let (dir, _http) = config::resolve_network(needle)?;
            return Ok(dir);
        }
        // the bottom rung `node run` and `node status` already stand on: with
        // exactly one workspace on the box there is nothing to disambiguate,
        // and demanding a selector here made `service list` the only read verb
        // on the box that refused to answer a machine with one network on it.
        let mut workspaces = config::list_workspaces()?;
        match workspaces.len() {
            // `list_workspaces` yields the node.toml PATH, not the directory —
            // these verbs want the workspace that CONTAINS it.
            1 => Ok(config::resolve_network(&workspaces.swap_remove(0).0)?.0),
            0 => Err(
                "no workspace: found one with `ducktape node init --name <name>` \
                      or `ducktape node join <invite>`"
                    .into(),
            ),
            _ => Err(format!(
                "several workspaces exist — pick one with -n, or --config <path> when two \
                 share a chain id:\n{}",
                config::workspace_choices(&workspaces)
            )),
        }
    }
}

/// `--node` is an http base here and means nothing else anywhere: the `agent`
/// family's host targeting — which PEER runs the work, a raw 64-hex node key —
/// is `--host-node`, because it is a different type of input.
#[derive(Debug, Default, Clone, clap::Args)]
pub struct NodeAddr {
    /// the node's http base url (wins over --config, -n/--network and DUCKTAPE_NODE)
    #[arg(long, value_name = "HTTP-URL", global = true)]
    pub node: Option<String>,
    /// a workspace's node.toml — names ONE of two workspaces that share a
    /// chain id (wins over -n/--network and DUCKTAPE_NODE; --node wins over it)
    #[arg(long, value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,
    /// a registered workspace's chain id — resolves to its node.toml http_listen
    #[arg(short = 'n', long = "network", value_name = "CHAIN-ID", global = true)]
    pub network: Option<String>,
}

/// one rung of the node-addressing ladder — ONE tagged value, so the precedence
/// is a single ordered expression instead of a hand-written `if` chain per
/// family. Four of those existed and disagreed about `DUCKTAPE_NODE`, so
/// `ducktape fs`, `ducktape agent` and `ducktape account create` could each
/// dial a DIFFERENT node in one shell.
#[derive(Debug)]
enum Rung {
    /// `--node <http-url>`
    Flag(String),
    /// `--config <node.toml>` → that file's `http_listen`. The one rung that
    /// separates two workspaces sharing a chain id, which `-n` cannot.
    Config(PathBuf),
    /// `-n/--network <chain-id>` → the workspace node.toml's `http_listen`
    Network(String),
    /// the `DUCKTAPE_NODE` environment variable
    Env(String),
    /// the caller's own ambient address (`fs` inside a checkout: the `.duckfs`
    /// index's recorded node url)
    Context(String),
    /// the single registered workspace, when exactly one is registered
    LoneWorkspace,
}

/// the message every unresolved address ends with — it names every rung, so a
/// user who hit the bottom of the ladder can see all of it.
const NO_NODE_ADDRESS: &str = "no node address: pass --node <http-url>, --config <node.toml>, \
     -n/--network <id>, or set DUCKTAPE_NODE";

/// turn a chosen rung into the http base. The one `match`: a new rung must be
/// routed here or the build fails.
fn rung_base(rung: Rung) -> Result<String, String> {
    match rung {
        Rung::Flag(url) => checked_base("--node", &url),
        Rung::Env(url) => checked_base("DUCKTAPE_NODE", &url),
        // not a user-typed string: the caller's own recorded address.
        Rung::Context(url) => Ok(trim_base(&url)),
        Rung::Config(file) => http_of_config(&file),
        Rung::Network(needle) => http_of_workspace(&needle),
        Rung::LoneWorkspace => lone_workspace_base(),
    }
}

/// the base a node.toml serves, read through [`config::resolve_service`] — the
/// keyless read `WorkspaceArgs` resolves the same `--config` with.
fn http_of_config(file: &std::path::Path) -> Result<String, String> {
    let listen = config::resolve_service(file)?.http_listen.ok_or_else(|| {
        format!(
            "{} sets no http_listen, so there is no node surface to dial — pass --node <http-url>",
            file.display()
        )
    })?;
    Ok(trim_base(&config::http_base_of(&listen)))
}

/// a trailing slash on the base would double up against every `/v1/...` path.
fn trim_base(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

/// Refuse a `--node` / `DUCKTAPE_NODE` value that is not an http base, HERE —
/// at the boundary that named it — instead of letting it travel to whichever
/// verb dials first and die inside reqwest's url parser as `builder error`.
///
/// A chain id is the mistake this actually catches: `--node mynet#d0cdf950`
/// parses, outranks `-n`, and then silently misdirects, so the message names
/// the flag that WOULD have taken it.
fn checked_base(source: &str, url: &str) -> Result<String, String> {
    let is_http = url.starts_with("http://") || url.starts_with("https://");
    if is_http {
        return Ok(trim_base(url));
    }
    Err(format!(
        "{source} is an http base url, and {url:?} is not one (expected http://host:port) — \
         for a network name use: -n/--network {url:?}"
    ))
}

fn http_of_workspace(needle: &str) -> Result<String, String> {
    let (_dir, http) = config::resolve_network(needle)?;
    let base = http.ok_or_else(|| {
        format!(
            "network {needle:?} resolves to a workspace with no http listen \
             (its node.toml sets no http_listen) — pass --node <http-url>"
        )
    })?;
    Ok(trim_base(&base))
}

/// the bottom rung: infer the node from the registry when exactly one workspace
/// is registered — the same "a freshly-init'd machine runs with no flags at all"
/// ergonomic [`Selector::config_path`] already has.
///
/// The chain id, not the base, because both questions this ladder answers stand
/// on it: [`rung_base`] wants the workspace's `http_listen` and
/// [`NodeAddr::workspace_with`] wants its directory. One rule, two readers.
fn lone_workspace_id() -> Result<String, String> {
    let mut workspaces = config::list_workspaces()?;
    match workspaces.len() {
        1 => Ok(workspaces.swap_remove(0).0),
        0 => Err(NO_NODE_ADDRESS.into()),
        _ => Err(format!(
            "{NO_NODE_ADDRESS}\nseveral workspaces are registered — pick one with -n, or \
             --config <path> when two share a chain id:\n{}",
            config::workspace_choices(&workspaces)
        )),
    }
}

fn lone_workspace_base() -> Result<String, String> {
    http_of_workspace(&lone_workspace_id()?)
}

/// where a rung's WORKSPACE comes from — the directory half of [`Rung`].
///
/// A second tagged value rather than a second precedence: the order is still
/// [`NodeAddr::ladder_rung`]'s alone, and this only says what each rung yields
/// once chosen. Split from the filesystem work for the same reason `Rung` is
/// split from [`rung_base`] — the mapping is then a decision a test can drive
/// with no registry on disk and no process env to mutate.
#[derive(Debug, PartialEq, Eq)]
enum WorkspaceSource {
    /// the operator named the workspace's config file: it IS the answer.
    File(PathBuf),
    /// the operator named a workspace: use it, and do NOT search.
    Named(String),
    /// the bottom rung's inference.
    LoneRegistered,
    /// the rung carried only an address; find the workspace that serves it.
    Serving(String),
}

/// map a chosen rung to where its workspace comes from. The one `match`: a new
/// rung must be routed here or the build fails.
fn rung_workspace_source(rung: Rung) -> WorkspaceSource {
    match rung {
        Rung::Config(file) => WorkspaceSource::File(file),
        // NOT `Serving`: two registered workspaces may share a base by default,
        // so searching backwards would refuse the very id the operator typed.
        Rung::Network(needle) => WorkspaceSource::Named(needle),
        Rung::LoneWorkspace => WorkspaceSource::LoneRegistered,
        Rung::Flag(url) | Rung::Env(url) | Rung::Context(url) => {
            WorkspaceSource::Serving(trim_base(&url))
        }
    }
}

/// resolve a source to a directory — the effectful half.
fn source_workspace(source: WorkspaceSource) -> Result<PathBuf, String> {
    let needle = match source {
        WorkspaceSource::File(file) => return Ok(config::resolve_service(&file)?.workspace),
        WorkspaceSource::Named(needle) => needle,
        WorkspaceSource::LoneRegistered => lone_workspace_id()?,
        WorkspaceSource::Serving(base) => return workspace_serving(&base),
    };
    config::resolve_network(&needle).map(|(dir, _)| dir)
}

/// the node.toml behind a source: the file itself when the operator named one,
/// else the one inside its workspace — [`WorkspaceArgs::config_file`]'s rule, so
/// a config that does not sit at `<workspace>/node.toml` is still the one read.
fn source_config_file(source: WorkspaceSource) -> Result<PathBuf, String> {
    match source {
        WorkspaceSource::File(file) => Ok(file),
        source @ (WorkspaceSource::Named(_)
        | WorkspaceSource::LoneRegistered
        | WorkspaceSource::Serving(_)) => Ok(source_workspace(source)?.join("node.toml")),
    }
}

/// Which registered workspace SERVES `base` — the reverse of [`http_of_workspace`].
///
/// The rungs that carry a bare url (`--node`, `DUCKTAPE_NODE`, a caller's
/// context) name an address and nothing else, but a workspace DIRECTORY is where
/// a node's 0600 secrets live, so the registry is searched backwards for the
/// workspace that answers on it. Kept beside the forward lookup so both spell
/// the base the same way through [`trim_base`]: a normalization that drifted
/// apart would silently match nothing.
/// public door onto [`workspace_serving`] for a caller that already has the
/// resolved http base in hand (a signing verb pinning the node's identity —
/// see `bin/node/src/node_http.rs::pinned_node_key`) and needs to ask "is this
/// address one of MY OWN registered nodes", independent of which rung of the
/// ladder produced it.
pub fn workspace_for_base(base: &str) -> Result<PathBuf, String> {
    workspace_serving(base)
}

/// The same reverse lookup for the OPERATOR RPC address rather than the http
/// base — the lane `ducktape node status`, `join state` and the module verbs
/// dial.
///
/// Those verbs hold an `rpc_listen` string and, several helper frames down, no
/// longer hold the config they read it out of. Rather than thread a directory
/// through every one of them, the registry answers the same question it
/// already answers for a url: which workspace is this address?
pub fn workspace_for_rpc(addr: &str) -> Result<PathBuf, String> {
    workspace_for_rpc_in(&config::ducktape_home()?, addr)
}

/// Split from the home lookup for the same reason [`workspace_serving_in`] is:
/// two workspaces sharing a default `rpc_listen` is the ordinary case, not an
/// exotic one, and no registry-free test can reach it.
fn workspace_for_rpc_in(root: &std::path::Path, addr: &str) -> Result<PathBuf, String> {
    let matches = config::list_workspaces_in(root)?
        .into_iter()
        .filter_map(|(chain_id, node_toml)| {
            let dir = node_toml.parent()?.to_path_buf();
            let listen = config::rpc_listen_in(&dir).ok()?;
            (listen == addr).then_some((chain_id, dir))
        })
        .collect::<Vec<_>>();
    workspace_of_matches(addr, matches)
}

fn workspace_serving(base: &str) -> Result<PathBuf, String> {
    workspace_serving_in(&config::ducktape_home()?, base)
}

/// Split from the home lookup so a test can lay out two workspaces that share a
/// chain id — which is what this used to get wrong, and what no registry-free
/// test could reach.
fn workspace_serving_in(root: &std::path::Path, base: &str) -> Result<PathBuf, String> {
    let matches = config::list_workspaces_in(root)?
        .into_iter()
        .filter_map(|(chain_id, node_toml)| {
            // The workspace the registry just handed us, read for its OWN
            // http_listen. Asking `resolve_network(&chain_id)` instead threw
            // that path away and searched the home again by chain id — and a
            // founder and the resident that joined it SHARE one. Both entries
            // then answered with whichever directory the scan reached first, so
            // a base exactly one workspace serves came back matched twice and
            // every --node verb refused, offering `-n <chain-id>` as the way
            // out when `-n` is the one thing that cannot separate them.
            let dir = node_toml.parent()?.to_path_buf();
            let http = config::http_base_in(&dir).ok()?;
            (trim_base(&http) == base).then_some((chain_id, dir))
        })
        .collect::<Vec<_>>();
    workspace_of_matches(base, matches)
}

/// Decide which of the registry's matches answers for `base`, or say why none
/// does. Split from the scan so the ambiguity rule is drivable by a test with no
/// registry on disk.
fn workspace_of_matches(base: &str, matches: Vec<(String, PathBuf)>) -> Result<PathBuf, String> {
    match matches.as_slice() {
        [(_, dir)] => Ok(dir.clone()),
        [] => Err(format!(
            "no registered workspace serves {base} — name it with -n/--network <chain-id>"
        )),
        // the DEFAULT case, not an exotic one: `node init` and `node join` both
        // leave `http_listen` at `DEFAULT_HTTP_LISTEN`, so two networks on one
        // machine share a base out of the box, and `list_workspaces` is chain-id
        // ordered. Taking the first would read the WRONG node's 0600 secret
        // under an id the operator never chose — so refuse, the way every other
        // ambiguous selection on this ladder does.
        // this list holds workspace DIRECTORIES; the choice list names the
        // config inside each, because that is the string an operator retypes.
        several => Err(format!(
            "several workspaces serve {base} — pick one with -n, or --config <path> when two \
             share a chain id:\n{}",
            config::workspace_choices(
                &several
                    .iter()
                    .map(|(chain_id, dir)| (chain_id.clone(), dir.join("node.toml")))
                    .collect::<Vec<_>>()
            )
        )),
    }
}

/// the ONE read of `DUCKTAPE_NODE` in the CLI — see
/// `the_cli_reads_ducktape_node_in_exactly_one_place`.
fn env_node() -> Option<String> {
    std::env::var("DUCKTAPE_NODE").ok()
}

impl NodeAddr {
    /// the whole ladder, for a caller with no ambient address of its own.
    pub fn resolve(&self) -> Result<String, String> {
        self.resolve_with(|| None)
    }

    /// the whole ladder: `--node` → `--config` → `-n/--network` →
    /// `DUCKTAPE_NODE` → `context()` → the lone registered workspace.
    ///
    /// `context` is the caller's own ambient address, tried AFTER what the
    /// operator stated and BEFORE the registry inference. `fs` inside a checkout
    /// passes the `.duckfs` index's recorded url: more specific than "the one
    /// workspace registered on this box", less specific than a flag.
    pub fn resolve_with(&self, context: impl FnOnce() -> Option<String>) -> Result<String, String> {
        rung_base(self.ladder_rung(env_node(), context))
    }

    /// the WORKSPACE DIRECTORY behind the address this ladder resolves, for a
    /// caller with no ambient address of its own.
    pub fn workspace(&self) -> Result<PathBuf, String> {
        self.workspace_with(|| None)
    }

    /// the workspace directory behind the resolved address — where that node's
    /// 0600 secrets live (`service-link.token`), which no url carries.
    ///
    /// The SAME ladder and the SAME rungs as [`Self::resolve_with`], because
    /// "which node" must be answered once: a second precedence over the same
    /// inputs is the defect this file exists to have deleted. Only the last step
    /// differs, and that is one `match` with no `_` arm — a rung names a
    /// workspace outright, or it names an address the registry is searched
    /// backwards for.
    ///
    /// Distinct from [`Selector::config_path`], which resolves a node.toml PATH
    /// for the daemon that IS the node and never reads the env. This asks where
    /// the node a CLIENT is dialling keeps its files.
    pub fn workspace_with(
        &self,
        context: impl FnOnce() -> Option<String>,
    ) -> Result<PathBuf, String> {
        source_workspace(rung_workspace_source(self.ladder_rung(env_node(), context)))
    }

    /// the node.toml behind the resolved address — `--config` itself when it
    /// is the rung, else the one inside [`Self::workspace`]. For a verb that
    /// reads the node's own config (its chain id, its consensus key).
    pub fn config_file(&self) -> Result<PathBuf, String> {
        source_config_file(rung_workspace_source(self.ladder_rung(env_node(), || None)))
    }

    /// pick the rung. `env` is a parameter rather than a read so the precedence
    /// is testable without mutating process env — racy across parallel tests,
    /// and `unsafe` since edition 2024.
    fn ladder_rung(&self, env: Option<String>, context: impl FnOnce() -> Option<String>) -> Rung {
        let flag = self.node.clone().filter(|url| !url.is_empty());
        let file = self
            .config
            .clone()
            .filter(|path| !path.as_os_str().is_empty());
        let network = self.network.clone().filter(|id| !id.is_empty());
        let env = env.filter(|url| !url.is_empty());
        // THE PRECEDENCE. Written once, in one expression, for every family.
        flag.map(Rung::Flag)
            .or_else(|| file.map(Rung::Config))
            .or_else(|| network.map(Rung::Network))
            .or_else(|| env.map(Rung::Env))
            .or_else(|| context().filter(|url| !url.is_empty()).map(Rung::Context))
            .unwrap_or(Rung::LoneWorkspace)
    }
}

/// a verb whose only arguments are the workspace selector.
#[derive(Debug, clap::Args)]
pub struct SelectorArgs {
    #[command(flatten)]
    pub selector: Selector,
}

/// `node qualify [--compose-only]`.
#[derive(Debug, clap::Args)]
pub struct QualifyArgs {
    #[command(flatten)]
    pub selector: Selector,
    /// ask only whether the components this network RUNS load against this
    /// binary's wasm world, reading the roster off the node's rpc and the
    /// bytes out of its blob files. takes no lock and opens no store, so it
    /// runs beside a live node — and it says nothing about state layout
    #[arg(long)]
    pub compose_only: bool,
}

/// selector + the machine-readable output toggle.
#[derive(Debug, clap::Args)]
pub struct StatusArgs {
    #[command(flatten)]
    pub selector: Selector,
    /// emit one machine-readable JSON object instead of prose
    #[arg(long)]
    pub json: bool,
}

/// a membership verb: the subject key + the workspace selector.
#[derive(Debug, clap::Args)]
pub struct PubkeyArgs {
    /// the subject's hex node pubkey
    #[arg(value_name = "HEX-PUBKEY")]
    pub pubkey: String,
    #[command(flatten)]
    pub selector: Selector,
}

#[derive(Debug, clap::Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub selector: Selector,
    /// exit once state sync completes instead of validating
    #[arg(long)]
    pub sync_only: bool,
}

#[derive(Debug, clap::Args)]
pub struct KeyArgs {
    /// write the identity file here
    #[arg(long, value_name = "PATH", conflicts_with = "dir")]
    pub out: Option<PathBuf>,
    /// mint (or reuse) <DIR>/identity.key, creating the dir
    #[arg(long, value_name = "DIR")]
    pub dir: Option<PathBuf>,
}

/// `init --name`, refused at parse time — before any workspace file is written
/// — when the `duck://` grammar cannot carry it as a chain id's label. The
/// refusal names sdk's reason token.
fn network_name(raw: &str) -> Result<String, String> {
    config::validate_network_name(raw)
        .map_err(|refused| format!("{} [{}]", refused.sentence, refused.reason))?;
    Ok(raw.to_string())
}

#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// network name: the label of every duck:// address naming the network
    /// (the chain id becomes <name>#<salt>)
    #[arg(long, value_name = "NAME", value_parser = network_name)]
    pub name: String,
    /// found the network here instead of under the ducktape home
    #[arg(long, value_name = "DIR")]
    pub dir: Option<PathBuf>,
    /// the founding set to compose the genesis from: a directory holding every
    /// `<id>.component.wasm` and `<id>.index.wasm` (default: $DUCKTAPE_MODULES_DIR,
    /// else the set the build staged beside this binary)
    #[arg(long, value_name = "DIR")]
    pub modules: Option<PathBuf>,
    /// milliseconds between idle blocks; every consensus timer scales with it.
    /// FOUNDING parameter, not plumbing: it lands in the network descriptor and
    /// every joiner inherits it, so `join` has no such flag.
    #[arg(
        long,
        value_name = "MS",
        default_value_t = config::DEFAULT_BLOCK_TIME_MS,
        value_parser = clap::value_parser!(u64).range(config::MIN_BLOCK_TIME_MS..),
    )]
    pub block_time_ms: u64,
    #[command(flatten)]
    pub plumbing: PlumbingArgs,
}

#[derive(Debug, clap::Args)]
pub struct InviteArgs {
    /// days until the token expires
    #[arg(
        long,
        value_name = "N",
        default_value_t = config::DEFAULT_INVITE_TTL_DAYS,
        value_parser = clap::value_parser!(u64).range(config::INVITE_TTL_DAYS),
    )]
    pub ttl_days: u64,
    #[command(flatten)]
    pub selector: Selector,
}

#[derive(Debug, clap::Args)]
pub struct AdmitArgs {
    /// the hex node pubkey to seed into the genesis validator set
    #[arg(value_name = "HEX-PUBKEY")]
    pub pubkey: String,
    #[command(flatten)]
    pub selector: Selector,
}

/// `join` is both a leaf (`join <blob>`) and a prefix (`join requests`,
/// `join state`) — a subcommand token wins, anything else is the blob.
#[derive(Debug, clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct JoinCmd {
    #[command(subcommand)]
    pub query: Option<JoinQuery>,
    /// the invite blob a member minted. several shell words are joined back
    /// together (a paste split by spaces still works); omitted entirely, the
    /// blob is read from stdin — paste it at the prompt and press Enter.
    #[arg(value_name = "INVITE-BLOB", num_args = 0..)]
    pub blob: Vec<String>,
    /// materialize here instead of the home dir named by the chain id
    #[arg(long, value_name = "DIR")]
    pub dir: Option<PathBuf>,
    /// the network's genesis file (the founder's `<workspace>/genesis`). A
    /// pre-genesis member boots straight into genesis, so it must hold the
    /// file before it starts; any other joiner fetches it off the mesh at
    /// first boot and needs no flag
    #[arg(long, value_name = "FILE")]
    pub genesis: Option<PathBuf>,
    #[command(flatten)]
    pub plumbing: PlumbingArgs,
}

#[derive(Debug, clap::Subcommand)]
pub enum JoinQuery {
    /// list parked joiners delivered to this member's node (JSON)
    Requests(SelectorArgs),
    /// this node's authoritative onboarding phase (JSON)
    State(SelectorArgs),
}

/// the network plumbing `init` and `join` share. every flag overrides a
/// compiled default (or a value an existing node.toml already carries); an
/// absent flag is deliberately NOT persisted, so the runtime keeps
/// re-deriving the same default the descriptor was founded with.
#[derive(Debug, Default, clap::Args)]
pub struct PlumbingArgs {
    /// p2p mesh listen address
    #[arg(long, value_name = "ADDR", hide_short_help = true)]
    pub listen: Option<String>,
    /// the address other members dial (or "overlay")
    #[arg(long, value_name = "ADDR", hide_short_help = true)]
    pub advertised: Option<String>,
    /// node HTTP API listen address
    #[arg(long, value_name = "ADDR", hide_short_help = true)]
    pub http: Option<String>,
    /// browser gateway listen address
    #[arg(long, value_name = "ADDR", hide_short_help = true)]
    pub gateway: Option<String>,
    /// local operator rpc listen address
    #[arg(long, value_name = "ADDR", hide_short_help = true)]
    pub rpc: Option<String>,
    /// ambient coordinator, `host:port` or `none`
    #[arg(long, value_name = "HOST:PORT|none", hide_short_help = true)]
    pub primary_coordinator: Option<String>,
    /// WireGuard UDP listen address (enables the reachability plane)
    #[arg(long, value_name = "ADDR", hide_short_help = true)]
    pub wireguard_listen: Option<String>,
    /// externally visible WireGuard endpoint (port-forwarded setups)
    #[arg(long, value_name = "HOST:PORT", hide_short_help = true)]
    pub wireguard_advertised: Option<String>,
    /// invite intro listener address
    #[arg(long, value_name = "ADDR", hide_short_help = true)]
    pub invite_listen: Option<String>,
}

impl PlumbingArgs {
    /// this flag set, named the way [`config::merged_plumbing`] wants them —
    /// `init` and `join` build the identical struct from their own flags.
    pub fn overrides(&self) -> config::PlumbingOverrides {
        config::PlumbingOverrides {
            listen: self.listen.clone(),
            advertised: self.advertised.clone(),
            http: self.http.clone(),
            gateway: self.gateway.clone(),
            rpc: self.rpc.clone(),
            primary_coordinator: self.primary_coordinator.clone(),
            wireguard_listen: self.wireguard_listen.clone(),
            wireguard_advertised: self.wireguard_advertised.clone(),
            invite_listen: self.invite_listen.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(node: Option<&str>, network: Option<&str>) -> NodeAddr {
        NodeAddr {
            node: node.map(str::to_string),
            config: None,
            network: network.map(str::to_string),
        }
    }

    /// `--config` is a flag of the ONE addressing group, so every family that
    /// dials a node takes it — `account` and `fs` had no way to name one of two
    /// workspaces sharing a chain id, while every ambiguity refusal told the
    /// operator to "pick one with --config <path>".
    #[test]
    fn every_node_addressed_family_takes_config() {
        let families: [&[&str]; 5] = [
            &["ducktape", "account", "show", "--config", "/w/node.toml"],
            &[
                "ducktape",
                "account",
                "key",
                "list",
                "--config",
                "/w/node.toml",
            ],
            &["ducktape", "fs", "ls", "/", "--config", "/w/node.toml"],
            &["ducktape", "fs", "cat", "/f", "--config", "/w/node.toml"],
            &[
                "ducktape",
                "user",
                "cred",
                "list",
                "--config",
                "/w/node.toml",
            ],
        ];
        for argv in families {
            if let Err(e) = <crate::Cli as clap::Parser>::try_parse_from(argv) {
                panic!("{argv:?} refused --config:\n{e}");
            }
        }
    }

    /// `invite` without `--ttl-days` mints the ONE default every door shares,
    /// and the flag is refused outside the ONE validated range — at parse
    /// time, before any workspace file is touched.
    #[test]
    fn invite_ttl_days_defaults_and_bounds_come_from_workspace_config() {
        #[derive(clap::Parser)]
        struct Probe {
            #[command(subcommand)]
            op: OpCmd,
        }
        let parse = |argv: &[&str]| <Probe as clap::Parser>::try_parse_from(argv);
        let ttl_of = |argv: &[&str]| match parse(argv).expect("parses").op {
            OpCmd::Invite(args) => args.ttl_days,
            other => panic!("not an invite: {other:?}"),
        };

        assert_eq!(
            ttl_of(&["probe", "invite"]),
            config::DEFAULT_INVITE_TTL_DAYS
        );
        assert_eq!(ttl_of(&["probe", "invite", "--ttl-days", "365"]), 365);
        assert!(parse(&["probe", "invite", "--ttl-days", "0"]).is_err());
        assert!(parse(&["probe", "invite", "--ttl-days", "366"]).is_err());
    }

    /// `init --block-time-ms` is refused below the drain-tick floor at parse
    /// time, mirroring `workspace_config::validate_block_time_ms` — the CLI
    /// and the descriptor boundary must refuse the exact same beats.
    #[test]
    fn init_block_time_ms_is_floored_at_the_drain_tick() {
        #[derive(clap::Parser)]
        struct Probe {
            #[command(subcommand)]
            op: OpCmd,
        }
        let parse = |argv: &[&str]| <Probe as clap::Parser>::try_parse_from(argv);
        let block_time_of = |argv: &[&str]| match parse(argv).expect("parses").op {
            OpCmd::Init(args) => args.block_time_ms,
            other => panic!("not an init: {other:?}"),
        };

        assert_eq!(
            block_time_of(&["probe", "init", "--name", "demo"]),
            config::DEFAULT_BLOCK_TIME_MS
        );
        assert_eq!(
            block_time_of(&["probe", "init", "--name", "demo", "--block-time-ms", "100"]),
            config::MIN_BLOCK_TIME_MS
        );
        assert!(parse(&["probe", "init", "--name", "demo", "--block-time-ms", "99"]).is_err());
        assert!(parse(&["probe", "init", "--name", "demo", "--block-time-ms", "0"]).is_err());
    }

    /// `init --name` is refused at parse time, by the address grammar's own
    /// reason token, when no `duck://` address could name the network.
    #[test]
    fn init_name_is_a_label_the_address_grammar_carries() {
        #[derive(clap::Parser)]
        struct Probe {
            #[command(subcommand)]
            op: OpCmd,
        }
        let parse = |argv: &[&str]| <Probe as clap::Parser>::try_parse_from(argv);

        let name = match parse(&["probe", "init", "--name", "my-team"])
            .expect("parses")
            .op
        {
            OpCmd::Init(args) => args.name,
            other => panic!("not an init: {other:?}"),
        };
        assert_eq!(name, "my-team");
        for (bad, reason) in [
            ("My Team", "[uppercase]"),
            ("my team", "[authority_incomplete]"),
        ] {
            let Err(refused) = parse(&["probe", "init", "--name", bad]) else {
                panic!("{bad:?} parsed");
            };
            let refused = refused.to_string();
            assert!(refused.contains(reason), "{bad:?}: {refused}");
        }
    }

    /// the precedence, pinned rung by rung and hermetically: only the `Flag`,
    /// `Env` and `Context` rungs are resolved to a base (the registry rungs are
    /// asserted as rungs, so no test ever walks `~/.ducktape`).
    #[test]
    fn the_node_address_ladder_ranks_flag_config_network_env_context_registry() {
        let env = || Some("http://env:1/".to_string());
        let ctx = || Some("http://ctx:1/".to_string());

        // 1. --node wins over everything below it, --config included: both
        //    name one node, and the url is the more direct of the two.
        let with_config = |node: Option<&str>| NodeAddr {
            config: Some(PathBuf::from("/w/node.toml")),
            ..addr(node, Some("some-workspace"))
        };
        let rung = with_config(Some("http://flag:1/")).ladder_rung(env(), ctx);
        assert_eq!(rung_base(rung).unwrap(), "http://flag:1");

        // 2. --config beats -n: a chain id cannot separate a founder from the
        //    member that joined it, and the file can.
        assert!(matches!(
            with_config(None).ladder_rung(env(), ctx),
            Rung::Config(file) if file == std::path::Path::new("/w/node.toml")
        ));

        // 3. -n/--network beats the env — a rung some user verbs used to
        //    reach only because they ignored DUCKTAPE_NODE entirely.
        assert!(matches!(
            addr(None, Some("some-workspace")).ladder_rung(env(), ctx),
            Rung::Network(id) if id == "some-workspace"
        ));

        // 4. the env beats the caller's ambient context.
        assert_eq!(
            rung_base(addr(None, None).ladder_rung(env(), ctx)).unwrap(),
            "http://env:1"
        );

        // 5. the context beats the registry: `fs commit` inside a checkout must
        //    reach the node it was checked out FROM, not "the one workspace
        //    registered on this box".
        assert_eq!(
            rung_base(addr(None, None).ladder_rung(None, ctx)).unwrap(),
            "http://ctx:1"
        );

        // 6. nothing at all → the registry inference, the bottom rung.
        assert!(matches!(
            addr(None, None).ladder_rung(None, || None),
            Rung::LoneWorkspace
        ));
    }

    /// The workspace question rides the SAME rungs as the address question, and
    /// each rung yields its directory the one way that rung can.
    ///
    /// Hermetic, for the same reason the test above is: the mapping is asserted,
    /// not the filesystem. `Rung::Network` is the sharp one — routing it through
    /// the reverse lookup would refuse the very id the operator typed, because
    /// two registered workspaces share a base by default.
    #[test]
    fn the_workspace_question_rides_the_same_rungs_as_the_address() {
        let env = || Some("http://env:1/".to_string());
        let ctx = || Some("http://ctx:1/".to_string());
        let source = |a: NodeAddr, e: Option<String>, c: fn() -> Option<String>| {
            rung_workspace_source(a.ladder_rung(e, c))
        };

        // a named workspace is USED, never searched for — by its file first.
        let config = NodeAddr {
            config: Some(PathBuf::from("/w/node.toml")),
            ..addr(None, Some("chain-a"))
        };
        assert_eq!(
            source(config, env(), ctx),
            WorkspaceSource::File(PathBuf::from("/w/node.toml"))
        );
        assert_eq!(
            source(addr(None, Some("chain-a")), env(), ctx),
            WorkspaceSource::Named("chain-a".into())
        );
        // the url-bearing rungs carry no directory of their own, so the reverse
        // lookup is theirs alone — and each is trimmed the way the forward
        // lookup spells it, or it would match nothing.
        assert_eq!(
            source(addr(Some("http://flag:1/"), Some("chain-a")), env(), ctx),
            WorkspaceSource::Serving("http://flag:1".into())
        );
        assert_eq!(
            source(addr(None, None), env(), ctx),
            WorkspaceSource::Serving("http://env:1".into())
        );
        assert_eq!(
            source(addr(None, None), None, ctx),
            WorkspaceSource::Serving("http://ctx:1".into())
        );
        // and the bottom rung infers, like it does for the address.
        assert_eq!(
            source(addr(None, None), None, || None),
            WorkspaceSource::LoneRegistered
        );
    }

    /// An ambiguous address must REFUSE a workspace, never pick one.
    ///
    /// `node init` and `node join` both leave `http_listen` at
    /// `DEFAULT_HTTP_LISTEN`, so two registered networks share a base by default
    /// and `list_workspaces` is chain-id ordered — a first-match would
    /// deterministically read the WRONG node's 0600 secret under an id the
    /// operator never chose.
    #[test]
    fn an_ambiguous_address_refuses_a_workspace_instead_of_picking_one() {
        let one = vec![("chain-a".to_string(), PathBuf::from("/ws/a"))];
        assert_eq!(
            workspace_of_matches("http://127.0.0.1:8844", one),
            Ok(PathBuf::from("/ws/a"))
        );

        let Err(why) = workspace_of_matches("http://127.0.0.1:8844", Vec::new()) else {
            panic!("an unmatched address has no workspace");
        };
        assert!(why.contains("no registered workspace"), "{why}");

        let several = vec![
            ("chain-a".to_string(), PathBuf::from("/ws/a")),
            ("chain-b".to_string(), PathBuf::from("/ws/b")),
        ];
        let Err(why) = workspace_of_matches("http://127.0.0.1:8844", several) else {
            panic!("an ambiguous address must refuse, not pick the first");
        };
        // it names BOTH candidates: the operator has to pick, so the message has
        // to say what there is to pick from.
        assert!(why.contains("chain-a") && why.contains("chain-b"), "{why}");
        assert!(why.contains("-n"), "{why}");
    }

    /// A founder and the resident that joined it live in one ducktape home and
    /// SHARE a chain id, on different ports. Every `--node`-addressed verb
    /// refused them: the scan threw away the path the registry handed it and
    /// re-resolved each workspace by chain id, which cannot tell two workspaces
    /// on one chain apart — so both entries came back as the same directory,
    /// and a base exactly one of them serves matched twice. `-n <chain-id>`,
    /// the remedy the refusal offers, cannot separate them either.
    #[test]
    fn a_founder_and_its_resident_on_one_chain_each_answer_for_their_own_port() {
        let home = tempfile::tempdir().expect("temp home");
        let founder = write_workspace(
            home.path(),
            "net",
            "dognet#d2a0ec8f",
            "127.0.0.1:28800",
            "127.0.0.1:28820",
        );
        let resident = write_workspace(
            home.path(),
            "net-joiner",
            "dognet#d2a0ec8f",
            "127.0.0.1:28801",
            "127.0.0.1:28821",
        );

        assert_eq!(
            workspace_serving_in(home.path(), "http://127.0.0.1:28800"),
            Ok(founder),
            "the founder's own port did not reach the founder"
        );
        assert_eq!(
            workspace_serving_in(home.path(), "http://127.0.0.1:28801"),
            Ok(resident),
            "the resident's own port did not reach the resident"
        );

        // A base nobody serves is still absent, not ambiguous.
        let Err(why) = workspace_serving_in(home.path(), "http://127.0.0.1:1") else {
            panic!("an unserved base resolved to a workspace");
        };
        assert!(why.contains("no registered workspace"), "{why}");

        // And the collision the refusal exists for is untouched: two workspaces
        // that really do serve one base still refuse rather than pick.
        write_workspace(
            home.path(),
            "other",
            "kitchen#99887766",
            "127.0.0.1:28800",
            "127.0.0.1:28822",
        );
        let Err(why) = workspace_serving_in(home.path(), "http://127.0.0.1:28800") else {
            panic!("two workspaces on one base must refuse, not pick the first");
        };
        assert!(why.contains("several workspaces serve"), "{why}");
    }

    /// The same reverse lookup for the OPERATOR RPC lane, which `node status`,
    /// `join state` and the module verbs dial. They hold an `rpc_listen` and
    /// never a url, so a refusal that wants to name the launcher supervising
    /// this node has to reach the workspace from that address instead.
    #[test]
    fn the_rpc_lane_reaches_each_workspace_by_its_own_rpc_address() {
        let home = tempfile::tempdir().expect("temp home");
        let founder = write_workspace(
            home.path(),
            "net",
            "dognet#d2a0ec8f",
            "127.0.0.1:28800",
            "127.0.0.1:28820",
        );
        let resident = write_workspace(
            home.path(),
            "net-joiner",
            "dognet#d2a0ec8f",
            "127.0.0.1:28801",
            "127.0.0.1:28821",
        );

        assert_eq!(
            workspace_for_rpc_in(home.path(), "127.0.0.1:28820"),
            Ok(founder),
            "the founder's own rpc port did not reach the founder"
        );
        assert_eq!(
            workspace_for_rpc_in(home.path(), "127.0.0.1:28821"),
            Ok(resident),
            "the resident's own rpc port did not reach the resident"
        );

        let Err(why) = workspace_for_rpc_in(home.path(), "127.0.0.1:1") else {
            panic!("an unserved rpc address resolved to a workspace");
        };
        assert!(why.contains("no registered workspace"), "{why}");

        // Two networks BOTH left on the default `rpc_listen` is the ordinary
        // case, and it is why the caller falls back to the plain sentence: a
        // wrong launcher's log is worse than no launcher's.
        write_workspace(
            home.path(),
            "other",
            "kitchen#99887766",
            "127.0.0.1:28802",
            "127.0.0.1:28820",
        );
        let Err(why) = workspace_for_rpc_in(home.path(), "127.0.0.1:28820") else {
            panic!("two workspaces on one rpc address must refuse, not pick the first");
        };
        assert!(why.contains("several workspaces serve"), "{why}");
    }

    /// `--config` is what separates a founder from the member that joined it:
    /// each file reaches its OWN port and its OWN directory, where `-n` over
    /// their one chain id can only refuse. Resolved from the file alone — the
    /// registry is never asked, so it cannot answer with the other row.
    #[test]
    fn config_reaches_the_one_of_two_workspaces_sharing_a_chain_id() {
        let home = tempfile::tempdir().expect("temp home");
        let chain = "dognet#d2a0ec8f";
        let founder = write_workspace(home.path(), "net", chain, "0.0.0.0:28800", "x:1");
        let joiner = write_workspace(home.path(), "net-joiner", chain, "0.0.0.0:28801", "x:2");
        let by_config = |dir: &std::path::Path| NodeAddr {
            config: Some(dir.join("node.toml")),
            ..addr(None, None)
        };

        for (dir, base) in [
            (&founder, "http://127.0.0.1:28800"),
            (&joiner, "http://127.0.0.1:28801"),
        ] {
            let at = by_config(dir);
            assert_eq!(rung_base(at.ladder_rung(None, || None)), Ok(base.into()));
            assert_eq!(
                source_workspace(rung_workspace_source(at.ladder_rung(None, || None))),
                Ok(dir.clone())
            );
            assert_eq!(
                source_config_file(rung_workspace_source(at.ladder_rung(None, || None))),
                Ok(dir.join("node.toml"))
            );
        }

        // a config that is not `<workspace>/node.toml` is still the file read:
        // `--config <elsewhere>/alt.toml` names ITS node, never a neighbour's.
        let alt = founder.join("alt.toml");
        std::fs::copy(founder.join("node.toml"), &alt).expect("copy config");
        let at = NodeAddr {
            config: Some(alt.clone()),
            ..addr(None, None)
        };
        assert_eq!(
            source_config_file(rung_workspace_source(at.ladder_rung(None, || None))),
            Ok(alt)
        );
    }

    /// A workspace on disk, complete enough for the registry to list it, for
    /// its own `http_listen` and `rpc_listen` to be read back, and for
    /// [`config::resolve_service`] to accept its descriptor.
    fn write_workspace(
        root: &std::path::Path,
        ws: &str,
        chain: &str,
        http: &str,
        rpc: &str,
    ) -> PathBuf {
        use commonware_cryptography::Signer as _;
        let dir = root.join(ws);
        std::fs::create_dir_all(&dir).expect("mk workspace");
        let validator = commonware_cryptography::ed25519::PrivateKey::from_seed(1).public_key();
        config::NetworkDescriptor {
            chain_id: chain.into(),
            validators: vec![config::hex_bytes(validator.as_ref())],
            bootstrap: Vec::new(),
            reach: Vec::new(),
            coordination: None,
            block_time_ms: config::DEFAULT_BLOCK_TIME_MS,
            genesis: "ab".repeat(32),
            modules: vec![config::ModuleCode {
                id: "pages".into(),
                code_hash: "11".repeat(32),
            }],
        }
        .save(&dir.join("network.toml"))
        .expect("save descriptor");
        let node_toml = format!(
            "network = \"network.toml\"\nkey_file = \"identity.key\"\n\
             listen = \"127.0.0.1:0\"\nadvertised = \"127.0.0.1:9000\"\n\
             storage_dir = 'storage'\nhttp_listen = \"{http}\"\n\
             gateway_listen = \"127.0.0.1:0\"\nrpc_listen = \"{rpc}\"\n\
             wireguard_listen = \"0.0.0.0:51820\"\ninvite_listen = \"0.0.0.0:51821\"\n\
             wireguard_advertised = \"auto\"\nprimary_coordinator = \"none\"\n\
             coordinator_relay = \"none\"\ncheckpoint_blocks = 32\n"
        );
        std::fs::write(dir.join("node.toml"), node_toml).expect("write node.toml");
        dir
    }

    /// `--node mynet#d0cdf950` parses, OUTRANKS `-n`, and then dies inside
    /// reqwest's url parser as `builder error` — a silent misdirection
    /// reported by the wrong layer. Refuse it here, where the flag was named,
    /// and point at the flag that would have taken it.
    #[test]
    fn a_node_flag_that_is_not_a_url_is_refused_where_it_was_typed() {
        let chain_id = addr(Some("mynet#d0cdf950"), None).ladder_rung(None, || None);
        let Err(why) = rung_base(chain_id) else {
            panic!("a chain id is not an http base");
        };
        assert!(
            why.contains("--node"),
            "it names the flag that took it: {why}"
        );
        assert!(
            why.contains("-n/--network"),
            "and the flag that should have: {why}"
        );

        // the env rung is the same input by another name, and says so.
        let Err(why) =
            rung_base(addr(None, None).ladder_rung(Some("mynet#d0cdf950".into()), || None))
        else {
            panic!("an env chain id is not an http base either");
        };
        assert!(why.contains("DUCKTAPE_NODE"), "{why}");

        // and a real base still resolves, trailing slash and all.
        assert_eq!(
            rung_base(addr(Some("https://node.example:8844/"), None).ladder_rung(None, || None))
                .unwrap(),
            "https://node.example:8844"
        );
    }

    /// an empty flag/env/context value is NOT an address — an exported but empty
    /// `DUCKTAPE_NODE` must fall through, not resolve to `""`.
    #[test]
    fn an_empty_value_is_not_a_rung() {
        assert!(matches!(
            addr(Some(""), Some("")).ladder_rung(Some(String::new()), || Some(String::new())),
            Rung::LoneWorkspace
        ));
    }

    /// the fifth-caller guard. `DUCKTAPE_NODE` was read by three families with
    /// three different precedences, so `ducktape fs`, `ducktape agent` and
    /// `ducktape account create` could each dial a different node in one
    /// shell. There is now exactly ONE read; a family that hand-writes its own
    /// ladder fails here instead of shipping a fourth answer.
    ///
    /// `bin/node/src/mcp/` is not an exception to find: it binds a RUN's tool
    /// plane to its node through `mcp::identity::ENV_NODE`, which is a different
    /// consumer and not a CLI addressing flag.
    // ponytail: matches the literal `env::var("DUCKTAPE_NODE")` call, so a
    // fifth caller routing through a const would slip past. Escalate to a full
    // parse only if that ever actually happens.
    #[test]
    fn the_cli_reads_ducktape_node_in_exactly_one_place() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut readers = Vec::new();
        let mut stack = vec![src.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read src dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("read source");
                let reads_env = text.contains(r#"env::var("DUCKTAPE_NODE")"#)
                    || text.contains(r#"env::var_os("DUCKTAPE_NODE")"#);
                if reads_env {
                    readers.push(
                        path.strip_prefix(&src)
                            .expect("under src")
                            .display()
                            .to_string(),
                    );
                }
            }
        }
        readers.sort();
        assert_eq!(
            readers,
            vec!["cli_args.rs".to_string()],
            "DUCKTAPE_NODE must be read only by the one node-addressing ladder"
        );
    }

    /// Every rung of the ladder that refuses with a CHOICE renders it through
    /// [`config::workspace_choices`], so no two of them can disagree about
    /// order or content — four hand-rolled lists printed the bare chain id,
    /// and on a box with a founder and its joiner that is the same string
    /// twice, which names neither.
    // ponytail: matches the literal line a hand-rolled list formats. A fifth
    // one built some other way slips past; escalate to a parse if that happens.
    #[test]
    fn every_pick_one_list_is_rendered_by_one_function() {
        // composed, never spelled out: a literal needle would match THIS file.
        let hand_rolled_line = format!("format!(\"  {}\")", "{chain_id}");
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut hand_rolled = Vec::new();
        let mut stack = vec![src.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read src dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("read source");
                if text.contains(&hand_rolled_line) {
                    hand_rolled.push(
                        path.strip_prefix(&src)
                            .expect("under src")
                            .display()
                            .to_string(),
                    );
                }
            }
        }
        assert!(
            hand_rolled.is_empty(),
            "a chain-id-only choice list cannot be acted on — render it with \
             config::workspace_choices: {hand_rolled:?}"
        );
    }
}
