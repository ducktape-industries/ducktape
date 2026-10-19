//! A node stopped while a state-sync request waits on it and its store compacts logs no error,
//! and the next start opens that state with nothing to replay.
use std::net::{IpAddr, TcpListener, UdpSocket};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use abi::HostOp;
use commonware_codec::Encode as _;
use commonware_cryptography::{Signer as _, ed25519};
use commonware_runtime::{Runner as _, tokio};
use fixture_probe::Step;
use fixture_wasm::{ACCESS, AWAITS, BLOBS, EGRESS, HALO, IDENTITY, PROBE, PROGRAMS, VALSET};
use futures::StreamExt as _;
use futures::channel::oneshot;
use host::Layer;
use node::{Frame, Preconfirm};
use noded::{Client, Daemon, HaloConfig, Listeners, LoadConfig, Logs, Workspace};
use statesync::Exchange as _;

const NETWORK: &str = "stop";
const FRAMES: u64 = 200;
const SETS: u64 = 100;
const VALUE_BYTES: usize = 1024;

type Node = Arc<Daemon<tokio::Context>>;

fn runner(dir: &Path) -> tokio::Runner {
    tokio::Runner::new(tokio::Config::new().with_storage_directory(dir))
}

fn outbound_ip() -> IpAddr {
    let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
    socket.connect("192.0.2.1:9").unwrap();
    socket.local_addr().unwrap().ip()
}

fn listeners() -> Listeners {
    let tcp = || TcpListener::bind("127.0.0.1:0").unwrap();
    Listeners {
        p2p: tcp(),
        http: tcp(),
        tls: None,
        halo: UdpSocket::bind((outbound_ip(), 0)).unwrap(),
        live: None,
    }
}

fn founding(root: &Path, key: &ed25519::PrivateKey, halo: &str, p2p: u16) -> PathBuf {
    for (name, code) in [
        ("programs", PROGRAMS),
        ("valset", VALSET),
        ("probe", PROBE),
        ("identity", IDENTITY),
        ("blobs", BLOBS),
        ("awaits", AWAITS),
        ("halo", HALO),
        ("access", ACCESS),
        ("egress", EGRESS),
    ] {
        std::fs::write(root.join(format!("{name}.wasm")), code).unwrap();
    }
    let lanes = vec![abi::role::halo::Founded {
        name: blob::contract::LANE.to_owned(),
        cap: abi::role::halo::Class::Critical,
        pinned: true,
    }];
    std::fs::write(
        root.join("halo.params"),
        abi::encode(&abi::role::halo::Genesis { lanes }),
    )
    .unwrap();
    let holders: Vec<(Vec<u8>, u64)> = Vec::new();
    std::fs::write(root.join("holders.bin"), abi::encode(&holders)).unwrap();
    std::fs::write(root.join("params.bin"), abi::encode(&Vec::<Step>::new())).unwrap();
    let key = hex::encode(key.public_key().as_ref());
    let text = format!(
        "network = \"{NETWORK}\"\ntime = 1700000000\nepoch_length = 1000\n\
         block_time_ms = 250\nmember_cap = 4\n\
         [[validators]]\nkey = \"{key}\"\naddress = \"tcp://localhost:{p2p}\"\nhalo = \"{halo}\"\n\
         [[programs]]\nid = \"programs\"\ncode = \"programs.wasm\"\nroles = [\"programs\"]\n\
         [[programs]]\nid = \"valset\"\ncode = \"valset.wasm\"\nroles = [\"validators\"]\n\
         [[programs]]\nid = \"identity\"\ncode = \"identity.wasm\"\nparams = \"holders.bin\"\n\
         roles = [\"identity\"]\n\
         [[programs]]\nid = \"blobs\"\ncode = \"blobs.wasm\"\nroles = [\"blobs\"]\n\
         [[programs]]\nid = \"probe\"\ncode = \"probe.wasm\"\nparams = \"params.bin\"\n\
         [[programs]]\nid = \"awaits\"\ncode = \"awaits.wasm\"\nroles = [\"awaits\"]\n\
         [[programs]]\nid = \"halo\"\ncode = \"halo.wasm\"\nparams = \"halo.params\"\n\
         roles = [\"halo\"]\n\
         [[programs]]\nid = \"access\"\ncode = \"access.wasm\"\nroles = [\"access\"]\n\
         [[programs]]\nid = \"egress\"\ncode = \"egress.wasm\"\nroles = [\"egress\"]\n"
    );
    let path = root.join("founding.toml");
    std::fs::write(&path, text).unwrap();
    path
}

fn key(n: u64, i: u64) -> Vec<u8> {
    format!("w/{n}/{i}").into_bytes()
}

fn written(writer: &ed25519::PrivateKey, anchor: host::Tip, n: u64) -> Vec<u8> {
    let steps: Vec<Step> = (0..SETS)
        .map(|i| {
            Step::Op(HostOp::Set {
                key: key(n, i),
                value: vec![(n % 251) as u8; VALUE_BYTES],
            })
        })
        .collect();
    Frame::sign(
        writer,
        NETWORK.as_bytes(),
        anchor,
        None,
        "probe",
        abi::encode(&steps),
    )
    .encode()
}

struct Started {
    http: String,
    daemon: Node,
    stop: oneshot::Sender<()>,
    thread: JoinHandle<()>,
}

