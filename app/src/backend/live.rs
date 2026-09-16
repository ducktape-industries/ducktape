use super::*;
use ::chat;

/// One UI publication may carry at most this many consecutive chat deltas.
/// The cap bounds one reducer pass; the capacity-one publication gate below
/// waits for the UI to finish that pass before the stream reads another one.
/// No clock participates in batching or fairness.
pub(crate) const LIVE_CHAT_BATCH_LIMIT: usize = 64;

enum PendingLiveEvent {
    Event(ducktape_rpc::Result<ModuleEvent>),
    Closed,
    Update(Box<LiveUpdate>),
}

struct LiveEventState {
    rpc: String,
    cursors: BTreeMap<String, String>,
    stream: Option<ducktape_rpc::ModuleEventStream>,
    pending: Option<PendingLiveEvent>,
    retry_attempt: u32,
    publication_gate: Arc<tokio::sync::Semaphore>,
    /// the registry ids the open stream was subscribed with; a registry that
    /// has since listed more reopens the stream (cursors carry over)
    registry_subscribed: Vec<String>,
}

/// The planes every console draws. A registry-listed id joins the list at
/// connect — a registered view reads its module's plane through `rpc.live`.
const BUILT_IN_PLANES: [&str; 10] = [
    "chat",
    "pages",
    "inbox",
    "forge",
    "valset",
    "governance",
    "identity",
    "agent",
    "runs",
    "files",
];

pub(crate) fn subscribed_planes(registry: &[String]) -> Vec<String> {
    let mut planes: Vec<String> = BUILT_IN_PLANES.iter().map(|id| (*id).to_string()).collect();
    for id in registry {
        let built_in = planes.iter().any(|plane| plane == id);
        if !built_in {
            planes.push(id.clone());
        }
    }
    planes
}

/// Merge one later, consecutive chat publication into `batch` when the shared
/// production cap permits it. A returned update is a non-chat publication (or
/// the next full chat batch) that the caller must preserve without reordering.
fn merge_live_chat_batch(batch: &mut LiveUpdate, mut next: LiveUpdate) -> Option<LiveUpdate> {
    let both_chat = batch.kind == crate::LiveKind::Chat && next.kind == crate::LiveKind::Chat;
    let fits = batch.chat.len().saturating_add(next.chat.len()) <= LIVE_CHAT_BATCH_LIMIT;
    if !both_chat || !fits {
        return Some(next);
    }
    batch.chat.append(&mut next.chat);
    batch.status = next.status;
    batch.height = next.height;
    None
}

/// Deterministic seam for allocation/update-count probes. Its input is the
/// sequence of already-ready, already-folded publications; production uses
/// the same [`merge_live_chat_batch`] decision while greedily polling the live
/// socket. Non-chat publications are ordering barriers.
#[cfg(test)]
pub(crate) fn batch_live_updates(updates: Vec<LiveUpdate>) -> Vec<LiveUpdate> {
    let mut emitted = Vec::new();
    for update in updates {
        let Some(batch) = emitted.last_mut() else {
            emitted.push(update);
            continue;
        };
        if let Some(update) = merge_live_chat_batch(batch, update) {
            emitted.push(update);
        }
    }
    emitted
}

/// Opening a workspace publishes the work it is waiting for before its result.
/// The retry delay belongs to the same cancellable task as the reads; no detached
/// worker may answer after the user has left this connection generation.
pub fn connect(
    rpc: String,
    attempt: i64,
    generation: i64,
) -> ducktape_view_guest::Task<crate::AppMessage> {
    use crate::AppMessage;
    use ducktape_view_guest::Task;

    let start = Task::perform(
        async move {
            if attempt > 0 {
                tokio::time::sleep(retry_delay(u32::try_from(attempt).unwrap_or(u32::MAX))).await;
            }
        },
        move |()| AppMessage::ConnectionProgress(generation, "Loading chat and workspace…"),
    );
    let load = Task::perform(
        async move {
            let rpc = rpc_client(&rpc)?;
            // The views and workspace load concurrently, as they do on a warm
            // connection. The next publication names the remaining view wait.
            let views = crate::module_view::connected(&rpc);
            let workspace = load_workspace(&rpc, None, generation).await?;
            Ok::<_, String>((workspace, views))
        },
        |result| result,
    )
    .then(move |result| match result {
        Ok((workspace, views)) => Task::done(AppMessage::ConnectionProgress(
            generation,
            "Preparing workspace screens…",
        ))
        .chain(Task::perform(
            async move {
                views.settled().await;
                workspace
            },
            AppMessage::WorkspaceConnected,
        )),
        Err(cause) => Task::done(AppMessage::ConnectFailed(HydrationError {
            generation,
            message: user_error(cause.to_string()),
        })),
    });
    start.chain(load)
}

