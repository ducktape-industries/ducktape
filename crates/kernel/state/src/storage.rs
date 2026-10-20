use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use abi::ProgramId;

use fluent31::{Db, Options, Snapshot, WriteBatch};

use crate::{Error, Result, Writes};

const SEPARATOR: u8 = 0;
const HEIGHT: &[u8] = b"\xffheight";
const PENDING: &[u8] = b"\xffpending";
pub const RESERVED_PREFIX: char = '$';

pub struct Storage {
    reads: Reads,
}

#[derive(Clone)]
pub struct Reads {
    db: Arc<Db>,
    at: At,
}

#[derive(Clone)]
enum At {
    Latest,
    Pinned(Arc<Snapshot>),
}

impl Reads {
    pub fn pinned(&self) -> Reads {
        let at = match &self.at {
            At::Latest => At::Pinned(Arc::new(self.db.snapshot())),
            At::Pinned(snapshot) => At::Pinned(snapshot.clone()),
        };
        Reads {
            db: self.db.clone(),
            at,
        }
    }

    pub fn height(&self) -> Result<Option<u64>> {
        self.raw(HEIGHT)?.map(|bytes| decode(&bytes)).transpose()
    }

    pub fn pending(&self) -> Result<Option<Writes>> {
        self.raw(PENDING)?.map(|bytes| decode(&bytes)).transpose()
    }

    pub fn get(&self, program: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.raw(&namespaced(program, key))
    }

    fn raw(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let value = match &self.at {
            At::Latest => self.db.get(key)?,
            At::Pinned(snapshot) => self.db.get_at(key, snapshot)?,
        };
        Ok(value)
    }

    pub fn iter(
        &self,
        program: &str,
        lo: &[u8],
        hi: Option<&[u8]>,
        reverse: bool,
    ) -> Result<impl Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + use<>> {
        let lo = namespaced(program, lo);
        let hi = match hi {
            Some(hi) => namespaced(program, hi),
            None => namespace_end(program),
        };
        let strip = program.len() + 1;
        let entries = match &self.at {
            At::Latest => self.db.iter(Some(&lo), Some(&hi), reverse)?,
            At::Pinned(snapshot) => self.db.iter_at(Some(&lo), Some(&hi), reverse, snapshot)?,
        };
        Ok(entries.map(move |entry| {
            let (key, value) = entry?;
            Ok((key[strip..].to_vec(), value))
        }))
    }
}

impl Storage {
    pub fn open(dir: &Path) -> Result<Storage> {
        let options = Options {
            wasm_enabled: false,
            max_key_size: usize::MAX,
            max_value_size: usize::MAX,
            max_txn_write_bytes: usize::MAX,
            ..Options::default()
        };
        Ok(Storage {
            reads: Reads {
                db: Arc::new(Db::open(dir, options)?),
                at: At::Latest,
            },
        })
    }

    fn db(&self) -> &Db {
        &self.reads.db
    }

    pub fn commit(&self, height: u64, writes: &Writes) -> Result<()> {
        let mut batch = WriteBatch::new();
        stage(&mut batch, writes);
        batch.put(HEIGHT, abi::encode(&height));
        batch.put(PENDING, abi::encode(writes));
        Ok(self.db().write(batch)?)
    }

    pub fn flush(&self) -> Result<()> {
        Ok(self.db().flush()?)
    }

    /// Deletes every key under `program`: a dropped program leaves no
    /// storage for one admitted again under its id to read.
    pub fn clear(&self, program: &str) -> Result<()> {
        let mut batch = WriteBatch::new();
        for entry in self.iter(program, b"", None, false)? {
            let (key, _) = entry?;
            batch.delete(namespaced(program, &key));
        }
        Ok(self.db().write(batch)?)
    }

    /// Replaces everything this storage holds with `writes` at `height`: a
    /// join installs a whole state, and one a failed or cut-off join left
    /// here, or another chain's, is no part of it. The old keys are read into
    /// the same batch, so only a retry over leftovers pays for them against
    /// the batch's write limit.
    pub fn install(&self, height: u64, writes: &Writes) -> Result<()> {
        let mut batch = WriteBatch::new();
        for entry in self.db().iter(None, None, false)? {
            let (key, _) = entry?;
            batch.delete(key);
        }
        stage(&mut batch, writes);
        batch.put(HEIGHT, abi::encode(&height));
        batch.delete(PENDING);
        Ok(self.db().write(batch)?)
    }

    pub fn advance(
        &self,
        height: u64,
        writes: &Writes,
        replaced: &BTreeSet<ProgramId>,
    ) -> Result<()> {
        let mut batch = WriteBatch::new();
        for program in replaced {
            for entry in self.iter(program, b"", None, false)? {
                let (key, _) = entry?;
                batch.delete(namespaced(program, &key));
            }
        }
        stage(&mut batch, writes);
        batch.put(HEIGHT, abi::encode(&height));
        batch.delete(PENDING);
        Ok(self.db().write(batch)?)
    }
}

impl std::ops::Deref for Storage {
    type Target = Reads;

    fn deref(&self) -> &Reads {
        &self.reads
    }
}

fn stage(batch: &mut WriteBatch, writes: &Writes) {
    for (program, keys) in &writes.programs {
        for (key, slot) in keys {
            match slot {
                Some(value) => batch.put(namespaced(program, key), value.clone()),
                None => batch.delete(namespaced(program, key)),
            }
        }
    }
}

pub fn namespaced(program: &str, key: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(program.len() + 1 + key.len());
    bytes.extend_from_slice(program.as_bytes());
    bytes.push(SEPARATOR);
    bytes.extend_from_slice(key);
    bytes
}

fn namespace_end(program: &str) -> Vec<u8> {
    let mut bytes = program.as_bytes().to_vec();
    bytes.push(SEPARATOR + 1);
    bytes
}

pub fn reserved(program: &str) -> bool {
    program.starts_with(RESERVED_PREFIX)
}

pub fn valid_program_id(program: &str) -> bool {
    let non_empty = !program.is_empty();
    let no_separator = !program.as_bytes().contains(&SEPARATOR);
    let path_segment = !program.contains('/');
    non_empty && no_separator && path_segment && !reserved(program)
}

fn decode<T: borsh::BorshDeserialize>(bytes: &[u8]) -> Result<T> {
    abi::decode(bytes).map_err(|refusal| Error::Corrupt(refusal.sentence))
}
