//! node.toml — the raw file shapes and the workspace plumbing merge that
//! keeps init/join idempotent.
//!
//! Two shapes, two structs, no unions:
//! - [`NodeToml`] is the OPERATOR file (network shape). Every key is
//!   REQUIRED: a file missing one refuses to parse loudly instead of
//!   silently meaning something, and `deny_unknown_fields` refuses retired
//!   keys the same way. init/join always write the complete set, so the
//!   file is its own documentation — no bare node.toml.
//! - [`DevSeedToml`] is the dev-seed harness shape (cluster e2e, wg-smoke):
//!   seed-derived identities, no descriptor, minimal keys. Not an operator
//!   surface.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Deserialize as _;

use super::DEFAULT_CHECKPOINT_BLOCKS;
use super::PlumbingOverrides;
use super::default_primary_coordinator;

/// the generated defaults: a fresh init/join with no flags yields a node
/// with every surface up. Loopback for the operator surfaces (HTTP app
/// API, browser gateway, admin RPC), dual-stack for the mesh, and the
/// conventional WireGuard port for the tunnel plane.
///
/// EVERY eagerly-bound TCP default sits BELOW [`EPHEMERAL_FLOOR`], and that is
/// a correctness property rather than tidiness — see
/// [`no_tcp_default_sits_in_the_ephemeral_range`]. The mesh listener was at
/// `52200`, inside the range, where any outbound connection on the box can take
/// the port first; commonware's mesh listener `expect`s its bind, so
/// losing that race is an unwinding panic ten seconds into boot. It now sits
/// beside the two operator surfaces, which were never at risk.
pub const DEFAULT_MESH_LISTEN: &str = "[::]:8846";
/// every interface: the app API is a network peer's surface, not a localhost
/// service. Its access rules do not depend on the bind — reads are open, a
/// mutation carries a per-request user signature, the operator credential is
/// honored only from a loopback peer, `/v1/admin/*` follows `DUCKTAPE_ADMIN` —
/// and every co-located process dials it over loopback whatever it is bound
/// to (`http_base_of`).
pub const DEFAULT_HTTP_LISTEN: &str = "0.0.0.0:8844";
pub const DEFAULT_RPC_LISTEN: &str = "127.0.0.1:8845";
/// port 0 on purpose: the browser gateway prints its bound port and its
/// consumers re-read it per session; a fixed port would only collide.
pub const DEFAULT_GATEWAY_LISTEN: &str = "127.0.0.1:0";
/// UDP, and deliberately still the CONVENTIONAL WireGuard port even though it
/// is inside the ephemeral range: a firewall rule, a NAT forward and an
/// operator's muscle memory all key on 51820, and the tunnel plane answers a
/// bind failure with a logged retry rather than a panic — so the trade the mesh
/// port made does not apply here.
pub const DEFAULT_WIREGUARD_LISTEN: &str = "0.0.0.0:51820";
/// `ducktape-noded`'s bind when `--listen` is absent: the node's HTTP port on
/// loopback only. The desktop shell spawns that daemon and dials it where it
/// dials a node, [`DEFAULT_APP_RPC`], so the two ports are one.
pub const DEFAULT_NODED_LISTEN: &str = "127.0.0.1:8844";
/// `ducktape-simnode`'s bind when `--listen` is absent. It sits outside the
/// node's operator block (HTTP, admin RPC, mesh) on purpose, and that is a
/// correctness property rather than tidiness: the sim is a dev tool run BESIDE
/// a node, so a shared port makes the second process to boot die on its bind,
/// and makes a client on that port reach whichever daemon won, answering an
/// admin-RPC caller with `/v1` http.
pub const DEFAULT_SIMNODE_LISTEN: &str = "127.0.0.1:8850";
/// where a client on this machine (the desktop app) finds a node started with
/// the defaults: [`DEFAULT_HTTP_LISTEN`] as a co-located process dials it
/// (`http_base_of`). A CLIENT default: no node.toml key reads it.
pub const DEFAULT_APP_RPC: &str = "http://127.0.0.1:8844";

/// The bottom of Linux's default `ip_local_port_range` (32768–60999). A
/// listener whose default port sits above this is racing every outbound
/// connection on the host for it.
#[cfg(test)]
pub const EPHEMERAL_FLOOR: u16 = 32768;

