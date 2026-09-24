use std::net::SocketAddr;

use abi::valset::{self, Seating};
use commonware_codec::DecodeExt as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_p2p::authenticated::lookup::{Config, Network, Oracle, Receiver, Sender};
use commonware_p2p::{Address, AddressableManager as _, AddressableTrackedPeers};
use commonware_runtime::{Handle, Quota};
use commonware_utils::ordered::Map;
use commonware_utils::{NZU32, NZUsize};
use consensus::channel::{BACKFILL, BROADCAST, CERTIFICATE, RELAY, RESOLVER, VOTE};
use consensus::{EngineChannels, MarshalLanes};

use crate::Context;

pub const PEERS_PER_SET: usize = valset::MAX_MEMBERS + 1;
pub const QUOTA_PER_SECOND: u32 = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    Public,
    Private,
}

pub type Lane<E> = (Sender<PublicKey, E>, Receiver<PublicKey>);

pub type Started<E> = (
    Mesh<E>,
    MarshalLanes<Sender<PublicKey, E>, Receiver<PublicKey>>,
    EngineChannels<Sender<PublicKey, E>, Receiver<PublicKey>>,
    Lane<E>,
);

pub struct Mesh<E: Context> {
    oracle: Oracle<PublicKey>,
    running: Handle<()>,
    _runtime: std::marker::PhantomData<fn() -> E>,
}

impl<E: Context> Drop for Mesh<E> {
    fn drop(&mut self) {
        self.running.abort();
    }
}

impl<E: Context> Mesh<E> {
    pub fn start(
        context: E,
        identity: PrivateKey,
        namespace: &[u8],
        listen: SocketAddr,
        reach: Reach,
    ) -> Started<E> {
        let config = match reach {
            Reach::Public => Config::recommended(
                identity,
                namespace,
                listen,
                NZUsize!(PEERS_PER_SET),
                node::MESSAGE_BYTES,
            ),
            Reach::Private => Config::local(
                identity,
                namespace,
                listen,
                NZUsize!(PEERS_PER_SET),
                node::MESSAGE_BYTES,
            ),
        };
        let (mut network, oracle) = Network::new(context, config);
        let quota = Quota::per_second(NZU32!(QUOTA_PER_SECOND));
        let marshal = MarshalLanes {
            broadcast: network.register(BROADCAST, quota),
            backfill: network.register(BACKFILL, quota),
        };
        let engine = EngineChannels {
            vote: network.register(VOTE, quota),
            certificate: network.register(CERTIFICATE, quota),
            resolver: network.register(RESOLVER, quota),
        };
        let relay = network.register(RELAY, quota);
        let running = network.start();
        let mesh = Mesh {
            oracle,
            running,
            _runtime: std::marker::PhantomData,
        };
        (mesh, marshal, engine, relay)
    }

    pub fn oracle(&self) -> Oracle<PublicKey> {
        self.oracle.clone()
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Tracked {
    pub peers: Vec<(PublicKey, SocketAddr)>,
    pub unreachable: usize,
    pub dropped: usize,
}

pub fn tracked(seating: &Seating, local: &[u8]) -> Tracked {
    let is_validator = |key: &[u8]| seating.validators.iter().any(|seated| seated == key);
    let validators = seating.members.iter().filter(|m| is_validator(&m.key));
    let residents = seating.members.iter().filter(|m| !is_validator(&m.key));
    let mut tracked = Tracked::default();
    let mut others = 0;
    for member in validators.chain(residents) {
        let decoded = PublicKey::decode(member.key.as_slice()).ok();
        let parsed: Option<SocketAddr> = member.address.parse().ok();
        let (Some(key), Some(address)) = (decoded, parsed) else {
            tracked.unreachable += 1;
            continue;
        };
        let is_local = member.key == local;
        if is_local {
            tracked.peers.push((key, address));
            continue;
        }
        let full = others == PEERS_PER_SET - 1;
        if full {
            tracked.dropped += 1;
            continue;
        }
        others += 1;
        tracked.peers.push((key, address));
    }
    tracked
}

pub fn track(oracle: &mut Oracle<PublicKey>, epoch: u64, seating: &Seating, local: &[u8]) {
    let tracked = tracked(seating, local);
    if tracked.unreachable > 0 {
        tracing::warn!(
            target: "ducktape::mesh",
            epoch,
            unreachable = tracked.unreachable,
            reason = "member_address_unparsable",
            "some members of the epoch cannot be dialed"
        );
    }
    if tracked.dropped > 0 {
        tracing::warn!(
            target: "ducktape::mesh",
            event = "peer_set_truncated",
            epoch,
            dropped = tracked.dropped,
            reason = "peer_set_full",
            "the epoch seats more members than the mesh holds; the rest are not tracked"
        );
    }
    let peers = tracked
        .peers
        .into_iter()
        .map(|(key, address)| (key, Address::Symmetric(address)));
    let _ = oracle.track(
        epoch,
        AddressableTrackedPeers::from(Map::from_iter_dedup(peers)),
    );
}
