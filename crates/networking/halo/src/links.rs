//! The real medium: QUIC on UDP at the halo address each member registers. A node dials every
//! seated member and keeps one connection to each, which carries streams both ways, so a member
//! no one can reach still takes part by dialing out.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use bytes::Bytes;
use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_runtime::Metrics;
use commonware_runtime::telemetry::metrics::Registered;
use commonware_runtime::telemetry::metrics::raw::Counter;
use futures::future::Either;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, watch};

use crate::members::Members;
use crate::quic::{self, Quic};
use crate::transport::{Error, Transport};

const DIAL: Duration = Duration::from_secs(5);
const YIELD: Duration = Duration::from_secs(1);
const REDIAL: Duration = Duration::from_secs(1);
const REDIAL_MOST: Duration = Duration::from_secs(60);
const INBOUND: usize = 256;
const DATAGRAMS: usize = 1024;

pub struct Carried {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
}

impl AsyncRead for Carried {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().recv).poll_read(cx, buf)
    }
}

impl AsyncWrite for Carried {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().send)
            .poll_write(cx, buf)
            .map_err(io::Error::from)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().send).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().send).poll_shutdown(cx)
    }
}

impl Carried {
    /// Abandons the stream, so the peer reads a reset rather than a clean end.
    pub fn reset(mut self) {
        let _ = self.send.reset(0u32.into());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Dialed,
    Accepted,
}

fn preferred(me: &PublicKey, peer: &PublicKey, side: Side) -> bool {
    match side {
        Side::Dialed => me < peer,
        Side::Accepted => peer < me,
    }
}

struct Link {
    connection: quinn::Connection,
    preferred: bool,
}

impl Link {
    fn open(&self) -> bool {
        self.connection.close_reason().is_none()
    }
}

struct Peer {
    dialing: tokio::sync::Mutex<()>,
    links: Mutex<Vec<Link>>,
    changed: watch::Sender<()>,
    kept: AtomicBool,
}

impl Default for Peer {
    fn default() -> Peer {
        Peer {
            dialing: tokio::sync::Mutex::default(),
            links: Mutex::default(),
            changed: watch::channel(()).0,
            kept: AtomicBool::new(false),
        }
    }
}

impl Peer {
    fn held(&self) -> std::sync::MutexGuard<'_, Vec<Link>> {
        self.links.lock().expect("the links lock is never poisoned")
    }

    fn live(&self) -> Option<quinn::Connection> {
        let held = self.held();
        let link = held
            .iter()
            .filter(|link| link.open())
            .max_by_key(|link| link.preferred)?;
        Some(link.connection.clone())
    }

    fn adopt(&self, link: Link) {
        let mut held = self.held();
        held.retain(Link::open);
        held.push(link);
        drop(held);
        self.changed.send_replace(());
    }

    fn forget(&self, connection: &quinn::Connection) {
        let mut held = self.held();
        held.retain(|kept| kept.connection.stable_id() != connection.stable_id());
        drop(held);
        self.changed.send_replace(());
    }

    fn unseat(&self) {
        let gone = std::mem::take(&mut *self.held());
        for link in gone {
            link.connection.close(0u32.into(), b"unseated");
        }
    }

    async fn arrival(&self, changed: &mut watch::Receiver<()>) -> Option<quinn::Connection> {
        loop {
            changed.changed().await.ok()?;
            if let Some(connection) = self.live() {
                return Some(connection);
            }
        }
    }
}

#[derive(Default)]
struct Redial {
    failures: u32,
    at: Option<String>,
    yielded: bool,
}

impl Redial {
    fn failed(&mut self, at: &str) -> Duration {
        let moved = self.at.as_deref() != Some(at);
        if moved {
            *self = Redial {
                failures: 0,
                at: Some(at.to_owned()),
                yielded: true,
            };
        }
        self.failures += 1;
        REDIAL.saturating_mul(self.failures).min(REDIAL_MOST)
    }
}

pub struct Links {
    me: PublicKey,
    members: Arc<Members>,
    quic: Quic,
    peers: Mutex<HashMap<PublicKey, Arc<Peer>>>,
    inbound: mpsc::Sender<(PublicKey, Carried)>,
    accepted: tokio::sync::Mutex<mpsc::Receiver<(PublicKey, Carried)>>,
    received: mpsc::Sender<(PublicKey, Bytes)>,
    datagrams: tokio::sync::Mutex<mpsc::Receiver<(PublicKey, Bytes)>>,
    opened: Registered<Counter>,
}

