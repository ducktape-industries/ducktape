// Blobs: the framing every blob's id is the hash of, the blobs program's contract, the store a
// node keeps them in (feature `store`) and how a node gathers, serves and receipts them over
// halo (feature `node`). The kernel host, noded, the CLI and the blobs program use it.

pub mod contract;
#[cfg(feature = "node")]
pub mod service;
#[cfg(feature = "store")]
mod store;

use abi::{BlobHeader, BlobId, HashKind, Refusal, hex, reason};
use sha1::Digest as _;

#[cfg(feature = "store")]
pub use store::{Blobs, Error, Layered, Result, Stage, file_entry, names, size_of, window};

pub fn is_kind(kind: &str) -> bool {
    !kind.is_empty() && !kind.contains([' ', '\0'])
}

pub fn frame(kind: &str, body: &[u8]) -> std::result::Result<Vec<u8>, Refusal> {
    if !is_kind(kind) {
        return Err(Refusal::new(
            reason::INVALID_INPUT,
            "a blob kind is one non-empty word without spaces or NUL",
        ));
    }
    let header = header(kind, body.len() as u64);
    let mut framed = Vec::with_capacity(header.len() + body.len());
    framed.extend_from_slice(&header);
    framed.extend_from_slice(body);
    Ok(framed)
}

pub fn header(kind: &str, len: u64) -> Vec<u8> {
    format!("{kind} {len}\0").into_bytes()
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

pub(crate) fn parse_header(head: &[u8]) -> Option<BlobHeader> {
    let head = std::str::from_utf8(head).ok()?;
    let (kind, len) = head.split_once(' ')?;
    Some(BlobHeader {
        kind: kind.to_owned(),
        len: len.parse().ok()?,
    })
}

pub fn id_of(hash: HashKind, framed: &[u8]) -> BlobId {
    let mut hasher = Hasher::new(hash);
    hasher.update(framed);
    hasher.finish()
}

pub enum Hasher {
    Sha256(sha2::Sha256),
    Sha1(sha1::Sha1),
}

impl Hasher {
    pub fn new(hash: HashKind) -> Hasher {
        match hash {
            HashKind::Sha256 => Hasher::Sha256(sha2::Sha256::new()),
            HashKind::Sha1 => Hasher::Sha1(sha1::Sha1::new()),
        }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        match self {
            Hasher::Sha256(hasher) => hasher.update(bytes),
            Hasher::Sha1(hasher) => hasher.update(bytes),
        }
    }

    pub fn finish(self) -> BlobId {
        match self {
            Hasher::Sha256(hasher) => BlobId::Sha256(hasher.finalize().into()),
            Hasher::Sha1(hasher) => BlobId::Sha1(hasher.finalize().into()),
        }
    }
}

pub fn name_of(id: &BlobId) -> String {
    hex(&abi::encode(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_git_blob_gets_its_git_oid() {
        let framed = frame("blob", b"hello\n").unwrap();
        assert_eq!(framed, b"blob 6\0hello\n");
        let id = id_of(HashKind::Sha1, &framed);
        assert_eq!(hex(id.digest()), "ce013625030ba8dba906f756967f9e9ca394464a");
        let (header, body) = parse(&framed).unwrap();
        assert_eq!(header.kind, "blob");
        assert_eq!(header.len, 6);
        assert_eq!(body, b"hello\n");
    }

    #[test]
    fn a_kind_is_one_word() {
        assert_eq!(frame("", b"").unwrap_err().reason, reason::INVALID_INPUT);
        assert_eq!(
            frame("two words", b"").unwrap_err().reason,
            reason::INVALID_INPUT
        );
        assert!(parse(b"blob 3\0hi").is_none());
        assert!(parse(b"blob\0hi").is_none());
    }
}
