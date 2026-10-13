//! The real medium. halo reaches a member whose endpoint is tcp:// or tls:// over QUIC on UDP
//! at the same host and port, one connection per peer with a stream per open; where UDP does
//! not answer in time, or the endpoint is ws:// or wss://, it opens a tcp path per stream. A
//! silent UDP path is remembered per peer and tried again later. Only QUIC carries datagrams.

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
use rustls::ClientConfig;
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::endpoint::{Endpoint, Scheme};
use crate::members::Members;
use crate::quic::{self, Quic};
use crate::tcp::{self, Bytes as Conn, Context, Handshake};
use crate::transport::{Error, Transport};

const QUIC_ANSWER: Duration = Duration::from_secs(2);
const QUIC_RETRY: Duration = Duration::from_secs(60);
const INBOUND: usize = 256;
const DATAGRAMS: usize = 1024;
const HANDSHAKES: usize = 64;

pub enum Carried {
    Quic(quinn::SendStream, quinn::RecvStream),
    Tcp(DuplexStream),
}

impl AsyncRead for Carried {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Carried::Quic(_, recv) => Pin::new(recv).poll_read(cx, buf),
            Carried::Tcp(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Carried {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Carried::Quic(send, _) => Pin::new(send).poll_write(cx, buf).map_err(io::Error::from),
            Carried::Tcp(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Carried::Quic(send, _) => Pin::new(send).poll_flush(cx),
            Carried::Tcp(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Carried::Quic(send, _) => Pin::new(send).poll_shutdown(cx),
            Carried::Tcp(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

#[derive(Clone, Default)]
enum Path {
    #[default]
    Untried,
    Quic(quinn::Connection),
    Tcp {
        until: Instant,
        attempts: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Untried,
    Quic,
    Tcp { attempts: u32 },
}

#[derive(Debug, PartialEq, Eq)]
enum Change {
    FellToTcp { attempts: u32 },
    StillTcp { attempts: u32 },
    BackToQuic,
}

impl Path {
    fn kind(&self) -> Kind {
        match self {
            Path::Untried => Kind::Untried,
            Path::Quic(_) => Kind::Quic,
            Path::Tcp { attempts, .. } => Kind::Tcp {
                attempts: *attempts,
            },
        }
    }

    fn attempts(&self) -> u32 {
        match self {
            Path::Tcp { attempts, .. } => *attempts,
            Path::Untried | Path::Quic(_) => 0,
        }
    }
}

fn change(before: Kind, after: Kind) -> Option<Change> {
    match (before, after) {
        (Kind::Untried | Kind::Quic, Kind::Tcp { attempts }) => {
            Some(Change::FellToTcp { attempts })
        }
        (Kind::Tcp { .. }, Kind::Tcp { attempts }) => Some(Change::StillTcp { attempts }),
        (Kind::Tcp { .. }, Kind::Quic) => Some(Change::BackToQuic),
        (Kind::Untried | Kind::Quic, Kind::Quic) => None,
        (Kind::Untried | Kind::Quic | Kind::Tcp { .. }, Kind::Untried) => None,
    }
}

fn noted(peer: &PublicKey, before: &Path, after: &Path) {
    let Some(change) = change(before.kind(), after.kind()) else {
        return;
    };
    match change {
        Change::FellToTcp { attempts } => {
            tracing::info!(target: "ducktape::halo", event = "halo_path", %peer, path = "tcp", reason = "udp_silent", attempts, "halo reaches a member over tcp: its udp did not answer");
        }
        Change::StillTcp { attempts } => {
            tracing::debug!(target: "ducktape::halo", event = "halo_path", %peer, path = "tcp", reason = "udp_silent", attempts, "a member's udp still does not answer");
        }
        Change::BackToQuic => {
            tracing::info!(target: "ducktape::halo", event = "halo_path", %peer, path = "quic", reason = "udp_answered", "halo reaches a member over QUIC again");
        }
    }
}

#[derive(Default)]
struct Peer {
    dialing: tokio::sync::Mutex<()>,
    path: Mutex<Path>,
}

impl Peer {
    fn path(&self) -> Path {
        self.path
            .lock()
            .expect("the path lock is never poisoned")
            .clone()
    }

    fn replace(&self, path: Path) -> Path {
        std::mem::replace(
            &mut *self.path.lock().expect("the path lock is never poisoned"),
            path,
        )
    }

    fn live(&self) -> Option<quinn::Connection> {
        let Path::Quic(connection) = self.path() else {
            return None;
        };
        let closed = connection.close_reason().is_some();
        if closed {
            return None;
        }
        Some(connection)
    }

    fn adopt(&self, connection: &quinn::Connection) -> Option<Path> {
        let mut path = self.path.lock().expect("the path lock is never poisoned");
        let live = matches!(&*path, Path::Quic(held) if held.close_reason().is_none());
        if live {
            return None;
        }
        Some(std::mem::replace(
            &mut *path,
            Path::Quic(connection.clone()),
        ))
    }

    fn forget(&self, connection: &quinn::Connection) {
        let mut path = self.path.lock().expect("the path lock is never poisoned");
        let same = matches!(&*path, Path::Quic(held) if held.stable_id() == connection.stable_id());
        if same {
            *path = Path::Untried;
        }
    }
}

struct Opened {
    quic: Registered<Counter>,
    tcp: Registered<Counter>,
}

pub struct Links<E: Context> {
    context: E,
    handshake: Handshake,
    members: Arc<Members>,
    quic: Option<Quic>,
    client: Arc<ClientConfig>,
    peers: Mutex<HashMap<PublicKey, Arc<Peer>>>,
    inbound: mpsc::Sender<(PublicKey, Carried)>,
    accepted: tokio::sync::Mutex<mpsc::Receiver<(PublicKey, Carried)>>,
    received: mpsc::Sender<(PublicKey, Bytes)>,
    datagrams: tokio::sync::Mutex<mpsc::Receiver<(PublicKey, Bytes)>>,
    opened: Opened,
    handshakes: Arc<tokio::sync::Semaphore>,
}

impl<E: Context + Metrics> Links<E> {
    pub fn start(
        context: E,
        identity: PrivateKey,
        namespace: &[u8],
        members: Arc<Members>,
        udp: &[SocketAddr],
    ) -> Result<Arc<Links<E>>, rustls::Error> {
        let quic = match Quic::bind(udp, &identity, members.clone()) {
            Ok(quic) => Some(quic),
            Err(error) => {
                tracing::warn!(target: "ducktape::halo", event = "halo_quic_unbound", %error, reason = "udp_unbound", "halo runs without QUIC: it could not bind its udp ports");
                None
            }
        };
        let (inbound, accepted) = mpsc::channel(INBOUND);
        let (received, datagrams) = mpsc::channel(DATAGRAMS);
        let opened = Opened {
            quic: context.register(
                "halo_streams_over_quic",
                "streams this node opened to a member over QUIC",
                Counter::default(),
            ),
            tcp: context.register(
                "halo_streams_over_tcp",
                "streams this node opened to a member over tcp, tls, ws or wss",
                Counter::default(),
            ),
        };
        let links = Arc::new(Links {
            context,
            handshake: Handshake::new(identity, namespace),
            members,
            quic,
            client: crate::dial::dialer()?,
            peers: Mutex::default(),
            inbound,
            accepted: tokio::sync::Mutex::new(accepted),
            received,
            datagrams: tokio::sync::Mutex::new(datagrams),
            handshakes: Arc::new(tokio::sync::Semaphore::new(HANDSHAKES)),
            opened,
        });
        let endpoints = links
            .quic
            .as_ref()
            .map(|quic| quic.endpoints().to_vec())
            .unwrap_or_default();
        for endpoint in endpoints {
            tokio::spawn(accepting(Arc::downgrade(&links), endpoint));
        }
        Ok(links)
    }

    pub fn udp(&self) -> Vec<SocketAddr> {
        self.quic.as_ref().map(Quic::local).unwrap_or_default()
    }

    pub fn inbound(self: &Arc<Self>, conn: impl Conn) {
        let Ok(handshaking) = self.handshakes.clone().try_acquire_owned() else {
            tracing::debug!(target: "ducktape::halo", reason = "handshakes_full", "halo dropped a tcp path");
            return;
        };
        let links = self.clone();
        tokio::spawn(async move {
            let listened = tcp::listen(
                &links.context,
                &links.handshake,
                links.members.clone(),
                conn,
            )
            .await;
            drop(handshaking);
            let (peer, stream) = match listened {
                Ok(listened) => listened,
                Err(error) => {
                    tracing::debug!(target: "ducktape::halo", %error, reason = "handshake_failed", "halo refused a tcp path");
                    return;
                }
            };
            let _ = links.inbound.send((peer, Carried::Tcp(stream))).await;
        });
    }

    fn peer(&self, key: &PublicKey) -> Arc<Peer> {
        self.peers
            .lock()
            .expect("the peers lock is never poisoned")
            .entry(key.clone())
            .or_default()
            .clone()
    }

    fn quic_reaches(&self, peer: &Peer, endpoint: &Endpoint) -> bool {
        let bound = self.quic.is_some();
        let udp_scheme = matches!(endpoint.scheme, Scheme::Tcp | Scheme::Tls);
        let resting = matches!(peer.path(), Path::Tcp { until, .. } if Instant::now() < until);
        bound && udp_scheme && !resting
    }

    async fn quic_to(self: &Arc<Self>, to: &PublicKey) -> Option<quinn::Connection> {
        let peer = self.peer(to);
        if let Some(connection) = peer.live() {
            return Some(connection);
        }
        let endpoint = self.members.endpoint(to)?;
        if !self.quic_reaches(&peer, &endpoint) {
            return None;
        }
        let _dialing = peer.dialing.lock().await;
        if let Some(connection) = peer.live() {
            return Some(connection);
        }
        let quic = self.quic.as_ref()?;
        let dialed = tokio::time::timeout(QUIC_ANSWER, async {
            let at = resolved(quic, &endpoint).await?;
            quic.dial(to, at).await.ok()
        })
        .await;
        let Ok(Some(connection)) = dialed else {
            let tcp = Path::Tcp {
                until: Instant::now() + QUIC_RETRY,
                attempts: peer.path().attempts() + 1,
            };
            let before = peer.replace(tcp.clone());
            noted(to, &before, &tcp);
            return None;
        };
        let quic = Path::Quic(connection.clone());
        let before = peer.replace(quic.clone());
        noted(to, &before, &quic);
        tokio::spawn(carrying(
            Arc::downgrade(self),
            to.clone(),
            connection.clone(),
        ));
        Some(connection)
    }
}

async fn resolved(quic: &Quic, endpoint: &Endpoint) -> Option<SocketAddr> {
    let mut found = tokio::net::lookup_host((endpoint.host.as_str(), endpoint.port))
        .await
        .ok()?;
    found.find(|at| quic.reaches(at))
}

async fn accepting<E: Context + Metrics>(
    links: std::sync::Weak<Links<E>>,
    endpoint: quinn::Endpoint,
) {
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
            let adopted = links.peer(&peer).adopt(&connection);
            if let Some(before) = adopted {
                noted(&peer, &before, &Path::Quic(connection.clone()));
            }
            carrying(Arc::downgrade(&links), peer, connection).await;
        });
    }
}

async fn carrying<E: Context + Metrics>(
    links: std::sync::Weak<Links<E>>,
    peer: PublicKey,
    connection: quinn::Connection,
) {
    let streams = async {
        while let Ok((send, recv)) = connection.accept_bi().await {
            let Some(links) = links.upgrade() else {
                return;
            };
            let _ = links
                .inbound
                .send((peer.clone(), Carried::Quic(send, recv)))
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

impl<E: Context + Metrics> Transport for Arc<Links<E>> {
    type Stream = Carried;

    async fn connect(&self, to: &PublicKey) -> Result<Carried, Error> {
        if let Some(connection) = self.quic_to(to).await {
            let opened = connection.open_bi().await;
            if let Ok((send, recv)) = opened {
                self.opened.quic.inc();
                return Ok(Carried::Quic(send, recv));
            }
        }
        let Some(endpoint) = self.members.endpoint(to) else {
            return Err(Error::Unreachable(to.clone()));
        };
        let stream = tcp::dial(
            &self.context,
            &self.handshake,
            self.client.clone(),
            &endpoint,
            to.clone(),
        )
        .await?;
        self.opened.tcp.inc();
        Ok(Carried::Tcp(stream))
    }

    async fn accept(&self) -> Option<(PublicKey, Carried)> {
        self.accepted.lock().await.recv().await
    }

    async fn send_datagram(&self, to: &PublicKey, frame: Bytes) -> Result<(), Error> {
        let Some(connection) = self.quic_to(to).await else {
            return Err(Error::NoDatagrams(to.clone()));
        };
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
    use super::*;

    #[test]
    fn only_a_change_of_path_is_news_and_a_tcp_retry_counts_its_attempts() {
        let tcp = |attempts| Kind::Tcp { attempts };
        assert_eq!(
            change(Kind::Untried, tcp(1)),
            Some(Change::FellToTcp { attempts: 1 })
        );
        assert_eq!(
            change(Kind::Quic, tcp(1)),
            Some(Change::FellToTcp { attempts: 1 })
        );
        assert_eq!(
            change(tcp(1), tcp(2)),
            Some(Change::StillTcp { attempts: 2 })
        );
        assert_eq!(change(tcp(9), Kind::Quic), Some(Change::BackToQuic));
        assert_eq!(change(Kind::Untried, Kind::Quic), None);
        assert_eq!(change(Kind::Quic, Kind::Untried), None);
    }
}
