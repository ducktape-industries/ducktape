//! How a node reaches another node's tcp, tls, ws or wss endpoint and how it accepts tls.
//! The consensus mesh's bridges and halo's tcp links both dial through here.

use std::io;
use std::sync::Arc;

use futures::{Sink, SinkExt as _, Stream, StreamExt as _};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use crate::endpoint::{Endpoint, Scheme};

const CHUNK: usize = 64 << 10;

pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
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
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, signed, &self.0)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, signed, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}

pub fn dialer() -> Result<Arc<ClientConfig>, rustls::Error> {
    let provider = provider();
    let carrier = Carrier(provider.signature_verification_algorithms);
    let client = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(carrier))
        .with_no_client_auth();
    Ok(Arc::new(client))
}

pub fn acceptor() -> Result<TlsAcceptor, rustls::Error> {
    let certified = rcgen::generate_simple_self_signed(vec!["ducktape".to_string()])
        .map_err(|error| rustls::Error::General(error.to_string()))?;
    let key = PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let server = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![certified.cert.der().clone()], key.into())?;
    Ok(TlsAcceptor::from(Arc::new(server)))
}

pub enum Remote {
    Tcp(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
    WebSocket(Box<WebSocketStream<MaybeTlsStream<TcpStream>>>),
}

pub async fn open(
    endpoint: &Endpoint,
    client: Arc<ClientConfig>,
    route: &str,
) -> io::Result<Remote> {
    match endpoint.scheme {
        Scheme::Tcp => tcp(endpoint).await.map(Remote::Tcp),
        Scheme::Tls => tls(endpoint, client)
            .await
            .map(|stream| Remote::Tls(Box::new(stream))),
        Scheme::Ws | Scheme::Wss => websocket(endpoint, client, route)
            .await
            .map(|socket| Remote::WebSocket(Box::new(socket))),
    }
}

async fn tcp(endpoint: &Endpoint) -> io::Result<TcpStream> {
    let stream = TcpStream::connect((endpoint.host.as_str(), endpoint.port)).await?;
    stream.set_nodelay(true)?;
    Ok(stream)
}

async fn tls(
    endpoint: &Endpoint,
    client: Arc<ClientConfig>,
) -> io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let name = ServerName::try_from(endpoint.host.clone()).map_err(io::Error::other)?;
    let stream = tcp(endpoint).await?;
    TlsConnector::from(client).connect(name, stream).await
}

async fn websocket(
    endpoint: &Endpoint,
    client: Arc<ClientConfig>,
    route: &str,
) -> io::Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
    let url = format!(
        "{}://{}{}",
        endpoint.scheme.name(),
        endpoint.authority(),
        route
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

pub async fn pipe(
    mut near: impl AsyncRead + AsyncWrite + Unpin,
    mut far: impl AsyncRead + AsyncWrite + Unpin,
) -> io::Result<()> {
    tokio::io::copy_bidirectional(&mut near, &mut far).await?;
    Ok(())
}

pub enum Frame {
    Data(Vec<u8>),
    Control,
    Closed,
}

pub fn tungstenite_frame(message: tungstenite::Message) -> Frame {
    use tungstenite::Message;
    match message {
        Message::Binary(bytes) => Frame::Data(bytes),
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Frame::Control,
        Message::Text(_) | Message::Close(_) => Frame::Closed,
    }
}

pub async fn splice<S, M, E>(
    socket: S,
    bytes: impl AsyncRead + AsyncWrite,
    wrap: fn(Vec<u8>) -> M,
    frame: fn(M) -> Frame,
) -> io::Result<()>
where
    S: Sink<M, Error = E> + Stream<Item = Result<M, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    let (mut to_peer, mut from_peer) = socket.split();
    let (mut from_near, mut to_near) = tokio::io::split(bytes);
    let sending = async {
        let mut buffer = vec![0; CHUNK];
        loop {
            let read = from_near.read(&mut buffer).await?;
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
                Frame::Data(bytes) => to_near.write_all(&bytes).await?,
                Frame::Control => {}
                Frame::Closed => break,
            }
        }
        to_near.shutdown().await
    };
    futures::pin_mut!(sending, receiving);
    futures::future::select(sending, receiving)
        .await
        .factor_first()
        .0
}
