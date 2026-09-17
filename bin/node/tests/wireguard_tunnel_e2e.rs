//! the tunnel-first invite on a LIVE WireGuard overlay
//! (`docs/records/architecture/reachability.md` §5), across two network
//! namespaces.
//!
//! Every other invite suite proves a half: `invite_e2e` and
//! `coordinated_invite_cli` run the ceremony over real sockets with no tunnel,
//! `join_request_e2e` the TCP-carrier halves, `crates/networking/wireguard`'s
//! own tests the record and handshake formats. NONE of them ever brings an
//! overlay up, so nothing proved the sentence §5 actually promises: the
//! joiner's tunnel comes up BEFORE any TCP, and there is no TCP ingress at all.
//!
//! Four legs, in order, all through the PRODUCT verbs (`node init`,
//! `node invite`, `node join`, `node run` — never a hand-written config, which
//! is how the container smoke this replaces drifted off the product path):
//!
//! 1. the joiner's chain-scoped `dt-*` overlay interface comes up, named
//!    exactly what the chain binding derives;
//! 2. the tunnel CARRIES traffic, in both directions, at the two members'
//!    derived overlay ULAs — the mesh has no other way to reach the founder,
//!    because
//! 3. neither node has a kernel TCP listener on the mesh port at all
//!    (`advertised = "overlay"` ⇒ the mesh listener keeps only its virtual
//!    leg), and the joiner still reaches serving-full-node;
//! 4. with EVERY underlay TCP packet to the founder rejected by nftables in
//!    the joiner's namespace, the joiner keeps folding blocks.
//!
//! ## What "a real WireGuard interface" means here
//!
//! There is no kernel interface to `ip addr show`, and there is no
//! `wireguard.ko` in the loop: the OS-interface backend is retired and the
//! node's only backend is `overlay-net`'s in-process userspace one, BoringTun
//! over smoltcp (see `crates/networking/wireguard/src/effect.rs`), so `dt-*`
//! names a virtual interface inside the node. The crypto, the wire format and
//! the cryptokey routing are the real thing; the privilege is not, and that is
//! the point — `ops/wg-smoke/run-interop.sh` proves the same stack under
//! `--cap-drop ALL`. So this lane needs privilege for the UNDERLAY only: two
//! namespaces, a veth pair and one nftables rule, with the nodes themselves
//! running unprivileged inside them.
//!
//! ## Why namespaces
//!
//! Two same-host nodes cannot both hold the tunnel-first shape: it is the
//! PRODUCT DEFAULT (`listen = "[::]:8846"` ⇒ `advertised = "overlay"`,
//! WireGuard on 51820, intro on 51821) and the ports collide, which is exactly
//! why `common::NetworkShapeCluster` forces its nodes off it. A namespace each
//! gives both nodes the untouched product defaults — only `wireguard_listen`
//! is set, to the namespace's own concrete underlay IP, because `0.0.0.0`
//! advertises no endpoint and the invite would then carry no direct path.
//! It is also the only place a TCP cut can be aimed at one peer's underlay
//! without touching the host.
//!
//! ## Where it runs
//!
//! A capable box: passwordless `sudo ip netns` + `nft` + `setpriv`. Anywhere
//! else it SKIPS with the reason on fd 2 — this is a gate on a box that can
//! hold a network, not a merge gate on CI.

mod common;

use std::io::Write as _;
use std::net::Ipv6Addr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use common::NodeProc;

/// the two namespaces' underlay addresses. RFC1918, on a /24 that exists only
/// inside this rig.
const FOUNDER_UNDERLAY: &str = "10.77.0.1";
const JOINER_UNDERLAY: &str = "10.77.0.2";
/// the product's own WireGuard port — a namespace each means no override.
const WG_PORT: u16 = 51820;

/// mesh formation, admission, statesync and the fold to head are real-time on
/// a loaded box; every wait below exits on the node's own event, so generosity
/// is free and only a WEDGE ever spends it.
const WEDGED: Duration = Duration::from_secs(180);

