use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use sdk::wire;

pub type SagaId = String;
pub const MAX_ASSIGNEE_BYTES: usize = 256;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SagaMsg {
    Trigger {
        saga_id: SagaId,
        spec: Vec<u8>,
        reply_to: Option<String>,
        reply_payload: Vec<u8>,
        deadline: Option<u64>,
        max_attempts: u32,
        lease_views: Option<u64>,
        capability: Option<String>,
        demands: BTreeMap<String, u64>,
        pinned_assignee: Option<Vec<u8>>,
    },
    Reassign {
        saga_id: SagaId,
        attempt: u32,
    },
    Cancel {
        saga_id: SagaId,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SagaOutcome {
    Done(Vec<u8>),
    Failed(String),
    TimedOut,
    Cancelled,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SagaCallback {
    pub saga_id: SagaId,
    pub payload: Vec<u8>,
    pub outcome: SagaOutcome,
}

pub fn encode_msg(message: &SagaMsg) -> Vec<u8> {
    wire::encode(message)
}

pub fn decode_msg(bytes: &[u8]) -> Result<SagaMsg, String> {
    wire::decode(bytes)
}

pub fn encode_callback(callback: &SagaCallback) -> Vec<u8> {
    wire::encode(callback)
}

pub fn decode_callback(bytes: &[u8]) -> Result<SagaCallback, String> {
    wire::decode(bytes)
}
