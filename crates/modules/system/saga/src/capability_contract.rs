use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use sdk::{ModuleId, wire};

pub const MAX_RESOURCE_DIMS: usize = 16;

pub fn validate_resources(resources: &BTreeMap<String, u64>) -> Result<(), String> {
    if resources.len() > MAX_RESOURCE_DIMS {
        return Err(format!(
            "too many resource dimensions: {} exceeds the {MAX_RESOURCE_DIMS} cap",
            resources.len()
        ));
    }
    for (key, value) in resources {
        validate_tag(key).map_err(|e| format!("resource dimension {key:?}: {e}"))?;
        if *value == 0 {
            return Err(format!(
                "resource dimension {key:?} is zero (omit it instead)"
            ));
        }
    }
    Ok(())
}

fn validate_tag(tag: &str) -> Result<(), String> {
    if tag.is_empty() {
        return Err("capability tag must be non-empty".into());
    }
    if tag.len() > 64 {
        return Err(format!(
            "capability tag exceeds 64 bytes: {} bytes",
            tag.len()
        ));
    }
    if !tag
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
    {
        return Err(format!(
            "capability tag has invalid characters (want [a-z0-9._-]): {tag:?}"
        ));
    }
    Ok(())
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CapabilityQuery {
    Providers {
        capability: String,
    },
    CapableProviders {
        capability: String,
        demands: BTreeMap<String, u64>,
    },
    Node {
        node: Vec<u8>,
    },
    Resources {
        node: Vec<u8>,
    },
    All,
    ResolveClass {
        class: String,
    },
    Classes,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CapabilityReply {
    Providers(Vec<Vec<u8>>),
    Node(Vec<String>),
    Resources(BTreeMap<String, u64>),
    All(Vec<(Vec<u8>, Vec<String>)>),
    ClassOwner(Option<ModuleId>),
    Classes(Vec<(String, ModuleId)>),
}

pub fn encode_query(query: &CapabilityQuery) -> Vec<u8> {
    wire::encode(query)
}

#[cfg(test)]
pub fn decode_query(bytes: &[u8]) -> Result<CapabilityQuery, String> {
    wire::decode(bytes)
}

#[cfg(test)]
pub fn encode_reply(reply: &CapabilityReply) -> Vec<u8> {
    wire::encode(reply)
}

pub fn decode_reply(bytes: &[u8]) -> Result<CapabilityReply, String> {
    wire::decode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owning_capability_query_encoder_matches_fixture() {
        assert_eq!(
            encode_query(&CapabilityQuery::Providers {
                capability: "codex".into(),
            }),
            include_bytes!("../tests/fixtures/capability-providers.json")
                .strip_suffix(b"\n")
                .unwrap()
        );
    }
}
