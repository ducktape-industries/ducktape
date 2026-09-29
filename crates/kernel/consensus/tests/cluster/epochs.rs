use super::*;

#[test]
fn a_follower_backfills_finalized_blocks_by_hint() {
    runner().start(|context| async move {
        let keys: Vec<_> = (1..=4).map(key).collect();
        let members: Vec<_> = keys[..3].iter().map(member).collect();
        let oracle = mesh(&context, &keys).await;
        let network = network(1_000);
        let mut peers = validators(&context, &oracle, &keys[..3], &members, &network).await;
        let alice = key(11);
        peers[0]
            .submit(frame(&alice, 0, vec![set(b"seen", b"by-followers")]))
            .await;
        let tip = peers[0].reached(5).await;

        let mut follower = Peer::spawn(
            context.child(LABELS[3]),
            LABELS[3],
            &oracle,
            keys[3].clone(),
            &members,
            network.clone(),
        )
        .await;
        assert!(!follower.roster.participates(0));
        follower.marshal.hint(tip, NonEmptyVec::new(peers[0].me()));
        follower.reached(tip).await;
        assert_eq!(follower.root_at(tip), peers[0].root_at(tip));
        assert_eq!(
            follower.confirmed("probe", b"seen").await,
            Some(b"by-followers".to_vec())
        );
    });
}

#[test]
fn a_validator_epochs_behind_catches_up_by_the_traffic_it_hears() {
    runner().start(|context| async move {
        let keys: Vec<_> = (1..=4).map(key).collect();
        let members: Vec<_> = keys.iter().map(member).collect();
        let oracle = mesh(&context, &keys).await;
        let network = network(4);
        let mut peers = validators(&context, &oracle, &keys[..3], &members, &network).await;
        let alice = key(11);
        peers[0]
            .submit(frame(&alice, 0, vec![set(b"early", b"yes")]))
            .await;
        let ahead = peers[0].reached(11).await;

        let mut late = Peer::spawn(
            context.child(LABELS[3]),
            LABELS[3],
            &oracle,
            keys[3].clone(),
            &members,
            network.clone(),
        )
        .await;
        assert!(late.roster.participates(0));
        let caught_up = late.reached(ahead).await;
        assert_eq!(late.root_at(ahead), peers[0].root_at(ahead));
        assert!(caught_up >= ahead);
        assert_eq!(
            late.confirmed("probe", b"early").await,
            Some(b"yes".to_vec())
        );
        assert!(late.roster.participates(network.epoch_after(ahead)));
    });
}

/// `follower` applies `height` before `source` closes that height's epoch:
/// it follows at the tip, not an epoch behind.
async fn follows_at_the_tip(follower: &mut Peer, source: &Peer, height: u64, network: &Network) {
    follower.reached(height).await;
    let tip = source.marshal.processed().await.unwrap();
    let closing = network.anchor(height / network.epoch_length + 1);
    assert!(
        tip < closing,
        "reached {height} only once the validators were at {tip}"
    );
}

/// The boundary seats the four validators; the fifth key is only a member
/// (a resident): no roster seats it, so it never votes and the leader
/// rotation never reaches it. Unhinted, it follows the chain at the tip on
/// the finalizations it hears, as the fourth key does before its seat.
#[test]
fn an_epoch_boundary_reseats_the_validators() {
    runner().start(|context| async move {
        let keys: Vec<_> = (1..=5).map(key).collect();
        let founding: Vec<_> = keys[..3].iter().map(member).collect();
        let seated: Vec<_> = keys[..4].iter().map(member).collect();
        let members: Vec<_> = keys.iter().map(member).collect();
        let seats: Vec<_> = seated.iter().map(|member| member.key.clone()).collect();
        let public: Vec<_> = keys.iter().map(|k| k.public_key()).collect();
        let resident = public[4].clone();
        let oracle = mesh(&context, &keys).await;
        let network = network(4);
        let mut peers = validators(&context, &oracle, &keys, &founding, &network).await;
        assert!(!peers[3].roster.participates(0));

        let alice = key(11);
        peers[0]
            .submit(frame(&alice, 0, vec![set(b"epoch", b"0")]))
            .await;
        let valset = abi::encode(&(&seated, vec![member(&keys[4])]));
        peers[0]
            .submit(Frame::sign(&alice, NETWORK, 1, "valset", valset).encode())
            .await;

        let (founders, followers) = peers.split_at_mut(3);
        for follower in followers {
            follows_at_the_tip(follower, &founders[0], 1, &network).await;
        }
        for peer in &mut peers {
            peer.reached(3).await;
        }
        for peer in &peers {
            let node = peer.node.lock().await;
            assert_eq!(node.epoch_members(1).unwrap(), Some(members.clone()));
            assert_eq!(node.epoch_validators(1).unwrap(), Some(seats.clone()));
            assert_eq!(peer.roster.seated(1), consensus::validators_of(&seats));
            assert_eq!(peer.roster.participates(1), peer.me() != resident);
        }

        let silenced = peers[2].me();
        sever(&oracle, &silenced, &public).await;
        peers[0]
            .submit(frame(&alice, 2, vec![set(b"epoch", b"1")]))
            .await;
        let (voters, residents) = peers.split_at_mut(4);
        follows_at_the_tip(&mut residents[0], &voters[0], 5, &network).await;

        let mut tips = Vec::new();
        for peer in peers.iter_mut().filter(|peer| peer.me() != silenced) {
            tips.push(peer.reached(9).await);
        }
        let common = *tips.iter().min().unwrap();
        let root = peers[0].root_at(common);
        for peer in peers.iter().filter(|peer| peer.me() != silenced) {
            assert_eq!(peer.root_at(common), root);
            assert_eq!(peer.confirmed("probe", b"epoch").await, Some(b"1".to_vec()));
        }
        let boundary = peers[3].marshal.certificate(3).await.unwrap();
        let after = peers[3].marshal.certificate(4).await.unwrap();
        let next = peers[3].marshal.certificate(8).await.unwrap();
        assert_eq!(boundary.round().epoch().get(), 0);
        assert_eq!(after.round().epoch().get(), 1);
        assert_eq!(next.round().epoch().get(), 2);
    });
}
