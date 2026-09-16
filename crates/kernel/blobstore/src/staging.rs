//! streaming staging: land a large content-addressed blob on disk without
//! ever holding it whole in memory.
//!
//! a [`StagedBlob`] is an exclusive, append-only writer for one expected
//! `(digest, len)`. bytes stream into `<root>/staging/<hex>`;
//! [`StagedBlob::finish`] re-reads the FILE, verifies its length and hash, and
//! only then publishes it under its content-addressed name (atomic rename) —
//! bytes are never addressable before they verify, so a crashed or lying
//! transfer can never leave a file whose name misdescribes its content.
//! rootless (in-memory) stores stage into a plain buffer so the API is uniform
//! for tests and embedders.
//!
//! the file, not the stream, is what verifies: a running hash only ever sees
//! what ONE writer appended, and the bytes that get renamed are whatever is on
//! disk. that gap is why exclusivity is the STORE's, not the caller's: the
//! staging slot for a digest has one live [`StagedBlob`] at a time, and a
//! second opener is refused [`StageError::AlreadyStaging`]. the code plane's
//! push and the mesh fetch lane both stage committed digests, and each was
//! guarding only against itself.
//!
//! resume: re-opening a digest's staging slot continues from the high-water
//! [`StagedBlob::offset`] the bytes already on disk give.

use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest as _, Sha256};

use crate::{BlobHandle, hex, sha256};

/// how long an abandoned staging file stays resumable. past it the partial is
/// garbage: no sender is coming back for it, and nothing else on the node ever
/// asks for a file under the staging name. this is what bounds the disk a
/// dropped (or deliberately abandoned) transfer can leave behind — a live
/// transfer keeps its file's mtime fresh, so only the dead ones age out.
pub const STAGING_RESUME_WINDOW: Duration = Duration::from_secs(600);

/// blobs at or below this size are cached in the store's memory map on put /
/// get; larger blobs live on disk only (persistent stores) so a 1 GB
/// component is never parked in RAM for the process lifetime.
pub const LARGE_BLOB_CACHE_BYTES: usize = 4 * 1024 * 1024;

/// why a staged transfer could not complete.
#[derive(Debug)]
pub enum StageError {
    /// appending past the declared length, or finishing short of it.
    Length { expected: u64, got: u64 },
    /// the finished bytes do not hash to the declared digest.
    HashMismatch,
    /// another writer holds this digest's staging slot.
    AlreadyStaging,
    /// the staging file could not be written / published.
    Io(std::io::Error),
}

impl std::fmt::Display for StageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Length { expected, got } => {
                write!(f, "staged length {got} does not match declared {expected}")
            }
            Self::HashMismatch => write!(f, "staged bytes do not hash to the declared digest"),
            Self::AlreadyStaging => write!(f, "another writer holds this digest's staging slot"),
            Self::Io(e) => write!(f, "staging io: {e}"),
        }
    }
}

impl std::error::Error for StageError {}

impl From<std::io::Error> for StageError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// where the in-flight bytes accumulate.
enum Sink {
    /// persistent store: `<root>/staging/<hex>`, appended in order.
    Disk { file: std::fs::File, path: PathBuf },
    /// rootless store: a plain buffer (tests, embedders — small blobs).
    Memory(Vec<u8>),
}

/// the digest's staging slot, held for as long as its writer lives. dropping
/// it — on `finish`, on `abort`, or when a transfer's task dies — is what lets
/// the next writer in.
struct StagingClaim {
    store: BlobHandle,
    digest: [u8; 32],
}

impl Drop for StagingClaim {
    fn drop(&mut self) {
        self.store.release_staging(&self.digest);
    }
}

/// an exclusive append-only staging slot for one expected `(digest, len)`.
pub struct StagedBlob {
    claim: StagingClaim,
    expected_len: u64,
    written: u64,
    sink: Sink,
}

