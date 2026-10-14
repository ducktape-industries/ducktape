mod crypto;
mod namespace;
mod parked;
mod parking;
mod unit;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The founding program that fills each role the kernel calls. A genesis
/// that leaves a role empty, or names a program it does not found, is refused.
pub use abi::Roles;
use abi::{
    BlobId, Cause, Env, HashKind, Invocation, ItemRef, Message, Origin, Outcome, Principal,
    ProgramCall, ProgramId, Refusal, Root, Scan, hook, reason,
    role::{identity, programs, validators},
};
use blob::{Blobs, Layered, Stage};
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_runtime::Spawner;
use commonware_storage::Context;
use runtime::{Fault, Suspended};
use sha2::{Digest as _, Sha256};
use state::{Commitment, Overlay, Storage, Store, View, Writes, valid_program_id};

use crate::parked::Waiter;
use crate::parking::{Run, Segment, Start, Status};
use crate::unit::{Frame, Loaded, World, refusal_of};

pub use namespace::{BLOBS, NETWORK, PARKED, PROGRAMS, RECENT, RESERVED, SPENT, spent};
pub use parking::PAGE_FUEL;
pub use runtime::Limits;

const STATE_DIR: &str = "state";
const BLOBS_DIR: &str = "blobs";
const CODE_KIND: &str = "program";
/// How deep messages nest in one frame: a submission runs at depth 0 and
/// each message, or reply, one deeper than the run that emitted it.
pub const MAX_DEPTH: u32 = 8;
pub const ANCHOR_WINDOW: u64 = 256;
pub const HOOK_FUEL: u64 = 2_000_000;
const HOOK_LOG_EVERY: u64 = 64;

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
    #[error("genesis binds the {role} role to {program:?}, which is not a founding program")]
    Unbound {
        role: &'static str,
        program: ProgramId,
    },
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
}

pub type Result<T> = std::result::Result<T, Error>;

pub type BlockId = [u8; 32];
pub type SubmissionId = [u8; 32];

pub struct Genesis {
    pub network: Vec<u8>,
    pub roles: Roles,
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
}

impl Submission {
    pub fn id(&self) -> SubmissionId {
        Sha256::digest(abi::encode(self)).into()
    }
}

/// One frame's run. A rejected receipt's run wrote nothing: the outcome
/// is the refusal of the run itself or, propagated, of a run nested in it.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Receipt {
    pub program: ProgramId,
    pub outcome: Outcome,
    pub events: Vec<Vec<u8>>,
    /// A nested receipt's `Applied` and events stand only when every
    /// receipt above it was applied too: an ancestor's rejection undid it.
    pub nested: Vec<Receipt>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    pub height: u64,
    pub admissions: Vec<Receipt>,
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
    roles: Roles,
    loaded: Loaded,
    preconfirmed: Overlay,
    hook_streaks: BTreeMap<ProgramId, u64>,
}

