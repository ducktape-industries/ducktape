use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum KvMsg {
    Set { key: Vec<u8>, value: Vec<u8> },
}

pub fn encode(value: &KvMsg) -> Vec<u8> {
    sdk::wire::encode(value)
}

pub fn decode(bytes: &[u8]) -> Result<KvMsg, String> {
    sdk::wire::decode(bytes)
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum KvQuery {
    Get { key: Vec<u8> },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum KvReply {
    Value(Option<Vec<u8>>),
}

pub fn encode_query(value: &KvQuery) -> Vec<u8> {
    sdk::wire::encode(value)
}

pub fn decode_query(bytes: &[u8]) -> Result<KvQuery, String> {
    sdk::wire::decode(bytes)
}

pub fn encode_reply(value: &KvReply) -> Vec<u8> {
    sdk::wire::encode(value)
}

pub fn decode_reply(bytes: &[u8]) -> Result<KvReply, String> {
    sdk::wire::decode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_wire_shapes() {
        assert_eq!(
            encode(&KvMsg::Set {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            }),
            br#"{"set":{"key":[107],"value":[118]}}"#
        );
        assert_eq!(
            encode_query(&KvQuery::Get { key: b"k".to_vec() }),
            br#"{"get":{"key":[107]}}"#
        );
        assert_eq!(
            encode_reply(&KvReply::Value(Some(b"v".to_vec()))),
            br#"{"value":[118]}"#
        );
    }
}
