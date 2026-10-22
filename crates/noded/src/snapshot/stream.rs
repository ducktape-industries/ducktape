// A kept snapshot as one answer: its head (its manifest with its tip's anchor), then each file under
// its runtime in path order as a header and its bytes, then an empty frame. A download writes the
// same tree and resumes from the last file it holds, at that file's length.

use std::cmp::Ordering;
use std::future::Future;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use abi::{Refusal, reason};
use bytes::{Bytes, BytesMut};
use futures::future::ready;
use futures::stream::BoxStream;
use futures::{Stream, StreamExt as _};
use statesync::{Head, Manifest};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};
use tokio::sync::watch;
use tokio_util::io::ReaderStream;

use super::{Kept, MANIFEST, RUNTIME};
use crate::wire::snapshot::{Ask, Part, Resume};
use crate::{Error, Result};

const END: [u8; 4] = 0u32.to_le_bytes();

struct Listed {
    name: String,
    path: PathBuf,
    len: u64,
}

pub(crate) fn served(
    kept: Arc<Kept>,
    ask: Ask,
) -> impl Stream<Item = io::Result<Bytes>> + Send + 'static {
    let head = Bytes::from(framed(&abi::encode(&kept.head)));
    let parts = futures::stream::once(sent(kept.clone(), ask)).flat_map(|listed| match listed {
        Ok(parts) => futures::stream::iter(parts).flat_map(part).left_stream(),
        Err(error) => futures::stream::once(ready(Err(error))).right_stream(),
    });
    let pieces = futures::stream::once(ready(Ok(head)))
        .chain(parts)
        .chain(futures::stream::once(ready(Ok(Bytes::from_static(&END)))))
        .boxed();
    let answer = Answer {
        pieces,
        cut: Box::pin(superseded(kept.cut.clone(), kept.height)),
        _held: kept,
    };
    futures::stream::unfold(Some(answer), next_piece)
}

struct Answer {
    pieces: BoxStream<'static, io::Result<Bytes>>,
    cut: Pin<Box<dyn Future<Output = ()> + Send>>,
    _held: Arc<Kept>,
}

async fn superseded(mut cut: watch::Receiver<u64>, height: u64) {
    let _ = cut.wait_for(|below| *below > height).await;
}

async fn next_piece(answer: Option<Answer>) -> Option<(io::Result<Bytes>, Option<Answer>)> {
    let mut answer = answer?;
    let next = tokio::select! {
        biased;
        () = &mut answer.cut => None,
        piece = answer.pieces.next() => Some(piece),
    };
    match next {
        None => Some((Err(replaced()), None)),
        Some(Some(piece)) => Some((piece, Some(answer))),
        Some(None) => None,
    }
}

pub(super) fn replaced() -> io::Error {
    io::Error::other(
        "two newer snapshots replaced the one this answer reads; a resume downloads the newest",
    )
}

async fn sent(kept: Arc<Kept>, ask: Ask) -> io::Result<Vec<(Listed, u64)>> {
    let runtime = kept.dir.join(RUNTIME);
    let listed = crate::offload(move || walked(&runtime))
        .await
        .map_err(|error| io::Error::other(error.to_string()))??;
    let resume = ask.resume.filter(|resume| resume.height == kept.height);
    Ok(listed
        .into_iter()
        .filter_map(|file| from(file, resume.as_ref()))
        .collect())
}

fn from(file: Listed, resume: Option<&Resume>) -> Option<(Listed, u64)> {
    let Some(resume) = resume else {
        return Some((file, 0));
    };
    match file.name.as_str().cmp(resume.path.as_str()) {
        Ordering::Less => None,
        Ordering::Equal => {
            let within = resume.offset <= file.len;
            let from = match within {
                true => resume.offset,
                false => 0,
            };
            Some((file, from))
        }
        Ordering::Greater => Some((file, 0)),
    }
}

