//! Minimal module-owned wire slices used by the MCP host.
//!
//! Shapes and fixtures are copied from ducktape-sdk
//! `736865710dcfa7c56f9834747287881c1c25d45d`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod forge {
    use super::*;

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ForgeQuery {
        ListRepos,
        ListItems { repo: String },
        GetItem { repo: String, number: u64 },
        PrDiff { repo: String, number: u64 },
    }
}

pub mod pages {
    use super::*;

    pub const MAX_PAGE_QUERY_LIMIT: u16 = 256;

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum PageQuery {
        GetPage {
            page_id: String,
            after: Option<String>,
            limit: u16,
        },
    }

    pub mod index {
        use super::*;

        #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
        #[serde(rename_all = "snake_case", deny_unknown_fields)]
        pub enum PagesViewQuery {
            ListPages {
                after: Option<String>,
                limit: Option<u16>,
            },
        }
    }
}

pub mod chat {
    use super::*;

    pub mod index {
        use super::*;

        #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
        #[serde(rename_all = "snake_case", deny_unknown_fields)]
        pub enum ChatViewQuery {
            Channels {
                after: Option<String>,
                limit: Option<usize>,
            },
            Channel {
                channel_id: String,
            },
            Roots {
                channel_id: String,
                before_seq: Option<u64>,
                limit: Option<usize>,
            },
            MessagesAround {
                channel_id: String,
                seq: u64,
                limit: Option<usize>,
            },
            Message {
                message_id: String,
            },
            Revisions {
                channel_id: String,
                seq: u64,
            },
            Thread {
                channel_id: String,
                root_seq: u64,
                after_reply_seq: Option<u64>,
                limit: Option<usize>,
            },
        }
    }
}

pub mod tasks {
    use super::*;