// ── the host's answer to "can this run here" ────────────

/// `Some(reason)` when this box cannot hold the rig.
///
/// `wireguard.ko` is deliberately NOT probed: the node's backend is in-process
/// userspace and never asks the kernel for a WireGuard interface.
fn unavailable() -> Option<String> {
    // Probed exactly the way the rig invokes them — through `sudo -n`, whose
    // `secure_path` is what resolves `ip` and `nft` out of `/usr/sbin`.
    // Probing this process's own PATH instead would skip on a box that can in
    // fact run the lane.
    const PROBES: [&[&str]; 3] = [
        &["ip", "netns", "list"],
        &["nft", "--version"],
        &["setpriv", "--version"],
    ];
    PROBES.into_iter().find_map(|probe| {
        let runs = Command::new("sudo")
            .arg("-n")
            .args(probe)
            .output()
            .is_ok_and(|out| out.status.success());
        (!runs).then(|| {
            format!(
                "`sudo -n {}` does not run here — the rig needs passwordless sudo, iproute2 \
                 and nftables",
                probe.join(" ")
            )
        })
    })
}

/// Print a skip to fd 2 DIRECTLY.
///
/// `eprintln!` routes through libtest's thread-local capture, which swallows
/// anything a PASSING test writes — the failure mode `nettest::skip_without`
/// exists to document. `Stderr::write_fmt` does not consult it. This lane
/// takes the opposite default from that helper on purpose: a box with no
/// `ip netns` is the NORMAL case (every CI runner), so a missing capability
/// here is a skip, not a failure.
fn skip(why: &str) {
    let _ = writeln!(
        std::io::stderr(),
        "SKIP tunnel_first_invite_carries_the_mesh_with_no_tcp_ingress: {why}"
    );
}

// ── the underlay ────────────────────────────────────────

/// two network namespaces joined by one veth pair.
///
/// Deleting a namespace takes its veth end, its addresses and its nftables
/// ruleset with it, so Drop is the entire teardown — a panicking assertion
/// unwinds through it and leaves the host as it found it.
struct Underlay {
    founder_ns: String,
    joiner_ns: String,
}

impl Underlay {
    fn up() -> Self {
        // Linux caps an interface name at 15 chars and the veth pair is born
        // in the HOST namespace, so both ends carry the pid and stay short.
        let tag = std::process::id();
        let rig = Self {
            founder_ns: format!("dtwg-f{tag}"),
            joiner_ns: format!("dtwg-j{tag}"),
        };
        let (founder_veth, joiner_veth) = (format!("dtf{tag}"), format!("dtj{tag}"));
        sudo(&["ip", "netns", "add", &rig.founder_ns]);
        sudo(&["ip", "netns", "add", &rig.joiner_ns]);
        sudo(&[
            "ip",
            "link",
            "add",
            &founder_veth,
            "type",
            "veth",
            "peer",
            "name",
            &joiner_veth,
        ]);
        for (ns, veth, addr) in [
            (&rig.founder_ns, &founder_veth, FOUNDER_UNDERLAY),
            (&rig.joiner_ns, &joiner_veth, JOINER_UNDERLAY),
        ] {
            sudo(&["ip", "link", "set", veth, "netns", ns]);
            sudo(&[
                "ip",
                "-n",
                ns,
                "addr",
                "add",
                &format!("{addr}/24"),
                "dev",
                veth,
            ]);
            sudo(&["ip", "-n", ns, "link", "set", veth, "up"]);
            sudo(&["ip", "-n", ns, "link", "set", "lo", "up"]);
        }
        rig
    }