/// the operator node.toml — the network shape, every key required.
///
/// Where "unset" is a meaningful state it is an EXPLICIT value, never a
/// missing key: `"none"` (primary_coordinator, coordinator_relay),
/// `"auto"` (wireguard_advertised), `0` = probe ([sandbox] cores/mem_gb).
/// the ONE table-level exception is `[sandbox]`: its PRESENCE is the
/// compute-plane switch (see [`SandboxToml`]), so a consensus-only node
/// simply has no such table.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeToml {
    /// path to the network descriptor, resolved beside this file.
    pub network: String,
    /// path to the identity secret, resolved beside this file.
    pub key_file: String,
    /// the p2p mesh listener.
    pub listen: String,
    /// what peers are told to dial: `"overlay"` (the chain-derived ULA —
    /// the right value for a member behind NAT) or a concrete dialable
    /// address.
    pub advertised: String,
    pub storage_dir: String,
    /// the HTTP app API.
    pub http_listen: String,
    /// the least-privilege browser gateway; must bind 127.0.0.1.
    pub gateway_listen: String,
    /// the local admin RPC bridge.
    pub rpc_listen: String,
    /// the UDP endpoint of this node's WireGuard tunnel plane.
    pub wireguard_listen: String,
    /// the UDP invite intro listener (where a fresh joiner announces its
    /// keys, token-authenticated, before any p2p).
    pub invite_listen: String,
    /// the advertised tunnel endpoint, independent of the bind:
    /// `"host:port"`, or `"auto"` = derive from `wireguard_listen` (its IP
    /// when concrete, endpoint-less/roaming when unspecified).
    pub wireguard_advertised: String,
    /// the ambient rendezvous coordinator: `"host:port"`, or `"none"` to
    /// run without coordination.
    pub primary_coordinator: String,
    /// the TCP first-contact fallback relay: `"host:port"`, or `"none"`.
    pub coordinator_relay: String,
    /// sealed blocks between recovery checkpoints.
    pub checkpoint_blocks: u64,
    /// the compute plane: PRESENT = provider runs execute in this sandbox;
    /// ABSENT = consensus-only node (no provider discovery, no announce, no
    /// terminal plane).
    pub sandbox: Option<SandboxToml>,
}

/// the `[sandbox]` compute-plane table. its PRESENCE is what makes a node a
/// compute node; inside it every key is required. there is deliberately no
/// bare/"direct" runtime — a provider run never executes directly on the
/// host, so the only selectable adapters are the audited in-tree ones.
///
/// It names no path. The guest kernel and rootfs every run boots are the
/// workspace's own (`<workspace>/guest/`, `crate::guest_dir`), shared
/// read-only across that workspace's concurrent runs, and the agent CLIs a
/// run may exec are `<workspace>/executors/` — so a table copied between
/// workspaces or machines points at nothing that is not there.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxToml {
    /// the isolation adapter: `"firecracker"` (Linux) or `"vz"` (macOS) — one
    /// microVM per run either way.
    pub runtime: String,
    /// announced capacity; `0` = probe the host.
    pub cores: u64,
    /// announced capacity in GiB; `0` = probe the host.
    pub mem_gb: u64,
}

impl NodeToml {
    /// `wireguard_advertised` with the sentinel mapped back to the runtime
    /// derivation: `"auto"` means "derive from `wireguard_listen`".
    pub fn wireguard_advertised_value(&self) -> Option<&str> {
        let is_auto = self.wireguard_advertised == "auto";
        (!is_auto).then_some(self.wireguard_advertised.as_str())
    }
}

/// the dev-seed harness shape: deterministic seed identities, no
/// descriptor. every peer's dial address rides `peer_addrs`, index-aligned
/// with `peer_seeds` — the mesh transport has no address gossip, so the
/// full list must come from config (the harness knows every port). Only
/// the test harnesses write this shape, so its plumbing stays optional —
/// a harness file says exactly what the test needs and nothing else.
#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevSeedToml {
    pub id: u64,
    pub namespace: String,
    pub peer_seeds: Vec<u64>,
    pub validator_seeds: Option<Vec<u64>>,
    /// the directory holding `<id>.component.wasm` for every wasm tenant — the
    /// dev shape has no descriptor, so its genesis code set is DERIVED from
    /// these files (every node of a dev cluster must point at identical bytes).
    ///
    /// ABSENT means the set the running binary's own build staged beside it
    /// ([`crate::modules_dir`]), which is what every node of a dev cluster run
    /// from one checkout resolves — and the only thing a CHECKED-IN example
    /// can say, since that directory carries the checkout's path in its name
    /// ([`crate::staged_key`]).
    pub modules: Option<String>,
    /// one dial address per `peer_seeds` entry, same order. optional only
    /// for a SOLO node (nobody to dial); a multi-node cluster without it
    /// is refused at resolve.
    pub peer_addrs: Option<Vec<String>>,
    pub listen: String,
    pub advertised: Option<String>,
    pub storage_dir: Option<String>,
    pub rpc_listen: Option<String>,
    pub http_listen: Option<String>,
    pub gateway_listen: Option<String>,
    pub checkpoint_blocks: Option<u64>,
    pub block_time_ms: Option<u64>,
    /// PRESENT = the reachability plane runs (userspace socket backend).
    pub wireguard_listen: Option<String>,
    pub invite_listen: Option<String>,
    pub wireguard_advertised: Option<String>,
    pub primary_coordinator: Option<String>,
    pub coordinator_relay: Option<String>,
    pub sandbox: Option<SandboxToml>,
}

