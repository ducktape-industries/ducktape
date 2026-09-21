//! the immutable object model — chunk/file/tree/snapshot records in the
//! content-addressed store. an id is sha256 over a domain-separating kind tag
//! byte followed by the object's canonical body, so one body means exactly one
//! thing under exactly one kind, and the id can never collide across kinds.
//!
//! every decode is strict by design: counts are capped before they are trusted,
//! strings are utf-8, tree names and meta keys strictly ascend, single-byte
//! enums and booleans reject any non-canonical byte, and a decode that does not
//! consume the whole input is rejected. canonical bytes in, canonical bytes out,
//! or an error — there is no lenient path.

use std::collections::BTreeMap;

use sha2::{Digest as _, Sha256};

use crate::codec::{Reader, push_string};

use crate::{
    CHUNK_SIZE, MAX_CHUNKS_PER_FILE, MAX_DIR_ENTRIES, MAX_MESSAGE_BYTES, MAX_META_ENTRIES,
    MAX_META_KEY_BYTES, MAX_META_VALUE_BYTES, MAX_NAME_BYTES,
};

/// a raw 32-byte object id: sha256 over `tag ‖ body` (64-char lowercase hex on
/// the wire).
pub type ObjectId = [u8; 32];

/// object kind — the domain-separating tag byte in every id preimage and the
/// `kind` field of a sync-fetched object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Kind {
    Chunk = 0,
    File = 1,
    Tree = 2,
    Snapshot = 3,
}

impl Kind {
    /// the id-preimage / wire tag byte.
    pub fn tag(self) -> u8 {
        self as u8
    }

    /// the inverse of [`Kind::tag`] — an unknown tag is rejected, never coerced.
    pub fn from_u8(value: u8) -> Option<Kind> {
        match value {
            0 => Some(Kind::Chunk),
            1 => Some(Kind::File),
            2 => Some(Kind::Tree),
            3 => Some(Kind::Snapshot),
            _ => None,
        }
    }
}

/// derive an object id: sha256 over the kind tag byte followed by the body —
/// the same preimage every receiver re-derives before trusting fetched bytes.
pub fn object_id(kind: Kind, body: &[u8]) -> ObjectId {
    let mut h = Sha256::new();
    h.update([kind.tag()]);
    h.update(body);
    h.finalize().into()
}

/// a file: total byte size, the ordered chunk ids that reconstruct it, and up
/// to [`MAX_META_ENTRIES`] small string metadata pairs. chunking is fixed at
/// [`CHUNK_SIZE`]; [`verify_chunk_len`] pins the length of each chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileObj {
    pub size: u64,
    pub chunks: Vec<ObjectId>,
    pub meta: BTreeMap<String, String>,
}

impl FileObj {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.size.to_le_bytes());
        out.extend_from_slice(&(self.chunks.len() as u32).to_le_bytes());
        for id in &self.chunks {
            out.extend_from_slice(id);
        }
        out.extend_from_slice(&(self.meta.len() as u16).to_le_bytes());
        for (key, value) in &self.meta {
            push_string(&mut out, key);
            push_string(&mut out, value);
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut r = Reader::new("object body", bytes);
        let size = r.u64()?;

        let chunk_count = r.u32()? as usize;
        if chunk_count > MAX_CHUNKS_PER_FILE {
            return Err("file chunk count over cap".into());
        }
        // do not pre-reserve to the declared count: it is untrusted until the
        // ids are actually read, so growth stays amortized instead.
        let mut chunks = Vec::new();
        for _ in 0..chunk_count {
            chunks.push(r.bytes32()?);
        }

        let meta_count = r.u16()? as usize;
        if meta_count > MAX_META_ENTRIES {
            return Err("file meta count over cap".into());
        }
        let mut meta: BTreeMap<String, String> = BTreeMap::new();
        for _ in 0..meta_count {
            let key = r.string()?;
            let value = r.string()?;
            if key.len() > MAX_META_KEY_BYTES {
                return Err("file meta key over cap".into());
            }
            if value.len() > MAX_META_VALUE_BYTES {
                return Err("file meta value over cap".into());
            }
            // strictly ascending keys keep the encoding canonical: no duplicate,
            // no reordered pair can produce a second valid preimage.
            if meta
                .last_key_value()
                .is_some_and(|(last, _)| last.as_str() >= key.as_str())
            {
                return Err("file meta keys not strictly ascending".into());
            }
            meta.insert(key, value);
        }

        r.finish()?;
        Ok(FileObj { size, chunks, meta })
    }
}

/// what a tree entry points at — the tag byte prefixing each entry's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EntryKind {
    File = 0,
    Dir = 1,
    Symlink = 2,
}

impl EntryKind {
    pub fn tag(self) -> u8 {
        self as u8
    }

    pub fn from_u8(value: u8) -> Option<EntryKind> {
        match value {
            0 => Some(EntryKind::File),
            1 => Some(EntryKind::Dir),
            2 => Some(EntryKind::Symlink),
            _ => None,
        }
    }
}

/// one entry in a directory tree: what it is, the object it names, whether it is
/// executable, and the resolved byte size (0 for a directory).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeEntry {
    pub kind: EntryKind,
    pub id: ObjectId,
    pub exec: bool,
    pub size: u64,
}

/// a directory: a name-keyed set of entries, encoded in strict ascending name
/// order so the same directory always hashes to the same id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeObj {
    pub entries: BTreeMap<String, TreeEntry>,
}

