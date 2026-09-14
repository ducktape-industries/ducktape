use super::*;

pub(crate) async fn load_workspace(
    rpc: &RpcClient,
    channel_id: Option<&str>,
    generation: i64,
) -> Result<WorkspaceData, String> {
    // Two independent reads. Serialized, chat's first paint waited on a status
    // probe it does not render; concurrent, the console opens on the slower
    // leg rather than their sum.
    let (status, chat) = tokio::try_join!(
        async { rpc.status().await.map_err(String::from) },
        load_chat_data(rpc, channel_id)
    )?;
    let tip = tip_from_status(status)?;
    Ok(WorkspaceData {
        generation,
        rpc: rpc.origin().to_string(),
        status: tip.status,
        height: tip.height,
        channels: chat.channels,
        active_channel: chat.active_channel,
        active_channel_name: chat.active_channel_name,
        active_channel_archived: chat.active_channel_archived,
        active_channel_members_only: chat.active_channel_members_only,
        huddle_roster: chat.huddle_roster,
        channel_members: chat.channel_members,
    })
}

fn tip_from_status(status: NodeStatus) -> Result<Tip, String> {
    let height = i64::try_from(status.height).map_err(|_| "node height exceeds i64")?;
    Ok(Tip {
        height,
        status: format!("Connected · block {height}"),
    })
}

/// Where a cold start lands when nothing was asked for. The wire orders
/// channels by ID, so "the first one" is an accident of naming — in the demo
/// workspace it is a `channel-1786073…` created minutes ago with nothing in it,
/// and the console opens on "No messages yet" while three rooms carrying
/// hundreds of messages sit under it. Land on somewhere with something to read.
///
/// Each fallback answers a workspace the one above it cannot: every room empty,
/// then every room archived. The last rung keeps the old behaviour so a landing
/// still happens rather than the console opening on no channel at all.
pub(crate) fn landing_channel(channels: &[ChatChannel]) -> Option<&ChatChannel> {
    let has_traffic = |channel: &&ChatChannel| !channel.archived && channel.head_seq > 0;
    let is_open = |channel: &&ChatChannel| !channel.archived;
    channels
        .iter()
        .find(has_traffic)
        .or_else(|| channels.iter().find(is_open))
        .or_else(|| channels.first())
}

pub(crate) async fn load_chat_data(
    rpc: &RpcClient,
    requested: Option<&str>,
) -> Result<ChatData, String> {
    // The directory before any row: every author, member and huddle tile below
    // is named through it, and this is the load that runs on boot and on every
    // resync — including the one an identity op triggers.
    //
    // A directory that cannot be read yet — a resident whose identity index is
    // still folding — is a room named by key hex, never a room that failed to
    // open: this must not be the `?` that takes the whole chat load down.
    if let Err(error) = refresh_names(rpc).await {
        tracing::debug!(
            target: "ducktape::chat",
            %error,
            "the name directory was not read; keys render as hex"
        );
    }
    let mut wire_channels = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let reply: ChatViewReply = rpc
            .view(
                "chat",
                &ChatViewQuery::Channels {
                    after: after.clone(),
                    limit: None,
                },
            )
            .await?;
        let ChatViewReply::Channels {
            channels: page,
            has_more,
            next_after,
        } = reply
        else {
            return Err("node returned an invalid channel list".into());
        };
        wire_channels.extend(page);
        if !has_more {
            break;
        }
        after = next_after;
        if after.is_none() {
            break;
        }
    }
    let facts = ReaderFacts::current().await;
    let channels = wire_channels
        .iter()
        .map(|info| ChatChannel {
            id: info.channel.id.clone(),
            name: info.channel.name.clone(),
            archived: info.channel.archived,
            members_only: info.channel.post_policy == PostPolicy::MembersOnly,
            huddle_count: count_i64(info.channel.huddle.len()),
            voice: info.channel.voice,
            head_seq: number_i64(info.head_seq),
            huddle: huddle_seats(&info.channel.huddle, facts.reader()),
        })
        .collect::<Vec<_>>();
    let active_channel = requested
        .filter(|id| channels.iter().any(|channel| channel.id == *id))
        .map(str::to_string)
        .or_else(|| landing_channel(&channels).map(|channel| channel.id.clone()))
        .unwrap_or_default();
    let active_channel_name = channels
        .iter()
        .find(|channel| channel.id == active_channel)
        .map(|channel| channel.name.clone())
        .unwrap_or_default();
    // WHAT THE READER IS LOOKING AT, recorded where it is decided. This load
    // runs on every room open and every chat resync, and it is the only place
    // that knows both the room names and which one won — the live decoder,
    // which needs both to word and to suppress a desktop notification, may not
    // query for either inside the fold. See `notify`.
    note_rooms(&channels, &active_channel);
    let active_wire_channel = wire_channels
        .iter()
        .find(|info| info.channel.id == active_channel);
    let active_channel_archived = active_wire_channel.is_some_and(|info| info.channel.archived);
    let active_channel_members_only =
        active_wire_channel.is_some_and(|info| info.channel.post_policy == PostPolicy::MembersOnly);
    let huddle_roster = active_wire_channel.map_or_else(Vec::new, |info| {
        huddle_roster(&info.channel.huddle, facts.reader())
    });
    // NO TIMELINE. The chat view reads its own room's messages off the index;
    // what the app still needs of a room is its record, its huddle roster and
    // its member roll — the roster the call's media leg hangs on, and the roll
    // the composers complete mentions against.
    let channel_members = match active_channel.is_empty() {
        true => Vec::new(),
        false => load_channel_members(rpc, &active_channel, facts.names()).await?,
    };
    Ok(ChatData {
        generation: 0,
        channels,
        active_channel,
        active_channel_name,
        active_channel_archived,
        active_channel_members_only,
        huddle_roster,
        channel_members,
    })
}

