use std::net::SocketAddr;

use abi::role::validators::Member;
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
        member_cap: u32,
    ) -> Started<E> {
        // An epoch seats at most `member_cap` members; this node takes one more
        // place whenever it is not among them (commonware `peer_set_limit`), and
        // it can leave or join the set without a restart, so the +1 is kept.
        let peers = NZUsize!(member_cap as usize + 1);
        let mut config = match reach {
            Reach::Public => Config::recommended(identity, namespace, listen, peers, MESSAGE_SIZE),
            Reach::Private => Config::local(identity, namespace, listen, peers, MESSAGE_SIZE),
        };
        // only the current epoch's set: nothing reads an older one, and the
        // previous epoch's engine is gone at the switch
        config.tracked_peer_sets = NZUsize!(1);
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

#[cfg(test)]
mod tests {
    use commonware_cryptography::Signer as _;
    use commonware_runtime::{Runner as _, tokio};

    use super::*;

    /// Starts a mesh capped at three members and tracks `foreign` members,
    /// none of them this node.
    fn track_foreign(foreign: u64) {
        tokio::Runner::default().start(|context| async move {
            let listen = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap();
            let identity = PrivateKey::from_seed(0);
            let (mesh, _, _) = Mesh::start(context, identity, b"mesh", listen, Reach::Private, 3);
            let members: Vec<Member> = (1..=foreign)
                .map(|seed| Member {
                    key: PrivateKey::from_seed(seed).public_key().as_ref().to_vec(),
                    address: format!("127.0.0.1:{}", 9_000 + seed),
                })
                .collect();
            track(&mut mesh.oracle(), 0, &members);
        });
    }

    #[test]
    fn a_full_set_that_omits_this_node_is_tracked() {
        track_foreign(3);
    }

    #[test]
    #[should_panic(expected = "peer set too large: 5 > 4")]
    fn a_set_past_the_cap_panics() {
        track_foreign(4);
    }
}
