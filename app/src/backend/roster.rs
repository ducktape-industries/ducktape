use super::*;
use identity::{AccountNumber, AccountView, IdentityQuery, IdentityReply};

/// One member of the network: a validator (quorum seat), a resident
/// (mesh + statesync standing), or a registered agent.
#[derive(Clone, Debug, Hash, PartialEq, serde::Serialize)]
pub struct MemberRow {
    pub key: String,
    pub label: String,
    pub role: String,
    pub is_this_node: bool,
    pub is_agent: bool,
    /// an agent's capability tag; empty for a human member.
    pub model: String,
    /// a HUMAN row: the mesh reports this key as a live peer (this node is
    /// live by definition). An AGENT row: the registry says active rather than
    /// paused — `MemberPresence` renders the two vocabularies apart on
    /// `is_agent`. Neither is "working right now"; that is `AgentRow.live`.
    pub live: bool,
}

#[derive(Clone, Debug, Hash, PartialEq)]
pub struct MembersData {
    pub generation: i64,
    pub members: Vec<MemberRow>,
}

/// The network's name directory as this process last read it: the account
/// name bound to every user key. Every surface that names a key reads it, and
/// every read of the identity roster ([`read_accounts`]) rewrites it whole —
/// on each chat load, before a row renders, and on every identity op the live
/// stream delivers.
static NAME_DIRECTORY: std::sync::RwLock<Names> = std::sync::RwLock::new(Names {
    generation: 0,
    directory: NameDirectory::empty(),
});

/// The directory with the generation of the read that seated it, so a
/// holder of a snapshot can tell when the directory has moved on without
/// cloning it again.
struct Names {
    generation: u64,
    directory: NameDirectory,
}

fn read_names() -> std::sync::RwLockReadGuard<'static, Names> {
    NAME_DIRECTORY
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Seats a freshly read directory as the one every surface reads.
fn seat_names(directory: NameDirectory) {
    let mut names = NAME_DIRECTORY
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    names.generation += 1;
    names.directory = directory;
}

/// The directory as last read — a snapshot the caller owns, so a loader can
/// lend it across its awaits and the update thread can read it without one.
pub(crate) fn names() -> NameDirectory {
    read_names().directory.clone()
}

/// The generation the directory is at: it moves on every read that seats
/// one, so a holder of [`names_at`]'s snapshot compares generations instead
/// of directories.
pub(crate) fn names_generation() -> u64 {
    read_names().generation
}

/// The directory and the generation it is at, read together.
pub(crate) fn names_at() -> (u64, NameDirectory) {
    let names = read_names();
    (names.generation, names.directory.clone())
}

/// Every identity account, paged the way the module serves them: numbered
/// from 1 with no gaps, at most `MAX_QUERY_LIMIT` per page. THE ONE read of
/// the identity roster; the name directory is rewritten from what it returns.
pub(crate) async fn read_accounts(client: &RpcClient) -> Result<Vec<AccountView>, String> {
    let page_limit =
        usize::try_from(identity::MAX_QUERY_LIMIT).expect("the identity page cap fits a usize");
    let mut accounts: Vec<AccountView> = Vec::new();
    let mut from: AccountNumber = 0;
    loop {
        let reply: IdentityReply = client
            .query(
                "identity",
                &IdentityQuery::All {
                    from,
                    limit: identity::MAX_QUERY_LIMIT,
                },
            )
            .await?;
        let IdentityReply::Accounts(page) = reply else {
            return Err("the identity module returned the wrong reply".to_string());
        };
        let page_is_last = page.len() < page_limit;
        let Some(last) = page.last().map(|account| account.number) else {
            break;
        };
        accounts.extend(page);
        if page_is_last {
            break;
        }
        from = last + 1;
    }
    seat_names(directory_of(&accounts));
    Ok(accounts)
}

/// The directory an account list binds: every key of an account resolves to
/// that account — its number and its name.
pub(crate) fn directory_of(accounts: &[AccountView]) -> NameDirectory {
    NameDirectory::from_accounts(accounts)
}

/// A test's directory, seated the way a roster read seats it, for as long as
/// the guard lives. The directory is one per process, so tests that seat one
/// take turns on it, and a guard dropped leaves it empty for the next.
#[cfg(test)]
pub(crate) fn seed_names(directory: NameDirectory) -> SeededNames {
    static TURN: Mutex<()> = Mutex::new(());
    let turn = TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let seeded = SeededNames { _turn: turn };
    seeded.seat(directory);
    seeded
}

