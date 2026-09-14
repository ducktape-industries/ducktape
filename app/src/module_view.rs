//! Module-owned views. A Rust WASM screen built from `crates/views` is
//! loaded either from the deployed artifact of the module it belongs to
//! (`backend::view_source`: the registry's ACTIVE code hash, fetched and
//! verified, never a desktop substitute) or, for the desktop's own views,
//! FROM A FILE beside the binary (`make views` stages
//! `target/views/<module>_view.wasm`; `DUCKTAPE_VIEWS_DIR` overrides); it is
//! ticked inside a fuel and time budget, and presented through native
//! gpui-kit controls in its tab.
//!
//! The boundary is the screen component's own contract. Its props go in as
//! JSON, one item per change, on the guest's `<module>.props` subscription;
//! its emits come out as [`ModuleViewEvent`] intents or kernel operations.
//! The host authorizes and signs writes. Props may carry public connection
//! context, but the guest receives no signing secret or direct OS clock.
//! A view that traps shows why in its place instead of taking the window with it.

mod kernel;

pub use kernel::{block_hit as view_block_hit, live_hit as view_live_hit};

pub(crate) fn runtime() -> tokio::runtime::Handle {
    kernel::runtime()
}

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::editor::wire::EditorStore;
use gpui_kit::AppContext as _;
use pictures::Pictures;
use ui_lang_wire as wire;
use wasmtime::component::{Component, Linker, TypedFunc};
use wasmtime::{
    Cache, CacheConfig, Config, Engine, OptLevel, Store, StoreContextMut, StoreLimits,
    StoreLimitsBuilder,
};

/// What a module view asked the app to do: `kind` is the operation
/// (`vote`, `execute`), `detail` the guest's JSON for it.
#[derive(Clone, Debug, Hash, PartialEq, Default)]
pub struct ModuleViewEvent {
    pub kind: String,
    pub detail: String,
}

/// What one call into a view may spend: about a 60 Hz frame of
/// instructions, and a wall-clock deadline for the time an import takes
/// that fuel cannot see.
const FUEL_PER_TICK: u64 = 100_000_000;
const TICK_DEADLINE: Duration = Duration::from_millis(100);
const EPOCH_TICK: Duration = Duration::from_millis(10);
const MEMORY_LIMIT: usize = 64 << 20;
const MAX_MODULE_BYTES: u64 = 64 << 20;
/// A frame the view sends past this ends it: nothing a screen needs is
/// megabytes, and the host would decode all of it on the window thread.
const MAX_FRAME_BYTES: usize = 8 << 20;
const MAX_REQUESTS_PER_TICK: usize = 256;
const MAX_PAYLOAD_BYTES: usize = 1 << 20;
/// A never-valid view may hold its replacement off for this long. Once a
/// guest publishes a valid tree, neither pending work nor a later trap
/// authorizes discarding its state.
const REPLACEMENT_WAIT: Duration = Duration::from_secs(30);
/// The gap before the first re-attempt at a candidate whose load failed,
/// and the longest that gap widens to. A load fetches the artifact,
/// instantiates it, restores the snapshot and verifies the first tree —
/// and compiles the component too, whenever `compiled_view` does not
/// already hold it. A deployment that cannot load fails that way every
/// time, so trying it once a block buys nothing and never stops. Widening
/// the gap rather than giving up keeps a fetch that failed on the
/// transport recoverable: nothing is suppressed for good, only spaced out.
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(60);

// ---------- the Approvals seat ----------

/// The Approvals tab, drawn by the `governance` view over the KERNEL
/// CONTRACT: the app pushes session facts only, the view reads its own
/// register through `rpc.query` / `rpc.blocks` / `rpc.live`, and a vote or
/// a settle comes back as `op.submit`, signed here with the seated key. The
/// one event the app hears is the kernel's `badge` (the tab's open count).
pub fn governance_view(dark: bool, connected: bool, admin: bool) -> ViewSpec {
    let props = serde_json::json!({
        "admin": admin,
        "connected": connected,
        "dark": dark,
    });
    module_view(
        "governance",
        serde_json::to_vec(&props).expect("props encode"),
    )
}

// ---------- the roster seats ----------

/// The Members tab, on the KERNEL CONTRACT: session facts go in, the view
/// reads the roster itself off the node and signs its writes through
/// `op.submit`. The one intent left is `copy` (`text`, `label`) — the
/// clipboard is an OS door the kernel has not opened.
pub fn members_view(dark: bool, connected: bool, admin: bool) -> ViewSpec {
    let props = serde_json::json!({
        "admin": admin,
        "connected": connected,
        "dark": dark,
    });
    module_view("members", serde_json::to_vec(&props).expect("props encode"))
}

/// The Agents tab, drawn by the `agents` view over the KERNEL CONTRACT:
/// the app pushes session facts only — connected, dark, the signing
/// account (`account`, its decimal number) and the run another tab opened
/// for the reader (`open_run`, its dispatch id; `opened` counts the doors)
/// — and the view reads the register, the run tracker and one run's
/// journal for itself through `rpc.query` / `rpc.view`, re-reading on
/// every `rpc.live` hit for the `runs` and `identity` planes. A pause or a
/// save leaves as `op.submit`, signed here with the seated key.
///
/// What still comes back as an intent: `badge` (how many of its agents are
/// working — the rail's pulse), `register` (a new agent, whose program
/// account only the app can provision), `open_run` (`dispatch_id`, "" to
/// close) and `open_link` (`url`, a chip's duck:// address).
pub fn agents_view(
    dark: bool,
    connected: bool,
    account: &str,
    open_run: &str,
    opened: i64,
) -> ViewSpec {
    let props = serde_json::json!({
        "account": account,
        "open_run": open_run,
        "opened": opened,
        "connected": connected,
        "dark": dark,
    });
    module_view("agents", serde_json::to_vec(&props).expect("props encode"))
}

pub fn agents_intent(event: &ModuleViewEvent) -> crate::AgentsIntent {
    match event.kind.as_str() {
        "register" => crate::AgentsIntent::Register,
        "open_run" => crate::AgentsIntent::OpenRun,
        "open_link" => crate::AgentsIntent::OpenLink,
        _ => crate::AgentsIntent::Badge,
    }
}

/// The string under `field` in an intent's JSON detail; empty when absent.
pub fn event_text(event: &ModuleViewEvent, field: &str) -> String {
    detail(event)
        .and_then(|detail| detail.get(field)?.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The flag under `field` in an intent's JSON detail; false when absent.
pub fn event_flag(event: &ModuleViewEvent, field: &str) -> bool {
    detail(event)
        .is_some_and(|detail| detail.get(field).and_then(serde_json::Value::as_bool) == Some(true))
}

fn detail(event: &ModuleViewEvent) -> Option<serde_json::Value> {
    serde_json::from_str(&event.detail).ok()
}

/// The integer under `field` in an intent's JSON detail; 0 when absent.
pub fn event_int(event: &ModuleViewEvent, field: &str) -> i64 {
    detail(event)
        .and_then(|detail| detail.get(field).and_then(serde_json::Value::as_i64))
        .unwrap_or_default()
}

/// The number under `field` in an intent's JSON detail; 0 when absent.
pub fn event_num(event: &ModuleViewEvent, field: &str) -> f64 {
    detail(event)
        .and_then(|detail| detail.get(field).and_then(serde_json::Value::as_f64))
        .unwrap_or_default()
}

// ---------- the node seat ----------

/// The Node tab, drawn by the `node` view over the KERNEL CONTRACT: the app
/// pushes SESSION FACTS ONLY — connected, dark, this seat's admin standing
/// and tier, the app's own connection reading, the workspace directory the
/// daemon runs out of, and the wall clock. Not one of those is on a `/v1`
/// route, which is why they cross; everything that is (the status facts, the
/// peers sample, the code registry, and the node's own log ring) the view
/// reads for itself through `rpc.status` / `rpc.peers` / `rpc.query` /
/// `rpc.stream`. The live tracing filter goes back as `rpc.admin`, and the
/// one intent that comes back is `copy` — the clipboard is an OS door.
#[allow(
    clippy::too_many_arguments,
    reason = "the caller supplies each session property explicitly"
)]
pub fn node_view(
    dark: bool,
    connected: bool,
    admin: bool,
    tier: &str,
    status: &str,
    data_dir: &str,
    wall_now: i64,
) -> ViewSpec {
    let props = serde_json::json!({
        "connected": connected,
        "dark": dark,
        "admin": admin,
        "tier": tier,
        "status": status,
        "data_dir": data_dir,
        "wall_now": wall_now,
    });
    module_view("node", serde_json::to_vec(&props).expect("props encode"))
}

// ---------- the explorer seat ----------

/// The Explorer tab, drawn by the `explorer` view over the KERNEL CONTRACT:
/// the app pushes session facts only — connected, dark, and the two node
/// facts the titlebar already holds, so the screen's head and the titlebar's
/// cannot disagree. The view reads the block window itself through
/// `rpc.blocks` (re-read on `rpc.live` for the `block` plane) and runs the
/// workspace search over `rpc.query` / `rpc.view`. The one intent that comes
/// back is `copy` (`text`, `label`) — the clipboard is an OS door.
pub fn explorer_view(dark: bool, connected: bool, head: i64, sync_line: &str) -> ViewSpec {
    let props = serde_json::json!({
        "connected": connected,
        "dark": dark,
        "head": head,
        "sync_line": sync_line,
    });
    module_view(
        "explorer",
        serde_json::to_vec(&props).expect("props encode"),
    )
}

// ---------- the settings seat ----------

/// The Settings tab, drawn by the `settings` view over the KERNEL CONTRACT.
///
/// What crosses is SESSION facts: the colour mode, whether there is a
/// connection and what the titlebar calls it (so the screen and the titlebar
/// cannot disagree), whether the signing seat is held, and the state of the
/// wallet/account machinery that is the kernel's alone — the keystore's
/// reading, a browser ceremony in flight, the ticket one minted, and the
/// account this device belongs to, which the rail, the bell and the agents
/// view all read too. The password and the key bytes never cross; the seated
/// key's PUBLIC half does, because the view resolves its own account by it.
///
/// What does NOT cross is what the view reads for itself: this node's
/// standing on the network and the account's key associations.
///
/// Its intents come back one per act (`settings_intent`), carrying only what
/// the reader typed — creating an account, minting a ticket, registering a
/// passkey, unlocking and locking the seat are operations the kernel signs.
#[allow(
    clippy::too_many_arguments,
    reason = "the caller supplies each session property explicitly"
)]
pub fn settings_view(
    dark: bool,
    connected: bool,
    loading: bool,
    status: &str,
    mutation_phase: crate::MutationPhase,
    appearance: crate::Appearance,
    desktop_notifications: bool,
    password: &str,
    seat_key: &str,
    account_name: &str,
    network_name: &str,
    connected_rpc: &str,
    account_ceremony_phase: &str,
    account_ceremony_qr: &str,
    account_ceremony_detail: &str,
    account_ceremony_left: &str,
    settings_key_state: &str,
    settings_key_path: &str,
    account_number: &str,
    account_exists: bool,
    account_busy: bool,
    account_ticket: &str,
) -> ViewSpec {
    let appearance = match appearance {
        crate::Appearance::System => "system",
        crate::Appearance::Light => "light",
        crate::Appearance::Dark => "dark",
    };
    let props = serde_json::json!({
        "dark": dark,
        "connected": connected,
        "loading": loading,
        "status": status,
        "busy": mutation_phase != crate::MutationPhase::Idle,
        "recovering": mutation_phase == crate::MutationPhase::Recovering,
        "appearance": appearance,
        "desktop_notifications": desktop_notifications,
        "unlocked": !password.is_empty(),
        "seat_key": seat_key,
        "account_name": account_name,
        "network_name": network_name,
        "connected_rpc": connected_rpc,
        "account_ceremony_phase": account_ceremony_phase,
        "account_ceremony_qr": account_ceremony_qr,
        "account_ceremony_detail": account_ceremony_detail,
        "account_ceremony_left": account_ceremony_left,
        "settings_key_state": settings_key_state,
        "settings_key_path": settings_key_path,
        "account_number": account_number,
        "account_exists": account_exists,
        "account_busy": account_busy,
        "account_ticket": account_ticket,
    });
    module_view(
        "settings",
        serde_json::to_vec(&props).expect("props encode"),
    )
}

/// Construct an intent without mounting a view. The `kind` is the
/// same string the guest emits, so a scenario names the act and not an enum the
/// intent mapping could drift from.
#[cfg(test)]
pub fn view_event(kind: String, detail: String) -> ModuleViewEvent {
    ModuleViewEvent { kind, detail }
}

pub fn settings_intent(event: &ModuleViewEvent) -> crate::SettingsIntent {
    use crate::SettingsIntent as Intent;
    match event.kind.as_str() {
        "tab" => Intent::Tab,
        "reconnect" => Intent::Reconnect,
        "switch_network" => Intent::SwitchNetwork,
        "unlock" => Intent::Unlock,
        "lock" => Intent::Lock,
        "rename" => Intent::Rename,
        "create" => Intent::Create,
        "key_add" => Intent::KeyAdd,
        "join" => Intent::Join,
        "key_remove" => Intent::KeyRemove,
        "passkey" => Intent::Passkey,
        "passkey_desktop" => Intent::PasskeyDesktop,
        "ceremony_cancel" => Intent::CeremonyCancel,
        "wallet" => Intent::Wallet,
        "login" => Intent::Login,
        "light" => Intent::Light,
        "dark" => Intent::Dark,
        "notifications" => Intent::Notifications,
        _ => Intent::Copy,
    }
}

/// The rail tab a `tab` intent names; the two the settings cards link to.
pub fn settings_event_tab(event: &ModuleViewEvent) -> crate::ShellTab {
    match event_text(event, "tab").as_str() {
        "members" => crate::ShellTab::Members,
        _ => crate::ShellTab::Node,
    }
}

// ---------- the forge seat ----------

/// The Forge tab: the session facts the `forge` view draws itself against,
/// and nothing else. The repo namespace, one repo's branches and tracker,
/// the open item with its patch, reviews and discussion, and the code
/// browse are the VIEW's own reads over the kernel contract. `link` is the
/// `duck://forge/...` the app's open plane last routed here, counted by
/// `link_tick` so the same address twice still lands.
#[allow(
    clippy::too_many_arguments,
    reason = "the caller supplies each session property explicitly"
)]
pub fn forge_view(
    dark: bool,
    connected: bool,
    org: &str,
    about: &str,
    tier: &str,
    network_chain_id: &str,
    connected_rpc: &str,
    link: &str,
    link_tick: i64,
) -> ViewSpec {
    let props = serde_json::json!({
        "connected": connected,
        "dark": dark,
        "org": org,
        "about": about,
        "tier": tier,
        "network_chain_id": network_chain_id,
        "connected_rpc": connected_rpc,
        "link": link,
        "link_tick": link_tick,
    });
    module_view("forge", serde_json::to_vec(&props).expect("props encode"))
}

/// The two OS doors the forge view asks the app for, plus its host
/// composer's send. Everything else it does itself.
pub fn forge_intent(event: &ModuleViewEvent) -> crate::ForgeIntent {
    use crate::ForgeIntent as Intent;
    match event.kind.as_str() {
        "open_link" => Intent::OpenLink,
        "composer" => Intent::Composer,
        "composer_attach" => Intent::Attach,
        _ => Intent::Copy,
    }
}

// ---------- the pages seat ----------

/// Pages speaks the KERNEL CONTRACT: session facts go in — the chain the
/// titlebar names, because a `duck://page/…` address carries it, and the page
/// a link asked the app to open — and the view reads the workspace, the
/// document and its comments off the node itself and signs its writes through
/// `op.submit`. The one event back is the kernel's `badge`, plus the two OS
/// doors (the clipboard and the open plane) the app still owns.
pub fn pages_view(
    dark: bool,
    connected: bool,
    network_chain_id: &str,
    route_page: &str,
    route_serial: i64,
) -> ViewSpec {
    let props = serde_json::json!({
        "dark": dark,
        "connected": connected,
        "chain": network_chain_id,
        "route_page": route_page,
        "route_serial": route_serial,
    });
    module_view("pages", serde_json::to_vec(&props).expect("props encode"))
}

/// The two OS doors the pages view still asks the app for: a `duck://`
/// address pressed in a document goes through the ONE open plane, and a
/// clipboard copy is the clipboard.
pub fn pages_intent(event: &ModuleViewEvent) -> crate::PagesIntent {
    use crate::PagesIntent as Intent;
    match event.kind.as_str() {
        "open_link" => Intent::OpenLink,
        _ => Intent::Copy,
    }
}

// ---------- the chat seat ----------

/// The chat view's session facts, as one document — a struct rather than a
/// `json!` literal because the macro recurses once per field.
#[derive(serde::Serialize)]
struct ChatProps<'a> {
    dark: bool,
    connected: bool,
    endpoint: &'a str,
    network_name: &'a str,
    network_chain_id: &'a str,
    status: &'a str,
    block_height: i64,
    me: String,
    me_key: &'a str,
    names_serial: i64,
    rooms: &'a [crate::backend::ChatSidebarRow],
    dm_rows: &'a [crate::backend::DmSidebarRow],
    channel_create_open: bool,
    active_channel: &'a str,
    active_dm_peer: &'a str,
    active_dm: &'a crate::backend::DmPeer,
    land_seq: i64,
    unread_boundary: i64,
    busy: bool,
    loading: bool,
    huddle_joined: bool,
    huddle_channel: &'a str,
    huddle_channel_name: &'a str,
    huddle_joined_at: i64,
    huddle_now: i64,
    call_muted: bool,
    /// this side's mic voice gate, for the reader's own seat
    call_speaking: bool,
    /// the node keys of the peers whose beacons say they are talking
    speaking_peers: Vec<String>,
    shift_held: bool,
    copy_chord_serial: i64,
    sent_serial: i64,
    pending_sends: &'a [crate::backend::PendingSend],
    /// THIS ROOM'S runs only, as hints: the reading is taken for the whole
    /// node and cut to `active_channel` on the way out.
    live_agents: Vec<crate::backend::LiveRunHint>,
}

/// The Chat tab, drawn by the `chat` view over the KERNEL CONTRACT: the app
/// pushes session facts only — who the reader is, which room the app is in,
/// the sidebar the bell and the tray share, the huddle, the sends in flight —
/// and the view reads the room itself through `rpc.view` / `rpc.live`, writing
/// reactions, edits, deletes, renames and membership as `op.submit`.
///
/// What still comes back as an intent is what another plane of the app steers
/// or owns: the room to open (`duck://` links, notifications, the tray), the
/// huddle, a link or a copy, a run to stop or open, and the seed for the edit
/// composer — because the composers are HOST SURFACES (`chat_composer`), whose
/// submit arrives as `composer`.
#[allow(
    clippy::too_many_arguments,
    reason = "the caller supplies each screen property explicitly"
)]
pub fn chat_view(
    dark: bool,
    connected: bool,
    endpoint: &str,
    network_name: &str,
    network_chain_id: &str,
    status: &str,
    block_height: i64,
    account_number: &str,
    user_key: &str,
    names_serial: i64,
    rooms: &[crate::backend::ChatSidebarRow],
    dm_rows: &[crate::backend::DmSidebarRow],
    channel_create_open: bool,
    active_channel: &str,
    active_dm_peer: &str,
    active_dm: &crate::backend::DmPeer,
    land_seq: i64,
    unread_boundary: i64,
    mutation_phase: crate::MutationPhase,
    loading: bool,
    huddle_joined: bool,
    huddle_channel: &str,
    huddle_channel_name: &str,
    huddle_joined_at: i64,
    huddle_now: i64,
    call_muted: bool,
    call_speaking: bool,
    call_peers: &[crate::call::CallEvent],
    shift_held: bool,
    copy_chord_serial: i64,
    sent_serial: i64,
    pending_sends: &[crate::backend::PendingSend],
    live_agents: &[crate::backend::LiveAgentRow],
) -> ViewSpec {
    let props = ChatProps {
        dark,
        connected,
        endpoint,
        network_name,
        network_chain_id,
        status,
        block_height,
        me: reader_handle(account_number, user_key),
        me_key: user_key,
        names_serial,
        rooms,
        dm_rows,
        channel_create_open,
        active_channel,
        active_dm_peer,
        active_dm,
        land_seq,
        unread_boundary,
        busy: mutation_phase != crate::MutationPhase::Idle,
        loading,
        huddle_joined,
        huddle_channel,
        huddle_channel_name,
        huddle_joined_at,
        huddle_now,
        call_muted,
        call_speaking,
        speaking_peers: crate::call::speaking_peers(call_peers),
        shift_held,
        copy_chord_serial,
        sent_serial,
        pending_sends,
        // THIS IS THE ONLY PLACE A RUN IS MATCHED TO A ROOM: the reading
        // covers the whole node, so a row from a room the reader left cannot
        // reach the screen no matter which handler moved `active_channel`.
        live_agents: live_agents_within(live_agents, active_channel, LIVE_AGENT_TEXT_BUDGET),
    };
    module_view("chat", serde_json::to_vec(&props).expect("props encode"))
}

/// The reader as the chat index spells her: her account when identity has
/// resolved one, else the bare key she signs with. `reacted by me` and the
/// post gate hang on this, so it is spelled once, here.
fn reader_handle(account_number: &str, user_key: &str) -> String {
    match account_number.is_empty() {
        true => format!("user:{user_key}"),
        false => format!("acct:{account_number}"),
    }
}

/// Bytes the live agent cards may take on one frame. The wire spends 64 KiB
/// of text per frame and EMPTIES whatever comes after; the cards draw inside
/// the stream, so this ceiling is what keeps a room with a great many runs in
/// flight from blanking the messages they sit under.
const LIVE_AGENT_TEXT_BUDGET: usize = 6 << 10;

/// The bytes a live run hint puts on the wire as text.
fn live_text_bytes(hint: &crate::backend::LiveRunHint) -> usize {
    let activity: usize = hint.activity.iter().map(|act| act.label.len()).sum();
    hint.agent.len() + hint.status.len() + activity + hint.answer_preview.len()
}

/// The runs anchored in `channel_id` whose hints fit `budget`, newest anchor
/// kept first — those are the ones at the tail the reader is looking at — and
/// handed back in anchor order so the list does not churn the guest's timeline
/// memo when two runs start in the same poll.
fn live_agents_within(
    rows: &[crate::backend::LiveAgentRow],
    channel_id: &str,
    budget: usize,
) -> Vec<crate::backend::LiveRunHint> {
    let mut here: Vec<crate::backend::LiveRunHint> = rows
        .iter()
        .filter(|row| row.channel_id == channel_id)
        .map(crate::backend::LiveRunHint::from)
        .collect();
    here.sort_by_key(|hint| std::cmp::Reverse(hint.anchor_seq));
    let mut spent = 0;
    let mut kept = Vec::new();
    for hint in here {
        let cost = live_text_bytes(&hint);
        if spent + cost > budget {
            break;
        }
        spent += cost;
        kept.push(hint);
    }
    kept.sort_by_key(|hint| hint.anchor_seq);
    kept
}

/// The act a chat intent names. The door ([`intents_of`]) refuses every kind
/// this does not list, so the wildcard is unreachable in practice; it verdicts
/// the link copy, whose handler refuses an empty link.
pub fn chat_intent(event: &ModuleViewEvent) -> crate::ChatIntent {
    use crate::ChatIntent as Intent;
    match event.kind.as_str() {
        "open_hit" => Intent::OpenHit,
        "toggle_create" => Intent::ToggleCreate,
        "choose_channel" => Intent::ChooseChannel,
        "choose_dm" => Intent::ChooseDm,
        "show_huddle" => Intent::ShowHuddle,
        "leave_huddle" => Intent::LeaveHuddle,
        "join_huddle" => Intent::JoinHuddle,
        "join_voice" => Intent::JoinVoice,
        "scrolled" => Intent::Scrolled,
        "open_link" => Intent::OpenLink,
        "copy" => Intent::Copy,
        "copy_link" => Intent::CopyLink,
        "begin_edit" => Intent::BeginEdit,
        "cancel_run" => Intent::CancelRun,
        "open_run" => Intent::OpenRun,
        "composer" => Intent::Composer,
        "composer_attach" => Intent::Attach,
        _ => Intent::CopyLink,
    }
}

/// Which composer a `composer` intent came from.
pub fn chat_event_kind(event: &ModuleViewEvent) -> crate::ComposerKind {
    match event_text(event, "kind").as_str() {
        "reply" => crate::ComposerKind::Reply,
        "edit" => crate::ComposerKind::Edit,
        "thread_edit" => crate::ComposerKind::ThreadEdit,
        _ => crate::ComposerKind::Message,
    }
}

/// A refused or failed body, handed back to the composer it was written in.
pub fn chat_composer_unsent(scope: &str, text: &str, committed: bool) -> bool {
    crate::composer_surface::unsent(scope, text, committed);
    true
}

/// Open the native edit composer on the body the view handed over. The view
/// decides WHETHER a row is editable (it holds the revisions); what it cannot
/// do is type — the editor is a host surface with an IME and a retained
/// document — so the markdown it opens on crosses as this seed.
pub fn chat_composer_seed(scope: &str, body: &str) -> bool {
    crate::composer_surface::seed(scope, body);
    true
}

/// The room's explicit roster, as the app just read it, handed to the
/// composers over that room (and its threads) for their mention menu.
pub fn chat_composer_roster(scope: &str, members: &[crate::backend::ChatMember]) -> bool {
    crate::composer_surface::roster(scope, members);
    true
}

// ---------- the files seat ----------

/// The Files tab, drawn by the `files` view over the KERNEL CONTRACT: the app
/// pushes session facts only, and the view lists the directory, reads the
/// preview, walks the snapshot history and diffs a snapshot for itself through
/// `files.get` / `rpc.live`, writing through `op.submit` signed here with the
/// seated key. Two events come back, both OS doors the app owns: `open_link`
/// for a link the Markdown reader activated, and `at` naming the directory a
/// file dropped on the window lands in. The picture viewer, the highlighted
/// reader and the Markdown document are host surfaces defined in `surfaces.rs`.
///
/// `route` is the one navigation fact that cannot be the view's: a
/// `duck://files/<path>` link is resolved by the shell's link plane, which
/// also moves the tab, so the address arrives as a SESSION fact like any
/// other. `route_serial` counts the pushes, which is what makes the same path
/// twice a second navigation rather than a value that never changed.
pub fn files_view(
    dark: bool,
    connected: bool,
    chain: &str,
    route: &str,
    route_serial: i64,
) -> ViewSpec {
    let props = serde_json::json!({
        "connected": connected,
        "dark": dark,
        "chain": chain,
        "route": route,
        "route_serial": route_serial,
    });
    module_view(
        "files",
        serde_json::to_vec(&props).expect("files props encode"),
    )
}

/// The string argument at `index` of a surface's args; "" when the guest
/// sent something else.
fn surface_str(args: &[wire::SurfaceValue], index: usize) -> String {
    match args.get(index) {
        Some(wire::SurfaceValue::Str(text)) => text.clone(),
        _ => String::new(),
    }
}

fn surface_bool(args: &[wire::SurfaceValue], index: usize) -> bool {
    matches!(args.get(index), Some(wire::SurfaceValue::Bool(true)))
}

fn surface_allowed(module: &str, surface: &str) -> bool {
    match (module, surface) {
        (_, "artifact_svg" | "artifact_image") => true,
        ("chat", "chat_composer" | "picture") => true,
        ("forge", "forge_composer" | "picture" | "forge_markdown" | "forge_code") => true,
        ("files", "picture" | "forge_code" | "agent_markdown") => true,
        _ => false,
    }
}

/// The operations a view may ask of the app, by module. An intent outside
/// the list is refused at the door, never handed to a handler.
fn intents_of(module: &str) -> &'static [&'static str] {
    match module {
        // the governance view speaks the kernel contract only: its writes
        // are `op.submit`, never an intent the app decodes
        "governance" => &[],
        // members speaks it too; `copy` is the clipboard door, not a write
        "members" => &["copy"],
        // the agents view speaks the kernel contract: its pause and its save
        // are `op.submit`. `register` stays an intent because it provisions a
        // program account before it registers, and `open_run`/`open_link`
        // navigate other tabs.
        "agents" => &["register", "open_run", "open_link"],
        // the node view speaks the kernel contract: it reads the node's own
        // status, peers, registry and log ring itself and retunes the live
        // tracing filter through `rpc.admin`. `copy` is the clipboard door.
        "node" => &["copy"],
        // the explorer view reads and searches through the kernel: the only
        // thing it asks the app for is the clipboard
        "explorer" => &["copy"],
        // the chat view reads its own room and signs its own writes; what is
        // left at the door is what another plane of the app steers or owns
        "chat" => &[
            "open_hit",
            "toggle_create",
            "choose_channel",
            "choose_dm",
            "show_huddle",
            "leave_huddle",
            "join_huddle",
            "join_voice",
            "scrolled",
            "open_link",
            "copy",
            "copy_link",
            "begin_edit",
            "cancel_run",
            "open_run",
        ],
        // the forge view reads, folds and writes through the kernel: what is
        // left at the door is the two OS doors. Its discussion composer is a
        // host surface, so its submit crosses as the surface's own event.
        "forge" => &["open_link", "copy"],
        // the files view speaks the kernel contract: its reads and writes go
        // through the kernel, and the two events left are the app's own doors
        "files" => &["open_link", "at"],
        "settings" => &[
            "tab",
            "reconnect",
            "switch_network",
            "unlock",
            "lock",
            "rename",
            "create",
            "key_add",
            "join",
            "key_remove",
            "passkey",
            "passkey_desktop",
            "ceremony_cancel",
            "wallet",
            "login",
            "copy",
            "light",
            "dark",
            "notifications",
        ],
        // pages speaks the kernel contract: every read is `rpc.view` and
        // every write `op.submit`. What is left are the two OS doors — the
        // clipboard, and the open plane a `duck://` link in a document goes
        // through.
        "pages" => &["copy", "open_link"],
        _ => &[],
    }
}

