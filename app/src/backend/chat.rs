use super::*;
use ::chat;

/// One participant of a channel's live huddle — the roster is consensus state
/// (`HuddleMember{user, node, joined_at}`), not a count.
#[derive(Clone, Debug, Hash, PartialEq)]
pub struct HuddleParticipant {
    pub key: String,
    pub label: String,
    pub initials: String,
    pub is_agent: bool,
    pub is_you: bool,
    pub joined_at: i64,
    /// The member's NODE key (hex) — the overlay identity the call hub fans
    /// media out to; `call_recipients` steers with exactly these.
    pub node: String,
}

/// Render canonical account or historical key seats from the huddle index.
/// The roster of a room the reader is seated in but not looking at (a voice
/// room), read off the room list's own seats: the seat carries the person
/// and the node, which is all the huddle window and the beacon match need.
pub(crate) fn roster_of_seats(seats: &[chat::client::HuddleSeat]) -> Vec<HuddleParticipant> {
    seats
        .iter()
        .map(|seat| HuddleParticipant {
            key: seat.node.clone(),
            label: seat.label.clone(),
            initials: seat.initials.clone(),
            is_agent: false,
            is_you: seat.is_you,
            joined_at: 0,
            node: seat.node.clone(),
        })
        .collect()
}

pub(crate) fn huddle_roster(
    members: &[chat::index::HuddleEntry],
    reader: ChatReader<'_>,
) -> Vec<HuddleParticipant> {
    members
        .iter()
        .map(|member| {
            let handle = member.party.clone();
            let label = author_display(&handle, reader.names);
            HuddleParticipant {
                initials: initials_of(&label),
                // Joining requires an external signer with a node proof.
                is_agent: false,
                // The reader's ACCOUNT, not one key: a seat taken with the
                // person's passkey or wallet is still their own seat, and a
                // roster that could not recognise it wiped itself on load.
                is_you: reader.is_me(&handle),
                joined_at: number_i64(member.joined_at),
                key: member.party.clone(),
                node: member.node.clone(),
                label,
            }
        })
        .collect()
}

/// The roster as the room list shows it under the room: a name, its
/// initials, and whether the seat is the reader's own.
pub(crate) fn huddle_seats(
    members: &[chat::index::HuddleEntry],
    reader: ChatReader<'_>,
) -> Vec<HuddleSeat> {
    huddle_roster(members, reader)
        .into_iter()
        .map(|seat| HuddleSeat {
            label: seat.label,
            initials: seat.initials,
            is_you: seat.is_you,
            node: seat.node,
        })
        .collect()
}

/// Am *I* in this huddle — the discriminant that splits the `Huddle` start
/// button from the LIVE pill with its ✕ Leave.
pub fn huddle_self(roster: Vec<HuddleParticipant>) -> bool {
    roster.iter().any(|participant| participant.is_you)
}

/// The call fan-out set: every roster peer's node key, self excluded — the
/// shape `CallClientControl::Recipients` wants.
///
/// Excludes by NODE, never by `is_you`: `is_you` answers by ACCOUNT (own key,
/// or any key bound to the same account), so two devices of one account in
/// the same huddle both answer it true, and a filter on `is_you` alone drops
/// BOTH rows — the devices go mutually dark instead of each excluding only
/// itself. `self_node` is THIS device's own node key; a roster row's
/// `node_proof` only proves that row's user holds the node key it names,
/// never that the name is unique, so any row naming this node — including a
/// stale or replayed one riding another user's `user` field — is a loopback
/// echo and never a recipient, regardless of whose row it rides in on. Only
/// when this device's own node key is unknown (no status read yet) is there
/// nothing to compare against, so `is_you` is the fallback then: it cannot
/// tell two of the reader's devices apart, but it is still the best guess
/// available for a lone-device roster.
pub fn huddle_recipient_nodes(
    roster: Vec<HuddleParticipant>,
    self_node: Option<&str>,
) -> Vec<String> {
    roster
        .into_iter()
        .filter(|participant| match self_node {
            Some(node) => participant.node != node,
            None => !participant.is_you,
        })
        .map(|participant| participant.node)
        .collect()
}

