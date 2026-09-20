mod commitment;
mod overlay;
mod storage;
mod view;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use abi::{BlobId, ProgramId, Root};
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_runtime::Spawner;
use commonware_storage::Context;

pub use commitment::{Commitment, Db, Family, Op, SyncTarget, codec_config, digest};
pub use overlay::{Checkpoint, Overlay, Slot};
pub use storage::{Storage, valid_program_id};
pub use view::View;

pub const ROSTER: &str = "roster";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("state storage: {0}")]
    Storage(#[from] fluent31::Error),
    #[error("state commitment: {0}")]
    Commitment(#[from] commonware_storage::qmdb::Error<Family>),
    #[error("state sync: {0}")]
    Sync(String),
    #[error("the commitment of {0} was lost to a failed apply")]
    Lost(String),
    #[error("state is corrupt: {0}")]
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Writes {
    pub programs: BTreeMap<ProgramId, BTreeMap<Vec<u8>, Slot>>,
    pub blobs: BTreeSet<BlobId>,
}

impl Writes {
    pub fn is_empty(&self) -> bool {
        self.programs.is_empty() && self.blobs.is_empty()
    }
}

pub fn program_commitment(program: &str) -> String {
    format!("program-{program}")
}

pub struct Store<E>
where
    E: Context + Spawner,
{
    context: E,
    storage: Storage,
    commitments: BTreeMap<ProgramId, Commitment<E>>,
    roster: Commitment<E>,
}

impl<E> Store<E>
where
    E: Context + Spawner,
{
    pub async fn open(
        context: E,
        dir: &Path,
        programs: impl IntoIterator<Item = ProgramId>,
    ) -> Result<Store<E>> {
        let storage = Storage::open(dir)?;
        let roster = Commitment::open(context.child(ROSTER), ROSTER).await?;
        let mut store = Store {
            context,
            storage,
            commitments: BTreeMap::new(),
            roster,
        };
        for program in programs {
            store.add_program(&program).await?;
        }
        store.reconcile().await?;
        Ok(store)
    }

    pub async fn add_program(&mut self, program: &str) -> Result<()> {
        if self.commitments.contains_key(program) {
            return Ok(());
        }
        let context = self
            .context
            .child("program")
            .with_attribute("program", program);
        let commitment = Commitment::open(context, &program_commitment(program)).await?;
        self.commitments.insert(program.to_owned(), commitment);
        Ok(())
    }

    async fn reconcile(&mut self) -> Result<()> {
        let Some(height) = self.storage.height()? else {
            return Ok(());
        };
        let Some(pending) = self.storage.pending()? else {
            return Ok(());
        };
        for (program, writes) in &pending.programs {
            let Some(commitment) = self.commitments.get_mut(program) else {
                continue;
            };
            let committed = commitment.height().await?;
            let behind = committed.is_none_or(|committed| committed < height);
            if behind {
                commitment.apply(height, writes).await?;
            }
        }
        let roster_behind = self
            .roster
            .height()
            .await?
            .is_none_or(|committed| committed < height);
        let roster_has_writes = !pending.blobs.is_empty();
        if roster_behind && roster_has_writes {
            self.roster.apply(height, &roster_writes(&pending.blobs)).await?;
        }
        Ok(())
    }

    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    pub fn programs(&self) -> impl Iterator<Item = &ProgramId> {
        self.commitments.keys()
    }

    pub fn commitment(&self, program: &str) -> Option<&Commitment<E>> {
        self.commitments.get(program)
    }

    pub fn into_parts(self) -> (BTreeMap<ProgramId, Commitment<E>>, Commitment<E>) {
        (self.commitments, self.roster)
    }

    pub fn roster(&self) -> &Commitment<E> {
        &self.roster
    }

    pub fn root(&self, program: &str) -> Result<Option<Root>> {
        self.commitments
            .get(program)
            .map(Commitment::root)
            .transpose()
    }

    pub fn height(&self) -> Result<Option<u64>> {
        self.storage.height()
    }

    pub fn has_blob(&self, id: &BlobId) -> Result<bool> {
        self.storage.has_blob(id)
    }

    pub fn blob_ids(&self) -> Result<BTreeSet<BlobId>> {
        self.storage.blob_ids()
    }

    pub fn view<'a>(&'a self, layers: Vec<&'a Overlay>) -> View<'a> {
        View::new(&self.storage, layers)
    }

    pub async fn commit(&mut self, height: u64, writes: Writes) -> Result<()> {
        self.storage.commit(height, &writes)?;
        for (program, keys) in &writes.programs {
            let commitment = self.commitments.get_mut(program).ok_or_else(|| {
                Error::Corrupt(format!("a write reached the unknown program {program}"))
            })?;
            commitment.apply(height, keys).await?;
        }
        if !writes.blobs.is_empty() {
            self.roster
                .apply(height, &roster_writes(&writes.blobs))
                .await?;
        }
        Ok(())
    }

    pub async fn adopt(
        context: E,
        dir: &Path,
        height: u64,
        commitments: BTreeMap<ProgramId, Commitment<E>>,
        roster: Commitment<E>,
    ) -> Result<Store<E>> {
        let storage = Storage::open(dir)?;
        let mut writes = Writes::default();
        for (program, commitment) in &commitments {
            let keys = commitment
                .entries()
                .await?
                .into_iter()
                .map(|(key, value)| (key, Some(value)))
                .collect();
            writes.programs.insert(program.clone(), keys);
        }
        for (key, _) in roster.entries().await? {
            let id = abi::decode(&key).map_err(|refusal| Error::Corrupt(refusal.sentence))?;
            writes.blobs.insert(id);
        }
        storage.install(height, &writes)?;
        Ok(Store {
            context,
            storage,
            commitments,
            roster,
        })
    }
}

fn roster_writes(blobs: &BTreeSet<BlobId>) -> BTreeMap<Vec<u8>, Slot> {
    blobs
        .iter()
        .map(|id| (abi::encode(id), Some(Vec::new())))
        .collect()
}
