// The block history this node keeps of its newest blocks, a window back from its tip, and crosspond's
// epoch-close and epoch-opening records, which the window keeps. The writer writes the blocks; the
// writer and the crosspond relay prune the records. A read drops the store's lock before any I/O.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwapOption;
use fluent31::{Compression, Db, Options, SyncMode, WriteBatch};

use crate::{Error, Result};

pub(crate) const DIR: &str = "history";

const SUMMARY: &[u8] = b"s/";
const ID: &[u8] = b"i/";
const PLACE: &[u8] = b"t/";
const PAYLOAD: &[u8] = b"p/";
const CLOSE: &[u8] = b"c/";
const OPENING: &[u8] = b"o/";

pub(crate) const WINDOW: u64 = 1024;

pub(crate) struct Row {
    pub(crate) height: u64,
    pub(crate) id: [u8; 32],
    pub(crate) summary: Vec<u8>,
    pub(crate) txs: Vec<[u8; 32]>,
    pub(crate) payloads: Vec<Vec<u8>>,
}

#[derive(Clone, Copy)]
pub(crate) enum Durability {
    EachBlock,
    AtClose,
}

#[derive(Clone)]
pub(crate) struct History {
    db: Arc<Db>,
    span: Arc<ArcSwapOption<(u64, u64)>>,
}

impl History {
    pub(crate) fn open(dir: &Path, durability: Durability) -> Result<History> {
        let sync = match durability {
            Durability::EachBlock => SyncMode::Always,
            Durability::AtClose => SyncMode::Never,
        };
        let options = Options {
            sync,
            wasm_enabled: false,
            compression: Compression::Lz4,
            max_key_size: usize::MAX,
            max_value_size: usize::MAX,
            max_txn_write_bytes: usize::MAX,
            ..Options::default()
        };
        let db = Arc::new(Db::open(dir, options)?);
        let span = spanned(&db)?.map(Arc::new);
        Ok(History {
            db,
            span: Arc::new(ArcSwapOption::new(span)),
        })
    }

    pub(crate) fn close(&self) -> Result<()> {
        Ok(self.db.flush()?)
    }

    pub(crate) fn has(&self, height: u64) -> Result<bool> {
        Ok(self.db.get(&summary_key(height))?.is_some())
    }

    pub(crate) fn keep(&self, row: Row) -> Result<()> {
        let Row {
            height,
            id,
            summary,
            txs,
            payloads,
        } = row;
        let mut batch = WriteBatch::new();
        let mut placed = HashSet::new();
        for (position, hash) in txs.iter().enumerate() {
            let key = [PLACE, hash.as_slice()].concat();
            let landed_before = !placed.insert(*hash) || self.db.get(&key)?.is_some();
            if landed_before {
                continue;
            }
            batch.put(key, abi::encode(&(height, position as u32)));
        }
        for (position, payload) in payloads.into_iter().enumerate() {
            batch.put(payload_key(height, position as u32), payload);
        }
        batch.put([ID, id.as_slice()].concat(), height.to_be_bytes());
        batch.put(summary_key(height), summary);
        let below = height.checked_sub(WINDOW + 1);
        let forgot = match below {
            Some(below) => self.forgetting(&mut batch, below)?.then_some(below),
            None => None,
        };
        self.db.write(batch)?;
        let (lowest, newest) = match self.span() {
            Some((lowest, newest)) => (lowest.min(height), newest.max(height)),
            None => (height, height),
        };
        let lowest = forgot.map_or(lowest, |below| lowest.max(below + 1));
        self.span.store(Some(Arc::new((lowest, newest))));
        Ok(())
    }

    pub(crate) fn forget_below(&self, floor: u64) -> Result<u64> {
        let Some((lowest, _)) = self.span() else {
            return Ok(0);
        };
        let mut forgotten = 0;
        for height in lowest..floor {
            let mut batch = WriteBatch::new();
            if self.forgetting(&mut batch, height)? {
                self.db.write(batch)?;
                forgotten += 1;
            }
        }
        if forgotten > 0 {
            self.span.store(spanned(&self.db)?.map(Arc::new));
        }
        Ok(forgotten)
    }