    pub const MAX_LIST_LIMIT: u64 = 256;

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum TaskQuery {
        List { limit: u64, after: Option<String> },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum JobsQuery {
        Get { job_id: String },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum WorkQuery {
        Task(TaskQuery),
        Job(JobsQuery),
    }
}

pub mod runs {
    use super::*;

    pub use sdk::Origin as RunOrigin;

    pub const OP_SUBMIT: &str = "submit";

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ActionEnvelope {
        pub operation: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub target: Option<Value>,
        #[serde(default)]
        pub input: Value,
    }

    impl ActionEnvelope {
        pub fn new(operation: impl Into<String>, target: Option<Value>, input: Value) -> Self {
            Self {
                operation: operation.into(),
                target,
                input,
            }
        }
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ModelStatus {
        Active,
        Paused,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ModelRole {
        #[default]
        General,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ModelRecord {
        pub account: u64,
        pub agent_id: String,
        pub owner: sdk::Origin,
        pub display_name: String,
        pub capability: String,
        pub status: ModelStatus,
        #[serde(default, skip_serializing_if = "is_general")]
        pub role: ModelRole,
        pub created_at: u64,
        pub updated_at: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub recipe_hash: Vec<u8>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub skills: Vec<SkillRef>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum LoadMode {
        Always,
        #[default]
        OnDemand,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct SkillRef {
        pub name: String,
        pub source_prefix: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub source_snapshot: Option<String>,
        #[serde(default)]
        pub load: LoadMode,
    }

    fn is_general(role: &ModelRole) -> bool {
        *role == ModelRole::General
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ModelQuery {
        Agents,
        Agent { agent_id: String },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum RunsMsg {
        AgentAction {
            run_id: String,
            request_id: String,
            action: ActionEnvelope,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum RunsQuery {
        Model { query: ModelQuery },
        Catalog { filter: Option<String> },
        PendingRuns,
        RecentRuns,
        ActionRequest { request_id: String },
        Delegations { caller_run_id: String },
    }

    pub fn encode<T: Serialize>(value: &T) -> Result<Value, String> {
        serde_json::to_value(value).map_err(|error| error.to_string())
    }

    pub fn encode_msg(message: &RunsMsg) -> Vec<u8> {
        sdk::wire::encode(message)
    }
}

#[cfg(test)]
pub mod producer_catalog_fixture {
    use super::runs;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum LaneKind {
        Live,
        Final,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct OperationView {
        pub name: String,
        pub lanes: Vec<LaneKind>,
    }

    // Producer-derived from SDK 736865710dcfa7c56f9834747287881c1c25d45d
    // crates/modules/apps/runs/wire/src/catalog.rs.
    // Keep this projection test-only: runtime actions fetch the live catalog from Runs.
    const LIVE_ONLY: &[LaneKind] = &[LaneKind::Live];
    const LIVE_AND_FINAL: &[LaneKind] = &[LaneKind::Live, LaneKind::Final];
    const FINAL_ONLY: &[LaneKind] = &[LaneKind::Final];

    const CATALOG: &[(&str, &[LaneKind])] = &[
        ("reply", LIVE_ONLY),
        ("react", LIVE_AND_FINAL),
        ("unreact", LIVE_AND_FINAL),
        ("chat.post_message", LIVE_AND_FINAL),
        ("pages.comment", LIVE_AND_FINAL),
        ("pages.set_checked", LIVE_AND_FINAL),
        ("pages.post", LIVE_AND_FINAL),
        ("jobs.comment", LIVE_AND_FINAL),
        ("tasks.create", LIVE_AND_FINAL),
        ("tasks.update_status", LIVE_AND_FINAL),
        ("duckfs.write_text", LIVE_AND_FINAL),
        ("modules.update", FINAL_ONLY),
        ("forge.open_pr", FINAL_ONLY),
        ("collaboration.deliver", LIVE_ONLY),
        ("collaboration.acknowledge", LIVE_ONLY),
        ("agent.call", LIVE_ONLY),
        (runs::OP_SUBMIT, LIVE_AND_FINAL),
    ];

    pub fn catalog(filter: Option<&str>) -> Vec<OperationView> {
        CATALOG
            .iter()
            .filter(|(name, _)| filter.is_none_or(|prefix| name.starts_with(prefix)))
            .map(|(name, lanes)| OperationView {
                name: (*name).into(),
                lanes: lanes.to_vec(),
            })
            .collect()
    }

    #[test]
    fn producer_catalog_fixture_keeps_order_lanes_and_filter() {
        let all = catalog(None);
        assert_eq!(
            all.iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            CATALOG.iter().map(|(name, _)| *name).collect::<Vec<_>>()
        );
        assert_eq!(all[0].lanes, vec![LaneKind::Live]);
        assert_eq!(all[11].lanes, vec![LaneKind::Final]);
        assert_eq!(all[12].lanes, vec![LaneKind::Final]);
        assert_eq!(all[13].lanes, vec![LaneKind::Live]);
        assert_eq!(all[16].lanes, vec![LaneKind::Live, LaneKind::Final]);
        assert_eq!(
            catalog(Some("modules."))
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["modules.update"]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn producer_fixtures_preserve_the_used_query_shapes() {
        assert_eq!(
            sdk::wire::encode(&forge::ForgeQuery::PrDiff {
                repo: "app".into(),
                number: 8,
            }),
            br#"{"pr_diff":{"repo":"app","number":8}}"#
        );
        assert_eq!(
            sdk::wire::encode(&tasks::WorkQuery::Task(tasks::TaskQuery::List {
                limit: 8,
                after: Some("t-3".into()),
            })),
            br#"{"task":{"list":{"limit":8,"after":"t-3"}}}"#
        );
    }

    #[test]
    fn model_skill_refs_keep_producer_validation_strict() {
        assert!(
            serde_json::from_str::<runs::SkillRef>(
                r#"{"name":"docs","source_prefix":"/shared/skills/docs","load":"unknown"}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<runs::SkillRef>(
                r#"{"name":"docs","source_prefix":"/shared/skills/docs","extra":true}"#
            )
            .is_err()
        );
    }
}