    /// a `ducktape` invocation inside `ns`, running as THIS user.
    ///
    /// `ip netns exec` needs root; the node does not — its WireGuard backend
    /// is in-process userspace, so it binds nothing privileged — hence the
    /// privilege is handed straight back before the binary starts. The
    /// `--pdeathsig` is what keeps a failed run clean: `sudo` is the only
    /// child this process can see, so without it killing `sudo` would orphan
    /// a live node holding the namespace open forever.
    ///
    /// `HOME` is the rig's own, so nothing here reaches the operator's real
    /// `~/.ducktape` (sudo resets the environment to root's otherwise).
    ///
    /// `extra_env` rides the same `env` invocation as `HOME` — `sudo`
    /// resets the environment for the target user, so a var set via
    /// `Command::env` on this outer process never reaches the node; the
    /// inner coreutils `env` is the only seam that does.
    fn ducktape(&self, ns: &str, home: &Path, extra_env: &[(&str, &str)]) -> Command {
        let mut cmd = Command::new("sudo");
        cmd.args(["-n", "ip", "netns", "exec", ns, "setpriv"])
            .arg("--pdeathsig=SIGKILL")
            .arg(format!("--reuid={}", this_uid()))
            .arg(format!("--regid={}", this_gid()))
            .arg("--clear-groups")
            .arg("env")
            .arg(format!("HOME={}", home.display()));
        for (key, value) in extra_env {
            cmd.arg(format!("{key}={value}"));
        }
        cmd.arg(common::node_bin()).arg("node");
        cmd
    }

    /// the height the node in `ns` has folded, through the product's own
    /// `node status` — which reads that node's rpc on ITS loopback, which is
    /// why this has to run inside the namespace.
    fn tip(&self, ns: &str, home: &Path, workspace: &Path) -> u64 {
        let mut status = self.ducktape(ns, home, &[]);
        status
            .args(["status", "--json", "--config"])
            .arg(workspace.join("node.toml"));
        let json: serde_json::Value = serde_json::from_str(&verb(status, "node status"))
            .expect("node status --json prints json");
        json["height"]
            .as_u64()
            .unwrap_or_else(|| panic!("node status carries no height: {json}"))
    }

