use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead as _, BufReader, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use abi::{Blob, BlobHeader, BlobId, HashKind, Refusal, hex, reason};
use sha1::Digest as _;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("blob store io: {0}")]
    Io(#[from] std::io::Error),
    #[error("blob {0:?} is corrupt on disk: {1}")]
    Corrupt(BlobId, String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn frame(kind: &str, body: &[u8]) -> std::result::Result<Vec<u8>, Refusal> {
    let kind_is_a_word = !kind.is_empty() && !kind.contains([' ', '\0']);
    if !kind_is_a_word {
        return Err(Refusal::new(
            reason::INVALID_INPUT,
            "a blob kind is one non-empty word without spaces or NUL",
        ));
    }
    let header = format!("{kind} {}\0", body.len());
    let mut framed = Vec::with_capacity(header.len() + body.len());
    framed.extend_from_slice(header.as_bytes());
    framed.extend_from_slice(body);
    Ok(framed)
}

pub fn parse(framed: &[u8]) -> Option<(BlobHeader, &[u8])> {
    let nul = framed.iter().position(|&b| b == 0)?;
    let header = parse_header(&framed[..nul])?;
    let body = &framed[nul + 1..];
    let len_matches = body.len() as u64 == header.len;
    if !len_matches {
        return None;
    }
    Some((header, body))
}

fn parse_header(head: &[u8]) -> Option<BlobHeader> {
    let head = std::str::from_utf8(head).ok()?;
    let (kind, len) = head.split_once(' ')?;
    Some(BlobHeader {
        kind: kind.to_owned(),
        len: len.parse().ok()?,
    })
}

pub fn id_of(hash: HashKind, framed: &[u8]) -> BlobId {
    match hash {
        HashKind::Sha256 => BlobId::Sha256(sha2::Sha256::digest(framed).into()),
        HashKind::Sha1 => BlobId::Sha1(sha1::Sha1::digest(framed).into()),
    }
}

pub fn name_of(id: &BlobId) -> String {
    hex(&abi::encode(id))
}

#[derive(Default)]
pub struct Stage {
    framed: BTreeMap<BlobId, Vec<u8>>,
}

impl Stage {
    pub fn put(&mut self, hash: HashKind, kind: &str, body: &[u8]) -> std::result::Result<BlobId, Refusal> {
        let framed = frame(kind, body)?;
        let id = id_of(hash, &framed);
        self.framed.entry(id).or_insert(framed);
        Ok(id)
    }

    pub fn get(&self, id: &BlobId) -> Option<&[u8]> {
        self.framed.get(id).map(Vec::as_slice)
    }

    pub fn retain(&mut self, keep: impl Fn(&BlobId) -> bool) {
        self.framed.retain(|id, _| keep(id));
    }

    pub fn ids(&self) -> impl Iterator<Item = &BlobId> {
        self.framed.keys()
    }

    pub fn is_empty(&self) -> bool {
        self.framed.is_empty()
    }
}

pub struct Blobs {
    dir: PathBuf,
}

impl Blobs {
    pub fn open(dir: &Path) -> Result<Blobs> {
        fs::create_dir_all(dir)?;
        Ok(Blobs {
            dir: dir.to_path_buf(),
        })
    }

    fn path(&self, id: &BlobId) -> PathBuf {
        self.dir.join(name_of(id))
    }

    pub fn has(&self, id: &BlobId) -> bool {
        self.path(id).is_file()
    }

    pub fn write(&self, id: &BlobId, framed: &[u8]) -> Result<()> {
        let target = self.path(id);
        if target.is_file() {
            return Ok(());
        }
        let partial = self.dir.join(format!("{}.partial", name_of(id)));
        fs::write(&partial, framed)?;
        fs::rename(partial, target)?;
        Ok(())
    }

    pub fn promote(&self, stage: Stage) -> Result<()> {
        for (id, framed) in stage.framed {
            self.write(&id, &framed)?;
        }
        Ok(())
    }

    pub fn framed(&self, id: &BlobId) -> Result<Option<Vec<u8>>> {
        match fs::read(self.path(id)) {
            Ok(framed) => Ok(Some(framed)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn get(&self, id: &BlobId) -> Result<Option<Blob>> {
        let Some(framed) = self.framed(id)? else {
            return Ok(None);
        };
        let (header, body) = parse(&framed)
            .ok_or_else(|| Error::Corrupt(*id, "the frame does not parse".into()))?;
        Ok(Some(Blob {
            kind: header.kind,
            body: body.to_vec(),
        }))
    }

    fn open_framed(&self, id: &BlobId) -> Result<Option<(fs::File, BlobHeader, u64)>> {
        let file = match fs::File::open(self.path(id)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut reader = BufReader::new(file);
        let mut head = Vec::new();
        reader.read_until(0, &mut head)?;
        let ended_at_nul = head.last() == Some(&0);
        if !ended_at_nul {
            return Err(Error::Corrupt(*id, "the frame has no header".into()));
        }
        let header = parse_header(&head[..head.len() - 1])
            .ok_or_else(|| Error::Corrupt(*id, "the header does not parse".into()))?;
        let body_start = head.len() as u64;
        let file = reader.into_inner();
        let body_len = file.metadata()?.len() - body_start;
        let header_matches_body = header.len == body_len;
        if !header_matches_body {
            return Err(Error::Corrupt(
                *id,
                "the header length disagrees with the body".into(),
            ));
        }
        Ok(Some((file, header, body_start)))
    }

    pub fn stat(&self, id: &BlobId) -> Result<Option<BlobHeader>> {
        Ok(self.open_framed(id)?.map(|(_, header, _)| header))
    }

    pub fn read(&self, id: &BlobId, offset: u64, len: u64) -> Result<Option<Vec<u8>>> {
        let Some((mut file, header, body_start)) = self.open_framed(id)? else {
            return Ok(None);
        };
        let start = offset.min(header.len);
        let end = offset.saturating_add(len).min(header.len);
        file.seek(SeekFrom::Start(body_start + start))?;
        let mut bytes = vec![0u8; (end - start) as usize];
        file.read_exact(&mut bytes)?;
        Ok(Some(bytes))
    }

    pub fn remove(&self, id: &BlobId) -> Result<()> {
        match fs::remove_file(self.path(id)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

pub fn read_framed(stage: &Stage, blobs: &Blobs, id: &BlobId) -> Result<Option<Vec<u8>>> {
    if let Some(framed) = stage.get(id) {
        return Ok(Some(framed.to_vec()));
    }
    blobs.framed(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_git_blob_gets_its_git_oid() {
        let framed = frame("blob", b"hello\n").unwrap();
        assert_eq!(framed, b"blob 6\0hello\n");
        let id = id_of(HashKind::Sha1, &framed);
        assert_eq!(
            hex(id.digest()),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
        let (header, body) = parse(&framed).unwrap();
        assert_eq!(header.kind, "blob");
        assert_eq!(header.len, 6);
        assert_eq!(body, b"hello\n");
    }

    #[test]
    fn a_kind_is_one_word() {
        assert_eq!(frame("", b"").unwrap_err().reason, reason::INVALID_INPUT);
        assert_eq!(frame("two words", b"").unwrap_err().reason, reason::INVALID_INPUT);
        assert!(parse(b"blob 3\0hi").is_none());
        assert!(parse(b"blob\0hi").is_none());
    }

    #[test]
    fn a_staged_blob_is_promoted_and_read_back_whole_or_by_range() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::open(dir.path()).unwrap();
        let mut stage = Stage::default();
        let id = stage.put(HashKind::Sha256, "page", b"0123456789").unwrap();
        let again = stage.put(HashKind::Sha256, "page", b"0123456789").unwrap();
        assert_eq!(id, again);
        assert!(!blobs.has(&id));
        assert_eq!(read_framed(&stage, &blobs, &id).unwrap().unwrap(), b"page 10\x000123456789");
        blobs.promote(stage).unwrap();
        assert!(blobs.has(&id));
        assert_eq!(
            blobs.get(&id).unwrap().unwrap(),
            Blob {
                kind: "page".into(),
                body: b"0123456789".to_vec()
            }
        );
        assert_eq!(
            blobs.stat(&id).unwrap().unwrap(),
            BlobHeader {
                kind: "page".into(),
                len: 10
            }
        );
        assert_eq!(blobs.read(&id, 3, 4).unwrap().unwrap(), b"3456");
        assert_eq!(blobs.read(&id, 8, 100).unwrap().unwrap(), b"89");
        assert_eq!(blobs.read(&id, 100, 1).unwrap().unwrap(), b"");
        assert_eq!(blobs.get(&BlobId::Sha1([0; 20])).unwrap(), None);
        assert_eq!(blobs.stat(&BlobId::Sha1([0; 20])).unwrap(), None);
        blobs.remove(&id).unwrap();
        assert!(!blobs.has(&id));
        blobs.remove(&id).unwrap();
    }

    #[test]
    fn a_stage_keeps_only_what_survived() {
        let mut stage = Stage::default();
        let kept = stage.put(HashKind::Sha1, "blob", b"kept").unwrap();
        let dropped = stage.put(HashKind::Sha1, "blob", b"dropped").unwrap();
        stage.retain(|id| *id == kept);
        assert_eq!(stage.ids().copied().collect::<Vec<_>>(), vec![kept]);
        assert!(stage.get(&dropped).is_none());
    }
}
