//! A fork of a stopped network runs alone: it makes blocks by itself and by hand, takes an op as
//! any signer with no proof and serves no join, and a consensus node does none of it.
use super::*;

use keyscheme::KeyScheme;

fn played(
    network: &str,
    anchor: Tip,
    signer: &ed25519::PrivateKey,
    steps: Vec<Step>,
) -> node::Body {
    node::Body {
        scheme: KeyScheme::Ed25519,
        signer: signer.public_key().as_ref().to_vec(),
        network: network.as_bytes().to_vec(),
        anchor,
        salt: None,
        target: "probe".to_owned(),
        payload: abi::encode(&steps),
        client: None,
    }
}

fn refusal_of(answer: noded::Result<Submitted>) -> String {
    match answer {
        Err(noded::Error::Refused(refusal)) => refusal.sentence,
        other => panic!("{other:?}"),
    }
}

fn status_of<T: std::fmt::Debug>(answer: noded::Result<T>) -> u16 {
    match answer {
        Err(noded::Error::Failed { status, .. }) => status,
        other => panic!("{other:?}"),
    }
}

fn loopback() -> Listeners {
    let tcp = || TcpListener::bind("127.0.0.1:0").unwrap();
    let udp = || UdpSocket::bind("127.0.0.1:0").unwrap();
    Listeners {
        p2p: tcp(),
        http: tcp(),
        tls: Some(tcp()),
        halo: udp(),
        live: Some(udp()),
    }
}

async fn route_status(client: &Client, route: &str) -> u16 {
    let url = format!("{}{route}", client.base());
    let answer = reqwest::Client::new().post(url).send().await.unwrap();
    answer.status().as_u16()
}

#[test]
fn a_consensus_node_serves_no_fork_route_and_refuses_a_frame_whose_proof_does_not_bind() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "validator");
    seat.init(&founding(root.path(), &[&seat], EPOCH_LENGTH));
    let live = seat.start();
    let runtime = client_runtime();
    runtime.block_on(async {
        let client = &live.client;
        let putter = ed25519::PrivateKey::from_seed(PUTTER_SEED);
        let anchor = client.status().await.unwrap().anchor();
        let body = played(NETWORK, anchor, &putter, vec![set(b"as", b"1")]);
        let unrouted = client.play(body.clone(), Preconfirm::Off).await;
        assert_eq!(status_of(unrouted), 404);
        assert_eq!(status_of(client.advance(10).await), 404);
        let unbound = Frame {
            body,
            proof: Vec::new(),
        };
        let refused = client.submit(unbound.encode(), Preconfirm::Off).await;
        assert_eq!(
            refusal_of(refused),
            "the proof does not bind the frame to its signer"
        );
    });
    shut_down(&runtime, &[&seat], vec![live]);
}

#[test]
fn a_fork_runs_alone_takes_an_op_as_any_signer_and_serves_no_join() {
    let root = tempfile::tempdir().unwrap();
    let parent = Seat::new(root.path(), "parent");
    parent.init(&founding(root.path(), &[&parent], EPOCH_LENGTH));
    let live = parent.start();
    let runtime = client_runtime();
    runtime.block_on(reach(&live.client, 3));
    shut_down(&runtime, &[&parent], vec![live]);

    let fork = Seat::new(root.path(), "fork");
    let (from, to) = (&parent.workspace, &fork.workspace);
    let forked = runner(&to.runtime_dir())
        .start(|context| async move { noded::fork(context, from, to).await.unwrap() });
    assert!(forked.height >= 3, "{forked:?}");
    let named = format!("{NETWORK}-fork-{}-", forked.height);
    assert!(forked.network.starts_with(&named), "{forked:?}");
    assert!(!fork.workspace.dir().join("anchor").exists());
    assert!(!fork.workspace.dir().join("egress.toml").exists());

    let refused = runner(&to.runtime_dir()).start(|context| async move {
        let logs = Logs::install("info").unwrap();
        let cadence = to
            .descriptor()
            .unwrap()
            .cadence(consensus::Timeouts::default())
            .unwrap();
        let started = noded::run_on(
            context,
            to,
            logs,
            listeners(),
            HaloConfig::default(),
            LoadConfig::default(),
            cadence,
        )
        .await;
        match started {
            Ok(_) => panic!("a fork started with its halo and live sockets off loopback"),
            Err(error) => error.to_string(),
        }
    });
    assert!(refused.contains("listens on loopback only"), "{refused}");
    assert!(refused.contains("halo "), "{refused}");

    *fork.held.lock().unwrap() = Some(loopback());
    let live = fork.start();
    runtime.block_on(async {
        let client = &live.client;
        let status = client.status().await.unwrap();
        assert_eq!(status.network, forked.network);
        reach(client, forked.height + 2).await;

        let target = client.status().await.unwrap().height + 2 * EPOCH_LENGTH;
        let reached = client.advance(target).await.unwrap();
        assert!(reached >= target, "{reached} < {target}");
        assert!(client.status().await.unwrap().height >= target);

        let putter = ed25519::PrivateKey::from_seed(PUTTER_SEED);
        let parents = frame(client, &putter, vec![set(b"parent", b"1")]).await;
        let refused = client.submit(parents, Preconfirm::Off).await;
        assert_eq!(refusal_of(refused), "the frame names another network");

        let anchor = client.status().await.unwrap().anchor();
        let body = played(&forked.network, anchor, &putter, vec![set(b"as", b"1")]);
        let (_, receipt) = client.land_played(&body).await.unwrap();
        assert!(
            matches!(receipt.outcome, Outcome::Applied { .. }),
            "{receipt:?}"
        );
        let value = client.get(Layer::Confirmed, "probe", b"as").await.unwrap();
        assert_eq!(value.as_deref(), Some(b"1".as_slice()));

        for route in [
            noded::wire::route::SYNC,
            noded::wire::route::SYNC_HOLD,
            noded::wire::route::SNAPSHOT,
        ] {
            assert_eq!(route_status(client, route).await, 404, "{route}");
        }

        let anchor = client.status().await.unwrap().anchor();
        let shutdown = Frame::sign(
            &fork.identity,
            forked.network.as_bytes(),
            anchor,
            None,
            noded::wire::ADMIN,
            abi::encode(&Admin::Shutdown),
        );
        client.admin(shutdown.encode()).await.unwrap();
    });
    live.thread.join().unwrap();
}
