//! A marshal whose application drops a block's acknowledgement ends its actor, and its
//! broadcast and resolver engines log no error: their mailboxes stay open until the stop.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use commonware_consensus::marshal::core::DigestFallback;
use commonware_cryptography::{Signer as _, ed25519, sha256};
use commonware_p2p::simulated;
use commonware_runtime::{Quota, Runner as _, Supervisor as _, deterministic};
use commonware_utils::acknowledgement::Exact;
use commonware_utils::{NZU32, NZUsize};
use consensus::{
    Ancestry, Anchor, Cadence, Chain, EngineMux, Marshal, Membership, Network, Roster, SimMesh,
    Timeouts, Transport,
};
use futures::StreamExt as _;
use futures::channel::mpsc;
use node::Block;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};

const NETWORK: &[u8] = b"stop";

#[derive(Clone)]
struct Dropping(mpsc::UnboundedSender<u64>);

impl Chain for Dropping {
    async fn propose(
        &mut self,
        parent: Arc<Block>,
        _: impl Ancestry<Block>,
        time: u64,
    ) -> Option<Block> {
        Some(Block::next(parent.tip(), time, Vec::new()))
    }

    fn deliver(&mut self, block: Arc<Block>, ack: Exact) {
        drop(ack);
        let _ = self.0.unbounded_send(block.height);
    }
}

#[derive(Clone, Default)]
struct Errors(Arc<Mutex<Vec<String>>>);

impl<S: Subscriber> Layer<S> for Errors {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let metadata = event.metadata();
        if *metadata.level() == Level::ERROR {
            let line = format!("{} {}", metadata.target(), metadata.name());
            self.0.lock().unwrap().push(line);
        }
    }
}

#[test]
fn an_actor_ended_by_a_dropped_acknowledgement_leaves_its_engines_no_closed_mailbox() {
    let errors = Errors::default();
    let subscriber = tracing_subscriber::registry().with(errors.clone());
    tracing::subscriber::with_default(subscriber, || {
        deterministic::Runner::default().start(|context| async move {
            let key = ed25519::PrivateKey::from_seed(1);
            let (simulated, oracle) = simulated::Network::new_with_peers(
                context.child("network"),
                simulated::Config {
                    max_size: 1 << 20,
                    disconnect_on_block: true,
                    max_peers_per_set: NZUsize!(16),
                    tracked_peer_sets: NZUsize!(1),
                },
                vec![key.public_key()],
            )
            .await;
            simulated.start();
            let mesh = SimMesh::new(oracle, key.public_key(), Quota::per_second(NZU32!(1024)));
            let network = Network {
                epoch_length: 1_000,
                cadence: Cadence::new(Duration::from_millis(1_000), Timeouts::default()).unwrap(),
            };
            let genesis = Block::genesis(NETWORK, 0);
            let roster = Roster::new(NETWORK.to_vec(), Some(key.clone()));
            let (delivered, mut deliveries) = mpsc::unbounded();
            let chain = Dropping(delivered);
            let marshal = Marshal::start(
                context.child("marshal"),
                "stop",
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
            .await
            .unwrap();
            let never = sha256::Digest([0xab; 32]);
            let ended = marshal
                .mailbox()
                .subscribe_by_digest(never, DigestFallback::Wait);
            let mux = EngineMux::start(
                context.child("lanes"),
                mesh.engine_channels().await,
                mesh.blocker(),
            );
            let mut membership = Membership::new(
                context.child("membership"),
                "stop".to_owned(),
                network,
                roster,
                mux,
                &marshal,
                chain,
            );
            let validators = vec![key.public_key().as_ref().to_vec()];
            membership.seat(genesis.tip(), &validators).await.unwrap();
            deliveries.next().await.expect("a block is delivered");
            assert!(ended.await.is_err(), "the actor ends on the dropped ack");
            membership.stop().await;
            marshal.stop().await;
        });
    });
    let errors = std::mem::take(&mut *errors.0.lock().unwrap());
    assert_eq!(errors, Vec::<String>::new());
}
