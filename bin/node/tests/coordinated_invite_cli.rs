// this binary founds networks with the real `node init`, which composes a
// founding set into the workspace genesis — the harness owns the one path to
// the set the build staged (`common::founding_set`), so it is not copied here.
mod common;

use std::io::BufRead as _;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use reachability::PersistedMesh;
use wireguard::{
    AdmissionRoot, Endpoint, EndpointAdvertisement, EndpointRecord, MeshVersion, PortPolicy, Root,
    Transport, ValidatorIdentity, X25519PublicKey,
};

/// serialize this binary's tests: every one that reaches `join` or spawns a
/// node touches real sockets (including init's default WireGuard listen), and
/// two ceremonies in flight collide on ports.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn command_output(out: &std::process::Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// wedged-detector backstop, NOT a tuned budget: the wait below is event-driven
/// (each stderr line arrives over a channel the moment the node writes it), so
/// this only fires when the node is stuck outright. Idle runs see every marker
/// in under 10s; a busy box merely arrives later on the same events.
const WEDGED_BACKSTOP: Duration = Duration::from_secs(120);

/// Event-driven wait on a spawned node's stderr — where tracing writes. A
/// reader thread forwards each line over a channel and appends it to a shared
/// transcript (the assertions also check ABSENCE of markers, and panics print
/// everything read so far). stdout stays on the log file, so a full pipe can
/// never block the child.
struct NodeStderr {
    rx: mpsc::Receiver<String>,
    transcript: Arc<Mutex<String>>,
}

impl NodeStderr {
    /// take the child's piped stderr and start the reader thread. The thread
    /// drains until EOF even if the test side stops listening, so the child can
    /// never block on a full stderr pipe.
    fn pipe(child: &mut Child) -> Self {
        let stderr = child.stderr.take().expect("piped stderr");
        let (tx, rx) = mpsc::channel();
        let transcript = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&transcript);
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stderr).lines() {
                let Ok(line) = line else { return };
                {
                    let mut t = sink.lock().unwrap_or_else(|e| e.into_inner());
                    t.push_str(&line);
                    t.push('\n');
                }
                // receiver may already be gone during teardown; keep draining.
                let _ = tx.send(line);
            }
        });
        Self { rx, transcript }
    }

    /// everything read from stderr so far — the panic/absence-check record.
    fn transcript(&self) -> String {
        self.transcript
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// block until a stderr line contains one of `markers` (returning the index
    /// of the matched marker), the child exits (stderr EOF — subsumes a
    /// `try_wait` liveness poll), or the wedged backstop fires.
    fn wait_for_any(&self, markers: &[&str]) -> Result<usize, &'static str> {
        let deadline = Instant::now() + WEDGED_BACKSTOP;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = match self.rx.recv_timeout(remaining) {
                Ok(line) => line,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("the node exited (stderr EOF)");
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err("no marker within the wedged-detector backstop");
                }
            };
            if let Some(matched) = markers.iter().position(|m| line.contains(m)) {
                return Ok(matched);
            }
        }
    }
}

/// mint (or reuse) `<dir>/identity.key` via the `keygen` verb and return its
/// pubkey hex — the join code every targeted invite locks to. `join --dir <dir>`
/// reuses this identity, so the join-side target self-check passes.
fn keygen(dir: &Path) -> String {
    let out = common::ducktape()
        .arg("node")
        .args(["key", "--dir"])
        .arg(dir)
        .output()
        .expect("run keygen");
    assert!(
        out.status.success(),
        "keygen failed:\n{}",
        command_output(&out)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

use nettest::alloc_ports;

/// the founder's genesis namespace, read from the descriptor `init` just
/// wrote, so a seeded `mesh-state.json` rides the SAME chain id the running
/// `invite` verb keys its `reachability::store::load` on. THE derivation lives
/// on the descriptor (it fingerprints the module set too, which a hand-rolled
/// copy here silently drifted from); this only loads it.
fn genesis_namespace(workspace: &Path) -> String {
    workspace_config::NetworkDescriptor::load(&workspace.join("network.toml"))
        .expect("load the founder descriptor")
        .genesis_namespace()
}

/// a signed advert for a reachable member with a routable WireGuard underlay
/// endpoint — the mesh peer `cmd_invite` maps into a DIRECT front. Mirrors the
/// node's own persistence shape so `reachability::store::load` verifies it.
fn direct_member_advert(namespace: &str, seed: u64, octet: u8) -> EndpointAdvertisement {
    let policy = PortPolicy::production();
    let signer = PrivateKey::from_seed(seed);
    let ep = |port, transport| {
        Endpoint::new(
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, octet)),
            port,
            transport,
            &policy,
        )
        .expect("endpoint within the production port policy")
    };
    let record = EndpointRecord {
        namespace: namespace.into(),
        epoch: 1,
        valset_root: Root([1; 32]),
        admission_root: AdmissionRoot([2; 32]),
        validator_identity: ValidatorIdentity::try_from(signer.public_key().as_ref()).unwrap(),
        wireguard_public_key: X25519PublicKey([octet; 32]),
        control_endpoint: ep(443, Transport::Tcp),
        wireguard_endpoint: Some(ep(51820, Transport::Udp)),
        nonce: 1,
    };
    EndpointAdvertisement::sign(record, MeshVersion([7; 32]), &signer)
}