impl TreeObj {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        // BTreeMap iterates in ascending key order, which is exactly the
        // canonical entry order the decoder re-checks.
        for (name, entry) in &self.entries {
            push_string(&mut out, name);
            out.push(entry.kind.tag());
            out.extend_from_slice(&entry.id);
            out.push(u8::from(entry.exec));
            out.extend_from_slice(&entry.size.to_le_bytes());
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut r = Reader::new("object body", bytes);
        let entry_count = r.u32()? as usize;
        if entry_count > MAX_DIR_ENTRIES {
            return Err("tree entry count over cap".into());
        }
        let mut entries: BTreeMap<String, TreeEntry> = BTreeMap::new();
        for _ in 0..entry_count {
            let name = r.string()?;
            if name.len() > MAX_NAME_BYTES {
                return Err("tree entry name over cap".into());
            }
            let kind = EntryKind::from_u8(r.u8()?)
                .ok_or_else(|| "tree entry has an unknown kind".to_string())?;
            let id = r.bytes32()?;
            let exec = r.boolean()?;
            let size = r.u64()?;
            if entries
                .last_key_value()
                .is_some_and(|(last, _)| last.as_str() >= name.as_str())
            {
                return Err("tree names not strictly ascending".into());
            }
            entries.insert(
                name,
                TreeEntry {
                    kind,
                    id,
                    exec,
                    size,
                },
            );
        }

        r.finish()?;
        Ok(TreeObj { entries })
    }
}

/// a snapshot: the committed root tree, the parent it descends from (absent
/// only for the first commit), and the consensus-witnessed authorship.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotObj {
    pub root: ObjectId,
    pub parent: Option<ObjectId>,
    pub author: crate::Actor,
    pub consensus_time: u64,
    pub height: u64,
    pub message: String,
}

impl SnapshotObj {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.root);
        match &self.parent {
            Some(parent) => {
                out.push(1);
                out.extend_from_slice(parent);
            }
            None => out.push(0),
        }
        self.author.encode_into(&mut out);
        out.extend_from_slice(&self.consensus_time.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        push_string(&mut out, &self.message);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut r = Reader::new("object body", bytes);
        let root = r.bytes32()?;
        // has_parent is a canonical bool: the parent id is present iff it is 1.
        let parent = if r.boolean()? {
            Some(r.bytes32()?)
        } else {
            None
        };
        let author = crate::Actor::decode(&mut r)?;
        let consensus_time = r.u64()?;
        let height = r.u64()?;
        let message = r.string()?;
        if message.len() > MAX_MESSAGE_BYTES {
            return Err("snapshot message over cap".into());
        }

        r.finish()?;
        Ok(SnapshotObj {
            root,
            parent,
            author,
            consensus_time,
            height,
            message,
        })
    }
}

/// the load-bearing exact-length rule: every chunk but the last must be exactly
/// [`CHUNK_SIZE`]; the last carries the remainder `size - (n-1)*CHUNK_SIZE`,
/// which must land in `1..=CHUNK_SIZE`. a size that cannot produce such a
/// remainder is an inconsistent size/chunks pair and rejects — so a zero-length
/// "chunk" can never spoof a hole, and the byte length alone pins each chunk.
pub fn verify_chunk_len(file: &FileObj, index: usize, got_len: u64) -> Result<(), String> {
    verify_chunk_len_at(file.size, file.chunks.len(), index, got_len)
}

/// [`verify_chunk_len`] over bare `(size, chunk_count)` parts — the commit
/// executor verifies referenced chunks BEFORE any [`FileObj`] exists, and the
/// read path already holds one; both charge the identical rule here.
pub fn verify_chunk_len_at(
    size: u64,
    chunk_count: usize,
    index: usize,
    got_len: u64,
) -> Result<(), String> {
    let n = chunk_count;
    if index >= n {
        return Err("chunk index out of range".into());
    }
    let expected = if index + 1 == n {
        let prefix = (n as u64 - 1)
            .checked_mul(CHUNK_SIZE)
            .ok_or_else(|| "chunk prefix length overflows".to_string())?;
        let last = size
            .checked_sub(prefix)
            .ok_or_else(|| "file size smaller than its chunk count implies".to_string())?;
        if last == 0 || last > CHUNK_SIZE {
            return Err("file size inconsistent with chunk count".into());
        }
        last
    } else {
        CHUNK_SIZE
    };
    if got_len != expected {
        return Err("chunk length does not match the size rule".into());
    }
    Ok(())
}

/// the file size/chunk-COUNT invariant, shared by commit validation and sync
/// ingest (single source of truth): size 0 requires no chunks; a non-empty file
/// needs exactly ceil(size / [`CHUNK_SIZE`]) chunks, pinned by the checked span
/// bounds `(n-1)*CHUNK_SIZE < size <= n*CHUNK_SIZE`.
///
/// this checks only the COUNT, never the chunk BODIES — at ingest time the
/// chunks a peer-synced FileObj names may not have arrived yet, so their lengths
/// cannot be verified here. [`verify_chunk_len`] (charged at read time, when the
/// bytes are in hand) closes that remaining gap. the two together stop a
/// self-consistent-but-lying FileObj from spoofing a hole: the shape check
/// rejects a wrong count, the length check rejects a short interior chunk.
pub fn verify_file_shape(size: u64, chunk_count: usize) -> Result<(), String> {
    if size == 0 {
        if chunk_count != 0 {
            return Err("an empty file must reference no chunks".into());
        }
        return Ok(());
    }
    let n = chunk_count as u64;
    if n == 0 {
        return Err("a non-empty file must reference at least one chunk".into());
    }
    let lower = (n - 1)
        .checked_mul(CHUNK_SIZE)
        .ok_or_else(|| "chunk span overflows".to_string())?;
    let upper = n
        .checked_mul(CHUNK_SIZE)
        .ok_or_else(|| "chunk span overflows".to_string())?;
    if size <= lower || size > upper {
        return Err("file size inconsistent with its chunk count".into());
    }
    Ok(())
}
