//! A node indexes every frame its blocks carry, serves the index at `/v1/activity`, and after a
//! stop opens the index where it left it.
use std::net::{IpAddr, TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use abi::HostOp;
use abi::Refusal;
use commonware_cryptography::{Signer as _, ed25519};
use commonware_runtime::{Runner as _, tokio};
use fixture_probe::Step;
use fixture_wasm::{ACCESS, AWAITS, BLOBS, EGRESS, HALO, IDENTITY, PROBE, PROGRAMS, VALSET};
use futures::StreamExt as _;
use futures::channel::oneshot;
use host::Layer;
use node::{Frame, Preconfirm};
use noded::wire::activity::{Indexed, PageRequest, Query, Reply};
use noded::{Client, Daemon, HaloConfig, Listeners, LoadConfig, Logs, Workspace};

const NETWORK: &str = "activity";
const FRAMES: u64 = 3;

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

fn key(n: u64) -> Vec<u8> {
    format!("w/{n}").into_bytes()
}

fn written(writer: &ed25519::PrivateKey, anchor: host::Tip, n: u64) -> Vec<u8> {
    let steps = vec![Step::Op(HostOp::Set {
        key: key(n),
        value: vec![1],
    })];
    signed(writer, anchor, steps)
}

fn failing(writer: &ed25519::PrivateKey, anchor: host::Tip) -> Vec<u8> {
    signed(writer, anchor, vec![Step::Fail("no".into())])
}

fn signed(writer: &ed25519::PrivateKey, anchor: host::Tip, steps: Vec<Step>) -> Vec<u8> {
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

impl Started {
    fn stopped(self) {
        self.stop.send(()).unwrap();
        drop(self.daemon);
        self.thread.join().unwrap();
    }
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

async fn indexed_to(daemon: &Node, height: u64) -> Indexed {
    let mut indexed = daemon.indexed();
    let reached = indexed
        .wait_for(|span| span.is_some_and(|span| span.height >= height))
        .await
        .unwrap();
    reached.unwrap()
}

async fn probed(client: &Client) -> Vec<noded::wire::activity::Frame> {
    let all = Query::Frames {
        page: PageRequest::default(),
    };
    let Reply::Frames(page) = client.activity(&all).await.unwrap() else {
        panic!("Frames answers frames");
    };
    page.items
        .into_iter()
        .filter(|frame| frame.target == "probe")
        .collect()
}

#[test]
fn a_node_indexes_the_frames_its_blocks_carry_and_reopens_its_index_where_it_stopped() {
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

    let runtime = client_runtime();
    let first = start(&workspace, bound);
    let client = Client::new(first.http.clone());
    let writer = ed25519::PrivateKey::from_seed(7);
    let (frames, span) = runtime.block_on(async {
        let anchor = client.status().await.unwrap().anchor();
        let mut blocks = client.changes(host::RECENT).await.unwrap();
        client
            .submit(failing(&writer, anchor), Preconfirm::Off)
            .await
            .unwrap();
        for n in 0..FRAMES {
            client
                .submit(written(&writer, anchor, n), Preconfirm::Off)
                .await
                .unwrap();
        }
        let last = key(FRAMES - 1);
        while client
            .get(Layer::Confirmed, "probe", &last)
            .await
            .unwrap()
            .is_none()
        {
            blocks.next().await.unwrap().unwrap();
        }
        let landed = client.status().await.unwrap().height;
        let span = indexed_to(&first.daemon, landed).await;
        (probed(&client).await, span)
    });
    assert_eq!(
        span.from, 1,
        "a node founded here indexes from its first block"
    );
    assert_eq!(frames.len() as u64, FRAMES + 1);
    let refused: Vec<_> = frames.iter().filter(|frame| !frame.applied).collect();
    assert_eq!(refused.len(), 1, "{frames:?}");
    let why = |frame: &noded::wire::activity::Frame| Query::Refusal {
        height: frame.height,
        position: frame.position,
    };
    assert_eq!(
        runtime.block_on(client.activity(&why(refused[0]))).unwrap(),
        Reply::Refusal(Some(Refusal::new("probe", "no")))
    );
    let landed = frames.iter().find(|frame| frame.applied).unwrap();
    assert_eq!(
        runtime.block_on(client.activity(&why(landed))).unwrap(),
        Reply::Refusal(None)
    );
    let newest_first = frames
        .windows(2)
        .all(|pair| (pair[0].height, pair[0].position) > (pair[1].height, pair[1].position));
    assert!(newest_first, "{frames:?}");
    let signed = Query::Signed {
        key: frames[0].signer.clone(),
        page: PageRequest::default(),
    };
    let Reply::Frames(by_signer) = runtime.block_on(client.activity(&signed)).unwrap() else {
        panic!("Signed answers frames");
    };
    assert_eq!(by_signer.items, frames);
    first.stopped();

    let second = start(&workspace, listeners());
    let client = Client::new(second.http.clone());
    let (reopened, again) = runtime.block_on(async {
        let span = indexed_to(&second.daemon, span.height).await;
        (span, probed(&client).await)
    });
    assert_eq!(reopened.from, 1, "a reopened index keeps its history");
    assert_eq!(again, frames);
    second.stopped();

    let logs = Logs::install("info").unwrap();
    let gaps: Vec<String> = logs
        .lines()
        .into_iter()
        .filter(|line| line.contains("activity_gap") || line.contains("activity_unwritten"))
        .collect();
    assert!(gaps.is_empty(), "{gaps:#?}");
}