pub fn live_events(rpc: String) -> futures::stream::BoxStream<'static, LiveUpdate> {
    futures::stream::unfold(
        LiveEventState {
            rpc,
            cursors: BTreeMap::new(),
            stream: None,
            pending: None,
            retry_attempt: 0,
            publication_gate: Arc::new(tokio::sync::Semaphore::new(1)),
            registry_subscribed: Vec::new(),
        },
        |mut state| async move {
            // One subscription item may exist outside this stream at a time.
            // The app drops the LiveUpdated message after `update`;
            // that drop is the acknowledgement that releases this permit.
            let publication_permit = state
                .publication_gate
                .clone()
                .acquire_owned()
                .await
                .expect("the live publication gate stays open");
            if state.stream.is_none() && state.retry_attempt > 0 {
                tokio::time::sleep(retry_delay(state.retry_attempt)).await;
            }
            // A MODULE REGISTERED AFTER THIS STREAM OPENED HAS NO TOPIC ON IT.
            // The block check reads the registry every block; once it lists an
            // id this stream never asked for, the stream is reopened with it,
            // resuming every held cursor.
            let registry = crate::module_view::registered_module_ids();
            let registry_grew = registry
                .iter()
                .any(|id| !state.registry_subscribed.contains(id));
            if state.stream.is_some() && registry_grew {
                state.stream = None;
            }
            if state.stream.is_none() {
                let connected = async {
                    let rpc = rpc_client(&state.rpc)?;
                    // THE PLANES THIS CONSOLE DRAWS. The first four fold; the
                    // rest reload the one plane they name (see `folded_update`).
                    //
                    // A module a node does not index no longer takes the others
                    // down — it comes back as `Refused` and that plane alone
                    // stays cold (`ModuleEvent::Refused`). `bin/noded` indexes
                    // no `valset` and no `governance`, so this list is only
                    // safe at all because of that.
                    rpc.module_events(subscribed_planes(&registry), state.cursors.clone())
                        .await
                        .map_err(Into::into)
                }
                .await;
                match connected {
                    Ok(stream) => {
                        state.stream = Some(stream);
                        state.registry_subscribed = registry;
                    }
                    Err(error) => {
                        state.retry_attempt = state.retry_attempt.saturating_add(1);
                        let mut update = live_retry(error);
                        update.permit = LivePermit::held(publication_permit);
                        return Some((update, state));
                    }
                }
            }
            let mut skipped_ready_frames = 0usize;
            loop {
                let event = match state.pending.take() {
                    Some(PendingLiveEvent::Event(event)) => Some(event),
                    Some(PendingLiveEvent::Closed) => None,
                    Some(PendingLiveEvent::Update(update)) => {
                        let mut update = *update;
                        update.permit = LivePermit::held(publication_permit);
                        return Some((update, state));
                    }
                    None => {
                        state
                            .stream
                            .as_mut()
                            .expect("stream initialized above")
                            .next()
                            .await
                    }
                };
                let mut update = match event {
                    Some(Ok(ModuleEvent::Ready { cursors })) => {
                        state.cursors = cursors;
                        state.retry_attempt = 0;
                        live_update(crate::LiveKind::Ready, "Live", -1)
                    }
                    Some(Ok(ModuleEvent::Changed { module, cursor, op })) => {
                        state.cursors.insert(format!("module:{module}"), cursor);
                        match folded_update(&state.rpc, &module, *op).await {
                            Some(update) => update,
                            // invisible to the UI (hook registration) — keep
                            // draining without emitting.
                            None => {
                                skipped_ready_frames += 1;
                                let exhausted_fairness_budget =
                                    skipped_ready_frames >= LIVE_CHAT_BATCH_LIMIT;
                                if exhausted_fairness_budget {
                                    tokio::task::yield_now().await;
                                    skipped_ready_frames = 0;
                                }
                                continue;
                            }
                        }
                    }
                    // THIS PLANE IS DEAD FOR THIS CONNECTION; THE OTHERS ARE
                    // NOT. Keep draining — the whole point is that a module
                    // this node does not index no longer takes chat and pages
                    // down with it.
                    //
                    // NOT surfaced, and saying so rather than pretending: the
                    // refusal arrives just before `ready`, and `live_updated`
                    // assigns `status` as its first statement, so any message
                    // put here is overwritten microseconds later. Showing it
                    // needs a per-plane field that no surface reads yet.
                    Some(Ok(ModuleEvent::Refused { .. })) => {
                        skipped_ready_frames += 1;
                        let exhausted_fairness_budget =
                            skipped_ready_frames >= LIVE_CHAT_BATCH_LIMIT;
                        if exhausted_fairness_budget {
                            tokio::task::yield_now().await;
                            skipped_ready_frames = 0;
                        }
                        continue;
                    }
                    Some(Ok(ModuleEvent::Lagged { module, cursor })) => {
                        state.cursors.insert(format!("module:{module}"), cursor);
                        live_resync(&module, -1)
                    }
                    // THE HEAD MOVES ON BLOCKS, NOT ON OPS. Height used to come
                    // only from a folded op, so a chain whose four subscribed
                    // modules were quiet left the console reading a frozen
                    // block number — on an idle chain, forever. The node has
                    // been sending this every block the whole time (the
                    // heartbeat rides the block wake, nop fillers included);
                    // the client threw it away by declaring the frame a unit
                    // variant, so the height never survived deserialization.
                    //
                    // It carries no cursor: a heartbeat is not a topic and
                    // resuming does not replay one. And it triggers NO load —
                    // see `ModuleEvent::Tip`.
                    //
                    // No de-duplication here, deliberately: the handler stops a
                    // tip immediately after the head assignment
                    // in the live-update handler, so a repeated height costs two
                    // scalar writes and no fold. Suppressing it would buy that
                    // back at the price of carrying a last-height in this state,
                    // and the fold path — the part that actually cost something
                    // — is already unreachable.
                    Some(Ok(ModuleEvent::Tip { height })) => {
                        // a block may have activated a module's code: the
                        // module-owned views check their deployments
                        tokio::spawn(crate::module_view::deployments_checked());
                        live_update(
                            crate::LiveKind::Tip,
                            &format!("Live · block {height}"),
                            i64::try_from(height).unwrap_or(i64::MAX),
                        )
                    }
                    Some(Err(error)) => {
                        state.stream = None;
                        state.retry_attempt = state.retry_attempt.saturating_add(1);
                        live_retry(error.into())
                    }
                    None => {
                        state.stream = None;
                        state.retry_attempt = state.retry_attempt.saturating_add(1);
                        live_retry("RPC stream closed".into())
                    }
                };
                if update.kind == crate::LiveKind::Chat {
                    update = collect_ready_chat_updates(&mut state, update).await;
                }
                update.permit = LivePermit::held(publication_permit);
                return Some((update, state));
            }
        },
    )
    .boxed()
}

