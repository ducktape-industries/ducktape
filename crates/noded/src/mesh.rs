use std::net::SocketAddr;

use abi::valset::Member;
use commonware_codec::DecodeExt as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_p2p::authenticated::lookup::{Config, Network, Oracle, Receiver, Sender};
use commonware_p2p::{Address, AddressableManager as _, AddressableTrackedPeers};
use commonware_runtime::{Handle, Quota};
use commonware_utils::ordered::Map;
use commonware_utils::{NZU32, NZUsize};
use consensus::channel::{BACKFILL, BROADCAST, CERTIFICATE, RESOLVER, VOTE};
use consensus::{EngineChannels, MarshalLanes};

use crate::Context;

pub const MESSAGE_SIZE: u32 = 1 << 26;
pub const PEERS_PER_SET: usize = 1024;
pub const QUOTA_PER_SECOND: u32 = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    Public,
    Private,
}

pub type Started<E> = (
    Mesh<E>,
    MarshalLanes<Sender<PublicKey, E>, Receiver<PublicKey>>,
    EngineChannels<Sender<PublicKey, E>, Receiver<PublicKey>>,
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
                MESSAGE_SIZE,
            ),
            Reach::Private => Config::local(
                identity,
                namespace,
                listen,
                NZUsize!(PEERS_PER_SET),
                MESSAGE_SIZE,
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
        let running = network.start();
        let mesh = Mesh {
            oracle,
            running,
            _runtime: std::marker::PhantomData,
        };
        (mesh, marshal, engine)
    }

    pub fn oracle(&self) -> Oracle<PublicKey> {
        self.oracle.clone()
    }
}

pub fn track(oracle: &mut Oracle<PublicKey>, epoch: u64, members: &[Member]) {
    let peers: Vec<(PublicKey, Address)> = members
        .iter()
        .filter_map(|member| {
            let key = PublicKey::decode(member.key.as_slice()).ok()?;
            let address: SocketAddr = member.address.parse().ok()?;
            Some((key, Address::Symmetric(address)))
        })
        .collect();
    let unreachable = members.len() - peers.len();
    if unreachable > 0 {
        tracing::warn!(
            target: "ducktape::mesh",
            epoch,
            unreachable,
            reason = "member_address_unparsable",
            "some members of the epoch cannot be dialed"
        );
    }
    let _ = oracle.track(
        epoch,
        AddressableTrackedPeers::from(Map::from_iter_dedup(peers)),
    );
}
