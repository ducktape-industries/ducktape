use std::collections::HashSet;
use std::net::SocketAddr;

use abi::role::validators::Member;
use commonware_codec::DecodeExt as _;
use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_p2p::authenticated::lookup::{Config, Network, Oracle, Receiver, Sender};
use commonware_p2p::{Address, AddressableManager as _, AddressableTrackedPeers};
use commonware_runtime::{Handle, Quota};
use commonware_utils::ordered::Map;
use commonware_utils::{NZU32, NZUsize};
use consensus::channel::{BACKFILL, BROADCAST, CERTIFICATE, RELAY, RESOLVER, VOTE};
use consensus::{EngineChannels, MarshalLanes};

use crate::endpoint::Endpoint;
use crate::transport::{self, Bridges};
use crate::{Context, Result};

pub const QUOTA_PER_SECOND: u32 = 1024;

pub type Lane<E> = (Sender<PublicKey, E>, Receiver<PublicKey>);

pub type Started<E> = (
    Mesh<E>,
    Tracker,
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
        member_cap: u32,
    ) -> Result<Started<E>> {
        // An epoch seats at most `member_cap` members; this node takes one more
        // place whenever it is not among them (commonware `peer_set_limit`), and
        // it can leave or join the set without a restart, so the +1 is kept.
        let peers = NZUsize!(member_cap as usize + 1);
        let me = identity.public_key();
        let mut config = Config::local(identity, namespace, listen, peers, node::MESSAGE_BYTES);
        config.bypass_ip_check = true;
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
        let relay = network.register(RELAY, quota);
        let running = network.start();
        let tracker = Tracker {
            oracle: oracle.clone(),
            me,
            listen: transport::loopback(listen),
            bridges: Bridges::new(transport::dialer()?),
        };
        let mesh = Mesh {
            oracle,
            running,
            _runtime: std::marker::PhantomData,
        };
        Ok((mesh, tracker, marshal, engine, relay))
    }

    pub fn oracle(&self) -> Oracle<PublicKey> {
        self.oracle.clone()
    }
}

pub struct Tracker {
    oracle: Oracle<PublicKey>,
    me: PublicKey,
    listen: SocketAddr,
    bridges: Bridges,
}

impl Tracker {
    pub fn track(&mut self, epoch: u64, members: &[Member]) {
        let mut kept = HashSet::new();
        let peers: Vec<(PublicKey, Address)> = members
            .iter()
            .filter_map(|member| self.peer(epoch, member, &mut kept))
            .collect();
        self.bridges.keep(&kept);
        let _ = self.oracle.track(
            epoch,
            AddressableTrackedPeers::from(Map::from_iter_dedup(peers)),
        );
    }

    fn peer(
        &mut self,
        epoch: u64,
        member: &Member,
        kept: &mut HashSet<Endpoint>,
    ) -> Option<(PublicKey, Address)> {
        let key = PublicKey::decode(member.key.as_slice()).ok()?;
        if key == self.me {
            return Some((key, Address::Symmetric(self.listen)));
        }
        let endpoint = match member.address.parse::<Endpoint>() {
            Ok(endpoint) => endpoint,
            Err(error) => {
                tracing::warn!(
                    target: "ducktape::mesh",
                    epoch,
                    reason = "member_address_unparsable",
                    %error,
                    "a member of the epoch cannot be dialed"
                );
                return None;
            }
        };
        let local = match self.bridges.local(&endpoint) {
            Ok(local) => local,
            Err(error) => {
                tracing::warn!(
                    target: "ducktape::mesh",
                    epoch,
                    reason = "bridge_unbound",
                    %endpoint,
                    %error,
                    "a member of the epoch cannot be dialed"
                );
                return None;
            }
        };
        kept.insert(endpoint);
        Some((key, Address::Symmetric(local)))
    }
}

#[cfg(test)]
mod tests {
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
            let (_mesh, mut tracker, _, _, _) =
                Mesh::start(context, identity, b"mesh", listen, 3).unwrap();
            let members: Vec<Member> = (1..=foreign)
                .map(|seed| Member {
                    key: PrivateKey::from_seed(seed).public_key().as_ref().to_vec(),
                    address: format!("tcp://localhost:{}", 9_000 + seed),
                })
                .collect();
            tracker.track(0, &members);
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