// ---------- mounting ----------

/// The widget for one module's view, with `props` as the app has them now.
/// Drawing never starts a load, and never finds one on its way: the view
/// was asked for when its source was — the staged file at boot
/// ([`booted`]), the node's deployment at connect ([`connected`]) — and
/// both hand every view over before anything draws. A block that moves the
/// deployment ([`deployments_checked`]) reloads the view in place: the tab
/// keeps the one it has until the replacement is ready.
pub(crate) struct ViewSpec {
    pub(crate) module: &'static str,
    pub(crate) props: Vec<u8>,
}

fn module_view(module: &'static str, props: Vec<u8>) -> ViewSpec {
    ViewSpec { module, props }
}

/// The node the app is connected to, for the views that come from its
/// deployments. Told by `backend::connect`; every module-owned view is
/// asked of this node, drawn or not, under a new generation, so an answer
/// the previous node is still composing lands nowhere. Every seat keeps
/// what it shows until this node's answer lands: one seated is swapped in
/// place, one failed or empty off the previous node shows that until then,
/// and one no tab has drawn yet is seated here. The connect answers only
/// once the loads returned here have [settled](Loads::settled), so the
/// tabs it opens onto find their views there.
pub fn connected(client: &ducktape_rpc::Client) -> Loads {
    // THE REGISTRY LOCK FIRST: a connection change and the restart of the
    // views under it are one step. Two callers — `backend::connect` runs on
    // the executor's threads, and two connects can overlap — otherwise
    // interleave into loads asked of one node under the other's revision,
    // every one of which dies at install, and the views stay "Loading".
    let mut registry = registry().lock().expect("module views");
    // the client and its revision move as one, and their lock is let go
    // before any view is touched: a load installs under it (see
    // `spawn_load`), and takes the view's own lock inside it. Lock order,
    // everywhere: registry, then connection, then a view.
    let snapshot = {
        let mut connection = connection().lock().expect("views rpc");
        connection.rev += 1;
        connection.client = Some(client.clone());
        connection.clone()
    };
    for module in crate::backend::view_source::MODULE_OWNED {
        registry.entry(module).or_insert_with(Mounted::seat);
    }
    let loads = registry
        .iter()
        .filter_map(|(module, mounted)| {
            let mut locked = mounted.lock().expect("module view lock");
            let desktop_view = !crate::backend::view_source::module_owned(module);
            let seated = matches!(locked.slot, Slot::Ready(_));
            // the desktop's own view is the same on every node: one seated,
            // or on its way from the staged file, is left alone
            let left_alone = desktop_view && (seated || locked.in_flight);
            if left_alone {
                return None;
            }
            // a module-owned view is reloaded from this node's deployment;
            // the seat keeps what it shows until that lands
            let generation = locked.start(None);
            Some(spawn_load(module, mounted, generation, snapshot.clone()))
        })
        .collect();
    Loads(loads)
}

/// The desktop's own views, staged beside the binary: every one is asked
/// for at boot, on its own thread, and `main` [joins](Loads::joined) them
/// before the window, so the first draw of any of their tabs finds the
/// view there.
pub fn booted() -> Loads {
    let mut registry = registry().lock().expect("module views");
    let snapshot = connection().lock().expect("views rpc").clone();
    let loads = crate::backend::view_source::DESKTOP_OWNED
        .into_iter()
        .map(|module| {
            let mounted = registry.entry(module).or_insert_with(Mounted::seat);
            let generation = mounted.lock().expect("module view lock").start(None);
            spawn_load(module, mounted, generation, snapshot.clone())
        })
        .collect();
    Loads(loads)
}

/// The loads one source event started, each on its own thread. The boot
/// and the connect hand their views over — the boot joins them before the
/// window, the connect settles them before it answers — so a tab never
/// draws a seat with its answer still on the way. A block's loads swap in
/// place and are waited on by nobody but a test.
pub struct Loads(Vec<std::thread::JoinHandle<()>>);

impl Loads {
    /// Every load in: the seats hold what their source answered.
    pub fn joined(self) {
        for load in self.0 {
            load.join().expect("a view load");
        }
    }

    /// [`joined`](Self::joined), off the executor's threads: a cranelift
    /// compile is a second or more cold, which no async worker sits
    /// through.
    pub async fn settled(self) {
        tokio::task::spawn_blocking(move || self.joined())
            .await
            .expect("the view loads");
    }

    /// How many loads the event started.
    #[cfg(test)]
    pub(crate) fn started(&self) -> usize {
        self.0.len()
    }
}

/// The node's deployments moved (a new block): every module-owned view
/// whose module's active code is not the one it was drawn from is loaded
/// again, under a new generation, and swapped in place when it is ready.
/// One check in flight at a time; a block that lands during one is
/// covered by the next. The loads it starts swap in place, so nobody waits
/// on them but a test; the block stream drops them.
pub async fn deployments_checked() -> Loads {
    static IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    use std::sync::atomic::Ordering;
    if IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return Loads(Vec::new());
    }
    let started = deployments_check().await;
    IN_FLIGHT.store(false, Ordering::SeqCst);
    started
}

async fn deployments_check() -> Loads {
    let asked_of = connection().lock().expect("views rpc").clone();
    let Some(client) = &asked_of.client else {
        return Loads(Vec::new());
    };
    let hashes = match crate::backend::view_source::active_hashes(client).await {
        Ok(hashes) => hashes,
        Err(error) => {
            tracing::warn!(
                target: "ducktape::app",
                reason = "module_status_unreadable",
                error = %error,
                "module deployments not checked"
            );
            return Loads(Vec::new());
        }
    };
    let registry = registry().lock().expect("module views");
    // the connection may have moved while the registry answered: a load
    // asked of the old one dies at install, and `connected` restarted
    // everything under the new one
    if connection().lock().expect("views rpc").rev != asked_of.rev {
        return Loads(Vec::new());
    }
    let loads = registry
        .iter()
        .filter(|(module, _)| crate::backend::view_source::module_owned(module))
        .filter_map(|(module, mounted)| {
            let mut locked = mounted.lock().expect("module view lock");
            // a module the node does not run has no deployment to move
            // to: the view stays as `connected` left it
            let active = hashes.get(*module).copied()?;
            // a load already after this deployment — or after whatever is
            // active, as a reconnect's is — lands or fails on its own;
            // starting over on every block would never let it land
            let waited_for =
                locked.in_flight && locked.wanted.is_none_or(|wanted| Some(wanted) == active);
            // a candidate that just failed is left alone until its gap is
            // up: the same bytes fail the same way, so one attempt a block
            // is unbounded work for a deployment that never lands
            if waited_for || active == locked.hash || locked.held_off(active) {
                return None;
            }
            let Some(replacement) = locked.replacement_after_wait() else {
                log_source(
                    module,
                    active.as_ref(),
                    "Failed",
                    locked.generation,
                    "replacement_waiting",
                );
                return None;
            };
            let generation = locked.start(active);
            locked.replacement = replacement;
            Some(spawn_load(module, mounted, generation, asked_of.clone()))
        })
        .collect();
    Loads(loads)
}

/// The node the module-owned views load from, and how many times the app
/// has moved: a load is asked of one snapshot of this and installs only
/// while it is still the one.
#[derive(Clone, Default)]
struct Connection {
    client: Option<ducktape_rpc::Client>,
    rev: u64,
}

fn connection() -> &'static Mutex<Connection> {
    static CONNECTION: OnceLock<Mutex<Connection>> = OnceLock::new();
    CONNECTION.get_or_init(Mutex::default)
}

struct Mounted {
    slot: Slot,
    props: Option<Vec<u8>>,
    generation: u64,
    /// The deployment the slot answers for — the view drawn, or the empty
    /// slot of a deployment without one — so a block moves it only when the
    /// active code moved. A load that failed never seated anything, so it
    /// leaves this alone and is held off by `retry` instead.
    hash: Option<[u8; 32]>,
    /// A load is on its way for `generation`, and the deployment it is
    /// after when a block named one: a block that names it again waits
    /// for it instead of starting over.
    in_flight: bool,
    wanted: Option<[u8; 32]>,
    /// The seated view has held a replacement off with pending work since
    /// then; a block that names a deployment for it waits under the same
    /// generation instead of opening one per block.
    waiting_since: Option<Instant>,
    replacement: Replacement,
    /// The candidate a load last failed on, and when the next block may try
    /// it again. Cleared by any load that comes back, so only a repeated
    /// failure on the same candidate widens the gap.
    retry: Option<Retry>,
}

/// A failed load's hold-off: the candidate it failed on, when the next
/// attempt at that same candidate is due, and the gap that produced it.
struct Retry {
    hash: Option<[u8; 32]>,
    next: Instant,
    gap: Duration,
}

impl Retry {
    /// The hold-off after a load for `hash` failed: the gap doubles up to
    /// `RETRY_MAX` while the same candidate keeps failing, and starts over
    /// at `RETRY_FIRST` for a different one.
    fn after(previous: Option<&Retry>, hash: Option<[u8; 32]>) -> Retry {
        let gap = match previous {
            Some(previous) if previous.hash == hash => (previous.gap * 2).min(RETRY_MAX),
            _ => RETRY_FIRST,
        };
        Retry {
            hash,
            next: Instant::now() + gap,
            gap,
        }
    }
}

#[derive(Clone, Copy)]
enum Replacement {
    Preserve,
    RecoverNeverValid,
}

impl Mounted {
    /// A seat with nothing asked for yet: `Loading` until its source is
    /// asked, under generation 0, which no load answers for.
    fn seat() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            slot: Slot::Loading,
            props: None,
            generation: 0,
            hash: None,
            in_flight: false,
            wanted: None,
            waiting_since: None,
            replacement: Replacement::Preserve,
            retry: None,
        }))
    }

    fn replacement_after_wait(&mut self) -> Option<Replacement> {
        let Slot::Ready(old) = &self.slot else {
            return Some(Replacement::Preserve);
        };
        let no_pending_state = old.ticks == 0 || (old.ever_valid_tree && old.settled());
        if no_pending_state {
            return Some(Replacement::Preserve);
        }
        let since = *self.waiting_since.get_or_insert_with(Instant::now);
        let never_valid_expired =
            old.can_recover_without_state() && since.elapsed() >= REPLACEMENT_WAIT;
        never_valid_expired.then_some(Replacement::RecoverNeverValid)
    }

    /// Whether a block naming `active` is still inside the hold-off a
    /// failed load for that same candidate left behind.
    fn held_off(&self, active: Option<[u8; 32]>) -> bool {
        self.retry
            .as_ref()
            .is_some_and(|retry| retry.hash == active && Instant::now() < retry.next)
    }

    /// Opens the next generation for a load after `wanted` (None: whatever
    /// the node holds active), and names it.
    fn start(&mut self, wanted: Option<[u8; 32]>) -> u64 {
        self.generation += 1;
        self.in_flight = true;
        self.wanted = wanted;
        self.waiting_since = None;
        self.replacement = Replacement::Preserve;
        self.generation
    }
}

enum Slot {
    Loading,
    Ready(Box<Guest>),
    /// The active deployment verified, and it ships no view.
    Empty,
    Failed(String),
}

type Registry = Mutex<HashMap<&'static str, Arc<Mutex<Mounted>>>>;

fn registry() -> &'static Registry {
    static MOUNTED: OnceLock<Registry> = OnceLock::new();
    MOUNTED.get_or_init(Mutex::default)
}

/// The seat of `module`'s view, made on its first ask. Making it asks for
/// nothing: a load starts at the view's source event, never at a draw, so
/// a seat a tab is first to ask for waits for the next [`connected`] or
/// [`deployments_checked`] like every other.
fn mounted(module: &'static str) -> Arc<Mutex<Mounted>> {
    registry()
        .lock()
        .expect("module views")
        .entry(module)
        .or_insert_with(Mounted::seat)
        .clone()
}

/// Loads the view on its own thread — a cold cranelift compile is a second
/// or more; the window thread shows "Loading" instead of freezing for it —
/// and installs it only if `mounted` still waits for this very load AND,
/// for a module's view, the app is still on the node it was asked of, both
/// checked under the connection lock so a move cannot slip between the
/// check and the seat. The desktop's own view is the same on every node:
/// its load lands wherever the app has moved to meanwhile.
fn spawn_load(
    module: &'static str,
    mounted: &Arc<Mutex<Mounted>>,
    generation: u64,
    asked_of: Connection,
) -> std::thread::JoinHandle<()> {
    let loading = mounted.clone();
    std::thread::spawn(move || {
        let loaded = Guest::load(module, asked_of.client.as_ref(), generation, &loading);
        let connection = connection().lock().expect("views rpc");
        let mut locked = loading.lock().expect("module view lock");
        if locked.generation != generation {
            return;
        }
        locked.in_flight = false;
        let from_the_node = crate::backend::view_source::module_owned(module);
        let node_since_left = connection.rev != asked_of.rev;
        if from_the_node && node_since_left {
            return;
        }
        let Mounted {
            slot,
            hash,
            replacement,
            retry,
            ..
        } = &mut *locked;
        let replacement = *replacement;
        // a load that came back at all clears the hold-off; only the error
        // arm below puts one back, widened against the one taken here
        let held_off = retry.take();
        match loaded {
            Ok(Loaded::Fresh(mut guest)) => {
                guest.installed_generation = Some(generation);
                guest.report_display_truncation();
                *hash = guest.hash;
                *slot = Slot::Ready(guest);
            }
            Ok(Loaded::Unchanged) => {}
            Ok(Loaded::Empty(removed)) => {
                *hash = Some(removed);
                *slot = Slot::Empty;
            }
            // the same tab, the same surface handle, the same host-side
            // input text and pictures: only the instance behind them moves
            Ok(Loaded::Swap {
                mut fresh,
                alive,
                ticks,
            }) => {
                let Slot::Ready(old) = slot else {
                    log_source(
                        module,
                        fresh.hash.as_ref(),
                        "Failed",
                        generation,
                        "the view left while its replacement was prepared",
                    );
                    return;
                };
                // A never-valid view may have become usable while bytes
                // were loading. Recheck at installation before dropping it.
                let still_eligible = match replacement {
                    Replacement::Preserve => old.ticks == ticks && old.settled(),
                    Replacement::RecoverNeverValid => old.can_recover_without_state(),
                };
                if !Arc::ptr_eq(&old.alive, &alive) || !still_eligible {
                    log_source(
                        module,
                        fresh.hash.as_ref(),
                        "Failed",
                        generation,
                        "the view moved while its replacement was prepared",
                    );
                    return;
                }
                fresh.frame_rev = old.frame_rev + 1;
                if let Some(root) = &fresh.frame.root
                    && let Err(reason) = fresh.inputs.retain_restored_projections(&old.inputs, root)
                {
                    log_source(module, fresh.hash.as_ref(), "Failed", generation, &reason);
                    return;
                }
                if let Some(root) = &mut fresh.frame.root {
                    fresh.pictures.hydrate(root);
                    old.pictures.adopt(root);
                    root.for_each_mut(&mut |node| match node {
                        wire::Node::Svg { bytes, .. } => *bytes = None,
                        wire::Node::Image { data, .. } | wire::Node::ImageViewer { data, .. } => {
                            *data = None
                        }
                        _ => {}
                    });
                }
                fresh.pictures = std::mem::take(&mut old.pictures);
                let reason = match replacement {
                    Replacement::RecoverNeverValid => "recovered_never_valid_view",
                    Replacement::Preserve => "",
                };
                fresh.installed_generation = Some(generation);
                fresh.report_display_truncation();
                log_source(module, fresh.hash.as_ref(), "Swapped", generation, reason);
                *hash = fresh.hash;
                *slot = Slot::Ready(fresh);
            }
            Err(Unloaded {
                hash: failed_on,
                reason,
            }) => {
                tracing::warn!(
                    target: "ducktape::app",
                    module,
                    reason = "module_view_unloadable",
                    error = %reason,
                    "module view not loaded"
                );
                // the next block leaves the candidate this failed on alone
                // until the gap is up; a failure before any candidate (a
                // status or transport error) holds nothing off, and any
                // other deployment is unaffected
                *retry = Some(Retry::after(held_off.as_ref(), failed_on));
                // a view that is there stays, with its hash: the failure is
                // the replacement's, not its own
                if !matches!(slot, Slot::Ready(_)) {
                    *slot = Slot::Failed(reason);
                }
            }
        }
    })
}

/// What a load came back with.
enum Loaded {
    /// A view where there was none.
    Fresh(Box<Guest>),
    /// The view already drawn is the active deployment's.
    Unchanged,
    /// A prepared replacement for the view drawn: restored from its
    /// snapshot, its first tree verified; `alive` and `ticks` name the
    /// instance it was prepared against.
    Swap {
        fresh: Box<Guest>,
        alive: Arc<()>,
        ticks: u64,
    },
    /// The deployment (this hash) ships no view.
    Empty([u8; 32]),
}

/// A load that came back with no view, and the candidate it failed on —
/// none when it never got as far as one — so the block check's hold-off
/// keys on the bytes that failed, never on what the load was asked after.
struct Unloaded {
    hash: Option<[u8; 32]>,
    reason: String,
}

/// Whether `hash` is still the module's active code, asked of the node
/// right before a load's result is taken as the deployment's word.
async fn still_active(
    client: &ducktape_rpc::Client,
    module: &str,
    hash: [u8; 32],
) -> Result<bool, String> {
    crate::backend::view_source::active_hash(client, module)
        .await
        .map(|active| active == Some(hash))
        .map_err(|error| error.to_string())
}

/// One `view_source` line per outcome, with the same fields every time —
/// the canary greps them.
fn log_source(module: &str, hash: Option<&[u8; 32]>, state: &str, generation: u64, reason: &str) {
    tracing::info!(
        target: "ducktape::app",
        module,
        hash = %hash.map_or_else(|| "-".to_owned(), |hash| crate::backend::hex_encode(hash)),
        state,
        gen = generation,
        reason = %if reason.is_empty() { "-" } else { reason },
        "view_source"
    );
    #[cfg(test)]
    {
        let line = format!(
            "module={module} hash={} state={state} gen={generation} reason={}",
            hash.map_or_else(|| "-".to_owned(), |hash| crate::backend::hex_encode(hash)),
            if reason.is_empty() { "-" } else { reason }
        );
        canary::TAPS
            .lock()
            .expect("view_source taps")
            .retain(|tap| tap.send(line.clone()).is_ok());
    }
}

/// How long each stage of one module-owned load took: `status` and
/// `fetch` are the node's answers, `compile` is cranelift, `init` the
/// instance and its `on mount` or restore, `first_frame` the tree a
/// replacement proves (a fresh view draws its first on the window thread),
/// `check` the second look at the registry before the seat. `path` is
/// `first` for a load over an empty slot, `swap` over a view drawn.
#[derive(Default)]
struct LoadTiming {
    path: &'static str,
    status: Duration,
    fetch: Duration,
    compile: Duration,
    init: Duration,
    first_frame: Option<Duration>,
    check: Duration,
}

impl LoadTiming {
    /// One `view_load` line per load, every field every time, through the
    /// same logger and test tap as `view_source`.
    fn log(&self, module: &str, hash: Option<&[u8; 32]>, started: Instant, state: &str) {
        let ms = |duration: Duration| duration.as_millis();
        let hash = hash.map_or_else(|| "-".to_owned(), |hash| crate::backend::hex_encode(hash));
        let first_frame = self
            .first_frame
            .map_or_else(|| "-".to_owned(), |frame| ms(frame).to_string());
        let total = ms(started.elapsed());
        tracing::info!(
            target: "ducktape::app",
            module,
            hash = %hash,
            path = self.path,
            outcome = state,
            status_ms = ms(self.status),
            fetch_ms = ms(self.fetch),
            compile_ms = ms(self.compile),
            init_ms = ms(self.init),
            first_frame_ms = %first_frame,
            check_ms = ms(self.check),
            total_ms = total,
            "view_load"
        );
        #[cfg(test)]
        {
            let line = format!(
                "view_load module={module} hash={hash} path={} outcome={state} status_ms={} fetch_ms={} compile_ms={} init_ms={} first_frame_ms={first_frame} check_ms={} total_ms={total}",
                self.path,
                ms(self.status),
                ms(self.fetch),
                ms(self.compile),
                ms(self.init),
                ms(self.check),
            );
            canary::TAPS
                .lock()
                .expect("view_source taps")
                .retain(|tap| tap.send(line.clone()).is_ok());
        }
    }
}

/// What the canary runner (`tests::canary`) needs of this registry: the
/// `view_source` lines as they are logged, and the hash a slot answers for.
#[cfg(test)]
pub(crate) mod canary {
    use std::sync::{Mutex, mpsc};

    pub(crate) use super::tests::connection_turn;
    pub(crate) fn input_presentation(
        view: &super::NativeModuleView,
        key: &str,
        window: &gpui_kit::Window,
        cx: &gpui_kit::App,
    ) -> Option<(String, usize, std::ops::Range<usize>, bool)> {
        view.content
            .as_ref()?
            .read(cx)
            .input_presentation(key, window, cx)
    }

    pub(crate) fn frame(module: &'static str) -> Option<super::wire::Node> {
        let mounted = super::mounted(module);
        let mounted = mounted.lock().expect("module view lock");
        let super::Slot::Ready(guest) = &mounted.slot else {
            return None;
        };
        let mut root = guest.frame.root.clone()?;
        root.for_each_mut(&mut |node| {
            if let super::wire::Node::Surface { name, args, .. } = node {
                if matches!(name.as_str(), "artifact_svg" | "artifact_image") {
                    *node = super::surfaces::asset_node(name, args, guest);
                }
            }
        });
        guest.pictures.hydrate(&mut root);
        Some(root)
    }

    pub(crate) static TAPS: Mutex<Vec<mpsc::Sender<String>>> = Mutex::new(Vec::new());

    pub(crate) fn tap() -> mpsc::Receiver<String> {
        let (sender, receiver) = mpsc::channel();
        TAPS.lock().expect("view_source taps").push(sender);
        receiver
    }

    pub(crate) fn seated_hash(module: &'static str) -> Option<[u8; 32]> {
        super::mounted(module)
            .lock()
            .expect("module view lock")
            .hash
    }

    /// Every text in the view's tree, or none where no tree is drawn yet.
    pub(crate) fn texts(module: &'static str) -> Vec<String> {
        use super::wire;
        let mut texts = Vec::new();
        let root = match &super::mounted(module)
            .lock()
            .expect("module view lock")
            .slot
        {
            super::Slot::Ready(guest) => guest.frame.root.clone(),
            _ => None,
        };
        if let Some(mut root) = root {
            root.for_each_mut(&mut |node| match node {
                wire::Node::Text { content, .. }
                | wire::Node::Button {
                    content: wire::ButtonContent::Label(content),
                    ..
                } => texts.push(content.clone()),
                wire::Node::RichText { spans, .. } => {
                    texts.push(spans.iter().map(|span| span.content.as_str()).collect())
                }
                _ => {}
            });
        }
        texts
    }
}

/// Where the staged views are: `$DUCKTAPE_VIEWS_DIR`, else `views/` beside
/// the binary or beside its profile directory — the shape
/// `workspace_config::staged_modules_dir` gives the founding set.
fn views_dir() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("DUCKTAPE_VIEWS_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let exe = std::env::current_exe().map_err(|error| format!("current executable: {error}"))?;
    let exe_dir = exe.parent().ok_or("the executable has no directory")?;
    [Some(exe_dir), exe_dir.parent()]
        .into_iter()
        .flatten()
        .map(|dir| dir.join("views"))
        .find(|dir| dir.is_dir())
        .ok_or_else(|| {
            format!(
                "no views beside {} — `make views` stages them under target/views, or set $DUCKTAPE_VIEWS_DIR",
                exe.display()
            )
        })
}

// ---------- the guest ----------

/// What a view's store holds: its limits, and the message its panic hook
/// handed over before the trap that follows.
struct HostState {
    limits: StoreLimits,
    panic: Option<String>,
}

/// The guest's `restore(state, macos)` export.
type Restore = TypedFunc<(Vec<u8>, bool), (Result<(), String>,)>;

struct Guest {
    module: &'static str,
    store: Store<HostState>,
    tick: TypedFunc<(Vec<u8>,), (Vec<u8>,)>,
    /// The guest's events for its next tick.
    pending: Vec<wire::Event>,
    /// Requests wait for their frame's native layout, inside this instance only.
    widget_commands: Vec<(u64, u64, wire::WidgetCommand)>,
    /// The last frame, its `root` kept across `unchanged` ticks and patched
    /// in place by a frame that carries patches instead of a tree.
    frame: wire::Frame,
    frame_reports: display_diagnostics::FrameReports,
    display_diagnostics: display_diagnostics::DisplayDiagnostics,
    installed_generation: Option<u64>,
    /// Bumped when `frame.root` changes: the widget rebuilds when it sees a
    /// number it has not rendered.
    frame_rev: u64,
    ticks: u64,
    /// A later patch gap or trap must not erase evidence of authored state.
    ever_valid_tree: bool,
    /// The live text of every input in the tree — the host's, not the guest's.
    inputs: EditorStore,
    /// Every picture the guest has sent, by hash: the bytes cross once.
    pictures: Pictures,
    /// The guest's `<module>.props` subscription, once it asked, and the
    /// props it was last given on it.
    props_subscription: Option<u64>,
    props_sent: Option<Vec<u8>>,
    /// What the guest asked the app to do this redraw.
    intents: Vec<ModuleViewEvent>,
    /// The kernel's answers to this guest's node calls, on their way in.
    replies: Arc<kernel::Replies>,
    /// The guest's `rpc.live` subscriptions, each with the plane it named:
    /// told on every block that moves that plane.
    live_subscriptions: Vec<(u64, String)>,
    /// The guest's `rpc.stream` subscriptions, each holding the node socket
    /// the kernel opened for it: retired with the cancel, and with the guest.
    streams: Vec<(u64, kernel::NodeStream)>,
    /// The guest's `clock.ticks` subscriptions: the period it asked for and
    /// the instant its next item is due. A module has no clock of its own,
    /// so periodic guest subscriptions use this list — driven from the window
    /// thread's own redraw, never from a thread that would have to wake it.
    clocks: Vec<kernel::Clock>,
    /// The trap that ended the view, if one did. A faulted guest never ticks again.
    fault: Option<String>,
    /// The assets the deployment shipped beside this view, for the host
    /// surfaces that paint them by canonical relative path; swapped with the
    /// instance as one unit. Empty for a staged desktop view.
    assets: Arc<crate::backend::view_source::Assets>,
    /// The deployment this instance came from; none for a staged view.
    hash: Option<[u8; 32]>,
    /// This instance's identity: a replacement prepared against it is
    /// installed only over it.
    alive: Arc<()>,
    /// A replacement's first tree is in `frame` with its requests still
    /// to dispatch — the first redraw does that, without another tick.
    staged: bool,
    snapshot: TypedFunc<(), (Result<Vec<u8>, String>,)>,
    restore: Restore,
    init: TypedFunc<(bool,), ()>,
}

/// The asset at `path` in a deployment's map: the canonical relative path,
/// exactly — no normalisation, no file, no network.
fn artifact_asset<'a>(
    assets: &'a crate::backend::view_source::Assets,
    path: &str,
) -> Option<&'a [u8]> {
    assets.get(path).map(Vec::as_slice)
}

fn hex_short(hash: &[u8; 32]) -> String {
    hash[..6].iter().map(|byte| format!("{byte:02x}")).collect()
}

const COMPILED_VIEW_LIMIT: usize = 16;
const COMPILED_VIEW_SOURCE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Default)]
struct ViewCodeCache {
    entries: VecDeque<CompiledView>,
}

struct CompiledView {
    hash: [u8; 32],
    source_bytes: usize,
    component: Arc<Component>,
}

impl ViewCodeCache {
    fn get(&mut self, hash: &[u8; 32]) -> Option<Arc<Component>> {
        let index = self.entries.iter().position(|entry| &entry.hash == hash)?;
        let entry = self.entries.remove(index)?;
        let component = entry.component.clone();
        self.entries.push_back(entry);
        Some(component)
    }

