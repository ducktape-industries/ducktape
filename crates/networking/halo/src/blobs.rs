//! The blob lane: a member asks another for a blob by id from an offset and gets its framed
//! bytes, paced at the class it is served at. A fetch asks the frame's sender, then the
//! validators, then the residents, resumes from what it already holds, and keeps only bytes
//! that hash to the id.

use std::sync::Arc;
use std::time::Duration;

use abi::BlobId;
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_cryptography::ed25519::PublicKey;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncSeekExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::Semaphore;

use crate::class::Class;
use crate::members::Members;
use crate::pacer::Paced;
use crate::plane::{Inbound, Lane, Plane, Streams, Taken};
use crate::transport::Transport;

const BACKLOG: usize = 64;
const SERVING: usize = 64;
const ANSWER_BYTES: u32 = 1 << 10;
const STALLED: Duration = Duration::from_secs(30);
const ROUND_PAUSE: Duration = Duration::from_secs(1);
const ROUND_PAUSE_MOST: Duration = Duration::from_secs(30);
const CHUNK: usize = 64 << 10;
pub const UNSIZED_BLOB_BYTES: u64 = 256 << 20;

#[derive(Debug, BorshSerialize, BorshDeserialize)]
pub struct Ask {
    pub id: BlobId,
    pub offset: u64,
}

#[derive(Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Answer {
    Have { len: u64, class: u8 },
    Lacks,
}

#[derive(Clone, Debug)]
pub struct Want {
    pub id: BlobId,
    pub len: Option<u64>,
    pub class: Class,
    pub from: Option<PublicKey>,
}

pub struct BlobLane<T: Transport> {
    streams: Streams<T>,
    members: Arc<Members>,
    me: PublicKey,
}

impl<T: Transport> BlobLane<T> {
    pub fn new(plane: &Plane<T>, members: Arc<Members>, me: PublicKey) -> Result<Arc<Self>, Taken> {
        Ok(Arc::new(BlobLane {
            streams: plane.streams(Lane::BLOBS, BACKLOG)?,
            members,
            me,
        }))
    }

    pub async fn serve(self: Arc<Self>, store: blobs::Blobs) {
        let store = Arc::new(store);
        let serving = Arc::new(Semaphore::new(SERVING));
        loop {
            let Ok(permit) = serving.clone().acquire_owned().await else {
                return;
            };
            let Some(inbound) = self.streams.accept().await else {
                return;
            };
            let store = store.clone();
            tokio::spawn(async move {
                let from = inbound.from.clone();
                let answered = answer(&store, inbound).await;
                if let Err(reason) = answered {
                    tracing::debug!(target: "ducktape::halo", %from, reason, "halo did not give a blob");
                }
                drop(permit);
            });
        }
    }

    pub async fn fetch(&self, want: Want) -> Vec<u8> {
        let mut partial = Vec::new();
        let mut rounds = 0u32;
        loop {
            for peer in self.members.candidates(&self.me, want.from.as_ref()) {
                let pulled = self.pull(&peer, &want, &mut partial).await;
                match pulled {
                    Ok(()) => return partial,
                    Err(reason) => {
                        tracing::debug!(target: "ducktape::halo", id = ?want.id, %peer, reason, "a member did not give a blob");
                    }
                }
            }
            rounds += 1;
            let worth_a_line = rounds == 1 || rounds.is_multiple_of(10);
            if worth_a_line {
                tracing::warn!(target: "ducktape::halo", event = "blob_unfetched", id = ?want.id, attempts = rounds, reason = "no_member_gave_it", "no member gave a wanted blob; halo asks again");
            }
            let pause = ROUND_PAUSE.saturating_mul(rounds).min(ROUND_PAUSE_MOST);
            tokio::time::sleep(pause).await;
        }
    }

