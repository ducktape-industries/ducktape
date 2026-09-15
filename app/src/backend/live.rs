use super::*;
use ::chat;
use identity::{AccountView, IdentityQuery, IdentityReply};

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
/// this fold runs for. What the app still folds is what OTHER screens read off
/// the same deltas — the channel list and read cursors the sidebar, the bell
/// and the tray paint from, the active room's roster the composers complete
/// mentions against.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatLiveFold {
    pub channels: Vec<ChatChannel>,
    pub channel_members: Vec<ChatMember>,
    pub channel_reads: Vec<ChannelRead>,
    pub rooms: Vec<ChatSidebarRow>,
    pub dm_rows: Vec<DmSidebarRow>,
    pub active_channel_name: String,
    pub active_channel_archived: bool,
    pub active_channel_members_only: bool,
    pub post_refusal: String,
    /// A huddle roster change in the active channel needs the canonical roster
    /// read that a delta cannot derive.
    pub refresh_chat: bool,
}

struct ChatFoldState {
    channels: Vec<ChatChannel>,
    channel_members: Vec<ChatMember>,
    active_channel: String,
    refresh_chat: bool,
}

fn fold_channel_created(state: &mut ChatFoldState, channel: ChatChannel) {
    state.channels = chat::client::insert_channel(std::mem::take(&mut state.channels), channel);
}

fn fold_channel_renamed(state: &mut ChatFoldState, channel_id: String, name: String) {
    state.channels =
        chat::client::rename_channel(std::mem::take(&mut state.channels), &channel_id, name);
}

fn fold_channel_archived(state: &mut ChatFoldState, channel_id: String, archived: bool) {
    state.channels =
        chat::client::archive_channel(std::mem::take(&mut state.channels), &channel_id, archived);
}

/// A post moves the room's head, which is the whole of what the app reads off
/// a message now: the unread mark, the sidebar order and the bell all count
/// heads. The MESSAGE is the chat view's, and it re-reads the room on the same
/// block this fold runs for.
fn fold_posted(state: &mut ChatFoldState, channel_id: String, seq: i64) {
    state.channels =
        chat::client::advance_channel_head(std::mem::take(&mut state.channels), &channel_id, seq);
}

fn fold_membership(state: &mut ChatFoldState, channel_id: String, added: bool, member: ChatMember) {
    let updates_active_members = channel_id == state.active_channel;
    if updates_active_members {
        state.channel_members = chat::client::apply_membership(
            std::mem::take(&mut state.channel_members),
            added,
            member,
        );
    }
}

fn fold_channel_refresh(state: &mut ChatFoldState, channel_id: String) {
    state.refresh_chat |= channel_id == state.active_channel;
}

fn fold_channel_updated(state: &mut ChatFoldState, channel_id: String, channel: ChatChannel) {
    state.refresh_chat |= channel_id == state.active_channel;
    state.channels =
        chat::client::replace_channel(std::mem::take(&mut state.channels), &channel_id, channel);
}

