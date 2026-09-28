//! A validator whose votes always land after the quorum is in no
//! finalization certificate, yet the others' vote books count it.

use std::sync::Arc;
use std::time::Duration;

use commonware_cryptography::{Digestible as _, Signer as _, ed25519};
use commonware_p2p::simulated::{self, Link, Oracle};
use commonware_runtime::{Quota, Runner as _, Spawner as _, Supervisor as _, deterministic};
use commonware_utils::Acknowledgement as _;
use commonware_utils::acknowledgement::Exact;
use commonware_utils::{NZU32, NZUsize};
use consensus::{
    Anchor, Cadence, Chain, EngineMux, Marshal, Membership, Network, Roster, SimMesh, Transport,
    Votes,
};
use futures::StreamExt as _;
use futures::channel::mpsc;
use node::Block;

type Ctx = deterministic::Context;

const NETWORK: &[u8] = b"votes";
const LABELS: [&str; 4] = ["v0", "v1", "v2", "v3"];

fn link(latency: Duration) -> Link {
    Link {
        latency,
        jitter: Duration::from_millis(1),
        success_rate: commonware_utils::probability!(1.0),
    }
}

/// A chain of empty blocks; each finalized one goes to the pump.
#[derive(Clone)]
struct Empty(mpsc::UnboundedSender<(Arc<Block>, Exact)>);

impl Chain for Empty {
    async fn propose(&mut self, parent: Arc<Block>, time: u64) -> Option<Block> {
        Some(Block::next(parent.tip(), time, Vec::new()))
    }

    fn deliver(&mut self, block: Arc<Block>, ack: Exact) {
        let _ = self.0.unbounded_send((block, ack));
    }
}

struct Peer {
    marshal: Marshal,
    votes: Votes,
    heights: mpsc::UnboundedReceiver<u64>,
}

impl Peer {
    async fn spawn(
        context: Ctx,
        name: &str,
        oracle: &Oracle<ed25519::PublicKey, Ctx>,
        key: &ed25519::PrivateKey,
        validators: &[Vec<u8>],
    ) -> Peer {
        let network = Network {
            epoch_length: 1_000,
            cadence: Cadence::from_millis(1_000),
        };
        let genesis = Block::genesis(NETWORK, 0);
        let roster = Roster::new(NETWORK.to_vec(), Some(key.clone()));
        let mesh = SimMesh::new(
            oracle.clone(),
            key.public_key(),
            Quota::per_second(NZU32!(1024)),
        );
        let (inbox, mut deliveries) = mpsc::unbounded();
        let chain = Empty(inbox);
        let (marshal, _receipts) = Marshal::start(
            context.child("marshal"),
            name,
            &network,
            roster.clone(),
            Anchor::Genesis(genesis.clone()),
            Transport {
                me: key.public_key(),
                provider: mesh.provider(),
                blocker: mesh.blocker(),
                lanes: mesh.marshal_lanes().await,
            },
            chain.clone(),
        )
        .await;
        let mux = EngineMux::start(
            context.child("lanes"),
            mesh.engine_channels().await,
            mesh.blocker(),
        );
        let mut membership = Membership::new(
            context.child("membership"),
            name.to_owned(),
            network,
            roster,
            mux,
            &marshal,
            chain,
        );
        membership.seat(genesis.tip(), validators).await.unwrap();
        let votes = membership.votes().clone();
        let (applied, heights) = mpsc::unbounded();
        context.child("pump").spawn({
            let votes = votes.clone();
            move |_| async move {
                let _seated = membership;
                while let Some((block, ack)) = deliveries.next().await {
                    votes.applied(block.digest(), block.height);
                    ack.acknowledge();
                    let _ = applied.unbounded_send(block.height);
                }
            }
        });
        Peer {
            marshal,
            votes,
            heights,
        }
    }

    async fn reached(&mut self, target: u64) -> u64 {
        loop {
            let height = self.heights.next().await.expect("the pump lives");
            if height >= target {
                return height;
            }
        }
    }
}

#[test]
fn a_validator_voting_after_the_quorum_is_counted() {
    deterministic::Runner::timed(Duration::from_secs(600)).start(|context| async move {
        let mut keys: Vec<_> = (1..=4).map(ed25519::PrivateKey::from_seed).collect();
        keys.sort_by_key(|key| key.public_key());
        let public: Vec<_> = keys.iter().map(|key| key.public_key()).collect();
        let validators: Vec<_> = public.iter().map(|key| key.as_ref().to_vec()).collect();
        let (network, oracle) = simulated::Network::new_with_peers(
            context.child("network"),
            simulated::Config {
                max_size: 1 << 20,
                disconnect_on_block: true,
                max_peers_per_set: NZUsize!(16),
                tracked_peer_sets: NZUsize!(1),
            },
            public.clone(),
        )
        .await;
        network.start();
        // every vote the last validator sends lands 300 ms late: well after
        // the other three make the quorum, well inside a 1 s block
        let late = 3;
        for (from, a) in public.iter().enumerate() {
            for b in public.iter().filter(|b| *b != a) {
                let latency = if from == late { 300 } else { 10 };
                let link = link(Duration::from_millis(latency));
                oracle.add_link(a.clone(), b.clone(), link).await.unwrap();
            }
        }
        let mut peers = Vec::new();
        for (i, key) in keys.iter().enumerate() {
            let context = context.child(LABELS[i]);
            peers.push(Peer::spawn(context, LABELS[i], &oracle, key, &validators).await);
        }

        let tip = peers[0].reached(8).await;
        let heights = peers[0].votes.heights();
        for (i, key) in validators.iter().enumerate() {
            let counted = heights.get(key).copied();
            // the late validator's vote for the tip is still in flight
            let expected = if i == late { tip - 1 } else { tip };
            assert_eq!(counted, Some(expected), "{}", LABELS[i]);
        }
        for height in 1..=tip {
            let certificate = peers[0].marshal.certificate(height).await.unwrap();
            let signers: Vec<usize> = certificate
                .certificate
                .signers
                .iter()
                .map(usize::from)
                .collect();
            assert!(!signers.contains(&late), "{height}: {signers:?}");
        }
    });
}
