use std::net::{IpAddr, TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use commonware_cryptography::{Signer as _, ed25519};
use commonware_runtime::{Runner as _, tokio};
use fixture_probe::Step;
use fixture_wasm::{ACCESS, AWAITS, BLOBS, EGRESS, HALO, IDENTITY, PROBE, PROGRAMS, VALSET};
use futures::channel::oneshot;
use noded::wire::route;
use noded::{HaloConfig, Listeners, LoadConfig, Logs, Workspace};

const NETWORK: &str = "cors";
const ORIGIN: &str = "https://any.example";

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

fn browsable(answer: &reqwest::Response) {
    let heads = answer.headers();
    assert_eq!(
        heads["access-control-allow-origin"],
        "*",
        "{}",
        answer.url()
    );
    assert!(
        !heads.contains_key("access-control-allow-credentials"),
        "{}",
        answer.url()
    );
}

#[test]
fn a_running_node_answers_a_browser_of_any_origin_on_every_answer() {
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
                ready.send(running.http).unwrap();
                running
                    .stopped(async move {
                        let _ = stopping.await;
                    })
                    .await
                    .unwrap();
            });
        }
    });
    let base = format!("http://{}", started.recv().unwrap());
    let runtime = ::tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let client = reqwest::Client::new();
        let preflight = client
            .request(reqwest::Method::OPTIONS, format!("{base}{}", route::QUERY))
            .header("origin", ORIGIN)
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "content-type")
            .send()
            .await
            .unwrap();
        assert_eq!(preflight.status(), reqwest::StatusCode::OK);
        browsable(&preflight);
        assert_eq!(preflight.headers()["access-control-allow-methods"], "*");
        assert_eq!(preflight.headers()["access-control-allow-headers"], "*");

        let status = client
            .get(format!("{base}{}", route::STATUS))
            .header("origin", ORIGIN)
            .send()
            .await
            .unwrap();
        assert_eq!(status.status(), reqwest::StatusCode::OK);
        browsable(&status);

        let refused = client
            .post(format!("{base}{}", route::QUERY))
            .header("origin", ORIGIN)
            .body(vec![0xff])
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), reqwest::StatusCode::BAD_REQUEST);
        browsable(&refused);

        let missing = client
            .get(format!("{base}/v1/no-such-route"))
            .header("origin", ORIGIN)
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
        browsable(&missing);
    });
    stop.send(()).unwrap();
    thread.join().unwrap();
}
