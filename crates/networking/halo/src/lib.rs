//! halo: the network between members, off consensus. It carries lanes of streams and
//! datagrams between current members over QUIC, at a priority class capped by who asks and by
//! the lane. Which lanes exist, and each one's wire id and cap, is the halo role's table.

pub mod class;
mod control;
pub mod links;
pub mod members;
#[cfg(feature = "sim")]
pub mod nat;
pub mod pacer;
pub mod plane;
pub mod quic;
pub mod reach;
pub mod relay;
#[cfg(feature = "sim")]
pub mod sim;
pub mod transport;

use std::net::SocketAddr;
use std::sync::Arc;

use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_runtime::Metrics;

pub use abi::role::halo::{Lane, LaneId};
pub use class::Class;
pub use links::{Carried, Links, Path, Queues};
pub use members::{Members, Seat};
pub use pacer::{Paced, Pacer};
pub use plane::{Datagrams, Inbound, Plane, Streams, Taken};
pub use quic::Udp;
pub use reach::{Https, RELAY_ROUTE, Reach, Reached, SEEN_ROUTE};
pub use relay::Pipe;
pub use transport::{Error, Transport};

const EGRESS_BURST: u64 = 256 << 10;

pub struct Config {
    pub egress_bytes_per_sec: Option<u64>,
    pub udp: Udp,
    pub queues: Queues,
    pub reach: Arc<dyn Reach>,
}

pub struct Halo {
    links: Arc<Links>,
    members: Arc<Members>,
    plane: Plane<Arc<Links>>,
    me: PublicKey,
    membership: tokio::sync::watch::Sender<()>,
}

impl Halo {
    pub fn start<E: Metrics>(
        context: &E,
        identity: PrivateKey,
        config: Config,
    ) -> Result<Halo, quic::Error> {
        let me = identity.public_key();
        let members = Arc::new(Members::default());
        let links = Links::start(
            context,
            identity,
            members.clone(),
            config.udp,
            config.queues,
            config.reach,
        )?;
        let pacer = match config.egress_bytes_per_sec {
            Some(rate) => Pacer::new(rate, EGRESS_BURST),
            None => Pacer::unlimited(),
        };
        let plane = Plane::new(Arc::new(links.clone()), members.clone(), pacer);
        Ok(Halo {
            links,
            members,
            plane,
            me,
            membership: tokio::sync::watch::channel(()).0,
        })
    }

    pub fn seat(&self, seats: impl IntoIterator<Item = Seat>) {
        let changed = self.members.seat(seats);
        let moved = !changed.is_empty();
        self.links.seated(changed);
        // A binding that reconciles per-peer state learns the membership moved.
        if moved {
            let _ = self.membership.send(());
        }
    }

    /// Fires whenever the seated members change, so a binding reconciles its per-peer state.
    pub fn membership_changed(&self) -> tokio::sync::watch::Receiver<()> {
        self.membership.subscribe()
    }

    pub async fn linked(&self, member: &PublicKey) {
        self.links.linked(member).await;
    }

    pub async fn direct(&self, member: &PublicKey) {
        self.links.direct(member).await;
    }

    pub fn path(&self, member: &PublicKey) -> Option<Path> {
        self.links.path(member)
    }

    pub fn paths(&self, member: &PublicKey) -> Vec<Path> {
        self.links.paths(member)
    }

    pub fn link_changes(&self, member: &PublicKey) -> tokio::sync::watch::Receiver<()> {
        self.links.link_changes(member)
    }

    pub fn reached_at(&self) -> tokio::sync::watch::Receiver<Vec<SocketAddr>> {
        self.links.reached_at()
    }

    pub async fn relayed(&self, pipe: Pipe) -> Result<PublicKey, quic::Error> {
        self.links.relayed(pipe).await
    }

    pub fn members(&self) -> &Arc<Members> {
        &self.members
    }

    pub fn me(&self) -> &PublicKey {
        &self.me
    }

    pub fn plane(&self) -> &Plane<Arc<Links>> {
        &self.plane
    }

    pub fn udp(&self) -> std::io::Result<SocketAddr> {
        self.links.udp()
    }
}
