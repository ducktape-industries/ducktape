//! The `ducktape/forward` lane: a call signed under `ducktape:forward` opens a byte stream through
//! any node to a service a member's node forwards, which that node's own `forward.toml` names with
//! the signers it lets through. `GET /v1/forward` is the calling end and `serve` the far one.
use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use abi::{Refusal, reason};
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket};
use commonware_codec::DecodeExt as _;
use commonware_cryptography::ed25519::PublicKey;
use futures::{SinkExt as _, StreamExt as _};
use halo::{Class, Halo, Inbound, Links, Streams};
use host::Submission;
use node::BLOCK_BYTES;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::watch;
use tokio_util::io::ReaderStream;

use crate::egress::lane::{read, write};
use crate::view::Current;
use crate::wire::Forwarded;
use crate::{Context, Daemon, Result};

pub const LANE: &str = "ducktape/forward";
const TARGET: &str = "ducktape::forward";

pub(crate) struct Forwarding {
    streams: Streams<Arc<Links>>,
    offers: PathBuf,
}

impl Forwarding {
    pub(crate) fn bind(halo: &Halo, backlog: NonZeroUsize, offers: PathBuf) -> Result<Forwarding> {
        let streams = halo.plane().streams(LANE, backlog.get())?;
        Ok(Forwarding { streams, offers })
    }
}

/// What `forward.toml` offers: each service by name.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Offers {
    #[serde(default)]
    services: BTreeMap<String, Offer>,
}

/// One forwarded service: where it listens and the keys whose calls reach it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Offer {
    address: String,
    #[serde(default)]
    signers: Vec<String>,
}

trait Bytestream: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Bytestream for T {}

pub(crate) fn spawn<E: Context>(daemon: Arc<Daemon<E>>, rpc: tokio::runtime::Handle) {
    let calls = rpc.clone();
    rpc.spawn(async move {
        while let Some(inbound) = daemon.forward.streams.accept().await {
            calls.spawn(serve(daemon.clone(), inbound));
        }
    });
}

async fn serve<E: Context, S>(daemon: Arc<Daemon<E>>, inbound: Inbound<S>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let from = inbound.from;
    let mut stream = Box::pin(inbound.stream);
    let opened = opened(&daemon, &inbound.meta).await;
    if let Err(refusal) = &opened {
        refused(&from, refusal);
    }
    let told = write(&mut stream, &Forwarded::of(&opened)).await;
    let (Ok(mut target), Ok(())) = (opened, told) else {
        let _ = stream.shutdown().await;
        return;
    };
    if let Err(error) = tokio::io::copy_bidirectional(&mut stream, &mut target).await {
        tracing::debug!(target: TARGET, event = "forward_cut", %from, reason = "stream_broken", %error, "a forwarded stream broke off");
    }
}

fn refused(from: &PublicKey, refusal: &Refusal) {
    tracing::debug!(target: TARGET, event = "forward_refused", %from, reason = %refusal.reason, sentence = %refusal.sentence, "a forward was refused");
}

pub(crate) async fn called<E: Context>(mut socket: WebSocket, daemon: Arc<Daemon<E>>) {
    let Some(Ok(Message::Binary(frame))) = socket.recv().await else {
        return;
    };
    let reached = reached(&daemon, &frame).await;
    let answer = Message::Binary(abi::encode(&Forwarded::of(&reached)).into());
    let told = socket.send(answer).await;
    let (Ok(stream), Ok(())) = (reached, told) else {
        let _ = socket.send(Message::Close(None)).await;
        return;
    };
    if let Err(error) = piped(socket, stream).await {
        tracing::debug!(target: TARGET, event = "forward_cut", reason = "stream_broken", %error, "a forward's caller or member left before both ends were done");
    }
}

async fn reached<E: Context>(
    daemon: &Daemon<E>,
    frame: &[u8],
) -> std::result::Result<Box<dyn Bytestream>, Refusal> {
    let submission = verified(daemon, frame).await?;
    let served_here = submission.payload == daemon.identity;
    if served_here {
        return Ok(Box::new(opened(daemon, frame).await?));
    }
    let member = PublicKey::decode(submission.payload.as_slice())
        .map_err(|_| Refusal::new(reason::INVALID_INPUT, "the call's payload is no node key"))?;
    let stream = daemon
        .forward
        .streams
        .open(&member, Class::Client, frame)
        .await
        .map_err(unopened)?;
    let mut stream = Box::pin(stream);
    let answer: Forwarded = read(&mut stream, BLOCK_BYTES).await.map_err(|error| {
        Refusal::new(
            reason::WRONG_STATE,
            format!("the member's node gave no answer: {error}"),
        )
    })?;
    match answer {
        Forwarded::Opened => Ok(Box::new(stream)),
        Forwarded::Refused(refusal) => Err(refusal),
    }
}

