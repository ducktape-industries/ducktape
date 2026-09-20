//! qmdb-backed ACCOUNT registry: numbered principals, key-held or programmed.
//!
//! an ACCOUNT is identified by a NUMBER (monotonic from 1) and is CONTROLLED
//! one of three ways ([`Control`]): by an ASSOCIATION of keys of mixed
//! schemes (an ed25519 device key, an Ethereum wallet, a WebAuthn passkey --
//! see [`KeyScheme`]), by a PROGRAM a module executes on a controller's
//! behalf, or not at all any more (REVOKED). the control decides which ops
//! apply; nothing else about a record does.
//!
//! ## key-held accounts
//!
//! the frame ORIGIN is the acting key for every key op; a node key is never
//! bound to an account, and no consumer ever asks "whose node is this" --
//! attribution comes only from a user-signed origin, resolved through
//! [`IdentityQuery::OfKey`].
//!
//! - [`IdentityMsg::Create`] founds an account for the origin key.
//! - [`IdentityMsg::AddKey`] admits the origin key into an existing member's
//!   account: the member consents over [`add_key_preimage`] at the origin's
//!   CURRENT generation; acceptance advances that generation, so the consent
//!   is single-use -- including after a compromised key is removed. a removed
//!   key is never burned (a wallet or an SSH key cannot be re-minted): it can
//!   be re-admitted anywhere by a consent signed at its next generation. the
//!   consent also NAMES the account it admits into and the consensus time it
//!   dies at: an unspent one neither follows its author onto another account
//!   nor outlives [`MAX_CONSENT_TTL`]. there is no revoke op -- the clock is
//!   the revocation, and it is why the ceiling is days rather than months.
//! - [`IdentityMsg::RemoveKey`] drops a key: a member removes ITSELF or a key
//!   admitted no earlier than it was, never the last one. seniority is what
//!   stops a key admitted by a mis-issued consent from evicting the founders
//!   who predate it -- an account cannot be taken over from the bottom.
//! - [`IdentityMsg::SetName`] / [`IdentityMsg::SetProfile`] are gated by the
//!   ACTING ACCOUNT alone: a member key's account, or the program account the
//!   host runs (below).
//!
//! ## program accounts
//!
//! a program account holds no key, ever, and no key op names one: a key is
//! only ever admitted into a key-held account, and `Origin::Program` is
//! refused by every key op. it exists because a module asked for it:
//!
//! - [`IdentityMsg::CreateProgram`] is module-origin only. the emitting
//!   module becomes the EXECUTOR (a payload can never name one); the
//!   CONTROLLER it names must be live (key-held, or an active program). the
//!   account is founded at generation 0, active, and
//!   [`IdentityEvent::ProgramCreated`] goes back to the executor as a
//!   follow-up in the same unit -- so the executor's binding and the account
//!   commit together or not at all. the executor authenticates that
//!   follow-up by origin ([`authenticate_event`]).
//! - the host runs a program's call as `Origin::Program(account)` only after
//!   checking its CURRENT record: executor, generation and an active
//!   standing. the GENERATION counts the record's mutations -- a standing
//!   change ([`IdentityMsg::SetProgramStanding`], executor-origin only, also
//!   when the standing is unchanged) or a control transfer
//!   ([`IdentityMsg::TransferControl`], controller-origin only) -- so every
//!   call queued under an older generation is refused at execution. that is
//!   the whole invalidation mechanism: an executor unbinding or replacing a
//!   program, or a controller handing it on, never has to find what was
//!   queued. it is advanced with checked arithmetic before any write; an
//!   exhausted generation refuses the op whole.
//! - [`IdentityMsg::RevokeProgram`] (controller-origin only) freezes the
//!   account as [`Control::Revoked`]: no standing, no generation, no op ever
//!   touches it again, and it can never act or control.
//! - the control graph is a forest: a controller is never the account itself
//!   nor one it transitively controls, checked at provisioning and at every
//!   transfer, so a chain of controllers always ends at a key-held account.
//!
//! an `Origin::Program(account)` acts as that account for the ops a member key
//! would perform on its own account (rename, profile). the account number is
//! never spelled where a key goes, so nothing resolves a program through the
//! key index.
//!
//! ## State model
//!
//! pure logic over a host-injected [`sdk::MerkleStore`]: the HOST constructs
//! the concrete store (qmdb today -- `statesync::qmdb::QmdbStore`) and hands
//! it to [`Identity::new`], so this crate never names a storage crate. one
//! record per account (`acct\0{number}`, an `AccountRecord` whose control
//! is the sum type the wire [`Control`] views), the KEY INDEX
//! (`key\0{pubkey}` -> number), the per-key ADMISSION COUNTER (`gen\0{pubkey}`
//! -> how many times that key has been admitted anywhere; absent = 0), the
//! CONTROLLED SET (`ctl\0{controller}` -> the sorted numbers of every account
//! whose control record names that controller; absent = none), and `next`
//! (the next account number; absent = 1). no roster: accounts are never
//! deleted, so `All` walks `acct\0{from..next}` directly, and [`MAX_ACCOUNTS`]
//! caps `next`. the store hashes every key, so nothing here is a prefix scan:
//! `Controlled` pages the one set record.
//!
//! writes are staged during a block and flushed to the store in one batch at
//! `commit_block`; the module root IS the store's merkle root. sync belongs
//! to the store, not this module: a joiner rebuilds the concrete store from a
//! peer (`QmdbStore::sync_from`) and wraps a fresh `Identity` around it.
//!
//! oversized values never reach the store (the poison-value lesson -- the
//! qmdb wire codec bounds a value at decode, so an over-cap committed value
//! would wedge every syncing peer): EVERY path that grows an account record
//! restages the whole record through the [`MAX_ACCOUNT_RECORD_BYTES`] gate,
//! and a controlled set through the store's own [`sdk::MAX_STORE_VALUE_BYTES`].
//! every op encodes and checks all of its writes before it stages any, so a
//! refused op leaves nothing staged.
//!
//! ## Genesis config (the chain id)
//!
//! the per-network chain id reaches the NATIVE module through
//! [`Identity::new`]. the wasm tenant is fixed bytes, so there the id rides
//! GENESIS CONFIG: the host seeds the reserved `__config` entry
//! ([`sdk::genesis_config`]) into this module's store at genesis construction
//! -- under [`sdk::store_key`], the same logical→store mapping every record
//! here uses -- and the guest decodes it per dispatch. the config is
//! consensus state in the store's merkle root from genesis and rides
//! state-sync like any other record. this module never writes that key.
//!
// the wire surface: this module's shared types, flattened at the crate root.
mod wire;
pub use wire::*;

