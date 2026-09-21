use abi::{Cause, ProgramId};
use borsh::{BorshDeserialize, BorshSerialize};

use crate::{AccountNumber, Page};

pub const PROGRAM: &str = "attribution";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, BorshSerialize, BorshDeserialize)]
pub struct Object {
    pub kind: String,
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, BorshSerialize, BorshDeserialize)]
pub struct Source {
    pub program: ProgramId,
    pub object: Object,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Actor {
    Account(AccountNumber),
    Key(Vec<u8>),
    Program(ProgramId),
    System,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, BorshSerialize, BorshDeserialize)]
pub enum Reason {
    Mention,
    Authorship,
    Ownership,
    Assignment,
    Credit,
    Result,
    Report,
    Defined(String),
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Relation {
    pub recipient: AccountNumber,
    pub reason: Reason,
    pub detail: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Transfer {
    pub reason: Reason,
    pub from: AccountNumber,
    pub to: AccountNumber,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Update {
    pub object: Object,
    pub revision: u64,
    pub actor: Actor,
    pub relations: Vec<Relation>,
    pub transfers: Vec<Transfer>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Kind {
    Added,
    Withdrawn,
    TransferredIn { from: AccountNumber },
    TransferredOut { to: AccountNumber },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Change {
    pub seq: u64,
    pub source: Source,
    pub revision: u64,
    pub recipient: AccountNumber,
    pub reason: Reason,
    pub kind: Kind,
    pub detail: Vec<u8>,
    pub actor: Actor,
    pub cause: Cause,
    pub height: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Relations {
    pub source: Source,
    pub revision: u64,
    pub relations: Vec<Relation>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Op {
    Attribute(Update),
    AttributeBatch { updates: Vec<Update> },
    Subscribe,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Query {
    Relations {
        source: Source,
    },
    Changes {
        page: Page,
    },
    ChangesTo {
        recipient: AccountNumber,
        page: Page,
    },
    ChangesOf {
        source: Source,
        page: Page,
    },
    Subscribers,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Reply {
    Relations(Option<Relations>),
    Changes(Vec<Change>),
    Subscribers(Vec<ProgramId>),
}
