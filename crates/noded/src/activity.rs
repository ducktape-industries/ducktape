// The activity index: each frame a block carried, newest first network-wide, under its signer and
// under its account, and each account's frames per program, in a store of the node's own that no
// commitment covers. A thread of its own writes it, so it may lag the chain and never holds a block.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use abi::hook::Record;
use abi::role::identity::AccountNumber;
use abi::{Entry, Outcome, Receipt, Refusal, reason};
use borsh::{BorshDeserialize, BorshSerialize};
use fluent31::{Db, Options, Snapshot, WriteBatch};
use tokio::sync::watch;

use crate::wire::activity::{Frame, Indexed, PageRequest, PageResponse, Query, Reply, Used};
use crate::{Error, Result};

pub(crate) const DIR: &str = "activity";
pub(crate) const PROGRAM: &str = "activity";

const FRAMES: &[u8] = b"f/";
const SIGNED: &[u8] = b"k/";
const ACTED: &[u8] = b"a/";
const USED: &[u8] = b"u/";
const REFUSED: &[u8] = b"r/";
const TABLES: [&[u8]; 4] = [FRAMES, SIGNED, ACTED, USED];
const HEIGHT: &[u8] = b"\xffheight";
const FROM: &[u8] = b"\xfffrom";

pub(crate) struct Block {
    pub(crate) height: u64,
    pub(crate) time: u64,
    pub(crate) records: Vec<Record>,
    pub(crate) refused: Vec<(u32, Refusal)>,
}

#[derive(Clone, Copy)]
pub(crate) struct Lag {
    pub(crate) from: u64,
    pub(crate) epoch: u64,
}

pub(crate) struct Seed {
    pub(crate) height: u64,
    pub(crate) rows: Vec<Entry>,
}

enum Event {
    Block(Block),
    Close,
}

enum Flow {
    Indexing,
    Closed,
}

enum Place {
    Behind,
    Next,
    Past,
}

#[derive(BorshSerialize, BorshDeserialize)]
struct Cursor {
    height: u64,
    scope: Vec<u8>,
    after: Vec<u8>,
}

type Answer<T> = Result<std::result::Result<T, Refusal>>;

pub(crate) struct Index {
    reads: Reads,
    feed: mpsc::Sender<Event>,
    indexed: watch::Receiver<Option<Indexed>>,
    lag: Lag,
    lagging: AtomicBool,
}

#[derive(Clone)]
pub(crate) struct Reads {
    db: Arc<Db>,
}

struct Writer {
    db: Arc<Db>,
    indexed: watch::Sender<Option<Indexed>>,
}

struct Listing<'a> {
    db: &'a Db,
    snapshot: &'a Snapshot,
    height: u64,
}

struct Rows {
    items: Vec<(Vec<u8>, Vec<u8>)>,
    next: Option<Vec<u8>>,
}

impl Index {
    pub(crate) fn open<S>(dir: &Path, lag: Lag, seed: S) -> Result<Index>
    where
        S: FnOnce() -> Result<Seed> + Send + 'static,
    {
        let options = Options {
            wasm_enabled: false,
            max_key_size: usize::MAX,
            max_value_size: usize::MAX,
            max_txn_write_bytes: usize::MAX,
            ..Options::default()
        };
        let db = Arc::new(Db::open(dir, options)?);
        let (feed, events) = mpsc::channel();
        let (written, indexed) = watch::channel(None);
        let writer = Writer {
            db: db.clone(),
            indexed: written,
        };
        std::thread::Builder::new()
            .name("activity".into())
            .spawn(move || writer.run(seed, events))?;
        Ok(Index {
            reads: Reads { db },
            feed,
            indexed,
            lag,
            lagging: AtomicBool::new(false),
        })
    }

    pub(crate) fn offer(&self, block: Block) {
        let indexed = self
            .indexed
            .borrow()
            .map_or(self.lag.from, |span| span.height);
        let lag = block.height.saturating_sub(indexed);
        let was = self.lagging.load(Ordering::Relaxed);
        let Lagged { behind, logs } = lagged(was, lag, self.lag.epoch);
        self.lagging.store(behind, Ordering::Relaxed);
        if logs {
            tracing::info!(
                target: "ducktape::activity",
                event = "activity_lag",
                lag,
                height = block.height,
                indexed,
                reason = "index_behind",
                "the activity index is more than an epoch behind the chain"
            );
        }
        let _ = self.feed.send(Event::Block(block));
    }

