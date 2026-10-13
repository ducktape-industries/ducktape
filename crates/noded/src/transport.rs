//! How connections reach other nodes and arrive at this one. The mesh dials each member at a
//! loopback bridge that carries the bytes to the member's tcp, tls, ws or wss endpoint. A
//! connection that arrives at the p2p or tls port goes to halo when it opens with halo's
//! preamble and to the mesh's own loopback listener otherwise; a websocket goes by its route.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use commonware_runtime::Metrics;
use halo::dial::{self, Frame, Remote};
use halo::{Endpoint, Halo, Sniffed};
use rustls::ClientConfig;
use tokio::io::AsyncWriteExt as _;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::AbortHandle;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite;

use crate::wire::route;

const CHUNK: usize = 64 << 10;
const OPEN: Duration = Duration::from_secs(5);
const ACCEPT_BACKOFF: Duration = Duration::from_secs(1);

pub fn internal() -> io::Result<SocketAddr> {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    listener.local_addr()
}

pub struct Bridges {
    client: Arc<ClientConfig>,
    open: HashMap<Endpoint, Bridge>,
}

struct Bridge {
    local: SocketAddr,
    accepting: AbortHandle,
}

impl Bridges {
    pub fn new(client: Arc<ClientConfig>) -> Bridges {
        Bridges {
            client,
            open: HashMap::new(),
        }
    }

    pub fn keep(&mut self, endpoints: &HashSet<Endpoint>) {
        self.open.retain(|endpoint, bridge| {
            let kept = endpoints.contains(endpoint);
            if !kept {
                bridge.accepting.abort();
            }
            kept
        });
    }

    pub fn local(&mut self, endpoint: &Endpoint) -> io::Result<SocketAddr> {
        if let Some(bridge) = self.open.get(endpoint) {
            return Ok(bridge.local);
        }
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let local = listener.local_addr()?;
        let listener = TcpListener::from_std(listener)?;
        let accepting =
            tokio::spawn(outbound(listener, endpoint.clone(), self.client.clone())).abort_handle();
        self.open
            .insert(endpoint.clone(), Bridge { local, accepting });
        Ok(local)
    }
}

impl Drop for Bridges {
    fn drop(&mut self) {
        for bridge in self.open.values() {
            bridge.accepting.abort();
        }
    }
}

async fn accepted(listener: &TcpListener) -> TcpStream {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => return stream,
            Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
        }
    }
}

async fn outbound(listener: TcpListener, endpoint: Endpoint, client: Arc<ClientConfig>) {
    loop {
        let mesh = accepted(&listener).await;
        let endpoint = endpoint.clone();
        let client = client.clone();
        tokio::spawn(async move {
            if let Err(error) = carry(mesh, &endpoint, client).await {
                tracing::debug!(
                    target: "ducktape::mesh",
                    %endpoint,
                    %error,
                    "a bridged connection ended"
                );
            }
        });
    }
}

async fn carry(mesh: TcpStream, endpoint: &Endpoint, client: Arc<ClientConfig>) -> io::Result<()> {
    mesh.set_nodelay(true)?;
    let remote = tokio::time::timeout(OPEN, dial::open(endpoint, client, route::P2P))
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    match remote {
        Remote::Tcp(stream) => dial::pipe(mesh, stream).await,
        Remote::Tls(stream) => dial::pipe(mesh, stream).await,
        Remote::WebSocket(socket) => {
            dial::splice(
                socket,
                mesh,
                tungstenite::Message::Binary,
                dial::tungstenite_frame,
            )
            .await
        }
    }
}

async fn routed<E: halo::Context + Metrics>(
    conn: impl halo::Bytes,
    halo: &Halo<E>,
    mesh: SocketAddr,
) -> io::Result<()> {
    let sniffed = tokio::time::timeout(OPEN, halo::sniffed(conn))
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    let (conn, first) = match sniffed {
        Sniffed::Halo(conn) => {
            halo.inbound(conn);
            return Ok(());
        }
        Sniffed::Other { conn, first } => (conn, first),
    };
    let mut local = TcpStream::connect(mesh).await?;
    local.set_nodelay(true)?;
    local.write_all(&[first]).await?;
    dial::pipe(local, conn).await
}

pub async fn inbound_p2p<E: halo::Context + Metrics>(
    listener: TcpListener,
    halo: Arc<Halo<E>>,
    mesh: SocketAddr,
) {
    loop {
        let remote = accepted(&listener).await;
        let halo = halo.clone();
        tokio::spawn(async move {
            let carried = async {
                remote.set_nodelay(true)?;
                routed(remote, &halo, mesh).await
            };
            if let Err(error) = carried.await {
                tracing::debug!(target: "ducktape::mesh", %error, "a p2p connection ended");
            }
        });
    }
}

pub async fn inbound_tls<E: halo::Context + Metrics>(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    halo: Arc<Halo<E>>,
    mesh: SocketAddr,
) {
    loop {
        let remote = accepted(&listener).await;
        let acceptor = acceptor.clone();
        let halo = halo.clone();
        tokio::spawn(async move {
            let carried = async {
                remote.set_nodelay(true)?;
                let remote = tokio::time::timeout(OPEN, acceptor.accept(remote))
                    .await
                    .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
                routed(remote, &halo, mesh).await
            };
            if let Err(error) = carried.await {
                tracing::debug!(target: "ducktape::mesh", %error, "a tls connection ended");
            }
        });
    }
}

pub async fn inbound_websocket(socket: axum::extract::ws::WebSocket, mesh: SocketAddr) {
    let carried = async {
        let local = TcpStream::connect(mesh).await?;
        local.set_nodelay(true)?;
        dial::splice(socket, local, axum_binary, axum_frame).await
    };
    if let Err(error) = carried.await {
        tracing::debug!(target: "ducktape::mesh", %error, "a websocket connection ended");
    }
}

pub async fn inbound_halo<E: halo::Context + Metrics>(
    socket: axum::extract::ws::WebSocket,
    halo: Arc<Halo<E>>,
) {
    let (ours, theirs) = tokio::io::duplex(CHUNK);
    halo.inbound(ours);
    let carried = dial::splice(socket, theirs, axum_binary, axum_frame).await;
    if let Err(error) = carried {
        tracing::debug!(target: "ducktape::halo", %error, "a halo websocket ended");
    }
}

fn axum_binary(bytes: Vec<u8>) -> axum::extract::ws::Message {
    axum::extract::ws::Message::Binary(bytes.into())
}

fn axum_frame(message: axum::extract::ws::Message) -> Frame {
    use axum::extract::ws::Message;
    match message {
        Message::Binary(bytes) => Frame::Data(bytes.to_vec()),
        Message::Ping(_) | Message::Pong(_) => Frame::Control,
        Message::Text(_) | Message::Close(_) => Frame::Closed,
    }
}
