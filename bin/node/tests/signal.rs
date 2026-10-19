//! `ducktape run` stops on SIGTERM: it exits 0 with its state closed and its sockets released,
//! and the next start on the same ports opens that state with nothing to replay. `ducktape submit
//! --land` against a running node answers with the block that carried the frame.
use std::fs::File;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};

use abi::role::halo;
use fixture_wasm::{ACCESS, AWAITS, BLOBS, EGRESS, HALO, IDENTITY, PROGRAMS, VALSET};
use futures::StreamExt as _;
use noded::Client;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

const HALO_ADDRESS: &str = "node.invalid:9100";
const PASSWORD: &str = "a test wallet password";

struct Run {
    child: Child,
    http: String,
    log: PathBuf,
}

fn ducktape(workspace: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ducktape"));
    command
        .arg("--workspace")
        .arg(workspace)
        .env("NO_COLOR", "1");
    command
}

fn said(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn founding(root: &Path, key: &str) -> PathBuf {
    let programs: [(&str, &[u8]); 8] = [
        ("programs", PROGRAMS),
        ("valset", VALSET),
        ("identity", IDENTITY),
        ("blobs", BLOBS),
        ("awaits", AWAITS),
        ("halo", HALO),
        ("access", ACCESS),
        ("egress", EGRESS),
    ];
    for (id, code) in programs {
        std::fs::write(root.join(format!("{id}.wasm")), code).unwrap();
    }
    let lanes = vec![
        halo::Founded {
            name: blob::contract::LANE.to_owned(),
            cap: halo::Class::Critical,
            pinned: true,
        },
        halo::Founded {
            name: live_wire::LANE.to_owned(),
            cap: halo::Class::Client,
            pinned: false,
        },
    ];
    std::fs::write(
        root.join("halo.params"),
        abi::encode(&halo::Genesis { lanes }),
    )
    .unwrap();
    let holders: Vec<(Vec<u8>, u64)> = Vec::new();
    std::fs::write(root.join("holders.bin"), abi::encode(&holders)).unwrap();
    let text = format!(
        "network = \"signal\"\ntime = 1700000000\nepoch_length = 64\nblock_time_ms = 250\n\
         [[validators]]\nkey = \"{key}\"\nhalo = \"{HALO_ADDRESS}\"\n\
         [[programs]]\nid = \"programs\"\ncode = \"programs.wasm\"\nroles = [\"programs\"]\n\
         [[programs]]\nid = \"valset\"\ncode = \"valset.wasm\"\nroles = [\"validators\"]\n\
         [[programs]]\nid = \"identity\"\ncode = \"identity.wasm\"\nparams = \"holders.bin\"\n\
         roles = [\"identity\"]\n\
         [[programs]]\nid = \"blobs\"\ncode = \"blobs.wasm\"\nroles = [\"blobs\"]\n\
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

fn founded(root: &Path) -> PathBuf {
    let workspace = root.join("node");
    let key = said(ducktape(&workspace).arg("identity"));
    let founding = founding(root, &key);
    said(
        ducktape(&workspace)
            .arg("init")
            .arg(&founding)
            .args(["--halo", HALO_ADDRESS]),
    );
    workspace
}

fn run(workspace: &Path, http: &str, name: &str) -> Run {
    let log = workspace.with_file_name(format!("{name}.log"));
    let mut child = ducktape(workspace)
        .args(["run", "--listen", "127.0.0.1:0", "--halo", "127.0.0.1:0"])
        .args(["--http", http])
        .stdout(Stdio::piped())
        .stderr(File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let line = BufReader::new(stdout).lines().next().unwrap().unwrap();
    let http = line.strip_prefix("http ").unwrap().to_owned();
    Run { child, http, log }
}

fn terminated(mut run: Run) -> (ExitStatus, String) {
    let signalled = Command::new("kill")
        .args(["-TERM", &run.child.id().to_string()])
        .status()
        .unwrap();
    assert!(signalled.success());
    let status = run.child.wait().unwrap();
    (status, std::fs::read_to_string(&run.log).unwrap())
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn a_node_stops_on_sigterm_and_opens_again_with_nothing_to_replay() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let workspace = founded(root.path());
    let first = run(&workspace, "127.0.0.1:0", "first");
    let client = Client::new(format!("http://{}", first.http));
    let watching = runtime.block_on(async {
        let url = format!("ws://{}/v1/changes/{}", first.http, host::RECENT);
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let mut changes = 0;
        while changes < 2 {
            let message = socket.next().await.unwrap().unwrap();
            if let Message::Binary(_) = message {
                changes += 1;
            }
        }
        client.status().await.unwrap();
        socket
    });
    let http = first.http.clone();

    let (status, log) = terminated(first);
    assert_eq!(status.code(), Some(0), "{log}");
    assert!(
        log.lines()
            .any(|line| line.contains("event=\"node_stopped\" reason=\"signal\"")),
        "{log}"
    );
    let rest: Vec<_> = runtime.block_on(watching.collect());
    let closed = rest.iter().find_map(|message| match message {
        Ok(Message::Close(Some(frame))) => Some(frame.code),
        _ => None,
    });
    assert_eq!(closed, Some(CloseCode::Away), "{rest:?}");

    let second = run(&workspace, &http, "second");
    assert_eq!(second.http, http);
    let (status, log) = terminated(second);
    assert_eq!(status.code(), Some(0), "{log}");
    let opened = log
        .lines()
        .find(|line| line.contains("store opened"))
        .unwrap_or_else(|| panic!("{log}"));
    assert!(opened.contains("records_replayed=0"), "{opened}");
    let recovered: Vec<&str> = log
        .lines()
        .filter(|line| line.contains(" WARN commonware_storage"))
        .collect();
    assert!(recovered.is_empty(), "{recovered:?}");
}

#[test]
fn submit_land_answers_with_the_block_that_carried_the_frame() {
    let root = tempfile::tempdir().unwrap();
    let workspace = founded(root.path());
    let node = run(&workspace, "127.0.0.1:0", "node");
    let key = root.path().join("wallet.key");
    keystore::userkey::mint_user_key(&key, PASSWORD).unwrap();
    let payload = root.path().join("op");
    std::fs::write(&payload, [0xff]).unwrap();
    let submit = |flags: &[&str]| {
        let mut command = ducktape(&workspace);
        command
            .args([
                "--node",
                &format!("http://{}", node.http),
                "submit",
                "identity",
            ])
            .arg(&payload)
            .args(flags)
            .env("DUCKTAPE_USER_KEY", &key)
            .env("DUCKTAPE_PASSWORD", PASSWORD);
        command
    };

    let refused = submit(&["--land", "--preconfirm"]).output().unwrap();
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");

    let answered = said(&mut submit(&["--land"]));
    let lines: Vec<&str> = answered.lines().collect();
    let [pooled, landed, ..] = lines.as_slice() else {
        panic!("{answered}");
    };
    let id = pooled
        .strip_prefix("pooled ")
        .unwrap_or_else(|| panic!("{answered}"));
    assert_eq!(hex::decode(id).unwrap().len(), 32, "{answered}");
    let verdict = landed
        .strip_prefix("landed ")
        .unwrap_or_else(|| panic!("{answered}"));
    let (height, receipt) = verdict.split_once(' ').unwrap();
    assert!(height.parse::<u64>().unwrap() > 0, "{answered}");
    assert!(receipt.starts_with("rejected "), "{answered}");

    let (status, log) = terminated(node);
    assert_eq!(status.code(), Some(0), "{log}");
}
