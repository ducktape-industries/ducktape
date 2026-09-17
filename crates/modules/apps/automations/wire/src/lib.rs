//! the automations module's public wire surface -- types only.
//!
//! writes go via [`AutomationsMsg`]; reads via [`AutomationsQuery`] ->
//! [`AutomationsReply`]. the chat hook seam delivers a `chat::ChatEvent`
//! payload; the automations module decodes it inside its origin-gated hook arm
//! (see [`AutomationsMsg::HookEvent`]).

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// what makes a rule fire: a chat message-posted filter. every `None` field is
/// a wildcard; every `Some` field must match the triggering event. a flat
/// single-shape struct in both the JSON wire and the snapshot codec.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
// Every field is optional, so unknown fields must reject instead of
// accidentally producing a trigger that matches everything.
#[serde(deny_unknown_fields)]
pub struct Trigger {
    /// an exact channel id, or `None` for any channel.
    pub channel_id: Option<String>,
    /// A substring matched against each mentioned account's `acct:{number}`
    /// rendering; `None` imposes no mention constraint.
    pub mention: Option<String>,
    /// a case-sensitive substring tested against the post's concatenated
    /// text blocks; `None` = no text constraint.
    pub text_contains: Option<String>,
}

/// what a firing rule does.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    /// post `template` (after placeholder substitution) into `channel_id`. the
    /// module derives a deterministic `message_id` = `auto-{rule_id}-{channel}-{seq}`.
    PostMessage {
        channel_id: String,
        template: String,
    },
    /// create a task whose id = `{task_id_prefix}-{channel}-{seq}` (deterministic,
    /// collision-free per triggering message) and title = substituted
    /// `title_template`.
    CreateTask {
        task_id_prefix: String,
        title_template: String,
    },
    /// Publish a source-owned report to `recipient`, attributed to the rule's
    /// creator. Body substitutions share the existing action-template budget.
    Report {
        recipient: sdk::AccountNumber,
        kind: String,
        body_template: String,
    },
}

/// a user-defined automation rule.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub rule_id: String,
    /// The account that registered the rule: the tasks it creates and the
    /// reports it publishes are attributed to this account.
    pub owner: sdk::AccountNumber,
    pub enabled: bool,
    pub trigger: Trigger,
    pub action: Action,
    pub created_at: u64,
    /// how many times this rule has emitted an action (successful fires only).
    pub fire_count: u64,
}

/// one entry in the module's bounded global run-history ring.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    pub rule_id: String,
    /// the triggering channel id.
    pub channel_id: String,
    pub seq: u64,
    pub height: u64,
    /// `true` when an action was emitted; `false` for a skipped fire (malformed
    /// action, or the per-event action budget was exceeded).
    pub action_ok: bool,
    pub detail: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AutomationsMsg {
    /// register a rule; the submitter is recorded as its creator.
    CreateRule {
        rule_id: String,
        trigger: Trigger,
        action: Action,
    },
    /// enable or disable a rule — a disabled rule stays registered and stops
    /// firing. Any account may administer any rule.
    SetEnabled { rule_id: String, enabled: bool },
    /// delete a rule. Any account may.
    DeleteRule { rule_id: String },
    /// the chat hook payload: the `chat::ChatEvent` bytes chat delivers
    /// as a follow-up. HONORED ONLY when the dispatch origin is the chat module;
    /// a non-chat origin claiming a hook event is rejected as a spoof.
    HookEvent(Vec<u8>),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AutomationsQuery {
    ListRules,
    GetRule {
        rule_id: String,
    },
    /// the most recent `limit` run-history records for `rule_id`, oldest-first.
    RunHistory {
        rule_id: String,
        limit: u64,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AutomationsReply {
    Rules(Vec<Rule>),
    Rule(Option<Rule>),
    History(Vec<RunRecord>),
}

pub fn encode_msg(m: &AutomationsMsg) -> Vec<u8> {
    sdk::wire::encode(m)
}

pub fn decode_msg(b: &[u8]) -> Result<AutomationsMsg, String> {
    sdk::wire::decode(b)
}

pub fn encode_query(q: &AutomationsQuery) -> Vec<u8> {
    sdk::wire::encode(q)
}

pub fn decode_query(b: &[u8]) -> Result<AutomationsQuery, String> {
    sdk::wire::decode(b)
}

pub fn encode_reply(r: &AutomationsReply) -> Vec<u8> {
    sdk::wire::encode(r)
}

pub fn decode_reply(b: &[u8]) -> Result<AutomationsReply, String> {
    sdk::wire::decode(b)
}
