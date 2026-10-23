// Signed calls through halo's ducktape/forward lane to a service a member's node forwards.

use ::tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::*;

const CALLER_SEED: u64 = 31;

const STRANGER_SEED: u64 = 32;

struct Echo {
    address: SocketAddr,
    accepted: Arc<AtomicUsize>,
}

async fn echo() -> Echo {
    let listener = ::tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counted = accepted.clone();
    ::tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            counted.fetch_add(1, Ordering::SeqCst);
            ::tokio::spawn(async move {
                let mut heard = Vec::new();
                stream.read_to_end(&mut heard).await.unwrap();
                stream.write_all(b"echo:").await.unwrap();
                stream.write_all(&heard).await.unwrap();
                stream.shutdown().await.unwrap();
            });
        }
    });
    Echo { address, accepted }
}

fn offer(seat: &Seat, address: SocketAddr, signer: &ed25519::PrivateKey) {
    let signer = hex::encode(signer.public_key().as_ref());
    let text = format!("[services.echo]\naddress = \"{address}\"\nsigners = [\"{signer}\"]\n");
    std::fs::write(seat.workspace.dir().join("forward.toml"), text).unwrap();
}

async fn call(
    client: &Client,
    key: &ed25519::PrivateKey,
    member: &[u8],
    service: &str,
    said: &[u8],
) -> noded::Result<Vec<u8>> {
    let anchor = client.status().await.unwrap().anchor();
    let frame = Frame::sign_forward(key, NETWORK.as_bytes(), anchor, service, member.to_vec());
    let mut heard = Vec::new();
    client.forward(&frame, said, &mut heard).await?;
    Ok(heard)
}

fn refusal_reason(called: noded::Result<Vec<u8>>) -> String {
    match called {
        Err(noded::Error::Refused(refusal)) => refusal.reason,
        other => panic!("the call is refused: {other:?}"),
    }
}

#[test]
fn a_listed_signer_reaches_a_members_forwarded_service_through_any_node_and_no_refused_call_connects()
 {
    apart!();
    let root = tempfile::tempdir().unwrap();
    let calling = Seat::new(root.path(), "n0");
    let serving = Seat::new(root.path(), "n1");
    let founding = founding(root.path(), &[&calling, &serving], EPOCH_LENGTH);
    calling.init(&founding);
    serving.init(&founding);
    let live = vec![calling.start(), serving.start()];
    let caller = ed25519::PrivateKey::from_seed(CALLER_SEED);
    let stranger = ed25519::PrivateKey::from_seed(STRANGER_SEED);
    let runtime = client_runtime();
    runtime.block_on(async {
        let echo = echo().await;
        offer(&serving, echo.address, &caller);
        let through = &live[0].client;
        let at = serving.key();
        let unopened = call(through, &caller, &at, "echo", b"hi").await;
        assert_eq!(refusal_reason(unopened), reason::UNSUPPORTED);

        let open = halo::Op::Open {
            name: noded::FORWARD_LANE.into(),
            cap: halo::Class::Client,
        };
        let motion = Motion::Role {
            role: role::HALO.into(),
            op: abi::encode(&open),
        };
        let height = membership(through, &calling.identity, Op::Vote(motion)).await;
        for node in &live {
            reach(&node.client, height).await;
        }

        let unlisted = call(through, &stranger, &at, "echo", b"hi").await;
        assert_eq!(refusal_reason(unlisted), reason::NOT_FOUND);
        let unoffered = call(through, &caller, &at, "nothing", b"hi").await;
        assert_eq!(refusal_reason(unoffered), reason::NOT_FOUND);
        let nobody = stranger.public_key();
        let unmembered = call(through, &caller, nobody.as_ref(), "echo", b"hi").await;
        assert_eq!(refusal_reason(unmembered), reason::NOT_FOUND);
        let anchor = through.status().await.unwrap().anchor();
        let asked = Frame::sign_query(&caller, NETWORK.as_bytes(), anchor, "echo", at.clone());
        let mut heard = Vec::new();
        let misnamed = through.forward(&asked, &b"hi"[..], &mut heard).await;
        let misnamed = misnamed.map(|()| heard);
        assert_eq!(refusal_reason(misnamed), reason::INVALID_INPUT);
        assert_eq!(echo.accepted.load(Ordering::SeqCst), 0);

        let relayed = call(through, &caller, &at, "echo", b"hello").await.unwrap();
        assert_eq!(relayed, b"echo:hello");
        let local = call(&live[1].client, &caller, &at, "echo", b"here")
            .await
            .unwrap();
        assert_eq!(local, b"echo:here");
        assert_eq!(echo.accepted.load(Ordering::SeqCst), 2);
    });
    shut_down(&runtime, &[&calling, &serving], live);
}

