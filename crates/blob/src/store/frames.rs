// A blob larger than a segment, stored as its own file: independent zstd frames of its framed bytes,
// each with a checksum of what it decodes to, and an index of the frames at the file's end, so a
// range read decodes only the frames that cover it.

use std::fs;
use std::io::{Read, Write as _};
use std::path::Path;

use abi::{BlobHeader, BlobId};
use borsh::{BorshDeserialize, BorshSerialize};

use super::{Error, PARTIAL, Result, framed_len, gone, named, read_at, verified, window};
use crate::{Hasher, parse_header};

const MAGIC: [u8; 8] = *b"dtframe1";
const TAIL: u64 = 16;

#[derive(BorshSerialize, BorshDeserialize)]
struct Index {
    id: BlobId,
    kind: String,
    len: u64,
    frame: u64,
    frames: Vec<Frame>,
}

#[derive(Clone, Copy, BorshSerialize, BorshDeserialize)]
struct Frame {
    at: u64,
    encoded: u64,
    crc: u32,
}

pub(super) struct Frames {
    file: fs::File,
    index: Index,
}

pub(super) fn write(
    path: &Path,
    id: BlobId,
    mut source: impl Read,
    framed: u64,
    frame: u64,
) -> Result<(String, u64)> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let mut hasher = Hasher::new(id.kind());
    let mut frames = Vec::new();
    let mut chunk = vec![0u8; frame as usize];
    let mut head = None;
    let mut at = 0u64;
    let mut left = framed;
    while left > 0 {
        let piece = &mut chunk[..left.min(frame) as usize];
        source.read_exact(piece)?;
        let piece = &*piece;
        if head.is_none() {
            head = Some(header_in(&id, piece)?);
        }
        hasher.update(piece);
        let encoded = zstd::bulk::compress(piece, zstd::DEFAULT_COMPRESSION_LEVEL)?;
        file.write_all(&encoded)?;
        frames.push(Frame {
            at,
            encoded: encoded.len() as u64,
            crc: crc32fast::hash(piece),
        });
        at += encoded.len() as u64;
        left -= piece.len() as u64;
    }
    let named = hasher.finish()? == id;
    if !named {
        return Err(Error::Corrupt(id, "its bytes do not hash to its id".into()));
    }
    let header = head.ok_or_else(|| Error::Corrupt(id, "it has no bytes".into()))?;
    let sized = framed_len(&header.kind, header.len) == framed;
    if !sized {
        return Err(Error::Corrupt(
            id,
            "its header disagrees with its length".into(),
        ));
    }
    let index = borsh::to_vec(&Index {
        id,
        kind: header.kind.clone(),
        len: header.len,
        frame,
        frames,
    })?;
    file.write_all(&index)?;
    file.write_all(&(index.len() as u32).to_le_bytes())?;
    file.write_all(&crc32fast::hash(&index).to_le_bytes())?;
    file.write_all(&MAGIC)?;
    file.sync_data()?;
    Ok((header.kind, header.len))
}

fn header_in(id: &BlobId, piece: &[u8]) -> Result<BlobHeader> {
    let nul = piece
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| Error::Corrupt(*id, "its first frame holds no header".into()))?;
    parse_header(&piece[..nul])
        .ok_or_else(|| Error::Corrupt(*id, "its header does not parse".into()))
}

