//! the governance module's public wire surface — types only.
//!
//! governance defaults to one ballot per validator node. the validators may
//! initialize non-transferable Identity-account shares, then governance can
//! switch future proposals between validator and account-share mode. every new
//! proposal freezes the electorate that decides it. passing
//! membership actions are performed by emitting the valset op as a host-drained
//! follow-up — governance remains the ONLY authorized author of valset changes.
//!
//! authorship is trusted because the ordered lane VERIFIES frame signatures:
//! `Origin::External(pubkey)` reaching a module is authenticated. validator
//! mode keys a ballot by that node key; share mode resolves the key to its one
//! Identity account (`OfKey`), so no key can forge another principal's ballot.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

// the invite capability: token types + verification, shared by the node's
// mint/lobby paths, workspace-config and the module's in-consensus `Redeem`.
pub mod invite;

/// floor on `activation_lead` (`UpdateModule`/`RegisterModule`), validated at
/// Propose: the lead is blocks after the EXECUTE height, and the modules
/// registry itself refuses any `activation_height <= execute_height +
/// modules::MIN_SWAP_LEAD` — so a lead this small can NEVER execute
/// successfully. strictly above [`modules::MIN_SWAP_LEAD`] guarantees the
/// registry's own floor is cleared whatever height Execute lands at.
pub const MIN_ACTIVATION_LEAD: u64 = modules::MIN_SWAP_LEAD + 1;
/// ceiling on `activation_lead` — a fat-fingered or hostile lead must not
/// arm a swap so far in the future it is effectively unreachable. generous,
/// same order as the module's `MAX_VOTING_PERIOD`.
pub const MAX_ACTIVATION_LEAD: u64 = 1_000_000_000;

