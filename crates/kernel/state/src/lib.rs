mod commitment;
mod overlay;
mod storage;
mod view;

use std::collections::BTreeMap;
use std::path::Path;

use abi::{ProgramId, Root};
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_runtime::Spawner;
use commonware_storage::Context;

pub use commitment::{Commitment, Db, Family, Op, SyncTarget, codec_config, digest};
pub use overlay::{Checkpoint, Overlay, Slot};
pub use storage::{Storage, valid_program_id};
pub use view::View;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("state storage: {0}")]
    Storage(#[from] fluent31::Error),
    #[error("state commitment: {0}")]
    Commitment(#[from] commonware_storage::qmdb::Error<Family>),
    #[error("state sync: {0}")]
    Sync(String),
    #[error("the commitment of {0} was lost to a failed apply")]
    Lost(ProgramId),
    #[error("state is corrupt: {0}")]
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Writes {
    pub programs: BTreeMap<ProgramId, BTreeMap<Vec<u8>, Slot>>,
}

impl Writes {
    pub fn is_empty(&self) -> bool {
        self.programs.is_empty()
    }
}

pub struct Store<E>
where
    E: Context + Spawner,
{
    context: E,
    storage: Storage,
    commitments: BTreeMap<ProgramId, Commitment<E>>,
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
        let mut store = Store {
            context,
            storage,
            commitments: BTreeMap::new(),
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
            .child("commitment")
            .with_attribute("program", program);
        let commitment = Commitment::open(context, program).await?;
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

    pub fn take_commitment(&mut self, program: &str) -> Option<Commitment<E>> {
        self.commitments.remove(program)
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
        Ok(())
    }

    pub async fn adopt(
        &mut self,
        height: u64,
        commitments: BTreeMap<ProgramId, Commitment<E>>,
    ) -> Result<()> {
        let mut entries = Vec::new();
        for (program, commitment) in &commitments {
            for (key, value) in commitment.entries().await? {
                entries.push((program.clone(), key, value));
            }
        }
        self.storage.install(height, entries)?;
        self.commitments = commitments;
        Ok(())
    }
}