/// Fold one ordered live chat batch in one Rust ownership domain. Lists move
/// into this function once, then each delta mutates those owned lists in
/// sequence, without cloning the whole timeline for every operation.
#[allow(clippy::too_many_arguments)]
pub fn fold_live_chat(
    deltas: Vec<ChatDelta>,
    channels: Vec<ChatChannel>,
    channel_members: Vec<ChatMember>,
    mut channel_reads: Vec<ChannelRead>,
    dm_peers: Vec<DmPeer>,
    me: String,
    active_channel: String,
    history_view: bool,
    chat_visible: bool,
    mut active_channel_name: String,
    mut active_channel_archived: bool,
    mut active_channel_members_only: bool,
) -> ChatLiveFold {
    let mut state = ChatFoldState {
        channels,
        channel_members,
        active_channel,
        refresh_chat: false,
    };
    for delta in deltas {
        match delta {
            ChatDelta::ChannelCreated { channel } => fold_channel_created(&mut state, channel),
            ChatDelta::ChannelRenamed { channel_id, name } => {
                fold_channel_renamed(&mut state, channel_id, name)
            }
            ChatDelta::ChannelArchived {
                channel_id,
                archived,
            } => fold_channel_archived(&mut state, channel_id, archived),
            ChatDelta::Posted {
                channel_id,
                seq,
                message: _,
            } => fold_posted(&mut state, channel_id, seq),
            ChatDelta::Reply {
                channel_id,
                seq,
                root_seq: _,
                message: _,
            } => fold_posted(&mut state, channel_id, seq),
            // A ROW CHANGING IN PLACE MOVES NOTHING THE APP HOLDS. An edit, a
            // delete and a reaction all leave the room's head where it was, and
            // the only reader of a message body is the chat view — which reads
            // its own room on this very block. Routed and dropped, so a new
            // delta still has to be named here.
            ChatDelta::Edited { .. } | ChatDelta::Deleted { .. } | ChatDelta::Reaction { .. } => {}
            ChatDelta::Membership {
                channel_id,
                added,
                member,
            } => fold_membership(&mut state, channel_id, added, member),
            ChatDelta::ChannelRefresh { channel_id } => {
                fold_channel_refresh(&mut state, channel_id)
            }
            ChatDelta::ChannelUpdated {
                channel_id,
                channel,
            } => fold_channel_updated(&mut state, channel_id, channel),
        }
    }
    let ChatFoldState {
        channels,
        channel_members,
        active_channel,
        refresh_chat,
        ..
    } = state;
    let reads_live_tail = !history_view && chat_visible;

    if let Some(channel) = channels.iter().find(|channel| channel.id == active_channel) {
        active_channel_name.clone_from(&channel.name);
        active_channel_archived = channel.archived;
        active_channel_members_only = channel.members_only;
    }
    let seated = seated_in(&channel_members, &me);
    let post_refusal = if active_channel_archived {
        "channel_archived".into()
    } else if active_channel_members_only && !seated {
        "members_only".into()
    } else {
        String::new()
    };

    if reads_live_tail {
        let head = channels
            .iter()
            .find(|channel| channel.id == active_channel)
            .map_or(0, |channel| channel.head_seq);
        match channel_reads
            .iter_mut()
            .find(|read| read.channel == active_channel)
        {
            Some(read) => read.seq = read.seq.max(head),
            None => channel_reads.push(ChannelRead {
                channel: active_channel.clone(),
                seq: head,
            }),
        }
    }
    let rooms = chat_sidebar_rooms(channels.clone(), dm_peers.clone(), channel_reads.clone());
    let dm_rows = chat_sidebar_dms(channels.clone(), dm_peers, channel_reads.clone());
    ChatLiveFold {
        channels,
        channel_members,
        channel_reads,
        rooms,
        dm_rows,
        active_channel_name,
        active_channel_archived,
        active_channel_members_only,
        post_refusal,
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
            // THE DIRECTORY AS LAST READ, NOT A FRESH READ: this is inside the
            // live decoder fold, where a query would freeze every subscriber
            // for as long as the node's select loop is busy (issue #1018). It
            // is warm by the connect that opened this stream.
            let facts = ReaderFacts::current().await;
            let origin_kind = stream_origin_kind(&op.origin.kind);
            // THE ONE ARRIVAL A READER CANNOT AFFORD TO FIND LATER. A mention
            // and a DM used to reach nothing but the in-app bell, which is
            // worth nothing behind another window. Pure decision, cached
            // reads, no query — see `notify`.
            notify_chat_op(&payload, op.assigned.as_ref(), facts.names());
            let folded = chat::client::delta_from_op(
                &payload,
                op.assigned.as_ref(),
                origin_kind,
                op.origin.id.as_deref(),
                facts.reader(),
                op.height,
            );
            let delta = match folded {
                Ok(Some(delta)) => delta,
                Ok(None) => return None,
                Err(_) => return Some(live_resync("chat", height)),
            };
            // huddle membership is roster-derived — reload the one channel
            // row from its canonical record instead of guessing the count.
            let delta = match delta {
                ChatDelta::ChannelRefresh { channel_id } => {
                    // Named through the same cached directory as the fold:
                    // the seats under the room are the reader's own "you"
                    // and their peers' names, not bare account numbers.
                    let channel = match load_channel_row(rpc, &channel_id, facts.reader()).await
                    {
                        Ok(Some(channel)) => channel,
                        Ok(None) | Err(_) => return Some(live_resync("chat", height)),
                    };
                    tracing::debug!(
                        target: "ducktape::live",
                        channel = %channel_id,
                        seats = channel.huddle.len(),
                        height,
                        "chat.channel_refresh"
                    );
                    let a_join = matches!(
                        chat::decode_msg(&payload),
                        Ok(ChatMsg::JoinHuddle { .. })
                    );
                    if a_join {
                        notify_huddle_started(&channel);
                    }
                    ChatDelta::ChannelUpdated {
                        channel_id,
                        channel,
                    }
                }
                ready => ready,
            };
            Some(LiveUpdate {
                kind: crate::LiveKind::Chat,
                status: format!("Live · block {height}"),
                height,
                module: "chat".into(),
                load_chat: false,
                debounce: false,
                chat: vec![delta],
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
/// `ChatViewQuery::Channel` reads the same `ChannelInfo` off an MVCC snapshot,
/// off-loop — identical payload, no checkpoint tax.
///
/// An unseen row is `None`, and the caller turns it into a scoped resync rather
/// than a banner: the op named a channel this node's index cannot answer for
/// yet, and a reload is the only thing that heals that.
pub(crate) async fn load_channel_row(
    rpc: &str,
    channel_id: &str,
    reader: ChatReader<'_>,
) -> Result<Option<ChatChannel>, String> {
    let rpc = rpc_client(rpc)?;
    let room = load_channel_facts(&rpc, channel_id, reader).await?;
    Ok(room.map(|(channel, _roster)| channel))
}

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
    pub active_channel_members_only: bool,
    pub huddle_roster: Vec<HuddleParticipant>,
    pub channel_members: Vec<ChatMember>,
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
            active_channel_members_only: false,
            huddle_roster: Vec::new(),
            channel_members: Vec::new(),
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
        refresh.active_channel_members_only = chat.active_channel_members_only;
        refresh.huddle_roster = chat.huddle_roster;
        refresh.channel_members = chat.channel_members;
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

pub fn keep_members(
    loaded: bool,
    next: Vec<ChatMember>,
    current: Vec<ChatMember>,
) -> Vec<ChatMember> {
    if loaded { next } else { current }
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

/// Land on the channel that was just made.
///
/// `submit_frame` returns when the node ACCEPTS a transaction, not when it
/// applies one, so a reload issued immediately after can read an index that
/// does not hold the new channel yet. `load_chat_data` is RIGHT to drop a
/// requested id it cannot see and fall back to `channels.first()` — a live
/// refresh can legitimately name a channel that has since been archived away —
/// so the correction belongs in the callers that know their id is good.
///
/// The fallback is not cosmetic here. Opening a DM left the DM header on
/// screen while `active_channel` pointed at whatever unrelated public room
/// sorted first, so the composer under that header posted into it.
///
/// `members` is seeded rather than left for the refresh because `post_gate`
/// refuses a members-only channel the viewer is not seated in: an empty list
/// would gate the composer of the DM you just opened.
fn landed_on_channel(
    mut data: ChatData,
    channel_id: String,
    name: String,
    members_only: bool,
    members: Vec<ChatMember>,
) -> ChatData {
    if data.active_channel == channel_id {
        return data;
    }
    data.active_channel = channel_id;
    data.active_channel_name = name;
    data.active_channel_archived = false;
    data.active_channel_members_only = members_only;
    data.huddle_roster = Vec::new();
    data.channel_members = members;
    data
}

pub async fn create_channel(
    rpc: String,
    password: String,
    name: String,
    members_only: bool,
    voice: bool,
    generation: i64,
) -> Result<ChatData, AppError> {
    async {
        let name = bounded_text(name, "channel name", 128)?;
        let landing_name = name.clone();
        let channel_id = fresh_id("channel");
        let rpc = rpc_client(&rpc)?;
        let op = match voice {
            true => ChatMsg::CreateVoiceChannel {
                channel_id: channel_id.clone(),
                name,
            },
            false => ChatMsg::CreateChannel {
                channel_id: channel_id.clone(),
                name,
                post_policy: match members_only {
                    true => PostPolicy::MembersOnly,
                    false => PostPolicy::Open,
                },
            },
        };
        signed_write(&rpc, "chat", chat::encode_msg(&op), password).await?;
        // a voice room is entered, not read: the room on screen stays
        if voice {
            let mut data = load_chat_data(&rpc, None).await.map_err(committed_error)?;
            data.generation = generation;
            return Ok(data);
        }
        let data = load_chat_data(&rpc, Some(&channel_id))
            .await
            .map_err(committed_error)?;
        let mut data = landed_on_channel(data, channel_id, landing_name, members_only, Vec::new());
        data.generation = generation;
        Ok(data)
    }
    .await
}

/// One peer of the DM directory. There is no `status`: presence has no source
/// anywhere in the product, and a dot that always reads "offline" is a lie.
///
/// `is_agent` identifies a keyless program account from its control record.
///
/// `channel_id` is the pair's deterministic two-party channel id
/// (`dm_channel_id(me, key)`), computed once at load time rather than at
/// every render. The prepared DIRECT projection uses it to attach the row's
/// scalar unread reading when channels or read cursors move.
#[derive(Clone, Debug, Default, Hash, PartialEq, serde::Serialize)]
pub struct DmPeer {
    pub key: String,
    pub name: String,
    pub initials: String,
    pub is_agent: bool,
    pub channel_id: String,
}

#[derive(Clone, Debug, Hash, PartialEq)]
pub struct DmPeersData {
    pub generation: i64,
    pub peers: Vec<DmPeer>,
}

/// One DM peer per identity account, including keyless program accounts.
/// The row and deterministic channel are keyed by account number, so a new
/// device on either account reaches the same room.
pub async fn load_dm_peers(rpc: String, generation: i64) -> Result<DmPeersData, HydrationError> {
    async {
        let client = rpc_client(&rpc)?;
        let me = local_user_key().await;
        // The same read that refreshes the name directory: an identity op
        // reloads this directory, and every label on screen moves with it.
        let accounts = read_accounts(&client).await?;
        // self is the account THIS key is a member of (a key holds at most one).
        let is_mine = |account: &AccountView| {
            me.as_ref()
                .is_some_and(|me| account.keys.iter().any(|key| &key.pubkey == me))
        };
        let my_number = accounts
            .iter()
            .find(|account| is_mine(account))
            .map(|account| account.number.to_string());
        let mut peers: Vec<DmPeer> = Vec::new();
        for account in accounts {
            if is_mine(&account) {
                continue;
            }
            let key = account.number.to_string();
            let is_agent = matches!(
                account.control,
                identity::Control::Program { .. } | identity::Control::Revoked { .. }
            );
            let name = account.name;
            let channel_id = my_number
                .as_ref()
                .map(|mine| dm_channel_id(mine.clone(), key.clone()))
                .unwrap_or_default();
            peers.push(DmPeer {
                initials: initials_of(&name),
                is_agent,
                key,
                name,
                channel_id,
            });
        }
        // THE DM ROOMS RIDE THIS LOAD TOO, for the reason the directory does:
        // the live decoder cannot ask which rooms are mine without a query
        // inside the fold, and this load already derived every one of them.
        note_dm_rooms(&peers);
        Ok(DmPeersData { generation, peers })
    }
    .await
    .map_err(|message: String| HydrationError {
        generation,
        message: user_error(message),
    })
}

/// The two-party channel id of a pair of member keys, derived by the chat
/// module's own client so the app and the module agree on one id.
///
/// It carries NO ':' deliberately: chat refuses a user-authored channel id in
/// the module namespace (`crates/modules/apps/chat/src/lib.rs`,
/// `validate_channel_namespace`), and a DM is created by the pair's own user
/// key. `chat::client`'s test round-trips the minted id against that rule.
pub fn dm_channel_id(a: String, b: String) -> String {
    chat::client::dm_channel_id(&a, &b)
}

/// THE DM PEER IS A READING OF THE ROOM ON SCREEN, NOT A FLAG THAT OUTLIVES IT.
/// `peer` survives only while `channel` is that peer's own two-party channel;
/// every other room answers "".
///
/// `active_dm_peer` decides the whole header, one step removed: it resolves the
/// row (`dm_peer_named`) that the header actually branches on, so a peer this
/// key names draws `DmHeader` with his avatar and name and SUPPRESSES the `#`
/// glyph and `active_channel_name` — while a key whose peer has left the
/// identity roster resolves to the blank row and falls THROUGH to that title
/// in the Chat view, instead of drawing
/// a nameless avatar plate the way branching on the key itself did.
///
/// Cleared by the channel picker alone, it rode a search hit, a create, a
/// reconnect and every resync into another room — Alice's face over #general's
/// timeline, with the room the composer actually posts into never named. So
/// every landing that assigns `active_channel` from a reply re-derives the peer
/// through here, and the field cannot disagree with the room again.
///
/// THE DIRECTORY'S OWN ID DECIDES, for the reason `chat_sidebar_rooms` gives:
/// `DmPeer.channel_id` was derived once, in `load_dm_peers`, from the account
/// number that load resolved for itself. Re-hashing it here against a separate
/// `account_number` reading made the header disagree with the sidebar whenever
/// that reading was late or missing — the peer's own room drew as a `#` channel
/// under his name in DIRECT.
pub fn dm_peer_of_channel(peer: String, peers: Vec<DmPeer>, channel: String) -> String {
    let peer_owns_the_room = peers
        .iter()
        .any(|row| row.key == peer && !row.channel_id.is_empty() && row.channel_id == channel);
    match peer_owns_the_room {
        true => peer,
        false => String::new(),
    }
}

/// The room a DIRECT row opens — the id `load_dm_peers` derived for that peer,
/// empty when the directory does not name him (or names him with no account
/// number of ours to pair against).
pub fn dm_room_of_peer(peers: Vec<DmPeer>, peer: String) -> String {
    peers
        .into_iter()
        .find(|row| row.key == peer)
        .map(|row| row.channel_id)
        .unwrap_or_default()
}

/// THE DM HEADER'S OWN ROW, resolved where `active_dm_peer` is written.
///
/// The header used to be a filter — `for peer in dm_peers` / `if peer.key ==
/// active_dm_peer` — which the extern-free view can express but which
/// deep-clones every peer AND allocates a per-child scope String, per frame, so
/// that at most one of them renders. A peer who has left the identity roster
/// resolves to the blank row, and the header falls through to the `#` title the
/// way the filter's no-match arm did.
pub fn dm_peer_named(peers: Vec<DmPeer>, key: String) -> DmPeer {
    peers
        .into_iter()
        .find(|peer| peer.key == key)
        .unwrap_or_default()
}

/// The blank peer — "no DM on screen", and the state field's own default.
pub fn no_dm_peer() -> DmPeer {
    DmPeer::default()
}

/// Open the DM with one peer (an account number): resolve the deterministic
/// channel when it exists, else create it with the two accounts as members,
/// then load it. Account membership follows each account's current keys.
///
/// NOT confidential. `MembersOnly` gates who may POST; every node replicates
/// the channel's plaintext, so a DM is a two-person room, not a private one.
/// Any copy on this surface that promises secrecy is a lie about the wire.
///
/// Fails with a generation for the reason [`load_channel_window`] gives: this
/// is one of the three routes that move the reader between rooms, and a
/// superseded failure must not land under the room she is in now. The writes it
/// makes are idempotent by construction: `CreateDmChannel` derives the
/// deterministic channel id and seats both accounts in the same transaction.
pub async fn open_dm(
    rpc: String,
    password: String,
    peer_key: String,
    generation: i64,
) -> Result<ChatData, HydrationError> {
    async {
        let number: u64 = peer_key
            .trim()
            .parse()
            .map_err(|_| "peer must be an account number".to_string())?;
        let me = local_user_key()
            .await
            .ok_or_else(|| "this device has no user key — a DM needs one".to_string())?;
        let client = rpc_client(&rpc)?;
        let reply: IdentityReply = client
            .query("identity", &IdentityQuery::OfKey { key: me })
            .await?;
        let mine = match reply {
            IdentityReply::Account(account) => account,
            IdentityReply::Accounts(_) | IdentityReply::Resolved(_) | IdentityReply::Gen(_) => {
                return Err("the identity module returned the wrong reply".to_string());
            }
        };
        let mine = mine.ok_or_else(|| "this key is on no account — a DM needs one".to_string())?;
        let channel_id = dm_channel_id(mine.number.to_string(), number.to_string());
        let mut existing = load_chat_data(&client, Some(&channel_id)).await?;
        if existing.active_channel == channel_id {
            existing.generation = generation;
            return Ok(existing);
        }
        let reply: IdentityReply = client
            .query("identity", &IdentityQuery::Get { number })
            .await?;
        let account = match reply {
            IdentityReply::Account(account) => account,
            IdentityReply::Accounts(_) | IdentityReply::Resolved(_) | IdentityReply::Gen(_) => {
                return Err("the identity module returned the wrong reply".to_string());
            }
        };
        let account = account.ok_or_else(|| format!("account {number} does not exist"))?;
        let peer_name = account.name;
        // The DM op, not a plain `CreateChannel` naming a `dm-` id: the module
        // reserves that shape and refuses it from anything but this op, which
        // resolves the creator's own account and derives the very id computed
        // above (`dm_channel_id(mine, peer)`), members-only by construction.
        signed_write(
            &client,
            "chat",
            chat::encode_msg(&ChatMsg::CreateDmChannel {
                counterpart: number,
                name: peer_name.clone(),
            }),
            password.clone(),
        )
        .await?;
        let names = names();
        let seated = [mine.number, number]
            .into_iter()
            .map(|account| {
                let handle = format!("acct:{account}");
                ChatMember {
                    label: names.member_label(&handle),
                    key: handle,
                }
            })
            .collect();
        let data = load_chat_data(&client, Some(&channel_id)).await?;
        let mut data = landed_on_channel(data, channel_id, peer_name, true, seated);
        data.generation = generation;
        Ok(data)
    }
    .await
    .map_err(|message: String| HydrationError {
        generation,
        message: user_error(message),
    })
}

/// Why the viewer may not post here, as a stable reason token — empty when
/// she may. A members-only channel she is not seated in refuses her post; a
/// seat is hers under any key of her account ([`seated_in`]).
pub fn post_gate(
    archived: bool,
    members_only: bool,
    members: Vec<ChatMember>,
    me: String,
) -> String {
    if archived {
        return "channel_archived".into();
    }
    let seated = seated_in(&members, &me);
    if members_only && !seated {
        return "members_only".into();
    }
    String::new()
}
