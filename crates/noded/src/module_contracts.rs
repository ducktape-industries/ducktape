use serde::{Deserialize, Serialize};

pub mod modules {
    use super::*;

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Kind {
        Module,
        View,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct Seed {
        pub kind: Kind,
        pub code_hash: Vec<u8>,
        #[serde(default)]
        pub lanes: Vec<module_artifact::LaneDecl>,
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
        pub fn armed_at(&self, height: u64) -> bool {
            self.ready_at.is_some_and(|latched| latched < height)
                && self.activation_height <= height
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

    pub fn code_at(entry: &ModuleCode, height: u64) -> Option<&[u8]> {
        if let Some(pending) = entry
            .pending
            .as_ref()
            .filter(|pending| pending.armed_at(height))
        {
            return Some(&pending.code_hash);
        }
        let sealed = entry
            .history
            .iter()
            .rev()
            .find(|activation| activation.height <= height);
        sealed
            .or_else(|| entry.history.first())
            .map(|activation| activation.code_hash.as_slice())
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ModulesMsg {
        RegisterModule {
            module_id: String,
            kind: Kind,
            code_hash: Vec<u8>,
            lanes: Vec<module_artifact::LaneDecl>,
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
            lanes: Vec<module_artifact::LaneDecl>,
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

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct LaneRecord {
        pub id: u8,
        pub module_id: String,
        pub name: String,
        pub stream: Option<module_artifact::LaneStream>,
    }

    pub use module_artifact::{LaneDecl, LanePacing, LaneStream};

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

pub mod governance {
    use super::*;

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

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ProposalStatus {
        Open,
        Passed,
        Rejected,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum GovAction {
        Signal { text: String },
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
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum GovQuery {
        Proposal { proposal_id: String },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum GovReply {
        Proposal(Option<ProposalView>),
    }

    pub fn encode_query(value: &GovQuery) -> Vec<u8> {
        sdk::wire::encode(value)
    }

    pub fn decode_reply(bytes: &[u8]) -> Result<GovReply, String> {
        sdk::wire::decode(bytes)
    }
}