/// lowercase hex of a seed's ed25519 public key — the identity `cmd_invite`
/// writes into a front's `member_key`.
fn identity_hex(seed: u64) -> String {
    PrivateKey::from_seed(seed)
        .public_key()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn coordinated_invite_persists_tunnel_bootstrap_without_direct_endpoint() {
    let _serial = serial();
    let dir = tempfile::tempdir().expect("tempdir");
    let founder = dir.path().join("founder");
    let friend = dir.path().join("friend");

    let init = common::ducktape()
        .arg("node")
        .args([
            "init",
            "--name",
            "coordinated-default",
            "--modules",
            common::founding_set(),
            "--dir",
            founder.to_str().expect("utf-8 founder dir"),
        ])
        .output()
        .expect("run init");
    assert!(
        init.status.success(),
        "init failed:\n{}",
        command_output(&init)
    );

    keygen(&friend);
    let invite = common::ducktape()
        .arg("node")
        .args(["invite", "--config"])
        .arg(founder.join("node.toml"))
        .output()
        .expect("run invite");
    assert!(
        invite.status.success(),
        "invite failed:\n{}",
        command_output(&invite)
    );
    let blob = String::from_utf8_lossy(&invite.stdout).trim().to_string();

    let join = common::ducktape()
        .arg("node")
        .args([
            "join",
            &blob,
            "--dir",
            friend.to_str().expect("utf-8 friend dir"),
        ])
        .output()
        .expect("run join");
    assert!(
        join.status.success(),
        "join failed:\n{}",
        command_output(&join)
    );

    let bootstrap = friend.join("invite-wireguard.toml");
    assert!(
        bootstrap.exists(),
        "coordinated invite must persist the inviter's WireGuard bootstrap"
    );
    let text = std::fs::read_to_string(&bootstrap).expect("read invite-wireguard");
    assert!(
        text.contains("public_key"),
        "bootstrap must carry the inviter's WireGuard key:\n{text}"
    );
    assert!(
        text.contains("mesh_port"),
        "bootstrap must carry the inviter's overlay mesh port:\n{text}"
    );
    assert!(
        !text.contains("endpoint"),
        "coordinated bootstrap must not bake in a direct underlay endpoint:\n{text}"
    );
}

/// The founder's coordinator rides its invite into the joiner's node.toml. A
/// founder with coordination off (the stranger lane's `--primary-coordinator
/// none`) and one on its own coordinator each hand the joiner exactly that,
/// never the compiled public default — which would point a NAT'd joiner at a
/// relay none of the network's members register with.
#[test]
fn a_joiner_takes_the_founders_coordinator() {
    let _serial = serial();
    for founding in ["none", "coord.example.net:3478"] {
        let dir = tempfile::tempdir().expect("tempdir");
        let founder = dir.path().join("founder");
        let friend = dir.path().join("friend");

        let init = common::ducktape()
            .arg("node")
            .args([
                "init",
                "--name",
                "own-coordinator",
                "--modules",
                common::founding_set(),
                "--primary-coordinator",
                founding,
                "--dir",
                founder.to_str().expect("utf-8 founder dir"),
            ])
            .output()
            .expect("run init");
        assert!(
            init.status.success(),
            "init failed:\n{}",
            command_output(&init)
        );

        let invite = common::ducktape()
            .arg("node")
            .args(["invite", "--config"])
            .arg(founder.join("node.toml"))
            .output()
            .expect("run invite");
        assert!(
            invite.status.success(),
            "invite failed:\n{}",
            command_output(&invite)
        );
        let blob = String::from_utf8_lossy(&invite.stdout).trim().to_string();

        let join = common::ducktape()
            .arg("node")
            .args(["join", &blob, "--dir"])
            .arg(&friend)
            .output()
            .expect("run join");
        assert!(
            join.status.success(),
            "join failed:\n{}",
            command_output(&join)
        );

        let (joined, _) =
            workspace_config::load_node_toml(&friend.join("node.toml")).expect("joined node.toml");
        assert_eq!(
            joined.primary_coordinator, founding,
            "the joiner must rendezvous where its founder does"
        );
    }
}

/// The unified all-paths invite bundles the inviter's reachable MEMBERS as
/// `fronts`: seed a founder's persisted `mesh-state.json` with one member that
/// has a routable WireGuard underlay endpoint, mint an invite, and prove the
/// decoded blob carries that member as a direct front. We read the fronts back
/// through `join`, which decodes the invite and persists them to
/// `invite-fronts.json` — the joiner's own record of the paths it will race.
#[test]
fn invite_bundles_reachable_member_fronts_from_seeded_mesh_state() {
    let _serial = serial();
    let dir = tempfile::tempdir().expect("tempdir");
    let founder = dir.path().join("founder");
    let friend = dir.path().join("friend");

    let init = common::ducktape()
        .arg("node")
        .args([
            "init",
            "--name",
            "fronts-bundle",
            "--modules",
            common::founding_set(),
            "--dir",
            founder.to_str().expect("utf-8 founder dir"),
        ])
        .output()
        .expect("run init");
    assert!(
        init.status.success(),
        "init failed:\n{}",
        command_output(&init)
    );
    // seed the founder's mesh with ONE reachable member (seed 7) that has a
    // routable underlay endpoint — a direct front. The persisted mesh's chain
    // id must equal the descriptor's genesis namespace or `invite` treats it as
    // a foreign file and carries no fronts.
    let namespace = genesis_namespace(&founder);
    let advert = direct_member_advert(&namespace, 7, 20);
    let mesh = PersistedMesh::new(namespace, 1, vec![advert], vec![], vec![]);
    let storage = founder.join("storage");
    std::fs::create_dir_all(&storage).expect("create founder storage");
    reachability::store::save(&storage.join("mesh-state.json"), &mesh)
        .expect("seed mesh-state.json");

    keygen(&friend);
    let invite = common::ducktape()
        .arg("node")
        .args(["invite", "--config"])
        .arg(founder.join("node.toml"))
        .output()
        .expect("run invite");
    assert!(
        invite.status.success(),
        "invite failed:\n{}",
        command_output(&invite)
    );
    let blob = String::from_utf8_lossy(&invite.stdout).trim().to_string();

    let join = common::ducktape()
        .arg("node")
        .args([
            "join",
            &blob,
            "--dir",
            friend.to_str().expect("utf-8 friend dir"),
        ])
        .output()
        .expect("run join");
    assert!(
        join.status.success(),
        "join failed:\n{}",
        command_output(&join)
    );

    // join decodes the invite and persists its fronts — proof the blob carried
    // them across.
    let fronts_file = friend.join("invite-fronts.json");
    assert!(
        fronts_file.exists(),
        "the decoded invite must carry the seeded member as a front (invite-fronts.json absent — \
         the mesh state was ignored, likely a chain-id mismatch):\n{}",
        command_output(&invite)
    );
    let fronts = std::fs::read_to_string(&fronts_file).expect("read invite-fronts.json");
    let member_hex = identity_hex(7);
    assert!(
        fronts.contains(&member_hex),
        "the front names the seeded member's identity:\n{fronts}"
    );
    assert!(
        fronts.contains("8.8.8.20:51820"),
        "the reachable member rides as a DIRECT front carrying its underlay endpoint:\n{fronts}"
    );
}

/// The field failure this branch fixes, end to end through the REAL binary: a
/// node boots while its coordinator is DARK (machine woke before its network,
/// coordinator restarting) and the coordinator only comes up LATER. The plane
/// must self-heal — retry in the background and register the moment the
/// coordinator answers — instead of degrading to pass-through for the life of
/// the process (the old behavior: one missed 3s window at boot meant no
/// rendezvous, no punch, no tunnels until an operator restarted the node by
/// hand).
#[test]
fn a_dark_coordinator_at_boot_heals_once_it_comes_up() {
    let _serial = serial();
    let dir = tempfile::tempdir().expect("tempdir");
    let founder = dir.path().join("founder");

    // distinct listen/http/rpc ports, a coordinator UDP port to leave DARK for
    // boot, and a distinct wireguard UDP port: this test binary runs its tests
    // in parallel, and two spawned nodes on init's defaults (p2p listen,
    // wireguard 0.0.0.0:51820) collide.
    //
    // All five come from ONE allocator. The coordinator and wireguard ports used
    // to be hand-rolled `UdpSocket::bind(":0")` probe-drops, which reserved
    // nothing in the allocator's handed-out set — so the `alloc_ports(3)` three
    // lines below could hand the very same number straight back out.
    let ports = alloc_ports(5);
    let coord_addr = std::net::SocketAddr::from(([127, 0, 0, 1], ports[3]));
    let wg_port = ports[4];
    let init = common::ducktape()
        .arg("node")
        .args([
            "init",
            "--name",
            "coordinator-heal",
            "--modules",
            common::founding_set(),
            "--dir",
            founder.to_str().expect("utf-8 founder dir"),
            "--wireguard-listen",
            &format!("127.0.0.1:{wg_port}"),
            "--primary-coordinator",
            &coord_addr.to_string(),
            "--listen",
            &format!("127.0.0.1:{}", ports[0]),
            "--advertised",
            &format!("127.0.0.1:{}", ports[0]),
            "--http",
            &format!("127.0.0.1:{}", ports[1]),
            "--rpc",
            &format!("127.0.0.1:{}", ports[2]),
        ])
        .output()
        .expect("run init");
    assert!(
        init.status.success(),
        "init failed:\n{}",
        command_output(&init)
    );

    // stdout goes to a file (a full pipe could block the child); stderr — where
    // tracing writes — is piped to a reader thread for event-driven waits.
    let log_path = dir.path().join("founder-heal.log");
    let out = std::fs::File::create(&log_path).expect("create node log");
    let mut child = common::ducktape()
        .arg("node")
        .arg("run")
        .arg("--config")
        .arg(founder.join("node.toml"))
        .stdout(Stdio::from(out))
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ducktape");
    let stderr = NodeStderr::pipe(&mut child);

    // act 1 — dark: the plane reports the outage and keeps the node up (an
    // early exit surfaces as stderr EOF).
    if let Err(why) = stderr.wait_for_any(&["coordinator rendezvous unavailable"]) {
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "the node must stay up and report the dark coordinator ({why}):\n{}",
            stderr.transcript()
        );
    }
    let log = stderr.transcript();
    assert!(
        !log.contains("coordinator-observed reflexive"),
        "nothing answered yet — no reflexive can exist:\n{log}"
    );

    // act 2 — the coordinator comes up on the same address...
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("coordinator runtime");
    let _coordinator = rt.spawn(async move {
        let sock = tokio::net::UdpSocket::bind(coord_addr)
            .await
            .expect("bind the late coordinator");
        nat_traversal::run_coordinator(
            nat_traversal::NatSocket::Owned(sock),
            nat_traversal::AuthPolicy::Public,
        )
        .await;
    });

    // ...and the running node registers on its own: establishment retries at
    // 3s doubling to 30s, and the channel wait follows the node's own backoff
    // instead of racing a tuned budget.
    let healed = stderr.wait_for_any(&["coordinator-observed reflexive"]);
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        healed.is_ok(),
        "the plane must establish rendezvous once the coordinator answers — \
         without a process restart:\n{}",
        stderr.transcript()
    );
}