/// One channel's row and its huddle roster, read from the index view. The
/// roster length is not derivable from an op, so the row still has to be read.
///
/// `None` IS AN ANSWER, NOT A FAILURE. A room this node cannot see is the
/// ordinary state of three things: a resident whose chat index has not folded
/// yet (a fresh join spends minutes in `joining` with every `Channel` query
/// answering `null`), a room id remembered from a network this session has
/// since left, and a landing id that never existed here. None of those is a
/// broken node, so none of them may reach the reader as a red banner over the
/// timeline — each caller decides what an unseen room means for it.
pub(crate) async fn load_channel_facts(
    rpc: &RpcClient,
    channel_id: &str,
    reader: ChatReader<'_>,
) -> Result<Option<(ChatChannel, Vec<HuddleParticipant>)>, String> {
    let reply: ChatViewReply = rpc
        .view(
            "chat",
            &ChatViewQuery::Channel {
                channel_id: channel_id.to_string(),
            },
        )
        .await?;
    let ChatViewReply::Channel(record) = reply else {
        return Err("node returned an invalid channel record".into());
    };
    let Some(info) = record else {
        return Ok(None);
    };
    // The roster is named through the reader handed in — the directory as
    // last read, never a filling query: `load_channel_row` awaits this inside
    // the live stream's decoder fold, where a `/v1/query` would freeze every
    // subscriber for as long as the node's select loop is busy.
    let roster = huddle_roster(&info.channel.huddle, reader);
    Ok(Some((
        ChatChannel {
            id: info.channel.id,
            name: info.channel.name,
            archived: info.channel.archived,
            members_only: info.channel.post_policy == PostPolicy::MembersOnly,
            huddle_count: count_i64(info.channel.huddle.len()),
            voice: info.channel.voice,
            head_seq: number_i64(info.head_seq),
            huddle: huddle_seats(&info.channel.huddle, reader),
        },
        roster,
    )))
}

