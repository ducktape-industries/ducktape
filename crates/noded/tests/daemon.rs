use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use abi::role::validators::{Motion, Op};
use abi::{Blob, BlobId, HashKind, HostOp, HostReply, Outcome, Scan, reason};
use commonware_cryptography::{Digestible as _, Signer as _, ed25519};
use commonware_runtime::{Runner as _, tokio};
use fixture_probe::{Reply, Step};
use futures::{SinkExt as _, StreamExt as _};
use host::attach::{ATTACH, Attached, Attachment};
use host::{Layer, Submission, Tip};
use node::Frame;
use noded::wire::{Admin, BlockRef, PREVIEW_BYTES, QUERY_BYTES, TxAsk, TxRef};
use noded::{Client, Endpoint, HaloConfig, Listen, Logs, Workspace};

const PROGRAMS: &[u8] =
    include_bytes!("../../kernel/fixtures/wasm/fixture_programs.wasm");
const VALSET: &[u8] = include_bytes!("../../kernel/fixtures/wasm/fixture_valset.wasm");
const RELAY: &[u8] = include_bytes!("../../kernel/fixtures/wasm/fixture_relay.wasm");
const IDENTITY: &[u8] = include_bytes!("../../kernel/fixtures/wasm/fixture_identity.wasm");
const PROBE: &[u8] = include_bytes!("../../kernel/fixtures/wasm/fixture_probe.wasm");

const NETWORK: &str = "daemon";
const TIME: u64 = 1_700_000_000;
const EPOCH_LENGTH: u64 = 8;
const BLOCK_TIME_MS: u64 = 250;
/// Room for the four seats the largest test founds.
const MEMBER_CAP: u32 = 4;

fn runner(dir: &Path) -> tokio::Runner {
    tokio::Runner::new(tokio::Config::new().with_storage_directory(dir))
}

fn free_port() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

fn unheard() -> String {
    format!("tcp://localhost:{}", free_port().port())
}

struct Seat {
    workspace: Workspace,
    identity: ed25519::PrivateKey,
    p2p: SocketAddr,
    http: SocketAddr,
    tls: SocketAddr,
    address: Option<String>,
}

impl Seat {
    fn new(root: &Path, name: &str) -> Seat {
        let workspace = Workspace::at(root.join(name));
        let identity = runner(&workspace.runtime_dir()).start({
            let workspace = workspace.clone();
            |mut context| async move { workspace.identity_or_create(&mut context).unwrap() }
        });
        let p2p = free_port();
        Seat {
            workspace,
            identity,
            p2p,
            http: free_port(),
            tls: free_port(),
            address: Some(format!("tcp://localhost:{}", p2p.port())),
        }
    }

    fn validator(&self) -> String {
        let key = hex::encode(self.identity.public_key().as_ref());
        let Some(address) = &self.address else {
            return format!("[[validators]]\nkey = \"{key}\"\n");
        };
        format!("[[validators]]\nkey = \"{key}\"\naddress = \"{address}\"\n")
    }

    fn init(&self, founding: &Path) {
        runner(&self.workspace.runtime_dir()).start(|context| async move {
            noded::init(context, &self.workspace, founding)
                .await
                .unwrap();
        });
    }

    fn join(&self, source: &Client) {
        runner(&self.workspace.runtime_dir()).start(|context| async move {
            let address = self
                .address
                .as_deref()
                .map(|text| text.parse::<Endpoint>().unwrap());
            noded::join(context, &self.workspace, source.clone(), address, None)
                .await
                .unwrap();
        });
    }

    fn key(&self) -> Vec<u8> {
        self.identity.public_key().as_ref().to_vec()
    }

    fn start(&self) -> Live {
        let (ready, started) = mpsc::channel();
        let workspace = self.workspace.clone();
        let listen = Listen {
            p2p: self.p2p,
            http: self.http,
            tls: Some(self.tls),
        };
        let thread = std::thread::spawn(move || {
            runner(&workspace.runtime_dir()).start(|context| async move {
                let logs = Logs::install("info").unwrap();
                let halo = HaloConfig {
                    egress_bytes_per_sec: 8 << 20,
                };
                let running = noded::run(context, &workspace, logs, listen, halo)
                    .await
                    .unwrap();
                ready.send(running.http).unwrap();
                running.stopped().await;
            });
        });
        let http = started.recv().unwrap();
        Live {
            client: Client::new(format!("http://{http}")),
            thread,
        }
    }

    fn admin_frame(&self, anchor: Tip, verb: Admin) -> Vec<u8> {
        Frame::sign(
            &self.identity,
            NETWORK.as_bytes(),
            anchor,
            None,
            noded::wire::ADMIN,
            abi::encode(&verb),
        )
        .encode()
    }

    async fn admin(&self, client: &Client, verb: Admin) {
        let anchor = client.status().await.unwrap().anchor();
        client.admin(self.admin_frame(anchor, verb)).await.unwrap();
    }
}

struct Live {
    client: Client,
    thread: JoinHandle<()>,
}