// the one verifier every consent rides — shared with the kernel frame codec,
// so an account key and a frame origin are verified identically.
pub use keyscheme::KeyScheme;

// test-only consent builders. dev-only: gated so a shipping build never
// compiles the ed25519 signing helpers into itself.
#[cfg(feature = "testkit")]
pub mod testkit;

use std::collections::{BTreeMap, BTreeSet};

use borsh::{BorshDeserialize, BorshSerialize};
use sdk::{
    Ctx, Error, MAX_STORE_VALUE_BYTES, MerkleStore, Module, ModuleId, Msg, Origin,
    ResolverSyncTarget, StagedStore, StateRoot, StateSyncHandle,
};

/// accounts retained over the network's life (an account is never deleted).
/// founding past this -- key-held or program -- refuses loudly at execute.
pub const MAX_ACCOUNTS: u64 = 65_536;
/// serialized account-record ceiling, enforced on EVERY staged account write.
/// there is no growth path that bypasses the gate -- create/add-key/profile
/// ops all restage the whole record through it -- so an op that would push a
/// record past the cap is refused loudly and deterministically instead of
/// poisoning the sync wire.
pub const MAX_ACCOUNT_RECORD_BYTES: usize = 512 * 1024;
/// keys one account may associate. the byte cap alone would allow ~11k of
/// them (a key entry is the pubkey plus its meta, well under 128 bytes), and
/// EVERY `OfKey`/`All` reader decodes the whole set -- so the association is
/// bounded by count too: 32 keys x ~128 bytes is ~4 KiB, three orders of
/// magnitude under [`MAX_ACCOUNT_RECORD_BYTES`]. a person's devices, wallets
/// and passkeys fit; a scripted key farm does not.
pub const MAX_KEYS_PER_ACCOUNT: usize = 32;

/// per-account record key: prefix + 0 + the number, little-endian.
fn acct_key(number: AccountNumber) -> Vec<u8> {
    let mut key = Vec::with_capacity(4 + 1 + 8);
    key.extend_from_slice(b"acct");
    key.push(0);
    key.extend_from_slice(&number.to_le_bytes());
    key
}

/// key-index key: prefix + 0 + public key. valued by the owning account
/// number; maintained by the execute paths as they stage.
fn key_owner_key(pubkey: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(3 + 1 + pubkey.len());
    key.extend_from_slice(b"key");
    key.push(0);
    key.extend_from_slice(pubkey);
    key
}

/// admission-counter key: prefix + 0 + public key. valued by how many times
/// the key has been admitted anywhere; absent = 0.
fn key_gen_key(pubkey: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(3 + 1 + pubkey.len());
    key.extend_from_slice(b"gen");
    key.push(0);
    key.extend_from_slice(pubkey);
    key
}

/// controlled-set key: prefix + 0 + the controller's number, little-endian.
/// valued by the sorted numbers of every account whose control record names
/// that controller; absent = none.
fn controlled_key(controller: AccountNumber) -> Vec<u8> {
    let mut key = Vec::with_capacity(3 + 1 + 8);
    key.extend_from_slice(b"ctl");
    key.push(0);
    key.extend_from_slice(&controller.to_le_bytes());
    key
}

/// the next account number's whole key. collides with no `acct\0...` /
/// `key\0...` / `gen\0...` / `ctl\0...` key (nor the host-seeded `__config`
/// record).
const NEXT_NUMBER_KEY: &[u8] = b"next";

/// per-key metadata; the public key is the map key, so it is not repeated.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
struct KeyMeta {
    scheme: KeyScheme,
    label: Option<String>,
    added_at: u64,
}

/// the association of a key-held account: public key -> its meta. borsh
/// writes the `BTreeMap` length-prefixed in key order, so one set has exactly
/// one encoding.
type Association = BTreeMap<Vec<u8>, KeyMeta>;

/// the set of accounts one controller's records name, sorted by number.
type ControlledSet = BTreeSet<AccountNumber>;

/// the control record of a program account: who may hand it on or revoke
/// it, who executes it, and the authority its queued calls are checked
/// against (`generation`, `standing`).
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
struct ProgramControl {
    controller: AccountNumber,
    executor: ModuleId,
    generation: u64,
    standing: ProgramStanding,
}

/// how an account is controlled, as stored: the keys of a key-held account
/// live INSIDE its variant, so a program or revoked account cannot hold one
/// by construction. the wire [`Control`] is this without the association.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
enum ControlRecord {
    Keys(Association),
    Program(ProgramControl),
    Revoked { controller: AccountNumber },
}

impl ControlRecord {
    /// the wire view: the control without the association.
    fn view(&self) -> Control {
        match self {
            ControlRecord::Keys(_) => Control::Keys,
            ControlRecord::Program(program) => Control::Program {
                controller: program.controller,
                executor: program.executor.clone(),
                generation: program.generation,
                standing: program.standing,
            },
            ControlRecord::Revoked { controller } => Control::Revoked {
                controller: *controller,
            },
        }
    }

    /// the program fields of a program account, or why a program op does not
    /// apply to this account.
    fn program(&self) -> Result<&ProgramControl, Error> {
        match self {
            ControlRecord::Program(program) => Ok(program),
            ControlRecord::Keys(_) => Err(Error::module(
                "not_a_program",
                "account is key-held, not a program",
            )),
            ControlRecord::Revoked { .. } => {
                Err(Error::module("program_revoked", "program is revoked"))
            }
        }
    }

    /// the association as queries expose it: ascending by public key, empty
    /// for anything but a key-held account.
    fn keys_view(&self) -> Vec<KeyView> {
        let ControlRecord::Keys(keys) = self else {
            return Vec::new();
        };
        keys.iter()
            .map(|(pubkey, meta)| KeyView {
                scheme: meta.scheme,
                pubkey: pubkey.clone(),
                label: meta.label.clone(),
                added_at: meta.added_at,
            })
            .collect()
    }