    fn insert(&mut self, entry: CompiledView) {
        if entry.source_bytes > COMPILED_VIEW_SOURCE_BYTES {
            return;
        }
        let mut bytes: usize = self.entries.iter().map(|entry| entry.source_bytes).sum();
        loop {
            let fits = self.entries.len() < COMPILED_VIEW_LIMIT
                && bytes + entry.source_bytes <= COMPILED_VIEW_SOURCE_BYTES;
            if fits {
                break;
            }
            let Some(old) = self.entries.pop_front() else {
                break;
            };
            bytes -= old.source_bytes;
        }
        self.entries.push_back(entry);
    }
}

fn compiled_view(bytes: &[u8]) -> Result<Arc<Component>, String> {
    static CODE: OnceLock<Mutex<ViewCodeCache>> = OnceLock::new();
    compile_view(engine(), CODE.get_or_init(Mutex::default), bytes)
}

// Cache code only: every load still creates its own Store, instance and assets.
// The cache belongs to this one Engine. Compile outside the lock so unrelated
// views can prepare concurrently; a competing result adopts the existing entry.
fn compile_view(
    engine: &Engine,
    cache: &Mutex<ViewCodeCache>,
    bytes: &[u8],
) -> Result<Arc<Component>, String> {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(bytes).into();
    if let Some(component) = cache.lock().expect("view code cache").get(&hash) {
        return Ok(component);
    }
    let component = Arc::new(Component::new(engine, bytes).map_err(|error| error.to_string())?);
    let mut cache = cache.lock().expect("view code cache");
    if let Some(existing) = cache.get(&hash) {
        return Ok(existing);
    }
    cache.insert(CompiledView {
        hash,
        source_bytes: bytes.len(),
        component: component.clone(),
    });
    Ok(component)
}

fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut config = Config::new();
        config.cranelift_opt_level(OptLevel::Speed);
        config.consume_fuel(true);
        config.epoch_interruption(true);
        match crate::backend::cache_dir() {
            Ok(directory) => {
                let mut cache = CacheConfig::new();
                cache.with_directory(directory.join("view-code"));
                match Cache::new(cache) {
                    Ok(cache) => {
                        config.cache(Some(cache));
                    }
                    Err(error) => tracing::warn!(reason = "view_cache_unavailable", %error),
                }
            }
            Err(error) => tracing::warn!(reason = "view_cache_directory_unavailable", %error),
        }
        let engine = Engine::new(&config).expect("wasmtime engine");
        // The clock every tick's deadline is measured against: one thread
        // for the process, never stopped.
        let ticking = engine.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(EPOCH_TICK);
                ticking.increment_epoch();
            }
        });
        engine
    })
}

/// What one call into the view may spend: instructions, and time.
fn arm(store: &mut Store<HostState>) {
    let _ = store.set_fuel(FUEL_PER_TICK);
    let epochs = TICK_DEADLINE.as_nanos().div_ceil(EPOCH_TICK.as_nanos()) as u64;
    store.set_epoch_deadline(epochs);
}

impl Guest {
    /// A module-owned view comes from the module's active deployment on the
    /// connected node — nothing else, so with no node there is nothing to
    /// load yet, and no staged file is ever opened for it; the desktop's own
    /// views come from the staged file. With a view of the module already
    /// drawn, the deployment's view is prepared as its replacement:
    /// instantiated without `init`, restored from the drawn view's snapshot,
    /// and its first tree verified — and the active code is read again
    /// right before it is handed over, so a deployment that moved meanwhile
    /// is not installed. Every outcome for a module-owned view is one
    /// `view_source` log line with stable fields.
    fn load(
        module: &'static str,
        client: Option<&ducktape_rpc::Client>,
        generation: u64,
        mounted: &Arc<Mutex<Mounted>>,
    ) -> Result<Loaded, Unloaded> {
        use crate::backend::view_source::{self, ViewSource};
        let before_any_candidate = |reason: String| Unloaded { hash: None, reason };
        if !view_source::module_owned(module) {
            let path = views_dir()
                .map_err(before_any_candidate)?
                .join(format!("{module}_view.wasm"));
            return Self::load_from(module, &path)
                .map(|guest| Loaded::Fresh(Box::new(guest)))
                .map_err(before_any_candidate);
        }
        let logged = |hash: Option<&[u8; 32]>, state: &str, reason: &str| {
            log_source(module, hash, state, generation, reason);
        };
        let client = match client {
            Some(client) => client,
            None => {
                let reason = "not connected to a node yet";
                logged(None, "Failed", reason);
                return Err(before_any_candidate(reason.to_owned()));
            }
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| before_any_candidate(error.to_string()))?;
        let started = Instant::now();
        let mut timing = LoadTiming {
            path: "first",
            ..LoadTiming::default()
        };
        let mut asked = view_source::Asked::default();
        let source = runtime.block_on(view_source::resolve(client, module, &mut asked));
        timing.status = asked.status;
        timing.fetch = asked.fetch;
        let source = match source {
            Ok(source) => source,
            Err(error) => {
                let reason = error.to_string();
                logged(None, "Failed", &reason);
                timing.log(module, None, started, "Failed");
                return Err(before_any_candidate(reason));
            }
        };
        let (hash, component, assets) = match source {
            ViewSource::NotActivated => {
                logged(None, "NotActivated", "");
                timing.log(module, None, started, "NotActivated");
                return Err(before_any_candidate(format!(
                    "the {module} module is not activated yet"
                )));
            }
            ViewSource::Missing { hash } => {
                // a removal answered late, after the code moved on, is not
                // the current deployment's word
                let checked = Instant::now();
                let active = runtime.block_on(still_active(client, module, hash));
                timing.check = checked.elapsed();
                let active = match active {
                    Ok(active) => active,
                    Err(reason) => {
                        logged(Some(&hash), "Failed", &reason);
                        timing.log(module, Some(&hash), started, "Failed");
                        return Err(Unloaded {
                            hash: Some(hash),
                            reason,
                        });
                    }
                };
                if !active {
                    let reason = "the active code moved while the view was prepared";
                    logged(Some(&hash), "Failed", reason);
                    timing.log(module, Some(&hash), started, "Failed");
                    return Err(Unloaded {
                        hash: Some(hash),
                        reason: reason.to_owned(),
                    });
                }
                logged(Some(&hash), "Missing", "");
                timing.log(module, Some(&hash), started, "Missing");
                return Ok(Loaded::Empty(hash));
            }
            ViewSource::Ready {
                hash,
                component,
                assets,
            } => (hash, component, assets),
        };
        let shown = format!("{module} view @ {}", hex_short(&hash));
        let outcome = (|| -> Result<Loaded, String> {
            // the instance in the slot, if the deployment is a new one for
            // it: the replacement is seated only against that very
            // instance at that very tick count
            let (mut against, replacement) = {
                let locked = mounted.lock().expect("module view lock");
                let against = match &locked.slot {
                    Slot::Ready(old) if old.hash == Some(hash) => return Ok(Loaded::Unchanged),
                    Slot::Ready(old) => Some((old.alive.clone(), old.ticks)),
                    _ => None,
                };
                (against, locked.replacement)
            };
            if against.is_some() {
                timing.path = "swap";
            }
            let compiled = Instant::now();
            let component = Self::compile(&component, &shown);
            timing.compile = compiled.elapsed();
            let component = component?;
            let seated = Instant::now();
            let prepared = (|| -> Result<Self, String> {
                let mut fresh = Self::instantiate(module, &component, &shown)?;
                fresh.deployed(hash, assets);
                match &mut against {
                    // A once-valid view carries its state over. Only an
                    // explicitly admitted never-valid recovery may initialize.
                    Some((alive, ticks)) => {
                        let snapshot = {
                            let mut locked = mounted.lock().expect("module view lock");
                            let Slot::Ready(old) = &mut locked.slot else {
                                return Err(
                                    "the view left while its replacement was prepared".into()
                                );
                            };
                            if !Arc::ptr_eq(alive, &old.alive) {
                                return Err(
                                    "the view changed while its replacement was prepared".into()
                                );
                            }
                            // Compilation may take many old-view frames. Fence the
                            // state actually captured here, not its precompile tick.
                            *ticks = old.ticks;
                            let preserve =
                                *ticks > 0 && matches!(replacement, Replacement::Preserve);
                            if preserve && !old.settled() {
                                return Err(
                                    "the view has pending work; its replacement waits".into()
                                );
                            }
                            if preserve {
                                Some(old.snapshot()?)
                            } else {
                                None
                            }
                        };
                        match snapshot {
                            Some(snapshot) => {
                                wire::Snapshot::decode(&snapshot)?;
                                fresh.restore(&snapshot, &shown)?;
                            }
                            None => fresh.init(&shown)?,
                        }
                        let framed = Instant::now();
                        let frame = fresh.first_frame(&shown);
                        timing.first_frame = Some(framed.elapsed());
                        frame?;
                    }
                    None => {
                        fresh.init(&shown)?;
                    }
                }
                Ok(fresh)
            })();
            // Include failed instantiate/restore/init work as well as success.
            timing.init = seated
                .elapsed()
                .saturating_sub(timing.first_frame.unwrap_or_default());
            let fresh = prepared?;
            // the deployment may have moved while this one was prepared;
            // the block that moved it starts another load
            let checked = Instant::now();
            let active = runtime.block_on(still_active(client, module, hash));
            timing.check = checked.elapsed();
            if !active? {
                return Err("the active code moved while the view was prepared".into());
            }
            Ok(match against {
                Some((alive, ticks)) => Loaded::Swap {
                    fresh: Box::new(fresh),
                    alive,
                    ticks,
                },
                None => Loaded::Fresh(Box::new(fresh)),
            })
        })();
        let state = match &outcome {
            Ok(Loaded::Fresh(_)) => {
                logged(Some(&hash), "Ready", "");
                "Ready"
            }
            Ok(Loaded::Unchanged) => "Unchanged",
            // `Swapped` is logged at the seat: the swap can still be refused there
            Ok(_) => "Swap",
            Err(reason) => {
                logged(Some(&hash), "Failed", reason);
                "Failed"
            }
        };
        timing.log(module, Some(&hash), started, state);
        outcome.map_err(|reason| Unloaded {
            hash: Some(hash),
            reason,
        })
    }

    /// The deployment's own: its assets and the surfaces that paint them
    /// by path, namespaced to this module and hash — a path the deployment
    /// does not ship leaves the slot empty and says so once.
    fn deployed(&mut self, hash: [u8; 32], assets: Arc<crate::backend::view_source::Assets>) {
        self.hash = Some(hash);
        self.assets = assets;
    }

    /// A trap or temporary missing tree cannot revoke previously accepted
    /// authored state. Both recovery admission and installation use this fact.
    fn can_recover_without_state(&self) -> bool {
        !self.ever_valid_tree
    }

    /// Candidate attempts do not retire a seated view or its input routes.
    /// Direct test fixtures have no loader-assigned generation and use zero.
    fn seated_generation(&self) -> u64 {
        self.installed_generation.unwrap_or_default()
    }

    /// Everything this instance was asked to do is done: nothing pending,
    /// no request the host has yet to route, no trap. A replacement not
    /// yet redrawn is settled too: the only requests its first tree
    /// carries are the subscriptions its restore rebuilt, which its own
    /// replacement rebuilds again — a tab not shown between two
    /// deployments is not stuck on the first.
    fn settled(&self) -> bool {
        self.fault.is_none()
            && self.replies.fault().is_none()
            && self.pending.is_empty()
            && self.widget_commands.is_empty()
            && self.inputs.ready() == Ok(true)
            && !self.inputs.pending()
            && !self.frame.busy
            && (self.staged || self.frame.requests.is_empty())
    }

    fn snapshot(&mut self) -> Result<Vec<u8>, String> {
        arm(&mut self.store);
        self.snapshot
            .call(&mut self.store, ())
            .map_err(|error| first_line(&error))?
            .0
    }

    fn restore(&mut self, snapshot: &[u8], shown: &str) -> Result<(), String> {
        arm(&mut self.store);
        self.restore
            .call(
                &mut self.store,
                (snapshot.to_vec(), cfg!(target_os = "macos")),
            )
            .map_err(|error| format!("{shown}: restore trapped: {}", first_line(&error)))?
            .0
            .map_err(|error| format!("{shown}: {error}"))
    }

    /// `on mount` runs in here, told which platform it keys for.
    fn init(&mut self, shown: &str) -> Result<(), String> {
        arm(&mut self.store);
        if let Err(error) = self
            .init
            .call(&mut self.store, (cfg!(target_os = "macos"),))
        {
            let trap = format!("{shown}: init trapped: {}", first_line(&error));
            return Err(panic_message(&mut self.store).unwrap_or(trap));
        }
        Ok(())
    }

    /// A restored instance's first tick: its whole tree, or it is no
    /// replacement. Its requests wait for the first redraw.
    fn first_frame(&mut self, shown: &str) -> Result<(), String> {
        let mut requests = Vec::new();
        let mut cancels = Vec::new();
        let limit = wire::editor_document::MAX_EDITOR_DOCUMENTS
            * (wire::editor_document::MAX_EDITOR_CHUNKS + 3)
            + 1;
        for _ in 0..limit {
            self.tick();
            #[cfg(test)]
            if tests::FIRST_FRAME_TRAPS.swap(false, std::sync::atomic::Ordering::SeqCst) {
                self.fault = Some("the first frame trapped (test)".into());
            }
            if let Some(fault) = &self.fault {
                return Err(format!("{shown}: {fault}"));
            }
            if self.frame.root.is_none() {
                return Err(format!(
                    "{shown}: the replacement did not publish a complete tree"
                ));
            }
            requests.append(&mut self.frame.requests);
            cancels.append(&mut self.frame.cancels);
            if requests.len() > MAX_REQUESTS_PER_TICK || cancels.len() > 2 * MAX_REQUESTS_PER_TICK {
                return Err(format!(
                    "{shown}: replacement requests exceed the first-frame budget"
                ));
            }
            if self.inputs.ready()? && self.pending.is_empty() {
                break;
            }
        }
        if !self.inputs.ready()? || !self.pending.is_empty() {
            return Err(format!(
                "{shown}: replacement document transfer did not complete"
            ));
        }
        requests.retain(|request| !cancels.contains(&request.id));
        self.frame.requests = requests;
        self.frame.cancels = cancels;
        self.ticks += 1;
        self.staged = true;
        Ok(())
    }

    fn load_from(module: &'static str, path: &std::path::Path) -> Result<Self, String> {
        let shown = path.display().to_string();
        let metadata = std::fs::metadata(path).map_err(|error| format!("{shown}: {error}"))?;
        if metadata.len() > MAX_MODULE_BYTES {
            return Err(format!(
                "{shown}: past the {MAX_MODULE_BYTES} byte module limit"
            ));
        }
        let bytes = std::fs::read(path).map_err(|error| format!("{shown}: {error}"))?;
        Self::from_bytes(module, &bytes, &shown)
    }

    /// The component instantiated and mounted; `shown` names it in errors.
    fn from_bytes(module: &'static str, bytes: &[u8], shown: &str) -> Result<Self, String> {
        let component = Self::compile(bytes, shown)?;
        let mut guest = Self::instantiate(module, &component, shown)?;
        guest.init(shown)?;
        Ok(guest)
    }

    /// The component's bytes checked and compiled — the cranelift stage of
    /// a load, measured on its own.
    fn compile(bytes: &[u8], shown: &str) -> Result<Arc<Component>, String> {
        if bytes.len() as u64 > MAX_MODULE_BYTES {
            return Err(format!(
                "{shown}: past the {MAX_MODULE_BYTES} byte module limit"
            ));
        }
        // its preferred size is for placing a new window; the tab embeds
        ui_lang_wire::manifest::read_manifest(bytes)
            .ok_or_else(|| format!("{shown}: the component's manifest cannot be read"))?
            .check_wire_protocol()
            .map_err(|error| format!("{shown}: {error}"))?;
        compiled_view(bytes).map_err(|error| format!("{shown}: {error}"))
    }

    /// The component instantiated, its exports bound, nothing run yet: a
    /// fresh view is `init`ed, a replacement `restore`d.
    fn instantiate(
        module: &'static str,
        component: &Component,
        shown: &str,
    ) -> Result<Self, String> {
        let engine = engine();
        // Tables are allocated eagerly at their declared minimum, before any
        // fuel or memory limit is consulted; a component is several core
        // instances — the guest and its component bindings — and one memory.
        let limits = StoreLimitsBuilder::new()
            .memory_size(MEMORY_LIMIT)
            .memories(1)
            .instances(8)
            .tables(4)
            .table_elements(1 << 20)
            .trap_on_grow_failure(true)
            .build();
        let mut store = Store::new(
            engine,
            HostState {
                limits,
                panic: None,
            },
        );
        store.limiter(|state| &mut state.limits);
        store.epoch_deadline_trap();
        // The `ice:view` world's one import is the panic hook's; anything
        // else the component asks for traps if it is ever called.
        let mut linker = Linker::<HostState>::new(engine);
        linker
            .root()
            .func_wrap(
                "panicked",
                |mut store: StoreContextMut<'_, HostState>, (message,): (String,)| {
                    let line = message.lines().next().unwrap_or_default();
                    store.data_mut().panic = Some(line.chars().take(1024).collect());
                    Ok(())
                },
            )
            .map_err(|error| error.to_string())?;
        linker
            .define_unknown_imports_as_traps(component)
            .map_err(|error| error.to_string())?;
        arm(&mut store);
        let instance = linker
            .instantiate(&mut store, component)
            .map_err(|error| format!("{shown}: {}", first_line(&error)))?;
        let init = instance
            .get_typed_func::<(bool,), ()>(&mut store, "init")
            .map_err(|error| format!("{shown}: {error}"))?;
        let tick = instance
            .get_typed_func::<(Vec<u8>,), (Vec<u8>,)>(&mut store, "tick")
            .map_err(|error| format!("{shown}: {error}"))?;
        let snapshot = instance
            .get_typed_func::<(), (Result<Vec<u8>, String>,)>(&mut store, "snapshot")
            .map_err(|error| format!("{shown}: {error}"))?;
        let restore = instance
            .get_typed_func::<(Vec<u8>, bool), (Result<(), String>,)>(&mut store, "restore")
            .map_err(|error| format!("{shown}: {error}"))?;
        Ok(Self {
            module,
            store,
            tick,
            pending: Vec::new(),
            widget_commands: Vec::new(),
            frame: wire::Frame::default(),
            frame_reports: Default::default(),
            display_diagnostics: Default::default(),
            installed_generation: None,
            frame_rev: 0,
            ticks: 0,
            ever_valid_tree: false,
            inputs: {
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
                EditorStore::new(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
            },
            pictures: Pictures::default(),
            props_subscription: None,
            props_sent: None,
            intents: Vec::new(),
            replies: Arc::default(),
            live_subscriptions: Vec::new(),
            streams: Vec::new(),
            clocks: Vec::new(),
            fault: None,
            assets: Arc::default(),
            hash: None,
            alive: Arc::new(()),
            staged: false,
            snapshot,
            restore,
            init,
        })
    }

    fn surface_event(&mut self, handler: Option<u32>, value: wire::SurfaceValue) {
        if let Some(handler) = handler {
            self.pending.push(wire::Event::Surface { handler, value });
            return;
        }
        if matches!(self.module, "chat" | "forge") {
            self.intents.extend(crate::composer_surface::intent(&value));
        }
    }

    /// Hands the guest the props the app holds, if they moved since the
    /// guest last saw them. Before the guest has subscribed they wait here.
    fn sync_props(&mut self, props: &Option<Vec<u8>>) {
        let Some(id) = self.props_subscription else {
            return;
        };
        if props.is_none() || *props == self.props_sent {
            return;
        }
        self.props_sent = props.clone();
        self.pending.push(wire::Event::Response {
            id,
            result: Ok(props.clone().unwrap_or_default()),
            done: false,
        });
    }

    /// One redraw: tick if there is anything to deliver — or never was a
    /// first frame — answer the requests, and say whether the guest is due
    /// again at once. A guest with nothing to deliver is left alone: the
    /// tree the host has is the tree it would send.
    fn redraw(&mut self, props: &Option<Vec<u8>>) -> bool {
        if self.fault.is_some() {
            return false;
        }
        self.sync_props(props);
        self.pending.extend(self.inputs.drain());
        if let Err(error) = self.inputs.ready() {
            self.fault = Some(error);
            return false;
        }
        if let Err(error) = self.replies.drain_into(&mut self.pending) {
            self.fault = Some(error);
            return false;
        }
        self.pending
            .extend(kernel::ticked(&mut self.clocks, std::time::Instant::now()));
        if self.staged {
            // a replacement's first tree is already here; only its
            // requests and cancels are still to route
            self.staged = false;
        } else {
            let quiet = self.ticks > 0
                && !self.frame.busy
                && self.pending.is_empty()
                && !self.inputs.pending()
                && self.inputs.ready() != Ok(false);
            if quiet {
                return false;
            }
            self.tick();
            self.ticks += 1;
        }
        for (nth, request) in std::mem::take(&mut self.frame.requests)
            .into_iter()
            .enumerate()
        {
            match nth < MAX_REQUESTS_PER_TICK {
                true => self.answer(request, props),
                false => self.refuse(request.id, "too many requests this tick".into()),
            }
        }
        for id in std::mem::take(&mut self.frame.cancels) {
            self.widget_commands
                .retain(|(request, _, _)| *request != id);
            if self.props_subscription == Some(id) {
                self.props_subscription = None;
            }
            self.live_subscriptions.retain(|(live, _)| *live != id);
            // dropping the stream aborts it: the node socket goes with the
            // subscription the view abandoned
            self.streams.retain(|(stream, _)| *stream != id);
            self.clocks.retain(|clock| clock.id != id);
        }
        self.fault.is_none()
            && (self.frame.busy
                || self.inputs.pending()
                || !self.pending.is_empty()
                || self.inputs.ready() == Ok(false))
    }

    /// Routes one request: the props subscription is answered from what the
    /// app holds, an intent the module declares goes to the app, the log
    /// goes to the log, and anything else is refused.
    fn answer(&mut self, request: wire::Request, props: &Option<Vec<u8>>) {
        let wire::Request { id, kind, payload } = request;
        if payload.len() > MAX_PAYLOAD_BYTES {
            self.refuse(
                id,
                format!("`{kind}` carries more than {MAX_PAYLOAD_BYTES} bytes"),
            );
            return;
        }
        // a test standing in for the node answers its own reads first
        #[cfg(test)]
        if let Some(bytes) = tests::canned_read(&kind, &payload) {
            self.reply(id, Ok(bytes));
            return;
        }
        let (capability, operation) = kind.split_once('.').unwrap_or((kind.as_str(), ""));
        // the kernel contract first: what every view may ask, module-free
        if kernel::answer(self, capability, operation, id, &payload) {
            return;
        }
        let own = capability == self.module;
        let declared_intent = own && intents_of(self.module).contains(&operation);
        match (capability, operation) {
            ("host", "widget") => self.widget_request(id, &payload),
            _ if own && operation == "props" => {
                self.props_subscription = Some(id);
                self.props_sent = None;
                self.sync_props(props);
            }
            _ if declared_intent => self.intents.push(ModuleViewEvent {
                kind: operation.to_owned(),
                detail: String::from_utf8_lossy(&payload).into_owned(),
            }),
            ("host", "log") => {
                tracing::debug!(
                    target: "ducktape::app",
                    module = self.module,
                    line = %String::from_utf8_lossy(&payload),
                    "module view log"
                );
                self.reply(id, Ok(Vec::new()));
            }
            _ => self.refuse(id, format!("unknown request `{kind}`")),
        }
    }

    fn refuse(&mut self, id: u64, message: String) {
        self.reply(id, Err(message));
    }

    fn widget_request(&mut self, id: u64, payload: &[u8]) {
        let admitted = (|| {
            let mut command: wire::WidgetCommand = wire::decode(payload)?;
            let exact_payload = wire::encoded_size(&command) == payload.len() as u64;
            if !exact_payload {
                return Err("widget request has trailing bytes".into());
            }
            command.validate()?;
            let queue_full = self.widget_commands.len() >= MAX_REQUESTS_PER_TICK;
            if queue_full {
                return Err("too many pending widget requests".into());
            }
            // Exhaustive by design: adding a command requires reviewing its scope.
            use wire::WidgetCommand as C;
            let target = match &command {
                C::FocusPrevious | C::FocusNext => None,
                C::Focus { target }
                | C::Focused { target }
                | C::CursorFront { target }
                | C::CursorEnd { target }
                | C::Cursor { target, .. }
                | C::SelectAll { target }
                | C::Select { target, .. }
                | C::Snap { target, .. }
                | C::SnapEnd { target }
                | C::ScrollTo { target, .. }
                | C::ScrollToKey { target, .. }
                | C::ScrollBy { target, .. } => Some(target),
            };
            fn contains(node: &wire::Node, target: &str) -> bool {
                node.key() == Some(target)
                    || node.children().iter().any(|child| contains(child, target))
            }
            if let Some(target) = target {
                let in_scope = self
                    .frame
                    .root
                    .as_ref()
                    .is_some_and(|root| contains(root, target));
                if !in_scope {
                    return Err("widget target is outside this guest tree".into());
                }
            }
            Ok(command)
        })();
        match admitted {
            Ok(command) => self.widget_commands.push((id, self.frame_rev, command)),
            Err(error) => self.refuse(id, error),
        }
    }

    /// Called only on the matching mounted tree after native editor work drains.
    fn execute_widget_commands(
        &mut self,
        mut execute: impl FnMut(wire::WidgetCommand) -> Result<Vec<u8>, String>,
    ) {
        for (id, revision, command) in std::mem::take(&mut self.widget_commands) {
            let result = if revision == self.frame_rev {
                execute(command)
            } else {
                Err("widget request belongs to a replaced frame".into())
            };
            self.reply(id, result);
        }
    }

    fn reply(&mut self, id: u64, result: Result<Vec<u8>, String>) {
        self.pending.push(wire::Event::Response {
            id,
            result,
            done: true,
        });
    }

    fn report_display_truncation(&mut self) {
        let Some(generation) = self.installed_generation else {
            return;
        };
        for origin in self
            .display_diagnostics
            .observe(self.frame_reports)
            .into_iter()
            .flatten()
        {
            tracing::warn!(target: "ducktape::app", module = self.module, generation,
                reason = "display_text_truncated", origin, "module view display text truncated");
        }
    }

    /// One call into the module with the pending events, inside the budget.
    /// A trap ends the view; the widget shows the message in its place.
    fn tick(&mut self) {
        let events = std::mem::take(&mut self.pending);
        let bytes = wire::encode(&events);
        arm(&mut self.store);
        let outcome = self
            .tick
            .call(&mut self.store, (bytes,))
            .map(|(frame,)| frame)
            .map_err(|error| first_line(&error))
            .and_then(|frame| shape(&frame));
        match outcome {
            Ok((mut frame, mut reports)) => {
                let inherits = frame.root.is_none();
                let mut previous = self.frame.root.take();
                let mut accepted = true;
                let merged = merge(&mut previous, &mut frame)
                    .map_err(str::to_owned)
                    .and_then(|changed| {
                        if changed.0
                            && let Some(root) = &frame.root
                        {
                            self.inputs.validate(root)?;
                        }
                        Ok(changed)
                    });
                match merged {
                    Ok((false, _)) => {}
                    Ok((true, report)) => {
                        reports.local.merge(report);
                        self.frame_rev += 1;
                        if let Some(root) = &mut frame.root {
                            self.ever_valid_tree = true;
                            if let Err(error) = self.inputs.replace(root) {
                                self.fault = Some(error);
                            }
                            self.pictures.adopt(root);
                            // The guest remembers its tree without the
                            // picture bytes; the tree its patches build on
                            // has to be that one.
                            root.for_each_mut(&mut |node| match node {
                                wire::Node::Svg { bytes, .. } => *bytes = None,
                                wire::Node::Image { data, .. }
                                | wire::Node::ImageViewer { data, .. } => *data = None,
                                _ => {}
                            });
                        }
                    }
                    // Preserve accepted document state while requesting a full tree.
                    Err(refused) => {
                        accepted = false;
                        frame.root = previous;
                        tracing::warn!(
                            target: "ducktape::app",
                            module = self.module,
                            reason = "module_view_patch_refused",
                            error = refused,
                            "module view patch refused"
                        );
                        self.frame_rev += 1;
                        self.pending.push(wire::Event::Resync);
                    }
                }
                if accepted {
                    if inherits {
                        reports.inherit(self.frame_reports);
                    }
                    self.frame_reports = reports;
                    self.report_display_truncation();
                    if let Err(error) = self.inputs.frame(&frame) {
                        self.fault = Some(error);
                    }
                    self.pending.extend(self.inputs.drain());
                }
                self.frame = frame;
            }
            Err(trap) => {
                let reason = panic_message(&mut self.store).unwrap_or(trap);
                tracing::warn!(
                    target: "ducktape::app",
                    module = self.module,
                    reason = "module_view_trapped",
                    error = %reason,
                    "module view ended"
                );
                self.fault = Some(reason);
                self.frame_rev += 1;
            }
        }
    }
}