impl Links {
    pub fn start<E: Metrics>(
        context: &E,
        identity: PrivateKey,
        members: Arc<Members>,
        udp: SocketAddr,
    ) -> Result<Arc<Links>, quic::Error> {
        let quic = Quic::bind(udp, &identity, members.clone())?;
        let (inbound, accepted) = mpsc::channel(INBOUND);
        let (received, datagrams) = mpsc::channel(DATAGRAMS);
        let opened = context.register(
            "halo_streams_opened",
            "streams this node opened to a member over QUIC",
            Counter::default(),
        );
        let links = Arc::new(Links {
            me: identity.public_key(),
            members,
            quic,
            peers: Mutex::default(),
            inbound,
            accepted: tokio::sync::Mutex::new(accepted),
            received,
            datagrams: tokio::sync::Mutex::new(datagrams),
            opened,
        });
        tokio::spawn(accepting(
            Arc::downgrade(&links),
            links.quic.endpoint().clone(),
        ));
        Ok(links)
    }

    pub fn udp(&self) -> io::Result<SocketAddr> {
        self.quic.local()
    }

    pub fn seated(self: &Arc<Self>, changed: Vec<PublicKey>) {
        for key in changed {
            if key == self.me {
                continue;
            }
            let peer = self.peer(&key);
            let unkept = !peer.kept.swap(true, Ordering::AcqRel);
            if unkept {
                tokio::spawn(keeping(Arc::downgrade(self), key, peer.changed.subscribe()));
            }
            peer.changed.send_replace(());
        }
    }

    pub async fn linked(&self, key: &PublicKey) {
        let peer = self.peer(key);
        let mut changed = peer.changed.subscribe();
        if peer.live().is_some() {
            return;
        }
        peer.arrival(&mut changed).await;
    }

    fn peer(&self, key: &PublicKey) -> Arc<Peer> {
        self.peers
            .lock()
            .expect("the peers lock is never poisoned")
            .entry(key.clone())
            .or_default()
            .clone()
    }

    async fn connection(self: &Arc<Self>, to: &PublicKey) -> Result<quinn::Connection, Error> {
        let peer = self.peer(to);
        let mut changed = peer.changed.subscribe();
        if let Some(connection) = peer.live() {
            return Ok(connection);
        }
        let dialed = self.dial(to, &peer);
        let arrived = peer.arrival(&mut changed);
        futures::pin_mut!(dialed, arrived);
        match futures::future::select(dialed, arrived).await {
            Either::Left((dialed, _)) => dialed,
            Either::Right((Some(connection), _)) => Ok(connection),
            Either::Right((None, dialed)) => dialed.await,
        }
    }

    async fn dial(
        self: &Arc<Self>,
        to: &PublicKey,
        peer: &Peer,
    ) -> Result<quinn::Connection, Error> {
        let _dialing = peer.dialing.lock().await;
        if let Some(connection) = peer.live() {
            return Ok(connection);
        }
        let Some(halo) = self.members.halo(to) else {
            return Err(Error::Unreachable(to.clone()));
        };
        let dialed = tokio::time::timeout(DIAL, async {
            let at = resolved(&self.quic, &halo).await?;
            self.quic.dial(to, at).await.ok()
        })
        .await;
        let Ok(Some(connection)) = dialed else {
            return Err(Error::Unreachable(to.clone()));
        };
        tracing::debug!(target: "ducktape::halo", peer = %to, at = %halo, "halo dialed a member at its halo address");
        self.carry(to, connection, Side::Dialed);
        peer.live().ok_or_else(|| Error::Unreachable(to.clone()))
    }

    fn carry(self: &Arc<Self>, key: &PublicKey, connection: quinn::Connection, side: Side) {
        let link = Link {
            connection: connection.clone(),
            preferred: preferred(&self.me, key, side),
        };
        self.peer(key).adopt(link);
        tokio::spawn(carried(Arc::downgrade(self), key.clone(), connection));
    }

    async fn keep(self: &Arc<Self>, key: &PublicKey, redial: &mut Redial) -> Option<Duration> {
        let peer = self.peer(key);
        let Some(halo) = self.members.halo(key) else {
            peer.unseat();
            return None;
        };
        if peer.live().is_some() {
            *redial = Redial::default();
            return None;
        }
        let dials_first = preferred(&self.me, key, Side::Dialed);
        let yields = !dials_first && !redial.yielded;
        if yields {
            redial.yielded = true;
            return Some(YIELD);
        }
        if self.dial(key, &peer).await.is_ok() {
            *redial = Redial::default();
            return None;
        }
        let pause = redial.failed(&halo);
        let attempts = redial.failures;
        let worth_a_line = attempts == 1 || attempts.is_multiple_of(10);
        if worth_a_line {
            tracing::debug!(target: "ducktape::halo", peer = %key, at = %halo, attempts, reason = "undialed", "halo did not reach a member at its halo address; it dials again");
        }
        Some(pause)
    }
}