fn part((file, from): (Listed, u64)) -> impl Stream<Item = io::Result<Bytes>> + Send {
    let Listed { name, path, len } = file;
    let header = Bytes::from(framed(&abi::encode(&Part {
        path: name,
        len,
        from,
    })));
    let bytes = futures::stream::once(opened(path, from, len)).flat_map(|opened| match opened {
        Ok(reading) => futures::stream::unfold(Some(reading), read).left_stream(),
        Err(error) => futures::stream::once(ready(Err(error))).right_stream(),
    });
    futures::stream::once(ready(Ok(header))).chain(bytes)
}

struct Reading {
    read: ReaderStream<tokio::io::Take<tokio::fs::File>>,
    left: u64,
}

async fn opened(path: PathBuf, from: u64, len: u64) -> io::Result<Reading> {
    let mut file = tokio::fs::File::open(&path).await?;
    let unit = io_unit(&file.metadata().await?)?;
    file.seek(io::SeekFrom::Start(from)).await?;
    let left = len - from;
    Ok(Reading {
        read: ReaderStream::with_capacity(file.take(left), unit),
        left,
    })
}

fn io_unit(metadata: &std::fs::Metadata) -> io::Result<usize> {
    use std::os::unix::fs::MetadataExt as _;
    usize::try_from(metadata.blksize())
        .ok()
        .filter(|unit| *unit > 0)
        .ok_or_else(|| io::Error::other("the filesystem names no I/O size for a snapshot file"))
}

async fn read(reading: Option<Reading>) -> Option<(io::Result<Bytes>, Option<Reading>)> {
    let mut reading = reading?;
    match reading.read.next().await {
        Some(Ok(bytes)) => {
            reading.left = reading.left.saturating_sub(bytes.len() as u64);
            Some((Ok(bytes), Some(reading)))
        }
        Some(Err(error)) => Some((Err(error), None)),
        None if reading.left == 0 => None,
        None => {
            let short = io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "a snapshot file ended before the length it was listed at",
            );
            Some((Err(short), None))
        }
    }
}

fn framed(bytes: &[u8]) -> Vec<u8> {
    let mut framed = (bytes.len() as u32).to_le_bytes().to_vec();
    framed.extend_from_slice(bytes);
    framed
}

fn walked(root: &Path) -> io::Result<Vec<Listed>> {
    let mut listed = Vec::new();
    walk(root, "", &mut listed)?;
    listed.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(listed)
}

fn walk(dir: &Path, above: &str, listed: &mut Vec<Listed>) -> io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is no UTF-8 name", entry.path().display()),
            ));
        };
        let name = match above.is_empty() {
            true => file_name,
            false => format!("{above}/{file_name}"),
        };
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            walk(&entry.path(), &name, listed)?;
            continue;
        }
        listed.push(Listed {
            name,
            path: entry.path(),
            len: metadata.len(),
        });
    }
    Ok(())
}

pub(crate) async fn resumed(into: &Path) -> Result<Option<Resume>> {
    let into = into.to_owned();
    Ok(crate::offload(move || holding(&into)).await??)
}

fn holding(into: &Path) -> io::Result<Option<Resume>> {
    let Some(manifest) = written(into)? else {
        return Ok(None);
    };
    let newest = walked(&into.join(RUNTIME))?.pop();
    Ok(newest.map(|file| Resume {
        height: manifest.tip.height,
        path: file.name,
        offset: file.len,
    }))
}

