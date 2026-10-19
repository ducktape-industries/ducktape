//! How mesh connections reach other nodes. The mesh dials each member at a loopback bridge
//! that carries the bytes to the member's tcp, tls, ws or wss endpoint; tls and websocket
//! connections that arrive at this node are carried into the mesh's own listener.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use futures::{Sink, SinkExt as _, Stream, StreamExt as _};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::AbortHandle;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use crate::endpoint::{Endpoint, Scheme};
use crate::wire::route;
use crate::{Error, Result};

const CHUNK: usize = 64 << 10;
const OPEN: Duration = Duration::from_secs(5);
const ACCEPT_BACKOFF: Duration = Duration::from_secs(1);

pub fn loopback(listen: SocketAddr) -> SocketAddr {
    let ip = match listen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, listen.port())
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn tls_error(error: impl std::fmt::Display) -> Error {
    Error::Tls(error.to_string())
}

#[derive(Debug)]
struct Carrier(WebPkiSupportedAlgorithms);

impl ServerCertVerifier for Carrier {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, signed, &self.0)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, signed, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}

/// A bound listener as tokio's, which accepts on it.
pub fn served(listener: std::net::TcpListener) -> io::Result<TcpListener> {
    listener.set_nonblocking(true)?;
    TcpListener::from_std(listener)
}

pub fn dialer() -> Result<Arc<ClientConfig>> {
    let provider = provider();
    let carrier = Carrier(provider.signature_verification_algorithms);
    let client = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(tls_error)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(carrier))
        .with_no_client_auth();
    Ok(Arc::new(client))
}

pub fn acceptor() -> Result<TlsAcceptor> {
    let certified =
        rcgen::generate_simple_self_signed(vec!["ducktape".to_string()]).map_err(tls_error)?;
    let key = PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let server = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(tls_error)?
        .with_no_client_auth()
        .with_single_cert(vec![certified.cert.der().clone()], key.into())
        .map_err(tls_error)?;
    Ok(TlsAcceptor::from(Arc::new(server)))
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

enum Remote {
    Tcp(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
    WebSocket(Box<WebSocketStream<MaybeTlsStream<TcpStream>>>),
}

async fn carry(
    mut mesh: TcpStream,
    endpoint: &Endpoint,
    client: Arc<ClientConfig>,
) -> io::Result<()> {
    mesh.set_nodelay(true)?;
    let (remote, early) = opened(&mut mesh, endpoint, client).await?;
    match remote {
        Remote::Tcp(mut stream) => {
            stream.write_all(&early).await?;
            pipe(mesh, stream).await
        }
        Remote::Tls(mut stream) => {
            stream.write_all(&early).await?;
            pipe(mesh, stream).await
        }
        Remote::WebSocket(mut socket) => {
            let any_early = !early.is_empty();
            if any_early {
                socket
                    .send(tungstenite::Message::Binary(early))
                    .await
                    .map_err(io::Error::other)?;
            }
            splice(
                socket,
                mesh,
                tungstenite::Message::Binary,
                tungstenite_frame,
            )
            .await
        }
    }
}

async fn opened(
    mesh: &mut TcpStream,
    endpoint: &Endpoint,
    client: Arc<ClientConfig>,
) -> io::Result<(Remote, Vec<u8>)> {
    let mut early = Vec::new();
    let open = open(endpoint, client);
    futures::pin_mut!(open);
    loop {
        tokio::select! {
            remote = &mut open => return Ok((remote?, early)),
            read = mesh.read_buf(&mut early) => {
                let gone = read? == 0;
                if gone {
                    return Err(io::Error::from(io::ErrorKind::ConnectionAborted));
                }
            }
        }
    }
}

async fn open(endpoint: &Endpoint, client: Arc<ClientConfig>) -> io::Result<Remote> {
    match endpoint.scheme {
        Scheme::Tcp => dial(endpoint).await.map(Remote::Tcp),
        Scheme::Tls => tls(endpoint, client)
            .await
            .map(|stream| Remote::Tls(Box::new(stream))),
        Scheme::Ws | Scheme::Wss => websocket(endpoint, client)
            .await
            .map(|socket| Remote::WebSocket(Box::new(socket))),
    }
}

async fn dial(endpoint: &Endpoint) -> io::Result<TcpStream> {
    let stream = TcpStream::connect((endpoint.host.as_str(), endpoint.port)).await?;
    stream.set_nodelay(true)?;
    Ok(stream)
}

async fn tls(
    endpoint: &Endpoint,
    client: Arc<ClientConfig>,
) -> io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let name = ServerName::try_from(endpoint.host.clone()).map_err(io::Error::other)?;
    let stream = dial(endpoint).await?;
    TlsConnector::from(client).connect(name, stream).await
}

async fn websocket(
    endpoint: &Endpoint,
    client: Arc<ClientConfig>,
) -> io::Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
    let url = format!(
        "{}://{}{}",
        endpoint.scheme.name(),
        endpoint.authority(),
        route::P2P
    );
    let (socket, _) = tokio_tungstenite::connect_async_tls_with_config(
        url,
        None,
        true,
        Some(Connector::Rustls(client)),
    )
    .await
    .map_err(io::Error::other)?;
    Ok(socket)
}

