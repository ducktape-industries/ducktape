//! Raw state streams over the node's QUIC plane and their framed replies.
use std::io;
use std::num::NonZeroU64;
use std::sync::Arc;

use abi::{BlobId, ProgramId, Refusal, reason};
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_codec::Encode as _;
use commonware_cryptography::ed25519::PublicKey;
use futures::{Stream, StreamExt as _};
use halo::{Class, Halo, Links, Streams, Transport};
use statesync::lane::{Ask, Closing, Piece};
use statesync::{Bulk, Exchange, Failure, Fault, HeldClosing, Request, Response};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Halo(#[from] halo::Error),
    #[error(transparent)]
    State(#[from] state::Error),
    #[error("state stream refused: {}: {}", .0.reason, .0.sentence)]
    Refused(Refusal),
    #[error("state stream encoding: {0}")]
    Encoding(#[source] io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
type Wire = halo::Paced<<Arc<Links> as Transport>::Stream>;

impl Fault for Error {
    fn failure(&self) -> Failure {
        match self {
            Self::Io(_)
            | Self::Halo(halo::Error::Io(_))
            | Self::Halo(halo::Error::Unreachable(_))
            | Self::Halo(halo::Error::Closed) => Failure::Transient("state_stream_cut"),
            Self::Halo(halo::Error::NoLane(_))
            | Self::Halo(halo::Error::NoDatagrams(_))
            | Self::Halo(halo::Error::Oversized(_))
            | Self::State(_)
            | Self::Refused(_)
            | Self::Encoding(_) => Failure::Definite,
        }
    }
}

#[derive(Clone)]
pub struct Source {
    halo: Arc<Halo>,
    streams: Arc<Streams<Arc<Links>>>,
    peer: PublicKey,
}

impl Source {
    pub fn new(halo: Arc<Halo>, peer: PublicKey) -> std::result::Result<Self, halo::Taken> {
        let streams = Arc::new(halo.plane().streams(statesync::lane::LANE, 0)?);
        Ok(Self {
            halo,
            streams,
            peer,
        })
    }

    async fn open(&self, ask: Ask) -> Result<Wire> {
        let meta = borsh::to_vec(&ask).map_err(Error::Encoding)?;
        let stream = self.streams.open(&self.peer, Class::Member, &meta).await?;
        Ok(stream)
    }
}

fn until_cut(halo: Arc<Halo>, mut stream: Wire) -> statesync::Ending {
    Box::pin(async move {
        let _halo = halo;
        let mut byte = [0];
        let _ = stream.read(&mut byte).await;
    })
}

impl Bulk for Source {
    async fn closing(&self, past: Option<u64>) -> Result<HeldClosing> {
        let mut stream = self.open(Ask::Head { past }).await?;
        let closing: Closing = read(&mut stream).await?;
        let ending = until_cut(self.halo.clone(), stream);
        Ok(HeldClosing { closing, ending })
    }

    fn ops(
        &self,
        hold: u64,
        program: ProgramId,
        from: u64,
    ) -> impl Stream<Item = Result<Piece>> + Send {
        let source = self.clone();
        futures::stream::once(async move {
            let stream = source
                .open(Ask::Ops {
                    hold,
                    program,
                    from,
                })
                .await;
            futures::stream::unfold(Some(stream), |stream| async move {
                let mut stream = match stream? {
                    Ok(stream) => stream,
                    Err(error) => return Some((Err(error), None)),
                };
                let piece: Result<Piece> = read(&mut stream).await;
                let ended = !matches!(&piece, Ok(Piece::Pinned(_) | Piece::Ops(_)));
                let next = (!ended).then_some(Ok(stream));
                Some((piece, next))
            })
        })
        .flatten()
    }
}

impl Exchange for Source {
    type Error = Error;

    async fn exchange(&self, request: Request) -> Result<Response> {
        match request {
            Request::Head => Ok(Response::Head(self.closing(None).await?.closing.head)),
            Request::Sync { .. } | Request::Release { .. } => Err(Error::Refused(Refusal::new(
                reason::PROTOCOL,
                "the raw state lane accepts no proof-sync request",
            ))),
        }
    }

    fn stream_blobs(&self, ids: Vec<BlobId>) -> impl Stream<Item = Result<Option<Vec<u8>>>> + Send {
        let source = self.clone();
        let count = ids.len();
        futures::stream::once(async move {
            let stream = async {
                let mut stream = source.open(Ask::Blobs).await?;
                write(&mut stream, &Ok(ids)).await?;
                Ok(stream)
            }
            .await;
            futures::stream::unfold((Some(stream), count), |(stream, left)| async move {
                if left == 0 {
                    return None;
                }
                let mut stream = match stream? {
                    Ok(stream) => stream,
                    Err(error) => return Some((Err(error), (None, 0))),
                };
                let blob: Result<Option<Vec<u8>>> = read(&mut stream).await;
                let next = blob.is_ok().then_some(Ok(stream));
                Some((blob, (next, left - 1)))
            })
        })
        .flatten()
    }
}

pub async fn write<W: AsyncWrite + Unpin, T: BorshSerialize>(
    writer: &mut W,
    answer: &std::result::Result<T, Refusal>,
) -> Result<()> {
    let bytes = borsh::to_vec(answer).map_err(Error::Encoding)?;
    let len =
        u32::try_from(bytes.len()).map_err(|error| Error::Encoding(io::Error::other(error)))?;
    writer.write_u32_le(len).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read<R: AsyncRead + Unpin, T: BorshDeserialize>(reader: &mut R) -> Result<T> {
    let len = reader.read_u32_le().await?;
    let mut bytes = Vec::new();
    let read = reader.take(u64::from(len)).read_to_end(&mut bytes).await?;
    if read != len as usize {
        return Err(Error::Io(io::ErrorKind::UnexpectedEof.into()));
    }
    let answer: std::result::Result<T, Refusal> =
        borsh::from_slice(&bytes).map_err(Error::Encoding)?;
    answer.map_err(Error::Refused)
}

pub async fn serve_ops<E: statesync::Context, W: AsyncWrite + Unpin>(
    serving: &state::Serving<E>,
    from: u64,
    batch: NonZeroU64,
    writer: &mut W,
) -> Result<()> {
    let nodes = serving
        .pinned()
        .await?
        .into_iter()
        .map(|digest| digest.0)
        .collect();
    write(writer, &Ok(Piece::Pinned(nodes))).await?;
    let mut from = from;
    loop {
        let ops = serving.ops(from, batch).await?;
        if ops.is_empty() {
            break;
        }
        from += ops.len() as u64;
        write(writer, &Ok(Piece::Ops(ops.encode().to_vec()))).await?;
    }
    write(writer, &Ok(Piece::End)).await?;
    writer.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests;
