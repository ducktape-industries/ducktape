//! The real medium: QUIC on UDP at the halo address each member registers, one connection per
//! peer with a stream per open. A connection either end opened carries streams both ways, so
//! a member no one can reach still takes part by dialing out.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use bytes::Bytes;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_runtime::Metrics;
use commonware_runtime::telemetry::metrics::Registered;
use commonware_runtime::telemetry::metrics::raw::Counter;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;

use crate::members::Members;
use crate::quic::{self, Quic};
use crate::transport::{Error, Transport};

const DIAL: Duration = Duration::from_secs(5);
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

#[derive(Default)]
struct Peer {
    dialing: tokio::sync::Mutex<()>,
    connection: Mutex<Option<quinn::Connection>>,
}

impl Peer {
    fn held(&self) -> std::sync::MutexGuard<'_, Option<quinn::Connection>> {
        self.connection
            .lock()
            .expect("the connection lock is never poisoned")
    }

    fn live(&self) -> Option<quinn::Connection> {
        let held = self.held();
        let connection = held.as_ref()?;
        let closed = connection.close_reason().is_some();
        if closed {
            return None;
        }
        Some(connection.clone())
    }

    fn adopt(&self, connection: &quinn::Connection) {
        let mut held = self.held();
        let live = held
            .as_ref()
            .is_some_and(|connection| connection.close_reason().is_none());
        if !live {
            *held = Some(connection.clone());
        }
    }

    fn forget(&self, connection: &quinn::Connection) {
        let mut held = self.held();
        let same = held
            .as_ref()
            .is_some_and(|kept| kept.stable_id() == connection.stable_id());
        if same {
            *held = None;
        }
    }
}

pub struct Links {
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
        if let Some(connection) = peer.live() {
            return Ok(connection);
        }
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
            tracing::debug!(target: "ducktape::halo", peer = %to, reason = "undialed", "halo did not reach a member at its halo address");
            return Err(Error::Unreachable(to.clone()));
        };
        peer.adopt(&connection);
        tokio::spawn(carrying(
            Arc::downgrade(self),
            to.clone(),
            connection.clone(),
        ));
        Ok(connection)
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

async fn accepting(links: std::sync::Weak<Links>, endpoint: quinn::Endpoint) {
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
            links.peer(&peer).adopt(&connection);
            carrying(Arc::downgrade(&links), peer, connection).await;
        });
    }
}

async fn carrying(links: std::sync::Weak<Links>, peer: PublicKey, connection: quinn::Connection) {
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