    fn forgetting(&self, batch: &mut WriteBatch, height: u64) -> Result<bool> {
        let Some(summary) = self.db.get(&summary_key(height))? else {
            return Ok(false);
        };
        let landed = crate::blocks::places_of(&summary)?;
        for (hash, position) in landed.places {
            let placed_here = self.place(hash)? == Some((height, position));
            if placed_here {
                batch.delete([PLACE, hash.as_slice()].concat());
            }
            batch.delete(payload_key(height, position));
        }
        batch.delete([ID, landed.id.as_slice()].concat());
        batch.delete(summary_key(height));
        Ok(true)
    }

    pub(crate) fn summary(&self, height: u64) -> Result<Option<Vec<u8>>> {
        Ok(self.db.get(&summary_key(height))?)
    }

    pub(crate) fn summary_by_id(&self, id: [u8; 32]) -> Result<Option<Vec<u8>>> {
        let snapshot = self.db.snapshot();
        let Some(height) = self.db.get_at(&[ID, id.as_slice()].concat(), &snapshot)? else {
            return Ok(None);
        };
        let height = height_of(&height)?;
        Ok(self.db.get_at(&summary_key(height), &snapshot)?)
    }

    pub(crate) fn place(&self, hash: [u8; 32]) -> Result<Option<(u64, u32)>> {
        let Some(bytes) = self.db.get(&[PLACE, hash.as_slice()].concat())? else {
            return Ok(None);
        };
        abi::decode(&bytes).map(Some).map_err(|refusal| {
            Error::Corrupt(format!(
                "a kept transaction place does not decode: {}",
                refusal.sentence
            ))
        })
    }

    pub(crate) fn payload(&self, height: u64, position: u32) -> Result<Option<Vec<u8>>> {
        Ok(self.db.get(&payload_key(height, position))?)
    }

    pub(crate) fn span(&self) -> Option<(u64, u64)> {
        self.span.load().as_deref().copied()
    }

    pub(crate) fn keep_close(&self, epoch: u64, record: Vec<u8>) -> Result<()> {
        Ok(self.db.put(close_key(epoch), record)?)
    }

    pub(crate) fn closes(&self) -> Result<Vec<Vec<u8>>> {
        let end = abi::prefix_end(CLOSE);
        let mut closes = Vec::new();
        for entry in self.db.iter(Some(CLOSE), end.as_deref(), false)? {
            let (_, record) = entry?;
            closes.push(record);
        }
        Ok(closes)
    }

    pub(crate) fn keep_opening(&self, epoch: u64, record: Vec<u8>) -> Result<()> {
        Ok(self.db.put(opening_key(epoch), record)?)
    }

    pub(crate) fn openings(&self) -> Result<Vec<Vec<u8>>> {
        let end = abi::prefix_end(OPENING);
        let mut openings = Vec::new();
        for entry in self.db.iter(Some(OPENING), end.as_deref(), false)? {
            let (_, record) = entry?;
            openings.push(record);
        }
        Ok(openings)
    }

    pub(crate) fn forget_closes_before(&self, epoch: u64) -> Result<()> {
        let mut batch = WriteBatch::new();
        for entry in self.db.iter(Some(CLOSE), Some(&close_key(epoch)), false)? {
            let (key, _) = entry?;
            batch.delete(key);
        }
        for entry in self
            .db
            .iter(Some(OPENING), Some(&opening_key(epoch)), false)?
        {
            let (key, _) = entry?;
            batch.delete(key);
        }
        if batch.is_empty() {
            return Ok(());
        }
        Ok(self.db.write(batch)?)
    }
}

fn close_key(epoch: u64) -> Vec<u8> {
    [CLOSE, &epoch.to_be_bytes()].concat()
}

