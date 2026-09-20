//! qmdb-backed validator governance with optional account-share voting.
//!
//! the module that closes the private network's membership loop: a CURRENT
//! valset member proposes (`AddValidator` / `RemoveValidator` / `Signal`),
//! members vote before a consensus-time deadline, and anyone may trigger
//! `Execute` once the outcome is decidable. a passing membership action emits
//! the valset op as a host-drained follow-up in the SAME block — and the
//! valset module only accepts membership ops from THIS module's origin (its
//! wired `governance_id`, never a bare module origin), so governance is the
//! sole authorized author of membership change.
//!
//! ## why authorship is trustworthy
//!
//! the ordered lane verifies every frame's signature before the host sees it,
//! so `Origin::External(pubkey)` here is AUTHENTICATED. validator mode keys
//! ballots by that node key directly; share mode deterministically resolves
//! the origin key to its Identity account (`OfKey`), so no key can forge
//! another principal's vote. a node key is never an account: no node is bound
//! to one, and no account fans out to nodes.
//!
//! ## determinism
//!
//! governance defaults to validator-node ballots. `AdoptShares` initializes an
//! explicit Identity-account allocation; governance may then switch future
//! proposals between validator and account principals. every new proposal
//! freezes its electorate and rule, so later mode/membership/share changes
//! cannot move its decision boundary or count the same shares twice.
//!
//! ## State model
//!
//! pure logic over a host-injected [`sdk::MerkleStore`]: the HOST constructs
//! the concrete store (qmdb today — `statesync::qmdb::QmdbStore`) and hands it
//! to [`Governance::new`], so this crate never names a storage crate. one
//! logical record per proposal (`prop\0{id}`) and per settled invite
//! redemption (`red\0{nonce}`), plus three aggregate records:
//!
//! - the proposal ROSTER (the sorted OPEN-proposal-id list, bounded by
//!   [`MAX_PROPOSALS`] and, per submitter, by
//!   [`MAX_OPEN_PROPOSALS_PER_SUBMITTER`]) — the ONE enumeration read. an id
//!   leaves the roster the moment it settles: `Execute` evicts it when the
//!   tally passes or rejects it, and `Propose` opportunistically evicts any
//!   OTHER roster entry whose own voting deadline has passed with nobody
//!   having executed it (the only way an unexecuted proposal expires
//!   deterministically without a network-wide per-block tick). the proposal
//!   RECORD itself is kept forever under its own key — only the roster
//!   narrows — so a settled id's history is still a point read away. it
//!   stays canonical because governance's read model CANNOT move to the
//!   derived index tier: a proposal's frozen electorate, every ballot's
//!   principal resolution, and the settlement tally all read the
//!   valset/identity SIBLINGS at execute time, so an index fold over
//!   governance's own applied ops could only reproduce proposal state by
//!   re-implementing the consensus tally over other modules' state — a
//!   second consensus implementation, which is worse than a bounded
//!   canonical id list. the operator ceremonies (the CLI's
//!   adopt-an-open-proposal flow) consume this listing;
//! - the SHARE REGISTRY (bounded by [`MAX_SHARE_ACCOUNTS`]) — consensus
//!   consumes it whenever a proposal freezes an account electorate;
//! - the share-MODE flag — consensus consumes it on every `Propose`.
//!
//! redemptions have NO enumeration: the exactly-once gate and the node's
//! join-lobby pre-check (V6) are both point reads by nonce, so the set lives
//! as point records alone and grows without an aggregate to poison.
//!
//! writes are staged during a block and flushed to the store in one batch at
//! `commit_block`; the module root IS the store's merkle root. sync belongs
//! to the store, not this module: a joiner rebuilds the concrete store from a
//! peer (`QmdbStore::sync_from`) and wraps a fresh `Governance` around it.
//!
//! oversized values never reach the store (the poison-value lesson — the qmdb
//! wire codec bounds a value at decode, so an over-cap committed value would
//! wedge every syncing peer): a NEW proposal record is byte-gated at half of
//! [`MAX_PROPOSAL_RECORD_BYTES`] (accumulated ballots can at most double the
//! frozen-electorate section, so a settled record stays under the full cap),
//! the roster is byte-gated at [`MAX_ROSTER_RECORD_BYTES`] on top of its
//! id-length and count caps, and a redemption record is fixed-size by
//! construction (32-byte keys, 16-byte nonce).
//!
//! ## Genesis config (the invite binding)
//!
//! the per-network invite binding reaches the NATIVE module through
//! [`Governance::with_invite_binding`]. the wasm tenant is fixed bytes, so
//! there the binding rides GENESIS CONFIG: the host seeds the reserved
//! `__config` entry ([`sdk::genesis_config`]) into this module's store at
//! genesis construction — under [`sdk::store_key`], the same logical→store
//! mapping every record here uses — and the guest decodes it per dispatch.
//! the config is consensus state in the store's merkle root from genesis and
//! rides state-sync like any other record. this module never writes that key.

// the wire surface: this module's shared types, flattened at the crate root.
mod wire;
pub use wire::*;
pub mod identity_contract;
mod module_contracts;

use std::collections::BTreeMap;

use borsh::{BorshDeserialize, BorshSerialize};
use commonware_codec::DecodeExt as _;
use commonware_cryptography::ed25519;
use identity_contract::{
    AccountView, IdentityQuery, IdentityReply, account_principal,
    decode_reply as identity_decode_reply, encode_query as identity_encode_query,
};
use module_contracts::{acl, modules, valset};
use modules::{ModulesMsg, encode_msg as modules_encode_msg};
use sdk::{
    Ctx, Error, MerkleStore, Module, ModuleId, Msg, Origin, ResolverSyncTarget, StagedStore,
    StateRoot, StateSyncHandle,
};
use valset::{
    MAX_MEMBERS, ValsetMsg, ValsetQuery, ValsetReply, decode_reply as valset_decode_reply,
    encode_msg as valset_encode_msg, encode_query as valset_encode_query,
};

/// ceiling on `voting_period` (in consensus-time units) — a fat-fingered or
/// hostile period must not park a proposal Open forever past any usable
/// horizon. views advance about once per finalized op, so this is generous.
const MAX_VOTING_PERIOD: u64 = 1_000_000_000;

/// how long a decided-but-unexecuted proposal stays enactable past its
/// `deadline`. `Execute` tallies against the electorate FROZEN at `Propose`,
/// so an unbounded window lets a since-removed electorate enact a mandate
/// long after it lost standing (the whole point of a deadline). Past
/// `deadline + EXECUTION_GRACE` the proposal is dead: refused on `Execute`
/// and settled `Rejected` on the spot, same as any other reap.
const EXECUTION_GRACE: u64 = 100_000;

/// Keep every share value and total exact in the JavaScript operator client.
const MAX_SAFE_SHARES: u64 = 9_007_199_254_740_991;
/// The frozen electorate copies the complete allocation into each proposal.
/// This is intentionally the small-network implementation; checkpointed power
/// history replaces it if real deployments outgrow this bound.
const MAX_SHARE_ACCOUNTS: usize = 256;
/// `proposal_id` byte bound — roster arithmetic and record keys need ids that
/// cannot balloon.
pub const MAX_PROPOSAL_ID_BYTES: usize = 256;
/// ceiling on the roster of currently-OPEN proposals — settled ids (passed,
/// rejected, or expired) are evicted, so this bounds live contention, not
/// the network's lifetime proposal count. proposing past this is refused
/// loudly at propose.
pub const MAX_PROPOSALS: usize = 1024;
/// ceiling on OPEN proposals a single frozen `proposer` principal may hold at
/// once — closes the roster-filling attack [`MAX_PROPOSALS`] alone does not:
/// without this, one electorate member submits proposals with a voting
/// window long enough that nobody can execute them early, and eviction on
/// expiry never triggers before the cap bites. small on purpose — a
/// legitimate member has no reason to run more than a handful of proposals
/// concurrently.
pub const MAX_OPEN_PROPOSALS_PER_SUBMITTER: usize = 8;
/// serialized roster-record byte bound, enforced at propose — the backstop
/// on top of the id-length and count caps that keeps the committed record
/// far under the qmdb value-decode ceiling (the poison-value lesson: a
/// committed over-cap value would wedge every syncing peer). generous: borsh
/// renders the worst-case roster (1024 ids of 256 bytes) in ~260 KiB.
pub const MAX_ROSTER_RECORD_BYTES: usize = 512 * 1024;
/// serialized proposal-record ceiling. a NEW proposal is gated at HALF this
/// value: ballots accrue only from principals inside the frozen electorate
/// and a ballot entry is no larger than its electorate entry, so a fully
/// voted record is at most twice its at-propose size — the settled record
/// can never cross the full cap (which itself sits under the qmdb 1 MiB
/// value-decode ceiling).
pub const MAX_PROPOSAL_RECORD_BYTES: usize = 512 * 1024;