/// what a passing proposal DOES.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum GovAction {
    /// admit a validator: emits `ValsetMsg::Join { key }` on execution.
    AddValidator { key: Vec<u8> },
    /// remove a validator: emits `ValsetMsg::Leave { key }` on execution.
    RemoveValidator { key: Vec<u8> },
    /// a binding signal with no on-chain effect beyond its recorded outcome.
    Signal { text: String },
    /// grant RESIDENT standing (mesh + statesync, no quorum seat — the
    /// staged-admission tier): emits `ValsetMsg::Grant { key }` on execution.
    AddResident { key: Vec<u8> },
    /// revoke resident standing: emits `ValsetMsg::Revoke { key }` on
    /// execution.
    RemoveResident { key: Vec<u8> },
    /// initialize non-transferable account shares and enable share mode. the
    /// allocation must be non-empty, unique by account number, and name
    /// existing Identity accounts. initialization is one-time; the registry is
    /// retained when governance later switches back to validator ballots.
    AdoptShares { allocations: Vec<ShareAllocation> },
    /// set one Identity account's shares. zero removes the account from the
    /// registry. only available after shares were configured; the current mode
    /// freezes the electorate and structural threshold that decides it.
    SetShares { account_id: u64, shares: u64 },
    /// choose the electorate for future proposals. `true` uses the configured
    /// account shares; `false` restores one ballot per validator node. this
    /// proposal itself is always decided by the mode frozen when it was opened.
    SetShareMode { enabled: bool },
    /// AUTHORIZE a height-gated wasm module code swap: on execution, computes
    /// `activation_height = execute_height + activation_lead` and emits
    /// `ModulesMsg::ScheduleSwap { name, module_id, activation_height, code_hash }`.
    /// the lead is RELATIVE (blocks after EXECUTION, not propose): an absolute
    /// height chosen at propose time can go stale if the ballot outlasts it,
    /// permanently rejecting the Execute (governance issue #1775). governance
    /// only authorizes — the code registry is the sole authority for the
    /// min-lead / at-most-one / no-op-swap gates, and the swap arms purely on
    /// height (the bytes travel out-of-band, content-addressed by the hash).
    UpdateModule {
        name: String,
        module_id: String,
        /// blocks after the EXECUTE height at which the swap activates;
        /// validated at Propose against [`crate::MIN_ACTIVATION_LEAD`] and
        /// [`crate::MAX_ACTIVATION_LEAD`].
        activation_lead: u64,
        /// sha256 of the target component bytes (32 bytes).
        code_hash: Vec<u8>,
    },
    /// AUTHORIZE the post-genesis ADMISSION of a brand-new registry entry — a
    /// wasm module, or a view alone (`kind`): on execution, computes
    /// `activation_height = execute_height + activation_lead` and emits
    /// `ModulesMsg::ScheduleRegister { name, module_id, kind,
    /// activation_height, code_hash }`. governance only authorizes
    /// — the code registry owns the not-already-registered / min-lead gates,
    /// the R = n readiness quorum arms it, and the host instantiates the
    /// module from the content-addressed bytes at the activation boundary.
    /// cancellation before the boundary rides `CancelModuleUpdate` (an
    /// admission cancel removes the registry entry entirely).
    RegisterModule {
        name: String,
        module_id: String,
        /// what the artifact is; fixed for the entry's life.
        kind: modules::Kind,
        /// blocks after the EXECUTE height at which the module activates;
        /// validated at Propose against [`crate::MIN_ACTIVATION_LEAD`] and
        /// [`crate::MAX_ACTIVATION_LEAD`].
        activation_lead: u64,
        /// sha256 of the initial component bytes (32 bytes).
        code_hash: Vec<u8>,
        /// the data-plane lanes the admitted module brings. Authorized by the
        /// same ballot as the code, because a lane id is network-wide state:
        /// the registry owns the collision and range gates, this only decides
        /// that the network wants them at all.
        #[serde(default)]
        lanes: Vec<modules::LaneDecl>,
    },
    /// AUTHORIZE clearing a pending module code swap before its boundary: emits
    /// `ModulesMsg::CancelSwap { name, module_id }` on execution.
    CancelModuleUpdate { name: String, module_id: String },
    /// set (or clear, with `standing: None`) one target's required submit
    /// standing in the acl module: emits `AclMsg::SetPolicy` on execution.
    /// `target` is a module id or the `"*"` wildcard. the acl table is EMPTY
    /// at genesis (allow-all); this action is how a network tightens — one
    /// ballot, not a release. a net without a wired acl module
    /// deterministically rejects it at propose.
    SetAclPolicy {
        target: String,
        standing: Option<acl::Standing>,
    },
}

/// one non-transferable governance-share allocation, keyed by the Identity
/// account NUMBER.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ShareAllocation {
    pub account_id: u64,
    pub shares: u64,
}

/// what principal the proposal's ballot keys identify.
#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum VoterKind {
    ValidatorNode,
    Account,
}

/// the exact decision rule frozen with a proposal.
#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum VotingRule {
    /// Pass once `yes_power >= required_yes`; this covers validator-set
    /// snapshots and structural proposals requiring two thirds of all shares.
    Threshold { required_yes: u64 },
    /// At the deadline, participation must reach `quorum` and yes must exceed
    /// no. Early passage is allowed only when no remaining ballot can reverse it.
    ParticipatingMajority { quorum: u64 },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum GovMsg {
    /// open a proposal. the verified submitter must belong to the current
    /// electorate selected by the current governance mode.
    /// the voting deadline is `consensus_time + voting_period`.
    Propose {
        proposal_id: String,
        action: GovAction,
        voting_period: u64,
    },
    /// cast (or change) the submitter's ballot while voting is open.
    Vote { proposal_id: String, approve: bool },
    /// tally and settle. anyone may trigger it after the deadline, or early
    /// once the proposal's frozen rule can no longer be reversed. passing
    /// membership actions emit their valset op as a follow-up in the same
    /// block. only executable inside a bounded window: past
    /// `deadline + EXECUTION_GRACE` the proposal is refused and reaped as
    /// Rejected on the spot instead of tallying a frozen electorate that may
    /// no longer hold standing.
    Execute { proposal_id: String },
    /// redeem an invite: no ballot — MINTING was the admission decision. the
    /// op carries the token's fields plus the joiner key and its
    /// proof-of-possession (all raw bytes, mirroring the lobby announce);
    /// the module re-verifies both signatures against the network binding,
    /// requires the issuer to be a CURRENT member, and enforces single-use
    /// via the redeemed-nonce set in consensus state. success emits
    /// `ValsetMsg::Grant` in the same block (full node RESIDENT standing —
    /// mesh + statesync, no quorum seat). EVERY invite is bearer (the
    /// targeted form was dropped): no `target` — the
    /// `proof` binds the redemption to whichever key presents it and the
    /// nonce set makes that exactly-once.
    Redeem {
        issuer: Vec<u8>,
        nonce: Vec<u8>,
        token_sig: Vec<u8>,
        joiner: Vec<u8>,
        proof: Vec<u8>,
        /// the token's unix-seconds expiry — enforced against block time.
        expires_unix_secs: u64,
    },
}

