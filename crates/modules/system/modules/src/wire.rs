use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

pub const MIN_SWAP_LEAD: u64 = 3;
pub const DEFAULT_MODULES_ID: &str = "modules";
pub const CODE_HASH_LEN: usize = 32;

#[derive(
    BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Module,
    View,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Seed {
    pub kind: Kind,
    pub code_hash: Vec<u8>,
    #[serde(default)]
    pub lanes: Vec<LaneDecl>,
}

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScheduledSwap {
    pub name: String,
    pub activation_height: u64,
    pub code_hash: Vec<u8>,
    pub readiness: Vec<Vec<u8>>,
    pub ready_at: Option<u64>,
}

impl ScheduledSwap {
    pub fn armed_at(&self, height: u64) -> bool {
        let latched_before = self.ready_at.is_some_and(|latched| latched < height);
        let floor_reached = self.activation_height <= height;
        latched_before && floor_reached
    }

    pub fn stale_at(&self, height: u64) -> bool {
        self.activation_height <= height && self.ready_at.is_none()
    }
}

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Activation {
    pub height: u64,
    pub code_hash: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModuleCode {
    pub module_id: String,
    pub kind: Kind,
    pub active_code_hash: Vec<u8>,
    pub pending: Option<ScheduledSwap>,
    pub history: Vec<Activation>,
}

pub fn code_at(entry: &ModuleCode, height: u64) -> Option<&[u8]> {
    if let Some(pending) = entry
        .pending
        .as_ref()
        .filter(|pending| pending.armed_at(height))
    {
        return Some(&pending.code_hash);
    }
    let sealed = entry
        .history
        .iter()
        .rev()
        .find(|activation| activation.height <= height);
    sealed
        .or_else(|| entry.history.first())
        .map(|activation| activation.code_hash.as_slice())
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArmedSwap {
    pub module_id: String,
    pub code_hash: Vec<u8>,
}

pub const RESERVED_LANE_IDS: &[u8] = &[1, 6];
pub const MAX_LANE_ID: u8 = 99;
pub use module_artifact::{
    LaneDecl, LanePacing, LaneStream, MAX_LANE_NAME_BYTES, lane_name_is_well_formed,
};

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LaneRecord {
    pub id: u8,
    pub module_id: String,
    pub name: String,
    pub stream: Option<LaneStream>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ModulesMsg {
    RegisterModule {
        module_id: String,
        kind: Kind,
        code_hash: Vec<u8>,
        lanes: Vec<LaneDecl>,
    },
    ScheduleSwap {
        name: String,
        module_id: String,
        activation_height: u64,
        code_hash: Vec<u8>,
    },
    ScheduleRegister {
        name: String,
        module_id: String,
        kind: Kind,
        activation_height: u64,
        code_hash: Vec<u8>,
        lanes: Vec<LaneDecl>,
    },
    CancelSwap {
        name: String,
        module_id: String,
    },
    SwapReady {
        name: String,
        module_id: String,
        code_hash: Vec<u8>,
    },
    Advance,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ModulesQuery {
    ModuleStatus,
    ArmedAt { height: u64 },
    Lanes,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ModulesReply {
    ModuleStatus { modules: Vec<ModuleCode> },
    ArmedAt { swaps: Vec<ArmedSwap> },
    Lanes { lanes: Vec<LaneRecord> },
}

pub fn encode_msg(value: &ModulesMsg) -> Vec<u8> {
    sdk::wire::encode(value)
}

pub fn decode_msg(bytes: &[u8]) -> Result<ModulesMsg, String> {
    sdk::wire::decode(bytes)
}

pub fn encode_query(value: &ModulesQuery) -> Vec<u8> {
    sdk::wire::encode(value)
}

pub fn decode_query(bytes: &[u8]) -> Result<ModulesQuery, String> {
    sdk::wire::decode(bytes)
}

pub fn encode_reply(value: &ModulesReply) -> Vec<u8> {
    sdk::wire::encode(value)
}

pub fn decode_reply(bytes: &[u8]) -> Result<ModulesReply, String> {
    sdk::wire::decode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt_msg(m: ModulesMsg) {
        assert_eq!(decode_msg(&encode_msg(&m)).unwrap(), m);
    }

    #[test]
    fn golden_wire_shapes() {
        assert_eq!(sdk::wire::encode(&Kind::Module), br#""module""#);
        assert_eq!(
            encode_query(&ModulesQuery::ArmedAt { height: 7 }),
            br#"{"armed_at":{"height":7}}"#
        );
        assert_eq!(encode_msg(&ModulesMsg::Advance), br#""advance""#);
        let seed = Seed {
            kind: Kind::View,
            code_hash: vec![1, 2],
            lanes: Vec::new(),
        };
        assert_eq!(
            sdk::wire::encode(&seed),
            br#"{"kind":"view","code_hash":[1,2],"lanes":[]}"#
        );
    }

    #[test]
    fn msg_query_reply_round_trip_every_variant() {
        rt_msg(ModulesMsg::RegisterModule {
            module_id: "hello".into(),
            kind: Kind::Module,
            code_hash: vec![1u8; CODE_HASH_LEN],
            lanes: Vec::new(),
        });
        rt_msg(ModulesMsg::ScheduleSwap {
            name: "swap-hello".into(),
            module_id: "hello".into(),
            activation_height: 10,
            code_hash: vec![2u8; CODE_HASH_LEN],
        });
        rt_msg(ModulesMsg::ScheduleRegister {
            name: "admit-kanban".into(),
            module_id: "kanban".into(),
            kind: Kind::Module,
            activation_height: 10,
            code_hash: vec![5u8; CODE_HASH_LEN],
            lanes: vec![LaneDecl {
                id: 7,
                name: "board_sync".into(),
                stream: Some(LaneStream {
                    pacing: LanePacing::Shared,
                    accept_backlog: 32,
                }),
            }],
        });
        rt_msg(ModulesMsg::ScheduleRegister {
            name: "admit-home".into(),
            module_id: "home".into(),
            kind: Kind::View,
            activation_height: 10,
            code_hash: vec![6u8; CODE_HASH_LEN],
            lanes: Vec::new(),
        });
        rt_msg(ModulesMsg::CancelSwap {
            name: "swap-hello".into(),
            module_id: "hello".into(),
        });
        rt_msg(ModulesMsg::SwapReady {
            name: "swap-hello".into(),
            module_id: "hello".into(),
            code_hash: vec![2u8; CODE_HASH_LEN],
        });
        rt_msg(ModulesMsg::Advance);

        for q in [
            ModulesQuery::ModuleStatus,
            ModulesQuery::ArmedAt { height: 9 },
            ModulesQuery::Lanes,
        ] {
            assert_eq!(decode_query(&encode_query(&q)).unwrap(), q);
        }

        let lanes = ModulesReply::Lanes {
            lanes: vec![
                LaneRecord {
                    id: 2,
                    module_id: "chat".into(),
                    name: "voice".into(),
                    stream: None,
                },
                LaneRecord {
                    id: 5,
                    module_id: "agent".into(),
                    name: "telemetry".into(),
                    stream: Some(LaneStream {
                        pacing: LanePacing::Local {
                            bulk_bytes_per_sec: 24 * 1024 * 1024,
                            bulk_burst_bytes: 512 * 1024,
                        },
                        accept_backlog: 16,
                    }),
                },
            ],
        };
        assert_eq!(decode_reply(&encode_reply(&lanes)).unwrap(), lanes);

        let reply = ModulesReply::ModuleStatus {
            modules: vec![ModuleCode {
                module_id: "hello".into(),
                kind: Kind::Module,
                active_code_hash: vec![1u8; CODE_HASH_LEN],
                pending: None,
                history: vec![Activation {
                    height: 0,
                    code_hash: vec![1u8; CODE_HASH_LEN],
                }],
            }],
        };
        assert_eq!(decode_reply(&encode_reply(&reply)).unwrap(), reply);
    }

    #[test]
    fn kind_is_snake_case_on_the_wire() {
        assert_eq!(sdk::wire::encode(&Kind::Module), br#""module""#);
        assert_eq!(sdk::wire::encode(&Kind::View), br#""view""#);
        assert_eq!(borsh::to_vec(&Kind::Module).unwrap(), [0]);
        assert_eq!(borsh::to_vec(&Kind::View).unwrap(), [1]);
        let seed = Seed {
            kind: Kind::View,
            code_hash: vec![7u8; CODE_HASH_LEN],
            lanes: Vec::new(),
        };
        assert_eq!(
            sdk::wire::decode::<Seed>(&sdk::wire::encode(&seed)).unwrap(),
            seed
        );
        let bare: Seed = sdk::wire::decode(br#"{"kind":"view","code_hash":[]}"#).unwrap();
        assert!(bare.lanes.is_empty());
    }

    #[test]
    fn a_lane_name_is_a_bounded_snake_case_token() {
        for good in ["voice", "video", "gateway", "telemetry", "run_output2"] {
            assert!(lane_name_is_well_formed(good), "{good}");
        }
        for bad in ["", "Voice", "voice-lane", "voice lane", "보이스", "v.1"] {
            assert!(!lane_name_is_well_formed(bad), "{bad:?}");
        }
        assert!(lane_name_is_well_formed(&"a".repeat(MAX_LANE_NAME_BYTES)));
        assert!(!lane_name_is_well_formed(
            &"a".repeat(MAX_LANE_NAME_BYTES + 1)
        ));
    }
}