    /// whether the account may be named as a controller, or act as one:
    /// key-held, or a program that is active.
    fn is_live(&self) -> bool {
        match self {
            ControlRecord::Keys(_) => true,
            ControlRecord::Program(program) => program.standing == ProgramStanding::Active,
            ControlRecord::Revoked { .. } => false,
        }
    }

    /// the account this record names as controller, if it names one.
    fn controller(&self) -> Option<AccountNumber> {
        match self {
            ControlRecord::Keys(_) => None,
            ControlRecord::Program(program) => Some(program.controller),
            ControlRecord::Revoked { controller } => Some(*controller),
        }
    }
}

/// one account: name, its control, profile fields, last-write time. the
/// number is the record key, so it is not repeated here.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
struct AccountRecord {
    name: String,
    control: ControlRecord,
    /// duckfs path the app resolves the avatar image against (`None` unset).
    avatar: Option<String>,
    /// short bio/status line (`None` unset).
    bio: Option<String>,
    updated_at: u64,
}

impl AccountRecord {
    /// the association the key index led to. an index entry that points at
    /// anything but a key-held account is a store bug -- loud, never skipped.
    fn association(&self) -> Result<&Association, Error> {
        match &self.control {
            ControlRecord::Keys(keys) => Ok(keys),
            ControlRecord::Program(_) | ControlRecord::Revoked { .. } => Err(Error::module(
                "key_index_corrupt",
                "key index points at an account that holds no keys",
            )),
        }
    }

    fn association_mut(&mut self) -> Result<&mut Association, Error> {
        match &mut self.control {
            ControlRecord::Keys(keys) => Ok(keys),
            ControlRecord::Program(_) | ControlRecord::Revoked { .. } => Err(Error::module(
                "key_index_corrupt",
                "key index points at an account that holds no keys",
            )),
        }
    }
}

/// who the dispatch acts as, once the origin is resolved against the store:
/// a member key of a key-held account, or the program account the host runs.
enum Actor {
    Key {
        key: Vec<u8>,
        account: AccountNumber,
    },
    Program(AccountNumber),
}

impl Actor {
    fn account(&self) -> AccountNumber {
        match self {
            Actor::Key { account, .. } | Actor::Program(account) => *account,
        }
    }
}

// ---- pure decisions --------------------------------------------------------------

/// the number the next account takes and the counter after it. the cap is
/// the one bound on the numbering; the counter itself is advanced checked so
/// a store at the cap can never wrap it.
fn allocate_number(next: AccountNumber) -> Result<(AccountNumber, AccountNumber), Error> {
    let cap_reached = next > MAX_ACCOUNTS;
    if cap_reached {
        return Err(Error::module(
            "account_cap",
            format!("account cap reached ({MAX_ACCOUNTS})"),
        ));
    }
    let after = next
        .checked_add(1)
        .ok_or_else(|| Error::module("numbering_exhausted", "account numbering is exhausted"))?;
    Ok((next, after))
}

/// a program's generation after one more mutation of its control record. an
/// exhausted generation refuses the mutation: the record must stay exactly
/// as every queued call saw it, never wrap back to a generation one did.
fn next_generation(generation: u64) -> Result<u64, Error> {
    generation.checked_add(1).ok_or_else(|| {
        Error::module(
            "generation_exhausted",
            "program generation is exhausted; the control record cannot change",
        )
    })
}

/// a key's admission counter after one more admission.
fn next_key_generation(generation: u64) -> Result<u64, Error> {
    generation.checked_add(1).ok_or_else(|| {
        Error::module(
            "key_counter_exhausted",
            "key admission counter is exhausted",
        )
    })
}

/// the account record as the store takes it, or the reason it cannot.
fn encode_account(record: &AccountRecord) -> Result<Vec<u8>, Error> {
    let bytes = borsh::to_vec(record).expect("identity value is serializable");
    if bytes.len() > MAX_ACCOUNT_RECORD_BYTES {
        return Err(Error::module(
            "record_too_large",
            format!(
                "account record too large: {} > {MAX_ACCOUNT_RECORD_BYTES} bytes",
                bytes.len()
            ),
        ));
    }
    Ok(bytes)
}

/// a non-account value as the store takes it, or the reason it cannot: the
/// backing codec's bound is the one every staged value must sit under.
fn encode_value<T: BorshSerialize>(value: &T) -> Result<Vec<u8>, Error> {
    let bytes = borsh::to_vec(value).expect("identity value is serializable");
    let fits_the_store = bytes.len() <= MAX_STORE_VALUE_BYTES;
    if !fits_the_store {
        return Err(Error::module(
            "record_too_large",
            format!(
                "a record of {} bytes exceeds the store's value bound of {MAX_STORE_VALUE_BYTES}",
                bytes.len()
            ),
        ));
    }
    Ok(bytes)
}

/// the window one page of the numbering covers: the first number read
/// (`from: 0` reads from 1) and how many at most (`limit` clamped to
/// [`MAX_QUERY_LIMIT`]).
fn page_window(from: u64, limit: u64) -> (AccountNumber, usize) {
    let first = from.max(1);
    let take = usize::try_from(limit.min(MAX_QUERY_LIMIT)).expect("clamped");
    (first, take)
}

/// one op's complete write set, every value already encoded and known to fit
/// the store. built whole before anything is staged, so a refused op stages
/// nothing.
#[derive(Default)]
struct WritePlan {
    writes: Vec<(Vec<u8>, Vec<u8>)>,
}

impl WritePlan {
    fn account(&mut self, number: AccountNumber, record: &AccountRecord) -> Result<(), Error> {
        self.writes
            .push((acct_key(number), encode_account(record)?));
        Ok(())
    }

    fn controlled(&mut self, controller: AccountNumber, set: &ControlledSet) -> Result<(), Error> {
        self.writes
            .push((controlled_key(controller), encode_value(set)?));
        Ok(())
    }