/// both file shapes, discriminated by the `network` key: PRESENT means the
/// operator (network) shape.
pub enum RawNodeToml {
    Network(NodeToml),
    DevSeed(DevSeedToml),
}

/// read a raw node.toml plus its base directory (which relative paths
/// inside the file resolve against). the `network` key picks the shape;
/// each shape then parses STRICTLY (all required keys present, no unknown
/// keys).
pub fn load_raw_node_toml(cfg_path: &Path) -> Result<(RawNodeToml, PathBuf), String> {
    let text = std::fs::read_to_string(cfg_path).map_err(|e| format!("read {cfg_path:?}: {e}"))?;
    let value: toml::Value = toml::from_str(&text).map_err(|e| format!("{cfg_path:?}: {e}"))?;
    let base = cfg_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let is_network_shape = value.get("network").is_some();
    let raw = if is_network_shape {
        RawNodeToml::Network(
            NodeToml::deserialize(value).map_err(|e| format!("{cfg_path:?}: {e}"))?,
        )
    } else {
        RawNodeToml::DevSeed(
            DevSeedToml::deserialize(value).map_err(|e| format!("{cfg_path:?}: {e}"))?,
        )
    };
    Ok((raw, base))
}

/// read a network-shape node.toml, refusing the dev-seed shape — the
/// workspace verbs (init/join/invite) only ever operate on operator files.
pub fn load_node_toml(cfg_path: &Path) -> Result<(NodeToml, PathBuf), String> {
    match load_raw_node_toml(cfg_path)? {
        (RawNodeToml::Network(raw), base) => Ok((raw, base)),
        (RawNodeToml::DevSeed(_), _) => Err(format!(
            "{cfg_path:?} is a dev-seed (harness) config — the workspace verbs need the \
             network shape"
        )),
    }
}

/// a workspace's COMPLETE plumbing (everything in node.toml that is not
/// the network reference) — every field concrete, mirroring the required
/// file 1:1. Built by [`merged_plumbing`] from three layers: explicit
/// flags win, else the values an EXISTING network-shape node.toml already
/// carries, else the WORKING defaults (`DEFAULT_*`: mesh, HTTP, gateway,
/// RPC, and the WireGuard plane all up — a flagless init/join yields a
/// node that works out of the box). always writing the merged result makes
/// init/join idempotent AND partial-flag-safe (one flag never resets the
/// others).
pub struct Plumbing {
    pub listen: String,
    pub advertised: String,
    pub storage_dir: String,
    pub http_listen: String,
    pub gateway_listen: String,
    pub rpc_listen: String,
    pub wireguard_listen: String,
    pub invite_listen: String,
    pub wireguard_advertised: String,
    pub primary_coordinator: String,
    pub coordinator_relay: String,
    pub checkpoint_blocks: u64,
    pub sandbox: Option<SandboxToml>,
}

