//! halo: the network between members, off consensus. It carries lanes of streams and
//! datagrams between current members over QUIC, at the halo address each member registers,
//! at a priority class capped by who asks.

pub mod class;
pub mod links;
pub mod members;
pub mod pacer;
pub mod plane;
pub mod quic;
#[cfg(feature = "sim")]
pub mod sim;
pub mod transport;

use std::net::SocketAddr;
use std::sync::Arc;

use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_runtime::Metrics;

pub use class::Class;
pub use links::{Carried, Links};
pub use members::{Members, Seat};
pub use pacer::{Paced, Pacer};
pub use plane::{Datagrams, Inbound, Lane, Plane, Streams, Taken};
pub use transport::{Error, Transport};

const EGRESS_BURST: u64 = 256 << 10;

pub struct Config {
    pub egress_bytes_per_sec: Option<u64>,
    pub udp: SocketAddr,
}

pub struct Halo {
    links: Arc<Links>,
    members: Arc<Members>,
    plane: Plane<Arc<Links>>,
    me: PublicKey,
}

impl Halo {
    pub fn start<E: Metrics>(
        context: &E,
        identity: PrivateKey,
        config: Config,
    ) -> Result<Halo, quic::Error> {
        let me = identity.public_key();
        let members = Arc::new(Members::default());
        let links = Links::start(context, identity, members.clone(), config.udp)?;
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
        })
    }

    pub fn seat(&self, seats: impl IntoIterator<Item = Seat>) {
        self.members.seat(seats);
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
