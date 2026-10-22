//! A node that caught up by backfill and stopped mid-epoch, on a tip it holds without
//! a finalization of its own, seats again when it restarts, and a validator signs no view twice.
//! Of the validator's two checks, the view after replay is the one shown to fail on a lost journal.
use commonware_p2p::Receiver as _;
use commonware_runtime::Metrics as _;
use consensus::channel;

use super::*;

impl Peer {
    async fn restart(
        self,
        release: oneshot::Sender<()>,
        context: Ctx,
        name: &'static str,
        oracle: &Oracle<ed25519::PublicKey, Ctx>,
        network: &Network,
    ) -> Peer {
        let Peer {
            key,
            node,
            pool,
            marshal,
            stalls,
            tamper,
            roots,
            skewed,
            carried,
            pump,
            _dir,
            ..
        } = self;
        drop(release);
        let _ = pump.await;
        marshal.stop().await;

        let (tip, root, recorded) = {
            let node = node.lock().await;
            let tip = node.tip().unwrap();
            let epoch = network.epoch_after(tip.height);
            let recorded: Vec<_> = (0..=epoch)
                .map(|epoch| node.epoch_validators(epoch).unwrap().unwrap())
                .collect();
            (tip, node.host().root().unwrap(), recorded)
        };
        let (current, earlier) = recorded.split_last().unwrap();
        let roster = Roster::new(NETWORK.to_vec(), Some(key.clone()));
        for (epoch, validators) in earlier.iter().enumerate() {
            roster.seat(epoch as u64, consensus::validators_of(validators).unwrap());
        }
        let mesh = SimMesh::new(
            oracle.clone(),
            key.public_key(),
            Quota::per_second(NZU32!(1024)),
        );
        let (inbox, deliveries) = mpsc::unbounded();
        let (applied, heights) = mpsc::unbounded();
        let (proposals, proposed) = mpsc::unbounded();
        let (tampering, tampered) = mpsc::unbounded();
        let chain = Participant {
            pool: pool.clone(),
            inbox,
            proposed: proposals,
            tamper: tamper.clone(),
            tampered: tampering,
        };
        let marshal = Marshal::start(
            context.child("marshal"),
            name,
            network,
            roster.clone(),
            Anchor::Genesis(Block::genesis(NETWORK, TIME)),
            Transport {
                me: key.public_key(),
                provider: mesh.provider(),
                blocker: mesh.blocker(),
                lanes: mesh.marshal_lanes().await,
            },
            chain.clone(),
        )
        .await
        .unwrap();
        let mux = EngineMux::start(
            context.child("lanes"),
            mesh.engine_channels().await,
            mesh.blocker(),
        );
        let mut membership = Membership::new(
            context.child("membership"),
            name.to_owned(),
            network.clone(),
            roster.clone(),
            mux,
            &marshal,
            chain,
        );
        membership
            .seat(tip, root, current)
            .await
            .expect("a node restarted on a backfilled tip seats");
        let pump = context.child("pump").spawn({
            let pump = Pump {
                node: node.clone(),
                pool: pool.clone(),
                applied,
                stalls: stalls.clone(),
                roots: roots.clone(),
                skewed: skewed.clone(),
                carried: carried.clone(),
                network: network.clone(),
            };
            move |_| pump.run(membership, deliveries)
        });
        Peer {
            key,
            node,
            pool,
            roster,
            marshal,
            heights,
            proposed,
            stalls,
            tamper,
            tampered,
            roots,
            skewed,
            carried,
            pump,
            _dir,
        }
    }
}

fn voter_view(context: &Ctx, node: &str, epoch: u64) -> u64 {
    let metrics = context.encode();
    let label = format!("epoch=\"{epoch}\"");
    let views: Vec<&str> = metrics
        .lines()
        .filter(|line| line.starts_with(node))
        .filter(|line| line.contains("_voter_state_current_view{"))
        .filter(|line| line.contains(&label))
        .collect();
    let [line] = views.as_slice() else {
        let gauges: Vec<&str> = metrics
            .lines()
            .filter(|line| line.contains("current_view"))
            .collect();
        panic!("no single {node} voter view in epoch {epoch}: {gauges:#?}");
    };
    line.rsplit(' ').next().unwrap().parse().unwrap()
}