/// Regression: an UNREACHABLE ambient coordinator must NOT take down the whole
/// reachability plane. A socket-mode node whose only coordinator is a blackhole
/// used to hard-fail at plane bring-up ("plane not started"), which then made
/// every subsequent first-contact send fail with "reachability plane is gone" —
/// killing even DIRECT joins that never needed a coordinator (and silently
/// overriding a founder's `--primary-coordinator none`). The plane must instead
/// degrade to pass-through: rendezvous off, direct/front paths still live, node
/// stays up.
#[test]
fn unreachable_coordinator_degrades_the_plane_instead_of_killing_it() {
    let _serial = serial();
    let dir = tempfile::tempdir().expect("tempdir");
    let founder = dir.path().join("founder");

    // a socket-mode network whose only coordinator is an unroutable blackhole
    // (RFC5737 TEST-NET-3 — guaranteed never to answer). every surface gets
    // an explicit ephemeral-range port: init's working defaults (the
    // `DEFAULT_*_LISTEN` ports) would collide with a real node on this host.
    let ports = alloc_ports(4);
    let wg_port = ports[3];
    let init = common::ducktape()
        .arg("node")
        .args([
            "init",
            "--name",
            "coordinator-degrade",
            "--modules",
            common::founding_set(),
            "--dir",
            founder.to_str().expect("utf-8 founder dir"),
            "--primary-coordinator",
            "203.0.113.1:3478",
            "--listen",
            &format!("127.0.0.1:{}", ports[0]),
            "--advertised",
            &format!("127.0.0.1:{}", ports[0]),
            "--http",
            &format!("127.0.0.1:{}", ports[1]),
            "--rpc",
            &format!("127.0.0.1:{}", ports[2]),
            "--wireguard-listen",
            &format!("127.0.0.1:{wg_port}"),
        ])
        .output()
        .expect("run init");
    assert!(
        init.status.success(),
        "init failed:\n{}",
        command_output(&init)
    );

    // the node never exits on its own here (a healthy solo founder runs
    // forever), so wait on its piped stderr for whichever decisive marker
    // arrives first — the plane DEGRADES (pass) or refuses to start (fail) —
    // then tear the node down. An early exit surfaces as stderr EOF.
    let log_path = dir.path().join("founder-run.log");
    let out = std::fs::File::create(&log_path).expect("create node log");
    let mut child = common::ducktape()
        .arg("node")
        .arg("run")
        .arg("--config")
        .arg(founder.join("node.toml"))
        .stdout(Stdio::from(out))
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ducktape");
    let stderr = NodeStderr::pipe(&mut child);

    let outcome = stderr.wait_for_any(&["coordinator rendezvous unavailable", "plane not started"]);
    let still_running = child.try_wait().expect("poll node").is_none();
    let _ = child.kill();
    let _ = child.wait();
    let log = stderr.transcript();

    let degraded = matches!(outcome, Ok(0));
    assert!(
        degraded,
        "an unreachable coordinator must DEGRADE the plane (\"coordinator rendezvous \
         unavailable\"), never refuse to start it:\n{log}"
    );
    assert!(
        !log.contains("plane not started"),
        "the reachability plane must START (degraded), never hard-fail, on a dead \
         coordinator — else direct/front joins break:\n{log}"
    );
    assert!(
        still_running,
        "the node must keep running to serve direct/front paths, not exit, on a dead \
         coordinator:\n{log}"
    );
}

