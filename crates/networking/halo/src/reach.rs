//! How halo reaches a member over HTTPS: a pipe of packets on its WebSocket `/v1/halo`, and the
//! address its `/v1/seen` saw this process call from. The QUIC inside admits by node key, so the
//! outer TLS checks no certificate.

use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures::{SinkExt as _, StreamExt as _};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::USER_AGENT;

use crate::relay::Pipe;

pub const RELAY_ROUTE: &str = "/v1/halo";
pub const SEEN_ROUTE: &str = "/v1/seen";
const AGENT: &str = "ducktape-halo";

pub type Reached<T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send>>;

pub trait Reach: Send + Sync + 'static {
    fn relay(&self, https: &str) -> Reached<Pipe>;

    fn seen(&self, https: &str, udp: SocketAddr) -> Reached<IpAddr>;
}

pub struct Https;

impl Https {
    pub fn shared() -> Arc<dyn Reach> {
        Arc::new(Https)
    }
}

impl Reach for Https {
    fn relay(&self, https: &str) -> Reached<Pipe> {
        let url = format!("{}{RELAY_ROUTE}", https.trim_end_matches('/'));
        Box::pin(async move {
            let mut request = url.into_client_request().map_err(io::Error::other)?;
            request
                .headers_mut()
                .insert(USER_AGENT, HeaderValue::from_static(AGENT));
            let connector = tokio_tungstenite::Connector::Rustls(unverified()?);
            let (socket, _) = tokio_tungstenite::connect_async_tls_with_config(
                request,
                None,
                true,
                Some(connector),
            )
            .await
            .map_err(io::Error::other)?;
            let (sink, stream) = socket.split();
            let send = sink.sink_map_err(io::Error::other).with(|packet: Bytes| {
                futures::future::ready(Ok::<Message, io::Error>(Message::Binary(packet.to_vec())))
            });
            let recv = stream.filter_map(|message| {
                futures::future::ready(match message {
                    Ok(Message::Binary(packet)) => Some(Ok(Bytes::from(packet))),
                    Ok(Message::Text(_) | Message::Close(_)) => {
                        Some(Err(io::ErrorKind::ConnectionAborted.into()))
                    }
                    Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => None,
                    Err(error) => Some(Err(io::Error::other(error))),
                })
            });
            Ok(Pipe {
                send: Box::pin(send),
                recv: Box::pin(recv),
            })
        })
    }

    fn seen(&self, https: &str, udp: SocketAddr) -> Reached<IpAddr> {
        let url = format!("{}{SEEN_ROUTE}", web(https));
        let from: IpAddr = match udp {
            SocketAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
            SocketAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
        };
        Box::pin(async move {
            let client = reqwest::Client::builder()
                .local_address(from)
                .danger_accept_invalid_certs(true)
                .user_agent(AGENT)
                .build()
                .map_err(io::Error::other)?;
            let answer = client
                .get(url)
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .map_err(io::Error::other)?
                .text()
                .await
                .map_err(io::Error::other)?;
            answer.trim().parse().map_err(io::Error::other)
        })
    }
}

fn web(https: &str) -> String {
    let trimmed = https.trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("wss://") {
        return format!("https://{rest}");
    }
    if let Some(rest) = trimmed.strip_prefix("ws://") {
        return format!("http://{rest}");
    }
    trimmed.to_owned()
}

#[derive(Debug)]
struct Unverified(WebPkiSupportedAlgorithms);

impl ServerCertVerifier for Unverified {
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

fn unverified() -> io::Result<Arc<ClientConfig>> {
    let provider: Arc<CryptoProvider> = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Unverified(provider.signature_verification_algorithms);
    let client = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(Arc::new(client))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_websocket_address_is_asked_over_its_own_web_scheme() {
        assert_eq!(
            web("wss://duckpond.example:443"),
            "https://duckpond.example:443"
        );
        assert_eq!(web("ws://127.0.0.1:8080/"), "http://127.0.0.1:8080");
    }
}