/// Brings the tree the host holds into `frame`: an `unchanged` frame takes
/// it as is, a frame without a tree patches it, a frame with one replaces
/// it. `Ok(true)` is a tree the widget has to rebuild for.
fn merge(
    held: &mut Option<wire::Node>,
    frame: &mut wire::Frame,
) -> Result<(bool, wire::SanitizeReport), &'static str> {
    if frame.unchanged {
        frame.root = held.take();
        return Ok((false, Default::default()));
    }
    if frame.root.is_some() {
        return Ok((true, Default::default()));
    }
    let patches = std::mem::take(&mut frame.patches);
    let mut root = held.as_ref().ok_or("no tree to patch")?.clone();
    let report = wire::apply(&mut root, patches)?;
    frame.root = Some(root);
    Ok((true, report))
}

/// What the host is willing to take from one tick's bytes: nothing in here
/// is trusted — the length, the counts, the tree.
fn shape(bytes: &[u8]) -> Result<(wire::Frame, display_diagnostics::FrameReports), String> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err("frame too large".to_string());
    }
    let mut frame: wire::Frame = wire::decode(bytes)?;
    let requests_exceed_budget = frame.requests.len() > MAX_REQUESTS_PER_TICK;
    let cancels_exceed_budget = frame.cancels.len() > 2 * MAX_REQUESTS_PER_TICK;
    if requests_exceed_budget || cancels_exceed_budget {
        return Err("frame request or cancellation budget exceeded".into());
    }
    if frame.unchanged {
        frame.root = None;
    }
    if frame.unchanged || frame.root.is_some() {
        frame.patches = Vec::new();
    }
    let upstream = frame.upstream_sanitization;
    let local = wire::sanitize(&mut frame).map_err(str::to_owned)?;
    frame.upstream_sanitization = upstream;
    Ok((frame, display_diagnostics::FrameReports { local, upstream }))
}

fn panic_message(store: &mut Store<HostState>) -> Option<String> {
    let text = store.data_mut().panic.take()?;
    (!text.is_empty()).then_some(text)
}

/// Why a call failed: the trap itself, not the wrapper and backtrace
/// wasmtime prints around it.
fn first_line(error: &wasmtime::Error) -> String {
    if let Some(wasmtime::Trap::Interrupt) = error.root_cause().downcast_ref::<wasmtime::Trap>() {
        return format!("tick exceeded {} ms", TICK_DEADLINE.as_millis());
    }
    error
        .root_cause()
        .to_string()
        .lines()
        .next()
        .unwrap_or("trap")
        .to_string()
}

#[path = "module_view/display_diagnostics.rs"]
mod display_diagnostics;

#[path = "module_view/input.rs"]
pub(crate) mod input;
#[path = "module_view/pictures.rs"]
mod pictures;
#[path = "module_view/surfaces.rs"]
mod surfaces;

// ---------- the widget ----------

/// The native window retains this entity while a tab is open. A deployment
/// replacement gets a fresh native tree, so no focus or event route survives
/// across guest instances; ordinary guest frames retain keyed control state.
pub(crate) struct NativeModuleView {
    module: &'static str,
    content: Option<gpui_kit::Entity<crate::view_tree::ViewTree>>,
    subscription: Option<gpui_kit::Subscription>,
    generation: u64,
    revision: u64,
    alive: Option<Arc<()>>,
    replies_changed: Option<gpui_kit::Task<()>>,
    deadline: Option<(Instant, gpui_kit::Task<()>)>,
    surfaces: HashMap<String, surfaces::Surface>,
    observers: Vec<gpui_kit::Subscription>,
    hovered_files: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    pointer_inside: std::rc::Rc<std::cell::Cell<bool>>,
}

impl gpui_kit::EventEmitter<ModuleViewEvent> for NativeModuleView {}

impl NativeModuleView {
    pub(crate) fn new(module: &'static str) -> Self {
        Self {
            module,
            content: None,
            subscription: None,
            generation: 0,
            revision: 0,
            alive: None,
            replies_changed: None,
            deadline: None,
            surfaces: HashMap::new(),
            observers: Vec::new(),
            hovered_files: Default::default(),
            pointer_inside: Default::default(),
        }
    }

    pub(crate) fn set_props(&mut self, props: Vec<u8>, cx: &mut gpui_kit::Context<Self>) {
        let seat = mounted(self.module);
        let mut seat = seat.lock().expect("module view lock");
        let changed = seat.props.as_ref() != Some(&props);
        if changed {
            seat.props = Some(props);
            cx.notify();
        }
    }

    /// Closing has no next paint. Deliver the final semantic observation through
    /// one bounded guest redraw and return its intents to the surviving shell.
    pub(crate) fn observe_final_window_event(
        &mut self,
        event: wire::events::Window,
        cx: &mut gpui_kit::Context<Self>,
    ) -> Vec<ModuleViewEvent> {
        let Some(alive) = &self.alive else {
            return Vec::new();
        };
        let seat = mounted(self.module);
        let mut mounted = seat.lock().expect("module view lock");
        let Mounted { slot, props, .. } = &mut *mounted;
        let Slot::Ready(guest) = slot else {
            return Vec::new();
        };
        if guest.seated_generation() != self.generation || !Arc::ptr_eq(alive, &guest.alive) {
            return Vec::new();
        }
        let accepted = input::deliver(
            guest,
            wire::Event::Observation {
                event: wire::events::Event::Window(event),
                captured: false,
            },
        );
        if !accepted {
            return Vec::new();
        }
        guest.redraw(props);
        cx.notify();
        std::mem::take(&mut guest.intents)
    }

    fn frame(
        &mut self,
        window: &mut gpui_kit::Window,
        cx: &mut gpui_kit::Context<Self>,
    ) -> Result<(), String> {
        let mounted = mounted(self.module);
        let mut locked = mounted.lock().expect("module view lock");
        let Mounted { slot, props, .. } = &mut *locked;
        let guest = match slot {
            Slot::Loading => {
                window.request_animation_frame();
                return Err("Loading the view…".into());
            }
            Slot::Empty => {
                return Err(format!(
                    "This network has no {} view. An admin can activate a deployment that ships one.",
                    self.module
                ));
            }
            Slot::Failed(reason) => return Err(reason.clone()),
            Slot::Ready(guest) => guest,
        };
        let generation = guest.seated_generation();
        let ticks = guest.ticks;
        let again = guest.redraw(props);
        if again {
            window.request_animation_frame();
        }
        let next = kernel::next_tick(&guest.clocks);
        let deadline_changed = self.deadline.as_ref().map(|(due, _)| *due) != next;
        if deadline_changed {
            self.deadline = next.map(|due| {
                let timer = cx
                    .background_executor()
                    .timer(due.saturating_duration_since(Instant::now()));
                let task = cx.spawn(async move |view, cx| {
                    timer.await;
                    let _ = view.update(cx, |_, cx| cx.notify());
                });
                (due, task)
            });
        }
        if let Some(fault) = &guest.fault {
            return Err(format!("This view was stopped: {fault}"));
        }
        let same_instance = self.generation == generation
            && self
                .alive
                .as_ref()
                .is_some_and(|alive| Arc::ptr_eq(alive, &guest.alive));
        let changed = !same_instance || self.revision != guest.frame_rev;
        if changed {
            let mut root = guest.frame.root.clone().unwrap_or_else(wire::Node::empty);
            guest.pictures.hydrate(&mut root);
            self.revision = guest.frame_rev;
            match (&self.content, same_instance) {
                (Some(content), true) => content.update(cx, |tree, cx| tree.replace(root, cx)),
                _ => {
                    self.surfaces.clear();
                    self.generation = generation;
                    self.alive = Some(guest.alive.clone());
                    let mut changes = guest.replies.changes();
                    self.replies_changed = Some(cx.spawn(async move |view, cx| {
                        while changes.changed().await.is_ok() {
                            if view.update(cx, |_, cx| cx.notify()).is_err() {
                                break;
                            }
                        }
                    }));
                    // An answer that landed before subscription still needs
                    // delivery; later answers wake the entity directly.
                    if guest.replies.answer_owed() {
                        window.request_animation_frame();
                    }
                    let presentation = self
                        .content
                        .as_ref()
                        .map(|content| content.read(cx).presentation(window, cx))
                        .unwrap_or_default();
                    let content = cx.new(|_| {
                        crate::view_tree::ViewTree::new(root).with_presentation(presentation)
                    });
                    content.update(cx, |tree, cx| {
                        tree.set_editor_store(guest.inputs.clone(), cx)
                    });
                    let seat = mounted.clone();
                    let alive = guest.alive.clone();
                    self.subscription = Some(cx.subscribe(&content, move |this, _, event, cx| {
                        let mut locked = seat.lock().expect("module view lock");
                        let Slot::Ready(guest) = &mut locked.slot else {
                            return;
                        };
                        let current_instance = guest.seated_generation() == generation
                            && Arc::ptr_eq(&alive, &guest.alive);
                        if !current_instance || guest.frame_rev != this.revision {
                            cx.notify();
                            return;
                        }
                        input::deliver(guest, event.clone());
                        cx.notify();
                    }));
                    self.content = Some(content);
                }
            }
            self.sync_surfaces(guest, window, cx)?;
        }
        if let Some(content) = &self.content {
            if ticks != guest.ticks {
                content.update(cx, |_, cx| cx.notify());
            }
            let commands_ready = !guest.inputs.pending() && !guest.widget_commands.is_empty();
            if commands_ready {
                let view = cx.entity().downgrade();
                let seat = mounted.clone();
                let alive = guest.alive.clone();
                let revision = self.revision;
                // The child tree mounts during this frame. A newly opened
                // menu or input cannot receive focus before that render.
                window.defer(cx, move |window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        let mut locked = seat.lock().expect("module view lock");
                        let Slot::Ready(guest) = &mut locked.slot else {
                            return;
                        };
                        let current_frame = guest.seated_generation() == generation
                            && Arc::ptr_eq(&alive, &guest.alive)
                            && guest.frame_rev == revision
                            && this.revision == revision;
                        if !current_frame || guest.inputs.pending() {
                            cx.notify();
                            return;
                        }
                        let Some(content) = &this.content else {
                            return;
                        };
                        guest.execute_widget_commands(|command| {
                            content.update(cx, |tree, cx| {
                                tree.execute_widget_command(command, window, cx)
                            })
                        });
                        cx.notify();
                    });
                });
            }
        }
        for intent in std::mem::take(&mut guest.intents) {
            cx.emit(intent);
        }
        Ok(())
    }
}

impl gpui_kit::Render for NativeModuleView {
    fn render(
        &mut self,
        window: &mut gpui_kit::Window,
        cx: &mut gpui_kit::Context<Self>,
    ) -> impl gpui_kit::IntoElement {
        use gpui_kit::{
            InteractiveElement as _, IntoElement as _, ParentElement as _, Styled as _,
        };
        self.bind_observers(window, cx);
        match self.frame(window, cx) {
            Ok(()) => match &self.content {
                Some(content) => {
                    let mut context = gpui_kit::KeyContext::default();
                    context.set(
                        "ducktape_guest",
                        format!("view{}", cx.entity().entity_id().as_u64()),
                    );
                    // A view owns its own inset: a split pane runs to the edges.
                    gpui_kit::div()
                        .key_context(context)
                        .size_full()
                        .child(input::Observe::new(
                            content.clone().into_any_element(),
                            self,
                            cx,
                        ))
                        .into_any_element()
                }
                None => gpui_kit::div().size_full().into_any_element(),
            },
            Err(reason) => gpui_kit::div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(reason)
                .into_any_element(),
        }
    }
}