    fn next_number(&mut self, next: AccountNumber) -> Result<(), Error> {
        self.writes
            .push((NEXT_NUMBER_KEY.to_vec(), encode_value(&next)?));
        Ok(())
    }
}

/// the control record and the founding writes of one new program account.
struct ProgramFounding {
    plan: WritePlan,
    account: AccountNumber,
}

/// decide a program's founding writes: the number, its record, the
/// controller's set and the counter after it. pure; writes nothing.
fn decide_program_founding(
    name: String,
    controller: AccountNumber,
    executor: ModuleId,
    next: AccountNumber,
    mut controlled: ControlledSet,
    now: u64,
) -> Result<ProgramFounding, Error> {
    let (account, after) = allocate_number(next)?;
    let record = AccountRecord {
        name,
        control: ControlRecord::Program(ProgramControl {
            controller,
            executor,
            generation: 0,
            standing: ProgramStanding::Active,
        }),
        avatar: None,
        bio: None,
        updated_at: now,
    };
    controlled.insert(account);
    let mut plan = WritePlan::default();
    plan.account(account, &record)?;
    plan.controlled(controller, &controlled)?;
    plan.next_number(after)?;
    Ok(ProgramFounding { plan, account })
}

/// everything a control transfer reads, loaded before it is decided.
struct TransferLoaded {
    /// the account whose control moves.
    record: AccountRecord,
    /// the account receiving control.
    to_record: AccountRecord,
    /// `to`, then each controller above it up to a key-held root.
    to_chain: Vec<AccountNumber>,
    /// the current controller's set and the receiver's set.
    from_set: ControlledSet,
    to_set: ControlledSet,
}

/// decide a control transfer: who may, where it may go, the generation after
/// it, and the two sets it moves between. pure; writes nothing.
fn decide_transfer(
    loaded: TransferLoaded,
    actor: AccountNumber,
    account: AccountNumber,
    to: AccountNumber,
    now: u64,
) -> Result<WritePlan, Error> {
    let TransferLoaded {
        record,
        to_record,
        to_chain,
        mut from_set,
        mut to_set,
    } = loaded;
    let program = record.control.program()?;
    let by_controller = program.controller == actor;
    if !by_controller {
        return Err(Error::module(
            "not_the_controller",
            "only its controller transfers control of a program",
        ));
    }
    let to_itself = to == account;
    if to_itself {
        return Err(Error::module(
            "self_control",
            "an account cannot control itself",
        ));
    }
    let to_current_controller = to == program.controller;
    if to_current_controller {
        return Err(Error::module(
            "control_unchanged",
            format!("account {account} is already controlled by {to}"),
        ));
    }
    if !to_record.control.is_live() {
        return Err(Error::module(
            "controller_not_live",
            format!("account {to} cannot control: it is not key-held or an active program"),
        ));
    }
    // `to` is controlled, transitively, by `account`: handing control to it
    // would close a cycle no chain could ever leave.
    let would_cycle = to_chain.contains(&account);
    if would_cycle {
        return Err(Error::module(
            "control_cycle",
            format!("account {to} is controlled by {account}: control would cycle"),
        ));
    }
    let mut moved = program.clone();
    moved.generation = next_generation(moved.generation)?;
    moved.controller = to;
    let from = program.controller;
    from_set.remove(&account);
    to_set.insert(account);
    let record = AccountRecord {
        control: ControlRecord::Program(moved),
        updated_at: now,
        ..record
    };
    let mut plan = WritePlan::default();
    plan.account(account, &record)?;
    plan.controlled(from, &from_set)?;
    plan.controlled(to, &to_set)?;
    Ok(plan)
}

/// the record after its executor set the standing. the generation advances
/// whether or not the standing changed: the op's promise is that nothing
/// queued before it stays executable.
fn decide_standing(
    record: AccountRecord,
    executor: &str,
    standing: ProgramStanding,
    now: u64,
) -> Result<AccountRecord, Error> {
    let program = record.control.program()?;
    let by_executor = program.executor == executor;
    if !by_executor {
        return Err(Error::module(
            "not_the_executor",
            "only its executor sets a program's standing",
        ));
    }
    let mut changed = program.clone();
    changed.generation = next_generation(changed.generation)?;
    changed.standing = standing;
    Ok(AccountRecord {
        control: ControlRecord::Program(changed),
        updated_at: now,
        ..record
    })
}

/// the record after its controller revoked it: frozen under the controller
/// that held it, its generation gone with its standing.
fn decide_revoke(
    record: AccountRecord,
    actor: AccountNumber,
    now: u64,
) -> Result<AccountRecord, Error> {
    let program = record.control.program()?;
    let by_controller = program.controller == actor;
    if !by_controller {
        return Err(Error::module(
            "not_the_controller",
            "only its controller revokes a program",
        ));
    }
    Ok(AccountRecord {
        control: ControlRecord::Revoked {
            controller: program.controller,
        },
        updated_at: now,
        ..record
    })
}

pub struct Identity {
    id: ModuleId,
    /// this network's chain id -- folded into every add-key consent so one
    /// minted for one network can never act on another.
    chain_id: String,
    /// the host-injected authenticated store plus this block's staging overlay
    /// (read-your-writes, folded into `root()` at `commit_block`). store key
    /// is `sha256(logical_key)`, owned by [`StagedStore`].
    staged: StagedStore,
}

impl Identity {
    /// wrap the host-constructed store under module identity `id`.
    pub fn new(id: impl Into<ModuleId>, store: Box<dyn MerkleStore>, chain_id: String) -> Self {
        Self {
            id: id.into(),
            chain_id,
            staged: StagedStore::new(store),
        }
    }

    // ---- staged-over-committed reads ----------------------------------------

    async fn load<T>(&self, key: &[u8]) -> Result<Option<T>, Error>
    where
        T: BorshDeserialize,
    {
        match self.staged.get(key).await? {
            Some(bytes) => Ok(Some(
                borsh::from_slice(&bytes).map_err(|e| Error::module("codec", e.to_string()))?,
            )),
            None => Ok(None),
        }
    }

