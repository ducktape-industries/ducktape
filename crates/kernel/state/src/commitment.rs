use std::collections::{BTreeMap, HashMap, HashSet};
use std::num::{NonZeroU16, NonZeroU64, NonZeroUsize};
use std::sync::Arc;
use std::time::{Duration, Instant};

use abi::{Entry, Root};
use commonware_codec::{Decode as _, Encode as _, RangeCfg};
use commonware_cryptography::{Sha256, sha256::Digest};
use commonware_parallel::Sequential;
use commonware_runtime::{Spawner, buffer::paged::CacheRef};
use commonware_storage::{
    Context,
    journal::{self, contiguous::Contiguous as _},
    merkle::{self, Location},
    qmdb::{
        self,
        any::{
            VariableConfig,
            unordered::{
                Update,
                variable::{Db as Qmdb, Operation},
            },
        },
        sync::{self, EngineError, Source as _, SourceFor, Target, engine::Config as SyncConfig},
    },
    translator::EightCap,
};
use commonware_utils::range::NonEmptyRange;
use sha2::Digest as _;

use crate::{Error, Result, Slot, SyncFailure};

pub type Family = merkle::mmr::Family;
pub type Db<E> = Qmdb<Family, E, Digest, Vec<u8>, Sha256, EightCap, Sequential>;
pub type Op = Operation<Family, Digest, Vec<u8>>;
pub type SyncTarget = Target<Family, Digest>;
pub type Config = VariableConfig<EightCap, ((), (RangeCfg<usize>, ())), Sequential>;
pub type Reader<E> = journal::authenticated::Reader<
    Family,
    E,
    journal::contiguous::variable::Reader<'static, E, Op>,
    Sha256,
>;

pub struct Serving<E: Context> {
    pub target: Option<SyncTarget>,
    pub reader: Reader<E>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpProof {
    pub location: u64,
    pub proof: Vec<u8>,
}

impl<E: Context> Serving<E> {
    pub fn size(&self) -> u64 {
        *self.reader.size()
    }

    pub async fn pinned(&self) -> Result<Vec<Digest>> {
        let Some(target) = &self.target else {
            return Ok(Vec::new());
        };
        self.reader
            .pinned_nodes_at(target.range.start())
            .await
            .map_err(qmdb::Error::<Family>::from)
            .map_err(Error::from)
    }

    pub async fn ops(&self, from: u64, max: NonZeroU64) -> Result<Vec<Op>> {
        let (start, end) = self
            .target
            .as_ref()
            .map(|target| (*target.range.start(), *target.range.end()))
            .unwrap_or((0, 0));
        let outside = from < start || from > end;
        if outside {
            return Err(Error::SyncRange {
                from,
                count: max.get(),
                start,
                end,
            });
        }
        let until = from.saturating_add(max.get()).min(end);
        let positions: Vec<_> = (from..until).collect();
        self.reader
            .read_many(&positions)
            .await
            .map_err(qmdb::Error::<Family>::from)
            .map_err(Error::from)
    }

    pub async fn op_proof(&self, size: u64, key: &[u8], value: &[u8]) -> Result<Option<OpProof>> {
        let bounds = self.reader.bounds();
        let held = bounds.start < size && size <= bounds.end;
        if !held {
            return Ok(None);
        }
        let wanted = update_of(key, value);
        let page = Tuning::default().sync_apply_batch.get();
        let mut end = size;
        while end > bounds.start {
            let start = end.saturating_sub(page).max(bounds.start);
            let positions: Vec<u64> = (start..end).collect();
            let ops = self
                .reader
                .read_many(&positions)
                .await
                .map_err(qmdb::Error::<Family>::from)?;
            let found = positions
                .iter()
                .zip(&ops)
                .rev()
                .find(|(_, op)| **op == wanted);
            if let Some((location, _)) = found {
                return self.proven(size, *location).await.map(Some);
            }
            end = start;
        }
        Ok(None)
    }

