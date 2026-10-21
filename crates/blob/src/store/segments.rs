// The log small blobs live in: numbered append-only segment files in two streams, the records blocks
// put and tombstones naming the put a removal killed, and the copies a tidy round writes beside them,
// with the index the records rebuild at open and the bookkeeping that says when to compact a segment.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{self, BufReader, Read, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use abi::{BlobHeader, BlobId};
use borsh::{BorshDeserialize, BorshSerialize};

use super::{Error, Result, framed_len, gone, not_kept, read_at, synced_dir, verified, write_at};
use crate::parse;

const APPLIED: [u8; 8] = *b"dtblobs1";
const COPIED: [u8; 8] = *b"dtblobc1";
const MARKED: u64 = APPLIED.len() as u64;
const LEGACY: Origin = Origin { segment: 0, at: 0 };

#[derive(BorshSerialize, BorshDeserialize)]
enum Head {
    Blob {
        id: BlobId,
        kind: String,
        len: u64,
        encoded: u64,
        crc: u32,
    },
    Tomb {
        id: BlobId,
        segment: u64,
        at: u64,
    },
    Copy {
        id: BlobId,
        kind: String,
        len: u64,
        encoded: u64,
        crc: u32,
        origin_segment: u64,
        origin_at: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) struct Origin {
    segment: u64,
    at: u64,
}

type Put = (BlobId, Origin);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stream {
    Applied,
    Copied,
}

pub(super) struct Record {
    id: BlobId,
    kind: String,
    len: u64,
    head: u32,
    encoded: u64,
    crc: u32,
    bytes: Vec<u8>,
}

pub(super) fn record(id: BlobId, framed: &[u8]) -> Result<Record> {
    encoded(id, framed, None)
}

pub(super) fn legacy_record(id: BlobId, framed: &[u8]) -> Result<Record> {
    encoded(id, framed, Some(LEGACY))
}

fn encoded(id: BlobId, framed: &[u8], origin: Option<Origin>) -> Result<Record> {
    let (header, _) =
        parse(framed).ok_or_else(|| Error::Corrupt(id, "the frame does not parse".into()))?;
    let body = zstd::bulk::compress(framed, zstd::DEFAULT_COMPRESSION_LEVEL)?;
    let crc = crc32fast::hash(&body);
    let kind = header.kind.clone();
    let encoded = body.len() as u64;
    let head = match origin {
        None => Head::Blob {
            id,
            kind,
            len: header.len,
            encoded,
            crc,
        },
        Some(origin) => Head::Copy {
            id,
            kind,
            len: header.len,
            encoded,
            crc,
            origin_segment: origin.segment,
            origin_at: origin.at,
        },
    };
    let head = borsh::to_vec(&head)?;
    Ok(Record {
        id,
        kind: header.kind,
        len: header.len,
        head: head.len() as u32,
        encoded,
        crc,
        bytes: laid(&head, &body),
    })
}

fn laid(head: &[u8], body: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8 + head.len() + body.len());
    bytes.extend_from_slice(&(head.len() as u32).to_le_bytes());
    bytes.extend_from_slice(head);
    let crc = crc32fast::hash(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
    bytes.extend_from_slice(body);
    bytes
}

pub(super) fn framed(
    file: &fs::File,
    id: &BlobId,
    logged: &Logged,
    kind: &str,
    once: bool,
) -> Result<Vec<u8>> {
    let mut body = vec![0u8; logged.encoded as usize];
    read_at(file, &mut body, logged.body())?;
    if once {
        not_kept(file, logged.body(), logged.encoded)?;
    }
    let intact = crc32fast::hash(&body) == logged.crc;
    if !intact {
        return Err(Error::Corrupt(
            *id,
            "its record's checksum disagrees with the bytes on disk".into(),
        ));
    }
    let framed = zstd::bulk::decompress(&body, framed_len(kind, logged.len) as usize)
        .map_err(|error| Error::Corrupt(*id, format!("its record does not decode: {error}")))?;
    verified(id, framed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Logged {
    segment: u64,
    at: u64,
    head: u32,
    encoded: u64,
    crc: u32,
    kind: u32,
    len: u64,
    origin: Origin,
}

impl Logged {
    fn size(&self) -> u64 {
        8 + u64::from(self.head) + self.encoded
    }

    fn body(&self) -> u64 {
        self.at + 8 + u64::from(self.head)
    }

    fn grows(&self) -> u64 {
        let blob = self.origin
            == Origin {
                segment: self.segment,
                at: self.at,
            };
        match blob {
            true => 2 * size_of::<u64>() as u64,
            false => 0,
        }
    }
}

#[derive(Clone, Copy)]
enum Held {
    Logged(Logged),
    Framed { kind: u32, len: u64 },
}

pub(super) enum Hit {
    Logged {
        file: Arc<fs::File>,
        logged: Logged,
        kind: String,
    },
    Framed {
        kind: String,
        len: u64,
    },
}

impl Hit {
    pub(super) fn header(&self) -> BlobHeader {
        match self {
            Hit::Logged { logged, kind, .. } => BlobHeader {
                kind: kind.clone(),
                len: logged.len,
            },
            Hit::Framed { kind, len } => BlobHeader {
                kind: kind.clone(),
                len: *len,
            },
        }
    }
}

struct Segment {
    file: Arc<fs::File>,
    stream: Stream,
    len: u64,
    live: u64,
    dead: u64,
    records: u64,
    tombs: u64,
    damaged: Option<u64>,
}

impl Segment {
    fn opened(file: Arc<fs::File>, len: u64, stream: Stream) -> Segment {
        Segment {
            file,
            stream,
            len,
            live: 0,
            dead: 0,
            records: 0,
            tombs: 0,
            damaged: None,
        }
    }
}

struct Kept {
    segment: u64,
    at: u64,
    size: u64,
    targets: Vec<u64>,
}

pub(super) struct Plan {
    segment: u64,
    file: Arc<fs::File>,
    records: Vec<(BlobId, Logged, String)>,
    tombs: Vec<(Put, u64, u64)>,
    pub(super) bytes: u64,
}

#[derive(Default)]
pub(super) struct Round {
    records: Vec<(BlobId, Logged, Logged)>,
    tombs: Vec<(Put, u64, u64, u64, u64)>,
    spoiled: Vec<Logged>,
    moved: Vec<(BlobId, String, Logged, PathBuf)>,
    sources: Vec<u64>,
}

impl Plan {
    pub(super) fn copy(self, copier: &mut Copier, round: &mut Round) -> Result<()> {
        round.sources.push(self.segment);
        for (id, logged, kind) in self.records {
            let mut body = vec![0u8; logged.encoded as usize];
            read_at(&self.file, &mut body, logged.body())?;
            let intact = crc32fast::hash(&body) == logged.crc;
            if !intact {
                round.spoiled.push(logged);
                continue;
            }
            let head = borsh::to_vec(&Head::Copy {
                id,
                kind,
                len: logged.len,
                encoded: logged.encoded,
                crc: logged.crc,
                origin_segment: logged.origin.segment,
                origin_at: logged.origin.at,
            })?;
            let (segment, at) = copier.append(&laid(&head, &body))?;
            let copy = Logged {
                segment,
                at,
                head: head.len() as u32,
                ..logged
            };
            round.records.push((id, logged, copy));
        }
        for (put, from, size) in self.tombs {
            let mut bytes = vec![0u8; size as usize];
            read_at(&self.file, &mut bytes, from)?;
            let (segment, at) = copier.append(&bytes)?;
            round.tombs.push((put, self.segment, from, segment, at));
        }
        Ok(())
    }
}

impl Round {
    pub(super) fn moved(
        &mut self,
        copier: &mut Copier,
        record: Record,
        path: PathBuf,
    ) -> Result<()> {
        let (segment, at) = copier.append(&record.bytes)?;
        let logged = Logged {
            segment,
            at,
            head: record.head,
            encoded: record.encoded,
            crc: record.crc,
            kind: 0,
            len: record.len,
            origin: LEGACY,
        };
        self.moved.push((record.id, record.kind, logged, path));
        Ok(())
    }
}

pub(super) struct Copier {
    dir: PathBuf,
    segment_bytes: u64,
    next: Arc<AtomicU64>,
    active: Option<(u64, Arc<fs::File>, u64)>,
    touched: BTreeMap<u64, (Arc<fs::File>, u64)>,
    dirty: BTreeSet<u64>,
    made: bool,
}

impl Copier {
    pub(super) fn active(&self) -> Option<u64> {
        self.active.as_ref().map(|(n, _, _)| *n)
    }

    pub(super) fn roll(&mut self) -> Result<()> {
        let sealing = self
            .active
            .as_ref()
            .filter(|(n, _, _)| self.dirty.remove(n));
        if let Some((_, file, _)) = sealing {
            file.sync_data()?;
        }
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(self.dir.join(format!("{n:020}")))?;
        write_at(&file, &COPIED, 0)?;
        let file = Arc::new(file);
        self.touched.insert(n, (file.clone(), MARKED));
        self.active = Some((n, file, MARKED));
        self.dirty.insert(n);
        self.made = true;
        Ok(())
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(u64, u64)> {
        let rolls = match &self.active {
            None => true,
            Some((_, _, len)) => *len > MARKED && len + bytes.len() as u64 > self.segment_bytes,
        };
        if rolls {
            self.roll()?;
        }
        let (n, file, len) = self
            .active
            .as_mut()
            .expect("a copy segment is open once rolled");
        let at = *len;
        write_at(file, bytes, at)?;
        *len += bytes.len() as u64;
        let (n, file, len) = (*n, file.clone(), *len);
        self.touched.insert(n, (file, len));
        self.dirty.insert(n);
        Ok((n, at))
    }

    pub(super) fn sync(&mut self) -> Result<()> {
        for n in std::mem::take(&mut self.dirty) {
            if let Some((file, _)) = self.touched.get(&n) {
                file.sync_data()?;
            }
        }
        if std::mem::take(&mut self.made) {
            synced_dir(&self.dir)?;
        }
        Ok(())
    }

    fn written(&mut self) -> BTreeMap<u64, (Arc<fs::File>, u64)> {
        std::mem::take(&mut self.touched)
    }
}

pub(super) struct Landed {
    pub(super) drained: Vec<PathBuf>,
    pub(super) unlink: Vec<PathBuf>,
}

pub(super) struct Log {
    dir: PathBuf,
    segment_bytes: u64,
    next: Arc<AtomicU64>,
    index: HashMap<BlobId, Held>,
    tombs: HashMap<Put, Kept>,
    left: HashMap<BlobId, Vec<u64>>,
    kinds: Vec<String>,
    kind_of: HashMap<String, u32>,
    segments: BTreeMap<u64, Segment>,
    active: u64,
    copying: Option<u64>,
    dirty: BTreeSet<u64>,
    made: bool,
}

enum Scanned {
    Whole(Head, u32),
    Spoiled(u64),
    Short,
}

#[derive(Default)]
struct Found {
    records: Vec<(BlobId, Logged)>,
    tombs: Vec<(Put, u64, u64, u64)>,
}

impl Log {
    pub(super) fn open(dir: &Path, segment_bytes: u64) -> Result<(Log, Copier)> {
        fs::create_dir_all(dir)?;
        let mut numbers = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let number = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u64>().ok());
            numbers.extend(number);
        }
        numbers.sort_unstable();
        let next = numbers.last().map_or(1, |n| n + 1);
        let mut log = Log {
            dir: dir.to_path_buf(),
            segment_bytes,
            next: Arc::new(AtomicU64::new(next)),
            index: HashMap::new(),
            tombs: HashMap::new(),
            left: HashMap::new(),
            kinds: Vec::new(),
            kind_of: HashMap::new(),
            segments: BTreeMap::new(),
            active: 0,
            copying: None,
            dirty: BTreeSet::new(),
            made: false,
        };
        let mut streams = Vec::new();
        for n in numbers {
            if let Some(stream) = log.mark(n)? {
                streams.push((n, stream));
            }
        }
        let newest = |want: Stream| {
            streams
                .iter()
                .filter(|(_, stream)| *stream == want)
                .map(|(n, _)| *n)
                .next_back()
        };
        let (applied, copied) = (newest(Stream::Applied), newest(Stream::Copied));
        let mut found = Found::default();
        for (n, _) in &streams {
            let newest = Some(*n) == applied || Some(*n) == copied;
            log.scan(*n, newest, &mut found)?;
        }
        log.resolve(found);
        let open = |n: &u64| {
            let segment = log.segment(*n);
            segment.len < segment_bytes && segment.damaged.is_none()
        };
        let (applied, copied) = (applied.filter(open), copied.filter(open));
        match applied {
            Some(n) => log.active = n,
            None => log.roll()?,
        }
        log.copying = copied;
        let active = log.copying.map(|n| {
            let segment = log.segment(n);
            (n, segment.file.clone(), segment.len)
        });
        let copier = Copier {
            dir: dir.to_path_buf(),
            segment_bytes,
            next: log.next.clone(),
            active,
            touched: BTreeMap::new(),
            dirty: BTreeSet::new(),
            made: false,
        };
        Ok((log, copier))
    }

    fn path(&self, n: u64) -> PathBuf {
        self.dir.join(format!("{n:020}"))
    }

    fn segment(&self, n: u64) -> &Segment {
        self.segments.get(&n).expect("an indexed segment is open")
    }

    fn segment_mut(&mut self, n: u64) -> &mut Segment {
        self.segments
            .get_mut(&n)
            .expect("an indexed segment is open")
    }

    fn mark(&mut self, n: u64) -> Result<Option<Stream>> {
        let file = Arc::new(
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(self.path(n))?,
        );
        let len = file.metadata()?.len();
        let mut magic = [0u8; 8];
        let marked = filled(&mut &*file, &mut magic)?;
        let stream = match magic {
            APPLIED if marked => Some(Stream::Applied),
            COPIED if marked => Some(Stream::Copied),
            _ => None,
        };
        let made_and_cut = stream.is_none() && len <= MARKED;
        if made_and_cut {
            drop(file);
            gone(fs::remove_file(self.path(n)))?;
            tracing::warn!(
                target: "ducktape::blobs",
                event = "blob_segment_unmarked_removed",
                path = %self.path(n).display(),
                len,
                "a blob segment a stop cut before its first mark holds no record; it is removed"
            );
            return Ok(None);
        }
        let segment = Segment::opened(file, len, stream.unwrap_or(Stream::Applied));
        self.segments.insert(n, segment);
        if stream.is_none() {
            self.damage(n, 0);
        }
        Ok(stream)
    }

    fn scan(&mut self, n: u64, newest: bool, found: &mut Found) -> Result<()> {
        let segment = self.segment(n);
        let (file, len) = (segment.file.clone(), segment.len);
        let mut reader = BufReader::new(&*file);
        reader.seek(SeekFrom::Start(MARKED))?;
        let mut at = MARKED;
        while at < len {
            let (head, head_len) = match next(&mut reader, at, len, newest)? {
                Scanned::Whole(head, head_len) => (head, head_len),
                Scanned::Spoiled(size) => {
                    self.damage(n, at);
                    at += size;
                    reader.seek(SeekFrom::Start(at))?;
                    continue;
                }
                Scanned::Short => {
                    let mut rest = vec![0u8; (len - at) as usize];
                    read_at(&file, &mut rest, at)?;
                    let Some(skip) = next_whole(&rest) else {
                        break;
                    };
                    self.damage(n, at);
                    at += skip as u64;
                    reader.seek(SeekFrom::Start(at))?;
                    continue;
                }
            };
            let here = Origin { segment: n, at };
            let blob = match head {
                Head::Blob {
                    id,
                    kind,
                    len,
                    encoded,
                    crc,
                } => Some((id, kind, len, encoded, crc, here)),
                Head::Copy {
                    id,
                    kind,
                    len,
                    encoded,
                    crc,
                    origin_segment,
                    origin_at,
                } => {
                    let origin = Origin {
                        segment: origin_segment,
                        at: origin_at,
                    };
                    Some((id, kind, len, encoded, crc, origin))
                }
                Head::Tomb {
                    id,
                    segment,
                    at: killed,
                } => {
                    let size = 8 + u64::from(head_len);
                    let origin = Origin {
                        segment,
                        at: killed,
                    };
                    found.tombs.push(((id, origin), n, at, size));
                    at += size;
                    None
                }
            };
            if let Some((id, kind, body, encoded, crc, origin)) = blob {
                let kind = self.kind(&kind);
                let logged = Logged {
                    segment: n,
                    at,
                    head: head_len,
                    encoded,
                    crc,
                    kind,
                    len: body,
                    origin,
                };
                found.records.push((id, logged));
                at += logged.size();
            }
        }
        let unread = at < len;
        if !unread {
            return Ok(());
        }
        match newest {
            true => {
                file.set_len(at)?;
                file.sync_data()?;
                self.segment_mut(n).len = at;
                tracing::warn!(
                    target: "ducktape::blobs",
                    event = "blob_segment_torn",
                    path = %self.path(n).display(),
                    at,
                    cut = len - at,
                    "the newest blob segment of its stream ended in a write a stop cut short; its \
                     tail is cut off"
                );
            }
            false => self.damage(n, at),
        }
        Ok(())
    }

    fn resolve(&mut self, found: Found) {
        for (put, segment, at, size) in found.tombs {
            self.segment_mut(segment).dead += size;
            self.tombs.entry(put).or_insert(Kept {
                segment,
                at,
                size,
                targets: Vec::new(),
            });
        }
        let mut live: HashMap<BlobId, Vec<Logged>> = HashMap::new();
        for (id, logged) in found.records {
            let put = (id, logged.origin);
            match self.tombs.contains_key(&put) {
                true => {
                    self.segment_mut(logged.segment).dead += logged.size();
                    self.need(put, logged.segment);
                }
                false => live.entry(id).or_default().push(logged),
            }
        }
        for (id, mut logged) in live {
            logged.sort_by_key(|logged| {
                let copied = self.segment(logged.segment).stream == Stream::Copied;
                (logged.origin, copied, logged.segment, logged.at)
            });
            let Some(winner) = logged.pop() else {
                continue;
            };
            self.held(id, winner);
            for other in logged {
                self.segment_mut(other.segment).dead += other.size();
                self.stray(id, other.origin, other.segment);
            }
        }
    }

    fn damage(&mut self, n: u64, at: u64) {
        let path = self.path(n);
        let segment = self.segment_mut(n);
        let first = segment.damaged.is_none_or(|was| at < was);
        if !first {
            return;
        }
        segment.damaged = Some(at);
        tracing::warn!(
            target: "ducktape::blobs",
            event = "blob_segment_damaged",
            path = %path.display(),
            at,
            "a blob segment holds bytes from this offset on that do not read; it is kept as it is \
             and never compacted, and what it held before the offset still reads"
        );
    }

    fn kind(&mut self, kind: &str) -> u32 {
        if let Some(n) = self.kind_of.get(kind) {
            return *n;
        }
        let n = self.kinds.len() as u32;
        self.kinds.push(kind.to_owned());
        self.kind_of.insert(kind.to_owned(), n);
        n
    }

    fn held(&mut self, id: BlobId, logged: Logged) {
        let segment = self.segment_mut(logged.segment);
        segment.live += logged.size();
        segment.records += 1;
        if let Some(Held::Logged(old)) = self.index.insert(id, Held::Logged(logged)) {
            self.dropped(&old);
            if old.origin == logged.origin {
                self.stray(id, old.origin, old.segment);
            }
        }
    }

    fn dropped(&mut self, old: &Logged) {
        let segment = self.segment_mut(old.segment);
        segment.live = segment.live.saturating_sub(old.size());
        segment.dead += old.size();
        segment.records -= 1;
    }

    fn kill(&mut self, id: &BlobId) {
        if let Some(Held::Logged(logged)) = self.index.remove(id) {
            self.dropped(&logged);
        }
    }

    fn stray(&mut self, id: BlobId, origin: Origin, segment: u64) {
        let held = matches!(self.index.get(&id), Some(Held::Logged(now)) if now.origin == origin);
        match held {
            true => {
                let left = self.left.entry(id).or_default();
                if !left.contains(&segment) {
                    left.push(segment);
                }
            }
            false => self.need((id, origin), segment),
        }
    }

    fn need(&mut self, put: Put, target: u64) {
        let Some(kept) = self.tombs.get_mut(&put) else {
            return;
        };
        let known = kept.segment == target || kept.targets.contains(&target);
        if known {
            return;
        }
        let first = kept.targets.is_empty();
        kept.targets.push(target);
        let (segment, size) = (kept.segment, kept.size);
        if first {
            let segment = self.segment_mut(segment);
            segment.dead = segment.dead.saturating_sub(size);
            segment.live += size;
            segment.tombs += 1;
        }
    }

    fn roll(&mut self) -> Result<()> {
        let was_dirty = self.dirty.remove(&self.active);
        let sealing = self.segments.get(&self.active).filter(|_| was_dirty);
        if let Some(segment) = sealing {
            segment.file.sync_data()?;
        }
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(self.path(n))?;
        write_at(&file, &APPLIED, 0)?;
        self.segments
            .insert(n, Segment::opened(Arc::new(file), MARKED, Stream::Applied));
        self.active = n;
        self.dirty.insert(n);
        self.made = true;
        Ok(())
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(u64, u64)> {
        let len = self.segment(self.active).len;
        let empty = len == MARKED;
        let over = len + bytes.len() as u64 > self.segment_bytes;
        let rolls = over && !empty;
        if rolls {
            self.roll()?;
        }
        let active = self.active;
        let segment = self.segment_mut(active);
        let at = segment.len;
        write_at(&segment.file, bytes, at)?;
        segment.len += bytes.len() as u64;
        self.dirty.insert(active);
        Ok((active, at))
    }

    pub(super) fn holds(&self, id: &BlobId) -> bool {
        self.index.contains_key(id)
    }

    pub(super) fn ids(&self) -> Vec<BlobId> {
        self.index.keys().copied().collect()
    }

    pub(super) fn hit(&self, id: &BlobId) -> Option<Hit> {
        match self.index.get(id)? {
            Held::Logged(logged) => Some(Hit::Logged {
                file: self.segment(logged.segment).file.clone(),
                logged: *logged,
                kind: self.kinds[logged.kind as usize].clone(),
            }),
            Held::Framed { kind, len } => Some(Hit::Framed {
                kind: self.kinds[*kind as usize].clone(),
                len: *len,
            }),
        }
    }

    pub(super) fn log(&mut self, record: &Record) -> Result<()> {
        if self.holds(&record.id) {
            return Ok(());
        }
        let (segment, at) = self.append(&record.bytes)?;
        let kind = self.kind(&record.kind);
        let logged = Logged {
            segment,
            at,
            head: record.head,
            encoded: record.encoded,
            crc: record.crc,
            kind,
            len: record.len,
            origin: Origin { segment, at },
        };
        self.held(record.id, logged);
        Ok(())
    }

    pub(super) fn hold_framed(&mut self, id: BlobId, kind: &str, len: u64) {
        if self.holds(&id) {
            return;
        }
        let kind = self.kind(kind);
        self.index.insert(id, Held::Framed { kind, len });
    }

    pub(super) fn unframe(&mut self, id: &BlobId) -> bool {
        let framed = matches!(self.index.get(id), Some(Held::Framed { .. }));
        if framed {
            self.index.remove(id);
        }
        framed
    }

    pub(super) fn bury(&mut self, id: &BlobId) -> Result<()> {
        let Some(Held::Logged(logged)) = self.index.get(id).copied() else {
            return Ok(());
        };
        let mut targets = self.left.remove(id).unwrap_or_default();
        targets.push(logged.segment);
        self.kill(id);
        self.entomb(*id, logged.origin, targets)
    }

    pub(super) fn bury_legacy(&mut self, id: &BlobId) -> Result<()> {
        self.entomb(*id, LEGACY, Vec::new())
    }

    fn entomb(&mut self, id: BlobId, origin: Origin, mut targets: Vec<u64>) -> Result<()> {
        let head = borsh::to_vec(&Head::Tomb {
            id,
            segment: origin.segment,
            at: origin.at,
        })?;
        let bytes = laid(&head, &[]);
        let (segment, at) = self.append(&bytes)?;
        let size = bytes.len() as u64;
        targets.retain(|target| *target != segment && self.segments.contains_key(target));
        targets.sort_unstable();
        targets.dedup();
        let needed = !targets.is_empty();
        let kept = Kept {
            segment,
            at,
            size,
            targets,
        };
        let was = self.tombs.insert((id, origin), kept);
        if let Some(was) = was {
            self.unkeep(&was);
        }
        let segment = self.segment_mut(segment);
        match needed {
            true => {
                segment.live += size;
                segment.tombs += 1;
            }
            false => segment.dead += size,
        }
        Ok(())
    }

    fn unkeep(&mut self, kept: &Kept) {
        let needed = !kept.targets.is_empty();
        let Some(segment) = self.segments.get_mut(&kept.segment) else {
            return;
        };
        if needed {
            segment.live = segment.live.saturating_sub(kept.size);
            segment.dead += kept.size;
            segment.tombs -= 1;
        }
    }

    pub(super) fn worn(&self) -> Vec<u64> {
        self.segments
            .iter()
            .filter(|(_, segment)| {
                let wasted = segment.dead > 0 && segment.dead >= segment.live;
                wasted && segment.damaged.is_none()
            })
            .map(|(n, _)| *n)
            .collect()
    }

    pub(super) fn plan(&mut self, n: u64, budget: u64, whole: u64) -> Result<Plan> {
        if n == self.active {
            self.roll()?;
        }
        let mut records: Vec<(BlobId, Logged)> = self
            .index
            .iter()
            .filter_map(|(id, held)| match held {
                Held::Logged(logged) if logged.segment == n => Some((*id, *logged)),
                _ => None,
            })
            .collect();
        records.sort_by_key(|(_, logged)| logged.at);
        let mut bytes = 0u64;
        let mut taken = Vec::new();
        let fits =
            |bytes: u64, size: u64| bytes + size <= budget || (bytes == 0 && budget == whole);
        for (id, logged) in records {
            let copied = logged.size() + logged.grows();
            if !fits(bytes, copied) {
                break;
            }
            bytes += copied;
            let kind = self.kinds[logged.kind as usize].clone();
            taken.push((id, logged, kind));
        }
        let mut tombs = Vec::new();
        let records_all_taken = taken.len() as u64 == self.segment(n).records;
        if records_all_taken {
            let mut kept: Vec<(Put, u64, u64)> = self
                .tombs
                .iter()
                .filter(|(_, kept)| kept.segment == n && !kept.targets.is_empty())
                .map(|(put, kept)| (*put, kept.at, kept.size))
                .collect();
            kept.sort_by_key(|(_, at, _)| *at);
            for (put, at, size) in kept {
                if !fits(bytes, size) {
                    break;
                }
                bytes += size;
                tombs.push((put, at, size));
            }
        }
        Ok(Plan {
            segment: n,
            file: self.segment(n).file.clone(),
            records: taken,
            tombs,
            bytes,
        })
    }

    fn written(&mut self, copier: &mut Copier) {
        for (n, (file, len)) in copier.written() {
            self.segments
                .entry(n)
                .and_modify(|segment| segment.len = len)
                .or_insert_with(|| Segment::opened(file, len, Stream::Copied));
        }
        self.copying = copier.active();
    }

    pub(super) fn land(&mut self, round: Round, copier: &mut Copier) -> Landed {
        self.written(copier);
        for logged in round.spoiled {
            self.damage(logged.segment, logged.at);
        }
        for (id, from, copy) in round.records {
            let still = matches!(self.index.get(&id), Some(Held::Logged(now)) if *now == from);
            match still {
                true => self.held(id, copy),
                false => {
                    self.segment_mut(copy.segment).dead += copy.size();
                    self.stray(id, copy.origin, copy.segment);
                }
            }
        }
        for (put, from_segment, from_at, segment, at) in round.tombs {
            self.landed_tomb(put, (from_segment, from_at), (segment, at));
        }
        let mut unlink = Vec::new();
        for (id, kind, copy, path) in round.moved {
            let copy = Logged {
                kind: self.kind(&kind),
                ..copy
            };
            let fresh = path.is_file() && !self.holds(&id);
            match fresh {
                true => self.held(id, copy),
                false => {
                    self.segment_mut(copy.segment).dead += copy.size();
                    self.stray(id, copy.origin, copy.segment);
                }
            }
            if self.holds(&id) {
                unlink.push(path);
            }
        }
        let mut drained = Vec::new();
        for n in round.sources {
            if self.drained(n) {
                drained.push(self.retire(n));
            }
        }
        Landed { drained, unlink }
    }

    pub(super) fn strays(&mut self, round: Round, copier: &mut Copier) {
        self.written(copier);
        let copies = round.records.into_iter().map(|(id, _, copy)| (id, copy));
        let moved = round.moved.into_iter().map(|(id, _, copy, _)| (id, copy));
        for (id, copy) in copies.chain(moved) {
            self.segment_mut(copy.segment).dead += copy.size();
            self.stray(id, copy.origin, copy.segment);
        }
    }

    fn landed_tomb(&mut self, put: Put, from: (u64, u64), to: (u64, u64)) {
        let size = match self.tombs.get(&put) {
            Some(kept) => kept.size,
            None => return,
        };
        let moves = self
            .tombs
            .get(&put)
            .is_some_and(|kept| (kept.segment, kept.at) == from && !kept.targets.is_empty());
        if !moves {
            self.segment_mut(to.0).dead += size;
            return;
        }
        let kept = self.tombs.get_mut(&put).expect("a moving tomb is kept");
        kept.segment = to.0;
        kept.at = to.1;
        kept.targets.retain(|target| *target != to.0);
        let needed = !kept.targets.is_empty();
        let old = self.segment_mut(from.0);
        old.live = old.live.saturating_sub(size);
        old.dead += size;
        old.tombs -= 1;
        let new = self.segment_mut(to.0);
        match needed {
            true => {
                new.live += size;
                new.tombs += 1;
            }
            false => new.dead += size,
        }
    }

    fn drained(&self, n: u64) -> bool {
        let Some(segment) = self.segments.get(&n) else {
            return false;
        };
        let empty = segment.records == 0 && segment.tombs == 0;
        let open = n == self.active || Some(n) == self.copying;
        empty && segment.damaged.is_none() && !open
    }

    fn retire(&mut self, n: u64) -> PathBuf {
        self.segments.remove(&n);
        self.dirty.remove(&n);
        for left in self.left.values_mut() {
            left.retain(|segment| *segment != n);
        }
        self.left.retain(|_, left| !left.is_empty());
        self.tombs.retain(|_, kept| kept.segment != n);
        let mut freed = Vec::new();
        for kept in self.tombs.values_mut() {
            let had = !kept.targets.is_empty();
            kept.targets.retain(|target| *target != n);
            if had && kept.targets.is_empty() {
                freed.push((kept.segment, kept.size));
            }
        }
        for (segment, size) in freed {
            let segment = self.segment_mut(segment);
            segment.live = segment.live.saturating_sub(size);
            segment.dead += size;
            segment.tombs -= 1;
        }
        self.path(n)
    }

    pub(super) fn dirty(&mut self) -> (Vec<Arc<fs::File>>, bool) {
        let dirty = std::mem::take(&mut self.dirty);
        let files = dirty
            .into_iter()
            .filter_map(|n| self.segments.get(&n))
            .map(|segment| segment.file.clone())
            .collect();
        (files, std::mem::take(&mut self.made))
    }
}

fn next_whole(rest: &[u8]) -> Option<usize> {
    (1..rest.len()).find(|skip| whole_at(&rest[*skip..]).is_some())
}

fn whole_at(bytes: &[u8]) -> Option<()> {
    let prefix: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    let head_end = 4usize.checked_add(u32::from_le_bytes(prefix) as usize)?;
    let head: Head = borsh::from_slice(bytes.get(4..head_end)?).ok()?;
    let crc: [u8; 4] = bytes
        .get(head_end..head_end.checked_add(4)?)?
        .try_into()
        .ok()?;
    let intact = crc32fast::hash(&bytes[..head_end]) == u32::from_le_bytes(crc);
    if !intact {
        return None;
    }
    let (encoded, crc) = match head {
        Head::Blob { encoded, crc, .. } | Head::Copy { encoded, crc, .. } => (encoded, crc),
        Head::Tomb { .. } => return Some(()),
    };
    let start = head_end + 4;
    let end = start.checked_add(usize::try_from(encoded).ok()?)?;
    let body = bytes.get(start..end)?;
    (crc32fast::hash(body) == crc).then_some(())
}

fn filled(reader: &mut impl Read, bytes: &mut [u8]) -> io::Result<bool> {
    match reader.read_exact(bytes) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error),
    }
}

fn next(reader: &mut BufReader<&fs::File>, at: u64, len: u64, newest: bool) -> Result<Scanned> {
    let mut prefix = [0u8; 4];
    if !filled(reader, &mut prefix)? {
        return Ok(Scanned::Short);
    }
    let head_len = u32::from_le_bytes(prefix);
    let fits = at + 8 + u64::from(head_len) <= len;
    if !fits {
        return Ok(Scanned::Short);
    }
    let mut head = vec![0u8; head_len as usize];
    let mut crc = [0u8; 4];
    let whole = filled(reader, &mut head)? && filled(reader, &mut crc)?;
    if !whole {
        return Ok(Scanned::Short);
    }
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&prefix);
    hasher.update(&head);
    let intact = hasher.finalize() == u32::from_le_bytes(crc);
    if !intact {
        return Ok(Scanned::Short);
    }
    let Ok(head) = borsh::from_slice::<Head>(&head) else {
        return Ok(Scanned::Short);
    };
    let (encoded, body_crc) = match &head {
        Head::Blob { encoded, crc, .. } | Head::Copy { encoded, crc, .. } => (*encoded, *crc),
        Head::Tomb { .. } => (0, crc32fast::hash(&[])),
    };
    let body_fits = at + 8 + u64::from(head_len) + encoded <= len;
    if !body_fits {
        return Ok(Scanned::Short);
    }
    match newest {
        true => {
            let mut body = vec![0u8; encoded as usize];
            if !filled(reader, &mut body)? {
                return Ok(Scanned::Short);
            }
            let intact = crc32fast::hash(&body) == body_crc;
            if !intact {
                return Ok(Scanned::Spoiled(8 + u64::from(head_len) + encoded));
            }
        }
        false => {
            let past = i64::try_from(encoded)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "a record past i64"))?;
            reader.seek_relative(past)?;
        }
    }
    Ok(Scanned::Whole(head, head_len))
}

#[cfg(test)]
pub(super) fn heads(path: &Path) -> Vec<(u64, u64)> {
    let bytes = fs::read(path).unwrap();
    let mut at = MARKED as usize;
    let mut heads = Vec::new();
    while at + 4 <= bytes.len() {
        let head_len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        let head: Head = borsh::from_slice(&bytes[at + 4..at + 4 + head_len]).unwrap();
        let encoded = match head {
            Head::Blob { encoded, .. } | Head::Copy { encoded, .. } => encoded as usize,
            Head::Tomb { .. } => 0,
        };
        let size = 8 + head_len + encoded;
        heads.push((at as u64, size as u64));
        at += size;
    }
    heads
}