    /// the TCP listeners the kernel holds in `ns`, one `ss` line each.
    fn tcp_listeners(&self, ns: &str) -> String {
        let out = Command::new("sudo")
            .args(["-n", "ip", "netns", "exec", ns, "ss", "-tlnH"])
            .output()
            .expect("run ss in the namespace");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Reject EVERY underlay TCP packet between the joiner and the founder,
    /// both directions, leaving WireGuard's UDP untouched.
    fn cut_underlay_tcp(&self) {
        let ruleset = format!(
            "table inet dtcut {{\n\
             \x20 chain out {{ type filter hook output priority 0; \
             ip daddr {FOUNDER_UNDERLAY} ip protocol tcp reject; }}\n\
             \x20 chain in {{ type filter hook input priority 0; \
             ip saddr {FOUNDER_UNDERLAY} ip protocol tcp reject; }}\n\
             }}\n"
        );
        let mut nft = Command::new("sudo")
            .args([
                "-n",
                "ip",
                "netns",
                "exec",
                &self.joiner_ns,
                "nft",
                "-f",
                "-",
            ])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("spawn nft");
        nft.stdin
            .take()
            .expect("piped nft stdin")
            .write_all(ruleset.as_bytes())
            .expect("feed nft its ruleset");
        let status = nft.wait().expect("reap nft");
        assert!(status.success(), "nft refused the cut ruleset:\n{ruleset}");
    }

    /// Prove the cut BITES before trusting what survives it: the founder's
    /// app-surface port IS a plain kernel TCP listener on the underlay (it is
    /// the one thing in this shape that binds a wildcard), and it must now
    /// refuse. Without this, a mistyped rule would let leg 4 pass while
    /// proving nothing.
    fn assert_underlay_tcp_is_dead(&self) {
        let http_port = workspace_config::DEFAULT_HTTP_LISTEN
            .rsplit_once(':')
            .expect("the default http listen is host:port")
            .1;
        let reached = Command::new("sudo")
            .args([
                "-n",
                "ip",
                "netns",
                "exec",
                &self.joiner_ns,
                "timeout",
                "5",
                "bash",
                "-c",
                &format!("exec 3<>/dev/tcp/{FOUNDER_UNDERLAY}/{http_port}"),
            ])
            .output()
            .expect("run the underlay tcp probe");
        assert!(
            !reached.status.success(),
            "the cut did not bite — underlay TCP to the founder still connects, so \
             nothing below proves the overlay carried anything"
        );
    }
}

impl Drop for Underlay {
    fn drop(&mut self) {
        for ns in [&self.founder_ns, &self.joiner_ns] {
            let _ = Command::new("sudo")
                .args(["-n", "ip", "netns", "del", ns])
                .status();
        }
    }
}

fn sudo(args: &[&str]) {
    let out = Command::new("sudo")
        .arg("-n")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run sudo {args:?}: {e}"));
    assert!(
        out.status.success(),
        "sudo {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// the ids `setpriv` drops back to — this process's own, so the node runs as
/// the user who ran the test and its workspace files are that user's.
fn this_uid() -> u32 {
    // SAFETY: getuid takes no arguments, touches no memory and cannot fail.
    unsafe { libc::getuid() }
}

fn this_gid() -> u32 {
    // SAFETY: getgid, as above.
    unsafe { libc::getgid() }
}

// ── the verbs ───────────────────────────────────────────

/// run a `ducktape node …` verb to completion and answer its stdout.
fn verb(mut cmd: Command, what: &str) -> String {
    let out = cmd.output().unwrap_or_else(|e| panic!("run {what}: {e}"));
    assert!(
        out.status.success(),
        "{what} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// the overlay `/128` a member presents on this chain — the same derivation
/// `advertised = "overlay"` resolves and the tunnel installs as its AllowedIP,
/// so an exact match in a log line is proof the traffic rode the overlay and
/// not some other address that happened to work.
fn overlay_ula(namespace: &str, pubkey_hex: &str) -> Ipv6Addr {
    let raw = common::unhex(pubkey_hex);
    let identity = wireguard::ValidatorIdentity::try_from(raw.as_slice())
        .expect("a `node key` pubkey is 32 bytes");
    wireguard::ula_v6_member_addr(namespace, identity)
}

#[test]
fn tunnel_first_invite_carries_the_mesh_with_no_tcp_ingress() {
    if let Some(why) = unavailable() {
        skip(&why);
        return;
    }
    let rig = Underlay::up();
    let dir = common::e2e_tempdir("wg-tunnel");
    let home = dir.path().join("home");
    let founder_ws = dir.path().join("founder");
    let joiner_ws = dir.path().join("joiner");
    let cfg = |ws: &PathBuf| ws.join("node.toml");
    let block_time = common::TEST_BLOCK_TIME_MS.to_string();

    // ── found the network ───────────────────────────────
    //
    // everything but `wireguard-listen` is the product default: the mesh
    // listens `[::]:8846` and therefore advertises `overlay`, which is the
    // whole shape under test. `primary-coordinator none` is hermetic (the
    // namespace has no route out, and the default names the LIVE public
    // coordinator); the direct path this invite carries needs no coordinator.
    let mut init = rig.ducktape(&rig.founder_ns, &home, &[]);
    init.args(["init", "--name", "wg-tunnel-first"])
        .args(["--modules", common::founding_set()])
        .arg("--dir")
        .arg(&founder_ws)
        .args(["--primary-coordinator", "none"])
        .args([
            "--wireguard-listen",
            &format!("{FOUNDER_UNDERLAY}:{WG_PORT}"),
        ])
        .args(["--block-time-ms", &block_time]);
    verb(init, "node init");

    let mut founder_run = rig.ducktape(&rig.founder_ns, &home, &[]);
    founder_run.arg("run").arg("--config").arg(cfg(&founder_ws));
    let founder = NodeProc::spawn(
        0,
        dir.path().join("founder.log"),
        founder_run,
        "the founder",
    );
    founder.expect_line(&["rpc listening on"], WEDGED);

    // the two members' derived identities, read back through the product's own
    // `node key` (which reuses the workspace's `identity.key`).
    let mut founder_key = rig.ducktape(&rig.founder_ns, &home, &[]);
    founder_key.arg("key").arg("--dir").arg(&founder_ws);
    let founder_hex = verb(founder_key, "node key (founder)");
    let mut joiner_key = rig.ducktape(&rig.joiner_ns, &home, &[]);
    joiner_key.arg("key").arg("--dir").arg(&joiner_ws);
    let joiner_hex = verb(joiner_key, "node key (joiner)");

    // THE chain id both the interface name and every overlay ULA derive from;
    // read off the descriptor `init` wrote rather than recomputed here.
    let namespace = workspace_config::NetworkDescriptor::load(&founder_ws.join("network.toml"))
        .expect("load the founder descriptor")
        .genesis_namespace();
    let interface = reachability::binding::interface_name(&namespace);
    let founder_ula = overlay_ula(&namespace, &founder_hex);
    let joiner_ula = overlay_ula(&namespace, &joiner_hex);
    // the port a member's overlay ULA is dialled at — the product default's,
    // because nothing here overrides `listen`.
    let mesh_port = workspace_config::DEFAULT_MESH_LISTEN
        .rsplit_once(':')
        .expect("the default mesh listen is host:port")
        .1;

    // ── the invite, and the join it materializes ────────
    let mut mint = rig.ducktape(&rig.founder_ns, &home, &[]);
    mint.args(["invite", "--config"]).arg(cfg(&founder_ws));
    let blob = verb(mint, "node invite");

    let mut join = rig.ducktape(&rig.joiner_ns, &home, &[]);
    join.args(["join", &blob])
        .arg("--dir")
        .arg(&joiner_ws)
        .args(["--primary-coordinator", "none"])
        .args([
            "--wireguard-listen",
            &format!("{JOINER_UNDERLAY}:{WG_PORT}"),
        ]);
    verb(join, "node join");

    // the blob IS the VPN credential (§5): it must carry the inviter's
    // WireGuard key and a DIRECT underlay endpoint, or the joiner has no path
    // to race and everything below would be testing the coordinated lane.
    let bootstrap = std::fs::read_to_string(joiner_ws.join("invite-wireguard.toml"))
        .expect("join persists the inviter's WireGuard bootstrap");
    assert!(
        bootstrap.contains(&format!("endpoint = \"{FOUNDER_UNDERLAY}:{WG_PORT}\"")),
        "the invite must carry the inviter's direct underlay endpoint:\n{bootstrap}"
    );

    // leg 4 rides the joiner's per-block resident marker (`ducktape::consensus`
    // debug), never the checkpoint cadence (`checkpoint_due` in
    // `bin/node/src/drain_actions.rs` only fires when the root moved since the
    // last manifest) — the rig's chain goes idle right after the cut, so a
    // wait on `node_checkpoint_written` only passes by luck. RUST_LOG appends
    // to the info floor, same as the compute-daemon marker above.
    let mut joiner_run = rig.ducktape(
        &rig.joiner_ns,
        &home,
        &[("RUST_LOG", "ducktape::consensus=debug")],
    );
    joiner_run.arg("run").arg("--config").arg(cfg(&joiner_ws));
    let joiner = NodeProc::spawn(1, dir.path().join("joiner.log"), joiner_run, "the joiner");

    // ── leg 1: the interface comes up, before any mesh ──
    //
    // The name is the chain binding's, exactly: a `dt-*` that is not
    // `interface_name(chain_id)` would mean the overlay came up on some other
    // network's interface.
    joiner.expect_line(
        &[
            "overlay interface configured",
            &format!("interface={interface}"),
        ],
        WEDGED,
    );
    joiner.expect_line(
        &["invite tunnel installed", &format!("interface={interface}")],
        WEDGED,
    );

    // ── leg 2: the tunnel CARRIES, at the derived ULAs ──
    //
    // "config accepted" is not "up" — the plane logs the two separately for
    // exactly this reason — so the assertion is the handshake sampler's, and
    // in BOTH directions: each side names the other's derived overlay /128.
    joiner.expect_line(
        &[
            "peer handshake COMPLETE",
            &format!("peer_ula={founder_ula}"),
        ],
        WEDGED,
    );
    founder.expect_line(
        &["peer handshake COMPLETE", &format!("peer_ula={joiner_ula}")],
        WEDGED,
    );
    // and the MESH ITSELF rides it: the p2p dialer names the address it
    // connected to, and it is the joiner's overlay ULA at the mesh port —
    // never an underlay address. (`commonware_p2p::authenticated::lookup` is
    // at debug in the node's DEFAULT filter, deliberately: `crates/noded/src/
    // log.rs` keeps the lookup mesh's dial/handshake health admitted.)
    founder.expect_line(
        &[
            "dialed peer",
            &format!("address=[{joiner_ula}]:{mesh_port}"),
        ],
        WEDGED,
    );

    // ── leg 3: no TCP ingress at all, and it still serves ──
    //
    // §5's actual promise. `advertised = "overlay"` means neither member's
    // mesh listener keeps an OS leg, so the mesh port is not in the kernel's
    // listener table on either side — the mesh CANNOT have ridden TCP.
    for (ns, who) in [(&rig.founder_ns, "founder"), (&rig.joiner_ns, "joiner")] {
        let listeners = rig.tcp_listeners(ns);
        assert!(
            !listeners.contains(&format!(":{mesh_port} ")),
            "the {who} holds a kernel TCP listener on the mesh port {mesh_port} — the \
             tunnel-first shape promises no TCP ingress at all:\n{listeners}"
        );
    }

    let admitted = joiner.expect_line(&["ADMITTED at height", "standing is committed"], WEDGED);
    assert!(
        admitted.contains(&format!("via=direct ({FOUNDER_UNDERLAY}:{WG_PORT})")),
        "admission must have come over the invite's DIRECT tunnel path:\n{admitted}"
    );
    joiner.expect_line(
        &[
            "event=\"node_phase_transition\"",
            "role=\"resident\"",
            "phase=\"serving\"",
        ],
        WEDGED,
    );

    // ── leg 4: cut the underlay TCP, keep folding ───────
    //
    // The founder's tip at the moment of the cut is the bar: clearing it means
    // the joiner folded blocks the founder SEALED AFTER every underlay TCP
    // packet between them was rejected, so they can only have crossed the
    // tunnel. `MARGIN` makes it sustained folding rather than one straggler
    // already in flight.
    //
    // The wait rides the joiner's own stderr, not its rpc: the app surface
    // lives on loopback inside a namespace this process is not in. It rides
    // the resident's per-block "resident folded block" marker (`ducktape::
    // consensus` at debug, turned up via RUST_LOG above), not
    // `node_checkpoint_written` — a checkpoint fires only when the folded
    // root MOVED since the last manifest (`checkpoint_due` in
    // `bin/node/src/drain_actions.rs`), and this rig's chain goes idle right
    // after the cut, so waiting on a checkpoint only passes when the
    // joiner's first post-boot checkpoint happens to land after the cut. The
    // per-block marker fires every folded height regardless of cadence.
    const MARGIN: u64 = 20;
    rig.cut_underlay_tcp();
    rig.assert_underlay_tcp_is_dead();
    let bar = rig.tip(&rig.founder_ns, &home, &founder_ws) + MARGIN;
    joiner.expect_line_where(
        &format!("a folded block at or past height {bar} with the underlay TCP path cut"),
        Duration::from_secs(60),
        |line| {
            let folded = line.contains("resident folded block");
            folded && folded_height(line).is_some_and(|height| height >= bar)
        },
    );
}

/// the `height=N` field of a "resident folded block" line.
fn folded_height(line: &str) -> Option<u64> {
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("height="))
        .and_then(|height| height.parse().ok())
}