    pub(crate) async fn close(&self) {
        let _ = self.feed.send(Event::Close);
        let mut indexed = self.indexed.clone();
        while indexed.changed().await.is_ok() {}
    }

    pub(crate) fn reads(&self) -> Reads {
        self.reads.clone()
    }

    pub(crate) fn indexed(&self) -> watch::Receiver<Option<Indexed>> {
        self.indexed.clone()
    }
}

impl Writer {
    fn run<S: FnOnce() -> Result<Seed>>(self, seed: S, events: mpsc::Receiver<Event>) {
        crate::server::lowest_priority();
        let span = match self.opened(seed) {
            Ok(span) => span,
            Err(error) => {
                tracing::error!(
                    target: "ducktape::activity",
                    event = "activity_unopened",
                    %error,
                    "the activity index did not open"
                );
                return;
            }
        };
        self.indexed.send_replace(Some(span));
        while let Ok(event) = events.recv() {
            let flow = match event {
                Event::Block(block) => self.took(block),
                Event::Close => self.closed(),
            };
            if let Flow::Closed = flow {
                return;
            }
        }
    }

    fn opened<S: FnOnce() -> Result<Seed>>(&self, seed: S) -> Result<Indexed> {
        let snapshot = self.db.snapshot();
        if let Some(span) = span_at(&self.db, &snapshot)? {
            return Ok(span);
        }
        seeded(&self.db, seed()?)
    }

    fn took(&self, block: Block) -> Flow {
        let Some(span) = *self.indexed.borrow() else {
            return Flow::Indexing;
        };
        let from = match place(span, block.height) {
            Place::Behind => return Flow::Indexing,
            Place::Next => span.from,
            Place::Past => skipped(span, block.height),
        };
        let span = Indexed {
            from,
            height: block.height,
        };
        match written(&self.db, &block, span) {
            Ok(()) => {
                self.indexed.send_replace(Some(span));
            }
            Err(error) => tracing::warn!(
                target: "ducktape::activity",
                event = "activity_unwritten",
                height = block.height,
                %error,
                reason = "store_failed",
                "the activity index did not keep a block"
            ),
        }
        Flow::Indexing
    }

    fn closed(&self) -> Flow {
        if let Err(error) = self.db.flush() {
            tracing::warn!(
                target: "ducktape::activity",
                event = "activity_unflushed",
                %error,
                reason = "store_failed",
                "the activity index did not flush as it closed"
            );
        }
        Flow::Closed
    }
}

pub(crate) fn refused(records: &[Record], receipts: &[Receipt]) -> Vec<(u32, Refusal)> {
    records
        .iter()
        .filter(|record| !record.applied)
        .filter_map(|record| {
            let receipt = receipts.get(record.position as usize)?;
            let Outcome::Rejected(refusal) = &receipt.outcome else {
                return None;
            };
            Some((record.position, refusal.clone()))
        })
        .collect()
}

struct Lagged {
    behind: bool,
    logs: bool,
}

fn lagged(was_behind: bool, lag: u64, epoch: u64) -> Lagged {
    let past_an_epoch = lag > epoch;
    let caught_up = lag <= 1;
    match (was_behind, past_an_epoch, caught_up) {
        (false, true, _) => Lagged {
            behind: true,
            logs: true,
        },
        (true, _, true) => Lagged {
            behind: false,
            logs: false,
        },
        (behind, _, _) => Lagged {
            behind,
            logs: false,
        },
    }
}

fn place(span: Indexed, height: u64) -> Place {
    let behind = height <= span.height;
    let next = height == span.height + 1;
    match (behind, next) {
        (true, _) => Place::Behind,
        (false, true) => Place::Next,
        (false, false) => Place::Past,
    }
}

fn skipped(span: Indexed, height: u64) -> u64 {
    tracing::warn!(
        target: "ducktape::activity",
        event = "activity_gap",
        indexed = span.height,
        height,
        reason = "blocks_unindexed",
        "the activity index missed blocks and holds a whole history only from this one"
    );
    height
}

fn newest(height: u64, position: u32) -> Vec<u8> {
    [
        (u64::MAX - height).to_be_bytes().as_slice(),
        &(u32::MAX - position).to_be_bytes(),
    ]
    .concat()
}