pub fn merged_plumbing(dir: &Path, overrides: &PlumbingOverrides) -> Result<Plumbing, String> {
    let listen = overrides.listen.as_deref();
    let advertised = overrides.advertised.as_deref();
    let http_listen = overrides.http.as_deref();
    let gateway_listen = overrides.gateway.as_deref();
    let rpc_listen = overrides.rpc.as_deref();
    let wireguard_listen = overrides.wireguard_listen.as_deref();
    let invite_listen = overrides.invite_listen.as_deref();
    let primary_coordinator = overrides.primary_coordinator.as_deref();
    let wireguard_advertised = overrides.wireguard_advertised.as_deref();
    let path = dir.join("node.toml");
    // an existing file must be a VALID network-shape file to contribute —
    // an incomplete or dev-seed file aborts the verb instead of being
    // silently half-inherited.
    let existing: Option<NodeToml> = if path.exists() {
        Some(load_node_toml(&path)?.0)
    } else {
        None
    };
    let e = existing.as_ref();
    let listen = listen
        .map(str::to_string)
        .or_else(|| e.map(|r| r.listen.clone()))
        .unwrap_or_else(|| DEFAULT_MESH_LISTEN.into());
    // "overlay" needs an IPv6 mesh listener (members reverse-dial the ULA
    // over tunnels); a v4-only listen advertises its own socket address.
    let derived_advertised = if listen.starts_with('[') {
        "overlay".to_string()
    } else {
        listen.clone()
    };
    let wireguard_listen = wireguard_listen
        .map(str::to_string)
        .or_else(|| e.map(|r| r.wireguard_listen.clone()))
        .unwrap_or_else(|| DEFAULT_WIREGUARD_LISTEN.into());
    let derived_invite_listen = derive_invite_listen(&wireguard_listen)?;
    let primary_coordinator = primary_coordinator
        .map(str::to_string)
        .or_else(|| e.map(|r| r.primary_coordinator.clone()))
        .unwrap_or_else(default_primary_coordinator);
    let derived_relay = derive_coordinator_relay(&primary_coordinator);
    Ok(Plumbing {
        advertised: advertised
            .map(str::to_string)
            .or_else(|| e.map(|r| r.advertised.clone()))
            .unwrap_or(derived_advertised),
        listen,
        http_listen: http_listen
            .map(str::to_string)
            .or_else(|| e.map(|r| r.http_listen.clone()))
            .unwrap_or_else(|| DEFAULT_HTTP_LISTEN.into()),
        gateway_listen: gateway_listen
            .map(str::to_string)
            .or_else(|| e.map(|r| r.gateway_listen.clone()))
            .unwrap_or_else(|| DEFAULT_GATEWAY_LISTEN.into()),
        rpc_listen: rpc_listen
            .map(str::to_string)
            .or_else(|| e.map(|r| r.rpc_listen.clone()))
            .unwrap_or_else(|| DEFAULT_RPC_LISTEN.into()),
        storage_dir: e
            .map(|r| r.storage_dir.clone())
            .unwrap_or_else(|| "storage".into()),
        invite_listen: invite_listen
            .map(str::to_string)
            .or_else(|| e.map(|r| r.invite_listen.clone()))
            .unwrap_or(derived_invite_listen),
        wireguard_advertised: wireguard_advertised
            .map(str::to_string)
            .or_else(|| e.map(|r| r.wireguard_advertised.clone()))
            .unwrap_or_else(|| "auto".into()),
        coordinator_relay: e
            .map(|r| r.coordinator_relay.clone())
            .unwrap_or(derived_relay),
        checkpoint_blocks: e
            .map(|r| r.checkpoint_blocks)
            .unwrap_or(DEFAULT_CHECKPOINT_BLOCKS),
        sandbox: e.and_then(|r| r.sandbox.clone()),
        wireguard_listen,
        primary_coordinator,
    })
}

/// the intro listener default: `wireguard_listen`'s port + 1, computed at
/// GENERATION time — the file always carries the concrete value.
fn derive_invite_listen(wireguard_listen: &str) -> Result<String, String> {
    let addr: std::net::SocketAddr = wireguard_listen
        .parse()
        .map_err(|e| format!("wireguard_listen {wireguard_listen:?}: {e}"))?;
    let intro_port = addr
        .port()
        .checked_add(1)
        .ok_or_else(|| format!("wireguard_listen {wireguard_listen:?}: no room for port + 1"))?;
    Ok(format!("0.0.0.0:{intro_port}"))
}

/// the relay default: the coordinator's host on the relay port
/// ([`nat_traversal::RELAY_PORT`]), or `"none"` when coordination itself is
/// off — computed at GENERATION time.
fn derive_coordinator_relay(primary_coordinator: &str) -> String {
    let coordination_off = matches!(primary_coordinator, "none" | "off" | "direct");
    if coordination_off {
        return "none".into();
    }
    let host = primary_coordinator
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(primary_coordinator);
    format!("{host}:{}", nat_traversal::RELAY_PORT)
}

/// one entry: a `# note` line ABOVE its live `key = value` line, blank-line
/// separated — the file reads as its own reference sheet.
fn keyline(s: &mut String, key: &str, value: std::fmt::Arguments<'_>, note: &str) {
    let _ = writeln!(s, "\n# {note}\n{key} = {value}");
}

/// a STRING entry: the value is encoded by the TOML serializer, never by
/// hand. Every string this file carries is a path or an address, and a path
/// is filesystem bytes — an apostrophe, a double quote and a backslash are
/// all legal in one and all fatal to hand-built quoting (`'…'` around a name
/// holding an apostrophe is invalid TOML; a backslash-n inside `"…"` is a
/// newline, silently a different path).
fn keystr(s: &mut String, key: &str, value: &str, note: &str) {
    let encoded = toml::Value::String(value.to_string());
    keyline(s, key, format_args!("{encoded}"), note);
}

