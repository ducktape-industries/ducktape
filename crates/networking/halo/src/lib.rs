//! halo: the network between members, off consensus. It carries lanes of streams and
//! datagrams between current members over QUIC, or over tcp/tls/ws/wss where UDP does not
//! answer, at a priority class capped by who asks, and serves the blobs members hold.

pub mod blobs;
pub mod class;
pub mod dial;
pub mod endpoint;
mod io;
pub mod links;
pub mod members;
pub mod pacer;
pub mod plane;
pub mod quic;
#[cfg(feature = "sim")]
pub mod sim;
pub mod tcp;
pub mod transport;

use std::net::SocketAddr;
use std::sync::Arc;

use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_runtime::Metrics;

pub use blobs::{Answer, Ask, BlobLane, Want};
pub use class::Class;
pub use endpoint::{Endpoint, Scheme};
pub use links::{Carried, Links};
pub use members::{Members, Seat};
pub use pacer::{Paced, Pacer};
pub use plane::{Datagrams, Inbound, Lane, Plane, Streams, Taken};
pub use tcp::{Bytes, Context, PREAMBLE, ROUTE, Sniffed, sniffed};
pub use transport::{Error, Transport};

const EGRESS_BURST: u64 = 256 << 10;

pub struct Config {
    pub egress_bytes_per_sec: u64,
    pub udp: Vec<SocketAddr>,
}

pub struct Halo<E: Context + Metrics> {
    links: Arc<Links<E>>,
    members: Arc<Members>,
    blobs: Arc<BlobLane<Arc<Links<E>>>>,
    plane: Plane<Arc<Links<E>>>,
    serving: tokio::task::JoinHandle<()>,
}

impl<E: Context + Metrics> Drop for Halo<E> {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

impl<E: Context + Metrics> Halo<E> {
    pub fn start(
        context: E,
        identity: PrivateKey,
        namespace: &[u8],
        config: Config,
        store: ::blobs::Blobs,
    ) -> Result<Halo<E>, rustls::Error> {
        let me = identity.public_key();
        let members = Arc::new(Members::default());
        let links = Links::start(context, identity, namespace, members.clone(), &config.udp)?;
        let pacer = Pacer::new(config.egress_bytes_per_sec, EGRESS_BURST);
        let plane = Plane::new(Arc::new(links.clone()), members.clone(), pacer);
        let blobs = BlobLane::new(&plane, members.clone(), me)
            .expect("a fresh plane has no lane registered");
        let serving = tokio::spawn(blobs.clone().serve(store));
        Ok(Halo {
            links,
            members,
            blobs,
            plane,
            serving,
        })
    }

    pub fn seat(&self, seats: impl IntoIterator<Item = Seat>) {
        self.members.seat(seats);
    }

    pub async fn fetch(&self, want: Want) -> Vec<u8> {
        self.blobs.fetch(want).await
    }

    pub async fn ask(
        &self,
        peer: &PublicKey,
        id: abi::BlobId,
        class: Class,
    ) -> Result<Answer, &'static str> {
        self.blobs.ask(peer, id, class).await
    }

    pub fn inbound(&self, conn: impl Bytes) {
        self.links.inbound(conn);
    }

    pub fn plane(&self) -> &Plane<Arc<Links<E>>> {
        &self.plane
    }

    pub fn udp(&self) -> Vec<SocketAddr> {
        self.links.udp()
    }
}
