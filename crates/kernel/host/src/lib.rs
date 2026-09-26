mod crypto;
mod namespace;
mod unit;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The founding program that fills each role the kernel calls. A genesis
/// that leaves a role empty, or names a program it does not found, is refused.
pub use abi::Roles;
use abi::{
    BlobId, Cause, Env, GuestCall, HashKind, Invocation, Origin, Outcome, Principal, ProgramId,
    Refusal, Root, Scan, reason,
    role::{identity, registry, validators},
};
use blobs::{Blobs, Layered, Stage};
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_runtime::Spawner;
use commonware_storage::Context;
use runtime::Fault;
use sha2::{Digest as _, Sha256};
use state::{Commitment, Overlay, Storage, Store, View, Writes, valid_program_id};

use crate::unit::{Frame, Loaded, World, refusal_of};

pub use namespace::{BLOBS, NETWORK, PROGRAMS, RESERVED, SIGNERS};
pub use runtime::Limits;

const STATE_DIR: &str = "state";
const BLOBS_DIR: &str = "blobs";
const CODE_KIND: &str = "program";
/// How deep messages nest in one frame: a submission runs at depth 0 and
/// each message, or reply, one deeper than the run that emitted it.
pub const MAX_DEPTH: u32 = 8;

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
    #[error("genesis binds the {role} role to {program:?}, which is not a founding program")]
    Unbound {
        role: &'static str,
        program: ProgramId,
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
    pub roles: Roles,
    /// The validators program's founding params: the kernel writes them.
    pub validators: Vec<validators::Member>,
    /// The registry's founding params are the kernel's too: every founding
    /// program and view.
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

/// One frame's run. A rejected receipt's frame wrote nothing: the outcome
/// is the refusal of the run itself or, propagated, of a run nested in it.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Receipt {
    pub program: ProgramId,
    pub outcome: Outcome,
    pub events: Vec<Vec<u8>>,
    /// The runs this one's messages caused, in order: each message's
    /// target, then, when a reply was wanted, this program's reply run.
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
            roles: genesis.roles.clone(),
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
        overlay.set(
            NETWORK,
            namespace::ROLES.to_vec(),
            abi::encode(&genesis.roles),
        );
        let mut entries: Vec<registry::Entry> = genesis
            .programs
            .into_iter()
            .map(|founding| registry::Entry {
                program: founding.program,
                code: put_code(&mut overlay, &mut stage, &founding.code),
                params: founding.params,
            })
            .collect();
        let roles = &genesis.roles;
        for (role, program) in [
            ("registry", &roles.registry),
            ("validators", &roles.validators),
            ("identity", &roles.identity),
        ] {
            let founded = entries.iter().any(|entry| entry.program == *program);
            if !founded {
                return Err(Error::Unbound {
                    role,
                    program: program.clone(),
                });
            }
        }
        // the registry, the validators, then identity are admitted before
        // the rest
        let founding_roles = [&roles.registry, &roles.validators, &roles.identity];
        entries.sort_by_key(|entry| {
            founding_roles
                .iter()
                .position(|role| **role == entry.program)
                .unwrap_or(founding_roles.len())
        });
        let params = abi::encode(&validators::Genesis {
            validators: genesis.validators,
        });
        for entry in entries.iter_mut() {
            if entry.program == roles.validators {
                entry.params = params.clone();
            }
        }
        let views: Vec<registry::View> = genesis
            .views
            .into_iter()
            .map(|founding| registry::View {
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
        entries[0].params = abi::encode(&registry::Genesis {
            programs: entries.clone(),
            views,
        });
        // the roles init before identity can give an account; every other
        // program has its account before its init runs, so what it emits
        // carries it
        let is_role = |entry: &registry::Entry| founding_roles.contains(&&entry.program);
        let (role_entries, rest) = entries.split_at(entries.partition_point(is_role));
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

    pub fn epoch_members(&self, epoch: u64) -> Result<Option<Vec<validators::Member>>> {
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
            self.roles.validators.clone(),
            abi::encode(&validators::Query::Members),
            &mut self.loaded.limits().fuel,
        )
        .await?;
        let bytes = reply.map_err(|refusal| {
            Error::Corrupt(format!(
                "the validators program refused the members query: {refusal}"
            ))
        })?;
        let validators::Reply::Members(members) = abi::decode(&bytes).map_err(corrupt)? else {
            return Err(Error::Corrupt(
                "the validators program answered Members with another reply".into(),
            ));
        };
        overlay.set(NETWORK, namespace::epoch(epoch), abi::encode(&members));
        Ok(())
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
        let asked = identity::Query::Account(submission.signer.clone());
        let fuel = &mut self.loaded.limits().fuel;
        let account = match self
            .account(asked, height, time, overlay, stage, fuel)
            .await?
        {
            Ok(account) => account,
            Err(refusal) => return Ok(rejected(&submission.target, refusal)),
        };
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
            // a key that holds no account still runs (identity's own
            // create is such a frame); it acts as no one
            sender: account.map(Principal::Account),
            roles: self.roles.clone(),
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
        let asked = identity::Query::OfModule(program.to_owned());
        let account = self
            .account(asked, height, time, overlay, stage, &mut frame.fuel)
            .await?;
        Ok(account.map(|account| account.map(Principal::Account)))
    }

    /// The account the identity role says a frame acts as: the one a key
    /// holds (`Account`), or a program's own (`OfModule`). The role's
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
        call: GuestCall,
        env: Env,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Receipt> {
        let mut frame = Frame {
            fuel: self.loaded.limits().fuel,
            next_item: 0,
        };
        self.run_frame(program, call, env, overlay, stage, &mut frame, 0)
            .await
    }

    /// Runs `call`, then the messages it emitted in order, each at
    /// `depth + 1` and depth first, and undoes the whole run on a rejection
    /// it does not absorb: a rejected message without a reply, or a
    /// rejected reply run. A message with a reply wanted comes back as a
    /// `Completion` frame of the emitter, the target's writes undone when
    /// it was rejected.
    #[allow(clippy::too_many_arguments)]
    async fn run_frame(
        &self,
        program: &str,
        call: GuestCall,
        env: Env,
        overlay: &mut Overlay,
        stage: &mut Stage,
        frame: &mut Frame,
        depth: u32,
    ) -> Result<Receipt> {
        if depth > MAX_DEPTH {
            return Ok(rejected(
                program,
                Refusal::new(
                    reason::CAPACITY,
                    format!("messages nest deeper than {MAX_DEPTH}"),
                ),
            ));
        }
        let (height, time) = (env.height, env.time);
        let checkpoint = overlay.checkpoint();
        let (mut receipt, emitted) = unit::execute(
            self.world(height, time),
            overlay,
            stage,
            frame,
            program,
            Invocation { env, call },
        )
        .await?;
        // the emitter's account, asked once for all its messages
        let emitter = if emitted.is_empty() {
            Ok(None)
        } else {
            self.account_of(program, height, time, overlay, stage, frame)
                .await?
        };
        for (item, message) in emitted {
            let env = |me: &str, origin: &str, sender, cause| Env {
                network: self.network.clone(),
                height,
                time,
                me: me.to_owned(),
                origin: Origin::Program(origin.to_owned()),
                sender,
                roles: self.roles.clone(),
                cause,
            };
            let ran = match emitter.clone() {
                Err(refusal) => rejected(&message.target, refusal),
                Ok(sender) => {
                    let env = env(
                        &message.target,
                        program,
                        sender,
                        Cause::Message(item.clone()),
                    );
                    let call = GuestCall::Execute(message.payload);
                    Box::pin(self.run_frame(
                        &message.target,
                        call,
                        env,
                        overlay,
                        stage,
                        frame,
                        depth + 1,
                    ))
                    .await?
                }
            };
            let outcome = ran.outcome.clone();
            receipt.nested.push(ran);
            let refused = matches!(outcome, Outcome::Rejected(_));
            let outcome = match (refused, message.reply) {
                (false, false) => continue,
                (true, false) => outcome,
                (_, true) => {
                    let by = &message.target;
                    let replied = match self
                        .account_of(by, height, time, overlay, stage, frame)
                        .await?
                    {
                        Err(refusal) => rejected(program, refusal),
                        Ok(sender) => {
                            let cause = Cause::Completion { item, outcome };
                            let env = env(program, by, sender, cause);
                            let call = GuestCall::Execute(Vec::new());
                            Box::pin(self.run_frame(
                                program,
                                call,
                                env,
                                overlay,
                                stage,
                                frame,
                                depth + 1,
                            ))
                            .await?
                        }
                    };
                    let outcome = replied.outcome.clone();
                    receipt.nested.push(replied);
                    match outcome {
                        Outcome::Applied { .. } => continue,
                        Outcome::Rejected(_) => outcome,
                    }
                }
            };
            overlay.restore(checkpoint);
            unit::discard_unrostered(self.store.storage(), overlay, stage)?;
            receipt.outcome = outcome;
            break;
        }
        Ok(receipt)
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
            self.roles.registry.clone(),
            abi::encode(&registry::Query::At(height)),
            &mut self.loaded.limits().fuel,
        )
        .await?;
        let Ok(bytes) = reply else {
            return Ok(Vec::new());
        };
        let Ok(registry::Reply::Programs(entries)) = abi::decode::<registry::Reply>(&bytes) else {
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
            }
        }
        Ok(receipts)
    }

    async fn admit(
        &mut self,
        entry: &registry::Entry,
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

    /// Gives an admitted program its account: the identity role's
    /// `RegisterModule`, run as the system. A refusal is the receipt's.
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
        let op = identity::Op::RegisterModule {
            module: program.to_owned(),
        };
        self.run(
            &self.roles.identity,
            GuestCall::Execute(abi::encode(&op)),
            env,
            overlay,
            stage,
        )
        .await
    }

    fn swap(
        &mut self,
        entry: &registry::Entry,
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
        entry: &registry::Entry,
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
        overlay.set(NETWORK, namespace::TIP.to_vec(), abi::encode(&tip.id));
        let writes = overlay.into_writes();
        self.blobs.promote(stage)?;
        self.store.commit(tip.height, writes.clone()).await?;
        self.preconfirmed = Overlay::default();
        Ok(Applied {
            height: tip.height,
            admissions,
            submissions,
            writes,
            root: self.root()?,
        })
    }
}

/// A founding run's receipt, or the refusal that stops the founding.
fn founded(entry: &registry::Entry, receipt: Receipt) -> Result<Receipt> {
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
