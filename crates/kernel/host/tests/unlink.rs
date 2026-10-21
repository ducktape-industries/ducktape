//! A release lets go of line rows and index keys as well as blobs, and the unlink after commit
//! logs none of them as a blob file that stayed.
use std::fmt::Debug;
use std::sync::{Arc, Mutex};

use abi::role::{self, halo, validators};
use abi::{BlobId, HashKind, HostOp};
use commonware_runtime::{Runner as _, deterministic};
use fixture_probe::Step;
use fixture_wasm::{
    ACCESS, AWAITS, BLOBS as BLOBS_PROGRAM, HALO, IDENTITY, PROBE, PROGRAMS, VALSET,
};
use host::{BLOBS, Block, BlockId, Founding, Genesis, Host, Layer, Limits, Submission, Tip};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{self as tracing_layer, SubscriberExt as _};

const SIGNER: &[u8] = b"signer";
const TIME: u64 = 1_700_000_000;

type Ctx = deterministic::Context;

fn founding(program: &str, code: &[u8], params: Vec<u8>) -> Founding {
    let roles: &[&str] = match program {
        "programs" => &[role::PROGRAMS],
        "valset" => &[role::VALIDATORS],
        "identity" => &[role::IDENTITY],
        "blobs" => &[role::BLOBS],
        "awaits" => &[role::AWAITS],
        "halo" => &[role::HALO],
        "access" => &[role::ACCESS],
        _ => &[],
    };
    Founding {
        program: program.to_owned(),
        code: code.to_vec(),
        params,
        roles: roles.iter().map(|role| (*role).to_owned()).collect(),
    }
}

fn genesis() -> Genesis {
    Genesis {
        network: b"net".to_vec(),
        validators: vec![validators::Member {
            key: b"v1".to_vec(),
            address: Some("v1:1".into()),
            halo: "v1:1".into(),
        }],
        programs: vec![
            founding("programs", PROGRAMS, Vec::new()),
            founding("valset", VALSET, Vec::new()),
            founding(
                "identity",
                IDENTITY,
                abi::encode(&vec![(SIGNER.to_vec(), 1u64)]),
            ),
            founding("blobs", BLOBS_PROGRAM, Vec::new()),
            founding("awaits", AWAITS, Vec::new()),
            founding("probe", PROBE, abi::encode(&Vec::<Step>::new())),
            founding(
                "halo",
                HALO,
                abi::encode(&halo::Genesis { lanes: Vec::new() }),
            ),
            founding("access", ACCESS, Vec::new()),
        ],
        views: Vec::new(),
        hooks: Vec::new(),
        limits: Limits::default(),
        epoch_length: 1_000,
        time: TIME,
        member_cap: 16,
        queries: abi::role::validators::QueryPolicy::Signed,
    }
}

fn block_id(height: u64) -> BlockId {
    let mut id = [0u8; 32];
    id[..8].copy_from_slice(&height.to_be_bytes());
    id
}

fn block(height: u64, steps: Vec<Step>) -> Block {
    let run = Submission {
        signer: SIGNER.to_vec(),
        anchor: Tip {
            height: 0,
            id: block_id(0),
        },
        salt: Some(height),
        target: "probe".to_owned(),
        payload: abi::encode(&steps),
        client: None,
    };
    Block {
        height,
        id: block_id(height),
        time: TIME + height,
        submissions: vec![run],
    }
}

fn blob_id(kind: &str, body: &[u8]) -> BlobId {
    blob::id_of(HashKind::Sha256, &blob::frame(kind, body).unwrap()).unwrap()
}

fn keep(body: &[u8], line: &[u8]) -> Step {
    Step::Op(HostOp::put(
        HashKind::Sha256,
        "file".to_owned(),
        body.to_vec(),
        Some(line.to_vec()),
    ))
}

fn gone(host: &Host<Ctx>, body: &[u8]) -> bool {
    let id = blob_id("file", body);
    let row = host
        .view(Layer::Confirmed)
        .get(BLOBS, &abi::encode(&id))
        .unwrap();
    row.is_none() && !host.blobs().has(&id)
}

#[derive(Clone, Default)]
struct Unlinks(Arc<Mutex<Vec<String>>>);

#[derive(Default)]
struct Unlink {
    failed: bool,
    reason: String,
}

impl Visit for Unlink {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "event" => self.failed = value == "blob_unlink_failed",
            "reason" => self.reason = value.to_owned(),
            _ => {}
        }
    }

    fn record_debug(&mut self, _: &Field, _: &dyn Debug) {}
}

impl<S: Subscriber> tracing_layer::Layer<S> for Unlinks {
    fn on_event(&self, event: &Event<'_>, _: tracing_layer::Context<'_, S>) {
        let mut unlink = Unlink::default();
        event.record(&mut unlink);
        if unlink.failed {
            self.0.lock().unwrap().push(unlink.reason);
        }
    }
}

#[test]
fn a_release_on_a_line_logs_no_blob_unlink_failed() {
    let unlinks = Unlinks::default();
    let seen = unlinks.clone();
    let subscriber = tracing_subscriber::registry().with(unlinks.clone());
    tracing::subscriber::with_default(subscriber, || {
        deterministic::Runner::default().start(|context| async move {
            let dir = tempfile::tempdir().unwrap();
            let mut host = Host::found(context, "net", dir.path(), block_id(0), genesis())
                .await
                .unwrap()
                .0;

            let lines = vec![keep(b"a", b"l/1"), keep(b"b", b"l/2"), keep(b"c", b"m")];
            host.apply(block(1, lines)).await.unwrap();
            host.apply(block(2, vec![keep(b"a2", b"l/1")]))
                .await
                .unwrap();
            let under = HostOp::BlobReleaseLines {
                prefix: b"l/".to_vec(),
            };
            host.apply(block(3, vec![Step::Op(under)])).await.unwrap();
            let release = HostOp::BlobRelease(blob_id("file", b"c"));
            host.apply(block(4, vec![Step::Op(release)])).await.unwrap();
            let bodies: [&[u8]; 4] = [b"a", b"a2", b"b", b"c"];
            assert_eq!(bodies.map(|body| gone(&host, body)), [true; 4]);
            assert_eq!(*seen.0.lock().unwrap(), Vec::<String>::new());

            let id = blob_id("part", b"stuck on disk");
            let put = HostOp::BlobPutAlone {
                hash: HashKind::Sha256,
                kind: "part".to_owned(),
                body: b"stuck on disk".to_vec(),
            };
            host.apply(block(5, vec![Step::Op(put)])).await.unwrap();
            let file = dir.path().join("blobs").join(blob::name_of(&id));
            std::fs::create_dir(&file).unwrap();
            std::fs::write(file.join("inside"), b"no remove_file takes a directory").unwrap();
            let release = HostOp::BlobRelease(id);
            host.apply(block(6, vec![Step::Op(release)])).await.unwrap();
        });
    });
    assert_eq!(
        *unlinks.0.lock().unwrap(),
        vec!["remove_failed".to_owned()],
        "only the file that would not go is logged"
    );
}
