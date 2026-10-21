//! What one attribution change asks of the bridge: a chat message to answer in its thread, a
//! wake's result, or nothing, with the reason.
use serde_json::{Value, json};

use super::chain::Chain;
use super::kept::{Input, Where};
use super::{ATTRIBUTION, CHAT, WAKE};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Heard {
    Input { at: Where, input: Input },
    Woke { wake: u64, input: Input },
    Ignored(String),
}

pub(crate) fn answer_key(account: u64, channel: &str, seq: u64) -> String {
    format!("bridge-{account}-{channel}-{seq}")
}

pub(crate) async fn fetch<C: Chain>(chain: &C, seq: u64) -> Result<Heard, String> {
    let reply = chain
        .query(ATTRIBUTION, json!({ "change": { "seq": seq } }))
        .await?;
    match &reply["change"] {
        Value::Null => Ok(Heard::Ignored("attribution holds no such change".into())),
        change => classify(chain, change.clone()).await,
    }
}

pub(crate) async fn classify<C: Chain>(chain: &C, change: Value) -> Result<Heard, String> {
    let seq = change["seq"].as_u64().unwrap_or_default();
    let source = &change["source"];
    let program = source["program"].as_str().unwrap_or_default();
    let kind = source["object"]["kind"].as_str().unwrap_or_default();
    let id = source["object"]["id"].as_str().unwrap_or_default();
    if change["kind"] != json!("added") {
        return Ok(Heard::Ignored(format!(
            "{program}'s {kind} {id} was not added for this account"
        )));
    }
    match (program, kind) {
        (WAKE, WAKE) => Ok(woke(chain.account(), seq, id, &change["detail"])),
        (CHAT, "message") => message(chain, seq, id).await,
        _ => Ok(Heard::Ignored(format!(
            "{program}'s {kind} {id} names no chat thread to answer in"
        ))),
    }
}

fn woke(account: u64, seq: u64, id: &str, detail: &Value) -> Heard {
    let Ok(wake) = id.parse::<u64>() else {
        return Heard::Ignored(format!("wake {id} is not a wake id"));
    };
    let detail = detail
        .as_str()
        .and_then(|detail| hex::decode(detail).ok())
        .unwrap_or_default();
    let input = Input {
        seq,
        key: format!("bridge-{account}-wake-{wake}"),
        text: String::from_utf8_lossy(&detail).into_owned(),
    };
    Heard::Woke { wake, input }
}

async fn message<C: Chain>(chain: &C, seq: u64, id: &str) -> Result<Heard, String> {
    let me = chain.account();
    let reply = chain
        .query(CHAT, json!({ "message_by_id": { "message_id": id } }))
        .await?;
    let row = &reply["message"];
    if row.is_null() || row["deleted"] == json!(true) {
        return Ok(Heard::Ignored(format!("message {id} is gone")));
    }
    let Some(author) = row["author"]["account"].as_u64() else {
        return Ok(Heard::Ignored(format!(
            "message {id} was not written by an account"
        )));
    };
    if author == me {
        return Ok(Heard::Ignored(format!(
            "message {id} is this account's own"
        )));
    }
    let (Some(channel), Some(at)) = (row["channel_id"].as_str(), row["seq"].as_u64()) else {
        return Err(format!("chat's message {id} reads with no channel or seq"));
    };
    let hears = chain
        .query(
            chain.identity(),
            json!({ "hears": { "account": me, "from": author } }),
        )
        .await?;
    if hears["hears"] != json!(true) {
        return Ok(Heard::Ignored(format!(
            "this account does not answer account {author}"
        )));
    }
    let thread = row["thread"].as_u64();
    let room = match thread {
        Some(root) => format!("thread {root} of #{channel}"),
        None => format!("#{channel}"),
    };
    let text = row["text"].as_str().unwrap_or_default();
    Ok(Heard::Input {
        at: Where {
            channel: channel.to_owned(),
            thread: thread.unwrap_or(at),
        },
        input: Input {
            seq,
            key: answer_key(me, channel, at),
            text: format!("<@{author}> wrote message {at} in {room}:\n{text}"),
        },
    })
}

pub(crate) async fn answered<C: Chain>(chain: &C, key: &str) -> Result<bool, String> {
    let reply = chain
        .query(CHAT, json!({ "message_by_id": { "message_id": key } }))
        .await?;
    Ok(!reply["message"].is_null())
}
