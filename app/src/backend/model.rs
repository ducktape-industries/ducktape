use super::*;

/// A selected loader call with every argument the chosen effect needs.
#[derive(Clone, Debug, Hash, PartialEq)]
pub struct LoadRequest {
    pub rpc: String,
    pub key: String,
    pub generation: i64,
}

/// Select a loader without launching an offscreen refusal. `try` turns
/// `None` into `Task::none`, leaving any unrelated in-flight lane untouched.
pub fn load_request(
    condition: bool,
    rpc: String,
    key: String,
    generation: i64,
) -> Option<LoadRequest> {
    condition.then_some(LoadRequest {
        rpc,
        key,
        generation,
    })
}

pub fn mutation_failure_phase(committed: bool) -> crate::MutationPhase {
    if committed {
        crate::MutationPhase::Recovering
    } else {
        crate::MutationPhase::Idle
    }
}

pub fn mutation_phase_after_recovery(current: crate::MutationPhase) -> crate::MutationPhase {
    if current == crate::MutationPhase::Recovering {
        crate::MutationPhase::Idle
    } else {
        current
    }
}

/// FOLD A LOAD'S ROWS INTO THE LIST ON SCREEN — do not replace it with them.
///
/// The switch loader is handed the list the reader is already looking at and
/// answers with the one row it refreshed (`load_channel_window_data`), so
/// assigning its list back would revert every delta the live stream folded
/// during the round trip: a peer's post in a THIRD room and the unread badge it
/// lit, a channel created, renamed or archived. Nothing re-pages the list
/// afterwards — `load_chat` is raised only by a reconnect — so that loss is
/// permanent, not a frame of staleness.
///
/// `head_seq` only moves FORWARD. The row was read mid-flight; a delta folded
/// after that read is the newer fact, and letting the row walk it back relights
/// a badge the reader has already cleared.
pub fn upsert_channel_rows(
    mut channels: Vec<ChatChannel>,
    refreshed: Vec<ChatChannel>,
) -> Vec<ChatChannel> {
    for mut row in refreshed {
        let Some(current) = channels.iter_mut().find(|current| current.id == row.id) else {
            channels.push(row);
            continue;
        };
        row.head_seq = row.head_seq.max(current.head_seq);
        *current = row;
    }
    channels
}

/// HAS THE CONSOLE'S CHANNEL LIST OUTLIVED ITS NETWORK?
///
/// `held` is the chain the list on screen was learned from; `live` is the chain
/// the node's own pushed status document is naming NOW. A workspace switch does
/// not change the endpoint — the node comes back on the same loopback port — so
/// the console can live right through one: the websocket drops, reconnects, and
/// resyncs, and no `connect` ever re-runs to install the new network's list
/// outright. Everything the resync does is a FOLD, which only ever adds, so the
/// sidebar went on drawing the previous workspace's `#general` in a network that
/// has no such room, clickable, with nothing behind it.
///
/// An empty reading on either side is not a move: it is a console that has not
/// been told which chain it is on yet, and dropping the list on that would blank
/// the sidebar on every cold boot.
pub fn chain_moved(held: String, live: String) -> bool {
    !held.is_empty() && !live.is_empty() && held != live
}

/// Everything a room click projects from the channel list, computed in one
/// ownership crossing. The old shape cloned and scanned the whole workspace
/// four times before the load task could even start.
#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct ChannelSwitchFacts {
    pub name: String,
    pub archived: bool,
    pub members_only: bool,
}

pub fn channel_switch_facts(
    channels: Vec<ChatChannel>,
    next_channel: String,
    current_name: String,
) -> ChannelSwitchFacts {
    let row = channels.iter().find(|row| row.id == next_channel);
    ChannelSwitchFacts {
        name: row.map_or(current_name, |row| row.name.clone()),
        archived: row.is_some_and(|row| row.archived),
        members_only: row.is_some_and(|row| row.members_only),
    }
}

