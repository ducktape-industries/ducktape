use serde::{Deserialize, Serialize};

pub type AccountNumber = u64;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Party {
    Account(AccountNumber),
    Key(Vec<u8>),
    Module(String),
    System,
}

impl Party {
    pub fn account(&self) -> Option<AccountNumber> {
        match self {
            Self::Account(account) => Some(*account),
            Self::Key(_) | Self::Module(_) | Self::System => None,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Span {
    pub text: String,
    pub marks: Vec<Mark>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Mark {
    Bold,
    Italic,
    Link(String),
    Mention(Party),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Block {
    Paragraph(Vec<Span>),
    Code { lang: Option<String>, text: String },
    Quote(Vec<Span>),
    Divider,
}

impl Block {
    pub(crate) fn paragraph(text: impl Into<String>) -> Self {
        Self::Paragraph(vec![Span {
            text: text.into(),
            marks: Vec::new(),
        }])
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PostPolicy {
    Open,
    MembersOnly,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ChatMsg {
    CreateChannel {
        channel_id: String,
        name: String,
        post_policy: PostPolicy,
    },
    PostMessage {
        channel_id: String,
        message_id: String,
        blocks: Vec<Block>,
        thread: Option<u64>,
    },
}

pub(crate) fn encode_msg(message: &ChatMsg) -> Vec<u8> {
    sdk::wire::encode(message)
}

#[cfg(test)]
pub(crate) fn decode_msg(bytes: &[u8]) -> Result<ChatMsg, String> {
    sdk::wire::decode(bytes)
}
