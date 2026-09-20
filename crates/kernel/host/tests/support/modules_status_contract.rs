use serde::{Deserialize, Serialize};

// Golden bytes were produced by modules-wire from ducktape-sdk
// 736865710dcfa7c56f9834747287881c1c25d45d. Keep this contract local to the
// host fixtures: the native `modules` dependency remains only the producer
// implementation used to compare against the committed guest.
pub const STATUS_QUERY_GOLDEN: &[u8] = br#""module_status""#;
pub const STATUS_REPLY_GOLDEN: &[u8] = br#"{"module_status":{"modules":[{"module_id":"hello","kind":"module","active_code_hash":[1,2,3],"pending":null,"history":[]}]}}"#;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Query {
    ModuleStatus,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    ModuleStatus { modules: Vec<ModuleCode> },
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Kind {
    Module,
    View,
    Plane,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModuleCode {
    pub module_id: String,
    pub kind: Kind,
    pub active_code_hash: Vec<u8>,
    pub pending: Option<ScheduledSwap>,
    pub history: Vec<Activation>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScheduledSwap {
    pub name: String,
    pub activation_height: u64,
    pub code_hash: Vec<u8>,
    pub readiness: Vec<Vec<u8>>,
    pub ready_at: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Activation {
    pub height: u64,
    pub code_hash: Vec<u8>,
}

pub fn status_query() -> Vec<u8> {
    sdk::wire::encode(&Query::ModuleStatus)
}

pub fn decode_status(bytes: &[u8]) -> Result<Vec<ModuleCode>, String> {
    match sdk::wire::decode::<Reply>(bytes)? {
        Reply::ModuleStatus { modules } => Ok(modules),
    }
}

#[test]
fn status_contract_matches_golden_and_refuses_unknown_shapes() {
    assert_eq!(status_query(), STATUS_QUERY_GOLDEN);
    let reply = Reply::ModuleStatus {
        modules: vec![ModuleCode {
            module_id: "hello".into(),
            kind: Kind::Module,
            active_code_hash: vec![1, 2, 3],
            pending: None,
            history: Vec::new(),
        }],
    };
    assert_eq!(sdk::wire::encode(&reply), STATUS_REPLY_GOLDEN);
    assert_eq!(decode_status(STATUS_REPLY_GOLDEN).unwrap().len(), 1);
    assert!(sdk::wire::decode::<Query>(br#""lanes""#).is_err());
    assert!(decode_status(
        br#"{"module_status":{"modules":[{"module_id":"hello","kind":"module","active_code_hash":[],"pending":null,"history":[],"extra":true}]}}"#
    )
    .is_err());
}