    /// stage a value whose serialized size is bounded by construction (an
    /// index entry, a counter, the next number) -- see the module doc's
    /// poison-value paragraph. account records go through
    /// [`Self::store_account`]; multi-record ops through [`Self::stage_plan`].
    fn store<T>(&mut self, key: Vec<u8>, value: &T)
    where
        T: BorshSerialize,
    {
        self.staged.stage(
            key,
            borsh::to_vec(value).expect("identity value is serializable"),
        );
    }

    async fn account(&self, number: AccountNumber) -> Result<Option<AccountRecord>, Error> {
        self.load(&acct_key(number)).await
    }

    /// an account an index or a control record points at. a pointer without
    /// its record is a store bug -- loud, never skipped.
    async fn stored_account(&self, number: AccountNumber) -> Result<AccountRecord, Error> {
        self.account(number)
            .await?
            .ok_or_else(|| Error::module("missing_account_record", "missing account record"))
    }

    /// the account `key` belongs to, if any -- the key-index read.
    async fn owner_of_key(&self, key: &[u8]) -> Result<Option<AccountNumber>, Error> {
        self.load(&key_owner_key(key)).await
    }

    /// Resolve only account numbers, with bounded read frontiers and the
    /// same staged ownership and dangling-index checks as `Get`/`OfKey`.
    async fn resolve_references(
        &self,
        references: &[AccountRef],
    ) -> Result<Vec<Option<AccountNumber>>, Error> {
        let exceeds_query_limit = references.len() as u64 > MAX_QUERY_LIMIT;
        if exceeds_query_limit {
            return Err(Error::module(
                "query_limit",
                format!("identity: reference count exceeds query limit ({MAX_QUERY_LIMIT})"),
            ));
        }
        let key_reads: Vec<_> = references
            .iter()
            .filter_map(|reference| match reference {
                AccountRef::Key(key) => Some(key_owner_key(key)),
                AccountRef::Account(_) => None,
            })
            .collect();
        self.staged.prefetch(&key_reads).await?;
        let mut numbers = Vec::with_capacity(references.len());
        for reference in references {
            numbers.push(match reference {
                AccountRef::Account(number) => Some(*number),
                AccountRef::Key(key) => self.owner_of_key(key).await?,
            });
        }
        let account_reads: Vec<_> = numbers
            .iter()
            .flatten()
            .map(|number| acct_key(*number))
            .collect();
        self.staged.prefetch(&account_reads).await?;
        let mut resolved = Vec::with_capacity(references.len());
        for (reference, number) in references.iter().zip(numbers) {
            let Some(number) = number else {
                resolved.push(None);
                continue;
            };
            let account = match reference {
                AccountRef::Account(_) => self.account(number).await?.map(|_| number),
                AccountRef::Key(_) => {
                    self.stored_account(number).await?;
                    Some(number)
                }
            };
            resolved.push(account);
        }
        Ok(resolved)
    }

    /// how many times `key` has been admitted anywhere (absent = 0).
    async fn key_gen(&self, key: &[u8]) -> Result<u64, Error> {
        Ok(self.load(&key_gen_key(key)).await?.unwrap_or(0))
    }

    /// the next account number (absent = 1).
    async fn next_number(&self) -> Result<AccountNumber, Error> {
        Ok(self.load(NEXT_NUMBER_KEY).await?.unwrap_or(1))
    }

    /// the accounts whose control record names `controller` (absent = none).
    async fn controlled_by(&self, controller: AccountNumber) -> Result<ControlledSet, Error> {
        Ok(self
            .load(&controlled_key(controller))
            .await?
            .unwrap_or_default())
    }

    /// the accounts from `start` up its controllers to a key-held root, in
    /// order. the control graph is a forest by invariant, so the walk ends;
    /// a revisit is store corruption, refused loudly rather than walked
    /// forever.
    async fn controller_chain(&self, start: AccountNumber) -> Result<Vec<AccountNumber>, Error> {
        let mut chain = Vec::new();
        let mut seen = BTreeSet::new();
        let mut at = start;
        loop {
            let revisited = !seen.insert(at);
            if revisited {
                return Err(Error::module(
                    "control_graph_corrupt",
                    format!("control chain revisits account {at}: the control graph is corrupt"),
                ));
            }
            chain.push(at);
            let record = self.stored_account(at).await?;
            let Some(controller) = record.control.controller() else {
                return Ok(chain);
            };
            at = controller;
        }
    }

    // ---- writers ------------------------------------------------------------

    /// stage an updated account record under the byte cap -- the write every
    /// single-record account mutation funnels through.
    fn store_account(
        &mut self,
        number: AccountNumber,
        record: &AccountRecord,
    ) -> Result<(), Error> {
        let bytes = encode_account(record)?;
        self.staged.stage(acct_key(number), bytes);
        Ok(())
    }

    /// stage a decided plan, every value of it. cannot fail: each value was
    /// encoded and checked against the store before it was planned.
    fn stage_plan(&mut self, plan: WritePlan) {
        for (key, value) in plan.writes {
            self.staged.stage(key, value);
        }
    }

    // ---- gates ---------------------------------------------------------------

    /// the AUTHENTICATED submitter key of a key op -- a non-empty external
    /// origin, or a deterministic rejection. a program account holds no key
    /// and manages none.
    fn origin_key(ctx: &dyn Ctx) -> Result<Vec<u8>, Error> {
        match &ctx.env().origin {
            Origin::External(bytes) => submitter_key(bytes),
            Origin::Program(_) => Err(Error::module(
                "not_a_key_account",
                "key operations are for member keys; a program account holds no key",
            )),
            Origin::Module(_) | Origin::System => Err(Error::module(
                "external_origin_required",
                "key operations are origin-gated to external submitters",
            )),
        }
    }

    /// the module behind a module-origin op -- the executor of the program it
    /// provisions or governs. no payload field ever stands in for it.
    fn acting_module(ctx: &dyn Ctx) -> Result<ModuleId, Error> {
        match &ctx.env().origin {
            Origin::Module(module) => Ok(module.clone()),
            Origin::External(_) | Origin::Program(_) | Origin::System => Err(Error::module(
                "module_origin_required",
                "program provisioning and standing are module-origin only \
                 (the emitting module is the executor)",
            )),
        }
    }

