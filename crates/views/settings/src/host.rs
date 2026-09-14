//! What the view asks of the host kernel, and the readings it folds off what
//! it reads for itself.
//!
//! The kernel pushes SESSION facts only (`settings.props`): the colour mode,
//! whether there is a connection and what the titlebar calls it, whether the
//! signing seat is held, and the state of the wallet/account machinery that
//! is the kernel's alone — the keystore, an account ceremony in flight, the
//! ticket one minted. The password and the key bytes never leave the app; the
//! SEATED public key does, because the view reads its own account with it.
//!
//! Everything else is the view's own read, through the kernel contract: this
//! node's standing on the network (`rpc.status` + `rpc.query` on `valset` and
//! `runs`, re-read on every `rpc.live` hit for the valset plane) and the key
//! associations of the account the seat belongs to (`rpc.query` on
//! `identity`, re-read on every identity block).
//!
//! Every act still leaves as an intent: creating an account, minting a
//! ticket, registering a passkey, unlocking or locking the seat are the
//! KERNEL's operations — the view presses, the host signs.

use std::fmt::Write as _;

use ducktape_view_guest::host;
use futures::{StreamExt, stream};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HostError {
    pub message: String,
}

/// One key association as the account card lists it: the scheme token the
/// CLI prints, the hex key and the label ("" when none).
#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct AccountKeyRow {
    pub scheme: String,
    pub pubkey: String,
    pub label: String,
}

// ---------- the session ----------

/// The session facts the kernel pushes, one item per change. The signing seat
/// crosses as `unlocked` plus `seat_key`, the seated key's PUBLIC half — the
/// password and the key bytes never leave the app, and the public half is
/// what the view resolves its own account by.
#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub dark: bool,
    pub connected: bool,
    pub loading: bool,
    pub status: String,
    pub busy: bool,
    pub recovering: bool,
    pub appearance: String,
    pub desktop_notifications: bool,
    /// what the host said about raising banners: `pending`, `ready`,
    /// `unbundled` (not an app bundle), `denied` (refused in System
    /// Settings) or `unavailable` (no notification service)
    pub desktop_notifications_host: String,
    pub unlocked: bool,
    pub seat_key: String,
    pub account_name: String,
    pub account_number: String,
    pub account_exists: bool,
    pub network_name: String,
    pub connected_rpc: String,
    pub account_ceremony_phase: String,
    pub account_ceremony_qr: String,
    pub account_ceremony_detail: String,
    pub account_ceremony_left: String,
    pub settings_key_state: String,
    pub settings_key_path: String,
    pub account_busy: bool,
    pub account_ticket: String,
}

/// One item of the session subscription: the facts, or why not.
#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct SessionItem {
    pub next: Session,
    pub error: String,
}

/// The session now, and again on every change the kernel sees.
pub fn session() -> ducktape_view_guest::Subscription<SessionItem> {
    ducktape_view_guest::Subscription::run(|| {
        host::subscribe("settings.props", &[]).map(|answer| {
            let read = answer.and_then(|bytes| {
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())
            });
            match read {
                Ok(next) => SessionItem {
                    next,
                    error: String::new(),
                },
                Err(error) => SessionItem {
                    next: Session::default(),
                    error,
                },
            }
        })
    })
}

/// The serial the reads are keyed by: it moves when the session comes up, so
/// a reconnect reads the standing and the account afresh.
pub fn connection_serial_after(was_connected: bool, connected: bool, serial: i64) -> i64 {
    let came_up = connected && !was_connected;
    match came_up {
        true => serial + 1,
        false => serial,
    }
}

// ---------- this node's standing ----------

/// What the network card says about this device: its standing, whether that
/// standing is a quorum seat, and the workspace's headcount.
#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct Standing {
    /// `validator` | `resident` | `guest`, or "" while the roster has not
    /// answered — the empty answer is load-bearing, see [`fold_standing`].
    pub tier: String,
    pub admin: bool,
    pub members_line: String,
}

/// One item of the standing subscription: the reading, whether the roster
/// answered at all, or why not.
#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct StandingItem {
    pub next: Standing,
    pub answered: bool,
    pub error: String,
}

/// This node's standing now and after every valset block.
pub fn standing(connection: i64) -> ducktape_view_guest::Subscription<StandingItem> {
    ducktape_view_guest::Subscription::run_with(connection, |_| {
        let live = host::subscribe("rpc.live", b"valset");
        stream::once(load_standing()).chain(live.then(|_| load_standing()))
    })
}

async fn load_standing() -> StandingItem {
    match read_standing().await {
        Ok(next) => StandingItem {
            next,
            answered: true,
            error: String::new(),
        },
        Err(error) => StandingItem {
            next: Standing::default(),
            answered: false,
            error,
        },
    }
}

async fn read_standing() -> Result<Standing, String> {
    let status = ask("rpc.status", &serde_json::json!({})).await?;
    let node_key = text(&status["public_key"]);
    let validators = seat_keys(&query_module("valset", &serde_json::json!("validators")).await?);
    let residents = seat_keys(&query_module("valset", &serde_json::json!("residents")).await?);
    Ok(fold_standing(
        &node_key,
        &validators,
        &residents,
        agent_count().await,
    ))
}

