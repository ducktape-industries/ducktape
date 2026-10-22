// The node's blob store: small blobs as zstd records in append-only segments, a blob larger than a
// segment as its own file of zstd frames, and the one file per blob older builds kept, read where it
// lies until a commit moves it in. Every whole read is checked against the blob's id.

mod frames;
mod segments;
mod tidier;

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead as _, BufReader, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(any(test, feature = "testkit"))]
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use abi::{Blob, BlobHeader, BlobId, HashKind, Refusal, reason};

use crate::contract::Entry;
use crate::{Collision, Hasher, frame, header, id_of, is_kind, name_of, parse, parse_header};
use segments::{Copier, Hit, Log, Plan, Round};
pub use tidier::{Tidied, Tidier};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("blob store io: {0}")]
    Io(#[from] std::io::Error),
    #[error("blob {0:?} is corrupt on disk: {1}")]
    Corrupt(BlobId, String),
    #[error(transparent)]
    Collision(#[from] Collision),
}

pub type Result<T> = std::result::Result<T, Error>;

pub const SEGMENT_BYTES: u64 = 16 << 20;
pub const FRAME_BYTES: u64 = 1 << 20;

const SEGMENTS: &str = "seg";
const FRAMED: &str = "big";
const HELD: &str = ".held";
const STAGING: &str = ".staging";
const PARTIAL: &str = ".partial";
const HEADER_BYTES: u64 = 4 << 10;

static OPEN: Mutex<BTreeMap<PathBuf, Weak<Shared>>> = Mutex::new(BTreeMap::new());

#[derive(Clone, Default)]
pub struct Stage {
    framed: BTreeMap<BlobId, Vec<u8>>,
}

impl Stage {
    pub fn put(
        &mut self,
        hash: HashKind,
        kind: &str,
        body: &[u8],
    ) -> std::result::Result<BlobId, Refusal> {
        let framed = frame(kind, body)?;
        let id = id_of(hash, &framed)?;
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

#[derive(Clone, Copy, Debug)]
struct Sizes {
    segment: u64,
    frame: u64,
}

#[derive(Clone)]
pub struct Blobs {
    shared: Arc<Shared>,
}

struct Shared {
    dir: PathBuf,
    sizes: Sizes,
    log: Mutex<Log>,
    copier: Mutex<Copier>,
    moving: Mutex<Moving>,
    partials: AtomicU64,
    held: fs::File,
    #[cfg(any(test, feature = "testkit"))]
    gate: Mutex<Option<Armed>>,
}

#[cfg(any(test, feature = "testkit"))]
struct Armed {
    reached: mpsc::SyncSender<()>,
    go: mpsc::Receiver<()>,
    on: std::thread::ThreadId,
}

#[cfg(any(test, feature = "testkit"))]
pub struct CopyGate {
    reached: mpsc::Receiver<()>,
    _go: mpsc::SyncSender<()>,
}

#[cfg(any(test, feature = "testkit"))]
impl CopyGate {
    pub fn reached(&self) {
        self.reached
            .recv()
            .expect("a tidy round reaches the gate before the store lets go of it");
    }
}

#[derive(Default)]
struct Moving {
    listing: Option<fs::ReadDir>,
    ended: bool,
    waiting: Option<Legacy>,
}

struct Legacy {
    id: BlobId,
    path: PathBuf,
    len: u64,
}

struct Partial {
    path: PathBuf,
    kind: String,
    len: u64,
}

enum Moved {
    Logged(segments::Record),
    Framed(Partial),
}

enum Found {
    Held(Hit),
    Legacy,
}

pub struct Reader {
    len: u64,
    at: u64,
    pieces: Pieces,
}

enum Pieces {
    Whole(Vec<u8>),
    Legacy(fs::File),
    Frames(frames::Frames),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unpacked {
    pub written: u64,
    pub unread: u64,
}

impl Blobs {
    pub fn open(dir: &Path) -> Result<Blobs> {
        let sizes = Sizes {
            segment: SEGMENT_BYTES,
            frame: FRAME_BYTES,
        };
        Blobs::sized(dir, sizes)
    }

    fn sized(dir: &Path, sizes: Sizes) -> Result<Blobs> {
        fs::create_dir_all(dir)?;
        let dir = fs::canonicalize(dir)?;
        let mut open = OPEN.lock().expect("the open stores are never poisoned");
        let alive = open.get(&dir).and_then(Weak::upgrade);
        if let Some(shared) = alive.filter(|shared| same_file(&shared.held, &dir.join(HELD))) {
            return Ok(Blobs { shared });
        }
        let held = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(HELD))?;
        held.try_lock().map_err(|refused| match refused {
            fs::TryLockError::Error(error) => error,
            fs::TryLockError::WouldBlock => io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("another process holds the blob store at {}", dir.display()),
            ),
        })?;
        let (mut log, copier) = Log::open(&dir.join(SEGMENTS), sizes.segment, sizes.frame)?;
        for (id, kind, len) in frames::held(&dir.join(FRAMED))? {
            log.hold_framed(id, &kind, len);
        }
        let shared = Arc::new(Shared {
            dir: dir.clone(),
            sizes,
            log: Mutex::new(log),
            copier: Mutex::new(copier),
            moving: Mutex::default(),
            partials: AtomicU64::new(0),
            held,
            #[cfg(any(test, feature = "testkit"))]
            gate: Mutex::default(),
        });
        open.retain(|_, weak| weak.strong_count() > 0);
        open.insert(dir, Arc::downgrade(&shared));
        Ok(Blobs { shared })
    }

    fn log(&self) -> MutexGuard<'_, Log> {
        self.shared
            .log
            .lock()
            .expect("the blob log is never poisoned")
    }

    fn dir(&self) -> &Path {
        &self.shared.dir
    }

    pub fn link(&self, to: &Path) -> Result<()> {
        let appended = self.log().appended();
        Blobs::linked(self.dir(), to, &appended)
    }

    fn linked(from: &Path, to: &Path, appended: &[PathBuf]) -> Result<()> {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            let name = entry.file_name();
            let hidden = name.to_string_lossy().starts_with('.');
            if hidden {
                continue;
            }
            let path = entry.path();
            let target = to.join(&name);
            if entry.file_type()?.is_dir() {
                Blobs::linked(&path, &target, appended)?;
                continue;
            }
            match appended.contains(&path) {
                true => fs::copy(&path, &target).map(drop)?,
                false => fs::hard_link(&path, &target)?,
            }
        }
        Ok(())
    }

    #[cfg(any(test, feature = "testkit"))]
    pub fn gate_copies(&self) -> CopyGate {
        let (reached, heard) = mpsc::sync_channel(1);
        let (go, wait) = mpsc::sync_channel(0);
        let armed = Armed {
            reached,
            go: wait,
            on: std::thread::current().id(),
        };
        *self
            .shared
            .gate
            .lock()
            .expect("the copy gate is never poisoned") = Some(armed);
        CopyGate {
            reached: heard,
            _go: go,
        }
    }

    fn copied(&self) {
        #[cfg(any(test, feature = "testkit"))]
        {
            let armed = self
                .shared
                .gate
                .lock()
                .expect("the copy gate is never poisoned")
                .take();
            if let Some(armed) = armed {
                let elsewhere = armed.on != std::thread::current().id();
                assert!(
                    elsewhere,
                    "a tidy round reached its copy on the thread that holds the gate, which can never let go"
                );
                let _ = armed.reached.send(());
                let _ = armed.go.recv();
            }
        }
    }

    fn legacy(&self, id: &BlobId) -> PathBuf {
        self.dir().join(name_of(id))
    }

    fn framed_path(&self, id: &BlobId) -> PathBuf {
        self.dir().join(FRAMED).join(name_of(id))
    }

    fn found(&self, id: &BlobId) -> Option<Found> {
        let hit = self.log().hit(id);
        if let Some(hit) = hit {
            return Some(Found::Held(hit));
        }
        if self.legacy(id).is_file() {
            return Some(Found::Legacy);
        }
        let hit = self.log().hit(id);
        hit.map(Found::Held)
    }

    pub fn has(&self, id: &BlobId) -> bool {
        self.found(id).is_some()
    }

    pub fn holds_named(&self, name: &str) -> bool {
        named(name).is_some_and(|id| self.has(&id))
    }

    pub fn write(&self, id: &BlobId, framed: &[u8]) -> Result<()> {
        if self.has(id) {
            return Ok(());
        }
        self.keep(*id, framed)?;
        self.sync()
    }

    fn keep(&self, id: BlobId, framed: &[u8]) -> Result<()> {
        let over = framed.len() as u64 > self.shared.sizes.segment;
        if over {
            let partial = self.framed_partial(id, framed, framed.len() as u64)?;
            return self.publish_framed(&mut self.log(), id, &partial);
        }
        let record = segments::record(id, framed, self.shared.sizes.frame)?;
        self.log().log(&record)
    }

    pub fn remove(&self, id: &BlobId) -> Result<()> {
        let mut log = self.log();
        let framed = log.unframe(id);
        let legacy = self.legacy(id);
        let only_legacy = !framed && !log.holds(id) && legacy.is_file();
        log.bury(id)?;
        if only_legacy {
            log.bury_legacy(id)?;
        }
        if framed {
            gone(fs::remove_file(self.framed_path(id)))?;
        }
        gone(fs::remove_file(legacy))
    }

    pub fn promote(&self, stage: Stage) -> Result<()> {
        let mut records = Vec::new();
        for (id, framed) in stage.framed {
            if self.has(&id) {
                continue;
            }
            let over = framed.len() as u64 > self.shared.sizes.segment;
            match over {
                true => self.keep(id, &framed)?,
                false => records.push(segments::record(id, &framed, self.shared.sizes.frame)?),
            }
        }
        let mut log = self.log();
        for record in &records {
            log.log(record)?;
        }
        drop(log);
        self.sync()
    }

    pub fn tidy(&self) -> Result<Tidied> {
        let whole = self.shared.sizes.segment;
        let mut copier = self
            .shared
            .copier
            .lock()
            .expect("the copy stream is never poisoned");
        let worn = self.log().worn();
        let into_worn = copier.active().is_some_and(|n| worn.contains(&n));
        if into_worn {
            copier.roll()?;
        }
        let (plans, batch) = {
            let mut moving = self
                .shared
                .moving
                .lock()
                .expect("the legacy listing is never poisoned");
            let waiting = moving.waiting.take_if(|_| !worn.is_empty());
            match waiting {
                Some(legacy) => (Vec::new(), vec![legacy]),
                None => {
                    let mut budget = whole;
                    let mut plans = Vec::new();
                    for n in worn {
                        if budget == 0 {
                            break;
                        }
                        let plan = self.log().plan(n, budget, whole)?;
                        planned();
                        budget = budget.saturating_sub(plan.bytes);
                        let leaves_records = !plan.drains;
                        plans.push(plan);
                        if leaves_records {
                            break;
                        }
                    }
                    let batch = self.legacy_batch(&mut moving, budget, whole)?;
                    (plans, batch)
                }
            }
        };
        let mut round = Round::default();
        let mut framed = Vec::new();
        let copied = self.copy(plans, batch, &mut copier, &mut round, &mut framed);
        let written = copier.appended();
        if let Err(error) = copied {
            self.log().strays(round, &mut copier);
            for (_, partial) in framed {
                let _ = fs::remove_file(&partial.path);
            }
            return Err(error);
        }
        self.copied();
        let mut log = self.log();
        let landed = log.land(round, &mut copier);
        let retired = landed.drained.len() as u64;
        let mut unlink = landed.unlink;
        let mut dropped = Vec::new();
        let mut published = Vec::new();
        for (legacy, partial) in framed {
            let fresh = legacy.path.is_file() && !log.holds(&legacy.id);
            match fresh {
                true => {
                    fs::rename(&partial.path, self.framed_path(&legacy.id))?;
                    log.hold_framed(legacy.id, &partial.kind, partial.len);
                    published.push(legacy.path);
                }
                false => {
                    dropped.push(partial.path);
                    if log.holds(&legacy.id) {
                        unlink.push(legacy.path);
                    }
                }
            }
        }
        drop(log);
        drop(copier);
        for path in landed.drained.into_iter().chain(dropped) {
            gone(fs::remove_file(path))?;
        }
        if !published.is_empty() {
            synced_dir(&self.dir().join(FRAMED))?;
        }
        let moved = (unlink.len() + published.len()) as u64;
        for path in unlink.into_iter().chain(published) {
            gone(fs::remove_file(path))?;
        }
        Ok(Tidied {
            copied: written,
            moved,
            retired,
        })
    }

    fn copy(
        &self,
        plans: Vec<Plan>,
        batch: Vec<Legacy>,
        copier: &mut Copier,
        round: &mut Round,
        framed: &mut Vec<(Legacy, Partial)>,
    ) -> Result<()> {
        for plan in plans {
            plan.copy(copier, round)?;
        }
        for legacy in batch {
            match self.moved(&legacy) {
                Ok(Some(Moved::Logged(record))) => round.moved(copier, record, legacy.path)?,
                Ok(Some(Moved::Framed(partial))) => framed.push((legacy, partial)),
                Ok(None) | Err(Error::Corrupt(..) | Error::Collision(_)) => {}
                Err(error) => return Err(error),
            }
        }
        copier.sync()
    }

    fn sync(&self) -> Result<()> {
        let (files, made) = self.log().dirty();
        for file in &files {
            file.sync_data()?;
        }
        if made {
            synced_dir(&self.dir().join(SEGMENTS))?;
        }
        Ok(())
    }

    fn framed_partial(&self, id: BlobId, source: impl io::Read, len: u64) -> Result<Partial> {
        let n = self.shared.partials.fetch_add(1, Ordering::Relaxed);
        let path = self
            .dir()
            .join(FRAMED)
            .join(format!("{}.{n}{PARTIAL}", name_of(&id)));
        match frames::write(&path, id, source, len, self.shared.sizes.frame) {
            Ok((kind, len)) => Ok(Partial { path, kind, len }),
            Err(error) => {
                let _ = fs::remove_file(&path);
                Err(error)
            }
        }
    }

    fn publish_framed(&self, log: &mut Log, id: BlobId, partial: &Partial) -> Result<()> {
        if log.holds(&id) {
            return gone(fs::remove_file(&partial.path));
        }
        fs::rename(&partial.path, self.framed_path(&id))?;
        synced_dir(&self.dir().join(FRAMED))?;
        log.hold_framed(id, &partial.kind, partial.len);
        Ok(())
    }

    fn legacy_batch(&self, moving: &mut Moving, budget: u64, whole: u64) -> Result<Vec<Legacy>> {
        let mut batch = Vec::new();
        let mut bytes = 0u64;
        let fits = |bytes: u64, len: u64| bytes + len <= budget || (bytes == 0 && budget == whole);
        if let Some(waiting) = moving.waiting.take() {
            if !fits(bytes, waiting.len) {
                moving.waiting = Some(waiting);
                return Ok(batch);
            }
            bytes += waiting.len;
            batch.push(waiting);
        }
        while !moving.ended {
            if moving.listing.is_none() {
                moving.listing = Some(fs::read_dir(self.dir())?);
            }
            let next = moving.listing.as_mut().and_then(Iterator::next);
            let Some(entry) = next else {
                moving.ended = true;
                moving.listing = None;
                break;
            };
            let entry = entry?;
            let Some(id) = entry.file_name().to_str().and_then(named) else {
                continue;
            };
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if !meta.is_file() {
                continue;
            }
            let legacy = Legacy {
                id,
                path: entry.path(),
                len: meta.len(),
            };
            let over = !fits(bytes, legacy.len);
            if over {
                moving.waiting = Some(legacy);
                break;
            }
            bytes += legacy.len;
            batch.push(legacy);
        }
        Ok(batch)
    }

    fn moved(&self, legacy: &Legacy) -> Result<Option<Moved>> {
        let over = legacy.len > self.shared.sizes.segment;
        if over {
            let file = match fs::File::open(&legacy.path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            let partial = self.framed_partial(legacy.id, file, legacy.len)?;
            return Ok(Some(Moved::Framed(partial)));
        }
        let framed = match fs::read(&legacy.path) {
            Ok(framed) => framed,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let framed = verified(&legacy.id, framed)?;
        let record = segments::legacy_record(legacy.id, &framed, self.shared.sizes.frame)?;
        Ok(Some(Moved::Logged(record)))
    }

    pub fn unpack(self) -> Result<Unpacked> {
        let ids = self.log().ids();
        let mut unpacked = Unpacked::default();
        for id in ids {
            let framed = match self.framed_held(&id) {
                Ok(Some(framed)) => framed,
                Ok(None) => continue,
                Err(Error::Corrupt(..) | Error::Collision(_)) => {
                    unpacked.unread += 1;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let partial = self.dir().join(format!("{}{PARTIAL}", name_of(&id)));
            let mut file = fs::File::create(&partial)?;
            file.write_all(&framed)?;
            file.sync_data()?;
            fs::rename(&partial, self.legacy(&id))?;
            unpacked.written += 1;
        }
        synced_dir(self.dir())?;
        gone(fs::remove_dir_all(self.dir().join(SEGMENTS)))?;
        gone(fs::remove_dir_all(self.dir().join(FRAMED)))?;
        Ok(unpacked)
    }

    pub fn framed(&self, id: &BlobId) -> Result<Option<Vec<u8>>> {
        match self.found(id) {
            None => Ok(None),
            Some(Found::Held(hit)) => self.whole(id, hit, false),
            Some(Found::Legacy) => match legacy_framed(&self.legacy(id), id, false)? {
                Some(framed) => Ok(Some(framed)),
                None => self.framed_held(id),
            },
        }
    }

    pub fn framed_once(&self, id: &BlobId) -> Result<Option<Vec<u8>>> {
        match self.found(id) {
            None => Ok(None),
            Some(Found::Held(hit)) => self.whole(id, hit, true),
            Some(Found::Legacy) => match legacy_framed(&self.legacy(id), id, true)? {
                Some(framed) => Ok(Some(framed)),
                None => self.framed_held(id),
            },
        }
    }

    fn framed_held(&self, id: &BlobId) -> Result<Option<Vec<u8>>> {
        let hit = self.log().hit(id);
        match hit {
            None => Ok(None),
            Some(hit) => self.whole(id, hit, false),
        }
    }

    fn whole(&self, id: &BlobId, hit: Hit, once: bool) -> Result<Option<Vec<u8>>> {
        match hit {
            Hit::Logged { file, logged, kind } => {
                segments::framed(&file, id, &logged, &kind, once).map(Some)
            }
            Hit::Framed { .. } => frames::whole(&self.framed_path(id), id),
        }
    }

    pub fn get(&self, id: &BlobId) -> Result<Option<Blob>> {
        let Some(framed) = self.framed(id)? else {
            return Ok(None);
        };
        let (header, body) =
            parse(&framed).ok_or_else(|| Error::Corrupt(*id, "the frame does not parse".into()))?;
        Ok(Some(Blob {
            kind: header.kind,
            body: body.to_vec(),
        }))
    }

    fn open_legacy(&self, id: &BlobId) -> Result<Option<(fs::File, BlobHeader, u64)>> {
        let file = match fs::File::open(self.legacy(id)) {
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
        match self.found(id) {
            None => Ok(None),
            Some(Found::Held(hit)) => Ok(Some(hit.header())),
            Some(Found::Legacy) => match self.open_legacy(id)? {
                Some((_, header, _)) => Ok(Some(header)),
                None => Ok(self.log().hit(id).map(|hit| hit.header())),
            },
        }
    }

    pub fn read(&self, id: &BlobId, offset: u64, len: u64) -> Result<Option<Vec<u8>>> {
        match self.found(id) {
            None => Ok(None),
            Some(Found::Held(hit)) => self.read_held(id, hit, offset, len),
            Some(Found::Legacy) => match self.read_legacy(id, offset, len)? {
                Some(bytes) => Ok(Some(bytes)),
                None => {
                    let hit = self.log().hit(id);
                    match hit {
                        None => Ok(None),
                        Some(hit) => self.read_held(id, hit, offset, len),
                    }
                }
            },
        }
    }

    fn read_held(&self, id: &BlobId, hit: Hit, offset: u64, want: u64) -> Result<Option<Vec<u8>>> {
        match hit {
            Hit::Logged { file, logged, kind } => {
                segments::range(&file, id, &logged, &kind, offset, want).map(Some)
            }
            Hit::Framed { .. } => frames::read(&self.framed_path(id), id, offset, want),
        }
    }

    fn read_legacy(&self, id: &BlobId, offset: u64, len: u64) -> Result<Option<Vec<u8>>> {
        let Some((mut file, header, body_start)) = self.open_legacy(id)? else {
            return Ok(None);
        };
        let (start, end) = window(header.len, offset, len);
        file.seek(SeekFrom::Start(body_start + start))?;
        let mut bytes = vec![0u8; (end - start) as usize];
        file.read_exact(&mut bytes)?;
        Ok(Some(bytes))
    }

    pub fn size(&self, id: &BlobId) -> Result<Option<u64>> {
        let header = self.stat(id)?;
        Ok(header.map(|header| framed_len(&header.kind, header.len)))
    }

    pub fn reader(&self, id: &BlobId, offset: u64) -> Result<Option<Reader>> {
        let pieces = match self.found(id) {
            None => None,
            Some(Found::Held(hit)) => self.pieces(id, hit)?,
            Some(Found::Legacy) => match open_file(&self.legacy(id))? {
                Some(file) => Some(Pieces::Legacy(file)),
                None => {
                    let hit = self.log().hit(id);
                    match hit {
                        None => None,
                        Some(hit) => self.pieces(id, hit)?,
                    }
                }
            },
        };
        let Some(pieces) = pieces else {
            return Ok(None);
        };
        Reader::at(pieces, offset).map(Some)
    }

    fn pieces(&self, id: &BlobId, hit: Hit) -> Result<Option<Pieces>> {
        match hit {
            Hit::Logged { file, logged, kind } => {
                let framed = segments::framed(&file, id, &logged, &kind, true)?;
                Ok(Some(Pieces::Whole(framed)))
            }
            Hit::Framed { .. } => {
                let frames = frames::Frames::open(&self.framed_path(id), id)?;
                Ok(frames.map(Pieces::Frames))
            }
        }
    }

    pub fn staged(&self, id: &BlobId) -> PathBuf {
        self.dir().join(STAGING).join(name_of(id))
    }

    pub fn staging(&self, id: &BlobId, upload: u64) -> Result<PathBuf> {
        let dir = self.dir().join(STAGING);
        fs::create_dir_all(&dir)?;
        Ok(dir.join(format!("{}.{upload}", name_of(id))))
    }

    pub fn clear_staging(&self) -> Result<()> {
        gone(fs::remove_dir_all(self.dir().join(STAGING)))
    }

    pub fn partial(&self, id: &BlobId) -> Result<PathBuf> {
        Ok(self.partials()?.join(name_of(id)))
    }

    pub fn partials(&self) -> Result<PathBuf> {
        let dir = self.dir().join(PARTIAL);
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    pub fn adopt(&self, id: &BlobId, from: &Path) -> Result<()> {
        if !self.has(id) {
            let len = fs::metadata(from)?.len();
            let over = len > self.shared.sizes.segment;
            match over {
                true => {
                    let partial = self.framed_partial(*id, fs::File::open(from)?, len)?;
                    self.publish_framed(&mut self.log(), *id, &partial)?;
                }
                false => {
                    let record = segments::record(*id, &fs::read(from)?, self.shared.sizes.frame)?;
                    self.log().log(&record)?;
                    self.sync()?;
                }
            }
        }
        fs::remove_file(from)?;
        Ok(())
    }
}

impl Reader {
    fn at(mut pieces: Pieces, offset: u64) -> Result<Reader> {
        let len = match &mut pieces {
            Pieces::Whole(framed) => framed.len() as u64,
            Pieces::Legacy(file) => {
                let len = file.metadata()?.len();
                file.seek(SeekFrom::Start(offset.min(len)))?;
                len
            }
            Pieces::Frames(frames) => frames.framed_len(),
        };
        Ok(Reader {
            len,
            at: offset,
            pieces,
        })
    }

    pub fn framed_len(&self) -> u64 {
        self.len
    }

    pub fn piece(&mut self) -> Result<Option<Vec<u8>>> {
        if self.at >= self.len {
            return Ok(None);
        }
        let piece = match &mut self.pieces {
            Pieces::Whole(framed) => framed.split_off(self.at as usize),
            Pieces::Legacy(file) => {
                let mut bytes = vec![0u8; (self.len - self.at).min(FRAME_BYTES) as usize];
                file.read_exact(&mut bytes)?;
                bytes
            }
            Pieces::Frames(frames) => frames.tail_at(self.at)?,
        };
        self.at += piece.len() as u64;
        Ok(Some(piece))
    }
}

fn framed_len(kind: &str, len: u64) -> u64 {
    header(kind, len).len() as u64 + len
}

fn verified(id: &BlobId, framed: Vec<u8>) -> Result<Vec<u8>> {
    let named = id_of(id.kind(), &framed)? == *id;
    if !named {
        return Err(Error::Corrupt(
            *id,
            "its bytes do not hash to its id".into(),
        ));
    }
    Ok(framed)
}

fn legacy_framed(path: &Path, id: &BlobId, once: bool) -> Result<Option<Vec<u8>>> {
    let Some(mut file) = open_file(path)? else {
        return Ok(None);
    };
    if once {
        not_kept(&file, 0, 0)?;
    }
    let mut framed = Vec::new();
    file.read_to_end(&mut framed)?;
    verified(id, framed).map(Some)
}

fn open_file(path: &Path) -> Result<Option<fs::File>> {
    match fs::File::open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn gone(removed: io::Result<()>) -> Result<()> {
    match removed {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn named(name: &str) -> Option<BlobId> {
    let bytes = unhex(name)?;
    let id: BlobId = abi::decode(&bytes).ok()?;
    let exact = name_of(&id) == name;
    exact.then_some(id)
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect()
}

#[cfg(unix)]
fn read_at(file: &fs::File, bytes: &mut [u8], at: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, bytes, at)
}

#[cfg(unix)]
fn write_at(file: &fs::File, bytes: &[u8], at: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, bytes, at)
}

#[cfg(windows)]
fn read_at(file: &fs::File, mut bytes: &mut [u8], mut at: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt as _;
    while !bytes.is_empty() {
        let read = file.seek_read(bytes, at)?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        bytes = &mut std::mem::take(&mut bytes)[read..];
        at += read as u64;
    }
    Ok(())
}

#[cfg(windows)]
fn write_at(file: &fs::File, mut bytes: &[u8], mut at: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt as _;
    while !bytes.is_empty() {
        let written = file.seek_write(bytes, at)?;
        if written == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        bytes = &bytes[written..];
        at += written as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn same_file(file: &fs::File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let (Ok(open), Ok(named)) = (file.metadata(), fs::metadata(path)) else {
        return false;
    };
    (open.dev(), open.ino()) == (named.dev(), named.ino())
}

#[cfg(not(unix))]
fn same_file(_: &fs::File, path: &Path) -> bool {
    path.is_file()
}

#[cfg(unix)]
fn synced_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn synced_dir(_: &Path) -> io::Result<()> {
    Ok(())
}

pub fn size_of(path: &Path) -> Result<Option<u64>> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata.len())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn file_entry(kind: &str, path: &Path) -> std::result::Result<Entry, Refusal> {
    if !is_kind(kind) {
        return Err(Refusal::new(
            reason::INVALID_INPUT,
            "a blob kind is one non-empty word without spaces or NUL",
        ));
    }
    let unread = |error: std::io::Error| {
        Refusal::new(
            reason::NOT_FOUND,
            format!("{} does not read: {error}", path.display()),
        )
    };
    let mut file = fs::File::open(path).map_err(unread)?;
    let len = file.metadata().map_err(unread)?.len();
    let header = header(kind, len);
    let mut hasher = Hasher::new(HashKind::Sha256);
    hasher.update(&header);
    let mut chunk = vec![0u8; 64 << 10];
    loop {
        let read = file.read(&mut chunk).map_err(unread)?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    Ok(Entry {
        id: hasher.finish()?,
        len: header.len() as u64 + len,
        kind: kind.to_owned(),
    })
}

pub fn names(path: &Path, id: &BlobId) -> Result<Option<BlobHeader>> {
    let mut file = BufReader::new(fs::File::open(path)?);
    let mut head = Vec::new();
    (&mut file).take(HEADER_BYTES).read_until(0, &mut head)?;
    let Some(header) = head.strip_suffix(&[0]).and_then(parse_header) else {
        return Ok(None);
    };
    let mut hasher = Hasher::new(id.kind());
    hasher.update(&head);
    let mut body = 0u64;
    let mut chunk = vec![0u8; 64 << 10];
    loop {
        let read = file.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
        body += read as u64;
    }
    let sized = body == header.len;
    let hashed = hasher.finish()? == *id;
    Ok((sized && hashed).then_some(header))
}

pub fn window(len: u64, offset: u64, want: u64) -> (u64, u64) {
    let start = offset.min(len);
    let end = offset.saturating_add(want).min(len);
    (start, end)
}

pub struct Layered<'a> {
    pub stage: &'a Stage,
    pub blobs: &'a Blobs,
}

impl Layered<'_> {
    pub fn framed(&self, id: &BlobId) -> Result<Option<Vec<u8>>> {
        if let Some(framed) = self.stage.get(id) {
            return Ok(Some(framed.to_vec()));
        }
        self.blobs.framed(id)
    }

    pub fn get(&self, id: &BlobId) -> Result<Option<Blob>> {
        let Some(framed) = self.stage.get(id) else {
            return self.blobs.get(id);
        };
        let (header, body) = staged(id, framed)?;
        Ok(Some(Blob {
            kind: header.kind,
            body: body.to_vec(),
        }))
    }

    pub fn stat(&self, id: &BlobId) -> Result<Option<BlobHeader>> {
        let Some(framed) = self.stage.get(id) else {
            return self.blobs.stat(id);
        };
        Ok(Some(staged(id, framed)?.0))
    }

    pub fn read(&self, id: &BlobId, offset: u64, len: u64) -> Result<Option<Vec<u8>>> {
        let Some(framed) = self.stage.get(id) else {
            return self.blobs.read(id, offset, len);
        };
        let (header, body) = staged(id, framed)?;
        let (start, end) = window(header.len, offset, len);
        Ok(Some(body[start as usize..end as usize].to_vec()))
    }
}

fn staged<'a>(id: &BlobId, framed: &'a [u8]) -> Result<(BlobHeader, &'a [u8])> {
    parse(framed).ok_or_else(|| Error::Corrupt(*id, "the staged frame does not parse".into()))
}

#[cfg(target_os = "linux")]
fn not_kept(file: &fs::File, offset: u64, len: u64) -> std::io::Result<()> {
    use rustix::fs::{Advice, fadvise};
    Ok(fadvise(
        file,
        offset,
        std::num::NonZeroU64::new(len),
        Advice::NoReuse,
    )?)
}

#[cfg(not(target_os = "linux"))]
fn not_kept(_: &fs::File, _: u64, _: u64) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
thread_local! {
    static INFLATED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static PLANNED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn planned() {
    PLANNED.with(|planned| planned.set(planned.get() + 1));
}

#[cfg(not(test))]
fn planned() {}

#[cfg(test)]
fn inflated(bytes: usize) {
    INFLATED.with(|inflated| inflated.set(inflated.get() + bytes as u64));
}

#[cfg(not(test))]
fn inflated(_: usize) {}

#[cfg(test)]
mod tests {
    use abi::BlobHeader;

    use super::*;

    #[test]
    fn a_staged_blob_is_promoted_and_read_back_whole_or_by_range() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::open(dir.path()).unwrap();
        let mut stage = Stage::default();
        let id = stage.put(HashKind::Sha256, "page", b"0123456789").unwrap();
        let again = stage.put(HashKind::Sha256, "page", b"0123456789").unwrap();
        assert_eq!(id, again);
        assert!(!blobs.has(&id));
        let layered = Layered {
            stage: &stage,
            blobs: &blobs,
        };
        assert_eq!(
            layered.framed(&id).unwrap().unwrap(),
            b"page 10\x000123456789"
        );
        assert_eq!(layered.read(&id, 3, 4).unwrap().unwrap(), b"3456");
        assert_eq!(layered.stat(&id).unwrap().unwrap().len, 10);
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
        assert_eq!(blobs.framed_once(&id).unwrap(), blobs.framed(&id).unwrap());
        assert_eq!(blobs.framed_once(&BlobId::Sha1([0; 20])).unwrap(), None);
        assert_eq!(blobs.get(&BlobId::Sha1([0; 20])).unwrap(), None);
        assert_eq!(blobs.stat(&BlobId::Sha1([0; 20])).unwrap(), None);
    }

    #[test]
    fn a_removed_blob_is_gone_and_removing_it_again_is_no_error() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::open(dir.path()).unwrap();
        let mut stage = Stage::default();
        let id = stage.put(HashKind::Sha256, "part", b"gone").unwrap();
        blobs.promote(stage).unwrap();
        assert!(blobs.has(&id));
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

    fn every_file(dir: &Path) -> BTreeMap<PathBuf, u64> {
        let mut files = BTreeMap::new();
        let mut dirs = vec![dir.to_path_buf()];
        while let Some(at) = dirs.pop() {
            for entry in fs::read_dir(&at).unwrap() {
                let entry = entry.unwrap();
                let meta = entry.metadata().unwrap();
                match meta.is_dir() {
                    true => dirs.push(entry.path()),
                    false => {
                        files.insert(entry.path(), meta.len());
                    }
                }
            }
        }
        files
    }

    fn allocated(dir: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt as _;
        every_file(dir)
            .keys()
            .map(|path| fs::metadata(path).unwrap().blocks() * 512)
            .sum()
    }

    fn grown(
        before: &BTreeMap<PathBuf, u64>,
        after: &BTreeMap<PathBuf, u64>,
    ) -> (PathBuf, u64, u64) {
        let changed: Vec<(PathBuf, u64, u64)> = after
            .iter()
            .map(|(path, len)| (path.clone(), before.get(path).copied().unwrap_or(0), *len))
            .filter(|(_, was, now)| was != now)
            .collect();
        assert_eq!(changed.len(), 1, "one file took the write: {changed:?}");
        changed.into_iter().next().unwrap()
    }

    fn noise(len: u64, seed: u64) -> Vec<u8> {
        let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn text(i: u64) -> Vec<u8> {
        format!(
            "{i:08} {}",
            "a blob of text that compresses well ".repeat(56)
        )
        .into_bytes()
    }

    #[test]
    fn small_blobs_take_less_room_than_their_frames_and_give_it_back_once_released() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::open(dir.path()).unwrap();
        let mut stage = Stage::default();
        let ids: Vec<BlobId> = (0..1000)
            .map(|i| stage.put(HashKind::Sha256, "page", &text(i)).unwrap())
            .collect();
        let framed: u64 = (0..1000)
            .map(|i| frame("page", &text(i)).unwrap().len() as u64)
            .sum();
        blobs.promote(stage).unwrap();
        blobs.tidy().unwrap();
        let held = allocated(dir.path());
        assert!(
            held < framed,
            "{held} bytes on disk hold {framed} framed bytes"
        );

        for id in &ids[100..] {
            blobs.remove(id).unwrap();
        }
        blobs.promote(Stage::default()).unwrap();
        blobs.tidy().unwrap();
        let kept = allocated(dir.path());
        assert!(
            kept * 4 < held,
            "{kept} bytes stay of {held} once 900 of 1000 blobs are released"
        );
        let reads = |blobs: &Blobs| {
            for (i, id) in ids.iter().enumerate() {
                let body = blobs.get(id).unwrap().map(|blob| blob.body);
                let want = (i < 100).then(|| text(i as u64));
                assert_eq!(body, want, "blob {i}");
            }
        };
        reads(&blobs);
        drop(blobs);
        reads(&Blobs::open(dir.path()).unwrap());
    }

    #[test]
    fn a_store_cut_in_its_last_write_reopens_with_every_earlier_blob_and_without_the_cut_one() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::open(dir.path()).unwrap();
        let mut stage = Stage::default();
        let page = stage.put(HashKind::Sha256, "page", &text(1)).unwrap();
        let object = stage.put(HashKind::Sha1, "blob", &noise(5000, 2)).unwrap();
        blobs.promote(stage).unwrap();
        let before = every_file(dir.path());
        let mut stage = Stage::default();
        let cut = stage
            .put(HashKind::Sha256, "page", &noise(5000, 3))
            .unwrap();
        blobs.promote(stage).unwrap();
        let (file, was, now) = grown(&before, &every_file(dir.path()));
        drop(blobs);
        fs::OpenOptions::new()
            .write(true)
            .open(&file)
            .unwrap()
            .set_len(was + (now - was) / 2)
            .unwrap();

        let blobs = Blobs::open(dir.path()).unwrap();
        assert!(!blobs.has(&cut), "the blob whose write was cut is absent");
        assert_eq!(blobs.get(&cut).unwrap(), None);
        assert_eq!(blobs.get(&page).unwrap().unwrap().body, text(1));
        assert_eq!(blobs.get(&object).unwrap().unwrap().body, noise(5000, 2));
        let mut stage = Stage::default();
        let after = stage.put(HashKind::Sha256, "page", &text(4)).unwrap();
        blobs.promote(stage).unwrap();
        drop(blobs);

        let blobs = Blobs::open(dir.path()).unwrap();
        assert_eq!(blobs.get(&after).unwrap().unwrap().body, text(4));
        assert_eq!(blobs.get(&object).unwrap().unwrap().body, noise(5000, 2));
        assert!(!blobs.has(&cut));
    }

    #[test]
    fn a_blob_whose_stored_bytes_flipped_reads_as_corrupt_never_as_other_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::open(dir.path()).unwrap();
        let before = every_file(dir.path());
        let mut stage = Stage::default();
        let id = stage.put(HashKind::Sha256, "page", &text(7)).unwrap();
        blobs.promote(stage).unwrap();
        let (file, _, now) = grown(&before, &every_file(dir.path()));
        let mut bytes = fs::read(&file).unwrap();
        bytes[now as usize - 1] ^= 1;
        fs::write(&file, bytes).unwrap();

        let got = blobs.get(&id);
        assert!(
            matches!(got, Err(Error::Corrupt(..))),
            "{:?}",
            got.map(|blob| blob.map(|blob| blob.body.len()))
        );
        assert!(matches!(blobs.framed(&id), Err(Error::Corrupt(..))));
        assert!(matches!(blobs.framed_once(&id), Err(Error::Corrupt(..))));
        assert!(matches!(blobs.read(&id, 0, 8), Err(Error::Corrupt(..))));
    }

    #[test]
    fn a_range_of_a_big_blob_reads_only_the_frames_it_covers() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::open(dir.path()).unwrap();
        let body = noise(SEGMENT_BYTES + 3 * FRAME_BYTES + 77, 5);
        let head = header("part", body.len() as u64).len();
        let before = every_file(dir.path());
        let mut stage = Stage::default();
        let id = stage.put(HashKind::Sha256, "part", &body).unwrap();
        blobs.promote(stage).unwrap();
        let (file, _, _) = grown(&before, &every_file(dir.path()));
        let mut bytes = fs::read(&file).unwrap();
        bytes[100] ^= 1;
        fs::write(&file, bytes).unwrap();

        let frame = FRAME_BYTES as usize;
        let inside = 8 * frame + 5;
        assert_eq!(
            blobs.read(&id, inside as u64, 4096).unwrap().unwrap(),
            body[inside..inside + 4096]
        );
        let across = 5 * frame - head - 10;
        assert_eq!(
            blobs.read(&id, across as u64, 20).unwrap().unwrap(),
            body[across..across + 20]
        );
        let end = body.len() - 10;
        assert_eq!(
            blobs.read(&id, end as u64, 100).unwrap().unwrap(),
            body[end..]
        );
        let first = blobs.read(&id, 0, 200);
        assert!(
            matches!(first, Err(Error::Corrupt(..))),
            "a range in the flipped first frame reads as corrupt: {:?}",
            first.map(|bytes| bytes.map(|bytes| bytes.len()))
        );
        assert!(matches!(blobs.get(&id), Err(Error::Corrupt(..))));
    }

    #[test]
    fn legacy_files_move_into_the_store_a_commit_at_a_time_while_every_read_answers() {
        let dir = tempfile::tempdir().unwrap();
        let bodies: Vec<(&str, Vec<u8>)> = vec![
            ("page", text(1)),
            ("blob", text(2)),
            ("page", text(3)),
            ("part", noise(9 << 20, 4)),
            ("part", noise(9 << 20, 5)),
        ];
        let ids: Vec<BlobId> = bodies
            .iter()
            .map(|(kind, body)| {
                let framed = frame(kind, body).unwrap();
                let id = id_of(HashKind::Sha256, &framed).unwrap();
                fs::write(dir.path().join(name_of(&id)), &framed).unwrap();
                id
            })
            .collect();
        let legacy = |dir: &Path| {
            ids.iter()
                .filter(|id| dir.join(name_of(id)).is_file())
                .count()
        };
        let reads = |blobs: &Blobs| {
            for (id, (kind, body)) in ids.iter().zip(&bodies) {
                let blob = blobs.get(id).unwrap().unwrap();
                assert_eq!((blob.kind.as_str(), &blob.body), (*kind, body));
                let header = blobs.stat(id).unwrap().unwrap();
                assert_eq!(
                    (header.kind.as_str(), header.len),
                    (*kind, body.len() as u64)
                );
                assert_eq!(blobs.read(id, 1, 3).unwrap().unwrap(), body[1..4]);
            }
        };
        let blobs = Blobs::open(dir.path()).unwrap();
        reads(&blobs);

        let mut commits = 0;
        let mut left = legacy(dir.path());
        while left > 0 {
            blobs.promote(Stage::default()).unwrap();
            blobs.tidy().unwrap();
            commits += 1;
            let now = legacy(dir.path());
            assert!(
                now < left,
                "commit {commits} moved no legacy file: {now} left"
            );
            reads(&blobs);
            left = now;
        }
        assert_eq!(
            commits, 2,
            "18 MiB of legacy blobs move in two commits of at most {SEGMENT_BYTES} bytes"
        );
        drop(blobs);
        reads(&Blobs::open(dir.path()).unwrap());
    }

    const SMALL: Sizes = Sizes {
        segment: 4096,
        frame: 512,
    };

    fn segment_bytes(dir: &Path) -> u64 {
        every_file(&dir.join(SEGMENTS)).values().sum()
    }

    #[test]
    fn a_release_holds_across_reopens_while_segments_roll_and_compact() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::sized(dir.path(), SMALL).unwrap();
        let mut ids = Vec::new();
        for i in 0..40 {
            let mut stage = Stage::default();
            ids.push(stage.put(HashKind::Sha256, "part", &noise(300, i)).unwrap());
            blobs.promote(stage).unwrap();
        }
        let segments = every_file(&dir.path().join(SEGMENTS)).len();
        assert!(
            segments > 2,
            "40 records of 300 bytes roll 4 KiB segments: {segments}"
        );
        for (i, id) in ids.iter().enumerate() {
            if i % 4 != 0 {
                blobs.remove(id).unwrap();
            }
        }
        blobs.promote(Stage::default()).unwrap();
        let reads = |blobs: &Blobs, again: bool| {
            for (i, id) in ids.iter().enumerate() {
                let kept = i % 4 == 0 || (again && i == 1);
                let want = kept.then(|| noise(300, i as u64));
                assert_eq!(
                    blobs.get(id).unwrap().map(|blob| blob.body),
                    want,
                    "blob {i}"
                );
            }
        };
        reads(&blobs, false);
        drop(blobs);

        let blobs = Blobs::sized(dir.path(), SMALL).unwrap();
        reads(&blobs, false);
        let before = segment_bytes(dir.path());
        blobs.tidy().unwrap();
        let after = segment_bytes(dir.path());
        assert!(
            after < before,
            "compaction gave back bytes: {before} then {after}"
        );
        reads(&blobs, false);
        let mut stage = Stage::default();
        assert_eq!(
            stage.put(HashKind::Sha256, "part", &noise(300, 1)).unwrap(),
            ids[1]
        );
        blobs.promote(stage).unwrap();
        reads(&blobs, true);
        drop(blobs);

        let blobs = Blobs::sized(dir.path(), SMALL).unwrap();
        reads(&blobs, true);
        blobs.tidy().unwrap();
        drop(blobs);
        reads(&Blobs::sized(dir.path(), SMALL).unwrap(), true);
    }

    #[test]
    fn a_reader_of_a_big_blob_reads_it_frame_by_frame_after_the_blob_is_released() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::sized(dir.path(), SMALL).unwrap();
        let body = noise(10_000, 9);
        let framed = frame("part", &body).unwrap();
        let mut stage = Stage::default();
        let id = stage.put(HashKind::Sha256, "part", &body).unwrap();
        blobs.promote(stage).unwrap();
        assert!(dir.path().join(FRAMED).join(name_of(&id)).is_file());
        let mut reader = blobs.reader(&id, 700).unwrap().unwrap();
        assert_eq!(reader.framed_len(), framed.len() as u64);
        blobs.remove(&id).unwrap();
        assert!(!blobs.has(&id));
        assert!(blobs.reader(&id, 0).unwrap().is_none());

        let mut read = Vec::new();
        let mut pieces = 0;
        while let Some(piece) = reader.piece().unwrap() {
            read.extend(piece);
            pieces += 1;
        }
        assert_eq!(read, framed[700..]);
        assert_eq!(pieces, framed.len().div_ceil(512) - 1, "one piece a frame");
    }

    #[test]
    fn a_reader_of_a_small_blob_reads_from_its_offset() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::open(dir.path()).unwrap();
        let mut stage = Stage::default();
        let id = stage.put(HashKind::Sha1, "blob", &text(3)).unwrap();
        blobs.promote(stage).unwrap();
        let framed = frame("blob", &text(3)).unwrap();
        let mut reader = blobs.reader(&id, 10).unwrap().unwrap();
        assert_eq!(reader.framed_len(), framed.len() as u64);
        assert_eq!(reader.piece().unwrap().unwrap(), framed[10..]);
        assert_eq!(reader.piece().unwrap(), None);
        assert_eq!(blobs.size(&id).unwrap(), Some(framed.len() as u64));
    }

    #[test]
    fn a_legacy_file_over_a_segment_moves_into_frames_and_a_copy_a_crash_left_is_unlinked() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::sized(dir.path(), SMALL).unwrap();
        let mut stage = Stage::default();
        let twice = stage.put(HashKind::Sha256, "page", &text(6)).unwrap();
        blobs.promote(stage).unwrap();
        drop(blobs);
        let bodies = [noise(10_000, 7), text(5), text(6)];
        let ids: Vec<BlobId> = bodies
            .iter()
            .enumerate()
            .map(|(i, body)| {
                let framed = frame("page", body).unwrap();
                let id = id_of(HashKind::Sha256, &framed).unwrap();
                if i != 0 {
                    fs::write(dir.path().join(name_of(&id)), &framed).unwrap();
                }
                id
            })
            .collect();
        assert_eq!(ids[2], twice);
        let blobs = Blobs::sized(dir.path(), SMALL).unwrap();
        assert_eq!(blobs.tidy().unwrap().moved, 2);
        assert!(!blobs.log().worn().is_empty());
        drop(blobs);
        fs::write(
            dir.path().join(name_of(&ids[0])),
            frame("page", &bodies[0]).unwrap(),
        )
        .unwrap();
        let left = || {
            ids.iter()
                .filter(|id| dir.path().join(name_of(id)).is_file())
                .count()
        };
        let reads = |blobs: &Blobs| {
            for (id, body) in ids.iter().zip(&bodies) {
                assert_eq!(blobs.get(id).unwrap().unwrap().body, *body);
                assert_eq!(
                    blobs.read(id, 5000, 3).unwrap().unwrap(),
                    body.get(5000..5003).unwrap_or_default()
                );
            }
        };

        let blobs = Blobs::sized(dir.path(), SMALL).unwrap();
        let (extra, _) = put_each(&blobs, dir.path(), 60);
        for (i, id) in extra.iter().enumerate() {
            if !i.is_multiple_of(3) {
                blobs.remove(id).unwrap();
            }
        }
        reads(&blobs);
        assert_eq!(left(), 1);
        let first = blobs.tidy().unwrap();
        assert!(first.copied > 0, "compaction gets the first round");
        assert_eq!(first.moved, 0);
        let worn = blobs.log().worn();
        assert!(!worn.is_empty(), "more compaction is pending");
        reads(&blobs);

        let second = blobs.tidy().unwrap();
        assert_eq!(second.copied, 0, "the waiting file gets its own round");
        assert_eq!(second.retired, 0);
        assert_eq!(second.moved, 1, "the big file moves within two rounds");
        assert_eq!(left(), 0);
        assert_eq!(blobs.log().worn(), worn, "compaction waits for its turn");
        reads(&blobs);

        let third = blobs.tidy().unwrap();
        assert!(third.copied > 0, "compaction resumes after the file moves");
        assert_eq!(third.moved, 0);
        reads(&blobs);
        for (i, id) in extra.iter().enumerate() {
            let want = i.is_multiple_of(3).then(|| noise(300, i as u64));
            assert_eq!(blobs.get(id).unwrap().map(|blob| blob.body), want);
        }
        assert!(dir.path().join(FRAMED).join(name_of(&ids[0])).is_file());
        drop(blobs);
        reads(&Blobs::sized(dir.path(), SMALL).unwrap());
    }

    #[test]
    fn unpack_writes_every_blob_back_as_the_one_file_an_older_build_reads() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::sized(dir.path(), SMALL).unwrap();
        let bodies = [text(1), noise(3000, 2), noise(10_000, 3)];
        let mut stage = Stage::default();
        let ids: Vec<BlobId> = bodies
            .iter()
            .map(|body| stage.put(HashKind::Sha256, "part", body).unwrap())
            .collect();
        blobs.promote(stage).unwrap();
        assert!(dir.path().join(FRAMED).join(name_of(&ids[2])).is_file());

        let unpacked = blobs.unpack().unwrap();
        assert_eq!(
            unpacked,
            Unpacked {
                written: 3,
                unread: 0
            }
        );
        for (id, body) in ids.iter().zip(&bodies) {
            let file = fs::read(dir.path().join(name_of(id))).unwrap();
            assert_eq!(file, frame("part", body).unwrap());
        }
        assert!(!dir.path().join(SEGMENTS).exists());
        assert!(!dir.path().join(FRAMED).exists());
        let reopened = Blobs::sized(dir.path(), SMALL).unwrap();
        for (id, body) in ids.iter().zip(&bodies) {
            assert_eq!(reopened.get(id).unwrap().unwrap().body, *body);
        }
    }

    fn appended(before: &BTreeMap<PathBuf, u64>, after: &BTreeMap<PathBuf, u64>) -> u64 {
        after
            .iter()
            .map(|(path, len)| len.saturating_sub(before.get(path).copied().unwrap_or(0)))
            .sum()
    }

    #[test]
    fn a_tidy_copies_at_most_a_segment_of_bytes_and_the_next_ones_finish_the_job() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::sized(dir.path(), SMALL).unwrap();
        let mut ids = Vec::new();
        for i in 0..60 {
            let mut stage = Stage::default();
            ids.push(stage.put(HashKind::Sha256, "part", &noise(300, i)).unwrap());
            blobs.promote(stage).unwrap();
        }
        let kept = |i: usize| i.is_multiple_of(3);
        for (i, id) in ids.iter().enumerate() {
            if !kept(i) {
                blobs.remove(id).unwrap();
            }
        }
        blobs.promote(Stage::default()).unwrap();
        let before = every_file(&dir.path().join(SEGMENTS));
        blobs.tidy().unwrap();
        let after = every_file(&dir.path().join(SEGMENTS));
        let copied = appended(&before, &after);
        assert!(
            copied <= SMALL.segment,
            "one tidy copied {copied} bytes, past a segment of {}",
            SMALL.segment
        );
        let reads = |blobs: &Blobs| {
            for (i, id) in ids.iter().enumerate() {
                let want = kept(i).then(|| noise(300, i as u64));
                assert_eq!(
                    blobs.get(id).unwrap().map(|blob| blob.body),
                    want,
                    "blob {i}"
                );
            }
        };
        reads(&blobs);
        let held = segment_bytes(dir.path());
        for _ in 0..ids.len() {
            blobs.tidy().unwrap();
        }
        reads(&blobs);
        let left = segment_bytes(dir.path());
        assert!(
            left * 2 < held,
            "{left} bytes stay of {held} once the tidies end"
        );
        drop(blobs);
        reads(&Blobs::sized(dir.path(), SMALL).unwrap());
    }

    #[test]
    fn a_segment_with_bytes_that_do_not_read_is_kept_and_what_it_held_before_them_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::sized(dir.path(), SMALL).unwrap();
        let mut ids = Vec::new();
        let mut first = 0;
        for i in 0..30 {
            let mut stage = Stage::default();
            ids.push(stage.put(HashKind::Sha256, "part", &noise(300, i)).unwrap());
            blobs.promote(stage).unwrap();
            if i == 0 {
                first = segment_bytes(dir.path());
            }
        }
        drop(blobs);
        let oldest = dir.path().join(SEGMENTS).join(format!("{:020}", 1));
        let mut bytes = fs::read(&oldest).unwrap();
        bytes[first as usize + 6] ^= 1;
        fs::write(&oldest, bytes).unwrap();

        for _ in 0..2 {
            let blobs = Blobs::sized(dir.path(), SMALL).unwrap();
            for _ in 0..3 {
                blobs.tidy().unwrap();
            }
            assert!(
                oldest.is_file(),
                "the segment whose bytes do not read is kept"
            );
            assert_eq!(blobs.get(&ids[0]).unwrap().unwrap().body, noise(300, 0));
            assert!(
                !blobs.has(&ids[1]),
                "the record whose head flipped is not indexed"
            );
            let last = ids.len() - 1;
            assert_eq!(
                blobs.get(&ids[last]).unwrap().unwrap().body,
                noise(300, last as u64)
            );
        }
    }

    fn put_each(blobs: &Blobs, dir: &Path, count: u64) -> (Vec<BlobId>, Vec<u64>) {
        let mut ids = Vec::new();
        let mut ends = Vec::new();
        for i in 0..count {
            let mut stage = Stage::default();
            ids.push(stage.put(HashKind::Sha256, "part", &noise(300, i)).unwrap());
            blobs.promote(stage).unwrap();
            ends.push(segment_bytes(dir));
        }
        (ids, ends)
    }

    fn flip(path: &Path, at: u64) {
        let mut bytes = fs::read(path).unwrap();
        bytes[at as usize] ^= 1;
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn a_record_that_does_not_read_mid_newest_segment_keeps_every_whole_record_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::sized(dir.path(), SMALL).unwrap();
        let (ids, ends) = put_each(&blobs, dir.path(), 5);
        drop(blobs);
        let newest = dir.path().join(SEGMENTS).join(format!("{:020}", 1));
        let len = fs::metadata(&newest).unwrap().len();
        assert_eq!(len, ends[4], "five records fit the one segment");
        flip(&newest, ends[1] + 6);

        let blobs = Blobs::sized(dir.path(), SMALL).unwrap();
        assert_eq!(fs::metadata(&newest).unwrap().len(), len, "nothing is cut");
        for i in [0, 1, 3, 4] {
            let body = blobs.get(&ids[i]).unwrap().map(|blob| blob.body);
            assert_eq!(body, Some(noise(300, i as u64)), "blob {i}");
        }
        assert!(
            !blobs.has(&ids[2]),
            "the record whose head flipped is not indexed"
        );
        let mut stage = Stage::default();
        let later = stage.put(HashKind::Sha256, "page", &text(9)).unwrap();
        blobs.promote(stage).unwrap();
        blobs.tidy().unwrap();
        drop(blobs);

        let blobs = Blobs::sized(dir.path(), SMALL).unwrap();
        assert_eq!(blobs.get(&later).unwrap().unwrap().body, text(9));
        assert_eq!(blobs.get(&ids[4]).unwrap().unwrap().body, noise(300, 4));
        assert_eq!(fs::metadata(&newest).unwrap().len(), len);
    }

    #[test]
    fn a_record_whose_body_does_not_read_keeps_its_segment_through_every_tidy() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::sized(dir.path(), SMALL).unwrap();
        let (ids, ends) = put_each(&blobs, dir.path(), 30);
        for (i, id) in ids.iter().enumerate() {
            if i != 1 {
                blobs.remove(id).unwrap();
            }
        }
        blobs.promote(Stage::default()).unwrap();
        drop(blobs);
        let oldest = dir.path().join(SEGMENTS).join(format!("{:020}", 1));
        flip(&oldest, ends[1] - 1);

        let blobs = Blobs::sized(dir.path(), SMALL).unwrap();
        for _ in 0..3 {
            blobs.tidy().unwrap();
        }
        assert!(
            oldest.is_file(),
            "a segment holding a record that does not read is kept"
        );
        assert!(blobs.has(&ids[1]), "the record stays indexed");
        let read = blobs.get(&ids[1]);
        assert!(
            matches!(read, Err(Error::Corrupt(..))),
            "{:?}",
            read.map(|blob| blob.is_some())
        );
    }

    #[test]
    fn a_record_whose_body_does_not_read_is_skipped_whole_and_nothing_inside_it_is_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::open(dir.path()).unwrap();
        let mut stage = Stage::default();
        let other = stage.put(HashKind::Sha256, "page", &text(1)).unwrap();
        blobs.promote(stage).unwrap();
        let mut head = vec![1u8];
        head.extend(abi::encode(&other));
        head.extend(1u64.to_le_bytes());
        head.extend(8u64.to_le_bytes());
        let mut tomb = (head.len() as u32).to_le_bytes().to_vec();
        tomb.extend(&head);
        let crc = crc32fast::hash(&tomb);
        tomb.extend(crc.to_le_bytes());
        let mut body = noise(1000, 3);
        body.extend(&tomb);
        body.extend(noise(1000, 4));
        let before = segment_bytes(dir.path());
        let mut stage = Stage::default();
        let carrier = stage.put(HashKind::Sha256, "part", &body).unwrap();
        blobs.promote(stage).unwrap();
        let after = segment_bytes(dir.path());
        drop(blobs);
        let newest = dir.path().join(SEGMENTS).join(format!("{:020}", 1));
        let stored = fs::read(&newest).unwrap();
        let carried = stored
            .windows(tomb.len())
            .position(|bytes| bytes == tomb.as_slice());
        assert!(
            carried.is_some_and(|at| (before..after).contains(&(at as u64))),
            "the carrier keeps a whole tombstone for the other blob in its stored bytes"
        );
        flip(&newest, after - 1);

        for _ in 0..2 {
            let blobs = Blobs::open(dir.path()).unwrap();
            blobs.tidy().unwrap();
            assert_eq!(blobs.get(&other).unwrap().unwrap().body, text(1));
            let read = blobs.get(&carrier);
            assert!(
                !matches!(read, Ok(Some(_))),
                "the record whose body does not read never reads as bytes"
            );
        }
    }

    #[test]
    fn a_store_whose_dir_was_moved_aside_while_open_is_not_the_store_opened_at_its_path() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("blobs");
        let old = Blobs::open(&dir).unwrap();
        let mut stage = Stage::default();
        let id = stage.put(HashKind::Sha256, "page", &text(8)).unwrap();
        old.promote(stage).unwrap();
        fs::rename(&dir, root.path().join("aside")).unwrap();
        let fresh = Blobs::open(&dir).unwrap();
        assert!(!fresh.has(&id), "a store opened at the path starts empty");
        assert_eq!(old.get(&id).unwrap().unwrap().body, text(8));
        drop(old);
        let aside = Blobs::open(&root.path().join("aside")).unwrap();
        assert_eq!(aside.get(&id).unwrap().unwrap().body, text(8));
    }

    fn worn(dir: &Path) -> (Blobs, Vec<BlobId>) {
        let blobs = Blobs::sized(dir, SMALL).unwrap();
        let (ids, _) = put_each(&blobs, dir, 30);
        for (i, id) in ids.iter().enumerate() {
            if !i.is_multiple_of(3) {
                blobs.remove(id).unwrap();
            }
        }
        blobs.promote(Stage::default()).unwrap();
        (blobs, ids)
    }

    fn reads_kept(blobs: &Blobs, ids: &[BlobId], released: &[usize]) {
        for (i, id) in ids.iter().enumerate() {
            let kept = i.is_multiple_of(3) && !released.contains(&i);
            let want = kept.then(|| noise(300, i as u64));
            assert_eq!(
                blobs.get(id).unwrap().map(|blob| blob.body),
                want,
                "blob {i}"
            );
        }
    }

    fn held_mid_copy(blobs: &Blobs) -> (CopyGate, std::thread::JoinHandle<()>) {
        let gate = blobs.gate_copies();
        let tidier = blobs.clone();
        let round = std::thread::spawn(move || {
            tidier.tidy().unwrap();
        });
        gate.reached();
        (gate, round)
    }

    fn lock_is_free(blobs: &Blobs) -> bool {
        blobs.shared.log.try_lock().is_ok()
    }

    fn copy_dir(from: &Path, to: &Path) {
        for path in every_file(from).into_keys() {
            let into = to.join(path.strip_prefix(from).unwrap());
            fs::create_dir_all(into.parent().unwrap()).unwrap();
            fs::copy(&path, &into).unwrap();
        }
    }

    fn newest(dir: &Path, magic: &[u8; 8]) -> Option<PathBuf> {
        every_file(&dir.join(SEGMENTS))
            .into_keys()
            .filter(|path| fs::read(path).unwrap().starts_with(magic))
            .max()
    }

    #[test]
    fn a_tidy_round_held_mid_copy_holds_no_lock_a_block_needs() {
        let dir = tempfile::tempdir().unwrap();
        let (blobs, ids) = worn(dir.path());
        let (gate, round) = held_mid_copy(&blobs);
        assert!(
            lock_is_free(&blobs),
            "a tidy round holds the store's lock while it copies"
        );
        let mut stage = Stage::default();
        let fresh = stage.put(HashKind::Sha256, "page", &text(5)).unwrap();
        blobs.promote(stage).unwrap();
        blobs.remove(&ids[3]).unwrap();
        blobs.promote(Stage::default()).unwrap();
        assert_eq!(blobs.get(&fresh).unwrap().unwrap().body, text(5));
        reads_kept(&blobs, &ids, &[3]);
        drop(gate);
        round.join().unwrap();

        reads_kept(&blobs, &ids, &[3]);
        assert_eq!(blobs.get(&fresh).unwrap().unwrap().body, text(5));
        drop(blobs);
        let blobs = Blobs::sized(dir.path(), SMALL).unwrap();
        reads_kept(&blobs, &ids, &[3]);
        assert_eq!(blobs.get(&fresh).unwrap().unwrap().body, text(5));
    }

    #[test]
    fn a_blob_released_between_its_copy_and_the_land_stays_gone_after_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let (blobs, ids) = worn(dir.path());
        let (gate, round) = held_mid_copy(&blobs);
        assert!(
            lock_is_free(&blobs),
            "a tidy round holds the store's lock while it copies"
        );
        blobs.remove(&ids[0]).unwrap();
        blobs.promote(Stage::default()).unwrap();
        let crashed = tempfile::tempdir().unwrap();
        copy_dir(dir.path(), crashed.path());
        drop(gate);
        round.join().unwrap();
        reads_kept(&blobs, &ids, &[0]);

        for _ in 0..2 {
            let reopened = Blobs::sized(crashed.path(), SMALL).unwrap();
            assert!(
                !reopened.has(&ids[0]),
                "a blob released while its copy waited to land is back after a crash"
            );
            reads_kept(&reopened, &ids, &[0]);
            for _ in 0..ids.len() {
                reopened.tidy().unwrap();
            }
        }
    }

    #[test]
    fn each_stream_cut_in_its_last_write_reopens_with_every_whole_record_before_the_cut() {
        let dir = tempfile::tempdir().unwrap();
        let (blobs, ids) = worn(dir.path());
        let (gate, round) = held_mid_copy(&blobs);
        assert!(
            newest(dir.path(), b"dtblobc1").is_some(),
            "a tidy round writes its copies to segments of their own"
        );
        let mut stage = Stage::default();
        let cut = stage.put(HashKind::Sha256, "page", &text(6)).unwrap();
        blobs.promote(stage).unwrap();
        let crashed = tempfile::tempdir().unwrap();
        copy_dir(dir.path(), crashed.path());
        drop(gate);
        round.join().unwrap();

        let mut cuts = Vec::new();
        for magic in [b"dtblobs1", b"dtblobc1"] {
            let path = newest(crashed.path(), magic).unwrap();
            let &(at, size) = segments::heads(&path).last().unwrap();
            fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(at + size / 2)
                .unwrap();
            cuts.push((path, at));
        }
        let reopened = Blobs::sized(crashed.path(), SMALL).unwrap();
        for (path, at) in &cuts {
            assert_eq!(
                fs::metadata(path).unwrap().len(),
                *at,
                "{} is cut back to its last whole record",
                path.display()
            );
        }
        assert!(
            !reopened.has(&cut),
            "the blob whose write was cut is absent"
        );
        reads_kept(&reopened, &ids, &[]);
        let mut stage = Stage::default();
        let after = stage.put(HashKind::Sha256, "page", &text(7)).unwrap();
        reopened.promote(stage).unwrap();
        for _ in 0..ids.len() {
            reopened.tidy().unwrap();
        }
        drop(reopened);
        let reopened = Blobs::sized(crashed.path(), SMALL).unwrap();
        reads_kept(&reopened, &ids, &[]);
        assert_eq!(reopened.get(&after).unwrap().unwrap().body, text(7));
        assert!(!reopened.has(&cut));
    }

    #[test]
    fn a_tidy_round_says_what_it_copied_moved_and_retired() {
        let dir = tempfile::tempdir().unwrap();
        let (blobs, ids) = worn(dir.path());
        let bodies = [text(11), text(12), text(13)];
        for body in &bodies {
            let framed = frame("page", body).unwrap();
            let id = id_of(HashKind::Sha256, &framed).unwrap();
            fs::write(dir.path().join(name_of(&id)), &framed).unwrap();
        }
        let before = every_file(&dir.path().join(SEGMENTS));
        let mut said = Tidied::default();
        for _ in 0..ids.len() + bodies.len() {
            let round = blobs.tidy().unwrap();
            said.copied += round.copied;
            said.moved += round.moved;
            said.retired += round.retired;
        }
        let after = every_file(&dir.path().join(SEGMENTS));
        let retired = before
            .keys()
            .filter(|path| !after.contains_key(*path))
            .count() as u64;
        let copied: u64 = after
            .iter()
            .filter(|(path, _)| fs::read(path).unwrap().starts_with(b"dtblobc1"))
            .map(|(path, len)| len - before.get(path).copied().unwrap_or(8))
            .sum();
        assert!(
            retired > 0 && copied > 0,
            "the rounds retired {retired} and copied {copied}"
        );
        assert_eq!(
            said,
            Tidied {
                copied,
                moved: bodies.len() as u64,
                retired
            }
        );
        reads_kept(&blobs, &ids, &[]);
    }

    #[test]
    fn a_store_opened_twice_in_a_process_is_one_store_and_holds_its_lock_until_the_last_drop() {
        let dir = tempfile::tempdir().unwrap();
        let first = Blobs::open(dir.path()).unwrap();
        let second = Blobs::open(dir.path()).unwrap();
        let mut stage = Stage::default();
        let id = stage.put(HashKind::Sha256, "page", &text(2)).unwrap();
        first.promote(stage).unwrap();
        assert_eq!(second.get(&id).unwrap().unwrap().body, text(2));

        let held = fs::OpenOptions::new()
            .write(true)
            .open(dir.path().join(HELD))
            .unwrap();
        assert!(matches!(held.try_lock(), Err(fs::TryLockError::WouldBlock)));
        drop(first);
        assert!(matches!(held.try_lock(), Err(fs::TryLockError::WouldBlock)));
        drop(second);
        held.try_lock().unwrap();
    }

    fn inflated_so_far() -> u64 {
        INFLATED.with(std::cell::Cell::get)
    }

    fn planned_so_far() -> u64 {
        PLANNED.with(std::cell::Cell::get)
    }

    fn segment_file(dir: &Path, n: u64) -> PathBuf {
        dir.join(SEGMENTS).join(format!("{n:020}"))
    }

    #[test]
    fn a_tidy_moves_a_needed_tomb_and_frees_it_once_the_put_it_names_is_retired() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::sized(dir.path(), SMALL).unwrap();
        let put = |seed: u64| {
            let mut stage = Stage::default();
            let id = stage
                .put(HashKind::Sha256, "part", &noise(300, seed))
                .unwrap();
            blobs.promote(stage).unwrap();
            id
        };
        let agrees = |blobs: &Blobs, when: &str| {
            let indexed = blobs.log().indexed();
            assert!(
                indexed.disagreements.is_empty(),
                "{when}: {:?}",
                indexed.disagreements
            );
        };
        let removed = put(1000);
        let first = blobs.log().segment_of(&removed).unwrap();
        let mut keepers = Vec::new();
        loop {
            let id = put(1001 + keepers.len() as u64);
            let rolled = blobs.log().segment_of(&id) != Some(first);
            keepers.push(id);
            if rolled {
                break;
            }
        }
        blobs.remove(&removed).unwrap();
        blobs.promote(Stage::default()).unwrap();
        let (tombed, targets) = blobs.log().tomb_of(&removed).unwrap();
        assert_ne!(tombed, first);
        assert_eq!(
            targets,
            vec![first],
            "the tomb names the segment its put still lies in"
        );
        let mut fillers = Vec::new();
        loop {
            let id = put(2000 + fillers.len() as u64);
            let rolled = blobs.log().segment_of(&id) != Some(tombed);
            fillers.push(id);
            if rolled {
                break;
            }
        }
        let in_the_tombs_segment = keepers.pop().unwrap();
        for id in fillers.iter().chain([&in_the_tombs_segment]) {
            blobs.remove(id).unwrap();
        }
        blobs.promote(Stage::default()).unwrap();
        agrees(&blobs, "worn");
        assert!(blobs.log().worn().contains(&tombed));
        assert!(!blobs.log().worn().contains(&first));
        let short = blobs.log().plan(tombed, 1, SMALL.segment).unwrap();
        assert!(
            !short.drains,
            "a plan that leaves a needed tomb behind does not drain its segment"
        );

        blobs.tidy().unwrap();
        agrees(&blobs, "after the round that moves the tomb");
        let (moved, targets) = blobs.log().tomb_of(&removed).unwrap();
        assert_ne!(moved, tombed, "the round moved the needed tomb");
        assert_eq!(targets, vec![first]);
        assert!(
            !segment_file(dir.path(), tombed).exists(),
            "the segment the tomb left is retired"
        );
        assert_eq!(blobs.get(&removed).unwrap(), None);

        for id in &keepers {
            blobs.remove(id).unwrap();
        }
        blobs.promote(Stage::default()).unwrap();
        for round in 0..fillers.len() + keepers.len() {
            blobs.tidy().unwrap();
            agrees(&blobs, &format!("round {round}"));
        }
        assert!(
            !segment_file(dir.path(), first).exists(),
            "the segment the removed put lay in is retired"
        );
        let freed = blobs
            .log()
            .tomb_of(&removed)
            .is_none_or(|(_, targets)| targets.is_empty());
        assert!(freed, "the tomb names no segment once its put is retired");
        assert_eq!(blobs.get(&removed).unwrap(), None);
        drop(blobs);

        let blobs = Blobs::sized(dir.path(), SMALL).unwrap();
        agrees(&blobs, "reopened");
        assert_eq!(blobs.get(&removed).unwrap(), None);
        for id in &keepers {
            assert_eq!(blobs.get(id).unwrap(), None);
        }
    }

    #[test]
    fn a_range_of_a_segment_record_inflates_only_the_frames_it_covers() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::open(dir.path()).unwrap();
        let body = noise(5 * FRAME_BYTES + 77, 6);
        let head = header("part", body.len() as u64).len();
        let mut stage = Stage::default();
        let id = stage.put(HashKind::Sha256, "part", &body).unwrap();
        blobs.promote(stage).unwrap();
        assert!(
            !blobs.framed_path(&id).is_file(),
            "a blob under a segment is a segment record"
        );
        let frame = FRAME_BYTES as usize;
        let ranges = [
            (3 * frame + 5, 6, 1),
            (2 * frame - head - 3, 6, 2),
            (body.len() - 10, 100, 1),
        ];
        for (at, want, frames) in ranges {
            let before = inflated_so_far();
            let read = blobs.read(&id, at as u64, want as u64).unwrap().unwrap();
            let spent = inflated_so_far() - before;
            assert_eq!(read, body[at..(at + want).min(body.len())]);
            assert!(
                spent <= frames * FRAME_BYTES,
                "a read of {want} bytes at {at} inflated {spent} bytes, past {frames} frames"
            );
        }
    }

    #[test]
    fn a_flipped_byte_in_one_frame_of_a_record_fails_only_the_ranges_that_cover_it() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::open(dir.path()).unwrap();
        let body = noise(3 * FRAME_BYTES, 8);
        let before = every_file(dir.path());
        let mut stage = Stage::default();
        let id = stage.put(HashKind::Sha256, "part", &body).unwrap();
        blobs.promote(stage).unwrap();
        let (file, _, now) = grown(&before, &every_file(dir.path()));
        flip(&file, now - 1);

        assert_eq!(blobs.read(&id, 5, 100).unwrap().unwrap(), body[5..105]);
        let end = body.len() as u64 - 100;
        assert!(matches!(blobs.read(&id, end, 100), Err(Error::Corrupt(..))));
        assert!(matches!(blobs.get(&id), Err(Error::Corrupt(..))));
    }

    const OLDER: &str = "fixtures/segment-whole-records";

    fn older_bodies() -> [(&'static str, Vec<u8>); 2] {
        let ramp: Vec<u8> = (0..3 * FRAME_BYTES + 5).map(|i| (i % 251) as u8).collect();
        [("page", text(1)), ("part", ramp)]
    }

    fn older_ids() -> Vec<BlobId> {
        older_bodies()
            .iter()
            .map(|(kind, body)| id_of(HashKind::Sha256, &frame(kind, body).unwrap()).unwrap())
            .collect()
    }

    fn older_store() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let segments = dir.path().join(SEGMENTS);
        fs::create_dir_all(&segments).unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join(OLDER);
        fs::copy(&fixture, segments.join(format!("{:020}", 1))).unwrap();
        dir
    }

    #[test]
    fn records_older_builds_wrote_read_back_whole_and_by_range() {
        let dir = older_store();
        let blobs = Blobs::open(dir.path()).unwrap();
        let bodies = older_bodies();
        for ((kind, body), id) in bodies.iter().zip(older_ids()) {
            let blob = blobs.get(&id).unwrap().unwrap();
            assert_eq!((blob.kind.as_str(), &blob.body), (*kind, body));
            let at = body.len() / 2;
            assert_eq!(
                blobs.read(&id, at as u64, 9).unwrap().unwrap(),
                body[at..at + 9]
            );
        }
    }

    #[test]
    fn a_tidy_rewrites_records_older_builds_wrote_so_a_range_inflates_one_frame() {
        let dir = older_store();
        let blobs = Blobs::open(dir.path()).unwrap();
        let bodies = older_bodies();
        let ids = older_ids();
        let big = &bodies[1].1;
        let at = 2 * FRAME_BYTES as usize + 7;
        let range = |blobs: &Blobs| {
            let before = inflated_so_far();
            let read = blobs.read(&ids[1], at as u64, 6).unwrap().unwrap();
            assert_eq!(read, big[at..at + 6]);
            inflated_so_far() - before
        };
        let whole = framed_len("part", big.len() as u64);
        assert_eq!(
            range(&blobs),
            whole,
            "a record an older build wrote inflates whole until a tidy rewrites it"
        );
        blobs.tidy().unwrap();
        let older = dir.path().join(SEGMENTS).join(format!("{:020}", 1));
        assert!(
            !older.exists(),
            "a tidy rewrites every record of the older segment and retires it"
        );
        let reads = |blobs: &Blobs| {
            for ((kind, body), id) in bodies.iter().zip(&ids) {
                let blob = blobs.get(id).unwrap().unwrap();
                assert_eq!((blob.kind.as_str(), &blob.body), (*kind, body));
            }
            let spent = range(blobs);
            assert!(
                spent <= FRAME_BYTES,
                "a range of a rewritten record inflated {spent} bytes"
            );
        };
        reads(&blobs);
        drop(blobs);
        reads(&Blobs::open(dir.path()).unwrap());
    }

    fn leb(bytes: &[u8]) -> Option<(u64, u64)> {
        let mut value = 0u64;
        for (index, byte) in bytes.iter().take(5).enumerate() {
            value |= u64::from(byte & 0x7f) << (7 * index);
            if byte & 0x80 == 0 {
                return Some((value, index as u64 + 1));
            }
        }
        None
    }

    fn counted(blobs: &Blobs, code: &BlobId, reads: &mut u64, offset: u64, len: u64) -> Vec<u8> {
        *reads += 1;
        blobs.read(code, offset, len).unwrap().unwrap()
    }

    fn source_of(blobs: &Blobs, code: &BlobId, reads: &mut u64) -> bool {
        let name = b"ducktape.source";
        if counted(blobs, code, reads, 0, 8) != b"\0asm\x01\0\0\0" {
            return false;
        }
        let mut offset = 8u64;
        loop {
            let head = counted(blobs, code, reads, offset, 6);
            let Some((&section, rest)) = head.split_first() else {
                return false;
            };
            let Some((size, used)) = leb(rest) else {
                return false;
            };
            let body = offset + 1 + used;
            let end = body + size;
            let custom = section == 0;
            if custom {
                let named = counted(blobs, code, reads, body, 5);
                let Some((length, used)) = leb(&named) else {
                    return false;
                };
                let at = body + used;
                let sized = length == name.len() as u64;
                let ours = sized && counted(blobs, code, reads, at, length) == name;
                if ours {
                    counted(blobs, code, reads, at + length, end - at - length);
                    return true;
                }
            }
            offset = end;
        }
    }

    #[test]
    fn a_round_plans_no_segment_past_one_its_budget_leaves_records_in() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::sized(dir.path(), SMALL).unwrap();
        let mut ids = Vec::new();
        for i in 0..120 {
            let mut stage = Stage::default();
            ids.push(stage.put(HashKind::Sha256, "part", &noise(300, i)).unwrap());
            blobs.promote(stage).unwrap();
        }
        for (i, id) in ids.iter().enumerate() {
            if !i.is_multiple_of(5) {
                blobs.remove(id).unwrap();
            }
        }
        blobs.promote(Stage::default()).unwrap();
        let worn = blobs.log().worn();
        assert!(worn.len() > 2, "removals wore several segments: {worn:?}");
        let first = blobs.log().plan(worn[0], 1, SMALL.segment).unwrap();
        assert!(
            !first.drains,
            "a budget of one byte leaves the segment's records"
        );
        assert_eq!(first.bytes, 0);
        let whole = blobs
            .log()
            .plan(worn[0], SMALL.segment, SMALL.segment)
            .unwrap();
        assert!(whole.drains, "a whole budget drains one worn segment");

        let worn = blobs.log().worn().len() as u64;
        let before = every_file(&dir.path().join(SEGMENTS));
        let planned_before = planned_so_far();
        let tidied = blobs.tidy().unwrap();
        let planned = planned_so_far() - planned_before;
        let after = every_file(&dir.path().join(SEGMENTS));
        let retired = before
            .keys()
            .filter(|path| !after.contains_key(*path))
            .count() as u64;
        assert!(retired > 0, "a round retires the worn segments it drains");
        assert_eq!(tidied.retired, retired);
        assert_eq!(
            planned,
            retired + 1,
            "a round plans the segments it drains and the one its budget stops in, and no more"
        );
        assert!(
            planned < worn,
            "worn segments past the stop wait for the next round: {planned} of {worn}"
        );
        for _ in 0..ids.len() {
            blobs.tidy().unwrap();
        }
        for (i, id) in ids.iter().enumerate() {
            let want = i.is_multiple_of(5).then(|| noise(300, i as u64));
            assert_eq!(
                blobs.get(id).unwrap().map(|blob| blob.body),
                want,
                "blob {i}"
            );
        }
        drop(blobs);
        let blobs = Blobs::sized(dir.path(), SMALL).unwrap();
        for (i, id) in ids.iter().enumerate() {
            let want = i.is_multiple_of(5).then(|| noise(300, i as u64));
            assert_eq!(
                blobs.get(id).unwrap().map(|blob| blob.body),
                want,
                "blob {i}"
            );
        }
    }

    #[test]
    fn each_segment_lists_exactly_the_records_and_tombs_the_log_holds_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let (blobs, ids) = worn(dir.path());
        let agrees = |blobs: &Blobs, when: &str| {
            let indexed = blobs.log().indexed();
            assert!(
                indexed.disagreements.is_empty(),
                "{when}: {:?}",
                indexed.disagreements
            );
            indexed
        };
        let first = agrees(&blobs, "worn");
        assert!(first.tombs > 0, "removals logged tombs");
        assert!(
            first.targets > 0,
            "a tomb names a segment its put still lies in"
        );
        for round in 0..ids.len() {
            blobs.tidy().unwrap();
            agrees(&blobs, &format!("after round {round}"));
        }
        let mut stage = Stage::default();
        stage.put(HashKind::Sha256, "page", &text(7)).unwrap();
        blobs.promote(stage).unwrap();
        blobs.remove(&ids[3]).unwrap();
        blobs.promote(Stage::default()).unwrap();
        agrees(&blobs, "after a put and a removal");
        drop(blobs);
        let blobs = Blobs::sized(dir.path(), SMALL).unwrap();
        agrees(&blobs, "reopened");
        for round in 0..ids.len() {
            blobs.tidy().unwrap();
            agrees(&blobs, &format!("after reopened round {round}"));
        }
    }

    const BENCH: Sizes = Sizes {
        segment: 1 << 20,
        frame: 64 << 10,
    };

    fn bench_dir() -> Option<PathBuf> {
        std::env::var_os("BLOB_BENCH_STORE").map(PathBuf::from)
    }

    fn bench_env(name: &str) -> Option<usize> {
        std::env::var(name).ok().map(|value| value.parse().unwrap())
    }

    #[cfg(target_os = "linux")]
    fn bench_place(cpu: Option<usize>, nice: i32) {
        if let Some(cpu) = cpu {
            let mut set = rustix::thread::CpuSet::new();
            set.set(cpu);
            rustix::thread::sched_setaffinity(None, &set).unwrap();
        }
        let thread = rustix::thread::gettid();
        rustix::process::setpriority_process(Some(thread), nice).unwrap();
    }

    #[cfg(not(target_os = "linux"))]
    fn bench_place(_: Option<usize>, _: i32) {}

    fn resident() -> u64 {
        let status = fs::read_to_string("/proc/self/status").unwrap();
        let line = status
            .lines()
            .find(|line| line.starts_with("VmRSS:"))
            .unwrap();
        let kib: u64 = line.split_whitespace().nth(1).unwrap().parse().unwrap();
        kib << 10
    }

    #[test]
    #[ignore]
    fn bench_build_a_store_of_worn_segments() {
        let Some(dir) = bench_dir() else {
            return;
        };
        let held: u64 = std::env::var("BLOB_BENCH_HELD").unwrap().parse().unwrap();
        let blobs = Blobs::sized(&dir, BENCH).unwrap();
        let mut stage = Stage::default();
        for i in 0..3 * held {
            stage.put(HashKind::Sha256, "part", &noise(100, i)).unwrap();
        }
        let written: Vec<BlobId> = stage.ids().copied().collect();
        blobs.promote(stage).unwrap();
        for (i, id) in written.iter().enumerate() {
            if !i.is_multiple_of(3) {
                blobs.remove(id).unwrap();
            }
        }
        blobs.promote(Stage::default()).unwrap();
        let worn = blobs.log().worn().len();
        println!("bench: built {held} held blobs over {worn} worn segments");
    }

    #[test]
    #[ignore]
    fn bench_one_tidy_round_beside_commits() {
        let Some(dir) = bench_dir() else {
            return;
        };
        let before = resident();
        let blobs = Blobs::sized(&dir, BENCH).unwrap();
        let grew = resident().saturating_sub(before);
        let held = blobs.log().ids().len() as u64;
        let worn = blobs.log().worn().len();
        let tidy_cpu = bench_env("BLOB_BENCH_TIDY_CPU");
        let commit_cpu = bench_env("BLOB_BENCH_COMMIT_CPU");
        let nice = bench_env("BLOB_BENCH_TIDY_NICE").unwrap_or(0) as i32;
        let spinners = bench_env("BLOB_BENCH_SPINNERS").unwrap_or(0);
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let spinning: Vec<_> = (0..spinners)
            .map(|_| {
                let done = done.clone();
                std::thread::spawn(move || {
                    bench_place(tidy_cpu, 0);
                    while !done.load(Ordering::Relaxed) {
                        std::hint::spin_loop();
                    }
                })
            })
            .collect();
        let committing = std::thread::spawn({
            let (blobs, done) = (blobs.clone(), done.clone());
            move || {
                bench_place(commit_cpu, 0);
                let mut longest = std::time::Duration::ZERO;
                let mut commits = 0u64;
                while !done.load(Ordering::Relaxed) {
                    let mut stage = Stage::default();
                    stage.put(HashKind::Sha256, "page", &text(commits)).unwrap();
                    let began = std::time::Instant::now();
                    blobs.promote(stage).unwrap();
                    longest = longest.max(began.elapsed());
                    commits += 1;
                }
                (commits, longest)
            }
        });
        let tidying = std::thread::spawn({
            let blobs = blobs.clone();
            move || {
                bench_place(tidy_cpu, nice);
                let began = std::time::Instant::now();
                let tidied = blobs.tidy().unwrap();
                (tidied, began.elapsed())
            }
        });
        let (tidied, round) = tidying.join().unwrap();
        done.store(true, Ordering::Relaxed);
        let (commits, longest) = committing.join().unwrap();
        for spinner in spinning {
            spinner.join().unwrap();
        }
        println!(
            "bench: tidy on cpu {tidy_cpu:?} at nice {nice} beside {spinners} nice-0 spinners, \
             commits on cpu {commit_cpu:?}"
        );
        println!(
            "bench: {held} held, {worn} worn segments, opening grew resident memory {grew} bytes \
             ({} a held blob), one round {round:?} copied {} bytes and retired {}, {commits} \
             commits beside it, the longest {longest:?}",
            grew / held.max(1),
            tidied.copied,
            tidied.retired
        );
    }

    #[test]
    #[ignore]
    fn bench_a_source_section_read_from_each_code() {
        let Some(codes) = std::env::var_os("BLOB_BENCH_CODES") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::open(dir.path()).unwrap();
        let mut stage = Stage::default();
        let mut ids = Vec::new();
        for entry in fs::read_dir(codes).unwrap() {
            let framed = fs::read(entry.unwrap().path()).unwrap();
            let (header, body) = parse(&framed).unwrap();
            ids.push(stage.put(HashKind::Sha256, &header.kind, body).unwrap());
        }
        blobs.promote(stage).unwrap();
        let mut reads = 0;
        let mut found = 0;
        let before = inflated_so_far();
        let began = std::time::Instant::now();
        for id in &ids {
            found += u64::from(source_of(&blobs, id, &mut reads));
        }
        let took = began.elapsed();
        println!(
            "bench: {} codes, {found} sources, {reads} ranged reads, {} bytes inflated, {took:?}",
            ids.len(),
            inflated_so_far() - before
        );
    }
}
