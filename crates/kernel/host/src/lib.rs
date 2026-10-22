mod answering;
mod crypto;
mod namespace;
mod parked;
mod parking;
mod reads;
mod scheduling;
mod snapshot;
mod unit;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub use abi::Roles;
use abi::{
    BlobId, Cause, Env, Foreign, HashKind, Invocation, ItemRef, Origin, Outcome, Principal,
    ProgramCall, ProgramId, Receipt, Refusal, Root, Scan, hook, reason,
    role::{self, access, identity, ownership, programs, validators},
};
use blob::{Blobs, Layered, Stage, Tidier};
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_runtime::Spawner;
use commonware_runtime::telemetry::metrics::{MetricsExt as _, histogram::Buckets};
use commonware_storage::Context;
use runtime::{Fault, Suspended};
use sha2::{Digest as _, Sha256};
use state::{
    Checkpoint, Commitment, Committed, Overlay, Reads, Storage, Store, View, Writes,
    valid_program_id,
};

use crate::parked::Waiter;
use crate::parking::{Run, Segment, Start, Status};
use crate::reads::Recorder;
use crate::unit::{Emitted, Frame, Loaded, World, refusal_of};

pub use namespace::{
    BLOBS, INDEXED_AT, NETWORK, PARKED, PROGRAMS, RECENT, RESERVED, SPENT, epoch_validators,
};
pub use parking::PAGE_FUEL;
pub use reads::{ReadSet, Written};
pub use runtime::{Limits, OUT_OF_FUEL};
pub use snapshot::{Queried, Snapshot, Source};
pub use state::LOCAL;
pub use unit::Places;