fn start(workspace: &Workspace, bound: Listeners) -> Started {
    let (ready, started) = mpsc::channel();
    let (stop, stopping) = oneshot::channel::<()>();
    let thread = std::thread::spawn({
        let workspace = workspace.clone();
        move || {
            runner(&workspace.runtime_dir()).start(|context| async move {
                let logs = Logs::install("info").unwrap();
                let cadence = workspace
                    .descriptor()
                    .unwrap()
                    .cadence(consensus::Timeouts::default())
                    .unwrap();
                let running = noded::run_on(
                    context,
                    &workspace,
                    logs,
                    bound,
                    HaloConfig::default(),
                    LoadConfig::default(),
                    cadence,
                )
                .await
                .unwrap();
                ready.send((running.http, running.daemon.clone())).unwrap();
                running
                    .stopped(async move {
                        let _ = stopping.await;
                    })
                    .await
                    .unwrap();
            });
        }
    });
    let (http, daemon) = started.recv().unwrap();
    Started {
        http: format!("http://{http}"),
        daemon,
        stop,
        thread,
    }
}

fn client_runtime() -> ::tokio::runtime::Runtime {
    ::tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn errors(lines: &[String]) -> Vec<&String> {
    lines
        .iter()
        .filter(|line| line.starts_with("ERROR"))
        .collect()
}

#[test]
fn a_node_stopped_while_it_serves_state_sync_and_compacts_logs_no_error_and_opens_clean() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::at(root.path().join("n0"));
    let identity = runner(&workspace.runtime_dir()).start({
        let workspace = workspace.clone();
        |mut context| async move { workspace.identity_or_create(&mut context).unwrap() }
    });
    let bound = listeners();
    let halo = bound.halo.local_addr().unwrap().to_string();
    let p2p = bound.p2p.local_addr().unwrap().port();
    let path = founding(root.path(), &identity, &halo, p2p);
    runner(&workspace.runtime_dir()).start(|context| {
        let workspace = workspace.clone();
        async move {
            noded::init(context, &workspace, &path, &halo)
                .await
                .unwrap();
        }
    });

    let Started {
        http,
        daemon,
        stop,
        thread,
    } = start(&workspace, bound);
    let runtime = client_runtime();
    let client = Client::new(http);
    let writer = ed25519::PrivateKey::from_seed(7);
    let stopped_at = runtime.block_on(async {
        let anchor = client.status().await.unwrap().anchor();
        let mut blocks = client.changes(host::RECENT).await.unwrap();
        for n in 0..FRAMES {
            client
                .submit(written(&writer, anchor, n), Preconfirm::Off)
                .await
                .unwrap();
        }
        let last = key(FRAMES - 1, SETS - 1);
        while client
            .get(Layer::Confirmed, "probe", &last)
            .await
            .unwrap()
            .is_none()
        {
            blocks.next().await.unwrap().unwrap();
        }
        let statesync::Response::Head(head) =
            client.exchange(statesync::Request::Head).await.unwrap()
        else {
            panic!("a head request answers a head");
        };
        let range = &head.targets["probe"].range;
        let asked = statesync::SyncRequest::Operations {
            size: range.end(),
            start: range.start(),
            max_ops: NonZeroU64::new(64).unwrap(),
        };
        let request = statesync::Request::Sync {
            program: "probe".to_owned(),
            request: asked.encode().to_vec(),
        };

        let held = daemon.hold().await.unwrap();
        let syncing = daemon.sync(request);
        futures::pin_mut!(syncing);
        assert!(futures::poll!(&mut syncing).is_pending());
        stop.send(()).unwrap();
        held.release();
        let answered = syncing.await.unwrap();
        assert!(
            matches!(answered, statesync::Response::Sync(_)),
            "a state-sync request queued before the stop is answered whole: {answered:?}"
        );
        head.tip.height
    });
    drop(daemon);
    thread.join().unwrap();

    let logs = Logs::install("info").unwrap();
    let lines = logs.lines();
    let at = |what: &str| {
        lines
            .iter()
            .rposition(|line| line.contains(what))
            .unwrap_or_else(|| panic!("no {what}: {lines:#?}"))
    };
    let trigger = fluent31::Options::default().l0_compaction_trigger;
    let compacting = format!(" l0_runs={trigger} ");
    let flushed = lines
        .iter()
        .rposition(|line| {
            line.contains("fluent31::db flushed memtable") && line.contains(&compacting)
        })
        .unwrap_or_else(|| {
            panic!("the stop's flush fills level 0, so a compaction runs as the state closes: {lines:#?}")
        });
    let closed = lines[flushed..]
        .iter()
        .position(|line| line.contains("fluent31::db store closed"))
        .map(|after| flushed + after)
        .unwrap_or_else(|| panic!("the state never closed: {lines:#?}"));
    let stopped = at("event=\"node_stopped\"");
    assert!(
        closed < stopped,
        "the state closes before the node says it stopped"
    );
    assert!(
        lines[closed].contains("degraded=false"),
        "{}",
        lines[closed]
    );
    assert!(errors(&lines).is_empty(), "{:#?}", errors(&lines));

    let second = start(&workspace, listeners());
    let reopened = runtime
        .block_on(Client::new(second.http.clone()).status())
        .unwrap();
    assert!(reopened.height >= stopped_at);
    second.stop.send(()).unwrap();
    drop(second.daemon);
    second.thread.join().unwrap();

    let lines = logs.lines();
    let restarted = lines
        .iter()
        .position(|line| line.contains("event=\"node_stopped\""))
        .unwrap_or_else(|| panic!("{lines:#?}"));
    let opened: Vec<&String> = lines[restarted..]
        .iter()
        .filter(|line| line.contains("fluent31::db store opened"))
        .collect();
    assert!(!opened.is_empty(), "{lines:#?}");
    assert!(
        opened
            .iter()
            .all(|line| line.contains("records_replayed=0")),
        "{opened:#?}"
    );
    let recovered: Vec<&String> = lines
        .iter()
        .filter(|line| line.starts_with("WARN commonware_storage"))
        .collect();
    assert!(recovered.is_empty(), "{recovered:#?}");
    assert!(errors(&lines).is_empty(), "{:#?}", errors(&lines));
}
