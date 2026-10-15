//! What halo needs from the medium under it: authenticated byte streams to and from members,
//! and datagrams. Links (QUIC) is the real medium; the sim medium serves tests.

use std::future::Future;

use bytes::Bytes;
use commonware_cryptography::ed25519::PublicKey;
use tokio::io::{AsyncRead, AsyncWrite};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0} is not a member halo can reach")]
    Unreachable(PublicKey),
    #[error("the path to {0} carries no datagrams")]
    NoDatagrams(PublicKey),
    #[error("the datagram is {0} bytes, past what the path carries")]
    Oversized(usize),
    #[error("halo is closed")]
    Closed,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub trait Transport: Send + Sync + 'static {
    type Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static;

    fn connect(&self, to: &PublicKey) -> impl Future<Output = Result<Self::Stream, Error>> + Send;

    fn accept(&self) -> impl Future<Output = Option<(PublicKey, Self::Stream)>> + Send;

    fn send_datagram(
        &self,
        to: &PublicKey,
        frame: Bytes,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    fn recv_datagram(&self) -> impl Future<Output = Option<(PublicKey, Bytes)>> + Send;
}