fn escaped(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 2);
    for &byte in bytes {
        out.push(byte);
        if byte == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0, 0]);
    out
}

fn unescaped(mut bytes: &[u8]) -> Option<Vec<u8>> {
    let mut value = Vec::new();
    loop {
        match bytes {
            [0, 0] => return Some(value),
            [0, 0xFF, rest @ ..] => {
                value.push(0);
                bytes = rest;
            }
            [byte, rest @ ..] if *byte != 0 => {
                value.push(*byte);
                bytes = rest;
            }
            _ => return None,
        }
    }
}

fn used_key(account: AccountNumber, program: &str) -> Vec<u8> {
    [USED, &account.to_be_bytes(), &escaped(program.as_bytes())].concat()
}

fn written(db: &Db, block: &Block, span: Indexed) -> Result<()> {
    let mut batch = WriteBatch::new();
    let mut used: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
    for record in &block.records {
        let frame = Frame {
            height: record.height,
            position: record.position,
            time: block.time,
            signer: record.signer.clone(),
            target: record.target.clone(),
            applied: record.applied,
        };
        let at = newest(frame.height, frame.position);
        let value = abi::encode(&frame);
        batch.put([FRAMES, &at].concat(), value.clone());
        batch.put(
            [SIGNED, &escaped(&frame.signer), &at].concat(),
            value.clone(),
        );
        let Some(account) = record.account else {
            continue;
        };
        batch.put([ACTED, &account.to_be_bytes(), &at].concat(), value);
        let key = used_key(account, &frame.target);
        let frames = match used.remove(&key) {
            Some(frames) => frames,
            None => counted(db, &key)?,
        };
        used.insert(key, frames + 1);
    }
    for (key, frames) in used {
        batch.put(key, abi::encode(&frames));
    }
    for (position, refusal) in &block.refused {
        let at = newest(block.height, *position);
        batch.put([REFUSED, &at].concat(), abi::encode(refusal));
    }
    batch.put(HEIGHT, abi::encode(&span.height));
    batch.put(FROM, abi::encode(&span.from));
    Ok(db.write(batch)?)
}

fn counted(db: &Db, key: &[u8]) -> Result<u64> {
    let Some(bytes) = db.get(key)? else {
        return Ok(0);
    };
    decoded(&bytes)
}

fn seeded(db: &Db, seed: Seed) -> Result<Indexed> {
    let kept: Vec<Entry> = seed
        .rows
        .into_iter()
        .filter(|entry| TABLES.iter().any(|table| entry.key.starts_with(table)))
        .collect();
    let oldest = kept.iter().filter_map(|entry| framed_at(&entry.key)).min();
    let span = Indexed {
        from: oldest.unwrap_or(seed.height + 1),
        height: seed.height,
    };
    let rows = kept.len();
    let mut batch = WriteBatch::new();
    for entry in kept {
        batch.put(entry.key, entry.value);
    }
    batch.put(HEIGHT, abi::encode(&span.height));
    batch.put(FROM, abi::encode(&span.from));
    db.write(batch)?;
    tracing::info!(
        target: "ducktape::activity",
        event = "activity_seeded",
        height = span.height,
        from = span.from,
        rows,
        "the activity index opened"
    );
    Ok(span)
}

fn framed_at(key: &[u8]) -> Option<u64> {
    let newest = key.strip_prefix(FRAMES)?.get(..size_of::<u64>())?;
    Some(u64::MAX - u64::from_be_bytes(newest.try_into().ok()?))
}

fn span_at(db: &Db, snapshot: &Snapshot) -> Result<Option<Indexed>> {
    let height = db.get_at(HEIGHT, snapshot)?;
    let from = db.get_at(FROM, snapshot)?;
    let (Some(height), Some(from)) = (height, from) else {
        return Ok(None);
    };
    Ok(Some(Indexed {
        from: decoded(&from)?,
        height: decoded(&height)?,
    }))
}

fn decoded<T: BorshDeserialize>(bytes: &[u8]) -> Result<T> {
    abi::decode(bytes).map_err(|refusal| {
        Error::Corrupt(format!(
            "an activity row does not decode: {}",
            refusal.sentence
        ))
    })
}