#[cfg(test)]
#[path = "module_view/input_tests.rs"]
mod input_tests;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{self as gpui, Entity, TestAppContext, VisualTestContext};

    pub(crate) fn close_observer_fixture() -> NativeModuleView {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/views/governance_view.wasm");
        assert!(
            path.is_file(),
            "close regression requires the staged governance view"
        );
        let mut guest = Guest::load_from("governance", &path).expect("close observer guest");
        guest.installed_generation = Some(1);
        guest.redraw(&None);
        assert!(guest.fault.is_none(), "{:?}", guest.fault);
        let mut view = NativeModuleView::new("governance");
        view.generation = 1;
        view.revision = guest.frame_rev;
        view.alive = Some(guest.alive.clone());
        let seat = fresh("governance");
        let mut seat = seat.lock().unwrap();
        seat.generation = 1;
        seat.slot = Slot::Ready(Box::new(guest));
        view
    }

    pub(crate) fn queue_close_intent(detail: &str) {
        let seat = mounted("governance");
        let mut seat = seat.lock().unwrap();
        let Slot::Ready(guest) = &mut seat.slot else {
            panic!("close guest missing")
        };
        // Governance does not request window events itself. Rearm this host
        // fixture after each real WASM frame and seed an already-produced intent.
        guest.frame.event_interest.close = true;
        guest.intents.push(ModuleViewEvent {
            kind: "close-test".into(),
            detail: detail.into(),
        });
    }

    pub(crate) fn close_observer_reading() -> (u64, usize) {
        let seat = mounted("governance");
        let seat = seat.lock().unwrap();
        let Slot::Ready(guest) = &seat.slot else {
            panic!("close guest missing")
        };
        assert!(guest.fault.is_none(), "{:?}", guest.fault);
        (guest.ticks, guest.pending.len())
    }

    fn native_tree(
        root: wire::Node,
        size: gpui::Size<gpui::Pixels>,
        cx: &mut TestAppContext,
    ) -> (Entity<crate::view_tree::ViewTree>, VisualTestContext) {
        cx.update(gpui_kit::init);
        let window = cx.open_window(size, |_, _| crate::view_tree::ViewTree::new(root));
        let view = window.root(cx).expect("native tree");
        let mut native = VisualTestContext::from_window(window.into(), cx);
        native.update(|window, cx| window.render_frame(cx));
        (view, native)
    }
    fn native_command(
        view: &Entity<crate::view_tree::ViewTree>,
        native: &mut VisualTestContext,
        command: wire::WidgetCommand,
    ) -> Result<Vec<u8>, String> {
        let result = native.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.execute_widget_command(command, window, cx)
            })
        });
        native.update(|window, cx| window.render_frame(cx));
        result
    }
    fn fixture_input(key: &str) -> wire::Node {
        wire::Node::Input {
            options: Default::default(),
            key: key.into(),
            placeholder: String::new(),
            value: "abcd".into(),
            on_input: 0,
            on_submit: None,
            width: None,
            secure: false,
            style: Box::default(),
        }
    }
    pub(super) fn button_key(guest: &Guest, name: &str) -> String {
        let message = button_message(guest, name);
        let mut root = guest.frame.root.clone().unwrap();
        let mut found = None;
        root.for_each_mut(&mut |node| {
            if let wire::Node::Button {
                key,
                on_press: Some(id),
                ..
            } = node
                && *id == message
            {
                found = Some(key.clone());
            }
        });
        found.expect("button key")
    }

    // Explicit manual measurement, not a wall-clock performance assertion.
    // Use a fresh XDG cache directory for cold-cache evidence.
    #[test]
    #[ignore = "requires DUCKTAPE_BENCH_VIEW pointing to a matching governance guest"]
    fn measure_governance_view_code_and_first_tree() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let path =
            std::env::var_os("DUCKTAPE_BENCH_VIEW").expect("DUCKTAPE_BENCH_VIEW is required");
        let bytes = std::fs::read(path).expect("read matching governance wasm");
        let mut config = Config::new();
        config.cranelift_opt_level(OptLevel::Speed);
        config.consume_fuel(true);
        config.epoch_interruption(true);
        let uncached = Engine::new(&config).unwrap();
        let before = Instant::now();
        let baseline = Component::new(&uncached, &bytes).expect("uncached compilation");
        let baseline_time = before.elapsed();
        drop(baseline);
        let before = Instant::now();
        let cold = Guest::compile(&bytes, "benchmark").unwrap();
        let cold_time = before.elapsed();
        let before = Instant::now();
        let warm = Guest::compile(&bytes, "benchmark").unwrap();
        let warm_time = before.elapsed();
        assert!(
            Arc::ptr_eq(&cold, &warm),
            "actual guest code must be reused"
        );
        let before = Instant::now();
        let mut first = Guest::instantiate("governance", &warm, "benchmark").unwrap();
        first.init("benchmark").unwrap();
        let initialized = before.elapsed();
        let before = Instant::now();
        first.redraw(&session_props());
        first.redraw(&session_props());
        assert!(first.fault.is_none(), "{:?}", first.fault);
        assert!(
            texts(&first).iter().any(|text| text == "node-7"),
            "real register tree: {:?}",
            texts(&first)
        );
        let tree_time = before.elapsed();
        let second = Guest::from_bytes("governance", &bytes, "benchmark").unwrap();
        assert!(
            !Arc::ptr_eq(&first.alive, &second.alive),
            "code reuse must not reuse mutable instances"
        );
        assert!(second.frame.root.is_none());
        tracing::info!(
            baseline_compile_us = baseline_time.as_micros(),
            cold_compile_us = cold_time.as_micros(),
            warm_compile_us = warm_time.as_micros(),
            instantiate_init_us = initialized.as_micros(),
            first_tree_us = tree_time.as_micros(),
            source_bytes = bytes.len(),
            "view_code_benchmark"
        );
    }

    #[test]
    fn view_code_reuses_identical_bytes_but_not_changed_code() {
        let engine = Engine::default();
        let cache = Mutex::new(ViewCodeCache::default());
        let first = compile_view(&engine, &cache, b"(component)").unwrap();
        let repeated = compile_view(&engine, &cache, b"(component)").unwrap();
        assert!(
            Arc::ptr_eq(&first, &repeated),
            "warm loads must reuse compiled code"
        );
        let changed = compile_view(&engine, &cache, b"(component (type (func)))").unwrap();
        assert!(!Arc::ptr_eq(&first, &changed));
        assert!(compile_view(&engine, &cache, b"invalid wasm").is_err());
        assert_eq!(cache.lock().unwrap().entries.len(), 2);
    }

    #[test]
    fn view_code_evicts_old_entries_and_bounds_retained_source_size() {
        let engine = Engine::default();
        let cache = Mutex::new(ViewCodeCache::default());
        let oldest = compile_view(&engine, &cache, b"(component)").unwrap();
        for i in 0..COMPILED_VIEW_LIMIT {
            let source = format!("(component) ;; entry {i}");
            compile_view(&engine, &cache, source.as_bytes()).unwrap();
        }
        assert_eq!(cache.lock().unwrap().entries.len(), COMPILED_VIEW_LIMIT);
        let revisited = compile_view(&engine, &cache, b"(component)").unwrap();
        assert!(!Arc::ptr_eq(&oldest, &revisited));
        let mut retained = cache.lock().unwrap();
        retained.insert(CompiledView {
            hash: [99; 32],
            source_bytes: COMPILED_VIEW_SOURCE_BYTES,
            component: revisited,
        });
        assert_eq!(retained.entries.len(), 1);
        assert_eq!(retained.entries[0].hash, [99; 32]);
    }

    /// Only the operations a module declares reach the app; the props
    /// subscription and the log are the host's, everything else is refused.
    /// The governance view declares none: it speaks the kernel contract.
    #[test]
    fn only_declared_intents_are_routed() {
        assert!(intents_of("governance").is_empty());
        assert_eq!(intents_of("members"), ["copy"]);
        // the agents view signs its own pause and save through `op.submit`
        assert_eq!(intents_of("agents"), ["register", "open_run", "open_link"]);
        let chat = intents_of("chat");
        assert_eq!(chat.len(), 15);
        // the writes the view signs for itself are nobody's intent
        for signed in ["react", "edit", "delete", "rename", "search", "mark_read"] {
            assert!(!chat.contains(&signed), "{signed} is an op.submit now");
        }
        assert!(chat.contains(&"choose_channel"));
        assert!(
            chat.contains(&"cancel_run"),
            "stopping an anchored agent run is an act the screen offers"
        );
        assert!(
            !chat.contains(&"composer"),
            "a submit reaches the app only through the composer surface it was typed in"
        );
    }

    /// Every kind a module's decoder names is admitted at that module's
    /// door, so a view never emits an intent the app knows how to decode
    /// but refuses to hear. The kinds a decoder names off its door are
    /// exactly the ones that cross by another route: a host surface's own
    /// event, or the composer's submit. In the other direction the door
    /// admits nothing the decoder leaves to its wildcard, except the kind
    /// the wildcard's verdict itself names.
    #[test]
    fn every_kind_a_decoder_names_is_admitted_at_its_door() {
        use std::collections::BTreeSet;
        let (source, _tests) = include_str!("module_view.rs")
            .split_once("\npub(crate) mod tests {")
            .expect("the tests module");
        let other_route_only: [(&str, &str, &[&str]); 4] = [
            ("agents", "agents_intent", &[]),
            ("settings", "settings_intent", &[]),
            // the composer's own events reach the app off its surface, not
            // through the guest's door
            ("forge", "forge_intent", &["composer", "composer_attach"]),
            ("pages", "pages_intent", &[]),
        ];
        let snake = |variant: &str| -> String {
            let mut word = String::new();
            for (index, letter) in variant.chars().enumerate() {
                let starts_a_word = letter.is_ascii_uppercase() && index > 0;
                if starts_a_word {
                    word.push('_');
                }
                word.push(letter.to_ascii_lowercase());
            }
            word
        };
        for (module, decoder, other_routes) in other_route_only {
            let header = format!("pub fn {decoder}(event: &ModuleViewEvent)");
            let body = &source[source.find(&header).expect(decoder)..];
            let body = &body[..body.find("\n}\n").expect("the decoder's end")];
            let mut arms = BTreeSet::new();
            let mut wildcard_verdict = None;
            for line in body.lines().map(str::trim) {
                let named_arm = line
                    .strip_prefix('"')
                    .and_then(|rest| rest.split_once("\" =>"))
                    .map(|(kind, _)| kind);
                if let Some(kind) = named_arm {
                    arms.insert(kind);
                }
                if let Some(verdict) = line.strip_prefix("_ => ") {
                    let variant = verdict.trim_end_matches(',').rsplit("::").next();
                    wildcard_verdict = variant.map(snake);
                }
            }
            let door: BTreeSet<&str> = intents_of(module).iter().copied().collect();
            let off_door: Vec<&str> = arms.difference(&door).copied().collect();
            assert_eq!(
                off_door, other_routes,
                "{module}: {decoder} names a kind the door refuses"
            );
            let left_to_the_wildcard: Vec<&str> = door.difference(&arms).copied().collect();
            let wildcard_verdict = wildcard_verdict.expect("a total decoder ends in a wildcard");
            assert!(
                left_to_the_wildcard
                    .iter()
                    .all(|kind| *kind == wildcard_verdict),
                "{module}: the door admits {left_to_the_wildcard:?}, which {decoder} decodes only by its wildcard ({wildcard_verdict})"
            );
        }
    }

    /// Every text in the tree the host holds, in tree order.
    pub(super) fn texts(guest: &Guest) -> Vec<String> {
        let mut root = guest.frame.root.clone().expect("a tree");
        let mut texts = Vec::new();
        root.for_each_mut(&mut |node| match node {
            wire::Node::Text { content, .. }
            | wire::Node::Button {
                content: wire::ButtonContent::Label(content),
                ..
            } => texts.push(content.clone()),
            wire::Node::RichText { spans, .. } => {
                texts.push(spans.iter().map(|span| span.content.as_str()).collect())
            }
            _ => {}
        });
        texts
    }

    /// The message index the button labelled `name` — by its `label=`, or
    /// by the text it shows — would send.
    pub(super) fn button_message(guest: &Guest, name: &str) -> u32 {
        let mut root = guest.frame.root.clone().expect("a tree");
        let mut message = None;
        root.for_each_mut(&mut |node| {
            if let wire::Node::Button {
                label,
                content,
                on_press,
                ..
            } = node
                && (label.as_deref() == Some(name)
                    || matches!(content, wire::ButtonContent::Label(text) if text == name))
            {
                message = *on_press;
            }
        });
        message.unwrap_or_else(|| {
            let mut seen = Vec::new();
            let mut root = guest.frame.root.clone().expect("a tree");
            root.for_each_mut(&mut |node| {
                if let wire::Node::Button {
                    label,
                    content,
                    on_press,
                    ..
                } = node
                {
                    let shown = label.clone().unwrap_or_else(|| match content {
                        wire::ButtonContent::Label(text) => text.clone(),
                        _ => "<no label>".to_owned(),
                    });
                    seen.push(format!(
                        "{shown}{}",
                        if on_press.is_some() {
                            ""
                        } else {
                            " (disabled)"
                        }
                    ));
                }
            });
            panic!(
                "no enabled button named {name:?}; the frame has buttons {seen:?} and texts {:?}",
                texts(guest)
            )
        })
    }

    /// `host.id` MINTS FOR A WRITER WITH NO CLOCK. A wasm view has neither
    /// entropy nor a wall clock, so a module whose records are addressed by
    /// ids its writer mints (pages, forge) cannot make one for itself. The
    /// door is module-free — a PREFIX naming the kind of record, never a
    /// module name — and it is bounded on both ends: an empty, an
    /// over-long, or a non-alphanumeric prefix is a payload smuggled in as
    /// a name and is refused.
    #[test]
    fn the_kernel_mints_an_id_for_a_named_prefix_and_refuses_a_payload() {
        let Some(staged) = staged("governance") else {
            return;
        };
        let _turn = blocking_connection_turn();
        let mut guest = Guest::load_from("governance", &staged).expect("the view loads");

        let answered = |guest: &mut Guest, id: u64, prefix: &[u8]| {
            assert!(
                kernel::answer(guest, "host", "id", id, prefix),
                "`host.id` is the kernel's"
            );
            guest
                .pending
                .drain(..)
                .find_map(|event| match event {
                    wire::Event::Response { id: at, result, .. } if at == id => Some(result),
                    _ => None,
                })
                .expect("the door answers in place")
        };

        let first = answered(&mut guest, 1, b"page").expect("a named prefix is minted");
        let second = answered(&mut guest, 2, b"page").expect("a named prefix is minted");
        let first = String::from_utf8(first).expect("an id is text");
        let second = String::from_utf8(second).expect("an id is text");
        assert!(first.starts_with("page-"), "{first}");
        assert_ne!(first, second, "two mints on one device never collide");

        for payload in [
            &b""[..],
            b"   ",
            b"page/../..",
            b"a page",
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            let refusal = answered(&mut guest, 3, payload)
                .expect_err("a prefix that is not one word is refused");
            assert_eq!(refusal, "`host.id` names no prefix", "for {payload:?}");
        }
    }

    /// A module has no clock, so periodic guest subscriptions use the kernel's:
    /// one item per period, on a deadline the window thread keeps. Driven
    /// through the real guest and the real redraw — the instant is the
    /// argument, so the rule is decided rather than waited for, and the
    /// answers are drained through `Replies` exactly as a node call's are.
    #[test]
    fn the_kernels_clock_ticks_once_per_period_and_re_arms() {
        let Some(staged) = staged("governance") else {
            return;
        };
        let _turn = blocking_connection_turn();
        let mut guest = Guest::load_from("governance", &staged).expect("the view loads");

        // A PERIOD, NOT A PAYLOAD: the bytes are `every`'s own i64 LE millis,
        // and a period the window thread would spin on is refused.
        for payload in [
            Vec::new(),
            b"900".to_vec(),
            0_i64.to_le_bytes().to_vec(),
            1_i64.to_le_bytes().to_vec(),
            (-900_i64).to_le_bytes().to_vec(),
            (24 * 60 * 60 * 1_000_i64).to_le_bytes().to_vec(),
        ] {
            let id = 40 + payload.len() as u64;
            assert!(kernel::answer(&mut guest, "clock", "ticks", id, &payload));
            guest.replies.wait_idle();
            guest
                .replies
                .drain_into(&mut guest.pending)
                .expect("bounded replies");
            let refusal = guest
                .pending
                .drain(..)
                .find_map(|event| match event {
                    wire::Event::Response { id: at, result, .. } if at == id => result.err(),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("the kernel refuses {payload:?}"));
            assert_eq!(refusal, "`clock.ticks` names no period");
            assert!(guest.clocks.is_empty(), "for {payload:?}");
        }

        // A NAMED PERIOD is kept, and nothing is due before it.
        let armed = std::time::Instant::now();
        assert!(kernel::answer(
            &mut guest,
            "clock",
            "ticks",
            7,
            &900_i64.to_le_bytes()
        ));
        let due = kernel::next_tick(&guest.clocks).expect("the clock is armed");
        assert!(due >= armed + std::time::Duration::from_millis(900));
        assert!(kernel::ticked(&mut guest.clocks, due - Duration::from_millis(1)).is_empty());

        // AT THE DEADLINE: one item, and the next period armed from it —
        // one item however far behind, so a window that slept owes the view
        // a tick, not the minutes it missed.
        let late = due + Duration::from_secs(60);
        let items = kernel::ticked(&mut guest.clocks, late);
        assert!(
            matches!(
                items.as_slice(),
                [wire::Event::Response { id: 7, result: Ok(bytes), done: false }] if bytes.is_empty()
            ),
            "{items:?}"
        );
        assert_eq!(
            kernel::next_tick(&guest.clocks),
            Some(late + Duration::from_millis(900)),
            "the period is re-armed from the tick that was taken"
        );

        // AND A CANCEL RETIRES IT: the view that stopped asking stops being
        // told, and the shell has no deadline left to draw for.
        guest.frame.cancels = vec![7];
        guest.staged = true;
        guest.redraw(&None);
        assert!(kernel::next_tick(&guest.clocks).is_none());
    }

    /// `rpc.view` READS THE MODULE'S INDEX TIER FOR ANY VIEW THAT ASKS, off
    /// the window thread and answered at the view's next redraw. The FOLD
    /// WAIT it runs first is the pages document save's correctness (its
    /// placement is pinned in `backend/tests/docs.rs`); this is the runtime
    /// proof that the door is wired, that it terminates, and that the
    /// reply is the node's own JSON — synchronised on the kernel's own
    /// answer through `Replies::wait_idle`, never on a clock.
    #[test]
    fn the_kernels_view_read_answers_the_nodes_reply_off_the_window_thread() {
        let Some(staged) = staged("governance") else {
            return;
        };
        let _turn = blocking_connection_turn();
        let mut guest = Guest::load_from("governance", &staged).expect("the view loads");
        let ask = |target: &str| {
            serde_json::to_vec(&serde_json::json!({
                "target": target, "query": { "list_pages": { "after": null, "limit": null } }
            }))
            .expect("the ask encodes")
        };
        let answered = |guest: &mut Guest, id: u64| {
            guest.replies.wait_idle();
            guest
                .replies
                .drain_into(&mut guest.pending)
                .expect("bounded replies");
            guest
                .pending
                .drain(..)
                .find_map(|event| match event {
                    wire::Event::Response { id: at, result, .. } if at == id => Some(result),
                    _ => None,
                })
                .expect("the kernel answers the read")
        };

        // NO NODE: the door refuses in place rather than hanging the view.
        assert!(
            kernel::answer(&mut guest, "rpc", "view", 7, &ask("pages")),
            "`rpc.view` is the kernel's"
        );
        let refusal = answered(&mut guest, 7).expect_err("there is no node behind the kernel yet");
        assert_eq!(refusal, "not connected to a node");

        // A NODE: one request leaves, and the reply the guest gets back is
        // the index tier's own.
        let origin = stub_index_node(r#"{"pages":{"pages":[],"has_more":false}}"#);
        let client = crate::backend::rpc_client(&origin).expect("a client for the stub node");
        connection().lock().expect("views rpc").client = Some(client);

        assert!(kernel::answer(&mut guest, "rpc", "view", 8, &ask("pages")));
        let reply = answered(&mut guest, 8).expect("the stub node answered");
        let reply: serde_json::Value = serde_json::from_slice(&reply).expect("a reply decodes");
        assert_eq!(
            reply,
            serde_json::json!({ "pages": { "pages": [], "has_more": false } }),
            "the view gets the node's own reply, not a rewrapping of it"
        );

        // AND THE TARGET IS VALIDATED, so a view cannot ask the node for a
        // path of its own choosing.
        assert!(kernel::answer(
            &mut guest,
            "rpc",
            "view",
            9,
            &ask("../secrets")
        ));
        let refusal = answered(&mut guest, 9).expect_err("a target that is not a module id");
        assert!(refusal.contains("module"), "{refusal}");
    }

    /// A stub node for the kernel's index reads: it answers every
    /// `/v1/index/<module>/view` with `body` and closes the socket, so the
    /// kernel's own fold probe and the read behind it each get a clean
    /// connection. Blocking sockets on a plain thread — the kernel's runtime
    /// is the one under test, and a stub sharing it would be driven by it.
    fn stub_index_node(body: &'static str) -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the stub node");
        let origin = format!("http://{}", listener.local_addr().expect("stub address"));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut request = Vec::new();
                let mut chunk = [0u8; 2048];
                while let Ok(read) = stream.read(&mut chunk) {
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    if String::from_utf8_lossy(&request).contains("\r\n\r\n") {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });
        origin
    }

    /// The bundled component, end to end through the host, on the kernel
    /// contract: it boots on the offline plate, and once the session says
    /// connected it reads its own register — an `rpc.live` subscription
    /// the kernel keeps, and an `rpc.query` the kernel refuses here (no
    /// node) — so the refusal is what the screen shows, and a block on the
    /// governance plane makes it ask again. Needs `make views`; without the
    /// staged component the test says so and does nothing.
    #[test]
    fn the_staged_governance_view_boots_and_reads_its_register_through_the_kernel() {
        let staged = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/views/governance_view.wasm");
        if !staged.is_file() {
            eprintln!("skipped: no {} — run `make views`", staged.display());
            return;
        }
        // the kernel answers off the app's connection: none here
        let _turn = blocking_connection_turn();
        let mut guest = Guest::load_from("governance", &staged).expect("the view loads");
        let no_props = None;
        assert!(
            !guest.redraw(&no_props),
            "a booted view with no props is quiet"
        );
        assert!(
            guest.props_subscription.is_some(),
            "the view subscribes to its session"
        );
        assert!(
            texts(&guest).iter().any(|text| text == "Not connected"),
            "{:?}",
            texts(&guest)
        );
        assert_eq!(
            surface_names(&guest),
            ["artifact_svg"],
            "the header leaves the deployment's seal to the host"
        );

        // connected: the view asks the kernel for its register
        let session = session_props();
        guest.redraw(&session);
        assert_eq!(
            guest.live_subscriptions.len(),
            1,
            "the view holds one `rpc.live` subscription on its plane"
        );
        // no node behind the kernel: the query is refused, and the view
        // says so in place
        while guest.redraw(&session) {}
        let shown = texts(&guest);
        assert!(
            shown
                .iter()
                .any(|text| text.contains("not connected to a node")),
            "{shown:?}"
        );
        assert!(guest.intents.is_empty(), "{:?}", guest.intents);
        // the same session again is not delivered again
        assert!(
            !guest.redraw(&session),
            "an unchanged session leaves the view quiet"
        );

        // a block on the governance plane: the live item lands and the
        // view reads again
        let live_id = guest.live_subscriptions[0].0;
        guest.pending.push(wire::Event::Response {
            id: live_id,
            result: Ok(b"{}".to_vec()),
            done: false,
        });
        let ticks = guest.ticks;
        guest.redraw(&session);
        assert!(guest.ticks > ticks, "the live item ticked the view");
        assert!(guest.fault.is_none(), "{:?}", guest.fault);
    }

    /// A chat block reaches the app as a CHAT update, never as a plane
    /// update, and the app's own fold of it is not the only reader: the
    /// chat room and a forge item's discussion are views that re-read on
    /// an `rpc.live` hit for the chat plane. Nothing else tells them — a
    /// message committed and indexed stayed off the open room until the
    /// reader left it and came back.
    #[test]
    fn a_chat_block_tells_every_view_holding_the_chat_plane() {
        let Some(staged) = staged("governance") else {
            return;
        };
        let _turn = blocking_connection_turn();
        let mut guest = Guest::load_from("governance", &staged).expect("the view loads");
        const CHAT_LIVE: u64 = 4242;
        guest
            .live_subscriptions
            .push((CHAT_LIVE, "chat".to_owned()));
        let seat = Arc::new(Mutex::new(Mounted {
            slot: Slot::Ready(Box::new(guest)),
            props: None,
            generation: 1,
            hash: None,
            in_flight: false,
            wanted: None,
            waiting_since: None,
            replacement: Replacement::Preserve,
            retry: None,
        }));
        registry()
            .lock()
            .expect("module views")
            .insert("governance", seat.clone());

        let (mut app, _) = crate::Ducktape::boot();
        app.connected = true;
        app.loading = false;
        let serial = app.views_live_serial;
        let _ = app.update(crate::AppMessage::LiveUpdated(crate::backend::LiveUpdate {
            kind: crate::LiveKind::Chat,
            status: "Live".into(),
            height: 12,
            module: "chat".into(),
            ..crate::backend::LiveUpdate::default()
        }));

        let locked = seat.lock().expect("module view lock");
        let Slot::Ready(guest) = &locked.slot else {
            panic!("the seat is still ready")
        };
        let told = guest.pending.iter().any(|event| {
            matches!(event, wire::Event::Response { id, done: false, .. } if *id == CHAT_LIVE)
        });
        assert!(
            told,
            "the chat-plane subscriber was not told: {:?}",
            guest.pending
        );
        assert_eq!(
            app.views_live_serial,
            serial + 1,
            "the serial moves so the redraw that delivers the item follows"
        );
    }

    /// The staged path for `module`, or None with a note when `make views`
    /// has not run.
    /// Redraw until the view is quiet and every editor document it draws
    /// has crossed the transfer: an editor node carries a document
    /// reference, and its bytes arrive over frames, so a test that reads
    /// the document right after the redraw that mounted the editor reads
    /// nothing. The pump is bounded by the frames a transfer can take.
    fn settle_documents(guest: &mut Guest, props: &Option<Vec<u8>>) {
        for _ in 0..128 {
            let busy = guest.redraw(props);
            assert!(guest.fault.is_none(), "{:?}", guest.fault);
            // a node call the kernel is running for the view, or one whose
            // answer landed while the redraw above was still running: its
            // answer is the next redraw's either way, so go round again —
            // waiting on the count, never on a clock
            if guest.replies.answer_owed() {
                guest.replies.wait_idle();
                continue;
            }
            if !busy && guest.inputs.ready() == Ok(true) {
                return;
            }
        }
        panic!(
            "the view did not settle: frame busy={} pending={:?} staged={:?} documents={:?} texts={:?}",
            guest.frame.busy,
            guest.pending,
            guest.staged,
            guest.inputs.ready(),
            texts(guest)
        );
    }

    fn staged(module: &str) -> Option<std::path::PathBuf> {
        let staged = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../target/views/{module}_view.wasm"));
        if staged.is_file() {
            return Some(staged);
        }
        eprintln!("skipped: no {} — run `make views`", staged.display());
        None
    }

    /// The bundled Members view through the host, on the kernel contract:
    /// it boots on the offline plate, and once the session says connected
    /// it reads the roster itself — an `rpc.live` subscription the kernel
    /// keeps on the VALSET plane (another module's, which the kernel
    /// serves an audited view), and an `rpc.status` the kernel refuses here
    /// (no node) — so the refusal is what the screen shows, and a block on
    /// the valset plane makes it ask again.
    #[test]
    fn the_staged_members_view_reads_its_roster_off_the_valset_plane() {
        let Some(staged) = staged("members") else {
            return;
        };
        // the kernel answers off the app's connection: none here
        let _turn = blocking_connection_turn();
        let mut guest = Guest::load_from("members", &staged).expect("the view loads");
        let no_props = None;
        guest.redraw(&no_props);
        assert!(
            guest.props_subscription.is_some(),
            "the view subscribes to its session"
        );
        assert!(
            texts(&guest).iter().any(|text| text == "Not connected"),
            "{:?}",
            texts(&guest)
        );

        let session = session_props();
        guest.redraw(&session);
        assert_eq!(
            guest.live_subscriptions,
            [(guest.live_subscriptions[0].0, "valset".to_string())],
            "the view holds one `rpc.live` subscription, on the valset plane"
        );
        while guest.redraw(&session) {}
        let shown = texts(&guest);
        assert!(
            shown
                .iter()
                .any(|text| text.contains("not connected to a node")),
            "{shown:?}"
        );
        assert!(guest.intents.is_empty(), "{:?}", guest.intents);

        // a block on the valset plane: the live item lands and the view
        // reads again
        let live_id = guest.live_subscriptions[0].0;
        guest.pending.push(wire::Event::Response {
            id: live_id,
            result: Ok(b"{}".to_vec()),
            done: false,
        });
        let ticks = guest.ticks;
        guest.redraw(&session);
        assert!(guest.ticks > ticks, "the live item ticked the view");
        assert!(guest.fault.is_none(), "{:?}", guest.fault);
    }

    /// Every host surface in the guest's tree, by name.
    fn surface_names(guest: &Guest) -> Vec<String> {
        fn walk(node: &wire::Node, out: &mut Vec<String>) {
            if let wire::Node::Surface { name, .. } = node {
                out.push(name.clone());
            }
            for child in node.children() {
                walk(child, out);
            }
        }
        let mut names = Vec::new();
        if let Some(root) = &guest.frame.root {
            walk(root, &mut names);
        }
        names
    }

    /// The bundled Node view through the host: offline plate, then the
    /// SESSION — and off the session alone it reads the node's own facts
    /// for itself, holding one `rpc.live` subscription on the block plane.
    /// It leaves no host surface: the log ring is the guest's now.
    #[test]
    fn the_staged_node_view_boots_and_reads_the_node_through_the_kernel() {
        let Some(staged) = staged("node") else {
            return;
        };
        // the kernel answers off the app's connection: none here
        let _turn = blocking_connection_turn();
        let mut guest = Guest::load_from("node", &staged).expect("the view loads");
        assert!(
            surface_names(&guest).is_empty(),
            "the node view leaves no host surface"
        );
        guest.redraw(&None);
        assert!(
            guest.props_subscription.is_some(),
            "the view subscribes to its session"
        );
        assert!(
            texts(&guest).iter().any(|text| text == "Not connected"),
            "{:?}",
            texts(&guest)
        );

        let session = Some(
            serde_json::to_vec(&serde_json::json!({
                "connected": true, "dark": false, "admin": true, "tier": "validator",
                "status": "Live", "data_dir": "/var/ducktape/demo", "wall_now": 1700000030
            }))
            .expect("props encode"),
        );
        guest.redraw(&session);
        let planes: Vec<&str> = guest
            .live_subscriptions
            .iter()
            .map(|(_, plane)| plane.as_str())
            .collect();
        assert_eq!(
            planes,
            ["block", "block"],
            "the status facts and the peers sample each follow the block plane"
        );
        // no node behind the kernel: the status read is refused, and the
        // view says so in place — but the session facts are its own
        while guest.redraw(&session) {}
        let shown = texts(&guest);
        for expected in ["This node", "/var/ducktape/demo"] {
            assert!(
                shown.iter().any(|text| text == expected),
                "missing {expected:?} in {shown:?}"
            );
        }
        assert!(
            shown
                .iter()
                .any(|text| text.contains("not connected to a node")),
            "{shown:?}"
        );
        assert!(guest.intents.is_empty(), "{:?}", guest.intents);
        assert!(surface_names(&guest).is_empty());
        assert!(guest.fault.is_none(), "{:?}", guest.fault);
    }

    /// The bundled Pages view through the host: session facts in, the
    /// workspace read for itself through the kernel, and one `rpc.live`
    /// subscription on the pages plane. The document editor, its history and
    /// its presentation have always been the guest's; now so is everything it
    /// renders.
    #[test]
    fn the_staged_pages_view_reads_its_workspace_through_the_kernel() {
        let Some(staged) = staged("pages") else {
            return;
        };
        // the kernel answers off the app's connection: none here
        let _turn = blocking_connection_turn();
        let mut guest = Guest::load_from("pages", &staged).expect("the view loads");
        let no_props = None;
        guest.redraw(&no_props);
        assert!(
            guest.props_subscription.is_some(),
            "the view subscribes to its session"
        );
        assert!(
            texts(&guest).iter().any(|text| text == "Not connected"),
            "{:?}",
            texts(&guest)
        );
        // The host paints nothing for this view: the document slot the app
        // used to own left with the loads.
        assert!(!surface_allowed(guest.module, "page_document"));

        let props = pages_facts();
        guest.redraw(&props);
        assert_eq!(
            guest.live_subscriptions,
            [(guest.live_subscriptions[0].0, "pages".to_string())],
            "the view holds one `rpc.live` subscription, on the pages plane"
        );
        while guest.redraw(&props) {}
        assert!(
            texts(&guest).iter().any(|text| text == "Pages"),
            "{:?}",
            texts(&guest)
        );
        assert!(guest.intents.is_empty(), "{:?}", guest.intents);

        // a block on the pages plane: the live item lands and the view reads
        // again
        let live_id = guest.live_subscriptions[0].0;
        guest.pending.push(wire::Event::Response {
            id: live_id,
            result: Ok(b"{}".to_vec()),
            done: false,
        });
        let ticks = guest.ticks;
        guest.redraw(&props);
        assert!(guest.ticks > ticks, "the live item ticked the view");
        assert!(guest.fault.is_none(), "{:?}", guest.fault);
    }

    /// The bundled Chat view through the host: the SESSION facts (the rooms
    /// the bell and the tray share — the stream the view reads for itself),
    /// a room pressed that leaves as `choose_channel`, the composer slot
    /// the host paints per room, and its submit crossing as the `composer`
    /// intent rather than a guest request.
    #[test]
    fn the_staged_chat_view_boots_takes_the_facts_and_leaves_the_composer_to_the_host() {
        let Some(staged) = staged("chat") else {
            return;
        };
        // NO NODE, ON PURPOSE — and the turn is what makes that true. The view
        // reads its own room through the kernel, so a sibling test's seated
        // client would answer those reads off-thread and land their replies in
        // the middle of this one.
        let _turn = blocking_connection_turn();
        let mut guest = Guest::load_from("chat", &staged).expect("the view loads");
        assert!(surface_allowed(guest.module, "chat_composer"));
        guest.redraw(&None);
        let props = chat_facts();
        guest.redraw(&props);
        let shown = texts(&guest);
        // (an unread room carries a dot, not a word)
        for expected in ["Channels", "general", "ops"] {
            assert!(
                shown.iter().any(|text| text == expected),
                "missing {expected:?} in {shown:?}"
            );
        }
        assert_eq!(surface_names(&guest), ["chat_composer"]);

        guest.pending.push(wire::Event::Message(button_message(
            &guest,
            "ops",
        )));
        guest.redraw(&props);
        assert_eq!(
            std::mem::take(&mut guest.intents),
            [ModuleViewEvent {
                kind: "choose_channel".into(),
                detail: r#"{"id":"channel-b"}"#.into(),
            }]
        );

        // a submit in the host's composer is the `composer` intent, and an
        // edit there never reaches the guest. What the ROOM's own reads left
        // waiting is not the composer's doing, so the seam is what the two
        // deliveries ADD — nothing.
        let waiting = guest.pending.len();
        guest.surface_event(
            None,
            wire::SurfaceValue::Record {
                name: "composer".into(),
                fields: vec![
                    (
                        "scope".into(),
                        wire::SurfaceValue::Str("testnet\u{1f}channel-b".into()),
                    ),
                    ("kind".into(), wire::SurfaceValue::Str("message".into())),
                    ("body".into(), wire::SurfaceValue::Str("hello".into())),
                    ("id".into(), wire::SurfaceValue::Str("message-1".into())),
                ],
            },
        );
        guest.surface_event(None, wire::SurfaceValue::Unit);
        assert_eq!(
            guest.pending.len(),
            waiting,
            "the host's composer queued an event for the guest: {:?}",
            guest.pending
        );
        assert_eq!(guest.intents.len(), 1);
        assert_eq!(guest.intents[0].kind, "composer");
        assert!(guest.intents[0].detail.contains(r#""body":"hello""#));
        assert!(guest.fault.is_none());
    }

    #[gpui_kit::test]
    fn a_chat_notification_scrolls_the_native_thread_to_its_target_once(cx: &mut TestAppContext) {
        let _turn = blocking_connection_turn();
        // The landing is a SESSION fact (`land_seq`) and the conversation is
        // the view's own read: the notification names a reply, the window
        // comes back around it, and the row's thread seats the rail.
        let root = chat_row_of(1, "Root of the conversation", None);
        let replies: Vec<_> = (2..=60)
            .map(|seq| {
                let body = format!("Reply {seq}: {}", "conversation context ".repeat(8));
                chat_row_of(seq, &body, Some(1))
            })
            .collect();
        let mut around = vec![root.clone()];
        around.extend(replies.iter().cloned());
        can_reads([
            ("all", chat_accounts_reply()),
            ("channel", chat_channel_reply()),
            ("messages_around", serde_json::json!({ "messages": around })),
            // a landing asks one more question than a tail read does: whether
            // anything is older than the window it centred
            (
                "roots",
                serde_json::json!({ "roots": { "roots": [], "has_more": false }}),
            ),
            ("members", chat_members_reply()),
            (
                "thread",
                serde_json::json!({ "thread": { "root": root, "replies": replies,
                    "has_more": false, "next_reply_seq": null }}),
            ),
        ]);
        let props = chat_facts_in("channel-a", 31, &[]);
        let path = staged("chat").expect("actual Chat Wasm is required");
        let mut guest = Guest::load_from("chat", &path).unwrap();
        guest.redraw(&None);
        for _ in 0..32 {
            if !guest.redraw(&props) {
                break;
            }
        }
        assert_eq!(guest.widget_commands.len(), 1, "{:?}", guest.fault);
        assert!(matches!(
            &guest.widget_commands[0].2,
            wire::WidgetCommand::ScrollToKey { key: 31, .. }
        ));
        let target = match &guest.widget_commands[0].2 {
            wire::WidgetCommand::ScrollToKey { target, .. } => target.clone(),
            _ => unreachable!(),
        };
        let (view, mut native) = native_tree(
            guest.frame.root.clone().unwrap(),
            gpui::size(gpui::px(1280.), gpui::px(800.)),
            cx,
        );
        guest.execute_widget_commands(|command| native_command(&view, &mut native, command));
        let offset = view
            .read_with(&native, |view, _| view.scroll_offset(&target))
            .expect("thread scroll exists");
        assert!(
            offset.y < gpui::px(0.),
            "target is inside the conversation: {offset:?}"
        );
        guest.redraw(&props);
        assert!(
            guest.widget_commands.is_empty(),
            "an unchanged target must not reset reading position"
        );
        assert!(guest.fault.is_none());
    }

    /// The bundled Explorer view end to end through the host, on the kernel
    /// contract: it boots on the offline plate, and once the session says
    /// connected it reads the block window itself — an `rpc.live`
    /// subscription on the `block` plane that the kernel keeps, and an
    /// `rpc.blocks` the kernel refuses here (no node), so the refusal is what
    /// the screen shows. A block on that plane makes it read again. What the
    /// window folds to is pinned in the view's own tests, which drive the same
    /// compiled Rust guest through the wire.
    #[test]
    fn the_staged_explorer_view_boots_and_reads_its_window_through_the_kernel() {
        let Some(staged) = staged("explorer") else {
            return;
        };
        // the kernel answers off the app's connection: none here
        let _turn = blocking_connection_turn();
        let mut guest = Guest::load_from("explorer", &staged).expect("the view loads");
        guest.redraw(&None);
        assert!(
            guest.props_subscription.is_some(),
            "the view subscribes to its session"
        );
        assert!(
            texts(&guest).iter().any(|text| text == "Not connected"),
            "the offline plate is what an unconnected Explorer shows"
        );

        let session = Some(
            serde_json::to_vec(&serde_json::json!({
                "connected": true, "dark": false, "head": 84_912, "sync_line": "live"
            }))
            .expect("props encode"),
        );
        guest.redraw(&session);
        assert_eq!(
            guest.live_subscriptions.len(),
            1,
            "the view holds one `rpc.live` subscription for the block plane"
        );
        assert_eq!(guest.live_subscriptions[0].1, "block");
        // no node behind the kernel: the read is refused, and the view says
        // so in place
        while guest.redraw(&session) {}
        let shown = texts(&guest);
        assert!(
            shown
                .iter()
                .any(|text| text.contains("not connected to a node")),
            "{shown:?}"
        );
        assert!(guest.intents.is_empty(), "{:?}", guest.intents);
        assert!(
            !guest.redraw(&session),
            "an unchanged session leaves the view quiet"
        );

        // a block: the live item lands and the view reads again
        let live_id = guest.live_subscriptions[0].0;
        guest.pending.push(wire::Event::Response {
            id: live_id,
            result: Ok(b"{}".to_vec()),
            done: false,
        });
        let ticks = guest.ticks;
        guest.redraw(&session);
        assert!(guest.ticks > ticks, "the live item ticked the view");
        assert!(guest.fault.is_none(), "{:?}", guest.fault);
    }

    /// The bundled Settings view through the host, on the KERNEL CONTRACT:
    /// session facts go in — the seat as a FLAG and its PUBLIC key, never the
    /// password — the view subscribes to its own reads (this node's standing
    /// and the seat's key associations, both refused here with no node), and
    /// the one thing that leaves is an intent the kernel would sign.
    #[test]
    fn the_staged_settings_view_boots_takes_the_facts_and_sends_a_rename() {
        let Some(staged) = staged("settings") else {
            return;
        };
        // the kernel answers off the app's connection: none here
        let _turn = blocking_connection_turn();
        let mut guest = Guest::load_from("settings", &staged).expect("the view loads");
        guest.redraw(&None);
        assert!(
            guest.props_subscription.is_some(),
            "the view subscribes to its session"
        );
        let props = Some(
            serde_json::to_vec(&serde_json::json!({
                "dark": false, "connected": true, "loading": false, "status": "Connected",
                "busy": false, "recovering": false, "appearance": "system",
                "desktop_notifications": true, "unlocked": true,
                "seat_key": "ab12cd34",
                "account_name": "duck", "network_name": "testnet",
                "connected_rpc": "http://127.0.0.1:1",
                "account_ceremony_phase": "", "account_ceremony_qr": "",
                "account_ceremony_detail": "", "account_ceremony_left": "",
                "settings_key_state": "sealed", "settings_key_path": "/keys/user.key",
                "account_number": "42", "account_exists": true,
                "account_busy": false, "account_ticket": ""
            }))
            .expect("props encode"),
        );
        guest.redraw(&props);
        let shown = texts(&guest);
        for expected in ["Settings", "Appearance"] {
            assert!(
                shown.iter().any(|text| text == expected),
                "missing {expected:?} in {shown:?}"
            );
        }
        guest
            .pending
            .push(wire::Event::Message(button_message(&guest, "Account")));
        guest.redraw(&props);
        guest
            .pending
            .push(wire::Event::Message(button_message(&guest, "Copy number")));
        guest.redraw(&props);
        assert_eq!(
            std::mem::take(&mut guest.intents),
            [ModuleViewEvent {
                kind: "copy".into(),
                detail: r#"{"text":"42","label":"Number copied"}"#.into(),
            }]
        );
        assert!(guest.fault.is_none());
    }

    /// The bundled Files view through the host, on the KERNEL CONTRACT: it
    /// boots on the offline plate, and once the session says connected it
    /// lists the directory itself — an `rpc.live` subscription the kernel
    /// keeps, and a `files.get` the kernel refuses here (no node) — so the
    /// refusal is what the screen shows, and a block on the files plane makes
    /// it ask again. Needs `make views`; without the staged component the test
    /// says so and does nothing.
    #[test]
    fn the_staged_files_view_boots_and_reads_duckfs_through_the_kernel() {
        let Some(staged) = staged("files") else {
            return;
        };
        // the kernel answers off the app's connection: none here
        let _turn = blocking_connection_turn();
        let mut guest = Guest::load_from("files", &staged).expect("the view loads");
        let no_props = None;
        // the first frame is drawn before any facts, and it settles at once:
        // with nothing connected the view asks the kernel for nothing
        assert!(
            !guest.redraw(&no_props),
            "the offline plate waits on nothing"
        );
        assert!(
            guest.props_subscription.is_some(),
            "the view subscribes to its session"
        );
        assert!(
            texts(&guest).iter().any(|text| text == "Not connected"),
            "{:?}",
            texts(&guest)
        );

        // connected: the view asks the kernel for its own directory
        let session = files_facts();
        guest.redraw(&session);
        assert_eq!(
            guest.live_subscriptions.len(),
            1,
            "the view holds one `rpc.live` subscription on its plane"
        );
        // no node behind the kernel: the read is refused, and the view says
        // so in place
        while guest.redraw(&session) {}
        let shown = texts(&guest);
        assert!(
            shown
                .iter()
                .any(|text| text.contains("not connected to a node")),
            "{shown:?}"
        );
        assert!(guest.intents.is_empty(), "{:?}", guest.intents);
        assert!(
            !guest.redraw(&session),
            "an unchanged session leaves the view quiet"
        );

        // a block on the files plane: the live item lands and the view reads
        // again
        let live_id = guest.live_subscriptions[0].0;
        guest.pending.push(wire::Event::Response {
            id: live_id,
            result: Ok(b"{}".to_vec()),
            done: false,
        });
        let ticks = guest.ticks;
        guest.redraw(&session);
        assert!(guest.ticks > ticks, "the live item ticked the view");
        assert!(guest.fault.is_none(), "{:?}", guest.fault);
    }

    /// A module-owned view has one source, the module's deployment on the
    /// connected node: with no node it is not there yet, and the staged file
    /// a desktop view would take is never opened for it.
    #[test]
    fn a_module_owned_view_never_comes_from_the_staged_file() {
        // a staged file for every one of them, where `views_dir` would look
        let _turn = blocking_connection_turn();
        let staged = tempfile::tempdir().expect("a staging dir");
        for module in crate::backend::view_source::MODULE_OWNED {
            std::fs::write(staged.path().join(format!("{module}_view.wasm")), b"\0asm")
                .expect("staged");
        }
        // SAFETY: set under the connection turn, the only one a desktop
        // view — the one reader of this variable — is loaded under;
        // `every_desktop_view_is_asked_at_boot` sets it too, under its own.
        unsafe { std::env::set_var("DUCKTAPE_VIEWS_DIR", staged.path()) };
        for module in crate::backend::view_source::MODULE_OWNED {
            let mounted = Mounted::seat();
            assert_eq!(
                Guest::load(module, None, 0, &mounted)
                    .err()
                    .map(|unloaded| unloaded.reason)
                    .as_deref(),
                Some("not connected to a node yet"),
                "{module}"
            );
        }
        assert!(!crate::backend::view_source::module_owned("settings"));
    }

    /// A load still in flight when the app moves to another node lands
    /// nowhere: the node it was asked of may answer late, and its view is
    /// not the view of the node the app is on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_load_the_previous_node_answers_late_is_not_installed() {
        let _turn = connection_turn().await;
        use crate::backend::view_source::tests::node;
        use module_artifact::{ModuleArtifact, ViewArtifact};
        let Some(staged) = staged("governance") else {
            return;
        };
        let component = std::fs::read(staged).expect("the staged view");
        let deployment = |asset: &str| ModuleArtifact {
            component: vec![1, 2, 3],
            index: None,
            view: Some(ViewArtifact {
                component: component.clone(),
                assets: [(asset.to_owned(), b"<svg/>".to_vec())].into(),
            }),
        };
        let status = |artifact: &ModuleArtifact| {
            serde_json::json!({"module_status": {"modules": [
                {"module_id": "forge", "active_code_hash": artifact.hash().to_vec(),
                 "pending": null, "history": []}
            ]}})
        };
        let (a, b) = (deployment("a.svg"), deployment("b.svg"));
        let hold = Arc::new(tokio::sync::Notify::new());
        let node_a = node(status(&a), Some(a), Some(hold.clone())).await;
        let node_b = node(status(&b), Some(b), None).await;

        // seated with no node: nothing asked yet; then A is asked and holds
        let mounted = mounted("forge");
        let asked_of_a = connected(&node_a);
        // the app moves to B while A is still composing its answer
        let asked_of_b = connected(&node_b);
        asked_of_b.joined();
        hold.notify_one();
        asked_of_a.joined();
        let locked = mounted.lock().expect("module view lock");
        let Slot::Ready(guest) = &locked.slot else {
            panic!("the view of B is not there");
        };
        assert!(
            guest.assets.contains_key("b.svg"),
            "{:?}",
            guest.assets.keys()
        );
        assert!(
            !guest.assets.contains_key("a.svg"),
            "A's late answer landed"
        );
    }

    /// The bundled Forge view through the host on the kernel contract: it
    /// boots on the offline plate, and once the session says connected it
    /// reads the repo namespace ITSELF — an `rpc.live` subscription the
    /// kernel keeps, and an `rpc.query` the kernel refuses here (no node),
    /// so the refusal is what the screen shows. Its four reader surfaces
    /// stay the host's to paint. Needs `make views`.
    #[test]
    fn the_staged_forge_view_boots_and_reads_its_repos_through_the_kernel() {
        let Some(staged) = staged("forge") else {
            return;
        };
        // the kernel answers off the app's connection: none here
        let _turn = blocking_connection_turn();
        let mut guest = Guest::load_from("forge", &staged).expect("the view loads");
        let no_props = None;
        assert!(
            !guest.redraw(&no_props),
            "a booted view with no props is quiet"
        );
        assert!(
            texts(&guest).iter().any(|text| text == "Not connected"),
            "{:?}",
            texts(&guest)
        );

        let session = forge_facts();
        guest.redraw(&session);
        // one per open slice — the namespace, the repo, the item, the
        // browse — on the forge plane, plus the item's discussion on the
        // CHAT plane, which is where a note it draws is committed
        let planes: std::collections::BTreeSet<&str> = guest
            .live_subscriptions
            .iter()
            .map(|(_, plane)| plane.as_str())
            .collect();
        assert_eq!(
            planes,
            ["chat", "forge"].into_iter().collect(),
            "{:?}",
            guest.live_subscriptions
        );
        while guest.redraw(&session) {
            guest.replies.wait_idle();
        }
        let shown = texts(&guest);
        assert!(
            shown
                .iter()
                .any(|text| text.contains("not connected to a node")),
            "{shown:?}"
        );
        assert!(guest.intents.is_empty(), "{:?}", guest.intents);

        // a block on the forge plane: the live item lands and it reads again
        let live_id = guest.live_subscriptions[0].0;
        guest.pending.push(wire::Event::Response {
            id: live_id,
            result: Ok(b"{}".to_vec()),
            done: false,
        });
        let ticks = guest.ticks;
        guest.redraw(&session);
        assert!(guest.ticks > ticks, "the live item ticked the view");

        for surface in ["picture", "forge_markdown", "forge_code", "forge_composer"] {
            assert!(
                surface_allowed("forge", surface),
                "the host paints the {surface} slot the view leaves"
            );
        }
        assert!(guest.fault.is_none(), "{:?}", guest.fault);
    }

    /// A deployment of `module` on the fake node: the staged governance
    /// component as its view, told apart by the one asset it ships.
    fn deployment(component: &[u8], asset: &str) -> module_artifact::ModuleArtifact {
        module_artifact::ModuleArtifact {
            component: vec![1, 2, 3],
            index: None,
            view: Some(module_artifact::ViewArtifact {
                component: component.to_vec(),
                assets: [(asset.to_owned(), b"<svg/>".to_vec())].into(),
            }),
        }
    }

    fn slot_assets(mounted: &Arc<Mutex<Mounted>>) -> Vec<String> {
        let locked = mounted.lock().expect("module view lock");
        match &locked.slot {
            Slot::Ready(guest) => guest.assets.keys().cloned().collect(),
            other => panic!("not a view: {}", slot_name(other)),
        }
    }

    fn slot_name(slot: &Slot) -> String {
        match slot {
            Slot::Loading => "loading".into(),
            Slot::Ready(_) => "ready".into(),
            Slot::Empty => "empty".into(),
            Slot::Failed(reason) => format!("failed: {reason}"),
        }
    }

    /// The connection is one per process: the tests that move it take
    /// turns, so one's node is not another's — including the round trip
    /// over a real node in `backend::tests::wire`, whose connect reloads
    /// every seat here from a node that runs none of these modules.
    fn connection_turn_lock() -> &'static tokio::sync::Mutex<()> {
        static TURN: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        TURN.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    fn reset_connection_turn() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        // Retire intentionally busy/failed seats left by the previous test.
        // Keep revisions monotonic so detached old loads remain stale.
        let mut registry = registry().lock().expect("module views");
        let mut connection = connection().lock().expect("views rpc");
        registry.clear();
        connection.client = None;
        connection.rev += 1;
    }

    /// One test's turn over the seats: taken with them retired, and
    /// retiring them again when it ends, so a test outside a turn never
    /// inherits what a deployment left seated — a pages test reads the
    /// `pages` seat through `current_page_document`, and a seat another
    /// test left behind is a document it never staged.
    pub(crate) struct ConnectionTurn {
        _held: tokio::sync::MutexGuard<'static, ()>,
    }

    impl Drop for ConnectionTurn {
        fn drop(&mut self) {
            reset_connection_turn();
        }
    }

    pub(crate) async fn connection_turn() -> ConnectionTurn {
        let held = connection_turn_lock().lock().await;
        reset_connection_turn();
        ConnectionTurn { _held: held }
    }

    /// Hold this outside allocation measurement until the render thread joins.
    pub(crate) fn blocking_connection_turn() -> ConnectionTurn {
        let held = connection_turn_lock().blocking_lock();
        reset_connection_turn();
        ConnectionTurn { _held: held }
    }

    #[test]
    fn connection_turn_excludes_renderers_and_retires_the_previous_busy_guest() {
        let turn = blocking_connection_turn();
        let staged = staged("chat").expect("build the actual Chat guest");
        let mut guest = Guest::load_from("chat", &staged).expect("actual Chat guest");
        guest.pending.push(wire::Event::Resync);
        let seat = Arc::new(Mutex::new(Mounted {
            slot: Slot::Ready(Box::new(guest)),
            props: Some(b"previous test props".to_vec()),
            generation: 9,
            hash: Some([7; 32]),
            in_flight: false,
            wanted: None,
            waiting_since: None,
            replacement: Replacement::Preserve,
            retry: None,
        }));
        registry().lock().unwrap().insert("chat", seat.clone());
        let revision = connection().lock().unwrap().rev;
        std::thread::spawn(|| {
            assert!(
                connection_turn_lock().try_lock().is_err(),
                "a renderer cannot enter while a deployment owns the seats"
            );
        })
        .join()
        .unwrap();
        drop(turn);
        let _next = blocking_connection_turn();
        assert!(
            registry().lock().unwrap().is_empty(),
            "the next test must not inherit a pending guest or its props/assets"
        );
        assert!(connection().lock().unwrap().rev > revision);
        let locked = seat.lock().unwrap();
        let Slot::Ready(guest) = &locked.slot else {
            panic!("old seat")
        };
        assert_eq!(
            guest.pending.len(),
            1,
            "retirement must not edit the old guest"
        );
    }

    /// The seat of `module` as no test has touched it: the registry is one
    /// per process, and every deployment test leaves its module drawn.
    fn fresh(module: &'static str) -> Arc<Mutex<Mounted>> {
        registry().lock().expect("module views").remove(module);
        mounted(module)
    }

    /// Set by a test: the next candidate's first frame traps.
    pub(super) static FIRST_FRAME_TRAPS: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    thread_local! {
        /// What a test answers the reads a kernel-contract view makes for
        /// itself, so a HOST contract — a native overlay, a retained one, a
        /// pointer drag — can be driven against a real view with no node
        /// behind it. Per-thread, so one test's node is never another's, and
        /// empty everywhere else: an empty table leaves every request to the
        /// kernel exactly as production does.
        static CANNED_READS: std::cell::RefCell<std::collections::BTreeMap<String, Vec<u8>>> =
            const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
    }

    /// The canned answer for one request, looked up by the NAME OF THE QUERY
    /// it asks (`roots`, `channel`, `thread`, …) — the one thing that tells a
    /// view's reads apart, since they all leave as `rpc.view`. A read that
    /// carries no query is looked up by its kind.
    pub(super) fn canned_read(kind: &str, payload: &[u8]) -> Option<Vec<u8>> {
        CANNED_READS.with(|canned| {
            let canned = canned.borrow();
            if canned.is_empty() {
                return None;
            }
            let ask: serde_json::Value = serde_json::from_slice(payload).unwrap_or_default();
            let named = ask["query"]
                .as_object()
                .and_then(|query| query.keys().next().cloned());
            canned.get(named.as_deref().unwrap_or(kind)).cloned()
        })
    }

    /// Cans the node this thread's view reads: every answer by the name of
    /// the query that asks for it.
    pub(super) fn can_reads(answers: impl IntoIterator<Item = (&'static str, serde_json::Value)>) {
        CANNED_READS.with(|canned| {
            let mut canned = canned.borrow_mut();
            for (named, reply) in answers {
                canned.insert(named.to_owned(), reply.to_string().into_bytes());
            }
        });
    }

    /// The session facts a kernel-contract view is pushed: connected, as an
    /// admin. Governance and members take the same three.
    fn session_props() -> Option<Vec<u8>> {
        Some(
            serde_json::to_vec(&serde_json::json!({
                "admin": true, "connected": true, "dark": false
            }))
            .expect("props encode"),
        )
    }

    /// The node's `proposals` reply the governance view reads for itself
    /// through the kernel: one open proposal, `prop-1`.
    fn proposals_reply() -> serde_json::Value {
        serde_json::json!({ "proposals": [{
            "proposal_id": "prop-1",
            "action": { "add_validator": { "key": [7, 7, 7, 7] } },
            "proposer": [1, 2, 3], "created_at": 1, "deadline": 4200,
            "status": "open", "votes": [[[1], true]], "voter_kind": "validator_node",
            "electorate": [[[1], 1], [[2], 1], [[3], 1], [[4], 1]],
            "voting_rule": { "threshold": { "required_yes": 2 } }
        }]})
    }

    /// Holds the node's next blob answer until released.
    fn hold_blob(
        node: &crate::backend::view_source::tests::FakeDeployment,
    ) -> Arc<tokio::sync::Notify> {
        let hold = Arc::new(tokio::sync::Notify::new());
        *node.hold.lock().unwrap() = Some(hold.clone());
        hold
    }

    /// Holds the node's next status answer until released.
    fn hold_status(
        node: &crate::backend::view_source::tests::FakeDeployment,
    ) -> Arc<tokio::sync::Notify> {
        let hold = Arc::new(tokio::sync::Notify::new());
        *node.hold_status.lock().unwrap() = Some(hold.clone());
        hold
    }

    /// The connect seats and loads every module-owned view, drawn or not,
    /// and a draw before any node asks for nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_module_owned_view_is_asked_at_connect_not_at_its_tabs_first_draw() {
        let _turn = connection_turn().await;
        use crate::backend::view_source::MODULE_OWNED;
        use crate::backend::view_source::tests::{FakeDeployment, fake_node};
        let Some(staged) = staged("governance") else {
            return;
        };
        let component = std::fs::read(staged).expect("the staged view");
        let a = deployment(&component, "a.svg");
        let client = fake_node(FakeDeployment::serving("governance", &a)).await;
        // A presenter asks for a seat before any node: nothing starts loading.
        drop(mounted("files"));
        {
            let seat = mounted("files");
            let locked = seat.lock().expect("module view lock");
            assert!(matches!(locked.slot, Slot::Loading));
            assert!(!locked.in_flight, "the draw started a load");
            assert_eq!(locked.generation, 0);
        }
        let loads = connected(&client);
        assert_eq!(
            loads.started(),
            MODULE_OWNED.len(),
            "one load per module-owned view, drawn or not"
        );
        for module in MODULE_OWNED {
            let seat = mounted(module);
            let locked = seat.lock().expect("module view lock");
            assert_eq!(
                locked.generation, 1,
                "{module} was not asked of the node at connect"
            );
        }
        loads.joined();
        assert_eq!(slot_assets(&mounted("governance")), ["a.svg"]);
        for module in MODULE_OWNED {
            registry().lock().expect("module views").remove(module);
        }
    }

    /// The desktop's own views are all asked for at boot and joined before
    /// the window, so the first draw of any of their tabs finds the view
    /// there: every one of them, once the boot's loads are in.
    #[test]
    fn every_desktop_view_is_asked_at_boot() {
        let _turn = blocking_connection_turn();
        use crate::backend::view_source::DESKTOP_OWNED;
        let Some(staged) = staged("members") else {
            return;
        };
        // SAFETY: set under the connection turn, the only one a desktop
        // view is loaded under; `a_module_owned_view_never_comes_from_the_
        // staged_file` sets it too, under its own turn.
        unsafe { std::env::set_var("DUCKTAPE_VIEWS_DIR", staged.parent().expect("staging dir")) };
        let loads = booted();
        assert_eq!(
            loads.started(),
            DESKTOP_OWNED.len(),
            "one load per desktop view"
        );
        for module in DESKTOP_OWNED {
            let seat = mounted(module);
            let locked = seat.lock().expect("module view lock");
            assert_eq!(locked.generation, 1, "{module} was not asked for at boot");
        }
        loads.joined();
        for module in DESKTOP_OWNED {
            let seat = mounted(module);
            let locked = seat.lock().expect("module view lock");
            assert!(!locked.in_flight, "{module}");
            match &locked.slot {
                Slot::Ready(_) => {}
                Slot::Failed(reason) => panic!("{module} is not there: {reason}"),
                Slot::Loading | Slot::Empty => panic!("{module}'s load never answered"),
            }
        }
        for module in DESKTOP_OWNED {
            registry().lock().expect("module views").remove(module);
        }
    }

    /// The desktop's own view is the same on every node: its load lands
    /// though the app moved to a node meanwhile, where a module's view
    /// asked of the node since left lands nowhere.
    #[test]
    fn a_desktop_views_load_lands_after_the_app_moves_where_a_modules_does_not() {
        let _turn = blocking_connection_turn();
        let since_left = Connection {
            client: None,
            rev: connection().lock().expect("views rpc").rev - 1,
        };
        for (module, lands) in [("members", true), ("files", false)] {
            let seat = Mounted::seat();
            let generation = seat.lock().expect("module view lock").start(None);
            spawn_load(module, &seat, generation, since_left.clone())
                .join()
                .expect("the load");
            let locked = seat.lock().expect("module view lock");
            assert!(!locked.in_flight, "{module}");
            let landed = !matches!(locked.slot, Slot::Loading);
            assert_eq!(landed, lands, "{module}");
        }
    }

    /// The connect answers only once every module-owned view has settled:
    /// while one node answer is still held, the loads are not settled; the
    /// moment it is released they are, and no seat is on its way.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_connect_settles_only_once_every_module_view_answered() {
        let _turn = connection_turn().await;
        use crate::backend::view_source::MODULE_OWNED;
        use crate::backend::view_source::tests::{FakeDeployment, fake_node};
        let Some(staged) = staged("governance") else {
            return;
        };
        let component = std::fs::read(staged).expect("the staged view");
        let a = deployment(&component, "a.svg");
        let node = FakeDeployment::serving("governance", &a);
        let hold = hold_status(&node);
        let client = fake_node(node.clone()).await;
        let settled = tokio::spawn(connected(&client).settled());
        // one status answer is waiting on the hold: that load cannot be
        // in, so neither can the loads
        node.held.notified().await;
        assert!(
            !settled.is_finished(),
            "the connect settled with a node answer still held"
        );
        hold.notify_one();
        settled.await.expect("the loads settle");
        for module in MODULE_OWNED {
            let seat = mounted(module);
            let locked = seat.lock().expect("module view lock");
            assert!(!locked.in_flight, "{module} is still on its way");
            assert!(
                !matches!(locked.slot, Slot::Loading),
                "{module} settled without an answer"
            );
        }
        assert_eq!(slot_assets(&mounted("governance")), ["a.svg"]);
        for module in MODULE_OWNED {
            registry().lock().expect("module views").remove(module);
        }
    }

    /// A seat keeps what it shows through a reconnect: one failed off the
    /// previous node shows that failure, never "Loading", until the new
    /// node's answer lands — and then that.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reconnect_keeps_what_a_seat_shows_until_the_new_node_answers() {
        let _turn = connection_turn().await;
        use crate::backend::view_source::tests::{FakeDeployment, fake_node, node};
        let Some(staged) = staged("governance") else {
            return;
        };
        let component = std::fs::read(staged).expect("the staged view");
        let a = deployment(&component, "a.svg");
        // a node that runs none of the modules: every seat fails
        let runs_nothing = node(
            serde_json::json!({"module_status": {"modules": []}}),
            None,
            None,
        )
        .await;
        connected(&runs_nothing).joined();
        let seat = mounted("governance");
        assert!(
            matches!(seat.lock().expect("module view lock").slot, Slot::Failed(_)),
            "the seat did not fail off the node that runs nothing"
        );
        // the app moves to a node that runs governance, whose answer waits
        let node_b = FakeDeployment::serving("governance", &a);
        let hold = hold_status(&node_b);
        let client = fake_node(node_b.clone()).await;
        let loads = connected(&client);
        node_b.held.notified().await;
        {
            let locked = seat.lock().expect("module view lock");
            assert!(locked.in_flight, "governance was not asked of the new node");
            assert!(
                matches!(locked.slot, Slot::Failed(_)),
                "the seat went back to loading while the new node composed its answer"
            );
        }
        hold.notify_one();
        loads.joined();
        assert_eq!(slot_assets(&seat), ["a.svg"]);
        for module in crate::backend::view_source::MODULE_OWNED {
            registry().lock().expect("module views").remove(module);
        }
    }

    /// A load starts at a view's source event and never at a draw: the only
    /// callers of `spawn_load` are the boot, the connect and the block
    /// check, and the views the shell draws are exactly the ones those ask
    /// for.
    #[test]
    fn a_load_starts_at_a_source_event_never_at_a_draw() {
        use crate::backend::view_source::{DESKTOP_OWNED, MODULE_OWNED};
        use std::collections::BTreeSet;
        let (shell, _tests) = include_str!("module_view.rs")
            .split_once("\npub(crate) mod tests {")
            .expect("the tests module");
        let mut current = "";
        let mut callers = BTreeSet::new();
        for line in shell.lines() {
            let trimmed = line.trim_start();
            let header = [
                "pub fn ",
                "pub(crate) fn ",
                "pub async fn ",
                "async fn ",
                "fn ",
            ]
            .into_iter()
            .find_map(|keyword| trimmed.strip_prefix(keyword));
            if let Some(header) = header {
                current = header.split(['(', '<']).next().unwrap_or(header);
                continue;
            }
            if trimmed.contains("spawn_load(") {
                callers.insert(current);
            }
        }
        assert_eq!(
            callers,
            BTreeSet::from(["booted", "connected", "deployments_check"]),
            "a load started outside the boot, the connect and the block check"
        );
        let drawn: BTreeSet<&str> = shell
            .split("module_view(")
            .skip(1)
            .filter_map(|after| after.trim_start().strip_prefix('"'))
            .filter_map(|name| name.split('"').next())
            .collect();
        let asked: BTreeSet<&str> = MODULE_OWNED.into_iter().chain(DESKTOP_OWNED).collect();
        assert_eq!(
            drawn, asked,
            "the views the shell draws are not the views the boot and the connect ask for"
        );
    }

    /// The node's `ls` reply the files view reads for itself through the
    /// kernel: one directory holding one file.
    fn files_listing_reply() -> serde_json::Value {
        serde_json::json!({ "entries": [
            { "path": "/shared/README.md", "kind": "file", "size": 1024, "object": "bb" }
        ]})
    }

    /// The files view's session facts: connected, on a named chain.
    fn files_facts() -> Option<Vec<u8>> {
        Some(
            serde_json::to_vec(&serde_json::json!({
                "connected": true, "dark": false, "chain": "chain-a",
                "route": "", "route_serial": 0
            }))
            .expect("props encode"),
        )
    }

    #[gpui_kit::test]
    fn widget_commands_reach_a_mounted_nested_overlay_without_touching_a_sibling(
        cx: &mut TestAppContext,
    ) {
        use wire::WidgetCommand as C;
        let popup = |key: &str, content| wire::Node::Overlay {
            key: key.into(),
            padding: 0.,
            backdrop: wire::Rgba([0.; 4]),
            align_x: wire::AlignX::Left,
            align_y: wire::AlignY::Top,
            on_dismiss: None,
            children: vec![wire::Node::empty(), content],
        };
        let root = popup("outer", popup("inner", fixture_input("popup-draft")));
        let (view, mut native) =
            native_tree(root.clone(), gpui::size(gpui::px(300.), gpui::px(220.)), cx);
        let (sibling, mut sibling_window) =
            native_tree(root, gpui::size(gpui::px(300.), gpui::px(220.)), cx);
        let focus = || C::Focused {
            target: "popup-draft".into(),
        };
        assert!(
            !wire::decode::<bool>(&native_command(&view, &mut native, focus()).unwrap()).unwrap()
        );
        native_command(
            &view,
            &mut native,
            C::Focus {
                target: "popup-draft".into(),
            },
        )
        .unwrap();
        assert!(
            wire::decode::<bool>(&native_command(&view, &mut native, focus()).unwrap()).unwrap()
        );
        assert!(
            !wire::decode::<bool>(&native_command(&sibling, &mut sibling_window, focus()).unwrap())
                .unwrap()
        );
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let observed = events.clone();
        let _subscription = native.update(|_, cx| {
            cx.subscribe(&view, move |_, event: &wire::Event, _| {
                observed.borrow_mut().push(event.clone())
            })
        });
        native_command(
            &view,
            &mut native,
            C::Select {
                target: "popup-draft".into(),
                start: 1,
                end: 3,
            },
        )
        .unwrap();
        native.simulate_input("X");
        assert!(
            events
                .borrow()
                .iter()
                .any(|event| matches!(event, wire::Event::Input { text, .. } if text == "aXd"))
        );
    }

    #[gpui_kit::test]
    fn widget_commands_reach_native_focus_input_selection_and_scroll(cx: &mut TestAppContext) {
        use wire::WidgetCommand as C;
        let path = staged("pages").expect("actual guest instantiation is required");
        let mut guest = Guest::load_from("pages", &path).unwrap();
        let input = |key: &str| wire::Node::Input {
            options: Default::default(),
            key: key.into(),
            placeholder: String::new(),
            value: "abcd".into(),
            on_input: 0,
            on_submit: None,
            width: None,
            secure: false,
            style: Box::default(),
        };
        // A capability fixture made from the same wire primitives guests publish.
        guest.frame.root = Some(wire::Node::Linear {
            max_width: None,
            clip: false,
            key: "fixture".into(),
            wrap: None,
            axis: wire::Axis::Column,
            spacing: None,
            padding: None,
            width: None,
            height: None,
            align: None,
            background: None,
            border: None,
            children: vec![
                input("draft"),
                input("second"),
                wire::Node::Scroll {
                    on_scroll: None,
                    virtual_rows: true,
                    key: "list".into(),
                    direction: wire::ScrollDirection::Vertical,
                    width: None,
                    height: Some(wire::Length::Fixed(100.0)),
                    bar_hidden: false,
                    bar_width: None,
                    bar_margin: None,
                    scroller_width: None,
                    bar_spacing: None,
                    anchor_x: wire::ScrollAnchor::Start,
                    anchor_y: wire::ScrollAnchor::Start,
                    auto_scroll: false,
                    background: None,
                    border: None,
                    content: Box::new(wire::Node::KeyedColumn {
                        key: "rows".into(),
                        keys: Some((0..20).map(wire::ListKey::Integer).collect()),
                        background: None,
                        border: None,
                        spacing: None,
                        padding: None,
                        width: None,
                        height: None,
                        max_width: None,
                        align: None,
                        virtual_row: Some(30.0),
                        children: (0..20)
                            .map(|_| wire::Node::Space {
                                width: None,
                                height: Some(wire::Length::Fixed(30.0)),
                            })
                            .collect(),
                    }),
                },
            ],
        });
        guest
            .inputs
            .replace(guest.frame.root.as_ref().unwrap())
            .expect("native projections");
        guest.pending.clear();
        let root = guest.frame.root.clone().unwrap();
        let (view, mut native) =
            native_tree(root.clone(), gpui::size(gpui::px(300.), gpui::px(220.)), cx);
        let (sibling, mut sibling_window) =
            native_tree(root.clone(), gpui::size(gpui::px(300.), gpui::px(220.)), cx);
        macro_rules! run {
            ($command:expr) => {{
                guest.answer(
                    wire::Request {
                        id: 900,
                        kind: "host.widget".into(),
                        payload: wire::encode(&$command),
                    },
                    &None,
                );
                guest
                    .execute_widget_commands(|command| native_command(&view, &mut native, command));
                let Some(wire::Event::Response {
                    id: 900,
                    result,
                    done: true,
                }) = guest.pending.pop()
                else {
                    panic!("missing native widget response");
                };
                result.expect("native command")
            }};
        }
        macro_rules! focused {
            ($target:expr) => {
                wire::decode::<bool>(&run!(C::Focused {
                    target: $target.into()
                }))
                .unwrap()
            };
        }
        assert!(!focused!("draft"));
        run!(C::Focus {
            target: "draft".into()
        });
        assert!(focused!("draft"));
        run!(C::FocusNext);
        assert!(focused!("second"));
        run!(C::FocusPrevious);
        assert!(focused!("draft"));
        assert!(
            !wire::decode::<bool>(
                &native_command(
                    &sibling,
                    &mut sibling_window,
                    C::Focused {
                        target: "draft".into()
                    }
                )
                .unwrap()
            )
            .unwrap()
        );
        for (command, expected) in [
            (
                C::CursorFront {
                    target: "draft".into(),
                },
                "Xabcd",
            ),
            (
                C::CursorEnd {
                    target: "draft".into(),
                },
                "abcdX",
            ),
            (
                C::Cursor {
                    target: "draft".into(),
                    position: 2,
                },
                "abXcd",
            ),
            (
                C::SelectAll {
                    target: "draft".into(),
                },
                "X",
            ),
            (
                C::Select {
                    target: "draft".into(),
                    start: 1,
                    end: 3,
                },
                "aXd",
            ),
        ] {
            let (view, mut native) =
                native_tree(root.clone(), gpui::size(gpui::px(300.), gpui::px(220.)), cx);
            let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let observed = events.clone();
            let _subscription = native.update(|_, cx| {
                cx.subscribe(&view, move |_, event: &wire::Event, _| {
                    observed.borrow_mut().push(event.clone())
                })
            });
            native_command(
                &view,
                &mut native,
                C::Focus {
                    target: "draft".into(),
                },
            )
            .unwrap();
            native_command(&view, &mut native, command).unwrap();
            native.simulate_input("X");
            assert!(
                events.borrow().iter().any(
                    |event| matches!(event, wire::Event::Input { text, .. } if text == expected)
                ),
                "{expected}: {:?}",
                events.borrow()
            );
        }
        for (command, expected) in [
            (
                C::ScrollTo {
                    target: "list".into(),
                    x: 0.,
                    y: 100.,
                },
                100.,
            ),
            (
                C::ScrollBy {
                    target: "list".into(),
                    x: 0.,
                    y: -24.,
                },
                76.,
            ),
            (
                C::Snap {
                    target: "list".into(),
                    x: 0.,
                    y: 0.5,
                },
                250.,
            ),
            (
                C::SnapEnd {
                    target: "list".into(),
                },
                500.,
            ),
            (
                C::ScrollToKey {
                    target: "list".into(),
                    key: 3,
                },
                90.,
            ),
        ] {
            run!(command);
            let offset = view
                .read_with(&native, |view, _| view.scroll_offset("list"))
                .unwrap();
            assert_eq!(-f32::from(offset.y), expected);
        }
        for reordered in [false, true] {
            let saved = native.update(|window, cx| view.read(cx).presentation(window, cx));
            let mut replacement = root.clone();
            if reordered {
                replacement.for_each_mut(&mut |node| {
                    if let wire::Node::KeyedColumn {
                        keys: Some(keys), ..
                    } = node
                    {
                        keys.reverse();
                    }
                });
            }
            let window = cx.open_window(gpui::size(gpui::px(300.), gpui::px(220.)), |_, _| {
                crate::view_tree::ViewTree::new(replacement).with_presentation(saved)
            });
            let fresh = window.root(cx).unwrap();
            let mut fresh_window = VisualTestContext::from_window(window.into(), cx);
            fresh_window.update(|window, cx| window.render_frame(cx));
            let offset = fresh
                .read_with(&fresh_window, |view, _| view.scroll_offset("list"))
                .unwrap();
            assert_eq!(
                -f32::from(offset.y),
                if reordered { 0. } else { 90. },
                "scroll restoration requires the same ordered row keys"
            );
        }
    }

    #[gpui_kit::test]
    fn replacement_inputs_restore_selection_only_for_identical_values_and_fresh_handlers(
        cx: &mut TestAppContext,
    ) {
        let input = |value: &str, secure, handler| wire::Node::Input {
            options: Default::default(),
            key: "draft".into(),
            placeholder: String::new(),
            value: value.into(),
            on_input: handler,
            on_submit: None,
            width: None,
            secure,
            style: Box::default(),
        };
        let (old, mut old_window) = native_tree(
            input("가🙂나", false, 1),
            gpui::size(gpui::px(300.), gpui::px(100.)),
            cx,
        );
        native_command(
            &old,
            &mut old_window,
            wire::WidgetCommand::Focus {
                target: "draft".into(),
            },
        )
        .unwrap();
        native_command(
            &old,
            &mut old_window,
            wire::WidgetCommand::Select {
                target: "draft".into(),
                start: 1,
                end: 2,
            },
        )
        .unwrap();
        for (value, secure, restore) in [
            ("가🙂나", false, true),
            ("changed", false, false),
            ("가🙂나", true, false),
        ] {
            let saved = old_window.update(|window, cx| old.read(cx).presentation(window, cx));
            let root = input(value, secure, 77);
            let window = cx.open_window(gpui::size(gpui::px(300.), gpui::px(100.)), |_, _| {
                crate::view_tree::ViewTree::new(root).with_presentation(saved)
            });
            let view = window.root(cx).unwrap();
            let mut native = VisualTestContext::from_window(window.into(), cx);
            native.update(|window, cx| window.render_frame(cx));
            let focused: bool = wire::decode(
                &native_command(
                    &view,
                    &mut native,
                    wire::WidgetCommand::Focused {
                        target: "draft".into(),
                    },
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(
                focused, restore,
                "restore requires exact source and masking"
            );
            if !restore {
                continue;
            }
            let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let observed = events.clone();
            let _subscription = native.update(|_, cx| {
                cx.subscribe(&view, move |_, event: &wire::Event, _| {
                    observed.borrow_mut().push(event.clone())
                })
            });
            native.simulate_input("X");
            assert!(events.borrow().iter().any(
                |event| matches!(event,wire::Event::Input {handler:77,text} if text == "가X나")
            ));
            assert!(
                !events
                    .borrow()
                    .iter()
                    .any(|event| matches!(event, wire::Event::Input { handler: 1, .. }))
            );
        }
    }

    #[test]
    fn widget_requests_validate_scope_payload_budget_and_frame() {
        let path = staged("pages").expect("actual Pages Wasm is required");
        let mut guest = Guest::load_from("pages", &path).unwrap();
        guest.redraw(&None);
        let target = guest.frame.root.as_ref().unwrap().key().unwrap().to_owned();
        let command = wire::WidgetCommand::Focus {
            target: target.clone(),
        };
        let request = |id, payload| wire::Request {
            id,
            kind: "host.widget".into(),
            payload,
        };
        guest.pending.clear();
        let mut trailing = wire::encode(&command);
        trailing.push(0);
        for payload in [
            vec![255; 4],
            trailing,
            vec![0; MAX_PAYLOAD_BYTES + 1],
            wire::encode(&wire::WidgetCommand::Focus {
                target: "OtherGuest/draft".into(),
            }),
            wire::encode(&wire::WidgetCommand::Focus {
                target: "x".repeat(wire::MAX_STRING_BYTES + 1),
            }),
            wire::encode(&wire::WidgetCommand::ScrollBy {
                target: target.clone(),
                x: f32::NAN,
                y: 0.0,
            }),
        ] {
            guest.answer(request(99, payload), &None);
            assert!(guest.widget_commands.is_empty());
            assert!(matches!(
                guest.pending.pop(),
                Some(wire::Event::Response { result: Err(_), .. })
            ));
        }
        for id in 0..MAX_REQUESTS_PER_TICK as u64 {
            guest.answer(request(id, wire::encode(&command)), &None);
        }
        assert_eq!(guest.widget_commands.len(), MAX_REQUESTS_PER_TICK);
        assert!(!guest.settled(), "native commands hold snapshot admission");
        guest.answer(request(999, wire::encode(&command)), &None);
        assert!(matches!(
            guest.pending.pop(),
            Some(wire::Event::Response { result: Err(_), .. })
        ));
        // A staged cancellation is routed before any mounted traversal.
        guest.staged = true;
        guest.frame.cancels = vec![0];
        guest.redraw(&None);
        assert!(!guest.widget_commands.iter().any(|(id, _, _)| *id == 0));
        guest.frame_rev += 1;
        guest.execute_widget_commands(|_| panic!("old-frame commands must not touch native state"));
        assert!(guest.widget_commands.is_empty());
        assert!(
            guest
                .pending
                .iter()
                .all(|event| matches!(event, wire::Event::Response { result: Err(_), .. }))
        );
    }

    /// The workspace the pages view reads for itself, one entry per query
    /// shape it asks the index tier with: the page list, the open page's
    /// blocks, and the comment threads anchored on them.
    fn pages_register_reply() -> serde_json::Value {
        serde_json::json!({
            "list_pages": { "pages": {
                "pages": [{ "id": "alpha", "title": "Alpha", "parent": null }],
                "has_more": false, "next_after": null
            }},
            "get_page": { "page": {
                "blocks": [
                    { "block_id": "alpha", "parent": null, "kind": "page",
                      "text": "Alpha", "checked": false, "children": ["alpha-1"] },
                    { "block_id": "alpha-1", "parent": "alpha", "kind": "paragraph",
                      "text": "the first paragraph", "checked": false, "children": [] }
                ],
                "next_after": null
            }},
            "threads_for_targets": { "threads": [] }
        })
    }

    /// The pages view's session facts: the chain because a `duck://page/…`
    /// address carries it, and the page a link asked the app to open.
    fn pages_facts() -> Option<Vec<u8>> {
        Some(
            serde_json::to_vec(&serde_json::json!({
                "dark": false, "connected": true, "chain": "mynet#d0cdf950",
                "route_page": "", "route_serial": 0
            }))
            .expect("props encode"),
        )
    }

    /// The node a pages view reads for the placement evidence, canned for this
    /// thread: a long page — a document that fills the pane at every window
    /// width the card is measured in — and a real conversation on one of its
    /// paragraphs, so the card in the picture is a card and not a plate.
    fn can_a_commented_page() {
        let blocks = (1..=80).map(|n| {
            serde_json::json!({
                "id": format!("alpha-{n}"), "parent": "alpha", "page": "alpha",
                "kind": "paragraph", "checked": false, "children": [],
                "text": format!("Paragraph {n}: editable document text.")
            })
        });
        let page = std::iter::once(serde_json::json!({
            "id": "alpha", "parent": null, "page": "alpha", "kind": "page",
            "text": "Alpha", "checked": false,
            "children": (1..=80).map(|n| format!("alpha-{n}")).collect::<Vec<_>>()
        }))
        .chain(blocks)
        .collect::<Vec<_>>();
        can_reads([
            (
                "all",
                serde_json::json!({ "accounts": [
                    { "number": 1, "name": "Ada Lovelace", "keys": [] },
                    { "number": 2, "name": "Bo Chen", "keys": [] }
                ]}),
            ),
            (
                "list_pages",
                serde_json::json!({ "pages": {
                    "pages": [{ "id": "alpha", "title": "Alpha", "parent": null },
                              { "id": "beta", "title": "Beta", "parent": null }],
                    "has_more": false, "next_after": null
                }}),
            ),
            (
                "get_page",
                serde_json::json!({ "page": { "blocks": page, "next_after": null }}),
            ),
            (
                "threads_for_targets",
                serde_json::json!({ "threads": [{ "target": "alpha-7", "threads": [
                    { "id": "t-block", "target": "alpha-7", "opener": "acct:1",
                      "resolved": false, "comments": [
                        { "id": "c1", "author": "acct:1",
                          "text": "This paragraph reads backwards." },
                        { "id": "c2", "author": "acct:2",
                          "text": "Agreed — the clause order is inverted." }
                      ] }
                ]}]}),
            ),
        ]);
    }

    /// The card answers the DOCUMENT PANE, not the window: it floats in the
    /// margin while there is one, squeezes the document left to make one while
    /// the document can still spare it, and below that drops full-width onto
    /// the document's own text column, in a gap the editor holds open for it.
    ///
    /// The native measurement canvas reports the usable content box. The
    /// comment card uses its native surface without guest-painted borders.
    #[gpui_kit::test]
    fn pages_comments_answer_the_pane_they_open_in(cx: &mut TestAppContext) {
        let _turn = blocking_connection_turn();
        can_a_commented_page();
        let props = pages_facts();
        let path = staged("pages").expect("actual Pages Wasm is required");
        let mut measured = |width: f32| {
            let mut guest = Guest::load_from("pages", &path).unwrap();
            settle_documents(&mut guest, &props);
            let mut sidebar_width = None;
            let mut divider_width = None;
            guest
                .frame
                .root
                .clone()
                .unwrap()
                .for_each_mut(&mut |node| match node {
                    wire::Node::Container {
                        key,
                        width: Some(wire::Length::Fixed(value)),
                        ..
                    } if key.ends_with("/page-list") => sidebar_width = Some(*value),
                    wire::Node::ResizeHandle { key, content, .. }
                        if key.ends_with("/sidebar-divider") =>
                    {
                        if let wire::Node::Space {
                            width: Some(wire::Length::Fixed(value)),
                            ..
                        } = content.as_ref()
                        {
                            divider_width = Some(*value);
                        }
                    }
                    _ => {}
                });
            let pane_width = width
                - sidebar_width.expect("authored sidebar")
                - divider_width.expect("authored divider");
            // Install the event bridge before mounting the guest, as the real
            // NativeModuleView does. Otherwise its first Sensor::on_show is lost.
            let (view, mut native) = native_tree(
                wire::Node::Space {
                    width: None,
                    height: None,
                },
                gpui::size(gpui::px(width), gpui::px(700.)),
                cx,
            );
            view.update(&mut native, |view, cx| {
                view.set_editor_store(guest.inputs.clone(), cx)
            });
            let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let observed = events.clone();
            let _subscription = native.update(|_, cx| {
                cx.subscribe(&view, move |_, event: &wire::Event, _| {
                    observed.borrow_mut().push(event.clone())
                })
            });
            macro_rules! layout {
                () => {
                    for _ in 0..8 {
                        for event in events.take() {
                            input::deliver(&mut guest, event);
                        }
                        settle_documents(&mut guest, &props);
                        view.update(&mut native, |view, cx| {
                            view.replace(guest.frame.root.clone().unwrap(), cx)
                        });
                        native.update(|window, cx| window.render_frame(cx));
                    }
                };
            }
            layout!();
            let key = button_key(&guest, "Comments");
            native.update(|window, cx| window.click(key, cx));
            layout!();
            if width == 1300. {
                let mut published_width = None;
                guest.frame.root.clone().unwrap().for_each_mut(&mut |node| {
                    if let wire::Node::Container { key, max_width, .. } = node {
                        if key == "pages/document/surface" {
                            published_width = *max_width;
                        }
                    }
                });
                assert_eq!(
                    published_width,
                    Some(pane_width - 340. - 32.),
                    "the measured pane must reach the guest before native layout"
                );
            }
            view.read_with(&native, |view, _| {
                let pane = view
                    .measured_bounds("PagesView/root/pages/pane-measure")
                    .expect("pane sensor");
                assert_eq!(
                    f32::from(pane.size.width),
                    pane_width,
                    "pane must exclude the fixed sidebar"
                );
                assert!(
                    f32::from(pane.size.height) > 0.,
                    "the pane measurement must have visible height: {pane:?}"
                );
                (
                    view.measured_bounds("PagesView/root/pages/document")
                        .expect("document editor"),
                    view.measured_bounds("PagesView/root/pages/comments-card")
                        .expect("comment card"),
                )
            })
        };
        let (beside_editor, beside_card) = measured(1500.);
        assert_eq!(f32::from(beside_editor.size.width), 704.);
        assert_eq!(f32::from(beside_card.size.width), 340.);
        let (squeeze_editor, squeeze_card) = measured(1300.);
        assert_eq!(
            f32::from(squeeze_editor.size.width),
            1066. - 340. - 32. - 62.
        );
        assert_eq!(f32::from(squeeze_card.size.width), 340.);
        assert!(squeeze_editor.size.width < beside_editor.size.width);
        let (inline_editor, inline_card) = measured(1100.);
        assert_eq!(f32::from(inline_editor.size.width), 704.);
        assert_eq!(inline_card.size.width, inline_editor.size.width);
    }

    /// The forge view draws itself against session facts only; everything
    /// on its screen it reads for itself over the kernel contract.
    fn forge_facts() -> Option<Vec<u8>> {
        Some(
            br#"{
          "dark": false, "connected": true, "org": "duckhouse", "about": "",
          "tier": "validator", "network_chain_id": "mynet#d0cdf950",
          "connected_rpc": "http://127.0.0.1:1", "link": "", "link_tick": 0
        }"#
            .to_vec(),
        )
    }

    pub(super) fn chat_facts() -> Option<Vec<u8>> {
        chat_facts_in("channel-a", 0, &[])
    }

    fn chat_row(seq: i64) -> serde_json::Value {
        chat_row_of(seq, "first light", None)
    }

    /// One committed row as the index answers it — `thread` naming the root it
    /// replies to, for the rows a landing seats a rail on.
    fn chat_row_of(seq: i64, text: &str, thread: Option<i64>) -> serde_json::Value {
        serde_json::json!({
            "channel_id": "channel-a", "seq": seq, "message_id": format!("m{seq}"),
            "author": "acct:7", "height": 84_912, "time": 84_912,
            "blocks": [{ "paragraph": [{ "text": text, "marks": [] }] }],
            "text": text, "deleted": false, "edited": false, "rev": 0,
            "edited_at": null, "base_rev": null, "thread": thread,
            "reply_count": 0, "last_reply_seq": null, "reactions": [], "tags": []
        })
    }

    fn chat_accounts_reply() -> serde_json::Value {
        serde_json::json!({ "accounts": [
            { "number": 7, "name": "mallard", "control": { "person": {} },
              "keys": [{ "pubkey": [0xaa] }] }
        ]})
    }

    fn chat_channel_reply() -> serde_json::Value {
        serde_json::json!({ "channel": {
            "id": "channel-a", "name": "general", "created_at": 1,
            "post_policy": "open", "owner": "acct:7", "archived": false,
            "hooks": [], "huddle": [], "head_seq": 1
        }})
    }

    fn chat_members_reply() -> serde_json::Value {
        serde_json::json!({ "members": {
            "members": [{ "party": "acct:7", "height": 1, "time": 1 }],
            "has_more": false
        }})
    }

    /// The node a chat view reads, canned for this thread: the identity
    /// directory, the room record, one message, the roster and its thread.
    /// A HOST contract driven against the real view needs a room on screen,
    /// and under the kernel contract the view reads that room for itself.
    pub(super) fn can_the_chat_room() {
        can_reads([
            ("all", chat_accounts_reply()),
            ("channel", chat_channel_reply()),
            (
                "roots",
                serde_json::json!({ "roots": { "roots": [chat_row(1)], "has_more": false }}),
            ),
            ("members", chat_members_reply()),
            (
                "thread",
                serde_json::json!({ "thread": { "root": chat_row(1), "replies": [],
                    "has_more": false, "next_reply_seq": null }}),
            ),
        ]);
    }

    /// The chat SESSION facts, encoded the way the host encodes them. NO
    /// TIMELINE: under the kernel contract the view reads its own room's
    /// messages off the index, so what the app pushes is who the reader is,
    /// which room she is in, and the runs the node has in flight — always the
    /// WHOLE node's rows, because narrowing them to `room` is what the host is
    /// on the hook for.
    fn chat_facts_in(
        room: &'static str,
        land_seq: i64,
        live: &[crate::backend::LiveAgentRow],
    ) -> Option<Vec<u8>> {
        let general = crate::backend::ChatChannel {
            id: "channel-a".into(),
            name: "general".into(),
            ..Default::default()
        };
        let ops = crate::backend::ChatChannel {
            id: "channel-b".into(),
            name: "ops".into(),
            ..Default::default()
        };
        let rooms = [
            crate::backend::ChatSidebarRow {
                channel: general,
                unread: false,
            },
            crate::backend::ChatSidebarRow {
                channel: ops,
                unread: true,
            },
        ];
        let props = ChatProps {
            dark: false,
            connected: true,
            endpoint: "http://127.0.0.1:1",
            network_name: "testnet",
            network_chain_id: "testnet#abcd",
            status: "Live",
            block_height: 84_912,
            me: "acct:7".into(),
            me_key: "aa",
            names_serial: 0,
            rooms: &rooms,
            dm_rows: &[],
            channel_create_open: false,
            active_channel: room,
            active_dm_peer: "",
            active_dm: &crate::backend::DmPeer::default(),
            land_seq,
            unread_boundary: 0,
            busy: false,
            loading: false,
            huddle_joined: false,
            huddle_channel: "",
            huddle_channel_name: "",
            huddle_joined_at: 0,
            huddle_now: 0,
            call_muted: false,
            call_speaking: false,
            speaking_peers: Vec::new(),
            shift_held: false,
            copy_chord_serial: 0,
            sent_serial: 0,
            pending_sends: &[],
            live_agents: live_agents_within(live, room, LIVE_AGENT_TEXT_BUDGET),
        };
        Some(serde_json::to_vec(&props).expect("props encode"))
    }

    /// One pending run of `agent`, anchored at seq 2 of `room`.
    fn live_run(room: &str, agent: &str, status: &str) -> crate::backend::LiveAgentRow {
        crate::backend::LiveAgentRow {
            channel_id: room.into(),
            anchor_seq: 2,
            run_id: "chat\u{1f}channel-a\u{1f}2\u{1f}chiefduck".into(),
            agent: agent.into(),
            status: status.into(),
            ..Default::default()
        }
    }

    /// ROOM ISOLATION, DECIDED BY THE HOST. A run lives in this process, not on
    /// the chain, so it reaches the view as a session fact — and the reading
    /// covers the WHOLE node. The room on screen is what picks rows out of it,
    /// and it is picked at encode time, which is why no handler that moves
    /// `active_channel` has to remember this lane exists. What the view then
    /// DRAWS of a run — the door under its anchor, the card in its thread, and
    /// Stop leaving as a cancel — is the view's own test.
    #[test]
    fn the_room_on_screen_decides_which_of_the_nodes_runs_are_drawn() {
        let reading = [
            live_run("channel-a", "Chief Duck", "Reading the repo"),
            live_run("channel-b", "Ops Duck", "Draining the queue"),
        ];

        let here =
            String::from_utf8(chat_facts_in("channel-a", 0, &reading).expect("props encode"))
                .unwrap();
        assert!(here.contains("Chief Duck"), "this room's run is missing");
        assert!(
            !here.contains("Ops Duck"),
            "another room's run crossed to the view: {here}"
        );

        let there =
            String::from_utf8(chat_facts_in("channel-b", 0, &reading).expect("props encode"))
                .unwrap();
        assert!(there.contains("Ops Duck"), "that room's run is missing");
        assert!(
            !there.contains("Chief Duck"),
            "the room she left kept its run on the frame: {there}"
        );
    }

    /// A room full of runs cannot blank the messages they sit under: the cards
    /// spend [`LIVE_AGENT_TEXT_BUDGET`] and the newest anchors are the ones
    /// kept, because those are the ones at the tail she is looking at.
    #[test]
    fn the_live_cards_are_held_to_their_slice_of_the_frame_budget() {
        let crowd: Vec<_> = (1..=60)
            .map(|seq| crate::backend::LiveAgentRow {
                channel_id: "channel-a".into(),
                anchor_seq: seq,
                run_id: format!("run-{seq}"),
                agent: format!("agent-{seq}"),
                status: "x".repeat(400),
                ..Default::default()
            })
            .collect();
        let kept = live_agents_within(&crowd, "channel-a", LIVE_AGENT_TEXT_BUDGET);
        let spent: usize = kept.iter().map(live_text_bytes).sum();
        assert!(
            spent <= LIVE_AGENT_TEXT_BUDGET,
            "{spent} bytes past the {LIVE_AGENT_TEXT_BUDGET} byte ceiling"
        );
        assert!(kept.len() < crowd.len(), "nothing was held back");
        assert_eq!(
            kept.last().map(|row| row.anchor_seq),
            Some(60),
            "the newest anchor is the one that must survive"
        );
        assert!(
            kept.windows(2)
                .all(|pair| pair[0].anchor_seq < pair[1].anchor_seq),
            "the kept rows are handed back in anchor order"
        );
    }

    /// The facts a module's host pushes, and one word of them the tree
    /// shows — what a swap must carry from A into B's first tree.
    fn facts(module: &str) -> (Option<Vec<u8>>, &'static str) {
        match module {
            "governance" => (session_props(), "prop-1"),
            "files" => (files_facts(), "README.md"),
            "pages" => (pages_facts(), "Alpha"),
            "chat" => (chat_facts(), "general"),
            // forge reads its whole screen off the node; what the session
            // alone paints is the network it is reading
            _ => (forge_facts(), "duckhouse"),
        }
    }

    /// A new deployment of a module whose view is drawn replaces the
    /// instance in place: restored from the drawn view's snapshot, under a
    /// new generation (so the old tree's messages route nowhere), with the
    /// new deployment's assets, and its first redraw routes the restored
    /// view's requests — the props subscription among them — without
    /// another tick. Every module-owned view, from its own staged wasm:
    /// a view whose props ride a mount task instead of a subscription can
    /// never be snapshotted while that task is live, and never asks again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_new_deployment_swaps_the_view_in_place() {
        let _turn = connection_turn().await;
        use crate::backend::view_source::tests::{FakeDeployment, fake_node};
        for module in ["governance", "files", "pages", "chat", "forge"] {
            let Some(staged) = staged(module) else {
                continue;
            };
            let (props, shown) = facts(module);
            let component = std::fs::read(staged).expect("the staged view");
            let (a, b) = (
                deployment(&component, "a.svg"),
                deployment(&component, "b.svg"),
            );
            let node = FakeDeployment::serving(module, &a);
            // the governance and files views read their own registers off
            // the node itself
            node.answer_query("governance", proposals_reply());
            node.answer_files("ls", files_listing_reply());
            node.answer_files("history", serde_json::json!({ "snapshots": [] }));
            node.answer_view("pages", pages_register_reply());
            let client = fake_node(node.clone()).await;

            let mounted = fresh(module);
            connected(&client).joined();
            assert_eq!(slot_assets(&mounted), ["a.svg"], "{module}");
            let (generation, frame_rev) = {
                let mut locked = mounted.lock().expect("module view lock");
                let generation = locked.generation;
                let Slot::Ready(guest) = &mut locked.slot else {
                    panic!("{module}: the view of A");
                };
                // the view draws, takes facts that are not its initial
                // state, and settles — its editor's document included
                settle_documents(guest, &props);
                assert!(guest.settled(), "{module} fault: {:?}", guest.fault);
                assert!(guest.props_subscription.is_some(), "{module}");
                assert!(
                    texts(guest).iter().any(|text| text == shown),
                    "{module}: {shown:?} not shown in {:?}",
                    texts(guest)
                );
                (generation, guest.frame_rev)
            };

            // a block activates B: the check finds the hash moved
            node.deploy(module, &b);
            deployments_checked().await.joined();
            assert_eq!(slot_assets(&mounted), ["b.svg"], "{module}: B installed");
            let generation = {
                let mut locked = mounted.lock().expect("module view lock");
                assert_eq!(locked.hash, Some(b.hash()), "{module}");
                assert!(
                    locked.generation > generation,
                    "{module}: the old tree's messages are refused"
                );
                let Slot::Ready(guest) = &mut locked.slot else {
                    panic!("{module}: the view of B");
                };
                assert!(
                    guest.frame_rev > frame_rev,
                    "{module}: the widget rebuilds for the new tree"
                );
                assert!(
                    guest.staged && guest.props_subscription.is_none(),
                    "{module}"
                );
                // B's first tree is A's state, before any facts reach it
                assert!(
                    texts(guest).iter().any(|text| text == shown),
                    "{module}: the facts did not carry over: {:?}",
                    texts(guest)
                );
                let ticks = guest.ticks;
                // the first redraw routes the staged requests without another
                // tick; a view that reads through the kernel then has those
                // reads in flight, and a view that owns an editor has its
                // restored document to install natively — either way it is
                // quiet once they land
                let busy = guest.redraw(&None);
                assert_eq!(guest.ticks, ticks, "{module}");
                if busy {
                    settle_documents(guest, &None);
                }
                assert!(guest.settled(), "{module}: {:?}", guest.fault);
                assert!(
                    guest.props_subscription.is_some(),
                    "{module}: the restored view asked for its props again"
                );
                assert!(guest.fault.is_none(), "{module}: {:?}", guest.fault);
                locked.generation
            };
            // and the same deployment again is nothing to do
            deployments_checked().await.joined();
            {
                let locked = mounted.lock().expect("module view lock");
                assert_eq!(
                    (locked.generation, locked.hash),
                    (generation, Some(b.hash())),
                    "{module}"
                );
            }
            // the seat is given back: the next module's node, and the
            // other deployment tests, know nothing of this one
            registry().lock().expect("module views").remove(module);
        }
    }

    /// A valid view with an admitted event it has not consumed yet.
    fn drawn_then_stuck(mounted: &Arc<Mutex<Mounted>>) -> u64 {
        let mut locked = mounted.lock().expect("module view lock");
        let generation = locked.generation;
        let Slot::Ready(guest) = &mut locked.slot else {
            panic!("the view of A");
        };
        assert!((0..4).any(|_| !guest.redraw(&session_props())));
        assert!(guest.settled(), "{:?}", guest.fault);
        assert!(guest.ever_valid_tree);
        guest.pending.push(wire::Event::Resync);
        assert!(!guest.settled());
        generation
    }

    /// The host boundary after a tick returned no valid frame. The actual
    /// staged instance has not published a tree; its next redraw can recover.
    fn never_valid_pending(mounted: &Arc<Mutex<Mounted>>) -> u64 {
        let mut locked = mounted.lock().expect("module view lock");
        let generation = locked.generation;
        let Slot::Ready(guest) = &mut locked.slot else {
            panic!("the view of A");
        };
        assert_eq!(guest.ticks, 0);
        assert!(!guest.ever_valid_tree);
        assert!(guest.frame.root.is_none());
        guest.ticks = 1;
        guest.pending.push(wire::Event::Resync);
        generation
    }

    /// A block that finds the seated view busy waits under the generation
    /// it has: one generation per load, not one per block.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_block_that_waits_on_pending_work_keeps_the_generation() {
        let _turn = connection_turn().await;
        use crate::backend::view_source::tests::{FakeDeployment, fake_node};
        let staged =
            staged("governance").expect("staged governance is required for recovery evidence");
        let component = std::fs::read(staged).expect("the staged view");
        let (a, b) = (
            deployment(&component, "a.svg"),
            deployment(&component, "b.svg"),
        );
        let node = FakeDeployment::serving("governance", &a);
        let client = fake_node(node.clone()).await;
        let mounted = fresh("governance");
        connected(&client).joined();
        let generation = drawn_then_stuck(&mounted);
        node.deploy("governance", &b);
        for _ in 0..3 {
            deployments_checked().await.joined();
            let locked = mounted.lock().expect("module view lock");
            assert_eq!(locked.generation, generation, "no load opened");
            assert_eq!(locked.hash, Some(a.hash()));
            assert!(locked.waiting_since.is_some(), "the wait is counted");
        }
        assert_eq!(slot_assets(&mounted), ["a.svg"]);
        registry()
            .lock()
            .expect("module views")
            .remove("governance");
    }

    /// A view that never drew a usable tree and keeps work pending past
    /// the wait is replaced without its state; the candidate still proves
    /// its first tree.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_view_without_a_tree_is_replaced_once_its_pending_work_outlives_the_wait() {
        let _turn = connection_turn().await;
        use crate::backend::view_source::tests::{FakeDeployment, fake_node};
        let staged =
            staged("governance").expect("staged governance is required for recovery evidence");
        let component = std::fs::read(staged).expect("the staged view");
        let (a, b) = (
            deployment(&component, "a.svg"),
            deployment(&component, "b.svg"),
        );
        let node = FakeDeployment::serving("governance", &a);
        let client = fake_node(node.clone()).await;
        let mounted = fresh("governance");
        connected(&client).joined();
        let generation = never_valid_pending(&mounted);
        node.deploy("governance", &b);
        // inside the wait: nothing moves
        deployments_checked().await.joined();
        assert_eq!(slot_assets(&mounted), ["a.svg"]);
        // the wait is over
        mounted.lock().expect("module view lock").waiting_since =
            Some(Instant::now() - REPLACEMENT_WAIT);
        deployments_checked().await.joined();
        assert_eq!(
            slot_assets(&mounted),
            ["b.svg"],
            "B replaced the stuck view"
        );
        let mut locked = mounted.lock().expect("module view lock");
        assert_eq!(locked.hash, Some(b.hash()));
        assert_eq!(
            locked.generation,
            generation + 1,
            "one load, one generation"
        );
        assert!(locked.waiting_since.is_none());
        let Slot::Ready(guest) = &mut locked.slot else {
            panic!("the view of B");
        };
        assert!(guest.frame.root.is_some(), "B proved its first tree");
        assert!(guest.settled(), "{:?}", guest.fault);
        drop(locked);
        registry()
            .lock()
            .expect("module views")
            .remove("governance");
    }

    /// A view that has drawn a tree is never replaced under it: its pending
    /// work may be a reader's, and it waits however long that takes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_view_that_drew_a_tree_keeps_waiting_past_the_wait() {
        let _turn = connection_turn().await;
        use crate::backend::view_source::tests::{FakeDeployment, fake_node};
        let staged =
            staged("governance").expect("staged governance is required for recovery evidence");
        let component = std::fs::read(staged).expect("the staged view");
        let (a, b) = (
            deployment(&component, "a.svg"),
            deployment(&component, "b.svg"),
        );
        let node = FakeDeployment::serving("governance", &a);
        let client = fake_node(node.clone()).await;
        let mounted = fresh("governance");
        connected(&client).joined();
        let generation = drawn_then_stuck(&mounted);
        node.deploy("governance", &b);
        deployments_checked().await.joined();
        mounted.lock().expect("module view lock").waiting_since =
            Some(Instant::now() - REPLACEMENT_WAIT * 4);
        deployments_checked().await.joined();
        assert_eq!(slot_assets(&mounted), ["a.svg"], "the drawn view stays");
        let locked = mounted.lock().expect("module view lock");
        assert_eq!(
            (locked.generation, locked.hash),
            (generation, Some(a.hash()))
        );
        drop(locked);
        registry()
            .lock()
            .expect("module views")
            .remove("governance");
    }

    /// A valid view can temporarily lose its rendered tree during resync,
    /// or trap after an edit. Neither state authorizes throwing its data away.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_once_valid_view_keeps_its_state_after_a_patch_gap_or_terminal_fault() {
        let _turn = connection_turn().await;
        use crate::backend::view_source::tests::{FakeDeployment, fake_node};
        let staged =
            staged("governance").expect("staged governance is required for recovery evidence");
        let component = std::fs::read(staged).expect("the staged view");
        let a = deployment(&component, "a.svg");
        let b = deployment(&component, "b.svg");
        let node = FakeDeployment::serving("governance", &a);
        let client = fake_node(node.clone()).await;
        let mounted = fresh("governance");
        connected(&client).joined();
        let generation = drawn_then_stuck(&mounted);
        node.deploy("governance", &b);
        {
            let mut locked = mounted.lock().unwrap();
            let Slot::Ready(guest) = &mut locked.slot else {
                panic!("A")
            };
            guest.frame.root = None;
            locked.waiting_since = Some(Instant::now() - REPLACEMENT_WAIT * 4);
        }
        deployments_checked().await.joined();
        assert_eq!(
            slot_assets(&mounted),
            ["a.svg"],
            "a temporary patch gap must preserve A"
        );
        {
            let mut locked = mounted.lock().unwrap();
            let Slot::Ready(guest) = &mut locked.slot else {
                panic!("A")
            };
            guest.fault = Some("terminal trap after unsaved edits (test)".into());
        }
        deployments_checked().await.joined();
        assert_eq!(
            slot_assets(&mounted),
            ["a.svg"],
            "a later trap must preserve authored state"
        );
        assert_eq!(mounted.lock().unwrap().generation, generation);
        registry().lock().unwrap().remove("governance");
    }

    /// Once A starts serving a real tree, an already prepared destructive
    /// recovery must be rejected at the final seat, even without a new block.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_never_valid_view_that_recovers_during_candidate_loading_is_not_discarded() {
        let _turn = connection_turn().await;
        use crate::backend::view_source::tests::{FakeDeployment, fake_node};
        let staged =
            staged("governance").expect("staged governance is required for recovery evidence");
        let component = std::fs::read(staged).expect("the staged view");
        let a = deployment(&component, "a.svg");
        let b = deployment(&component, "b.svg");
        let node = FakeDeployment::serving("governance", &a);
        let client = fake_node(node.clone()).await;
        let mounted = fresh("governance");
        connected(&client).joined();
        let generation = never_valid_pending(&mounted);
        mounted.lock().unwrap().waiting_since = Some(Instant::now() - REPLACEMENT_WAIT);
        node.deploy("governance", &b);
        let hold = hold_blob(&node);
        let loads = deployments_checked().await;
        node.held.notified().await;
        drawn_then_stuck(&mounted);
        hold.notify_one();
        loads.joined();
        assert_eq!(
            slot_assets(&mounted),
            ["a.svg"],
            "healthy A must not be reset by stale recovery"
        );
        let locked = mounted.lock().unwrap();
        assert_eq!(locked.generation, generation + 1);
        assert!(!locked.in_flight);
        let Slot::Ready(guest) = &locked.slot else {
            panic!("A")
        };
        assert!(guest.ever_valid_tree);
        assert!(
            !guest.pending.is_empty(),
            "the admitted work remains owned by A"
        );
        drop(locked);
        registry().lock().unwrap().remove("governance");
    }

    /// A deployment that moves while its view is prepared is not installed:
    /// the active code is read again right before the seat, and the view
    /// drawn keeps its place and its hash.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_deployment_that_moves_while_its_view_is_prepared_is_not_installed() {
        let _turn = connection_turn().await;
        use crate::backend::view_source::tests::{FakeDeployment, fake_node};
        let Some(staged) = staged("governance") else {
            return;
        };
        let component = std::fs::read(staged).expect("the staged view");
        let (a, b, c) = (
            deployment(&component, "a.svg"),
            deployment(&component, "b.svg"),
            deployment(&component, "c.svg"),
        );
        let node = FakeDeployment::serving("forge", &a);
        let client = fake_node(node.clone()).await;
        let mounted = fresh("forge");
        connected(&client).joined();
        assert_eq!(slot_assets(&mounted), ["a.svg"]);

        // B activates, but its bytes are slow — and C activates meanwhile
        node.deploy("forge", &b);
        let hold = hold_blob(&node);
        let loads = deployments_checked().await;
        node.held.notified().await;
        node.deploy("forge", &c);
        hold.notify_one();
        loads.joined();
        assert_eq!(slot_assets(&mounted), ["a.svg"], "B is not installed");
        assert_eq!(mounted.lock().unwrap().hash, Some(a.hash()));
        // the next block's check brings C
        deployments_checked().await.joined();
        assert_eq!(slot_assets(&mounted), ["c.svg"]);
    }

    /// A deployment that removed its view leaves an explicit empty slot —
    /// never the old view.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_removed_view_leaves_an_empty_slot() {
        let _turn = connection_turn().await;
        use crate::backend::view_source::tests::{FakeDeployment, fake_node};
        let Some(staged) = staged("governance") else {
            return;
        };
        let component = std::fs::read(staged).expect("the staged view");
        let a = deployment(&component, "a.svg");
        let removed = module_artifact::ModuleArtifact::component(vec![9, 9, 9]);
        let node = FakeDeployment::serving("forge", &a);
        let client = fake_node(node.clone()).await;
        let mounted = fresh("forge");
        connected(&client).joined();
        assert_eq!(slot_assets(&mounted), ["a.svg"]);
        node.deploy("forge", &removed);
        deployments_checked().await.joined();
        let locked = mounted.lock().unwrap();
        assert!(
            matches!(locked.slot, Slot::Empty),
            "{}",
            slot_name(&locked.slot)
        );
        assert_eq!(locked.hash, Some(removed.hash()));
    }

    /// Blocks keep landing while a deployment's bytes are slow: the load
    /// after it is left to land, not started over as stale every time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_slow_deployment_is_not_restarted_by_every_block() {
        let _turn = connection_turn().await;
        use crate::backend::view_source::tests::{FakeDeployment, fake_node};
        let Some(staged) = staged("governance") else {
            return;
        };
        let component = std::fs::read(staged).expect("the staged view");
        let (a, b) = (
            deployment(&component, "a.svg"),
            deployment(&component, "b.svg"),
        );
        let node = FakeDeployment::serving("forge", &a);
        let client = fake_node(node.clone()).await;
        let mounted = fresh("forge");
        connected(&client).joined();
        assert_eq!(slot_assets(&mounted), ["a.svg"]);

        let before = mounted.lock().unwrap().generation;
        node.deploy("forge", &b);
        let hold = hold_blob(&node);
        let loads = deployments_checked().await;
        node.held.notified().await;
        let generation = mounted.lock().unwrap().generation;
        assert_eq!(generation, before + 1);
        // three more blocks while B's bytes are held: the status still
        // answers, and none of them starts this view over (the views of
        // earlier tests, unknown to this node, get their own loads)
        let mut later_blocks = Vec::new();
        for _ in 0..3 {
            later_blocks.push(deployments_checked().await);
            let locked = mounted.lock().unwrap();
            assert_eq!((locked.generation, locked.in_flight), (generation, true));
        }
        hold.notify_one();
        loads.joined();
        for loads in later_blocks {
            loads.joined();
        }
        assert_eq!(slot_assets(&mounted), ["b.svg"]);
        let locked = mounted.lock().unwrap();
        assert_eq!((locked.generation, locked.in_flight), (generation, false));
    }

    /// Blocks keep landing on a deployment whose view cannot load: it is
    /// tried again, then held off with a widening gap, instead of paying a
    /// cranelift compile once a block for a candidate that never lands.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_deployment_is_not_retried_by_every_block() {
        let _turn = connection_turn().await;
        use crate::backend::view_source::tests::{FakeDeployment, fake_node};
        // bytes that are not a component: every load of them fails alike
        let bad = deployment(b"not a component", "a.svg");
        let node = FakeDeployment::serving("forge", &bad);
        let client = fake_node(node.clone()).await;
        let mounted = fresh("forge");
        connected(&client).joined();
        {
            let locked = mounted.lock().unwrap();
            assert!(
                matches!(locked.slot, Slot::Failed(_)),
                "{}",
                slot_name(&locked.slot)
            );
        }

        // the first block names the candidate the reconnect did not: it is
        // tried once under its own hash, and every block after it is held
        deployments_checked().await.joined();
        let generation = mounted.lock().unwrap().generation;
        for _ in 0..5 {
            deployments_checked().await.joined();
        }
        {
            let locked = mounted.lock().unwrap();
            assert_eq!(
                locked.generation, generation,
                "a failed candidate was loaded again on every block"
            );
            assert!(
                matches!(locked.slot, Slot::Failed(_)),
                "the failure is still what the tab shows: {}",
                slot_name(&locked.slot)
            );
        }

        // nothing is suppressed for good: once the gap is up the same
        // candidate is tried again, so a load that failed on the transport
        // still recovers on its own
        tokio::time::sleep(RETRY_FIRST + Duration::from_millis(100)).await;
        deployments_checked().await.joined();
        assert_eq!(
            mounted.lock().unwrap().generation,
            generation + 1,
            "the hold-off never expired"
        );
        registry().lock().unwrap().remove("forge");
    }

    /// A removal answered late — the code moved on to C while the
    /// verified "no view" for B was on its way — does not empty the slot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_removal_that_moves_while_it_is_verified_is_not_installed() {
        let _turn = connection_turn().await;
        use crate::backend::view_source::tests::{FakeDeployment, fake_node};
        let Some(staged) = staged("governance") else {
            return;
        };
        let component = std::fs::read(staged).expect("the staged view");
        let (a, c) = (
            deployment(&component, "a.svg"),
            deployment(&component, "c.svg"),
        );
        let removed = module_artifact::ModuleArtifact::component(vec![9, 9, 9]);
        let node = FakeDeployment::serving("governance", &a);
        let client = fake_node(node.clone()).await;
        let mounted = fresh("governance");
        connected(&client).joined();
        assert_eq!(slot_assets(&mounted), ["a.svg"]);

        node.deploy("governance", &removed);
        let hold = hold_blob(&node);
        let loads = deployments_checked().await;
        node.held.notified().await;
        node.deploy("governance", &c);
        hold.notify_one();
        loads.joined();
        assert_eq!(slot_assets(&mounted), ["a.svg"], "A is still drawn");
        assert_eq!(mounted.lock().unwrap().hash, Some(a.hash()));
        deployments_checked().await.joined();
        assert_eq!(slot_assets(&mounted), ["c.svg"]);
    }

    /// Every module-owned load leaves one `view_load` line naming its path
    /// and the time of each of its stages — the node's two answers, the
    /// compile, the instance, the tree a replacement proves, the second
    /// look at the registry — so where a "Loading" wait goes is read off
    /// the log, not guessed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_load_reports_the_time_of_each_of_its_stages() {
        let _turn = connection_turn().await;
        use crate::backend::view_source::tests::{FakeDeployment, fake_node};
        let staged = staged("governance").expect("build the governance guest before this test");
        let component = std::fs::read(staged).expect("the staged view");
        let (a, b) = (
            deployment(&component, "a.svg"),
            deployment(&component, "b.svg"),
        );
        let node = FakeDeployment::serving("governance", &a);
        let client = fake_node(node.clone()).await;
        let lines = super::canary::tap();
        let _mounted = fresh("governance");
        connected(&client).joined();
        node.deploy("governance", &b);
        deployments_checked().await.joined();

        let loads: Vec<String> = lines
            .try_iter()
            .filter(|line| line.starts_with("view_load module=governance "))
            .collect();
        let field = |line: &str, name: &str| -> String {
            line.split_whitespace()
                .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
                .unwrap_or_else(|| panic!("no {name} in {line}"))
                .to_owned()
        };
        let of = |hash: [u8; 32]| -> String {
            let hash = crate::backend::hex_encode(&hash);
            loads
                .iter()
                .find(|line| field(line, "hash") == hash)
                .cloned()
                .unwrap_or_else(|| panic!("no line for {hash} in {loads:?}"))
        };
        let (first, swap) = (of(a.hash()), of(b.hash()));
        assert_eq!(field(&first, "path"), "first");
        assert_eq!(field(&first, "outcome"), "Ready");
        assert_eq!(
            field(&first, "first_frame_ms"),
            "-",
            "a fresh view draws its first tree on the window thread"
        );
        assert_eq!(field(&swap, "path"), "swap");
        assert_eq!(field(&swap, "outcome"), "Swap");
        field(&swap, "first_frame_ms")
            .parse::<u128>()
            .expect("the replacement's first frame is timed");
        for line in [&first, &swap] {
            let ms = |name: &str| {
                field(line, name)
                    .parse::<u128>()
                    .unwrap_or_else(|_| panic!("{name} in {line}"))
            };
            let total = ms("total_ms");
            let stages = ms("status_ms")
                + ms("fetch_ms")
                + ms("compile_ms")
                + ms("init_ms")
                + ms("check_ms");
            assert!(total >= stages, "{line}");
        }
    }

    /// A view mounted but not yet ticked names its instance too: a tick
    /// that reaches it while a fresh candidate is prepared refuses the
    /// candidate, and the next block's replacement carries the tick's
    /// state over.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_view_ticked_while_its_fresh_candidate_is_prepared_keeps_its_state() {
        let _turn = connection_turn().await;
        use crate::backend::view_source::tests::{FakeDeployment, fake_node};
        let Some(staged) = staged("governance") else {
            return;
        };
        let component = std::fs::read(staged).expect("the staged view");
        let (a, b) = (
            deployment(&component, "a.svg"),
            deployment(&component, "b.svg"),
        );
        let node = FakeDeployment::serving("governance", &a);
        let client = fake_node(node.clone()).await;
        let mounted = fresh("governance");
        connected(&client).joined();
        assert_eq!(slot_assets(&mounted), ["a.svg"]);

        node.deploy("governance", &b);
        // the candidate reads the instance after its bytes arrive, and is
        // seated after the node confirms the code did not move: hold the
        // bytes to get past the first, the confirmation to sit between
        let blob = hold_blob(&node);
        let loads = deployments_checked().await;
        node.held.notified().await;
        let status = hold_status(&node);
        blob.notify_one();
        node.held.notified().await;
        {
            let mut locked = mounted.lock().unwrap();
            let Slot::Ready(guest) = &mut locked.slot else {
                panic!("the view of A");
            };
            assert_eq!(guest.ticks, 0);
            assert!((0..4).any(|_| !guest.redraw(&None)));
        }
        status.notify_one();
        loads.joined();
        let ticks = {
            let locked = mounted.lock().unwrap();
            assert_eq!(locked.hash, Some(a.hash()), "the candidate was refused");
            let Slot::Ready(guest) = &locked.slot else {
                panic!("the view of A");
            };
            guest.ticks
        };
        assert!(ticks > 0);
        deployments_checked().await.joined();
        let locked = mounted.lock().unwrap();
        assert_eq!(locked.hash, Some(b.hash()));
        let Slot::Ready(guest) = &locked.slot else {
            panic!("the view of B");
        };
        assert!(guest.staged, "restored from A, not booted fresh");
    }

    /// A candidate for a view never ticked proves its first tree too: one
    /// whose first frame traps is not seated, and the view stays.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_fresh_candidate_whose_first_frame_fails_is_not_installed() {
        let _turn = connection_turn().await;
        use crate::backend::view_source::tests::{FakeDeployment, fake_node};
        let Some(staged) = staged("governance") else {
            return;
        };
        let component = std::fs::read(staged).expect("the staged view");
        let (a, b) = (
            deployment(&component, "a.svg"),
            deployment(&component, "b.svg"),
        );
        let node = FakeDeployment::serving("forge", &a);
        let client = fake_node(node.clone()).await;
        let mounted = fresh("forge");
        connected(&client).joined();
        assert_eq!(slot_assets(&mounted), ["a.svg"]);
        {
            let locked = mounted.lock().unwrap();
            let Slot::Ready(guest) = &locked.slot else {
                panic!("A is seated");
            };
            assert_eq!(guest.ticks, 0);
        }

        node.deploy("forge", &b);
        let hold = hold_blob(&node);
        let loads = deployments_checked().await;
        node.held.notified().await;
        FIRST_FRAME_TRAPS.store(true, std::sync::atomic::Ordering::SeqCst);
        hold.notify_one();
        loads.joined();
        assert!(!FIRST_FRAME_TRAPS.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(slot_assets(&mounted), ["a.svg"], "A stays");
        {
            let locked = mounted.lock().unwrap();
            assert_eq!((locked.hash, locked.in_flight), (Some(a.hash()), false));
        }
        // a first frame that trapped is a property of those bytes, so the
        // very next block does not pay for the same candidate again
        deployments_checked().await.joined();
        assert_eq!(
            slot_assets(&mounted),
            ["a.svg"],
            "B is held off, not retried at once"
        );
        // once its gap is up the candidate is tried again, and this time
        // its first tree draws
        {
            let mut locked = mounted.lock().unwrap();
            let retry = locked.retry.as_mut().expect("B left a hold-off");
            retry.next = Instant::now();
        }
        deployments_checked().await.joined();
        assert_eq!(slot_assets(&mounted), ["b.svg"]);
    }

    /// An artifact asset is found by its canonical relative path, exactly.
    #[test]
    fn an_artifact_asset_is_an_exact_path_lookup() {
        let assets: crate::backend::view_source::Assets =
            [("icons/seal.svg".to_owned(), b"<svg/>".to_vec())].into();
        assert_eq!(
            artifact_asset(&assets, "icons/seal.svg"),
            Some(&b"<svg/>"[..])
        );
        for missing in [
            "seal.svg",
            "/icons/seal.svg",
            "icons/../icons/seal.svg",
            "icons/SEAL.svg",
        ] {
            assert_eq!(artifact_asset(&assets, missing), None, "{missing}");
        }
    }

    /// A staged view carries a manifest the host reads; a component without
    /// one is no view. The manifest's preferred size is for placing a new
    /// window, which the embedded tab never does: it keeps the tab's limits.
    #[test]
    fn a_view_carries_a_readable_manifest() {
        if let Some(staged) = staged("governance") {
            let bytes = std::fs::read(staged).expect("the staged view");
            let manifest = ui_lang_wire::manifest::read_manifest(&bytes).expect("a manifest");
            assert_eq!(manifest.name, "Approvals");
            assert!(
                ui_lang_wire::manifest::read_manifest(b"\0asm\x01\0\0\0").is_none(),
                "a bare core module has no manifest"
            );
        }
    }

    /// The three doors the Forge view needed and the kernel did not have,
    /// driven on the kernel's own runtime. None of them names a module:
    /// `picture.put` stores decoded bytes under whatever surface the view
    /// asks for (the pages are decoded one by one — each is padded base64
    /// in its own right), and `host.roster` seats the mention roster for
    /// whatever composer scope the view built. A door that is handed
    /// nothing refuses rather than storing an empty slot.
    #[test]
    fn the_kernel_stores_a_picture_and_seats_a_composer_roster() {
        let Some(staged) = staged("forge") else {
            return;
        };
        let mut guest = Guest::load_from("forge", &staged).expect("the view loads");

        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            4,
            3,
            image::Rgba([10, 20, 30, 255]),
        ))
        .write_to(&mut png, image::ImageFormat::Png)
        .expect("encode");
        let png = png.into_inner();
        // two pages, each padded on its own, exactly as the guest sends them
        let (head, tail) = png.split_at(png.len() / 2);
        let put = serde_json::json!({
            "surface": "forge", "path": "docs/logo.png",
            "pages": [
                crate::backend::base64_encode(head),
                crate::backend::base64_encode(tail),
            ],
        });
        assert!(kernel::answer(
            &mut guest,
            "picture",
            "put",
            7,
            &serde_json::to_vec(&put).expect("encodes")
        ));
        guest.replies.wait_idle();
        let mut landed = Vec::new();
        guest
            .replies
            .drain_into(&mut landed)
            .expect("bounded replies");
        let [wire::Event::Response { id, result, .. }] = landed.as_slice() else {
            panic!("one answer, got {landed:?}");
        };
        assert_eq!(*id, 7);
        let stored: serde_json::Value =
            serde_json::from_slice(result.as_ref().expect("stored")).expect("dimensions");
        assert_eq!(stored, serde_json::json!({ "width": 4, "height": 3 }));
        assert!(
            crate::backend::stored_picture("forge", "docs/logo.png").is_some(),
            "the surface holds the picture the view put there"
        );

        // a put that names no surface stores nothing
        assert!(kernel::answer(&mut guest, "picture", "put", 8, b"{}"));
        guest.replies.wait_idle();
        let mut landed = Vec::new();
        guest
            .replies
            .drain_into(&mut landed)
            .expect("bounded replies");
        let [wire::Event::Response { result, .. }] = landed.as_slice() else {
            panic!("one answer, got {landed:?}");
        };
        assert!(result.is_err(), "{result:?}");

        // the composer roster is seated synchronously, under the scope the
        // view built
        let scope = "http://127.0.0.1:1\u{1f}forge:core:7";
        let roster = serde_json::json!({
            "scope": scope,
            "members": [{ "key": "aa", "label": "Mallard" }],
        });
        assert!(kernel::answer(
            &mut guest,
            "host",
            "roster",
            9,
            &serde_json::to_vec(&roster).expect("encodes")
        ));
        assert_eq!(
            crate::composer_surface::testing::roster_of(scope),
            ["Mallard"],
            "the composer completes an `@` against what the view read"
        );
        assert!(kernel::answer(&mut guest, "host", "roster", 10, b"{}"));
        // a synchronous door answers into the guest's own pending queue
        assert!(
            guest.pending.iter().any(|event| matches!(
                event,
                wire::Event::Response {
                    id: 9,
                    result: Ok(_),
                    ..
                }
            )),
            "the seated roster is answered: {:?}",
            guest.pending
        );
        assert!(
            guest.pending.iter().any(|event| matches!(
                event,
                wire::Event::Response {
                    id: 10,
                    result: Err(_),
                    ..
                }
            )),
            "a roster with no scope is refused: {:?}",
            guest.pending
        );
    }

    /// The Forge view leaves its discussion note composer to the host, the
    /// way Chat does: a send in that surface crosses as the `composer`
    /// intent carrying the scope it was typed under and its body, and the
    /// typing itself never reaches the guest. The scope the view builds is
    /// `<endpoint>\u{1f}<channel>`, which is what the app recovers the item's
    /// channel from.
    #[test]
    fn the_staged_forge_view_hears_a_note_typed_in_the_hosts_composer() {
        let Some(staged) = staged("forge") else {
            return;
        };
        let mut guest = Guest::load_from("forge", &staged).expect("the view loads");
        assert!(surface_allowed(guest.module, "forge_composer"));
        guest.redraw(&None);
        guest.surface_event(
            None,
            wire::SurfaceValue::Record {
                name: "composer".into(),
                fields: vec![
                    (
                        "scope".into(),
                        wire::SurfaceValue::Str("http://127.0.0.1:1\u{1f}forge:core:7".into()),
                    ),
                    ("kind".into(), wire::SurfaceValue::Str("note".into())),
                    ("body".into(), wire::SurfaceValue::Str("hi".into())),
                    ("id".into(), wire::SurfaceValue::Str("note-1".into())),
                ],
            },
        );
        assert!(guest.pending.is_empty(), "the words never reach the guest");
        assert_eq!(guest.intents.len(), 1);
        assert_eq!(guest.intents[0].kind, "composer");
        assert!(guest.intents[0].detail.contains(r#""body":"hi""#));
        assert_eq!(
            crate::backend::scope_channel(
                serde_json::from_str::<serde_json::Value>(&guest.intents[0].detail)
                    .expect("the intent decodes")["scope"]
                    .as_str()
                    .unwrap_or_default(),
                "http://127.0.0.1:1"
            ),
            "forge:core:7",
            "the app recovers the item's channel from the scope alone"
        );
        assert!(guest.fault.is_none());
    }

    #[test]
    fn oversized_request_batches_are_refused_before_any_prefix_can_execute() {
        let mut frame = wire::Frame {
            requests: (0..MAX_REQUESTS_PER_TICK as u64)
                .map(|id| wire::Request {
                    id,
                    kind: "op.submit".into(),
                    payload: Vec::new(),
                })
                .collect(),
            ..Default::default()
        };
        let (accepted, _) = shape(&wire::encode(&frame)).expect("exact request budget");
        assert_eq!(accepted.requests.len(), MAX_REQUESTS_PER_TICK);
        frame.requests.push(wire::Request {
            id: MAX_REQUESTS_PER_TICK as u64,
            kind: "op.submit".into(),
            payload: Vec::new(),
        });
        assert_eq!(
            shape(&wire::encode(&frame)).err().as_deref(),
            Some("frame request or cancellation budget exceeded")
        );
        frame.requests.clear();
        frame.cancels = (0..(2 * MAX_REQUESTS_PER_TICK) as u64).collect();
        let (accepted, _) = shape(&wire::encode(&frame)).expect("exact cancellation budget");
        assert_eq!(accepted.cancels.len(), 2 * MAX_REQUESTS_PER_TICK);
        frame.cancels.push((2 * MAX_REQUESTS_PER_TICK) as u64);
        assert_eq!(
            shape(&wire::encode(&frame)).err().as_deref(),
            Some("frame request or cancellation budget exceeded")
        );
    }

    #[test]
    fn a_frame_that_changed_nothing_keeps_the_held_tree() {
        let held_tree = wire::Node::empty();
        let mut held = Some(held_tree.clone());
        let mut unchanged = wire::Frame {
            unchanged: true,
            ..wire::Frame::default()
        };
        assert_eq!(
            merge(&mut held, &mut unchanged),
            Ok((false, Default::default()))
        );
        assert_eq!(unchanged.root, Some(held_tree));
        let mut patched = wire::Frame::default();
        assert_eq!(merge(&mut None, &mut patched), Err("no tree to patch"));
    }
}