pub const STATE_DIR: &str = "state";
pub const BLOBS_DIR: &str = "blobs";
const CODE_KIND: &str = "program";
/// How deep messages nest in one frame: a submission runs at depth 0 and
/// each message, or reply, one deeper than the run that emitted it.
pub const MAX_DEPTH: u32 = 8;
pub const ANCHOR_WINDOW: u64 = 256;
pub use abi::BLOCK_BYTES;
const _: () = assert!(blob::SEGMENT_BYTES == BLOCK_BYTES as u64);
pub const HOOK_FUEL: u64 = 2_000_000;
const STREAK_LOG_EVERY: u64 = 64;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    State(#[from] state::Error),
    #[error(transparent)]
    Blobs(#[from] blob::Error),
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
    #[error("the founding programs' roles are refused: {0}")]
    Roles(Refusal),
    #[error("the network binds no program to the {role} role, which the kernel calls")]
    Unfilled { role: String },
    #[error("genesis hooks {0:?}, which is not a founding program")]
    Unhooked(ProgramId),
    #[error("no network was founded here")]
    Unfounded,
    #[error(
        "a network's member cap is 1 to {max}; this founding asks for {cap}",
        max = validators::MAX_MEMBERS
    )]
    MemberCap { cap: u32 },
    #[error("host state is corrupt: {0}")]
    Corrupt(String),
    #[error("a fork takes a network id of its own, and this state is already {0}")]
    Unrenamed(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub type BlockId = [u8; 32];
pub type SubmissionId = [u8; 32];

type Sender = std::result::Result<Option<Principal>, Refusal>;

pub struct Genesis {
    pub network: Vec<u8>,
    /// The validators program's founding params: the kernel writes them.
    pub validators: Vec<validators::Member>,
    /// The programs role's founding params are the kernel's too: every founding
    /// program and view.
    pub programs: Vec<Founding>,
    /// Views with no program behind them: each blob is stored and listed by
    /// the programs role under its name; nothing is admitted.
    pub views: Vec<FoundingView>,
    pub hooks: Vec<ProgramId>,
    pub limits: Limits,
    pub epoch_length: u64,
    pub time: u64,
    /// How many members the network ever holds at once: 1..=MAX_MEMBERS,
    /// fixed here and never rewritten.
    pub member_cap: u32,
    pub queries: validators::QueryPolicy,
}

pub struct Founding {
    pub program: ProgramId,
    pub code: Vec<u8>,
    pub params: Vec<u8>,
    pub roles: Vec<String>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Tip {
    pub height: u64,
    pub id: BlockId,
}

impl Tip {
    pub fn anchors(&self, height: u64) -> bool {
        self.height < height && height - self.height <= ANCHOR_WINDOW
    }
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Submission {
    pub signer: Vec<u8>,
    pub anchor: Tip,
    pub salt: Option<u64>,
    pub target: ProgramId,
    pub payload: Vec<u8>,
    pub client: Option<String>,
}

impl Submission {
    pub fn id(&self) -> SubmissionId {
        Sha256::digest(abi::encode(self)).into()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    pub height: u64,
    pub admissions: Vec<Receipt>,
    pub submissions: Vec<Receipt>,
    pub records: Vec<hook::Record>,
    pub writes: Writes,
    pub root: Root,
    pub places: Arc<Places>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Spent {
    pub executing: Duration,
    pub committing: Duration,
    pub slowest: Option<Slowest>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slowest {
    pub position: u32,
    pub spent: Duration,
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
    tidier: Tidier,
    network: Vec<u8>,
    parent: Option<Vec<u8>>,
    roles: Roles,
    loaded: Loaded,
    hook_streaks: BTreeMap<ProgramId, u64>,
    scheduler_streak: u64,
    roster_streak: u64,
    rename_streaks: BTreeMap<ProgramId, u64>,
    spent: Spent,
    layout_from: u64,
    reads_from: u64,
}

#[derive(Clone, Default)]
pub(crate) struct Preconfirmed {
    pub(crate) overlay: Overlay,
    pub(crate) stage: Stage,
}

struct Mark {
    checkpoint: Checkpoint,
    staged: BTreeSet<BlobId>,
}

impl Preconfirmed {
    fn mark(&self) -> Mark {
        Mark {
            checkpoint: self.overlay.checkpoint(),
            staged: self.stage.ids().copied().collect(),
        }
    }

    fn rewind(&mut self, mark: Mark) {
        self.overlay.restore(mark.checkpoint);
        self.stage.retain(|id| mark.staged.contains(id));
    }
}

enum Submitted {
    Stale(Receipt),
    Carried {
        receipt: Receipt,
        account: Option<identity::AccountNumber>,
    },
}

#[derive(Default)]
pub(crate) struct Accounts(BTreeMap<Vec<u8>, Asked>);

struct Asked {
    code: Option<BlobId>,
    reply: std::result::Result<Option<identity::AccountNumber>, Refusal>,
    reads: ReadSet,
    mark: u64,
}

impl Asked {
    fn holds(&self, code: Option<BlobId>, overlay: &Overlay) -> bool {
        let same_code = self.code == code;
        let unwritten = self
            .reads
            .programs()
            .all(|program| !overlay.written_since(program, self.mark));
        same_code && unwritten
    }
}

impl Submitted {
    fn receipt(self) -> Receipt {
        match self {
            Submitted::Stale(receipt) | Submitted::Carried { receipt, .. } => receipt,
        }
    }
}

struct Hooks {
    programs: Vec<ProgramId>,
    batched: Vec<ProgramId>,
    tallies: BTreeMap<ProgramId, HookTally>,
}

#[derive(Default)]
struct HookTally {
    refused: u64,
    reason: Option<String>,
}

impl<E> Host<E>
where
    E: Context + Spawner,
{
    /// Founds the network `genesis` describes as the store `name` in `dir`.
    ///
    /// A store already at a height was founded or joined, and is refused.
    /// A refused founding removes the `state/` and `blobs/` this call made; a
    /// directory that was there before, or that cannot be told apart from
    /// one, is left as the refusal leaves it. The commitments it opened stay
    /// in the runtime's storage: an unfounded store opens each commitment
    /// empty, so the next founding under `name` builds on none of them. A
    /// removal that fails is dropped: the founding's own error is what
    /// returns.
    pub async fn found(
        context: E,
        name: &str,
        dir: &Path,
        block: BlockId,
        genesis: Genesis,
    ) -> Result<(Host<E>, Applied)> {
        let state = dir.join(STATE_DIR);
        let blobs = dir.join(BLOBS_DIR);
        let made_state = !state.try_exists().unwrap_or(true);
        let made_blobs = !blobs.try_exists().unwrap_or(true);
        // boxed: unboxed, a caller that awaits the founding deep in its own
        // futures nests past the compiler's layout query depth
        let founded = Box::pin(Host::found_in(context, name, dir, block, genesis)).await;
        if founded.is_ok() {
            return founded;
        }
        // the refused host is dropped, its store closed
        if made_state {
            let _ = std::fs::remove_dir_all(&state);
        }
        if made_blobs {
            let _ = std::fs::remove_dir_all(&blobs);
        }
        founded
    }

    async fn found_in(
        context: E,
        name: &str,
        dir: &Path,
        block: BlockId,
        genesis: Genesis,
    ) -> Result<(Host<E>, Applied)> {
        if !(1..=validators::MAX_MEMBERS).contains(&genesis.member_cap) {
            return Err(Error::MemberCap {
                cap: genesis.member_cap,
            });
        }
        let mut overlay = Overlay::default();
        let mut stage = Stage::default();
        overlay.set(NETWORK, namespace::ID.to_vec(), genesis.network.clone());
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
        overlay.set(
            NETWORK,
            namespace::MEMBER_CAP.to_vec(),
            abi::encode(&genesis.member_cap),
        );
        let declared = genesis.programs.iter().flat_map(|founding| {
            founding
                .roles
                .iter()
                .map(|role| (role.as_str(), founding.program.as_str()))
        });
        let roles = Roles::declared(declared)
            .and_then(|roles| roles.filled().map(|()| roles))
            .map_err(Error::Roles)?;
        overlay.set(NETWORK, namespace::ROLES.to_vec(), abi::encode(&roles));
        let roles = &roles;
        let mut entries: Vec<programs::Entry> = genesis
            .programs
            .into_iter()
            .map(|founding| programs::Entry {
                program: founding.program,
                code: put_code(&mut overlay, &mut stage, &founding.code),
                params: founding.params,
            })
            .collect();
        for hook in &genesis.hooks {
            let founded = entries.iter().any(|entry| entry.program == *hook);
            if !founded {
                return Err(Error::Unhooked(hook.clone()));
            }
        }
        let founding_roles = [
            role::PROGRAMS,
            role::VALIDATORS,
            role::IDENTITY,
            role::BLOBS,
            role::ACCESS,
        ]
        .into_iter()
        .filter_map(|role| roles.get(role))
        .collect::<Vec<_>>();
        let validators_role = bound(roles, role::VALIDATORS)?;
        entries.sort_by_key(|entry| {
            founding_roles
                .iter()
                .position(|role| **role == entry.program)
                .unwrap_or(founding_roles.len())
        });
        let params = abi::encode(&validators::Genesis {
            validators: genesis.validators,
            member_cap: genesis.member_cap,
            queries: genesis.queries,
        });
        for entry in entries.iter_mut() {
            if entry.program == *validators_role {
                entry.params = params.clone();
            }
        }
        let views: Vec<programs::View> = genesis
            .views
            .into_iter()
            .map(|founding| programs::View {
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
        entries[0].params = abi::encode(&programs::Genesis {
            programs: entries.clone(),
            views,
            hooks: genesis.hooks,
            roles: roles.clone(),
        });
        // the roles init before identity can give an account; every other
        // program has its account before its init runs, so what it emits
        // carries it
        let is_role = |entry: &programs::Entry| founding_roles.contains(&&entry.program);
        let (role_entries, rest) = entries.split_at(entries.partition_point(is_role));
        // nothing above touches `dir`: a genesis refused there leaves no state
        let storage = Storage::open(&dir.join(STATE_DIR))?;
        // a store at a height was founded or joined: block 0 is not its next
        if let Some(height) = storage.height()? {
            return Err(Error::Height {
                expected: height + 1,
                got: 0,
            });
        }
        let store = Store::open(context, name, storage, RESERVED.map(str::to_owned)).await?;
        let blobs = Blobs::open(&dir.join(BLOBS_DIR))?;
        let tidier = tidier_of(&store, &blobs)?;
        let namespace::Activations {
            layout_from,
            reads_from,
        } = namespace::Activations::of(&genesis.network, None);
        let mut host = Host {
            store,
            blobs,
            tidier,
            network: genesis.network,
            parent: None,
            roles: roles.clone(),
            loaded: Loaded::new(genesis.limits, None),
            hook_streaks: BTreeMap::new(),
            scheduler_streak: 0,
            roster_streak: 0,
            rename_streaks: BTreeMap::new(),
            spent: Spent::default(),
            layout_from,
            reads_from,
        };
        let mut receipts = Vec::new();
        for entry in role_entries {
            let receipt = host
                .admit(entry, 0, genesis.time, &mut overlay, &mut stage)
                .await?;
            receipts.push(founded(entry, receipt)?);
        }
        for entry in role_entries {
            let receipt = host
                .register(&entry.program, 0, genesis.time, &mut overlay, &mut stage)
                .await?;
            receipts.push(founded(entry, receipt)?);
        }
        for entry in rest {
            let registered = host
                .register(&entry.program, 0, genesis.time, &mut overlay, &mut stage)
                .await?;
            receipts.push(founded(entry, registered)?);
            let admitted = host
                .admit(entry, 0, genesis.time, &mut overlay, &mut stage)
                .await?;
            receipts.push(founded(entry, admitted)?);
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
        host.close().await?;
        Ok((host, applied))
    }

    /// Opens the store `name` a founding or a join left in `dir`. A store at
    /// no height was never founded here, and is refused before any of its
    /// commitments opens: the commitments under `name` stay as they are.
    pub async fn open(context: E, name: &str, dir: &Path) -> Result<Host<E>> {
        let storage = Storage::open(&dir.join(STATE_DIR))?;
        if storage.height()?.is_none() {
            return Err(Error::Unfounded);
        }
        let programs = programs_in(&storage)?;
        let dropped = dropped_in(&storage)?;
        let mut store = Store::open(context, name, storage, programs).await?;
        // a node that died after the last block's writes landed and before
        // `commit` removed the programs it dropped still holds their
        // commitments and keys on disk
        for program in &dropped {
            store.remove_program(program).await?;
        }
        let blobs = Blobs::open(&dir.join(BLOBS_DIR))?;
        Host::assemble(store, blobs)
    }

    pub async fn fork(context: E, dir: &Path, network: &str) -> Result<Host<E>> {
        let mut host = Host::open(context, network, dir).await?;
        let renamed = host.network.as_slice() != network.as_bytes();
        if !renamed {
            return Err(Error::Unrenamed(network.to_owned()));
        }
        let parent = host.parent.clone().unwrap_or_else(|| host.network.clone());
        let tip = host.tip()?;
        let mut overlay = Overlay::default();
        overlay.set(NETWORK, namespace::ID.to_vec(), network.as_bytes().to_vec());
        overlay.set(NETWORK, namespace::PARENT.to_vec(), parent.clone());
        let namespace::Activations {
            layout_from,
            reads_from,
        } = namespace::Activations::of(network.as_bytes(), Some(parent.as_slice()));
        host.layout_from = layout_from;
        host.reads_from = reads_from;
        host.network = network.as_bytes().to_vec();
        host.parent = Some(parent);
        host.commit(
            tip,
            overlay,
            Stage::default(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .await?;
        host.close().await?;
        Ok(host)
    }

    pub async fn adopt(
        context: E,
        name: &str,
        dir: &Path,
        height: u64,
        commitments: BTreeMap<ProgramId, Commitment<E>>,
    ) -> Result<Host<E>> {
        let mut adoption = state::Adoption::default();
        Host::adopt_from(context, name, dir, height, commitments, &mut adoption).await
    }

    pub async fn adopt_from(
        context: E,
        name: &str,
        dir: &Path,
        height: u64,
        commitments: BTreeMap<ProgramId, Commitment<E>>,
        adoption: &mut state::Adoption,
    ) -> Result<Host<E>> {
        let storage = Storage::open(&dir.join(STATE_DIR))?;
        let store =
            Store::adopt_from(context, name, storage, height, commitments, adoption).await?;
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
        let roles = roles_of(&store.view(Vec::new()))?;
        let parent = store.view(Vec::new()).get(NETWORK, namespace::PARENT)?;
        let namespace::Activations {
            layout_from,
            reads_from,
        } = namespace::Activations::of(&network, parent.as_deref());
        let tidier = tidier_of(&store, &blobs)?;
        let mut host = Host {
            store,
            blobs,
            tidier,
            network,
            parent,
            roles,
            loaded: Loaded::new(limits, None),
            hook_streaks: BTreeMap::new(),
            scheduler_streak: 0,
            roster_streak: 0,
            rename_streaks: BTreeMap::new(),
            spent: Spent::default(),
            layout_from,
            reads_from,
        };
        host.sync_code()?;
        host.index()?;
        Ok(host)
    }

    fn index(&self) -> Result<()> {
        let height = self.height()?;
        let indexed = self
            .store
            .view(Vec::new())
            .get(LOCAL, namespace::INDEXED_AT)?;
        if indexed == Some(namespace::height(height)) {
            return Ok(());
        }
        let started = self.store.clock().current();
        let rebuilt = rebuild_index(self.store.storage(), height, BLOCK_BYTES)?;
        let elapsed = self
            .store
            .clock()
            .current()
            .duration_since(started)
            .unwrap_or_default();
        tracing::info!(
            target: "ducktape::host",
            event = "spent_indexed",
            height,
            ids = rebuilt.ids,
            largest_batch = rebuilt.largest,
            elapsed_ms = elapsed.as_millis() as u64,
            "the node rebuilt its index of spent frames from the committed spent rows"
        );
        Ok(())
    }

    fn sync_code(&mut self) -> Result<()> {
        sync_code(&mut self.loaded, &self.store.view(Vec::new()), &self.blobs)
    }

    fn ready(&mut self) -> Result<()> {
        self.sync_code()?;
        runnable(&self.loaded)
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

    pub fn parent(&self) -> Option<&[u8]> {
        self.parent.as_deref()
    }

    pub fn source(&self) -> Source<'_, E> {
        Source {
            committed: Committed::Live(&self.store),
            blobs: &self.blobs,
            loaded: &self.loaded,
            network: &self.network,
            roles: &self.roles,
            preconfirmed: None,
            layout_from: self.layout_from,
            reads_from: self.reads_from,
        }
    }

    pub fn spent(&self) -> Spent {
        self.spent
    }

    pub fn meter_queries(&mut self, fuel: Option<u64>) {
        self.loaded.meter_queries(fuel);
    }

    pub fn set_layout_from(&mut self, height: u64) {
        self.layout_from = height;
    }

    pub fn set_reads_from(&mut self, height: u64) {
        self.reads_from = height;
    }

    pub fn snapshot(&self) -> Result<Snapshot<E>> {
        Ok(Snapshot {
            pinned: self.store.pin()?,
            blobs: self.blobs.clone(),
            loaded: self.loaded.clone(),
            network: self.network.clone(),
            roles: self.roles.clone(),
            preconfirmed: Arc::default(),
            layout_from: self.layout_from,
            reads_from: self.reads_from,
            context: std::marker::PhantomData,
        })
    }

    pub fn height(&self) -> Result<u64> {
        self.source().height()
    }

    fn next_height(&self) -> Result<u64> {
        self.source().next_height()
    }

    pub fn tip(&self) -> Result<Tip> {
        self.source().tip()
    }

    pub fn epoch_length(&self) -> Result<u64> {
        self.source().epoch_length()
    }

    /// How many members the network ever holds at once, as founded.
    pub fn member_cap(&self) -> Result<u32> {
        self.source().member_cap()
    }

    pub fn epoch_members(&self, epoch: u64) -> Result<Option<Vec<validators::Member>>> {
        self.source().epoch_members(epoch)
    }

    /// The keys that vote and propose in `epoch`: the validators program's
    /// `Validators` answer. [`Host::epoch_members`] is who the mesh admits,
    /// residents included; consensus seats only these.
    pub fn epoch_validators(&self, epoch: u64) -> Result<Option<Vec<Vec<u8>>>> {
        self.source().epoch_validators(epoch)
    }

    pub fn roles(&self) -> &Roles {
        &self.roles
    }

    pub fn role(&self, role: &str) -> Result<&ProgramId> {
        bound(&self.roles, role)
    }

    pub fn root(&self) -> Result<Root> {
        self.source().root()
    }

    pub fn view(&self, layer: Layer) -> View<'_> {
        self.source().view(layer)
    }

    pub fn fresh(&self, submission: &Submission) -> Result<std::result::Result<(), Refusal>> {
        self.source().fresh(submission)
    }

    pub fn programs(&self) -> Result<BTreeMap<ProgramId, BlobId>> {
        self.source().programs()
    }

    pub fn missing_blobs(&self) -> Result<Vec<BlobId>> {
        self.source().missing_blobs()
    }

    pub async fn close(&mut self) -> Result<()> {
        let dropped = dropped_in(self.store.storage())?;
        Ok(self.store.close(&dropped).await?)
    }

    pub fn install(&mut self, id: BlobId, framed: &[u8]) -> Result<()> {
        let hashed = blob::id_of(id.kind(), framed)
            .map_err(|collision| Error::Corrupt(collision.to_string()))?;
        let hashes_to_id = hashed == id;
        if !hashes_to_id {
            return Err(Error::Corrupt(format!(
                "the bytes offered for {id:?} do not hash to it"
            )));
        }
        self.blobs.write(&id, framed)?;
        self.sync_code()
    }

    pub fn blobs(&self) -> &Blobs {
        &self.blobs
    }

    pub async fn readable(&self, id: &BlobId, time: u64) -> Result<bool> {
        self.source().readable(id, time).await
    }

    pub async fn blob(&self, id: &BlobId, time: u64) -> Result<Option<Vec<u8>>> {
        self.source().blob(id, time).await
    }

    fn world(&self, height: u64, time: u64) -> World<'_, E> {
        self.source().world(height, time)
    }

    pub async fn query(
        &self,
        layer: Layer,
        time: u64,
        origin: Origin,
        program: &str,
        request: Vec<u8>,
    ) -> Result<std::result::Result<Vec<u8>, Refusal>> {
        self.source()
            .query(layer, time, origin, program, request)
            .await
    }

    /// Records who `epoch` seats (`Validators`) and who its mesh admits
    /// (`Members`, residents included).
    async fn record_epoch(
        &self,
        epoch: u64,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &Stage,
    ) -> Result<()> {
        let validators::Reply::Members(members) = self
            .ask_validators(validators::Query::Members, height, time, overlay, stage)
            .await?
        else {
            return Err(Error::Corrupt(
                "the validators program answered Members with another reply".into(),
            ));
        };
        // every mesh is sized for the cap, so no node could track such an
        // epoch: the block that closes the one before is not applied, and
        // as that block is final the network halts there and is founded again
        let cap = member_cap_of(&self.store.view(vec![&*overlay]))?;
        if members.len() > cap as usize {
            return Err(Error::Corrupt(format!(
                "the validators program seats {} members for epoch {epoch}, past the network's \
                 cap of {cap}",
                members.len()
            )));
        }
        let validators::Reply::Validators(seated) = self
            .ask_validators(validators::Query::Validators, height, time, overlay, stage)
            .await?
        else {
            return Err(Error::Corrupt(
                "the validators program answered Validators with another reply".into(),
            ));
        };
        overlay.set(NETWORK, namespace::epoch(epoch), abi::encode(&members));
        overlay.set(
            NETWORK,
            namespace::epoch_validators(epoch),
            abi::encode(&seated),
        );
        Ok(())
    }

    async fn ask_validators(
        &self,
        query: validators::Query,
        height: u64,
        time: u64,
        overlay: &Overlay,
        stage: &Stage,
    ) -> Result<validators::Reply> {
        let reply = unit::query(
            self.world(height, time),
            vec![overlay],
            stage,
            &[],
            Origin::System,
            self.role(role::VALIDATORS)?.clone(),
            abi::encode(&query),
            &mut self.loaded.limits().fuel,
        )
        .await?;
        let bytes = reply.map_err(|refusal| {
            Error::Corrupt(format!(
                "the validators program refused {query:?}: {refusal}"
            ))
        })?;
        abi::decode(&bytes).map_err(corrupt)
    }

    pub async fn apply(&mut self, block: Block) -> Result<Applied> {
        let started = self.store.clock().current();
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
        let mut hooks = self
            .source()
            .hooks(block.height, block.time, &overlay, &stage)
            .await?;
        let mut submissions = Vec::new();
        let mut records = Vec::new();
        let mut slowest: Option<Slowest> = None;
        let mut accounts = Accounts::default();
        for (position, submission) in block.submissions.into_iter().enumerate() {
            let began = self.store.clock().current();
            let signer = submission.signer.clone();
            let target = submission.target.clone();
            let submitted = self
                .source()
                .submit(
                    submission,
                    block.height,
                    block.time,
                    &mut overlay,
                    &mut stage,
                    &mut accounts,
                )
                .await?;
            let receipt = match submitted {
                Submitted::Stale(receipt) => receipt,
                Submitted::Carried { receipt, account } => {
                    let record = hook::Record {
                        height: block.height,
                        position: position as u32,
                        signer,
                        account,
                        target,
                        applied: matches!(receipt.outcome, Outcome::Applied { .. }),
                    };
                    self.source()
                        .tell(&mut hooks, &record, block.time, &mut overlay, &mut stage)
                        .await?;
                    records.push(record);
                    receipt
                }
            };
            submissions.push(receipt);
            let spent = self
                .store
                .clock()
                .current()
                .duration_since(began)
                .unwrap_or_default();
            if slowest.is_none_or(|slowest| spent > slowest.spent) {
                slowest = Some(Slowest {
                    position: position as u32,
                    spent,
                });
            }
        }
        self.source()
            .tell_batched(
                &mut hooks,
                &records,
                block.height,
                block.time,
                &mut overlay,
                &mut stage,
            )
            .await?;
        self.log_hooks(block.height, hooks);
        let refusals = self
            .run_scheduled(block.height, block.time, &mut overlay, &mut stage)
            .await?;
        self.log_scheduler(block.height, refusals);
        self.source()
            .expire_reads(block.height, block.time, &mut overlay, &mut stage)
            .await?;
        let epoch_length = self.epoch_length()?;
        let ends_an_epoch = (block.height + 1).is_multiple_of(epoch_length);
        if ends_an_epoch {
            let next_epoch = (block.height + 1) / epoch_length;
            self.record_epoch(next_epoch, block.height, block.time, &mut overlay, &stage)
                .await?;
        }
        let executed = self.store.clock().current();
        let applied = self
            .commit(
                Tip {
                    height: block.height,
                    id: block.id,
                },
                overlay,
                stage,
                admissions,
                submissions,
                records,
            )
            .await;
        let committed = self.store.clock().current();
        self.spent = Spent {
            executing: executed.duration_since(started).unwrap_or_default(),
            committing: committed.duration_since(executed).unwrap_or_default(),
            slowest,
        };
        let applied = applied?;
        self.unlink(&applied.writes, block.time).await;
        self.tidier.wake();
        Ok(applied)
    }
}

impl<'a, E> Source<'a, E>
where
    E: Context + Spawner,
{
    pub fn place_of(&self, program: &str) -> Option<String> {
        self.loaded.places().of(program).map(str::to_owned)
    }

    async fn submit(
        &self,
        submission: Submission,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
        accounts: &mut Accounts,
    ) -> Result<Submitted> {
        let id = submission.id();
        let view = self.committed.view(vec![&*overlay]);
        if let Err(refusal) = fresh(&view, &submission, &id, height)? {
            return Ok(Submitted::Stale(rejected(&submission.target, refusal)));
        }
        if height < self.layout_from {
            let anchored = namespace::spent(submission.anchor.height, &id);
            overlay.set(SPENT, anchored, Vec::new());
        }
        overlay.set(LOCAL, id.to_vec(), Vec::new());
        let asked = identity::Query::Account(submission.signer.clone());
        let account = match self
            .known(accounts, asked, height, time, overlay, stage)
            .await?
        {
            Ok(account) => account,
            Err(refusal) => {
                return Ok(Submitted::Carried {
                    receipt: rejected(&submission.target, refusal),
                    account: None,
                });
            }
        };
        let checkpoint = overlay.checkpoint();
        if self.reads_at(&submission.target, height) {
            let receipt = self
                .answer_read(
                    &submission.signer,
                    &submission.payload,
                    height,
                    time,
                    overlay,
                    stage,
                )
                .await?;
            if let Outcome::Rejected(_) = receipt.outcome {
                overlay.restore(checkpoint);
            }
            return Ok(Submitted::Carried { receipt, account });
        }
        let env = Env {
            network: self.network.to_vec(),
            height,
            time,
            me: submission.target.clone(),
            origin: Origin::External(submission.signer),
            // a key that holds no account still runs (identity's own
            // create is such a frame); it acts as no one
            sender: account.map(Principal::Account),
            roles: self.roles.clone(),
            cause: Cause::Direct,
        };
        let receipt = self
            .run_from(
                submission.client,
                &submission.target,
                ProgramCall::Execute(submission.payload),
                env,
                overlay,
                stage,
            )
            .await?;
        if let Outcome::Rejected(_) = receipt.outcome {
            overlay.restore(checkpoint);
        }
        Ok(Submitted::Carried { receipt, account })
    }

    async fn hooks(
        &self,
        height: u64,
        time: u64,
        overlay: &Overlay,
        stage: &Stage,
    ) -> Result<Hooks> {
        let none = || Hooks {
            programs: Vec::new(),
            batched: Vec::new(),
            tallies: BTreeMap::new(),
        };
        let reply = unit::query(
            self.world(height, time),
            vec![overlay],
            stage,
            &[],
            Origin::System,
            self.role(role::PROGRAMS)?.clone(),
            abi::encode(&programs::Query::Hooks(height)),
            &mut self.loaded.limits().fuel,
        )
        .await?;
        let Ok(bytes) = reply else {
            return Ok(none());
        };
        let Ok(programs::Reply::Hooks(listed)) = abi::decode::<programs::Reply>(&bytes) else {
            return Ok(none());
        };
        let mut hooks = none();
        for program in listed {
            let Some(code) = self.loaded.module(&program) else {
                continue;
            };
            let told = if code.exports(hook::BATCH_EXPORT) {
                &mut hooks.batched
            } else {
                &mut hooks.programs
            };
            if !told.contains(&program) {
                told.push(program);
            }
        }
        Ok(hooks)
    }

    async fn tell(
        &self,
        hooks: &mut Hooks,
        record: &hook::Record,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<()> {
        for program in &hooks.programs {
            self.tell_one(&mut hooks.tallies, program, record, time, overlay, stage)
                .await?;
        }
        Ok(())
    }

    async fn tell_batched(
        &self,
        hooks: &mut Hooks,
        records: &[hook::Record],
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        for program in &hooks.batched {
            let call = ProgramCall::Hooks(records.to_vec());
            let refused = self
                .call_hook(program, height, call, time, overlay, stage)
                .await?;
            hooks.tallies.entry(program.clone()).or_default();
            let Some(refusal) = refused else {
                continue;
            };
            tracing::debug!(
                target: "ducktape::hook",
                event = "hook_batch_refused",
                hook = %program,
                height,
                records = records.len(),
                reason = %refusal.reason,
            );
            for record in records {
                self.tell_one(&mut hooks.tallies, program, record, time, overlay, stage)
                    .await?;
            }
        }
        Ok(())
    }

    async fn tell_one(
        &self,
        tallies: &mut BTreeMap<ProgramId, HookTally>,
        program: &str,
        record: &hook::Record,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<()> {
        let call = ProgramCall::Hook(record.clone());
        let refused = self
            .call_hook(program, record.height, call, time, overlay, stage)
            .await?;
        let tally = tallies.entry(program.to_owned()).or_default();
        let Some(refusal) = refused else {
            return Ok(());
        };
        tracing::debug!(
            target: "ducktape::hook",
            event = "hook_refused",
            hook = %program,
            height = record.height,
            position = record.position,
            reason = %refusal.reason,
        );
        tally.refused += 1;
        tally.reason.get_or_insert(refusal.reason);
        Ok(())
    }

    async fn call_hook(
        &self,
        program: &str,
        height: u64,
        call: ProgramCall,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Option<Refusal>> {
        let told = match &call {
            ProgramCall::Hooks(records) => records.len() as u64,
            _ => 1,
        };
        let checkpoint = overlay.checkpoint();
        let env = Env {
            network: self.network.to_vec(),
            height,
            time,
            me: program.to_owned(),
            origin: Origin::System,
            sender: Some(Principal::System),
            roles: self.roles.clone(),
            cause: Cause::Direct,
        };
        let mut frame = Frame {
            fuel: self
                .loaded
                .limits()
                .fuel
                .map(|_| HOOK_FUEL.saturating_mul(told)),
            first_item: 0,
            next_item: 0,
            client: None,
        };
        let invocation = Invocation { env, call };
        let (receipt, effects) = unit::execute(
            self.world(height, time),
            overlay,
            stage,
            &mut frame,
            program,
            None,
            invocation,
        )
        .await?;
        let sent_messages =
            !(effects.emitted.is_empty() && effects.calls.is_empty() && effects.resumes.is_empty());
        let refusal = match (receipt.outcome, sent_messages) {
            (Outcome::Rejected(refusal), _) => refusal,
            (Outcome::Applied { .. }, false) => return Ok(None),
            (Outcome::Applied { .. }, true) => {
                overlay.restore(checkpoint);
                unit::discard_unrostered(self.committed.reads(), overlay, stage)?;
                Refusal::new(reason::UNSUPPORTED, "a hook sends no messages")
            }
        };
        Ok(Some(refusal))
    }

    /// The account the identity role says `program` runs as, for a frame
    /// it causes: asked on the frame's fuel.
    async fn account_of(
        &self,
        program: &str,
        height: u64,
        time: u64,
        overlay: &Overlay,
        stage: &Stage,
        frame: &mut Frame,
    ) -> Result<Sender> {
        let asked = identity::Query::OfProgram(program.to_owned());
        let account = self
            .account(asked, height, time, overlay, stage, &mut frame.fuel)
            .await?;
        Ok(account.map(|account| account.map(Principal::Account)))
    }

    #[allow(clippy::too_many_arguments)]
    async fn own_sender(
        &self,
        program: &str,
        asked: bool,
        height: u64,
        time: u64,
        overlay: &Overlay,
        stage: &Stage,
        frame: &mut Frame,
    ) -> Result<Sender> {
        match asked {
            true => {
                self.account_of(program, height, time, overlay, stage, frame)
                    .await
            }
            false => Ok(Ok(None)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn sender(
        &self,
        program: &str,
        acting: Option<identity::AccountNumber>,
        own: &Sender,
        height: u64,
        time: u64,
        overlay: &Overlay,
        stage: &Stage,
        frame: &mut Frame,
    ) -> Result<Sender> {
        match acting {
            None => Ok(own.clone()),
            Some(account) => {
                self.run_by(program, account, height, time, overlay, stage, frame)
                    .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_by(
        &self,
        program: &str,
        account: identity::AccountNumber,
        height: u64,
        time: u64,
        overlay: &Overlay,
        stage: &Stage,
        frame: &mut Frame,
    ) -> Result<Sender> {
        let asked = identity::Query::Runs {
            program: program.to_owned(),
            account,
        };
        let answered = self
            .account(asked, height, time, overlay, stage, &mut frame.fuel)
            .await?;
        Ok(answered.and_then(|answer| match answer {
            Some(number) if number == account => Ok(Some(Principal::Account(account))),
            _ => Err(Refusal::new(
                reason::UNAUTHORIZED,
                format!("{program} does not run account {account}"),
            )),
        }))
    }

    async fn known(
        &self,
        accounts: &mut Accounts,
        asked: identity::Query,
        height: u64,
        time: u64,
        overlay: &Overlay,
        stage: &Stage,
    ) -> Result<std::result::Result<Option<identity::AccountNumber>, Refusal>> {
        let code = self.loaded.code(self.role(role::IDENTITY)?);
        let key = abi::encode(&asked);
        if let Some(known) = accounts.0.get(&key)
            && known.holds(code, overlay)
        {
            return Ok(known.reply.clone());
        }
        let mark = overlay.mark();
        let recorder = Recorder::default();
        let world = World {
            recorder: Some(&recorder),
            ..self.world(height, time)
        };
        let fuel = &mut self.loaded.limits().fuel;
        let reply = self.account_in(asked, world, overlay, stage, fuel).await?;
        let known = Asked {
            code,
            reply: reply.clone(),
            reads: recorder.into_reads(),
            mark,
        };
        accounts.0.insert(key, known);
        Ok(reply)
    }

    /// The account the identity role says a frame acts as: the one a key
    /// holds (`Account`), or a program's own (`OfProgram`). The role's
    /// refusal, or a reply that is not its interface's, rejects the frame.
    async fn account(
        &self,
        asked: identity::Query,
        height: u64,
        time: u64,
        overlay: &Overlay,
        stage: &Stage,
        fuel: &mut Option<u64>,
    ) -> Result<std::result::Result<Option<identity::AccountNumber>, Refusal>> {
        let world = self.world(height, time);
        self.account_in(asked, world, overlay, stage, fuel).await
    }

    async fn account_in<'w>(
        &'w self,
        asked: identity::Query,
        world: World<'w, E>,
        overlay: &'w Overlay,
        stage: &'w Stage,
        fuel: &mut Option<u64>,
    ) -> Result<std::result::Result<Option<identity::AccountNumber>, Refusal>> {
        let reply = unit::query(
            world,
            vec![overlay],
            stage,
            &[],
            Origin::System,
            self.role(role::IDENTITY)?.clone(),
            abi::encode(&asked),
            fuel,
        )
        .await?;
        Ok(reply.and_then(|bytes| match abi::decode(&bytes) {
            Ok(identity::Reply::Account(account)) => Ok(account),
            Ok(other) => Err(Refusal::new(
                reason::UNEXPECTED_REPLY,
                format!("the identity program answered {asked:?} with {other:?}"),
            )),
            Err(refusal) => Err(Refusal::new(
                reason::UNEXPECTED_REPLY,
                format!(
                    "the identity program answered {asked:?} with {}",
                    refusal.sentence
                ),
            )),
        }))
    }

    /// One frame: `program` runs `call` on the network's whole budget, and
    /// what it emits runs after it.
    async fn run(
        &self,
        program: &str,
        call: ProgramCall,
        env: Env,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Receipt> {
        self.run_from(None, program, call, env, overlay, stage)
            .await
    }

    async fn run_from(
        &self,
        client: Option<String>,
        program: &str,
        call: ProgramCall,
        env: Env,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Receipt> {
        let mut frame = self.open_frame(self.loaded.limits().fuel, client, overlay)?;
        let ran = self
            .run_frame(
                program,
                call,
                env,
                Waiter::Nobody,
                overlay,
                stage,
                &mut frame,
                0,
            )
            .await?;
        self.close_frame(overlay, &frame);
        Ok(ran.receipt)
    }

    fn open_frame(
        &self,
        fuel: Option<u64>,
        client: Option<String>,
        overlay: &Overlay,
    ) -> Result<Frame> {
        let view = self.committed.view(vec![overlay]);
        let next_item = parked::counter(&view, NETWORK, namespace::NEXT_ITEM)?;
        Ok(Frame {
            fuel,
            first_item: next_item,
            next_item,
            client,
        })
    }

    fn close_frame(&self, overlay: &mut Overlay, frame: &Frame) {
        let emitted = frame.next_item != frame.first_item;
        if emitted {
            let next = frame.next_item.to_be_bytes().to_vec();
            overlay.set(NETWORK, namespace::NEXT_ITEM.to_vec(), next);
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_frame(
        &self,
        program: &str,
        call: ProgramCall,
        env: Env,
        waiter: Waiter,
        overlay: &mut Overlay,
        stage: &mut Stage,
        frame: &mut Frame,
        depth: u32,
    ) -> Result<Run> {
        if depth > MAX_DEPTH {
            let too_deep = Refusal::new(
                reason::CAPACITY,
                format!("messages nest deeper than {MAX_DEPTH}"),
            );
            return Ok(Run::finished(rejected(program, too_deep)));
        }
        let checkpoint = overlay.checkpoint();
        if let Some(payload) = gated(&call, &env)
            && let Err(refusal) = self
                .gate(program, payload, &env, overlay, stage, frame)
                .await?
        {
            overlay.restore(checkpoint);
            unit::discard_unrostered(self.committed.reads(), overlay, stage)?;
            return Ok(Run::finished(rejected(program, refusal)));
        }
        let segment = self
            .segment(program, &env, None, call, overlay, stage, frame, depth)
            .await?;
        let refused = matches!(segment.receipt.outcome, Outcome::Rejected(_));
        if refused {
            overlay.restore(checkpoint);
            unit::discard_unrostered(self.committed.reads(), overlay, stage)?;
        }
        let start = Start {
            frame: None,
            waiter,
            checkpoint,
        };
        self.follow(program, env, start, segment, overlay, stage, frame, depth)
            .await
    }

    async fn gate(
        &self,
        program: &str,
        payload: &[u8],
        env: &Env,
        overlay: &mut Overlay,
        stage: &mut Stage,
        frame: &mut Frame,
    ) -> Result<std::result::Result<(), Refusal>> {
        let Some(checker) = self.roles.get(role::ACCESS).cloned() else {
            return Ok(Ok(()));
        };
        let world = self.world(env.height, env.time);
        let answered = unit::access(
            world,
            vec![&*overlay],
            &*stage,
            env.clone(),
            payload.to_vec(),
            &mut frame.fuel,
        )
        .await?;
        let needs = match answered.and_then(|bytes| needs_of(program, &bytes)) {
            Ok(needs) => needs,
            Err(refusal) => return Ok(Err(refusal)),
        };
        if needs.is_empty() {
            return Ok(Ok(()));
        }
        let check = access::Op::Check {
            program: program.to_owned(),
            actor: actor_of(env),
            needs,
        };
        let invocation = Invocation {
            env: Env {
                me: checker.clone(),
                origin: Origin::System,
                sender: Some(Principal::System),
                cause: Cause::Direct,
                ..env.clone()
            },
            call: ProgramCall::Execute(abi::encode(&check)),
        };
        let (receipt, effects) =
            unit::execute(world, overlay, stage, frame, &checker, None, invocation).await?;
        let unit::Effects {
            emitted,
            calls,
            resumes,
            deferred,
            parked,
            ..
        } = effects;
        let quiet = emitted.is_empty()
            && calls.is_empty()
            && resumes.is_empty()
            && !deferred
            && parked.is_none();
        match (receipt.outcome, quiet) {
            (Outcome::Rejected(refusal), _) => Ok(Err(refusal)),
            (Outcome::Applied { .. }, true) => Ok(Ok(())),
            (Outcome::Applied { .. }, false) => Ok(Err(Refusal::new(
                reason::UNSUPPORTED,
                format!("{checker} sends no message while it checks access"),
            ))),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn segment(
        &self,
        program: &str,
        env: &Env,
        suspended: Option<Suspended>,
        call: ProgramCall,
        overlay: &mut Overlay,
        stage: &mut Stage,
        frame: &mut Frame,
        depth: u32,
    ) -> Result<Segment> {
        let checkpoint = overlay.checkpoint();
        let invocation = Invocation {
            env: env.clone(),
            call,
        };
        let world = self.world(env.height, env.time);
        let (mut receipt, effects) =
            unit::execute(world, overlay, stage, frame, program, suspended, invocation).await?;
        let unit::Effects {
            emitted,
            calls,
            resumes,
            deferred,
            parked,
            writes,
        } = effects;
        let guarded = self
            .guard(program, env, writes, overlay, stage, frame)
            .await?;
        if let Err(refusal) = guarded {
            overlay.restore(checkpoint);
            unit::discard_unrostered(self.committed.reads(), overlay, stage)?;
            receipt.outcome = Outcome::Rejected(refusal);
            return Ok(Segment::finished(receipt));
        }
        let refused = self
            .deliver(
                program,
                env,
                emitted,
                &mut receipt,
                overlay,
                stage,
                frame,
                depth,
            )
            .await?;
        if let Some(outcome) = refused {
            overlay.restore(checkpoint);
            unit::discard_unrostered(self.committed.reads(), overlay, stage)?;
            receipt.outcome = outcome;
            return Ok(Segment::finished(receipt));
        }
        for resumed in resumes {
            let ran = Box::pin(self.resume_parked(
                resumed,
                env.height,
                env.time,
                overlay,
                stage,
                frame,
                depth + 1,
            ))
            .await?;
            receipt.nested.push(ran);
        }
        Ok(Segment {
            receipt,
            calls,
            deferred,
            parked,
        })
    }

    /// Asks the ownership role whether the run `program` just made stands,
    /// given the owned keys it wrote: one call however many there are, made
    /// once the run has returned and before anything it emitted is
    /// delivered. The call says who acted and what each key was before and
    /// after; the role decides, and its refusal undoes the run. A run that
    /// wrote no owned key makes no call, and a network that binds no
    /// ownership role guards nothing.
    async fn guard(
        &self,
        program: &str,
        env: &Env,
        writes: Vec<ownership::Write>,
        overlay: &mut Overlay,
        stage: &mut Stage,
        frame: &mut Frame,
    ) -> Result<std::result::Result<(), Refusal>> {
        if writes.is_empty() {
            return Ok(Ok(()));
        }
        let Some(guardian) = self.roles.get(role::OWNERSHIP).cloned() else {
            return Ok(Ok(()));
        };
        let guard = ownership::Op::Guard(ownership::Guard {
            program: program.to_owned(),
            origin: env.origin.clone(),
            sender: env.sender.clone(),
            writes,
        });
        let invocation = Invocation {
            env: Env {
                me: guardian.clone(),
                origin: Origin::System,
                sender: Some(Principal::System),
                cause: Cause::Direct,
                ..env.clone()
            },
            call: ProgramCall::Execute(abi::encode(&guard)),
        };
        let world = self.world(env.height, env.time);
        let (receipt, effects) =
            unit::execute(world, overlay, stage, frame, &guardian, None, invocation).await?;
        match (receipt.outcome, effects.quiet()) {
            (Outcome::Rejected(refusal), _) => Ok(Err(refusal)),
            (Outcome::Applied { .. }, true) => Ok(Ok(())),
            (Outcome::Applied { .. }, false) => Ok(Err(Refusal::new(
                reason::UNSUPPORTED,
                format!("{guardian} sends no message while it guards"),
            ))),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn deliver(
        &self,
        program: &str,
        env: &Env,
        emitted: Vec<Emitted>,
        receipt: &mut Receipt,
        overlay: &mut Overlay,
        stage: &mut Stage,
        frame: &mut Frame,
        depth: u32,
    ) -> Result<Option<Outcome>> {
        let (height, time) = (env.height, env.time);
        // the emitter's account, asked once for all its messages
        let as_itself = emitted.iter().any(Emitted::as_its_emitter);
        let emitter = self
            .own_sender(program, as_itself, height, time, overlay, stage, frame)
            .await?;
        for Emitted {
            item,
            message,
            foreign,
        } in emitted
        {
            let sender = match foreign {
                Some(foreign) => foreign_sender(program, env, message.acting, foreign),
                None => {
                    self.sender(
                        program,
                        message.acting,
                        &emitter,
                        height,
                        time,
                        overlay,
                        stage,
                        frame,
                    )
                    .await?
                }
            };
            let ran = match sender {
                Err(refusal) => Run::finished(rejected(&message.target, refusal)),
                Ok(sender) => {
                    let cause = Cause::Message(item.clone());
                    let env = self.env(&message.target, program, sender, cause, height, time);
                    let call = ProgramCall::Execute(message.payload);
                    let waiter = match message.reply {
                        true => Waiter::Reply {
                            emitter: program.to_owned(),
                            item: item.clone(),
                        },
                        false => Waiter::Nobody,
                    };
                    Box::pin(self.run_frame(
                        &message.target,
                        call,
                        env,
                        waiter,
                        overlay,
                        stage,
                        frame,
                        depth + 1,
                    ))
                    .await?
                }
            };
            let outcome = ran.receipt.outcome.clone();
            receipt.nested.push(ran.receipt);
            let waits = matches!(ran.status, Status::Parked);
            if waits {
                continue;
            }
            let refused = matches!(outcome, Outcome::Rejected(_));
            let outcome = match (refused, message.reply) {
                (false, false) => continue,
                (true, false) => outcome,
                (_, true) => {
                    let replied = self
                        .complete(
                            program,
                            &message.target,
                            item,
                            outcome,
                            height,
                            time,
                            overlay,
                            stage,
                            frame,
                            depth + 1,
                        )
                        .await?;
                    let outcome = replied.outcome.clone();
                    receipt.nested.push(replied);
                    match outcome {
                        Outcome::Applied { .. } => continue,
                        Outcome::Rejected(_) => outcome,
                    }
                }
            };
            return Ok(Some(outcome));
        }
        Ok(None)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn complete(
        &self,
        emitter: &str,
        by: &str,
        item: ItemRef,
        outcome: Outcome,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
        frame: &mut Frame,
        depth: u32,
    ) -> Result<Receipt> {
        let sender = match self
            .account_of(by, height, time, overlay, stage, frame)
            .await?
        {
            Err(refusal) => return Ok(rejected(emitter, refusal)),
            Ok(sender) => sender,
        };
        let cause = Cause::Completion { item, outcome };
        let env = self.env(emitter, by, sender, cause, height, time);
        let call = ProgramCall::Execute(Vec::new());
        let waiter = Waiter::Nobody;
        let ran =
            Box::pin(self.run_frame(emitter, call, env, waiter, overlay, stage, frame, depth))
                .await?;
        Ok(ran.receipt)
    }

    pub(crate) fn env(
        &self,
        me: &str,
        origin: &str,
        sender: Option<Principal>,
        cause: Cause,
        height: u64,
        time: u64,
    ) -> Env {
        Env {
            network: self.network.to_vec(),
            height,
            time,
            me: me.to_owned(),
            origin: Origin::Program(origin.to_owned()),
            sender,
            roles: self.roles.clone(),
            cause,
        }
    }
}

impl<E> Host<E>
where
    E: Context + Spawner,
{
    fn log_hooks(&mut self, height: u64, hooks: Hooks) {
        for (program, tally) in hooks.tallies {
            let Some(reason) = tally.reason else {
                self.hook_streaks.remove(&program);
                continue;
            };
            let blocks = self.hook_streaks.entry(program.clone()).or_default();
            *blocks += 1;
            let due = *blocks == 1 || blocks.is_multiple_of(STREAK_LOG_EVERY);
            if due {
                tracing::warn!(
                    target: "ducktape::hook",
                    event = "hook_refused",
                    hook = %program,
                    height,
                    frames = tally.refused,
                    blocks = *blocks,
                    reason = %reason,
                );
            }
        }
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
            self.role(role::PROGRAMS)?.clone(),
            abi::encode(&programs::Query::At(height)),
            &mut self.loaded.limits().fuel,
        )
        .await?;
        let Ok(bytes) = reply else {
            return Ok(Vec::new());
        };
        let Ok(programs::Reply::Programs(entries)) = abi::decode::<programs::Reply>(&bytes) else {
            return Ok(Vec::new());
        };
        let roles = match self
            .roster_roles(&entries, height, time, overlay, stage)
            .await?
        {
            Ok(roles) => roles,
            Err(refusal) => {
                self.log_roster(height, refusal);
                return Ok(Vec::new());
            }
        };
        self.roster_streak = 0;
        let mut running = programs_of(&View::new(self.store.storage(), vec![&*overlay]))?;
        let mut receipts = Vec::new();
        let mut wanted = BTreeSet::new();
        let mut changed = Vec::new();
        let asked = self.renames(height, time, overlay, stage).await?;
        let mut held = BTreeSet::new();
        let mut accepted = Vec::new();
        for (from, to) in renaming(asked, &running, &entries) {
            let receipt = self
                .rename_account(&from, &to, height, time, overlay, stage)
                .await?;
            match &receipt.outcome {
                Outcome::Applied { .. } => {
                    self.rename_streaks.remove(&from);
                    accepted.push((from, to));
                }
                Outcome::Rejected(refusal) => {
                    self.log_rename(height, &from, &to, refusal);
                    held.insert(from);
                    held.insert(to);
                }
            }
            receipts.push(receipt);
        }
        for (from, to) in accepted {
            self.rename(&from, &to, &mut running, overlay);
            changed.push(from);
        }
        for entry in entries {
            let admissible = valid_program_id(&entry.program)
                && !wanted.contains(&entry.program)
                && !held.contains(&entry.program);
            if !admissible {
                continue;
            }
            let readable = self
                .source()
                .readable_in(&entry.code, height, time, overlay, stage)
                .await?;
            if !readable {
                continue;
            }
            wanted.insert(entry.program.clone());
            match running.get(&entry.program) {
                Some(code) if *code == entry.code => {}
                Some(_) => {
                    receipts.push(self.swap(&entry, overlay, stage)?);
                    changed.push(entry.program.clone());
                }
                None => {
                    // a program runs only as its account, its init too:
                    // identity's refusal leaves it unadmitted, and the next
                    // height registers it again
                    let checkpoint = overlay.checkpoint();
                    let registered = self
                        .register(&entry.program, height, time, overlay, stage)
                        .await?;
                    let refused = matches!(registered.outcome, Outcome::Rejected(_));
                    receipts.push(registered);
                    if refused {
                        continue;
                    }
                    let admitted = self.admit(&entry, height, time, overlay, stage).await?;
                    let rejected = matches!(admitted.outcome, Outcome::Rejected(_));
                    receipts.push(admitted);
                    // a rejected init undoes the account with it, as a
                    // rejected unit's writes are undone
                    if rejected {
                        overlay.restore(checkpoint);
                        unit::discard_unrostered(self.store.storage(), overlay, stage)?;
                    }
                }
            }
        }
        for program in running.keys() {
            let dropped = !wanted.contains(program) && !held.contains(program);
            if dropped {
                overlay.delete(PROGRAMS, program.clone().into_bytes());
                if self.loaded.placed(program) {
                    overlay.delete(NETWORK, namespace::place(program));
                }
                self.loaded.unload(program);
                changed.push(program.clone());
            }
        }
        self.fill_roles(roles, overlay)?;
        for program in changed {
            let cancelled = self
                .source()
                .cancel_parked(&program, height, time, overlay, stage)
                .await?;
            receipts.extend(cancelled);
        }
        Ok(receipts)
    }

    async fn roster_roles(
        &self,
        entries: &[programs::Entry],
        height: u64,
        time: u64,
        overlay: &Overlay,
        stage: &Stage,
    ) -> Result<std::result::Result<Roles, Refusal>> {
        let reply = unit::query(
            self.world(height, time),
            vec![overlay],
            stage,
            &[],
            Origin::System,
            self.role(role::PROGRAMS)?.clone(),
            abi::encode(&programs::Query::Roles),
            &mut self.loaded.limits().fuel,
        )
        .await?;
        Ok(reply.and_then(|bytes| {
            let roles = match abi::decode::<programs::Reply>(&bytes) {
                Ok(programs::Reply::Roles(roles)) => roles,
                Ok(_) | Err(_) => {
                    return Err(Refusal::new(
                        reason::UNEXPECTED_REPLY,
                        "the programs role answered Roles with another reply",
                    ));
                }
            };
            roles.filled()?;
            let unlisted = roles
                .iter()
                .find(|(_, program)| !entries.iter().any(|entry| entry.program == **program));
            match unlisted {
                Some((role, program)) => Err(Refusal::new(
                    reason::INVALID_INPUT,
                    format!("the {role} role names {program}, which the roster does not run"),
                )),
                None => Ok(roles),
            }
        }))
    }

    fn fill_roles(&mut self, roles: Roles, overlay: &mut Overlay) -> Result<()> {
        let view = View::new(self.store.storage(), vec![&*overlay]);
        let running = programs_of(&view)?;
        let filled: Roles = roles
            .iter()
            .filter(|(role, program)| {
                role::REQUIRED.contains(role) || running.contains_key(*program)
            })
            .map(|(role, program)| (role, program.clone()))
            .collect();
        let recorded = roles_of(&view)?;
        if filled != recorded {
            overlay.set(NETWORK, namespace::ROLES.to_vec(), abi::encode(&filled));
        }
        self.roles = filled;
        Ok(())
    }

    fn log_roster(&mut self, height: u64, refusal: Refusal) {
        self.roster_streak += 1;
        let blocks = self.roster_streak;
        let due = blocks == 1 || blocks.is_multiple_of(STREAK_LOG_EVERY);
        if due {
            tracing::warn!(
                target: "ducktape::programs",
                event = "roster_refused",
                height,
                blocks,
                reason = %refusal.reason,
            );
        }
    }

    async fn renames(
        &self,
        height: u64,
        time: u64,
        overlay: &Overlay,
        stage: &Stage,
    ) -> Result<Vec<(ProgramId, ProgramId)>> {
        let reply = unit::query(
            self.world(height, time),
            vec![overlay],
            stage,
            &[],
            Origin::System,
            self.role(role::PROGRAMS)?.clone(),
            abi::encode(&programs::Query::Renames(height)),
            &mut self.loaded.limits().fuel,
        )
        .await?;
        let decoded = reply.map(|bytes| abi::decode::<programs::Reply>(&bytes));
        Ok(match decoded {
            Ok(Ok(programs::Reply::Renames(renames))) => renames,
            _ => Vec::new(),
        })
    }

    fn rename(
        &mut self,
        from: &str,
        to: &str,
        running: &mut BTreeMap<ProgramId, BlobId>,
        overlay: &mut Overlay,
    ) {
        let place = self.loaded.place(from).to_owned();
        if let Some(code) = running.remove(from) {
            overlay.delete(PROGRAMS, from.as_bytes().to_vec());
            overlay.set(PROGRAMS, to.as_bytes().to_vec(), abi::encode(&code));
            running.insert(to.to_owned(), code);
        }
        if self.loaded.placed(from) {
            overlay.delete(NETWORK, namespace::place(from));
        }
        overlay.set(NETWORK, namespace::place(to), place.clone().into_bytes());
        self.loaded.rename(from, to.to_owned(), place);
    }

    fn log_rename(&mut self, height: u64, from: &str, to: &str, refusal: &Refusal) {
        let blocks = self.rename_streaks.entry(from.to_owned()).or_default();
        *blocks += 1;
        let due = *blocks == 1 || blocks.is_multiple_of(STREAK_LOG_EVERY);
        if due {
            tracing::warn!(
                target: "ducktape::programs",
                event = "rename_refused",
                height,
                from,
                to,
                blocks = *blocks,
                reason = %refusal.reason,
                sentence = %refusal.sentence,
            );
        }
    }

    fn place_for(&self, program: &str, height: u64) -> String {
        match self.loaded.is_place(program) {
            true => format!("{program}@{height}"),
            false => program.to_owned(),
        }
    }

    async fn admit(
        &mut self,
        entry: &programs::Entry,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Receipt> {
        let module = match self.load(entry, stage)? {
            Ok(module) => module,
            Err(refusal) => return Ok(rejected(&entry.program, refusal)),
        };
        let place = self.place_for(&entry.program, height);
        let placed = place != entry.program;
        self.loaded.admit(entry.program.clone(), entry.code, module);
        if placed {
            self.loaded.set_place(entry.program.clone(), place.clone());
        }
        let env = Env {
            network: self.network.clone(),
            height,
            time,
            me: entry.program.clone(),
            origin: Origin::System,
            sender: Some(Principal::System),
            roles: self.roles.clone(),
            cause: Cause::Direct,
        };
        let receipt = self
            .source()
            .run(
                &entry.program,
                ProgramCall::Init(entry.params.clone()),
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
                if placed {
                    overlay.set(
                        NETWORK,
                        namespace::place(&entry.program),
                        place.clone().into_bytes(),
                    );
                }
                self.store.add_program(&place).await?;
            }
            Outcome::Rejected(_) => self.loaded.unload(&entry.program),
        }
        Ok(receipt)
    }

    /// Gives an admitted program its account: the identity role's
    /// `RegisterProgram`, run as the system. A refusal is the receipt's.
    async fn register(
        &self,
        program: &str,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Receipt> {
        let op = identity::Op::RegisterProgram {
            program: program.to_owned(),
        };
        self.tell_identity(op, height, time, overlay, stage).await
    }

    async fn rename_account(
        &self,
        from: &str,
        to: &str,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Receipt> {
        let op = identity::Op::RenameProgram {
            from: from.to_owned(),
            to: to.to_owned(),
        };
        self.tell_identity(op, height, time, overlay, stage).await
    }

    async fn tell_identity(
        &self,
        op: identity::Op,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Receipt> {
        let identity = self.role(role::IDENTITY)?;
        let env = Env {
            network: self.network.clone(),
            height,
            time,
            me: identity.clone(),
            origin: Origin::System,
            sender: Some(Principal::System),
            roles: self.roles.clone(),
            cause: Cause::Direct,
        };
        self.source()
            .run(
                identity,
                ProgramCall::Execute(abi::encode(&op)),
                env,
                overlay,
                stage,
            )
            .await
    }

    fn swap(
        &mut self,
        entry: &programs::Entry,
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
            nested: Vec::new(),
        })
    }

    fn load(
        &self,
        entry: &programs::Entry,
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

    async fn commit(
        &mut self,
        tip: Tip,
        mut overlay: Overlay,
        stage: Stage,
        admissions: Vec<Receipt>,
        submissions: Vec<Receipt>,
        records: Vec<hook::Record>,
    ) -> Result<Applied> {
        overlay.set(RECENT, namespace::height(tip.height), abi::encode(&tip.id));
        if tip.height >= self.layout_from {
            let row: Vec<u8> = overlay
                .range(LOCAL, b"", None, false)
                .flat_map(|(id, _)| id.iter().copied())
                .collect();
            if !row.is_empty() {
                overlay.set(SPENT, namespace::height(tip.height), row);
            }
        }
        if let Some(expired) = tip.height.checked_sub(ANCHOR_WINDOW) {
            self.forget(expired, &mut overlay)?;
        }
        overlay.set(
            LOCAL,
            namespace::INDEXED_AT.to_vec(),
            namespace::height(tip.height),
        );
        let mut writes = overlay.into_writes();
        let local = std::mem::take(&mut writes.local);
        self.blobs.promote(stage)?;
        let committed = Writes {
            programs: writes.programs.clone(),
            local,
        };
        self.store.commit(tip.height, committed).await?;
        self.roles = roles_of(&self.store.view(Vec::new()))?;
        // a commitment outlives neither its program's drop nor an undone
        // admission: the root is over the programs a reopened node opens
        let running: BTreeSet<ProgramId> = programs_in(self.store.storage())?.into_iter().collect();
        let stale: Vec<ProgramId> = self
            .store
            .programs()
            .filter(|program| !running.contains(*program))
            .cloned()
            .collect();
        for program in stale {
            self.store.remove_program(&program).await?;
        }
        Ok(Applied {
            height: tip.height,
            admissions,
            submissions,
            records,
            writes,
            root: self.root()?,
            places: Arc::new(self.loaded.places().clone()),
        })
    }

    pub async fn drop_unnamed(&self, ids: &BTreeSet<BlobId>, time: u64) -> Result<()> {
        for id in self.source().unnamed(ids, time).await? {
            self.blobs.remove(&id)?;
        }
        Ok(())
    }

    async fn unlink(&self, writes: &Writes, time: u64) {
        let Some(roster) = writes.programs.get(BLOBS) else {
            return;
        };
        let dropped: Vec<&Vec<u8>> = roster
            .iter()
            .filter(|(key, slot)| slot.is_none() && !namespace::is_index(key))
            .map(|(key, _)| key)
            .collect();
        if dropped.is_empty() {
            return;
        }
        let running: BTreeSet<BlobId> = match self.programs() {
            Ok(programs) => programs.into_values().collect(),
            Err(error) => return kept_on_disk("running_unread", None, &error.to_string()),
        };
        for key in dropped {
            let id = match abi::decode::<BlobId>(key) {
                Ok(id) => id,
                Err(refusal) => {
                    kept_on_disk("roster_key", None, &refusal.sentence);
                    continue;
                }
            };
            if running.contains(&id) {
                continue;
            }
            let still_read = match self.readable(&id, time).await {
                Ok(readable) => readable,
                Err(error) => {
                    kept_on_disk("readable_unread", Some(id), &error.to_string());
                    continue;
                }
            };
            if still_read {
                continue;
            }
            if let Err(error) = self.blobs.remove(&id) {
                kept_on_disk("remove_failed", Some(id), &error.to_string());
            }
        }
    }

    fn forget(&self, height: u64, overlay: &mut Overlay) -> Result<()> {
        let key = namespace::height(height);
        let (rows, ids) = {
            let view = self.store.view(vec![&*overlay]);
            if height < self.layout_from {
                let anchored = view.scan(SPENT, &Scan::prefix(&key))?;
                let ids: Vec<Vec<u8>> = anchored
                    .iter()
                    .map(|entry| entry.key[key.len()..].to_vec())
                    .collect();
                (anchored.into_iter().map(|entry| entry.key).collect(), ids)
            } else {
                match view.get(SPENT, &key)? {
                    Some(row) => (
                        vec![key.clone()],
                        namespace::ids(&row)?.map(<[u8]>::to_vec).collect(),
                    ),
                    None => (Vec::new(), Vec::new()),
                }
            }
        };
        for id in ids {
            overlay.delete(LOCAL, id);
        }
        for row in rows {
            overlay.delete(SPENT, row);
        }
        overlay.delete(RECENT, key);
        Ok(())
    }
}

struct Rebuilt {
    ids: usize,
    largest: usize,
}

fn rebuild_index(storage: &Storage, height: u64, most_bytes: usize) -> Result<Rebuilt> {
    storage.clear_local()?;
    let mut rebuilt = Rebuilt { ids: 0, largest: 0 };
    let mut batch: Vec<Vec<u8>> = Vec::new();
    for entry in storage.iter(SPENT, b"", None, false)? {
        let (key, row) = entry?;
        for id in namespace::carried(&key, &row)? {
            let past = (batch.len() + 1) * id.len() > most_bytes;
            if past {
                write_ids(storage, &mut batch, &mut rebuilt)?;
            }
            batch.push(id.to_vec());
        }
    }
    write_ids(storage, &mut batch, &mut rebuilt)?;
    let at = namespace::height(height);
    storage.write_local([(namespace::INDEXED_AT.to_vec(), Some(at))])?;
    Ok(rebuilt)
}

fn write_ids(storage: &Storage, batch: &mut Vec<Vec<u8>>, rebuilt: &mut Rebuilt) -> Result<()> {
    rebuilt.ids += batch.len();
    rebuilt.largest = rebuilt.largest.max(batch.len());
    let slots = batch.drain(..).map(|id| (id, Some(Vec::new())));
    Ok(storage.write_local(slots)?)
}

fn kept_on_disk(reason: &'static str, blob: Option<BlobId>, error: &str) {
    tracing::warn!(
        target: "ducktape::blobs",
        event = "blob_unlink_failed",
        reason,
        blob = ?blob,
        error,
        "a blob its block released stays on disk; the block is applied"
    );
}

fn gated<'c>(call: &'c ProgramCall, env: &Env) -> Option<&'c [u8]> {
    let ProgramCall::Execute(payload) = call else {
        return None;
    };
    let completes = matches!(env.cause, Cause::Completion { .. });
    let by_the_system = env.origin == Origin::System;
    match completes || by_the_system {
        true => None,
        false => Some(payload),
    }
}

fn needs_of(program: &str, bytes: &[u8]) -> std::result::Result<Vec<access::Need>, Refusal> {
    abi::decode(bytes).map_err(|refusal| {
        Refusal::new(
            reason::UNEXPECTED_REPLY,
            format!(
                "{program} answered what its op touches with {}",
                refusal.sentence
            ),
        )
    })
}

fn foreign_sender(
    program: &str,
    env: &Env,
    acting: Option<identity::AccountNumber>,
    foreign: Foreign,
) -> Sender {
    let bound_to_crosspond = env.roles.get(role::CROSSPOND).map(String::as_str) == Some(program);
    match (bound_to_crosspond, acting) {
        (false, _) => Err(Refusal::new(
            reason::UNAUTHORIZED,
            "only the program bound to the crosspond role sends as a foreign user",
        )),
        (true, Some(account)) => Err(Refusal::new(
            reason::INVALID_INPUT,
            format!("a message sent as a foreign user cannot also act as account {account}"),
        )),
        (true, None) => Ok(Some(Principal::Foreign(foreign))),
    }
}

fn actor_of(env: &Env) -> Vec<access::Party> {
    let account = match env.sender {
        Some(Principal::Account(number)) => Some(access::Party::Account(number)),
        Some(Principal::Foreign(foreign)) => Some(access::Party::Foreign(foreign)),
        Some(Principal::System) | None => None,
    };
    let program = match &env.origin {
        Origin::Program(program) => Some(access::Party::Program(program.clone())),
        Origin::External(_) | Origin::System => None,
    };
    account.into_iter().chain(program).collect()
}

/// A founding run's receipt, or the refusal that stops the founding.
fn founded(entry: &programs::Entry, receipt: Receipt) -> Result<Receipt> {
    match &receipt.outcome {
        Outcome::Applied { .. } => Ok(receipt),
        Outcome::Rejected(refusal) => Err(Error::Genesis {
            program: entry.program.clone(),
            refusal: refusal.clone(),
        }),
    }
}

fn put_code(overlay: &mut Overlay, stage: &mut Stage, code: &[u8]) -> BlobId {
    let id = stage
        .put(HashKind::Sha256, CODE_KIND, code)
        .expect("the code kind is one word");
    overlay.set(BLOBS, namespace::blob(&id), Vec::new());
    id
}

pub(crate) fn rejected(program: &str, refusal: Refusal) -> Receipt {
    Receipt {
        program: program.to_owned(),
        outcome: Outcome::Rejected(refusal),
        events: Vec::new(),
        nested: Vec::new(),
    }
}

fn programs_in(storage: &Reads) -> Result<Vec<ProgramId>> {
    let places = places_of(&View::new(storage, Vec::new()))?;
    let mut programs: Vec<ProgramId> = RESERVED.map(str::to_owned).to_vec();
    for entry in storage.iter(PROGRAMS, b"", None, false)? {
        let (key, _) = entry?;
        let program = program_id(key)?;
        programs.push(places.place(&program).to_owned());
    }
    Ok(programs)
}

/// The programs the last committed block dropped from the roster.
fn dropped_in(storage: &Reads) -> Result<Vec<ProgramId>> {
    let live: BTreeSet<ProgramId> = programs_in(storage)?.into_iter().collect();
    let dropped: Vec<ProgramId> = storage
        .pending_deleted(PROGRAMS)?
        .into_iter()
        .map(program_id)
        .collect::<Result<_>>()?;
    Ok(dropped
        .into_iter()
        .filter(|program| !live.contains(program))
        .collect())
}

fn places_of(view: &View<'_>) -> Result<Places> {
    let rows = view.scan(NETWORK, &Scan::prefix(namespace::PLACES))?;
    let mut places = BTreeMap::new();
    for entry in rows {
        let id = entry.key.get(namespace::PLACES.len()..).unwrap_or_default();
        places.insert(program_id(id.to_vec())?, program_id(entry.value)?);
    }
    Ok(Places::new(places))
}

fn renaming(
    asked: Vec<(ProgramId, ProgramId)>,
    running: &BTreeMap<ProgramId, BlobId>,
    entries: &[programs::Entry],
) -> Vec<(ProgramId, ProgramId)> {
    let listed = |program: &str| entries.iter().any(|entry| entry.program == program);
    let mut froms = BTreeSet::new();
    let mut tos = BTreeSet::new();
    asked
        .into_iter()
        .filter(|(from, to)| {
            let runs = running.contains_key(from) && !running.contains_key(to);
            let moves = listed(to) && !listed(from) && valid_program_id(to);
            runs && moves && froms.insert(from.clone()) && tos.insert(to.clone())
        })
        .collect()
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

fn fresh(
    view: &View<'_>,
    submission: &Submission,
    id: &SubmissionId,
    height: u64,
) -> Result<std::result::Result<(), Refusal>> {
    let anchor = submission.anchor;
    let recorded = view.get(RECENT, &namespace::height(anchor.height))?;
    let names_a_recent_block = anchor.anchors(height) && recorded == Some(abi::encode(&anchor.id));
    if !names_a_recent_block {
        return Ok(Err(Refusal::new(
            reason::ANCHOR,
            format!(
                "the frame's anchor is not one of the {ANCHOR_WINDOW} blocks before block {height}"
            ),
        )));
    }
    let carried = view.get(LOCAL, id)?.is_some();
    if carried {
        return Ok(Err(Refusal::new(
            reason::DUPLICATE,
            "a block already carried this frame",
        )));
    }
    Ok(Ok(()))
}

fn tidier_of<E: Context + Spawner>(store: &Store<E>, blobs: &Blobs) -> Result<Tidier> {
    let metrics = store.clock();
    let copied = metrics.counter(
        "blob_tidy_copied_bytes",
        "bytes the blob store's tidy rounds wrote into its copy segments",
    );
    let moved = metrics.counter(
        "blob_tidy_moved_files",
        "files an older build kept one per blob that tidy rounds moved into the store",
    );
    let retired = metrics.counter(
        "blob_tidy_retired_segments",
        "segments the blob store's tidy rounds emptied and removed",
    );
    let rounds = metrics.histogram(
        "blob_tidy_round_seconds",
        "how long one blob store tidy round took, on its own thread",
        Buckets::NETWORK,
    );
    let tidier = Tidier::spawn(blobs.clone(), move |tidied, spent| {
        copied.inc_by(tidied.copied);
        moved.inc_by(tidied.moved);
        retired.inc_by(tidied.retired);
        rounds.observe(spent.as_secs_f64());
    })?;
    Ok(tidier)
}

pub(crate) fn sync_code(loaded: &mut Loaded, view: &View<'_>, blobs: &Blobs) -> Result<()> {
    let limits = limits_of(view)?;
    let limits_changed = loaded.limits() != limits;
    if limits_changed {
        *loaded = Loaded::new(limits, loaded.query_fuel());
    }
    let wanted = programs_of(view)?;
    loaded.set_places(places_of(view)?);
    loaded.sync(&wanted, blobs)
}

pub(crate) fn runnable(loaded: &Loaded) -> Result<()> {
    match loaded.missing().first() {
        Some((_, code)) => Err(Error::BlobUnavailable(*code)),
        None => Ok(()),
    }
}

fn limits_of(view: &View<'_>) -> Result<Limits> {
    match view.get(NETWORK, namespace::LIMITS)? {
        Some(bytes) => abi::decode(&bytes).map_err(corrupt),
        None => Ok(Limits::default()),
    }
}

fn member_cap_of(view: &View<'_>) -> Result<u32> {
    let bytes = view.get(NETWORK, namespace::MEMBER_CAP)?.ok_or_else(|| {
        Error::Corrupt(
            "the network records no member cap: it was founded before member caps; found it again"
                .into(),
        )
    })?;
    let cap: u32 = abi::decode(&bytes).map_err(corrupt)?;
    if !(1..=validators::MAX_MEMBERS).contains(&cap) {
        return Err(Error::Corrupt(format!(
            "the network's member cap {cap} is outside 1..={}",
            validators::MAX_MEMBERS
        )));
    }
    Ok(cap)
}

pub(crate) fn bound<'r>(roles: &'r Roles, role: &str) -> Result<&'r ProgramId> {
    roles
        .get(role)
        .ok_or_else(|| Error::Unfilled { role: role.into() })
}

/// The roles table as `view` holds it.
pub(crate) fn roles_of(view: &View<'_>) -> Result<Roles> {
    let roles = view
        .get(NETWORK, namespace::ROLES)?
        .ok_or_else(|| Error::Corrupt("the network records no roles".into()))?;
    abi::decode(&roles).map_err(corrupt)
}

fn corrupt(refusal: Refusal) -> Error {
    Error::Corrupt(refusal.sentence)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use sha2::{Digest as _, Sha256};
    use state::{LOCAL, Storage, Writes};

    use abi::BlobId;
    use state::Overlay;

    use super::{Asked, ReadSet, SPENT, namespace, rebuild_index};

    #[test]
    fn an_asked_account_holds_until_a_program_it_read_is_written_or_its_code_changes() {
        let code = Some(BlobId::Sha256([1; 32]));
        let mut overlay = Overlay::default();
        overlay.set("probe", b"k".to_vec(), b"v".to_vec());
        let mut reads = ReadSet::default();
        reads.point("identity", b"signer");
        let asked = Asked {
            code,
            reply: Ok(Some(5)),
            reads,
            mark: overlay.mark(),
        };
        overlay.set("probe", b"k2".to_vec(), b"v".to_vec());
        assert!(asked.holds(code, &overlay));
        assert!(!asked.holds(Some(BlobId::Sha256([2; 32])), &overlay));
        let checkpoint = overlay.checkpoint();
        overlay.set("identity", b"other".to_vec(), b"v".to_vec());
        overlay.restore(checkpoint);
        assert!(!asked.holds(code, &overlay));
    }

    #[test]
    fn a_rebuild_writes_no_batch_past_its_bound_whatever_one_anchor_holds() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        let id = |n: u32| -> [u8; 32] { Sha256::digest(n.to_be_bytes()).into() };
        let anchored = 0..1000;
        let carried = 1000..1010;
        let mut spent: BTreeMap<Vec<u8>, Option<Vec<u8>>> = anchored
            .clone()
            .map(|n| (namespace::spent(5, &id(n)), Some(Vec::new())))
            .collect();
        let row = carried.clone().flat_map(id).collect();
        spent.insert(namespace::height(7), Some(row));
        let writes = Writes {
            programs: BTreeMap::from([(SPENT.to_owned(), spent)]),
            local: BTreeMap::new(),
        };
        storage.commit(7, &writes).unwrap();

        let block = 100 * size_of::<[u8; 32]>();
        let rebuilt = rebuild_index(&storage, 7, block).unwrap();
        assert_eq!(rebuilt.ids, 1010);
        assert_eq!(rebuilt.largest, 100);
        let indexed: Vec<Vec<u8>> = storage
            .iter(LOCAL, b"", None, false)
            .unwrap()
            .map(|entry| entry.unwrap().0)
            .filter(|key| key != namespace::INDEXED_AT)
            .collect();
        let mut expected: Vec<Vec<u8>> = anchored.chain(carried).map(|n| id(n).to_vec()).collect();
        expected.sort();
        assert_eq!(indexed, expected);
        assert_eq!(
            storage.get(LOCAL, namespace::INDEXED_AT).unwrap(),
            Some(namespace::height(7))
        );
    }
}