    async fn asked(
        &self,
        peer: &PublicKey,
        class: Class,
        ask: &Ask,
    ) -> Result<(Answer, Paced<T::Stream>), &'static str> {
        let opened = self.streams.open(peer, class, &abi::encode(ask)).await;
        let Ok(mut stream) = opened else {
            return Err("unopened");
        };
        let answer = within(read_answer(&mut stream)).await?;
        Ok((answer, stream))
    }

    pub async fn ask(
        &self,
        peer: &PublicKey,
        id: BlobId,
        class: Class,
    ) -> Result<Answer, &'static str> {
        let (answer, _) = self.asked(peer, class, &Ask { id, offset: 0 }).await?;
        Ok(answer)
    }

    async fn pull(
        &self,
        peer: &PublicKey,
        want: &Want,
        partial: &mut Vec<u8>,
    ) -> Result<(), &'static str> {
        let ask = Ask {
            id: want.id,
            offset: partial.len() as u64,
        };
        let (answer, mut stream) = self.asked(peer, want.class, &ask).await?;
        let Answer::Have { len, .. } = answer else {
            return Err("lacks");
        };
        let most = want.len.unwrap_or(UNSIZED_BLOB_BYTES);
        let sized = want.len.is_none_or(|declared| declared == len);
        let bounded = len <= most && len >= ask.offset;
        if !(sized && bounded) {
            return Err("missized");
        }
        let mut chunk = vec![0u8; CHUNK];
        while (partial.len() as u64) < len {
            let left = (len - partial.len() as u64).min(CHUNK as u64) as usize;
            let read =
                within(async { stream.read(&mut chunk[..left]).await.map_err(|_| "broken") })
                    .await?;
            if read == 0 {
                return Err("cut");
            }
            partial.extend_from_slice(&chunk[..read]);
        }
        let named = blobs::id_of(want.id.kind(), partial) == want.id;
        if !named {
            partial.clear();
            return Err("misnamed");
        }
        Ok(())
    }
}

async fn within<T>(step: impl Future<Output = Result<T, &'static str>>) -> Result<T, &'static str> {
    tokio::time::timeout(STALLED, step)
        .await
        .unwrap_or(Err("stalled"))
}

pub async fn read_answer<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Answer, &'static str> {
    let len = stream.read_u32_le().await.map_err(|_| "cut")?;
    let fits = len <= ANSWER_BYTES;
    if !fits {
        return Err("malformed");
    }
    let mut bytes = vec![0u8; len as usize];
    stream.read_exact(&mut bytes).await.map_err(|_| "cut")?;
    abi::decode(&bytes).map_err(|_| "malformed")
}

async fn write_answer<S: AsyncWrite + Unpin>(
    stream: &mut S,
    answer: &Answer,
) -> Result<(), &'static str> {
    let bytes = abi::encode(answer);
    stream
        .write_u32_le(bytes.len() as u32)
        .await
        .map_err(|_| "broken")?;
    stream.write_all(&bytes).await.map_err(|_| "broken")
}

async fn answer<S: AsyncRead + AsyncWrite + Unpin>(
    store: &Arc<blobs::Blobs>,
    inbound: Inbound<S>,
) -> Result<(), &'static str> {
    let Inbound {
        class,
        meta,
        mut stream,
        ..
    } = inbound;
    let ask: Ask = abi::decode(&meta).map_err(|_| "malformed")?;
    let opened = {
        let store = store.clone();
        tokio::task::spawn_blocking(move || store.file(&ask.id))
            .await
            .map_err(|_| "unread")?
            .map_err(|_| "unread")?
    };
    let Some(file) = opened else {
        write_answer(&mut stream, &Answer::Lacks).await?;
        return Err("lacks");
    };
    let mut file = tokio::fs::File::from_std(file);
    let len = file.metadata().await.map_err(|_| "unread")?.len();
    let past = ask.offset > len;
    if past {
        write_answer(&mut stream, &Answer::Lacks).await?;
        return Err("offset_past_end");
    }
    let have = Answer::Have {
        len,
        class: u8::from(class),
    };
    write_answer(&mut stream, &have).await?;
    file.seek(std::io::SeekFrom::Start(ask.offset))
        .await
        .map_err(|_| "unread")?;
    let mut left = (&mut file).take(len - ask.offset);
    tokio::io::copy(&mut left, &mut stream)
        .await
        .map_err(|_| "broken")?;
    stream.shutdown().await.map_err(|_| "broken")
}