enum Submitted {
    Stale(Receipt),
    Carried {
        receipt: Receipt,
        account: Option<identity::AccountNumber>,
    },
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
        overlay.set(
            NETWORK,
            namespace::ROLES.to_vec(),
            abi::encode(&genesis.roles),
        );
        let mut entries: Vec<programs::Entry> = genesis
            .programs
            .into_iter()
            .map(|founding| programs::Entry {
                program: founding.program,
                code: put_code(&mut overlay, &mut stage, &founding.code),
                params: founding.params,
            })
            .collect();
        let roles = &genesis.roles;
        for (role, program) in [
            ("programs", &roles.programs),
            ("validators", &roles.validators),
            ("identity", &roles.identity),
            ("blobs", &roles.blobs),
            ("awaits", &roles.awaits),
        ] {
            let founded = entries.iter().any(|entry| entry.program == *program);
            if !founded {
                return Err(Error::Unbound {
                    role,
                    program: program.clone(),
                });
            }
        }
        for hook in &genesis.hooks {
            let founded = entries.iter().any(|entry| entry.program == *hook);
            if !founded {
                return Err(Error::Unhooked(hook.clone()));
            }
        }
        let founding_roles = [
            &roles.programs,
            &roles.validators,
            &roles.identity,
            &roles.blobs,
        ];
        entries.sort_by_key(|entry| {
            founding_roles
                .iter()
                .position(|role| **role == entry.program)
                .unwrap_or(founding_roles.len())
        });
        let params = abi::encode(&validators::Genesis {
            validators: genesis.validators,
            member_cap: genesis.member_cap,
        });
        for entry in entries.iter_mut() {
            if entry.program == roles.validators {
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
        let mut host = Host {
            store,
            blobs,
            network: genesis.network,
            roles: genesis.roles.clone(),
            loaded: Loaded::new(genesis.limits),
            preconfirmed: Overlay::default(),
            hook_streaks: BTreeMap::new(),
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
            )
            .await?;
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
        let roles = store
            .view(Vec::new())
            .get(NETWORK, namespace::ROLES)?
            .ok_or_else(|| Error::Corrupt("the network records no roles".into()))?;
        let roles = abi::decode(&roles).map_err(corrupt)?;
        let mut host = Host {
            store,
            blobs,
            network,
            roles,
            loaded: Loaded::new(limits),
            preconfirmed: Overlay::default(),
            hook_streaks: BTreeMap::new(),
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
            .get(RECENT, &namespace::height(height))?
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

    /// How many members the network ever holds at once, as founded.
    pub fn member_cap(&self) -> Result<u32> {
        member_cap_of(&self.store.view(Vec::new()))
    }

    pub fn epoch_members(&self, epoch: u64) -> Result<Option<Vec<validators::Member>>> {
        self.store
            .view(Vec::new())
            .get(NETWORK, &namespace::epoch(epoch))?
            .map(|bytes| abi::decode(&bytes).map_err(corrupt))
            .transpose()
    }

    /// The keys that vote and propose in `epoch`: the validators program's
    /// `Validators` answer. [`Host::epoch_members`] is who the mesh admits,
    /// residents included; consensus seats only these.
    pub fn epoch_validators(&self, epoch: u64) -> Result<Option<Vec<Vec<u8>>>> {
        self.store
            .view(Vec::new())
            .get(NETWORK, &namespace::epoch_validators(epoch))?
            .map(|bytes| abi::decode(&bytes).map_err(corrupt))
            .transpose()
    }

    pub fn roles(&self) -> &Roles {
        &self.roles
    }

    pub fn root(&self) -> Result<Root> {
        let roots = self.store.roots()?;
        Ok(Root(Sha256::digest(abi::encode(&roots)).into()))
    }

    pub fn view(&self, layer: Layer) -> View<'_> {
        self.store.view(self.layers(layer))
    }

    pub fn fresh(&self, submission: &Submission) -> Result<std::result::Result<(), Refusal>> {
        fresh(
            &self.view(Layer::Confirmed),
            submission,
            self.next_height()?,
        )
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
        let view = self.store.view(Vec::new());
        let mut named = BTreeSet::new();
        for entry in view.scan(BLOBS, &Scan::prefix(b""))? {
            named.insert(abi::decode::<BlobId>(&entry.key).map_err(corrupt)?);
        }
        named.extend(programs_of(&view)?.into_values());
        Ok(named.into_iter().filter(|id| !self.blobs.has(id)).collect())
    }

    pub fn install(&mut self, id: BlobId, framed: &[u8]) -> Result<()> {
        let hashes_to_id = blob::id_of(id.kind(), framed) == id;
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
        let height = self.height()?;
        self.readable_in(id, height, time, &Overlay::default(), &Stage::default())
            .await
    }

    async fn readable_in(
        &self,
        id: &BlobId,
        height: u64,
        time: u64,
        overlay: &Overlay,
        stage: &Stage,
    ) -> Result<bool> {
        let rostered = View::new(self.store.storage(), vec![overlay])
            .get(BLOBS, &namespace::blob(id))?
            .is_some();
        if rostered {
            return Ok(true);
        }
        unit::held(
            self.world(height, time),
            vec![overlay],
            stage,
            &[],
            id,
            &mut self.loaded.limits().fuel,
        )
        .await
    }

    pub async fn blob(&self, id: &BlobId, time: u64) -> Result<Option<Vec<u8>>> {
        if !self.readable(id, time).await? {
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
            roles: &self.roles,
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
            &mut self.loaded.limits().fuel,
        )
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
            self.roles.validators.clone(),
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
            let submitted = self
                .submit(submission, height, time, &mut overlay, &mut stage)
                .await?;
            receipts.push(submitted.receipt());
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
        let mut hooks = self
            .hooks(block.height, block.time, &overlay, &stage)
            .await?;
        let mut submissions = Vec::new();
        for (position, submission) in block.submissions.into_iter().enumerate() {
            let signer = submission.signer.clone();
            let target = submission.target.clone();
            let submitted = self
                .submit(
                    submission,
                    block.height,
                    block.time,
                    &mut overlay,
                    &mut stage,
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
                    self.tell(&mut hooks, &record, block.time, &mut overlay, &mut stage)
                        .await?;
                    receipt
                }
            };
            submissions.push(receipt);
        }
        self.log_hooks(block.height, hooks);
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
    ) -> Result<Submitted> {
        let view = self.store.view(vec![&*overlay]);
        if let Err(refusal) = fresh(&view, &submission, height)? {
            return Ok(Submitted::Stale(rejected(&submission.target, refusal)));
        }
        overlay.set(SPENT, namespace::spent(&submission), Vec::new());
        let asked = identity::Query::Account(submission.signer.clone());
        let fuel = &mut self.loaded.limits().fuel;
        let account = match self
            .account(asked, height, time, overlay, stage, fuel)
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
        let env = Env {
            network: self.network.clone(),
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
            .run(
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
            tallies: BTreeMap::new(),
        };
        let reply = unit::query(
            self.world(height, time),
            vec![overlay],
            stage,
            &[],
            Origin::System,
            self.roles.programs.clone(),
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
        for hook in listed {
            let runs = self.loaded.module(&hook).is_some();
            let first = !hooks.programs.contains(&hook);
            if runs && first {
                hooks.programs.push(hook);
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
            let refused = self
                .call_hook(program, record, time, overlay, stage)
                .await?;
            let tally = hooks.tallies.entry(program.clone()).or_default();
            let Some(refusal) = refused else {
                continue;
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
        }
        Ok(())
    }

    async fn call_hook(
        &self,
        program: &str,
        record: &hook::Record,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Option<Refusal>> {
        let checkpoint = overlay.checkpoint();
        let env = Env {
            network: self.network.clone(),
            height: record.height,
            time,
            me: program.to_owned(),
            origin: Origin::System,
            sender: Some(Principal::System),
            roles: self.roles.clone(),
            cause: Cause::Direct,
        };
        let mut frame = Frame {
            fuel: self.loaded.limits().fuel.map(|_| HOOK_FUEL),
            first_item: 0,
            next_item: 0,
        };
        let invocation = Invocation {
            env,
            call: ProgramCall::Hook(record.clone()),
        };
        let (receipt, effects) = unit::execute(
            self.world(record.height, time),
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
                unit::discard_unrostered(self.store.storage(), overlay, stage)?;
                Refusal::new(reason::UNSUPPORTED, "a hook sends no messages")
            }
        };
        Ok(Some(refusal))
    }

    fn log_hooks(&mut self, height: u64, hooks: Hooks) {
        for (program, tally) in hooks.tallies {
            let Some(reason) = tally.reason else {
                self.hook_streaks.remove(&program);
                continue;
            };
            let blocks = self.hook_streaks.entry(program.clone()).or_default();
            *blocks += 1;
            let due = *blocks == 1 || blocks.is_multiple_of(HOOK_LOG_EVERY);
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
    ) -> Result<std::result::Result<Option<Principal>, Refusal>> {
        let asked = identity::Query::OfProgram(program.to_owned());
        let account = self
            .account(asked, height, time, overlay, stage, &mut frame.fuel)
            .await?;
        Ok(account.map(|account| account.map(Principal::Account)))
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
    ) -> Result<std::result::Result<Option<Principal>, Refusal>> {
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
        let reply = unit::query(
            self.world(height, time),
            vec![overlay],
            stage,
            &[],
            Origin::System,
            self.roles.identity.clone(),
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
        let mut frame = self.open_frame(self.loaded.limits().fuel, overlay)?;
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

    fn open_frame(&self, fuel: Option<u64>, overlay: &Overlay) -> Result<Frame> {
        let view = self.store.view(vec![overlay]);
        let next_item = parked::counter(&view, NETWORK, namespace::NEXT_ITEM)?;
        Ok(Frame {
            fuel,
            first_item: next_item,
            next_item,
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
        let segment = self
            .segment(program, &env, None, call, overlay, stage, frame, depth)
            .await?;
        let start = Start {
            frame: None,
            waiter,
            checkpoint,
        };
        self.follow(program, env, start, segment, overlay, stage, frame, depth)
            .await
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
        } = effects;
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
            unit::discard_unrostered(self.store.storage(), overlay, stage)?;
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

    #[allow(clippy::too_many_arguments)]
    async fn deliver(
        &self,
        program: &str,
        env: &Env,
        emitted: Vec<(ItemRef, Message)>,
        receipt: &mut Receipt,
        overlay: &mut Overlay,
        stage: &mut Stage,
        frame: &mut Frame,
        depth: u32,
    ) -> Result<Option<Outcome>> {
        let (height, time) = (env.height, env.time);
        // the emitter's account, asked once for all its messages
        let as_itself = emitted.iter().any(|(_, message)| message.acting.is_none());
        let emitter = if as_itself {
            self.account_of(program, height, time, overlay, stage, frame)
                .await?
        } else {
            Ok(None)
        };
        for (item, message) in emitted {
            let sender = match message.acting {
                None => emitter.clone(),
                Some(account) => {
                    self.run_by(program, account, height, time, overlay, stage, frame)
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
            network: self.network.clone(),
            height,
            time,
            me: me.to_owned(),
            origin: Origin::Program(origin.to_owned()),
            sender,
            roles: self.roles.clone(),
            cause,
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
            self.roles.programs.clone(),
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
        let running = programs_of(&View::new(self.store.storage(), vec![&*overlay]))?;
        let mut receipts = Vec::new();
        let mut wanted = BTreeSet::new();
        let mut changed = Vec::new();
        for entry in entries {
            let admissible = valid_program_id(&entry.program) && !wanted.contains(&entry.program);
            if !admissible {
                continue;
            }
            let readable = self
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
            let dropped = !wanted.contains(program);
            if dropped {
                overlay.delete(PROGRAMS, program.clone().into_bytes());
                self.loaded.unload(program);
                changed.push(program.clone());
            }
        }
        for program in changed {
            let cancelled = self
                .cancel_parked(&program, height, time, overlay, stage)
                .await?;
            receipts.extend(cancelled);
        }
        Ok(receipts)
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
        self.loaded.admit(entry.program.clone(), entry.code, module);
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
                self.store.add_program(&entry.program).await?;
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
        let env = Env {
            network: self.network.clone(),
            height,
            time,
            me: self.roles.identity.clone(),
            origin: Origin::System,
            sender: Some(Principal::System),
            roles: self.roles.clone(),
            cause: Cause::Direct,
        };
        let op = identity::Op::RegisterProgram {
            program: program.to_owned(),
        };
        self.run(
            &self.roles.identity,
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
    ) -> Result<Applied> {
        overlay.set(RECENT, namespace::height(tip.height), abi::encode(&tip.id));
        if let Some(expired) = tip.height.checked_sub(ANCHOR_WINDOW) {
            self.forget(expired, &mut overlay)?;
        }
        let writes = overlay.into_writes();
        self.blobs.promote(stage)?;
        self.store.commit(tip.height, writes.clone()).await?;
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
        self.preconfirmed = Overlay::default();
        Ok(Applied {
            height: tip.height,
            admissions,
            submissions,
            writes,
            root: self.root()?,
        })
    }

    fn forget(&self, height: u64, overlay: &mut Overlay) -> Result<()> {
        let key = namespace::height(height);
        let spent = self
            .store
            .view(vec![&*overlay])
            .scan(SPENT, &Scan::prefix(&key))?;
        for entry in spent {
            overlay.delete(SPENT, entry.key);
        }
        overlay.delete(RECENT, key);
        Ok(())
    }
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

fn programs_in(storage: &Storage) -> Result<Vec<ProgramId>> {
    let mut programs: Vec<ProgramId> = RESERVED.map(str::to_owned).to_vec();
    for entry in storage.iter(PROGRAMS, b"", None, false)? {
        let (key, _) = entry?;
        programs.push(program_id(key)?);
    }
    Ok(programs)
}

/// The programs the last committed block dropped from the roster.
fn dropped_in(storage: &Storage) -> Result<Vec<ProgramId>> {
    let Some(pending) = storage.pending()? else {
        return Ok(Vec::new());
    };
    let Some(roster) = pending.programs.get(PROGRAMS) else {
        return Ok(Vec::new());
    };
    roster
        .iter()
        .filter(|(_, slot)| slot.is_none())
        .map(|(key, _)| program_id(key.clone()))
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
    let carried = view.get(SPENT, &namespace::spent(submission))?.is_some();
    if carried {
        return Ok(Err(Refusal::new(
            reason::DUPLICATE,
            "a block already carried this frame",
        )));
    }
    Ok(Ok(()))
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

fn corrupt(refusal: Refusal) -> Error {
    Error::Corrupt(refusal.sentence)
}
