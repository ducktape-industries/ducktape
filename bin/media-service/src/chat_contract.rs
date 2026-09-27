//! The media service's local chat wire slice.
//!
//! Shapes and fixtures are copied from ducktape-sdk
//! `736865710dcfa7c56f9834747287881c1c25d45d`.

use serde::{Deserialize, Serialize};

pub const DEFAULT_CHAT_TARGET: &str = "chat";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Party {
    Account(u64),
    Key(Vec<u8>),
    Module(String),
    System,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Mark {
    Bold,
    Italic,
    Link(String),
    Mention(Party),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Span {
    pub text: String,
    pub marks: Vec<Mark>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Block {
    Paragraph(Vec<Span>),
    Quote(Vec<Span>),
    Code { lang: Option<String>, text: String },
    Divider,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PostPolicy {
    Open,
    MembersOnly,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HuddleMember {
    pub party: Party,
    pub node: Vec<u8>,
    pub joined_at: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Channel {
    pub id: String,
    pub name: String,
    pub created_at: u64,
    pub head_seq: u64,
    pub post_policy: PostPolicy,
    pub hooks: Vec<String>,
    pub pinned: Vec<u64>,
    pub huddle: Vec<HuddleMember>,
    pub voice: bool,
    pub owner: Party,
    pub archived: bool,
    pub revision: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ChatMsg {
    CreateChannel {
        channel_id: String,
        name: String,
        post_policy: PostPolicy,
    },
    CreateVoiceChannel {
        channel_id: String,
        name: String,
    },
    CreateDmChannel {
        counterpart: u64,
        name: String,
    },
    RenameChannel {
        channel_id: String,
        name: String,
    },
    SetChannelArchived {
        channel_id: String,
        archived: bool,
    },
    PostMessage {
        channel_id: String,
        message_id: String,
        blocks: Vec<Block>,
        thread: Option<u64>,
    },
    EditMessage {
        channel_id: String,
        seq: u64,
        blocks: Vec<Block>,
        base_rev: Option<u32>,
    },
    DeleteMessage {
        channel_id: String,
        seq: u64,
    },
    AddReaction {
        channel_id: String,
        seq: u64,
        emoji: String,
    },
    RemoveReaction {
        channel_id: String,
        seq: u64,
        emoji: String,
    },
    RegisterHook {
        channel_id: String,
        module_id: String,
    },
    UnregisterHook {
        channel_id: String,
        module_id: String,
    },
    SetMembership {
        channel_id: String,
        party: Party,
        member: bool,
    },
    JoinHuddle {
        channel_id: String,
        node: Vec<u8>,
        node_proof: Vec<u8>,
    },
    LeaveHuddle {
        channel_id: String,
    },
    SweepHuddle {
        channel_id: String,
        party: Party,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ChatQuery {
    Channel {
        channel_id: String,
    },
    MessagesRange {
        channel_id: String,
        from_seq: u64,
        limit: u64,
    },
    Message {
        message_id: String,
    },
    Access {
        channel_id: String,
        party: Party,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ChatReply {
    Channel(Option<Channel>),
    Messages(Vec<serde_json::Value>),
    Message(Option<serde_json::Value>),
    Access(serde_json::Value),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn producer_fixture_preserves_message_tags() {
        let message = ChatMsg::SweepHuddle {
            channel_id: "room".into(),
            party: Party::Account(42),
        };
        assert_eq!(
            sdk::wire::encode(&message),
            br#"{"sweep_huddle":{"channel_id":"room","party":{"account":42}}}"#
        );
        let query = ChatQuery::Channel {
            channel_id: "room".into(),
        };
        assert_eq!(
            sdk::wire::encode(&query),
            br#"{"channel":{"channel_id":"room"}}"#
        );
    }
}