impl Frames {
    pub(super) fn open(path: &Path, id: &BlobId) -> Result<Option<Frames>> {
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let corrupt = |why: &str| Error::Corrupt(*id, format!("its frame index {why}"));
        let len = file.metadata()?.len();
        if len < TAIL {
            return Err(corrupt("is missing"));
        }
        let mut tail = [0u8; TAIL as usize];
        read_at(&file, &mut tail, len - TAIL)?;
        let marked = tail[8..] == MAGIC;
        if !marked {
            return Err(corrupt("is missing"));
        }
        let index_len = u64::from(u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]));
        let crc = u32::from_le_bytes([tail[4], tail[5], tail[6], tail[7]]);
        let fits = index_len + TAIL <= len;
        if !fits {
            return Err(corrupt("runs past the file"));
        }
        let mut bytes = vec![0u8; index_len as usize];
        read_at(&file, &mut bytes, len - TAIL - index_len)?;
        let intact = crc32fast::hash(&bytes) == crc;
        if !intact {
            return Err(corrupt("disagrees with its checksum"));
        }
        let index: Index = borsh::from_slice(&bytes).map_err(|_| corrupt("does not decode"))?;
        let names_it = index.id == *id;
        if !names_it {
            return Err(corrupt("names another blob"));
        }
        let framed = framed_len(&index.kind, index.len);
        let counted = index.frame > 0 && index.frames.len() as u64 == framed.div_ceil(index.frame);
        if !counted {
            return Err(corrupt("does not cover the blob"));
        }
        Ok(Some(Frames { file, index }))
    }

    pub(super) fn framed_len(&self) -> u64 {
        framed_len(&self.index.kind, self.index.len)
    }

    fn decode(&self, n: usize) -> Result<Vec<u8>> {
        let id = self.index.id;
        let frame = self.index.frames[n];
        let mut encoded = vec![0u8; frame.encoded as usize];
        read_at(&self.file, &mut encoded, frame.at)?;
        let start = n as u64 * self.index.frame;
        let want = (self.framed_len() - start).min(self.index.frame);
        let decoded = zstd::bulk::decompress(&encoded, want as usize)
            .map_err(|error| Error::Corrupt(id, format!("frame {n} does not decode: {error}")))?;
        super::inflated(decoded.len());
        let intact = decoded.len() as u64 == want && crc32fast::hash(&decoded) == frame.crc;
        if !intact {
            return Err(Error::Corrupt(
                id,
                format!("frame {n} disagrees with its checksum"),
            ));
        }
        Ok(decoded)
    }

    pub(super) fn tail_at(&self, at: u64) -> Result<Vec<u8>> {
        let n = at / self.index.frame;
        let mut decoded = self.decode(n as usize)?;
        Ok(decoded.split_off((at - n * self.index.frame) as usize))
    }

    fn range(&self, from: u64, to: u64) -> Result<Vec<u8>> {
        let mut bytes = Vec::with_capacity((to - from) as usize);
        let mut at = from;
        while at < to {
            let piece = self.tail_at(at)?;
            let take = piece.len().min((to - at) as usize);
            bytes.extend_from_slice(&piece[..take]);
            at += take as u64;
        }
        Ok(bytes)
    }
}

pub(super) fn whole(path: &Path, id: &BlobId) -> Result<Option<Vec<u8>>> {
    let Some(frames) = Frames::open(path, id)? else {
        return Ok(None);
    };
    let framed = frames.range(0, frames.framed_len())?;
    verified(id, framed).map(Some)
}

pub(super) fn read(path: &Path, id: &BlobId, offset: u64, want: u64) -> Result<Option<Vec<u8>>> {
    let Some(frames) = Frames::open(path, id)? else {
        return Ok(None);
    };
    let head = frames.framed_len() - frames.index.len;
    let (start, end) = window(frames.index.len, offset, want);
    frames.range(head + start, head + end).map(Some)
}

pub(super) fn held(dir: &Path) -> Result<Vec<(BlobId, String, u64)>> {
    fs::create_dir_all(dir)?;
    let mut held = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.ends_with(PARTIAL) {
            gone(fs::remove_file(entry.path()))?;
            continue;
        }
        let Some(id) = named(name) else {
            continue;
        };
        match Frames::open(&entry.path(), &id) {
            Ok(Some(frames)) => held.push((id, frames.index.kind, frames.index.len)),
            Ok(None) | Err(Error::Corrupt(..)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(held)
}