    /// the origin resolved against the store: who acts, and that account's
    /// record. a key acts as the account it belongs to; a program origin acts
    /// as the account the host runs, which must be an active program -- the
    /// host proved that before running it, and a record that disagrees is
    /// refused rather than trusted.
    async fn acting(&self, ctx: &dyn Ctx) -> Result<(Actor, AccountRecord), Error> {
        match &ctx.env().origin {
            Origin::External(bytes) => {
                let key = submitter_key(bytes)?;
                let account = self.owner_of_key(&key).await?.ok_or_else(|| {
                    Error::module("no_account_for_key", "origin key belongs to no account")
                })?;
                let record = self.stored_account(account).await?;
                Ok((Actor::Key { key, account }, record))
            }
            Origin::Program(account) => {
                let record = self.account(*account).await?.ok_or_else(|| {
                    Error::module(
                        "unknown_account",
                        format!("program origin names account {account}, which does not exist"),
                    )
                })?;
                let runs = matches!(
                    &record.control,
                    ControlRecord::Program(program) if program.standing == ProgramStanding::Active
                );
                if !runs {
                    return Err(Error::module(
                        "not_an_active_program",
                        format!(
                            "program origin names account {account}, \
                             which is not an active program"
                        ),
                    ));
                }
                Ok((Actor::Program(*account), record))
            }
            Origin::Module(_) | Origin::System => Err(Error::module(
                "no_actor_for_origin",
                "account operations act as a key's account or a program account; \
                 a module or the system holds none",
            )),
        }
    }

    // ---- views ---------------------------------------------------------------

    fn account_view(number: AccountNumber, record: &AccountRecord) -> AccountView {
        AccountView {
            number,
            name: record.name.clone(),
            control: record.control.view(),
            keys: record.control.keys_view(),
            avatar: record.avatar.clone(),
            bio: record.bio.clone(),
            updated_at: record.updated_at,
        }
    }

    async fn view_of(&self, number: Option<AccountNumber>) -> Result<Option<AccountView>, Error> {
        match number {
            Some(number) => Ok(Some(Self::account_view(
                number,
                &self.stored_account(number).await?,
            ))),
            None => Ok(None),
        }
    }

    /// the views of `numbers`, each of which a numbering or a set names, so
    /// each has a record.
    async fn views(
        &self,
        numbers: impl Iterator<Item = AccountNumber>,
    ) -> Result<Vec<AccountView>, Error> {
        let mut accounts = Vec::new();
        for number in numbers {
            let record = self.stored_account(number).await?;
            accounts.push(Self::account_view(number, &record));
        }
        Ok(accounts)
    }
}

/// a non-empty submitter id, or a deterministic rejection.
fn submitter_key(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    if bytes.is_empty() {
        return Err(Error::module(
            "empty_submitter_id",
            "external origin must carry a non-empty submitter id",
        ));
    }
    Ok(bytes.to_vec())
}

#[async_trait::async_trait(?Send)]
impl Module for Identity {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    /// the store's merkle root over all committed records, verbatim — the
    /// staged overlay is invisible here until `commit_block`.
    fn root(&self) -> StateRoot {
        self.staged.root()
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        self.staged.state_sync_handle()
    }

    /// the network state-sync serve lane: answers the shared qmdb wire requests
    /// (historical proof-carrying op ranges) from committed state. read-only;
    /// the joiner's sync engine merkle-verifies every batch.
    async fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        self.staged.serve_sync(req).await
    }

    async fn resolver_sync_target(&self) -> Result<ResolverSyncTarget, Error> {
        self.staged.sync_target().await
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        match decode_msg(&msg.payload).map_err(|e| Error::module("codec", e))? {
            IdentityMsg::Create { name, scheme } => self.create(ctx, name, scheme).await,
            IdentityMsg::AddKey {
                scheme,
                label,
                authorizer,
            } => self.add_key(ctx, scheme, label, authorizer).await,
            IdentityMsg::RemoveKey { key } => self.remove_key(ctx, key).await,
            IdentityMsg::SetName { name } => self.set_name(ctx, name).await,
            IdentityMsg::SetProfile { avatar, bio } => self.set_profile(ctx, avatar, bio).await,
            IdentityMsg::CreateProgram {
                name,
                controller,
                request,
            } => self.create_program(ctx, name, controller, request).await,
            IdentityMsg::SetProgramStanding { account, standing } => {
                self.set_program_standing(ctx, account, standing).await
            }
            IdentityMsg::TransferControl { account, to } => {
                self.transfer_control(ctx, account, to).await
            }
            IdentityMsg::RevokeProgram { account } => self.revoke_program(ctx, account).await,
        }
    }

    /// read projection — committed plus this block's staged changes (the
    /// staged-over-committed store view).
    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        match decode_query(req).map_err(|e| Error::module("codec", e))? {
            IdentityQuery::All { from, limit } => {
                // walk the numbering directly: no deletion ever, so no gaps.
                let (first, take) = page_window(from, limit);
                let end = self.next_number().await?;
                let accounts = self.views((first..end).take(take)).await?;
                Ok(encode_reply(&IdentityReply::Accounts(accounts)))
            }
            IdentityQuery::Get { number } => Ok(encode_reply(&IdentityReply::Account(
                self.account(number)
                    .await?
                    .map(|record| Self::account_view(number, &record)),
            ))),
            IdentityQuery::OfKey { key } => {
                let number = self.owner_of_key(&key).await?;
                Ok(encode_reply(&IdentityReply::Account(
                    self.view_of(number).await?,
                )))
            }
            IdentityQuery::Resolve { references } => Ok(encode_reply(&IdentityReply::Resolved(
                self.resolve_references(&references).await?,
            ))),
            IdentityQuery::KeyGen { key } => {
                Ok(encode_reply(&IdentityReply::Gen(self.key_gen(&key).await?)))
            }
            IdentityQuery::Controlled { by, from, limit } => {
                // the set is sorted by number, so a page is a range of it.
                let (first, take) = page_window(from, limit);
                let set = self.controlled_by(by).await?;
                let accounts = self.views(set.range(first..).take(take).copied()).await?;
                Ok(encode_reply(&IdentityReply::Accounts(accounts)))
            }
        }
    }

    /// publish the block's staged writes in ONE store batch. no-op (and no
    /// root movement) if nothing was staged.
    async fn commit_block(&mut self) -> Result<(), Error> {
        self.staged.commit().await
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged.abort();
        Ok(())
    }
}