async fn pipe(
    mut mesh: TcpStream,
    mut remote: impl AsyncRead + AsyncWrite + Unpin,
) -> io::Result<()> {
    tokio::io::copy_bidirectional(&mut mesh, &mut remote).await?;
    Ok(())
}

pub async fn inbound_tls(listener: TcpListener, acceptor: TlsAcceptor, mesh: SocketAddr) {
    loop {
        let remote = accepted(&listener).await;
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let carried = async {
                remote.set_nodelay(true)?;
                let remote = tokio::time::timeout(OPEN, acceptor.accept(remote))
                    .await
                    .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
                let local = TcpStream::connect(mesh).await?;
                local.set_nodelay(true)?;
                pipe(local, remote).await
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
        splice(socket, local, axum_binary, axum_frame).await
    };
    if let Err(error) = carried.await {
        tracing::debug!(target: "ducktape::mesh", %error, "a websocket connection ended");
    }
}

enum Frame {
    Data(Vec<u8>),
    Control,
    Closed,
}

fn tungstenite_frame(message: tungstenite::Message) -> Frame {
    use tungstenite::Message;
    match message {
        Message::Binary(bytes) => Frame::Data(bytes),
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Frame::Control,
        Message::Text(_) | Message::Close(_) => Frame::Closed,
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

async fn splice<S, M, E>(
    socket: S,
    tcp: TcpStream,
    wrap: fn(Vec<u8>) -> M,
    frame: fn(M) -> Frame,
) -> io::Result<()>
where
    S: Sink<M, Error = E> + Stream<Item = std::result::Result<M, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    let (mut to_peer, mut from_peer) = socket.split();
    let (mut from_mesh, mut to_mesh) = tcp.into_split();
    let sending = async {
        let mut buffer = vec![0; CHUNK];
        loop {
            let read = from_mesh.read(&mut buffer).await?;
            if read == 0 {
                return to_peer.close().await.map_err(io::Error::other);
            }
            to_peer
                .send(wrap(buffer[..read].to_vec()))
                .await
                .map_err(io::Error::other)?;
        }
    };
    let receiving = async {
        while let Some(message) = from_peer.next().await {
            match frame(message.map_err(io::Error::other)?) {
                Frame::Data(bytes) => to_mesh.write_all(&bytes).await?,
                Frame::Control => {}
                Frame::Closed => break,
            }
        }
        to_mesh.shutdown().await
    };
    futures::pin_mut!(sending, receiving);
    futures::future::select(sending, receiving)
        .await
        .factor_first()
        .0
}

#[cfg(test)]
mod tests;
