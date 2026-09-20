use std::path::Path;

use abi::ProgramId;
use fluent31::{Db, Options, WriteBatch};

use crate::{Error, Result, Writes};

const SEPARATOR: u8 = 0;
const HEIGHT: &[u8] = b"\xffheight";
const PENDING: &[u8] = b"\xffpending";

pub struct Storage {
    db: Db,
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
            db: Db::open(dir, options)?,
        })
    }

    pub fn height(&self) -> Result<Option<u64>> {
        self.db.get(HEIGHT)?.map(|bytes| decode(&bytes)).transpose()
    }

    pub fn pending(&self) -> Result<Option<Writes>> {
        self.db.get(PENDING)?.map(|bytes| decode(&bytes)).transpose()
    }

    pub fn get(&self, program: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.db.get(&namespaced(program, key))?)
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
        Ok(self
            .db
            .iter(Some(&lo), Some(&hi), reverse)?
            .map(move |entry| {
                let (key, value) = entry?;
                Ok((key[strip..].to_vec(), value))
            }))
    }

    pub fn commit(&self, height: u64, writes: &Writes) -> Result<()> {
        let mut batch = WriteBatch::new();
        for (program, keys) in &writes.programs {
            for (key, slot) in keys {
                match slot {
                    Some(value) => batch.put(namespaced(program, key), value.clone()),
                    None => batch.delete(namespaced(program, key)),
                }
            }
        }
        batch.put(HEIGHT, abi::encode(&height));
        batch.put(PENDING, abi::encode(writes));
        Ok(self.db.write(batch)?)
    }

    pub fn install(
        &self,
        height: u64,
        entries: impl IntoIterator<Item = (ProgramId, Vec<u8>, Vec<u8>)>,
    ) -> Result<()> {
        let mut batch = WriteBatch::new();
        for (program, key, value) in entries {
            batch.put(namespaced(&program, &key), value);
        }
        batch.put(HEIGHT, abi::encode(&height));
        batch.delete(PENDING);
        Ok(self.db.write(batch)?)
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

pub fn valid_program_id(program: &str) -> bool {
    let non_empty = !program.is_empty();
    let no_separator = !program.as_bytes().contains(&SEPARATOR);
    let path_segment = !program.contains('/');
    non_empty && no_separator && path_segment
}

fn decode<T: borsh::BorshDeserialize>(bytes: &[u8]) -> Result<T> {
    abi::decode(bytes).map_err(|refusal| Error::Corrupt(refusal.sentence))
}