/// THE COMPOSER'S INSTANCE KEY (ducktape-ui#697). One retained
/// `ChatComposer` per room, so a draft never rides a room switch — and the
/// ENDPOINT is in the key because a channel id is a user-chosen string:
/// network A's `#general` and network B's `#general` are two rooms, and the
/// park store this replaced had to be emptied by hand on every network switch
/// to keep one from handing its words to the other.
pub fn composer_scope(endpoint: &str, channel_id: &str) -> String {
    format!("{endpoint}\u{1f}{channel_id}")
}

/// Whether a submitted body may be posted, decided ONCE at delivery from
/// state that may have moved since the composer's frame drew its gate.
///
/// It is a verdict and not a bool because the two answers do different work:
/// an admitted body starts a send, a refused one goes back to the composer
/// it came from. One discriminant, one `match`, each arm ending in its own
/// task — a boolean would have to be read twice, and the second read is
/// where a `return if` swallows the words.
///
/// `scope` is the box the body was written in and `current` the box the
/// screen would post from now: a submit queued before the reader moved —
/// another room, another item, another network — is refused, and the arm
/// hands it back to the box it came from rather than posting it here.
pub fn submit_verdict(
    busy: bool,
    connected: bool,
    channel: String,
    refusal: String,
    seated: bool,
    scope: String,
    current: String,
) -> crate::SubmitVerdict {
    let refused = busy
        || !connected
        || channel.is_empty()
        || !refusal.is_empty()
        || !seated
        || scope != current;
    if refused {
        crate::SubmitVerdict::Refused
    } else {
        crate::SubmitVerdict::Admitted
    }
}

pub(crate) struct Tip {
    pub(crate) height: i64,
    pub(crate) status: String,
}

pub(crate) fn rpc_client(input: &str) -> Result<RpcClient, String> {
    let configured = if input.trim().is_empty() {
        std::env::var("DUCKTAPE_NODE")
            .ok()
            .or_else(super::shell::lone_workspace_endpoint)
            .unwrap_or_else(|| DEFAULT_RPC.to_string())
    } else {
        input.trim().to_string()
    };
    // One client per origin for the process's life: the reqwest pool and TLS
    // setup survive across externs instead of being rebuilt by every `run`
    // (one hydrate fans out 13 of them in a single parallel).
    // ponytail: never evicts — the map holds one entry per endpoint the user
    // has ever pointed this session at, which is their handful of networks.
    static CLIENTS: std::sync::Mutex<std::collections::BTreeMap<String, RpcClient>> =
        std::sync::Mutex::new(std::collections::BTreeMap::new());
    let mut clients = CLIENTS.lock().expect("rpc client cache");
    let client = match clients.get(&configured) {
        Some(client) => client.clone(),
        None => {
            let client = RpcClient::new(&configured).map_err(String::from)?;
            clients.insert(configured, client.clone());
            client
        }
    };
    // The node's own operator credential, when this device holds the node's
    // workspace: every MUTATING `/v1` route (a files commit, an invite mint, a
    // frameless submit) refuses a caller that presents neither it nor a
    // per-request user signature. The app already reads this same directory for
    // the service-link token, and a node with no local workspace here is a
    // REMOTE one — read-only from this device, which the node's own 401 says.
    //
    // Read per call and NEVER latched into the cached client: the node re-mints
    // this secret on every boot, so a client that kept the one it was built
    // with would 401 every write from the first node restart until the app
    // itself restarted. The cache exists for the connection pool, not the
    // credential.
    Ok(match operator_token_for(client.origin()) {
        Some(token) => client.with_operator_token(token),
        None => client,
    })
}

/// The `admin.token` of the node serving `origin`, if this device has its
/// workspace registered.
///
/// Matches on the ALREADY-CANONICAL origin the client just parsed, never
/// through `shell::workspace_at` — that canonicalizes by calling
/// [`rpc_client`], which is where this runs, holding its cache lock. The
/// bug that shape produced was not a slow test but a dead process.
fn operator_token_for(origin: &str) -> Option<String> {
    let (_, workspace) = super::shell::workspaces()
        .into_iter()
        .find(|(_, dir)| super::shell::workspace_endpoint(dir).as_deref() == Some(origin))?;
    let token = std::fs::read_to_string(workspace.join("admin.token")).ok()?;
    let token = token.trim().to_string();
    (!token.is_empty()).then_some(token)
}