/// Greedily take only chat frames that are ready *now*. The first frame of any
/// other kind is parked for the next unfold, so a pages/forge/tip/error frame
/// cannot be overtaken by later chat traffic. Dropping a pending `next()`
/// future is safe: it owns no frame and the boxed stream retains its socket.
async fn collect_ready_chat_updates(
    state: &mut LiveEventState,
    mut batch: LiveUpdate,
) -> LiveUpdate {
    // Count consumed CHAT FRAMES, not only visible deltas. Hook registration
    // folds to `None`; without this separate budget an always-ready run of
    // invisible chat frames could monopolise this one stream poll forever.
    let mut consumed = batch.chat.len();
    while consumed < LIVE_CHAT_BATCH_LIMIT {
        let ready = state
            .stream
            .as_mut()
            .expect("a chat update came from an initialized stream")
            .next()
            .now_or_never();
        let Some(event) = ready else {
            break;
        };
        let Some(event) = event else {
            state.pending = Some(PendingLiveEvent::Closed);
            break;
        };
        let (module, cursor, op) = match event {
            Ok(ModuleEvent::Changed { module, cursor, op }) => (module, cursor, op),
            other => {
                state.pending = Some(PendingLiveEvent::Event(other));
                break;
            }
        };
        let is_chat = module == "chat";
        if !is_chat {
            state.pending = Some(PendingLiveEvent::Event(Ok(ModuleEvent::Changed {
                module,
                cursor,
                op,
            })));
            break;
        }
        consumed += 1;
        state.cursors.insert("module:chat".into(), cursor);
        let Some(update) = folded_update(&state.rpc, "chat", *op).await else {
            continue;
        };
        if let Some(update) = merge_live_chat_batch(&mut batch, update) {
            state.pending = Some(PendingLiveEvent::Update(Box::new(update)));
            break;
        }
    }
    batch
}