impl Reads {
    pub(crate) fn read(&self, query: Query) -> Answer<Reply> {
        let snapshot = self.db.snapshot();
        let Some(span) = span_at(&self.db, &snapshot)? else {
            return Ok(Err(Refusal::new(
                reason::NOT_YET,
                "the activity index is still opening",
            )));
        };
        let listing = Listing {
            db: &self.db,
            snapshot: &snapshot,
            height: span.height,
        };
        match query {
            Query::Acted { account, page } => {
                listing.frames([ACTED, &account.to_be_bytes()].concat(), &page)
            }
            Query::Signed { key, page } => listing.frames([SIGNED, &escaped(&key)].concat(), &page),
            Query::Used { page } => listing.used(&page),
            Query::Frames { page } => listing.frames(FRAMES.to_vec(), &page),
            Query::Indexed => Ok(Ok(Reply::Indexed(span))),
            Query::Refusal { height, position } => listing.refusal(height, position),
        }
    }
}

impl Listing<'_> {
    fn refusal(&self, height: u64, position: u32) -> Answer<Reply> {
        let key = [REFUSED, &newest(height, position)].concat();
        let kept = self.db.get_at(&key, self.snapshot)?;
        let refusal = kept.map(|bytes| decoded(&bytes)).transpose()?;
        Ok(Ok(Reply::Refusal(refusal)))
    }

    fn frames(&self, scope: Vec<u8>, page: &PageRequest) -> Answer<Reply> {
        let rows = match self.rows(scope, page)? {
            Ok(rows) => rows,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let items = rows
            .items
            .iter()
            .map(|(_, value)| decoded(value))
            .collect::<Result<_>>()?;
        Ok(Ok(Reply::Frames(PageResponse {
            height: self.height,
            items,
            next: rows.next,
        })))
    }

    fn used(&self, page: &PageRequest) -> Answer<Reply> {
        let rows = match self.rows(USED.to_vec(), page)? {
            Ok(rows) => rows,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let items = rows
            .items
            .iter()
            .map(|(key, value)| used_of(key, value))
            .collect::<Result<_>>()?;
        Ok(Ok(Reply::Used(PageResponse {
            height: self.height,
            items,
            next: rows.next,
        })))
    }

    fn rows(&self, scope: Vec<u8>, page: &PageRequest) -> Answer<Rows> {
        let after = match resumed(page, &scope) {
            Ok(after) => after,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let lo = match &after {
            Some(after) => [after.as_slice(), &[0]].concat().max(scope.clone()),
            None => scope.clone(),
        };
        let hi = abi::prefix_end(&scope);
        let limit = page.limit.unwrap_or(u64::MAX).max(1);
        let kept = usize::try_from(limit).unwrap_or(usize::MAX);
        let mut items = self
            .db
            .iter_at(Some(&lo), hi.as_deref(), false, self.snapshot)?
            .take(kept.saturating_add(1))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let more = items.len() > kept;
        items.truncate(kept);
        let last = items.last().map(|(key, _)| key.clone());
        let next = last.filter(|_| more).map(|after| {
            abi::encode(&Cursor {
                height: self.height,
                scope,
                after,
            })
        });
        Ok(Ok(Rows { items, next }))
    }
}

fn resumed(page: &PageRequest, scope: &[u8]) -> std::result::Result<Option<Vec<u8>>, Refusal> {
    let Some(bytes) = &page.after else {
        return Ok(None);
    };
    let cursor: Cursor = abi::decode(bytes)
        .map_err(|_| Refusal::new(reason::INVALID_INPUT, "a page cursor does not decode"))?;
    let elsewhere = cursor.scope != scope;
    if elsewhere {
        return Err(Refusal::new(
            reason::STALE,
            "the cursor belongs to another listing",
        ));
    }
    Ok(Some(cursor.after))
}

fn used_of(key: &[u8], value: &[u8]) -> Result<Used> {
    let unread = || {
        Error::Corrupt(format!(
            "an activity count's key does not decode: {}",
            hex::encode(key)
        ))
    };
    let counted = key.strip_prefix(USED).ok_or_else(unread)?;
    let (account, program) = counted
        .split_at_checked(size_of::<AccountNumber>())
        .ok_or_else(unread)?;
    let account = AccountNumber::from_be_bytes(account.try_into().map_err(|_| unread())?);
    let program = unescaped(program)
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or_else(unread)?;
    Ok(Used {
        account,
        program,
        frames: decoded(value)?,
    })
}

#[cfg(test)]
mod tests;