/// per-proposal record key: prefix + 0 + id (the single-component shape chat
/// uses). safe because every key literal below is fixed and none is another
/// followed by a 0 byte.
fn prop_key(proposal_id: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(4 + 1 + proposal_id.len());
    key.extend_from_slice(b"prop");
    key.push(0);
    key.extend_from_slice(proposal_id.as_bytes());
    key
}

/// per-redemption record key: prefix + 0 + nonce (16 raw bytes).
fn red_key(nonce: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(3 + 1 + nonce.len());
    key.extend_from_slice(b"red");
    key.push(0);
    key.extend_from_slice(nonce);
    key
}

/// the proposal roster's whole key. collides with no `prop\0...`/`red\0...`
/// key (nor the host-seeded `__config` genesis-config record).
const PROPOSAL_ROSTER_KEY: &[u8] = b"proposals";

/// the share registry's whole key. present = shares were configured.
const SHARES_KEY: &[u8] = b"shares";

/// the share-mode flag's whole key. absent = validator ballots (the default).
const SHARE_MODE_KEY: &[u8] = b"mode";

/// one proposal, stored verbatim — borsh writes the ballot and electorate
/// `BTreeMap`s length-prefixed in key order, so one proposal state has
/// exactly one encoding.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
struct Proposal {
    action: GovAction,
    proposer: Vec<u8>,
    created_at: u64,
    deadline: u64,
    status: ProposalStatus,
    votes: BTreeMap<Vec<u8>, bool>,
    /// the electorate and rule frozen at `Propose` — every proposal has one, so
    /// later mode/membership/share changes cannot move its decision boundary.
    electorate: Electorate,
}

#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
struct Electorate {
    voter_kind: VoterKind,
    powers: BTreeMap<Vec<u8>, u64>,
    rule: VotingRule,
}

/// who a verified frame origin speaks for (see [`Governance::resolve_actor`]).
enum Actor {
    /// the origin is a member key of Identity account `number`: in share mode
    /// it casts that account's ONE ballot, whichever of the account's keys
    /// signed.
    Account { number: u64 },
    /// the origin belongs to no account — it acts as itself, a node key, and
    /// only a validator-mode electorate can seat it (by the origin bytes the
    /// caller already holds, so the variant carries nothing).
    Node,
}

/// one settled invite redemption — the single-use record plus the audit
/// trail (who invited whom, when). the nonce is the record KEY, not a field.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
struct Redemption {
    joiner: Vec<u8>,
    issuer: Vec<u8>,
    height: u64,
}

pub struct Governance {
    id: ModuleId,
    /// the id of the valset module this governance instance gates. genesis
    /// wiring — identical on every node.
    valset_id: ModuleId,
    /// the id of the modules registry a passing `UpdateModule`/`CancelModuleUpdate`
    /// authorizes (the code-registry path — the same module, gated separately).
    /// genesis wiring — identical on every node; `None` (a net without the code
    /// registry wired) rejects those proposals at the door, deterministically.
    code_registry_id: Option<ModuleId>,
    /// the Identity account registry used in account-share mode (an origin
    /// key resolves to its account number through `OfKey`).
    identity_id: ModuleId,
    /// the acl submit-policy table `SetAclPolicy` proposals write. genesis
    /// wiring — identical on every node; `None` (a net without the module)
    /// rejects those proposals at the door, deterministically.
    acl_id: Option<ModuleId>,
    /// the network binding invite tokens sign over (the genesis namespace).
    /// genesis wiring — identical on every node of the same network. `None`
    /// (a shape without a descriptor) refuses every `Redeem` with a clear
    /// error, deterministically.
    invite_binding: Option<Vec<u8>>,
    /// the host-injected authenticated store plus this block's staging overlay
    /// (read-your-writes, folded into `root()` at `commit_block`). store key
    /// is `sha256(logical_key)`, owned by [`StagedStore`].
    staged: StagedStore,
}

impl Governance {
    /// wrap the host-constructed store under module identity `id`.
    pub fn new(
        id: impl Into<ModuleId>,
        store: Box<dyn MerkleStore>,
        valset_id: impl Into<ModuleId>,
        identity_id: impl Into<ModuleId>,
    ) -> Self {
        Self {
            id: id.into(),
            valset_id: valset_id.into(),
            code_registry_id: None,
            identity_id: identity_id.into(),
            acl_id: None,
            invite_binding: None,
            staged: StagedStore::new(store),
        }
    }

    /// enable the acl path (`SetAclPolicy`) against the submit-policy module at
    /// `id`. genesis wiring — every node of a network must wire the same id (or
    /// none), or nodes diverge on whether those proposals are accepted.
    pub fn with_acl(mut self, id: impl Into<ModuleId>) -> Self {
        self.acl_id = Some(id.into());
        self
    }

    /// enable the code-registry path (`UpdateModule`/`CancelModuleUpdate`) on the
    /// modules registry. genesis wiring — every node of a network must wire the
    /// same id (or none), or nodes diverge on whether those proposals are
    /// accepted.
    pub fn with_code_registry(mut self, id: impl Into<ModuleId>) -> Self {
        self.code_registry_id = Some(id.into());
        self
    }