/// The complete chat-owned result of one live batch. Each list is folded
/// once before the app assigns the result fields; no delta in
/// the batch can wander through Pages, Bell, or Forge lifecycle reducers.
/// THE CHAT TAB'S TIMELINE IS NOT IN HERE. The Chat tab is a module-owned
/// view on the kernel contract: it re-reads its own room on the same block
/// this fold runs for. The app retains channel facts for navigation and the roster
/// used by native call flows.
/// Sidebar rows and their unread presentation belong to the deployed view.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatLiveFold {
    pub channels: Vec<ChatChannel>,
    pub active_channel_name: String,
    pub active_channel_archived: bool,
    /// A huddle roster change in the active channel needs the canonical roster
    /// read that a delta cannot derive.
    pub refresh_chat: bool,
}

/// Changes to the shell's retained rows, projected by the deployed Chat view.
#[derive(Clone, Debug, Hash, PartialEq, serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ChatDelta {
    Channel { channel: ChatChannel },
    Head { channel_id: String, seq: i64 },
}

struct ChatFoldState {
    channels: Vec<ChatChannel>,
    active_channel: String,
    refresh_chat: bool,
}

fn fold_head(state: &mut ChatFoldState, channel_id: String, seq: i64) {
    state.channels =
        chat::client::advance_channel_head(std::mem::take(&mut state.channels), &channel_id, seq);
}

fn fold_channel(state: &mut ChatFoldState, channel: ChatChannel) {
    state.refresh_chat |= channel.id == state.active_channel;
    let id = channel.id.clone();
    state.channels =
        chat::client::replace_channel(std::mem::take(&mut state.channels), &id, channel);
}

/// Fold one ordered live chat batch in one Rust ownership domain. Lists move
/// into this function once, then each delta mutates those owned lists in
/// sequence, without cloning the whole timeline for every operation.
pub fn fold_live_chat(
    deltas: Vec<ChatDelta>,
    channels: Vec<ChatChannel>,
    active_channel: String,
    mut active_channel_name: String,
    mut active_channel_archived: bool,
) -> ChatLiveFold {
    let mut state = ChatFoldState {
        channels,
        active_channel,
        refresh_chat: false,
    };
    for delta in deltas {
        match delta {
            ChatDelta::Channel { channel } => fold_channel(&mut state, channel),
            ChatDelta::Head { channel_id, seq } => fold_head(&mut state, channel_id, seq),
        }
    }

    let ChatFoldState {
        channels,
        active_channel,
        refresh_chat,
        ..
    } = state;

    if let Some(channel) = channels.iter().find(|channel| channel.id == active_channel) {
        active_channel_name.clone_from(&channel.name);
        active_channel_archived = channel.archived;
    }
    ChatLiveFold {
        channels,
        active_channel_name,
        active_channel_archived,
        refresh_chat,
    }
}