/// THE FAN-OUT SET, READ FROM CONSENSUS — the live call session's own poll
/// (`crate::call`), not the roster on screen.
///
/// The hub gates admission on this set at BOTH ends: a datagram from a peer
/// the local session does not list is dropped at demux, media and 1 Hz
/// presence beacon alike. So a peer who joins after us has to enter the set
/// from somewhere, and the only thing that used to re-steer it was a peer
/// beacon — which their join could not deliver, because they were not in the
/// set yet. Two people joining a huddle therefore heard and saw nothing of
/// each other, forever.
///
/// It is read here rather than taken from `huddle_roster` because that field
/// belongs to the channel the user is LOOKING at, which need not be the
/// channel they are huddling in.
pub(crate) async fn huddle_fanout_nodes(
    rpc: &str,
    channel_id: &str,
) -> Result<Vec<String>, String> {
    let client = rpc_client(rpc)?;
    let facts = ReaderFacts::current().await;
    let status = client.status().await.map_err(|error| error.to_string())?;
    let self_node = (!status.public_key.is_empty()).then_some(status.public_key);
    // A huddle in a room this node cannot see has no fan-out set — an empty
    // one, not a failure: the poll re-reads on its own cadence and picks the
    // roster up as soon as the index answers for the room.
    let room = load_channel_facts(&client, channel_id, facts.reader()).await?;
    let roster = room.map_or_else(Vec::new, |(_channel, roster)| roster);
    Ok(huddle_recipient_nodes(roster, self_node.as_deref()))
}

// The huddle's elapsed clock is a LOCAL session fact on a NATIVE `every 1s`
// subscription — ui-lang ships one, so this app has no tick stream of its own.

pub async fn join_huddle(
    rpc: String,
    password: String,
    channel_id: String,
) -> Result<bool, AppError> {
    async {
        let channel_id = required_id(channel_id, "channel")?;
        let rpc = rpc_client(&rpc)?;
        let status = rpc.status().await.map_err(|error| error.to_string())?;
        let node = public_key(&status.public_key, "node public key")?;
        // Proof of possession: THIS node signs the join under its own key —
        // never asserted by the joiner — so the roster can only ever name a
        // node that agreed to route this user's media (issue #1792). The mint
        // is a SIGNED request: the node binds the key that signed it, which
        // is what lets a device join through a node it does not host — no
        // operator token, just the person's own account key.
        let signed = rpc
            .clone()
            .with_write_auth(data_plane_signer(&rpc, password.clone()).await?);
        let (_, node_proof_hex) = signed
            .huddle_node_proof(&channel_id)
            .await
            .map_err(|error| error.to_string())?;
        let node_proof = hex_decode(&node_proof_hex)
            .map_err(|_| "huddle node proof must be hexadecimal".to_string())?;
        signed_write(
            &rpc,
            "chat",
            chat::encode_msg(&ChatMsg::JoinHuddle {
                channel_id: channel_id.clone(),
                node,
                node_proof,
            }),
            password,
        )
        .await?;
        Ok(true)
    }
    .await
}

/// Enter a voice room from the list: leave the huddle the reader is in (if
/// any) and join `channel_id`'s. Two ops, in order — the roster is consensus
/// state, so a person is never seated in two rooms at once. Answers the room
/// joined.
pub async fn move_huddle(
    rpc: String,
    password: String,
    leaving: String,
    channel_id: String,
) -> Result<String, AppError> {
    if !leaving.is_empty() {
        leave_huddle(rpc.clone(), password.clone(), leaving).await?;
    }
    join_huddle(rpc, password, channel_id.clone()).await?;
    Ok(channel_id)
}

pub async fn leave_huddle(
    rpc: String,
    password: String,
    channel_id: String,
) -> Result<bool, AppError> {
    async {
        let channel_id = required_id(channel_id, "channel")?;
        let rpc = rpc_client(&rpc)?;
        signed_write(
            &rpc,
            "chat",
            chat::encode_msg(&ChatMsg::LeaveHuddle {
                channel_id: channel_id.clone(),
            }),
            password,
        )
        .await?;
        Ok(true)
    }
    .await
}

pub async fn send_message(
    rpc: String,
    password: String,
    channel_id: String,
    message_id: String,
    body: String,
) -> Result<SendReceipt, OptimisticMutationError> {
    let operation_id = message_id.clone();
    let operation_scope = channel_id.clone();
    let operation_body = body.clone();
    let result = async {
        if channel_id.is_empty() {
            return Err("choose a channel first".to_string().into());
        }
        let body = bounded_text(body, "message", 16 * 1024)?;
        let rpc = rpc_client(&rpc)?;
        signed_write(
            &rpc,
            "chat",
            chat::encode_msg(&ChatMsg::PostMessage {
                channel_id: channel_id.clone(),
                message_id: required_id(message_id, "message")?,
                blocks: ::chat::client::parse_message(&body),
                thread: None,
            }),
            password,
        )
        .await?;
        Ok(())
    }
    .await;
    result
        .map(|()| SendReceipt {
            operation_id: operation_id.clone(),
            channel_id: operation_scope.clone(),
        })
        .map_err(|cause: AppError| OptimisticMutationError {
            message: cause.message,
            committed: cause.committed,
            operation_id,
            scope_id: operation_scope,
            // A message is not in a thread; the stream's composer is keyed
            // by its room alone.
            thread_seq: 0,
            body: operation_body,
        })
}

