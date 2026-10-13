//! halo's stream path over a member's tcp, tls, ws or wss endpoint. A tcp or tls connection
//! opens with the preamble so the node it reaches tells it apart from the consensus mesh; a
//! ws or wss connection uses halo's own route. The node-key handshake then runs end to end.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_runtime::{BufferPooler, Clock, Supervisor};
use commonware_stream::encrypted::{self, Config};
use rustls::ClientConfig;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, DuplexStream};
use tokio_tungstenite::tungstenite;

use crate::dial::{self, Remote};
use crate::endpoint::Endpoint;
use crate::io::{FRAME, Reader, Writer, carried};
use crate::members::Members;

pub const PREAMBLE: &[u8] = b"ducktape-halo\n";
pub const ROUTE: &str = "/v1/halo";
const OPEN: Duration = Duration::from_secs(10);
const SYNCHRONY: Duration = Duration::from_secs(5);
const HANDSHAKE_AGE: Duration = Duration::from_secs(10);
const HANDSHAKE_WAIT: Duration = Duration::from_secs(5);

pub trait Bytes: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static {}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static> Bytes for S {}

#[derive(Clone)]
pub struct Handshake {
    identity: PrivateKey,
    namespace: Vec<u8>,
}

impl Handshake {
    pub fn new(identity: PrivateKey, namespace: &[u8]) -> Handshake {
        Handshake {
            identity,
            namespace: namespace.to_vec(),
        }
    }

    fn config(&self) -> Config<PrivateKey> {
        Config {
            signing_key: self.identity.clone(),
            namespace: self.namespace.clone(),
            max_message_size: (2 * FRAME) as u32,
            synchrony_bound: SYNCHRONY,
            max_handshake_age: HANDSHAKE_AGE,
            handshake_timeout: HANDSHAKE_WAIT,
        }
    }
}

pub enum Sniffed<S> {
    Halo(S),
    Other { conn: S, first: u8 },
}

pub async fn sniffed<S: AsyncRead + Unpin>(mut conn: S) -> io::Result<Sniffed<S>> {
    let first = conn.read_u8().await?;
    let halo = first == PREAMBLE[0];
    if !halo {
        return Ok(Sniffed::Other { conn, first });
    }
    let mut rest = vec![0u8; PREAMBLE.len() - 1];
    conn.read_exact(&mut rest).await?;
    let whole = rest == PREAMBLE[1..];
    if !whole {
        return Err(io::Error::other(
            "the connection opened with half a halo preamble",
        ));
    }
    Ok(Sniffed::Halo(conn))
}

pub trait Context: BufferPooler + rand_core::CryptoRng + Clock + Supervisor {}

impl<E> Context for E where E: BufferPooler + rand_core::CryptoRng + Clock + Supervisor {}

async fn handshaken<E: Context>(
    context: &E,
    handshake: &Handshake,
    peer: PublicKey,
    conn: impl Bytes,
) -> io::Result<DuplexStream> {
    let (read, write) = tokio::io::split(conn);
    let (sender, receiver) = encrypted::dial(
        context.child("handshake"),
        handshake.config(),
        peer,
        Reader::new(read),
        Writer(write),
    )
    .await
    .map_err(io::Error::other)?;
    Ok(carried(sender, receiver))
}

pub async fn dial<E: Context>(
    context: &E,
    handshake: &Handshake,
    client: Arc<ClientConfig>,
    endpoint: &Endpoint,
    peer: PublicKey,
) -> io::Result<DuplexStream> {
    let reaching = tokio::time::timeout(OPEN, dial::open(endpoint, client, ROUTE)).await;
    let remote = reaching.map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    match remote {
        Remote::Tcp(mut stream) => {
            stream.write_all(PREAMBLE).await?;
            handshaken(context, handshake, peer, stream).await
        }
        Remote::Tls(mut stream) => {
            stream.write_all(PREAMBLE).await?;
            handshaken(context, handshake, peer, *stream).await
        }
        Remote::WebSocket(socket) => {
            let (ours, theirs) = tokio::io::duplex(FRAME);
            tokio::spawn(async move {
                let _ = dial::splice(
                    *socket,
                    theirs,
                    tungstenite::Message::Binary,
                    dial::tungstenite_frame,
                )
                .await;
            });
            handshaken(context, handshake, peer, ours).await
        }
    }
}

pub async fn listen<E: Context>(
    context: &E,
    handshake: &Handshake,
    members: Arc<Members>,
    conn: impl Bytes,
) -> io::Result<(PublicKey, DuplexStream)> {
    let (read, write) = tokio::io::split(conn);
    let admitting = move |peer: PublicKey| async move { members.admits(&peer) };
    let (peer, sender, receiver) = encrypted::listen(
        context.child("handshake"),
        admitting,
        handshake.config(),
        Reader::new(read),
        Writer(write),
    )
    .await
    .map_err(io::Error::other)?;
    Ok((peer, carried(sender, receiver)))
}