/// Fold one applied op into a live update. A decode failure (payload or
/// stamp) degrades to a scoped resync of that module — a CLIENT reloads,
/// never wedges. `None` = the op is invisible to this UI.
pub(crate) async fn folded_update(
    rpc: &str,
    module: &str,
    op: ducktape_rpc::StreamOp,
) -> Option<LiveUpdate> {
    let height = i64::try_from(op.height).unwrap_or(i64::MAX);
    // WHAT THE STREAM DELIVERED, THE RELOADS BEHIND IT MUST NOT PREDATE.
    //
    // A push reports APPLICATION, which is the acceptance gap closed and the
    // FOLD gap still open: the node's block loop writes the op feed and the
    // index folds behind it on its own runner (only the sim's
    // `wait_folds_drained` ever joins the two). A reload prompted by this push
    // reads the folded view — so without this it installs a tree that predates
    // the very op that prompted it, with no further op coming to correct it.
    // The kernel's `rpc.view` waits on the same record, which is what gives a
    // module view reading its own plane the same guarantee.
    //
    // Recorded HERE, once, rather than carried down through the update and
    // the handler's debounce: this is where the height is learned, and every
    // read that must not answer behind it waits on the record
    // (`rpc.rs`, `SEEN_BLOCKS`).
    if let Ok(client) = rpc_client(rpc) {
        note_module_block(&client, module, op.height);
    }
    let Some(payload) = op
        .payload
        .as_ref()
        .and_then(|value| serde_json::to_vec(value).ok())
    else {
        return Some(live_resync(module, height));
    };
    match module {
        "chat" => {
            let facts = ReaderFacts::current().await;
            notify_chat_op(rpc, &payload, op.assigned.as_ref());
            let key = facts.reader().key.map(hex_encode).unwrap_or_default();
            let projected = chat_background(
                rpc,
                serde_json::json!({
                    "kind":"shell_delta", "payload":op.payload, "assigned":op.assigned,
                    "key":key, "names":facts.names()
                }),
            )
            .await;
            let delta = match projected {
                Ok(value) => serde_json::from_value::<Option<ChatDelta>>(value["delta"].clone()),
                Err(_) => return Some(live_resync("chat", height)),
            };
            let delta = match delta {
                Ok(delta) => delta,
                Err(_) => return Some(live_resync("chat", height)),
            };
            Some(LiveUpdate {
                kind: crate::LiveKind::Chat,
                status: format!("Live · block {height}"),
                height,
                module: "chat".into(),
                load_chat: false,
                debounce: false,
                chat: delta.into_iter().collect(),
                bell: BellDelta::default(),
                permit: LivePermit::default(),
            })
        }
        "inbox" => {
            // Stream folds use the cached identity directory, with no RPC read.
            let facts = ReaderFacts::current().await;
            let key = facts.reader().key?;
            let account = facts.names().account_of(&hex_encode(key))?;
            let origin_kind = stream_origin_kind(&op.origin.kind);
            let folded = inbox::client::delta_from_op(
                &payload,
                op.assigned.as_ref(),
                origin_kind,
                op.origin.id.as_deref(),
                account,
                "attribution",
            );
            match folded {
                Ok(Some(bell)) => Some(LiveUpdate {
                    kind: crate::LiveKind::Bell,
                    status: format!("Live · block {height}"),
                    height,
                    module: "inbox".into(),
                    load_chat: false,
                    debounce: false,
                    chat: Vec::new(),
                    bell,
                    permit: LivePermit::default(),
                }),
                Ok(None) => None,
                Err(_) => None,
            }
        }
        // THE RELOAD PLANES. No client fold exists for these modules and none
        // is worth writing: a validator set changes when someone joins, a
        // proposal when someone votes, an account when someone renames a
        // device. Human-rate, all of them — so the op is a signal that ONE
        // plane is stale, and the handler refetches exactly that one.
        //
        // Reading rather than folding costs a checkpoint-gated query
        // (`connect`), which is why this is not the answer for chat or pages.
        // At these rates it is the right trade: no fold to keep correct, and
        // nothing at all on a block that does not touch them.
        //
        // Model configuration and run activity both refresh the Agents view.
        // `pages` and `forge` are here because the app holds no state of
        // either any more: the plane arm is what tells a module view's
        // `rpc.live` subscription, and each view re-reads exactly what it
        // has open.
        // and so does a registry-listed module (the only other id the lane
        // subscribes): its registered view re-reads its own plane.
        _ => Some(live_plane(module, height)),
    }
}