#[test]
fn a_follower_restarted_mid_epoch_on_a_tip_it_backfilled_seats_and_follows() {
    runner().start(|context| async move {
        let network = network(8);
        let keys: Vec<_> = (1..=4).map(key).collect();
        let members: Vec<_> = keys[..3].iter().map(member).collect();
        let oracle = mesh(&context, &keys).await;
        let mut peers = validators(&context, &oracle, &keys[..3], &members, &network).await;
        peers[0].reached(network.first(3)).await;

        let follower = Peer::spawn(
            context.child(LABELS[3]),
            LABELS[3],
            &oracle,
            keys[3].clone(),
            &members,
            network.clone(),
        )
        .await;
        let stopped = network.first(3) + 2;
        let (held, release) = follower.stall_at(stopped + 1);
        held.await.unwrap();
        let unfinalized = follower.marshal.certificate(stopped).await.is_none();
        assert!(
            unfinalized,
            "the follower holds a finalization at {stopped}"
        );

        let mut follower = follower
            .restart(release, context.child("r3"), LABELS[3], &oracle, &network)
            .await;
        let next = stopped + 3;
        follower.reached(next).await;
        peers[0].reached(next).await;
        assert_eq!(follower.root_at(next), peers[0].root_at(next));
    });
}

#[test]
fn a_validator_restarted_mid_epoch_on_a_tip_it_backfilled_seats_and_signs_no_view_again() {
    runner().start(|context| async move {
        let network = network(16);
        let epoch = 2;
        let keys: Vec<_> = (1..=4).map(key).collect();
        let observer = key(9);
        let everyone: Vec<_> = keys.iter().chain([&observer]).cloned().collect();
        let members: Vec<_> = keys.iter().map(member).collect();
        let seated: Vec<_> = keys.iter().map(|key| key.public_key()).collect();
        let oracle = mesh(&context, &everyone).await;
        sever(&oracle, &observer.public_key(), &seated).await;
        let mut peers = validators(&context, &oracle, &keys, &members, &network).await;
        let late = peers[3].me();
        let quiet = peers[2].me();
        let voting = [peers[0].me(), peers[1].me()];
        sever(&oracle, &late, &seated).await;
        peers[0].reached(network.first(epoch) + 1).await;
        let lead = peers[0].marshal.processed().await.unwrap();
        let stopped = lead + 2;
        let mid_epoch = network.epoch_after(stopped) == epoch;
        assert!(mid_epoch, "the validators left epoch {epoch} at {lead}");

        let (held, release) = peers[3].stall_at(stopped + 1);
        heal(&oracle, &late, &seated).await;
        held.await.unwrap();
        sever(
            &oracle,
            &quiet,
            &[voting[0].clone(), voting[1].clone(), late.clone()],
        )
        .await;
        let unfinalized = peers[3].marshal.certificate(stopped).await.is_none();
        assert!(
            unfinalized,
            "the validator holds a finalization at {stopped}"
        );
        let tip = peers[0].marshal.processed().await.unwrap();
        let signed = tip + 2;
        let closing = network.anchor(epoch + 1);
        assert!(
            signed < closing,
            "the validators left epoch {epoch} at {tip}"
        );
        peers[0].reached(signed).await;

        sever(&oracle, &late, &voting).await;
        let before = voter_view(&context, LABELS[3], epoch);
        let (_votes, mut heard) = oracle
            .control(observer.public_key())
            .register(channel::VOTE, Quota::per_second(NZU32!(1024)))
            .await
            .unwrap();
        oracle
            .add_link(late.clone(), observer.public_key(), link())
            .await
            .unwrap();
        let restarted = peers
            .pop()
            .unwrap()
            .restart(release, context.child("r3"), LABELS[3], &oracle, &network)
            .await;
        loop {
            let (from, _) = heard.recv().await.unwrap();
            if from == late {
                break;
            }
        }
        let after = voter_view(&context, "r3", epoch);
        assert!(
            after >= before,
            "the restarted validator is at view {after}, below the {before} it reached"
        );

        heal(&oracle, &late, &voting).await;
        peers.push(restarted);
        let next = signed + 3;
        peers[0].reached(next).await;
        peers[3].reached(next).await;
        let blocked = oracle.blocked().await.unwrap();
        let equivocated = blocked.iter().any(|(_, to)| *to == late);
        assert!(
            !equivocated,
            "a validator blocked the restarted one: {blocked:?}"
        );
    });
}