/// write the network-shape node.toml (init/join): the COMPLETE key set,
/// every key live under a brief comment — the parser requires every key, so
/// the file IS the reference: what each key does, and what its sentinel
/// values mean. the file references its siblings relatively, so the whole
/// dir is relocatable.
pub fn write_node_toml(dir: &Path, p: &Plumbing) -> Result<PathBuf, String> {
    let mut s = String::from(
        "# ducktape node config (network shape) — see network.toml for the network.\n\
         # every key is required; edit values, don't delete lines (rewrites re-fill).\n",
    );
    keystr(
        &mut s,
        "network",
        "network.toml",
        "the network descriptor, beside this file",
    );
    keystr(
        &mut s,
        "key_file",
        "identity.key",
        "this node's identity secret, beside this file",
    );
    keystr(
        &mut s,
        "listen",
        &p.listen,
        "p2p mesh listener (dual-stack)",
    );
    keystr(
        &mut s,
        "advertised",
        &p.advertised,
        "what peers dial: \"overlay\" = the chain ULA, or host:port",
    );
    keystr(
        &mut s,
        "storage_dir",
        &p.storage_dir,
        "chain + module state, beside this file",
    );
    keystr(
        &mut s,
        "http_listen",
        &p.http_listen,
        "HTTP app API; reads open to any peer, writes signed",
    );
    keystr(
        &mut s,
        "gateway_listen",
        &p.gateway_listen,
        "browser gateway; loopback only, port 0 = pick free",
    );
    keystr(
        &mut s,
        "rpc_listen",
        &p.rpc_listen,
        "local admin RPC (keep loopback)",
    );
    keystr(
        &mut s,
        "wireguard_listen",
        &p.wireguard_listen,
        "the WireGuard tunnel plane (UDP)",
    );
    keystr(
        &mut s,
        "invite_listen",
        &p.invite_listen,
        "invite intro listener (UDP; convention: wireguard port + 1)",
    );
    keystr(
        &mut s,
        "wireguard_advertised",
        &p.wireguard_advertised,
        "tunnel endpoint peers dial; \"auto\" = derive from wireguard_listen",
    );
    keystr(
        &mut s,
        "primary_coordinator",
        &p.primary_coordinator,
        "ambient rendezvous coordinator; \"none\" disables",
    );
    keystr(
        &mut s,
        "coordinator_relay",
        &p.coordinator_relay,
        "TCP first-contact fallback; \"none\" disables",
    );
    keyline(
        &mut s,
        "checkpoint_blocks",
        format_args!("{}", p.checkpoint_blocks),
        "sealed blocks between recovery checkpoints",
    );
    // the [sandbox] table LAST — everything after a toml table header belongs
    // to the table, so no top-level key may follow it.
    match &p.sandbox {
        Some(sb) => {
            let _ = writeln!(
                s,
                "\n# compute plane: provider runs execute inside this sandbox and the node\n\
                 # can announce capabilities. delete the whole table for a consensus-only node.\n\
                 [sandbox]"
            );
            keystr(
                &mut s,
                "runtime",
                &sb.runtime,
                "isolation adapter: \"firecracker\" on Linux, \"vz\" on macOS (runs never execute bare on the host)",
            );
            keyline(
                &mut s,
                "cores",
                format_args!("{}", sb.cores),
                "announced capacity; 0 = probe the host",
            );
            keyline(
                &mut s,
                "mem_gb",
                format_args!("{}", sb.mem_gb),
                "announced capacity (GiB); 0 = probe the host",
            );
        }
        None => {
            // the commented-out template names THIS OS's adapter, so
            // uncommenting it on the machine `node init` ran on is enough; the
            // images it boots are this workspace's own guest/ directory.
            let runtime = sandbox_host::Vmm::platform_default().config_token();
            let _ = writeln!(
                s,
                "\n# compute plane (off): uncomment [sandbox] to run providers on this node.\n\
                 # runtime: \"{runtime}\" — one microVM per run; runs never execute\n\
                 # bare on the host. The two images it boots live in this workspace's\n\
                 # guest/ directory: OUT=<workspace>/guest ops/build-guest-rootfs.sh\n\
                 #[sandbox]\n\
                 #runtime = \"{runtime}\"\n\
                 #cores = 0\n\
                 #mem_gb = 0"
            );
        }
    }
    // publish only what survives its own encoding. This file is a REWRITE:
    // it replaces a workspace's live plumbing, so a value that does not read
    // back as itself is a corrupted workspace, not a cosmetic defect. Parsing
    // the rendered text before it reaches the disk turns any future quoting
    // slip into a loud error at the verb that caused it.
    let rendered: NodeToml = toml::from_str(&s).map_err(|e| format!("node.toml render: {e}"))?;
    let drifted = [
        ("listen", &rendered.listen, &p.listen),
        ("advertised", &rendered.advertised, &p.advertised),
        ("storage_dir", &rendered.storage_dir, &p.storage_dir),
        ("http_listen", &rendered.http_listen, &p.http_listen),
        (
            "gateway_listen",
            &rendered.gateway_listen,
            &p.gateway_listen,
        ),
        ("rpc_listen", &rendered.rpc_listen, &p.rpc_listen),
        (
            "wireguard_listen",
            &rendered.wireguard_listen,
            &p.wireguard_listen,
        ),
        ("invite_listen", &rendered.invite_listen, &p.invite_listen),
        (
            "wireguard_advertised",
            &rendered.wireguard_advertised,
            &p.wireguard_advertised,
        ),
        (
            "primary_coordinator",
            &rendered.primary_coordinator,
            &p.primary_coordinator,
        ),
        (
            "coordinator_relay",
            &rendered.coordinator_relay,
            &p.coordinator_relay,
        ),
    ]
    .into_iter()
    .find(|(_, got, want)| got != want);
    if let Some((key, got, want)) = drifted {
        return Err(format!(
            "node.toml render: {key} did not round-trip ({want:?} -> {got:?})"
        ));
    }
    let sandbox_drifted = rendered.sandbox != p.sandbox;
    if sandbox_drifted {
        return Err(format!(
            "node.toml render: [sandbox] did not round-trip ({:?} -> {:?})",
            p.sandbox, rendered.sandbox
        ));
    }
    let path = dir.join("node.toml");
    std::fs::write(&path, s).map_err(|e| format!("write {path:?}: {e}"))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ducktape-config-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn fresh_default_plumbing(dir: &Path) -> Plumbing {
        merged_plumbing(dir, &PlumbingOverrides::default()).expect("fresh merge")
    }

    /// No TCP listener a node binds EAGERLY may default into the ephemeral
    /// range, because the kernel hands those ports out to outbound connections
    /// and the loser of that race is a node that will not start.
    ///
    /// The mesh listener is the one that bit: it sat at `52200`, and
    /// commonware's mesh listener `expect`s its bind inside the runtime,
    /// so losing the race was `thread 'tokio-rt-worker' panicked … BindFailed`
    /// ten seconds into an otherwise healthy boot.
    ///
    /// Scoped to TCP on purpose. `wireguard_listen` is UDP, is the conventional
    /// 51820 that firewalls and NAT forwards are written against, and answers a
    /// failed bind with a logged retry rather than a panic — so it is named
    /// here as a deliberate exclusion instead of quietly not being checked.
    ///
    /// The re-found routine's set is a default too — every network it founds
    /// runs on it, and it names its ports explicitly, so the constants never
    /// reach that node.toml. It sat at 32989+, and a resident restarting there
    /// lost its http port to an outbound socket for 25 s.
    #[test]
    fn no_tcp_default_sits_in_the_ephemeral_range() {
        let flagless = [
            ("listen", DEFAULT_MESH_LISTEN),
            ("http_listen", DEFAULT_HTTP_LISTEN),
            ("rpc_listen", DEFAULT_RPC_LISTEN),
        ]
        .map(|(key, value)| (key, value.rsplit_once(':').map_or(value, |(_, port)| port)));
        // `F_HTTP=28800 F_GATEWAY=…`: the founder's (F_) and the resident's
        // (J_) tcp surfaces. WG and INVITE are the UDP exclusion above.
        let refound = include_str!("../../../ops/refound-net.sh");
        assert!(
            !refound.contains("DUCKTAPE_MODULES_DIR="),
            "refound launcher/service invocations must use current/modules from each release"
        );
        let routine: Vec<(&str, &str)> = refound
            .split_whitespace()
            .filter_map(|word| word.split_once('='))
            .filter(|(key, _)| {
                let surface = key.strip_prefix("F_").or_else(|| key.strip_prefix("J_"));
                surface.is_some_and(|surface| ["HTTP", "GATEWAY", "RPC", "P2P"].contains(&surface))
            })
            .filter(|(_, value)| {
                !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
            })
            .collect();
        assert_eq!(
            routine.len(),
            8,
            "four tcp surfaces each for the founder and the resident: {routine:?}"
        );
        for (key, value) in flagless.into_iter().chain(routine) {
            let port = value
                .parse::<u16>()
                .unwrap_or_else(|_| panic!("{key} default {value:?} names a port"));
            assert!(
                port < EPHEMERAL_FLOOR,
                "{key} defaults to {port}, inside the kernel's ephemeral range \
                 ({EPHEMERAL_FLOOR}+) — it will lose bind races to outbound sockets"
            );
        }
        // the gateway is the one exception and it is the SAFE direction: port 0
        // asks the kernel for a free port instead of racing for a fixed one.
        assert!(DEFAULT_GATEWAY_LISTEN.ends_with(":0"));
    }

    /// the sim binary and a real node are run side by side all day, so their
    /// eagerly-bound defaults must not overlap. A sim default back inside the
    /// node's operator block means the second process to boot dies on its
    /// bind, and a client on that port reaches whichever daemon won, speaking
    /// the wrong protocol.
    #[test]
    fn the_sim_default_avoids_the_nodes_operator_ports() {
        let port = |addr: &str| addr.parse::<std::net::SocketAddr>().expect("parses").port();
        let node_operator_ports =
            [DEFAULT_HTTP_LISTEN, DEFAULT_RPC_LISTEN, DEFAULT_MESH_LISTEN].map(port);
        let sim = port(DEFAULT_SIMNODE_LISTEN);
        assert!(
            !node_operator_ports.contains(&sim),
            "the sim default {DEFAULT_SIMNODE_LISTEN} is inside the node's operator block \
             {node_operator_ports:?}: it will fight a real node for the bind"
        );
    }

    /// the app's default reaches a node started with the defaults, and the
    /// local daemon the app spawns in its place serves that same port.
    #[test]
    fn the_client_defaults_name_the_nodes_http_port() {
        assert_eq!(DEFAULT_APP_RPC, crate::http_base_of(DEFAULT_HTTP_LISTEN));
        assert_eq!(DEFAULT_APP_RPC, crate::http_base_of(DEFAULT_NODED_LISTEN));
    }

    /// flagless defaults are a WORKING node: every surface up, every
    /// derivation materialized concretely.
    #[test]
    fn generated_file_is_complete_and_defaults_are_working() {
        let dir = tmp("full-print");
        let p = fresh_default_plumbing(&dir);
        write_node_toml(&dir, &p).expect("write");
        let (raw, _) = load_node_toml(&dir.join("node.toml")).expect("strict parse");
        assert_eq!(raw.listen, DEFAULT_MESH_LISTEN);
        assert_eq!(raw.advertised, "overlay");
        assert_eq!(raw.http_listen, DEFAULT_HTTP_LISTEN);
        assert_eq!(raw.rpc_listen, DEFAULT_RPC_LISTEN);
        assert_eq!(raw.gateway_listen, DEFAULT_GATEWAY_LISTEN);
        assert_eq!(raw.wireguard_listen, DEFAULT_WIREGUARD_LISTEN);
        assert_eq!(raw.invite_listen, "0.0.0.0:51821");
        assert_eq!(raw.wireguard_advertised, "auto");
        assert_eq!(raw.primary_coordinator, default_primary_coordinator());
        assert_eq!(
            raw.coordinator_relay,
            derive_coordinator_relay(&default_primary_coordinator())
        );
        assert_eq!(raw.checkpoint_blocks, DEFAULT_CHECKPOINT_BLOCKS);
        // no [sandbox] table by default: a fresh node is consensus-only, and
        // the commented example in the file must not parse as a live table.
        assert_eq!(raw.sandbox, None);
    }

    /// nothing optional: a file missing ANY key refuses to parse, and the
    /// retired `wireguard_effect` key is an unknown-field error — old files
    /// break loudly instead of half-working.
    #[test]
    fn incomplete_or_retired_files_fail_loudly() {
        let dir = tmp("strict");
        let p = fresh_default_plumbing(&dir);
        write_node_toml(&dir, &p).expect("write");
        let full = std::fs::read_to_string(dir.join("node.toml")).expect("read");

        // drop one required key → parse error naming it.
        let missing: String = full
            .lines()
            .filter(|l| !l.starts_with("rpc_listen"))
            .map(|l| format!("{l}\n"))
            .collect();
        std::fs::write(dir.join("node.toml"), missing).expect("write");
        let err = load_node_toml(&dir.join("node.toml")).expect_err("missing key must fail");
        assert!(err.contains("rpc_listen"), "{err}");

        // a retired key → unknown-field error.
        std::fs::write(
            dir.join("node.toml"),
            format!("{full}wireguard_effect = \"socket\"\n"),
        )
        .expect("write");
        let err = load_node_toml(&dir.join("node.toml")).expect_err("retired key must fail");
        assert!(err.contains("wireguard_effect"), "{err}");
    }

    /// flags win over an existing file; unflagged values survive a
    /// re-merge byte-for-byte (idempotent, partial-flag-safe).
    #[test]
    fn plumbing_merges_flags_over_existing_file_over_defaults() {
        let dir = tmp("plumbing");
        let p = fresh_default_plumbing(&dir);
        write_node_toml(&dir, &p).expect("write defaults");

        let p = merged_plumbing(
            &dir,
            &PlumbingOverrides {
                listen: Some("127.0.0.1:53000".to_string()),
                http: Some("127.0.0.1:53001".to_string()),
                ..Default::default()
            },
        )
        .expect("merge");
        assert_eq!(p.listen, "127.0.0.1:53000");
        assert_eq!(p.http_listen, "127.0.0.1:53001");
        // unflagged values came from the existing file, not re-derivation:
        // advertised stays "overlay" (from the file) even though the new
        // listen is v4.
        assert_eq!(p.advertised, "overlay");
        assert_eq!(p.rpc_listen, DEFAULT_RPC_LISTEN);
        write_node_toml(&dir, &p).expect("rewrite");
        let (raw, _) = load_node_toml(&dir.join("node.toml")).expect("reload");
        assert_eq!(raw.listen, "127.0.0.1:53000");
        assert_eq!(raw.http_listen, "127.0.0.1:53001");
        assert_eq!(raw.rpc_listen, DEFAULT_RPC_LISTEN);
    }

    /// file-only keys (no CLI flag) survive every rewrite via the same
    /// chain — a hand-edit is never silently reset.
    #[test]
    fn hand_edited_values_survive_rewrite() {
        let dir = tmp("hand-edit");
        let p = fresh_default_plumbing(&dir);
        write_node_toml(&dir, &p).expect("write defaults");
        let edited = std::fs::read_to_string(dir.join("node.toml"))
            .expect("read")
            .replace("checkpoint_blocks = 32", "checkpoint_blocks = 7")
            + "\n[sandbox]\nruntime = \"firecracker\"\ncores = 4\nmem_gb = 0\n";
        std::fs::write(dir.join("node.toml"), edited).expect("write");
        let p = merged_plumbing(&dir, &PlumbingOverrides::default()).expect("merge");
        write_node_toml(&dir, &p).expect("rewrite");
        let (raw, _) = load_node_toml(&dir.join("node.toml")).expect("reload");
        assert_eq!(raw.checkpoint_blocks, 7);
        let sandbox = raw.sandbox.expect("hand-added [sandbox] survives rewrite");
        assert_eq!(sandbox.runtime, "firecracker");
        assert_eq!(sandbox.cores, 4);
    }

    /// a path is filesystem bytes, not a quoting-friendly subset: an
    /// apostrophe, a double quote, a backslash and non-ASCII all survive the
    /// write → read → merge → rewrite chain that init/join and `sandbox
    /// enable` run. Hand-built quoting corrupted every one of them —
    /// `'…'` around a path holding an apostrophe is invalid TOML, and a
    /// literal backslash-n inside `"…"` reads back as a newline.
    #[test]
    fn awkward_paths_survive_every_rewrite() {
        let dir = tmp("awkward-paths");
        let awkward = "/srv/eddy's \"chain\"\\note/보관함";
        let p = Plumbing {
            storage_dir: awkward.to_string(),
            primary_coordinator: awkward.to_string(),
            ..fresh_default_plumbing(&dir)
        };
        write_node_toml(&dir, &p).expect("write");
        let (raw, _) = load_node_toml(&dir.join("node.toml")).expect("reload");
        assert_eq!(raw.storage_dir, awkward);
        assert_eq!(raw.primary_coordinator, awkward);

        // and through the merge every rewriting verb runs first.
        let p = merged_plumbing(&dir, &PlumbingOverrides::default()).expect("merge");
        assert_eq!(p.storage_dir, awkward);
        write_node_toml(&dir, &p).expect("rewrite");
        let (raw, _) = load_node_toml(&dir.join("node.toml")).expect("reload after rewrite");
        assert_eq!(raw.storage_dir, awkward);
        assert_eq!(raw.primary_coordinator, awkward);
    }

    /// the dev-seed shape parses through the same loader, discriminated by
    /// the absent `network` key — and the workspace verbs refuse it.
    #[test]
    fn dev_seed_shape_parses_and_workspace_verbs_refuse_it() {
        let dir = tmp("dev-seed");
        std::fs::write(
            dir.join("node.toml"),
            "id = 0\nnamespace = \"demo\"\npeer_seeds = [0]\nlisten = \"127.0.0.1:0\"\n\
             modules = \"/srv/modules\"\n",
        )
        .expect("write");
        let (raw, _) = load_raw_node_toml(&dir.join("node.toml")).expect("parse");
        assert!(matches!(raw, RawNodeToml::DevSeed(_)));
        let err = load_node_toml(&dir.join("node.toml")).expect_err("verbs refuse dev shape");
        assert!(err.contains("dev-seed"), "{err}");
    }

    #[test]
    fn derived_relay_follows_the_coordinator() {
        assert_eq!(
            derive_coordinator_relay("coord.example:3478"),
            "coord.example:443"
        );
        assert_eq!(derive_coordinator_relay("none"), "none");
    }
}