fn stream_origin_kind(kind: &ducktape_rpc::StreamOriginKind) -> &'static str {
    match kind {
        ducktape_rpc::StreamOriginKind::External => "external",
        ducktape_rpc::StreamOriginKind::Program => "program",
        ducktape_rpc::StreamOriginKind::Module => "module",
        ducktape_rpc::StreamOriginKind::System => "system",
    }
}

/// One channel's row rebuilt from the index view — the huddle roster length is
/// not derivable from the op, so the row still has to be read.
///
/// THE VIEW LANE, NOT `/v1/query`. This is awaited inside the live stream's
/// decoder fold, so a `/v1/query` here freezes every subscriber's fold for as
/// long as the node's select loop is busy writing a checkpoint (issue #1018).
/// One scoped catch-up load of the chat slices: the channel list, the active
/// window and its members. Runs on stream `ready` (the subscribe→hydrate
/// ordering race) and on a `resync` (lag or an unfoldable op), never per chat
/// commit. A resync for a plane this app folds nothing for comes back with
/// `chat_loaded` false and the handler keeps current state.
#[derive(Clone, Debug, Hash, PartialEq)]
pub struct LiveRefresh {
    pub generation: i64,
    pub chat_loaded: bool,
    pub channels: Vec<ChatChannel>,
    pub active_channel: String,
    pub active_channel_name: String,
    pub active_channel_archived: bool,
    pub huddle_roster: Vec<HuddleParticipant>,
}

/// One scoped catch-up load of the chat slices. `load_chat` false is the
/// resync of a plane this app folds nothing for — it answers with
/// `chat_loaded` false and the handler keeps what it has.
pub async fn live_resync_load(
    rpc: String,
    channel_id: String,
    load_chat: bool,
    debounce: bool,
    generation: i64,
    attempt: i64,
) -> Result<LiveRefresh, HydrationError> {
    if debounce {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if attempt > 0 {
        tokio::time::sleep(retry_delay(u32::try_from(attempt).unwrap_or(u32::MAX))).await;
    }
    async {
        let rpc = rpc_client(&rpc)?;
        let mut refresh = LiveRefresh {
            generation,
            chat_loaded: false,
            channels: Vec::new(),
            active_channel: String::new(),
            active_channel_name: String::new(),
            active_channel_archived: false,
            huddle_roster: Vec::new(),
        };
        if !load_chat {
            return Ok(refresh);
        }
        let chat =
            load_chat_data(&rpc, (!channel_id.is_empty()).then_some(channel_id.as_str())).await?;
        refresh.chat_loaded = true;
        refresh.channels = chat.channels;
        refresh.active_channel = chat.active_channel;
        refresh.active_channel_name = chat.active_channel_name;
        refresh.active_channel_archived = chat.active_channel_archived;
        refresh.huddle_roster = chat.huddle_roster;
        Ok(refresh)
    }
    .await
    .map_err(|message: String| HydrationError {
        generation,
        message: user_error(message),
    })
}

/// Did this live update say `want`'s plane went stale?
pub fn plane_live_hit(kind: crate::LiveKind, module: String, want: String) -> bool {
    kind == crate::LiveKind::Plane && module == want
}

// per-field keepers: apply a refreshed value only when its plane loaded —
// unchanged planes retain their current values.

/// The channel keeper folds rather than replaces — [`upsert_channel_rows`]
/// states why — and it owns the loaded pick so the fold is never paid for on a
/// plane-only resync, which is most of them. Written as an argument
/// (`keep_channels(loaded, upsert_channel_rows(channels, next), channels)`) the
/// upsert ran on every pages-only refresh and was thrown away one call later.
/// Same early-return shape as [`resynced_messages`] below.
///
/// EXCEPT ACROSS A NETWORK. `chain_moved` says the list on screen was learned
/// from a chain this node is no longer on — a workspace switch under a console
/// that never reconnected, because the endpoint did not change — and a fold has
/// no way to express "that room does not exist here": it only ever adds. So the
/// one thing that can be true of the previous network's rooms is that they are
/// gone, and the answer replaces the list outright.
pub fn keep_channels(
    loaded: bool,
    chain_moved: bool,
    next: Vec<ChatChannel>,
    current: Vec<ChatChannel>,
) -> Vec<ChatChannel> {
    if !loaded {
        return current;
    }
    if chain_moved {
        return next;
    }
    upsert_channel_rows(current, next)
}

/// Everything a chat load says about the huddle — the one rule, in one place,
/// for the folds that used to spell it out in four lines each.
///
/// A LOAD CARRIES THE ROSTER OF THE CHANNEL IT LOADED, AND THAT IS NOT ALWAYS
/// THE HUDDLE'S. The huddle window follows you onto every other room and
/// every other screen — that is what it is FOR — so reading
/// "am I in a huddle" off the room you happen to be looking at answered no the
/// moment you clicked a second channel. And that answer is not cosmetic:
/// `huddle_joined` is the media leg's subscription gate, so a channel click cut
/// the audio and video of the call you were in, closed the window, and blanked
/// the `huddle_channel` that `leave_huddle_here` needs — leaving you on the
/// on-chain roster with no control left that could take you off it.
///
/// So: while joined, a load of ANY OTHER channel says nothing about the huddle
/// and changes nothing about it. A load of the huddle's own channel (or any
/// load at all while not joined) answers in full, and a resync that carried no
/// chat at all (`loaded == false`) answers not at all.
#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct HuddleAfterLoad {
    pub joined: bool,
    pub roster: Vec<HuddleParticipant>,
    pub channel: String,
    pub channel_name: String,
}