impl Identity {
    /// found a key-held account for the origin key.
    async fn create(
        &mut self,
        ctx: &mut dyn Ctx,
        name: String,
        scheme: KeyScheme,
    ) -> Result<(), Error> {
        let origin = Self::origin_key(ctx)?;
        if !scheme.pubkey_wellformed(&origin) {
            return Err(Error::module(
                "bad_key",
                "founding key is malformed for its scheme",
            ));
        }
        if self.owner_of_key(&origin).await?.is_some() {
            return Err(Error::module(
                "key_already_claimed",
                "key already belongs to an account",
            ));
        }
        let name = clean_name(name)?;
        let (number, after) = allocate_number(self.next_number().await?)?;

        let now = ctx.env().consensus_time;
        let mut keys = Association::new();
        keys.insert(
            origin.clone(),
            KeyMeta {
                scheme,
                label: None,
                added_at: now,
            },
        );
        let record = AccountRecord {
            name,
            control: ControlRecord::Keys(keys),
            avatar: None,
            bio: None,
            updated_at: now,
        };
        self.store_account(number, &record)?;
        // bounded by construction: a well-formed key and two integers.
        self.store(key_owner_key(&origin), &number);
        self.store(NEXT_NUMBER_KEY.to_vec(), &after);
        ctx.set_assigned(encode_assigned(&IdentityAssigned::Founded {
            account: number,
        }));
        Ok(())
    }

    /// admit the origin key into the authorizer's account.
    async fn add_key(
        &mut self,
        ctx: &mut dyn Ctx,
        scheme: KeyScheme,
        label: Option<String>,
        authorizer: Authorizer,
    ) -> Result<(), Error> {
        let origin = Self::origin_key(ctx)?;
        if !scheme.pubkey_wellformed(&origin) {
            return Err(Error::module(
                "bad_key",
                "joining key is malformed for its scheme",
            ));
        }
        if self.owner_of_key(&origin).await?.is_some() {
            return Err(Error::module(
                "key_already_claimed",
                "key already belongs to an account",
            ));
        }
        let number = self.owner_of_key(&authorizer.key).await?.ok_or_else(|| {
            Error::module("no_account_for_key", "authorizer belongs to no account")
        })?;
        // the account is under the authorizer's signature AND is its account
        // right now: a consent minted on one account never admits into another
        // its author later joins.
        if number != authorizer.account {
            return Err(Error::module(
                "consent_account_mismatch",
                format!(
                    "consent names account {}, its authorizer is on account {number}",
                    authorizer.account
                ),
            ));
        }
        let now = ctx.env().consensus_time;
        if now > authorizer.expires_at {
            return Err(Error::module(
                "consent_expired",
                format!("consent expired at {} (now {now})", authorizer.expires_at),
            ));
        }
        if authorizer.expires_at - now > MAX_CONSENT_TTL {
            return Err(Error::module(
                "consent_ttl",
                format!("consent outlives the {MAX_CONSENT_TTL}-block ceiling"),
            ));
        }
        let mut record = self.stored_account(number).await?;
        let keys = record.association()?;
        let full = keys.len() >= MAX_KEYS_PER_ACCOUNT;
        if full {
            return Err(Error::module(
                "key_cap",
                format!("account key cap reached ({MAX_KEYS_PER_ACCOUNT})"),
            ));
        }
        let authorizer_scheme = keys
            .get(&authorizer.key)
            .map(|meta| meta.scheme)
            .ok_or_else(|| {
                Error::module(
                    "key_index_corrupt",
                    "key index disagrees with its account record",
                )
            })?;

        // the consent names THIS key at ITS current generation, that account,
        // and that expiry -- nothing else.
        let generation = self.key_gen(&origin).await?;
        let preimage = add_key_preimage(
            &self.chain_id,
            scheme,
            &origin,
            generation,
            authorizer.account,
            authorizer.expires_at,
        );
        if !authorizer_scheme.verify(
            &authorizer.key,
            IDENTITY_ADD_KEY_NS,
            &preimage,
            &authorizer.proof,
        ) {
            return Err(Error::module(
                "consent_unverified",
                "authorizer consent does not verify",
            ));
        }
        let label = clean_label(label)?;
        let spent = next_key_generation(generation)?;

        record.association_mut()?.insert(
            origin.clone(),
            KeyMeta {
                scheme,
                label,
                added_at: now,
            },
        );
        record.updated_at = now;
        self.store_account(number, &record)?;
        self.store(key_owner_key(&origin), &number);
        // the consent just spent is dead from here on.
        self.store(key_gen_key(&origin), &spent);
        Ok(())
    }

    /// drop `key` from the origin's account: itself, or a key admitted no
    /// earlier than the origin was. never the last remaining one.
    async fn remove_key(&mut self, ctx: &mut dyn Ctx, key: Vec<u8>) -> Result<(), Error> {
        let (actor, mut record) = self.acting(ctx).await?;
        let Actor::Key {
            key: origin,
            account: number,
        } = actor
        else {
            return Err(Error::module(
                "not_a_key_account",
                "key operations are for member keys; a program account holds no key",
            ));
        };
        let keys = record.association_mut()?;
        let Some(target) = keys.get(&key) else {
            return Err(Error::module(
                "not_a_member_key",
                "target key is not a member of this account",
            ));
        };
        let last_key = keys.len() == 1;
        if last_key {
            return Err(Error::module(
                "last_key",
                "cannot remove the last key of an account",
            ));
        }
        // SENIORITY. the origin is a member, so the record holds its meta.
        // keys admitted in the same block are peers; a key admitted later is
        // junior and may be dropped. this is what an outstanding consent
        // cannot buy: the key it admits can never evict the members that
        // predate it, so a mis-issued ticket costs a squatter, not the
        // account.
        let removing_self = key == origin;
        let senior_target = target.added_at
            < keys
                .get(&origin)
                .expect("the origin is a member of its own account")
                .added_at;
        if !removing_self && senior_target {
            return Err(Error::module(
                "senior_key",
                "cannot remove a key admitted before your own",
            ));
        }
        keys.remove(&key);
        record.updated_at = ctx.env().consensus_time;
        self.store_account(number, &record)?;
        // the generation counter stays: a re-admission signs at the next one.
        self.staged.delete(key_owner_key(&key));
        Ok(())
    }

