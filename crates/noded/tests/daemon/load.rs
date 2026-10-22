// Pruning and finalization under floods and held blocking threads.

use super::*;

const PRUNING_BOUNDARY: &str = "node_host_program_pruning_boundary";

const KEPT: u32 = 32 << 10;

const PARKS: u64 = 24;

const PRUNES: usize = 3;

const ROUNDS: u64 = 400;

const AGREED: usize = 3;

fn bytes_under(path: &Path) -> u64 {
    let Ok(metadata) = std::fs::metadata(path) else {
        return 0;
    };
    if !metadata.is_dir() {
        return metadata.len();
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| bytes_under(&entry.path()))
        .sum()
}

fn parked_bytes(seat: &Seat) -> u64 {
    let marker = format!("-{}-", hex::encode(host::PARKED));
    std::fs::read_dir(seat.workspace.runtime_dir())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            name.starts_with("commitment-") && name.contains(&marker)
        })
        .map(|path| bytes_under(&path))
        .sum()
}

async fn parked_gauge(client: &Client, name: &str) -> u64 {
    let text = client.metrics().await.unwrap();
    let prefix = format!("{name}{{program=\"{}\"}} ", host::PARKED);
    let value = text
        .lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .unwrap();
    value.trim().parse().unwrap()
}

fn wait_plan() -> Vec<u8> {
    abi::encode(&fixture_awaiter::Plan {
        target: "oracle".to_owned(),
        acting: None,
        asks: vec![abi::encode(&fixture_oracle::Ask::Later)],
        together: false,
        kept: KEPT,
        fail: false,
    })
}

async fn applied_all(client: &Client, frames: &[Frame]) {
    let landed = futures::future::join_all(frames.iter().map(|frame| client.land(frame))).await;
    for result in landed {
        let (_, receipt) = result.unwrap();
        assert!(
            matches!(receipt.outcome, Outcome::Applied { .. }),
            "{receipt:?}"
        );
    }
}

async fn park_and_resume(client: &Client, key: &ed25519::PrivateKey, round: u64) {
    let anchor = client.status().await.unwrap().anchor();
    let parks: Vec<Frame> = (0..PARKS)
        .map(|n| {
            let salt = Some(round * PARKS + n);
            Frame::sign(
                key,
                NETWORK.as_bytes(),
                anchor,
                salt,
                "awaiter",
                wait_plan(),
            )
        })
        .collect();
    applied_all(client, &parks).await;
    let owed = client
        .scan(Layer::Confirmed, "oracle", Scan::prefix(b"owed/"))
        .await
        .unwrap();
    let anchor = client.status().await.unwrap().anchor();
    let answers: Vec<Frame> = owed
        .iter()
        .map(|entry| {
            let ticket = u64::from_be_bytes(entry.key[b"owed/".len()..].try_into().unwrap());
            let op = fixture_oracle::Op::Answer {
                ticket,
                answer: Ok(b"answered".to_vec()),
            };
            Frame::sign(
                key,
                NETWORK.as_bytes(),
                anchor,
                None,
                "oracle",
                abi::encode(&op),
            )
        })
        .collect();
    applied_all(client, &answers).await;
}

struct Churn<'a> {
    client: Client,
    key: ed25519::PrivateKey,
    seat: &'a Seat,
    round: u64,
    sizes: Vec<u64>,
    boundaries: Vec<u64>,
}

impl Churn<'_> {
    async fn round(&mut self) {
        park_and_resume(&self.client, &self.key, self.round).await;
        self.round += 1;
        self.sizes.push(parked_bytes(self.seat));
        self.boundaries
            .push(parked_gauge(&self.client, PRUNING_BOUNDARY).await);
    }

    async fn prune_past(&mut self, past: u64, prunes: usize) {
        let from = self.boundaries.len() - 1;
        for _ in 0..ROUNDS {
            let seen = &self.boundaries[from..];
            let pruned = seen.windows(2).filter(|pair| pair[1] > pair[0]).count();
            let beyond = seen.last().is_some_and(|last| *last > past);
            if beyond && pruned >= prunes {
                return;
            }
            self.round().await;
        }
        panic!(
            "$parked's log start moved {:?}, not {prunes} times past {past}",
            &self.boundaries[from..]
        );
    }
}