/// The registered agents. A node whose runs registry cannot answer counts
/// none rather than losing the whole card.
async fn agent_count() -> i64 {
    let ask = serde_json::json!({ "model": { "query": "agents" } });
    let Ok(reply) = query_module("runs", &ask).await else {
        return 0;
    };
    fold_agent_count(&reply)
}

pub fn fold_agent_count(reply: &serde_json::Value) -> i64 {
    let agents = reply["model"]["agents"].as_array().map_or(0, Vec::len);
    i64::try_from(agents).unwrap_or(i64::MAX)
}

/// A valset key list — `{"validators": [[byte, …], …]}` — as hex.
pub fn seat_keys(reply: &serde_json::Value) -> Vec<String> {
    let named = ["validators", "residents"]
        .into_iter()
        .find(|name| reply[name].is_array());
    let Some(name) = named else {
        return Vec::new();
    };
    reply[name]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|key| hex_encode(&json_bytes(key)))
        .collect()
}

/// The roster as the network card reads it.
///
/// AN UNANSWERED ROSTER IS NOT A GUEST. Folding silence into `guest` told a
/// validator's operator — with no error anywhere on screen — that this device
/// may not post. A roster that DID answer always carries the chain's own
/// validators, so an empty pair of lists is silence and reads `""`; a roster
/// that answered and holds no seat for this node is a real guest.
pub fn fold_standing(
    node_key: &str,
    validators: &[String],
    residents: &[String],
    agents: i64,
) -> Standing {
    let humans = validators.len() + residents.len();
    let silent = humans == 0;
    let admin = validators.iter().any(|key| key == node_key);
    let resident = residents.iter().any(|key| key == node_key);
    let tier = match (silent, admin, resident) {
        (true, _, _) => String::new(),
        (false, true, _) => "validator".into(),
        (false, false, true) => "resident".into(),
        (false, false, false) => "guest".into(),
    };
    Standing {
        tier,
        admin,
        members_line: headcount(i64::try_from(humans).unwrap_or(i64::MAX), agents),
    }
}

/// `N humans · M agents` — the workspace shows people AND machines.
pub fn headcount(humans: i64, agents: i64) -> String {
    format!(
        "{} · {}",
        plural(humans, "human", "humans"),
        plural(agents, "agent", "agents")
    )
}

fn plural(count: i64, one: &str, many: &str) -> String {
    match count == 1 {
        true => format!("{count} {one}"),
        false => format!("{count} {many}"),
    }
}

// ---------- the account's keys ----------

/// One item of the key-association subscription.
#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct KeysItem {
    pub rows: Vec<AccountKeyRow>,
    pub answered: bool,
    pub error: String,
}

/// The key associations of the account the seat belongs to, now and after
/// every identity block — an op that admits or drops a key lands on that
/// plane, so the card re-reads itself without the host saying so.
pub fn account_keys(connection: i64, seat: String) -> ducktape_view_guest::Subscription<KeysItem> {
    ducktape_view_guest::Subscription::run_with((connection, seat), |(_, seat)| {
        let seat = seat.clone();
        let again = seat.clone();
        let live = host::subscribe("rpc.live", b"identity");
        stream::once(load_keys(seat)).chain(live.then(move |_| load_keys(again.clone())))
    })
}

async fn load_keys(seat: String) -> KeysItem {
    match read_keys(&seat).await {
        Ok(rows) => KeysItem {
            rows,
            answered: true,
            error: String::new(),
        },
        Err(error) => KeysItem {
            rows: Vec::new(),
            answered: false,
            error,
        },
    }
}

async fn read_keys(seat: &str) -> Result<Vec<AccountKeyRow>, String> {
    let key = hex_decode(seat);
    if key.is_empty() {
        return Ok(Vec::new());
    }
    let ask = serde_json::json!({ "of_key": { "key": key } });
    Ok(fold_key_rows(&query_module("identity", &ask).await?))
}

/// The identity module's `Account(Some)` reply as the card's key rows,
/// ascending by public key the way the module stores them. A reply naming no
/// account — this key belongs to none yet — lists nothing.
pub fn fold_key_rows(reply: &serde_json::Value) -> Vec<AccountKeyRow> {
    reply["account"]["keys"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|key| AccountKeyRow {
            scheme: text(&key["scheme"]),
            pubkey: hex_encode(&json_bytes(&key["pubkey"])),
            label: text(&key["label"]),
        })
        .collect()
}

// ---------- asking the kernel ----------

/// One kernel request, asked and answered as JSON.
async fn ask(kind: &str, request: &serde_json::Value) -> Result<serde_json::Value, String> {
    let reply = host::request(kind, &serde_json::to_vec(request).expect("encodes")).await?;
    serde_json::from_slice(&reply).map_err(|error| error.to_string())
}

/// One module query on the connected node.
async fn query_module(
    target: &str,
    query: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    ask(
        "rpc.query",
        &serde_json::json!({ "target": target, "query": query }),
    )
    .await
}

