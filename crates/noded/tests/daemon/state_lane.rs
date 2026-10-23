// A node serves its newest epoch close on halo's ducktape/state lane, and a joiner adopts it.

use super::*;

#[test]
fn a_joiner_adopts_the_epoch_close_a_node_serves_on_the_state_lane() {
    apart!();
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
    let written = runtime.block_on(async {
        let through = &live[0].client;
        let mut changes = through.changes("probe").await.unwrap();
        through
            .submit(
                frame(through, &alice, vec![set(b"a", b"1")]).await,
                Preconfirm::Off,
            )
            .await
            .unwrap();
        let written = changes.next().await.unwrap().unwrap().height;
        let open = halo::Op::Open {
            name: statesync::lane::LANE.into(),
            cap: halo::Class::Member,
        };
        let motion = Motion::Role {
            role: role::HALO.into(),
            op: abi::encode(&open),
        };
        let opened = membership(through, &seats[0].identity, Op::Vote(motion)).await;
        for node in &live {
            reach(&node.client, opened).await;
        }
        written
    });

    let client = live[0].client.clone();
    let serving = seats[0].identity.public_key();
    let joiner = seats[3].identity.clone();
    let trusted = statesync::lane::Trusted {
        network: NETWORK.as_bytes().to_vec(),
        epoch: 0,
        validators: seats.iter().map(Seat::key).collect(),
    };
    let space = Workspace::at(root.path().join("joiner"));
    let dir = space.dir().to_path_buf();
    std::fs::create_dir_all(&dir).unwrap();
    let adopted = runner(&space.runtime_dir()).start(|context| async move {
        let status = client.status().await.unwrap();
        let seating = noded::seating(&client, &joiner, &status).await.unwrap();
        let config = ::halo::Config {
            egress_bytes_per_sec: None,
            udp: UdpSocket::bind((outbound_ip(), 0)).unwrap().into(),
            queues: ::halo::Queues::default(),
            reach: ::halo::Https::shared(),
        };
        let halo = Arc::new(::halo::Halo::start(&context, joiner, config).unwrap());
        halo.seat(seating.seats);
        halo.plane().lanes(seating.lanes);
        let source = noded::state_lane::Source::new(halo, serving).unwrap();
        let joined = statesync::bulk_join(
            context.child("join"),
            NETWORK,
            &dir,
            trusted,
            std::time::Duration::from_millis(BLOCK_TIME_MS),
            source,
        )
        .await
        .unwrap();
        joined.node.tip().unwrap().height
    });
    assert!(
        adopted >= written,
        "the close adopted, {adopted}, is past the write at {written}"
    );
    assert_eq!(
        (adopted + 1) % EPOCH_LENGTH,
        0,
        "a joiner adopts an epoch's closing block"
    );
    shut_down(&runtime, &seats[..3].iter().collect::<Vec<_>>(), live);
}
