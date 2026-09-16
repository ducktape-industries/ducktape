//! the data-plane transport binding: a [`SyncClient`] over the off-consensus
//! stream class.
//!
//! where [`p2p`](crate::p2p) multiplexes every request over ONE mesh channel
//! (so it must tag requests with ids and reap the ones a dead peer never
//! answers), the stream class hands out a fresh reliable byte stream per
//! `open()`. so this binding is one-stream-per-request: the stream itself IS
//! the request/response correlation — no ids, no dispatch task, no reaper.
//! open a stream, write the request frame, read the response frame, drop. TCP
//! (or the sim's in-order pipe) gives ordering and reliability; the stream's
//! own close is the liveness signal a dead peer would otherwise strand.
//!
//! the request and response ride length-prefixed frames on the stream BODY,
//! not the hello `meta` (capped at 1 KiB): a future request shape that carries
//! more than a KiB must not silently overflow the handshake. the plane owns
//! the hello/ack itself, so this layer only frames the two payloads.
//!
//! statesync payloads are bulk (a snapshot chunk is 256 KiB), so paying one
//! hello/ack round-trip per request is free — which is exactly why the stream
//! class, not the datagram class, carries state sync.

use std::future::Future;
use std::io;
use std::sync::Arc;

use data_plane::{DataPlaneTransport, FlowId, PeerId, Service, StreamService};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::{SyncClient, SyncError, SyncRequest, SyncResponse, decode_response, encode_request};

/// the well-known flow every statesync stream rides. both ends derive it from
/// this fixed label — no signaling — and a node's admission policy names this
/// exact `(Service::STATE_SYNC, flow)` triple to permit a joiner's pull.
pub fn statesync_flow() -> FlowId {
    FlowId::derive(b"statesync")
}

/// the stream-hello `intent` marking a statesync RPC stream: one request in,
/// one response out. the plane treats it as opaque; the serve loop needs no
/// finer discrimination because every statesync stream is exactly this.
pub const INTENT_RPC: u8 = 1;

/// defensive cap on a single length-prefixed frame. sized well above the
/// largest legitimate response (a 256 KiB snapshot chunk, or a manifest /
/// frame batch) so an honest payload never trips it, but bounded so a lying
/// or corrupt length prefix cannot make the reader allocate unboundedly.
pub const MAX_FRAME_LEN: u64 = 64 * 1024 * 1024;

/// write one length-prefixed frame: `u64-be len || bytes`, then flush. shared
/// by the client (request out) and the inline serve loop (response out).
///
/// network byte order (BE), not the wire crate's LE convention: this frame
/// prefix lives on the raw stream body, outside `wire.rs`'s length-prefixed
/// helpers, so it uses its own framing convention.
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, bytes: &[u8]) -> io::Result<()> {
    writer
        .write_all(&(bytes.len() as u64).to_be_bytes())
        .await?;
    writer.write_all(bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// read one length-prefixed frame written by [`write_frame`]. a length past
/// [`MAX_FRAME_LEN`] is a hard decode error, never an allocation.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut len_bytes = [0u8; 8];
    reader.read_exact(&mut len_bytes).await?;
    let len = u64::from_be_bytes(len_bytes);
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds cap {MAX_FRAME_LEN}"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await?;
    Ok(buf)
}

/// a [`SyncClient`] whose every request opens one stream-class flow to a single
/// serving peer. `Clone` (an `Arc` over the registered service) because the
/// qmdb sync engine holds the client across concurrent fetches — each fetch
/// just opens its own stream.
pub struct DataPlaneSyncClient<T: DataPlaneTransport> {
    service: Arc<StreamService<T>>,
    server: PeerId,
}

impl<T: DataPlaneTransport> Clone for DataPlaneSyncClient<T> {
    fn clone(&self) -> Self {
        Self {
            service: Arc::clone(&self.service),
            server: self.server,
        }
    }
}

impl<T: DataPlaneTransport> DataPlaneSyncClient<T> {
    /// bind a client to `server` over a registered [`StreamService`] for
    /// [`Service::STATE_SYNC`]. the caller registers the service (it owns the
    /// plane); the client only ever opens streams on it.
    pub fn new(service: Arc<StreamService<T>>, server: PeerId) -> Self {
        Self { service, server }
    }

    /// the service id every statesync stream is opened on.
    pub const SERVICE: Service = Service::STATE_SYNC;
}

impl<T: DataPlaneTransport> SyncClient for DataPlaneSyncClient<T> {
    fn request(
        &self,
        req: SyncRequest,
    ) -> impl Future<Output = Result<SyncResponse, SyncError>> + Send {
        let service = Arc::clone(&self.service);
        let server = self.server;
        async move {
            let mut stream = service
                .open(server, statesync_flow(), INTENT_RPC, Vec::new())
                .await
                .map_err(|e| SyncError::Transport(format!("open statesync stream: {e}")))?;
            write_frame(&mut stream, &encode_request(&req))
                .await
                .map_err(|e| SyncError::Transport(format!("send request: {e}")))?;
            let resp_bytes = read_frame(&mut stream)
                .await
                .map_err(|e| SyncError::Transport(format!("read response: {e}")))?;
            Ok(decode_response(&resp_bytes)?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_round_trips_on_a_duplex() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let payload = b"manifest response bytes".to_vec();
        write_frame(&mut a, &payload).await.unwrap();
        let read = read_frame(&mut b).await.unwrap();
        assert_eq!(read, payload);
    }

    #[tokio::test]
    async fn frame_length_prefix_is_big_endian() {
        let (mut a, mut b) = tokio::io::duplex(64);
        write_frame(&mut a, b"abc").await.unwrap();
        let mut buf = [0u8; 11];
        tokio::io::AsyncReadExt::read_exact(&mut b, &mut buf)
            .await
            .unwrap();
        assert_eq!(buf, [0, 0, 0, 0, 0, 0, 0, 3, b'a', b'b', b'c']);
    }

    #[tokio::test]
    async fn oversize_length_prefix_is_rejected_without_allocating() {
        let (mut a, mut b) = tokio::io::duplex(64);
        // a lying prefix well past the cap: the reader must error, not try to
        // allocate `MAX_FRAME_LEN + 1` bytes.
        a.write_all(&(MAX_FRAME_LEN + 1).to_le_bytes())
            .await
            .unwrap();
        a.flush().await.unwrap();
        let err = read_frame(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