/// One channel's window, WITHOUT re-paging the channel list.
///
/// This is the SWITCH path's loader; [`load_chat_data`] is the cold-boot and
/// resync one, where the list itself is the thing being learned. A channel
/// click already holds that list in state and the live fold keeps it fresh, so
/// re-paging it put a whole extra round trip — more on a workspace past one
/// page — in front of the first row the reader was waiting for.
///
/// What is left is two INDEPENDENT reads: the channel's own row (for the
/// huddle roster) and the member roll. They run concurrently, so a switch
/// costs one round trip — and no timeline: the chat view reads its own room's
/// messages off the index, on the kernel contract.
///
/// The answer CARRIES BACK ONLY THE ROW THIS REFRESHED. Handing the pre-click
/// channel snapshot back would have the reducer revert every delta the live
/// stream folded during the round trip — a peer's post in a third room and its
/// unread badge, a channel created, renamed or archived. See
/// `upsert_channel_rows`, which folds this row into the list on screen instead
/// of replacing it.
///
/// The window is authoritative about WHERE IT LANDED, which is not the same as
/// always landing where it was asked. A room this node cannot see is not an
/// error the reader can act on — it is a resident whose index has not folded
/// yet, or an id remembered from a network this session has left — so it falls
/// through to the cold load, which names the landing channel it settles on (or
/// no channel at all, on a workspace with nothing to read yet) and carries the
/// whole list back with it. Every reducer downstream lands on
/// `next.active_channel`, so the answer stays truthful either way; what it must
/// never do is put "channel record was not found" over the timeline.
pub(crate) async fn load_channel_window_data(
    rpc: &RpcClient,
    channel_id: &str,
) -> Result<ChatData, String> {
    // Awaited before the fan-out so the cached identity is warm for every leg:
    // there is no single-flight, and two cold callers would each spawn the
    // CLI. Same reason `load_chat_data` awaits it above its own read.
    let facts = ReaderFacts::current().await;
    let (room, channel_members) = tokio::try_join!(
        load_channel_facts(rpc, channel_id, facts.reader()),
        load_channel_members(rpc, channel_id, facts.names())
    )?;
    let Some((channel, huddle_roster)) = room else {
        return load_chat_data(rpc, None).await;
    };
    Ok(ChatData {
        generation: 0,
        channels: vec![channel.clone()],
        active_channel: channel.id,
        active_channel_name: channel.name,
        active_channel_archived: channel.archived,
        active_channel_members_only: channel.members_only,
        huddle_roster,
        channel_members,
    })
}

pub(crate) async fn load_channel_members(
    rpc: &RpcClient,
    channel_id: &str,
    names: &NameDirectory,
) -> Result<Vec<ChatMember>, String> {
    let mut members = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let reply: ChatViewReply = rpc
            .view(
                "chat",
                &ChatViewQuery::Members {
                    channel_id: channel_id.to_string(),
                    after: after.clone(),
                    limit: None,
                },
            )
            .await?;
        let ChatViewReply::Members {
            members: page,
            has_more,
            next_after,
        } = reply
        else {
            return Err("node returned an invalid channel member list".into());
        };
        members.extend(page);
        if !has_more {
            break;
        }
        after = next_after;
        if after.is_none() {
            break;
        }
    }
    Ok(members
        .into_iter()
        .map(|member| {
            let id = member_id(&member.party);
            ChatMember {
                label: names.member_label(id),
                key: id.to_string(),
            }
        })
        .collect())
}

/// The member's key id: the part after `user:` in a rendered member handle,
/// or the whole handle when it carries no such prefix.
pub(crate) fn member_id(user: &str) -> &str {
    user.strip_prefix("user:").unwrap_or(user)
}

pub(crate) async fn load_page_index(rpc: &RpcClient) -> Result<Vec<PageRow>, String> {
    let mut pages = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let reply: PagesViewReply = rpc
            .view(
                "pages",
                &PagesViewQuery::ListPages {
                    after: after.clone(),
                    limit: None,
                },
            )
            .await?;
        let PagesViewReply::Pages {
            pages: page,
            has_more,
            next_after,
        } = reply
        else {
            return Err("node returned an invalid page list".into());
        };
        pages.extend(page);
        if !has_more {
            return Ok(pages);
        }
        let Some(next) = next_after else {
            return Ok(pages);
        };
        if after.as_ref() == Some(&next) {
            return Err("node repeated the page-list cursor".into());
        }
        after = Some(next);
    }
}
