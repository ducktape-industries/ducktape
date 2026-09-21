use abi::{ItemRef, ProgramId, Refusal};
use borsh::{BorshDeserialize, BorshSerialize};

use crate::{AccountNumber, Page, acl, roster, valset};

pub const PROGRAM: &str = "governance";
pub const INVITE_NAMESPACE: &[u8] = b"ducktape:governance:invite";

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Allocation {
    pub account: AccountNumber,
    pub shares: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Action {
    SetMembership(valset::Membership),
    RemoveMember {
        key: Vec<u8>,
    },
    Signal {
        text: String,
    },
    AdoptShares {
        allocations: Vec<Allocation>,
    },
    SetShares {
        account: AccountNumber,
        shares: u64,
    },
    SetShareMode {
        enabled: bool,
    },
    ScheduleProgram {
        lead: u64,
        change: roster::Change,
    },
    CancelProgram {
        height: u64,
        program: ProgramId,
    },
    SetPolicy {
        target: String,
        standing: Option<acl::Standing>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Electorate {
    Validators,
    Shareholders,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Rule {
    Threshold { required_yes: u64 },
    ParticipatingMajority { quorum: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Voter {
    pub principal: Vec<u8>,
    pub power: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Ballot {
    pub principal: Vec<u8>,
    pub approve: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Effect {
    Pending(ItemRef),
    Applied,
    Refused(Refusal),
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Status {
    Open,
    Passed { effect: Option<Effect> },
    Rejected,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Proposal {
    pub id: String,
    pub action: Action,
    pub proposer: Vec<u8>,
    pub opened_at: u64,
    pub deadline: u64,
    pub electorate: Electorate,
    pub voters: Vec<Voter>,
    pub rule: Rule,
    pub ballots: Vec<Ballot>,
    pub status: Status,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Shares {
    pub enabled: bool,
    pub allocations: Vec<Allocation>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Invite {
    pub issuer: Vec<u8>,
    pub nonce: Vec<u8>,
    pub expires_at: u64,
    pub signature: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Grant {
    pub network: Vec<u8>,
    pub nonce: Vec<u8>,
    pub expires_at: u64,
}

impl Grant {
    pub fn preimage(&self) -> Vec<u8> {
        abi::encode(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Redemption {
    pub nonce: Vec<u8>,
    pub issuer: Vec<u8>,
    pub joiner: Vec<u8>,
    pub height: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Op {
    Propose {
        id: String,
        action: Action,
        voting_blocks: u64,
    },
    Vote {
        id: String,
        approve: bool,
    },
    Execute {
        id: String,
    },
    Redeem {
        invite: Invite,
        address: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Query {
    Proposal { id: String },
    Proposals { page: Page },
    Shares,
    Redemption { nonce: Vec<u8> },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Reply {
    Proposal(Option<Proposal>),
    Proposals(Vec<Proposal>),
    Shares(Shares),
    Redemption(Option<Redemption>),
}