fn founding(root: &Path, seats: &[&Seat], epoch_length: u64) -> PathBuf {
    std::fs::write(root.join("programs.wasm"), PROGRAMS).unwrap();
    std::fs::write(root.join("valset.wasm"), VALSET).unwrap();
    std::fs::write(root.join("relay.wasm"), RELAY).unwrap();
    std::fs::write(root.join("probe.wasm"), PROBE).unwrap();
    std::fs::write(root.join("identity.wasm"), IDENTITY).unwrap();
    std::fs::write(
        root.join("holders.bin"),
        abi::encode(&Vec::<(Vec<u8>, u64)>::new()),
    )
    .unwrap();
    std::fs::write(root.join("params.bin"), abi::encode(&Vec::<Step>::new())).unwrap();
    let validators: String = seats.iter().map(|seat| seat.validator()).collect();
    let text = format!(
        "network = \"{NETWORK}\"\ntime = {TIME}\nepoch_length = {epoch_length}\n\
         block_time_ms = {BLOCK_TIME_MS}\nmember_cap = {MEMBER_CAP}\n\
         [roles]\nprograms = \"programs\"\nvalidators = \"valset\"\nidentity = \"identity\"\n\
         {validators}\
         [[programs]]\nid = \"programs\"\ncode = \"programs.wasm\"\n\
         [[programs]]\nid = \"valset\"\ncode = \"valset.wasm\"\n\
         [[programs]]\nid = \"identity\"\ncode = \"identity.wasm\"\nparams = \"holders.bin\"\n\
         [[programs]]\nid = \"ping\"\ncode = \"relay.wasm\"\n\
         [[programs]]\nid = \"probe\"\ncode = \"probe.wasm\"\nparams = \"params.bin\"\n"
    );
    let path = root.join("genesis.toml");
    std::fs::write(&path, text).unwrap();
    path
}

fn set(key: &[u8], value: &[u8]) -> Step {
    Step::Op(HostOp::Set {
        key: key.to_vec(),
        value: value.to_vec(),
    })
}

async fn signed(
    client: &Client,
    key: &ed25519::PrivateKey,
    target: &str,
    payload: Vec<u8>,
) -> Vec<u8> {
    let anchor = client.status().await.unwrap().anchor();
    Frame::sign(key, NETWORK.as_bytes(), anchor, None, target, payload).encode()
}

async fn frame(client: &Client, key: &ed25519::PrivateKey, steps: Vec<Step>) -> Vec<u8> {
    signed(client, key, "probe", abi::encode(&steps)).await
}

async fn membership(client: &Client, key: &ed25519::PrivateKey, op: Op) -> u64 {
    let anchor = client.status().await.unwrap().anchor();
    let frame = Frame::sign(
        key,
        NETWORK.as_bytes(),
        anchor,
        None,
        "valset",
        abi::encode(&op),
    );
    let (height, receipt) = client.land(&frame).await.unwrap();
    assert!(
        matches!(receipt.outcome, Outcome::Applied { .. }),
        "{receipt:?}"
    );
    height
}

async fn page_reader(
    client: &Client,
    key: &ed25519::PrivateKey,
    body: &[u8],
) -> (BlobId, Vec<u8>, Vec<u8>) {
    let framed = blobs::frame("page", body).unwrap();
    let id = blobs::id_of(HashKind::Sha256, &framed);
    let attached = Attached {
        target: "probe".into(),
        payload: abi::encode(&vec![Step::Op(HostOp::BlobGet(id))]),
        blobs: vec![Attachment {
            id,
            len: framed.len() as u64,
        }],
    };
    let frame = signed(client, key, ATTACH, abi::encode(&attached)).await;
    (id, framed, frame)
}

fn submission(frame: &[u8]) -> Submission {
    Frame::decode(frame).unwrap().body.submission()
}

