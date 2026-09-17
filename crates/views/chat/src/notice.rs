//! Notification policy and wording belong to the deployed Chat view.
use crate::host::{
    ChatChannel, Names, author_display, dm_channel_id, hex_encode, message_body, names_at, view,
};
use chat_message::{Block, Mark, Party};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub payload: serde_json::Value,
    pub assigned: Option<serde_json::Value>,
    pub context: Context,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Context {
    pub key: Vec<u8>,
    pub screen: OnScreen,
}

pub async fn notice(request: Request) -> Option<DesktopNotice> {
    let operation = request.payload.as_object()?;
    let single_operation = operation.len() == 1;
    if !single_operation {
        return None;
    }
    match operation.keys().next()?.as_str() {
        "post_message" => message_notice(request).await,
        "join_huddle" => joined_notice(request).await,
        _ => None,
    }
}

async fn message_notice(request: Request) -> Option<DesktopNotice> {
    request.assigned.as_ref()?.get("posted")?;
    let names = names_at(0).await;
    let mut arrival = chat_arrival(
        &request.payload,
        request.assigned.as_ref(),
        &names,
        &request.context,
    )?;
    notice_reason(&arrival)?;
    if !arrival.in_my_dm {
        let reply = view(
            "channel",
            serde_json::json!({"channel":{"channel_id":arrival.channel_id}}),
        )
        .await
        .ok()?;
        if let Some(name) = reply["name"].as_str() {
            arrival.room = format!("#{name}");
        }
    }
    desktop_notice(&arrival, true, &request.context.screen)
}

async fn joined_notice(request: Request) -> Option<DesktopNotice> {
    let channel_id = request.payload["join_huddle"]["channel_id"].as_str()?;
    let record = view(
        "channel",
        serde_json::json!({"channel":{"channel_id":channel_id}}),
    )
    .await
    .ok()?;
    let [first] = record["huddle"].as_array()?.as_slice() else {
        return None;
    };
    let names = names_at(0).await;
    let key = hex_encode(&request.context.key);
    let me = names
        .account_of(&key)
        .map(|number| format!("acct:{number}"));
    let actor = first["party"].as_str()?;
    let own_seat = me.as_deref() == Some(actor) || actor == format!("user:{key}");
    let channel = ChatChannel {
        id: channel_id.into(),
        name: record["name"].as_str()?.into(),
        huddle: vec![crate::host::HuddleSeat {
            label: author_display(actor, &names),
            is_you: own_seat,
            ..Default::default()
        }],
        ..Default::default()
    };
    huddle_started_notice(&channel, true)
}

fn chat_arrival(
    payload: &serde_json::Value,
    assigned: Option<&serde_json::Value>,
    names: &Names,
    context: &Context,
) -> Option<Arrival> {
    let post = payload.get("post_message")?;
    let channel_id = post["channel_id"].as_str()?.to_owned();
    let stamp = assigned?.get("posted")?;
    let actor: Party = serde_json::from_value(stamp["actor"].clone()).ok()?;
    let mentions: Vec<u64> = serde_json::from_value(stamp["key_mentions"].clone()).ok()?;
    let blocks = chat_message::resolve_assigned_mentions(
        serde_json::from_value(post["blocks"].clone()).ok()?,
        &mentions,
    )
    .ok()?;
    let key_hex = hex_encode(&context.key);
    let me = names
        .account_of(&key_hex)
        .map(Party::Account)
        .unwrap_or_else(|| Party::Key(context.key.clone()));
    let handle = match &actor {
        Party::Account(number) => format!("acct:{number}"),
        Party::Key(key) => format!("user:{}", hex_encode(key)),
        Party::Module(module) => format!("module:{module}"),
        Party::System => "system".into(),
    };
    let has_reader = !context.key.is_empty();
    let mentions_me = has_reader
        && blocks.iter().any(|block| {
            let spans = match block {
                Block::Paragraph(spans) | Block::Quote(spans) => spans,
                Block::Code { .. } | Block::Divider => return false,
            };
            spans.iter().any(|span| {
                span.marks
                    .iter()
                    .any(|mark| matches!(mark, Mark::Mention(party) if party == &me))
            })
        });
    let dm = names.account_of(&key_hex).and_then(|mine| {
        names.by_account.iter().find_map(|(peer, name)| {
            let matches_room =
                *peer != mine && dm_channel_id(&mine.to_string(), &peer.to_string()) == channel_id;
            matches_room.then(|| name.clone())
        })
    });
    Some(Arrival {
        in_my_dm: dm.is_some(),
        room: dm.unwrap_or_else(|| channel_id.clone()),
        channel_id,
        author: author_display(&handle, names),
        body: message_body(&serde_json::to_value(blocks).ok()?, names),
        mentions_me,
        authored_by_me: has_reader && (actor == me || actor == Party::Key(context.key.clone())),
    })
}

