//! Raw state streams over the node's QUIC plane: the joining end, and the serving end that answers
//! from the newest epoch close this node holds.
use std::io;
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use abi::{BlobId, ProgramId, Refusal, reason};
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_codec::Encode as _;
use commonware_cryptography::ed25519::PublicKey;
use consensus::Certificate;
use futures::{Stream, StreamExt as _};
use halo::{Class, Halo, Inbound, Links, Streams, Transport};
use node::Block;
use statesync::lane::{Ask, Closing, Piece};
use statesync::{
    Bulk, Exchange, Failure, Fault, HeldClosing, Hold, Holds, Published, Request, Response,
};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::watch;

use crate::Daemon;
use crate::ctx::Ctx;

const TARGET: &str = "ducktape::sync";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Halo(#[from] halo::Error),
    #[error(transparent)]
    State(#[from] state::Error),
    #[error(transparent)]
    Node(Box<crate::Error>),
    #[error("state stream refused: {}: {}", .0.reason, .0.sentence)]
    Refused(Refusal),
    #[error("state stream encoding: {0}")]
    Encoding(#[source] io::Error),
}

impl From<crate::Error> for Error {
    fn from(error: crate::Error) -> Error {
        Error::Node(Box::new(error))
    }
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
            | Self::Node(_)
            | Self::Encoding(_) => Failure::Definite,
            Self::Refused(refusal) => refused(refusal),
        }
    }
}