struct Gate {
    held: AtomicUsize,
    asked: StdMutex<Option<oneshot::Sender<u64>>>,
    release: StdMutex<Option<oneshot::Receiver<()>>>,
    heads: AtomicUsize,
    stale: AtomicUsize,
}

#[derive(Clone)]
struct Held {
    client: Client,
    gate: Arc<Gate>,
}

fn sync_start(request: &statesync::Request) -> u64 {
    let statesync::Request::Sync { request, .. } = request else {
        panic!("only a sync request starts somewhere");
    };
    let decoded =
        <statesync::SyncRequest as commonware_codec::DecodeExt<_>>::decode(request.as_slice())
            .unwrap();
    *decoded.start()
}

impl statesync::Exchange for Held {
    type Error = noded::Error;

    async fn exchange(&self, request: statesync::Request) -> noded::Result<statesync::Response> {
        let parked =
            matches!(&request, statesync::Request::Sync { program, .. } if program == host::PARKED);
        let first = parked && self.gate.held.fetch_add(1, Ordering::SeqCst) == 0;
        if first {
            let asked = self.gate.asked.lock().unwrap().take().unwrap();
            asked.send(sync_start(&request)).unwrap();
            let release = self.gate.release.lock().unwrap().take().unwrap();
            release.await.unwrap();
        }
        let response = self.client.exchange(request).await?;
        match &response {
            statesync::Response::Head(_) => {
                self.gate.heads.fetch_add(1, Ordering::SeqCst);
            }
            statesync::Response::Refused(refusal) if refusal.reason == reason::STALE => {
                self.gate.stale.fetch_add(1, Ordering::SeqCst);
            }
            statesync::Response::Sync(_)
            | statesync::Response::Refused(_)
            | statesync::Response::Released => {}
        }
        Ok(response)
    }

    fn stream_blobs(
        &self,
        ids: Vec<abi::BlobId>,
    ) -> impl futures::Stream<Item = noded::Result<Option<Vec<u8>>>> + Send {
        statesync::Exchange::stream_blobs(&self.client, ids)
    }
}

fn join_held(seat: &Seat, source: Client, gate: Arc<Gate>) -> JoinHandle<()> {
    let workspace = seat.workspace.clone();
    std::thread::spawn(move || {
        runner(&workspace.runtime_dir()).start(|context| async move {
            let status = source.status().await.unwrap();
            let descriptor = noded::Descriptor {
                network: status.network,
                time: status.time,
                block_time_ms: status.block_time_ms,
            };
            let held = Held {
                client: source,
                gate,
            };
            let joined = statesync::join(
                context.child("join"),
                &descriptor.network,
                workspace.dir(),
                descriptor.id(),
                std::time::Duration::from_millis(descriptor.block_time_ms),
                held,
            )
            .await
            .unwrap();
            workspace.write_descriptor(&descriptor).unwrap();
            workspace.write_anchor(&joined.anchor).unwrap();
            let stamp = noded::Stamp {
                contract: noded::NODE_CONTRACT,
                from: joined.node.tip().unwrap().height + 1,
            };
            workspace.write_stamp(&stamp).unwrap();
        });
    })
}

async fn agreed(clients: &[Client]) -> Vec<u64> {
    let mut ticks = clients[0].changes(host::RECENT).await.unwrap();
    let mut seen: Vec<BTreeMap<u64, abi::Root>> = clients.iter().map(|_| BTreeMap::new()).collect();
    loop {
        ticks.next().await.unwrap().unwrap();
        let statuses = futures::future::join_all(clients.iter().map(Client::status)).await;
        for (status, heights) in statuses.into_iter().zip(seen.iter_mut()) {
            let status = status.unwrap();
            heights.insert(status.height, status.root);
        }
        let common: Vec<u64> = seen[0]
            .keys()
            .copied()
            .filter(|height| seen.iter().all(|heights| heights.contains_key(height)))
            .collect();
        for height in &common {
            let roots: Vec<&abi::Root> = seen.iter().map(|heights| &heights[height]).collect();
            assert!(
                roots.windows(2).all(|pair| pair[0] == pair[1]),
                "the roots differ at {height}: {roots:?}"
            );
        }
        if common.len() >= AGREED {
            return common;
        }
    }
}

