mod crypto;
mod namespace;
mod queue;
mod unit;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use abi::{
    BlobId, Cause, Env, GuestCall, HashKind, Invocation, ItemRef, Origin, Outcome, ProgramId,
    Refusal, Root, Scan, module_registry, reason, valset,
};
use blobs::{Blobs, Layered, Stage};
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_runtime::Spawner;
use commonware_storage::Context;
use runtime::Fault;
use sha2::{Digest as _, Sha256};
use state::{Commitment, Overlay, Storage, Store, View, Writes, valid_program_id};

use crate::unit::{Loaded, World, refusal_of};

pub use namespace::{BLOBS, NETWORK, PROGRAMS, QUEUE, RESERVED, SIGNERS};
pub use queue::Item;
pub use runtime::Limits;

const STATE_DIR: &str = "state";
const BLOBS_DIR: &str = "blobs";
const CODE_KIND: &str = "program";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    State(#[from] state::Error),
    #[error(transparent)]
    Blobs(#[from] blobs::Error),
    #[error("blob {0:?} is recorded on chain but not on this node")]
    BlobUnavailable(BlobId),
    #[error("{program} runs {code:?}, which this node cannot load: {fault}")]
    Load {
        program: ProgramId,
        code: BlobId,
        fault: Fault,
    },
    #[error("block {got} is out of sequence; the next block is {expected}")]
    Height { expected: u64, got: u64 },
    #[error("founding program {program} was not admitted: {refusal}")]
    Genesis {
        program: ProgramId,
        refusal: Refusal,
    },
    #[error("no network was founded here")]
    Unfounded,
    #[error("host state is corrupt: {0}")]
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub type BlockId = [u8; 32];

pub struct Genesis {
    pub network: Vec<u8>,
    pub module_registry: Vec<u8>,
    pub valset: Vec<u8>,
    pub validators: Vec<valset::Member>,
    pub programs: Vec<Founding>,
    /// Views with no program behind them: each blob is stored and listed by
    /// the registry under its name; nothing is admitted.
    pub views: Vec<FoundingView>,
    pub limits: Limits,
    pub epoch_length: u64,
    pub time: u64,
}

pub struct Founding {
    pub program: ProgramId,
    pub code: Vec<u8>,
    pub params: Vec<u8>,
}

pub struct FoundingView {
    pub name: ProgramId,
    pub view: Vec<u8>,
}

pub struct Block {
    pub height: u64,
    pub id: BlockId,
    pub time: u64,
    pub submissions: Vec<Submission>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tip {
    pub height: u64,
    pub id: BlockId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submission {
    pub signer: Vec<u8>,
    pub seq: u64,
    pub target: ProgramId,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Receipt {
    pub program: ProgramId,
    pub outcome: Outcome,
    pub events: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivered {
    pub item: u64,
    pub receipt: Receipt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    pub height: u64,
    pub admissions: Vec<Receipt>,
    pub deliveries: Vec<Delivered>,
    pub submissions: Vec<Receipt>,
    pub writes: Writes,
    pub root: Root,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Layer {
    Confirmed,
    Preconfirmed,
}

pub struct Host<E>
where
    E: Context + Spawner,
{
    store: Store<E>,
    blobs: Blobs,
    network: Vec<u8>,
    loaded: Loaded,
    preconfirmed: Overlay,
}

impl<E> Host<E>
where
    E: Context + Spawner,
{
    pub async fn found(
        context: E,
        name: &str,
        dir: &Path,
        block: BlockId,
        genesis: Genesis,
    ) -> Result<(Host<E>, Applied)> {
        let storage = Storage::open(&dir.join(STATE_DIR))?;
        let store = Store::open(context, name, storage, RESERVED.map(str::to_owned)).await?;
        let blobs = Blobs::open(&dir.join(BLOBS_DIR))?;
        let mut host = Host {
            store,
            blobs,
            network: genesis.network.clone(),
            loaded: Loaded::new(genesis.limits),
            preconfirmed: Overlay::default(),
        };
        let mut overlay = Overlay::default();
        let mut stage = Stage::default();
        overlay.set(NETWORK, namespace::ID.to_vec(), genesis.network);
        overlay.set(
            NETWORK,
            namespace::LIMITS.to_vec(),
            abi::encode(&genesis.limits),
        );
        overlay.set(
            NETWORK,
            namespace::EPOCH_LENGTH.to_vec(),
            abi::encode(&genesis.epoch_length),
        );
        let mut entries = vec![
            module_registry::Entry {
                program: module_registry::PROGRAM.to_owned(),
                code: put_code(&mut overlay, &mut stage, &genesis.module_registry),
                params: Vec::new(),
            },
            module_registry::Entry {
                program: valset::PROGRAM.to_owned(),
                code: put_code(&mut overlay, &mut stage, &genesis.valset),
                params: abi::encode(&valset::Genesis {
                    validators: genesis.validators,
                }),
            },
        ];
        for founding in genesis.programs {
            entries.push(module_registry::Entry {
                program: founding.program,
                code: put_code(&mut overlay, &mut stage, &founding.code),
                params: founding.params,
            });
        }
        let views: Vec<module_registry::View> = genesis
            .views
            .into_iter()
            .map(|founding| module_registry::View {
                name: founding.name,
                view: put_code(&mut overlay, &mut stage, &founding.view),
            })
            .collect();
        let mut seen = BTreeSet::new();
        let names = entries
            .iter()
            .map(|entry| &entry.program)
            .chain(views.iter().map(|view| &view.name));
        for name in names {
            let admissible = valid_program_id(name) && seen.insert(name.clone());
            if !admissible {
                return Err(Error::Genesis {
                    program: name.clone(),
                    refusal: Refusal::new(reason::INVALID_INPUT, "not a distinct program id"),
                });
            }
        }
        entries[0].params = abi::encode(&module_registry::Genesis {
            programs: entries.clone(),
            views,
        });
        let mut receipts = Vec::new();
        for entry in &entries {
            let receipt = host
                .admit(entry, 0, genesis.time, &mut overlay, &mut stage)
                .await?;
            if let Outcome::Rejected(refusal) = &receipt.outcome {
                return Err(Error::Genesis {
                    program: entry.program.clone(),
                    refusal: refusal.clone(),
                });
            }
            receipts.push(receipt);
        }
        host.record_epoch(0, 0, genesis.time, &mut overlay, &stage)
            .await?;
        let applied = host
            .commit(
                Tip {
                    height: 0,
                    id: block,
                },
                overlay,
                stage,
                receipts,
                Vec::new(),
                Vec::new(),
            )
            .await?;
        Ok((host, applied))
    }

    pub async fn open(context: E, name: &str, dir: &Path) -> Result<Host<E>> {
        let storage = Storage::open(&dir.join(STATE_DIR))?;
        let programs = programs_in(&storage)?;
        let store = Store::open(context, name, storage, programs).await?;
        let blobs = Blobs::open(&dir.join(BLOBS_DIR))?;
        Host::assemble(store, blobs)
    }

    pub async fn adopt(
        context: E,
        name: &str,
        dir: &Path,
        height: u64,
        commitments: BTreeMap<ProgramId, Commitment<E>>,
    ) -> Result<Host<E>> {
        let storage = Storage::open(&dir.join(STATE_DIR))?;
        let store = Store::adopt(context, name, storage, height, commitments).await?;
        let blobs = Blobs::open(&dir.join(BLOBS_DIR))?;
        Host::assemble(store, blobs)
    }

    fn assemble(store: Store<E>, blobs: Blobs) -> Result<Host<E>> {
        let founded = store.height()?.is_some();
        if !founded {
            return Err(Error::Unfounded);
        }
        let limits = limits_of(&store.view(Vec::new()))?;
        let network = store
            .view(Vec::new())
            .get(NETWORK, namespace::ID)?
            .ok_or_else(|| Error::Corrupt("the network records no id".into()))?;
        let mut host = Host {
            store,
            blobs,
            network,
            loaded: Loaded::new(limits),
            preconfirmed: Overlay::default(),
        };
        host.sync_code()?;
        Ok(host)
    }

    fn sync_code(&mut self) -> Result<()> {
        let view = self.store.view(Vec::new());
        let limits = limits_of(&view)?;
        let limits_changed = self.loaded.limits() != limits;
        if limits_changed {
            self.loaded = Loaded::new(limits);
        }
        let wanted = programs_of(&view)?;
        self.loaded.sync(&wanted, &self.blobs)
    }

    fn ready(&mut self) -> Result<()> {
        self.sync_code()?;
        match self.loaded.missing().first() {
            Some((_, code)) => Err(Error::BlobUnavailable(*code)),
            None => Ok(()),
        }
    }

    pub fn store(&self) -> &Store<E> {
        &self.store
    }

    pub fn into_store(self) -> Store<E> {
        self.store
    }

    pub fn network(&self) -> &[u8] {
        &self.network
    }

    pub fn height(&self) -> Result<u64> {
        self.store.height()?.ok_or(Error::Unfounded)
    }

    fn next_height(&self) -> Result<u64> {
        Ok(self.store.height()?.map_or(0, |height| height + 1))
    }

    pub fn tip(&self) -> Result<Tip> {
        let height = self.height()?;
        let bytes = self
            .store
            .view(Vec::new())
            .get(NETWORK, namespace::TIP)?
            .ok_or_else(|| Error::Corrupt("the network records no tip".into()))?;
        let id = abi::decode(&bytes).map_err(corrupt)?;
        Ok(Tip { height, id })
    }

    pub fn epoch_length(&self) -> Result<u64> {
        let bytes = self
            .store
            .view(Vec::new())
            .get(NETWORK, namespace::EPOCH_LENGTH)?
            .ok_or_else(|| Error::Corrupt("the network records no epoch length".into()))?;
        abi::decode(&bytes).map_err(corrupt)
    }

    pub fn epoch_members(&self, epoch: u64) -> Result<Option<Vec<valset::Member>>> {
        self.store
            .view(Vec::new())
            .get(NETWORK, &namespace::epoch(epoch))?
            .map(|bytes| abi::decode(&bytes).map_err(corrupt))
            .transpose()
    }

    pub fn root(&self) -> Result<Root> {
        let roots = self.store.roots()?;
        Ok(Root(Sha256::digest(abi::encode(&roots)).into()))
    }

    pub fn view(&self, layer: Layer) -> View<'_> {
        self.store.view(self.layers(layer))
    }

    fn layers(&self, layer: Layer) -> Vec<&Overlay> {
        match layer {
            Layer::Confirmed => Vec::new(),
            Layer::Preconfirmed => vec![&self.preconfirmed],
        }
    }

    fn height_of(&self, layer: Layer) -> Result<u64> {
        match layer {
            Layer::Confirmed => self.height(),
            Layer::Preconfirmed => self.next_height(),
        }
    }

    pub fn programs(&self) -> Result<BTreeMap<ProgramId, BlobId>> {
        programs_of(&self.store.view(Vec::new()))
    }

    pub fn missing_blobs(&self) -> Result<Vec<BlobId>> {
        let rostered = self
            .store
            .view(Vec::new())
            .scan(BLOBS, &Scan::prefix(b""))?;
        let mut missing = Vec::new();
        for entry in rostered {
            let id: BlobId = abi::decode(&entry.key).map_err(corrupt)?;
            if !self.blobs.has(&id) {
                missing.push(id);
            }
        }
        Ok(missing)
    }

    pub fn install(&mut self, id: BlobId, framed: &[u8]) -> Result<()> {
        let hashes_to_id = blobs::id_of(id.kind(), framed) == id;
        if !hashes_to_id {
            return Err(Error::Corrupt(format!(
                "the bytes offered for {id:?} do not hash to it"
            )));
        }
        self.blobs.write(&id, framed)?;
        self.sync_code()
    }

    pub fn blob(&self, id: &BlobId) -> Result<Option<Vec<u8>>> {
        let rostered = self
            .store
            .view(Vec::new())
            .get(BLOBS, &namespace::blob(id))?
            .is_some();
        if !rostered {
            return Ok(None);
        }
        self.blobs
            .framed(id)?
            .map(Some)
            .ok_or(Error::BlobUnavailable(*id))
    }

    fn world(&self, height: u64, time: u64) -> World<'_, E> {
        World {
            store: &self.store,
            blobs: &self.blobs,
            loaded: &self.loaded,
            network: &self.network,
            height,
            time,
        }
    }

    pub async fn query(
        &self,
        layer: Layer,
        time: u64,
        origin: Origin,
        program: &str,
        request: Vec<u8>,
    ) -> Result<std::result::Result<Vec<u8>, Refusal>> {
        if let Some(code) = self.loaded.awaiting(program) {
            return Err(Error::BlobUnavailable(code));
        }
        let height = self.height_of(layer)?;
        let stage = Stage::default();
        unit::query(
            self.world(height, time),
            self.layers(layer),
            &stage,
            &[],
            origin,
            program.to_owned(),
            request,
        )
        .await
    }

    async fn record_epoch(
        &self,
        epoch: u64,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &Stage,
    ) -> Result<()> {
        let reply = unit::query(
            self.world(height, time),
            vec![&*overlay],
            stage,
            &[],
            Origin::System,
            valset::PROGRAM.to_owned(),
            abi::encode(&valset::Query::Members),
        )
        .await?;
        let bytes = reply.map_err(|refusal| {
            Error::Corrupt(format!("valset refused the members query: {refusal}"))
        })?;
        let valset::Reply::Members(members) = abi::decode(&bytes).map_err(corrupt)? else {
            return Err(Error::Corrupt(
                "valset answered Members with another reply".into(),
            ));
        };
        overlay.set(NETWORK, namespace::epoch(epoch), abi::encode(&members));
        Ok(())
    }

    pub fn deliveries_due(&self) -> Result<bool> {
        Ok(!queue::pending(&self.store.view(Vec::new()))?.is_empty())
    }

    pub async fn preconfirm(
        &mut self,
        time: u64,
        submissions: Vec<Submission>,
    ) -> Result<Vec<Receipt>> {
        self.ready()?;
        let height = self.next_height()?;
        let mut overlay = std::mem::take(&mut self.preconfirmed);
        let mut stage = Stage::default();
        let mut receipts = Vec::new();
        for submission in submissions {
            let receipt = self
                .submit(submission, height, time, &mut overlay, &mut stage)
                .await?;
            receipts.push(receipt);
        }
        self.preconfirmed = overlay;
        Ok(receipts)
    }

    pub async fn apply(&mut self, block: Block) -> Result<Applied> {
        let expected = self.next_height()?;
        let in_sequence = block.height == expected;
        if !in_sequence {
            return Err(Error::Height {
                expected,
                got: block.height,
            });
        }
        self.ready()?;
        let mut overlay = Overlay::default();
        let mut stage = Stage::default();
        let admissions = self
            .refresh_programs(block.height, block.time, &mut overlay, &mut stage)
            .await?;
        let deliveries = self
            .deliver(block.height, block.time, &mut overlay, &mut stage)
            .await?;
        let mut submissions = Vec::new();
        for submission in block.submissions {
            let receipt = self
                .submit(
                    submission,
                    block.height,
                    block.time,
                    &mut overlay,
                    &mut stage,
                )
                .await?;
            submissions.push(receipt);
        }
        let epoch_length = self.epoch_length()?;
        let ends_an_epoch = (block.height + 1).is_multiple_of(epoch_length);
        if ends_an_epoch {
            let next_epoch = (block.height + 1) / epoch_length;
            self.record_epoch(next_epoch, block.height, block.time, &mut overlay, &stage)
                .await?;
        }
        self.commit(
            Tip {
                height: block.height,
                id: block.id,
            },
            overlay,
            stage,
            admissions,
            deliveries,
            submissions,
        )
        .await
    }

    async fn submit(
        &self,
        submission: Submission,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Receipt> {
        let expected = next_sequence(&self.store.view(vec![&*overlay]), &submission.signer)?;
        let in_sequence = submission.seq == expected;
        if !in_sequence {
            return Ok(rejected(
                &submission.target,
                Refusal::new(
                    reason::SEQUENCE,
                    format!(
                        "sequence {} is not the signer's next, {expected}",
                        submission.seq
                    ),
                ),
            ));
        }
        let checkpoint = overlay.checkpoint();
        overlay.set(
            SIGNERS,
            submission.signer.clone(),
            abi::encode(&(submission.seq + 1)),
        );
        let env = Env {
            network: self.network.clone(),
            height,
            time,
            me: submission.target.clone(),
            origin: Origin::External(submission.signer),
            cause: Cause::Direct,
        };
        let receipt = self
            .run(
                &submission.target,
                GuestCall::Execute(submission.payload),
                env,
                overlay,
                stage,
            )
            .await?;
        if let Outcome::Rejected(_) = receipt.outcome {
            overlay.restore(checkpoint);
        }
        Ok(receipt)
    }

    async fn run(
        &self,
        program: &str,
        call: GuestCall,
        env: Env,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Receipt> {
        unit::execute(
            self.world(env.height, env.time),
            overlay,
            stage,
            program,
            Invocation { env, call },
        )
        .await
    }

    async fn refresh_programs(
        &mut self,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Vec<Receipt>> {
        let reply = unit::query(
            self.world(height, time),
            vec![&*overlay],
            &*stage,
            &[],
            Origin::System,
            module_registry::PROGRAM.to_owned(),
            abi::encode(&module_registry::Query::At(height)),
        )
        .await?;
        let Ok(bytes) = reply else {
            return Ok(Vec::new());
        };
        let Ok(module_registry::Reply::Programs(entries)) =
            abi::decode::<module_registry::Reply>(&bytes)
        else {
            return Ok(Vec::new());
        };
        let running = programs_of(&View::new(self.store.storage(), vec![&*overlay]))?;
        let mut receipts = Vec::new();
        let mut wanted = BTreeSet::new();
        for entry in entries {
            let admissible = valid_program_id(&entry.program) && !wanted.contains(&entry.program);
            if !admissible {
                continue;
            }
            let rostered = View::new(self.store.storage(), vec![&*overlay])
                .get(BLOBS, &namespace::blob(&entry.code))?
                .is_some();
            if !rostered {
                continue;
            }
            wanted.insert(entry.program.clone());
            match running.get(&entry.program) {
                Some(code) if *code == entry.code => {}
                Some(_) => receipts.push(self.swap(&entry, overlay, stage)?),
                None => receipts.push(self.admit(&entry, height, time, overlay, stage).await?),
            }
        }
        for program in running.keys() {
            let dropped = !wanted.contains(program);
            if dropped {
                overlay.delete(PROGRAMS, program.clone().into_bytes());
                self.loaded.unload(program);
            }
        }
        Ok(receipts)
    }

    async fn admit(
        &mut self,
        entry: &module_registry::Entry,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Receipt> {
        let module = match self.load(entry, stage)? {
            Ok(module) => module,
            Err(refusal) => return Ok(rejected(&entry.program, refusal)),
        };
        self.loaded.admit(entry.program.clone(), entry.code, module);
        let env = Env {
            network: self.network.clone(),
            height,
            time,
            me: entry.program.clone(),
            origin: Origin::System,
            cause: Cause::Direct,
        };
        let receipt = self
            .run(
                &entry.program,
                GuestCall::Init(entry.params.clone()),
                env,
                overlay,
                stage,
            )
            .await?;
        match &receipt.outcome {
            Outcome::Applied { .. } => {
                overlay.set(
                    PROGRAMS,
                    entry.program.clone().into_bytes(),
                    abi::encode(&entry.code),
                );
                self.store.add_program(&entry.program).await?;
            }
            Outcome::Rejected(_) => self.loaded.unload(&entry.program),
        }
        Ok(receipt)
    }

    fn swap(
        &mut self,
        entry: &module_registry::Entry,
        overlay: &mut Overlay,
        stage: &Stage,
    ) -> Result<Receipt> {
        let module = match self.load(entry, stage)? {
            Ok(module) => module,
            Err(refusal) => return Ok(rejected(&entry.program, refusal)),
        };
        self.loaded.admit(entry.program.clone(), entry.code, module);
        overlay.set(
            PROGRAMS,
            entry.program.clone().into_bytes(),
            abi::encode(&entry.code),
        );
        Ok(Receipt {
            program: entry.program.clone(),
            outcome: Outcome::Applied { output: Vec::new() },
            events: Vec::new(),
        })
    }

    fn load(
        &self,
        entry: &module_registry::Entry,
        stage: &Stage,
    ) -> Result<std::result::Result<runtime::Code, Refusal>> {
        let layered = Layered {
            stage,
            blobs: &self.blobs,
        };
        match self.loaded.load(&layered, &entry.code)? {
            None => Err(Error::BlobUnavailable(entry.code)),
            Some(Err(fault)) => Ok(Err(refusal_of(fault))),
            Some(Ok(module)) => Ok(Ok(module)),
        }
    }

    async fn deliver(
        &self,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Vec<Delivered>> {
        let pending = queue::pending(&self.store.view(Vec::new()))?;
        let mut delivered = Vec::new();
        for queued in pending {
            queue::take(overlay, queued.seq);
            let receipt = match queued.item {
                Item::Message { source, message } => {
                    let item = ItemRef {
                        source: source.clone(),
                        item: queued.seq,
                    };
                    let env = Env {
                        network: self.network.clone(),
                        height,
                        time,
                        me: message.target.clone(),
                        origin: Origin::Program(source),
                        cause: Cause::Delivery(item.clone()),
                    };
                    let receipt = self
                        .run(
                            &message.target,
                            GuestCall::Execute(message.payload),
                            env,
                            overlay,
                            stage,
                        )
                        .await?;
                    if message.reply {
                        let completion = Item::Completion {
                            item,
                            by: message.target,
                            outcome: receipt.outcome.clone(),
                        };
                        queue::push(self.store.storage(), overlay, completion)?;
                    }
                    receipt
                }
                Item::Completion { item, by, outcome } => {
                    let env = Env {
                        network: self.network.clone(),
                        height,
                        time,
                        me: item.source.clone(),
                        origin: Origin::Program(by),
                        cause: Cause::Completion {
                            item: item.clone(),
                            outcome,
                        },
                    };
                    self.run(
                        &item.source,
                        GuestCall::Execute(Vec::new()),
                        env,
                        overlay,
                        stage,
                    )
                    .await?
                }
            };
            delivered.push(Delivered {
                item: queued.seq,
                receipt,
            });
        }
        Ok(delivered)
    }

    async fn commit(
        &mut self,
        tip: Tip,
        mut overlay: Overlay,
        stage: Stage,
        admissions: Vec<Receipt>,
        deliveries: Vec<Delivered>,
        submissions: Vec<Receipt>,
    ) -> Result<Applied> {
        overlay.set(NETWORK, namespace::TIP.to_vec(), abi::encode(&tip.id));
        let writes = overlay.into_writes();
        self.blobs.promote(stage)?;
        self.store.commit(tip.height, writes.clone()).await?;
        self.preconfirmed = Overlay::default();
        Ok(Applied {
            height: tip.height,
            admissions,
            deliveries,
            submissions,
            writes,
            root: self.root()?,
        })
    }
}

fn put_code(overlay: &mut Overlay, stage: &mut Stage, code: &[u8]) -> BlobId {
    let id = stage
        .put(HashKind::Sha256, CODE_KIND, code)
        .expect("the code kind is one word");
    overlay.set(BLOBS, namespace::blob(&id), Vec::new());
    id
}

fn rejected(program: &str, refusal: Refusal) -> Receipt {
    Receipt {
        program: program.to_owned(),
        outcome: Outcome::Rejected(refusal),
        events: Vec::new(),
    }
}

fn programs_in(storage: &Storage) -> Result<Vec<ProgramId>> {
    let mut programs: Vec<ProgramId> = RESERVED.map(str::to_owned).to_vec();
    for entry in storage.iter(PROGRAMS, b"", None, false)? {
        let (key, _) = entry?;
        programs.push(program_id(key)?);
    }
    Ok(programs)
}

fn programs_of(view: &View<'_>) -> Result<BTreeMap<ProgramId, BlobId>> {
    view.scan(PROGRAMS, &Scan::prefix(b""))?
        .into_iter()
        .map(|entry| {
            Ok((
                program_id(entry.key)?,
                abi::decode(&entry.value).map_err(corrupt)?,
            ))
        })
        .collect()
}

fn program_id(key: Vec<u8>) -> Result<ProgramId> {
    String::from_utf8(key).map_err(|_| Error::Corrupt("a program key is not a string".into()))
}

fn next_sequence(view: &View<'_>, signer: &[u8]) -> Result<u64> {
    match view.get(SIGNERS, signer)? {
        Some(bytes) => abi::decode(&bytes).map_err(corrupt),
        None => Ok(0),
    }
}

fn limits_of(view: &View<'_>) -> Result<Limits> {
    match view.get(NETWORK, namespace::LIMITS)? {
        Some(bytes) => abi::decode(&bytes).map_err(corrupt),
        None => Ok(Limits::default()),
    }
}

fn corrupt(refusal: Refusal) -> Error {
    Error::Corrupt(refusal.sentence)
}
