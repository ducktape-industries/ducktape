use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use abi::{BlobId, ProgramId};

use crate::Writes;

pub type Slot = Option<Vec<u8>>;
pub type Slotted<'a> = (&'a [u8], Option<&'a [u8]>);

#[derive(Default)]
pub struct Overlay {
    programs: BTreeMap<ProgramId, BTreeMap<Vec<u8>, Slot>>,
    blobs: BTreeSet<BlobId>,
    undo: Vec<Undo>,
}

enum Undo {
    Key {
        program: ProgramId,
        key: Vec<u8>,
        before: Option<Slot>,
    },
    Blob(BlobId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checkpoint(usize);

impl Overlay {
    pub fn is_empty(&self) -> bool {
        self.programs.values().all(BTreeMap::is_empty) && self.blobs.is_empty()
    }

    pub fn get(&self, program: &str, key: &[u8]) -> Option<Option<&[u8]>> {
        self.programs
            .get(program)
            .and_then(|writes| writes.get(key))
            .map(|slot| slot.as_deref())
    }

    pub fn set(&mut self, program: &str, key: Vec<u8>, value: Vec<u8>) {
        self.write(program, key, Some(value));
    }

    pub fn delete(&mut self, program: &str, key: Vec<u8>) {
        self.write(program, key, None);
    }

    fn write(&mut self, program: &str, key: Vec<u8>, slot: Slot) {
        let writes = self.programs.entry(program.to_owned()).or_default();
        let before = writes.insert(key.clone(), slot);
        self.undo.push(Undo::Key {
            program: program.to_owned(),
            key,
            before,
        });
    }

    pub fn put_blob(&mut self, id: BlobId) {
        let newly_staged = self.blobs.insert(id);
        if newly_staged {
            self.undo.push(Undo::Blob(id));
        }
    }

    pub fn has_blob(&self, id: &BlobId) -> bool {
        self.blobs.contains(id)
    }

    pub fn blobs(&self) -> impl Iterator<Item = &BlobId> {
        self.blobs.iter()
    }

    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint(self.undo.len())
    }

    pub fn restore(&mut self, checkpoint: Checkpoint) {
        while self.undo.len() > checkpoint.0 {
            let Some(undo) = self.undo.pop() else { return };
            match undo {
                Undo::Key {
                    program,
                    key,
                    before,
                } => {
                    let Some(writes) = self.programs.get_mut(&program) else { continue };
                    match before {
                        Some(slot) => writes.insert(key, slot),
                        None => writes.remove(&key),
                    };
                }
                Undo::Blob(id) => {
                    self.blobs.remove(&id);
                }
            }
        }
    }

    pub fn range<'a>(
        &'a self,
        program: &str,
        lo: &[u8],
        hi: Option<&[u8]>,
        reverse: bool,
    ) -> Box<dyn Iterator<Item = Slotted<'a>> + 'a> {
        let Some(writes) = self.programs.get(program) else {
            return Box::new(std::iter::empty());
        };
        let hi = match hi {
            Some(hi) => Bound::Excluded(hi.to_vec()),
            None => Bound::Unbounded,
        };
        let entries = writes
            .range::<[u8], _>((Bound::Included(lo), hi.as_ref().map(Vec::as_slice)))
            .map(|(key, slot)| (key.as_slice(), slot.as_deref()));
        if reverse {
            return Box::new(entries.rev());
        }
        Box::new(entries)
    }

    pub fn into_writes(self) -> Writes {
        Writes {
            programs: self
                .programs
                .into_iter()
                .filter(|(_, writes)| !writes.is_empty())
                .collect(),
            blobs: self.blobs,
        }
    }
}