impl BlobHandle {
    /// open — or resume — the staging slot for `digest`/`len`. exclusive: a
    /// digest already being staged answers [`StageError::AlreadyStaging`]. on
    /// a persistent store appending continues from [`StagedBlob::offset`],
    /// the length of the file already there; a leftover file longer than `len`
    /// (a superseded transfer under a lying length) is discarded and staging
    /// restarts clean.
    pub fn stage(&self, digest: [u8; 32], len: u64) -> Result<StagedBlob, StageError> {
        if !self.claim_staging(digest) {
            return Err(StageError::AlreadyStaging);
        }
        // from here every exit releases the claim: it lives in the guard.
        let claim = StagingClaim {
            store: self.clone(),
            digest,
        };
        let (sink, written) = match self.persistence_root() {
            None => (Sink::Memory(Vec::new()), 0),
            Some(root) => {
                let dir = root.join("staging");
                std::fs::create_dir_all(&dir)?;
                let path = dir.join(hex(&digest));
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(&path)?;
                let existing = file.metadata()?.len();
                let resumable = if existing > len {
                    file.set_len(0)?;
                    0
                } else {
                    existing
                };
                file.seek(std::io::SeekFrom::End(0))?;
                (Sink::Disk { file, path }, resumable)
            }
        };
        Ok(StagedBlob {
            claim,
            expected_len: len,
            written,
            sink,
        })
    }
}

/// delete every staging file under `root` that has not been touched within
/// `keep_within` and that `live` does not name, and report how many went. a
/// partial nobody resumed inside the window is bytes no lane will ever ask for
/// again; a partial a writer holds is never the sweep's, whatever its mtime.
pub(crate) fn sweep_staging_dir(
    root: &Path,
    keep_within: Duration,
    live: &std::collections::HashSet<String>,
) -> std::io::Result<usize> {
    let dir = root.join("staging");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // no staging directory means nothing was ever staged here.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut swept = 0usize;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if live.contains(&entry.file_name().to_string_lossy().to_string()) {
            continue;
        }
        let abandoned = meta
            .modified()
            .ok()
            .and_then(|m| SystemTime::now().duration_since(m).ok())
            .is_some_and(|age| age >= keep_within);
        if !meta.is_file() || !abandoned {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => {
                swept += 1;
                tracing::debug!(
                    target: "ducktape::blobstore",
                    reason = "staging_abandoned",
                    file = %entry.file_name().to_string_lossy(),
                    bytes = meta.len(),
                    "reclaimed an abandoned staging file"
                );
            }
            Err(error) => tracing::warn!(
                target: "ducktape::blobstore",
                reason = "staging_sweep_failed",
                file = %entry.file_name().to_string_lossy(),
                error = %error,
                "cannot reclaim an abandoned staging file"
            ),
        }
    }
    Ok(swept)
}

impl BlobHandle {
    /// reclaim abandoned staging files — see [`sweep_staging_dir`]. a rootless
    /// store stages into memory and has nothing to sweep.
    pub fn sweep_staging(&self, keep_within: Duration) -> std::io::Result<usize> {
        match self.persistence_root() {
            None => Ok(0),
            Some(root) => sweep_staging_dir(&root, keep_within, &self.staged_names()),
        }
    }
}

/// an append-only ingest for bytes whose digest is not known until they stop
/// arriving — the door a client pushes through, where the store NAMES what it
/// received instead of being told. The bytes land on disk as they arrive (a
/// rootless store buffers, as everywhere else here), so the size of what
/// someone uploads is a disk question, never a memory one.
pub struct IngestBlob {
    store: BlobHandle,
    /// taken at `publish`; what is left at drop is an upload that died.
    sink: Option<Sink>,
    /// the name `seal` read off the bytes — the only thing `publish` will use.
    sealed: Option<[u8; 32]>,
    written: u64,
}