    async fn proven(&self, size: u64, location: u64) -> Result<OpProof> {
        let request = sync::Request::Operations {
            size: Location::new(size),
            start: Location::new(location),
            max_ops: NonZeroU64::MIN,
        };
        let (response, _) = self.reader.serve(request).await?;
        match response {
            sync::Response::Operations { proof, .. } => Ok(OpProof {
                location,
                proof: proof.encode().to_vec(),
            }),
            sync::Response::Boundary { .. } => Err(Error::Corrupt(
                "the op log answered an operations request with a boundary".into(),
            )),
        }
    }
}

pub fn holds(root: &Root, proven: &OpProof, key: &[u8], value: &[u8]) -> bool {
    let Ok(proof) = merkle::Proof::<Family, Digest>::decode_cfg(
        proven.proof.as_slice(),
        &merkle::MAX_PROOF_DIGESTS_PER_ELEMENT,
    ) else {
        return false;
    };
    let hasher = qmdb::hasher::<Sha256>();
    proof.verify_range_inclusion(
        &hasher,
        &[update_of(key, value).encode()],
        Location::new(proven.location),
        &Digest(root.0),
    )
}

fn update_of(key: &[u8], value: &[u8]) -> Op {
    let leaf = abi::encode(&Entry {
        key: key.to_vec(),
        value: value.to_vec(),
    });
    Op::Update(Update(digest(key), leaf))
}

pub struct Tuning {
    pub items_per_blob: NonZeroU64,
    pub items_per_section: NonZeroU64,
    pub write_buffer: NonZeroUsize,
    pub replay_buffer: NonZeroUsize,
    pub page_size: NonZeroU16,
    pub page_cache_pages: NonZeroUsize,
    pub sync_fetch_batch: NonZeroU64,
    pub sync_apply_batch: NonZeroU64,
    pub sync_retained_roots: usize,
}

impl Default for Tuning {
    fn default() -> Tuning {
        const {
            Tuning {
                items_per_blob: NonZeroU64::new(4096).unwrap(),
                items_per_section: NonZeroU64::new(4096).unwrap(),
                write_buffer: NonZeroUsize::new(1024).unwrap(),
                replay_buffer: NonZeroUsize::new(1 << 20).unwrap(),
                page_size: NonZeroU16::new(128).unwrap(),
                page_cache_pages: NonZeroUsize::new(64).unwrap(),
                sync_fetch_batch: NonZeroU64::new(64).unwrap(),
                sync_apply_batch: NonZeroU64::new(1024).unwrap(),
                sync_retained_roots: 8,
            }
        }
    }
}

pub fn codec_config() -> ((), (RangeCfg<usize>, ())) {
    ((), (RangeCfg::from(..), ()))
}

pub fn partition_prefix(name: &str) -> String {
    format!("commitment-{name}-")
}

pub fn config<E: Context>(context: &E, name: &str, live_keys: usize) -> Config {
    let tuning = Tuning::default();
    let prefix = partition_prefix(name);
    let page_cache = CacheRef::from_pooler(context, tuning.page_size, tuning.page_cache_pages);
    VariableConfig {
        merkle_config: merkle::full::Config {
            journal_partition: format!("{prefix}merkle-journal"),
            metadata_partition: format!("{prefix}merkle-meta"),
            items_per_blob: tuning.items_per_blob,
            write_buffer: tuning.write_buffer,
            replay_buffer: tuning.replay_buffer,
            strategy: Sequential,
            page_cache: page_cache.clone(),
        },
        journal_config: journal::contiguous::variable::Config {
            partition: format!("{prefix}log"),
            items_per_section: tuning.items_per_section,
            write_buffer: tuning.write_buffer,
            replay_buffer: tuning.replay_buffer,
            compression: None,
            codec_config: codec_config(),
            page_cache,
        },
        translator: EightCap,
        init_cache_size: NonZeroUsize::new(live_keys),
        init_buffer: tuning.replay_buffer,
        init_concurrency: (),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Steps {
    pub merkleizing: Duration,
    pub applying: Duration,
    pub committing: Duration,
    pub pruning: Duration,
    pub serving: Duration,
}

impl Steps {
    pub fn named(&self) -> [(&'static str, Duration); 5] {
        [
            ("commitment_merkleize", self.merkleizing),
            ("commitment_apply", self.applying),
            ("commitment_commit", self.committing),
            ("commitment_prune", self.pruning),
            ("commitment_reader", self.serving),
        ]
    }

    pub fn total(&self) -> Duration {
        self.named().into_iter().map(|(_, spent)| spent).sum()
    }
}

/// A program's authenticated state: a qmdb in the runtime's storage, named
/// for its store and program. A sync cut short leaves a log that starts past
/// 0 with no merkle over it. The next sync of the same chain resumes it, one
/// of another chain destroys it and starts over, and `open` cannot open it:
/// a store that owns the name destroys it, then opens it empty.
pub struct Commitment<E>
where
    E: Context + Spawner,
{
    name: String,
    db: Option<Db<E>>,
    serving: Arc<Serving<E>>,
}

impl<E> Commitment<E>
where
    E: Context + Spawner,
{
    /// Reopens the commitment `name` as it was left.
    pub async fn open(context: E, name: &str, live_keys: usize) -> Result<Commitment<E>> {
        let config = config(&context, name, live_keys);
        let db = Db::<E>::init(context, config).await?;
        let (db, serving) = served(db).await?;
        Ok(Commitment {
            name: name.to_owned(),
            db: Some(db),
            serving,
        })
    }

    /// Destroys what the commitment `name` holds, whatever it was left
    /// holding: its op log and merkle open apart, because a log a sync left
    /// short of its target does not replay into a database.
    pub(crate) async fn destroy(context: E, name: &str) -> Result<()> {
        let config = config(&context, name, 0);
        let log = async {
            journal::contiguous::variable::Journal::<E, Op>::init(
                context.child("log"),
                config.journal_config,
            )
            .await?
            .destroy()
            .await
        };
        let merkle = async {
            merkle::full::Merkle::<Family, E, Digest, Sequential>::init(
                context.child("merkle"),
                &qmdb::hasher::<Sha256>(),
                config.merkle_config,
            )
            .await?
            .destroy()
            .await
        };
        // both are tried: a log that will not go leaves no merkle behind
        let log = log.await;
        let merkle = merkle.await;
        log.map_err(qmdb::Error::<Family>::from)?;
        merkle.map_err(qmdb::Error::<Family>::from)?;
        Ok(())
    }

    /// Syncs the commitment `name` to `target` from `source`, resuming what
    /// an earlier sync left. A journal another chain left under `name` is
    /// reused and ends in a root mismatch: it is destroyed and the sync runs
    /// once more from nothing. Any other failure destroys nothing, so a
    /// retry resumes.
    pub async fn sync_from<S>(
        context: E,
        name: &str,
        target: SyncTarget,
        source: S,
    ) -> std::result::Result<Commitment<E>, SyncFailure<S::Error>>
    where
        S: SourceFor<Db<E>>,
    {
        let source = Arc::new(source);
        let heal = context.child("heal");
        let resync = context.child("resync");
        let first = sync_to(context, name, target.clone(), source.clone()).await;
        let outcome = match first {
            Err(sync::Error::Engine(EngineError::RootMismatch { .. })) => {
                Self::destroy(heal, name).await?;
                sync_to(resync, name, target, source).await
            }
            first => first,
        };
        let db = outcome.map_err(failure)?;
        let (db, serving) = served(db).await?;
        Ok(Commitment {
            name: name.to_owned(),
            db: Some(db),
            serving,
        })
    }

    pub async fn receive(
        context: E,
        name: &str,
        target: SyncTarget,
    ) -> Result<crate::Receiving<E>> {
        crate::Receiving::open(context, name, target).await
    }

    pub(crate) async fn received(name: String, db: Db<E>) -> Result<Self> {
        let (db, serving) = served(db).await?;
        Ok(Self {
            name,
            db: Some(db),
            serving,
        })
    }

    pub fn db(&self) -> Result<&Db<E>> {
        self.db
            .as_ref()
            .ok_or_else(|| Error::Lost(self.name.clone()))
    }

    pub fn into_db(self) -> Result<Db<E>> {
        self.db.ok_or(Error::Lost(self.name))
    }

    pub fn root(&self) -> Result<Root> {
        Ok(Root(self.db()?.root().0))
    }

    pub async fn height(&self) -> Result<Option<u64>> {
        let Some(metadata) = self.db()?.get_metadata().await? else {
            return Ok(None);
        };
        abi::decode(&metadata)
            .map(Some)
            .map_err(|refusal| Error::Corrupt(refusal.sentence))
    }

    pub fn target(&self) -> Result<Option<SyncTarget>> {
        Ok(target_of(self.db()?))
    }

    pub fn serving(&self) -> Arc<Serving<E>> {
        self.serving.clone()
    }

    pub async fn apply(&mut self, height: u64, writes: &BTreeMap<Vec<u8>, Slot>) -> Result<Steps> {
        let began = Instant::now();
        let db = self
            .db
            .take()
            .ok_or_else(|| Error::Lost(self.name.clone()))?;
        let mut batch = db.new_batch();
        for (key, slot) in writes {
            let leaf = slot.as_ref().map(|value| {
                abi::encode(&Entry {
                    key: key.clone(),
                    value: value.clone(),
                })
            });
            batch = batch.write(digest(key), leaf);
        }
        let batch = match batch.merkleize(&db, Some(abi::encode(&height))).await {
            Ok(batch) => batch,
            Err(error) => {
                self.db = Some(db);
                return Err(error.into());
            }
        };
        let merkleized = Instant::now();
        let (db, _) = db.apply_batch(batch).await?;
        let applied = Instant::now();
        let db = db.commit().await?;
        let committed = Instant::now();
        let db = self.pruned(db).await?;
        let pruned = Instant::now();
        let (db, serving) = served(db).await?;
        self.serving = serving;
        self.db = Some(db);
        Ok(Steps {
            merkleizing: merkleized.duration_since(began),
            applying: applied.duration_since(merkleized),
            committing: committed.duration_since(applied),
            pruning: pruned.duration_since(committed),
            serving: pruned.elapsed(),
        })
    }

    pub async fn sync(&mut self) -> Result<()> {
        let db = self
            .db
            .take()
            .ok_or_else(|| Error::Lost(self.name.clone()))?;
        self.db = Some(db.sync().await?);
        Ok(())
    }

    async fn pruned(&self, db: Db<E>) -> Result<Db<E>> {
        let floor = db.sync_boundary();
        let from = db.bounds().start;
        let section = Tuning::default().items_per_section.get();
        let drops_a_section = *floor / section > *from / section;
        if !drops_a_section {
            return Ok(db);
        }
        let db = db.prune(floor).await?;
        tracing::debug!(
            target: "ducktape::state",
            event = "commitment_pruned",
            commitment = %self.name,
            floor = *floor,
            from = *from,
            to = *db.bounds().start,
        );
        Ok(db)
    }

    pub async fn entries(&self) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
        Ok(self
            .indexed()
            .await?
            .into_iter()
            .filter_map(|(key, slot)| slot.map(|value| (key, value)))
            .collect())
    }

    pub(crate) async fn indexed(&self) -> Result<BTreeMap<Vec<u8>, Slot>> {
        let from = self.db()?.sync_boundary();
        let (keys, _) = self.replay(from).await?;
        Ok(keys
            .into_iter()
            .filter(|(_, slot)| slot.is_some())
            .collect())
    }

    pub(crate) async fn changes(
        &self,
        from: Location<Family>,
    ) -> Result<Option<(BTreeMap<Vec<u8>, Slot>, HashSet<Digest>)>> {
        let bounds = self.db()?.bounds();
        let held = bounds.start <= from && from <= bounds.end;
        if !held {
            return Ok(None);
        }
        self.replay(from).await.map(Some)
    }

    pub(crate) fn end(&self) -> Result<Location<Family>> {
        Ok(self.db()?.bounds().end)
    }

    async fn replay(
        &self,
        from: Location<Family>,
    ) -> Result<(BTreeMap<Vec<u8>, Slot>, HashSet<Digest>)> {
        let db = self.db()?;
        let bounds = db.bounds();
        let page = Tuning::default().sync_apply_batch.get();
        let mut names: HashMap<Digest, Vec<u8>> = HashMap::new();
        let mut unnamed = HashSet::new();
        let mut keys = BTreeMap::new();
        let mut at = from;
        while at < bounds.end {
            let end = at.checked_add(page).unwrap_or(bounds.end).min(bounds.end);
            let positions: Vec<_> = (*at..*end).collect();
            let ops = self
                .serving
                .reader
                .read_many(&positions)
                .await
                .map_err(qmdb::Error::<Family>::from)?;
            if ops.is_empty() {
                return Err(Error::Corrupt(format!(
                    "the op log of {} is empty at {at} inside its bounds",
                    self.name
                )));
            }
            at = Location::new(*at + ops.len() as u64);
            for op in ops {
                match op {
                    Op::Update(Update(name, leaf)) => {
                        let entry: Entry = abi::decode(&leaf)
                            .map_err(|refusal| Error::Corrupt(refusal.sentence))?;
                        names.insert(name, entry.key.clone());
                        keys.insert(entry.key, Some(entry.value));
                    }
                    Op::Delete(name) => match names.get(&name) {
                        Some(key) => {
                            keys.insert(key.clone(), None);
                        }
                        None => {
                            unnamed.insert(name);
                        }
                    },
                    Op::CommitFloor(_, _) => {}
                }
            }
        }
        Ok((keys, unnamed))
    }
}

fn target_of<E: Context>(db: &Db<E>) -> Option<SyncTarget> {
    let bounds = db.bounds();
    let range = NonEmptyRange::new(db.sync_boundary()..bounds.end).ok()?;
    Some(Target {
        root: db.root(),
        range,
    })
}

async fn served<E: Context + Spawner>(db: Db<E>) -> Result<(Db<E>, Arc<Serving<E>>)> {
    let target = target_of(&db);
    let (db, reader) = db.reader().await?;
    Ok((db, Arc::new(Serving { target, reader })))
}

fn failure<X>(error: sync::Error<Family, X, Digest>) -> SyncFailure<X>
where
    X: std::error::Error + Send + 'static,
{
    match error {
        sync::Error::Source(source) => SyncFailure::Source(source),
        sync::Error::Database(database) => SyncFailure::State(Error::Commitment(database)),
        sync::Error::Engine(engine) => SyncFailure::State(Error::Sync(format!("{engine:?}"))),
    }
}

async fn sync_to<E, S>(
    context: E,
    name: &str,
    target: SyncTarget,
    source: Arc<S>,
) -> std::result::Result<Db<E>, sync::Error<Family, S::Error, Digest>>
where
    E: Context + Spawner,
    S: SourceFor<Db<E>>,
{
    let tuning = Tuning::default();
    let db_config = config(&context, name, 0);
    sync::sync(SyncConfig {
        context,
        source,
        target,
        max_outstanding_requests: 1,
        fetch_batch_size: tuning.sync_fetch_batch,
        apply_batch_size: tuning.sync_apply_batch,
        db_config,
        update_rx: None,
        finish_rx: None,
        reached_target_tx: None,
        max_retained_roots: tuning.sync_retained_roots,
    })
    .await
}

pub fn digest(key: &[u8]) -> Digest {
    Digest(sha2::Sha256::digest(key).into())
}