// Eight, because the rule compares two whole huddles — the standing one and
// the one the load carries. Folding either half into a struct only moves the
// four names to the call site, where five folds would each build it by hand.
#[allow(clippy::too_many_arguments)]
pub fn huddle_after_load(
    loaded: bool,
    joined: bool,
    channel: String,
    channel_name: String,
    roster: Vec<HuddleParticipant>,
    loaded_channel: String,
    loaded_channel_name: String,
    loaded_roster: Vec<HuddleParticipant>,
) -> HuddleAfterLoad {
    let standing = HuddleAfterLoad {
        joined,
        roster,
        channel,
        channel_name,
    };
    let speaks_for_the_huddle = loaded && (!joined || loaded_channel == standing.channel);
    if !speaks_for_the_huddle {
        return standing;
    }
    let joined_now = huddle_self(loaded_roster.clone());
    if !joined_now {
        return HuddleAfterLoad::default();
    }
    HuddleAfterLoad {
        joined: true,
        roster: loaded_roster,
        channel: loaded_channel,
        channel_name: loaded_channel_name,
    }
}

pub fn keep_str(loaded: bool, next: &str, current: &str) -> String {
    if loaded { next } else { current }.to_owned()
}

pub fn keep_bool(loaded: bool, next: bool, current: bool) -> bool {
    if loaded { next } else { current }
}

pub fn keep_i64(loaded: bool, next: i64, current: i64) -> i64 {
    if loaded { next } else { current }
}

/// The channel the reader just clicked, without cloning or re-paging the
/// channel list. Its timeline is one root-index page; no head hint is needed.
///
/// A GENERATION, NOT AN `AppError` — the same reason [`connect`] fails with
/// one. Nothing serializes these any more (`choose_channel` takes every click
/// and drops the superseded REPLY), so a failure has to be able to say which
/// switch it belongs to: without that, B erroring after the reader has clicked
/// on to C clears `loading` under C, swapping C's plate for "No messages yet",
/// and writes B's error into the banner. `committed` is what `AppError` adds,
/// and a room switch has nothing to commit.
pub async fn load_channel_window(
    rpc: String,
    channel_id: String,
    generation: i64,
) -> Result<ChatData, HydrationError> {
    async {
        let rpc = rpc_client(&rpc)?;
        let mut chat = load_channel_window_data(&rpc, &channel_id).await?;
        chat.generation = generation;
        Ok(chat)
    }
    .await
    .map_err(|message: String| HydrationError {
        generation,
        message: user_error(message),
    })
}

/// Refresh the native readers' shared identity directory.
pub async fn refresh_name_directory(rpc: String, generation: i64) -> Result<i64, HydrationError> {
    async {
        let client = rpc_client(&rpc)?;
        read_accounts(&client).await?;
        Ok(generation)
    }
    .await
    .map_err(|message: String| HydrationError {
        generation,
        message: user_error(message),
    })
}