fn text(value: &serde_json::Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

/// A serde `Vec<u8>` as it arrives over JSON: an array of numbers.
fn json_bytes(value: &serde_json::Value) -> Vec<u8> {
    value
        .as_array()
        .map(|bytes| {
            bytes
                .iter()
                .filter_map(|byte| byte.as_u64().map(|byte| byte as u8))
                .collect()
        })
        .unwrap_or_default()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

/// A hex key back to its bytes. A string that is not whole pairs of hex
/// decodes to nothing, and a read with no key asks for nothing.
fn hex_decode(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text.bytes().collect();
    if !digits.len().is_multiple_of(2) {
        return Vec::new();
    }
    let mut bytes = Vec::with_capacity(digits.len() / 2);
    for pair in digits.chunks(2) {
        let high = (pair[0] as char).to_digit(16);
        let low = (pair[1] as char).to_digit(16);
        let (Some(high), Some(low)) = (high, low) else {
            return Vec::new();
        };
        bytes.push((high * 16 + low) as u8);
    }
    bytes
}

/// `settings.tab` — open another rail tab (`members`, `node`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tab {
    pub tab: String,
}

/// `settings.unlock` — verify the key password and keep the signing seat.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unlock {
    pub password: String,
}

/// `settings.rename` and `settings.create` — an account name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Name {
    pub name: String,
}

/// `settings.key_add` — mint a ticket for another device's key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyAdd {
    pub pubkey: String,
    pub label: String,
}

/// `settings.join` — join the account a ticket was minted for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Join {
    pub ticket: String,
}

/// `settings.key_remove` — drop one key association.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRemove {
    pub pubkey: String,
}

/// `settings.passkey`, `settings.passkey_desktop`, `settings.wallet` — the
/// label the new key is admitted under.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    pub label: String,
}

/// `settings.copy` — the host puts `text` on the clipboard and toasts `label`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Copy {
    pub text: String,
    pub label: String,
}

/// `settings.notifications` — desktop banners on or off.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notifications {
    pub enabled: bool,
}

pub fn open_tab(tab: &str) -> bool {
    notify("settings.tab", &Tab { tab: tab.into() })
}

pub fn reconnect_network() -> bool {
    notify("settings.reconnect", &())
}

pub fn switch_workspace() -> bool {
    notify("settings.switch_network", &())
}

pub fn unlock(password: &str) -> bool {
    notify(
        "settings.unlock",
        &Unlock {
            password: password.into(),
        },
    )
}

pub fn lock() -> bool {
    notify("settings.lock", &())
}

pub fn rename_account(name: &str) -> bool {
    notify("settings.rename", &Name { name: name.into() })
}

pub fn create_account(name: &str) -> bool {
    notify("settings.create", &Name { name: name.into() })
}

pub fn mint_ticket(pubkey: &str, label: &str) -> bool {
    notify(
        "settings.key_add",
        &KeyAdd {
            pubkey: pubkey.into(),
            label: label.into(),
        },
    )
}

pub fn join_account(ticket: &str) -> bool {
    notify(
        "settings.join",
        &Join {
            ticket: ticket.into(),
        },
    )
}

pub fn remove_key(pubkey: &str) -> bool {
    notify(
        "settings.key_remove",
        &KeyRemove {
            pubkey: pubkey.into(),
        },
    )
}

pub fn add_passkey(label: &str) -> bool {
    notify(
        "settings.passkey",
        &Label {
            label: label.into(),
        },
    )
}

pub fn add_passkey_here(label: &str) -> bool {
    notify(
        "settings.passkey_desktop",
        &Label {
            label: label.into(),
        },
    )
}

pub fn cancel_ceremony() -> bool {
    notify("settings.ceremony_cancel", &())
}

pub fn link_wallet(label: &str) -> bool {
    notify(
        "settings.wallet",
        &Label {
            label: label.into(),
        },
    )
}

pub fn login() -> bool {
    notify("settings.login", &())
}

pub fn copy(text: &str, label: &str) -> bool {
    notify(
        "settings.copy",
        &Copy {
            text: text.into(),
            label: label.into(),
        },
    )
}

pub fn set_light() -> bool {
    notify("settings.light", &())
}

pub fn set_dark() -> bool {
    notify("settings.dark", &())
}

pub fn set_notifications(enabled: bool) -> bool {
    notify("settings.notifications", &Notifications { enabled })
}

fn notify<T: Serialize>(operation: &str, payload: &T) -> bool {
    let bytes = serde_json::to_vec(payload).expect("an intent encodes");
    host::notify(operation, &bytes);
    true
}

/// The draft as it stands, or nothing once the read it targeted moved.
pub fn keep_draft(consumed: bool, draft: &str) -> String {
    match consumed {
        true => String::new(),
        false => draft.into(),
    }
}

/// A RENAME LANDED when the account the session reports carries the name that
/// was sent. The op itself is the kernel's, so this is the only signal the
/// view has that it took — and it is the right one: the card is showing the
/// new name, so the draft that asked for it is spent.
pub fn renamed_to(account_name: &str, sent: &str) -> bool {
    !sent.is_empty() && account_name.trim() == sent.trim()
}