pub async fn send_reply(
    rpc: String,
    password: String,
    channel_id: String,
    root_seq: i64,
    message_id: String,
    body: String,
) -> Result<SendReceipt, OptimisticMutationError> {
    let operation_id = message_id.clone();
    let operation_scope = channel_id.clone();
    let operation_thread = root_seq;
    let operation_body = body.clone();
    let result = async {
        let root_seq = positive_sequence(root_seq)?;
        let body = bounded_text(body, "reply", 16 * 1024)?;
        let rpc = rpc_client(&rpc)?;
        let message_id = required_id(message_id, "message")?;
        signed_write(
            &rpc,
            "chat",
            chat::encode_msg(&ChatMsg::PostMessage {
                channel_id: channel_id.clone(),
                message_id: message_id.clone(),
                blocks: ::chat::client::parse_message(&body),
                thread: Some(root_seq),
            }),
            password,
        )
        .await?;
        Ok(())
    }
    .await;
    result
        .map(|()| SendReceipt {
            operation_id: operation_id.clone(),
            channel_id: operation_scope.clone(),
        })
        .map_err(|cause: AppError| OptimisticMutationError {
            message: cause.message,
            committed: cause.committed,
            operation_id,
            scope_id: operation_scope,
            thread_seq: operation_thread,
            body: operation_body,
        })
}

pub async fn edit_message(
    rpc: String,
    password: String,
    channel_id: String,
    seq: i64,
    base_rev: i64,
    body: String,
) -> Result<bool, AppError> {
    async {
        let seq = positive_sequence(seq)?;
        let base_rev =
            u32::try_from(base_rev).map_err(|_| "invalid message revision".to_string())?;
        let body = bounded_text(body, "message", 16 * 1024)?;
        let rpc = rpc_client(&rpc)?;
        signed_write(
            &rpc,
            "chat",
            chat::encode_msg(&ChatMsg::EditMessage {
                channel_id: channel_id.clone(),
                seq,
                blocks: ::chat::client::parse_message(&body),
                base_rev: Some(base_rev),
            }),
            password,
        )
        .await?;
        Ok(true)
    }
    .await
}

/// THE COMMAND PALETTE'S CHAT SEARCH. The Chat tab searches the index for
/// itself now; this is the workspace-wide search the palette runs across chat
/// and pages at once, which is the app's own screen.
pub async fn search_chat(
    rpc: String,
    channel_id: String,
    text: String,
) -> Result<ChatSearchData, AppError> {
    // The same naming the timeline does — a search hit was showing the RAW
    // wire author (`user:3f8dc8…773`, the full 64-hex key with its prefix) as
    // the row's headline, above the text it matched.
    let facts = ReaderFacts::current().await;
    let result = async {
        let text = bounded_text(text, "search", 512)?;
        let rpc = rpc_client(&rpc)?;
        // a `#tag` query filters by the exact hashtag (the index's tag
        // postings); anything else is full-text search.
        let query = match text.strip_prefix('#') {
            Some(tag) if !tag.is_empty() => serde_json::json!({
                "tag_search": {
                    "tag": tag.to_lowercase(),
                    "channel_id": (!channel_id.is_empty()).then_some(channel_id),
                    "limit": 50
                }
            }),
            _ => serde_json::json!({
                "search": {
                    "text": text,
                    "channel_id": (!channel_id.is_empty()).then_some(channel_id),
                    "limit": 50
                }
            }),
        };
        let reply: chat::index::ChatViewReply = rpc.view("chat", &query).await?;
        let chat::index::ChatViewReply::Hits(hits) = reply else {
            return Err("chat search returned an invalid reply".into());
        };
        Ok(ChatSearchData {
            hits: hits
                .into_iter()
                .map(|hit| ChatSearchHit {
                    // THE ROOM COMES FIRST, because it is the thing a hit is
                    // missing. `#12` alone reads as a CHANNEL in this app —
                    // every channel is written `# General` — while it is
                    // actually the message's sequence number, and the channel
                    // it was found in went unsaid in both the palette and the
                    // sidebar. Every renderer of a hit shows `meta`, so the
                    // room belongs in it rather than composed at one call site
                    // (which is what the Explorer was doing alone).
                    meta: format!("{} · #{}", hit.channel_id, hit.seq),
                    channel_id: hit.channel_id,
                    seq: number_i64(hit.seq),
                    root_seq: number_i64(hit.thread.unwrap_or(hit.seq)),
                    author: author_display(&hit.author, facts.names()),
                    text: ::chat::client::draft_mentions(&hit.text, facts.names()).0,
                })
                .collect(),
        })
    }
    .await;
    result.map_err(app_error)
}

