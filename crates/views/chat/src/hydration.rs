//! Channel selection and the remaining native shell's room facts.
use crate::host::{self, ChatChannel, Names};
use serde_json::{Value, json};

async fn reader(key: &str) -> (Names, String) {
    let names = host::names_at(0).await;
    let handle = names
        .account_of(key)
        .map(|number| format!("acct:{number}"))
        .unwrap_or_else(|| format!("user:{key}"));
    host::seat_reader(&handle, key);
    (names, handle)
}

fn landing(channels: &[ChatChannel]) -> Option<&ChatChannel> {
    let has_traffic = |channel: &&ChatChannel| !channel.archived && channel.head_seq > 0;
    let is_open = |channel: &&ChatChannel| !channel.archived;
    channels
        .iter()
        .find(has_traffic)
        .or_else(|| channels.iter().find(is_open))
        .or_else(|| channels.first())
}

async fn facts(id: &str, names: &Names) -> Result<Option<(ChatChannel, Value)>, String> {
    let row = host::view("channel", json!({"channel":{"channel_id":id}})).await?;
    if row.is_null() {
        return Ok(None);
    }
    let matches_id = row["id"].as_str() == Some(id);
    if !matches_id {
        return Err("channel reply names another room".into());
    }
    let channel = host::channel_row(&row, names);
    let roster = row["huddle"].as_array().into_iter().flatten().zip(&channel.huddle)
        .map(|(raw, seat)| json!({
            "key":raw["party"],"label":seat.label,"initials":seat.initials,
            "is_agent":false,"is_you":seat.is_you,"joined_at":raw["joined_at"].as_i64().unwrap_or_default(),
            "node":seat.node
        })).collect::<Vec<_>>();
    Ok(Some((channel, json!(roster))))
}

fn data(channels: Vec<ChatChannel>, active: Option<ChatChannel>, roster: Value) -> Value {
    let active = active.unwrap_or_default();
    json!({
        "generation":0, "channels":channels,
        "active_channel":active.id,"active_channel_name":active.name,
        "active_channel_archived":active.archived,
        "huddle_roster":roster
    })
}

pub(crate) async fn workspace(requested: Option<String>, key: String) -> Result<Value, String> {
    let (names, handle) = reader(&key).await;
    let sidebar = host::read_sidebar_now(0, 0, &handle).await?;
    let selected = requested
        .as_deref()
        .and_then(|id| sidebar.channels.iter().find(|row| row.id == id))
        .or_else(|| landing(&sidebar.channels));
    let Some(selected) = selected else {
        return Ok(data(sidebar.channels, None, json!([])));
    };
    let id = selected.id.clone();
    let facts = facts(&id, &names).await?;
    let (active, roster) = facts.ok_or("selected channel disappeared during loading")?;
    Ok(data(sidebar.channels, Some(active), roster))
}

pub(crate) async fn window(id: String, key: String) -> Result<Value, String> {
    let (names, _) = reader(&key).await;
    let facts = facts(&id, &names).await?;
    let Some((active, roster)) = facts else {
        return workspace(None, key).await;
    };
    Ok(data(vec![active.clone()], Some(active), roster))
}

pub(crate) async fn channel(id: String, key: String, snapshot: Value) -> Result<Value, String> {
    let names = host::cached_names(snapshot)?;
    let handle = names
        .account_of(&key)
        .map(|number| format!("acct:{number}"))
        .unwrap_or_else(|| format!("user:{key}"));
    host::seat_reader(&handle, &key);
    let result = facts(&id, &names).await?;
    Ok(json!({"channel":result}))
}

/// The shell retains channel rows and unread heads, never message bodies.
pub(crate) async fn delta(
    payload: Value,
    assigned: Option<Value>,
    key: String,
    names: Value,
) -> Result<Value, String> {
    let operation = payload
        .as_object()
        .ok_or("Chat operation is not an object")?;
    let single = operation.len() == 1;
    if !single {
        return Err("Chat operation must name one action".into());
    }
    let (kind, body) = operation.iter().next().unwrap();
    let stamp = assigned.unwrap_or(Value::Null);
    if kind == "post_message" {
        let channel_id = body["channel_id"].as_str().ok_or("post has no channel")?;
        let seq = stamp["posted"]["seq"]
            .as_u64()
            .ok_or("post has no assigned sequence")?;
        let seq = i64::try_from(seq).unwrap_or(i64::MAX);
        return Ok(json!({"delta":{"head":{"channel_id":channel_id,"seq":seq}}}));
    }
    let id = match kind.as_str() {
        "create_dm_channel" => stamp["dm_channel"]["channel_id"].as_str(),
        "create_channel"
        | "create_voice_channel"
        | "rename_channel"
        | "set_channel_archived"
        | "join_huddle"
        | "leave_huddle" => body["channel_id"].as_str(),
        _ => return Ok(json!({"delta":null})),
    }
    .ok_or("channel operation has no channel")?;
    let result = channel(id.into(), key, names).await?;
    let row = result["channel"][0].clone();
    if row.is_null() {
        return Err("changed channel disappeared".into());
    }
    Ok(json!({"delta":{"channel":{"channel":row}}}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_cold_start_lands_on_a_room_with_something_in_it() {
        let channel = |id: &str, head: i64, archived: bool| ChatChannel {
            id: id.into(),
            name: id.into(),
            archived,
            members_only: false,
            huddle_count: 0,
            voice: false,
            huddle: Vec::new(),
            head_seq: head,
        };
        let landing = |channels: &[ChatChannel]| {
            super::landing(channels)
                .map(|channel| channel.id.clone())
                .unwrap_or_default()
        };

        // The demo's own shape: the empty room sorts first by ID.
        let demo = vec![
            channel("channel-1786073", 0, false),
            channel("engineering", 46, false),
            channel("general", 9, false),
        ];
        assert_eq!(landing(&demo), "engineering");

        // An archived room is not a landing even when it is the only one with
        // traffic — you cannot post into it.
        let archived_history = vec![channel("archive", 500, true), channel("general", 0, false)];
        assert_eq!(landing(&archived_history), "general");

        // Every room empty, and every room archived: still land somewhere.
        assert_eq!(
            landing(&[channel("a", 0, false), channel("b", 0, false)]),
            "a"
        );
        assert_eq!(
            landing(&[channel("a", 0, true), channel("b", 5, true)]),
            "a"
        );
        assert_eq!(landing(&[]), "");
    }
}