    /// wire the network binding invite tokens verify against (the genesis
    /// namespace). every node of a network must wire the same bytes — a node
    /// without it rejects `Redeem` ops its peers accept, which forks.
    pub fn with_invite_binding(mut self, binding: impl Into<Vec<u8>>) -> Self {
        self.invite_binding = Some(binding.into());
        self
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

    /// stage a value whose serialized size is bounded by construction (a
    /// redemption record, the share registry, the mode flag) — see the module
    /// doc's poison-value paragraph. proposals and the roster go through
    /// [`Self::store_bounded`].
    fn store<T>(&mut self, key: Vec<u8>, value: &T)
    where
        T: BorshSerialize,
    {
        self.staged.stage(
            key,
            borsh::to_vec(value).expect("governance value is serializable"),
        );
    }

    /// stage a value only if its serialized size fits `cap` — the write-time
    /// guard against poison values (the qmdb codec cap is decode-only).
    fn store_bounded<T>(
        &mut self,
        key: Vec<u8>,
        value: &T,
        cap: usize,
        what: &str,
    ) -> Result<(), Error>
    where
        T: BorshSerialize,
    {
        let bytes = borsh::to_vec(value).expect("governance value is serializable");
        if bytes.len() > cap {
            return Err(Error::module(
                "record_too_large",
                format!("{what} record too large: {} > {cap} bytes", bytes.len()),
            ));
        }
        self.staged.stage(key, bytes);
        Ok(())
    }

    async fn proposal(&self, proposal_id: &str) -> Result<Option<Proposal>, Error> {
        self.load(&prop_key(proposal_id)).await
    }

    /// stage a settled/updated proposal record under the FULL byte cap — see
    /// [`MAX_PROPOSAL_RECORD_BYTES`] for why accrued ballots cannot cross it.
    fn store_proposal(&mut self, proposal_id: &str, proposal: &Proposal) -> Result<(), Error> {
        self.store_bounded(
            prop_key(proposal_id),
            proposal,
            MAX_PROPOSAL_RECORD_BYTES,
            "proposal",
        )
    }

    /// the proposal roster — every OPEN proposal id, sorted. record and
    /// roster are staged (and commit or abort) together, so roster
    /// membership implies an existing record; the roster is the ONE
    /// existence authority at propose and the ONE enumeration read (module
    /// doc: why it is canonical, and why it narrows on settlement).
    async fn roster(&self) -> Result<Vec<String>, Error> {
        Ok(self.load(PROPOSAL_ROSTER_KEY).await?.unwrap_or_default())
    }

    fn stage_roster(&mut self, roster: &Vec<String>) -> Result<(), Error> {
        self.store_bounded(
            PROPOSAL_ROSTER_KEY.to_vec(),
            roster,
            MAX_ROSTER_RECORD_BYTES,
            "proposal roster",
        )
    }

    /// settle every OPEN roster entry already past its EXECUTION deadline
    /// (`deadline + EXECUTION_GRACE`) as Rejected — nobody executed it in
    /// time, or nobody ever will again — and evict it from `roster`. this is
    /// the one place an unexecuted or now-stale-electorate proposal expires
    /// deterministically without a network-wide per-block tick, so `Propose`,
    /// `Vote`, and `Execute` all run it before acting: `Propose` right where
    /// the roster cap and the per-submitter cap are about to be checked,
    /// `Vote`/`Execute` so expiry never depends on someone else proposing.
    /// when `Execute` calls this on the very proposal it targets, reaping it
    /// here IS the bounded-execution-window refusal — the subsequent "no
    /// such open proposal" check catches it. returns how many of the
    /// SURVIVING open proposals belong to `proposer`, computed in the same
    /// pass so the caps cost one roster walk together (bounded by
    /// [`MAX_PROPOSALS`] point reads, the same cost class as
    /// `GovQuery::Proposals`).
    async fn reap_expired(
        &mut self,
        now: u64,
        proposer: &[u8],
        roster: &mut Vec<String>,
    ) -> Result<usize, Error> {
        let mut open_by_proposer = 0usize;
        let mut i = 0;
        while i < roster.len() {
            let id = roster[i].clone();
            let mut proposal = self.proposal(&id).await?.ok_or_else(|| {
                Error::module(
                    "missing_proposal_record",
                    format!("missing proposal record: {id}"),
                )
            })?;
            let execution_deadline = proposal.deadline.saturating_add(EXECUTION_GRACE);
            let expired = proposal.status == ProposalStatus::Open && now >= execution_deadline;
            if expired {
                proposal.status = ProposalStatus::Rejected;
                self.store_proposal(&id, &proposal)?;
                roster.remove(i);
                continue;
            }
            if proposal.proposer.as_slice() == proposer {
                open_by_proposer += 1;
            }
            i += 1;
        }
        Ok(open_by_proposer)
    }

    /// [`Self::reap_expired`] against the whole roster, for callers that
    /// don't need the per-submitter count `Propose` uses.
    async fn reap_roster(&mut self, now: u64) -> Result<(), Error> {
        let mut roster = self.roster().await?;
        self.reap_expired(now, &[], &mut roster).await?;
        self.stage_roster(&roster)
    }

    /// persist a just-tallied proposal (status already terminal — Passed or
    /// Rejected) and evict its id from the open-proposal roster in the same
    /// staged write. the proposal RECORD stays under its own key forever;
    /// only the roster narrows.
    async fn settle(&mut self, proposal_id: &str, proposal: &Proposal) -> Result<(), Error> {
        self.store_proposal(proposal_id, proposal)?;
        let mut roster = self.roster().await?;
        if let Some(position) = roster.iter().position(|id| id.as_str() == proposal_id) {
            roster.remove(position);
            self.stage_roster(&roster)?;
        }
        Ok(())
    }

    /// the share registry: account number → shares.
    async fn shares(&self) -> Result<Option<BTreeMap<u64, u64>>, Error> {
        Ok(self
            .load::<Vec<ShareAllocation>>(SHARES_KEY)
            .await?
            .map(|allocations| {
                allocations
                    .into_iter()
                    .map(|a| (a.account_id, a.shares))
                    .collect()
            }))
    }

    fn stage_shares(&mut self, shares: &BTreeMap<u64, u64>) {
        let allocations: Vec<ShareAllocation> = shares
            .iter()
            .map(|(account_id, shares)| ShareAllocation {
                account_id: *account_id,
                shares: *shares,
            })
            .collect();
        // bounded by construction: at most MAX_SHARE_ACCOUNTS allocations.
        self.store(SHARES_KEY.to_vec(), &allocations);
    }

    async fn share_mode(&self) -> Result<bool, Error> {
        Ok(self.load(SHARE_MODE_KEY).await?.unwrap_or(false))
    }

    // ---- sibling reads ------------------------------------------------------

    /// the AUTHENTICATED submitter key, or an error for module/system origins —
    /// proposals and ballots are member actions, never emitted by modules.
    fn external_origin(ctx: &dyn Ctx) -> Result<Vec<u8>, Error> {
        match &ctx.env().origin {
            Origin::External(key) => Ok(key.clone()),
            other => Err(Error::module(
                "external_origin_required",
                format!("governance actions require an external submitter, got {other:?}"),
            )),
        }
    }

    /// resolve the verified frame origin to a governance ACTOR through the ONE
    /// resolver, `OfKey`: a key of an Identity account acts for that account;
    /// any other key acts as ITSELF, a node key. only share mode reads this —
    /// validator mode seats node keys with no identity read at all.
    async fn resolve_actor(&self, ctx: &dyn Ctx, origin: &[u8]) -> Result<Actor, Error> {
        let query = IdentityQuery::OfKey {
            key: origin.to_vec(),
        };
        match self.identity_account(ctx, query).await? {
            Some(account) => Ok(Actor::Account {
                number: account.number,
            }),
            None => Ok(Actor::Node),
        }
    }

    /// the Identity account a submitter speaks for in account (share) mode.
    /// a key that belongs to no account has no share-mode standing: a node
    /// key is never an account.
    async fn submitter_account(&self, ctx: &dyn Ctx, submitter: &[u8]) -> Result<u64, Error> {
        match self.resolve_actor(ctx, submitter).await? {
            Actor::Account { number } => Ok(number),
            Actor::Node => Err(Error::module(
                "no_identity_account",
                "submitter key belongs to no Identity account",
            )),
        }
    }

    /// the CURRENT member set: the valset module's staged-over-committed
    /// projection, via the shared `valset::members` read.
    async fn members(&self, ctx: &dyn Ctx) -> Result<Vec<Vec<u8>>, Error> {
        valset::members(ctx, &self.valset_id).await
    }

    async fn identity_account(
        &self,
        ctx: &dyn Ctx,
        query: IdentityQuery,
    ) -> Result<Option<AccountView>, Error> {
        let reply = ctx
            .query(&self.identity_id, &identity_encode_query(&query))
            .await?;
        match identity_decode_reply(&reply)
            .map_err(|e| Error::module("identity_reply_decode", e))?
        {
            IdentityReply::Account(account) => Ok(account),
            IdentityReply::Accounts(_) | IdentityReply::Resolved(_) | IdentityReply::Gen(_) => Err(
                Error::module("unexpected_identity_reply", "unexpected identity reply"),
            ),
        }
    }

    async fn require_account(&self, ctx: &dyn Ctx, number: u64) -> Result<(), Error> {
        let exists = self
            .identity_account(ctx, IdentityQuery::Get { number })
            .await?
            .is_some();
        if !exists {
            return Err(Error::module(
                "unknown_identity_account",
                "share allocation names no existing Identity account",
            ));
        }
        Ok(())
    }

    fn total_power<K>(powers: &BTreeMap<K, u64>) -> Result<u64, Error> {
        let total = powers.values().try_fold(0u64, |sum, power| {
            sum.checked_add(*power).ok_or_else(|| {
                Error::module("shares_overflow", "total governance shares overflow u64")
            })
        })?;
        if total == 0 || total > MAX_SAFE_SHARES {
            return Err(Error::module(
                "bad_total_shares",
                format!("total governance shares must be in 1..={MAX_SAFE_SHARES}"),
            ));
        }
        Ok(total)
    }

    /// #1777: would a policy of `standing` on `target` still let the
    /// electorate that decides FUTURE governance proposals submit to
    /// governance itself? only a target that GOVERNS governance matters —
    /// its own module id, or the `"*"` wildcard fallback every unlisted
    /// target resolves through. `standing: None` (clearing an entry) and
    /// `Standing::Open` are always safe — they widen or leave access alone.
    /// a validator-ballot electorate is bare node keys (never account
    /// members — a node key is never an Identity account) holding
    /// `Standing::Validator`/`Standing::Node`; a share-mode electorate is
    /// Identity-account principals holding `Standing::User`. `SetPolicy` is
    /// reachable only through governance, so a policy neither electorate
    /// kind can satisfy would brick the module and everything gated behind
    /// it, permanently.
    fn electorate_can_still_submit(
        share_mode: bool,
        governance_id: &str,
        target: &str,
        standing: Option<acl::Standing>,
    ) -> bool {
        let governs_governance = target == governance_id || target == acl::WILDCARD_TARGET;
        if !governs_governance {
            return true;
        }
        match standing {
            None | Some(acl::Standing::Open) => true,
            Some(acl::Standing::User) => share_mode,
            Some(acl::Standing::Node) | Some(acl::Standing::Validator) => !share_mode,
        }
    }

    fn threshold_rule(total: u64, action: &GovAction, share_mode: bool) -> VotingRule {
        if !share_mode {
            return VotingRule::Threshold {
                required_yes: total / 2 + 1,
            };
        }
        match action {
            GovAction::Signal { .. } => VotingRule::ParticipatingMajority {
                quorum: total / 2 + total % 2,
            },
            _ => VotingRule::Threshold {
                // ceil(2n/3), without multiplication overflow.
                required_yes: total - total / 3,
            },
        }
    }

    async fn frozen_electorate(
        &self,
        ctx: &dyn Ctx,
        submitter: &[u8],
        action: &GovAction,
    ) -> Result<(Vec<u8>, Electorate), Error> {
        if self.share_mode().await? {
            let shares = self.shares().await?.ok_or_else(|| {
                Error::module(
                    "no_share_registry",
                    "account-share mode has no configured registry",
                )
            })?;
            let number = self.submitter_account(ctx, submitter).await?;
            let holds_shares = shares.contains_key(&number);
            if !holds_shares {
                return Err(Error::module(
                    "not_a_shareholder",
                    "submitter account holds no governance shares",
                ));
            }
            let total = Self::total_power(&shares)?;
            let powers = shares
                .into_iter()
                .map(|(number, shares)| (account_principal(number), shares))
                .collect();
            return Ok((
                account_principal(number),
                Electorate {
                    voter_kind: VoterKind::Account,
                    rule: Self::threshold_rule(total, action, true),
                    powers,
                },
            ));
        }

        // validator mode (the default): ballots are NODE-keyed — N validators
        // = N votes — and the submitter must ITSELF be a member node, with no
        // identity read (a node key is never an account, and an account never
        // fans out to nodes).
        let members = self.members(ctx).await?;
        let submitter_is_member = members.iter().any(|member| member == submitter);
        if !submitter_is_member {
            return Err(Error::module(
                "not_a_member",
                "submitter is not a validator-set member node",
            ));
        }
        let powers: BTreeMap<Vec<u8>, u64> =
            members.into_iter().map(|member| (member, 1)).collect();
        let total = Self::total_power(&powers)?;
        Ok((
            submitter.to_vec(),
            Electorate {
                voter_kind: VoterKind::ValidatorNode,
                rule: Self::threshold_rule(total, action, false),
                powers,
            },
        ))
    }

    /// the CURRENT resident set (valset's staged-over-committed projection) —
    /// the standing a redeemed joiner already holds.
    async fn residents(&self, ctx: &dyn Ctx) -> Result<Vec<Vec<u8>>, Error> {
        let reply = ctx
            .query(
                &self.valset_id,
                &valset_encode_query(&ValsetQuery::Residents),
            )
            .await?;
        match valset_decode_reply(&reply).map_err(|e| Error::module("valset_reply_decode", e))? {
            ValsetReply::Residents(residents) => Ok(residents),
            other => Err(Error::module(
                "unexpected_valset_reply",
                format!("valset answered a Residents query with {other:?}"),
            )),
        }
    }

    fn view_of(id: &str, p: &Proposal) -> ProposalView {
        let electorate = &p.electorate;
        ProposalView {
            proposal_id: id.to_string(),
            action: p.action.clone(),
            proposer: p.proposer.clone(),
            created_at: p.created_at,
            deadline: p.deadline,
            status: p.status,
            votes: p.votes.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            voter_kind: electorate.voter_kind,
            electorate: electorate
                .powers
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            voting_rule: electorate.rule,
        }
    }

    async fn shares_view(&self) -> Result<SharesView, Error> {
        let Some(shares) = self.shares().await? else {
            return Ok(SharesView {
                active: false,
                allocations: Vec::new(),
                total: 0,
            });
        };
        Ok(SharesView {
            active: self.share_mode().await?,
            allocations: shares
                .iter()
                .map(|(account_id, shares)| ShareAllocation {
                    account_id: *account_id,
                    shares: *shares,
                })
                .collect(),
            total: shares.values().sum(),
        })
    }

    async fn normalize_share_action(
        &self,
        ctx: &dyn Ctx,
        mut action: GovAction,
    ) -> Result<GovAction, Error> {
        match &mut action {
            GovAction::AdoptShares { allocations } => {
                if self.shares().await?.is_some() {
                    return Err(Error::module(
                        "shares_already_configured",
                        "governance shares are already configured",
                    ));
                }
                if allocations.is_empty() || allocations.len() > MAX_SHARE_ACCOUNTS {
                    return Err(Error::module(
                        "bad_share_allocation",
                        format!(
                            "initial share allocation must contain 1..={MAX_SHARE_ACCOUNTS} accounts"
                        ),
                    ));
                }
                let mut normalized = BTreeMap::new();
                for allocation in std::mem::take(allocations) {
                    if allocation.shares == 0 {
                        return Err(Error::module(
                            "bad_share_allocation",
                            "initial share allocations must be positive",
                        ));
                    }
                    self.require_account(ctx, allocation.account_id).await?;
                    let duplicate = normalized
                        .insert(allocation.account_id, allocation.shares)
                        .is_some();
                    if duplicate {
                        return Err(Error::module(
                            "bad_share_allocation",
                            "initial share allocation contains a duplicate account",
                        ));
                    }
                }
                Self::total_power(&normalized)?;
                *allocations = normalized
                    .into_iter()
                    .map(|(account_id, shares)| ShareAllocation { account_id, shares })
                    .collect();
            }
            GovAction::SetShares { account_id, shares } => {
                let current = self.shares().await?.ok_or_else(|| {
                    Error::module(
                        "shares_not_configured",
                        "adopt governance shares before changing them",
                    )
                })?;
                if *shares > MAX_SAFE_SHARES {
                    return Err(Error::module(
                        "bad_share_count",
                        format!("account shares must be at most {MAX_SAFE_SHARES}"),
                    ));
                }
                self.require_account(ctx, *account_id).await?;
                let mut after = current.clone();
                if *shares == 0 {
                    after.remove(account_id);
                } else {
                    after.insert(*account_id, *shares);
                }
                if after.len() > MAX_SHARE_ACCOUNTS {
                    return Err(Error::module(
                        "share_registry_full",
                        format!("share registry supports at most {MAX_SHARE_ACCOUNTS} accounts"),
                    ));
                }
                Self::total_power(&after)?;
            }
            GovAction::SetShareMode { enabled } => {
                if *enabled && self.shares().await?.is_none() {
                    return Err(Error::module(
                        "shares_not_configured",
                        "configure governance shares before enabling share mode",
                    ));
                }
                if *enabled == self.share_mode().await? {
                    return Err(Error::module(
                        "voting_mode_unchanged",
                        "governance is already using the requested voting mode",
                    ));
                }
            }
            _ => {}
        }
        Ok(action)
    }

    // ---- the op handlers ----------------------------------------------------

    async fn handle_propose(
        &mut self,
        ctx: &mut dyn Ctx,
        proposal_id: String,
        action: GovAction,
        voting_period: u64,
    ) -> Result<(), Error> {
        if proposal_id.is_empty() {
            return Err(Error::module(
                "bad_proposal_id",
                "proposal_id must not be empty",
            ));
        }
        if proposal_id.len() > MAX_PROPOSAL_ID_BYTES {
            return Err(Error::module(
                "bad_proposal_id",
                format!(
                    "proposal_id exceeds {MAX_PROPOSAL_ID_BYTES} bytes ({} given)",
                    proposal_id.len()
                ),
            ));
        }
        if voting_period == 0 || voting_period > MAX_VOTING_PERIOD {
            return Err(Error::module(
                "bad_voting_period",
                format!("voting_period must be in 1..={MAX_VOTING_PERIOD}"),
            ));
        }
        if let GovAction::AddValidator { key }
        | GovAction::RemoveValidator { key }
        | GovAction::AddResident { key }
        | GovAction::RemoveResident { key } = &action
        {
            // shape-check the key here so a proposal that can never execute is
            // rejected at the door, not at tally time.
            if key.len() != 32 {
                return Err(Error::module(
                    "bad_membership_key",
                    "membership key must be a 32-byte ed25519 public key",
                ));
            }
        }
        // a promotion names a key the network has already met. `AddValidator`
        // is the second half of a STAGED admission — valset is deliberately
        // permissionless ("no authorization, no gating", its own header), so
        // governance is the layer that decides who may be seated, and the
        // resident tier is what "has a node behind it, and it is caught up"
        // means. seating a key nobody has met grows the quorum by a seat that
        // never votes, and the chain stops at the next epoch cutover with no
        // way back: removing the phantom is itself a proposal, and it now
        // needs the phantom's ballot to pass.
        if let GovAction::AddValidator { key } = &action {
            let seated = self.members(ctx).await?.iter().any(|m| m == key);
            let stands_for_promotion =
                seated || self.residents(ctx).await?.iter().any(|r| r == key);
            if !stands_for_promotion {
                return Err(Error::module(
                    "not_a_resident",
                    "a validator is promoted out of the resident tier — \
                     grant standing first and let it sync",
                ));
            }
        }
        // module-update authorizations: shape-checked at the door (a proposal
        // that can never execute is rejected here, not at tally time); the code
        // registry's min-lead / at-most-one / no-op gates are NOT duplicated —
        // the modules registry is their sole authority at ingest. a net without a
        // wired code registry deterministically rejects these (genesis wiring is
        // identical on every node).
        if let GovAction::UpdateModule {
            name, module_id, ..
        }
        | GovAction::RegisterModule {
            name, module_id, ..
        }
        | GovAction::CancelModuleUpdate { name, module_id } = &action
        {
            if self.code_registry_id.is_none() {
                return Err(Error::module(
                    "no_code_registry",
                    "no code registry wired: module updates are not available on this network",
                ));
            }
            if name.is_empty() {
                return Err(Error::module(
                    "bad_module_update_name",
                    "module update name must not be empty",
                ));
            }
            if module_id.is_empty() {
                return Err(Error::module(
                    "empty_module_id",
                    "module_id must not be empty",
                ));
            }
        }
        if let GovAction::UpdateModule { code_hash, .. }
        | GovAction::RegisterModule { code_hash, .. } = &action
            && code_hash.len() != modules::CODE_HASH_LEN
        {
            return Err(Error::module(
                "bad_code_hash",
                format!(
                    "code_hash must be {} bytes (sha256 of the component)",
                    modules::CODE_HASH_LEN
                ),
            ));
        }
        // the lead is RELATIVE to the EXECUTE height, not the propose height
        // (issue #1775: an absolute height chosen here can go stale if the
        // ballot outlasts it, permanently rejecting Execute). shape-checked
        // here so a lead the registry can never accept is refused at the
        // door, not discovered by every Execute retrying and failing forever.
        if let GovAction::UpdateModule {
            activation_lead, ..
        }
        | GovAction::RegisterModule {
            activation_lead, ..
        } = &action
            && !(MIN_ACTIVATION_LEAD..=MAX_ACTIVATION_LEAD).contains(activation_lead)
        {
            return Err(Error::module(
                "bad_activation_lead",
                format!(
                    "activation_lead must be in \
                     {MIN_ACTIVATION_LEAD}..={MAX_ACTIVATION_LEAD} blocks after execution"
                ),
            ));
        }
        // acl policy authorizations: shape-checked at the door like the module
        // updates above (a proposal that can never execute is rejected here,
        // not at tally time); the acl module's own target validation is the
        // sole authority at ingest. a net without a wired acl module
        // deterministically rejects these (genesis wiring is identical on
        // every node).
        if let GovAction::SetAclPolicy { target, standing } = &action {
            if self.acl_id.is_none() {
                return Err(Error::module(
                    "no_acl_module",
                    "no acl module wired: submit-policy changes are not available on this network",
                ));
            }
            let well_formed_target = !target.is_empty()
                && target.trim() == target
                && target.len() <= acl::MAX_TARGET_LEN;
            if !well_formed_target {
                return Err(Error::module(
                    "bad_acl_target",
                    format!(
                        "acl target must be a non-empty, untrimmed module id of at most {} bytes",
                        acl::MAX_TARGET_LEN
                    ),
                ));
            }
            // #1777: never let a proposal that CAN pass lock the electorate
            // out of governance itself — SetPolicy is reachable only through
            // this module, so a policy neither ballot kind can satisfy would
            // brick the network permanently, with no repair proposal able to
            // reach the door that just closed on it.
            let share_mode = self.share_mode().await?;
            if !Self::electorate_can_still_submit(share_mode, &self.id, target, standing.clone()) {
                return Err(Error::module(
                    "electorate_lockout",
                    "acl policy would lock the current electorate out of governance itself",
                ));
            }
        }
        // AN ID IS SPENT FOREVER, and the check is against the RECORD, not the
        // roster. The roster holds OPEN ids only, while a settled proposal's
        // record is kept under its own key for good — so a roster-only check
        // let a second `Propose` reuse a settled id and OVERWRITE the record,
        // erasing the settled outcome and its ballots. Worse, it is invisible:
        // a driver that waits for "the proposal exists" is answered by the
        // stale record before the new one lands, and reports the old outcome
        // for a ceremony that never voted (#1766).
        let id_is_spent = self.proposal(&proposal_id).await?.is_some();
        if id_is_spent {
            return Err(Error::module(
                "proposal_id_spent",
                format!("proposal already exists: {proposal_id}"),
            ));
        }
        let mut roster = self.roster().await?;
        let submitter = Self::external_origin(ctx)?;
        let (proposer, electorate) = self.frozen_electorate(ctx, &submitter, &action).await?;
        let now = ctx.env().consensus_time;
        // reap anything already past its own deadline before either cap
        // below — otherwise a submitter who never calls `Execute` keeps a
        // permanent roster slot (and a permanent per-submitter slot) past
        // its own voting window.
        let open_by_proposer = self.reap_expired(now, &proposer, &mut roster).await?;
        if roster.len() >= MAX_PROPOSALS {
            return Err(Error::module(
                "proposal_cap",
                format!("proposal cap reached ({MAX_PROPOSALS})"),
            ));
        }
        if open_by_proposer >= MAX_OPEN_PROPOSALS_PER_SUBMITTER {
            return Err(Error::module(
                "submitter_proposal_cap",
                format!("submitter already has {MAX_OPEN_PROPOSALS_PER_SUBMITTER} open proposals"),
            ));
        }
        // Gate the submitter before resolving up to MAX_SHARE_ACCOUNTS Identity
        // records for an adoption proposal.
        let action = self.normalize_share_action(ctx, action).await?;

        let deadline = now.checked_add(voting_period).ok_or_else(|| {
            Error::module(
                "deadline_overflow",
                "voting deadline overflows consensus time",
            )
        })?;
        let proposal = Proposal {
            action,
            proposer,
            created_at: now,
            deadline,
            status: ProposalStatus::Open,
            votes: BTreeMap::new(),
            electorate,
        };
        // both byte gates first: a refusal must stage nothing new for THIS
        // proposal (any reaping above already staged its own settle writes —
        // fine either way this call ends: accepted, they're real
        // settlements that happened anyway; rejected, the whole unit's stage
        // rolls back with it and the next `Propose` redoes the reap). a NEW
        // proposal is gated at HALF the record cap so accrued ballots (at
        // most one per electorate entry, each no larger than its entry) can
        // never push the settled record past the full cap.
        self.store_bounded(
            prop_key(&proposal_id),
            &proposal,
            MAX_PROPOSAL_RECORD_BYTES / 2,
            "proposal",
        )?;
        let position = roster.binary_search(&proposal_id).unwrap_or_else(|p| p);
        roster.insert(position, proposal_id);
        self.stage_roster(&roster)?;
        Ok(())
    }

    async fn handle_vote(
        &mut self,
        ctx: &mut dyn Ctx,
        proposal_id: String,
        approve: bool,
    ) -> Result<(), Error> {
        self.reap_roster(ctx.env().consensus_time).await?;
        let mut proposal = self.proposal(&proposal_id).await?.ok_or_else(|| {
            Error::module(
                "no_such_proposal",
                format!("no such proposal: {proposal_id}"),
            )
        })?;
        if proposal.status != ProposalStatus::Open {
            return Err(Error::module("proposal_settled", "proposal is settled"));
        }
        if ctx.env().consensus_time >= proposal.deadline {
            return Err(Error::module(
                "voting_closed",
                "voting closed at the deadline",
            ));
        }
        let submitter = Self::external_origin(ctx)?;
        let electorate = &proposal.electorate;
        // the ONE ballot this op casts, by the proposal's frozen principal
        // kind: a node key is its own ballot (N validators = N votes); an
        // account key casts its account's ballot — however many keys the
        // account has, re-voting overwrites the same principal.
        let voter = match electorate.voter_kind {
            VoterKind::ValidatorNode => submitter,
            VoterKind::Account => account_principal(self.submitter_account(ctx, &submitter).await?),
        };
        let in_electorate = electorate.powers.contains_key(&voter);
        if !in_electorate {
            return Err(Error::module(
                "not_in_electorate",
                "voter is not in the frozen electorate",
            ));
        }
        proposal.votes.insert(voter, approve);
        self.store_proposal(&proposal_id, &proposal)
    }

    async fn handle_execute(
        &mut self,
        ctx: &mut dyn Ctx,
        proposal_id: String,
    ) -> Result<(), Error> {
        let mut proposal = self.proposal(&proposal_id).await?.ok_or_else(|| {
            Error::module(
                "no_such_proposal",
                format!("no such proposal: {proposal_id}"),
            )
        })?;
        if proposal.status != ProposalStatus::Open {
            return Err(Error::module("proposal_settled", "proposal is settled"));
        }

        let now = ctx.env().consensus_time;
        // the bounded execution window: a frozen electorate cannot enact its
        // mandate forever, only until `deadline + EXECUTION_GRACE` — past
        // that it is settled Rejected right here (an `Ok` write, not a
        // refusal that would roll it back) instead of tallying a membership
        // snapshot that may no longer hold standing.
        let execution_deadline = proposal.deadline.saturating_add(EXECUTION_GRACE);
        let past_execution_window = now >= execution_deadline;
        if past_execution_window {
            proposal.status = ProposalStatus::Rejected;
            return self.settle(&proposal_id, &proposal).await;
        }

        // opportunistic hygiene: reap every OTHER roster entry already past
        // its own execution window too, so expiry never depends on someone
        // else calling `Propose`.
        self.reap_roster(now).await?;

        let electorate = &proposal.electorate;
        let mut yes = 0u64;
        let mut no = 0u64;
        for (principal, power) in &electorate.powers {
            match proposal.votes.get(principal) {
                Some(true) => yes += power,
                Some(false) => no += power,
                None => {}
            }
        }
        let total = Self::total_power(&electorate.powers)?;
        let rule = electorate.rule;

        let (passes, decidable_early) = match rule {
            VotingRule::Threshold { required_yes } => (yes >= required_yes, yes >= required_yes),
            VotingRule::ParticipatingMajority { quorum } => {
                let participation = yes + no;
                let passes = participation >= quorum && yes > no;
                // Safe early passage only when every still-uncast ballot could
                // vote no and yes would remain the strict majority.
                let irreversible = participation >= quorum && yes > total - yes;
                (passes, irreversible)
            }
        };
        if now < proposal.deadline && !decidable_early {
            return Err(Error::module(
                "not_decidable_yet",
                format!(
                    "not decidable yet: voting open until {} (yes={yes}, no={no}, total={total})",
                    proposal.deadline
                ),
            ));
        }

        if passes {
            proposal.status = ProposalStatus::Passed;
            // a passing membership action is PERFORMED by emitting the valset
            // op as a follow-up — the host drains it in this same block, and
            // valset accepts it because the origin is Module(governance).
            match &proposal.action {
                GovAction::AddValidator { key } => {
                    // mirror valset's own require_capacity: a key already
                    // seated is an idempotent no-op Join (nothing to
                    // pre-check), but admitting a NEW key past the validator
                    // cap would Err out of the follow-up and reject the whole
                    // Execute unit (#1776's AddValidator sibling case) — check
                    // it here and settle Rejected instead, the same way the
                    // set-emptying Leave above settles rather than errors.
                    let members = self.members(ctx).await?;
                    let already_seated = members.iter().any(|m| m == key);
                    let would_overflow_capacity = !already_seated && members.len() >= MAX_MEMBERS;
                    // the propose door refuses a key with no resident record,
                    // and it is not the last word: standing can be revoked
                    // while the ballot is open, and a proposal can arrive from
                    // a peer's synced store minted before this rule. seating a
                    // key that stands for nothing halts the chain at the next
                    // epoch cutover, so settle Rejected here too.
                    let stands_for_promotion =
                        already_seated || self.residents(ctx).await?.iter().any(|r| r == key);
                    if would_overflow_capacity || !stands_for_promotion {
                        proposal.status = ProposalStatus::Rejected;
                    } else {
                        ctx.emit_msg(Msg {
                            target: self.valset_id.clone(),
                            payload: valset_encode_msg(&ValsetMsg::Join { key: key.clone() }),
                        });
                    }
                }
                GovAction::RemoveValidator { key } => {
                    // never enact a removal that would empty the validator set: a
                    // zero-validator orderer hits commonware `quorum(0)`, which
                    // panics. the valset Leave handler enforces this invariant
                    // authoritatively (returning an Err that would abort the WHOLE
                    // block), so we pre-check here and cleanly REJECT the proposal
                    // instead — the happy path never emits a set-emptying Leave.
                    let members = self.members(ctx).await?;
                    if members.iter().all(|m| m == key) {
                        proposal.status = ProposalStatus::Rejected;
                    } else {
                        ctx.emit_msg(Msg {
                            target: self.valset_id.clone(),
                            payload: valset_encode_msg(&ValsetMsg::Leave { key: key.clone() }),
                        });
                    }
                }
                // a passing module-update authorization is PERFORMED the same
                // way: emit the modreg op as a follow-up, accepted because the
                // origin is Module(governance). governance only authorizes; the
                // code registry owns the min-lead / at-most-one / no-op gates,
                // and the swap arms purely on height. Propose door-checks the
                // wiring, so an unwired registry here means the proposal was
                // adopted from a peer's synced store minted under DIFFERENT
                // genesis wiring — reject it cleanly rather than emit into the
                // void.
                GovAction::UpdateModule {
                    name,
                    module_id,
                    activation_lead,
                    code_hash,
                } => match &self.code_registry_id {
                    Some(registry) => {
                        // relative to THIS execute height, not the propose
                        // height (#1775) — a ballot that outlives its lead
                        // still schedules cleanly whenever it finally executes.
                        let activation_height = ctx.env().height.saturating_add(*activation_lead);
                        ctx.emit_msg(Msg {
                            target: registry.clone(),
                            payload: modules_encode_msg(&ModulesMsg::ScheduleSwap {
                                name: name.clone(),
                                module_id: module_id.clone(),
                                activation_height,
                                code_hash: code_hash.clone(),
                            }),
                        })
                    }
                    None => proposal.status = ProposalStatus::Rejected,
                },
                // a passing ADMISSION is performed the same way; the code
                // registry owns the not-already-registered / min-lead gates and
                // the R=n readiness quorum is what arms it.
                GovAction::RegisterModule {
                    name,
                    module_id,
                    kind,
                    activation_lead,
                    code_hash,
                    lanes,
                } => match &self.code_registry_id {
                    Some(registry) => {
                        let activation_height = ctx.env().height.saturating_add(*activation_lead);
                        ctx.emit_msg(Msg {
                            target: registry.clone(),
                            payload: modules_encode_msg(&ModulesMsg::ScheduleRegister {
                                name: name.clone(),
                                module_id: module_id.clone(),
                                kind: *kind,
                                activation_height,
                                code_hash: code_hash.clone(),
                                lanes: lanes.clone(),
                            }),
                        })
                    }
                    None => proposal.status = ProposalStatus::Rejected,
                },
                GovAction::CancelModuleUpdate { name, module_id } => match &self.code_registry_id {
                    Some(registry) => ctx.emit_msg(Msg {
                        target: registry.clone(),
                        payload: modules_encode_msg(&ModulesMsg::CancelSwap {
                            name: name.clone(),
                            module_id: module_id.clone(),
                        }),
                    }),
                    None => proposal.status = ProposalStatus::Rejected,
                },
                // the staged-admission grant/revoke: mirror handle_redeem's
                // own pre-checks (#1776) — valset's handle_grant Errs when the
                // key already sits in the validator tier ("resident standing
                // is the pre-promotion tier"), which would reject the whole
                // Execute unit forever if the key was promoted after this
                // proposal opened. a stale mandate settles cleanly Rejected
                // instead. an already-resident key is left to valset's own
                // idempotent no-op (like AddValidator's already-seated case
                // above), and the resident cap is pre-checked the same way
                // the validator cap is.
                GovAction::AddResident { key } => {
                    let members = self.members(ctx).await?;
                    let already_validator = members.iter().any(|m| m == key);
                    if already_validator {
                        proposal.status = ProposalStatus::Rejected;
                    } else {
                        let residents = self.residents(ctx).await?;
                        let already_resident = residents.iter().any(|o| o == key);
                        let would_overflow_capacity =
                            !already_resident && residents.len() >= MAX_MEMBERS;
                        if would_overflow_capacity {
                            proposal.status = ProposalStatus::Rejected;
                        } else {
                            ctx.emit_msg(Msg {
                                target: self.valset_id.clone(),
                                payload: valset_encode_msg(&ValsetMsg::Grant { key: key.clone() }),
                            });
                        }
                    }
                }
                GovAction::RemoveResident { key } => ctx.emit_msg(Msg {
                    target: self.valset_id.clone(),
                    payload: valset_encode_msg(&ValsetMsg::Revoke { key: key.clone() }),
                }),
                GovAction::AdoptShares { allocations } => {
                    if self.shares().await?.is_some() {
                        // A competing adoption may have won since this proposal
                        // opened. Settle cleanly; initialization is one-time.
                        proposal.status = ProposalStatus::Rejected;
                    } else {
                        let shares: BTreeMap<u64, u64> = allocations
                            .iter()
                            .map(|a| (a.account_id, a.shares))
                            .collect();
                        if Self::total_power(&shares).is_err() {
                            proposal.status = ProposalStatus::Rejected;
                        } else {
                            self.stage_shares(&shares);
                            self.store(SHARE_MODE_KEY.to_vec(), &true);
                        }
                    }
                }
                GovAction::SetShares { account_id, shares } => {
                    let Some(mut after) = self.shares().await? else {
                        proposal.status = ProposalStatus::Rejected;
                        return self.settle(&proposal_id, &proposal).await;
                    };
                    if *shares == 0 {
                        after.remove(account_id);
                    } else {
                        after.insert(*account_id, *shares);
                    }
                    if after.len() > MAX_SHARE_ACCOUNTS || Self::total_power(&after).is_err() {
                        proposal.status = ProposalStatus::Rejected;
                    } else {
                        self.stage_shares(&after);
                    }
                }
                GovAction::SetShareMode { enabled } => {
                    if *enabled && self.shares().await?.is_none() {
                        proposal.status = ProposalStatus::Rejected;
                    } else {
                        self.store(SHARE_MODE_KEY.to_vec(), enabled);
                    }
                }
                GovAction::SetAclPolicy { target, standing } => match &self.acl_id {
                    // the propose-time gate refused an unwired net, so this arm
                    // only rejects on a wiring change between propose and pass.
                    None => proposal.status = ProposalStatus::Rejected,
                    Some(acl_id) => {
                        // #1777: re-check at Execute — a DIFFERENT proposal may
                        // have flipped share_mode since this one opened, and
                        // the frozen electorate is a ballot snapshot, not a
                        // guarantee about who submits AFTER this lands.
                        let share_mode = self.share_mode().await?;
                        let safe = Self::electorate_can_still_submit(
                            share_mode,
                            &self.id,
                            target,
                            standing.clone(),
                        );
                        if safe {
                            ctx.emit_msg(Msg {
                                target: acl_id.clone(),
                                payload: acl::encode_msg(&acl::AclMsg::SetPolicy {
                                    target: target.clone(),
                                    standing: standing.clone(),
                                }),
                            });
                        } else {
                            proposal.status = ProposalStatus::Rejected;
                        }
                    }
                },
                GovAction::Signal { .. } => {}
            }
        } else {
            proposal.status = ProposalStatus::Rejected;
        }
        self.settle(&proposal_id, &proposal).await
    }

    /// redeem an invite — no ballot, the mint WAS the admission decision.
    /// verification is fully in-consensus so every validator settles the op
    /// identically: token signature and join proof against the wired binding,
    /// issuer against CURRENT membership, nonce against the redeemed set
    /// (single-use — a second redemption of the same token deterministically
    /// rejects). success emits the observer grant in the same block.
    #[allow(clippy::too_many_arguments)]
    async fn handle_redeem(
        &mut self,
        ctx: &mut dyn Ctx,
        issuer: Vec<u8>,
        nonce: Vec<u8>,
        token_sig: Vec<u8>,
        joiner: Vec<u8>,
        proof: Vec<u8>,
        expires_unix_secs: u64,
    ) -> Result<(), Error> {
        // the submitter must be an authenticated frame origin, but is NOT
        // required to be a member — the token authorizes the admission, not
        // the relaying node.
        Self::external_origin(ctx)?;
        let Some(binding) = self.invite_binding.as_deref() else {
            return Err(Error::module(
                "no_invite_binding",
                "this network is not wired for invite redemption (no binding)",
            ));
        };
        let issuer_key = ed25519::PublicKey::decode(issuer.as_slice())
            .map_err(|e| Error::module("bad_issuer_key", format!("issuer key: {e}")))?;
        let joiner_key = ed25519::PublicKey::decode(joiner.as_slice())
            .map_err(|e| Error::module("bad_joiner_key", format!("joiner key: {e}")))?;
        if nonce.len() != invite::INVITE_NONCE_LEN {
            return Err(Error::module(
                "bad_invite_nonce",
                format!("nonce must be {} bytes", invite::INVITE_NONCE_LEN),
            ));
        }
        let mut nonce_arr = [0u8; invite::INVITE_NONCE_LEN];
        nonce_arr.copy_from_slice(&nonce);
        let sig = ed25519::Signature::decode(token_sig.as_slice())
            .map_err(|e| Error::module("bad_token_signature", format!("token signature: {e}")))?;
        let proof_sig = ed25519::Signature::decode(proof.as_slice())
            .map_err(|e| Error::module("bad_join_proof", format!("join proof: {e}")))?;
        // EVERY invite is bearer (the targeted form was dropped): there is
        // no target lock. The join proof below binds the redemption to
        // whichever key presents it, and the nonce set makes that
        // exactly-once — that is the whole containment story.
        let token = invite::InviteToken {
            issuer: issuer_key,
            nonce: nonce_arr,
            expires_unix_secs,
            sig,
        };
        if !invite::verify_invite_token(&token, binding) {
            return Err(Error::module(
                "invite_token_unverified",
                "invite token signature does not verify for this network",
            ));
        }
        // expiry is NOT enforced here: `consensus_time` is block height on
        // this chain, so no deterministic wall clock exists in-consensus.
        // enforcement lives at the joiner's decode and at every gating
        // member's wall clock before it submits this Redeem (lobby + intro
        // doorbells), and single-use bounds any residual window. the field
        // stays in the op because it is signature-covered — members need it
        // to check expiry against the same bytes the issuer signed.
        if !invite::verify_join_proof(&joiner_key, binding, &token, &proof_sig) {
            return Err(Error::module(
                "join_proof_unverified",
                "joiner proof-of-possession does not verify",
            ));
        }
        // a removed member's outstanding invites die with it.
        let members = self.members(ctx).await?;
        if !members.iter().any(|m| m == &issuer) {
            return Err(Error::module(
                "issuer_not_a_member",
                "the inviting member is no longer part of this network",
            ));
        }
        // an invite grants exactly one thing: valset RESIDENT standing
        // (mesh + statesync, pre-promotion). submit authorization needs no
        // grant at all — the door admits any validly signed frame, and
        // per-module policy is the acl module's dispatch gate.
        if members.iter().any(|m| m == &joiner) {
            return Err(Error::module(
                "joiner_already_validator",
                "joiner is already a validator",
            ));
        }
        if self.residents(ctx).await?.iter().any(|o| o == &joiner) {
            return Err(Error::module(
                "joiner_already_resident",
                "joiner already holds resident standing",
            ));
        }
        let grant = Msg {
            target: self.valset_id.clone(),
            payload: valset_encode_msg(&ValsetMsg::Grant {
                key: joiner.clone(),
            }),
        };
        // exactly-once: the nonce is the single-use key (the
        // staged-over-committed read collapses two redemptions in one
        // block to first-wins too).
        if self.load::<Redemption>(&red_key(&nonce)).await?.is_some() {
            return Err(Error::module(
                "invite_already_redeemed",
                "invite already redeemed",
            ));
        }
        self.store(
            red_key(&nonce),
            &Redemption {
                joiner,
                issuer,
                height: ctx.env().height,
            },
        );
        ctx.emit_msg(grant);
        Ok(())
    }
}

#[async_trait::async_trait(?Send)]
impl Module for Governance {
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
            GovMsg::Propose {
                proposal_id,
                action,
                voting_period,
            } => {
                self.handle_propose(ctx, proposal_id, action, voting_period)
                    .await
            }
            GovMsg::Vote {
                proposal_id,
                approve,
            } => self.handle_vote(ctx, proposal_id, approve).await,
            GovMsg::Execute { proposal_id } => self.handle_execute(ctx, proposal_id).await,
            GovMsg::Redeem {
                issuer,
                nonce,
                token_sig,
                joiner,
                proof,
                expires_unix_secs,
            } => {
                self.handle_redeem(
                    ctx,
                    issuer,
                    nonce,
                    token_sig,
                    joiner,
                    proof,
                    expires_unix_secs,
                )
                .await
            }
        }
    }

    /// read projection — committed plus this block's staged changes (the
    /// staged-over-committed store view).
    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        match decode_query(req).map_err(|e| Error::module("codec", e))? {
            GovQuery::Proposals => {
                // walk the roster by derived key (≤ MAX_PROPOSALS point
                // reads). a rostered id without a record is a store bug —
                // loud, never skipped.
                let mut views = Vec::new();
                for proposal_id in self.roster().await? {
                    let Some(proposal) = self.proposal(&proposal_id).await? else {
                        return Err(Error::module(
                            "missing_proposal_record",
                            format!("missing proposal record: {proposal_id}"),
                        ));
                    };
                    views.push(Self::view_of(&proposal_id, &proposal));
                }
                Ok(encode_reply(&GovReply::Proposals(views)))
            }
            GovQuery::Proposal { proposal_id } => Ok(encode_reply(&GovReply::Proposal(
                self.proposal(&proposal_id)
                    .await?
                    .map(|p| Self::view_of(&proposal_id, &p)),
            ))),
            GovQuery::Redemption { nonce } => {
                let view =
                    self.load::<Redemption>(&red_key(&nonce))
                        .await?
                        .map(|r| RedemptionView {
                            nonce: nonce.clone(),
                            joiner: r.joiner,
                            issuer: r.issuer,
                            height: r.height,
                        });
                Ok(encode_reply(&GovReply::Redemption(view)))
            }
            GovQuery::Shares => Ok(encode_reply(&GovReply::Shares(self.shares_view().await?))),
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

// the wasm-guest port: the store-backed dispatch shell that adapts this module
// to the ducktape:module world. compiled only by the guest-builder's
// synthesized wasm32 cdylib workspace (feature `guest`), never by the native
// build.
#[cfg(feature = "guest")]
mod guest;

#[cfg(test)]
mod tests {
    use super::*;

    // #1777: `electorate_can_still_submit` is the one place the self-lockout
    // rule lives — unit-tested directly against both electorate kinds, since
    // the end-to-end host tests (governance_gates_acl.rs) cannot cheaply drive
    // share mode's full AdoptShares/identity ceremony for every case.

    #[test]
    fn none_and_open_are_always_permitted_on_governance() {
        for share_mode in [false, true] {
            assert!(Governance::electorate_can_still_submit(
                share_mode,
                "governance",
                "governance",
                None
            ));
            assert!(Governance::electorate_can_still_submit(
                share_mode,
                "governance",
                "governance",
                Some(acl::Standing::Open)
            ));
        }
    }

    #[test]
    fn validator_mode_electorate_cannot_satisfy_user_standing_on_governance_or_wildcard() {
        // a validator-ballot electorate is bare node keys — never account
        // members — so User is the one standing that would brick it.
        assert!(!Governance::electorate_can_still_submit(
            false,
            "governance",
            "governance",
            Some(acl::Standing::User)
        ));
        assert!(!Governance::electorate_can_still_submit(
            false,
            "governance",
            acl::WILDCARD_TARGET,
            Some(acl::Standing::User)
        ));
        // Node and Validator are exactly what the electorate already holds.
        assert!(Governance::electorate_can_still_submit(
            false,
            "governance",
            "governance",
            Some(acl::Standing::Node)
        ));
        assert!(Governance::electorate_can_still_submit(
            false,
            "governance",
            "governance",
            Some(acl::Standing::Validator)
        ));
    }

    #[test]
    fn share_mode_electorate_cannot_satisfy_node_or_validator_standing_on_governance_or_wildcard() {
        // a share-mode electorate is Identity-account principals — not
        // guaranteed to be node keys at all.
        assert!(!Governance::electorate_can_still_submit(
            true,
            "governance",
            "governance",
            Some(acl::Standing::Node)
        ));
        assert!(!Governance::electorate_can_still_submit(
            true,
            "governance",
            acl::WILDCARD_TARGET,
            Some(acl::Standing::Validator)
        ));
        // User is exactly what the electorate already holds.
        assert!(Governance::electorate_can_still_submit(
            true,
            "governance",
            "governance",
            Some(acl::Standing::User)
        ));
    }

    #[test]
    fn a_target_that_does_not_govern_governance_is_always_permitted() {
        // any standing on an unrelated target never bricks governance itself
        // — this rule guards only "governance" and the "*" fallback.
        assert!(Governance::electorate_can_still_submit(
            false,
            "governance",
            "chat",
            Some(acl::Standing::User)
        ));
        assert!(Governance::electorate_can_still_submit(
            true,
            "governance",
            "chat",
            Some(acl::Standing::Node)
        ));
    }
}