impl Drop for Links {
    fn drop(&mut self) {
        self.quic.endpoint().close(0u32.into(), b"halo stopped");
    }
}

async fn resolved(quic: &Quic, halo: &str) -> Option<SocketAddr> {
    let mut found = tokio::net::lookup_host(halo).await.ok()?;
    found.find(|at| quic.reaches(at))
}

async fn keeping(links: Weak<Links>, key: PublicKey, mut changed: watch::Receiver<()>) {
    let mut redial = Redial::default();
    loop {
        let Some(strong) = links.upgrade() else {
            return;
        };
        let pause = strong.keep(&key, &mut redial).await;
        drop(strong);
        let woken = match pause {
            None => changed.changed().await.is_ok(),
            Some(pause) => {
                let woken = changed.changed();
                let paused = tokio::time::sleep(pause);
                futures::pin_mut!(woken, paused);
                match futures::future::select(woken, paused).await {
                    Either::Left((woken, _)) => woken.is_ok(),
                    Either::Right(((), _)) => true,
                }
            }
        };
        if !woken {
            return;
        }
    }
}

async fn accepting(links: Weak<Links>, endpoint: quinn::Endpoint) {
    while let Some(incoming) = endpoint.accept().await {
        let links = links.clone();
        tokio::spawn(async move {
            let Ok(connection) = incoming.await else {
                return;
            };
            let Some(links) = links.upgrade() else {
                return;
            };
            let Some(peer) = quic::peer_of(&connection) else {
                connection.close(0u32.into(), b"no node key");
                return;
            };
            links.carry(&peer, connection, Side::Accepted);
        });
    }
}

async fn carried(links: Weak<Links>, peer: PublicKey, connection: quinn::Connection) {
    let streams = async {
        while let Ok((send, recv)) = connection.accept_bi().await {
            let Some(links) = links.upgrade() else {
                return;
            };
            let _ = links
                .inbound
                .send((peer.clone(), Carried { send, recv }))
                .await;
        }
    };
    let datagrams = async {
        while let Ok(datagram) = connection.read_datagram().await {
            let Some(links) = links.upgrade() else {
                return;
            };
            let _ = links.received.try_send((peer.clone(), datagram));
        }
    };
    futures::pin_mut!(streams, datagrams);
    futures::future::select(streams, datagrams).await;
    if let Some(links) = links.upgrade() {
        links.peer(&peer).forget(&connection);
    }
}

impl Transport for Arc<Links> {
    type Stream = Carried;

    async fn connect(&self, to: &PublicKey) -> Result<Carried, Error> {
        let connection = self.connection(to).await?;
        let (send, recv) = connection
            .open_bi()
            .await
            .map_err(|_| Error::Unreachable(to.clone()))?;
        self.opened.inc();
        Ok(Carried { send, recv })
    }

    async fn accept(&self) -> Option<(PublicKey, Carried)> {
        self.accepted.lock().await.recv().await
    }

    async fn send_datagram(&self, to: &PublicKey, frame: Bytes) -> Result<(), Error> {
        let connection = self.connection(to).await?;
        let most = connection.max_datagram_size().unwrap_or(0);
        let fits = frame.len() <= most;
        if !fits {
            return Err(Error::Oversized(frame.len()));
        }
        connection
            .send_datagram(frame)
            .map_err(|_| Error::NoDatagrams(to.clone()))
    }

    async fn recv_datagram(&self) -> Option<(PublicKey, Bytes)> {
        self.datagrams.lock().await.recv().await
    }
}

#[cfg(test)]
mod tests {
    use commonware_cryptography::Signer as _;

    use super::*;

    #[test]
    fn of_two_connections_between_a_pair_both_ends_prefer_the_one_the_lower_key_dialed() {
        let one = PrivateKey::from_seed(1).public_key();
        let two = PrivateKey::from_seed(2).public_key();
        let (low, high) = if one < two { (one, two) } else { (two, one) };
        assert!(preferred(&low, &high, Side::Dialed));
        assert!(preferred(&high, &low, Side::Accepted));
        assert!(!preferred(&high, &low, Side::Dialed));
        assert!(!preferred(&low, &high, Side::Accepted));
    }
}
