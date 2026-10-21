// Bundles gathered over halo, its lanes, and the halo addresses members are dialed at.

use super::*;

#[test]
fn a_bundle_put_on_one_validator_is_gathered_over_quic_held_by_a_quorum_and_read_by_a_program() {
    let root = tempfile::tempdir().unwrap();
    let taker = Seat::new(root.path(), "n0");
    let gatherer = Seat::new(root.path(), "n1");
    let founding = founding(root.path(), &[&taker, &gatherer], EPOCH_LENGTH);
    taker.init(&founding);
    gatherer.init(&founding);
    let live = [taker.start(), gatherer.start()];
    let putter = ed25519::PrivateKey::from_seed(PUTTER_SEED);
    let alice = ed25519::PrivateKey::from_seed(11);
    let files = tempfile::tempdir().unwrap();
    let small = files.path().join("small");
    let big = files.path().join("big");
    std::fs::write(&small, b"a small page").unwrap();
    std::fs::write(&big, vec![7u8; 3 << 20]).unwrap();
    let uploads = || {
        [&small, &big]
            .into_iter()
            .map(|path| Upload {
                kind: "page".into(),
                path: path.clone(),
            })
            .collect::<Vec<_>>()
    };
    client_runtime().block_on(async {
        let client = &live[0].client;
        let status = client.status().await.unwrap();
        let role = status.roles.get(role::BLOBS).unwrap().clone();
        let entries = vec![
            blob::file_entry("page", &small).unwrap(),
            blob::file_entry("page", &big).unwrap(),
        ];
        let put = |blobs: Vec<Entry>, salt: u64| {
            Frame::sign(
                &putter,
                NETWORK.as_bytes(),
                status.anchor(),
                Some(salt),
                &role,
                abi::encode(&blob::contract::Op::Put { blobs }),
            )
        };

        let mut mislabelled = entries.clone();
        mislabelled[1].kind = "pack".into();
        let refused = client
            .put_blobs(&put(mislabelled, 1), uploads())
            .await
            .unwrap_err();
        let noded::Error::Refused(refusal) = refused else {
            panic!("an upload whose bytes are not the blobs its put names is refused: {refused}");
        };
        assert_eq!(refusal.reason, reason::INVALID_INPUT);

        let (_, receipt) = client
            .land_blobs(&put(entries.clone(), 2), uploads())
            .await
            .unwrap();
        let Outcome::Applied { output } = receipt.outcome else {
            panic!("the put applies: {receipt:?}");
        };
        let bundle: u64 = abi::decode(&output).unwrap();
        let gathered = &live[1].client;
        // the gatherer applies the block where both validators hold the bundle
        gathered.held(&role, bundle).await.unwrap();

        let ids: Vec<BlobId> = entries.iter().map(|entry| entry.id).collect();
        let framed = [
            blob::frame("page", b"a small page").unwrap(),
            blob::frame("page", &vec![7u8; 3 << 20]).unwrap(),
        ];
        assert_eq!(
            gathered.blobs(&ids).await.unwrap(),
            framed.clone().map(Some).to_vec()
        );
        let named_by_none = blob::id_of(HashKind::Sha256, b"page 1\0x").unwrap();
        assert_eq!(gathered.blobs(&[named_by_none]).await.unwrap(), vec![None]);
        let mut asked = ids.clone();
        asked.push(named_by_none);
        asked.push(ids[1]);
        let http = reqwest::Client::new();
        let answer_of = |at: &str| {
            let request = http
                .post(format!("{}{at}", gathered.base()))
                .body(abi::encode(&asked));
            async move {
                let response = request.send().await.unwrap();
                assert_eq!(response.status(), reqwest::StatusCode::OK);
                response.bytes().await.unwrap()
            }
        };
        let plain = answer_of(noded::wire::route::BLOBS).await;
        let packed = answer_of(noded::wire::route::BLOBS_ZSTD).await;
        assert_eq!(zstd::decode_all(packed.as_ref()).unwrap(), plain.to_vec());
        assert!(
            packed.len() < plain.len(),
            "{} of {}",
            packed.len(),
            plain.len()
        );

        let reading = frame(gathered, &alice, vec![Step::Op(HostOp::BlobGet(ids[0]))]).await;
        let (_, read) = gathered
            .land(&Frame::decode(&reading).unwrap())
            .await
            .unwrap();
        let Outcome::Applied { output } = read.outcome else {
            panic!("the probe reads the held blob: {read:?}");
        };
        let replies: Vec<Reply> = abi::decode(&output).unwrap();
        assert_eq!(
            replies,
            vec![Reply::Host(HostReply::Blob(Some(abi::Blob {
                kind: "page".into(),
                body: b"a small page".to_vec(),
            })))]
        );
        for (seat, node) in [(&taker, &live[0]), (&gatherer, &live[1])] {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in live {
        node.thread.join().unwrap();
    }
}

/// Founded with no lane: the block that votes blobs' lane open has both validators serve it, so a
/// later put is gathered over it and held.
#[test]
fn a_lane_the_validators_vote_open_is_served_from_its_block() {
    let root = tempfile::tempdir().unwrap();
    let taker = Seat::new(root.path(), "n0");
    let gatherer = Seat::new(root.path(), "n1");
    let founding = founding(root.path(), &[&taker, &gatherer], EPOCH_LENGTH);
    let unlaned = halo::Genesis { lanes: Vec::new() };
    std::fs::write(root.path().join("halo.params"), abi::encode(&unlaned)).unwrap();
    taker.init(&founding);
    gatherer.init(&founding);
    let live = vec![taker.start(), gatherer.start()];
    let page = root.path().join("page");
    std::fs::write(&page, b"gathered once the lane is open").unwrap();
    let runtime = client_runtime();
    runtime.block_on(async {
        let client = &live[0].client;
        assert_eq!(lanes(client, &taker.identity).await, []);
        let open = halo::Op::Open {
            name: blob::contract::LANE.into(),
            cap: halo::Class::Critical,
        };
        let motion = Motion::Role {
            role: role::HALO.into(),
            op: abi::encode(&open),
        };
        let height = membership(client, &taker.identity, Op::Vote(motion)).await;
        let opened = halo::Lane {
            name: blob::contract::LANE.into(),
            id: 0,
            cap: halo::Class::Critical,
            enabled: true,
            pinned: false,
            since: height,
        };
        assert_eq!(lanes(client, &taker.identity).await, [opened]);

        let (id, bundle) = put_page(client, &page, 1).await;
        let role = client
            .status()
            .await
            .unwrap()
            .roles
            .bound(role::BLOBS)
            .unwrap()
            .clone();
        // the gatherer gathers and receipts the bundle over the lane the vote opened
        live[1].client.held(&role, bundle).await.unwrap();
        let framed = blob::frame("page", b"gathered once the lane is open").unwrap();
        assert_eq!(
            live[1].client.blobs(&[id]).await.unwrap(),
            vec![Some(framed)]
        );
    });
    shut_down(&runtime, &[&taker, &gatherer], live);
}

async fn dialed_self(client: &Client) -> noded::wire::HaloSelf {
    let mut blocks = client.changes(host::RECENT).await.unwrap();
    loop {
        let reach = client.halo_self().await.unwrap();
        if !matches!(reach, noded::wire::HaloSelf::Dialing { .. }) {
            return reach;
        }
        blocks.next().await.unwrap().unwrap();
    }
}

#[test]
fn a_node_reports_whether_it_reaches_itself_at_its_registered_halo_address() {
    let root = tempfile::tempdir().unwrap();
    let reachable = Seat::new(root.path(), "n0");
    let unreachable = Seat::unreachable(root.path(), "n1");
    let founding = founding(root.path(), &[&reachable, &unreachable], EPOCH_LENGTH);
    reachable.init(&founding);
    unreachable.init(&founding);
    let live = vec![reachable.start(), unreachable.start()];
    let runtime = client_runtime();
    runtime.block_on(async {
        let reached = dialed_self(&live[0].client).await;
        let at = reachable.halo_address();
        assert_eq!(reached, noded::wire::HaloSelf::Reached { at });
        let unreached = dialed_self(&live[1].client).await;
        let at = unreachable.halo_address();
        let reason = "undialed".to_string();
        assert_eq!(unreached, noded::wire::HaloSelf::Unreachable { at, reason });
        let lines = live[1].client.logs().await.unwrap();
        let errors = |at: String| {
            let named = format!("at={at}");
            lines
                .iter()
                .filter(|line| line.contains("halo_unreachable"))
                .filter(|line| line.split_whitespace().any(|word| word == named))
                .count()
        };
        assert_eq!(errors(unreachable.halo_address()), 1, "{lines:?}");
        assert_eq!(errors(reachable.halo_address()), 0, "{lines:?}");
    });
    shut_down(&runtime, &[&reachable, &unreachable], live);
}

#[test]
fn a_put_through_a_member_no_one_can_dial_is_gathered_over_the_connections_it_dials_and_held() {
    let root = tempfile::tempdir().unwrap();
    let first = Seat::new(root.path(), "n0");
    let second = Seat::new(root.path(), "n1");
    let hidden = Seat::unreachable(root.path(), "n2");
    let founding = founding(root.path(), &[&first, &second, &hidden], EPOCH_LENGTH);
    for seat in [&first, &second, &hidden] {
        seat.init(&founding);
    }
    let live = vec![first.start(), second.start(), hidden.start()];
    let page = root.path().join("page");
    std::fs::write(&page, b"held through a hidden member").unwrap();
    let runtime = client_runtime();
    runtime.block_on(async {
        let (id, bundle) = put_page(&live[2].client, &page, 1).await;
        let role = live[2]
            .client
            .status()
            .await
            .unwrap()
            .roles
            .get(role::BLOBS)
            .unwrap()
            .clone();
        // both members that can be dialed gather the bundle from the one that cannot
        live[2].client.held(&role, bundle).await.unwrap();
        let framed = blob::frame("page", b"held through a hidden member").unwrap();
        for gathered in &live[..2] {
            // the gatherer applied the block that holds the bundle
            gathered.client.held(&role, bundle).await.unwrap();
            assert_eq!(
                gathered.client.blobs(&[id]).await.unwrap(),
                vec![Some(framed.clone())]
            );
        }
    });
    shut_down(&runtime, &[&first, &second, &hidden], live);
}

#[test]
fn a_member_that_moves_its_halo_address_is_dialed_there_within_the_epoch() {
    const EPOCH: u64 = 400;
    let root = tempfile::tempdir().unwrap();
    let moving = Seat::unreachable(root.path(), "n0");
    let hidden = Seat::unreachable(root.path(), "n1");
    let founding = founding(root.path(), &[&moving, &hidden], EPOCH);
    moving.init(&founding);
    hidden.init(&founding);
    let live = vec![moving.start(), hidden.start()];
    let page = root.path().join("page");
    std::fs::write(&page, b"gathered at the new address").unwrap();
    let runtime = client_runtime();
    runtime.block_on(async {
        let client = &live[0].client;
        let (id, bundle) = put_page(client, &page, 1).await;
        let moved = membership(
            client,
            &moving.identity,
            Op::SetHalo {
                halo: moving.halo.to_string(),
            },
        )
        .await;
        let role = client
            .status()
            .await
            .unwrap()
            .roles
            .get(role::BLOBS)
            .unwrap()
            .clone();
        // the hidden member gathers the bundle once it dials the moved address
        let since = client.held(&role, bundle).await.unwrap();
        assert_eq!(
            since / EPOCH,
            moved / EPOCH,
            "the move landed at {moved} and the bundle was held at {since}, past the epoch's end"
        );
        // the gatherer applied the block that holds the bundle
        live[1].client.held(&role, bundle).await.unwrap();
        let framed = blob::frame("page", b"gathered at the new address").unwrap();
        assert_eq!(
            live[1].client.blobs(&[id]).await.unwrap(),
            vec![Some(framed)]
        );
    });
    shut_down(&runtime, &[&moving, &hidden], live);
}

#[test]
fn members_no_one_can_reach_over_udp_link_over_halo_carried_on_one_members_http_and_gather() {
    let root = tempfile::tempdir().unwrap();
    let mut served = Seat::unreachable(root.path(), "n0");
    let mut hidden = Seat::unreachable(root.path(), "n1");
    served.address = Some(format!("ws://localhost:{}", served.http.port()));
    hidden.address = None;
    let founding = founding(root.path(), &[&served, &hidden], EPOCH_LENGTH);
    served.init(&founding);
    hidden.init(&founding);
    let live = vec![served.start(), hidden.start()];
    let page = root.path().join("page");
    std::fs::write(&page, b"gathered over a relayed halo link").unwrap();
    let runtime = client_runtime();
    runtime.block_on(async {
        let client = &live[0].client;
        let (id, bundle) = put_page(client, &page, 1).await;
        let role = client
            .status()
            .await
            .unwrap()
            .roles
            .get(role::BLOBS)
            .unwrap()
            .clone();
        client.held(&role, bundle).await.unwrap();
        live[1].client.held(&role, bundle).await.unwrap();
        let framed = blob::frame("page", b"gathered over a relayed halo link").unwrap();
        assert_eq!(
            live[1].client.blobs(&[id]).await.unwrap(),
            vec![Some(framed)]
        );
    });
    shut_down(&runtime, &[&served, &hidden], live);
}