impl BlobHandle {
    /// open a streaming ingest. The unknown-digest half of [`BlobHandle::stage`]:
    /// no slot is claimed, because there is no name to claim until
    /// [`IngestBlob::finish`] hashes what arrived.
    pub fn ingest(&self) -> Result<IngestBlob, StageError> {
        let sink = match self.persistence_root() {
            None => Sink::Memory(Vec::new()),
            Some(root) => {
                let dir = root.join("staging");
                std::fs::create_dir_all(&dir)?;
                // unique per writer: two concurrent uploads of the SAME bytes
                // must not share a file, and neither is named by its content
                // yet. the counter is what separates two opens inside one
                // nanosecond.
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let nonce = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|since| since.as_nanos())
                    .unwrap_or_default();
                let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let path = dir.join(format!("ingest-{}-{nonce:x}-{seq:x}", std::process::id()));
                let file = std::fs::OpenOptions::new()
                    .create_new(true)
                    .read(true)
                    .write(true)
                    .open(&path)?;
                Sink::Disk { file, path }
            }
        };
        Ok(IngestBlob {
            store: self.clone(),
            sink: Some(sink),
            sealed: None,
            written: 0,
        })
    }
}

impl IngestBlob {
    /// how many bytes have landed so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    pub fn append(&mut self, bytes: &[u8]) -> Result<(), StageError> {
        let Some(sink) = self.sink.as_mut() else {
            return Err(StageError::Io(std::io::Error::other(
                "this ingest already published",
            )));
        };
        match sink {
            Sink::Disk { file, .. } => file.write_all(bytes)?,
            Sink::Memory(buf) => buf.extend_from_slice(bytes),
        }
        self.written += bytes.len() as u64;
        Ok(())
    }

    /// hash what arrived WITHOUT publishing it. The caller may still have to
    /// decide whether these bytes are admitted at all — a signed upload proves
    /// itself over this digest — and bytes nobody admitted must not be
    /// addressable. Like [`StagedBlob::finish`] this hashes the FILE, not a
    /// running digest over what one writer appended.
    pub fn seal(&mut self) -> Result<[u8; 32], StageError> {
        let Some(sink) = self.sink.as_mut() else {
            return Err(StageError::Io(std::io::Error::other(
                "this ingest already published",
            )));
        };
        let digest = match sink {
            Sink::Disk { file, .. } => hash_whole_file(file)?.0,
            Sink::Memory(buf) => sha256(buf),
        };
        self.sealed = Some(digest);
        Ok(digest)
    }

    /// publish the sealed bytes under the digest [`IngestBlob::seal`] read off
    /// them. Unsealed bytes have no name, so there is nothing to publish.
    pub fn publish(mut self) -> Result<[u8; 32], StageError> {
        let (Some(sink), Some(digest)) = (self.sink.take(), self.sealed) else {
            return Err(StageError::Io(std::io::Error::other(
                "this ingest was never sealed",
            )));
        };
        match sink {
            Sink::Disk { file, path } => {
                file.sync_all()?;
                drop(file);
                self.store.publish_staged(&digest, &path)?;
            }
            Sink::Memory(buf) => {
                self.store.put_chunk(buf);
            }
        }
        Ok(digest)
    }
}

