//! DuckDNS wire surface: one optional human name for one Identity account.
//!
//! `.duck` is Ducktape presentation syntax. It is not installed into the host
//! DNS stack and it never returns nodes, routes, endpoints, or transports.
//! Resolution stops at the stable AccountId; Identity and the existing peer /
//! reachability planes own every later step.

use serde::{Deserialize, Serialize};

pub const DUCKDNS_ZONE: &str = "duck";
/// Structural labels reserved directly below `.duck`. `net` is the duck
/// browser's inert internal namespace (`net.duck` pages render inline and
/// must never resolve to an account) — a registrable "net" handle would
/// collide with it. `agents` is the synthetic domain of agent attribution
/// idents (`<agent_id>@agents.duck`, see the forge lane): a registrable
/// "agents" handle would let one account own every agent address AND every
/// `<route>.agents.duck` browse.
///
/// This is THE reserved set — every other copy mirrors it: the app's
/// `RESERVED_ROOT_LABELS` (`app/src/domain/duckdns-client.ts`, pinned to this
/// literal by `duckdns-client.test.ts`) and `ops/demo-gateway.mjs`.
///
/// Enforced by registration, hostname parsing, and snapshot validation.
pub const RESERVED_ROOT_LABELS: &[&str] = &["net", "agents"];
pub const MAX_LABEL_LEN: usize = 63;
pub const MAX_QUERY_LIMIT: u64 = 256;

/// One parsed account name in Ducktape's internal `.duck` namespace.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DuckDnsName {
    /// `<handle>.duck` — an optional human alias for one stable AccountId.
    pub handle: String,
}

/// The stable result of resolving one account name: the account NUMBER. Node
/// selection is deliberately absent and must go through Identity plus peer
/// management.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAccount {
    pub account_id: u64,
}

/// One optional human-readable alias for an Identity account. The account
/// number is authority; `handle` is only a mutable presentation key.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct HandleRegistration {
    pub handle: String,
    pub account_id: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DuckDnsMsg {
    /// Declaratively replace the authenticated account's optional handle.
    /// `None` unregisters it without changing Identity.
    SetHandle { handle: Option<String> },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DuckDnsQuery {
    Resolve { name: DuckDnsName },
    Registrations { from: u64, limit: u64 },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DuckDnsReply {
    Resolved(Option<ResolvedAccount>),
    Registrations(Vec<HandleRegistration>),
}

pub fn encode_msg(message: &DuckDnsMsg) -> Vec<u8> {
    serde_json::to_vec(message).expect("serializable")
}

pub fn decode_msg(bytes: &[u8]) -> Result<DuckDnsMsg, String> {
    serde_json::from_slice(bytes).map_err(|error| error.to_string())
}

pub fn encode_query(query: &DuckDnsQuery) -> Vec<u8> {
    serde_json::to_vec(query).expect("serializable")
}

pub fn decode_query(bytes: &[u8]) -> Result<DuckDnsQuery, String> {
    serde_json::from_slice(bytes).map_err(|error| error.to_string())
}

pub fn encode_reply(reply: &DuckDnsReply) -> Vec<u8> {
    serde_json::to_vec(reply).expect("serializable")
}

pub fn decode_reply(bytes: &[u8]) -> Result<DuckDnsReply, String> {
    serde_json::from_slice(bytes).map_err(|error| error.to_string())
}