fn written(into: &Path) -> io::Result<Option<Manifest>> {
    match std::fs::read(into.join(MANIFEST)) {
        Ok(bytes) => Ok(abi::decode(&bytes).ok()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) async fn fetched(
    answer: impl Stream<Item = Result<Bytes>>,
    into: &Path,
) -> Result<Head> {
    let mut answer = std::pin::pin!(answer);
    let mut fetch = Fetch {
        into: into.to_owned(),
        held: BytesMut::new(),
        head: None,
        writing: None,
    };
    let fetched = fetch.read(&mut answer).await;
    if let Some(writing) = &mut fetch.writing {
        let _ = writing.file.flush().await;
    }
    fetched
}

struct Fetch {
    into: PathBuf,
    held: BytesMut,
    head: Option<Head>,
    writing: Option<Writing>,
}

struct Writing {
    file: tokio::fs::File,
    left: u64,
}

impl Fetch {
    async fn read(
        &mut self,
        answer: &mut (impl Stream<Item = Result<Bytes>> + Unpin),
    ) -> Result<Head> {
        loop {
            if let Some(head) = self.taken().await? {
                return Ok(head);
            }
            match answer.next().await {
                Some(Ok(bytes)) => self.held.extend_from_slice(&bytes),
                Some(Err(error)) => return Err(error),
                None => return Err(protocol("the snapshot's answer ended before its end")),
            }
        }
    }

    async fn taken(&mut self) -> Result<Option<Head>> {
        loop {
            if let Some(writing) = &mut self.writing {
                if writing.left == 0 {
                    writing.file.flush().await?;
                    self.writing = None;
                    continue;
                }
                if self.held.is_empty() {
                    return Ok(None);
                }
                let take = usize::try_from(writing.left)
                    .map_or(self.held.len(), |left| left.min(self.held.len()));
                let bytes = self.held.split_to(take);
                writing.file.write_all(&bytes).await?;
                writing.left -= take as u64;
                continue;
            }
            let Some(frame) = frame(&mut self.held)? else {
                return Ok(None);
            };
            match self.head.is_some() {
                false => self.head = Some(self.settled(&frame).await?),
                true if frame.is_empty() => return Ok(self.head.take()),
                true => self.writing = Some(self.opened(&frame).await?),
            }
        }
    }

    async fn settled(&self, frame: &[u8]) -> Result<Head> {
        let head: Head = abi::decode(frame).map_err(Error::Decode)?;
        let manifest = head.manifest();
        let into = self.into.clone();
        let held = crate::offload(move || written(&into)).await??;
        let holds_another_snapshot = held.as_ref() != Some(&manifest);
        if holds_another_snapshot {
            cleared(&self.into.join(RUNTIME)).await?;
            tokio::fs::create_dir_all(&self.into).await?;
            tokio::fs::write(self.into.join(MANIFEST), abi::encode(&manifest)).await?;
        }
        Ok(head)
    }

    async fn opened(&self, frame: &[u8]) -> Result<Writing> {
        let part: Part = abi::decode(frame).map_err(Error::Decode)?;
        if part.from > part.len {
            return Err(protocol(format!(
                "{} starts at {}, past its length {}",
                part.path, part.from, part.len
            )));
        }
        let path = inside(&self.into.join(RUNTIME), &part.path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = match part.from {
            0 => tokio::fs::File::create(&path).await?,
            from => {
                let file = tokio::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .await?;
                let held = file.metadata().await?.len();
                if held != from {
                    return Err(protocol(format!(
                        "{} resumes at {from} and this download holds {held} bytes of it",
                        part.path
                    )));
                }
                file
            }
        };
        Ok(Writing {
            file,
            left: part.len - part.from,
        })
    }
}

fn frame(held: &mut BytesMut) -> Result<Option<Bytes>> {
    let Some(len) = held.first_chunk::<4>() else {
        return Ok(None);
    };
    let len = u32::from_le_bytes(*len) as usize;
    if len > node::BLOCK_BYTES {
        return Err(protocol(format!(
            "the snapshot frames {len} bytes in one header, past the {} a node message holds",
            node::BLOCK_BYTES
        )));
    }
    let whole = held.len() >= 4 + len;
    if !whole {
        return Ok(None);
    }
    let mut frame = held.split_to(4 + len);
    Ok(Some(frame.split_off(4).freeze()))
}

fn inside(runtime: &Path, path: &str) -> Result<PathBuf> {
    let relative = Path::new(path);
    let plain = !path.is_empty()
        && relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)));
    match plain {
        true => Ok(runtime.join(relative)),
        false => Err(protocol(format!(
            "the snapshot names {path:?}, which is no path inside it"
        ))),
    }
}

async fn cleared(dir: &Path) -> io::Result<()> {
    match tokio::fs::remove_dir_all(dir).await {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        cleared => cleared,
    }
}

fn protocol(sentence: impl Into<String>) -> Error {
    Error::Decode(Refusal::new(reason::PROTOCOL, sentence))
}
