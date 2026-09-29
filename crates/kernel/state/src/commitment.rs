use std::collections::{BTreeMap, HashMap};
use std::num::{NonZeroU16, NonZeroU64, NonZeroUsize};
use std::sync::Arc;

use abi::{Entry, Root};
use commonware_codec::RangeCfg;
use commonware_cryptography::{Sha256, sha256::Digest};
use commonware_parallel::Sequential;
use commonware_runtime::{Spawner, buffer::paged::CacheRef};
use commonware_storage::{
    Context, journal,
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
        sync::{self, EngineError, SourceFor, Target, engine::Config as SyncConfig},
    },
    translator::TwoCap,
};
use commonware_utils::range::NonEmptyRange;
use sha2::Digest as _;

use crate::{Error, Result, Slot};

pub type Family = merkle::mmr::Family;
pub type Db<E> = Qmdb<Family, E, Digest, Vec<u8>, Sha256, TwoCap, Sequential>;
pub type Op = Operation<Family, Digest, Vec<u8>>;
pub type SyncTarget = Target<Family, Digest>;
pub type Config = VariableConfig<TwoCap, ((), (RangeCfg<usize>, ())), Sequential>;

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
        Tuning {
            items_per_blob: NonZeroU64::new(64).unwrap(),
            items_per_section: NonZeroU64::new(64).unwrap(),
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

pub fn codec_config() -> ((), (RangeCfg<usize>, ())) {
    ((), (RangeCfg::from(..), ()))
}

pub fn config<E: Context>(context: &E, name: &str) -> Config {
    let tuning = Tuning::default();
    let page_cache = CacheRef::from_pooler(context, tuning.page_size, tuning.page_cache_pages);
    VariableConfig {
        merkle_config: merkle::full::Config {
            journal_partition: format!("commitment-{name}-merkle-journal"),
            metadata_partition: format!("commitment-{name}-merkle-meta"),
            items_per_blob: tuning.items_per_blob,
            write_buffer: tuning.write_buffer,
            replay_buffer: tuning.replay_buffer,
            strategy: Sequential,
            page_cache: page_cache.clone(),
        },
        journal_config: journal::contiguous::variable::Config {
            partition: format!("commitment-{name}-log"),
            items_per_section: tuning.items_per_section,
            write_buffer: tuning.write_buffer,
            replay_buffer: tuning.replay_buffer,
            compression: None,
            codec_config: codec_config(),
            page_cache,
        },
        translator: TwoCap,
        init_cache_size: None,
        init_buffer: tuning.replay_buffer,
        init_concurrency: (),
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
}

impl<E> Commitment<E>
where
    E: Context + Spawner,
{
    /// Reopens the commitment `name` as it was left.
    pub async fn open(context: E, name: &str) -> Result<Commitment<E>> {
        let config = config(&context, name);
        let db = Db::<E>::init(context, config).await?;
        Ok(Commitment {
            name: name.to_owned(),
            db: Some(db),
        })
    }

    /// Destroys what the commitment `name` holds, whatever it was left
    /// holding: its op log and merkle open apart, because a log a sync left
    /// short of its target does not replay into a database.
    pub(crate) async fn destroy(context: E, name: &str) -> Result<()> {
        let config = config(&context, name);
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
    ) -> Result<Commitment<E>>
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
        let db = outcome.map_err(|e| Error::Sync(format!("{e:?}")))?;
        Ok(Commitment {
            name: name.to_owned(),
            db: Some(db),
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
        let db = self.db()?;
        let bounds = db.bounds();
        let Ok(range) = NonEmptyRange::new(db.sync_boundary()..bounds.end) else {
            return Ok(None);
        };
        Ok(Some(Target {
            root: db.root(),
            range,
        }))
    }

    pub async fn apply(&mut self, height: u64, writes: &BTreeMap<Vec<u8>, Slot>) -> Result<()> {
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
        let (db, _) = db.apply_batch(batch).await?;
        self.db = Some(db.commit().await?);
        Ok(())
    }

    pub async fn entries(&self) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
        let db = self.db()?;
        let bounds = db.bounds();
        let page = Tuning::default().sync_apply_batch;
        let mut names: HashMap<Digest, Vec<u8>> = HashMap::new();
        let mut live = BTreeMap::new();
        let mut at = bounds.start;
        while at < bounds.end {
            let (_, ops) = db.proof(at, page).await?;
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
                        live.insert(entry.key, entry.value);
                    }
                    Op::Delete(name) => {
                        if let Some(key) = names.get(&name) {
                            live.remove(key);
                        }
                    }
                    Op::CommitFloor(_, _) => {}
                }
            }
        }
        Ok(live)
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
    let db_config = config(&context, name);
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