/// Hand a WEB link to the OS opener — the `DuckKind::Web` arm of the open
/// plane, and its only caller. Only http(s) leaves the app this way
/// (this passes a string to a shell command, and the scheme gate is the trust
/// boundary); every other scheme is the open plane's own to resolve.
pub async fn open_external_url(url: String) -> Result<bool, AppError> {
    async {
        // "" is the open plane's "not my turn" — nothing was asked, nothing
        // opens.
        if url.is_empty() {
            return Ok(false);
        }
        let is_web = url.starts_with("http://") || url.starts_with("https://");
        if !is_web {
            return Err("only web links are handed to the system browser".to_string());
        }
        let opener = match std::env::consts::OS {
            "macos" => "open",
            "windows" => "explorer",
            _ => "xdg-open",
        };
        tokio::process::Command::new(opener)
            .arg(&url)
            .spawn()
            .map_err(|error| format!("could not open the link: {error}"))?;
        Ok(true)
    }
    .await
    .map_err(app_error)
}

/// Every hit joined to the TITLE of the page it lives in.
///
/// A hit row from the index names only its page id, so every surface that
/// rendered one had to say something else instead: the Explorer printed the
/// block text as BOTH the row's title and its snippet, the palette showed a
/// bare block kind, and the pages search panel showed an opaque block id.
/// None of the three said which page the match was in.
pub(crate) fn titled_page_hits(
    hits: Vec<pages::index::PageBlockRow>,
    index: Vec<PageRow>,
) -> Vec<PageSearchHit> {
    let titles = index
        .into_iter()
        .map(|page| (page.id, page.title))
        .collect::<BTreeMap<_, _>>();
    hits.into_iter()
        .map(|hit| PageSearchHit {
            // An untitled page reads "Untitled" in the sidebar (`page_items`),
            // so a hit must not read differently. A page id the index does not
            // carry takes the same fallback rather than a blank run.
            page_title: titles
                .get(&hit.page_id)
                .filter(|title| !title.is_empty())
                .cloned()
                .unwrap_or_else(|| "Untitled".to_string()),
            page_id: hit.page_id,
            block_id: hit.block_id,
            kind: block_kind_name(hit.kind).into(),
            text: hit.text,
        })
        .collect()
}

pub async fn search_pages(
    rpc: String,
    page_id: String,
    text: String,
) -> Result<PageSearchData, AppError> {
    let result = async {
        let text = bounded_text(text, "search", 512)?;
        let rpc = rpc_client(&rpc)?;
        let reply: pages::index::PagesViewReply = rpc
            .view(
                "pages",
                &serde_json::json!({
                    "search": {
                        "text": text,
                        "page_id": (!page_id.is_empty()).then_some(page_id),
                        "limit": 50
                    }
                }),
            )
            .await?;
        let pages::index::PagesViewReply::Hits(hits) = reply else {
            return Err("page search returned an invalid reply".into());
        };
        if hits.is_empty() {
            return Ok(PageSearchData { hits: Vec::new() });
        }
        // The titles live one view over, in the same index — this is the very
        // call the pages sidebar makes. Paid once per search that matched
        // something, never on the empty keystrokes that dominate the palette.
        //
        // A LABEL IS DECORATION AND MUST NEVER DESTROY THE PAYLOAD. Taking `?`
        // here turned a search the node had already ANSWERED into an `Err`, and
        // both readers throw those away without a word: the Explorer's
        // `if let Ok(pages)` (backend/search.rs) drops every page hit from a
        // workspace search, and the palette keeps whichever leg survived. So one
        // failed `ListPages` — a second round trip, on a paged view, after the
        // search already returned — silently emptied page results that existed.
        // An index we could not read leaves every hit on the "Untitled"
        // fallback `titled_page_hits` already takes for an unknown page id.
        let index = load_page_index(&rpc).await.unwrap_or_default();
        Ok(PageSearchData {
            hits: titled_page_hits(hits, index),
        })
    }
    .await;
    result.map_err(app_error)
}
