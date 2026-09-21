use std::collections::BTreeMap;

use abi::ProgramId;
use borsh::{BorshDeserialize, BorshSerialize};

use crate::Page;

pub const PROGRAM: &str = "capability";

#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Announcement {
    pub tags: Vec<String>,
    pub resources: BTreeMap<String, u64>,
}

impl Announcement {
    pub fn covers(&self, demands: &BTreeMap<String, u64>) -> bool {
        demands
            .iter()
            .all(|(dimension, amount)| self.resources.get(dimension) >= Some(amount))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Provider {
    pub node: Vec<u8>,
    pub announcement: Announcement,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Claim {
    pub class: String,
    pub program: ProgramId,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Op {
    Announce(Announcement),
    ClaimClass { class: String },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Query {
    Providers {
        tag: String,
        demands: BTreeMap<String, u64>,
    },
    Node {
        node: Vec<u8>,
    },
    All {
        page: Page,
    },
    Class {
        class: String,
    },
    Classes,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Reply {
    Providers(Vec<Vec<u8>>),
    Node(Option<Announcement>),
    All(Vec<Provider>),
    Class(Option<ProgramId>),
    Classes(Vec<Claim>),
}

pub fn tag_is_well_formed(tag: &str) -> bool {
    let charset = tag
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b));
    !tag.is_empty() && charset
}

pub fn class_is_well_formed(class: &str) -> bool {
    let charset = class
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    !class.is_empty() && charset
}

pub fn class_of(tag: &str) -> Option<&str> {
    let (class, rest) = tag.split_once(':')?;
    (class_is_well_formed(class) && !rest.is_empty()).then_some(class)
}

#[cfg(target_arch = "wasm32")]
pub fn providers(tag: &str, demands: &BTreeMap<String, u64>) -> Result<Vec<Vec<u8>>, abi::Refusal> {
    let query = Query::Providers {
        tag: tag.to_owned(),
        demands: demands.clone(),
    };
    match guest::ask::<Query, Reply>(PROGRAM, &query)? {
        Reply::Providers(providers) => Ok(providers),
        other => Err(abi::Refusal::new(
            abi::reason::PROTOCOL,
            format!("capability answered Providers with {other:?}"),
        )),
    }
}
