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
    /// A consuming Cargo repository commits `.cargo/config.toml` with
    /// `[net] git-fetch-with-cli = true`: Cargo's built-in fetcher cannot
    /// run a remote helper. Setup does not touch Cargo configuration.
    Setup(SetupArgs),
}

#[derive(Debug, clap::Args)]
pub struct SetupArgs {
    /// register the node at this http base as a remote workspace: its chain
    /// id is read off its `/v1/status`
    #[arg(long, value_name = "HTTP-URL")]
    pub node: Option<String>,
}

pub(crate) fn run(cmd: ForgeCmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        ForgeCmd::Setup(args) => setup(args),
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
    let (_chain_id, registered) =
        workspace_config::resolve_chain_in(root, &address.chain).map_err(sentence)?;
    let node = registered.node_base()?;
    let via = gateway(&node)?;
    reachable_gateway(&node, &via).map_err(sentence)?;
    Ok(GitTarget {
        url: format!("{}/{}", via.trim_end_matches('/'), repository.repo),
        authority: format!("{GIT_ROUTE_LABEL}.{}.duck", repository.owner),
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
    if workspace_config::registered_networks_in(&root)?.is_empty() {
        return Err(format!(
            "no network is registered under {} — redeem an invite first (`ducktape node join \
             <invite>`), or register a node you can reach with `ducktape forge setup --node <url>`",
            root.display()
        )
        .into());
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> tempfile::TempDir {
        tempfile::tempdir().expect("scratch ducktape home")
    }

    fn gateway(base: &'static str) -> impl FnOnce(&str) -> Result<String, String> {
        move |_node| Ok(base.to_string())
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
}
