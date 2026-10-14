//! halo over QUIC. Each node presents a self-signed certificate made from its ed25519 node
//! key, and both ends admit the other only if that key is a current member; the dialer also
//! requires the key it meant to reach. The same socket takes a desktop client on the live
//! ALPN: it presents no certificate, so a member's certificate is asked for, never required,
//! and a connection without one is told apart by its ALPN.

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use commonware_codec::{DecodeExt as _, Encode as _};
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};

use crate::dial::provider;
use crate::members::Members;

pub const ALPN: &[u8] = b"ducktape-halo";
/// The server name every node's certificate carries and every dialer asks for.
pub const NAME: &str = "ducktape-halo";
const IDLE: Duration = Duration::from_secs(30);
const KEEP_ALIVE: Duration = Duration::from_secs(10);
const ED25519_PKCS8: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("tls: {0}")]
    Tls(#[from] rustls::Error),
    #[error("certificate: {0}")]
    Certificate(#[from] rcgen::Error),
    #[error("quic: {0}")]
    Quic(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

struct Certified {
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
}

fn certified(identity: &PrivateKey) -> Result<Certified, Error> {
    let mut pkcs8 = ED25519_PKCS8.to_vec();
    pkcs8.extend_from_slice(identity.encode().as_ref());
    let key = PrivatePkcs8KeyDer::from(pkcs8);
    let signing = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&key, &rcgen::PKCS_ED25519)?;
    let cert = rcgen::CertificateParams::new(vec![NAME.to_string()])?.self_signed(&signing)?;
    Ok(Certified {
        cert: cert.der().clone(),
        key,
    })
}

pub fn key_of(cert: &CertificateDer<'_>) -> Option<PublicKey> {
    let (_, parsed) = x509_parser::parse_x509_certificate(cert.as_ref()).ok()?;
    let spki = parsed.public_key();
    let ed25519 = spki.algorithm.algorithm == x509_parser::oid_registry::OID_SIG_ED25519;
    if !ed25519 {
        return None;
    }
    PublicKey::decode(spki.subject_public_key.data.as_ref()).ok()
}

pub fn peer_of(connection: &quinn::Connection) -> Option<PublicKey> {
    let identity = connection.peer_identity()?;
    let certs = identity.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    key_of(certs.first()?)
}

/// The ALPN a connection was accepted with; `None` only for a handshake that named none.
pub fn alpn_of(connection: &quinn::Connection) -> Option<Vec<u8>> {
    let data = connection.handshake_data()?;
    let data = data
        .downcast::<quinn::crypto::rustls::HandshakeData>()
        .ok()?;
    data.protocol
}

/// A client that is no member: it presents no certificate and admits the one node key it
/// means to reach. The shape a desktop client dials a node's live port with.
pub fn pinned_client(peer: &PublicKey, alpn: &[u8]) -> Result<quinn::ClientConfig, Error> {
    let pinned = Pinned {
        peer: peer.clone(),
        algorithms: provider().signature_verification_algorithms,
    };
    let mut crypto = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(pinned))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![alpn.to_vec()];
    let crypto =
        QuicClientConfig::try_from(crypto).map_err(|error| Error::Quic(error.to_string()))?;
    let mut client = quinn::ClientConfig::new(Arc::new(crypto));
    client.transport_config(transport());
    Ok(client)
}

#[derive(Debug)]
struct Pinned {
    peer: PublicKey,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let meant = key_of(cert).is_some_and(|key| key == self.peer);
        if !meant {
            return Err(rustls::Error::General(
                "the node key is not the one dialed".into(),
            ));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("halo speaks only tls 1.3".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, signed, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

struct Admit {
    members: Arc<Members>,
    expected: Option<PublicKey>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl fmt::Debug for Admit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Admit")
            .field("expected", &self.expected)
            .finish()
    }
}

impl Admit {
    fn admitted(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        let Some(key) = key_of(cert) else {
            return Err(rustls::Error::General(
                "the certificate holds no node key".into(),
            ));
        };
        let meant = self
            .expected
            .as_ref()
            .is_none_or(|expected| *expected == key);
        let member = self.members.admits(&key);
        if !(meant && member) {
            return Err(rustls::Error::General(
                "the node key is not a member".into(),
            ));
        }
        Ok(())
    }

    fn signed(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, signed, &self.algorithms)
    }
}

impl ServerCertVerifier for Admit {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.admitted(cert)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("halo speaks only tls 1.3".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.signed(message, cert, signed)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

impl ClientCertVerifier for Admit {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    // A desktop client on the live ALPN presents no certificate; a member that presents one
    // is still checked by `verify_client_cert`.
    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn verify_client_cert(
        &self,
        cert: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.admitted(cert)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("halo speaks only tls 1.3".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.signed(message, cert, signed)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

/// The transport both ends run: a connection dies after 30 s without a packet, so a dead peer
/// is forgotten within a block or two, and a keep-alive every 10 s holds an idle one open.
pub fn transport() -> Arc<quinn::TransportConfig> {
    let mut transport = quinn::TransportConfig::default();
    let idle = quinn::IdleTimeout::try_from(IDLE).expect("thirty seconds is a valid idle timeout");
    transport.max_idle_timeout(Some(idle));
    transport.keep_alive_interval(Some(KEEP_ALIVE));
    Arc::new(transport)
}

pub struct Quic {
    endpoints: Vec<quinn::Endpoint>,
    certified: Certified,
    members: Arc<Members>,
}

impl Quic {
    pub fn bind(
        addresses: &[SocketAddr],
        identity: &PrivateKey,
        members: Arc<Members>,
    ) -> Result<Quic, Error> {
        let certified = certified(identity)?;
        let admit = Admit {
            members: members.clone(),
            expected: None,
            algorithms: provider().signature_verification_algorithms,
        };
        let mut crypto = rustls::ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_client_cert_verifier(Arc::new(admit))
            .with_single_cert(
                vec![certified.cert.clone()],
                PrivateKeyDer::Pkcs8(certified.key.clone_key()),
            )?;
        crypto.alpn_protocols = vec![ALPN.to_vec(), live_wire::ALPN.to_vec()];
        let crypto =
            QuicServerConfig::try_from(crypto).map_err(|error| Error::Quic(error.to_string()))?;
        let mut server = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        server.transport_config(transport());
        let endpoints = addresses
            .iter()
            .map(|address| quinn::Endpoint::server(server.clone(), *address))
            .collect::<std::io::Result<Vec<_>>>()?;
        Ok(Quic {
            endpoints,
            certified,
            members,
        })
    }

    pub fn endpoints(&self) -> &[quinn::Endpoint] {
        &self.endpoints
    }

    pub fn local(&self) -> Vec<SocketAddr> {
        self.endpoints
            .iter()
            .filter_map(|endpoint| endpoint.local_addr().ok())
            .collect()
    }

    pub fn reaches(&self, at: &SocketAddr) -> bool {
        self.local()
            .iter()
            .any(|local| local.is_ipv4() == at.is_ipv4())
    }

    pub async fn dial(&self, peer: &PublicKey, at: SocketAddr) -> Result<quinn::Connection, Error> {
        let same_family = |endpoint: &&quinn::Endpoint| {
            endpoint
                .local_addr()
                .is_ok_and(|local| local.is_ipv4() == at.is_ipv4())
        };
        let Some(endpoint) = self.endpoints.iter().find(same_family) else {
            return Err(Error::Quic("halo bound no udp of the peer's family".into()));
        };
        let admit = Admit {
            members: self.members.clone(),
            expected: Some(peer.clone()),
            algorithms: provider().signature_verification_algorithms,
        };
        let mut crypto = rustls::ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(admit))
            .with_client_auth_cert(
                vec![self.certified.cert.clone()],
                PrivateKeyDer::Pkcs8(self.certified.key.clone_key()),
            )?;
        crypto.alpn_protocols = vec![ALPN.to_vec()];
        let crypto =
            QuicClientConfig::try_from(crypto).map_err(|error| Error::Quic(error.to_string()))?;
        let mut client = quinn::ClientConfig::new(Arc::new(crypto));
        client.transport_config(transport());
        let connecting = endpoint
            .connect_with(client, at, NAME)
            .map_err(|error| Error::Quic(error.to_string()))?;
        connecting
            .await
            .map_err(|error| Error::Quic(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use commonware_cryptography::Signer as _;

    use super::*;

    #[test]
    fn a_node_key_certificate_names_its_key() {
        let identity = PrivateKey::from_seed(7);
        let certified = certified(&identity).unwrap();
        assert_eq!(key_of(&certified.cert), Some(identity.public_key()));
    }
}
