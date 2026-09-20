//! The node's consumed module contracts.
//!
//! These are deliberately local wire views.  The module crates own their
//! encoders; this crate only names the fields it reads and keeps golden byte
//! checks at this consumer boundary.

use serde::{Deserialize, Serialize};

pub mod modules {
    use super::*;

    pub const MIN_SWAP_LEAD: u64 = 3;

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum Kind {
        Module,
        View,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ScheduledSwap {
        pub name: String,
        pub activation_height: u64,
        pub code_hash: Vec<u8>,
        pub readiness: Vec<Vec<u8>>,
        pub ready_at: Option<u64>,
    }

    impl ScheduledSwap {
        #[cfg(test)]
        pub fn armed_at(&self, height: u64) -> bool {
            self.ready_at.is_some_and(|latched| latched < height)
                && self.activation_height <= height
        }

        pub fn stale_at(&self, height: u64) -> bool {
            self.activation_height <= height && self.ready_at.is_none()
        }
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct Activation {
        pub height: u64,
        pub code_hash: Vec<u8>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ModuleCode {
        pub module_id: String,
        pub kind: Kind,
        pub active_code_hash: Vec<u8>,
        pub pending: Option<ScheduledSwap>,
        pub history: Vec<Activation>,
    }

    #[cfg(test)]
    pub fn code_at(entry: &ModuleCode, height: u64) -> Option<&[u8]> {
        if let Some(pending) = entry.pending.as_ref().filter(|p| p.armed_at(height)) {
            return Some(&pending.code_hash);
        }
        let sealed = entry.history.iter().rev().find(|a| a.height <= height);
        sealed
            .or_else(|| entry.history.first())
            .map(|a| a.code_hash.as_slice())
    }

    pub use module_artifact::{LaneDecl, LaneStream};

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct LaneRecord {
        pub id: u8,
        pub module_id: String,
        pub name: String,
        pub stream: Option<LaneStream>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ModulesMsg {
        RegisterModule {
            module_id: String,
            kind: Kind,
            code_hash: Vec<u8>,
            lanes: Vec<LaneDecl>,
        },
        ScheduleSwap {
            name: String,
            module_id: String,
            activation_height: u64,
            code_hash: Vec<u8>,
        },
        ScheduleRegister {
            name: String,
            module_id: String,
            kind: Kind,
            activation_height: u64,
            code_hash: Vec<u8>,
            lanes: Vec<LaneDecl>,
        },
        CancelSwap {
            name: String,
            module_id: String,
        },
        SwapReady {
            name: String,
            module_id: String,
            code_hash: Vec<u8>,
        },
        Advance,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ModulesQuery {
        ModuleStatus,
        ArmedAt { height: u64 },
        Lanes,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ModulesReply {
        ModuleStatus { modules: Vec<ModuleCode> },
        ArmedAt { swaps: Vec<ArmedSwap> },
        Lanes { lanes: Vec<LaneRecord> },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ArmedSwap {
        pub module_id: String,
        pub code_hash: Vec<u8>,
    }

    pub fn encode_msg(value: &ModulesMsg) -> Vec<u8> {
        sdk::wire::encode(value)
    }

    pub fn encode_query(value: &ModulesQuery) -> Vec<u8> {
        sdk::wire::encode(value)
    }

    pub fn decode_reply(bytes: &[u8]) -> Result<ModulesReply, String> {
        sdk::wire::decode(bytes)
    }
}

pub mod valset {
    use super::*;

    #[cfg(test)]
    pub const RETAINED_GENERATIONS: u64 = 4;

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ValsetQuery {
        Validators,
        Residents,
        MeshWindow,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct GenerationSet {
        pub generation: u64,
        pub validators: Vec<Vec<u8>>,
        pub residents: Vec<Vec<u8>>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ValsetReply {
        Validators(Vec<Vec<u8>>),
        Residents(Vec<Vec<u8>>),
        MeshWindow(Vec<GenerationSet>),
    }

    pub fn encode_query(value: &ValsetQuery) -> Vec<u8> {
        sdk::wire::encode(value)
    }

    #[cfg(test)]
    pub fn decode_query(bytes: &[u8]) -> Result<ValsetQuery, String> {
        sdk::wire::decode(bytes)
    }

    #[cfg(test)]
    pub fn encode_reply(value: &ValsetReply) -> Vec<u8> {
        sdk::wire::encode(value)
    }

    pub fn decode_reply(bytes: &[u8]) -> Result<ValsetReply, String> {
        sdk::wire::decode(bytes)
    }
}

pub mod governance {
    use super::*;

    pub type Kind = modules::Kind;

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum GovAction {
        AddValidator {
            key: Vec<u8>,
        },
        RemoveValidator {
            key: Vec<u8>,
        },
        Signal {
            text: String,
        },
        AddResident {
            key: Vec<u8>,
        },
        RemoveResident {
            key: Vec<u8>,
        },
        AdoptShares {
            allocations: Vec<ShareAllocation>,
        },
        SetShares {
            account_id: u64,
            shares: u64,
        },
        SetShareMode {
            enabled: bool,
        },
        UpdateModule {
            name: String,
            module_id: String,
            activation_lead: u64,
            code_hash: Vec<u8>,
        },
        RegisterModule {
            name: String,
            module_id: String,
            kind: Kind,
            activation_lead: u64,
            code_hash: Vec<u8>,
            #[serde(default)]
            lanes: Vec<modules::LaneDecl>,
        },
        CancelModuleUpdate {
            name: String,
            module_id: String,
        },
        SetAclPolicy {
            target: String,
            standing: Option<Standing>,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ShareAllocation {
        pub account_id: u64,
        pub shares: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Standing {
        Validator,
        Node,
        User,
        Open,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum VoterKind {
        ValidatorNode,
        Account,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum VotingRule {
        Threshold { required_yes: u64 },
        ParticipatingMajority { quorum: u64 },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum GovMsg {
        Propose {
            proposal_id: String,
            action: GovAction,
            voting_period: u64,
        },
        Vote {
            proposal_id: String,
            approve: bool,
        },
        Execute {
            proposal_id: String,
        },
        Redeem {
            issuer: Vec<u8>,
            nonce: Vec<u8>,
            token_sig: Vec<u8>,
            joiner: Vec<u8>,
            proof: Vec<u8>,
            expires_unix_secs: u64,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ProposalStatus {
        Open,
        Passed,
        Rejected,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ProposalView {
        pub proposal_id: String,
        pub action: GovAction,
        pub proposer: Vec<u8>,
        pub created_at: u64,
        pub deadline: u64,
        pub status: ProposalStatus,
        pub votes: Vec<(Vec<u8>, bool)>,
        pub voter_kind: VoterKind,
        pub electorate: Vec<(Vec<u8>, u64)>,
        pub voting_rule: VotingRule,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct SharesView {
        pub active: bool,
        pub allocations: Vec<ShareAllocation>,
        pub total: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct RedemptionView {
        pub nonce: Vec<u8>,
        pub joiner: Vec<u8>,
        pub issuer: Vec<u8>,
        pub height: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum GovQuery {
        Proposals,
        Proposal { proposal_id: String },
        Redemption { nonce: Vec<u8> },
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

    pub fn encode_msg(value: &GovMsg) -> Vec<u8> {
        sdk::wire::encode(value)
    }

    pub fn encode_query(value: &GovQuery) -> Vec<u8> {
        sdk::wire::encode(value)
    }

    pub fn decode_reply(bytes: &[u8]) -> Result<GovReply, String> {
        sdk::wire::decode(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::{governance, modules, valset};

    #[test]
    fn consumer_contracts_keep_canonical_tags() {
        assert_eq!(
            modules::encode_query(&modules::ModulesQuery::ModuleStatus),
            br#""module_status""#
        );
        assert_eq!(
            valset::encode_query(&valset::ValsetQuery::Validators),
            br#""validators""#
        );
        assert_eq!(
            governance::encode_query(&governance::GovQuery::Proposals),
            br#""proposals""#
        );
        assert_eq!(
            governance::encode_msg(&governance::GovMsg::Vote {
                proposal_id: "p".into(),
                approve: true,
            }),
            br#"{"vote":{"proposal_id":"p","approve":true}}"#
        );
    }
}
