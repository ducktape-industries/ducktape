use std::collections::BTreeMap;

use abi::{Origin, ProgramId};
use borsh::{BorshDeserialize, BorshSerialize};

use crate::Page;

pub const PROGRAM: &str = "saga";
pub const SEPARATOR: char = '/';

pub fn actor(origin: &Origin) -> String {
    match origin {
        Origin::External(key) => abi::hex(key),
        Origin::Program(program) => program.clone(),
        Origin::System => "system".into(),
    }
}

pub fn id_for(origin: &Origin, local: &str) -> String {
    format!("{}{SEPARATOR}{local}", actor(origin))
}

pub fn owns(origin: &Origin, id: &str) -> bool {
    id.strip_prefix(&actor(origin))
        .is_some_and(|rest| rest.starts_with(SEPARATOR))
}

#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Trigger {
    pub id: String,
    pub spec: Vec<u8>,
    pub reply_to: Option<ProgramId>,
    pub correlation: Vec<u8>,
    pub deadline: Option<u64>,
    pub attempts: u32,
    pub lease: Option<u64>,
    pub capability: Option<String>,
    pub demands: BTreeMap<String, u64>,
    pub pinned: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Assignment {
    Unassigned,
    Leased {
        assignee: Vec<u8>,
        until: Option<u64>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Outcome {
    Done(Vec<u8>),
    Failed(String),
    TimedOut,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Status {
    Pending,
    Settled(Outcome),
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Saga {
    pub trigger: Trigger,
    pub origin: Origin,
    pub opened_at: u64,
    pub attempt: u32,
    pub assignment: Assignment,
    pub status: Status,
    pub usage: Usage,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Work {
    pub id: String,
    pub attempt: u32,
    pub spec: Vec<u8>,
    pub deadline: Option<u64>,
    pub assignee: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Callback {
    pub id: String,
    pub correlation: Vec<u8>,
    pub outcome: Outcome,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Op {
    Trigger(Trigger),
    Result {
        id: String,
        attempt: u32,
        outcome: Result<Vec<u8>, String>,
        usage: Usage,
    },
    Renew {
        id: String,
        attempt: u32,
    },
    Reassign {
        id: String,
        attempt: u32,
    },
    Accept {
        id: String,
        attempt: u32,
    },
    Crank,
    Cancel {
        id: String,
    },
    Prune {
        ids: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Query {
    Get { id: String },
    NextExpiry,
    Assigned { node: Vec<u8>, page: Page },
    Pending { page: Page },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Reply {
    Saga(Option<Box<Saga>>),
    NextExpiry(Option<u64>),
    Work(Vec<Work>),
    Sagas(Vec<Saga>),
}