/// Polls until `client`'s node has applied `height`.
async fn reach(client: &Client, height: u64) {
    while client.status().await.unwrap().height < height {
        ::tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn client_runtime() -> ::tokio::runtime::Runtime {
    ::tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn a_validator_serves_its_network_over_http() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);

    let kept = client_runtime().block_on(async {
        let status = client.status().await.unwrap();
        assert_eq!(status.network, NETWORK);
        assert_eq!(status.epoch_length, EPOCH_LENGTH);
        assert_eq!(
            status.identity,
            seat.identity.public_key().as_ref().to_vec()
        );
        assert_eq!(status.contract, noded::NODE_CONTRACT);
        assert_eq!(
            status.genesis,
            node::Block::genesis(NETWORK.as_bytes(), TIME).digest().0,
            "the genesis digest a client salts the network name with"
        );
        assert_eq!(status.roles.validators, "valset");

        let landing = frame(&client, &alice, vec![Step::Fail("landed".into())]).await;
        let (height, receipt) = client
            .land(&Frame::decode(&landing).unwrap())
            .await
            .unwrap();
        let carried = client
            .block(BlockRef::Height(height))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(carried.txs[0].hash, submission(&landing).id());
        assert!(
            matches!(&receipt.outcome, Outcome::Rejected(refusal) if refusal.sentence == "landed"),
            "{receipt:?}"
        );

        let mut changes = client.changes("probe").await.unwrap();
        let failed = frame(&client, &alice, vec![Step::Fail("no".into())]).await;
        let rejected = client.submit(failed.clone()).await.unwrap();
        let Outcome::Rejected(refusal) = &rejected.outcome else {
            panic!("the failing run is rejected: {rejected:?}");
        };
        let submitted = frame(&client, &alice, vec![set(b"a", b"1")]).await;
        let receipt = client.submit(submitted.clone()).await.unwrap();
        assert!(
            matches!(receipt.outcome, Outcome::Applied { .. }),
            "{receipt:?}"
        );
        assert_eq!(
            client
                .get(Layer::Preconfirmed, "probe", b"a")
                .await
                .unwrap(),
            Some(b"1".to_vec())
        );
        let change = changes.next().await.unwrap().unwrap();
        assert_eq!(change.writes, vec![(b"a".to_vec(), Some(b"1".to_vec()))]);
        assert_eq!(
            client.get(Layer::Confirmed, "probe", b"a").await.unwrap(),
            Some(b"1".to_vec())
        );
        let entries = client
            .scan(Layer::Confirmed, "probe", Scan::prefix(b""))
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        let status = client.status().await.unwrap();
        assert!(status.height >= change.height);
        assert_eq!(status.root, change.root);

        // the lone validator's row: its engine booked its own finalize vote
        // for the block that wrote the change before that block could be
        // finalized, so the book held it once the change was published,
        // and a read takes the tip and the book under one lock
        let network = client.network().await.unwrap();
        assert!(network.height >= change.height);
        let [row] = network.members.as_slice() else {
            panic!("one member: {network:?}");
        };
        assert_eq!(row.key, status.identity);
        assert!(
            row.signed
                .is_some_and(|signed| (change.height..=network.height).contains(&signed)),
            "{row:?} against change {} and tip {}",
            change.height,
            network.height
        );

        // the block that wrote it, read back from the archive
        let block = client
            .block(BlockRef::Height(change.height))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(block.height, change.height);
        assert_eq!(block.epoch, change.height / EPOCH_LENGTH);
        let tx = block
            .txs
            .last()
            .expect("the writing frame is the block's last");
        assert_eq!(tx.hash, submission(&submitted).id());
        assert_eq!(tx.signer, alice.public_key().as_ref().to_vec());
        assert_eq!(tx.anchor, submission(&submitted).anchor);
        assert_eq!(tx.target, "probe");
        let payload = abi::encode(&vec![set(b"a", b"1")]);
        assert_eq!(tx.size as usize, payload.len());
        assert_eq!(
            tx.payload.as_ref(),
            Some(&payload),
            "a short payload rides along"
        );
        assert_eq!(tx.receipt.as_ref(), Some(&receipt), "the kept receipt");
        let position = (block.txs.len() - 1) as u32;
        let at = TxRef::At {
            height: block.height,
            position,
        };
        let located = client
            .tx(TxAsk {
                tx: at,
                most: u32::MAX,
            })
            .await
            .unwrap()
            .expect("the transaction by its place");
        assert_eq!(
            (located.height, located.position, located.time),
            (block.height, position, block.time)
        );
        assert_eq!(&located.tx, tx);
        let by_hash = client
            .tx(TxAsk {
                tx: TxRef::Hash(tx.hash),
                most: 0,
            })
            .await
            .unwrap()
            .expect("the transaction by its hash");
        assert_eq!((by_hash.height, by_hash.position), (block.height, position));
        assert_eq!(by_hash.tx.payload, None, "over the asked cap");
        let past = TxRef::At {
            height: block.height,
            position: position + 1,
        };
        let none = client.tx(TxAsk { tx: past, most: 0 }).await.unwrap();
        assert_eq!(none, None);

        let large = vec![set(b"large", &[7; 2 * PREVIEW_BYTES as usize])];
        let large = frame(&client, &alice, large).await;
        let (landed, _) = client.land(&Frame::decode(&large).unwrap()).await.unwrap();
        let carrier = client
            .block(BlockRef::Height(landed))
            .await
            .unwrap()
            .unwrap();
        let large_id = submission(&large).id();
        let position = carrier
            .txs
            .iter()
            .position(|tx| tx.hash == large_id)
            .expect("the block carries the large frame") as u32;
        let listed = &carrier.txs[position as usize];
        let large_payload = submission(&large).payload;
        assert_eq!(listed.size as usize, large_payload.len());
        assert_eq!(listed.payload, None, "a block read carries no long payload");
        let opened = client
            .tx(TxAsk {
                tx: TxRef::At {
                    height: landed,
                    position,
                },
                most: u32::MAX,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(opened.tx.payload, Some(large_payload.clone()));
        assert_eq!(opened.tx.receipt, listed.receipt);

        let fails = |tx: &&noded::wire::Tx| tx.hash == submission(&failed).id();
        let holding = client.blocks(Some(block.height + 1), 3).await.unwrap();
        assert_eq!(
            holding.listed_from,
            Some(0),
            "a founding node lists from genesis"
        );
        let (holder, rejected_tx) = holding
            .blocks
            .iter()
            .find_map(|seen| Some((seen, seen.txs.iter().find(fails)?)))
            .expect("the rejected frame rides a block");
        let kept = rejected_tx.receipt.as_ref().expect("its receipt is kept");
        assert_eq!(kept.outcome, Outcome::Rejected(refusal.clone()));
        // the receipts ride beside the block: its id is the digest of its
        // header and frames alone
        let frames: Vec<Vec<u8>> = if holder.height == block.height {
            vec![failed.clone(), submitted.clone()]
        } else {
            vec![submitted.clone()]
        };
        let parent = host::Tip {
            height: block.height - 1,
            id: block.parent,
        };
        assert_eq!(
            node::Block::next(parent, block.time, frames).digest().0,
            block.id
        );
        assert_eq!(
            block.proposer.as_deref(),
            Some(seat.identity.public_key().as_ref())
        );
        let by_id = client.block(BlockRef::Id(block.id)).await.unwrap();
        assert_eq!(by_id.as_ref(), Some(&block));
        assert_eq!(client.block(BlockRef::Id([7; 32])).await.unwrap(), None);
        let newest = client.blocks(None, 1_000).await.unwrap().blocks;
        assert!(newest.len() as u32 <= noded::wire::MAX_BLOCKS);
        assert!(
            newest
                .windows(2)
                .all(|pair| pair[1].height + 1 == pair[0].height && pair[1].id == pair[0].parent)
        );
        assert!(newest.iter().any(|seen| seen == &block));
        let below = client.blocks(Some(block.height), 2).await.unwrap().blocks;
        assert_eq!(below[0].height, block.height - 1);
        assert_eq!(below[0].id, block.parent);
        assert!(client.blocks(Some(0), 5).await.unwrap().blocks.is_empty());
        let genesis = client.blocks(Some(1), 5).await.unwrap().blocks;
        assert_eq!(genesis.len(), 1);
        assert_eq!(
            genesis[0].id,
            node::Block::genesis(NETWORK.as_bytes(), TIME).digest().0
        );

        let query = frame(&client, &alice, vec![Step::Op(HostOp::Get(b"a".to_vec()))]).await;
        let answer = client.query(Layer::Confirmed, query).await.unwrap();
        assert_eq!(
            answer,
            abi::encode(&vec![Reply::Host(HostReply::Value(Some(b"1".to_vec())))])
        );

        let envs = |count: usize| (0..count).map(|_| Step::Env).collect::<Vec<_>>();
        let one = frame(&client, &alice, envs(1)).await;
        let env_bytes = client.query(Layer::Confirmed, one).await.unwrap().len() - 4;
        let fits = (QUERY_BYTES - 4) / env_bytes;
        let at_the_cap = frame(&client, &alice, envs(fits)).await;
        let answer = client.query(Layer::Confirmed, at_the_cap).await.unwrap();
        assert_eq!(answer.len(), 4 + fits * env_bytes);
        let past_the_cap = frame(&client, &alice, envs(fits + 1)).await;
        let Err(noded::Error::Refused(refusal)) =
            client.query(Layer::Confirmed, past_the_cap).await
        else {
            panic!("an answer past the cap is refused");
        };
        assert_eq!(refusal.reason, reason::CAPACITY);
        assert!(refusal.sentence.contains("smaller page"), "{refusal:?}");

        let programs = client.programs().await.unwrap();
        let code = programs["probe"];
        let framed = client.blob(code).await.unwrap().unwrap();
        assert_eq!(blobs::id_of(code.kind(), &framed), code);
        assert!(client.missing_blobs().await.unwrap().is_empty());
        assert!(
            client
                .logs()
                .await
                .unwrap()
                .iter()
                .any(|line| line.contains("node_started"))
        );
        assert!(client.metrics().await.unwrap().contains("tasks_spawned"));

        let forged = frame(&client, &alice, Vec::new()).await;
        let refused = client.admin(forged).await.unwrap_err();
        assert!(matches!(refused, noded::Error::Refused(_)), "{refused}");
        let unknown = Tip {
            height: 0,
            id: [7; 32],
        };
        let stale = client
            .admin(seat.admin_frame(unknown, Admin::Shutdown))
            .await
            .unwrap_err();
        let noded::Error::Refused(refusal) = stale else {
            panic!("an admin verb anchored to no recent block is refused: {stale}");
        };
        assert_eq!(refusal.reason, reason::ANCHOR);
        seat.admin(&client, Admin::LogFilter("debug".into())).await;
        seat.admin(&client, Admin::Shutdown).await;
        (block, holder.clone(), (landed, position, large_payload))
    });
    live.thread.join().unwrap();

    // reopened, the node answers the same receipts out of its archive: the
    // host is past these blocks and runs neither again
    let (block, holder, (landed, position, large_payload)) = kept;
    let live = seat.start();
    let client = live.client.clone();
    client_runtime().block_on(async {
        for seen in [&block, &holder] {
            let again = client.block(BlockRef::Height(seen.height)).await.unwrap();
            assert_eq!(again.as_ref(), Some(seen));
        }
        let at = TxRef::At {
            height: landed,
            position,
        };
        let opened = client
            .tx(TxAsk {
                tx: at,
                most: u32::MAX,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(opened.tx.payload, Some(large_payload));
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

#[test]
fn a_validator_runs_a_frame_once_it_holds_the_blob_the_frame_attaches() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);

    client_runtime().block_on(async {
        let (id, framed, attaching) = page_reader(&client, &alice, b"body").await;

        let unheld = client.submit(attaching.clone()).await.unwrap_err();
        let noded::Error::Refused(refusal) = unheld else {
            panic!("a frame whose attachment is not held is refused: {unheld}");
        };
        assert_eq!(refusal.reason, reason::NOT_FOUND);

        client.put_blob(id, framed).await.unwrap();
        let (_, receipt) = client
            .land(&Frame::decode(&attaching).unwrap())
            .await
            .unwrap();
        assert_eq!(receipt.program, "probe");
        let Outcome::Applied { output } = &receipt.outcome else {
            panic!("the attaching frame runs: {receipt:?}");
        };
        let read = Reply::Host(HostReply::Blob(Some(Blob {
            kind: "page".into(),
            body: b"body".to_vec(),
        })));
        assert_eq!(abi::decode::<Vec<Reply>>(output).unwrap(), vec![read]);
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

fn page_put(body: &[u8]) -> (Step, BlobId, Vec<u8>) {
    let put = Step::Op(HostOp::BlobPut {
        hash: HashKind::Sha256,
        kind: "page".into(),
        body: body.to_vec(),
    });
    let framed = blobs::frame("page", body).unwrap();
    (put, blobs::id_of(HashKind::Sha256, &framed), framed)
}

#[test]
fn a_node_serves_the_blobs_it_rosters_a_page_at_a_time() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);

    client_runtime().block_on(async {
        let (put_only, put_framed) = {
            let framed = blobs::frame("page", b"put, never rostered").unwrap();
            (blobs::id_of(HashKind::Sha256, &framed), framed)
        };
        client.put_blob(put_only, put_framed).await.unwrap();
        assert_eq!(client.blobs(&[put_only]).await.unwrap(), vec![None]);
        assert_eq!(client.blob(put_only).await.unwrap(), None);

        let (first, first_id, first_framed) = page_put(&vec![1; 5 << 19]);
        let (second, second_id, second_framed) = page_put(&vec![2; 5 << 19]);
        let (small, small_id, small_framed) = page_put(b"small");
        let steps = vec![first, second, small];
        let (_, receipt) = client
            .land(&Frame::decode(&frame(&client, &alice, steps).await).unwrap())
            .await
            .unwrap();
        assert!(
            matches!(receipt.outcome, Outcome::Applied { .. }),
            "{receipt:?}"
        );

        let asked = [put_only, first_id, small_id, second_id];
        let page = client.blobs(&asked).await.unwrap();
        assert_eq!(page, vec![None, Some(first_framed), Some(small_framed)]);
        let rest = client.blobs(&asked[page.len()..]).await.unwrap();
        assert_eq!(rest, vec![Some(second_framed.clone())]);
        assert_eq!(client.blob(second_id).await.unwrap(), Some(second_framed));
        assert!(client.blobs(&[]).await.unwrap().is_empty());
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

#[test]
fn a_reader_brings_a_node_a_rostered_blob_it_lacks_and_never_an_unrostered_one() {
    let root = tempfile::tempdir().unwrap();
    let holder = Seat::new(root.path(), "n0");
    let lacker = Seat::new(root.path(), "n1");
    let founding = founding(root.path(), &[&holder, &lacker], EPOCH_LENGTH);
    holder.init(&founding);
    lacker.init(&founding);
    let live = [holder.start(), lacker.start()];
    let alice = ed25519::PrivateKey::from_seed(11);
    client_runtime().block_on(async {
        let (holding, lacking) = (&live[0].client, &live[1].client);
        let opened = |metrics: &str| halo_streams(metrics, "quic") + halo_streams(metrics, "tcp");

        let framed = blobs::frame("page", b"held by one node, rostered by none").unwrap();
        let unrostered = blobs::id_of(HashKind::Sha256, &framed);
        holding.put_blob(unrostered, framed).await.unwrap();
        assert_eq!(lacking.blobs(&[unrostered]).await.unwrap(), vec![None]);
        assert_eq!(lacking.blob(unrostered).await.unwrap(), None);
        assert_eq!(opened(&lacking.metrics().await.unwrap()), 0);

        let (put, id, framed) = page_put(b"rostered, then lost by one node");
        let landing = frame(lacking, &alice, vec![put]).await;
        lacking
            .land(&Frame::decode(&landing).unwrap())
            .await
            .unwrap();
        let file = lacker
            .workspace
            .dir()
            .join("blobs")
            .join(blobs::name_of(&id));
        std::fs::remove_file(file).unwrap();
        assert_eq!(lacking.missing_blobs().await.unwrap(), vec![id]);

        assert_eq!(lacking.blobs(&[id]).await.unwrap(), vec![Some(framed)]);
        assert!(lacking.missing_blobs().await.unwrap().is_empty());
        assert!(opened(&lacking.metrics().await.unwrap()) > 0);
        for (seat, node) in [(&holder, &live[0]), (&lacker, &live[1])] {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in live {
        node.thread.join().unwrap();
    }
}

#[test]
fn a_node_reads_a_transaction_of_a_block_it_lists_no_summary_for() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);

    let large = client_runtime().block_on(async {
        let steps = vec![set(b"large", &[7; 2 * PREVIEW_BYTES as usize])];
        let large = frame(&client, &alice, steps).await;
        let (landed, _) = client.land(&Frame::decode(&large).unwrap()).await.unwrap();
        let block = client
            .block(BlockRef::Height(landed))
            .await
            .unwrap()
            .unwrap();
        let id = submission(&large).id();
        let position = block.txs.iter().position(|tx| tx.hash == id).unwrap() as u32;
        let mut blocks = client.changes(host::RECENT).await.unwrap();
        while blocks.next().await.unwrap().unwrap().height < landed + 4 {}
        seat.admin(&client, Admin::Shutdown).await;
        (landed, position, submission(&large))
    });
    live.thread.join().unwrap();

    let (landed, position, large) = large;
    for entry in std::fs::read_dir(seat.workspace.runtime_dir()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let summarized = name.contains("-summaries-") || name.contains("-txs-");
        if summarized {
            std::fs::remove_dir_all(&path).unwrap();
        }
    }

    let live = seat.start();
    let client = live.client.clone();
    client_runtime().block_on(async {
        let mut blocks = client.changes(host::RECENT).await.unwrap();
        blocks.next().await.unwrap().unwrap();
        let page = client.blocks(None, 10).await.unwrap();
        let listed_from = page.listed_from.expect("the blocks applied since");
        assert!(listed_from > landed, "{listed_from} lists {landed}");
        assert!(page.blocks.iter().all(|block| block.height >= listed_from));
        let below = client.blocks(Some(listed_from), 10).await.unwrap();
        assert!(below.blocks.is_empty());
        assert_eq!(below.listed_from, Some(listed_from));
        assert_eq!(client.block(BlockRef::Height(landed)).await.unwrap(), None);

        let at = TxRef::At {
            height: landed,
            position,
        };
        let read = client
            .tx(TxAsk {
                tx: at,
                most: u32::MAX,
            })
            .await
            .unwrap()
            .expect("read out of the block archive");
        assert_eq!((read.height, read.position), (landed, position));
        assert_eq!(read.tx.hash, large.id());
        assert_eq!(read.tx.size as usize, large.payload.len());
        assert_eq!(read.tx.payload.as_ref(), Some(&large.payload));
        assert_eq!(read.tx.receipt, None, "no run recorded");
        let by_hash = TxAsk {
            tx: TxRef::Hash(large.id()),
            most: 0,
        };
        assert_eq!(client.tx(by_hash).await.unwrap(), None);
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

#[test]
fn a_changes_subscriber_that_closes_is_answered() {
    use tokio_tungstenite::tungstenite::Message;

    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();

    client_runtime().block_on(async {
        let url = format!("ws://{}{}/probe", seat.http, noded::wire::route::CHANGES);
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        socket.send(Message::Close(None)).await.unwrap();
        let mut answered = Vec::new();
        while let Some(frame) = socket.next().await {
            answered.push(frame.unwrap());
        }
        assert!(
            matches!(answered.as_slice(), [Message::Close(_)]),
            "{answered:?}"
        );
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

/// One validator, and a resident the valset adds whose node never runs.
/// Only the validator is seated, so the chain finalizes on through the
/// epoch that records the resident; seating the resident too would ask a
/// quorum of two and stop it at that epoch's first block.
#[test]
fn an_offline_resident_leaves_the_chain_finalizing() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    let offline = ed25519::PrivateKey::from_seed(12);
    let enroll = Op::Enroll {
        address: Some(unheard()),
        invite: None,
    };

    client_runtime().block_on(async {
        let recorded = membership(&client, &offline, enroll).await;
        let resident_epoch = recorded / EPOCH_LENGTH + 1;
        let beyond = resident_epoch * EPOCH_LENGTH + 1;
        ::tokio::time::timeout(Duration::from_secs(30), reach(&client, beyond))
            .await
            .expect("the chain finalizes past the first block of the resident's epoch");
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

/// One validator, and a stranger that joins: it enrolls, waits for the epoch
/// that seats it as a resident, and state-syncs inside it. It runs no
/// engine, yet it follows at the tip: it applies the changes the validator
/// makes after it starts, and comes within two blocks of the validator before
/// the validator applies the block that closes the epoch, so no next-epoch
/// traffic carried it.
#[test]
fn a_resident_follows_at_the_tip() {
    // long enough that the resident joins, starts and catches up inside
    // one epoch
    const EPOCH: u64 = 100;
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let mut resident = Seat::new(root.path(), "n1");
    resident.address = None;
    let founding = founding(root.path(), &[&seat], EPOCH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);
    let runtime = client_runtime();

    resident.join(&client);
    let epoch = runtime.block_on(client.status()).unwrap().epoch;
    let closing = (epoch + 1) * EPOCH - 1;
    let follower = resident.start();
    runtime.block_on(async {
        let mut changes = follower.client.changes("probe").await.unwrap();
        let follow = async {
            let mut round: u64 = 1;
            loop {
                let write = set(b"a", &round.to_be_bytes());
                client
                    .submit(frame(&client, &alice, vec![write]).await)
                    .await
                    .unwrap();
                changes.next().await.unwrap().unwrap();
                // the resident first, so the validator's height bounds it
                let followed = follower.client.status().await.unwrap().height;
                let validator = client.status().await.unwrap().height;
                assert!(
                    validator < closing,
                    "the resident was at {followed} once the validator applied {validator}: \
                     block {closing} closes epoch {epoch}, so it followed only across the boundary"
                );
                if followed + 2 >= validator {
                    return;
                }
                round += 1;
            }
        };
        ::tokio::time::timeout(Duration::from_secs(120), follow)
            .await
            .expect("the resident applies each change");
        // it runs no engine, so it hears no finalize vote: every member,
        // the validator too, reads None on it
        let network = follower.client.network().await.unwrap();
        assert_eq!(network.members.len(), 2, "{network:?}");
        assert!(
            network.members.iter().all(|row| row.signed.is_none()),
            "{network:?}"
        );
        for (seat, node) in [(&seat, &live), (&resident, &follower)] {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    live.thread.join().unwrap();
    follower.thread.join().unwrap();
}

/// Three validators, and the valset drops the third. Once the chain is in
/// the epoch that seats two, the third node is shut down: the set of three
/// needed all three, so the two finalize on only because the shrink took
/// effect.
#[test]
fn a_shrunk_set_finalizes_without_the_removed_member() {
    let root = tempfile::tempdir().unwrap();
    let seats: Vec<Seat> = (0..3)
        .map(|i| Seat::new(root.path(), &format!("n{i}")))
        .collect();
    let founding = founding(root.path(), &seats.iter().collect::<Vec<_>>(), EPOCH_LENGTH);
    for seat in &seats {
        seat.init(&founding);
    }
    let live: Vec<Live> = seats.iter().map(Seat::start).collect();
    let remove = Op::Vote(Motion::Remove {
        key: seats[2].key(),
    });

    client_runtime().block_on(async {
        let client = &live[0].client;
        let recorded = membership(client, &seats[0].identity, remove).await;
        let shrunk = recorded / EPOCH_LENGTH + 1;
        ::tokio::time::timeout(
            Duration::from_secs(30),
            reach(client, shrunk * EPOCH_LENGTH),
        )
        .await
        .expect("the chain enters the epoch that seats two");
        seats[2].admin(&live[2].client, Admin::Shutdown).await;
        let beyond = (shrunk + 1) * EPOCH_LENGTH + 1;
        for node in &live[..2] {
            ::tokio::time::timeout(Duration::from_secs(30), reach(&node.client, beyond))
                .await
                .expect("the two finalize across the next boundary");
        }
        for (seat, node) in seats.iter().zip(&live).take(2) {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in live {
        node.thread.join().unwrap();
    }
}

#[test]
fn a_late_validator_joins_by_state_sync_and_follows() {
    let root = tempfile::tempdir().unwrap();
    let seats: Vec<Seat> = (0..4)
        .map(|i| Seat::new(root.path(), &format!("n{i}")))
        .collect();
    let founding = founding(root.path(), &seats.iter().collect::<Vec<_>>(), EPOCH_LENGTH);
    for seat in &seats[..3] {
        seat.init(&founding);
    }
    let live: Vec<Live> = seats[..3].iter().map(Seat::start).collect();
    let alice = ed25519::PrivateKey::from_seed(11);
    let runtime = client_runtime();

    let first = runtime.block_on(async {
        let mut changes = live[0].client.changes("probe").await.unwrap();
        live[0]
            .client
            .submit(frame(&live[0].client, &alice, vec![set(b"a", b"1")]).await)
            .await
            .unwrap();
        changes.next().await.unwrap().unwrap()
    });
    assert!(first.height >= 1);

    seats[3].join(&live[0].client);
    let joined = seats[3].start();
    let synced = runtime.block_on(async {
        let status = joined.client.status().await.unwrap();
        assert!(status.height >= first.height, "{status:?}");
        assert_eq!(
            joined
                .client
                .get(Layer::Confirmed, "probe", b"a")
                .await
                .unwrap(),
            Some(b"1".to_vec())
        );
        let mut changes = joined.client.changes("probe").await.unwrap();
        live[1]
            .client
            .submit(frame(&live[1].client, &alice, vec![set(b"b", b"2")]).await)
            .await
            .unwrap();
        changes.next().await.unwrap().unwrap()
    });
    assert_eq!(synced.writes, vec![(b"b".to_vec(), Some(b"2".to_vec()))]);

    runtime.block_on(async {
        for (seat, node) in seats
            .iter()
            .zip(live.iter().chain(std::iter::once(&joined)))
        {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in live.into_iter().chain(std::iter::once(joined)) {
        node.thread.join().unwrap();
    }
}

async fn landed(
    changes: &mut (impl futures::Stream<Item = noded::Result<noded::wire::Change>> + Unpin),
    keys: &[&[u8]],
) -> Vec<u64> {
    let mut heights = vec![None; keys.len()];
    while heights.iter().any(Option::is_none) {
        let change = changes.next().await.unwrap().unwrap();
        for (key, _) in &change.writes {
            if let Some(at) = keys.iter().position(|wanted| wanted == key) {
                heights[at].get_or_insert(change.height);
            }
        }
    }
    heights.into_iter().flatten().collect()
}

#[test]
fn a_resident_hands_every_frame_a_client_gives_it_to_the_validators() {
    let root = tempfile::tempdir().unwrap();
    let seats: Vec<Seat> = (0..3)
        .map(|i| Seat::new(root.path(), &format!("n{i}")))
        .collect();
    let resident = Seat::new(root.path(), "n3");
    let founding = founding(root.path(), &seats.iter().collect::<Vec<_>>(), EPOCH_LENGTH);
    for seat in &seats {
        seat.init(&founding);
    }
    let founders: Vec<Live> = seats.iter().map(Seat::start).collect();
    let bob = ed25519::PrivateKey::from_seed(12);
    let runtime = client_runtime();

    resident.join(&founders[0].client);
    let follower = resident.start();
    let given = |steps: Vec<Step>| runtime.block_on(frame(&follower.client, &bob, steps));

    let mut changes = runtime
        .block_on(founders[1].client.changes("probe"))
        .unwrap();
    let early = given(vec![set(b"early", b"at start")]);
    let early = runtime.block_on(follower.client.submit(early)).unwrap();
    assert!(
        matches!(early.outcome, Outcome::Applied { .. }),
        "{early:?}"
    );
    runtime.block_on(async {
        ::tokio::time::timeout(Duration::from_secs(60), landed(&mut changes, &[b"early"]))
            .await
            .expect("a frame given the instant the resident starts lands");
    });

    let (id, framed, unfetchable) = runtime.block_on(page_reader(
        &follower.client,
        &bob,
        b"only the resident holds this",
    ));
    runtime
        .block_on(follower.client.put_blob(id, framed))
        .unwrap();
    let pooled = runtime
        .block_on(follower.client.submit(unfetchable))
        .unwrap();
    assert!(
        matches!(pooled.outcome, Outcome::Applied { .. }),
        "{pooled:?}"
    );

    let failing = given(vec![Step::Fail("no".into())]);
    let mut spent = runtime
        .block_on(founders[1].client.changes(host::SPENT))
        .unwrap();
    let predicted = runtime
        .block_on(follower.client.submit(failing.clone()))
        .unwrap();
    assert!(
        matches!(predicted.outcome, Outcome::Rejected(_)),
        "{predicted:?}"
    );
    let carried = host::spent(&submission(&failing));
    runtime.block_on(async {
        ::tokio::time::timeout(Duration::from_secs(60), landed(&mut spent, &[&carried]))
            .await
            .expect(
                "a frame predicted to fail is relayed past one whose attachment no validator can fetch, and a block carries it",
            );
    });
    drop(spent);

    let nearly_a_block = node::BLOCK_BYTES - (1 << 16);
    let large: Vec<Vec<u8>> = (1..=2u8).map(|n| vec![n; nearly_a_block]).collect();
    for (n, value) in large.iter().enumerate() {
        let key = format!("large{n}");
        let large = given(vec![set(key.as_bytes(), value)]);
        runtime.block_on(follower.client.submit(large)).unwrap();
    }
    let [one, two] = runtime.block_on(async {
        ::tokio::time::timeout(
            Duration::from_secs(120),
            landed(&mut changes, &[b"large0", b"large1"]),
        )
        .await
        .expect("two nearly full frames land")
    })[..] else {
        panic!("both large frames land");
    };
    assert!(
        one < two,
        "two nearly full frames share no block: {one}, {two}"
    );
    drop(changes);

    let refused = runtime.block_on(follower.client.submit(vec![0; node::BLOCK_BYTES + 1]));
    let Err(noded::Error::Refused(refusal)) = refused else {
        panic!("a frame no block carries is refused: {refused:?}");
    };
    assert_eq!(refusal.reason, abi::reason::CAPACITY);

    runtime.block_on(async {
        for (seat, node) in seats.iter().zip(&founders).chain([(&resident, &follower)]) {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in founders.into_iter().chain([follower]) {
        node.thread.join().unwrap();
    }
}

/// Ends TLS at `listen` and carries the bytes to `to`, as a tunnel or a
/// reverse proxy in front of a node's HTTP port does.
fn terminate_tls(runtime: &::tokio::runtime::Runtime, listen: SocketAddr, to: SocketAddr) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let server = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![certified.cert.der().clone()], key.into())
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(server));
    runtime.spawn(async move {
        let listener = ::tokio::net::TcpListener::bind(listen).await.unwrap();
        loop {
            let (remote, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            ::tokio::spawn(async move {
                let Ok(mut remote) = acceptor.accept(remote).await else {
                    return;
                };
                let mut node = ::tokio::net::TcpStream::connect(to).await.unwrap();
                let _ = ::tokio::io::copy_bidirectional(&mut remote, &mut node).await;
            });
        }
    });
}

fn halo_streams(metrics: &str, path: &str) -> u64 {
    let name = format!("halo_streams_over_{path}_total");
    metrics
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter(|line| {
            let metric = line.split_whitespace().next().unwrap_or_default();
            metric.ends_with(&name)
        })
        .filter_map(|line| line.split_whitespace().last()?.parse::<u64>().ok())
        .sum()
}

async fn fetched_over(holder: &Client, fetcher: &Client, path: &str, other: &str) {
    let alice = ed25519::PrivateKey::from_seed(12);
    let (id, framed, attaching) = page_reader(holder, &alice, b"carried over halo").await;
    holder.put_blob(id, framed).await.unwrap();
    let (_, receipt) = ::tokio::time::timeout(
        Duration::from_secs(60),
        holder.land(&Frame::decode(&attaching).unwrap()),
    )
    .await
    .expect("the frame lands once the other validator fetched what it attaches")
    .unwrap();
    let Outcome::Applied { output } = &receipt.outcome else {
        panic!("the attaching frame runs: {receipt:?}");
    };
    let read = Reply::Host(HostReply::Blob(Some(Blob {
        kind: "page".into(),
        body: b"carried over halo".to_vec(),
    })));
    assert_eq!(abi::decode::<Vec<Reply>>(output).unwrap(), vec![read]);
    let metrics = fetcher.metrics().await.unwrap();
    assert!(halo_streams(&metrics, path) > 0, "{metrics}");
    assert_eq!(halo_streams(&metrics, other), 0, "{metrics}");
}

fn reached(
    runtime: &::tokio::runtime::Runtime,
    root: &Path,
    dialer: Seat,
    dialed: Seat,
    (path, other): (&str, &str),
) {
    let founding = founding(root, &[&dialer, &dialed], EPOCH_LENGTH);
    dialer.init(&founding);
    dialed.init(&founding);
    let live = [dialer.start(), dialed.start()];
    let alice = ed25519::PrivateKey::from_seed(11);
    runtime.block_on(async {
        let client = &live[0].client;
        let mut changes = client.changes("probe").await.unwrap();
        client
            .submit(frame(client, &alice, vec![set(b"a", b"1")]).await)
            .await
            .unwrap();
        let change = ::tokio::time::timeout(Duration::from_secs(60), changes.next())
            .await
            .expect("the two validators finalize a block")
            .unwrap()
            .unwrap();
        assert_eq!(change.writes, vec![(b"a".to_vec(), Some(b"1".to_vec()))]);
        fetched_over(&live[1].client, client, path, other).await;
        for (seat, node) in [(&dialer, &live[0]), (&dialed, &live[1])] {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in live {
        node.thread.join().unwrap();
    }
}

const OVER_QUIC: (&str, &str) = ("quic", "tcp");
const OVER_TCP: (&str, &str) = ("tcp", "quic");

fn dialed_at(address: impl FnOnce(&Seat) -> String, halo: (&str, &str)) {
    let root = tempfile::tempdir().unwrap();
    let mut dialer = Seat::new(root.path(), "n0");
    let mut dialed = Seat::new(root.path(), "n1");
    dialer.address = None;
    dialed.address = Some(address(&dialed));
    reached(&client_runtime(), root.path(), dialer, dialed, halo);
}

#[test]
fn a_validator_is_dialed_over_tcp_at_its_name() {
    dialed_at(
        |seat| format!("tcp://localhost:{}", seat.p2p.port()),
        OVER_QUIC,
    );
}

#[test]
fn a_validator_is_dialed_over_tls() {
    dialed_at(
        |seat| format!("tls://localhost:{}", seat.tls.port()),
        OVER_QUIC,
    );
}

#[test]
fn a_validator_is_dialed_over_a_websocket_on_its_http_port() {
    dialed_at(
        |seat| format!("ws://localhost:{}", seat.http.port()),
        OVER_TCP,
    );
}

#[test]
fn a_validator_whose_udp_stays_silent_is_fetched_from_over_tcp() {
    let root = tempfile::tempdir().unwrap();
    let mut dialer = Seat::new(root.path(), "n0");
    let mut dialed = Seat::new(root.path(), "n1");
    let _silent = std::net::UdpSocket::bind(dialed.p2p).unwrap();
    dialer.address = None;
    dialed.address = Some(format!("tcp://localhost:{}", dialed.p2p.port()));
    reached(&client_runtime(), root.path(), dialer, dialed, OVER_TCP);
}

#[test]
fn a_validator_is_dialed_over_wss_through_a_tls_terminator() {
    let root = tempfile::tempdir().unwrap();
    let runtime = client_runtime();
    let mut dialer = Seat::new(root.path(), "n0");
    let mut dialed = Seat::new(root.path(), "n1");
    let terminator = free_port();
    terminate_tls(&runtime, terminator, dialed.http);
    dialer.address = None;
    dialed.address = Some(format!("wss://localhost:{}", terminator.port()));
    reached(&runtime, root.path(), dialer, dialed, OVER_TCP);
}