fn opening_key(epoch: u64) -> Vec<u8> {
    [OPENING, &epoch.to_be_bytes()].concat()
}

fn summary_key(height: u64) -> Vec<u8> {
    [SUMMARY, &height.to_be_bytes()].concat()
}

fn payload_key(height: u64, position: u32) -> Vec<u8> {
    [PAYLOAD, &height.to_be_bytes(), &position.to_be_bytes()].concat()
}

fn height_of(bytes: &[u8]) -> Result<u64> {
    let bytes: [u8; 8] = bytes
        .try_into()
        .map_err(|_| Error::Corrupt(format!("a kept height is {} bytes", bytes.len())))?;
    Ok(u64::from_be_bytes(bytes))
}

fn spanned(db: &Db) -> Result<Option<(u64, u64)>> {
    let end = abi::prefix_end(SUMMARY);
    let first = db.iter(Some(SUMMARY), end.as_deref(), false)?.next();
    let last = db.iter(Some(SUMMARY), end.as_deref(), true)?.next();
    let (Some(first), Some(last)) = (first.transpose()?, last.transpose()?) else {
        return Ok(None);
    };
    let height = |key: &[u8]| height_of(&key[SUMMARY.len()..]);
    Ok(Some((height(&first.0)?, height(&last.0)?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(height: u64, txs: Vec<[u8; 32]>) -> Row {
        let payloads = txs.iter().map(|hash| hash.to_vec()).collect();
        Row {
            height,
            id: [height as u8; 32],
            summary: vec![height as u8; 4],
            txs,
            payloads,
        }
    }

    #[test]
    fn a_kept_block_reads_back_by_height_id_tx_and_payload_and_a_tx_keeps_its_first_landing() {
        let dir = tempfile::tempdir().unwrap();
        let history = History::open(dir.path(), Durability::EachBlock).unwrap();
        assert_eq!(history.span(), None);
        history.keep(row(4, vec![[1; 32], [2; 32]])).unwrap();
        history.keep(row(5, vec![[2; 32], [3; 32]])).unwrap();
        assert_eq!(history.span(), Some((4, 5)));
        assert_eq!(history.summary(5).unwrap(), Some(vec![5; 4]));
        assert_eq!(history.summary_by_id([4; 32]).unwrap(), Some(vec![4; 4]));
        assert_eq!(history.summary_by_id([9; 32]).unwrap(), None);
        assert_eq!(history.place([2; 32]).unwrap(), Some((4, 1)));
        assert_eq!(history.place([3; 32]).unwrap(), Some((5, 1)));
        assert_eq!(history.payload(5, 0).unwrap(), Some(vec![2; 32]));
        assert_eq!(history.payload(5, 2).unwrap(), None);
        assert!(history.has(4).unwrap());
        assert!(!history.has(6).unwrap());
        history.close().unwrap();
        drop(history);
        let reopened = History::open(dir.path(), Durability::EachBlock).unwrap();
        assert_eq!(reopened.span(), Some((4, 5)));
    }

    #[test]
    fn the_writer_keeps_a_block_while_a_reader_holds_a_version_mid_read() {
        let dir = tempfile::tempdir().unwrap();
        let history = History::open(dir.path(), Durability::EachBlock).unwrap();
        history.keep(row(0, vec![[1; 32]])).unwrap();
        history.keep(row(1, vec![[2; 32]])).unwrap();
        let reader = history.clone();
        let snapshot = reader.db.snapshot();
        let mut reading = reader.db.iter_at(None, None, false, &snapshot).unwrap();
        assert!(reading.next().is_some());
        history.keep(row(2, vec![[3; 32]])).unwrap();
        assert_eq!(history.summary(2).unwrap(), Some(vec![2; 4]));
        let unseen = reader.db.get_at(&summary_key(2), &snapshot).unwrap();
        assert_eq!(unseen, None, "a reader's version stays what it read");
        drop(reading);
    }
}