/// How much of a message body a notification carries. A banner shows two or
/// three lines; past that the excerpt is only paying for itself in memory.
const EXCERPT_CHARS: usize = 140;

/// One notification, already worded — the shape the platform call takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopNotice {
    /// the room, as the sidebar says it: `#general`, or the peer's name.
    pub title: String,
    /// who wrote it, by account name.
    pub subtitle: String,
    pub body: String,
    /// the room, for grouping every notice from one conversation together.
    pub thread: String,
}

/// WHY THIS ARRIVAL WOULD BE WORTH INTERRUPTING FOR. One discriminant, so a
/// third reason has to be routed rather than folded into a boolean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeReason {
    Mentioned,
    DirectMessage,
}

/// One arrived chat message, as the live fold reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Arrival {
    pub channel_id: String,
    /// the room's display name — `#general`, or the DM peer's name.
    pub room: String,
    /// the author's account name (or the short handle, unnamed).
    pub author: String,
    pub body: String,
    pub mentions_me: bool,
    pub in_my_dm: bool,
    pub authored_by_me: bool,
}

/// What the reader can already see for themselves.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnScreen {
    pub app_focused: bool,
    pub active_channel: String,
}

/// The reason to notify about this arrival, or `None` for the arrivals that
/// are none: my own writing, and everything addressed to nobody in particular.
pub fn notice_reason(arrival: &Arrival) -> Option<NoticeReason> {
    if arrival.authored_by_me {
        return None;
    }
    // A mention outranks the room it was written in: being named in your own
    // DM is still being named.
    match (arrival.mentions_me, arrival.in_my_dm) {
        (true, _) => Some(NoticeReason::Mentioned),
        (false, true) => Some(NoticeReason::DirectMessage),
        (false, false) => None,
    }
}

/// THE WHOLE DECISION, pure: what to post for this arrival, or nothing.
///
/// Suppression is narrow ON PURPOSE — only a reader who is LOOKING AT the room
/// the message landed in has already been told. A focused window on another
/// room, or the Files tab, has not.
pub fn desktop_notice(
    arrival: &Arrival,
    enabled: bool,
    screen: &OnScreen,
) -> Option<DesktopNotice> {
    if !enabled {
        return None;
    }
    let reason = notice_reason(arrival)?;
    let already_read_it = screen.app_focused && screen.active_channel == arrival.channel_id;
    if already_read_it {
        return None;
    }
    Some(notice_text(arrival, reason))
}

/// The words. Split from the decision so both are checkable, and so the one
/// place a person's message is rendered for the lock screen is nameable.
pub fn notice_text(arrival: &Arrival, reason: NoticeReason) -> DesktopNotice {
    let subtitle = match reason {
        NoticeReason::Mentioned => format!("{} mentioned you", arrival.author),
        NoticeReason::DirectMessage => arrival.author.clone(),
    };
    DesktopNotice {
        title: arrival.room.clone(),
        subtitle,
        body: notice_excerpt(&arrival.body),
        thread: arrival.channel_id.clone(),
    }
}

/// A message body as one line of banner text: newlines collapse to spaces,
/// runs of whitespace collapse to one, and a long body is cut on a CHARACTER
/// boundary with an ellipsis.
pub fn notice_excerpt(body: &str) -> String {
    let flat: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= EXCERPT_CHARS {
        return flat;
    }
    let kept: String = flat.chars().take(EXCERPT_CHARS).collect();
    format!("{}…", kept.trim_end())
}