/// #2386, end to end through the REAL binaries: a second workspace on a host
/// whose WireGuard port is already taken used to die saying "ask the inviter
/// for a fresh invite". The operator does that — it is not free, and the new
/// invite fails identically — while the real cause was logged three lines
/// earlier at boot and never reached the message the process died with.
///
/// Here the TEST holds the port, so the holder the node names is this very
/// process: the join must refuse with the bind failure, name the port and the
/// flag that moves it, and never send anyone back to the inviter.
#[test]
fn a_taken_wireguard_port_refuses_the_join_instead_of_blaming_the_invite() {
    let _serial = serial();
    let dir = tempfile::tempdir().expect("tempdir");
    let founder = dir.path().join("founder");
    let friend = dir.path().join("friend");

    // explicit ephemeral ports throughout: `init`/`join` default to the FIXED
    // 8845/51820/51821, which is the very collision under test and would take
    // a real node on this host with it.
    let ports = alloc_ports(9);
    let init = common::ducktape()
        .arg("node")
        .args([
            "init",
            "--name",
            "port-collision",
            "--modules",
            common::founding_set(),
            "--dir",
            founder.to_str().expect("utf-8 founder dir"),
            "--listen",
            &format!("127.0.0.1:{}", ports[0]),
            "--advertised",
            &format!("127.0.0.1:{}", ports[0]),
            "--http",
            &format!("127.0.0.1:{}", ports[1]),
            "--rpc",
            &format!("127.0.0.1:{}", ports[2]),
            "--wireguard-listen",
            &format!("127.0.0.1:{}", ports[3]),
        ])
        .output()
        .expect("run init");
    assert!(
        init.status.success(),
        "init failed:\n{}",
        command_output(&init)
    );

    // the invite must carry at least one front or the joiner never races at
    // all and never reaches the terminal message under test. Seeded exactly as
    // `invite_bundles_reachable_member_fronts_from_seeded_mesh_state` does; the
    // front is unroutable on purpose, so the race exhausts.
    let namespace = genesis_namespace(&founder);
    let advert = direct_member_advert(&namespace, 7, 20);
    let mesh = PersistedMesh::new(namespace, 1, vec![advert], vec![], vec![]);
    let storage = founder.join("storage");
    std::fs::create_dir_all(&storage).expect("create founder storage");
    reachability::store::save(&storage.join("mesh-state.json"), &mesh)
        .expect("seed mesh-state.json");

    keygen(&friend);
    let invite = common::ducktape()
        .arg("node")
        .args(["invite", "--config"])
        .arg(founder.join("node.toml"))
        .output()
        .expect("run invite");
    assert!(
        invite.status.success(),
        "invite failed:\n{}",
        command_output(&invite)
    );
    let blob = String::from_utf8_lossy(&invite.stdout).trim().to_string();

    // THE collision: this process takes the joiner's WireGuard port and keeps
    // it for the whole test. `UnderlaySocket::bind` binds `0.0.0.0:<port>` with
    // no address reuse, so a loopback holder collides with it exactly as a
    // second node would.
    let joiner_wireguard = ports[7];
    let _squatter = std::net::UdpSocket::bind(("0.0.0.0", joiner_wireguard))
        .expect("hold the joiner's wireguard port");

    let join = common::ducktape()
        .arg("node")
        .args([
            "join",
            &blob,
            "--dir",
            friend.to_str().expect("utf-8 friend dir"),
            "--genesis",
            founder
                .join("genesis")
                .to_str()
                .expect("utf-8 genesis path"),
            "--listen",
            &format!("127.0.0.1:{}", ports[4]),
            "--advertised",
            &format!("127.0.0.1:{}", ports[4]),
            "--http",
            &format!("127.0.0.1:{}", ports[5]),
            "--rpc",
            &format!("127.0.0.1:{}", ports[6]),
            "--wireguard-listen",
            &format!("127.0.0.1:{joiner_wireguard}"),
            "--invite-listen",
            &format!("127.0.0.1:{}", ports[8]),
        ])
        .output()
        .expect("run join");
    assert!(
        join.status.success(),
        "join failed:\n{}",
        command_output(&join)
    );

    let log_path = dir.path().join("joiner-run.log");
    let out = std::fs::File::create(&log_path).expect("create node log");
    let mut child = common::ducktape()
        .arg("node")
        .arg("run")
        .arg("--config")
        .arg(friend.join("node.toml"))
        .stdout(Stdio::from(out))
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ducktape");
    let stderr = NodeStderr::pipe(&mut child);

    // the plane refuses at boot, long before the join window elapses.
    let bind_refusal = stderr.wait_for_any(&["underlay_bind_failed"]);
    assert!(
        bind_refusal.is_ok(),
        "the plane must refuse the taken port: {:?}\n{}",
        bind_refusal,
        stderr.transcript()
    );

    // and the join — one full `INVITE_JOIN_WINDOW_MS` later — must die saying
    // THAT, not sending the operator back to the inviter.
    let verdict = stderr.wait_for_any(&[
        "no overlay to reach the mesh with",
        "ask the inviter for a fresh invite",
    ]);
    // both markers are the last thing the node prints before exiting itself, so
    // it is reaped, never killed — a SIGKILL here would race the exit and leave
    // the status code below unreadable. The kill is for a wedged run only.
    if verdict.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().expect("reap the joiner");
    let log = stderr.transcript();

    assert_eq!(
        verdict,
        Ok(0),
        "the join must refuse with the bind failure, never the invite:\n{log}"
    );
    assert!(
        !log.contains("ask the inviter for a fresh invite"),
        "a fresh invite cannot help and is not free — the message must never ask \
         for one when the plane never started:\n{log}"
    );
    assert!(
        log.contains(&format!("pid {}", std::process::id())),
        "the refusal must name the process holding the port (this test):\n{log}"
    );
    assert!(
        log.contains("--wireguard-listen"),
        "the refusal must name the flag that moves this node off the port:\n{log}"
    );
    assert!(
        log.contains(&joiner_wireguard.to_string()),
        "the refusal must name the port itself:\n{log}"
    );
    assert_eq!(
        status.code(),
        Some(3),
        "a join that ran out of paths exits 3, and this one still does:\n{log}"
    );
}
