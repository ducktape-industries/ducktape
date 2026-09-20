use sdk::{AccountNumber, ModuleId};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObjectRef {
    pub kind: String,
    pub object: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Actor {
    Account(AccountNumber),
    Key(Vec<u8>),
    Module(ModuleId),
    System,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Reason {
    Mention,
    Authorship,
    Ownership,
    Assignment,
    Credit,
    Result,
    Report,
    Defined(String),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Relation {
    pub recipient: AccountNumber,
    pub reason: Reason,
    pub detail: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum AttributionMsg {
    Attribute {
        object: ObjectRef,
        revision: u64,
        actor: Actor,
        relations: Vec<Relation>,
        transfers: Vec<Transfer>,
    },
    AttributeBatch {
        updates: Vec<AttributionUpdate>,
    },
    Subscribe {},
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttributionUpdate {
    pub object: ObjectRef,
    pub revision: u64,
    pub actor: Actor,
    pub relations: Vec<Relation>,
    pub transfers: Vec<Transfer>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Transfer {
    pub reason: Reason,
    pub from: AccountNumber,
    pub to: AccountNumber,
}

pub(crate) fn encode_msg(message: &AttributionMsg) -> Vec<u8> {
    sdk::wire::encode(message)
}
