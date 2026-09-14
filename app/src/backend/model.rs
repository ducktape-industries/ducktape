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

pub fn fresh_operation_id(prefix: String) -> String {
    fresh_id(&prefix)
}

pub fn restore_draft(current: String, pending: String, keep_pending: bool) -> String {
    if keep_pending {
        return current;
    }
    if current.is_empty() { pending } else { current }
}

pub fn remember_failed_draft(
    existing: String,
    current: String,
    pending: String,
    committed: bool,
) -> String {
    if committed || current.is_empty() || pending.is_empty() {
        return existing;
    }
    if existing.is_empty() {
        return pending;
    }
    format!("{existing}\n{pending}")
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

fn committed_message_change(phase: crate::MutationPhase, committed: bool) -> bool {
    if !committed {
        return false;
    }
    match phase {
        crate::MutationPhase::MessageEdit => true,
        crate::MutationPhase::Idle
        | crate::MutationPhase::Recovering
        | crate::MutationPhase::Channel
        | crate::MutationPhase::Huddle
        | crate::MutationPhase::Onboarding => false,
    }
}

pub fn message_seq_after_failure(
    current: i64,
    phase: crate::MutationPhase,
    committed: bool,
) -> i64 {
    if committed_message_change(phase, committed) {
        0
    } else {
        current
    }
}

// --- Client-local unread tracking (no wire read-cursor) ------------------
//
// `channel_reads` is a per-channel last-seen `seq`. `unread_boundary` is that
// value FROZEN at the moment you entered the current channel, used only to
// place the in-channel "New messages" divider for this visit.

fn last_read_of(reads: &[ChannelRead], channel: &str) -> i64 {
    reads
        .iter()
        .find(|read| read.channel == channel)
        .map_or(0, |read| read.seq)
}

fn head_seq_of(channels: &[ChatChannel], channel: &str) -> i64 {
    channels
        .iter()
        .find(|entry| entry.id == channel)
        .map_or(0, |entry| entry.head_seq)
}

pub fn channel_last_read(reads: Vec<ChannelRead>, channel: String) -> i64 {
    last_read_of(&reads, &channel)
}

pub fn channel_head_seq(channels: Vec<ChatChannel>, channel: String) -> i64 {
    head_seq_of(&channels, &channel)
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
    pub unread_boundary: i64,
    pub name: String,
    pub archived: bool,
    pub members_only: bool,
}

pub fn channel_switch_facts(
    reads: Vec<ChannelRead>,
    channels: Vec<ChatChannel>,
    current_channel: String,
    next_channel: String,
    current_boundary: i64,
    current_name: String,
) -> ChannelSwitchFacts {
    let row = channels.iter().find(|row| row.id == next_channel);
    let head_seq = row.map_or(0, |row| row.head_seq);
    let unread_boundary = if current_channel == next_channel {
        current_boundary
    } else {
        let last_read = last_read_of(&reads, &next_channel);
        if head_seq > last_read { last_read } else { 0 }
    };
    ChannelSwitchFacts {
        unread_boundary,
        name: row.map_or(current_name, |row| row.name.clone()),
        archived: row.is_some_and(|row| row.archived),
        members_only: row.is_some_and(|row| row.members_only),
    }
}

/// Is the reader AT the live tail — the other end of the same offset.
///
/// 0.0 is the end the stream is anchored to, so a small band around it counts
/// as "now": the last row is on screen and the next arrival scrolls itself into
/// view.
///
/// An undefined relative offset (`0/0` when content fits) must read as AT
/// THE TAIL — a conversation too short to scroll is entirely on screen — and NaN
/// compares false against everything, so the band is written as the comparison
/// that must SUCCEED to be at the tail, with NaN taken by the explicit arm.
pub fn near_scroll_tail(relative_offset: f64) -> bool {
    relative_offset.is_nan() || relative_offset <= 0.02
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

/// The channel a composer scope names, or "" when the scope belongs to
/// another endpoint. The forge view builds its note composer's scope itself
/// ([`composer_scope`] over the item's channel), so the app reads the
/// channel back out of the scope a send arrives with rather than
/// remembering which item is open — a note written before the reader
/// switched networks addresses a store this endpoint does not hold, and
/// goes back to its own box.
pub fn scope_channel(scope: &str, endpoint: &str) -> String {
    let Some((wrote_at, channel)) = scope.split_once('\u{1f}') else {
        return String::new();
    };
    match wrote_at == endpoint {
        true => channel.to_owned(),
        false => String::new(),
    }
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

/// The rail's key: a reply belongs to its THREAD, and the same seq under two
/// rooms is two different threads.
pub fn thread_scope(endpoint: &str, channel_id: &str, thread_seq: i64) -> String {
    format!("{endpoint}\u{1f}{channel_id}#{thread_seq}")
}

pub fn edit_scope(endpoint: &str, channel_id: &str, seq: i64) -> String {
    format!("{endpoint}\u{1f}{channel_id}#{seq}/edit")
}

/// The thread a reply composer's scope names — the `#<seq>` tail
/// [`thread_scope`] appends — or 0 for a room's own box. The rail belongs to
/// the view, so the thread a submitted reply is for is read back off the box
/// it was written in rather than off any app state that may have moved.
pub fn scope_thread_seq(scope: &str) -> i64 {
    let Some((_, seq)) = scope.rsplit_once('#') else {
        return 0;
    };
    seq.parse().unwrap_or_default()
}

/// The room a composer scope belongs to: a thread scope shorn of the
/// `#<seq>` tail [`thread_scope`] appends, a room scope as it is. A room
/// whose channel id itself ends in `#<digits>` is looked up under its own
/// scope first, so the shearing only ever reaches a thread.
pub fn room_scope(scope: &str) -> String {
    let Some((room, seq)) = scope.rsplit_once('#') else {
        return scope.to_owned();
    };
    let seq = seq.strip_suffix("/edit").unwrap_or(seq);
    let seq_is_thread = !seq.is_empty() && seq.bytes().all(|b| b.is_ascii_digit());
    if seq_is_thread {
        return room.to_owned();
    }
    scope.to_owned()
}
pub fn mark_channel_read(
    mut reads: Vec<ChannelRead>,
    channel: String,
    seq: i64,
) -> Vec<ChannelRead> {
    if channel.is_empty() {
        return reads;
    }
    if let Some(read) = reads.iter_mut().find(|read| read.channel == channel) {
        read.seq = read.seq.max(seq);
        return reads;
    }
    reads.push(ChannelRead { channel, seq });
    reads
}

/// ONE SEND IN FLIGHT, as the screen must paint it before any block carries
/// it. The room's timeline is the chat view's own reading of the index, which
/// cannot know about an operation the node has not committed yet — so the app
/// keeps the admitted sends here and the view paints them at the tail of the
/// surface each was written in (`thread_seq` 0 is the room itself).
#[derive(Clone, Debug, Default, Hash, PartialEq, serde::Serialize)]
pub struct PendingSend {
    pub id: String,
    pub body: String,
    pub thread_seq: i64,
}

/// A newly admitted send, at the end of the queue.
pub fn send_pending(
    mut sends: Vec<PendingSend>,
    id: String,
    body: String,
    thread_seq: i64,
) -> Vec<PendingSend> {
    sends.push(PendingSend {
        id,
        body,
        thread_seq,
    });
    sends
}

/// The queue without the send `id` names — it committed, or it failed and its
/// words went back to the composer it was written in.
pub fn send_settled(mut sends: Vec<PendingSend>, id: &str) -> Vec<PendingSend> {
    sends.retain(|send| send.id != id);
    sends
}

/// The queue after a send FAILED. A committed one stays: the block carrying it
/// landed and only the read after it failed, so taking the row off now would
/// blank the message she just sent until the recovery resync puts it back.
pub fn send_failed(sends: Vec<PendingSend>, id: &str, committed: bool) -> Vec<PendingSend> {
    match committed {
        true => sends,
        false => send_settled(sends, id),
    }
}

/// One channel row with the unread decision already attached.
#[derive(Clone, Debug, Hash, PartialEq, serde::Serialize)]
pub struct ChatSidebarRow {
    pub channel: ChatChannel,
    pub unread: bool,
}

/// The CHANNELS section, prepared when its source state moves.
///
/// A DM IS EXCLUDED BY THE DIRECTORY'S OWN ID, not by a second derivation of
/// it. This used to re-hash `dm_channel_id(account_number, peer.key)` per row,
/// which answers NOTHING while `account_number` is empty — an account load that
/// lost its race with the console (a freshly joined resident spends minutes
/// with a node that cannot answer for identity yet) put every DM in the room
/// list, `#` glyph, "Members only" badge, Huddle button and all, beside the same
/// person's DIRECT row. `DmPeer.channel_id` is the id `load_dm_peers` already
/// derived from the account number IT resolved, and `chat_sidebar_dms` reads
/// that same field — so the two sections cannot disagree about what a DM is.
pub fn chat_sidebar_rooms(
    channels: Vec<ChatChannel>,
    peers: Vec<DmPeer>,
    reads: Vec<ChannelRead>,
) -> Vec<ChatSidebarRow> {
    let read_seqs: BTreeMap<&str, i64> = reads
        .iter()
        .map(|read| (read.channel.as_str(), read.seq))
        .collect();
    let dm_ids: BTreeSet<&str> = peers
        .iter()
        .map(|peer| peer.channel_id.as_str())
        .filter(|id| !id.is_empty())
        .collect();
    // NO DM IS A CHANNEL — not mine, and least of all somebody else's.
    //
    // A DM record is an ordinary channel row and the chat index serves every
    // channel to every member, so the two-party room a COLLABORATOR opened
    // between their own two accounts arrived in this list. Being in no peer's
    // `channel_id` it fell through the directory exclusion and drew under
    // CHANNELS with a `#` glyph and that person's name, beside their real
    // DIRECT row — it reads as "clicking a DM created a channel". Mine belong
    // in DIRECT, which `chat_sidebar_dms` builds from the peer directory; a
    // DM of theirs belongs nowhere on my screen, so the derived SHAPE is the
    // second half of the rule.
    let is_a_dm_room = |channel: &ChatChannel| {
        dm_ids.contains(channel.id.as_str()) || ::chat::client::is_derived_dm_channel(&channel.id)
    };
    channels
        .into_iter()
        .filter(|channel| !is_a_dm_room(channel))
        .map(|channel| ChatSidebarRow {
            unread: channel.head_seq
                > read_seqs
                    .get(channel.id.as_str())
                    .copied()
                    .unwrap_or_default(),
            channel,
        })
        .collect()
}

/// One DIRECT row with the unread decision already attached.
#[derive(Clone, Debug, Hash, PartialEq, serde::Serialize)]
pub struct DmSidebarRow {
    pub peer: DmPeer,
    pub unread: bool,
}

/// The DIRECT section, prepared when its directory, channels, or read cursors
/// move. Channel heads are indexed once so the projection itself stays linear.
pub fn chat_sidebar_dms(
    channels: Vec<ChatChannel>,
    peers: Vec<DmPeer>,
    reads: Vec<ChannelRead>,
) -> Vec<DmSidebarRow> {
    let read_seqs: BTreeMap<&str, i64> = reads
        .iter()
        .map(|read| (read.channel.as_str(), read.seq))
        .collect();
    let heads: BTreeMap<String, i64> = channels
        .into_iter()
        .map(|channel| (channel.id, channel.head_seq))
        .collect();
    peers
        .into_iter()
        .map(|peer| {
            let head_seq = heads.get(&peer.channel_id).copied().unwrap_or_default();
            DmSidebarRow {
                unread: head_seq
                    > read_seqs
                        .get(peer.channel_id.as_str())
                        .copied()
                        .unwrap_or_default(),
                peer,
            }
        })
        .collect()
}

/// On first connect, seed each not-yet-tracked channel's cursor to its own head
/// so the session starts fully caught up. Existing entries are preserved.
pub fn initial_channel_reads(
    channels: Vec<ChatChannel>,
    existing: Vec<ChannelRead>,
) -> Vec<ChannelRead> {
    let mut reads = existing;
    for channel in channels {
        let tracked = reads.iter().any(|read| read.channel == channel.id);
        if !tracked {
            reads.push(ChannelRead {
                channel: channel.id,
                seq: channel.head_seq,
            });
        }
    }
    reads
}

/// Where to freeze the "New messages" divider when entering a channel. Only
/// re-freezes on an actual channel change — a same-channel refresh keeps the
/// divider still. Returns 0 (no divider) when arriving already caught up, so a
/// caught-up channel never grows a divider above your own later sends or live
/// arrivals during the visit.
pub fn frozen_unread_boundary(
    reads: Vec<ChannelRead>,
    channels: Vec<ChatChannel>,
    current_channel: String,
    next_channel: String,
    current_boundary: i64,
) -> i64 {
    if current_channel == next_channel {
        return current_boundary;
    }
    let last_read = last_read_of(&reads, &next_channel);
    let head = head_seq_of(&channels, &next_channel);
    let arrived_with_unread = head > last_read;
    if arrived_with_unread { last_read } else { 0 }
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