/// A test's turn on the process-wide directory.
#[cfg(test)]
pub(crate) struct SeededNames {
    _turn: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl SeededNames {
    /// Replace the seated directory, the turn kept.
    pub(crate) fn seat(&self, directory: NameDirectory) {
        seat_names(directory);
    }
}

#[cfg(test)]
impl Drop for SeededNames {
    fn drop(&mut self) {
        self.seat(NameDirectory::empty());
    }
}

/// Whether the account or exact key represented by `me` holds a seat.
pub(crate) fn seated_in(members: &[ChatMember], me: &str) -> bool {
    let Ok(key) = hex_decode(me) else {
        return false;
    };
    let names = names();
    members.iter().any(|member| {
        let handle = match member.key.starts_with("acct:") || member.key.starts_with("user:") {
            true => member.key.clone(),
            false => format!("user:{}", member.key),
        };
        names.owns_handle(&handle, &key)
    })
}

/// Refresh the directory and nothing else — what a chat load does before it
/// renders a row.
pub(crate) async fn refresh_names(client: &RpcClient) -> Result<(), String> {
    read_accounts(client).await.map(|_accounts| ())
}

/// What every chat renderer is handed: this device's key (the `by me` facts)
/// and the directory (every label), owned so a loader can lend a
/// [`ChatReader`] across its awaits.
pub(crate) struct ReaderFacts {
    key: Option<Vec<u8>>,
    names: NameDirectory,
}

impl ReaderFacts {
    pub(crate) async fn current() -> Self {
        Self {
            key: local_user_key().await,
            names: names(),
        }
    }

    /// The update thread's reading — it cannot await, so it takes the cached
    /// key (warm by the time anyone sends) and the directory as last read.
    pub(crate) fn cached() -> Self {
        Self {
            key: rpc::cached_user_key(),
            names: names(),
        }
    }

    pub(crate) fn reader(&self) -> ChatReader<'_> {
        ChatReader::new(self.key.as_deref(), &self.names)
    }

    pub(crate) fn names(&self) -> &NameDirectory {
        &self.names
    }
}

/// Load the roster: validators, then residents, then the registered agents —
/// one list, this node marked, liveness folded in from the mesh sample.
pub async fn load_members(rpc: String, generation: i64) -> Result<MembersData, HydrationError> {
    async {
        let client = rpc_client(&rpc)?;
        let node_key = client.status().await?.public_key;
        let live_keys = live_peer_keys(&client).await;
        let mut members = Vec::new();
        for (query, role) in [("validators", "validator"), ("residents", "resident")] {
            let reply: serde_json::Value =
                client.query("valset", &serde_json::json!(query)).await?;
            let keys = reply[query].as_array().cloned().unwrap_or_default();
            for key in keys {
                let hex = hex_encode(&json_bytes(&key));
                let is_this_node = hex == node_key;
                members.push(MemberRow {
                    label: short_label(&hex),
                    live: is_this_node || live_keys.contains(&hex),
                    is_this_node,
                    is_agent: false,
                    model: String::new(),
                    role: role.into(),
                    key: hex,
                });
            }
        }
        // registered agents are members of the workspace too — the roster shows
        // people AND machines, keyed on the agent id (agents hold no node key;
        // the roster labels that cell "agent id", not "public key").
        let agents = load_agents(rpc, generation).await.map(|data| data.agents);
        for agent in agents.unwrap_or_default() {
            members.push(MemberRow {
                key: agent.id,
                label: agent.name,
                role: "agent".into(),
                is_this_node: false,
                is_agent: true,
                model: agent.capability,
                // for an agent row this is REGISTRATION state (active vs
                // paused), which is what `MemberPresence` renders for a
                // machine — not "working now". The run-in-flight fact is
                // `AgentRow.live`, and only that one may pulse the rail.
                live: agent.status == "active",
            });
        }
        Ok(MembersData {
            generation,
            members,
        })
    }
    .await
    .map_err(|message: String| HydrationError {
        generation,
        message: user_error(message),
    })
}

/// The peer sample's live keys, full hex — the join key for member liveness.
/// A node that cannot answer `/v1/peers` simply reports nobody live.
async fn live_peer_keys(rpc: &RpcClient) -> BTreeSet<String> {
    let Ok(reply) = rpc.peers().await else {
        return BTreeSet::new();
    };
    reply["peers"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        // `connected` and `peer`, NOT `live`/`key`: those are the names
        // `PeerView` serializes (crates/noded/src/peers.rs). Reading the wrong
        // ones made every lookup return null, so this set came back empty on
        // every call and every member rendered offline.
        .filter(|peer| peer["connected"].as_bool().unwrap_or(false))
        .filter_map(|peer| peer["peer"].as_str().map(str::to_string))
        .collect()
}

/// This node holds a quorum seat — the ONE authority predicate behind the
/// approvals gate, the members Invite button and the forge write gate.
pub fn members_is_admin(rows: &[MemberRow]) -> bool {
    rows.iter()
        .any(|row| row.is_this_node && row.role == "validator")
}

/// This node's standing: `validator` | `resident` | `guest`, or `""` when the
/// roster has not answered.
///
/// The empty answer is load-bearing. `load_members` is one of thirteen parallel
/// loads, so it can be the only one that fails, and folding its silence into
/// `guest` told a validator's operator — with no error anywhere on screen —
/// that this device may not post. `""` lights the STANDING UNKNOWN arm in
/// Node view instead.
///
/// An empty vec is the only unanswered signal a pure row function has, and it
/// is a sound one: an answered roster always carries the chain's own
/// validators. So a roster that DID answer and holds no row for this node is a
/// real guest and still reads `guest` — the guest card is not collateral here.
pub fn member_tier(rows: &[MemberRow]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    rows.iter()
        .find(|row| row.is_this_node)
        .map_or_else(|| "guest".into(), |row| row.role.clone())
}