    /// rename the acting account.
    async fn set_name(&mut self, ctx: &mut dyn Ctx, name: String) -> Result<(), Error> {
        let (actor, mut record) = self.acting(ctx).await?;
        record.name = clean_name(name)?;
        record.updated_at = ctx.env().consensus_time;
        self.store_account(actor.account(), &record)
    }

    /// set the avatar ref and/or bio of the acting account. each field
    /// empty-trims to cleared, over its byte cap rejects.
    async fn set_profile(
        &mut self,
        ctx: &mut dyn Ctx,
        avatar: Option<String>,
        bio: Option<String>,
    ) -> Result<(), Error> {
        let (actor, mut record) = self.acting(ctx).await?;
        record.avatar = clean_field(avatar, MAX_AVATAR_REF_LEN, "avatar reference")?;
        record.bio = clean_field(bio, MAX_BIO_LEN, "bio")?;
        record.updated_at = ctx.env().consensus_time;
        self.store_account(actor.account(), &record)
    }

    /// found a program account executed by the emitting module for a live
    /// controller, and tell the executor in the same unit.
    async fn create_program(
        &mut self,
        ctx: &mut dyn Ctx,
        name: String,
        controller: AccountNumber,
        request: u64,
    ) -> Result<(), Error> {
        let executor = Self::acting_module(ctx)?;
        let name = clean_name(name)?;
        let controller_record = self.account(controller).await?.ok_or_else(|| {
            Error::module(
                "unknown_account",
                format!("controller {controller} is not an account"),
            )
        })?;
        if !controller_record.control.is_live() {
            return Err(Error::module(
                "controller_not_live",
                format!(
                    "controller {controller} cannot control: \
                     it is not key-held or an active program"
                ),
            ));
        }
        let next = self.next_number().await?;
        let controlled = self.controlled_by(controller).await?;
        let founding = decide_program_founding(
            name,
            controller,
            executor.clone(),
            next,
            controlled,
            ctx.env().consensus_time,
        )?;
        let account = founding.account;
        self.stage_plan(founding.plan);
        ctx.emit_msg(Msg {
            target: executor,
            payload: encode_event(&IdentityEvent::ProgramCreated {
                request,
                account,
                controller,
            }),
        });
        ctx.set_assigned(encode_assigned(&IdentityAssigned::Founded { account }));
        Ok(())
    }

    /// the executor sets a program's standing; the generation advances.
    async fn set_program_standing(
        &mut self,
        ctx: &mut dyn Ctx,
        account: AccountNumber,
        standing: ProgramStanding,
    ) -> Result<(), Error> {
        let executor = Self::acting_module(ctx)?;
        let record = self.account(account).await?.ok_or_else(|| {
            Error::module(
                "unknown_account",
                format!("account {account} does not exist"),
            )
        })?;
        let changed = decide_standing(record, &executor, standing, ctx.env().consensus_time)?;
        self.store_account(account, &changed)
    }

    /// the controller hands a program to another live account.
    async fn transfer_control(
        &mut self,
        ctx: &mut dyn Ctx,
        account: AccountNumber,
        to: AccountNumber,
    ) -> Result<(), Error> {
        let (actor, _) = self.acting(ctx).await?;
        let record = self.account(account).await?.ok_or_else(|| {
            Error::module(
                "unknown_account",
                format!("account {account} does not exist"),
            )
        })?;
        let from = record.control.program()?.controller;
        let to_record = self.account(to).await?.ok_or_else(|| {
            Error::module("unknown_account", format!("account {to} does not exist"))
        })?;
        let loaded = TransferLoaded {
            record,
            to_record,
            to_chain: self.controller_chain(to).await?,
            from_set: self.controlled_by(from).await?,
            to_set: self.controlled_by(to).await?,
        };
        let plan = decide_transfer(
            loaded,
            actor.account(),
            account,
            to,
            ctx.env().consensus_time,
        )?;
        self.stage_plan(plan);
        Ok(())
    }

    /// the controller freezes a program for good.
    async fn revoke_program(
        &mut self,
        ctx: &mut dyn Ctx,
        account: AccountNumber,
    ) -> Result<(), Error> {
        let (actor, _) = self.acting(ctx).await?;
        let record = self.account(account).await?.ok_or_else(|| {
            Error::module(
                "unknown_account",
                format!("account {account} does not exist"),
            )
        })?;
        let revoked = decide_revoke(record, actor.account(), ctx.env().consensus_time)?;
        self.store_account(account, &revoked)
    }
}

/// trim an account name: empty -> reject, over the limit -> reject.
fn clean_name(name: String) -> Result<String, Error> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(Error::module("bad_name", "account name is empty"));
    }
    if trimmed.len() > MAX_NAME_LEN {
        return Err(Error::module(
            "bad_name",
            format!("account name exceeds the {MAX_NAME_LEN}-byte limit"),
        ));
    }
    Ok(trimmed.to_string())
}

/// trim an optional profile field: `None` or empty-after-trim -> cleared
/// (`None`), over `max` bytes -> reject, else the trimmed string.
fn clean_field(value: Option<String>, max: usize, what: &str) -> Result<Option<String>, Error> {
    match value {
        None => Ok(None),
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else if trimmed.len() > max {
                Err(Error::module(
                    "too_long",
                    format!("{what} exceeds the {max}-byte limit"),
                ))
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
    }
}

/// trim a key label: empty -> `None` (no label), over the limit -> reject.
fn clean_label(label: Option<String>) -> Result<Option<String>, Error> {
    clean_field(label, MAX_LABEL_LEN, "key label")
}

#[cfg(test)]
mod tests;

// the wasm-guest port: the store-backed dispatch shell that adapts this module
// to the ducktape:module world. compiled only by the guest-builder's
// synthesized wasm32 cdylib workspace (feature `guest`), never by the native
// build.
#[cfg(feature = "guest")]
mod guest;