impl Drop for IngestBlob {
    /// an upload that died mid-flight leaves nothing behind: its file is named
    /// by nobody and no resume will ever ask for it.
    fn drop(&mut self) {
        if let Some(Sink::Disk { path, .. }) = &self.sink {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl StagedBlob {
    /// the resume high-water: how many bytes are already staged. a sender
    /// continues from exactly here.
    pub fn offset(&self) -> u64 {
        self.written
    }

    /// append the next in-order bytes. rejects growth past the declared
    /// length so a lying sender is stopped at the boundary it declared.
    pub fn append(&mut self, bytes: &[u8]) -> Result<(), StageError> {
        let next = self.written + bytes.len() as u64;
        if next > self.expected_len {
            return Err(StageError::Length {
                expected: self.expected_len,
                got: next,
            });
        }
        match &mut self.sink {
            Sink::Disk { file, .. } => file.write_all(bytes)?,
            Sink::Memory(buf) => buf.extend_from_slice(bytes),
        }
        self.written = next;
        Ok(())
    }

    /// verify length + hash OF THE FILE, then publish: the staging file
    /// renames onto its content-addressed name (or the buffer inserts via the
    /// store's put). the bytes re-read here are the bytes that get renamed —
    /// a running hash over what this writer appended would attest to something
    /// else entirely if the file were ever truncated or holed underneath it.
    /// on a length shortfall the staging bytes are kept for resume; a hash
    /// mismatch is never resumable and drops them.
    pub fn finish(mut self) -> Result<[u8; 32], StageError> {
        if self.written != self.expected_len {
            return Err(StageError::Length {
                expected: self.expected_len,
                got: self.written,
            });
        }
        let digest = self.claim.digest;
        let (got, on_disk) = match &mut self.sink {
            Sink::Disk { file, .. } => hash_whole_file(file)?,
            Sink::Memory(buf) => (sha256(buf), buf.len() as u64),
        };
        let verified = on_disk == self.expected_len && got == digest;
        if !verified {
            if let Sink::Disk { path, .. } = &self.sink {
                let _ = std::fs::remove_file(path);
            }
            return Err(StageError::HashMismatch);
        }
        match self.sink {
            Sink::Disk { file, path } => {
                file.sync_all()?;
                drop(file);
                self.claim.store.publish_staged(&digest, &path)?;
            }
            Sink::Memory(buf) => {
                self.claim.store.put_chunk(buf);
            }
        }
        Ok(digest)
    }

    /// discard the staged bytes (a refused or superseded transfer).
    pub fn abort(self) {
        if let Sink::Disk { path, .. } = &self.sink {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// the whole file's sha256 and its length, read from offset 0.
fn hash_whole_file(file: &mut std::fs::File) -> std::io::Result<([u8; 32], u64)> {
    file.seek(std::io::SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut read = 0u64;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok((hasher.finalize().into(), read));
        }
        hasher.update(&buf[..n]);
        read += n as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(bytes: &[u8]) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(bytes);
        h.finalize().into()
    }

    #[test]
    fn staged_blob_streams_verifies_and_publishes() {
        let root = tempfile::tempdir().unwrap();
        let store = BlobHandle::persistent(root.path()).unwrap();
        let payload = vec![7u8; 3 * 256 * 1024 + 17];
        let digest = sha(&payload);

        let mut slot = store.stage(digest, payload.len() as u64).unwrap();
        assert_eq!(slot.offset(), 0);
        for chunk in payload.chunks(256 * 1024) {
            slot.append(chunk).unwrap();
        }
        assert_eq!(slot.finish().unwrap(), digest);
        assert!(store.has_chunk(&digest));
        assert_eq!(
            store.get_chunk(&digest).as_deref(),
            Some(payload.as_slice())
        );
        // published under the content-addressed name; staging slot gone.
        assert!(root.path().join(hex(&digest)).is_file());
        assert!(!root.path().join("staging").join(hex(&digest)).exists());
    }

    #[test]
    fn resume_continues_from_the_high_water() {
        let root = tempfile::tempdir().unwrap();
        let store = BlobHandle::persistent(root.path()).unwrap();
        let payload = b"first half|second half".to_vec();
        let digest = sha(&payload);

        let mut slot = store.stage(digest, payload.len() as u64).unwrap();
        slot.append(b"first half|").unwrap();
        drop(slot); // transfer died mid-flight — file stays for resume.

        let mut resumed = store.stage(digest, payload.len() as u64).unwrap();
        assert_eq!(resumed.offset(), b"first half|".len() as u64);
        resumed.append(b"second half").unwrap();
        assert_eq!(resumed.finish().unwrap(), digest);
        assert_eq!(
            store.get_chunk(&digest).as_deref(),
            Some(payload.as_slice())
        );
    }

    #[test]
    fn one_digest_has_exactly_one_writer() {
        let root = tempfile::tempdir().unwrap();
        let store = BlobHandle::persistent(root.path()).unwrap();
        let digest = sha(b"contested");

        let first = store.stage(digest, 9).unwrap();
        assert!(
            matches!(store.stage(digest, 9), Err(StageError::AlreadyStaging)),
            "a second writer took the same staging file"
        );
        // a different digest is a different slot, and finishing frees this one.
        store.stage(sha(b"elsewhere"), 4).unwrap().abort();
        first.abort();
        store.stage(digest, 9).unwrap().abort();
    }

    #[test]
    fn finish_hashes_the_file_not_the_stream() {
        let root = tempfile::tempdir().unwrap();
        let store = BlobHandle::persistent(root.path()).unwrap();
        let payload = b"the true bytes".to_vec();
        let digest = sha(&payload);
        let path = root.path().join("staging").join(hex(&digest));

        let mut slot = store.stage(digest, payload.len() as u64).unwrap();
        slot.append(&payload).unwrap();
        // the file is holed under the writer — exactly what a second lane
        // truncating the shared staging file does. the writer's own running
        // hash saw only the true bytes; the FILE is what publishes.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(4)
            .unwrap();

        assert!(matches!(slot.finish(), Err(StageError::HashMismatch)));
        assert!(!store.has_chunk(&digest), "holed bytes were published");
        assert!(!path.exists(), "the poisoned partial was kept");
    }

    #[test]
    fn an_abandoned_partial_survives_its_window_and_is_swept_past_it() {
        let root = tempfile::tempdir().unwrap();
        let digest = sha(b"long gone");
        let path = root.path().join("staging").join(hex(&digest));
        {
            let store = BlobHandle::persistent(root.path()).unwrap();
            let mut slot = store.stage(digest, 9).unwrap();
            slot.append(b"long").unwrap();
        }
        assert!(path.is_file(), "a dropped transfer keeps its partial");

        // a fresh partial is resumable, so re-opening the store keeps it.
        let store = BlobHandle::persistent(root.path()).unwrap();
        assert_eq!(store.sweep_staging(STAGING_RESUME_WINDOW).unwrap(), 0);
        assert!(path.is_file());
        drop(store);

        // past the window nothing will ever resume it, and a store opened over
        // the root (a node boot) reclaims it.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::now() - STAGING_RESUME_WINDOW - Duration::from_secs(60))
            .unwrap();
        let _store = BlobHandle::persistent(root.path()).unwrap();
        assert!(!path.exists(), "the stale partial was not reclaimed");
    }

    #[test]
    fn wrong_bytes_never_publish_and_do_not_resume() {
        let root = tempfile::tempdir().unwrap();
        let store = BlobHandle::persistent(root.path()).unwrap();
        let digest = sha(b"the true bytes");

        let mut slot = store.stage(digest, 14).unwrap();
        slot.append(b"the LIED bytes").unwrap();
        assert!(matches!(slot.finish(), Err(StageError::HashMismatch)));
        assert!(!store.has_chunk(&digest));
        // poisoned staging is dropped — a retry starts clean.
        let fresh = store.stage(digest, 14).unwrap();
        assert_eq!(fresh.offset(), 0);
        fresh.abort();
    }

    #[test]
    fn length_lies_are_rejected_at_the_boundary() {
        let store = BlobHandle::default();
        let digest = sha(b"xyz");
        let mut slot = store.stage(digest, 3).unwrap();
        assert!(matches!(
            slot.append(b"xyzz"),
            Err(StageError::Length {
                expected: 3,
                got: 4
            })
        ));
        slot.append(b"xyz").unwrap();
        assert_eq!(slot.finish().unwrap(), digest);
        assert!(store.has_chunk(&digest));
    }

    #[test]
    fn short_finish_is_length_error_and_resumable() {
        let store = BlobHandle::default();
        let digest = sha(b"abcdef");
        let mut slot = store.stage(digest, 6).unwrap();
        slot.append(b"abc").unwrap();
        assert!(matches!(
            slot.finish(),
            Err(StageError::Length {
                expected: 6,
                got: 3
            })
        ));
    }
}