fn refused(refusal: &Refusal) -> Failure {
    let served_later = refusal.reason == reason::NOT_YET;
    if served_later {
        return Failure::Transient("state_not_served_yet");
    }
    Failure::Definite
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

/// The node's end of `ducktape/state`: the streams it accepts and the newest close it serves.
pub(crate) struct Lane<E: crate::Context> {
    streams: Arc<Streams<Arc<Links>>>,
    ready: watch::Sender<Option<Arc<Ready<E>>>>,
}

/// An epoch close a joiner can adopt: the state published at the closing block, and the block
/// that opens the next epoch with its finalization.
pub(crate) struct Ready<E: crate::Context> {
    published: Arc<Published<E>>,
    opening: Block,
    certificate: Certificate,
}

impl<E: crate::Context> Lane<E> {
    pub(crate) fn bind(
        halo: &Halo,
        backlog: NonZeroUsize,
    ) -> std::result::Result<Lane<E>, halo::Taken> {
        let streams = halo.plane().streams(statesync::lane::LANE, backlog.get())?;
        Ok(Lane {
            streams: Arc::new(streams),
            ready: watch::Sender::new(None),
        })
    }

    pub(crate) fn opened(
        &self,
        published: Arc<Published<E>>,
        opening: Block,
        certificate: Certificate,
    ) {
        let ready = Ready {
            published,
            opening,
            certificate,
        };
        self.ready.send_replace(Some(Arc::new(ready)));
    }

    fn unserved(&self, past: Option<u64>) -> Option<Refusal> {
        let asks_any = past.is_none();
        let serves_none = self.ready.borrow().is_none();
        let refused = asks_any && serves_none;
        refused.then(|| {
            Refusal::new(
                reason::NOT_YET,
                "this node serves no epoch close yet: it serves one once it applies a close and the block after it",
            )
        })
    }

    async fn newer(&self, past: Option<u64>) -> Option<Arc<Ready<E>>> {
        let mut readies = self.ready.subscribe();
        let above = |ready: &Option<Arc<Ready<E>>>| {
            ready
                .as_ref()
                .is_some_and(|ready| past.is_none_or(|past| ready.published.tip().height > past))
        };
        let held = readies.wait_for(above).await.ok()?;
        let ready = Option::clone(&held);
        drop(held);
        ready
    }
}

pub(crate) fn spawn<E: crate::Context>(daemon: Arc<Daemon<E>>, rpc: tokio::runtime::Handle) {
    let asks = rpc.clone();
    rpc.spawn(async move {
        while let Some(inbound) = daemon.state_lane.streams.accept().await {
            asks.spawn(served(daemon.clone(), inbound));
        }
    });
}

async fn served<E: crate::Context, S>(daemon: Arc<Daemon<E>>, inbound: Inbound<S>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let peer = inbound.from;
    let ask = borsh::from_slice::<Ask>(&inbound.meta);
    let mut stream = inbound.stream;
    let answered = match ask {
        Ok(Ask::Head { past }) => head(&daemon, past, &mut stream).await,
        Ok(Ask::Ops {
            hold,
            program,
            from,
        }) => ops(&daemon, hold, &program, from, &mut stream).await,
        Ok(Ask::Blobs) => blobs(&daemon, &mut stream).await,
        Err(error) => unasked(error, &mut stream).await,
    };
    if let Err(error) = answered {
        tracing::debug!(target: TARGET, event = "state_stream_cut", %peer, %error, reason = "stream_unanswered", "a state stream ended before its answer did");
    }
}

async fn head<E: crate::Context, S: AsyncRead + AsyncWrite + Unpin>(
    daemon: &Daemon<E>,
    past: Option<u64>,
    stream: &mut S,
) -> Result<()> {
    let held = held_close(&daemon.state_lane, &daemon.holds, past, &mut *stream).await?;
    let Some(hold) = held else {
        return Ok(());
    };
    let mut byte = [0];
    let _ = stream.read(&mut byte).await;
    drop(hold);
    daemon.holds_ended.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

async fn held_close<E: crate::Context, S: AsyncWrite + Unpin>(
    lane: &Lane<E>,
    holds: &Holds<E>,
    past: Option<u64>,
    stream: &mut S,
) -> Result<Option<Hold<E>>> {
    if let Some(none) = lane.unserved(past) {
        write::<_, Closing>(stream, &Err(none)).await?;
        return Ok(None);
    }
    let Some(ready) = lane.newer(past).await else {
        return Ok(None);
    };
    let (served, hold) = statesync::held(ready.published.clone(), holds);
    let answer = statesync::anchored(served, &*ready.published).await;
    let (closing, hold) = match closed(answer, hold, &ready) {
        Ok(closed) => closed,
        Err(refusal) => {
            write::<_, Closing>(stream, &Err(refusal)).await?;
            return Ok(None);
        }
    };
    write(stream, &Ok(closing)).await?;
    Ok(Some(hold))
}

fn closed<E: crate::Context>(
    answer: Response,
    hold: Option<Hold<E>>,
    ready: &Ready<E>,
) -> std::result::Result<(Closing, Hold<E>), Refusal> {
    let (head, hold) = match (answer, hold) {
        (Response::Head(head), Some(hold)) => (head, hold),
        (Response::Refused(refusal), _) => return Err(refusal),
        (_, _) => {
            let unheld = "this node holds no epoch close it can serve";
            return Err(Refusal::new(reason::NOT_FOUND, unheld));
        }
    };
    let closing = Closing {
        head,
        hold: hold.number(),
        opening: ready.opening.clone(),
        certificate: ready.certificate.clone(),
    };
    Ok((closing, hold))
}

async fn ops<E: crate::Context, S: AsyncWrite + Unpin>(
    daemon: &Daemon<E>,
    hold: u64,
    program: &str,
    from: u64,
    stream: &mut S,
) -> Result<()> {
    let Some(serving) = daemon.holds.serving(hold, program) else {
        let unheld = Refusal::new(
            reason::NOT_FOUND,
            "no hold of this node's serves the program; read the head again",
        );
        return write::<_, Piece>(&mut *stream, &Err(unheld)).await;
    };
    let batch = state::Tuning::default().sync_apply_batch;
    serve_ops(&serving, from, batch, stream).await
}

async fn blobs<E: crate::Context, S: AsyncRead + AsyncWrite + Unpin>(
    daemon: &Arc<Daemon<E>>,
    stream: &mut S,
) -> Result<()> {
    let ids: Vec<BlobId> = read(&mut *stream).await?;
    let ctx = Ctx::of(daemon)?;
    for id in ids {
        let blob = crate::server::served(&ctx, id).await?.framed();
        write(&mut *stream, &Ok(blob)).await?;
    }
    stream.shutdown().await?;
    Ok(())
}

async fn unasked<S: AsyncWrite + Unpin>(error: io::Error, stream: &mut S) -> Result<()> {
    let refusal = Refusal::new(
        reason::PROTOCOL,
        format!("a state stream's ask does not decode: {error}"),
    );
    write::<_, ()>(&mut *stream, &Err(refusal)).await
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
    match sent_ops(serving, from, batch, writer).await {
        Err(Error::State(error)) => unread(error, writer).await,
        sent => sent,
    }
}

async fn sent_ops<E: statesync::Context, W: AsyncWrite + Unpin>(
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

async fn unread<W: AsyncWrite + Unpin>(error: state::Error, writer: &mut W) -> Result<()> {
    let refusal = Refusal::new(reason::INVALID_INPUT, error.to_string());
    write::<_, Piece>(writer, &Err(refusal)).await?;
    writer.shutdown().await?;
    Err(Error::State(error))
}

#[cfg(test)]
mod tests;
