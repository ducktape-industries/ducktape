mod commitment;
mod overlay;
mod storage;
mod view;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use abi::{ProgramId, Root};
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_runtime::{Clock, Spawner};
use commonware_storage::Context;

pub use commitment::{Commitment, Db, Family, Op, SyncTarget, Tuning, codec_config, digest};
pub use overlay::{Checkpoint, Overlay, Slot};
pub use storage::{RESERVED_PREFIX, Reads, Storage, reserved, valid_program_id};
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
    Lost(String),
    #[error("the task opening the commitment of {program} ended without opening it: {source}")]
    Unopened {
        program: ProgramId,
        #[source]
        source: commonware_runtime::Error,
    },
    #[error("state is corrupt: {0}")]
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum SyncFailure<X: std::error::Error + 'static> {
    #[error("the sync source failed: {0}")]
    Source(#[source] X),
    #[error(transparent)]
    State(#[from] Error),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Writes {
    pub programs: BTreeMap<ProgramId, BTreeMap<Vec<u8>, Slot>>,
}

impl Writes {
    pub fn is_empty(&self) -> bool {
        self.programs.is_empty()
    }
}

pub fn commitment_name(store: &str, program: &str) -> String {
    format!(
        "{}-{}",
        abi::hex(store.as_bytes()),
        abi::hex(program.as_bytes())
    )
}

#[derive(Default)]
pub struct Adoption {
    adopted: BTreeMap<ProgramId, Adopted>,
    replaced: BTreeSet<ProgramId>,
}

struct Adopted {
    end: commonware_storage::merkle::Location<Family>,
    names: HashMap<commonware_cryptography::sha256::Digest, Vec<u8>>,
}

impl Adoption {
    pub fn replaced(&self) -> &BTreeSet<ProgramId> {
        &self.replaced
    }
}

pub struct Store<E>
where
    E: Context + Spawner,
{
    context: E,
    name: String,
    storage: Storage,
    commitments: BTreeMap<ProgramId, Commitment<E>>,
}

impl<E> Store<E>
where
    E: Context + Spawner,
{
    pub async fn open(
        context: E,
        name: &str,
        storage: Storage,
        programs: impl IntoIterator<Item = ProgramId>,
    ) -> Result<Store<E>> {
        let mut store = Store {
            context,
            name: name.to_owned(),
            storage,
            commitments: BTreeMap::new(),
        };
        // only a founding opens a store with no committed height, and
        // nothing under its name is its own: a failed founding's or join's
        let fresh = store.storage.height()?.is_none();
        store.commitments = store
            .open_commitments(programs.into_iter().collect(), fresh)
            .await?;
        store.reconcile().await?;
        Ok(store)
    }

    async fn open_commitments(
        &self,
        programs: BTreeSet<ProgramId>,
        fresh: bool,
    ) -> Result<BTreeMap<ProgramId, Commitment<E>>> {
        let scans = programs
            .into_iter()
            .map(|program| {
                let keys = self.storage.iter(&program, b"", None, false)?;
                Ok((program, keys))
            })
            .collect::<Result<Vec<_>>>()?;
        let opening: Vec<_> = scans
            .into_iter()
            .map(|(program, keys)| {
                let context = self.context(&program);
                let name = commitment_name(&self.name, &program);
                let id = program.clone();
                let thread = self.context.child("opening").dedicated();
                let task = thread.spawn(move |clock| async move {
                    open_commitment(&clock, context, &id, &name, fresh, keys).await
                });
                (program, task)
            })
            .collect();
        let mut outcomes = Vec::with_capacity(opening.len());
        for (program, task) in opening {
            let outcome = match task.await {
                Ok(outcome) => outcome,
                Err(source) => Err(Error::Unopened {
                    program: program.clone(),
                    source,
                }),
            };
            outcomes.push((program, outcome));
        }
        outcomes
            .into_iter()
            .map(|(program, outcome)| outcome.map(|commitment| (program, commitment)))
            .collect()
    }

    pub async fn adopt(
        context: E,
        name: &str,
        storage: Storage,
        height: u64,
        commitments: BTreeMap<ProgramId, Commitment<E>>,
    ) -> Result<Store<E>> {
        let mut adoption = Adoption::default();
        Store::adopt_from(context, name, storage, height, commitments, &mut adoption).await
    }

    pub async fn adopt_from(
        context: E,
        name: &str,
        storage: Storage,
        height: u64,
        commitments: BTreeMap<ProgramId, Commitment<E>>,
        adoption: &mut Adoption,
    ) -> Result<Store<E>> {
        let first = adoption.adopted.is_empty();
        let mut earlier = std::mem::take(&mut adoption.adopted);
        let mut writes = Writes::default();
        let mut replaced = BTreeSet::new();
        for (program, commitment) in &commitments {
            let changed = match earlier.remove(program) {
                Some(Adopted { end, mut names }) => commitment
                    .changes(end, &mut names)
                    .await?
                    .map(|keys| (keys, names)),
                None => None,
            };
            let (keys, names) = match changed {
                Some(changed) => changed,
                None => {
                    replaced.insert(program.clone());
                    commitment.indexed().await?
                }
            };
            writes.programs.insert(program.clone(), keys);
            let end = commitment.end()?;
            adoption
                .adopted
                .insert(program.clone(), Adopted { end, names });
        }
        replaced.extend(earlier.into_keys());
        if first {
            storage.install(height, &writes)?;
        } else {
            storage.advance(height, &writes, &replaced)?;
        }
        adoption.replaced = replaced;
        Ok(Store {
            context,
            name: name.to_owned(),
            storage,
            commitments,
        })
    }

    /// Adds an admitted program's commitment, empty: what another chain, a
    /// failed join or a dropped program left under its id is none of its.
    pub async fn add_program(&mut self, program: &str) -> Result<()> {
        if self.commitments.contains_key(program) {
            return Ok(());
        }
        let name = commitment_name(&self.name, program);
        let commitment = open_commitment(
            &self.context,
            self.context(program),
            program,
            &name,
            true,
            std::iter::empty(),
        )
        .await?;
        self.commitments.insert(program.to_owned(), commitment);
        Ok(())
    }

    /// Removes a program's commitment and destroys what it holds on disk,
    /// its storage keys with it: a program that no longer runs carries no
    /// root, and one admitted again under the same id starts empty. A
    /// commitment this store did not open is destroyed by name: a node that
    /// died between a block's commit and this call left it there.
    pub async fn remove_program(&mut self, program: &str) -> Result<()> {
        self.storage.clear(program)?;
        match self.commitments.remove(program) {
            Some(commitment) => commitment.into_db()?.destroy().await?,
            None => {
                Commitment::destroy(self.context(program), &commitment_name(&self.name, program))
                    .await?
            }
        }
        Ok(())
    }

    fn context(&self, program: &str) -> E {
        self.context
            .child("program")
            .with_attribute("program", program)
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

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    pub fn clock(&self) -> &E {
        &self.context
    }

    pub fn programs(&self) -> impl Iterator<Item = &ProgramId> {
        self.commitments.keys()
    }

    pub fn commitment(&self, program: &str) -> Option<&Commitment<E>> {
        self.commitments.get(program)
    }

    pub fn commitments(&self) -> impl Iterator<Item = (&ProgramId, &Commitment<E>)> {
        self.commitments.iter()
    }

    pub fn into_parts(self) -> (Storage, BTreeMap<ProgramId, Commitment<E>>) {
        (self.storage, self.commitments)
    }

    pub fn root(&self, program: &str) -> Result<Option<Root>> {
        self.commitments
            .get(program)
            .map(Commitment::root)
            .transpose()
    }

    pub fn roots(&self) -> Result<Vec<(ProgramId, Root)>> {
        self.commitments
            .iter()
            .map(|(program, commitment)| Ok((program.clone(), commitment.root()?)))
            .collect()
    }

    pub fn height(&self) -> Result<Option<u64>> {
        self.storage.height()
    }

    pub fn view<'a>(&'a self, layers: Vec<&'a Overlay>) -> View<'a> {
        View::new(&self.storage, layers)
    }

    pub fn pin(&self) -> Result<Pinned> {
        Ok(Pinned {
            reads: self.storage.pinned(),
            roots: self
                .commitments
                .iter()
                .map(|(program, commitment)| Ok((program.clone(), commitment.root()?)))
                .collect::<Result<_>>()?,
        })
    }

    pub async fn sync(&mut self) -> Result<()> {
        self.storage.flush()?;
        for commitment in self.commitments.values_mut() {
            commitment.sync().await?;
        }
        Ok(())
    }

    pub async fn commit(&mut self, height: u64, writes: Writes) -> Result<()> {
        let unknown = writes
            .programs
            .keys()
            .find(|program| !self.commitments.contains_key(*program));
        if let Some(program) = unknown {
            return Err(Error::Corrupt(format!(
                "a write reached the unknown program {program}"
            )));
        }
        self.storage.commit(height, &writes)?;
        let applies = self
            .commitments
            .iter_mut()
            .filter_map(|(program, commitment)| {
                let keys = writes.programs.get(program)?;
                Some(commitment.apply(height, keys))
            });
        futures::future::try_join_all(applies).await?;
        Ok(())
    }
}

async fn open_commitment<E>(
    clock: &impl Clock,
    context: E,
    program: &str,
    name: &str,
    fresh: bool,
    keys: impl Iterator<Item = Result<(Vec<u8>, Vec<u8>)>>,
) -> Result<Commitment<E>>
where
    E: Context + Spawner,
{
    let started = clock.current();
    let live_keys = keys.map(|entry| entry.map(|_| 1)).sum::<Result<usize>>()?;
    if fresh {
        Commitment::destroy(context.child("fresh"), name).await?;
    }
    let commitment = Commitment::open(context, name, live_keys).await?;
    let elapsed = clock.current().duration_since(started).unwrap_or_default();
    tracing::info!(
        target: "ducktape::state",
        event = "commitment_opened",
        program,
        live_keys,
        elapsed_ms = elapsed.as_millis(),
        "the node opened a program's commitment"
    );
    Ok(commitment)
}

#[derive(Clone)]
pub struct Pinned {
    reads: Reads,
    roots: BTreeMap<ProgramId, Root>,
}

impl Pinned {
    pub fn reads(&self) -> &Reads {
        &self.reads
    }

    pub fn view<'a>(&'a self, layers: Vec<&'a Overlay>) -> View<'a> {
        View::new(&self.reads, layers)
    }

    pub fn height(&self) -> Result<Option<u64>> {
        self.reads.height()
    }

    pub fn root(&self, program: &str) -> Option<Root> {
        self.roots.get(program).copied()
    }

    pub fn roots(&self) -> Vec<(ProgramId, Root)> {
        self.roots
            .iter()
            .map(|(program, root)| (program.clone(), *root))
            .collect()
    }
}

pub enum Committed<'a, E>
where
    E: Context + Spawner,
{
    Live(&'a Store<E>),
    Pinned(&'a Pinned),
}

impl<E> Clone for Committed<'_, E>
where
    E: Context + Spawner,
{
    fn clone(&self) -> Self {
        *self
    }
}

impl<E> Copy for Committed<'_, E> where E: Context + Spawner {}

impl<'a, E> Committed<'a, E>
where
    E: Context + Spawner,
{
    pub fn view(self, layers: Vec<&'a Overlay>) -> View<'a> {
        View::new(self.reads(), layers)
    }

    pub fn reads(self) -> &'a Reads {
        match self {
            Committed::Live(store) => store.storage(),
            Committed::Pinned(pinned) => pinned.reads(),
        }
    }

    pub fn root(self, program: &str) -> Result<Option<Root>> {
        match self {
            Committed::Live(store) => store.root(program),
            Committed::Pinned(pinned) => Ok(pinned.root(program)),
        }
    }

    pub fn roots(self) -> Result<Vec<(ProgramId, Root)>> {
        match self {
            Committed::Live(store) => store.roots(),
            Committed::Pinned(pinned) => Ok(pinned.roots()),
        }
    }
}