async fn told(stream: &mut (impl ::tokio::io::AsyncRead + Unpin)) -> noded::wire::Forwarded {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).await.unwrap();
    let mut bytes = vec![0; u32::from_le_bytes(len) as usize];
    stream.read_exact(&mut bytes).await.unwrap();
    abi::decode(&bytes).unwrap()
}

#[test]
fn a_member_node_checks_a_call_itself_and_refuses_one_sent_straight_down_the_lane_before_connecting()
 {
    apart!();
    let root = tempfile::tempdir().unwrap();
    let calling = Seat::new(root.path(), "n0");
    let serving = Seat::new(root.path(), "n1");
    let founding = founding(root.path(), &[&calling, &serving], EPOCH_LENGTH);
    calling.init(&founding);
    serving.init(&founding);
    let live = vec![calling.start(), serving.start()];
    let caller = ed25519::PrivateKey::from_seed(CALLER_SEED);
    let stranger = ed25519::PrivateKey::from_seed(STRANGER_SEED);
    let runtime = client_runtime();
    let through = &live[0].client;
    let accepted = runtime.block_on(async {
        let echo = echo().await;
        offer(&serving, echo.address, &caller);
        let open = halo::Op::Open {
            name: noded::FORWARD_LANE.into(),
            cap: halo::Class::Client,
        };
        let motion = Motion::Role {
            role: role::HALO.into(),
            op: abi::encode(&open),
        };
        membership(through, &calling.identity, Op::Vote(motion)).await;
        echo.accepted
    });
    let rogue = Workspace::at(root.path().join("rogue"));
    let udp = UdpSocket::bind((outbound_ip(), 0)).unwrap();
    let registered = udp.local_addr().unwrap().to_string();
    runner(&rogue.runtime_dir())
        .start(|context| noded::enroll(context, &rogue, through.clone(), registered, None))
        .unwrap();
    let served = &live[1].client;
    let member = serving.identity.public_key();
    let at = serving.key();
    let elsewhere_key = calling.key();
    let reasons = runner(&rogue.runtime_dir()).start(|context| async move {
        let key = rogue.identity().unwrap();
        let status = through.status().await.unwrap();
        reach(served, status.height).await;
        let config = ::halo::Config {
            egress_bytes_per_sec: None,
            udp: udp.into(),
            queues: ::halo::Queues::default(),
            reach: ::halo::Https::shared(),
        };
        let halo = ::halo::Halo::start(&context, key.clone(), config).unwrap();
        let seating = noded::seating(through, &key, &status).await.unwrap();
        halo.seat(seating.seats);
        halo.plane().lanes(seating.lanes);
        let streams = halo.plane().streams(noded::FORWARD_LANE, 1).unwrap();
        let anchor = status.anchor();
        let network = NETWORK.as_bytes();
        let good = Frame::sign_forward(&caller, network, anchor, "echo", at.clone());
        let mut opened = streams
            .open(&member, ::halo::Class::Client, &good.encode())
            .await
            .unwrap();
        assert_eq!(told(&mut opened).await, noded::wire::Forwarded::Opened);
        opened.write_all(b"down the lane").await.unwrap();
        opened.shutdown().await.unwrap();
        let mut heard = Vec::new();
        opened.read_to_end(&mut heard).await.unwrap();
        assert_eq!(heard, b"echo:down the lane");

        let mut forged = Frame::sign_forward(&stranger, network, anchor, "echo", at.clone());
        forged.body.signer = caller.public_key().as_ref().to_vec();
        let unknown = host::Tip {
            height: anchor.height,
            id: [7; 32],
        };
        let stale = Frame::sign_forward(&caller, network, unknown, "echo", at.clone());
        let elsewhere = Frame::sign_forward(&caller, network, anchor, "echo", elsewhere_key);
        let queried = Frame::sign_query(&caller, network, anchor, "echo", at.clone());
        let whole = good.encode();
        let cut = whole[..whole.len() / 2].to_vec();
        let bad = [
            ("forged", forged.encode()),
            ("stale", stale.encode()),
            ("elsewhere", elsewhere.encode()),
            ("queried", queried.encode()),
            ("cut", cut),
        ];
        let mut reasons = Vec::new();
        for (name, frame) in bad {
            let mut stream = streams
                .open(&member, ::halo::Class::Client, &frame)
                .await
                .unwrap();
            match told(&mut stream).await {
                noded::wire::Forwarded::Refused(refusal) => reasons.push((name, refusal.reason)),
                noded::wire::Forwarded::Opened => panic!("the {name} frame was let through"),
            }
        }
        reasons
    });
    println!("refused straight down the lane: {reasons:?}");
    let reason_of = |wanted: &str| {
        reasons
            .iter()
            .find(|(name, _)| *name == wanted)
            .map(|(_, reason)| reason.clone())
    };
    assert_eq!(reason_of("forged").as_deref(), Some(reason::INVALID_INPUT));
    assert_eq!(reason_of("queried").as_deref(), Some(reason::INVALID_INPUT));
    assert_eq!(
        reason_of("elsewhere").as_deref(),
        Some(reason::UNAUTHORIZED)
    );
    assert_eq!(reasons.len(), 5);
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    shut_down(&runtime, &[&calling, &serving], live);
}

