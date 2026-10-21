use commonware_consensus::simplex::elector::{Config, Elector as _, RoundRobin};
use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_consensus::types::{Epoch, Round, View};
use commonware_cryptography::Sha256;
use commonware_utils::ordered::Set;

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
            .submit(frame(&alice, vec![set(b"seen", b"by-followers")]))
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
            .submit(frame(&alice, vec![set(b"early", b"yes")]))
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

async fn every_epoch_opens_on_the_root_it_closed_on(peer: &Peer, network: &Network, tip: u64) {
    for height in 1..=tip {
        let block = peer.marshal.block(height).await.unwrap();
        let records: Vec<(usize, Settled)> = block.records().collect();
        let opens = height % network.epoch_length == 0;
        let expected = match opens {
            true => vec![(
                block.frames.len().saturating_sub(1),
                Settled {
                    height: height - 1,
                    root: peer.root_at(height - 1).0,
                },
            )],
            false => Vec::new(),
        };
        assert_eq!(records, expected, "block {height}");
    }
}

#[test]
fn each_epoch_opens_on_the_root_its_last_epoch_closed_on() {
    runner().start(|context| async move {
        let keys: Vec<_> = (1..=3).map(key).collect();
        let members: Vec<_> = keys.iter().map(member).collect();
        let oracle = mesh(&context, &keys).await;
        let network = network(4);
        let mut peers = validators(&context, &oracle, &keys, &members, &network).await;
        let alice = key(11);
        peers[0]
            .submit(frame(&alice, vec![set(b"settled", b"yes")]))
            .await;
        let tip = peers[0].reached(network.first(3)).await;
        every_epoch_opens_on_the_root_it_closed_on(&peers[0], &network, tip).await;
    });
}

#[test]
fn validators_refuse_an_epoch_opening_on_a_root_they_never_reached() {
    runner().start(|context| async move {
        let keys: Vec<_> = (1..=4).map(key).collect();
        let members: Vec<_> = keys.iter().map(member).collect();
        let oracle = mesh(&context, &keys).await;
        let network = network(4);
        let mut peers = validators(&context, &oracle, &keys, &members, &network).await;
        peers[3].skew_root();

        let opened = peers[3].proposes_an_opening(&network).await;
        let tip = peers[0].reached(opened + 1).await;
        every_epoch_opens_on_the_root_it_closed_on(&peers[0], &network, tip).await;
    });
}

#[test]
fn validators_refuse_an_opening_with_no_record_once_the_last_one_carried_one() {
    runner().start(|context| async move {
        let keys: Vec<_> = (1..=4).map(key).collect();
        let members: Vec<_> = keys.iter().map(member).collect();
        let oracle = mesh(&context, &keys).await;
        let network = network(4);
        let mut peers = validators(&context, &oracle, &keys, &members, &network).await;
        peers[3].tamper(Tamper::Unseal {
            from: network.first(2),
        });

        let unsealed = peers[3].tampered().await;
        let tip = peers[0].reached(unsealed + 1).await;
        every_epoch_opens_on_the_root_it_closed_on(&peers[0], &network, tip).await;
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
            .submit(frame(&alice, vec![set(b"epoch", b"0")]))
            .await;
        let promote = validators::Op::Vote(validators::Motion::Promote {
            key: member(&keys[3]).key,
        });
        for frame in [enroll(&keys[3]), enroll(&keys[4]), valset(&alice, &promote)] {
            peers[0].submit(frame).await;
        }

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
            .submit(frame(&alice, vec![set(b"epoch", b"1")]))
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

#[test]
fn the_new_epochs_first_view_finalizes_while_its_newcomer_is_cut_off() {
    runner().start(|context| async move {
        let network = network(8);
        let epoch = 1;
        let mut keys: Vec<_> = (1..=4).map(key).collect();
        keys.sort_by_key(|key| key.public_key());
        let seats =
            Set::try_from(keys.iter().map(|key| key.public_key()).collect::<Vec<_>>()).unwrap();
        let first_view = Round::new(Epoch::new(epoch), View::new(1));
        let round_robin = Config::<Scheme>::build(RoundRobin::<Sha256>::default(), &seats);
        let first_leader = round_robin.elect(first_view, None);
        let newcomer = keys.remove(first_leader.get() as usize);
        let founders = keys;
        let everyone: Vec<_> = founders.iter().chain([&newcomer]).cloned().collect();
        let public: Vec<_> = everyone.iter().map(|key| key.public_key()).collect();
        let members: Vec<_> = founders.iter().map(member).collect();
        let oracle = mesh(&context, &everyone).await;
        let mut peers = validators(&context, &oracle, &founders, &members, &network).await;
        let mut joiner = Peer::spawn(
            context.child(LABELS[3]),
            LABELS[3],
            &oracle,
            newcomer.clone(),
            &members,
            network.clone(),
        )
        .await;

        let alice = key(11);
        let promote = validators::Op::Vote(validators::Motion::Promote {
            key: member(&newcomer).key,
        });
        for frame in [enroll(&newcomer), valset(&alice, &promote)] {
            peers[0].submit(frame).await;
        }
        let closing = network.anchor(epoch);
        joiner.reached(closing - 2).await;
        sever(&oracle, &joiner.me(), &public).await;
        for peer in &mut peers {
            peer.reached(closing + 1).await;
        }
        heal(&oracle, &joiner.me(), &public).await;

        let seated = peers[0].node.lock().await.epoch_validators(epoch).unwrap();
        assert_eq!(
            seated.as_deref().and_then(consensus::validators_of),
            Some(seats.clone())
        );
        let opening = peers[0].marshal.certificate(closing + 1).await.unwrap();
        assert_eq!(opening.round(), first_view, "the first view was nullified");
        let previous = peers[0].roster.seated(epoch - 1);
        let proposer = consensus::proposer(&opening, &seats, previous.as_ref()).unwrap();
        assert!(
            founders
                .iter()
                .any(|founder| founder.public_key() == proposer)
        );

        joiner.reached(closing + 2).await;
        assert_eq!(joiner.root_at(closing + 1), peers[0].root_at(closing + 1));
        assert!(joiner.roster.participates(epoch));
    });
}
