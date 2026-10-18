use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use abi::role::validators::Member;
use commonware_codec::DecodeExt as _;
use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_p2p::authenticated::lookup::{Config, Network, Oracle, Receiver, Sender};
use commonware_p2p::{Address, AddressableManager as _, AddressableTrackedPeers, Ingress};
use commonware_runtime::{Handle, Quota};
use commonware_utils::ordered::Map;
use commonware_utils::{NZU32, NZUsize, hostname};
use consensus::channel::{BACKFILL, BROADCAST, CERTIFICATE, RELAY, RESOLVER, VOTE};
use consensus::{Cadence, EngineChannels, MarshalLanes};

use crate::endpoint::Endpoint;
use crate::link::Linked;
use crate::transport::{self, Bridges};
use crate::{Context, Result};

pub const QUOTA_PER_SECOND: u32 = 1024;

pub fn ping_frequency(cadence: Cadence) -> Duration {
    Duration::from_millis(cadence.block_time_ms()) * 5
}

pub fn link_timeout(cadence: Cadence) -> Duration {
    ping_frequency(cadence) * 4
}

pub type Outbound<E> = Sender<PublicKey, Linked<E>>;

pub type Lane<E> = (Outbound<E>, Receiver<PublicKey>);

pub type Started<E> = (
    Mesh<E>,
    Tracker,
    MarshalLanes<Outbound<E>, Receiver<PublicKey>>,
    EngineChannels<Outbound<E>, Receiver<PublicKey>>,
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
        cadence: Cadence,
    ) -> Result<Started<E>> {
        // An epoch seats at most `member_cap` members; this node takes one more
        // place whenever it is not among them (commonware `peer_set_limit`), and
        // it can leave or join the set without a restart, so the +1 is kept.
        let peers = NZUsize!(member_cap as usize + 1);
        let me = identity.public_key();
        let mut config = Config::local(identity, namespace, listen, peers, node::MESSAGE_BYTES);
        config.bypass_ip_check = true;
        config.allow_dns = false;
        config.ping_frequency = ping_frequency(cadence);
        // only the current epoch's set: nothing reads an older one, and the
        // previous epoch's engine is gone at the switch
        config.tracked_peer_sets = NZUsize!(1);
        let linked = Linked::new(context, link_timeout(cadence));
        let (mut network, oracle) = Network::new(linked, config);
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

fn undialed() -> Address {
    Address::Asymmetric {
        ingress: Ingress::Dns {
            host: hostname!("undialed"),
            port: 0,
        },
        egress: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
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
            .filter_map(|member| {
                let key = PublicKey::decode(member.key.as_slice()).ok()?;
                let address = self
                    .dialed_at(epoch, &key, member, &mut kept)
                    .unwrap_or_else(undialed);
                Some((key, address))
            })
            .collect();
        self.bridges.keep(&kept);
        let _ = self.oracle.track(
            epoch,
            AddressableTrackedPeers::from(Map::from_iter_dedup(peers)),
        );
    }

    fn dialed_at(
        &mut self,
        epoch: u64,
        key: &PublicKey,
        member: &Member,
        kept: &mut HashSet<Endpoint>,
    ) -> Option<Address> {
        if *key == self.me {
            return Some(Address::Symmetric(self.listen));
        }
        let address = member.address.as_deref()?;
        let endpoint = match address.parse::<Endpoint>() {
            Ok(endpoint) => endpoint,
            Err(error) => {
                tracing::warn!(
                    target: "ducktape::mesh",
                    epoch,
                    reason = "member_address_unparsable",
                    %error,
                    "a member of the epoch is not dialed; it is accepted when it dials"
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
                    "a member of the epoch is not dialed; it is accepted when it dials"
                );
                return None;
            }
        };
        kept.insert(endpoint);
        Some(Address::Symmetric(local))
    }
}

#[cfg(test)]
mod tests {
    use commonware_p2p::Provider as _;
    use commonware_runtime::{Runner as _, tokio};

    use super::*;

    fn cadence() -> Cadence {
        Cadence::new(Duration::from_millis(250), consensus::Timeouts::default()).unwrap()
    }

    #[test]
    fn a_link_pings_every_five_block_times_and_dies_after_twenty_silent_ones() {
        assert_eq!(ping_frequency(cadence()), Duration::from_millis(1_250));
        assert_eq!(link_timeout(cadence()), Duration::from_secs(5));
        let one_second =
            Cadence::new(Duration::from_secs(1), consensus::Timeouts::default()).unwrap();
        assert_eq!(ping_frequency(one_second), Duration::from_secs(5));
        assert_eq!(link_timeout(one_second), Duration::from_secs(20));
    }

    #[test]
    fn the_consensus_timeouts_leave_the_link_alone() {
        let one_second =
            Cadence::new(Duration::from_secs(1), consensus::Timeouts::default()).unwrap();
        let tuned = Cadence::new(
            Duration::from_secs(1),
            consensus::Timeouts {
                leader: Some(Duration::from_secs(8)),
                certification: Some(Duration::from_secs(12)),
                retry: Some(Duration::from_secs(4)),
            },
        )
        .unwrap();
        assert_eq!(ping_frequency(tuned), ping_frequency(one_second));
        assert_eq!(link_timeout(tuned), link_timeout(one_second));
    }

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
                Mesh::start(context, identity, b"mesh", listen, 3, cadence()).unwrap();
            let members: Vec<Member> = (1..=foreign)
                .map(|seed| Member {
                    key: PrivateKey::from_seed(seed).public_key().as_ref().to_vec(),
                    address: Some(format!("tcp://localhost:{}", 9_000 + seed)),
                    halo: format!("localhost:{}", 9_000 + seed),
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

    #[test]
    fn a_member_without_an_address_is_a_primary_peer() {
        tokio::Runner::default().start(|context| async move {
            let listen = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap();
            let (mesh, mut tracker, _, _, _) = Mesh::start(
                context,
                PrivateKey::from_seed(0),
                b"mesh",
                listen,
                3,
                cadence(),
            )
            .unwrap();
            let undialed = PrivateKey::from_seed(1).public_key();
            tracker.track(
                0,
                &[Member {
                    key: undialed.as_ref().to_vec(),
                    address: None,
                    halo: "localhost:1".into(),
                }],
            );
            let tracked = mesh.oracle().peer_set(0).await.unwrap();
            assert!(tracked.primary.position(&undialed).is_some(), "{tracked:?}");
        });
    }
}
