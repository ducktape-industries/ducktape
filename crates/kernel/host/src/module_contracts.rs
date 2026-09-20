use serde::{Deserialize, Serialize};

pub mod acl {
    use super::*;

    pub const DEFAULT_ACL_ID: &str = "acl";

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Standing {
        Validator,
        Node,
        User,
        Open,
    }

    impl Standing {
        pub fn as_str(self) -> &'static str {
            match self {
                Self::Validator => "validator",
                Self::Node => "node",
                Self::User => "user",
                Self::Open => "open",
            }
        }
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum AclQuery {
        PolicyFor { target: String },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum AclReply {
        Policy(Vec<(String, Standing)>),
        PolicyFor(Option<Standing>),
    }

    pub fn encode_query(value: &AclQuery) -> Vec<u8> {
        sdk::wire::encode(value)
    }

    pub fn decode_reply(bytes: &[u8]) -> Result<AclReply, String> {
        sdk::wire::decode(bytes)
    }
}

pub mod valset {
    use super::*;

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ValsetQuery {
        Validators,
        Residents,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ValsetReply {
        Validators(Vec<Vec<u8>>),
        Residents(Vec<Vec<u8>>),
    }

    pub fn encode_query(value: &ValsetQuery) -> Vec<u8> {
        sdk::wire::encode(value)
    }

    pub fn decode_reply(bytes: &[u8]) -> Result<ValsetReply, String> {
        sdk::wire::decode(bytes)
    }
}

pub mod modules {
    use super::*;

    pub const DEFAULT_MODULES_ID: &str = "modules";

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Kind {
        Module,
        View,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
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
            self.ready_at.is_some_and(|latched| latched < height)
                && self.activation_height <= height
        }
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
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
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ModulesMsg {
        Advance,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ModulesQuery {
        ModuleStatus,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ModulesReply {
        ModuleStatus { modules: Vec<ModuleCode> },
    }

    pub fn encode_msg(value: &ModulesMsg) -> Vec<u8> {
        sdk::wire::encode(value)
    }

    pub fn encode_query(value: &ModulesQuery) -> Vec<u8> {
        sdk::wire::encode(value)
    }

    pub fn decode_reply(bytes: &[u8]) -> Result<ModulesReply, String> {
        sdk::wire::decode(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::{acl, modules, valset};

    #[test]
    fn consumed_contracts_keep_canonical_wire_bytes() {
        assert_eq!(
            acl::encode_query(&acl::AclQuery::PolicyFor {
                target: "governance".into(),
            }),
            br#"{"policy_for":{"target":"governance"}}"#
        );
        assert_eq!(
            valset::encode_query(&valset::ValsetQuery::Residents),
            br#""residents""#
        );
        assert_eq!(
            modules::encode_query(&modules::ModulesQuery::ModuleStatus),
            br#""module_status""#
        );
        assert_eq!(
            modules::encode_msg(&modules::ModulesMsg::Advance),
            br#""advance""#
        );
    }
}