/// a proposal's lifecycle. `Open` accepts votes; the rest are terminal.
#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProposalStatus {
    Open,
    Passed,
    Rejected,
}

/// the readable projection of one proposal. every principal (`proposer`,
/// `votes`, `electorate`) is a node key in validator mode and
/// `identity::account_principal(n)` (8 bytes LE) in share mode, per
/// `voter_kind`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProposalView {
    pub proposal_id: String,
    pub action: GovAction,
    /// the submitter's principal: its node key, or its account principal.
    pub proposer: Vec<u8>,
    pub created_at: u64,
    pub deadline: u64,
    pub status: ProposalStatus,
    /// ballots by validator-node key or account principal, per `voter_kind`.
    pub votes: Vec<(Vec<u8>, bool)>,
    pub voter_kind: VoterKind,
    /// the proposal-time power snapshot — every proposal freezes its electorate
    /// when it is opened. keyed like `votes`.
    pub electorate: Vec<(Vec<u8>, u64)>,
    pub voting_rule: VotingRule,
}

/// the current non-transferable share registry.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SharesView {
    pub active: bool,
    pub allocations: Vec<ShareAllocation>,
    pub total: u64,
}

/// the readable projection of one settled invite redemption — who invited
/// whom, and when it landed.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RedemptionView {
    /// the redeemed token's nonce (the single-use key).
    pub nonce: Vec<u8>,
    /// the admitted key.
    pub joiner: Vec<u8>,
    /// the member whose token authorized the admission.
    pub issuer: Vec<u8>,
    /// the block height the redemption landed at.
    pub height: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum GovQuery {
    /// every proposal, sorted by id (the roster-served enumeration — see the
    /// module doc for why this listing stays canonical).
    Proposals,
    /// one proposal by id.
    Proposal { proposal_id: String },
    /// one settled invite redemption by token nonce — the point read the
    /// node's join-lobby spent-invite pre-check (V6) consumes. redemptions
    /// have no enumeration: the set is point records alone.
    Redemption { nonce: Vec<u8> },
    /// the current share registry and whether it governs new proposals.
    Shares,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum GovReply {
    Proposals(Vec<ProposalView>),
    Proposal(Option<ProposalView>),
    Redemption(Option<RedemptionView>),
    Shares(SharesView),
}

pub fn encode_msg(m: &GovMsg) -> Vec<u8> {
    sdk::wire::encode(m)
}
pub fn decode_msg(b: &[u8]) -> Result<GovMsg, String> {
    sdk::wire::decode(b)
}
pub fn encode_query(q: &GovQuery) -> Vec<u8> {
    sdk::wire::encode(q)
}
pub fn decode_query(b: &[u8]) -> Result<GovQuery, String> {
    sdk::wire::decode(b)
}
pub fn encode_reply(r: &GovReply) -> Vec<u8> {
    sdk::wire::encode(r)
}
pub fn decode_reply(b: &[u8]) -> Result<GovReply, String> {
    sdk::wire::decode(b)
}