#[test]
fn a_node_names_a_block_in_its_status_only_once_it_serves_the_lane_and_seats_of_that_block() {
    apart!();
    let root = tempfile::tempdir().unwrap();
    let calling = Seat::new(root.path(), "n0");
    let serving = Seat::new(root.path(), "n1");
    let founding = founding(root.path(), &[&calling, &serving], EPOCH_LENGTH);
    calling.init(&founding);
    serving.init(&founding);
    let (through, daemon) =
        calling.start_handing(LoadConfig::default(), |running| running.daemon.clone());
    let live = vec![through, serving.start()];
    let caller = ed25519::PrivateKey::from_seed(CALLER_SEED);
    let stranger = ed25519::PrivateKey::from_seed(STRANGER_SEED);
    let runtime = client_runtime();
    runtime.block_on(async {
        let echo = echo().await;
        offer(&serving, echo.address, &caller);
        let through = &live[0].client;
        let at = serving.key();
        let open = halo::Op::Open {
            name: noded::FORWARD_LANE.into(),
            cap: halo::Class::Client,
        };
        let motion = Motion::Role {
            role: role::HALO.into(),
            op: abi::encode(&open),
        };
        let announced = through.status().await.unwrap().height;
        let retabling = daemon.retabling.arm();
        let reseating = daemon.reseating.arm();
        let (voted_at, voted) = ::tokio::sync::watch::channel(u64::MAX);
        let voting = async {
            let height = membership(through, &calling.identity, Op::Vote(motion)).await;
            voted_at.send_replace(height);
            height
        };
        let (height, retabled, reseated) = futures::join!(
            voting,
            served_before_announced(through, retabling, announced, voted.clone()),
            served_before_announced(through, reseating, announced, voted),
        );
        assert_eq!(
            (retabled, reseated),
            (height, height),
            "the node retables and reseats at the block that lands the vote"
        );
        for node in &live {
            reach(&node.client, height).await;
        }
        let unlisted = call(through, &stranger, &at, "echo", b"hi").await;
        assert_eq!(refusal_reason(unlisted), reason::NOT_FOUND);
        assert_eq!(echo.accepted.load(Ordering::SeqCst), 0);
    });
    drop(daemon);
    shut_down(&runtime, &[&calling, &serving], live);
}