#[test]
fn parked_state_prunes_while_a_restart_and_two_joins_reach_the_same_root() {
    apart!();
    let root = tempfile::tempdir().unwrap();
    let mut seats: Vec<Seat> = (0..4)
        .map(|i| Seat::new(root.path(), &format!("n{i}")))
        .collect();
    // the second restarts on listeners it binds afresh, so it registers no address and dials out
    seats[1].address = None;
    let mut plain = Seat::new(root.path(), "r0");
    plain.address = None;
    let mut held = Seat::new(root.path(), "r1");
    held.address = None;
    let founding = founding_with(
        root.path(),
        &seats.iter().collect::<Vec<_>>(),
        EPOCH_LENGTH,
        6,
        &[("awaiter", AWAITER), ("oracle", ORACLE)],
    );
    for seat in &seats {
        seat.init(&founding);
    }
    let mut live: Vec<Live> = seats.iter().map(Seat::start).collect();
    let source = live[0].client.clone();
    let runtime = client_runtime();
    let mut churn = Churn {
        client: source.clone(),
        key: ed25519::PrivateKey::from_seed(11),
        seat: &seats[0],
        round: 0,
        sizes: vec![parked_bytes(&seats[0])],
        boundaries: vec![runtime.block_on(parked_gauge(&source, PRUNING_BOUNDARY))],
    };

    runtime.block_on(churn.prune_past(0, 1));
    plain.join(&source);
    let plain_live = plain.start();

    runtime.block_on(seats[1].admin(&live[1].client, Admin::Shutdown));
    live.remove(1).thread.join().unwrap();
    runtime.block_on(async {
        churn.round().await;
        churn.round().await;
    });
    live.insert(1, seats[1].start());

    runtime.block_on(membership(
        &source,
        &held.identity,
        Op::Enroll {
            address: None,
            halo: held.halo_address(),
            invite: None,
        },
    ));
    let (asked, start) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let gate = Arc::new(Gate {
        held: AtomicUsize::new(0),
        asked: StdMutex::new(Some(asked)),
        release: StdMutex::new(Some(released)),
        heads: AtomicUsize::new(0),
        stale: AtomicUsize::new(0),
    });
    let joining = join_held(&held, source.clone(), gate.clone());
    let start = runtime.block_on(start).unwrap();
    runtime.block_on(churn.prune_past(start, PRUNES));
    release.send(()).unwrap();
    joining.join().unwrap();
    let held_live = held.start();

    let clients: Vec<Client> = live
        .iter()
        .chain([&plain_live, &held_live])
        .map(|node| node.client.clone())
        .collect();
    let common = runtime.block_on(agreed(&clients));

    let stale = gate.stale.load(Ordering::SeqCst);
    let heads = gate.heads.load(Ordering::SeqCst);
    assert!(stale >= 1, "the held join was refused stale {stale} times");
    assert!(heads >= 2, "the held join read the head {heads} times");
    let peak = *churn.sizes.iter().max().unwrap();
    let last = *churn.sizes.last().unwrap();
    let shrank = churn.sizes.windows(2).any(|pair| pair[1] < pair[0]);
    assert!(shrank && last < peak, "$parked on disk {:?}", churn.sizes);
    println!(
        "rounds {} | $parked pruning boundary {:?} | $parked bytes {:?} | held start {start} | stale {stale} | heads {heads} | agreed heights {common:?}",
        churn.round, churn.boundaries, churn.sizes
    );

    runtime.block_on(async {
        for (seat, node) in seats
            .iter()
            .chain([&plain, &held])
            .zip(live.iter().chain([&plain_live, &held_live]))
        {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in live.into_iter().chain([plain_live, held_live]) {
        node.thread.join().unwrap();
    }
}

fn unlanded() -> TxAsk {
    TxAsk {
        tx: TxRef::Hash([7; 32]),
        most: 0,
    }
}

const FLOOD_SEED: u64 = 1 << 20;

fn crowded(anchor: Tip, key: &ed25519::PrivateKey, salt: u64) -> Vec<u8> {
    let steps = vec![set(b"crowd", &salt.to_le_bytes())];
    Frame::sign(
        key,
        NETWORK.as_bytes(),
        anchor,
        Some(salt),
        "probe",
        abi::encode(&steps),
    )
    .encode()
}

async fn held_under_a_submit_flood(client: &Client, page: &Path) {
    let anchor = client.status().await.unwrap().anchor();
    let (stop, stopped) = ::tokio::sync::watch::channel(false);
    let mut floods = ::tokio::task::JoinSet::new();
    for submitter in 0..RPC_BLOCKING_THREADS.get() as u64 {
        let client = client.clone();
        let stopped = stopped.clone();
        floods.spawn(async move {
            let key = ed25519::PrivateKey::from_seed(FLOOD_SEED + submitter);
            let resent = crowded(anchor, &key, submitter);
            while !*stopped.borrow() {
                let _ = client.submit(resent.clone(), Preconfirm::Off).await;
            }
        });
    }
    let (_, bundle) = put_page(client, page, 1).await;
    let status = client.status().await.unwrap();
    let role = status.roles.bound(role::BLOBS).unwrap();
    client.held(role, bundle).await.unwrap();
    stop.send_replace(true);
    floods.join_all().await;
}

fn sync_flood(
    client: &Client,
    stopped: ::tokio::sync::watch::Receiver<bool>,
    answered: Arc<std::sync::atomic::AtomicU64>,
) -> ::tokio::task::JoinSet<()> {
    let mut floods = ::tokio::task::JoinSet::new();
    for _ in 0..RPC_BLOCKING_THREADS.get() {
        let client = client.clone();
        let stopped = stopped.clone();
        let answered = answered.clone();
        floods.spawn(async move {
            while !*stopped.borrow() {
                let asked = statesync::Exchange::exchange(&client, statesync::Request::Head).await;
                let Some(request) = asked.ok().as_ref().and_then(first_target) else {
                    continue;
                };
                let synced = statesync::Exchange::exchange(&client, request).await;
                if matches!(synced, Ok(statesync::Response::Sync(_))) {
                    answered.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        });
    }
    floods
}

#[test]
fn blocks_finalize_under_a_submit_flood_with_every_rpc_blocking_thread_held_then_under_an_answered_sync_flood()
 {
    apart!();
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let load = LoadConfig {
        rpc_blocking_threads: Some(RPC_BLOCKING_THREADS),
        ..LoadConfig::default()
    };
    let (live, rpc) = seat.start_handing(load, |running| running.rpc());
    let rpc = rpc.expect("a running node serves rpc");
    let client = live.client.clone();
    let page = root.path().join("page");
    std::fs::write(&page, b"receipted while submits flood the pool").unwrap();
    let runtime = client_runtime();
    runtime.block_on(held_under_a_submit_flood(&client, &page));
    let release = Arc::new(std::sync::RwLock::new(()));
    let held = release.write().unwrap();
    let (holding, holds) = mpsc::channel();
    for _ in 0..RPC_BLOCKING_THREADS.get() {
        let release = release.clone();
        let holding = holding.clone();
        rpc.spawn_blocking(move || {
            holding.send(()).unwrap();
            drop(release.read().unwrap());
        });
    }
    for _ in 0..RPC_BLOCKING_THREADS.get() {
        holds.recv().unwrap();
    }
    runtime.block_on(async {
        let mut blocks = client.changes(host::RECENT).await.unwrap();
        let from = client.status().await.unwrap().height;
        let mut height = from;
        while height < from + EPOCH_LENGTH {
            let reader = client.clone();
            ::tokio::spawn(async move { reader.tx(unlanded()).await });
            height = blocks.next().await.unwrap().unwrap().height;
        }
    });
    drop(held);
    runtime.block_on(async {
        let answered = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (stop, stopped) = ::tokio::sync::watch::channel(false);
        let syncs = sync_flood(&client, stopped, answered.clone());
        let mut blocks = client.changes(host::RECENT).await.unwrap();
        let from = client.status().await.unwrap().height;
        let mut height = from;
        while height < from + EPOCH_LENGTH {
            height = blocks.next().await.unwrap().unwrap().height;
        }
        let during = answered.load(std::sync::atomic::Ordering::Relaxed);
        stop.send_replace(true);
        syncs.join_all().await;
        assert!(
            during > 0,
            "no sync was answered while {EPOCH_LENGTH} blocks finalized"
        );
    });
    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}
