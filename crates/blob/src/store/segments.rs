// The log small blobs live in: numbered append-only segment files of records, each a blob's
// zstd-encoded framed bytes or a tombstone naming the record a removal killed, with the index the
// records rebuild at open and the bookkeeping that says when a segment is worth compacting.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{self, BufReader, Read, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use abi::{BlobHeader, BlobId};
use borsh::{BorshDeserialize, BorshSerialize};

use super::{Error, Result, framed_len, gone, not_kept, read_at, verified, write_at};
use crate::parse;

const MAGIC: [u8; 8] = *b"dtblobs1";
const MARKED: u64 = MAGIC.len() as u64;

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
    let (header, _) =
        parse(framed).ok_or_else(|| Error::Corrupt(id, "the frame does not parse".into()))?;
    let body = zstd::bulk::compress(framed, zstd::DEFAULT_COMPRESSION_LEVEL)?;
    let crc = crc32fast::hash(&body);
    let head = borsh::to_vec(&Head::Blob {
        id,
        kind: header.kind.clone(),
        len: header.len,
        encoded: body.len() as u64,
        crc,
    })?;
    Ok(Record {
        id,
        kind: header.kind,
        len: header.len,
        head: head.len() as u32,
        encoded: body.len() as u64,
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
}

impl Logged {
    fn size(&self) -> u64 {
        8 + u64::from(self.head) + self.encoded
    }

    fn body(&self) -> u64 {
        self.at + 8 + u64::from(self.head)
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
    len: u64,
    live: u64,
    dead: u64,
    records: u64,
    tombs: Vec<Tomb>,
    damaged: Option<u64>,
}

impl Segment {
    fn opened(file: Arc<fs::File>, len: u64) -> Segment {
        Segment {
            file,
            len,
            live: 0,
            dead: 0,
            records: 0,
            tombs: Vec::new(),
            damaged: None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Tomb {
    at: u64,
    size: u64,
    target: u64,
}

pub(super) struct Plan {
    segment: u64,
    file: Arc<fs::File>,
    records: Vec<(BlobId, Logged)>,
    tombs: Vec<Tomb>,
    pub(super) bytes: u64,
}

pub(super) struct Copies {
    segment: u64,
    records: Vec<(BlobId, Logged, Vec<u8>)>,
    tombs: Vec<(Tomb, Vec<u8>)>,
}

impl Plan {
    pub(super) fn read(self) -> Result<Copies> {
        let mut records = Vec::with_capacity(self.records.len());
        for (id, logged) in self.records {
            let mut bytes = vec![0u8; logged.size() as usize];
            read_at(&self.file, &mut bytes, logged.at)?;
            records.push((id, logged, bytes));
        }
        let mut tombs = Vec::with_capacity(self.tombs.len());
        for tomb in self.tombs {
            let mut bytes = vec![0u8; tomb.size as usize];
            read_at(&self.file, &mut bytes, tomb.at)?;
            tombs.push((tomb, bytes));
        }
        Ok(Copies {
            segment: self.segment,
            records,
            tombs,
        })
    }
}

pub(super) struct Log {
    dir: PathBuf,
    segment_bytes: u64,
    index: HashMap<BlobId, Held>,
    kinds: Vec<String>,
    kind_of: HashMap<String, u32>,
    segments: BTreeMap<u64, Segment>,
    active: u64,
    dirty: BTreeSet<u64>,
    made: bool,
}

enum Scanned {
    Whole(Head, u32),
    Spoiled(u64),
    Short,
}

impl Log {
    pub(super) fn open(dir: &Path, segment_bytes: u64) -> Result<Log> {
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
        let mut log = Log {
            dir: dir.to_path_buf(),
            segment_bytes,
            index: HashMap::new(),
            kinds: Vec::new(),
            kind_of: HashMap::new(),
            segments: BTreeMap::new(),
            active: 0,
            dirty: BTreeSet::new(),
            made: false,
        };
        let newest = numbers.last().copied();
        for n in numbers {
            log.scan(n, newest == Some(n))?;
        }
        let open = newest.filter(|n| {
            let segment = log.segment(*n);
            segment.len < segment_bytes && segment.damaged.is_none()
        });
        match open {
            Some(n) => log.active = n,
            None => log.roll()?,
        }
        Ok(log)
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

    fn scan(&mut self, n: u64, newest: bool) -> Result<()> {
        let file = Arc::new(
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(self.path(n))?,
        );
        let len = file.metadata()?.len();
        self.segments.insert(n, Segment::opened(file.clone(), len));
        let mut reader = BufReader::new(&*file);
        let mut magic = [0u8; 8];
        let marked = filled(&mut reader, &mut magic)? && magic == MAGIC;
        if !marked {
            let made_and_cut = newest && len <= MARKED;
            return match made_and_cut {
                true => self.restart(n),
                false => {
                    self.damage(n, 0);
                    Ok(())
                }
            };
        }
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
            match head {
                Head::Blob {
                    id,
                    kind,
                    len: body,
                    encoded,
                    crc,
                } => {
                    let kind = self.kind(&kind);
                    let logged = Logged {
                        segment: n,
                        at,
                        head: head_len,
                        encoded,
                        crc,
                        kind,
                        len: body,
                    };
                    self.held(id, logged);
                    at += logged.size();
                }
                Head::Tomb {
                    id,
                    segment,
                    at: killed,
                } => {
                    let kills = match self.index.get(&id) {
                        Some(Held::Logged(logged)) => {
                            (logged.segment, logged.at) == (segment, killed)
                        }
                        _ => false,
                    };
                    if kills {
                        self.kill(&id);
                    }
                    let size = 8 + u64::from(head_len);
                    let tomb = Tomb {
                        at,
                        size,
                        target: segment,
                    };
                    self.keep_tomb(n, tomb);
                    at += size;
                }
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
                    "the newest blob segment ended in a write a stop cut short; its tail is cut off"
                );
            }
            false => self.damage(n, at),
        }
        Ok(())
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

    fn restart(&mut self, n: u64) -> Result<()> {
        let file = self.segment(n).file.clone();
        file.set_len(0)?;
        write_at(&file, &MAGIC, 0)?;
        file.sync_data()?;
        self.segment_mut(n).len = MARKED;
        Ok(())
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

    fn keep_tomb(&mut self, n: u64, tomb: Tomb) {
        let needed = tomb.target != n && self.segments.contains_key(&tomb.target);
        let segment = self.segment_mut(n);
        match needed {
            true => {
                segment.live += tomb.size;
                segment.tombs.push(tomb);
            }
            false => segment.dead += tomb.size,
        }
    }

    fn roll(&mut self) -> Result<()> {
        let was_dirty = self.dirty.remove(&self.active);
        let sealing = self.segments.get(&self.active).filter(|_| was_dirty);
        if let Some(segment) = sealing {
            segment.file.sync_data()?;
        }
        let n = self.segments.keys().next_back().map_or(1, |n| n + 1);
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(self.path(n))?;
        write_at(&file, &MAGIC, 0)?;
        self.segments
            .insert(n, Segment::opened(Arc::new(file), MARKED));
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
        let head = borsh::to_vec(&Head::Tomb {
            id: *id,
            segment: logged.segment,
            at: logged.at,
        })?;
        let bytes = laid(&head, &[]);
        let (segment, at) = self.append(&bytes)?;
        self.kill(id);
        let tomb = Tomb {
            at,
            size: bytes.len() as u64,
            target: logged.segment,
        };
        self.keep_tomb(segment, tomb);
        Ok(())
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
            if !fits(bytes, logged.size()) {
                break;
            }
            bytes += logged.size();
            taken.push((id, logged));
        }
        let mut tombs = Vec::new();
        let records_all_taken = taken.len() as u64 == self.segment(n).records;
        if records_all_taken {
            for tomb in &self.segment(n).tombs {
                if !fits(bytes, tomb.size) {
                    break;
                }
                bytes += tomb.size;
                tombs.push(*tomb);
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

    pub(super) fn land(&mut self, copies: Copies) -> Result<()> {
        let n = copies.segment;
        for (id, logged, bytes) in copies.records {
            let still = match self.index.get(&id) {
                Some(Held::Logged(now)) => *now == logged,
                _ => false,
            };
            if !still {
                continue;
            }
            let body = &bytes[(logged.body() - logged.at) as usize..];
            let intact = crc32fast::hash(body) == logged.crc;
            if !intact {
                self.damage(n, logged.at);
                continue;
            }
            let (segment, at) = self.append(&bytes)?;
            self.held(
                id,
                Logged {
                    segment,
                    at,
                    ..logged
                },
            );
        }
        for (tomb, bytes) in copies.tombs {
            let kept = self.segment(n).tombs.iter().position(|kept| *kept == tomb);
            let Some(kept) = kept else {
                continue;
            };
            let old = self.segment_mut(n);
            old.tombs.remove(kept);
            old.live = old.live.saturating_sub(tomb.size);
            old.dead += tomb.size;
            let (segment, at) = self.append(&bytes)?;
            self.keep_tomb(segment, Tomb { at, ..tomb });
        }
        Ok(())
    }

    pub(super) fn drained(&self, n: u64) -> bool {
        let segment = self.segment(n);
        let empty = segment.records == 0 && segment.tombs.is_empty();
        empty && segment.damaged.is_none() && n != self.active
    }

    pub(super) fn retire(&mut self, n: u64) -> Result<()> {
        gone(fs::remove_file(self.path(n)))?;
        self.segments.remove(&n);
        self.dirty.remove(&n);
        for segment in self.segments.values_mut() {
            let tombs = std::mem::take(&mut segment.tombs);
            let (freed, kept): (Vec<Tomb>, Vec<Tomb>) =
                tombs.into_iter().partition(|tomb| tomb.target == n);
            let bytes: u64 = freed.iter().map(|tomb| tomb.size).sum();
            segment.live = segment.live.saturating_sub(bytes);
            segment.dead += bytes;
            segment.tombs = kept;
        }
        Ok(())
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
    match head {
        Head::Blob { encoded, crc, .. } => {
            let start = head_end + 4;
            let end = start.checked_add(usize::try_from(encoded).ok()?)?;
            let body = bytes.get(start..end)?;
            (crc32fast::hash(body) == crc).then_some(())
        }
        Head::Tomb { .. } => Some(()),
    }
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
        Head::Blob { encoded, crc, .. } => (*encoded, *crc),
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
