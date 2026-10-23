// Blobs: the framing every blob's id is the hash of, the blobs program's contract, the store a
// node keeps them in (feature `store`) and how a node gathers, serves and receipts them over
// halo (feature `node`). The kernel host, noded, the CLI and the blobs program use it.

pub mod contract;
#[cfg(feature = "node")]
pub mod service;
#[cfg(feature = "store")]
mod store;

use abi::{BlobHeader, BlobId, HashKind, Refusal, hex, reason};
use sha1_checked::CollisionResult;
use sha2::Digest as _;

#[cfg(feature = "testkit")]
pub use store::CopyGate;
#[cfg(feature = "store")]
pub use store::{
    Blobs, Error, FRAME_BYTES, Layered, Promoted, Reader, Result, SEGMENT_BYTES, Stage, Tidied,
    Tidier, Unpacked, file_entry, names, size_of, window,
};

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

pub fn id_of(hash: HashKind, framed: &[u8]) -> std::result::Result<BlobId, Collision> {
    let mut hasher = Hasher::new(hash);
    hasher.update(framed);
    hasher.finish()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Collision(pub BlobId);

impl std::fmt::Display for Collision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the bytes named {:?} carry a SHA-1 collision attack",
            self.0
        )
    }
}

impl std::error::Error for Collision {}

impl From<Collision> for Refusal {
    fn from(collision: Collision) -> Refusal {
        Refusal::new(reason::INVALID_INPUT, collision.to_string())
    }
}

pub enum Hasher {
    Sha256(sha2::Sha256),
    Sha1(Box<sha1_checked::Sha1>),
}

impl Hasher {
    pub fn new(hash: HashKind) -> Hasher {
        match hash {
            HashKind::Sha256 => Hasher::Sha256(sha2::Sha256::new()),
            HashKind::Sha1 => Hasher::Sha1(Box::new(
                sha1_checked::Sha1::builder().safe_hash(false).build(),
            )),
        }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        match self {
            Hasher::Sha256(hasher) => hasher.update(bytes),
            Hasher::Sha1(hasher) => hasher.update(bytes),
        }
    }

    pub fn finish(self) -> std::result::Result<BlobId, Collision> {
        match self {
            Hasher::Sha256(hasher) => Ok(BlobId::Sha256(hasher.finalize().into())),
            Hasher::Sha1(hasher) => sha1_id(hasher.try_finalize()),
        }
    }
}

fn sha1_id(result: CollisionResult) -> std::result::Result<BlobId, Collision> {
    match result {
        CollisionResult::Ok(digest) => Ok(BlobId::Sha1(digest.into())),
        CollisionResult::Collision(digest) | CollisionResult::Mitigated(digest) => {
            Err(Collision(BlobId::Sha1(digest.into())))
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
        let id = id_of(HashKind::Sha1, &framed).unwrap();
        assert_eq!(hex(id.digest()), "ce013625030ba8dba906f756967f9e9ca394464a");
        let (header, body) = parse(&framed).unwrap();
        assert_eq!(header.kind, "blob");
        assert_eq!(header.len, 6);
        assert_eq!(body, b"hello\n");
    }

    #[test]
    fn a_git_object_gets_the_id_git_gives_it_in_either_hash() {
        let sha1 = |framed: &[u8]| hex(id_of(HashKind::Sha1, framed).unwrap().digest());
        let sha256 = |framed: &[u8]| hex(id_of(HashKind::Sha256, framed).unwrap().digest());
        assert_eq!(
            sha1(b"blob 0\0"),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert_eq!(
            sha1(b"tree 0\0"),
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        );
        assert_eq!(
            sha256(b"blob 0\0"),
            "473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813"
        );
        assert_eq!(
            sha256(b"blob 6\0hello\n"),
            "2cf8d83d9ee29543b34a87727421fdecb7e3f3a183d337639025de576db9ebb4"
        );
        assert_eq!(
            sha256(b"tree 0\0"),
            "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321"
        );
    }

    #[test]
    fn both_halves_of_the_shattered_collision_are_refused_naming_the_id_they_share() {
        let first: &[u8] = include_bytes!("../fixtures/shattered-1.bin");
        let second: &[u8] = include_bytes!("../fixtures/shattered-2.bin");
        assert_ne!(first, second);
        let shared = "f92d74e3874587aaf443d1db961d4e26dde13e9c";
        for half in [first, second] {
            let Err(Collision(id)) = id_of(HashKind::Sha1, half) else {
                panic!("a SHAttered half hashed without a collision");
            };
            assert_eq!(hex(id.digest()), shared);
            let mut streamed = Hasher::new(HashKind::Sha1);
            for chunk in half.chunks(7) {
                streamed.update(chunk);
            }
            assert_eq!(streamed.finish(), Err(Collision(id)));
            let refusal = Refusal::from(Collision(id));
            assert_eq!(refusal.reason, reason::INVALID_INPUT);
            assert!(refusal.sentence.contains(shared), "{}", refusal.sentence);
        }
        assert!(id_of(HashKind::Sha256, first).is_ok());
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
