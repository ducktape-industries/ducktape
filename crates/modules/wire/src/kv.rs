use abi::Entry;
use borsh::{BorshDeserialize, BorshSerialize};

use crate::Page;

pub const PROGRAM: &str = "kv";

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Op {
    Set { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Query {
    Get { key: Vec<u8> },
    List { prefix: Vec<u8>, page: Page },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Reply {
    Value(Option<Vec<u8>>),
    Entries(Vec<Entry>),
}