/// The decision, pure: the room's first seat, taken by someone else.
pub fn huddle_started_notice(channel: &ChatChannel, enabled: bool) -> Option<DesktopNotice> {
    if !enabled {
        return None;
    }
    let [first] = channel.huddle.as_slice() else {
        return None;
    };
    if first.is_you {
        return None;
    }
    Some(DesktopNotice {
        title: format!("#{}", channel.name),
        subtitle: format!("{} started a huddle", first.label),
        body: "Join from the room list.".into(),
        thread: channel.id.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::HuddleSeat;
    #[test]
    fn a_dm_id_is_pair_derived_and_cannot_be_forged() {
        // the viewer's key, and a peer's ACCOUNT NUMBER as the directory keys it.
        let a = "aa".repeat(32);
        let b = "42".to_string();
        assert_eq!(
            dm_channel_id(&a, &b),
            dm_channel_id(&b, &a),
            "both sides derive the same channel"
        );
        // the id the app mints is the id chat will accept from a USER author:
        // ':' is reserved for module origins and '/' is refused outright, so a
        // minted id carrying either is a DM that can never be created.
        // `chat::client`'s own test runs the id through that rule directly.
        let id = dm_channel_id(&a, &b);
        assert!(
            !id.contains(':'),
            "a user-authored channel id may not carry ':'"
        );
        assert!(!id.contains('/'), "a channel id may not carry '/'");
        assert!(id.starts_with("dm-") && id.len() == 67);
    }

    #[test]
    fn notification_uses_committed_author_and_mentions_instead_of_current_key_ownership() {
        let key = vec![7; 32];
        let names = crate::host::fold_names(&serde_json::json!([
            {"number":1,"name":"Reader","keys":[{"pubkey":key}]},
            {"number":2,"name":"Reporter","keys":[]},
            {"number":3,"name":"Changed owner","keys":[{"pubkey":vec![8;32]}]}
        ]));
        let context = Context {
            key,
            screen: elsewhere(),
        };
        let payload = serde_json::json!({"post_message":{
            "channel_id":"general", "message_id":"m", "thread":null,
            "blocks":[{"paragraph":[{"text":"@Old Name", "marks":[{"mention":{"key":vec![8;32]}}]}]}]
        }});
        let assigned =
            serde_json::json!({"posted":{"seq":1, "actor":{"account":2}, "key_mentions":[1]}});
        let arrival = chat_arrival(&payload, Some(&assigned), &names, &context).unwrap();
        assert_eq!(arrival.author, "Reporter");
        assert_eq!(arrival.body, "@Reader");
        assert!(arrival.mentions_me);
        assert!(!arrival.authored_by_me);
        assert!(chat_arrival(&payload, None, &names, &context).is_none());
        let mine = serde_json::json!({"posted":{"seq":2,"actor":{"account":1},"key_mentions":[1]}});
        let own = chat_arrival(&payload, Some(&mine), &names, &context).unwrap();
        assert!(notice_reason(&own).is_none());
        let invalid =
            serde_json::json!({"posted":{"seq":2,"actor":{"account":2},"key_mentions":[]}});
        assert!(chat_arrival(&payload, Some(&invalid), &names, &context).is_none());
    }

    /// The first seat in a room, taken by someone else, is the one banner a
    /// huddle raises: a second joiner is not news, and neither is the
    /// reader's own seat.
    #[test]
    fn a_huddle_banner_is_the_rooms_first_seat_taken_by_someone_else() {
        let seat = |label: &str, is_you: bool| HuddleSeat {
            label: label.into(),
            initials: "A".into(),
            is_you,
            node: "aa".into(),
        };
        let room = |seats: Vec<HuddleSeat>| ChatChannel {
            id: "channel-a".into(),
            name: "general".into(),
            huddle: seats,
            ..ChatChannel::default()
        };
        let started = huddle_started_notice(&room(vec![seat("Ada", false)]), true)
            .expect("the first seat is a banner");
        assert_eq!(started.title, "#general");
        assert_eq!(started.subtitle, "Ada started a huddle");
        assert_eq!(started.thread, "channel-a");
        assert!(huddle_started_notice(&room(vec![seat("Me", true)]), true).is_none());
        assert!(
            huddle_started_notice(&room(vec![seat("Ada", false), seat("Bob", false)]), true)
                .is_none()
        );
        assert!(huddle_started_notice(&room(vec![seat("Ada", false)]), false).is_none());
    }

    fn arrival() -> Arrival {
        Arrival {
            channel_id: "general".into(),
            room: "#general".into(),
            author: "orthory".into(),
            body: "ping @eddy about the deploy".into(),
            mentions_me: true,
            in_my_dm: false,
            authored_by_me: false,
        }
    }

    fn elsewhere() -> OnScreen {
        OnScreen {
            app_focused: false,
            active_channel: String::new(),
        }
    }

    #[test]
    fn a_mention_notifies_and_names_the_room_the_author_and_the_words() {
        let notice = desktop_notice(&arrival(), true, &elsewhere()).expect("a mention notifies");
        assert_eq!(notice.title, "#general");
        assert_eq!(notice.subtitle, "orthory mentioned you");
        assert_eq!(notice.body, "ping @eddy about the deploy");
        assert_eq!(notice.thread, "general", "banners group per room");
    }

    #[test]
    fn a_dm_notifies_without_a_mention() {
        let dm = Arrival {
            channel_id: "dm-1".into(),
            room: "orthory".into(),
            mentions_me: false,
            in_my_dm: true,
            ..arrival()
        };
        let notice = desktop_notice(&dm, true, &elsewhere()).expect("a DM notifies");
        assert_eq!(notice.title, "orthory");
        assert_eq!(
            notice.subtitle, "orthory",
            "a DM is already addressed to me — 'mentioned you' would be a lie"
        );
    }

    /// A mention in my own DM is still a mention.
    #[test]
    fn a_mention_outranks_the_room_it_landed_in() {
        let both = Arrival {
            in_my_dm: true,
            ..arrival()
        };
        assert_eq!(notice_reason(&both), Some(NoticeReason::Mentioned));
    }

    #[test]
    fn an_ordinary_room_message_never_notifies() {
        let chatter = Arrival {
            mentions_me: false,
            ..arrival()
        };
        assert_eq!(notice_reason(&chatter), None);
        assert!(desktop_notice(&chatter, true, &elsewhere()).is_none());
    }

    #[test]
    fn my_own_writing_never_notifies_me() {
        let mine = Arrival {
            authored_by_me: true,
            ..arrival()
        };
        assert_eq!(notice_reason(&mine), None);
    }

    /// The one suppression: the reader is LOOKING AT the room it landed in.
    #[test]
    fn the_room_on_screen_in_a_focused_window_suppresses_it() {
        let watching = OnScreen {
            app_focused: true,
            active_channel: "general".into(),
        };
        assert!(desktop_notice(&arrival(), true, &watching).is_none());

        let other_room = OnScreen {
            app_focused: true,
            active_channel: "random".into(),
        };
        assert!(
            desktop_notice(&arrival(), true, &other_room).is_some(),
            "a focused window on ANOTHER room has not shown me this"
        );

        let behind = OnScreen {
            app_focused: false,
            active_channel: "general".into(),
        };
        assert!(
            desktop_notice(&arrival(), true, &behind).is_some(),
            "the right room behind an editor is not a room anyone has read"
        );
    }

    #[test]
    fn the_preference_is_the_first_gate() {
        assert!(desktop_notice(&arrival(), false, &elsewhere()).is_none());
    }

    #[test]
    fn an_excerpt_is_one_flat_bounded_line() {
        assert_eq!(notice_excerpt("one\ntwo   three\n"), "one two three");
        assert_eq!(notice_excerpt(""), "");

        let long = "duck ".repeat(80);
        let excerpt = notice_excerpt(&long);
        assert!(excerpt.chars().count() <= EXCERPT_CHARS + 1, "…is the +1");
        assert!(excerpt.ends_with('…'));
        assert!(!excerpt.contains("  "));

        // A multi-byte body is cut on a CHARACTER boundary, never a byte one.
        let wide = "한글 ".repeat(80);
        assert!(notice_excerpt(&wide).ends_with('…'));
    }
}
