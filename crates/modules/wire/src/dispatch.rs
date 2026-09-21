use std::collections::BTreeMap;

use abi::{Origin, ProgramId};
use borsh::{BorshDeserialize, BorshSerialize};

use crate::Page;

pub const PROGRAM: &str = "dispatch";

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Routing {
    Capability,
    Pinned(Vec<u8>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Contract {
    Bytes,
    Json,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Manifest {
    pub description: String,
    pub capability: String,
    pub routing: Routing,
    pub contract: Contract,
    pub attempts: u32,
    pub deadline: Option<u64>,
    pub lease: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Recipe {
    pub id: String,
    pub owner: Origin,
    pub manifest: Manifest,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Admission {
    #[default]
    Queue,
    FailFast,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Spec {
    pub receiver: ProgramId,
    pub id: String,
    pub capability: String,
    pub payload: Vec<u8>,
    pub demands: BTreeMap<String, u64>,
    pub admission: Admission,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Status {
    Running { saga: String },
    Delivered { outcome: Result<Vec<u8>, String> },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Dispatch {
    pub id: String,
    pub recipe: String,
    pub receiver: ProgramId,
    pub status: Status,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Outcome {
    pub id: String,
    pub recipe: String,
    pub outcome: Result<Vec<u8>, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Op {
    SetRecipe {
        id: String,
        manifest: Manifest,
    },
    RemoveRecipe {
        id: String,
    },
    Dispatch {
        id: String,
        recipe: String,
        payload: Vec<u8>,
        demands: BTreeMap<String, u64>,
        admission: Admission,
    },
    Cancel {
        id: String,
    },
    Reassign {
        id: String,
        attempt: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Query {
    Recipe { id: String },
    Recipes { page: Page },
    Dispatch { receiver: ProgramId, id: String },
    Dispatches { receiver: ProgramId, page: Page },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Reply {
    Recipe(Option<Recipe>),
    Recipes(Vec<Recipe>),
    Dispatch(Option<Dispatch>),
    Dispatches(Vec<Dispatch>),
}

pub fn saga_id(receiver: &str, id: &str) -> String {
    crate::saga::id_for(
        &Origin::Program(PROGRAM.into()),
        &format!("{receiver}{}{id}", crate::saga::SEPARATOR),
    )
}