fn unopened(error: halo::Error) -> Refusal {
    match error {
        halo::Error::NoLane(lane) => Refusal::new(
            reason::UNSUPPORTED,
            format!("this network has not opened the {lane} lane: the validators' vote opens it"),
        ),
        halo::Error::Unreachable(key) => Refusal::new(
            reason::NOT_FOUND,
            format!("{key} is not a member this node reaches"),
        ),
        error @ (halo::Error::NoDatagrams(_)
        | halo::Error::Oversized(_)
        | halo::Error::Closed
        | halo::Error::Io(_)) => Refusal::new(
            reason::WRONG_STATE,
            format!("this node could not open the lane to the member: {error}"),
        ),
    }
}

async fn verified<E: Context>(
    daemon: &Daemon<E>,
    frame: &[u8],
) -> std::result::Result<Submission, Refusal> {
    let submission = node::verify_forward(frame, &daemon.descriptor.id())?;
    let tips = daemon.tips.subscribe();
    fresh(&daemon.current, tips, &daemon.finalized, &submission).await?;
    Ok(submission)
}

async fn fresh<E: Context>(
    current: &Current<E>,
    tips: watch::Receiver<u64>,
    finalized: &watch::Receiver<u64>,
    submission: &Submission,
) -> std::result::Result<(), Refusal> {
    let view = current
        .for_anchor(tips, finalized, submission.anchor.height)
        .await
        .map_err(unchecked)?;
    view.source().fresh(submission).map_err(unchecked)?
}

fn unchecked(error: impl std::fmt::Display) -> Refusal {
    Refusal::new(
        reason::WRONG_STATE,
        format!("this node could not check the call against its tip: {error}"),
    )
}

async fn opened<E: Context>(
    daemon: &Daemon<E>,
    frame: &[u8],
) -> std::result::Result<tokio::net::TcpStream, Refusal> {
    let submission = verified(daemon, frame).await?;
    let served_here = submission.payload == daemon.identity;
    if !served_here {
        return Err(Refusal::new(
            reason::UNAUTHORIZED,
            "the call names another node as the one that forwards it",
        ));
    }
    let offers = offers(&daemon.forward.offers).await?;
    let service = &submission.target;
    let Some(offer) = offers.services.get(service) else {
        return Err(unoffered(service));
    };
    let signer = hex::encode(&submission.signer);
    let listed = offer
        .signers
        .iter()
        .any(|listed| listed.trim().eq_ignore_ascii_case(&signer));
    if !listed {
        tracing::debug!(target: TARGET, event = "forward_unlisted", service = %service, signer = %signer, "a forward named a service that does not list its signer");
        return Err(unoffered(service));
    }
    let target = tokio::net::TcpStream::connect(&offer.address)
        .await
        .map_err(|error| {
            Refusal::new(
                reason::WRONG_STATE,
                format!("{service} did not take the connection: {error}"),
            )
        })?;
    tracing::info!(target: TARGET, event = "forward_opened", service = %service, signer = %signer, "a listed signer opened a forwarded service");
    Ok(target)
}

fn unoffered(service: &str) -> Refusal {
    Refusal::new(
        reason::NOT_FOUND,
        format!("this node forwards no service named {service} to this signer"),
    )
}

async fn offers(path: &Path) -> std::result::Result<Offers, Refusal> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Offers::default());
        }
        Err(error) => {
            return Err(Refusal::new(
                reason::CORRUPT,
                format!("forward.toml does not read: {error}"),
            ));
        }
    };
    toml::from_str(&text).map_err(|error| {
        Refusal::new(
            reason::CORRUPT,
            format!("forward.toml does not parse: {error}"),
        )
    })
}

fn cut() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::ConnectionAborted,
        "the caller left before its bytes ended",
    )
}

async fn piped(socket: WebSocket, stream: Box<dyn Bytestream>) -> std::io::Result<()> {
    let (mut sink, mut source) = socket.split();
    let (reading, mut writing) = tokio::io::split(stream);
    let up = async {
        while let Some(message) = source.next().await {
            let bytes = match message.map_err(std::io::Error::other)? {
                Message::Binary(bytes) => bytes,
                Message::Ping(_) | Message::Pong(_) => continue,
                Message::Text(_) | Message::Close(_) => return Err(cut()),
            };
            let ended = bytes.is_empty();
            if ended {
                return writing.shutdown().await;
            }
            writing.write_all(&bytes).await?;
        }
        Err(cut())
    };
    let down = async {
        let mut chunks = ReaderStream::new(reading);
        while let Some(chunk) = chunks.next().await {
            sink.send(Message::Binary(chunk?))
                .await
                .map_err(std::io::Error::other)?;
        }
        sink.send(Message::Binary(Bytes::new()))
            .await
            .map_err(std::io::Error::other)
    };
    futures::future::try_join(up, down).await?;
    let _ = sink.send(Message::Close(None)).await;
    Ok(())
}

#[cfg(test)]
mod tests;
