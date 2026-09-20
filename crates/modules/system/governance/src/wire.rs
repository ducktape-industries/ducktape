use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

pub mod invite;

pub const MIN_ACTIVATION_LEAD: u64 = 4;
pub const MAX_ACTIVATION_LEAD: u64 = 1_000_000_000;

#[derive(
    BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Kind {
    Module,
    View,
}

#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Standing {
    Validator,
    Node,
    User,
    Open,
}

#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
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
        lanes: Vec<module_artifact::LaneDecl>,
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

#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ShareAllocation {
    pub account_id: u64,
    pub shares: u64,
}

#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum VoterKind {
    ValidatorNode,
    Account,
}

#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
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

#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
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

pub fn decode_msg(bytes: &[u8]) -> Result<GovMsg, String> {
    sdk::wire::decode(bytes)
}

pub fn encode_query(value: &GovQuery) -> Vec<u8> {
    sdk::wire::encode(value)
}

pub fn decode_query(bytes: &[u8]) -> Result<GovQuery, String> {
    sdk::wire::decode(bytes)
}

pub fn encode_reply(value: &GovReply) -> Vec<u8> {
    sdk::wire::encode(value)
}

pub fn decode_reply(bytes: &[u8]) -> Result<GovReply, String> {
    sdk::wire::decode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_wire_shapes() {
        assert_eq!(
            encode_msg(&GovMsg::Vote {
                proposal_id: "p".into(),
                approve: true,
            }),
            br#"{"vote":{"proposal_id":"p","approve":true}}"#
        );
        assert_eq!(encode_query(&GovQuery::Proposals), br#""proposals""#);
        assert_eq!(
            encode_reply(&GovReply::Redemption(None)),
            br#"{"redemption":null}"#
        );
    }
}
